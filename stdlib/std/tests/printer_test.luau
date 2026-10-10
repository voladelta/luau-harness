local printer = require("std/printer")

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

test.case("long strings are cut with a notice", function()
	local out = printer.top(string.rep("x", 7000))
	test.ok(out:find("string truncated: 6000 of 7000 chars shown", 1, true), out:sub(-120))
end)

test.case("output cap", function()
	test.eq(printer.cap("short"), "short")
	local out = printer.cap(string.rep("y", 13000))
	test.ok(out:find("output truncated: 1000 chars omitted", 1, true))
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
