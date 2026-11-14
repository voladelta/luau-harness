-- The hx loop, built into hx. A project changes it in .hx/loop.luau, which loads this module, replaces some of
-- its fields and returns it (`hx --init` writes such a file), or owns all of it as a copy (`hx --init --full`). The
-- parts meant to be replaced are fields of M and are always called through M.
--
-- The host supplies a small kernel and nothing else:
--   host.llm(body, {timeout=, beta=}) -> status, text | nil, error     POST to the configured endpoint; the host
--                                                     adds the API key, and beta as the anthropic-beta header
--   host.sh(command, {timeout=}) -> {out=, code=, signal=, timed_out=}
--   host.read(path), host.write(path, data), host.list(dir)
--   host.load(source, name, env) -> function | nil, error
--   host.try(fn, ...) -> true, ... | false, error     a pcall that is also where a stop ends
--   host.timed(seconds, fn, ...) -> true, ... | false, error     host.try with a time limit
--   host.input(prompt), host.log(text), host.sleep(seconds), host.env(name)
--   host.args, host.cwd, host.interactive, host.quiet, host.sandboxed, host.default_loop
--   host.tty                                          stdout is a terminal
--   host.tool(name, request, {timeout=}) -> status, text | nil, error     DeepSeek's search, fetch or ocr endpoint;
--                                                     the host adds DEEPSEEK_API_KEY
--   host.tools                                        that key is there
--   host.extensions                                   the built-in extensions' sources, by name
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
M.call_seconds = 600 -- time limit of one `lua` tool call, commands and all
M.effort = host.env("HX_EFFORT") -- low, medium, high, xhigh or max: how hard the model thinks; nil leaves it to
-- the model's entry in M.claude, or else to the endpoint
M.repeat_reminders = { 3, 5, 8 } -- a function call with the same result this many times in a task gets a reminder

-- A progress line on stderr: what the loop is doing, left out with --quiet. Warnings and errors use host.log.
local function progress(text)
	if not host.quiet then
		host.log(text)
	end
end

