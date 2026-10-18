-- Pure RPI policy; the host owns submission provenance, artifacts, receipts and gates.
local loop = require("std/loop")
local M = {}
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
local function start(s, name, submission)
	s.attempts[name] = (s.attempts[name] or 0) + 1
	local refs = {}
	for _, input in inputs[name] do
		if s.artifacts[input] then table.insert(refs, s.artifacts[input]) end
	end
	s.stage = { id = name .. "-" .. s.attempts[name], name = name,
		access = if name == "implementation" or name == "verification" or name == "repair" then "workspace" else "inspect",
		inputs = refs, instructions = instruction(name), verify = name == "verification", artifact_name = artifact_names[name] }
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
local function valid(payload, name)
	if type(payload) ~= "table" then return "Missing structured submission payload." end
	if payload.outcome ~= "complete" and payload.outcome ~= "needs_input" and payload.outcome ~= "blocked" then return "Invalid outcome." end
	if type(payload.summary) ~= "string" or payload.summary:match("^%s*$") then return "A nonempty summary is required." end
	if type(payload.evidence) ~= "table" then return "evidence must be an array, including when empty." end
	if payload.outcome ~= "complete" then
		if type(payload.questions) ~= "table" or #payload.questions == 0 then return "Provide concrete owner questions for blocked or needs_input." end
	elseif name == "program_design" then
		if type(payload.checks) ~= "table" then return "Program design requires a structured checks array." end
		local ids = {}
		for _, check in payload.checks do
			if type(check.id) ~= "string" or ids[check.id] or type(check.command) ~= "string" or type(check.requirements) ~= "table" then
				return "Every check needs a unique id, command and requirements array."
			end
			ids[check.id] = true
		end
	elseif name == "verification" then
		if type(payload.findings) ~= "table" then return "Verification requires a findings array (empty only when clean)." end
		if type(payload.verification) ~= "table" then return "Host verification evidence is missing." end
	elseif name == "repair" and type(payload.redesign_stage) == "string" and payload.redesign_stage ~= "system_design" and payload.redesign_stage ~= "program_design" then
		return "redesign_stage must be system_design or program_design."
	end
	return nil
end
local function correct(s, message)
	s.corrections += 1
	if s.corrections >= 3 then return finish(s, "stopped", "Stage submission protocol failed: " .. message) end
	local command
	s.worker, command = loop.continue(s.worker, "RPI stage is not complete. " .. message .. " Call stage.submit with the full document, summary, evidence array and required stage fields in the luau tool; plain text cannot finish this stage.")
	s.messages = s.worker.messages
	return s, command
