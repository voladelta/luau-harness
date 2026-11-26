--!strict

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
	source: any,
}
type Message = { role: string, content: string | { Block } }
type Usage = {
	input_tokens: number?,
	cache_read_input_tokens: number?,
	cache_creation_input_tokens: number?,
	output_tokens: number?,
	iterations: { Usage }?,
}
type Reply = { content: { Block }, stop_reason: string?, stop_details: any, usage: Usage? }
type Function = { name: string, usage: string, run: (...any) -> ...any, lead: boolean? }
type Schema = { type: string?, properties: { [string]: any }?, required: { string }? }
type Tool = {
	name: string,
	description: string | () -> string,
	parameters: Schema?,
	run: ({ [string]: any }) -> (string, boolean?, { Block }?),
}
type Skill = { name: string, description: string, dir: string, path: string, hidden: boolean, source: string? }
type Features = { effort: string?, fallbacks: string?, display: string?, compaction: boolean? }

local M = {}

M.model = host.model
M.max_output = 16000
M.max_replies = 200
M.compact_at = 150000
M.output_limit = 30000
M.call_seconds = 600
M.effort = host.env("HX_EFFORT")
M.repeat_reminders = { 3, 5, 8 }

type Repeats = { counts: { [string]: number }, worst: { call: string, count: number }? }
type Count = { model: string, replies: number, input: number, cached: number, written: number, output: number }
local live: {
	usage: { Count },
	spills: number,
	subs: number,
	turns: number,
	ext_cache: { [string]: { source: string, value: any } },
	wrappers: { [Function]: (...any) -> ...any },
	repeats: Repeats,
	printed: { string }?,
	agent: { name: string, run: number }?,
	seen: { [string]: string },
	wrote: { string },
	shared_since: number?,
	images: { Block }?,
} = {
	usage = {},
	spills = 0,
	subs = 0,
	turns = 0,
	ext_cache = {},
	wrappers = {},
	repeats = { counts = {}, worst = nil },
	printed = nil,
	agent = nil,
	seen = {},
	wrote = {},
	shared_since = nil,
	images = nil,
}
setmetatable(live.wrappers, { __mode = "k" })

local function progress(text: string)
	if not host.quiet then
		local agent = live.agent
		if agent then
			local head = ("[%s] "):format(agent.name)
			text = head .. text:gsub("^%s+", ""):gsub("\n", "\n" .. head)
		end
		host.log(M.printable(text))
	end
end

local function utf8_prefix(text: string, limit: number): string
	if limit <= 0 then
		return ""
	end
	if #text <= limit then
		return text
	end
	local cut = limit
	local next_byte = text:byte(cut + 1)
	if next_byte and next_byte >= 0x80 and next_byte < 0xC0 then
		while cut > 0 do
			local b = text:byte(cut)
			cut -= 1
			if b < 0x80 or b >= 0xC0 then
				break
			end
		end
	end
	return text:sub(1, cut)
end

local function clip(text: string): string
	local head: string, tail: string? = host.clip(text, M.output_limit)
	if tail == nil then
		return text
	end
	live.spills += 1
	local agent = live.agent
	local tmpdir: string = host.env("TMPDIR") or ""
	local owner = if agent then ("%s-%d-"):format(agent.name, agent.run) else ""
	local path = if tmpdir ~= "" then ("%s/hx-output-%s%d.txt"):format(tmpdir, owner, live.spills) else nil
	local where: string = if path and host.write(path, text) then "; the whole output is in " .. path else ""
	return head .. (head:sub(-1) == "\n" and "" or "\n")
		.. ("[... %d bytes omitted%s ...]\n"):format(#text - #head - #tail, where) .. tail
end

M.printable = host.printable :: (string) -> string

local ext_dir = ".hx/ext/"

local function as_json(value: any): string?
	local ok, text = pcall(json.encode, value == nil and json.null or value)
	return ok and text or nil
end

local function digest(text: string): string
	if #text <= 256 then
		return text
	end
	local data, hash, tail = buffer.fromstring(text), 0, #text - #text % 4
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
		local result = results[1]
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

local function read_only(name: string, real: { [any]: any }, own: { [string]: any }?): any
	local view = {}
	setmetatable(view, table.freeze({
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
	}))
	return table.freeze(view)
end

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

local views: { [any]: any } = {}

local function offered(fn: Function): boolean
	return not (fn.lead and live.agent)
end

local function lookup(env: any, name: string): any
	for _, fn in M.functions do
		if fn.name == name and offered(fn) then
			local wrapper = live.wrappers[fn] or counted(fn)
			live.wrappers[fn] = wrapper
			return wrapper
		end
	end
	if name == "print" then
		return capture_print
	elseif name == "_G" then
		return env
	elseif name == "getfenv" or name == "setfenv" then
		return nil
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
		setmetatable({}, { __index = lookup }))
	if not chunk then
		error(load_err, 0)
	end
	local value = chunk()
	live.ext_cache[name] = { source = source, value = value }
	return value
end

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
	local keyed = { exa = host.endpoints.exa, web = host.endpoints.search, ocr = host.endpoints.ocr }
	for name in host.extensions do
		if keyed[name] ~= false then
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

local home = host.env("HOME")
M.skill_dirs = { ".hx/skills/", ".agents/skills/", ".claude/skills/" }
if home and home:sub(1, 1) == "/" then
	home = home:gsub("/+$", "")
	table.insert(M.skill_dirs, home .. "/.agents/skills/")
	table.insert(M.skill_dirs, home .. "/.claude/skills/")
end
M.skill_index_full = 30
M.skill_listed = 50
M.skill_description_limit = 250

local function trim(text: string): string
	return (text:gsub("^%s+", ""):gsub("%s+$", ""))
end