-- Keeps the start and the end of a long result: the start says what ran, the end says how it went. Only the result
-- the model is sent is clipped, once; the functions return everything, so code can work on all of it. The whole text
-- goes to a file in $TMPDIR (hx makes one for every run, inside the wall), so what was left out can still be read
-- or searched.
local spills = 0
local function clip(text)
	if #text <= M.output_limit then
		return text
	end
	local half = M.output_limit // 2
	spills += 1
	local path = ("%s/hx-output-%d.txt"):format(host.env("TMPDIR"), spills)
	local where = host.write(path, text) and "; the whole output is in " .. path or ""
	return text:sub(1, half) .. ("\n[... %d bytes omitted%s ...]\n"):format(#text - 2 * half, where) .. text:sub(-half)
end

-- Removes control characters but newline and tab. Escape sequences in text from the model would otherwise
-- reach the terminal, which they can retitle, scramble or (OSC 52) use to write the clipboard.
function M.printable(text)
	return (text:gsub("[\0-\8\11-\31\127]", ""))
end

-- Extensions ------------------------------------------------------------------------------------------------
--
-- An extension is a Luau module in .hx/ext/NAME.luau that returns a table of functions. The model writes them
-- with write() and calls them from its code, so the tool never changes. A module is loaded again whenever its
-- file has changed. Extensions see the same functions the model's code does. hx has some built in
-- (host.extensions: web and ocr); a project's file of the same name takes the place of one.

local ext_dir = ".hx/ext/"
local ext_cache = {}
-- What code from the model and from extensions sees: the functions of M.functions (looked up when used, so a
-- project's changes to the list count at once), ext, and the Luau globals.
--
-- Every function call is counted by what it was asked and what it returned. A call that returns what it returned
-- before is going nowhere, whatever ran in between and whether the model repeats it across tool calls or in a loop
-- inside one: that is the unit the repeat reminder counts, not the tool call, whose code may print a counter or a
-- clock and so never repeat exactly. A command rerun after a fix returns something new, so it does not count.
local repeats = { counts = {}, worst = nil } -- this turn's counts, and the call repeated most

-- A value as text: JSON where it has a JSON form, and nil as null.
local function as_text(value)
	local ok, text = pcall(json.encode, value == nil and json.null or value)
	return ok and text or tostring(value)
end

local function counted(fn)
	return function(...)
		local result = fn.run(...)
		local args = {}
		for i = 1, select("#", ...) do
			args[i] = as_text((select(i, ...)))
		end
		local call = ("%s(%s)"):format(fn.name, table.concat(args, ", "))
		local key = call .. "\0" .. as_text(result)
		local count = (repeats.counts[key] or 0) + 1
		repeats.counts[key] = count
		if not repeats.worst or count > repeats.worst.count then
			repeats.worst = { call = call, count = count }
		end
		return result
	end
end

local function lookup(_, name)
	if name == "ext" then
		return M.ext
	end
	for _, fn in M.functions do
		if fn.name == name then
			return counted(fn)
		end
	end
	return _G[name]
end
local ext_env = setmetatable({}, { __index = lookup })

function M.ext(name)
	if type(name) ~= "string" or not name:match("^[%w_%-]+$") then
		error("an extension name is letters, digits, _ and -", 0)
	end
	local source, err = host.read(ext_dir .. name .. ".luau")
	local built_in = not source and host.extensions[name]
	if built_in then
		source = built_in
	elseif not source then
		error("no extension " .. name .. ": " .. err, 0)
	end
	local cached = ext_cache[name]
	if cached and cached.source == source then
		return cached.value
	end
	local chunk, load_err = host.load(source, (built_in and "=hx/ext/" or "=ext/") .. name, ext_env)
	if not chunk then
		error(load_err, 0)
	end
	local value = chunk()
	ext_cache[name] = { source = source, value = value }
	return value
end

-- One line per extension: its name and the comment on its first line. The built-in ones are listed when the
-- DeepSeek key they need is there, unless the project has its own of that name.
local function extension_index()
	local lines, own = {}, {}
	local function add(name, source, mark)
		table.insert(lines, ("- %s%s: %s"):format(name, mark, source:match("^%-%-%s*([^\n]*)") or "(no description)"))
	end
	for _, file in host.list(ext_dir) or {} do
		local name = file:match("^([%w_%-]+)%.luau$")
		if name then
			own[name] = true
			add(name, host.read(ext_dir .. file) or "", "")
		end
	end
	if host.tools then
		local names = {}
		for name in host.extensions do
			table.insert(names, name)
		end
		table.sort(names)
		for _, name in names do
			if not own[name] then
				add(name, host.extensions[name], " (built in)")
			end
		end
	end
	return lines
end

-- Functions -------------------------------------------------------------------------------------------------
--
-- The model has one tool, `lua`, and acts by calling functions from the code it sends. Each function is
-- {name, usage, run}: `usage` is the line the model reads about it, `run` takes the arguments and returns a value,
-- usually the text the model is meant to see, and reports failure by raising. The same functions are there for
-- extensions. Add project functions to this list; the order is the order the model reads them in.

-- Checks one argument, and names the call's whole shape when it is wrong: a model used to JSON tools may pass a
-- table of named arguments where a string belongs.
local function check(fname, value, position, name, kind, optional)
	if type(value) == kind or value == nil and optional then
		return
	end
	local usage = fname
	for _, fn in M.functions do
		if fn.name == fname then
			usage = fn.usage:match("^[^>]*%)") or fn.name
		end
	end
	error(("%s: argument %d (%s) must be a %s, not %s; usage: %s"):format(fname, position, name, kind, type(value),
		usage), 0)
end

M.functions = {
	{
		name = "bash",
		usage = "bash(command, timeout?) -> text: runs a bash command in the project directory, a new shell each "
			.. "time, and returns stdout and stderr together, then [exit N]. The command is killed after timeout "
			.. "seconds (default 120). A process left in the background must send its output to a file, and is "
			.. "killed when this session ends, daemons included: nothing you start outlives it.",
		run = function(command, timeout)
			check("bash", command, 1, "command", "string")
			check("bash", timeout, 2, "timeout", "number", true)
			progress("  bash " .. M.printable(command:sub(1, 100):gsub("\n", " ")))
			local result = host.sh(command, { timeout = timeout })
			local status
			if result.timed_out then
				status = "timed out and was killed"
			elseif result.signal then
				status = "killed by signal " .. result.signal
			else
				status = "exit " .. result.code
			end
			local out = result.out
			if out ~= "" and out:sub(-1) ~= "\n" then
				out ..= "\n"
			end
			return out .. "[" .. status .. "]"
		end,
	},
	{
		name = "read",
		usage = "read(path, offset?, limit?) -> text: whole lines of a text file from line offset (default 1), up "
			.. "to limit lines (default 2000) or the output limit, each as `NUMBER: text`; the numbers are not part "
			.. "of the file. When that is not the whole file, it ends with the range and the offset to go on from. "
			.. "For the file's exact contents, as data, use host.read(path).",
		run = function(path, offset, limit)
			check("read", path, 1, "path", "string")
			check("read", offset, 2, "offset", "number", true)
			check("read", limit, 3, "limit", "number", true)
			progress("  read " .. M.printable(path))
			local text, err = host.read(path)
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
			-- A \r\n line end is shown as a plain one. Shown, the \r reaches the model as the text \r, which it then
			-- types into an edit as a backslash and an r; edit finds plain \n text in a \r\n file anyway.
			for i, line in lines do
				if line:sub(-1) == "\r" then
					lines[i] = line:sub(1, -2)
				end
			end
			local first = math.max(1, offset or 1)
			local last = math.min(#lines, first + (limit or 2000) - 1)
			if first > #lines then
				error(("offset %d is past the end: the file has %d lines"):format(first, #lines), 0)
			end
			-- Whole lines up to the byte limit, never head and tail, so the range says exactly what was left out. The
			-- limit leaves room for the range line, so that a read on its own is never cut when the result is.
			local budget = M.output_limit - 200
			local function numbered(i)
				return ("%d: %s"):format(i, lines[i])
			end
			local shown = { numbered(first) }
			local size = #shown[1]
			for i = first + 1, last do
				local line = numbered(i)
				size += 1 + #line
				if size > budget then
					last = i - 1
					break
				end
				table.insert(shown, line)
			end
			local body
			if #lines[first] > budget then
				-- A line longer than the limit still counts as read, so the next offset moves past it.
				last = first
				body = ("%d: %s"):format(first, lines[first]:sub(1, budget))
					.. ("\n[line %d is %d bytes; only the first %d are shown]"):format(first, #lines[first], budget)
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
		usage = "write(path, content) -> text: creates or overwrites a file. Missing directories are created. For a "
			.. "change to part of an existing file, edit is cheaper and safer.",
		run = function(path, content)
			check("write", path, 1, "path", "string")
			check("write", content, 2, "content", "string")
			progress("  write " .. M.printable(path))
			local ok, err = host.write(path, content)
			if not ok then
				error(err, 0)
			end
			return ("wrote %d bytes to %s"):format(#content, path)
		end,
	},
	{
		name = "edit",
		usage = "edit(path, old, new, all?) -> text: replaces the exact text old with new in a file. old must occur "
			.. "exactly once, unless all is true, which replaces every occurrence. Copy old from the file without "
			.. "the line numbers read puts in front.",
		run = function(path, old, new, all)
			check("edit", path, 1, "path", "string")
			check("edit", old, 2, "old", "string")
			check("edit", new, 3, "new", "string")
			check("edit", all, 4, "all", "boolean", true)
			progress("  edit " .. M.printable(path))
			if old == "" then
				error("edit: old must not be empty", 0)
			end
			if old == new then
				error("edit: old and new are the same, so this edit would change nothing", 0)
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
				error(("edit: old text not found in %s: read the file again and copy the text exactly%s"):format(path, hint), 0)
			end
			if count > 1 and not all then
				error(("edit: old text occurs %d times in %s: add surrounding lines to make it unique, or pass all = true"):format(count, path), 0)
			end
			local ok, write_err = host.write(path, table.concat(pieces))
			if not ok then
				error(write_err, 0)
			end
			return ("replaced %d occurrence%s in %s"):format(count, count == 1 and "" or "s", path)
		end,
	},
}

-- Tools -----------------------------------------------------------------------------------------------------
--
-- Each tool is {name, description, parameters (a JSON schema), run(args) -> text, failed?}. A tool may also report
-- failure by raising; the message goes back to the model. The built-in loop has one tool.

local scratch = setmetatable({}, { __index = lookup }) -- globals of `lua` code live here, across calls

local function show(value)
	local meta = getmetatable(value)
	if type(meta) == "table" and meta.__tostring then -- a value that says how it reads, as search results do
		return tostring(value)
	end
	if type(value) == "table" then
		local ok, text = pcall(json.encode, value)
		if ok then
			return text
		end
	end
	return tostring(value)
end

local function lua_description()
	local lines = {
		"Run Luau code in the project directory. This is how you act: the code calls the functions below. The "
			.. "result is what the code prints and then what it returns; a function's value shows only when you "
			.. "print or return it. Globals persist between calls. A call is stopped after "
			.. M.call_seconds .. " seconds.",
		"",
	}
	for _, fn in M.functions do
		table.insert(lines, "- " .. fn.usage)
	end
	table.insert(lines, "- ext(name) -> table: the extension .hx/ext/NAME.luau, or one built into hx.")
	table.insert(lines, "- host.sh(command) -> {out, code}, host.read(path) -> text, host.write(path, text), "
		.. "host.list(dir), json.encode, json.decode: the plain primitives, for working on data.")
	table.insert(lines, "")
	table.insert(lines, "A function that fails raises an error, which stops the code there: what ran before it "
		.. "has happened, and what it printed is shown with the error. Put text with quotes, backslashes or "
		.. "several lines in long brackets, [==[like this]==], which need no escaping; a newline right after the "
		.. "opening bracket is dropped.")
	return table.concat(lines, "\n")
end

M.tools = {
	{
		name = "lua",
		description = lua_description,
		parameters = {
			type = "object",
			properties = { code = { type = "string" } },
			required = { "code" },
		},
		run = function(args)
			if type(args.code) ~= "string" then
				error('argument "code" must be a string', 0)
			end
			local lines = {}
			scratch.print = function(...)
				local parts = table.pack(...)
				for i = 1, parts.n do
					parts[i] = tostring(parts[i])
				end
				table.insert(lines, table.concat(parts, "\t"))
			end
			local chunk, err = host.load(args.code, "=lua", scratch)
			if not chunk then
				return "error: " .. err, true
			end
			local results = table.pack(host.timed(M.call_seconds, chunk))
			local failed = not results[1]
			if failed then
				table.insert(lines, "error: " .. tostring(results[2]))
			else
				for i = 2, results.n do
					table.insert(lines, show(results[i]))
				end
			end
			return #lines > 0 and clip(table.concat(lines, "\n")) or "[no output]", failed
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
	local args = call.input
	if type(args) ~= "table" then
		return "error: the tool input is not an object", true
	end
	if tool.name ~= "lua" then -- the functions the code calls say what they do
		progress("  " .. tool.name)
	end
	local problem = argument_problem(tool, args)
	if problem then
		return "error: " .. problem, true
	end
	local ok, result, failed = pcall(tool.run, args)
	if ok then
		return result, failed == true
	end
	return "error: " .. tostring(result), true
end

-- The model --------------------------------------------------------------------------------------------------
--
-- This speaks the Anthropic Messages format: a `system` string, messages whose content is a list of blocks
-- (text, thinking, tool_use, tool_result), and tools given as {name, description, input_schema}. To use another
-- wire format, replace M.request and M.turn.

-- What the loop sends to the Claude models it knows, beyond adaptive thinking, by model name. A model not listed
-- gets the plain request, so DeepSeek and older Claude models see nothing they might reject.
--   effort     sent when M.effort is unset: these models think by effort alone, and the API's own default
--              differs between them (medium on Opus and Haiku 5.5, high on Sonnet 5.5); medium is where
--              Anthropic suggests starting agentic coding on all three
--   fallbacks  a declined request is run again on another model, chosen by why it was declined, in the same
--              round trip; Haiku 5.5 has no fallback
--   display    "updates": the notes the model writes between tool calls come back in thinking blocks, which
--              the loop shows as progress lines; without it they come back empty
--   compaction the API writes the summary when the conversation grows too long (on-demand compaction, the
--              kind Anthropic says to use wherever it is available), instead of the loop asking for one as an
--              ordinary reply; see M.compact
-- Every listed model checks that a thinking block goes back after the same history it was written after. The
-- loop only ever appends, but a project may change it: block_binding drop_block then drops a stale block instead
-- of failing the request.
M.claude = {
	["claude-opus-5-5"] = { effort = "medium", fallbacks = "default", display = "updates", compaction = true },
	["claude-sonnet-5-5"] = { effort = "medium", fallbacks = "default", display = "updates", compaction = true },
	["claude-haiku-5-5"] = { effort = "medium", compaction = true },
}

local usage = { replies = 0, input = 0, cached = 0, written = 0, output = 0 }

-- Adds a reply's tokens to the run's count. A compaction reply has zero at the top and the summarization call in
-- usage.iterations; where iterations are given, they are what was billed.
local function count_usage(reply)
	local counts = type(reply.usage) == "table" and reply.usage or {}
	local parts = type(counts.iterations) == "table" and #counts.iterations > 0 and counts.iterations or { counts }
	usage.replies += 1
	for _, part in parts do
		usage.input += (part.input_tokens or 0) + (part.cache_read_input_tokens or 0)
			+ (part.cache_creation_input_tokens or 0)
		usage.cached += part.cache_read_input_tokens or 0
		usage.written += part.cache_creation_input_tokens or 0
		usage.output += part.output_tokens or 0
	end
end

local function tool_specs()
	local specs = json.array({}) -- a list even when a project has taken every tool away
	for _, tool in M.tools do
		local description = type(tool.description) == "function" and tool.description() or tool.description
		table.insert(specs, { name = tool.name, description = description, input_schema = tool.parameters })
	end
	return specs
end

-- Tokens the last request put in front of the model, cached or not.
local function prompt_size(reply)
	local counts = reply.usage or {}
	return (counts.input_tokens or 0) + (counts.cache_read_input_tokens or 0) + (counts.cache_creation_input_tokens or 0)
end

-- Whether a reply has anything in it. Thinking alone counts; an empty text block does not. A reply cut off by
-- the output limit (the loop asks it to go on) or refused (that is the answer) counts however empty it is.
local function has_content(reply)
	if reply.stop_reason == "max_tokens" or reply.stop_reason == "refusal" then
		return true
	end
	for _, block in reply.content do
		if block.type ~= "text" or block.text ~= "" then
			return true
		end
	end
	return false
end

-- A reply a fallback model finished carries a `fallback` block where the switch happened. What the declining model
-- wrote before the last one is not its to send back: its thinking, tool calls and other model-internal blocks are
-- dropped, and the fallback block stays where it is, because the API checks the thinking around it by its place.
local internal_before_fallback = { thinking = true, redacted_thinking = true, tool_use = true, server_tool_use = true }
local function after_fallback(reply)
	local last = nil
	for i, block in reply.content do
		if block.type == "fallback" then
			last = i
		end
	end
	if not last then
		return
	end
	local from, to = reply.content[last]["from"], reply.content[last].to
	progress(("  %s declined; %s continued"):format(tostring(from and from.model), tostring(to and to.model)))
	local kept = {}
	for i, block in reply.content do
		if i >= last or not internal_before_fallback[block.type] then
			table.insert(kept, block)
		end
	end
	reply.content = kept
end

-- Sends the conversation and returns the decoded reply. Retries what is worth retrying, then raises. The endpoint
-- sometimes answers 200 with nothing in it; that is retried like a busy server, since the same request may well
-- get a real reply. `compaction`, given, asks a Claude model for a summary in place of a reply (see M.compact);
-- that reply comes back however empty it is, since an empty one is an answer too.
function M.request(system, messages, compaction)
	local claude = M.claude[M.model]
	local thinking = { type = "adaptive" }
	local betas = {}
	local fallbacks = nil
	if claude then
		thinking.block_binding = { prefix_mismatch_behavior = "drop_block" }
		table.insert(betas, "thinking-binding-controls-2026-08-01")
		if claude.display then
			thinking.display = claude.display
			table.insert(betas, "thinking-display-updates-2026-08-18")
		end
		if claude.fallbacks then
			fallbacks = claude.fallbacks
			table.insert(betas, "server-side-fallback-2026-07-01")
		end
		-- On every request, not only the one that asks for the summary: a request that carries the block needs it.
		if claude.compaction then
			table.insert(betas, "compact-2026-09-04")
		end
	end
	local effort = M.effort or (claude and claude.effort)
	-- Caching. The Claude API caches a prefix only up to a breakpoint, and a request reads only what an earlier one
	-- wrote at a breakpoint. Two of them, both with the default five-minute life, which every request in a running
	-- loop refreshes:
	--   on the system prompt: tools and system, which never change in a run, get an entry of their own. A
	--     compaction replaces every message; without this entry it would pay for tools and system again too. A
	--     new run in the same project within five minutes reads it as well.
	--   top-level, automatic: the API puts it on the last block and moves it forward as the conversation grows, so
	--     each request reads everything the one before it sent and writes only what was added since.
	-- Everything ahead of a breakpoint has to be byte for byte the same, request after request: the system prompt
	-- is built once per run, the tool description is the same each time, json.encode sorts keys, the conversation
	-- is only ever appended to (a reminder is a text block behind the tool results, never taken out again), and
	-- thinking and effort are fixed for the run.
	-- DeepSeek caches every prefix by itself and ignores cache_control, so any model that is not Claude gets the
	-- system prompt as the plain string; the same stable prefix is what makes its cache hits.
	local system_field = system
	if M.model:match("^claude%-") then
		system_field = { { type = "text", text = system, cache_control = { type = "ephemeral" } } }
	end
	local body = json.encode({
		model = M.model,
		max_tokens = M.max_output,
		thinking = thinking,
		output_config = effort and { effort = effort } or nil,
		fallbacks = fallbacks,
		cache_control = { type = "ephemeral" },
		system = system_field,
		messages = messages,
		tools = tool_specs(),
		compaction = compaction,
	})
	local options = { beta = #betas > 0 and table.concat(betas, ",") or nil }
	local attempts = 5
	for attempt = 1, attempts do
		local status, text = host.llm(body, options)
		if status == 200 then
			local reply, err = json.decode(text)
			if type(reply) == "table" and type(reply.content) == "table" then
				after_fallback(reply)
				count_usage(reply)
				if compaction or has_content(reply) then
					return reply
				end
				status, text = "empty reply", "the model sent a reply with nothing in it"
			else
				error("the model endpoint sent a reply this loop cannot read: " .. (err or text:sub(1, 300)), 0)
			end
		end
		local transient = type(status) ~= "number" or status == 408 or status == 409 or status == 429 or status >= 500
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
		("You are a coding agent working in %s. You act by running Luau code with the lua tool; the code calls "
			.. "the functions the tool lists. Put several steps in one call when you know what each will do, and take "
			.. "one step at a time when the next depends on what the last returned."):format(host.cwd),
		host.sandboxed
				and "The kernel confines you: you can write inside this project directory and $TMPDIR, and read those "
					.. "plus the system directories. \"Permission denied\" anywhere else is that wall. Do not refuse "
					.. "in advance: try what the task needs, and only when the wall denies it, say so and stop "
					.. "rather than looking for a way around it."
			or "Nothing confines you in this session: you act with the user's full access to this machine. Stay "
				.. "inside this project directory unless the task says otherwise.",
		"You can extend yourself. You run on the loop built into hx; .hx/loop.luau, if the project has one, starts "
			.. "from it and changes parts. Your code can read the built-in source as host.default_loop. Extensions "
			.. "are Luau modules in .hx/ext/NAME.luau: the first line is a `--` comment saying what the module does, "
			.. "and it returns "
			.. "a table of functions. Call one from your code as ext(\"NAME\").fn(...); write it with write(). Extensions stay with "
			.. "the project, so write one when you notice a chore you will meet again, and keep it as small as the chore.",
		"Use read(), not bash() with cat, sed, head or tail, to look at text files; its offset and limit read a "
			.. "range of lines or go on through a large file. Read a file before you edit it, unless "
			.. "you wrote or edited it yourself in this session.",
		"When the task is done, finish with a short report: what you changed, what you checked, what is left.",
	}
	local extensions = extension_index()
	if #extensions > 0 then
		table.insert(parts, "Extensions:\n" .. table.concat(extensions, "\n"))
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

-- Forgets everything and starts again from a compaction block, which the API reads in place of the messages it
-- summarized, followed by one user message. The block goes first in the first message, exactly as it came.
function Conversation:resume(block, text)
	expect(self, "resume", "idle", "ready")
	table.clear(self.messages)
	table.insert(self.messages, { role = "user", content = { block, { type = "text", text = text } } })
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
local function ask(system, talk, compaction)
	local problem = talk:state() ~= "ready" and "it is " .. talk:state() .. ", not ready" or talk:problem()
	if problem then
		error("this conversation cannot be sent: " .. problem, 0)
	end
	return M.request(system, talk.messages, compaction)
end

-- The request for the summary: a Claude model with on-demand compaction gets it as the `instructions`; any other
-- model gets it behind the whole conversation, as the last user message, so the request reuses the cached prefix.
-- The summary is in English whatever the conversation's language: it becomes the start
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

-- What follows a compaction block, so that the model goes on with the work.
M.resume_prompt = "Earlier turns were replaced by the checkpoint above, written by you. Treat it as established "
	.. "and continue the work from it directly, without restating or acknowledging it."

-- On-demand compaction, for the Claude models whose M.claude entry says so. The request is the conversation as it
-- stands, with the same model, system, tools, thinking and effort, so it reads the cached prefix, plus
-- `compaction`; M.compact_prompt is its `instructions`, which replace the API's own summarization prompt. The API
-- writes the summary into a signed `compaction` block and no reply. The block then takes the place of every
-- message it summarized: it goes first, exactly as it came, and is sent with every later request. A newer block
-- summarizes the old one with what came after it. A request that brings no block (cut off, refused, a tool call)
-- or fails leaves the conversation as it is, to be compacted after a later reply, as the docs advise.
local function compact_on_server(system, talk)
	local ok, reply = pcall(ask, system, talk, { type = "summarize", instructions = M.compact_prompt })
	if not ok then
		progress("  compaction failed, the conversation stays as it is: " .. tostring(reply))
		return
	end
	local block = nil
	if reply.stop_reason == "compaction" then
		for _, b in reply.content do
			if b.type == "compaction" then
				block = b
			end
		end
	end
	if not block then
		progress(("  no summary came back (%s); keeping the conversation as it is"):format(tostring(reply.stop_reason)))
		return
	end
	talk:resume(block, M.resume_prompt)
end

-- Replaces the conversation with the model's own summary of what happened. A Claude model that has on-demand
-- compaction gets it; any other model, DeepSeek's included, is asked for the summary as an ordinary reply,
-- behind the whole conversation so that the request reuses the cached prefix.
function M.compact(system, talk)
	progress("  compacting the conversation")
	local claude = M.claude[M.model]
	if claude and claude.compaction then
		return compact_on_server(system, talk)
	end
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

-- What the model is told when a function call has returned the same result `count` times in this task (see
-- `counted`), as `call`, the call written out. `first` is the first reminder of the turn.
function M.repeat_reminder(call, count, first)
	if #call > 500 then
		call = call:sub(1, 500) .. ("... (%d more bytes)"):format(#call - 500)
	end
	if first then
		return ("%s has now returned the same result %d times. If it is not going to change, stop and report what "
			.. "you found; otherwise try a different approach. Do not change what you are checking to make it give "
			.. "another answer."):format(call, count)
	end
	return ("%s has returned the same result %d times in this task, so asking again is not making progress. Do not "
		.. "call it again, and do not change what it checks to make it give another answer. Choose a different "
		.. "action, or finish the task and say what you found."):format(call, count)
end

-- Prints the text of a reply: the model's words are the answer, on stdout. A model asked for progress updates
-- (M.claude's display) writes its notes between tool calls into thinking blocks; those are progress lines.
local function show_text(reply)
	local claude = M.claude[M.model]
	local updates = claude and claude.display == "updates"
	for _, block in reply.content do
		if block.type == "text" and block.text ~= "" then
			print(host.tty and M.printable(block.text) or block.text)
		elseif updates and block.type == "thinking" and type(block.thinking) == "string" and block.thinking ~= "" then
			progress("  " .. M.printable(block.thinking))
		end
	end
end

-- Runs the model until it answers without asking for a tool.
function M.turn(system, talk)
	repeats.counts, repeats.worst = {}, nil
	local reminded = 0 -- the highest count already reminded of
	for _ = 1, M.max_replies do
		local reply = ask(system, talk)
		local cut_off = reply.stop_reason == "max_tokens"
		show_text(reply)
		local calls = talk:reply(reply.content)
		if #calls > 0 then
			talk:answer(function(call)
				if cut_off then
					return "error: this call was cut off by the output limit and did not run. Send it again, smaller.", true
				end
				return run_tool(call)
			end)
			-- One reminder for the highest threshold the worst call has passed since the last one, however many
			-- times a loop inside one call passed.
			local worst, due = repeats.worst, nil
			for i, at in M.repeat_reminders do
				if worst and worst.count >= at and at > reminded then
					due = i
				end
			end
			if due then
				reminded = M.repeat_reminders[due]
				talk:say(M.repeat_reminder(worst.call, worst.count, due == 1))
			end
			if prompt_size(reply) > M.compact_at then
				M.compact(system, talk)
			end
		elseif cut_off then
			talk:say("Your reply was cut off by the output limit. Continue.")
		else
			if reply.stop_reason == "refusal" then
				local details = type(reply.stop_details) == "table" and reply.stop_details or {}
				local category = type(details.category) == "string" and " (" .. details.category .. ")" or ""
				host.log("[the model declined to answer" .. category .. "]")
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
	-- A healthy run reads most of what goes in from the cache and writes little of it. DeepSeek reports no writes.
	progress(("[%d replies, %d tokens in (%d read from cache, %d written to it), %d out]"):format(usage.replies,
		usage.input, usage.cached, usage.written, usage.output))
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
