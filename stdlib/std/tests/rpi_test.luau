local rpi = require("std/rpi")
local loop = require("std/loop")
local hooks = require("std/hooks")
local function initial(mode, max_turns)
	hooks.clear()
	local s = rpi.init({ workflow = { name = "rpi", mode = mode or "auto", max_repairs = 3 }, task_message = "Add a feature", max_turns = max_turns or 100 })
	local command
	s, command = rpi.step(s, { kind = "next" })
	return s, command
end
local function deliver(s, command, owner_inputs)
	return rpi.step(s, { kind = "boundary_ready", stage = command.next.stage, sealed = if type(newproxy) == "function" then newproxy() else nil,
		artifacts = { { id = "request", name = "request", document = "Add a feature" } }, owner_inputs = owner_inputs or {} })
end
local function submit(s, payload, id)
	s.worker.phase = { kind = "run", calls = { { id = "call", name = "luau", input = { code = "stage.submit({})" } }, { id = "trailing", name = "luau", input = { code = "error('must not run')" } } }, index = 1 }
	return rpi.step(s, { kind = "run_result", result = { ops = 0, feedback = "accepted", submission = { id = id or s.stage.id, payload = payload } } })
end
local function complete(name)
	return { outcome = "complete", summary = name, evidence = {}, checks = {}, findings = {}, verification = { passed = true, findings = {} } }
end

