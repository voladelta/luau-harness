-- find files and text in the project as data, with no shell quoting: files(glob?, dir?) -> {path}, what git lists (tracked, or new and not ignored), or every file where git lists none; grep(pattern, {dir?, glob?, fixed?, ignore_case?, limit?}) -> {{path, line, text}}, a regex search that skips what .gitignore names and prints as path:line: text
--!strict

local M = {}

M.limit = 1000
M.text_limit = 500
M.seconds = 60
M.ripgrep = nil :: boolean?

export type Match = { path: string, line: number, text: string }

type Options = {
	dir: string?,
	glob: string?,
	fixed: boolean?,
	ignore_case: boolean?,
	limit: number?,
}

local function check(fname: string, value: unknown, position: number, name: string, kind: string, optional: boolean?)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("search.%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

local function prefix_of(dir: string?): string
	if not dir or dir == "." or dir == "./" or dir == "" then
		return ""
	end
	return if dir:sub(-1) == "/" then dir else dir .. "/"
end

local function listed(args: { string }, dir: string?): { string }?
	local result = host.sh(args, { split = true, cwd = dir, timeout = M.seconds })
	if result.code ~= 0 then
		return nil
	end
	local paths = {}
	for _, path in result.out:split("\0") do
		if path ~= "" then
			table.insert(paths, path)
		end
	end
	return paths
end

function M.files(glob: string?, dir: string?): { string }
	check("files", glob, 1, "glob", "string", true)
	check("files", dir, 2, "dir", "string", true)
	local prefix = prefix_of(dir)
	local where = if prefix == "" then nil else prefix
	local spec = if glob then { "--", ":(glob)" .. glob } else {}
	local list = { "git", "ls-files", "-z", "--cached", "--others", "--exclude-standard" }
	local gone = { "git", "ls-files", "-z", "--deleted" }
	table.move(spec, 1, #spec, #list + 1, list)
	table.move(spec, 1, #spec, #gone + 1, gone)
	local found = listed(list, where)
	local out = {}
	if found and #found > 0 then
		local deleted = {}
		for _, path in listed(gone, where) or {} do
			deleted[path] = true
		end
		for _, path in found do
			if not deleted[path] then
				table.insert(out, prefix .. path)
			end
		end
	else
		for _, path in host.walk(where or ".", { glob = glob }) do
			if path:sub(-1) ~= "/" then
				table.insert(out, prefix .. path)
			end
		end
	end
	table.sort(out)
	return out
end

local shown = {
	__tostring = function(matches: any): string
		local lines = {}
		for _, match in ipairs(matches) do
			table.insert(lines, ("%s:%d: %s"):format(match.path, match.line, match.text))
		end
		if #lines == 0 then
			return "[no match]"
		end
		if matches.more then
			table.insert(lines, ("[more than these %d match; narrow the pattern, dir or glob, or raise limit]"):format(#lines))
		end
		return table.concat(lines, "\n")
	end,
}

local function row(path: string, line: number, text: string): Match
	text = text:gsub("\r?\n$", "")
	if #text > M.text_limit then
		text = text:sub(1, M.text_limit) .. " ..."
	end
	return { path = (path:gsub("^%./", "")), line = line, text = text }
end

local function with_ripgrep(pattern: string, options: Options, limit: number): { Match }
	local args = { "rg", "--json", "--hidden", "--glob", "!.git", "--sort", "path" }
	if options.fixed then
		table.insert(args, "--fixed-strings")
	end
	if options.ignore_case then
		table.insert(args, "--ignore-case")
	end
	if options.glob then
		table.insert(args, "--glob")
		table.insert(args, options.glob)
	end
	table.insert(args, "--regexp")
	table.insert(args, pattern)
	table.insert(args, prefix_of(options.dir) == "" and "./" or options.dir :: string)
	local result = host.sh(args, { split = true, timeout = M.seconds })
	if result.timed_out then
		error(("search.grep: no answer in %d s; narrow dir or glob"):format(M.seconds), 0)
	end
	if result.code ~= 0 and result.code ~= 1 then
		error("search.grep: " .. result.err:gsub("%s+$", ""), 0)
	end
	local matches: any = {}
	for _, line in result.out:split("\n") do
		if line:sub(1, 15) == '{"type":"match"' then
			local ok, event = pcall(json.decode, line)
			local data = ok and event.data
			if data and data.path.text and data.lines.text then
				if #matches == limit then
					matches.more = true
					break
				end
				table.insert(matches, row(data.path.text, data.line_number, data.lines.text))
			end
		end
	end
	return matches
end

local function with_grep(pattern: string, options: Options, limit: number): { Match }
	local args = { "grep", "-rnIZ", "--exclude-dir=.git", if options.fixed then "-F" else "-E" }
	if options.ignore_case then
		table.insert(args, "-i")
	end
	if options.glob then
		table.insert(args, "--include=" .. (options.glob:match("[^/]*$") :: string))
	end
	table.insert(args, "-e")
	table.insert(args, pattern)
	table.insert(args, prefix_of(options.dir) == "" and "./" or options.dir :: string)
	local result = host.sh(args, { split = true, timeout = M.seconds })
	if result.timed_out then
		error(("search.grep: no answer in %d s; narrow dir or glob"):format(M.seconds), 0)
	end
	if result.code ~= 0 and result.code ~= 1 then
		error("search.grep: " .. result.err:gsub("%s+$", ""), 0)
	end
	local matches: any = {}
	for path, line, text in result.out:gmatch("([^%z\n]+)%z(%d+):([^\n]*)") do
		if #matches == limit then
			matches.more = true
			break
		end
		table.insert(matches, row(path, tonumber(line) :: number, text))
	end
	table.sort(matches, function(a: Match, b: Match): boolean
		if a.path ~= b.path then
			return a.path < b.path
		end
		return a.line < b.line
	end)
	return matches
end

function M.grep(pattern: string, options: Options?): { Match }
	check("grep", pattern, 1, "pattern", "string")
	check("grep", options, 2, "options", "table", true)
	local chosen: Options = options or {}
	check("grep", chosen.dir, 2, "options.dir", "string", true)
	check("grep", chosen.glob, 2, "options.glob", "string", true)
	check("grep", chosen.limit, 2, "options.limit", "number", true)
	if pattern == "" then
		error("search.grep: argument 1 (pattern) is empty", 0)
	end
	local limit = chosen.limit or M.limit
	local ripgrep = M.ripgrep
	if ripgrep == nil then
		ripgrep = host.sh("command -v rg", { timeout = 10 }).code == 0
	end
	local matches = if ripgrep then with_ripgrep(pattern, chosen, limit) else with_grep(pattern, chosen, limit)
	return setmetatable(matches, shown) :: any
end

return M
