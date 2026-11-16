--!strict
-- The hx loop, built into hx. A project changes it in .hx/loop.luau, which loads this module, replaces some of
-- its fields and returns it (`hx --init` writes such a file), or owns all of it as a copy (`hx --init --full`). The
-- parts meant to be replaced are fields of M and are always called through M.
--
-- The host supplies a small kernel and nothing else:
--   host.llm(body, {timeout=, beta=}) -> status, text, retry_after? | nil, error     POST to the endpoint; the host
--                                                     adds the API key, and beta as the anthropic-beta header
--   host.sh(command, {timeout=}) -> {out=, code=, signal=, timed_out=}
--   host.read(path), host.write(path, data), host.list(dir)
--   host.clip(text, limit) -> text | head, tail       text when it fits in limit bytes; else up to limit // 2 from
--                                                     each end, cut at a line or a character, as host.sh's output
--   host.load(source, name, env) -> function | nil, error
--   host.try(fn, ...) -> true, ... | false, error     a pcall that is also where a stop ends
--   host.timed(seconds, fn, ...) -> true, ... | false, error     host.try with a time limit
--   host.input(prompt), host.log(text), host.sleep(seconds), host.env(name)
--   host.args, host.cwd, host.interactive, host.quiet, host.sandboxed, host.default_loop
--   host.tty                                          stdout is a terminal
--   host.model                                        the model --model or HX_MODEL chose, its alias resolved
--                                                     (opus is claude-opus-5-5), or the default
--   host.tool(name, request, {timeout=}) -> status, text | nil, error     DeepSeek's search, fetch or ocr endpoint,
--                                                     or Exa's search for "exa"; the host adds the key
--   host.tools, host.exa                              DEEPSEEK_API_KEY, EXA_API_KEY is there
--   host.extensions                                   the built-in extensions' sources, by name
--   host.skills                                       the built-in skills' SKILL.md texts, by name
--   host.judge(line?) -> {lines}, pending, host.judged     the judge's channel, with --judge KIT
--   json.encode, json.decode, json.array, json.null
-- Ctrl-C raises "interrupted" and SIGTERM "terminated", again at every step, until the outermost host.try or
-- host.timed returns; a plain pcall cannot swallow them.
-- The whole process sits inside one Landlock ruleset, so nothing in this file can reach more of the machine
-- than the model's own shell can.

-- The shapes the loop is built on. host and json are untyped, so what they return is `any`: a local that holds it
-- is annotated with what the primitive says it returns, so that a check of it narrows the type.

-- A block of a message's content, as the API sends it: text, thinking, tool_use, tool_result, compaction,
-- fallback and others. Blocks go back exactly as they came, so a block keeps fields the loop never names.
type Block = {
	type: string,
	text: string?,
	thinking: any,
	id: string?,
	name: string?,
	input: any,
	tool_use_id: string?,
	content: any,
	is_error: boolean?,
	from: any,
	to: any,
}
-- The user's content may be a plain string; the model's is always a list of blocks.
type Message = { role: string, content: string | { Block } }
-- What a reply's usage says: tokens, and for a compaction the summarization call's in iterations.
type Usage = {
	input_tokens: number?,
	cache_read_input_tokens: number?,
	cache_creation_input_tokens: number?,
	output_tokens: number?,
	iterations: { Usage }?,
}
-- A decoded reply from the model.
type Reply = { content: { Block }, stop_reason: string?, stop_details: any, usage: Usage? }
-- A function the model's code calls (see M.functions).
type Function = { name: string, usage: string, run: (...any) -> ...any }
-- The JSON schema of a tool's arguments, as far as the loop reads it.
type Schema = { type: string?, properties: { [string]: any }?, required: { string }? }
-- A tool the model is offered (see M.tools): the text of its result, and whether it failed.
type Tool = {
	name: string,
	description: string | () -> string,
	parameters: Schema?,
	run: ({ [string]: any }) -> (string, boolean?),
}
-- A skill found in one of M.skill_dirs, or built into hx (see M.skills). A built-in one has no directory: its
-- SKILL.md is `source`, and `path` says where it is from instead of naming a file.
type Skill = { name: string, description: string, dir: string, path: string, hidden: boolean, source: string? }
-- What M.claude says of a model the loop knows.
type Features = { effort: string?, fallbacks: string?, display: string?, compaction: boolean? }

local M = {}

M.model = host.model
M.max_output = 16000 -- tokens per model reply
M.max_replies = 200 -- model replies per task before giving up
M.compact_at = 150000 -- prompt tokens at which the conversation is replaced by a summary of itself
M.output_limit = 30000 -- bytes of a tool result shown to the model
M.call_seconds = 600 -- time limit of one `lua` tool call, commands and all
M.effort = host.env("HX_EFFORT") -- low, medium, high, xhigh or max: how hard the model thinks; nil leaves it to
-- the model's entry in M.claude, or else to the endpoint
M.repeat_reminders = { 3, 5, 8 } -- a function call with the same result this many times in a task gets a reminder

-- Everything the loop changes as it runs, in one place, by how long it lives. A project function or extension can
-- run a `lua` call inside another, or a turn on another Conversation inside a turn (a sub-agent), so whatever lives
-- for one call or one turn is set on the way in and the outer value put back on the way out, however it ends: a
-- nested call or turn leaves its parent's state as it found it. The globals of the model's code are the model's,
-- not the loop's, and live in `scratch` (see the lua tool).
type Repeats = { counts: { [string]: number }, worst: { call: string, count: number }? }
local live: {
	usage: { replies: number, input: number, cached: number, written: number, output: number },
	spills: number,
	ext_cache: { [string]: { source: string, value: any } },
	wrappers: { [Function]: (...any) -> ...any },
	repeats: Repeats,
	printed: { string }?,
} = {
	-- For the whole run, from when the module loads:
	usage = { replies = 0, input = 0, cached = 0, written = 0, output = 0 }, -- tokens, for the line at the end
	spills = 0, -- files that clipped results went to, so the next one has a name of its own
	ext_cache = {}, -- each extension loaded, by name, with the source it was loaded from
	wrappers = {}, -- each function's counting wrapper, made once; weak keys, below
	-- For one turn, set by M.turn: each function call's count by what it was asked and what it returned, and the
	-- call repeated most. Outside any turn (a test calling the functions), one set for the run.
	repeats = { counts = {}, worst = nil },
	-- For one `lua` call, set by the tool: the lines it printed. nil outside any call.
	printed = nil,
}
-- A function a project takes out of M.functions takes its wrapper with it.
setmetatable(live.wrappers, { __mode = "k" })

-- A progress line on stderr: what the loop is doing, left out with --quiet. Warnings and errors use host.log.
-- Its text is made printable here, once: it carries file names, skill names and server replies, none of them hx's.
local function progress(text: string)
	if not host.quiet then
		host.log(M.printable(text))
	end
end

