local extract = require("std/extract")
local D = "｜"

test.case("plain block", function()
	local code, multi = extract.code("hi\n```luau\nprint(1)\n```\nbye")
	test.eq(code, "print(1)")
	test.eq(multi, false)
end)

test.case("backticks inside a line of code do not close the fence", function()
	local t = '```luau\nwrite("p.py", [==[\nif line.strip() == "```":\n    x = 1\n]==])\nprint(1)\n```'
	local code = extract.code(t)
	test.ok(code and code:find("x = 1", 1, true) and code:sub(-8) == "print(1)", tostring(code))
end)

test.case("a longer fence can hold a line of backticks", function()
	local t = '````luau\nwrite("a.md", [==[\n```\ncode\n```\n]==])\n````'
	local code = extract.code(t)
	test.ok(code and code:find("code", 1, true) and code:sub(-5) == "]==])", tostring(code))
end)

test.case("first of several blocks wins and is flagged", function()
	local code, multi = extract.code("```luau\na()\n```\n```luau\nb()\n```")
	test.eq(code, "a()")
	test.eq(multi, true)
end)

test.case("tool-call markup", function()
	local a = "<" .. D .. "DSML" .. D .. ' invoke name="luau">\n<' .. D .. "DSML" .. D .. ' parameter name="code" string="true">\nprint(2)\n</' .. D .. "DSML" .. D .. " parameter>\n</" .. D .. "DSML" .. D .. " invoke>"
	test.eq(extract.code(a), "print(2)")
	local b = "```luau\nprint(3)\n</" .. D .. "DSML" .. D .. " parameter>\n</" .. D .. "DSML" .. D .. " invoke>"
	test.eq(extract.code(b), "print(3)")
end)

test.case("truncated or missing blocks are not runnable", function()
	test.eq(extract.code('```luau\nwrite("x", [==[\nhalf of a fi'), nil)
	test.eq(extract.code("no code here"), nil)
end)

test.case("claude-style invoke/parameter markup, with and without a namespace prefix", function()
	local plain = '<invoke name="luau">\n<parameter name="code">\nprint(7)\n</parameter>\n</invoke>'
	test.eq(extract.code(plain), "print(7)")
	local ns = "x" .. ":parameter"
	local prefixed = '<invoke name="luau"><' .. ns .. ' name="code" string="true">write("a", "1")\nprint(2)</' .. ns .. '></invoke>'
	test.eq(extract.code(prefixed), 'write("a", "1")\nprint(2)')
	test.eq(extract.code('<invoke name="luau"><parameter name="code">print(1)'), nil) -- cut off: not runnable
	test.eq(extract.code('<parameter name="path">a.txt</parameter>'), nil) -- not a code parameter
end)
