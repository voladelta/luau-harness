-- The hxm loop. `hxm init` copies this file to .hxm/loop.luau; from then on that copy is the project's harness,
-- and editing it is how the harness is changed.
--
-- The host supplies a small kernel and nothing else:
--   host.llm(body) -> status, text | nil, error      POST to the configured endpoint; the host adds the API key
--   host.sh(command, {timeout=}) -> {out=, code=, signal=, timed_out=}
--   host.read(path), host.write(path, data), host.list(dir)
--   host.load(source, name, env) -> function | nil, error
--   host.timed(seconds, fn, ...) -> true, ... | false, error
--   host.input(prompt), host.log(text), host.sleep(seconds), host.env(name)
--   host.args, host.cwd, host.interactive, host.default_loop
--   json.encode, json.decode, json.array, json.null
-- The whole process sits inside one Landlock ruleset, so nothing in this file can reach more of the machine
-- than the model's own shell can.

local M = {}

M.model = host.env("HXM_MODEL") or "deepseek-flash"
M.max_output = 16000 -- tokens per model reply
M.max_replies = 200 -- model replies per task before giving up
M.compact_at = 150000 -- prompt tokens at which the conversation is replaced by a summary of itself
M.output_limit = 30000 -- bytes of a tool result shown to the model
M.lua_seconds = 60 -- time limit of one `lua` tool call

-- Errors the host raises when the user or the system wants the run to stop. They must never be swallowed.
local function is_stop(err)
	return err == "interrupted" or err == "terminated"
end

