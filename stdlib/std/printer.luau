-- Smart printer: values become truncated summaries instead of dumps.
-- Override per project with .harness/modules/printer.luau (same API) -- no rebuild needed.
local M = {}

M.STR_TOP = 6000 -- a bare string the model asked to see
M.STR_CELL = 200 -- a string nested inside a table
M.ROWS = 5 -- record rows shown under a table summary
M.ITEMS = 10 -- scalars / keys shown inline
M.DEPTH = 3
M.ROW_CHARS = 240
M.OUTPUT_CAP = 12000 -- total characters of printed output per turn

local function clip(s: string, max: number): string
	local n = utf8.len(s)
	if n == nil or n <= max then
		if n == nil and #s > max then
			return s:sub(1, max) .. "…(+" .. (#s - max) .. " bytes)"
		end
		return s
	end
	local cut = utf8.offset(s, max + 1) - 1
	return s:sub(1, cut) .. "…(+" .. (n - max) .. " chars)"
end

local function num(n: number): string
	if n == math.floor(n) and math.abs(n) < 1e15 then
		return string.format("%d", n)
	end
	return tostring(n)
end

-- Split a table into its array part and its sorted keyed entries.
local function split(t)
	local n = rawlen(t)
	local arr = table.create(n)
	for i = 1, n do
		arr[i] = rawget(t, i)
	end
	local rest = {}
	for k, v in next, t do
		local inarr = type(k) == "number" and k >= 1 and k <= n and k == math.floor(k)
		if not inarr then
			local key = if type(k) == "string" then k elseif type(k) == "number" then num(k) else "<" .. typeof(k) .. ">"
			table.insert(rest, { key, v })
		end
	end
	table.sort(rest, function(a, b)
		return a[1] < b[1]
	end)
	return arr, rest
end

local function keyed(t): { string }?
	local arr, rest = split(t)
	if #arr == 0 and #rest > 0 then
		local keys = {}
		for _, kv in rest do
			table.insert(keys, kv[1])
		end
		return keys
	end
	return nil
end

local function isrecords(arr): boolean
	if #arr == 0 then
		return false
	end
	for i = 1, math.min(#arr, M.ROWS) do
		if type(arr[i]) ~= "table" or keyed(arr[i]) == nil then
			return false
		end
	end
	return true
end

local fmt

local function fmttable(t, depth: number): string
	local arr, rest = split(t)
	if #arr == 0 and #rest == 0 then
		return "{}"
	end
	if depth >= M.DEPTH then
		return "<table: " .. (#arr + #rest) .. " items>"
	end
	if isrecords(arr) then
		return "<table: " .. #arr .. " rows {" .. table.concat(keyed(arr[1]) :: any, ",") .. "}>"
	end
	local parts = {}
	for i = 1, math.min(#arr, M.ITEMS) do
		table.insert(parts, fmt(arr[i], depth + 1))
	end
	local shown = #parts
	for i = 1, math.min(#rest, math.max(M.ITEMS - shown, 0)) do
		table.insert(parts, rest[i][1] .. "=" .. fmt(rest[i][2], depth + 1))
		shown += 1
	end
	local total = #arr + #rest
	if total > shown then
		table.insert(parts, "…+" .. (total - shown))
	end
	if #rest == 0 then
		return "[" .. table.concat(parts, ", ") .. "]"
	end
	return "{" .. table.concat(parts, ", ") .. "}"
end

fmt = function(v: any, depth: number): string
	local ty = typeof(v)
	if ty == "nil" then
		return "nil"
	elseif ty == "boolean" then
		return tostring(v)
	elseif ty == "number" then
		return num(v)
	elseif ty == "string" then
		return '"' .. clip((v:gsub("\n", "\\n")), M.STR_CELL) .. '"'
	elseif ty == "table" then
		return fmttable(v, depth)
	elseif ty == "function" then
		return "<function>"
	elseif ty == "error" or ty == "userdata" then
		-- error objects raised by native primitives (caught with pcall): show the message, not the Rust traceback
		local msg = tostring(v):gsub("^runtime error: ", ""):gsub("\nstack traceback:.*$", "")
		return msg
	end
	return "<" .. ty .. ">"
end

local function toptable(t): string
	local arr, rest = split(t)
	local capped = false
	local ok, flag = pcall(function()
		return t.truncated
	end)
	if ok and flag == true then
		capped = true
	end
	local cap = if capped then ", capped" else ""
	if isrecords(arr) then
		local out = { "<table: " .. #arr .. " rows {" .. table.concat(keyed(arr[1]) :: any, ",") .. "}" .. cap .. ">" }
		for i = 1, math.min(#arr, M.ROWS) do
			table.insert(out, "  [" .. i .. "] " .. clip(fmt(arr[i], 1), M.ROW_CHARS))
		end
		if #arr > M.ROWS then
			table.insert(out, "  …+" .. (#arr - M.ROWS) .. " more rows")
		end
		return table.concat(out, "\n")
	end
	if #arr > 0 and #rest == 0 then
		return "<array: " .. #arr .. " items" .. cap .. "> " .. fmttable(t, 0)
	end
	if #arr == 0 and #rest > 0 then
		return "<table: " .. #rest .. " keys> " .. fmttable(t, 0)
	end
	return fmttable(t, 0)
end

-- Summary of one top-level value (what print() and the last expression show).
function M.top(v: any): string
	if type(v) == "string" then
		local n = utf8.len(v) or #v
		if n <= M.STR_TOP then
			return v
		end
		return clip(v, M.STR_TOP)
			.. "\n…[string truncated: "
			.. M.STR_TOP
			.. " of "
			.. n
			.. " chars shown; slice it or use read(path,{offset,limit})]"
	elseif type(v) == "table" then
		return toptable(v)
	end
	return fmt(v, 0)
end

-- Nested form (used for variable listings).
function M.inline(v: any): string
	return fmt(v, 1)
end

-- Cap the total printed output of one turn.
function M.cap(text: string): string
	local n = utf8.len(text) or #text
	if n <= M.OUTPUT_CAP then
		return text
	end
	return clip(text, M.OUTPUT_CAP) .. "\n…[output truncated: " .. (n - M.OUTPUT_CAP) .. " chars omitted; print less or summarize]"
end

return M
