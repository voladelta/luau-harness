-- Minimal test library: test.case(name, fn), test.eq(a, b), test.ok(cond, msg). `hx test <dir>` runs tests/*.luau.
local T = { results = {} }

local function show(v: any): string
	if type(v) == "string" then
		return string.format("%q", v)
	end
	return tostring(v)
end

function T.case(name: string, fn: () -> ())
	local ok, err = pcall(fn)
	table.insert(T.results, { name = name, ok = ok, err = if ok then nil else tostring(err) })
end

function T.eq(a: any, b: any)
	if a ~= b then
		error("expected " .. show(b) .. ", got " .. show(a), 2)
	end
end

function T.ok(cond: any, msg: any?)
	if not cond then
		error("assertion failed" .. (if msg then ": " .. tostring(msg) else ""), 2)
	end
end

return T
