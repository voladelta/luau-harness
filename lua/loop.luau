-- The hx loop, built into hx. A project changes it in .hx/loop.luau, which loads this module, replaces some of
-- its fields and returns it (`hx --init` writes such a file), or owns all of it as a copy (`hx --init --full`). The
-- parts meant to be replaced are fields of M and are always called through M.
--
-- The host supplies a small kernel and nothing else:
--   host.llm(body) -> status, text | nil, error      POST to the configured endpoint; the host adds the API key
--   host.sh(command, {timeout=}) -> {out=, code=, signal=, timed_out=}
--   host.read(path), host.write(path, data), host.list(dir)
--   host.load(source, name, env) -> function | nil, error
--   host.try(fn, ...) -> true, ... | false, error     a pcall that is also where a stop ends
--   host.timed(seconds, fn, ...) -> true, ... | false, error     host.try with a time limit
--   host.input(prompt), host.log(text), host.sleep(seconds), host.env(name)
--   host.args, host.cwd, host.interactive, host.quiet, host.sandboxed, host.default_loop
--   host.tty                                          stdout is a terminal
--   json.encode, json.decode, json.array, json.null
-- Ctrl-C raises "interrupted" and SIGTERM "terminated", again at every step, until the outermost host.try or
-- host.timed returns; a plain pcall cannot swallow them.
-- The whole process sits inside one Landlock ruleset, so nothing in this file can reach more of the machine
-- than the model's own shell can.

local M = {}

M.model = host.env("HX_MODEL") or "deepseek-flash"
M.max_output = 16000 -- tokens per model reply
M.max_replies = 200 -- model replies per task before giving up
M.compact_at = 150000 -- prompt tokens at which the conversation is replaced by a summary of itself
M.output_limit = 30000 -- bytes of a tool result shown to the model
M.lua_seconds = 60 -- time limit of one `lua` tool call
M.effort = host.env("HX_EFFORT") -- how hard the model thinks, as output_config.effort; nil leaves it to the endpoint
M.repeat_reminders = { 3, 5, 8 } -- the same call made this many times in a row gets a reminder
M.empty_retries = 2 -- a reply with nothing in it is asked for again this many times

-- A progress line on stderr: what the loop is doing, left out with --quiet. Warnings and errors use host.log.
local function progress(text)
	if not host.quiet then
		host.log(text)
	end
end

