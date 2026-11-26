-- find files and text in the project as data, with no shell quoting: files(glob?, dir?) -> {path}, what git lists (tracked, or new and not ignored), or every file outside a git repository; grep(pattern, {dir?, glob?, fixed?, ignore_case?, limit?}) -> {{path, line, text}}, a regex search that skips what .gitignore names and prints as path:line: text; .unreadable lists files it could not read
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
	if result.omitted then
		error("search.files: shell output was clipped; narrow dir or glob", 0)
	end
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
	local git_glob = if glob and not glob:find("/", 1, true) then "**/" .. glob else glob
	local spec = if git_glob then { "--", ":(glob)" .. git_glob } else {}
	local list = { "git", "ls-files", "-z", "--cached", "--others", "--exclude-standard" }
	local gone = { "git", "ls-files", "-z", "--deleted" }
	table.move(spec, 1, #spec, #list + 1, list)
	table.move(spec, 1, #spec, #gone + 1, gone)
	local found = listed(list, where)
	local out = {}
	if found then
		local seen = {}
		for _, path in listed(gone, where) or {} do
			seen[path] = true
		end
		for _, path in found do
			if not seen[path] then
				seen[path] = true
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
			table.insert(lines, "[no match]")
		elseif matches.more then
			table.insert(lines, ("[more than these %d match; narrow the pattern, dir or glob, or raise limit]"):format(#lines))
		end
		local unreadable: { string }? = matches.unreadable
		if unreadable then
			local named = table.concat(unreadable, ", ", 1, math.min(#unreadable, 5))
			local rest = if #unreadable > 5 then (" and %d more"):format(#unreadable - 5) else ""
			table.insert(lines, ("[could not read %d file%s: %s%s]"):format(#unreadable, if #unreadable == 1 then "" else "s",
				named, rest))
		end
		return table.concat(lines, "\n")
	end,
}

local preview_matches = [[
count=$1 width=$2
shift 2
{ printf "hx\0awk\0"; "$@"; } | LC_ALL=C awk -v count="$count" -v width="$width" '
	BEGIN { RS = "\0" }
	NR == 1 { if ($0 != "hx") exit; next }
	NR == 2 { if ($0 != "awk") exit; split_on_nul = 1; next }
	NR == 3 { path = $0; next }
	{
		nl = index($0, "\n")
		colon = index($0, ":")
		if (colon > 1 && colon < nl && substr($0, 1, colon - 1) ~ /^[0-9]+$/) {
			last = nl - 1
			if (substr($0, last, 1) == "\r") --last
			cut = colon + width
			suffix = last > cut ? " ..." : ""
			printf "%s%c%s%s\n", path, 0, substr($0, 1, last > cut ? cut : last), suffix
			if (++shown == count) exit
		}
		path = substr($0, nl + 1)
	}
	END {
		if (!split_on_nul) {
			print "awk must support NUL record separators" > "/dev/stderr"
			exit 1
		}
	}
'
status=("${PIPESTATUS[@]}")
if ((status[1] != 0)); then exit 250; fi
exit "${status[0]}"
]]

local function collect(args: { string }, limit: number, unreadable_in: (line: string) -> (string?, boolean?), input: string?): { Match }
	local command = { "bash", "-c", preview_matches, "search.grep", tostring(limit + 1), tostring(M.text_limit) }
	table.move(args, 1, #args, #command + 1, command)
	local result = host.sh(command, { split = true, input = input, timeout = M.seconds })
	if result.timed_out then
		error(("search.grep: no answer in %d s; narrow dir or glob"):format(M.seconds), 0)
	end
	if result.omitted then
		error("search.grep: shell output was clipped; narrow dir or glob, or lower limit", 0)
	end
	local matches: any = {}
	local at = 1
	while at <= #result.out do
		local separator = result.out:find("\0", at, true) :: number
		local colon = result.out:find(":", separator + 1, true) :: number
		local stop = result.out:find("\n", colon + 1, true) :: number
		if #matches == limit then
			matches.more = true
			break
		end
		table.insert(matches, {
			path = (result.out:sub(at, separator - 1):gsub("^%./", "")),
			line = tonumber(result.out:sub(separator + 1, colon - 1)) :: number,
			text = result.out:sub(colon + 1, stop - 1),
		})
		at = stop + 1
	end
	if result.code == 250 then
		error("search.grep: " .. result.err:gsub("%s+$", ""), 0)
	end
	local unreadable, problems = {}, {}
	for _, line in result.err:split("\n") do
		local path, ignore = unreadable_in(line)
		if path then
			table.insert(unreadable, (path:gsub("^%./", "")))
		elseif not ignore and line:find("%S") then
			table.insert(problems, line)
		end
	end
	if #matches == 0 and not matches.more and result.code ~= 0 and #problems > 0 then
		error("search.grep: " .. table.concat(problems, "\n"), 0)
	end
	if #unreadable > 0 then
		table.sort(unreadable)
		matches.unreadable = unreadable
	end
	return matches
end

local function within_target(path: string, target: string): boolean
	if target:sub(1, 1) ~= "/" then
		return path:sub(1, 1) ~= "/"
	end
	local root = prefix_of(target)
	local bare = if root == "/" then "/" else root:sub(1, -2)
	return path == bare
		or path:sub(1, #root) == root
		or path:sub(1, #bare + 2) == bare .. ": "
		or path:sub(1, #root + 2) == root .. ": "
end

local function with_ripgrep(pattern: string, options: Options, limit: number): { Match }
	local args = { "rg", "--no-config", "--line-number", "--with-filename", "--null", "--no-heading", "--color", "never",
		"--hidden", "--glob", "!.git", "--sort", "path" }
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
	table.insert(args, "--")
	local target = if prefix_of(options.dir) == "" then "./" else options.dir :: string
	table.insert(args, target)
	local in_os_error = false
	return collect(args, limit, function(line: string): (string?, boolean?)
		local body = line:match("^rg: (.+)$")
		local candidate = body or (if in_os_error then line else nil)
		local path = if candidate then candidate:match("^(.+): .+ %(os error %d+%)$") else nil
		in_os_error = path ~= nil
		if not path then
			return nil
		end
		if not within_target(path, target) then
			return nil, true
		end
		return path
	end)
end

local function with_grep(pattern: string, options: Options, limit: number): { Match }
	local paths = M.files(options.glob, options.dir)
	if #paths == 0 then
		return {}
	end
	local listed_paths = {}
	for _, path in paths do
		listed_paths[path] = true
	end
	local args = { "env", "LC_ALL=C", "xargs", "-0", "grep", "-nHIZ", "-d", "skip", if options.fixed then "-F" else "-E" }
	if options.ignore_case then
		table.insert(args, "-i")
	end
	table.insert(args, "-e")
	table.insert(args, pattern)
	table.insert(args, "--")
	return collect(args, limit, function(line: string): (string?, boolean?)
		local path = line:match("^grep: (.+): [^:]+$")
		return if path and listed_paths[path] then path else nil
	end, table.concat(paths, "\0"))
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
	if limit < 0 or limit >= math.huge or limit % 1 ~= 0 then
		error("search.grep: limit must be a nonnegative whole number", 0)
	end
	local ripgrep = M.ripgrep
	if ripgrep == nil then
		ripgrep = host.sh("command -v rg", { timeout = 10 }).code == 0
	end
	local matches = if ripgrep then with_ripgrep(pattern, chosen, limit) else with_grep(pattern, chosen, limit)
	return setmetatable(matches, shown) :: any
end

return M
