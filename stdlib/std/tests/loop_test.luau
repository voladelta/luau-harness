-- std/loop compaction control, driven with scripted events (no model, no VM).
local loop = require("std/loop")
local compact = require("std/compact")
local hooks = require("std/hooks")

local PAD = string.rep("p", 4000) -- 1000 estimated tokens per call
local SYSTEM = 4000

local function start()
	hooks.clear()
	hx.model_info = function() return {} end -- the test VM has no host: no window, no history archive
	compact.context_window = 40000 -- the test VM reports no window: trigger = 40000 - 16384 - 16384 = 7232
	local s = loop.init({ task_message = "TASK", max_turns = 50 })
	return loop.step(s, { kind = "next" })
end

-- One agent reply with one call and its result, then the next model command; `prompt` is the provider's prompt count.
local function turn(s, prompt, n, pad)
	s = loop.step(s, { kind = "model_reply", system_chars = SYSTEM,
		reply = { text = "", finish = "stop", input = prompt, output = 10, calls = { { id = "c" .. n, name = "luau", input = { code = "x = 1 -- " .. (pad or PAD) } } } } })
	s = loop.step(s, { kind = "run_result", result = { ops = 1, feedback = "ok" } })
	return loop.step(s, { kind = "next" })
end

local function summary(s, text)
	return loop.step(s, { kind = "model_reply", reply = { text = text, finish = "stop", calls = {} } })
end

test.case("a summary that leaves the context above the trigger compacts again, at most twice per agent reply", function()
	local s, command = start()
	for n = 1, 6 do s, command = turn(s, 1000 * n, n) end
	-- a huge prompt count: one cut cannot get under the trigger, and every summary still shrinks what it replaces
	s, command = turn(s, 30000, 7)
	test.eq(command.purpose, "compact")
	s, command = summary(s, "SUMMARY-ONE " .. string.rep("s", 2000))
	test.eq(s.passes, 1)
	test.eq(command.kind, "next")
	s, command = loop.step(s, { kind = "next" })
	test.eq(command.purpose, "compact")
	s, command = summary(s, "SUMMARY-TWO")
	test.eq(s.passes, 2)
	s, command = loop.step(s, { kind = "next" })
	test.eq(command.purpose, "agent") -- the pass limit hands the turn back to the agent
	s, command = turn(s, 5000, 8)
	test.eq(s.passes, 0) -- an agent reply resets the count
end)

test.case("an accepted summary resets the failure count", function()
	local s, command = start()
	for n = 1, 6 do s, command = turn(s, 1000 * n, n) end
	s, command = turn(s, 9000, 7)
	test.eq(command.purpose, "compact")
	s, command = summary(s, "")
	test.eq(s.compact_failures, 1)
	s, command = loop.step(s, { kind = "next" })
	test.eq(command.purpose, "agent") -- a rejected summary skips compaction for this turn
	s, command = turn(s, 9000, 8)
	test.eq(command.purpose, "compact")
	s, command = summary(s, "SUMMARY")
	test.eq(s.compact_failures, 0)
end)

test.case("an overflow bounds the window for its one compaction only", function()
	local s, command = start()
	compact.context_window = 1000000 -- assumed 1M, real window smaller
	for n = 1, 6 do s, command = turn(s, 10000 * n, n, string.rep("p", 40000)) end -- ~60k tokens
	test.eq(command.purpose, "agent")
	local measured = compact.measure(s.anchor, s.messages)
	s, command = loop.step(s, { kind = "model_error", error = "API 400 : maximum context length exceeded", safe_context_rejection = true })
	test.eq(s.phase.bound, measured)
	test.eq(command.kind, "next")
	s, command = loop.step(s, { kind = "next" })
	test.eq(command.purpose, "compact")
	test.ok(s.phase.size <= measured - compact.reserve_tokens, "the summarizer prefix fits under the bound minus the reserve")
	s, command = summary(s, "SUMMARY")
	s, command = loop.step(s, { kind = "next" })
	test.eq(command.purpose, "agent") -- the retry: under the 1M window there is no further compaction
	test.eq(s.phase.bound, nil)
	test.eq(s.window_bound, nil) -- nothing persists: a false positive cannot shrink the window for the rest of the run
	compact.context_window = 1000000
end)

test.case("the anchor scale uses the system prompt as sent", function()
	local s = start()
	s = loop.step(s, { kind = "model_reply", system_chars = 40000,
		reply = { text = "", finish = "stop", input = 10001, output = 0, calls = { { id = "a", name = "luau", input = { code = "x = 1" } } } } })
	test.eq(s.system_chars, 40000)
	test.ok(math.abs(s.anchor.scale - 1) < 0.01, "10000 system tokens + the task: scale ~1, not the 4x clamp")
end)

test.case("a tool call that reached the text as markup is nudged, not accepted as the answer", function()
	local s, command = start()
	s, command = turn(s, 1000, 1) -- a code turn first, so plain prose would otherwise be a final answer
	s, command = loop.step(s, { kind = "model_reply", system_chars = SYSTEM,
		reply = { text = "part07: SECRET=614</｜DSML｜ parameter>", finish = "tool_markup", input = 1100, output = 10, calls = {} } })
	test.eq(s.report.finished, false)
	test.eq(s.report.truncated_turns, 1)
	test.eq(s.messages[#s.messages].content, require("std/prompt").nudge_markup)
	test.eq(command.kind, "next")
end)