-- Keeps the start and the end of a long text: the start says what ran, the end says how it went. The whole text
-- goes to a file in $TMPDIR, so what was left out can still be read or searched.
local spills = 0
local function clip(text)
	if #text <= M.output_limit then
		return text
	end
	local half = M.output_limit // 2
	local where = ""
	local tmp = host.env("TMPDIR")
	if tmp then
		spills += 1
		local path = ("%s/hx-output-%d.txt"):format(tmp, spills)
		if host.write(path, text) then
			where = "; the whole output is in " .. path
		end
	end
	return text:sub(1, half) .. ("\n[... %d bytes omitted%s ...]\n"):format(#text - 2 * half, where) .. text:sub(-half)
end

-- Removes control characters but newline and tab. Escape sequences in text from the model would otherwise
-- reach the terminal, which they can retitle, scramble or (OSC 52) use to write the clipboard.
function M.printable(text)
	return (text:gsub("[\0-\8\11-\31\127]", ""))
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
-- An extension is a Luau module in .hx/ext/NAME.luau that returns a table of functions. The model writes them
-- with the ordinary write tool and calls them from the `lua` tool, so the tool list never changes. A module is
-- loaded again whenever its file has changed.

local ext_dir = ".hx/ext/"
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
-- Each tool is {name, description, parameters (a JSON schema), run(args) -> string}, and may have `aliases`, a
-- map from an argument name models often send to the tool's own name for it. A tool reports failure by raising;
-- the message goes back to the model. Add project tools to this list.

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
			.. "A process left in the background must send its output to a file, and is killed when this session "
			.. "ends, daemons included: nothing you start outlives it.",
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
		description = "Read a text file. Returns whole lines from line `offset` (default 1), up to `limit` lines "
			.. "(default 2000) or the output limit, each as `NUMBER: text`; the numbers are not part of the file. "
			.. "When that is not the whole file, it ends with the range it returned and the offset to go on from.",
		parameters = {
			type = "object",
			properties = {
				file_path = { type = "string" },
				offset = { type = "number", description = "first line, from 1" },
				limit = { type = "number", description = "most lines to return" },
			},
			required = { "file_path" },
		},
		aliases = { path = "file_path" },
		run = function(args)
			local text, err = host.read(need(args, "file_path", "string"))
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
			-- Whole lines up to the byte limit, never head and tail, so the range says exactly what was left out.
			local function numbered(i)
				return ("%d: %s"):format(i, lines[i])
			end
			local shown = { numbered(first) }
			local size = #shown[1]
			for i = first + 1, last do
				local line = numbered(i)
				size += 1 + #line
				if size > M.output_limit then
					last = i - 1
					break
				end
				table.insert(shown, line)
			end
			local body
			if #lines[first] > M.output_limit then
				-- A line longer than the limit still counts as read, so the next offset moves past it.
				last = first
				body = ("%d: %s"):format(first, lines[first]:sub(1, M.output_limit))
					.. ("\n[line %d is %d bytes; only the first %d are shown]"):format(first, #lines[first], M.output_limit)
			else
				body = table.concat(shown, "\n")
			end
			if last < #lines then
				body ..= ("\n[lines %d-%d of %d; offset=%d goes on]"):format(first, last, #lines, last + 1)
			elseif first > 1 then
				body ..= ("\n[lines %d-%d of %d]"):format(first, last, #lines)
			end
			return body
		end,
	},
	{
		name = "write",
		description = "Create or overwrite a file with `content`. Missing directories are created. For a change to "
			.. "part of an existing file, edit is cheaper and safer.",
		parameters = {
			type = "object",
			properties = { file_path = { type = "string" }, content = { type = "string" } },
			required = { "file_path", "content" },
		},
		aliases = { path = "file_path" },
		run = function(args)
			local path, content = need(args, "file_path", "string"), need(args, "content", "string")
			local ok, err = host.write(path, content)
			if not ok then
				error(err, 0)
			end
			return ("wrote %d bytes to %s"):format(#content, path)
		end,
	},
	{
		name = "edit",
		description = "Replace the exact text `old_string` with `new_string` in a file. `old_string` must occur "
			.. "exactly once, unless `replace_all` is true, which replaces every occurrence. Copy it from the file "
			.. "without the line numbers read puts in front.",
		parameters = {
			type = "object",
			properties = {
				file_path = { type = "string" },
				old_string = { type = "string" },
				new_string = { type = "string" },
				replace_all = { type = "boolean" },
			},
			required = { "file_path", "old_string", "new_string" },
		},
		aliases = { path = "file_path" },
		run = function(args)
			local path, old, new = need(args, "file_path", "string"), need(args, "old_string", "string"),
				need(args, "new_string", "string")
			if old == "" then
				error("old_string must not be empty", 0)
			end
			if old == new then
				error("old_string and new_string are the same, so this edit would change nothing", 0)
			end
			local text, err = host.read(path)
			if not text then
				error(err, 0)
			end
			local function replace(from_text, to_text)
				local pieces, count, at = {}, 0, 1
				while true do
					local from, to = text:find(from_text, at, true)
					if not from then
						break
					end
					table.insert(pieces, text:sub(at, from - 1))
					table.insert(pieces, to_text)
					count += 1
					at = to + 1
				end
				table.insert(pieces, text:sub(at))
				return pieces, count
			end
			local pieces, count = replace(old, new)
			-- Models write line ends as \n. In a file with \r\n ends, the same text is found with them.
			if count == 0 and text:find("\r\n", 1, true) and old:find("\n", 1, true) then
				local function crlf(s)
					return (s:gsub("\r?\n", "\r\n"))
				end
				pieces, count = replace(crlf(old), crlf(new))
			end
			if count == 0 then
				local hint = old:match("^%s*%d+: ") and " without the line numbers read adds" or ""
				error(("old text not found in %s: read the file again and copy the text exactly%s"):format(path, hint), 0)
			end
			if count > 1 and not args.replace_all then
				error(("old text occurs %d times in %s: add surrounding lines to make it unique, or pass replace_all"):format(count, path), 0)
			end
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
			.. "between calls. Available: ext(name) loads the extension .hx/ext/NAME.luau, host.sh / host.read / "
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

-- Checks a call's argument names against the tool's own schema. The message names what was sent and what the
-- tool takes, so a model that reached for another harness's names can see its mistake. Returns nil or it. A
-- schema without `properties` names no arguments, so then any name is accepted.
local function argument_problem(tool, args)
	local schema = tool.parameters or {}
	local properties, required = schema.properties, schema.required or {}
	local takes, optional = table.clone(required), {}
	for name in properties or {} do
		if not table.find(required, name) then
			table.insert(optional, name)
		end
	end
	table.sort(optional)
	table.move(optional, 1, #optional, #takes + 1, takes)
	local listing = ("%s takes %s"):format(tool.name, table.concat(takes, ", "))
	for name in args do
		if properties and properties[name] == nil then
			return ("unknown argument %q: %s"):format(tostring(name), listing)
		end
	end
	for _, name in required do
		if args[name] == nil then
			return ("missing argument %q: %s"):format(name, listing)
		end
	end
	return nil
end

local function find_tool(name)
	for _, tool in M.tools do
		if tool.name == name then
			return tool
		end
	end
	return nil
end

-- Runs one tool_use block from the model. Returns the text of the result and whether it is an error. Nothing a
-- tool does can end the run; a stop does, because the host raises it again as soon as this returns.
local function run_tool(call)
	local tool = find_tool(call.name)
	if not tool then
		return "error: there is no tool named " .. tostring(call.name), true
	end
	if type(call.input) ~= "table" then
		return "error: the tool input is not an object", true
	end
	-- A copy, so that renaming an alias leaves the call in the history as the model sent it.
	local args = table.clone(call.input)
	for alias, name in tool.aliases or {} do
		if args[alias] ~= nil and args[name] == nil then
			args[name], args[alias] = args[alias], nil
		end
	end
	local what = M.printable(tostring(args.command or args.file_path or args.code or ""):sub(1, 100):gsub("\n", " "))
	progress(("  %s %s"):format(tool.name, what))
	local problem = argument_problem(tool, args)
	if problem then
		return "error: " .. problem, true
	end
	local ok, result = pcall(tool.run, args)
	if ok then
		return result, false
	end
	return "error: " .. tostring(result), true
end

-- The model --------------------------------------------------------------------------------------------------
--
-- This speaks the Anthropic Messages format: a `system` string, messages whose content is a list of blocks
-- (text, thinking, tool_use, tool_result), and tools given as {name, description, input_schema}. To use another
-- wire format, replace M.request and M.turn.

local usage = { replies = 0, input = 0, cached = 0, output = 0 }

local function tool_specs()
	local specs = {}
	for _, tool in M.tools do
		table.insert(specs, { name = tool.name, description = tool.description, input_schema = tool.parameters })
	end
	return specs
end

-- Tokens the last request put in front of the model, cached or not.
local function prompt_size(reply)
	local counts = reply.usage or {}
	return (counts.input_tokens or 0) + (counts.cache_read_input_tokens or 0) + (counts.cache_creation_input_tokens or 0)
end

-- Sends the conversation and returns the decoded reply. Retries what is worth retrying, then raises.
function M.request(system, messages)
	local body = json.encode({
		model = M.model,
		max_tokens = M.max_output,
		thinking = { type = "adaptive" },
		output_config = M.effort and { effort = M.effort } or nil,
		cache_control = { type = "ephemeral" }, -- cache the longest prefix seen before, where caching is opt-in
		system = system,
		messages = messages,
		tools = tool_specs(),
	})
	local attempts = 5
	for attempt = 1, attempts do
		local status, text = host.llm(body)
		if status == 200 then
			local reply, err = json.decode(text)
			if type(reply) == "table" and type(reply.content) == "table" then
				usage.replies += 1
				usage.input += prompt_size(reply)
				usage.cached += (reply.usage or {}).cache_read_input_tokens or 0
				usage.output += (reply.usage or {}).output_tokens or 0
				return reply
			end
			error("the model endpoint sent a reply this loop cannot read: " .. (err or text:sub(1, 300)), 0)
		end
		local transient = status == nil or status == 408 or status == 409 or status == 429 or status >= 500
		if not transient or attempt == attempts then
			error(("the model request failed (%s): %s"):format(tostring(status or "no response"), tostring(text):sub(1, 500)), 0)
		end
		local wait = 2 ^ attempt
		progress(("model request failed (%s), retrying in %d s"):format(tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

function M.system_prompt()
	local parts = {
		("You are a coding agent working in %s through the tools you are given."):format(host.cwd),
		host.sandboxed
				and "The kernel confines you: you can write inside this project directory and $TMPDIR, and read those "
					.. "plus the system directories. \"Permission denied\" anywhere else is that wall. Do not refuse "
					.. "in advance: try what the task needs, and only when the wall denies it, say so and stop "
					.. "rather than looking for a way around it."
			or "Nothing confines you in this session: you act with the user's full access to this machine. Stay "
				.. "inside this project directory unless the task says otherwise.",
		"You can extend yourself. You run on the loop built into hx; .hx/loop.luau, if the project has one, starts "
			.. "from it and changes parts. The lua tool can read the built-in source as host.default_loop. Extensions "
			.. "are Luau modules in .hx/ext/NAME.luau: the first line is a `--` comment saying what the module does, "
			.. "and it returns "
			.. "a table of functions. Call one from the lua tool as ext(\"NAME\").fn(...). Extensions stay with "
			.. "the project, so write one when you notice a chore you will meet again, and keep it as small as the chore.",
		"Use the read tool, not shell commands like cat, sed, head or tail, to look at text files; its offset and "
			.. "limit read a range of lines or go on through a large file. Read a file before you edit it, unless "
			.. "you wrote or edited it yourself in this session. Check the [exit N] status at the end of every bash "
			.. "result, and look into a failure before moving on.",
		"When the task is done, finish with a short report: what you changed, what you checked, what is left.",
	}
	local extensions = extension_index()
	if #extensions > 0 then
		table.insert(parts, "Extensions in this project:\n" .. table.concat(extensions, "\n"))
	end
	local notes = host.read(".hx/prompt.md")
	if notes and notes ~= "" then
		table.insert(parts, "Notes from the project owner:\n" .. notes)
	end
	return table.concat(parts, "\n\n")
end

-- The conversation -------------------------------------------------------------------------------------------
--
-- The Messages API accepts one shape of history only: it starts with the user, roles alternate, and a reply
-- that calls tools is answered by the very next message with exactly one result per call, results first. So a
-- conversation is always in one of three states, read off its last message, and each operation is legal in
-- some of them only:
--
--   state     the last message is                 legal operations
--   "idle"    nothing, or a finished reply        say
--   "ready"   the user's: the model may run       say (adds to that message), reply
--   "owing"   a reply that called tools           answer, abandon
--
--   say:    idle -> ready, ready -> ready         reply:   ready -> owing, idle, or ready (an empty reply)
--   answer: owing -> ready                        abandon: owing -> ready
--
-- Every change to the history goes through these functions. An operation that is illegal in the current state
-- raises and changes nothing, so a history the API would reject cannot be built.

local Conversation = {}
Conversation.__index = Conversation
M.Conversation = Conversation

function Conversation.new()
	return setmetatable({ messages = {} }, Conversation)
end

local function tool_calls(message)
	local calls = {}
	for _, block in message.content do
		if block.type == "tool_use" then
			table.insert(calls, block)
		end
	end
	return calls
end

function Conversation:state()
	local last = self.messages[#self.messages]
	if not last then
		return "idle"
	elseif last.role == "user" then
		return "ready"
	end
	return #tool_calls(last) > 0 and "owing" or "idle"
end

local function expect(self, operation, ...)
	local state = self:state()
	for _, allowed in { ... } do
		if state == allowed then
			return
		end
	end
	error(("conversation: cannot %s while %s"):format(operation, state), 3)
end

-- Adds text from the user. After tool results or earlier text it joins that message, behind what is there.
function Conversation:say(text)
	expect(self, "say", "idle", "ready")
	local last = self.messages[#self.messages]
	if not last or last.role ~= "user" then
		table.insert(self.messages, { role = "user", content = text })
		return
	end
	if type(last.content) == "string" then
		last.content = { { type = "text", text = last.content } }
	end
	table.insert(last.content, { type = "text", text = text })
end

-- Records the model's reply exactly as it came (thinking blocks have to go back unchanged) and returns its
-- tool calls. An empty reply records nothing: the user's message is still the last word.
function Conversation:reply(content)
	expect(self, "reply", "ready")
	if #content == 0 then
		return {}
	end
	local message = { role = "assistant", content = content }
	table.insert(self.messages, message)
	return tool_calls(message)
end

-- Answers every tool call of the last reply with what run(call) returns: the result text, and whether it is an
-- error. If run raises, nothing is recorded and the calls are still owed.
function Conversation:answer(run)
	expect(self, "answer", "owing")
	local results = {}
	for _, call in tool_calls(self.messages[#self.messages]) do
		local text, failed = run(call)
		table.insert(results, { type = "tool_result", tool_use_id = call.id, content = text, is_error = failed or nil })
	end
	table.insert(self.messages, { role = "user", content = results })
end

-- Answers whatever is owed with an error, so a turn stopped half-way leaves a history that can continue.
function Conversation:abandon(reason)
	if self:state() == "owing" then
		self:answer(function()
			return "error: " .. reason, true
		end)
	end
end

-- Forgets everything and starts again from one user message.
function Conversation:restart(text)
	expect(self, "restart", "idle", "ready")
	table.clear(self.messages)
	self:say(text)
end

-- A copy to try something on. It shares the old messages; the last one, which say() may extend, is its own.
function Conversation:fork()
	local copy = Conversation.new()
	copy.messages = table.clone(self.messages)
	local last = copy.messages[#copy.messages]
	if last and type(last.content) == "table" then
		copy.messages[#copy.messages] = { role = last.role, content = table.clone(last.content) }
	end
	return copy
end

-- Checks the whole history against the rules above. Returns nil, or what is wrong. The functions above keep it
-- nil; this is the independent check, run before every request and by the tests.
function Conversation:problem()
	local owed = nil -- the calls the next message has to answer
	for i, message in self.messages do
		local expected = i % 2 == 1 and "user" or "assistant"
		if message.role ~= expected then
			return ("message %d is from the %s, where the %s should speak"):format(i, tostring(message.role), expected)
		end
		if message.role == "assistant" then
			if type(message.content) ~= "table" or #message.content == 0 then
				return ("message %d is an empty reply"):format(i)
			end
			owed = tool_calls(message)
		else
			local answered = 0
			for j, block in type(message.content) == "table" and message.content or {} do
				if block.type == "tool_result" then
					answered += 1
					if answered ~= j then
						return ("message %d has a tool result behind other content"):format(i)
					end
					if not owed or not owed[answered] or owed[answered].id ~= block.tool_use_id then
						return ("message %d answers a tool call that was not made"):format(i)
					end
				end
			end
			if owed and answered ~= #owed then
				return ("message %d leaves a tool call unanswered"):format(i)
			end
			owed = nil
		end
	end
	return nil
end

local function text_of(reply)
	local parts = {}
	for _, block in reply.content do
		if block.type == "text" then
			table.insert(parts, block.text)
		end
	end
	return table.concat(parts, "\n")
end

-- Sends the conversation to the model. Only a conversation that is ready, and sound, is ever sent.
local function ask(system, talk)
	local problem = talk:state() ~= "ready" and "it is " .. talk:state() .. ", not ready" or talk:problem()
	if problem then
		error("this conversation cannot be sent: " .. problem, 0)
	end
	return M.request(system, talk.messages)
end

-- Replaces the conversation with the model's own summary of what happened.
function M.compact(system, talk)
	progress("  compacting the conversation")
	local asking = talk:fork()
	asking:say(M.compact_prompt)
	local summary = text_of(ask(system, asking))
	if summary == "" then
		progress("  no summary came back; keeping the conversation as it is")
		return
	end
	talk:restart(
		"Earlier turns were replaced by this checkpoint, written by you:\n\n"
			.. summary
			.. "\n\nTreat it as established and continue the work from it directly, without restating or "
			.. "acknowledging it."
	)
end

-- The request for the summary. It goes behind the whole conversation, as the last user message, so the request
-- reuses the cached prefix. The summary is in English whatever the conversation's language: it becomes the start
-- of everything after it, and a summary in another language pulls the model's own reasoning into that language.
M.compact_prompt = table.concat({
	"The conversation is about to be replaced by a summary, and you will continue from that summary alone. "
		.. "Write it now as a checkpoint, without calling tools, in concise English, with exactly these sections "
		.. "in this order. Use terse bullets; write (none) for an empty section rather than leaving it out.",
	"",
	"## Task",
	"- what the user asked for, and how that changed; quote the user where the exact words matter",
	"## Files",
	"- exact path: why it matters, what was changed",
	"## Errors and fixes",
	"- the error, and how it was fixed",
	"## Learned",
	"- facts about this project that were costly to find out: commands, conventions, traps",
	"## Done",
	"- what is finished and checked",
	"## Current work",
	"- what was in progress just now",
	"## Next step",
	"- the single next action",
	"",
	"Keep exact file paths, commands, error text, identifiers and numbers. Keep the user's corrections and "
		.. "instructions. If the conversation starts with an earlier checkpoint, merge it: keep what is still "
		.. "true, drop what is not, and do not copy it over unchanged.",
}, "\n")

-- A JSON text of a value with the keys of every object in order, so that two equal calls give the same text.
local function canonical(value)
	if type(value) ~= "table" then
		return json.encode(value)
	end
	if #value > 0 or next(value) == nil then
		local items = {}
		for _, item in value do
			table.insert(items, canonical(item))
		end
		return "[" .. table.concat(items, ",") .. "]"
	end
	local keys = {}
	for key in value do
		table.insert(keys, tostring(key))
	end
	table.sort(keys)
	local members = {}
	for _, key in keys do
		table.insert(members, json.encode(key) .. ":" .. canonical(value[key]))
	end
	return "{" .. table.concat(members, ",") .. "}"
end

-- What the model is told when it makes the same call again and again.
function M.repeat_reminder(call, count, first)
	if first then
		return "You are repeating the exact same tool call with the same arguments. Look at what it returned "
			.. "before calling it again: if the task is not done, try a different approach or different arguments."
	end
	local args = canonical(call.input)
	if #args > 500 then
		args = args:sub(1, 500) .. ("... (%d more bytes)"):format(#args - 500)
	end
	return ("You have made the same %s call %d times in a row, with these arguments: %s\nThe calls are not "
		.. "making progress. Do not make this call again. Look at the last result and choose a different action "
		.. "or different arguments, or finish the task if you have what you need."):format(call.name, count, args)
end

-- Whether a reply has anything in it. Thinking alone counts; an empty text block does not.
local function has_content(content)
	for _, block in content do
		if block.type ~= "text" or block.text ~= "" then
			return true
		end
	end
	return false
end

-- Prints the text of a reply: the model's words are the answer, on stdout.
local function show_text(reply)
	for _, block in reply.content do
		if block.type == "text" and block.text ~= "" then
			print(host.tty and M.printable(block.text) or block.text)
		end
	end
end

-- Runs the model until it answers without asking for a tool.
function M.turn(system, talk)
	local last_call, repeats = nil, 0 -- the run of identical calls, which ends with this turn
	for _ = 1, M.max_replies do
		local reply = ask(system, talk)
		for _ = 1, M.empty_retries do
			if has_content(reply.content) or reply.stop_reason == "max_tokens" then
				break
			end
			progress("  the model sent an empty reply; asking again")
			reply = ask(system, talk)
		end
		local cut_off = reply.stop_reason == "max_tokens"
		show_text(reply)
		-- A call whose input is not an object is kept as an empty one: the endpoint refuses a history that has
		-- one, and would refuse every request after it. The model still hears that the call was malformed.
		local malformed = {}
		for _, block in reply.content do
			if block.type == "tool_use" and type(block.input) ~= "table" then
				malformed[block.id] = true
				block.input = {}
			end
		end
		local calls = talk:reply(reply.content)
		if #calls > 0 then
			local reminder = nil
			talk:answer(function(call)
				if cut_off then
					return "error: this call was cut off by the output limit and did not run. Send it again, smaller.", true
				end
				if malformed[call.id] then
					return "error: the tool input is not an object", true
				end
				local key = call.name .. canonical(call.input)
				repeats = key == last_call and repeats + 1 or 1
				last_call = key
				local at = table.find(M.repeat_reminders, repeats)
				if at then
					reminder = M.repeat_reminder(call, repeats, at == 1)
				end
				return run_tool(call)
			end)
			if reminder then
				talk:say(reminder)
			end
			if prompt_size(reply) > M.compact_at then
				M.compact(system, talk)
			end
		elseif cut_off then
			talk:say("Your reply was cut off by the output limit. Continue.")
		else
			if reply.stop_reason == "refusal" then
				host.log("[the model declined to answer]")
			end
			return
		end
	end
	-- Out of replies. The model gets one more, to say where the work stands, rather than none.
	talk:say(("You have used all %d replies this task allows. Do not call any more tools. Write your final "
		.. "report now: what you changed, what you checked, what is left."):format(M.max_replies))
	local reply = ask(system, talk)
	show_text(reply)
	if #talk:reply(reply.content) > 0 then
		talk:abandon("the reply limit was reached")
	end
	error(("stopped after %d model replies without a final answer"):format(M.max_replies), 0)
end

-- With a .hx directory, each session's conversation is kept in .hx/log/ as JSON.
local log_path = host.list(".hx") and (".hx/log/%s.json"):format(os.date("%Y%m%d-%H%M%S")) or nil
local function save(system, messages)
	if log_path then
		-- Logs hold file contents, so the directory keeps itself out of git, however .hx/ came to exist.
		if not host.read(".hx/log/.gitignore") then
			host.write(".hx/log/.gitignore", "*\n")
		end
		host.write(log_path, json.encode({ system = system, messages = messages }))
	end
end

local function report()
	progress(("[%d replies, %d tokens in (%d cached), %d out]"):format(usage.replies, usage.input, usage.cached, usage.output))
end

-- Bracketed paste. While it is on, the terminal wraps pasted text in the two markers, so a pasted message of many
-- lines can be told from lines typed one by one. A terminal that does not know the sequences ignores them, and
-- its lines carry no markers. The terminal stays in line mode, so it echoes the markers as ^[[200~ and ^[[201~.
local PASTE_ON, PASTE_OFF = "\27[?2004h", "\27[?2004l"
local PASTE_START, PASTE_END = "\27[200~", "\27[201~"

-- Reads one message with read(), which returns a line or nil at the end of input. A line is a message, unless it
-- carries the start of a paste: then lines are read up to the one that carries its end, and the message is all
-- of them, joined with newlines, without the markers. Text typed before or after the markers belongs to it. A
-- paste cut short by the end of input is the message so far. Returns nil at the end of input.
function M.read_message(read)
	local line = read()
	if not line or not line:find(PASTE_START, 1, true) then
		return line
	end
	local lines = { line }
	while not line:find(PASTE_END, 1, true) do
		line = read()
		if not line then
			break
		end
		table.insert(lines, line)
	end
	return (table.concat(lines, "\n"):gsub("\27%[20[01]~", ""))
end

function M.main()
	local system = M.system_prompt()
	local talk = Conversation.new()
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
			error('no task: give it as arguments, as in hx "fix the tests", or on stdin', 0)
		end
		talk:say(task)
		local ok, err = host.try(M.turn, system, talk)
		save(system, talk.messages)
		report()
		if not ok then
			error(err, 0)
		end
		return 0
	end

	progress(("hx: %s in %s. Ctrl-C stops a turn, Ctrl-D leaves."):format(M.model, host.cwd))
	while true do
		-- Paste is turned on with every prompt, in case a command the model ran turned it off. Only the first
		-- line of a message gets a prompt. The read is its own host.try so that a stop there can turn paste off
		-- before it ends the run.
		local prompt = PASTE_ON .. "> "
		local read_ok, line = host.try(M.read_message, function()
			local text = host.input(prompt)
			prompt = ""
			return text
		end)
		if not read_ok then
			host.log(PASTE_OFF)
			error(line, 0)
		end
		if not line then
			break
		end
		if not line:match("^%s*$") then
			talk:say(line)
			local ok, err = host.try(M.turn, system, talk)
			if not ok then
				if err == "terminated" then
					save(system, talk.messages)
					host.log(PASTE_OFF)
					error(err, 0)
				end
				-- Any other failure ends the turn, not the session: what is owed is answered and the prompt comes back.
				if err == "interrupted" then
					talk:abandon("interrupted by the user")
					host.log("[interrupted]")
				else
					talk:abandon("the turn failed: " .. tostring(err))
					host.log("[the turn failed: " .. tostring(err) .. "]")
				end
			end
			save(system, talk.messages)
		end
	end
	host.log(PASTE_OFF)
	report()
	return 0
end

return M