-- Keeps the start and the end of a long result: the start says what ran, the end says how it went. Only the result
-- the model is sent is clipped, once; the functions return everything, so code can work on all of it. host.clip
-- cuts it, as host.sh cuts a command's output, at a line where one is near and never inside a character. The whole
-- text goes to a file in $TMPDIR (hx makes one for every run, inside the wall), so what was left out can still be
-- read or searched.
local function clip(text: string): string
	local head: string, tail: string? = host.clip(text, M.output_limit)
	if tail == nil then
		return text
	end
	live.spills += 1
	local path = ("%s/hx-output-%d.txt"):format(host.env("TMPDIR"), live.spills)
	local where: string = host.write(path, text) and "; the whole output is in " .. path or ""
	return head .. (head:sub(-1) == "\n" and "" or "\n")
		.. ("[... %d bytes omitted%s ...]\n"):format(#text - #head - #tail, where) .. tail
end

-- Removes control characters but newline and tab. Escape sequences in text from the model would otherwise
-- reach the terminal, which they can retitle, scramble or (OSC 52) use to write the clipboard. The C1 controls
-- (U+0080 to U+009F, which some terminals take for escape sequences too) and the characters that reverse the
-- direction of text (U+202A to U+202E, U+2066 to U+2069) go as well.
function M.printable(text: string): string
	return (text:gsub("[\0-\8\11-\31\127]", ""):gsub("\194[\128-\159]", ""):gsub("\226\128[\170-\174]", "")
		:gsub("\226\129[\166-\169]", ""))
end

-- Extensions ------------------------------------------------------------------------------------------------
--
-- An extension is a Luau module in .hx/ext/NAME.luau that returns a table of functions. The model writes them
-- with write() and calls them from its code, so the tool never changes. A module is loaded again whenever its
-- file has changed. Extensions see the same functions the model's code does, and what they print goes into the
-- tool result with what that code prints. hx has some built in
-- (host.extensions: web, ocr and exa); a project's file of the same name takes the place of one.

local ext_dir = ".hx/ext/"

-- Every function call is counted by what it was asked and what it returned. A call that returns what it returned
-- before is going nowhere, whatever ran in between and whether the model repeats it across tool calls or in a loop
-- inside one: that is the unit the repeat reminder counts, not the tool call, whose code may print a counter or a
-- clock and so never repeat exactly. A command rerun after a fix returns something new, so it does not count. The
-- counts are live.repeats, the running turn's.

-- A value as JSON, nil as null; nil for a value with no JSON form, such as a table of functions.
local function as_json(value: any): string?
	local ok, text = pcall(json.encode, value == nil and json.null or value)
	return ok and text or nil
end

-- A short stand-in for a text, so that the counts do not keep every result of the turn (a bash call returns up to a
-- mebibyte): a short text itself, a longer one its length and a hash of all of it, read four bytes at a time.
-- Different texts of the same length rarely share a hash, and when they do it costs only an early reminder.
local function digest(text: string): string
	if #text <= 256 then
		return text
	end
	local data, hash, tail = buffer.fromstring(text), 0, #text - #text % 4
	-- 65599 times a hash below the prime 4294967291, plus a 32-bit word, stays below 2^53, so it is exact.
	for at = 0, tail - 4, 4 do
		hash = (hash * 65599 + buffer.readu32(data, at)) % 4294967291
	end
	for at = tail, #text - 1 do
		hash = (hash * 65599 + buffer.readu8(data, at)) % 4294967291
	end
	return ("%d:%d"):format(#text, hash)
end

local function counted(fn: Function): (...any) -> ...any
	return function(...)
		local results = table.pack(fn.run(...))
		local result = results[1] -- what a function shows the model; any further values pass through uncounted
		-- A command that succeeded and printed nothing did something rather than looked at something (git add,
		-- mkdir, a sleep between polls): that it does so again says nothing about progress. Nor does a result with
		-- no text form, such as ext's table of functions, which is the same table each time.
		local text: string? = nil
		if result ~= "[exit 0]" then
			text = if type(result) == "string" then result else as_json(result)
		end
		if not text then
			return table.unpack(results, 1, results.n)
		end
		local args = {}
		for i = 1, select("#", ...) do
			local value = (select(i, ...))
			args[i] = as_json(value) or tostring(value)
		end
		local call = ("%s(%s)"):format(fn.name, table.concat(args, ", "))
		local key = digest(call) .. "\0" .. digest(text)
		local repeats = live.repeats
		local count = (repeats.counts[key] or 0) + 1
		repeats.counts[key] = count
		if not repeats.worst or count > repeats.worst.count then
			repeats.worst = { call = call, count = count }
		end
		return table.unpack(results, 1, results.n)
	end
end

-- Where print goes: into the result of the `lua` call that is running, which is what the model sees, whether the
-- model's code prints or an extension it called does. Outside a call (a test calling an extension), to stderr, never
-- to stdout, which carries only the answer, and as text only, since stderr may be a terminal.
local function capture_print(...: any)
	local parts = table.pack(...)
	for i = 1, parts.n do
		parts[i] = tostring(parts[i])
	end
	local line = table.concat(parts, "\t")
	if live.printed then
		table.insert(live.printed, line)
	else
		host.log(M.printable(line))
	end
end

-- Code from the model and from extensions sees the host's tables (host, json, string, table and the other
-- libraries) only through read-only views. The loop shares those tables: `host.sh = nil` or `json.encode = nil` in
-- one call would otherwise break every later call, and the loop with them, for the rest of the session. A view
-- reads through to its table at each access, so what the host or a test puts there later is seen, and an
-- assignment raises. rawset, getfenv and debug still reach the tables themselves: the views stop mistakes, not
-- code that digs on purpose.
local function read_only(name: string, real: { [any]: any }, own: { [string]: any }?): any
	local view = {}
	return setmetatable(view, {
		__index = function(_, key: any): any
			local mine = if own and type(key) == "string" then own[key] else nil
			if mine ~= nil then
				return mine
			end
			return real[key]
		end,
		__newindex = function(_, key: any)
			error(("%s is read-only: %s.%s cannot be assigned"):format(name, name, tostring(key)), 2)
		end,
		-- `for name, value in host.extensions` and the like iterate the table itself, with the view's values.
		__iter = function()
			local key = nil
			return function(): (any, any)
				key = next(real, key)
				if key == nil then
					return nil, nil
				end
				return key, view[key]
			end
		end,
		__len = function(): number
			return #real
		end,
		__metatable = "read-only",
	})
end

-- What differs in the view of `host`. host.log and host.input write to a terminal, so the text they are given is
-- made printable, as the loop's own lines are; the loop itself keeps the real host, because it writes escape
-- sequences (bracketed paste) on purpose. A chunk that host.load gives no environment gets globals of its own
-- that read these views, as an extension does, not the real ones.
local function host_view_parts(real: { [any]: any }, fresh_globals: () -> any): { [string]: any }
	return {
		log = function(text: any, ...: any)
			return real.log(if type(text) == "string" then M.printable(text) else text, ...)
		end,
		input = function(prompt: any, ...: any)
			return real.input(if type(prompt) == "string" then M.printable(prompt) else prompt, ...)
		end,
		load = function(source: any, name: any, env: any, ...: any)
			return real.load(source, name, if env == nil then fresh_globals() else env, ...)
		end,
	}
end

-- Each table's view, by the table, made once, so that a view is the same value from call to call.
local views: { [any]: any } = {}

-- The globals of code from the model and from extensions: the functions of M.functions (looked up when used, so a
-- project's changes to the list count at once), print, the views above, and the other Luau globals. `env` is the
-- code's own table of globals, which is also its _G.
local function lookup(env: any, name: string): any
	for _, fn in M.functions do
		if fn.name == name then
			local wrapper = live.wrappers[fn] or counted(fn)
			live.wrappers[fn] = wrapper
			return wrapper
		end
	end
	if name == "print" then
		return capture_print
	elseif name == "_G" then
		return env
	end
	local value = _G[name]
	if type(value) ~= "table" then
		return value
	end
	local view = views[value]
	if not view then
		view = read_only(name, value, if name == "host"
			then host_view_parts(value, function(): any
				return setmetatable({}, { __index = lookup })
			end)
			else nil)
		views[value] = view
	end
	return view
end

function M.ext(name: string): any
	if type(name) ~= "string" or not name:match("^[%w_%-]+$") then
		error("an extension name is letters, digits, _ and -", 0)
	end
	local own: string?, err: string? = host.read(ext_dir .. name .. ".luau")
	local built_in: string? = if own then nil else host.extensions[name]
	local source = own or built_in
	if not source then
		error("no extension " .. name .. ": " .. tostring(err), 0)
	end
	local cached = live.ext_cache[name]
	if cached and cached.source == source then
		return cached.value
	end
	local chunk: (() -> any)?, load_err: string? = host.load(source, (built_in and "=hx/ext/" or "=ext/") .. name,
		setmetatable({}, { __index = lookup })) -- globals of its own: one extension's do not leak into another
	if not chunk then
		error(load_err, 0)
	end
	local value = chunk()
	live.ext_cache[name] = { source = source, value = value }
	return value
end

-- One line per extension: its name and the comment on its first line. The built-in ones are listed when the key
-- they need is there (EXA_API_KEY for exa, DEEPSEEK_API_KEY for the rest), unless the project has its own of that
-- name. A name that starts with _ is a helper, code that other extensions share: ext() loads it, but it is not
-- listed, the project's or built in.
local function extension_index(): { string }
	local lines, own = {}, {}
	local function add(name: string, source: string, mark: string)
		if name:sub(1, 1) ~= "_" then
			table.insert(lines, ("- %s%s: %s"):format(name, mark, source:match("^%-%-%s*([^\n]*)") or "(no description)"))
		end
	end
	for _, file in host.list(ext_dir) or {} do
		local name = file:match("^([%w_%-]+)%.luau$")
		if name then
			own[name] = true
			add(name, host.read(ext_dir .. file) or "", "")
		end
	end
	local names = {}
	for name in host.extensions do
		if if name == "exa" then host.exa else host.tools then
			table.insert(names, name)
		end
	end
	table.sort(names)
	for _, name in names do
		if not own[name] then
			add(name, host.extensions[name], " (built in)")
		end
	end
	return lines
end

-- Skills ----------------------------------------------------------------------------------------------------
--
-- A skill is instructions for one kind of task, in the Agent Skills format: a directory with a SKILL.md, whose
-- YAML front matter has a name and a description, and whatever scripts and references the text points to. The
-- system prompt lists them; skill(name) returns one's text, and a name no skill has returns the list to pick from.
-- Only name, description and disable-model-invocation are read: hx has one tool and one wall, so allowed-tools,
-- model and hooks mean nothing here. Directories are searched in this order, and the first skill of a name wins.
-- A directory in the home directory is outside the wall unless hx was started with --ro for it; then it is
-- simply not found. hx has one skill built in (host.skills: hx, on using and extending hx itself), which comes
-- after every directory, so a skill of the same name in any of them takes its place.

local home = host.env("HOME")
M.skill_dirs = { ".hx/skills/", ".agents/skills/", ".claude/skills/" }
if home and home:sub(1, 1) == "/" then
	home = home:gsub("/+$", "")
	table.insert(M.skill_dirs, home .. "/.agents/skills/")
	table.insert(M.skill_dirs, home .. "/.claude/skills/")
end
M.skill_index_full = 30 -- up to this many skills, the system prompt lists each with its description; past it, names
M.skill_listed = 50 -- skills, with their descriptions, that skill() lists for a name no skill has
M.skill_description_limit = 250 -- characters of a description in those lists

local function trim(text: string): string
	return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

-- The fields of a SKILL.md's front matter that are plain or block scalars, and the text after it. Enough YAML for
-- what skills put there: `key: value`, quoted values, values continued on indented lines, and | and > blocks.
-- Nested maps are read as text and ignored.
function M.skill_front_matter(text: string): ({ [string]: string }, string)
	text = text:gsub("^\239\187\191", ""):gsub("\r\n", "\n")
	local head, rest = text:match("^%-%-%-[ \t]*\n(.-)\n%-%-%-[ \t]*\n(.*)$")
	head = head or text:match("^%-%-%-[ \t]*\n(.-)\n%-%-%-[ \t]*$") -- front matter with nothing after it
	if not head then
		return {}, text
	end
	local body = rest or ""
	local fields: { [string]: string }, lines, i = {}, head:split("\n"), 1
	while i <= #lines do
		local key, matched = lines[i]:match("^([%w_%-]+):%s*(.-)%s*$")
		i += 1
		if key and matched then -- both or neither
			local value: string = matched
			if value:match("^[|>][-+]?%d*$") then
				local folded, block = value:sub(1, 1) == ">", {}
				while i <= #lines and (lines[i]:match("^%s") or lines[i] == "") do
					table.insert(block, trim(lines[i]))
					i += 1
				end
				value = trim(table.concat(block, folded and " " or "\n"))
				if folded then
					value = value:gsub("  +", " ")
				end
			else
				while i <= #lines and lines[i]:match("^%s+%S") do
					value ..= " " .. trim(lines[i])
					i += 1
				end
				if value:match('^".*"$') then
					value = value:sub(2, -2):gsub("\\(.)", function(c) -- one pass, so an escaped backslash is not read twice
						return c == "n" and "\n" or c
					end)
				elseif value:match("^'.*'$") then
					value = value:sub(2, -2):gsub("''", "'")
				end
			end
			fields[key] = value
		end
	end
	return fields, body
end

-- Lower case, without - _ . and spaces: pdf-tools, PDF_Tools and pdftools are one name.
local function skill_key(name: string): string
	return (name:lower():gsub("[%-_%.%s]", ""))
end

-- Every skill that can be found now: {name, description, dir, path, hidden, source?}, in the order of M.skill_dirs
-- and then by directory name, and after those the ones built into hx, by name. Found anew on each call, so a skill
-- written in this session is found too.
function M.skills(): { Skill }
	local skills: { Skill }, seen = {}, {}
	-- `text` is a SKILL.md; `fallback` names the skill when its front matter does not.
	local function add(text: string, fallback: string, dir: string, path: string, source: string?)
		local fields = M.skill_front_matter(text)
		local name = trim(fields.name or "")
		name = name ~= "" and name or fallback
		if not seen[name] then
			seen[name] = true
			table.insert(skills, {
				name = name,
				description = trim(M.printable(fields.description or "")),
				dir = dir,
				path = path,
				hidden = fields["disable-model-invocation"] == "true",
				source = source,
			})
		end
	end
	for _, dir in M.skill_dirs do
		for _, entry in host.list(dir) or {} do
			local dir_name: string? = entry:match("^(.+)/$")
			if dir_name then
				local path = dir .. dir_name .. "/SKILL.md"
				local text: string? = host.read(path)
				if text then
					add(text, dir_name, dir .. dir_name .. "/", path, nil)
				end
			end
		end
	end
	local built_in: { string } = {}
	for name in host.skills do
		table.insert(built_in, name)
	end
	table.sort(built_in)
	for _, name in built_in do
		local text: string = host.skills[name]
		add(text, name, "", "built into hx", text)
	end
	return skills
end

local function cut(text: string, limit: number): string
	text = text:gsub("%s+", " ")
	local length = utf8.len(text)
	if length and length > limit then
		text = text:sub(1, utf8.offset(text, limit + 1) - 1) .. " ..."
	elseif not length and #text > limit then -- not UTF-8, so by bytes
		text = text:sub(1, limit) .. " ..."
	end
	return text
end

local function skill_line(skill: Skill): string
	local description = skill.description ~= "" and cut(skill.description, M.skill_description_limit)
		or "(no description)"
	return ("- %s%s: %s"):format(skill.name, skill.source and " (built in)" or "", description)
end

M.skill_files_shown = 30 -- files of a loaded skill named, with their paths, in its heading

-- The files of a skill other than SKILL.md, as paths from the project directory, so that the model can use them as
-- they are: up to M.skill_files_shown, three directories deep.
local function skill_files(dir: string): { string }
	local files = {}
	local function walk(path: string, depth: number)
		for _, entry in host.list(path) or {} do
			if #files >= M.skill_files_shown then
				return
			end
			if entry:sub(-1) == "/" then
				if depth < 3 then
					walk(path .. entry, depth + 1)
				end
			elseif path .. entry ~= dir .. "SKILL.md" then
				table.insert(files, path .. entry)
			end
		end
	end
	walk(dir, 1)
	return files
end

-- The skills the model is told about: all but those marked disable-model-invocation.
local function listed_skills(): { Skill }
	local listed = {}
	for _, skill in M.skills() do
		if not skill.hidden then
			table.insert(listed, skill)
		end
	end
	return listed
end

-- What skill(name) returns: the skill of that name, or the skills whose names differ from it only in case, -, _, .
-- and spaces when there are several, or else the list of skills to pick from. No guessing past that: the model
-- reads the list and picks better than a fuzzy match would.
function M.skill(name: string): string
	local skills = M.skills()
	if #skills == 0 then -- only where hx has none built in, or a project took them out of host.skills
		return "There are no skills. A skill is a directory with a SKILL.md, looked for in "
			.. table.concat(M.skill_dirs, ", ") .. "."
	end
	local found: { Skill } = {}
	for _, skill in skills do
		if skill.name == name then
			found = { skill }
			break
		end
	end
	if #found == 0 then
		for _, skill in skills do
			if skill_key(skill.name) == skill_key(name) then
				table.insert(found, skill)
			end
		end
	end
	if #found == 1 then
		local skill = found[1]
		local source: string?, err: string? = skill.source, nil
		if not source then
			source, err = host.read(skill.path)
		end
		if not source then
			error(("skill: %s: %s"):format(skill.path, tostring(err)), 0)
		end
		local _, body = M.skill_front_matter(source)
		progress("  skill " .. skill.name)
		if skill.source then -- no directory, so no files and no paths of its own
			return ("[skill %s, built into hx. Every path in it is the project's.]\n\n%s"):format(skill.name, trim(body))
		end
		local files = skill_files(skill.dir)
		local head = ("[skill %s, in %s. A path in it that names one of the skill's own files is relative to that "
			.. "directory; everything else is the project's."):format(skill.name, skill.dir)
		if #files > 0 then
			head ..= " Its files: " .. table.concat(files, ", ")
		end
		return head .. "]\n\n" .. trim(body)
	elseif #found > 1 then
		local lines = { ("Several skills have a name like %q; load one by its exact name:"):format(name) }
		for _, skill in found do
			table.insert(lines, skill_line(skill) .. " (" .. skill.path .. ")")
		end
		return table.concat(lines, "\n")
	end

	local listed = listed_skills()
	local lines = { ("No skill is named %q."):format(name) }
	if #listed == 0 then
		return lines[1]
	end
	table.insert(lines, "Pick one of these and load it by its name:")
	for i = 1, math.min(#listed, M.skill_listed) do
		table.insert(lines, skill_line(listed[i]))
	end
	if #listed > M.skill_listed then
		table.insert(lines, ("and %d more, named in the system prompt"):format(#listed - M.skill_listed))
	end
	return table.concat(lines, "\n")
end

-- The system prompt's part on skills: each with its description, or past M.skill_index_full only their names, which
-- keeps the prompt short; skill() with a name no skill has gives the descriptions.
local function skill_index(): string?
	local listed = listed_skills()
	if #listed == 0 then
		return nil
	end
	local head = "Skills are instructions for particular kinds of task. When the task matches one, load it with "
		.. "skill(name) before you start, and follow it."
	if #listed <= M.skill_index_full then
		local lines = { head }
		for _, skill in listed do
			table.insert(lines, skill_line(skill))
		end
		return table.concat(lines, "\n")
	end
	local names = {}
	for _, skill in listed do
		table.insert(names, skill.name)
	end
	return ("%s There are %d: %s. To see what they are for, call skill() with a name none has, as skill(\"?\"), "
		.. "which lists them with their descriptions."):format(head, #listed, table.concat(names, ", "))
end

-- The layout ------------------------------------------------------------------------------------------------
--
-- The system prompt shows the project's files as a tree, so that the model's first call need not be a listing.
-- The files are the ones git lists: tracked, or new and not ignored, which leaves out build output and the like.
-- Where git lists none (no repository, no git, nothing in it yet, or a tree it cannot walk in ten seconds), the
-- tree is the project directory's own entries, one level. It is made when the session starts, like the rest of
-- the prompt, so it does not upset the prompt cache, and says so, since files come and go after that.

M.layout_lines = 60 -- entries of the project's file tree in the system prompt; 0 leaves the tree out

-- A directory of the tree: the names in it, which of them are directories, and its files at any depth.
type Dir = { names: { string }, dirs: { [string]: Dir }, files: number }

-- A list of paths as a tree: one name per line, indented under its directory, directories before files, at most
-- `limit` lines. Directories are opened level by level, each one whose entries still fit; one that stays closed
-- is a line with the number of files below it. A path that ends in / is a directory with nothing known to be in
-- it. When the top level alone does not fit, the tree is its first `limit` entries and how many more there are.
function M.tree(paths: { string }, limit: number): string
	local root: Dir = { names = {}, dirs = {}, files = 0 }
	local previous: string? = nil
	for _, path in paths do
		if path ~= previous then -- git lists a file in a merge conflict once for each side
			previous = path
			local parts = path:split("/")
			local is_file = parts[#parts] ~= ""
			local dir = root
			for i = 1, #parts - 1 do
				dir.files += is_file and 1 or 0
				local sub = dir.dirs[parts[i]]
				if not sub then
					sub = { names = {}, dirs = {}, files = 0 }
					dir.dirs[parts[i]] = sub
					table.insert(dir.names, parts[i])
				end
				dir = sub
			end
			if is_file then
				dir.files += 1
				table.insert(dir.names, parts[#parts])
			end
		end
	end

	local open: { [Dir]: boolean } = { [root] = true }
	local queue, shown = { root }, math.min(#root.names, limit)
	local at = 0
	while at < #queue do -- the queue grows as it is read: a level's directories come before the next level's
		at += 1
		local dir = queue[at]
		table.sort(dir.names, function(a: string, b: string): boolean
			local a_dir, b_dir = dir.dirs[a] ~= nil, dir.dirs[b] ~= nil
			if a_dir ~= b_dir then
				return a_dir
			end
			return a < b
		end)
		for _, name in dir.names do
			local sub = dir.dirs[name]
			if sub and #sub.names > 0 and shown + #sub.names <= limit then
				open[sub] = true
				shown += #sub.names
				table.insert(queue, sub)
			end
		end
	end

	local lines: { string } = {}
	local function walk(dir: Dir, indent: string)
		for i, name in dir.names do
			if #lines == limit then -- the top level alone: nothing else is opened past what fits
				table.insert(lines, ("... and %d more"):format(#dir.names - i + 1))
				return
			end
			local sub = dir.dirs[name]
			-- A file's name is not hx's: what a terminal would obey goes, and a line break in it is a space.
			local line = indent .. (M.printable(name):gsub("%s+", " "))
			if not sub then
				table.insert(lines, line)
			elseif open[sub] then
				table.insert(lines, line .. "/")
				walk(sub, indent .. "  ")
			elseif sub.files > 0 then
				table.insert(lines, ("%s/ (%d %s)"):format(line, sub.files, sub.files == 1 and "file" or "files"))
			else
				table.insert(lines, line .. "/")
			end
		end
	end
	walk(root, "")
	return table.concat(lines, "\n")
end

-- The files git lists in the project directory, as paths from it, or nil when it lists none. The list goes
-- through a file in $TMPDIR, because host.sh keeps only both ends of a long output.
local function git_files(): { string }?
	local result = host.sh('git ls-files -z --cached --others --exclude-standard 2>/dev/null >"$TMPDIR/hx-files"',
		{ timeout = 10 })
	local text: string? = result.code == 0 and host.read((host.env("TMPDIR") or "") .. "/hx-files") or nil
	local files = {}
	for _, path in (text or ""):split("\0") do
		if path ~= "" then
			table.insert(files, path)
		end
	end
	return #files > 0 and files or nil
end

-- The system prompt's part on the project's files, or nil for none. A project that wants another picture
-- replaces this.
function M.layout(): string?
	if M.layout_lines < 1 then
		return nil
	end
	local files = git_files()
	if files then
		return ("The project's files as this session starts, %d in all, as git lists them: tracked, or new and not "
			.. "ignored. A directory with a count is not opened here.\n%s"):format(#files, M.tree(files, M.layout_lines))
	end
	local entries: { string }? = host.list(".")
	if not entries or #entries == 0 then
		return nil
	end
	return "The project directory as this session starts, its own entries only, since git lists no files here.\n"
		.. M.tree(entries, M.layout_lines)
end

-- Functions -------------------------------------------------------------------------------------------------
--
-- The model has one tool, `lua`, and acts by calling functions from the code it sends. Each function is
-- {name, usage, run}: `usage` is the line the model reads about it, `run` takes the arguments and returns a value,
-- usually the text the model is meant to see, and reports failure by raising. The same functions are there for
-- extensions. Add project functions to this list; the order is the order the model reads them in.

-- The start of a command or a note, on one line, for its progress line.
local function headline(text: string): string
	return M.printable((text:sub(1, 100):gsub("\n", " ")))
end

-- What a host primitive returns, when it worked: its value, or `nil, error`, which raises the error.
local function or_raise<T>(value: T?, err: string?): T
	if not value then
		error(err, 0)
	end
	return value
end

-- Checks one argument, and names the call's whole shape when it is wrong: a model used to JSON tools may pass a
-- table of named arguments where a string belongs.
local function check(fname: string, value: any, position: number, name: string, kind: string, optional: boolean?)
	if value == nil and optional then
		return
	end
	local got = type(value)
	if kind == "whole number" then -- a line number or a count: 2.5, inf and nan are numbers too
		if got == "number" and value % 1 == 0 then
			return
		end
		got = got == "number" and tostring(value) or got
	elseif got == kind then
		return
	end
	local usage = fname
	for _, fn in M.functions do
		if fn.name == fname then
			usage = fn.usage:match("^[^>]*%)") or fn.name
		end
	end
	error(("%s: argument %d (%s) must be a %s, not %s; usage: %s"):format(fname, position, name, kind, got, usage), 0)
end

M.functions = {
	{
		name = "bash",
		usage = "bash(command, timeout?) -> text: runs a bash command in the project directory, a new shell each "
			.. "time, and returns stdout and stderr together, then [exit N]. That last line is hx's, not the "
			.. "command's: to use the output as data, as in a file or a string you build, take host.sh(command).out "
			.. "instead. The command is killed after timeout "
			.. "seconds (default 120). A process left in the background must send its output to a file, and is "
			.. "killed when this session ends, daemons included: nothing you start outlives it.",
		run = function(command: any, timeout: any): string
			check("bash", command, 1, "command", "string")
			check("bash", timeout, 2, "timeout", "number", true)
			progress("  bash " .. headline(command))
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
		run = function(path: any, offset: any, limit: any): string
			check("read", path, 1, "path", "string")
			check("read", offset, 2, "offset", "whole number", true)
			check("read", limit, 3, "limit", "whole number", true)
			if limit and limit < 1 then
				error(("read: limit must be 1 or more, not %d"):format(limit), 0)
			end
			progress("  read " .. M.printable(path))
			local text: string = or_raise(host.read(path))
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
		run = function(path: any, content: any): string
			check("write", path, 1, "path", "string")
			check("write", content, 2, "content", "string")
			progress("  write " .. M.printable(path))
			or_raise(host.write(path, content))
			return ("wrote %d bytes to %s"):format(#content, path)
		end,
	},
	{
		name = "edit",
		usage = "edit(path, old, new, all?) -> text: replaces the exact text old with new in a file. old must occur "
			.. "exactly once, unless all is true, which replaces every occurrence. Copy old from the file without "
			.. "the line numbers read puts in front.",
		run = function(path: any, old: any, new: any, all: any): string
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
			local text: string = or_raise(host.read(path))
			local function replace(from_text: string, to_text: string): ({ string }, number)
				local pieces, count, at = {}, 0, 1
				while true do
					local from, to = text:find(from_text, at, true)
					if not from or not to then -- both or neither
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
			-- Models write line ends as \n. In a file whose line ends are \r\n (its first line's are), new gets them
			-- too, so the file keeps one kind. Where old is not found as written and the file has \r\n ends at all,
			-- old is looked for with them, and new takes them like the text it replaces.
			local function crlf(s: string): string
				return (s:gsub("\r?\n", "\r\n"))
			end
			local pieces, count = replace(old, if text:match("\r?\n") == "\r\n" then crlf(new) else new)
			if count == 0 and text:find("\r\n", 1, true) and old:find("\n", 1, true) then
				pieces, count = replace(crlf(old), crlf(new))
			end
			if count == 0 then
				local hint = old:match("^%s*%d+: ") and " without the line numbers read adds" or ""
				error(("edit: old text not found in %s: read the file again and copy the text exactly%s"):format(path, hint), 0)
			end
			if count > 1 and not all then
				error(("edit: old text occurs %d times in %s: add surrounding lines to make it unique, or pass all = true"):format(count, path), 0)
			end
			or_raise(host.write(path, table.concat(pieces)))
			return ("replaced %d occurrence%s in %s"):format(count, count == 1 and "" or "s", path)
		end,
	},
	{
		name = "skill",
		usage = "skill(name) -> text: the instructions of the skill of that name (case, -, _, . and spaces do not "
			.. "matter), headed by its directory; for a name no skill has, the skills with what each is for.",
		run = function(name: any): string
			check("skill", name, 1, "name", "string")
			if trim(name) == "" then
				error("skill: give a skill's name", 0)
			end
			return M.skill(trim(name))
		end,
	},
	{
		name = "ext",
		usage = "ext(name) -> table: the extension .hx/ext/NAME.luau, or one built into hx.",
		run = function(name: any): any
			return M.ext(name)
		end,
	},
}

-- With --judge, one more function: the only way a change counts.
if host.judged then
	table.insert(M.functions, {
		name = "submit",
		usage = "submit(note?) -> text: hands the project as it stands to the judge, with a note on what you "
			.. "changed and what you expect. The judge builds and measures it beside the champion, the best version "
			.. "so far, with a kit you cannot see, and returns its verdict. Only verdicts count. One submission at a "
			.. "time: with no note, it submits nothing and waits for a verdict still owed.",
		run = function(note: any): string
			check("submit", note, 1, "note", "string", true)
			if note then
				progress("  submit " .. headline(note))
			end
			local lines, pending = host.judge(note and json.encode({ note = note:sub(1, 4000) }) or nil)
			local verdicts = {}
			for _, line in lines do
				local verdict = json.decode(line)
				progress("  " .. M.printable(verdict:match("^[^\n]*")))
				table.insert(verdicts, verdict)
			end
			if pending > 0 then
				table.insert(verdicts, ("The judge is still measuring %d submission%s, longer than one lua call allows. "
					.. "Call submit() with no note to wait for the verdict."):format(pending, pending == 1 and "" or "s"))
			elseif #verdicts == 0 then
				table.insert(verdicts, "No verdict is owed.")
			end
			return table.concat(verdicts, "\n\n")
		end,
	})
end

-- Tools -----------------------------------------------------------------------------------------------------
--
-- The tools the model is offered. The built-in loop has one, `lua`, and a project rarely needs more: a function in
-- M.functions is usually the better choice, since code can call it in a loop or with other calls, and so can
-- extensions. M.tools is still a real extension point, for a tool the model should call directly rather than from
-- code (one a model was trained on, say); a project adds one with
-- table.insert(M.tools, {...}), and the loop checks its arguments against its schema and runs it like `lua`.
-- Each tool is {name, description (text, or a function that returns it), parameters (a JSON schema),
-- run(args) -> text, failed?}. A tool may also report failure by raising; the message goes back to the model.

-- The globals of `lua` code live here, across calls, for the whole run. It is the model's workspace rather than
-- the loop's state, so a nested call shares it on purpose: code a project function runs as a `lua` call sees the
-- globals the model set, as the model's own next call would. A sub-agent's turn shares it too: a call is not told
-- which conversation it answers.
local scratch = setmetatable({}, { __index = lookup })

local function show(value: any): string
	local meta = getmetatable(value)
	if type(meta) == "table" and meta.__tostring then -- a value that says how it reads, as search results do
		return tostring(value)
	end
	local text = if type(value) == "table" then as_json(value) else nil
	return text or tostring(value)
end

local function lua_description(): string
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
	table.insert(lines, "- host.sh(command) -> {out, code}, host.read(path) -> text, host.write(path, text), "
		.. "host.list(dir), json.encode, json.decode: the plain primitives, for working on data.")
	table.insert(lines, "")
	table.insert(lines, "A function that fails raises an error, which stops the code there: what ran before it "
		.. "has happened, and what it printed is shown with the error. Put text with quotes, backslashes or "
		.. "several lines in long brackets, [==[like this]==], which need no escaping; a newline right after the "
		.. "opening bracket is dropped.")
	return table.concat(lines, "\n")
end

local tools: { Tool } = {
	{
		name = "lua",
		description = lua_description,
		parameters = {
			type = "object",
			properties = { code = { type = "string" } },
			required = { "code" },
		},
		run = function(args: { [string]: any }): (string, boolean)
			if type(args.code) ~= "string" then
				error('argument "code" must be a string', 0)
			end
			local lines: { string } = {}
			scratch.print = capture_print -- again each call, in case the model's code assigned print
			local chunk: (() -> ...any)?, err: string? = host.load(args.code, "=lua", scratch)
			if not chunk then
				return "error: " .. tostring(err), true
			end
			-- The outer call's lines are put back before any other call or return: on a stop the host raises again
			-- at the first of those, and this call's lines must not be left in the outer call's place.
			local outer = live.printed
			live.printed = lines
			local ok, results = host.timed(M.call_seconds, function()
				return table.pack(chunk())
			end)
			live.printed = outer
			if not ok then
				table.insert(lines, "error: " .. tostring(results))
			else
				for i = 1, results.n do
					table.insert(lines, show(results[i]))
				end
			end
			local failed = not ok
			return #lines > 0 and clip(table.concat(lines, "\n")) or "[no output]", failed
		end,
	},
}
M.tools = tools

-- Checks a call's argument names against the tool's own schema. The message names what was sent and what the
-- tool takes, so a model that reached for another harness's names can see its mistake. Returns nil or it. A
-- schema without `properties` names no arguments, so then any name is accepted.
local function argument_problem(tool: Tool, args: { [string]: any }): string?
	local schema: Schema = tool.parameters or {}
	local properties, required: { string } = schema.properties, schema.required or {}
	local takes, optional = table.clone(required), {}
	if properties then
		for name in properties do
			if not table.find(required, name) then
				table.insert(optional, name)
			end
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

local function find_tool(name: string?): Tool?
	for _, tool in M.tools do
		if tool.name == name then
			return tool
		end
	end
	return nil
end

-- Runs one tool_use block from the model. Returns the text of the result and whether it is an error. Nothing a
-- tool does can end the run; a stop does, because the host raises it again as soon as this returns.
local function run_tool(call: Block): (string, boolean)
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

-- What the loop knows of M.model, the one place that decides. "Is this Claude" has two answers, and each use
-- needs a different one:
--   any claude-* model speaks the Claude API, so it gets the system prompt as a block with cache_control, which
--     every Claude model accepts (DeepSeek ignores the field and gets a plain string)
--   only a model listed in M.claude gets the newer features of its entry; an older Claude model gets none
-- Returns whether the model is Claude, and its M.claude entry or nil.
local function claude_model(): (boolean, Features?)
	return M.model:match("^claude%-") ~= nil, M.claude[M.model]
end

-- Tokens a request put in front of the model, cached or not.
local function prompt_tokens(counts: Usage): number
	return (counts.input_tokens or 0) + (counts.cache_read_input_tokens or 0) + (counts.cache_creation_input_tokens or 0)
end

-- Adds a reply's tokens to the run's count. A compaction reply has zero at the top and the summarization call in
-- usage.iterations; where iterations are given, they are what was billed.
local function count_usage(reply: Reply)
	local counts: Usage = type(reply.usage) == "table" and reply.usage or {}
	local iterations = counts.iterations
	local parts: { Usage } = if type(iterations) == "table" and #iterations > 0 then iterations else { counts }
	local usage = live.usage
	usage.replies += 1
	for _, part in parts do
		usage.input += prompt_tokens(part)
		usage.cached += part.cache_read_input_tokens or 0
		usage.written += part.cache_creation_input_tokens or 0
		usage.output += part.output_tokens or 0
	end
end

local function tool_specs(): { any }
	local specs = json.array({}) -- a list even when a project has taken every tool away
	for _, tool in M.tools do
		local description = type(tool.description) == "function" and tool.description() or tool.description
		table.insert(specs, { name = tool.name, description = description, input_schema = tool.parameters })
	end
	return specs
end

-- Whether a reply has anything in it. Thinking alone counts; an empty text block does not. A reply cut off by
-- the output limit (the loop asks it to go on) or refused (that is the answer) counts however empty it is.
local function has_content(reply: Reply): boolean
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
local function after_fallback(reply: Reply)
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
function M.request(system: string, messages: { Message }, compaction: { [string]: any }?): Reply
	local is_claude, features = claude_model()
	local thinking = { type = "adaptive" }
	local betas = {}
	local fallbacks = nil
	if features then
		thinking.block_binding = { prefix_mismatch_behavior = "drop_block" }
		table.insert(betas, "thinking-binding-controls-2026-08-01")
		if features.display then
			thinking.display = features.display
			table.insert(betas, "thinking-display-updates-2026-08-18")
		end
		if features.fallbacks then
			fallbacks = features.fallbacks
			table.insert(betas, "server-side-fallback-2026-07-01")
		end
		-- On every request, not only the one that asks for the summary: a request that carries the block needs it.
		if features.compaction then
			table.insert(betas, "compact-2026-09-04")
		end
	end
	local effort = M.effort or (features and features.effort)
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
	local system_field: string | { { [string]: any } } = system
	if is_claude then
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
		local status, text, retry_after = host.llm(body, options)
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
		-- As long as the server asks (Retry-After on a 429 or 503), within reason.
		local wait = math.min(math.max(2 ^ attempt, retry_after or 0), 120)
		progress(("model request failed (%s), retrying in %d s"):format(tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

function M.system_prompt(): string
	-- hx does not bring ripgrep. Where it is installed, the model is told: none reaches for it unasked.
	local ripgrep = host.sh("command -v rg", { timeout = 10 }).code == 0
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
			.. "the project, so write one when you notice a chore you will meet again, and keep it as small as the chore. "
			.. "A procedure you will follow again, rather than code, is a skill: .hx/skills/NAME/SKILL.md, with front "
			.. "matter giving its name and a description of when to use it."
			-- The skill hx is built with says the rest. Without the pointer a model takes this paragraph for all
			-- there is to know, and never loads it.
			.. (host.skills.hx and " skill(\"hx\") says which of these a need calls for and how each is written "
				.. "and tried, and covers the loop, sub-agents and scripts: load it before you add to the harness." or ""),
		"Use read(), not bash() with cat, sed, head or tail, to look at text files; its offset and limit read a "
			.. "range of lines or go on through a large file. Read a file before you edit it, unless "
			.. "you wrote or edited it yourself in this session."
			.. (ripgrep and " To search, ripgrep is installed: use rg through bash rather than plain grep. It is "
				.. "faster, and it skips what .gitignore names." or ""),
		"When the task is done, finish with a short report: what you changed, what you checked, what is left.",
	}
	if host.judged then
		table.insert(parts, "This session is judged. A judge outside your wall measures the project with a kit "
			.. "you cannot read: it builds it, runs it beside the champion, the best version so far, in pairs, and "
			.. "keeps a change only when it is better by more than a margin and holds up again on fresh runs. "
			.. "Your own measurements guide you; only submit() decides. Work one idea at a time: make the change, "
			.. "check with your own tests that it still works, then submit it. After a win, commit it with git so "
			.. "you can come back to it; after any other verdict, put the project back to the last win before the "
			.. "next idea, unless you mean to build on this one. Work that was never submitted does not count, so do "
			.. "not finish without a verdict on your best change. Keep going while you have ideas with a real chance, "
			.. "then report each verdict and what the champion holds.")
	end
	local extensions = extension_index()
	if #extensions > 0 then
		table.insert(parts, "Extensions:\n" .. table.concat(extensions, "\n"))
	end
	local skills = skill_index()
	if skills then
		table.insert(parts, skills)
	end
	local layout = M.layout()
	if layout then
		table.insert(parts, layout)
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
-- Written with typeof(setmetatable(...)) rather than setmetatable<...>, which only the new type solver knows.
type ConversationFields = { messages: { Message }, on_answer: ((Conversation) -> ())? }
type Conversation = typeof(setmetatable({} :: ConversationFields, Conversation))
type State = "idle" | "ready" | "owing"

-- on_answer, if set, is called with the conversation after each answer: after every reply that called tools and
-- their results. M.main sets it to save the log, so a run killed half-way still leaves one; it is here rather than
-- in M.turn so that it goes on when a project replaces M.turn. A fork does not carry it.
function Conversation.new(): Conversation
	return setmetatable({ messages = {}, on_answer = nil }, Conversation)
end

local function tool_calls(message: Message): { Block }
	local calls = {}
	for _, block in message.content :: { Block } do -- only a reply is asked, and a reply's content is a list
		if block.type == "tool_use" then
			table.insert(calls, block)
		end
	end
	return calls
end

function Conversation.state(self: Conversation): State
	local last = self.messages[#self.messages]
	if not last then
		return "idle"
	elseif last.role == "user" then
		return "ready"
	end
	if #tool_calls(last) > 0 then
		return "owing"
	end
	return "idle"
end

local function expect(self: Conversation, operation: string, ...: State)
	local now = self:state()
	for _, allowed in { ... } do
		if now == allowed then
			return
		end
	end
	error(("conversation: cannot %s while %s"):format(operation, now), 3)
end

-- Adds text from the user. After tool results or earlier text it joins that message, behind what is there.
function Conversation.say(self: Conversation, text: string)
	expect(self, "say", "idle", "ready")
	local last = self.messages[#self.messages]
	if not last or last.role ~= "user" then
		table.insert(self.messages, { role = "user", content = text })
		return
	end
	local content = last.content
	local blocks: { Block } = if type(content) == "string" then { { type = "text", text = content } } else content
	last.content = blocks
	table.insert(blocks, { type = "text", text = text })
end

-- Records the model's reply exactly as it came (thinking blocks have to go back unchanged) and returns its
-- tool calls. An empty reply records nothing: the user's message is still the last word.
function Conversation.reply(self: Conversation, content: { Block }): { Block }
	expect(self, "reply", "ready")
	if #content == 0 then
		return {}
	end
	local message: Message = { role = "assistant", content = content }
	table.insert(self.messages, message)
	return tool_calls(message)
end

-- Answers every tool call of the last reply with what run(call) returns: the result text, and whether it is an
-- error. If run raises, nothing is recorded and the calls are still owed.
function Conversation.answer(self: Conversation, run: (Block) -> (string, boolean?))
	expect(self, "answer", "owing")
	local results: { Block } = {}
	for _, call in tool_calls(self.messages[#self.messages]) do
		local text, failed = run(call)
		local is_error: boolean? = failed or nil -- the field is left out, not false, when the call worked
		table.insert(results, { type = "tool_result", tool_use_id = call.id, content = text, is_error = is_error })
	end
	table.insert(self.messages, { role = "user", content = results })
	if self.on_answer then
		self.on_answer(self)
	end
end

-- Answers whatever is owed with an error, so a turn stopped half-way leaves a history that can continue.
function Conversation.abandon(self: Conversation, reason: string)
	if self:state() == "owing" then
		self:answer(function()
			return "error: " .. reason, true
		end)
	end
end

-- Forgets everything and starts again from one user message.
function Conversation.restart(self: Conversation, text: string)
	expect(self, "restart", "idle", "ready")
	table.clear(self.messages)
	self:say(text)
end

-- Forgets everything and starts again from a compaction block, which the API reads in place of the messages it
-- summarized, followed by one user message. The block goes first in the first message, exactly as it came.
function Conversation.resume(self: Conversation, block: Block, text: string)
	expect(self, "resume", "idle", "ready")
	table.clear(self.messages)
	table.insert(self.messages, { role = "user", content = { block, { type = "text", text = text } } })
end

-- A copy to try something on. It shares the old messages; the last one, which say() may extend, is its own.
function Conversation.fork(self: Conversation): Conversation
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
function Conversation.problem(self: Conversation): string?
	local owed: { Block }? = nil -- the calls the next message has to answer
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
			local content = message.content
			local blocks: { Block } = if type(content) == "table" then content else {} -- text alone answers nothing
			for j, block in blocks do
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

local function text_of(reply: Reply): string
	local parts = {}
	for _, block in reply.content do
		if block.type == "text" then
			table.insert(parts, block.text)
		end
	end
	return table.concat(parts, "\n")
end

-- Sends the conversation to the model. Only a conversation that is ready, and sound, is ever sent.
local function ask(system: string, talk: Conversation, compaction: { [string]: any }?): Reply
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

-- Sends the request for a summary. One that fails leaves the conversation as it is, to be compacted after a later
-- reply, on any model: losing the summary must not lose the task. Returns the reply, or nil.
local function ask_for_summary(system: string, talk: Conversation, compaction: { [string]: any }?): Reply?
	local ok, reply = pcall(ask, system, talk, compaction)
	if not ok then
		progress("  compaction failed, the conversation stays as it is: " .. tostring(reply))
		return nil
	end
	return reply
end

-- On-demand compaction, for the Claude models whose M.claude entry says so. The request is the conversation as it
-- stands, with the same model, system, tools, thinking and effort, so it reads the cached prefix, plus
-- `compaction`; M.compact_prompt is its `instructions`, which replace the API's own summarization prompt. The API
-- writes the summary into a signed `compaction` block and no reply. The block then takes the place of every
-- message it summarized: it goes first, exactly as it came, and is sent with every later request. A newer block
-- summarizes the old one with what came after it. A request that brings no block (cut off, refused, a tool call)
-- or fails leaves the conversation as it is, to be compacted after a later reply, as the docs advise.
local function compact_on_server(system: string, talk: Conversation)
	local reply = ask_for_summary(system, talk, { type = "summarize", instructions = M.compact_prompt })
	if not reply then
		return
	end
	local block: Block? = nil
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
function M.compact(system: string, talk: Conversation)
	progress("  compacting the conversation")
	local _, features = claude_model()
	if features and features.compaction then
		compact_on_server(system, talk)
		return
	end
	local asking = talk:fork()
	asking:say(M.compact_prompt)
	local reply = ask_for_summary(system, asking, nil)
	if not reply then
		return
	end
	local summary = text_of(reply)
	if summary == "" then
		progress("  no summary came back; keeping the conversation as it is")
		return
	end
	talk:restart(summary .. "\n\n" .. M.resume_prompt) -- the same words as behind a compaction block
end

-- What the model is told when a function call has returned the same result `count` times in this task (see
-- `counted`), as `call`, the call written out. `first` is true at the lowest threshold of M.repeat_reminders; a
-- loop inside one call that passes several at once gets the later wording.
function M.repeat_reminder(call: string, count: number, first: boolean): string
	if #call > 500 then
		call = call:sub(1, 500) .. ("... (%d more bytes)"):format(#call - 500)
	end
	if first then
		return ("%s has now returned the same result %d times. If it is something you are waiting on rather than "
			.. "something your own work changes, stop now and report what you found, without looking further. "
			.. "Otherwise change what you do, not what you check, before you run it again."):format(call, count)
	end
	return ("%s has returned the same result %d times in this task, so asking again is not making progress. Do not "
		.. "call it again, and do not change what it checks to make it give another answer. Unless you know an "
		.. "action that will change the result, finish now and say what you found."):format(call, count)
end

-- Prints the text of a reply: the model's words are the answer, on stdout. A model asked for progress updates
-- (M.claude's display) writes its notes between tool calls into thinking blocks; those are progress lines.
local function show_text(reply: Reply)
	local _, features = claude_model()
	local updates = features and features.display == "updates"
	for _, block in reply.content do
		if block.type == "text" and block.text ~= "" then
			local text = block.text :: string -- a text block has its text
			print(host.tty and M.printable(text) or text)
		elseif updates and block.type == "thinking" and type(block.thinking) == "string" and block.thinking ~= "" then
			progress("  " .. M.printable(block.thinking))
		end
	end
end

-- The body of M.turn, which gives it a count of repeats of its own. Returns the reply that ended the turn, which
-- M.turn does not pass on: it is there so that the type of pcall(turn, ...) has a second value, the error.
local function turn(system: string, talk: Conversation): Reply
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
			local worst, due = live.repeats.worst, nil
			for i, at in M.repeat_reminders do
				if worst and worst.count >= at and at > reminded then
					due = i
				end
			end
			if due and worst then -- due is set only when there is a worst
				reminded = M.repeat_reminders[due]
				talk:say(M.repeat_reminder(worst.call, worst.count, due == 1))
			end
			if prompt_tokens(reply.usage or {}) > M.compact_at then
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
			return reply
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

-- Runs the model until it answers without asking for a tool. The turn counts repeats afresh, and puts the outer
-- turn's counts back when it ends, however it ends: as with printed lines in the lua tool, before any other call.
function M.turn(system: string, talk: Conversation)
	local outer = live.repeats
	live.repeats = { counts = {}, worst = nil }
	local ok, err = pcall(turn, system, talk)
	live.repeats = outer
	if not ok then
		error(err, 0)
	end
end

-- With a .hx directory, each session's conversation is kept in .hx/log/ as JSON. The name starts with the time, so
-- the logs sort by it, and ends with the random part of this run's $TMPDIR (hx-XXXXXX), which no other running hx
-- shares, so two sessions started in the same second keep two logs. Whether .hx exists is asked at each save, not
-- once at the start: a session that makes it, say by writing its first extension, keeps its log too.
local log_path = (".hx/log/%s-%s.json"):format(os.date("%Y%m%d-%H%M%S"),
	(host.env("TMPDIR") :: string? or ""):match("([%w]+)/*$") or "0")
local function save(system: string, messages: { Message })
	if host.list(".hx") then
		-- Logs hold file contents, so the directory keeps itself out of git, however .hx/ came to exist.
		if not host.read(".hx/log/.gitignore") then
			host.write(".hx/log/.gitignore", "*\n")
		end
		host.write(log_path, json.encode({ system = system, messages = messages }))
	end
end

local function report()
	-- A healthy run reads most of what goes in from the cache and writes little of it. DeepSeek reports no writes.
	local usage = live.usage
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
function M.read_message(read: () -> string?): string?
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

function M.main(): number
	local system = M.system_prompt()
	local talk = Conversation.new()
	-- Encoding even a long conversation takes milliseconds, next to seconds for a reply.
	talk.on_answer = function()
		save(system, talk.messages)
	end
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
					host.log(M.printable("[the turn failed: " .. tostring(err) .. "]"))
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
