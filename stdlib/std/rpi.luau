-- Pure RPI policy; the host owns submission provenance, artifacts, receipts and gates.
local loop = require("std/loop")
local M = {}
-- Rejected submissions a stage may have before it stops (a protocol limit, separate from the repair cycles the
-- host freezes in `cfg.workflow.max_repairs`).
local MAX_CORRECTIONS = 3
local route = { "research_questions", "research", "system_design", "program_design", "implementation", "verification" }
local artifact_names = {
	research_questions = "02-research-questions.md", research = "03-research.md",
	system_design = "04-system-design.md", program_design = "05-program-design.md",
	implementation = "06-implementation.md", verification = "07-verification.md", repair = "06-repair.md",
}
local position = {}
for i, name in route do position[name] = i end
local inputs = {
	research_questions = { "request" }, research = { "research_questions" },
	system_design = { "request", "research" },
	program_design = { "request", "research", "system_design" },
	implementation = { "request", "research", "system_design", "program_design" },
	verification = { "request", "system_design", "program_design", "implementation", "repair" },
	repair = { "request", "research", "system_design", "program_design", "implementation", "verification" },
}
local function report(s, status, reason)
	return { finished = status == "completed", final_answer = reason or "", status = status,
		workflow = "rpi", stage = s.stage and s.stage.name, turns = s.turns, repairs = s.repairs,
		artifacts = s.artifacts, waived_checks = s.waived_checks, stopped = if status == "stopped" then reason else nil }
end
local function boundary(s, next, submission)
	s.serial += 1
	s.phase = "boundary"
	return s, { kind = "boundary", id = "rpi-boundary-" .. s.serial, submission = submission, next = next }
end
local function finish(s, status, reason, submission)
	return boundary(s, { kind = "finish", report = report(s, status, reason) }, submission)
end
local function instruction(name)
	return hx.asset("std/rpi/common.md") .. "\n\n" .. hx.asset("std/rpi/" .. name .. ".md")
end
-- `revisit`: the stage replaces its own earlier document (owner answer or revision, manual results, a redesign), so it
-- receives that document too.
local function start(s, name, submission, revisit)
	s.attempts[name] = (s.attempts[name] or 0) + 1
	local refs = {}
	for _, input in inputs[name] do
		if s.artifacts[input] then table.insert(refs, s.artifacts[input]) end
	end
	if revisit and s.artifacts[name] then table.insert(refs, s.artifacts[name]) end
	-- A redesign detour carries the repair record and the failed verification it answers until verification runs again.
	if name == "verification" then
		s.redesign = nil
	elseif s.redesign then
		for _, id in s.redesign do table.insert(refs, id) end
	end
	s.stage = { id = name .. "-" .. s.attempts[name], name = name,
		access = if name == "implementation" or name == "verification" or name == "repair" then "workspace" else "inspect",
		inputs = refs, verify = name == "verification", plans_checks = name == "program_design", artifact_name = artifact_names[name] }
	s.corrections = 0
	return boundary(s, { kind = "stage", stage = s.stage }, submission)