-- Keeps the start and the end of a long text: the start says what ran, the end says how it went.
local function clip(text)
	if #text <= M.output_limit then
		return text
	end
	local half = M.output_limit // 2
	return text:sub(1, half) .. ("\n[... %d bytes omitted ...]\n"):format(#text - 2 * half) .. text:sub(-half)
end

local function need(args, name, kind)
	local value = args[name]
	if type(value) ~= kind then
		error(("argument %q must be a %s"):format(name, kind), 0)
	end
	return value
end

-- Extensions ------------------------------------------------------------------------------------------------
--
-- An extension is a Luau module in .hxm/ext/NAME.luau that returns a table of functions. The model writes them
-- with the ordinary write tool and calls them from the `lua` tool, so the tool list never changes. A module is
-- loaded again whenever its file has changed.

local ext_dir = ".hxm/ext/"
local ext_cache = {}
local ext_env = setmetatable({}, { __index = _G })

function M.ext(name)
	if type(name) ~= "string" or not name:match("^[%w_%-]+$") then
		error("an extension name is letters, digits, _ and -", 0)
	end
	local source, err = host.read(ext_dir .. name .. ".luau")
	if not source then
		error("no extension " .. name .. ": " .. err, 0)
	end
	local cached = ext_cache[name]
	if cached and cached.source == source then
		return cached.value
	end
	local chunk, load_err = host.load(source, "=ext/" .. name, ext_env)
	if not chunk then
		error(load_err, 0)
	end
	local value = chunk()
	ext_cache[name] = { source = source, value = value }
	return value
end
ext_env.ext = M.ext

-- One line per extension: its name and the comment on its first line.
local function extension_index()
	local lines = {}
	for _, file in host.list(ext_dir) or {} do
		local name = file:match("^([%w_%-]+)%.luau$")
		if name then
			local source = host.read(ext_dir .. file) or ""
			table.insert(lines, ("- %s: %s"):format(name, source:match("^%-%-%s*([^\n]*)") or "(no description)"))
		end
	end
	return lines
end

-- Tools -----------------------------------------------------------------------------------------------------
--
-- Each tool is {name, description, parameters (a JSON schema), run(args) -> string}. A tool reports failure by
-- raising; the message goes back to the model. Add project tools to this list.

local scratch = setmetatable({}, { __index = _G }) -- globals of `lua` snippets live here, across calls

local function show(value)
	if type(value) == "table" then
		local ok, text = pcall(json.encode, value)
		if ok then
			return text
		end
	end
	return tostring(value)
end

M.tools = {
	{
		name = "bash",
		description = "Run a bash command in the project directory. Returns stdout and stderr together, then the "
			.. "exit status. Every call is a new shell. The command is killed after `timeout` seconds (default 120). "
			.. "A process left in the background must send its output to a file.",
		parameters = {
			type = "object",
			properties = {
				command = { type = "string" },
				timeout = { type = "number", description = "seconds" },
			},
			required = { "command" },
		},
		run = function(args)
			local result = host.sh(need(args, "command", "string"), { timeout = args.timeout })
			local status
			if result.timed_out then
				status = "timed out and was killed"
			elseif result.signal then
				status = "killed by signal " .. result.signal
			else
				status = "exit " .. result.code
			end
			local out = clip(result.out)
			if out ~= "" and out:sub(-1) ~= "\n" then
				out ..= "\n"
			end
			return out .. "[" .. status .. "]"
		end,
	},
	{
		name = "read",
		description = "Read a text file. Returns up to `limit` lines (default 2000) starting at line `offset` "
			.. "(default 1).",
		parameters = {
			type = "object",
			properties = {
				path = { type = "string" },
				offset = { type = "number" },
				limit = { type = "number" },
			},
			required = { "path" },
		},
		run = function(args)
			local text, err = host.read(need(args, "path", "string"))
			if not text then
				error(err, 0)
			end
			if text == "" then
				return "[empty file]"
			end
			local lines = text:split("\n")
			if text:sub(-1) == "\n" then
				table.remove(lines)
			end
			local first = math.max(1, args.offset or 1)
			local last = math.min(#lines, first + (args.limit or 2000) - 1)
			if first > #lines then
				error(("offset %d is past the end: the file has %d lines"):format(first, #lines), 0)
			end
			local body = clip(table.concat(lines, "\n", first, last))
			if first > 1 or last < #lines then
				body ..= ("\n[lines %d-%d of %d]"):format(first, last, #lines)
			end
			return body
		end,
	},
	{
		name = "write",
		description = "Create or overwrite a file with `content`. Missing directories are created.",
		parameters = {
			type = "object",
			properties = { path = { type = "string" }, content = { type = "string" } },
			required = { "path", "content" },
		},
		run = function(args)
			local path, content = need(args, "path", "string"), need(args, "content", "string")
			local ok, err = host.write(path, content)
			if not ok then
				error(err, 0)
			end
			return ("wrote %d bytes to %s"):format(#content, path)
		end,
	},
	{
		name = "edit",
		description = "Replace the exact text `old` with `new` in a file. `old` must occur exactly once, unless "
			.. "`all` is true, which replaces every occurrence.",
		parameters = {
			type = "object",
			properties = {
				path = { type = "string" },
				old = { type = "string" },
				new = { type = "string" },
				all = { type = "boolean" },
			},
			required = { "path", "old", "new" },
		},
		run = function(args)
			local path, old, new = need(args, "path", "string"), need(args, "old", "string"), need(args, "new", "string")
			if old == "" then
				error("old must not be empty", 0)
			end
			local text, err = host.read(path)
			if not text then
				error(err, 0)
			end
			local pieces, count, at = {}, 0, 1
			while true do
				local from, to = text:find(old, at, true)
				if not from then
					break
				end
				table.insert(pieces, text:sub(at, from - 1))
				table.insert(pieces, new)
				count += 1
				at = to + 1
			end
			if count == 0 then
				error("old text not found in " .. path, 0)
			end
			if count > 1 and not args.all then
				error(("old text occurs %d times in %s: add surrounding lines to make it unique, or pass all"):format(count, path), 0)
			end
			table.insert(pieces, text:sub(at))
			local ok, write_err = host.write(path, table.concat(pieces))
			if not ok then
				error(write_err, 0)
			end
			return ("replaced %d occurrence%s in %s"):format(count, count == 1 and "" or "s", path)
		end,
	},
	{
		name = "lua",
		description = "Run Luau code inside the harness. Returns what the code prints and returns. Globals persist "
			.. "between calls. Available: ext(name) loads the extension .hxm/ext/NAME.luau, host.sh / host.read / "
			.. "host.write / host.list, json.encode / json.decode.",
		parameters = {
			type = "object",
			properties = { code = { type = "string" } },
			required = { "code" },
		},
		run = function(args)
			local lines = {}
			scratch.ext = M.ext
			scratch.print = function(...)
				local parts = table.pack(...)
				for i = 1, parts.n do
					parts[i] = tostring(parts[i])
				end
				table.insert(lines, table.concat(parts, "\t"))
			end
			local chunk, err = host.load(need(args, "code", "string"), "=lua", scratch)
			if not chunk then
				error(err, 0)
			end
			local results = table.pack(host.timed(M.lua_seconds, chunk))
			if not results[1] then
				error(results[2], 0)
			end
			for i = 2, results.n do
				table.insert(lines, show(results[i]))
			end
			return #lines > 0 and clip(table.concat(lines, "\n")) or "[no output]"
		end,
	},
}

local function find_tool(name)
	for _, tool in M.tools do
		if tool.name == name then
			return tool
		end
	end
	return nil
end

-- Runs one tool call from the model and returns the text of its result. Nothing a tool does can end the run,
-- except a stop signal.
local function run_tool(call)
	local fn = call["function"] or {}
	local tool = find_tool(fn.name)
	if not tool then
		return "error: there is no tool named " .. tostring(fn.name)
	end
	local args, err = json.decode(fn.arguments or "")
	if type(args) ~= "table" then
		return "error: the arguments are not a JSON object (" .. tostring(err or "cut off by the output limit?") .. ")"
	end
	local what = tostring(args.command or args.path or args.code or ""):sub(1, 100):gsub("\n", " ")
	host.log(("  %s %s"):format(tool.name, what))
	local ok, result = pcall(tool.run, args)
	if ok then
		return result
	end
	if is_stop(result) then
		error(result, 0)
	end
	return "error: " .. tostring(result)
end

-- The model --------------------------------------------------------------------------------------------------
--
-- This speaks the OpenAI chat-completions format. To use another wire format, change request() and the two
-- places in turn() that read the reply.

local usage = { replies = 0, input = 0, cached = 0, output = 0 }

local function tool_specs()
	local specs = {}
	for _, tool in M.tools do
		table.insert(specs, {
			type = "function",
			["function"] = { name = tool.name, description = tool.description, parameters = tool.parameters },
		})
	end
	return specs
end

-- Sends the conversation and returns the decoded reply. Retries what is worth retrying, then raises.
local function request(messages)
	local body = json.encode({
		model = M.model,
		max_tokens = M.max_output,
		messages = messages,
		tools = tool_specs(),
	})
	local attempts = 5
	for attempt = 1, attempts do
		local status, text = host.llm(body)
		if status == 200 then
			local reply, err = json.decode(text)
			if type(reply) == "table" and reply.choices and reply.choices[1] and reply.choices[1].message then
				local counts = reply.usage or {}
				usage.replies += 1
				usage.input += counts.prompt_tokens or 0
				usage.output += counts.completion_tokens or 0
				usage.cached += (counts.prompt_tokens_details or {}).cached_tokens or 0
				return reply
			end
			error("the model endpoint sent a reply this loop cannot read: " .. (err or text:sub(1, 300)), 0)
		end
		if status == nil and is_stop(text) then
			error(text, 0)
		end
		local transient = status == nil or status == 408 or status == 409 or status == 429 or status >= 500
		if not transient or attempt == attempts then
			error(("the model request failed (%s): %s"):format(tostring(status or "no response"), tostring(text):sub(1, 500)), 0)
		end
		local wait = 2 ^ attempt
		host.log(("model request failed (%s), retrying in %d s"):format(tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

local function system_prompt()
	local parts = {
		("You are a coding agent working in %s through the tools you are given."):format(host.cwd),
		"The kernel confines you: you can write inside this project directory and $TMPDIR, and read those plus "
			.. "the system directories. \"Permission denied\" anywhere else is that wall. Say so and stop, rather "
			.. "than looking for a way around it.",
		"You can extend yourself. The harness running you is .hxm/loop.luau, and extensions are Luau modules in "
			.. ".hxm/ext/NAME.luau: the first line is a `--` comment saying what the module does, and it returns "
			.. "a table of functions. Call one from the lua tool as ext(\"NAME\").fn(...). Extensions stay with "
			.. "the project, so write one when you notice a chore you will meet again, and keep it as small as the chore.",
		"When the task is done, finish with a short report: what you changed, what you checked, what is left.",
	}
	local extensions = extension_index()
	if #extensions > 0 then
		table.insert(parts, "Extensions in this project:\n" .. table.concat(extensions, "\n"))
	end
	local notes = host.read(".hxm/prompt.md")
	if notes and notes ~= "" then
		table.insert(parts, "Notes from the project owner:\n" .. notes)
	end
	return table.concat(parts, "\n\n")
end

-- Replaces the conversation with the system prompt and the model's own summary of what happened.
local function compact(messages)
	host.log("  compacting the conversation")
	table.insert(messages, {
		role = "user",
		content = "The conversation is about to be replaced by a summary, and you will continue from that summary "
			.. "alone. Write it now, as plain text, without calling tools: the task, what is done, what you "
			.. "learned about this project that was costly to find out, and the exact next steps.",
	})
	local summary = request(messages).choices[1].message.content
	table.remove(messages)
	if type(summary) ~= "string" or summary == "" then
		host.log("  no summary came back; keeping the conversation as it is")
		return
	end
	local system = messages[1]
	table.clear(messages)
	table.insert(messages, system)
	table.insert(messages, {
		role = "user",
		content = "Earlier turns were replaced by this summary, written by you:\n\n" .. summary .. "\n\nContinue.",
	})
end

-- A model reply that asked for tools must be followed by one result per call. After an interrupt some are
-- missing; this adds them so the conversation stays valid.
local function repair(messages)
	for i = #messages, 1, -1 do
		local message = messages[i]
		if message.role == "assistant" then
			local answered = {}
			for j = i + 1, #messages do
				if messages[j].tool_call_id then
					answered[messages[j].tool_call_id] = true
				end
			end
			for _, call in message.tool_calls or {} do
				if not answered[call.id] then
					table.insert(messages, { role = "tool", tool_call_id = call.id, content = "error: interrupted by the user" })
				end
			end
			return
		end
	end
end

-- Runs the model until it answers without asking for a tool.
local function turn(messages)
	for _ = 1, M.max_replies do
		local reply = request(messages)
		local choice = reply.choices[1]
		local message = choice.message
		table.insert(messages, message) -- as received, so provider fields such as reasoning survive
		if type(message.content) == "string" and message.content ~= "" then
			print(message.content)
		end
		local calls = message.tool_calls or {}
		for _, call in calls do
			table.insert(messages, { role = "tool", tool_call_id = call.id, content = run_tool(call) })
		end
		if #calls == 0 then
			if choice.finish_reason ~= "length" then
				return
			end
			table.insert(messages, { role = "user", content = "Your reply was cut off by the output limit. Continue." })
		elseif ((reply.usage or {}).prompt_tokens or 0) > M.compact_at then
			compact(messages)
		end
	end
	error(("stopped after %d model replies without a final answer"):format(M.max_replies), 0)
end

-- With a .hxm directory, each session's conversation is kept in .hxm/log/ as JSON.
local log_path = host.list(".hxm") and (".hxm/log/%s.json"):format(os.date("%Y%m%d-%H%M%S")) or nil
local function save(messages)
	if log_path then
		host.write(log_path, json.encode(messages))
	end
end

local function report()
	host.log(("[%d replies, %d tokens in (%d cached), %d out]"):format(usage.replies, usage.input, usage.cached, usage.output))
end

function M.main()
	local messages = { { role = "system", content = system_prompt() } }
	local task = table.concat(host.args, " ")

	if not host.interactive then
		if task == "" then
			local lines = {}
			for line in host.input do
				table.insert(lines, line)
			end
			task = table.concat(lines, "\n")
		end
		if task:match("^%s*$") then
			error("no task: pass it as arguments or on stdin", 0)
		end
		table.insert(messages, { role = "user", content = task })
		local ok, err = pcall(turn, messages)
		save(messages)
		report()
		if not ok then
			error(err, 0)
		end
		return 0
	end

	host.log(("hxm: %s in %s. Ctrl-C stops a turn, Ctrl-D leaves."):format(M.model, host.cwd))
	while true do
		local line = host.input("> ")
		if not line then
			break
		end
		if not line:match("^%s*$") then
			table.insert(messages, { role = "user", content = line })
			local ok, err = pcall(turn, messages)
			if not ok then
				if err ~= "interrupted" then
					save(messages)
					error(err, 0)
				end
				repair(messages)
				host.log("[interrupted]")
			end
			save(messages)
		end
	end
	report()
	return 0
end

return M
