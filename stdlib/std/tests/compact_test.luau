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

test.case("find_cut walks back to an assistant message and never cuts the task or a tool exchange", function()
	local pad = string.rep("x", 400) -- 100 tokens
	local m = { user("task"), asst(pad, "a"), tool("a", "ok"), asst(pad, "b"), tool("b", "ok"), asst(pad, "c"), tool("c", "ok") }
	for keep = 0, 400 do
		local cut = compact.find_cut(m, keep, 1, 1000, 1000)
		if cut then
			test.eq(m[cut].role, "assistant")
			test.ok(cut >= 3)
		end
	end
	test.eq(compact.find_cut(m, 0, 1, 1000, 1000), 6) -- keep 0: the newest complete exchange
	test.eq(compact.find_cut(m, 101, 1, 1000, 1000), 6) -- "c" and its result are 101 tokens
	test.eq(compact.find_cut(m, 102, 1, 1000, 1000), 4) -- one more token: back to "b"
	test.eq(compact.find_cut(m, 1000, 1, 1000, 1000), nil) -- everything fits in the kept budget: nothing to summarize
	test.eq(compact.find_cut({ user("task"), asst("x", "a"), tool("a", "ok") }, 0, 1, 1000, 1000), nil) -- only one exchange
	test.eq(compact.find_cut({ user("task"), user("u"), user("tail") }, 0, 1, 1000, 1000), nil) -- no assistant message
end)

test.case("find_cut keeps more until the summarizer prefix fits, and returns its size", function()
	local pad = string.rep("x", 400)
	local m = { user("task"), asst(pad, "a"), tool("a", "ok"), asst(pad, "b"), tool("b", "ok"), asst(pad, "c"), tool("c", "ok") }
	local cut, size = compact.find_cut(m, 0, 1, 1000, 1000)
	test.eq(cut, 6); test.eq(size, 1000 - 101)
	cut, size = compact.find_cut(m, 0, 1, 1000, 850) -- 899 does not fit: keep "b" too
	test.eq(cut, 4); test.eq(size, 1000 - 202)
	test.eq(compact.find_cut(m, 0, 1, 1000, 100), nil) -- no cut leaves a prefix that small
	test.eq(compact.find_cut(m, 150, 2, 1000, 1000), 6) -- the scale applies: "c" weighs 202
end)

test.case("request replays the agent prefix and appends one instruction", function()
	local m = { user("TASK"), asst("x = 1", "a"), tool("a", "1"), asst("y = 2", "b"), tool("b", "2") }
	local tools = { { name = "luau" } }
	local r = compact.request("SYS", tools, m, 4)
	test.eq(r.system, "SYS"); test.eq(r.tools, tools)
	test.eq(#r.messages, 4)
	for i = 1, 3 do test.eq(r.messages[i], m[i]) end -- the same message tables, unchanged
	test.eq(r.messages[4].role, "user")
	test.ok(r.messages[4].content:find("context checkpoint", 1, true))
	test.ok(r.messages[4].content:find("[compaction-summary]", 1, true), "asks to merge an earlier checkpoint")
	test.eq(r.replay, true)
	test.eq(#m, 5)
end)

test.case("accept takes only a complete, text-only, smaller summary", function()
	local m = { user("TASK"), asst(string.rep("x", 4000), "a"), tool("a", "ok"), asst("y", "b") }
	local first = compact.accept({ text = "SUM", finish = "stop" }, "TASK", m, 4, false)
	test.ok(first:find("^TASK\n\n%[compaction%-summary%]"), first)
	test.ok(first:find("SUM", 1, true))
	test.ok(compact.accept({ text = "SUM" }, "TASK", m, 4, true):find("history({", 1, true), "the archive notice when history is on")
	test.eq(select(2, compact.accept({ text = "SUM", finish = "length" }, "TASK", m, 4, false)), "truncated")
	test.eq(select(2, compact.accept({ text = "SUM", calls = { {} } }, "TASK", m, 4, false)), "tool call")
	test.eq(select(2, compact.accept({ text = "  \n" }, "TASK", m, 4, false)), "empty")
	test.eq(select(2, compact.accept({ text = string.rep("s", 5000) }, "TASK", m, 4, false)), "not smaller")
end)

test.case("estimate counts the raw provider message (with its reasoning) instead of the text", function()
	local plain = { role = "assistant", content = "", calls = { { id = "a", name = "luau", input = { code = string.rep("c", 40) } } } }
	test.eq(compact.estimate(plain), 10)
	plain.raw = string.rep("r", 4000) -- reasoning echoed back each turn
	test.eq(compact.estimate(plain), 1000)
	test.eq(compact.estimate({ role = "user", content = "abcd", images = { {}, {} } }), 1 + 2 * 1200)
end)

test.case("anchor calibrates on the provider count; measure adds later messages at that scale", function()
	local m = { user(string.rep("u", 400)), tool("a", string.rep("t", 400)) } -- 200 estimated tokens
	local a = compact.anchor(string.rep("s", 400), m, 600, 50) -- 300 estimated with the system prompt: scale 2
	test.eq(a.scale, 2); test.eq(a.count, 3); test.eq(a.tokens, 650)
	test.eq(compact.measure(a, m), 650) -- nothing after the reply yet
	table.insert(m, { role = "assistant", content = "", raw = string.rep("r", 40) })
	table.insert(m, tool("b", string.rep("t", 400)))
	test.eq(compact.measure(a, m), 650 + 200) -- the reply is counted; the new result is 100 estimated, times 2
	test.eq(compact.anchor("", m, 0, 0).scale, 1) -- no usage
	test.eq(compact.anchor("", m, 10 ^ 6, 0).scale, 4)
	test.eq(compact.anchor("", m, 1, 0).scale, 0.5)
	local r = compact.reanchor(a, string.rep("s", 400), { user(string.rep("u", 400)) })
	test.eq(r.tokens, 400); test.eq(r.count, 1); test.eq(r.scale, 2)
end)

test.case("trigger caps the threshold at max_trigger_tokens", function()
	test.eq(compact.trigger(1000000, 16384), 256000) -- an unknown window is assumed to be 1M: start at 256k
	test.eq(compact.trigger(200000, 16384), 167232) -- a known smaller window: the threshold wins
	test.ok(compact.trigger(10000, 16384) <= 0, "too small a window stays off")
end)