end
local function gate(s, kind, questions, check_ids, continuation, submission)
	s.pending = { gate_id = "rpi-gate-" .. (s.serial + 1), kind = kind,
		artifacts = { s.artifacts[s.stage.name] },
		questions = if questions and #questions > 0 then questions else nil,
		check_ids = if check_ids and #check_ids > 0 then check_ids else nil,
		decisions = if kind == "input" then (if #(check_ids or {}) > 0 then { "answer", "waive" } else { "answer" }) else { "approve", "revise" },
		continuation = continuation }
	return boundary(s, { kind = "review", request = s.pending }, submission)
end
local function next_stage(s, submission)
	local name = s.stage.name
	if name == "repair" then return start(s, "verification", submission) end
	local i = position[name]
	return start(s, route[i + 1], submission)
end
local function repair(s, submission)
	if s.repairs >= s.max_repairs then return finish(s, "stopped", "Repair limit exhausted; verification still has findings.", submission) end
	s.repairs += 1
	s.waived_checks = {}
	return start(s, "repair", submission)
end
-- The host's stage.submit already refused a payload without a valid outcome, nonempty summary, evidence array, owner
-- questions (when not complete) or well-formed checks (workflow.rs validate_submission), and added the verification
-- stage's check results. What remains is which fields each RPI stage requires, which only this workflow knows.
local function valid(payload, name)
	if payload.outcome ~= "complete" then return nil end
	if name == "program_design" then
		if type(payload.checks) ~= "table" then return "Program design requires a structured checks array." end
	elseif name == "verification" then
		if type(payload.findings) ~= "table" then return "Verification requires a findings array (empty only when clean)." end
	elseif name == "repair" and type(payload.redesign_stage) == "string" and payload.redesign_stage ~= "system_design" and payload.redesign_stage ~= "program_design" then
		return "redesign_stage must be system_design or program_design."
	end
	return nil
end
-- The worker transcript lives once, in `s.messages` (which the host and history read); `s.worker.messages` is set only
-- while std/loop runs.
local function with_worker(s, run)
	s.worker.messages = s.messages
	local result = table.pack(run(s.worker))
	s.worker = result[1]
	s.messages = s.worker.messages
	s.worker.messages = nil
	return table.unpack(result, 2, result.n)
end
local function correct(s, message)
	s.corrections += 1
	if s.corrections >= MAX_CORRECTIONS then return finish(s, "stopped", "Stage submission protocol failed: " .. message) end
	local command = with_worker(s, function(worker)
		return loop.continue(worker, "RPI stage is not complete. " .. message .. " Call stage.submit with the full document, summary, evidence array and required stage fields in the luau tool; plain text cannot finish this stage.")
	end)
	return s, command
end
function M.init(cfg)
	assert(cfg.workflow and cfg.workflow.name == "rpi", "RPI requires explicit workflow selection")
	return { phase = "initial", cfg = cfg, mode = cfg.workflow.mode, max_repairs = assert(cfg.workflow.max_repairs, "workflow config lacks max_repairs"),
		turns = 0, repairs = 0, serial = 0, attempts = {}, artifacts = { request = "request" }, waived_checks = {}, corrections = 0, messages = {} }
end
local function describe(value, depth)
	if type(value) ~= "table" then return tostring(value) end
	if (depth or 0) > 12 then return "[metadata depth limit]" end
	local keys, parts = {}, {}
	for key in value do table.insert(keys, key) end
	table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
	for _, key in keys do table.insert(parts, tostring(key) .. ": " .. describe(value[key], (depth or 0) + 1)) end
	return "{ " .. table.concat(parts, ", ") .. " }"
end
local function strings(value)
	return if type(value) == "table" then value else {}
end
-- The findings of a failed verification artifact, or nil when it passed.
local function failed_verification(s, artifact)
	local sub = artifact.submission
	if artifact.name ~= "verification" or type(sub) ~= "table" or type(sub.verification) ~= "table" then return nil end
	local host_findings, agent_findings = strings(sub.verification.findings), strings(sub.findings)
	if sub.verification.passed == true and #host_findings == 0 and #agent_findings == 0 then return nil end
	local lines = { "<host-verification-findings>\nThe latest verification stage was REJECTED by the harness." }
	if #host_findings > 0 then
		table.insert(lines, "Host verification findings:")
		for _, f in host_findings do table.insert(lines, "- " .. tostring(f)) end
	end
	if #agent_findings > 0 then
		table.insert(lines, "Agent self-reported findings:")
		for _, f in agent_findings do table.insert(lines, "- " .. tostring(f)) end
	end
	if s.stage.name == "repair" then
		table.insert(lines, "\nINSTRUCTIONS FOR REPAIR:\nAddress the specific verification failures listed above.\nIf automated tests passed (exit 0) but check receipts or working tree integrity failed (e.g. stale receipts or unexpected file modifications), fix the command sequence or workspace cleanliness; do not speculate on unobserved code bugs.")
	else
		table.insert(lines, "\nA repair returned the work to this stage because the design must change; the repair record explains why.")
	end
	table.insert(lines, "</host-verification-findings>")
	return table.concat(lines, "\n")
end
local function packet(s, event)
	local parts = { instruction(s.stage.name) }
	local selected = {}
	for _, id in s.stage.inputs do selected[id] = true end
	local documents = {}
	for _, artifact in event.artifacts or {} do
		if selected[artifact.id] then
			local findings = failed_verification(s, artifact)
			if findings then table.insert(parts, findings) end
			if artifact.name == s.stage.name then
				local lines = { "<previous-version name=" .. tostring(artifact.name) .. " id=" .. artifact.id .. ">",
					"This stage's earlier document, which this run replaces. Owner input below says what changed.", artifact.document }
				local questions = if type(artifact.submission) == "table" then strings(artifact.submission.questions) else {}
				if #questions > 0 then
					table.insert(lines, "Questions it asked the owner:")
					for _, q in questions do table.insert(lines, "- " .. tostring(q)) end
				end
				table.insert(lines, "</previous-version>")
				table.insert(documents, table.concat(lines, "\n"))
			else
				table.insert(documents, "<artifact name=" .. tostring(artifact.name) .. " id=" .. artifact.id .. ">\n" .. artifact.document .. "\n</artifact>")
			end
		end
	end
	for _, document in documents do table.insert(parts, document) end
	table.insert(parts, "<owner-inputs>\nLatest explicit owner feedback takes precedence over older design assumptions.")
	for _, input in event.owner_inputs or {} do table.insert(parts, tostring(input.decision) .. ": " .. tostring(input.text or "")) end
	table.insert(parts, "</owner-inputs>")
	if type(event.metadata) == "table" then table.insert(parts, "<workspace-metadata>\n" .. describe(event.metadata) .. "\n</workspace-metadata>") end
	return table.concat(parts, "\n\n")
end
function M.step(s, event)
	if s.phase == "initial" then return start(s, "research_questions") end
	if event.kind == "boundary_ready" then
		s.stage = event.stage
		s.worker = loop.init({ max_turns = s.cfg.max_turns - s.turns, task_message = packet(s, event) })
		s.messages = s.worker.messages
		s.worker.messages = nil
		s.phase = "worker"
		return s, { kind = "next" }
	elseif event.kind == "owner_decision" then
		local request = event.request
		assert(s.pending and request.gate_id == s.pending.gate_id, "unexpected owner decision")
		local continuation = request.continuation
		s.pending = nil
		if event.decision == "revise" then
			s.waived_checks = {}
			-- Only verification has an acceptance gate. An owner revision is not a failed verification, so its repair
			-- spends no repair cycle.
			if s.stage.name == "verification" then return start(s, "repair") end
			local p = position[s.stage.name]
			for i = p + 1, #route do s.artifacts[route[i]] = nil end
			s.artifacts.repair = nil
			return start(s, s.stage.name, nil, true)
		elseif continuation == "finish" then
			return finish(s, "completed", "Implementation verified and accepted.")
		elseif continuation == "manual_results" then
			if event.decision == "waive" then
				for _, id in request.check_ids do s.waived_checks[id] = true end
			end
			return start(s, "verification", nil, true)
		elseif continuation == "same_stage" then
			return start(s, s.stage.name, nil, true)
		else
			return next_stage(s)
		end
	end
	assert(s.phase == "worker", "worker event received outside active stage")
	-- Host additions (including images) update this same worker transcript.
	local before = s.worker.report.turns
	local outcome = with_worker(s, function(worker)
		return loop.advance(worker, event, { submissions = true, recheck = false })
	end)
	s.turns += s.worker.report.turns - before
	if outcome.kind == "command" then
		local result = if event.kind == "run_result" then event.result else nil
		-- The host sets submission_error only when stage.submit refused a payload in this chunk.
		if result and result.submission_error == true then
			s.corrections += 1
			if s.corrections >= MAX_CORRECTIONS then
				return finish(s, "stopped", "Stage submission protocol failed: " .. (if type(result.error) == "string" then result.error else "stage.submit refused the submission"))
			end
		end
		return s, outcome.command
	end
	if outcome.kind == "stopped" then return finish(s, "stopped", outcome.reason) end
	if outcome.kind == "candidate" then return correct(s, "A structured stage.submit result is required.") end
	assert(outcome.kind == "submission", "unexpected worker outcome")
	local submission = outcome.submission
	local payload = submission.payload
	local error_text = valid(payload, s.stage.name)
	if error_text then return correct(s, error_text) end
	s.artifacts[s.stage.name] = submission.id
	if payload.outcome ~= "complete" then return gate(s, "input", payload.questions, {}, "same_stage", submission.id) end
	if s.stage.name == "repair" and type(payload.redesign_stage) == "string" then
		local name = payload.redesign_stage
		-- The repair entry already consumed one cycle; returning to design cannot reset it.
		s.redesign = { s.artifacts.verification, submission.id }
		for i = position[name] + 1, #route do s.artifacts[route[i]] = nil end
		s.artifacts.repair = nil
		return start(s, name, submission.id, true)
	end
	if s.stage.name == "verification" then
		if payload.verification.passed ~= true or #payload.findings > 0 then return repair(s, submission.id) end
		local manual = {}
		for _, id in strings(payload.manual_check_ids) do
			if not s.waived_checks[id] then table.insert(manual, id) end
		end
		if #manual > 0 then return gate(s, "input", payload.questions, manual, "manual_results", submission.id) end
		if s.mode == "manual" then return gate(s, "acceptance", {}, {}, "finish", submission.id) end
		return finish(s, "completed", payload.summary, submission.id)
	end
	if s.mode == "manual" and (position[s.stage.name] or 99) <= 4 then
		return gate(s, "review", {}, {}, "next_stage", submission.id)
	end
	return next_stage(s, submission.id)
end
return M
