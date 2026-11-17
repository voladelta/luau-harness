-- plain text without pattern traps: escape(s) for a Lua pattern, replace(s, old, new, n?) -> s, count with no pattern meaning, trim, starts(s, prefix), ends(s, suffix), lines(s) -> {string}
--!strict

local M = {}

local function check(fname: string, value: unknown, position: number, name: string, kind: string, optional: boolean?)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("text.%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

function M.escape(s: string): string
	check("escape", s, 1, "s", "string")
	return (s:gsub("[%^%$%(%)%%%.%[%]%*%+%-%?]", "%%%0"))
end

function M.replace(s: string, old: string, new: string, n: number?): (string, number)
	check("replace", s, 1, "s", "string")
	check("replace", old, 2, "old", "string")
	check("replace", new, 3, "new", "string")
	check("replace", n, 4, "n", "number", true)
	if old == "" then
		error("text.replace: argument 2 (old) is empty", 0)
	end
	return s:gsub(M.escape(old), (new:gsub("%%", "%%%%")), n)
end

function M.trim(s: string): string
	check("trim", s, 1, "s", "string")
	return s:match("^%s*(.-)%s*$") :: string
end

function M.starts(s: string, prefix: string): boolean
	check("starts", s, 1, "s", "string")
	check("starts", prefix, 2, "prefix", "string")
	return s:sub(1, #prefix) == prefix
end

function M.ends(s: string, suffix: string): boolean
	check("ends", s, 1, "s", "string")
	check("ends", suffix, 2, "suffix", "string")
	return suffix == "" or s:sub(-#suffix) == suffix
end

function M.lines(s: string): { string }
	check("lines", s, 1, "s", "string")
	local out = {}
	for line in s:gmatch("([^\n]*)\n") do
		table.insert(out, (line:gsub("\r$", "")))
	end
	local rest = s:match("[^\n]*$") :: string
	if rest ~= "" then
		table.insert(out, (rest:gsub("\r$", "")))
	end
	return out
end

return M