end
function M.init(cfg)
	assert(cfg.workflow and cfg.workflow.name == "rpi", "RPI requires explicit workflow selection")
	return { phase = "initial", cfg = cfg, mode = cfg.workflow.mode, max_repairs = cfg.workflow.max_repairs or 3,
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
local function packet(s, event)
	local parts = { s.stage.instructions }
	if s.stage.name == "repair" then
		for _, artifact in event.artifacts or {} do
			if artifact.name == "verification" and type(artifact.submission) == "table" then
				local sub = artifact.submission
				local v = if type(sub.verification) == "table" then sub.verification else nil
				local host_findings = if v and type(v.findings) == "table" then v.findings else {}
				local agent_findings = if type(sub.findings) == "table" then sub.findings else {}
				if (v and v.passed == false) or #host_findings > 0 or #agent_findings > 0 then
					local lines = { "<host-verification-findings>\nThe previous verification stage was REJECTED by the harness." }
					if #host_findings > 0 then
						table.insert(lines, "Host verification findings:")
						for _, f in host_findings do table.insert(lines, "- " .. tostring(f)) end
					end
					if #agent_findings > 0 then
						table.insert(lines, "Agent self-reported findings:")
						for _, f in agent_findings do table.insert(lines, "- " .. tostring(f)) end
					end
					table.insert(lines, "\nINSTRUCTIONS FOR REPAIR:\nAddress the specific verification failures listed above.\nIf automated tests passed (exit 0) but check receipts or working tree integrity failed (e.g. stale receipts or unexpected file modifications), fix the command sequence or workspace cleanliness; do not speculate on unobserved code bugs.\n</host-verification-findings>")
					table.insert(parts, table.concat(lines, "\n"))
				end
			end
		end
	end
	local selected = {}
	for _, id in s.stage.inputs do selected[id] = true end
	for _, artifact in event.artifacts or {} do
		if selected[artifact.id] then
			table.insert(parts, "<artifact name=" .. tostring(artifact.name) .. " id=" .. artifact.id .. ">\n" .. artifact.document .. "\n</artifact>")
		end
	end
	table.insert(parts, "<owner-inputs>\nLatest explicit owner feedback takes precedence over older design assumptions.")
	for _, input in event.owner_inputs or {} do table.insert(parts, tostring(input.decision) .. ": " .. tostring(input.text or "")) end
	table.insert(parts, "</owner-inputs>")
	if type(event.metadata) == "table" then table.insert(parts, "<workspace-metadata>\n" .. describe(event.metadata) .. "\n</workspace-metadata>") end
	return table.concat(parts, "\n\n")
end
function M.step(s, event)
	if s.phase == "initial" then return start(s, "research_questions") end
	if event.kind == "boundary_ready" then
		for _, artifact in event.artifacts or {} do
			if artifact.name == "request" then s.artifacts.request = artifact.id end
		end
		if type(event.sealed) == "table" then
			-- Host may replace provisional submission refs with canonical artifact refs.
			for name, id in s.artifacts do
				if id == event.sealed.submission then s.artifacts[name] = event.sealed.id end
			end
		end
		if type(event.stage) ~= "table" then return s, { kind = "next" } end
		s.stage = event.stage
		-- First boundary is created before the host request artifact exists.
		if s.stage.name == "research_questions" and #s.stage.inputs == 0 and s.artifacts.request then s.stage.inputs = { s.artifacts.request } end
		s.worker = loop.init({ max_turns = s.cfg.max_turns - s.turns, task_message = packet(s, event) })
		s.messages = s.worker.messages
		s.phase = "worker"
		return s, { kind = "next" }
	elseif event.kind == "owner_decision" then
		local request = event.request or s.pending
		assert(request and request.gate_id == s.pending.gate_id, "unexpected owner decision")
		local continuation = request.continuation
		s.pending = nil
		if event.decision == "revise" then
			s.waived_checks = {}
			if s.stage.name == "verification" then return repair(s) end
			local p = position[s.stage.name]
			for i = p + 1, #route do s.artifacts[route[i]] = nil end
			s.artifacts.repair = nil
			return start(s, s.stage.name)
		elseif continuation == "finish" then
			if request.kind == "input" and s.mode == "manual" then return gate(s, "acceptance", {}, {}, "finish") end
			return finish(s, "completed", "Implementation verified and accepted.")
		elseif continuation == "manual_results" then
			if event.decision == "waive" then
				for _, id in request.check_ids do s.waived_checks[id] = true end
			end
			return start(s, "verification")
		elseif continuation == "same_stage" then
			return start(s, s.stage.name)
		else
			return next_stage(s)
		end
	end
	assert(s.phase == "worker", "worker event received outside active stage")
	-- Host additions (including images) update this same worker transcript.
	s.worker.messages = s.messages
	local before = s.worker.report.turns
	local outcome
	s.worker, outcome = loop.advance(s.worker, event, { submissions = true, recheck = false })
	s.turns += s.worker.report.turns - before
	s.messages = s.worker.messages
	if outcome.kind == "command" then
		local result = if event.kind == "run_result" then event.result else nil
		local error_text = if result and type(result.error) == "string" then result.error else ""
		if result and (result.submission_error == true or error_text:find("stage.submit", 1, true) or error_text:find("submission", 1, true)) then
			s.corrections += 1
			if s.corrections >= 3 then return finish(s, "stopped", "Stage submission protocol failed: " .. error_text) end
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
		for i = position[name], #route do s.artifacts[route[i]] = nil end
		s.artifacts.repair = nil
		return start(s, name, submission.id)
	end
	if s.stage.name == "verification" then
		local verification = payload.verification
		if verification.passed ~= true or #verification.findings > 0 or #payload.findings > 0 then return repair(s, submission.id) end
		local manual = {}
		for _, id in (if type(payload.manual_check_ids) == "table" then payload.manual_check_ids else {}) do
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
