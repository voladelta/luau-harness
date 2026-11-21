local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = checks.check, checks.raises

local text = loop.ext("text")

local magic = "^$()%.[]*+-?"
check("escape makes every magic character literal", (magic):match(text.escape(magic)), magic)
check("escape leaves the rest", text.escape("a b_c"), "a b_c")

local out, count = text.replace("1.5 + 1.5 = 3", "1.5", "%1")
check("replace takes old as it is", out, "%1 + %1 = 3")
check("and counts", count, 2)
check("replace at most n", (text.replace("a-a-a", "-", "+", 1)), "a+a-a")
check("replace with nothing found", (text.replace("abc", "x.", "y")), "abc")
raises("replace with nothing to find", "text.replace: argument 2 (old) is empty", text.replace, "abc", "", "x")

check("trim", text.trim(" \t a b \n"), "a b")
check("trim of spaces", text.trim("   "), "")
check("starts", text.starts("src/a.c", "src/"), true)
check("starts not", text.starts("src", "src/"), false)
check("ends", text.ends("a.luau", ".luau"), true)
check("ends with nothing", text.ends("a", ""), true)
check("ends not", text.ends("a.lua", ".luau"), false)

check("lines", json.encode(text.lines("a\nb\r\n\nc")), '["a","b","","c"]')
check("lines with a final newline", json.encode(text.lines("a\nb\n")), '["a","b"]')
check("lines of nothing", #text.lines(""), 0)
raises("lines of a number", "text.lines: argument 1 (s) must be a string, not number", text.lines, 1)

checks.done("text")