function M.skill_front_matter(text: string): ({ [string]: string }, string)
	text = text:gsub("^\239\187\191", ""):gsub("\r\n", "\n")
	local head, rest = text:match("^%-%-%-[ \t]*\n(.-)\n%-%-%-[ \t]*\n(.*)$")
	head = head or text:match("^%-%-%-[ \t]*\n(.-)\n%-%-%-[ \t]*$")
	if not head then
		return {}, text
	end
	local body = rest or ""
	local fields: { [string]: string }, lines, i = {}, head:split("\n"), 1
	while i <= #lines do
		local key, matched = lines[i]:match("^([%w_%-]+):%s*(.-)%s*$")
		i += 1
		if key and matched then
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
					value = value:sub(2, -2):gsub("\\(.)", function(c)
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

local function skill_key(name: string): string
	return (name:lower():gsub("[%-_%.%s]", ""))
end

function M.skills(): { Skill }
	local skills: { Skill }, seen = {}, {}
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
	elseif not length and #text > limit then
		text = text:sub(1, limit) .. " ..."
	end
	return text
end

local function skill_line(skill: Skill): string
	local description = skill.description ~= "" and cut(skill.description, M.skill_description_limit)
		or "(no description)"
	return ("- %s%s: %s"):format(skill.name, skill.source and " (built in)" or "", description)
end

M.skill_files_shown = 30

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

local function listed_skills(): { Skill }
	local listed = {}
	for _, skill in M.skills() do
		if not skill.hidden then
			table.insert(listed, skill)
		end
	end
	return listed
end

function M.skill(name: string?): string
	local skills = M.skills()
	if #skills == 0 then
		return "There are no skills. A skill is a directory with a SKILL.md, looked for in "
			.. table.concat(M.skill_dirs, ", ") .. "."
	end
	local found: { Skill } = {}
	for _, skill in skills do
		if name and skill.name == name then
			found = { skill }
			break
		end
	end
	if name and #found == 0 then
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
		if skill.source then
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
	local lines = { if name then ("No skill is named %q."):format(name) else "The skills:" }
	if #listed == 0 then
		return if name then lines[1] else "No skill is listed."
	end
	if name then
		table.insert(lines, "Pick one of these and load it by its name:")
	end
	for i = 1, math.min(#listed, M.skill_listed) do
		table.insert(lines, skill_line(listed[i]))
	end
	if #listed > M.skill_listed then
		table.insert(lines, ("and %d more, named in the system prompt"):format(#listed - M.skill_listed))
	end
	return table.concat(lines, "\n")
end

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
	return ("%s There are %d: %s. To see what they are for, call skill() with no name, which lists them with "
		.. "their descriptions."):format(head, #listed, table.concat(names, ", "))
end

M.layout_lines = 60

type Dir = { names: { string }, dirs: { [string]: Dir }, files: number }

function M.tree(paths: { string }, limit: number): string
	local root: Dir = { names = {}, dirs = {}, files = 0 }
	local previous: string? = nil
	for _, path in paths do
		if path ~= previous then
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
	while at < #queue do
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
			if #lines == limit then
				table.insert(lines, ("... and %d more"):format(#dir.names - i + 1))
				return
			end
			local sub = dir.dirs[name]
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

local function headline(text: string): string
	return M.printable((text:sub(1, 100):gsub("\n", " ")))
end

local function or_raise<T>(value: T?, err: string?): T
	if not value then
		error(err, 0)
	end
	return value
end

local function tracked(path: string): (string, { type: string, mtime: number, revision: string, target: string? }?)
	local pending = M.ext("path").join(host.cwd or "/", path):split("/")
	local resolved, hops = "", 0
	while #pending > 0 do
		local part = table.remove(pending, 1) :: string
		if part == ".." then
			resolved = resolved:match("^(.*)/[^/]*$") or ""
		elseif part ~= "" and part ~= "." then
			local stat = host.stat(resolved .. "/" .. part)
			if stat and stat.type == "link" and stat.target and hops < M.link_hops then
				hops += 1
				local target = stat.target:split("/")
				if stat.target:sub(1, 1) == "/" then
					resolved = ""
				end
				pending = table.move(pending, 1, #pending, #target + 1, target)
			else
				resolved ..= "/" .. part
			end
		end
	end
	local whole = if resolved == "" then "/" else resolved
	return whole, host.stat(whole)
end

local function file_clock(): number
	local marker = ("%s/hx-clock%s"):format(host.env("TMPDIR") or "/tmp", if live.agent then "-" .. live.agent.name else "")
	local stat = host.write(marker, "") and host.stat(marker)
	return if stat then stat.mtime else os.time() + 1
end

local function saw(path: string)
	local key, stat = tracked(path)
	if stat then
		live.seen[key] = stat.revision
	end
end

local function wrote(path: string)
	saw(path)
	if not table.find(live.wrote, path) then
		table.insert(live.wrote, path)
	end
end

local function unseen_change(path: string): string?
	local key, stat = tracked(path)
	if not stat or stat.type ~= "file" then
		return nil
	end
	local seen = live.seen[key]
	if seen then
		if seen ~= stat.revision then
			return ("write: %s has changed since you last read or wrote it, and this would overwrite a change you "
				.. "have not seen. Read it again, then write it, or edit it."):format(path)
		end
	elseif live.shared_since and stat.mtime > live.shared_since then
		return ("write: %s was written while agents share this directory, and not by your write or edit: this "
			.. "may overwrite another's work. Read it first. If it is not yours to change, leave it and say so "
			.. "in your report."):format(path)
	end
	return nil
end

local function check(fname: string, value: any, position: number, name: string, kind: string, optional: boolean?)
	if value == nil and optional then
		return
	end
	local got = type(value)
	if kind == "whole number" then
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

M.vision = true
M.image_bytes = 5 * 1024 * 1024
M.images_per_call = 8
M.call_image_bytes = 20 * 1024 * 1024

local image_kinds = {
	{ type = "image/png", starts = "\137PNG\r\n\26\n" },
	{ type = "image/jpeg", starts = "\255\216\255" },
	{ type = "image/gif", starts = "GIF8" },
	{ type = "image/webp", starts = "RIFF", at_9 = "WEBP" },
}

function M.image(data: string): Block?
	for _, kind in image_kinds do
		if data:sub(1, #kind.starts) == kind.starts and (not kind.at_9 or data:sub(9, 12) == kind.at_9) then
			return {
				type = "image",
				source = { type = "base64", media_type = kind.type, data = M.ext("bytes").base64(data) },
			}
		end
	end
	return nil
end

M.functions = {
	{
		name = "bash",
		usage = "bash(command, timeout?) -> text: runs a bash command in the project directory, a new shell each "
			.. "time, and returns stdout and stderr together, then [exit N], for you to read. Output you will use "
			.. "as data comes from host.sh, below. The command is killed after timeout "
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
			saw(path)
			if text == "" then
				return "[empty file]"
			end
			local first = math.max(1, offset or 1)
			local until_line = first + (limit or 2000) - 1
			local budget = math.max(0, M.output_limit - 200)
			local shown = {}
			local at, total, last, size = 1, 0, first - 1, 0
			while at <= #text do
				local newline = text:find("\n", at, true) or #text + 1
				total += 1
				if total >= first and total <= until_line then
					local stop = newline - 1
					if stop >= at and text:byte(stop) == 13 then
						stop -= 1
					end
					local length = stop - at + 1
					local number = ("%d: "):format(total)
					if total == first and length > budget then
						local prefix = utf8_prefix(text:sub(at, at + budget), budget)
						table.insert(shown, number .. prefix)
						table.insert(shown, ("[line %d is %d bytes; only the first %d are shown]"):format(total, length, #prefix))
						last, until_line = total, total
					elseif total == first or size + 1 + #number + length <= budget then
						table.insert(shown, number .. text:sub(at, stop))
						size += (if total == first then 0 else 1) + #number + length
						last = total
					else
						until_line = last
					end
				end
				at = newline + 1
			end
			if first > total then
				error(("offset %d is past the end: the file has %d lines"):format(first, total), 0)
			end
			if last < total then
				table.insert(shown, ("[lines %d-%d of %d; offset=%d goes on]"):format(first, last, total, last + 1))
			elseif first > 1 then
				table.insert(shown, ("[lines %d-%d of %d]"):format(first, last, total))
			end
			return table.concat(shown, "\n")
		end,
	},
	{
		name = "write",
		usage = "write(path, content) -> text: creates or overwrites a file. Missing directories are created. For a "
			.. "change to part of an existing file, edit is cheaper and safer. It refuses to overwrite a file that "
			.. "has changed since you last read or wrote it: read it again first.",
		run = function(path: any, content: any): string
			check("write", path, 1, "path", "string")
			check("write", content, 2, "content", "string")
			progress("  write " .. M.printable(path))
			local changed = unseen_change(path)
			if changed then
				error(changed, 0)
			end
			or_raise(host.write(path, content))
			wrote(path)
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
					if not from or not to then
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
			wrote(path)
			return ("replaced %d occurrence%s in %s"):format(count, count == 1 and "" or "s", path)
		end,
	},
	{
		name = "see",
		usage = "see(path) -> text: puts an image file, PNG, JPEG, GIF or WebP, into this call's result for you to "
			.. "look at: a screenshot, a chart, a rendered page. It is for looking; text in a document comes from "
			.. "read, or from a tool that extracts it.",
		run = function(path: any): string
			check("see", path, 1, "path", "string")
			progress("  see " .. M.printable(path))
			if not M.vision then
				error("see: this session's model takes no images", 0)
			end
			local images = live.images
			if not images then
				error("see: an image is shown in the result of the lua call that asks for it, and this is not one", 0)
			end
			if #images >= M.images_per_call then
				error(("see: one call shows %d images at most; look at these first"):format(M.images_per_call), 0)
			end
			local data: string = or_raise(host.read(path))
			local sent = 4 * math.ceil(#data / 3)
			if sent > M.image_bytes then
				error(("see: %s is %d bytes, %d as base64, and an image may be %d at most: scale it down first")
					:format(path, #data, sent, M.image_bytes), 0)
			end
			local total = sent
			for _, shown in images do
				total += #shown.source.data
			end
			if total > M.call_image_bytes then
				error(("see: the images of one call may be %d bytes as base64 at most; look at these first")
					:format(M.call_image_bytes), 0)
			end
			local image = M.image(data)
			if not image then
				error(("see: %s is not a PNG, JPEG, GIF or WebP image"):format(path), 0)
			end
			table.insert(images, image)
			return ("[image %d of this result: %s, %s, %d bytes]"):format(#images, path, image.source.media_type, #data)
		end,
	},
	{
		name = "skill",
		usage = "skill(name?) -> text: the instructions of the skill of that name (case, -, _, . and spaces do not "
			.. "matter), headed by its directory; with no name, the skills with what each is for.",
		run = function(name: any): string
			check("skill", name, 1, "name", "string", true)
			if name == nil then
				return M.skill()
			end
			if trim(name) == "" then
				error("skill: give a skill's name, or nothing for the list", 0)
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

if host.judged then
	table.insert(M.functions, {
		name = "submit",
		lead = true,
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

local scratch = setmetatable({}, { __index = lookup })

local function show(value: any): string
	local meta = getmetatable(value)
	if type(meta) == "table" and meta.__tostring then
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
		if offered(fn) then
			table.insert(lines, "- " .. fn.usage)
		end
	end
	table.insert(lines, "- host.sh(command, {split?, input?, timeout?, cwd?, env?}) -> {out, err, code}: runs a "
		.. "command for its output as data, with nothing of hx's added, whole up to 16 MiB; send more to a file and "
		.. "host.read it. With split = true, out is stdout alone and err is stderr, so a warning cannot spoil what "
		.. "you parse; otherwise out is both together and there is no err. Parse out in your code, with "
		.. "json.decode or string patterns, not in a python or jq pipeline. "
		.. "command is a bash string, or a list {program, arg, ...} whose words reach the program as they are, with "
		.. "no quoting; input is text for its stdin; cwd is the directory it runs in; env is {NAME = value} to set, "
		.. "false to take one away.")
	table.insert(lines, "- host.walk(dir, {glob?, depth?}) -> {path}: every path under dir, sorted, .git left out, "
		.. "directories ending in /; glob as \"**/*.luau\". host.stat(path) -> {type, size, mtime, mode, revision}, or nil.")
	table.insert(lines, "- host.read(path) -> text, host.write(path, text), host.list(dir), json.encode(value, "
		.. "{indent?}), json.decode(text): the other plain primitives, for working on data.")
	table.insert(lines, "")
	table.insert(lines, "Do the work in Luau: parsing, counting, deciding and building text and files belong in "
		.. "your code, with string patterns, string.pack and string.unpack for binary data, ext(\"bytes\") for hex, "
		.. "base64, sha256 and hmac, ext(\"text\") to replace literal "
		.. "text without pattern traps, ext(\"path\") for paths, ext(\"http\") for HTTP as data, and os.date. "
		.. "Commands run programs: the build, tests, git, rg, curl, and tools that turn data into data, such as bc "
		.. "for exact arithmetic on "
		.. "large integers. Join them in Luau: json.decode(host.sh({\"curl\", \"-sfL\", url}, {split = true}).out), or "
		.. "host.sh({\"bc\"}, {input = expression}).out. "
		.. "Write a script in another language only for a library the job needs that neither Luau nor the "
		.. "system's tools have, and pass it its data through input or a file.")
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
		run = function(args: { [string]: any }): (string, boolean, { Block }?)
			if type(args.code) ~= "string" then
				error('argument "code" must be a string', 0)
			end
			local lines: { string } = {}
			local images: { Block } = {}
			scratch.print = capture_print
			local chunk: (() -> ...any)?, err: string? = host.load(args.code, "=lua", scratch)
			if not chunk then
				return "error: " .. tostring(err), true
			end
			local outer, outer_images = live.printed, live.images
			live.printed, live.images = lines, images
			local ok, results = host.timed(M.call_seconds, function()
				return table.pack(chunk())
			end)
			live.printed, live.images = outer, outer_images
			if not ok then
				table.insert(lines, "error: " .. tostring(results))
			else
				for i = 1, results.n do
					table.insert(lines, show(results[i]))
				end
			end
			local failed = not ok
			return #lines > 0 and clip(table.concat(lines, "\n")) or "[no output]", failed, #images > 0 and images or nil
		end,
	},
}
M.tools = tools

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

local function run_tool(call: Block): (string, boolean, { Block }?)
	local tool = find_tool(call.name)
	if not tool then
		local problem = "error: there is no tool named " .. tostring(call.name)
		for _, fn in M.functions do
			if fn.name == call.name and find_tool("lua") then
				problem ..= (": %s is a function, for code sent to the lua tool to call, as in %s")
					:format(fn.name, fn.usage:match("^[^>]*%)") or fn.name .. "(...)")
			end
		end
		return problem, true
	end
	local args = call.input
	if type(args) ~= "table" then
		return "error: the tool input is not an object", true
	end
	if tool.name ~= "lua" then
		progress("  " .. tool.name)
	end
	local problem = argument_problem(tool, args)
	if problem then
		return "error: " .. problem, true
	end
	local ok, result, failed, images = pcall(tool.run, args)
	if ok then
		return result, failed == true, images
	end
	return "error: " .. tostring(result), true
end

M.claude = {
	["claude-opus-5-5"] = { effort = "medium", fallbacks = "default", display = "updates", compaction = true },
	["claude-sonnet-5-5"] = { effort = "medium", fallbacks = "default", display = "updates", compaction = true },
	["claude-haiku-5-5"] = { effort = "medium", compaction = true },
}

local function claude_model(model: string): (boolean, Features?)
	return model:match("^claude%-") ~= nil, M.claude[model]
end

local function prompt_tokens(counts: Usage): number
	return (counts.input_tokens or 0) + (counts.cache_read_input_tokens or 0) + (counts.cache_creation_input_tokens or 0)
end

local function count_of(model: string): Count
	for _, count in live.usage do
		if count.model == model then
			return count
		end
	end
	local count: Count = { model = model, replies = 0, input = 0, cached = 0, written = 0, output = 0 }
	table.insert(live.usage, count)
	return count
end

local function count_usage(reply: Reply, model: string)
	local counts: Usage = type(reply.usage) == "table" and reply.usage or {}
	local iterations = counts.iterations
	local parts: { Usage } = if type(iterations) == "table" and #iterations > 0 then iterations else { counts }
	local usage = count_of(model)
	usage.replies += 1
	for _, part in parts do
		usage.input += prompt_tokens(part)
		usage.cached += part.cache_read_input_tokens or 0
		usage.written += part.cache_creation_input_tokens or 0
		usage.output += part.output_tokens or 0
	end
end

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

local function tool_description(tool: Tool): string
	return if type(tool.description) == "function" then tool.description() else tool.description
end

type Wire = {
	body: (system: string, messages: { Message }, model: string, compaction: { [string]: any }?) -> { [string]: any },
	beta: ((model: string) -> string?)?,
	timeout: number?,
	reply: (answer: any) -> Reply?,
}

local function claude_betas(features: Features?): { string }
	local betas = {}
	if features then
		table.insert(betas, "thinking-binding-controls-2026-08-01")
		if features.display then
			table.insert(betas, "thinking-display-updates-2026-08-18")
		end
		if features.fallbacks then
			table.insert(betas, "server-side-fallback-2026-07-01")
		end
		if features.compaction then
			table.insert(betas, "compact-2026-09-04")
		end
	end
	return betas
end

local messages_wire: Wire = {
	body = function(system, messages, model, compaction)
		local is_claude, features = claude_model(model)
		local thinking = { type = "adaptive" }
		if features then
			thinking.block_binding = { prefix_mismatch_behavior = "drop_block" }
			thinking.display = features.display
		end
		local effort = M.effort or (features and features.effort)
		local system_field: string | { { [string]: any } } = system
		if is_claude then
			system_field = { { type = "text", text = system, cache_control = { type = "ephemeral" } } }
		end
		local specs = json.array({})
		for _, tool in M.tools do
			table.insert(specs, { name = tool.name, description = tool_description(tool), input_schema = tool.parameters })
		end
		return {
			model = model,
			max_tokens = M.max_output,
			thinking = if is_claude then thinking else nil,
			output_config = effort and { effort = effort } or nil,
			fallbacks = features and features.fallbacks,
			cache_control = { type = "ephemeral" },
			system = system_field,
			messages = messages,
			tools = specs,
			compaction = compaction,
		}
	end,
	beta = function(model)
		local betas = claude_betas(select(2, claude_model(model)))
		return #betas > 0 and table.concat(betas, ",") or nil
	end,
	reply = function(answer)
		if type(answer.content) ~= "table" then
			return nil
		end
		after_fallback(answer)
		return answer
	end,
}

M.gemini = {
	["gemini-4-neon"] = { deepest = "xhigh" },
	["gemini-4-hydrogen"] = { deepest = "xhigh" },
}
local efforts = { "low", "medium", "high", "xhigh", "max" }

local function thinking_level(model: string, effort: string?): string?
	local deepest = (M.gemini[model] or {}).deepest or "high"
	local asked = effort and table.find(efforts, effort)
	return if asked and asked > (table.find(efforts, deepest) :: number) then deepest else effort
end

local function gemini_content(block: Block): { [string]: any }?
	if block.type == "text" then
		return block.text ~= "" and { type = "text", text = block.text } or nil
	elseif block.type == "image" then
		return { type = "image", data = block.source.data, mime_type = block.source.media_type }
	elseif block.type == "compaction" and type(block.content) == "string" then
		return { type = "text", text = block.content }
	end
	return nil
end

local function gemini_thought(block: Block): { [string]: any }?
	local signature = (block :: any).signature
	local step = type(signature) == "string" and json.decode(signature)
	return type(step) == "table" and step.type == "thought" and step or nil
end

local function gemini_result(block: Block, name: string?): { [string]: any }
	local content = block.content
	local parts: { Block } = if type(content) == "table" then content else { { type = "text", text = tostring(content or "") } }
	local result = json.array({})
	for _, part in parts do
		table.insert(result, gemini_content(part))
	end
	return { type = "function_result", call_id = block.tool_use_id, name = name, is_error = block.is_error, result = result }
end

local function gemini_steps(messages: { Message }): { any }
	local steps = json.array({})
	local names: { [string]: string } = {}
	for _, message in messages do
		local content = message.content
		local blocks: { Block } = if type(content) == "string" then { { type = "text", text = content } } else content
		local said: { any }? = nil
		for _, block in blocks do
			local part = gemini_content(block)
			if part then
				if not said then
					said = json.array({})
					table.insert(steps, { type = message.role == "assistant" and "model_output" or "user_input", content = said })
				end
				table.insert(said :: { any }, part)
				continue
			end
			local step = nil
			if block.type == "thinking" then
				step = gemini_thought(block)
			elseif block.type == "tool_use" then
				names[block.id :: string] = block.name :: string
				step = { type = "function_call", id = block.id, name = block.name, arguments = block.input }
			elseif block.type == "tool_result" then
				step = gemini_result(block, names[block.tool_use_id :: string])
			end
			if step then
				said = nil
				table.insert(steps, step)
			end
		end
	end
	return steps
end

local function gemini_blocks(steps: { any }): { Block }
	local blocks: { Block } = {}
	for _, step in steps do
		if step.type == "thought" then
			local summary = {}
			for _, part in step.summary or {} do
				table.insert(summary, part.text)
			end
			table.insert(blocks, { type = "thinking", thinking = table.concat(summary, "\n"), signature = json.encode(step) } :: any)
		elseif step.type == "model_output" then
			for _, part in step.content or {} do
				if part.type == "text" then
					table.insert(blocks, { type = "text", text = part.text })
				end
			end
		elseif step.type == "function_call" then
			table.insert(blocks, { type = "tool_use", id = step.id, name = step.name, input = step.arguments })
		end
	end
	return blocks
end

local interactions_wire: Wire = {
	timeout = 900,
	body = function(system, messages, model)
		local specs = json.array({})
		for _, tool in M.tools do
			table.insert(specs,
				{ type = "function", name = tool.name, description = tool_description(tool), parameters = tool.parameters })
		end
		local level = thinking_level(model, M.effort)
		return {
			model = model,
			store = false,
			system_instruction = system,
			generation_config = level and { thinking_level = level },
			input = gemini_steps(messages),
			tools = specs,
		}
	end,
	reply = function(answer)
		if type(answer.steps) ~= "table" then
			return nil
		end
		local status = answer.status
		if status ~= "completed" and status ~= "requires_action" and status ~= "incomplete" then
			local fault = type(answer.errors) == "table" and answer.errors[1] or {}
			error(("the model request failed (%s): %s"):format(tostring(status), tostring(fault.message or fault.code)), 0)
		end
		local counts = type(answer.usage) == "table" and answer.usage or {}
		local cached = counts.total_cached_tokens or 0
		local content = gemini_blocks(answer.steps)
		local calls = false
		for _, block in content do
			calls = calls or block.type == "tool_use"
		end
		return {
			content = content,
			stop_reason = if status == "incomplete" then "max_tokens" elseif calls then "tool_use" else "end_turn",
			usage = {
				input_tokens = (counts.total_input_tokens or 0) - cached,
				cache_read_input_tokens = cached,
				output_tokens = (counts.total_output_tokens or 0) + (counts.total_thought_tokens or 0),
			},
		}
	end,
}

local function wire_of(model: string): Wire
	return if model:match("^gemini%-") then interactions_wire else messages_wire
end

function M.request(system: string, messages: { Message }, compaction: { [string]: any }?, model: string?): Reply
	local name = model or M.model
	local wire = wire_of(name)
	local body = json.encode(wire.body(system, messages, name, compaction))
	local endpoint = host.route(name)
	local options = { beta = wire.beta and wire.beta(name), timeout = wire.timeout }
	local attempts = 5
	for attempt = 1, attempts do
		local status, text, retry_after = host.post(endpoint, body, options)
		if status == 200 then
			local answer, err = json.decode(text)
			local reply = type(answer) == "table" and wire.reply(answer)
			if not reply then
				error("the model endpoint sent a reply this loop cannot read: " .. (err or text:sub(1, 300)), 0)
			end
			count_usage(reply, name)
			if compaction or has_content(reply) then
				return reply
			end
			status, text = "empty reply", "the model sent a reply with nothing in it"
		end
		local transient = type(status) ~= "number" or status == 408 or status == 409 or status == 429 or status >= 500
		if not transient or attempt == attempts then
			error(("the model request failed (%s): %s"):format(tostring(status or "no response"), tostring(text):sub(1, 500)), 0)
		end
		local wait = math.min(math.max(2 ^ attempt, retry_after or 0), 120)
		progress(("model request failed (%s), retrying in %d s"):format(tostring(status or text), wait))
		host.sleep(wait)
	end
	error("unreachable")
end

function M.system_prompt(): string
	local ripgrep = host.sh("command -v rg", { timeout = 10 }).code == 0
	local parts = {
		("You are a coding agent working in %s. You act by running Luau code with the lua tool; the code calls "
			.. "the functions the tool lists. Put several steps in one call when you know what each will do, and take "
			.. "one step at a time when the next depends on what the last returned."):format(host.cwd),
		host.sandboxed
				and "The kernel confines you: you can write inside this project directory and $TMPDIR, and read those "
					.. "plus the system directories and the programs on PATH. \"Permission denied\" anywhere else is "
					.. "that wall. Do not refuse in advance: try what the task needs, and only when the wall denies it, "
					.. "say so and stop rather than looking for a way around it. When a program the task needs is "
					.. "denied, name the path it needs, which the user can open by starting hx with --ro PATH."
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
			.. (host.skills.hx and " skill(\"hx\") says which of these a need calls for and how each is written "
				.. "and tried, and covers the loop, sub-agents and scripts: load it before you add to the harness." or ""),
		"Use read(), not bash() with cat, sed, head or tail, to look at text files; its offset and limit read a "
			.. "range of lines or go on through a large file. Read a file before you edit it, unless "
			.. "you wrote or edited it yourself in this session."
			.. " To search, call ext(\"search\"): grep(pattern, {dir?, glob?}) returns the matches as data and "
			.. "shows them as path:line: text, and files(glob?) lists the files git does not ignore."
			.. (ripgrep and " For what they do not cover, ripgrep is installed: use rg through bash rather than plain "
				.. "grep. It is faster, and it skips what .gitignore names." or ""),
		"Do the task you were given, as its instructions say. When it is unclear, or doing it would take a step it "
			.. "did not ask for, stop and ask: end your reply with the question. Never sleep or poll to wait for "
			.. "something outside your control, such as a market, a deploy or a person: report what you found and "
			.. "ask whether to wait.",
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

local Conversation = {}
Conversation.__index = Conversation
M.Conversation = Conversation
type ConversationFields = { messages: { Message }, model: string?, on_answer: ((Conversation) -> ())? }
type Conversation = typeof(setmetatable({} :: ConversationFields, Conversation))
type State = "idle" | "ready" | "owing"

function Conversation.new(model: string?): Conversation
	return setmetatable({ messages = {}, model = model, on_answer = nil }, Conversation)
end

local function tool_calls(message: Message): { Block }
	local calls = {}
	for _, block in message.content :: { Block } do
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

function Conversation.reply(self: Conversation, content: { Block }): { Block }
	expect(self, "reply", "ready")
	if #content == 0 then
		return {}
	end
	local message: Message = { role = "assistant", content = content }
	table.insert(self.messages, message)
	return tool_calls(message)
end

function Conversation.answer(self: Conversation, run: (Block) -> (string, boolean?, { Block }?))
	expect(self, "answer", "owing")
	local results: { Block } = {}
	for _, call in tool_calls(self.messages[#self.messages]) do
		local text, failed, images = run(call)
		local is_error: boolean? = failed or nil
		local content: any = text
		if images and #images > 0 then
			content = table.move(images, 1, #images, 2, { { type = "text", text = text } })
		end
		table.insert(results, { type = "tool_result", tool_use_id = call.id, content = content, is_error = is_error })
	end
	table.insert(self.messages, { role = "user", content = results })
	if self.on_answer then
		self.on_answer(self)
	end
end

function Conversation.abandon(self: Conversation, reason: string)
	if self:state() == "owing" then
		self:answer(function()
			return "error: " .. reason, true
		end)
	end
end

function Conversation.restart(self: Conversation, text: string)
	expect(self, "restart", "idle", "ready")
	table.clear(self.messages)
	self:say(text)
end

function Conversation.resume(self: Conversation, block: Block, text: string)
	expect(self, "resume", "idle", "ready")
	table.clear(self.messages)
	table.insert(self.messages, { role = "user", content = { block, { type = "text", text = text } } })
end

function Conversation.fork(self: Conversation): Conversation
	local copy = Conversation.new(self.model)
	copy.messages = table.clone(self.messages)
	local last = copy.messages[#copy.messages]
	if last and type(last.content) == "table" then
		copy.messages[#copy.messages] = { role = last.role, content = table.clone(last.content) }
	end
	return copy
end

function Conversation.problem(self: Conversation): string?
	local owed: { Block }? = nil
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
			local blocks: { Block } = if type(content) == "table" then content else {}
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

local function ask(system: string, talk: Conversation, compaction: { [string]: any }?): Reply
	local problem = talk:state() ~= "ready" and "it is " .. talk:state() .. ", not ready" or talk:problem()
	if problem then
		error("this conversation cannot be sent: " .. problem, 0)
	end
	return M.request(system, talk.messages, compaction, talk.model)
end

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

M.resume_prompt = "Earlier turns were replaced by the checkpoint above, written by you. Treat it as established "
	.. "and continue the work from it directly, without restating or acknowledging it."
M.globals_listed = 40
M.link_hops = 40

function M.resume_text(): string
	local names = {}
	for name in pairs(scratch :: { [any]: any }) do
		if type(name) == "string" and name ~= "print" then
			table.insert(names, name)
		end
	end
	if #names == 0 then
		return M.resume_prompt
	end
	table.sort(names)
	local listed = table.move(names, 1, math.min(#names, M.globals_listed), 1, {})
	local more = if #names > #listed then (" and %d more"):format(#names - #listed) else ""
	return ("%s The globals your Luau code set before the checkpoint are still set: %s%s. The checkpoint may not "
		.. "say what is in them, so look at one before you rely on it."):format(M.resume_prompt,
		table.concat(listed, ", "), more)
end

local function ask_for_summary(system: string, talk: Conversation, compaction: { [string]: any }?): Reply?
	local ok, reply = pcall(ask, system, talk, compaction)
	if not ok then
		progress("  compaction failed, the conversation stays as it is: " .. tostring(reply))
		return nil
	end
	return reply
end

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
	talk:resume(block, M.resume_text())
end

function M.compact(system: string, talk: Conversation)
	progress("  compacting the conversation")
	local _, features = claude_model(talk.model or M.model)
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
	talk:restart(summary .. "\n\n" .. M.resume_text())
end

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

local function show_text(reply: Reply, model: string)
	local _, features = claude_model(model)
	local updates = features and features.display == "updates"
	for _, block in reply.content do
		if block.type == "text" and block.text ~= "" then
			local text = block.text :: string
			if live.agent then
				progress(text)
			else
				print(host.tty and M.printable(text) or text)
			end
		elseif updates and block.type == "thinking" and type(block.thinking) == "string" and block.thinking ~= "" then
			progress("  " .. M.printable(block.thinking))
		end
	end
end

local at_work: () -> { string }

function M.agents_reminder(names: { string }): string
	return ("Your agents are still at work: %s. What they are doing ends half done when this session does. Wait for "
		.. "their reports with agents(), check the work, then finish. If you mean to stop without them, say so in "
		.. "your answer."):format(table.concat(names, ", "))
end

local function turn(system: string, talk: Conversation): Reply
	local reminded = 0
	local waited = false
	for _ = 1, M.max_replies do
		local reply = ask(system, talk)
		local cut_off = reply.stop_reason == "max_tokens"
		show_text(reply, talk.model or M.model)
		local calls = talk:reply(reply.content)
		if #calls > 0 then
			talk:answer(function(call)
				if cut_off then
					return "error: this call was cut off by the output limit and did not run. Send it again, smaller.", true
				end
				return run_tool(call)
			end)
			local worst, due = live.repeats.worst, nil
			for i, at in M.repeat_reminders do
				if worst and worst.count >= at and at > reminded then
					due = i
				end
			end
			if due and worst then
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
			local busy = if live.turns == 1 and not live.agent and not waited then at_work() else {}
			if #busy == 0 then
				return reply
			end
			waited = true
			talk:say(M.agents_reminder(busy))
		end
	end
	talk:say(("You have used all %d replies this task allows. Do not call any more tools. Write your final "
		.. "report now: what you changed, what you checked, what is left."):format(M.max_replies))
	local reply = ask(system, talk)
	show_text(reply, talk.model or M.model)
	if #talk:reply(reply.content) > 0 then
		talk:abandon("the reply limit was reached")
	end
	error(("stopped after %d model replies without a final answer"):format(M.max_replies), 0)
end

local started_at, started_ns = host.wall_time()
local log_path = (".hx/log/%s-t%dn%09dx%s.json"):format(os.date("%Y%m%d-%H%M%S", started_at), started_at, started_ns,
	(host.env("TMPDIR") :: string? or ""):match("([%w]+)/*$") or "0")
local function save(system: string, messages: { Message }, path: string)
	if host.list(".hx") then
		if not host.read(".hx/log/.gitignore") then
			host.write(".hx/log/.gitignore", "*\n")
		end
		host.write(path, json.encode({ system = system, messages = messages }))
	end
end

local sub_logs: { [Conversation]: string } = {}
setmetatable(sub_logs, { __mode = "k" })
local function sub_log(talk: Conversation): string
	local path = sub_logs[talk]
	if not path then
		live.subs += 1
		path = (log_path:gsub("%.json$", ("-sub-%d.json"):format(live.subs)))
		sub_logs[talk] = path
	end
	return path
end

function M.turn(system: string, talk: Conversation)
	local outer = live.repeats
	local path: string? = if live.turns > 0 then sub_log(talk) else nil
	if path and not talk.on_answer then
		talk.on_answer = function()
			save(system, talk.messages, path)
		end
	end
	live.repeats = { counts = {}, worst = nil }
	live.turns += 1
	local ok, err = pcall(turn, system, talk)
	live.turns -= 1
	live.repeats = outer
	if path then
		save(system, talk.messages, path)
	end
	if not ok then
		error(err, 0)
	end
end

M.agent_model = nil :: string?
M.agent_limit = 8
M.agent_wait = 270
M.agent_seconds = 1800
M.agent_prompt = "You are one of several agents at work in this directory at the same time, each on a task from "
	.. "the lead agent. The lead reads your last message as your report, checks your work, and sends it back "
	.. "to you if something is wrong. The other agents are changing other files right now. Change only what your "
	.. "task gives you, and leave git's index, branches and stash as they are: no commit, checkout, reset or "
	.. "stash. Check your own work as the task says; where it does not say, check narrowly, your own files and "
	.. "tests, since a whole build or test run may trip over what the others have half written. If the task "
	.. "cannot be done as given, or needs a change outside it, stop and say so in your report rather than work "
	.. "around it. Finish with the report: what you changed, file by file, what you ran to check it and what "
	.. "that showed, and what is left."

type Agent = {
	name: string,
	talk: Conversation,
	state: "idle" | "waiting" | "working",
	task: string,
	runs: number,
	id: number?,
	started: number,
	report: string?,
	stopping: boolean,
	seen: { [string]: string },
}
local fleet: { agents: { Agent }, waiting: { Agent }, system: string? } = { agents = {}, waiting = {}, system = nil }

function at_work(): { string }
	local names = {}
	for _, agent in fleet.agents do
		if agent.state ~= "idle" then
			table.insert(names, agent.name)
		end
	end
	return names
end

local function agent_model(): string
	return M.agent_model or (if host.endpoints.deepseek then "deepseek-flash" else M.model)
end

local function agent_named(name: string): Agent
	for _, agent in fleet.agents do
		if agent.name == name then
			return agent
		end
	end
	local agent: Agent = {
		name = name,
		talk = Conversation.new(),
		state = "idle",
		task = "",
		runs = 0,
		id = nil,
		started = 0,
		report = nil,
		stopping = false,
		seen = {},
	}
	sub_log(agent.talk)
	table.insert(fleet.agents, agent)
	return agent
end

local function work(agent: Agent, system: string): string
	local began = os.clock()
	live.agent = { name = agent.name, run = agent.runs }
	live.usage = {}
	live.seen, live.wrote, live.shared_since = agent.seen, {}, file_clock()
	agent.talk.model = agent_model()
	agent.talk:say(agent.task)
	local ok, err = host.timed(M.agent_seconds, M.turn, system, agent.talk)
	if not ok then
		local was_owing = agent.talk:state() == "owing"
		agent.talk:abandon("the turn ended: " .. tostring(err))
		if not was_owing then
			save(system, agent.talk.messages, sub_log(agent.talk))
		end
	end
	return json.encode({
		messages = agent.talk.messages,
		usage = live.usage,
		wrote = live.wrote,
		seen = live.seen,
		seconds = os.clock() - began,
		error = if ok then nil else tostring(err),
	})
end

local function start(agent: Agent)
	local system = fleet.system or (M.system_prompt() .. "\n\n" .. M.agent_prompt
		.. (if host.judged then " The judge is the lead's to submit to, not yours: you submit nothing." else ""))
	fleet.system = system
	agent.id = host.spawn(work, agent, system)
	agent.state = "working"
	agent.started = os.clock()
	progress(("  agent %s: %s"):format(agent.name, headline(agent.task)))
end

M.agent_wrote_listed = 20

local function trail(agent: Agent, files: { string }): string
	local lines = {}
	if #files > 0 then
		local listed = table.move(files, 1, math.min(#files, M.agent_wrote_listed), 1, {})
		local more = if #files > #listed then (" and %d more"):format(#files - #listed) else ""
		table.insert(lines, ("[wrote, with write and edit: %s%s]"):format(table.concat(listed, ", "), more))
	end
	local log = sub_logs[agent.talk]
	if log and host.stat(log) then
		table.insert(lines, ("[its conversation: %s]"):format(log))
	end
	return if #lines > 0 then table.concat(lines, "\n") .. "\n" else ""
end

local function finish(agent: Agent, returned: boolean, text: string)
	agent.state, agent.id = "idle", nil
	local done: { messages: { Message }, usage: { Count }, wrote: { string }?, seen: any, seconds: number, error: string? }? = nil
	if returned then
		local decoded: any = json.decode(text)
		if type(decoded) == "table" and type(decoded.messages) == "table" then
			done = decoded
		end
	end
	local stopping = agent.stopping
	agent.stopping = false
	if not done and stopping and text == "terminated" then
		agent.report = ("[agent %s: stopped as you asked, after %.0f s. It has the conversation it had before this "
			.. "task, and may have changed files.]\n%s"):format(agent.name, os.clock() - agent.started, trail(agent, {}))
			:gsub("\n$", "")
		return
	end
	if not done then
		agent.report = ("[agent %s: stopped after %.0f s (%s). It has the conversation it had before this task, "
			.. "and may have changed files.]\n%s"):format(agent.name, os.clock() - agent.started,
			if returned then "its result could not be read" else text, trail(agent, {})):gsub("\n$", "")
		return
	end
	agent.talk.messages = done.messages
	agent.seen = if type(done.seen) == "table" then done.seen else {}
	for _, count in done.usage do
		local mine = count_of(count.model)
		mine.replies += count.replies
		mine.input += count.input
		mine.cached += count.cached
		mine.written += count.written
		mine.output += count.output
	end
	local last, words = done.messages[#done.messages], {}
	if last and last.role == "assistant" and type(last.content) == "table" then
		for _, block in last.content do
			if block.type == "text" and block.text ~= "" then
				table.insert(words, block.text :: string)
			end
		end
	end
	local how, failure = ("done in %.0f s"):format(done.seconds), done.error
	if failure == "timeout" then
		how = ("stopped at the %.0f s one run may take"):format(M.agent_seconds)
	elseif failure then
		how = ("failed after %.0f s: %s"):format(done.seconds, failure)
	end
	agent.report = ("[agent %s: %s]\n%s%s"):format(agent.name, how, trail(agent, done.wrote or {}),
		if #words > 0 then table.concat(words, "\n") else "(it said nothing)")
end

local function checked_tasks(tasks: any): { { name: string, task: string?, stop: boolean? } }
	check("agents", tasks, 1, "tasks", "table", true)
	if not tasks then
		return {}
	end
	if type(tasks.name) == "string" and (type(tasks.task) == "string" or tasks.stop == true) then
		tasks = { tasks }
	elseif #tasks == 0 and next(tasks) ~= nil then
		error('agents: tasks is a list, as in agents({ { name = "parser", task = "..." } })', 0)
	end
	local sends, seen = {}, {}
	for i, item in ipairs(tasks) do
		local stop = type(item) == "table" and item.stop == true
		if stop and item.task ~= nil then
			error(("agents: task %d has a task and stop = true; send one or the other"):format(i), 0)
		end
		if type(item) ~= "table" or type(item.name) ~= "string" or not (stop or type(item.task) == "string") then
			error(('agents: task %d must be a table { name = "...", task = "..." }'):format(i), 0)
		end
		local name: string, task: string? = item.name, item.task
		if not name:match("^[%w_%-]+$") or #name > 40 then
			error(("agents: task %d: a name is up to 40 letters, digits, _ and -, not %q"):format(i, name), 0)
		end
		if task and task:match("^%s*$") then
			error(("agents: the task for %s is empty"):format(name), 0)
		end
		if seen[name] then
			error(("agents: %s is given two tasks in one call; an agent takes one at a time"):format(name), 0)
		end
		seen[name] = true
		local busy = false
		for _, agent in fleet.agents do
			busy = busy or agent.name == name and agent.state ~= "idle"
		end
		if busy and not stop then
			error(("agents: %s is still at work on its last task. Wait for its report, with agents() and no "
				.. "tasks, before you send it more, or stop it with { name = %q, stop = true }."):format(name, name), 0)
		elseif stop and not busy then
			error(("agents: %s is not at work, so there is nothing to stop"):format(name), 0)
		end
		table.insert(sends, { name = name, task = task, stop = stop })
	end
	return sends
end

table.insert(M.functions, {
	name = "agents",
	lead = true,
	usage = "agents(tasks) -> text: gives each task to an agent and returns the agents' reports. An agent is a "
		.. "model with a conversation of its own, at work in this directory with these functions; all of them work "
		.. 'at the same time. tasks is a list of { name = "...", task = "..." }. An agent knows nothing of your '
		.. "conversation: its task has to say all it needs, the files to change, what to leave alone and how to "
		.. "check the work. Agents at work together must not write the same files. A task for a name that already "
		.. "has an agent goes to that agent as your next message to it, with all it did still in its "
		.. "conversation: that is how work goes back to be fixed. The call returns after a few minutes at most, "
		.. "with the reports so far and the names of the agents still at work; agents() with no tasks waits for "
		.. "those. { name = \"...\", stop = true } stops an agent at work, which keeps the conversation it had before that task. A report "
		.. "names the files the agent wrote and, when the project keeps logs, the file its whole conversation "
		.. "is in, to read when the report leaves something out.",
	run = function(tasks: any): string
		if live.agent then
			error("agents: an agent has no agents of its own", 0)
		end
		for _, send in checked_tasks(tasks) do
			live.shared_since = live.shared_since or file_clock()
			local agent = agent_named(send.name)
			local waiting = table.find(fleet.waiting, agent)
			if send.task then
				agent.task, agent.state, agent.report = send.task, "waiting", nil
				agent.runs += 1
				table.insert(fleet.waiting, agent)
			elseif waiting then
				table.remove(fleet.waiting, waiting)
				agent.state = "idle"
				agent.report = ("[agent %s: stopped as you asked, before it began]"):format(agent.name)
			elseif agent.id then
				agent.stopping = host.stop(agent.id)
			end
		end

		local function take_in(id: number, returned: boolean, text: string)
			for _, agent in fleet.agents do
				if agent.id == id then
					finish(agent, returned, text)
				end
			end
		end
		local until_time = os.clock() + M.agent_wait
		while true do
			while true do
				local id: number?, returned: boolean, text: string = host.wait(0)
				if not id then
					break
				end
				take_in(id, returned, text)
			end
			local working = #at_work() - #fleet.waiting
			while #fleet.waiting > 0 and working < math.max(M.agent_limit, 1) do
				start(table.remove(fleet.waiting, 1) :: Agent)
				working += 1
			end
			local left = until_time - os.clock()
			if working == 0 or left <= 0 then
				break
			end
			local id: number?, returned: boolean, text: string = host.wait(left)
			if not id then
				break
			end
			take_in(id, returned, text)
		end

		local lines: { string } = {}
		local busy = 0
		for _, agent in fleet.agents do
			if agent.report then
				table.insert(lines, agent.report)
				agent.report = nil
			elseif agent.state == "working" then
				busy += 1
				table.insert(lines, ("[agent %s: still at work, %.0f s so far]"):format(agent.name, os.clock() - agent.started))
			elseif agent.state == "waiting" then
				busy += 1
				table.insert(lines, ("[agent %s: waiting for one of the %d places at work]"):format(agent.name,
					math.max(M.agent_limit, 1)))
			end
		end
		if busy > 0 then
			table.insert(lines, "agents() with no tasks waits for the agents still at work.")
		elseif #lines == 0 then
			local names = {}
			for _, agent in fleet.agents do
				table.insert(names, agent.name)
			end
			return if #names > 0
				then "No agent is at work, and you have every report. Your agents so far: " .. table.concat(names, ", ") .. "."
				else "You have no agents yet: give agents a list of tasks."
		end
		return table.concat(lines, "\n\n")
	end,
})

M.workflow = host.workflow :: string?

function M.lead_prompt(): string?
	local name = M.workflow
	if not name then
		return nil
	end
	for _, skill in M.skills() do
		if skill.name == name then
			return ("This session follows the %s workflow, whatever the task: its instructions are below, already "
				.. "loaded, and they are yours to follow from the first step.\n\n%s"):format(name, M.skill(name))
		end
	end
	error(("the workflow %s needs a skill of that name, and there is none"):format(name), 0)
end

local function report()
	local used: { Count } = live.usage
	if #used == 0 then
		used = { { model = M.model, replies = 0, input = 0, cached = 0, written = 0, output = 0 } }
	end
	for _, usage in used do
		progress(("[%s%d replies, %d tokens in (%d read from cache, %d written to it), %d out]"):format(
			#used > 1 and usage.model .. ": " or "", usage.replies, usage.input, usage.cached, usage.written, usage.output))
	end
end

local PASTE_ON, PASTE_OFF = "\27[?2004h", "\27[?2004l"
local PASTE_START, PASTE_END = "\27[200~", "\27[201~"

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

function M.resume_log(named: string): string
	if named ~= "last" then
		local logged = ".hx/log/" .. named
		if not named:find("/", 1, true) and not host.read(named) and host.read(logged) then
			return logged
		end
		return named
	end
	local newest: string? = nil
	local newest_seconds, newest_nanos = -math.huge, 0
	for _, name in host.list(".hx/log") or {} do
		if name:match("%.json$") and not name:match("%-sub%-%d+%.json$") then
			local epoch, fraction = name:match("^%d+%-%d+%-t(%d+)n(%d+)x%w+%.json$")
			local seconds, nanos = tonumber(epoch), tonumber(fraction)
			if not seconds or not nanos then
				local stat = host.stat(".hx/log/" .. name)
				if not stat then
					continue
				end
				seconds = math.floor(stat.mtime)
				nanos = (stat.mtime - seconds) * 1e9
			end
			if not newest or seconds > newest_seconds or (seconds == newest_seconds
				and (nanos > newest_nanos or (nanos == newest_nanos and name > newest))) then
				newest, newest_seconds, newest_nanos = name, seconds, nanos
			end
		end
	end
	if not newest then
		error("--resume last: .hx/log holds no conversation; one is saved there when the project has .hx/", 0)
	end
	return ".hx/log/" .. newest
end

function M.resumed(talk: Conversation, named: string)
	local path = M.resume_log(named)
	local ok, saved = pcall(json.decode, or_raise(host.read(path)))
	if not ok or type(saved) ~= "table" or type(saved.messages) ~= "table" then
		error(("--resume: %s is not a conversation hx saved"):format(path), 0)
	end
	talk.messages = saved.messages
	local problem = talk:problem()
	if problem then
		error(("--resume: %s cannot be gone on from: %s"):format(path, problem), 0)
	end
	talk:abandon("the session ended before this call was answered")
	progress(("hx: going on from %s, %d messages"):format(path, #talk.messages))
end

function M.main(): number
	local system = M.system_prompt()
	local lead = M.lead_prompt()
	if lead then
		system ..= "\n\n" .. lead
	end
	local talk = Conversation.new()
	talk.on_answer = function()
		save(system, talk.messages, log_path)
	end
	local task = table.concat(host.args, " ")
	if host.resume then
		M.resumed(talk, host.resume)
	end

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
		save(system, talk.messages, log_path)
		report()
		if not ok then
			error(err, 0)
		end
		return 0
	end

	progress(("hx: %s in %s. Ctrl-C stops a turn, Ctrl-D leaves."):format(M.model, host.cwd))
	while true do
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
					save(system, talk.messages, log_path)
					host.log(PASTE_OFF)
					error(err, 0)
				end
				if err == "interrupted" then
					talk:abandon("interrupted by the user")
					host.log("[interrupted]")
				else
					talk:abandon("the turn failed: " .. tostring(err))
					host.log(M.printable("[the turn failed: " .. tostring(err) .. "]"))
				end
			end
			save(system, talk.messages, log_path)
		end
	end
	host.log(PASTE_OFF)
	report()
	return 0
end

return M