test.case("auto runs all stages with fresh workers and explicit verification", function()
	local s, command = initial()
	for _, name in { "research_questions", "research", "system_design", "program_design", "implementation", "verification" } do
		test.eq(command.kind, "boundary")
		test.eq(command.next.stage.name, name)
		test.eq(command.next.stage.access, if name == "implementation" or name == "verification" then "workspace" else "inspect")
		s = deliver(s, command)
		test.eq(#s.messages, 1)
		test.eq(s.messages, s.worker.messages)
		test.eq(s.worker.report.turns, 0)
		s, command = submit(s, complete(name))
	end
	test.eq(command.next.kind, "finish")
	test.eq(command.next.report.finished, true)
end)

test.case("manual review and owner feedback survive fresh contexts", function()
	local s, command = initial("manual")
	s = deliver(s, command)
	s, command = submit(s, complete("questions"))
	test.eq(command.next.kind, "review")
	test.eq(command.next.request.kind, "review")
	local request = command.next.request
	s, command = rpi.step(s, { kind = "owner_decision", request = request, decision = "approve", text = "Keep APIs stable" })
	test.eq(command.next.stage.name, "research")
	s = deliver(s, command, { { decision = "approve", text = "Keep APIs stable" } })
	test.ok(s.messages[1].content:find("Keep APIs stable", 1, true))
end)

test.case("three plain prose candidates stop instead of finishing", function()
	local s, command = initial()
	s = deliver(s, command)
	for _ = 1, 3 do
		s.worker.phase = { kind = "model" }
		s, command = rpi.step(s, { kind = "model_reply", reply = { text = "Done", calls = {}, finish = "stop" } })
	end
	test.eq(command.next.kind, "finish")
	test.eq(command.next.report.status, "stopped")
	test.eq(command.next.report.finished, false)
	test.eq(s.turns, 3)
end)

test.case("native submission is captured before hooks and trailing calls", function()
	hooks.clear()
	hooks.on("after_run", function(p) p.submission.payload.summary = "forged"; return p end)
	local worker = loop.init({ task_message = "test", max_turns = 2 })
	worker.phase = { kind = "run", calls = { { id = "one" }, { id = "two", name = "luau", input = { code = "error('trailing')" } } }, index = 1 }
	local outcome
	worker, outcome = loop.advance(worker, { kind = "run_result", result = { ops = 0, feedback = "ok", submission = { id = "native", payload = { summary = "original" } } } }, { submissions = true, recheck = false })
	test.eq(outcome.kind, "submission")
	test.eq(outcome.submission.payload.summary, "original")
	test.eq(worker.phase.kind, "submitted")
	hooks.clear()
end)

test.case("worker stops cannot be revived and root turn budget survives stages", function()
	local s, command = initial("auto", 1)
	s = deliver(s, command)
	s.worker.report.turns = 1
	s.turns = 1
	s, command = submit(s, complete("questions"))
	s = deliver(s, command)
	test.eq(s.worker.max_turns, 0)
	s, command = rpi.step(s, { kind = "next" })
	test.eq(command.next.report.status, "stopped")
	test.ok(not pcall(loop.continue, s.worker, "revive"))
end)

test.case("verification failures trigger three bounded repair cycles", function()
	local s, command = initial()
	for _ = 1, 5 do s = deliver(s, command); s, command = submit(s, complete("phase")) end
	for i = 1, 4 do
		s = deliver(s, command)
		local payload = complete("failed verification")
		payload.verification = { passed = false, findings = { "failed receipt" } }
		s, command = submit(s, payload)
		if i <= 3 then
			test.eq(command.next.stage.name, "repair")
			s = deliver(s, command)
			s, command = submit(s, complete("repair"))
			test.eq(command.next.stage.name, "verification")
		end
	end
	test.eq(command.next.report.status, "stopped")
	test.eq(command.next.report.repairs, 3)
end)

test.case("necessary input pauses even automatic mode and answer retries same stage", function()
	local s, command = initial()
	s = deliver(s, command)
	s, command = submit(s, { outcome = "needs_input", document = "draft", summary = "Need API source", evidence = {}, questions = { "Which API?" } })
	test.eq(command.next.request.kind, "input")
	local request = command.next.request
	s, command = rpi.step(s, { kind = "owner_decision", decision = "answer", request = request, text = "v2" })
	test.eq(command.next.stage.name, "research_questions")
	test.eq(command.next.stage.id, "research_questions-2")
end)

test.case("manual answers require fresh verification and waivers remain explicit", function()
	local s, command = initial()
	for _ = 1, 5 do s = deliver(s, command); s, command = submit(s, complete("phase")) end
	s = deliver(s, command)
	local payload = complete("manual required")
	payload.manual_check_ids = { "visual-review" }
	payload.questions = { "Confirm the visual review result" }
	s, command = submit(s, payload)
	local request = command.next.request
	test.eq(request.kind, "input")
	test.eq(request.check_ids[1], "visual-review")
	s, command = rpi.step(s, { kind = "owner_decision", request = request, decision = "waive", text = "Skip visual review for this task" })
	test.eq(command.next.stage.name, "verification")
	test.eq(s.waived_checks["visual-review"], true)
	s = deliver(s, command)
	s, command = submit(s, payload)
	test.eq(command.next.kind, "finish")
	test.eq(command.next.report.waived_checks["visual-review"], true)
end)

test.case("redesign detours consume existing repair budget and invalidate downstream", function()
	local s, command = initial()
	for _ = 1, 5 do s = deliver(s, command); s, command = submit(s, complete("phase")) end
	s = deliver(s, command)
	local payload = complete("failed verification")
	payload.findings = { "wrong design assumption" }
	s, command = submit(s, payload)
	s = deliver(s, command)
	payload = complete("needs redesign")
	payload.redesign_stage = "system_design"
	s, command = submit(s, payload)
	test.eq(command.next.stage.name, "system_design")
	test.eq(s.repairs, 1)
	test.eq(s.artifacts.program_design, nil)
	test.eq(s.artifacts.implementation, nil)
	test.eq(s.artifacts.verification, nil)
end)

test.case("submission validation errors count across unrelated successful tool calls", function()
	local s, command = initial()
	s = deliver(s, command)
	for i = 1, 3 do
		s.worker.phase = { kind = "run", calls = { { id = "invalid" } }, index = 1 }
		s, command = rpi.step(s, { kind = "run_result", result = { ops = 0, feedback = "invalid", error = "stage.submit: malformed payload" } })
		if i < 3 then
			s.worker.phase = { kind = "run", calls = { { id = "ordinary" } }, index = 1 }
			s, command = rpi.step(s, { kind = "run_result", result = { ops = 1, feedback = "read succeeded" } })
		end
	end
	test.eq(command.next.report.status, "stopped")
end)

test.case("native submissions accept truthy non-string null sentinel errors", function()
	hooks.clear()
	-- Native JSON null is truthy userdata. A Luau userdata exercises the same type guard.
	local null_like = if type(newproxy) == "function" then newproxy() else {}
	local worker = loop.init({ task_message = "test", max_turns = 2 })
	worker.phase = { kind = "run", calls = { { id = "one" }, { id = "two" } }, index = 1 }
	local outcome
	worker, outcome = loop.advance(worker, { kind = "run_result", result = { error = null_like, ops = 0, feedback = "ok", submission = { id = "native", payload = complete("done") } } }, { submissions = true, recheck = false })
	test.eq(outcome.kind, "submission")
	test.eq(outcome.submission.id, "native")
	test.eq(worker.messages[#worker.messages].id, "two")
	test.ok(worker.messages[#worker.messages].content:find("not executed", 1, true))
end)

test.case("RPI explicitly suppresses optional recheck system instructions", function()
	local recheck = require("std/recheck")
	local prompt = require("std/prompt")
	local previous = recheck.enabled
	recheck.enabled = true
	local ordinary = prompt.system()
	local rpi_system = prompt.system({ recheck = false })
	recheck.enabled = previous
	test.ok(ordinary:find("# Finish review", 1, true))
	test.ok(not rpi_system:find("# Finish review", 1, true))
end)
