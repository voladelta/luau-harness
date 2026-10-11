local compact = require("std/compact")

local function user(text) return { role = "user", content = text } end
local function asst(code, id) return { role = "assistant", content = "", calls = { { id = id, name = "luau", input = { code = code } } } } end
local function tool(id, text) return { role = "tool", id = id, content = text } end

test.case("threshold", function()
	test.eq(compact.threshold(128000, 16384, 16384), 95232)
	test.eq(compact.threshold(40000, 1000, 16384), 22616)
	test.eq(compact.threshold(200000, 1000, 100), 190000) -- 95% of the window wins; the reserve is at least 4096
	test.ok(compact.threshold(10000, 16384, 16384) <= 0, "a window smaller than the reply budget disables compaction")
end)

test.case("find_cut moves to an assistant message and never returns a tool message", function()
	local pad = string.rep("x", 400) -- 100 tokens
	local m = { user("task"), asst(pad, "a"), tool("a", "ok"), asst(pad, "b"), tool("b", "ok"), asst(pad, "c"), tool("c", "ok") }
	for keep = 1, 400 do
		local cut = compact.find_cut(m, 2, keep)
		if cut then
			test.eq(m[cut].role, "assistant")
			test.ok(cut > 2)
		end
	end
	test.eq(compact.find_cut(m, 2, 150), 4) -- "c" and its result are ~101 tokens, "b" brings it past 150
	test.eq(compact.find_cut(m, 2, 50), 6) -- "c" alone is enough
	test.eq(compact.find_cut(m, 2, 102), 6) -- reached at the tool result of "b": move forward to "c"...
	test.eq(compact.find_cut({ user("task"), asst("a", "a"), tool("a", string.rep("r", 400)), asst("b", "b"), tool("b", "ok") }, 2, 60), 4) -- ...or inside a result: move forward to the next assistant
end)

test.case("find_cut is nil when the whole history fits or nothing would be summarized", function()
	local m = { user("task"), asst("x = 1", "a"), tool("a", "ok"), asst("y = 2", "b"), tool("b", "ok") }
	test.eq(compact.find_cut(m, 2, 100000), nil)
	test.eq(compact.find_cut({ user("task") }, 2, 1), nil)
	-- the only assistant message after `start` is the first one: nothing before it to summarize
	local n = { user("task"), asst(string.rep("x", 400), "a"), tool("a", "ok") }
	test.eq(compact.find_cut(n, 2, 10), nil)
	-- no assistant message to cut at
	test.eq(compact.find_cut({ user("task"), user(string.rep("u", 400)), user("tail") }, 2, 10), nil)
end)

test.case("serialize numbers calls and results and labels roles", function()
	local m = { user("task"), asst("print(1)", "a"), tool("a", "1"), { role = "assistant", content = "thinking out loud" }, user("go on") }
	local out = compact.serialize(m, 2, 5)
	test.ok(out:find("[Assistant tool calls]: #1 luau(code=print(1))", 1, true), out)
	test.ok(out:find("[Tool result #1]: 1", 1, true), out)
	test.ok(out:find("[Assistant]: thinking out loud", 1, true), out)
	test.ok(out:find("[User]: go on", 1, true), out)
	test.ok(not out:find("task", 1, true), "from/to bound the range")
end)

test.case("serialize truncates long text keeping the head and the 500-char tail", function()
	local long = string.rep("a", 1500) .. string.rep("m", 3000) .. string.rep("z", 500)
	local out = compact.serialize({ tool("a", long) }, 1, 1)
	test.ok(out:find(string.rep("a", 1500) .. "\n…[truncated]…\n" .. string.rep("z", 500), 1, true), "head 1500 + marker + tail 500")
	test.ok(not out:find("m", 1, true), "the middle is gone")
	test.eq(compact.serialize({ tool("a", string.rep("q", 2000)) }, 1, 1), "[Tool result]: " .. string.rep("q", 2000))
end)

test.case("serialize keeps UTF-8 intact when truncating", function()
	local out = compact.serialize({ user(string.rep("é", 2000)) }, 1, 1)
	test.ok(utf8.len(out) ~= nil, "valid UTF-8")
end)

test.case("serialize shows images as [image]", function()
	local m = { { role = "user", content = "look", images = { { mime = "image/png", data = "AAAA" } } } }
	test.eq(compact.serialize(m, 1, 1), "[User]: look\n[image]")
end)
