-- POSIX paths as strings, as Python's posixpath has them: join(...), dirname, basename, ext (".luau" or ""), normalize, relative(path, from?) -> string
--!strict

local M = {}

local function check(fname: string, value: unknown, position: number, name: string, optional: boolean?)
	if type(value) == "string" or value == nil and optional then
		return
	end
	error(("path.%s: argument %d (%s) must be a string, not %s"):format(fname, position, name, type(value)), 0)
end

function M.join(first: string, ...: string): string
	check("join", first, 1, "path")
	local out: string = first
	for i, part in { ... } do
		check("join", part, i + 1, "path")
		local glue = if out == "" or out:sub(-1) == "/" then "" else "/"
		out = if part:sub(1, 1) == "/" then part else out .. glue .. part
	end
	return out
end

local function split(path: string): (string, string)
	local cut = 0
	for i = #path, 1, -1 do
		if path:sub(i, i) == "/" then
			cut = i
			break
		end
	end
	return path:sub(1, cut), path:sub(cut + 1)
end

function M.basename(path: string): string
	check("basename", path, 1, "path")
	local _, tail = split(path)
	return tail
end

function M.dirname(path: string): string
	check("dirname", path, 1, "path")
	local head = split(path)
	if head ~= "" and head ~= string.rep("/", #head) then
		head = head:gsub("/+$", "")
	end
	return head
end

function M.ext(path: string): string
	check("ext", path, 1, "path")
	local name = M.basename(path)
	local dot = name:match("^%.*[^.].*()%.[^.]*$") :: number?
	return if dot then name:sub(dot) else ""
end

function M.normalize(path: string): string
	check("normalize", path, 1, "path")
	if path == "" then
		return "."
	end
	local slashes = path:match("^/*") :: string
	local root = if #slashes == 2 then "//" elseif #slashes > 0 then "/" else ""
	local parts: { string } = {}
	for part in path:gmatch("[^/]+") do
		if part == ".." then
			if #parts > 0 and parts[#parts] ~= ".." then
				table.remove(parts)
			elseif root == "" then
				table.insert(parts, part)
			end
		elseif part ~= "." then
			table.insert(parts, part)
		end
	end
	local out = root .. table.concat(parts, "/")
	return if out == "" then "." else out
end

local function absolute(path: string): string
	return M.normalize(if path:sub(1, 1) == "/" then path else M.join(host.cwd, path))
end

function M.relative(path: string, from: string?): string
	check("relative", path, 1, "path")
	check("relative", from, 2, "from", true)
	if path == "" then
		error("path.relative: argument 1 (path) is empty", 0)
	end
	local to_parts = absolute(path):split("/")
	local from_parts = absolute(from or "."):split("/")
	local function clean(parts: { string }): { string }
		local out = {}
		for _, part in parts do
			if part ~= "" then
				table.insert(out, part)
			end
		end
		return out
	end
	local a, b = clean(from_parts), clean(to_parts)
	local same = 0
	while same < #a and same < #b and a[same + 1] == b[same + 1] do
		same += 1
	end
	local out = {}
	for _ = same + 1, #a do
		table.insert(out, "..")
	end
	for i = same + 1, #b do
		table.insert(out, b[i])
	end
	return if #out == 0 then "." else table.concat(out, "/")
end

return M
