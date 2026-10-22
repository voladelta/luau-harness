local printer = require("std/printer")
-- The session keeps cut values as `_cut` (hx.retain); these tests run in the module env, so a stub stands in for it.
local real_retain = hx.retain
local _cut = {}
hx.retain = function(v) table.insert(_cut, v); return #_cut end

test.case("scalars", function()
	test.eq(printer.top(42), "42")
	test.eq(printer.top(1.5), "1.5")
	test.eq(printer.top(true), "true")
	test.eq(printer.top(nil), "nil")
	test.eq(printer.top("hi"), "hi")
end)

test.case("record tables print as a summary plus first rows", function()
	local rows = {}
	for i = 1, 30 do
		rows[i] = { file = "a" .. i, line = i }
	end
	local out = printer.top(rows)
	test.eq(out:match("^[^\n]+"), "<table: 30 rows {file,line}>")
	test.ok(out:find('[1] {file="a1", line=1}', 1, true), out)
	test.ok(out:find("…+25 more rows", 1, true), out)
end)

test.case("capped marker comes from the truncated flag", function()
	local rows = setmetatable({ { a = 1 } }, { __index = { truncated = true } })
	test.eq(printer.top(rows):match("^[^\n]+"), "<table: 1 rows {a}, capped>")
end)

test.case("arrays and maps", function()
	test.eq(printer.top({ 2, 4, 6 }), "<array: 3 items> [2, 4, 6]")
	test.eq(printer.top({ b = 1, a = "x" }), '<table: 2 keys> {a="x", b=1}')
	test.eq(printer.top({}), "{}")
end)

test.case("long strings keep their start and end, and the notice names the omitted middle", function()
	local s = string.rep("x", 3500) .. "MIDDLE" .. string.rep("y", 3494) .. "THE-END"
	local out = printer.top(s)
	test.ok(out:sub(1, 10) == "xxxxxxxxxx" and out:sub(-7) == "THE-END", "head and tail kept")
	test.ok(out:find("string truncated: the first 3000 and last 3000 of 7007 chars are shown", 1, true), out:sub(2990, 3200))
	local expr = out:match("the full string is _cut%[%d+%] in your next call: search it.-%(omitted bytes: (_cut%[%d+%]:sub%(%d+, %d+%))%)")
	test.ok(expr, "names a retrieval expression")
	local i, a, b = expr:match("_cut%[(%d+)%]:sub%((%d+), (%d+)%)")
	local middle = _cut[tonumber(i)]:sub(tonumber(a), tonumber(b))
	test.eq(#middle, 1007)
	test.ok(middle:find("MIDDLE", 1, true))
	test.eq(out:sub(1, 3000) .. middle .. out:sub(-3000), s) -- head, middle and tail rebuild the string exactly
end)

test.case("cutting multibyte text keeps whole characters and byte-exact retrieval", function()
	local s = string.rep("é", 7000)
	local out = printer.top(s)
	test.ok(utf8.len(out) ~= nil, "still valid UTF-8")
	local i, a, b = out:match("_cut%[(%d+)%]:sub%((%d+), (%d+)%)")
	test.eq(utf8.len(_cut[tonumber(i)]:sub(tonumber(a), tonumber(b))), 1000)
end)

test.case("output cap keeps start and end", function()
	test.eq(printer.cap("short"), "short")
	local out = printer.cap(string.rep("y", 13000) .. "LAST")
	test.ok(out:find("output truncated: the first 6000 and last 6000 of 13004 chars are shown", 1, true))
	test.eq(out:sub(-4), "LAST")
end)

test.case("more rows name the retained table", function()
	local rows = {}
	for i = 1, 30 do rows[i] = { n = i } end
	local out = printer.top(rows)
	local i = tonumber(out:match("all rows: _cut%[(%d+)%]"))
	test.eq(_cut[i], rows)
end)

test.case("depth limit", function()
	test.eq(printer.top({ a = { b = { c = { d = 1 } } } }), "<table: 1 keys> {a={b={c=<table: 1 items>}}}")
end)

test.case("errors caught from native primitives print their message", function()
	local ok, e = pcall(read, "definitely-not-a-file.txt")
	test.eq(ok, false)
	local out = printer.top(e)
	test.ok(out:find("No such file", 1, true), out)
	test.ok(not out:find("stack traceback", 1, true), out)
end)

hx.retain = real_retain
