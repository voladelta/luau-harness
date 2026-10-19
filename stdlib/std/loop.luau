-- Pure controller: all durable workflow state is passed in and returned. Rust dispatches commands.
-- `s.phase` is a tagged table; each kind carries only the data that is valid in it:
--   { kind = "ready", compact = "auto" | "skip" | "force", overflow = error? }  overflow only with "force"
--   { kind = "model" }                       waiting for the agent's reply
--   { kind = "compact", cut = n, overflow = error? }  waiting for a summary of messages before `cut`
--   { kind = "run", calls = {...}, index = n }  working through one reply's tool calls
--   { kind = "finished" }
local hooks = require("std/hooks")
local prompt = require("std/prompt")
local compact = require("std/compact")
local recheck = require("std/recheck")
local M = {}

local function copy_data(value)
	if type(value) ~= "table" then return value end
	local result = {}
	for key, item in value do result[key] = copy_data(item) end
	return result
end

local function code_of(call)
	return if type(call.input) == "table" and type(call.input.code) == "string" then call.input.code else nil
end

local function ready(s, compact, overflow)
	for _, message in s.notifications or {} do table.insert(s.messages, message) end
	s.notifications = nil
	s.phase = { kind = "ready", compact = compact or "auto", overflow = overflow }
	return s, { kind = "next" }
end

local function finish(s)
	s.phase = { kind = "finished" }
	return s, { kind = "finish", report = s.report }
end

function M.init(cfg)
	return {
		phase = { kind = "ready", compact = "auto" }, max_turns = cfg.max_turns, task_text = cfg.task_message, messages = { { role = "user", content = cfg.task_message } },
		report = { turns = 0, code_turns = 0, luau_errors = 0, truncated_turns = 0, ops_per_turn = {}, finished = false, final_answer = "" },
		bad = 0, no_call = 0, recheck_asked = false, last_tokens = 0, compact_failures = 0,
		retried = false, hook_state = {},
	}
end

local function save_hook_state(s)
	hooks.state = s.hook_state
end

local function next_call(s)
	local cur = s.phase
	local c = cur.calls[cur.index]
	if not c then return ready(s) end
	local code = code_of(c)
	local error_text
	if c.name ~= "luau" then
		error_text = "error: the only tool is luau(code)."
	elseif not code then
		s.report.truncated_turns += 1
		error_text = prompt.cut_call
	else
		local pre = hooks.emit("before_run", { code = code })
		if pre.deny then error_text = "Blocked by a project hook: " .. tostring(pre.deny)
		else
			return s, { kind = "run", code = pre.code }
		end
	end
	table.insert(s.report.ops_per_turn, 0)
	table.insert(s.messages, { role = "tool", id = c.id, content = error_text })
	cur.index += 1
	return s, { kind = "next_call" }
end

local function step(s, event, options)
	options = options or {}
	save_hook_state(s)
	if event.kind == "recover" then
		-- Close every pending tool call with receipts, never dispatch its old code again.
		local phase = s.phase
		if phase.kind == "run" then
			for i = phase.index, #phase.calls do
				table.insert(s.messages, { role = "tool", id = phase.calls[i].id,
					content = "Interrupted by a crash. This chunk was NOT rerun. Consult the recovery receipts in the next user message." })
			end
		end
		if phase.kind == "finished" then return finish(s) end
		s.retried = false
		s.last_tokens = 0
		table.insert(s.messages, { role = "user", content = event.message })
		return ready(s, "skip")
	elseif event.kind == "run_result" then
		assert(s.phase.kind == "run", "unexpected chunk result")
		local native_submission = if options.submissions and type(event.result.error) ~= "string" and type(event.result.submission) == "table" then copy_data(event.result.submission) else nil
		local res = hooks.emit("after_run", event.result)
		s.report.code_turns += 1
		if type(res.error) == "string" then s.report.luau_errors += 1 end
		s.notifications = s.notifications or {}
		for _, message in event.notifications or {} do table.insert(s.notifications, message) end
		table.insert(s.report.ops_per_turn, res.ops)
		table.insert(s.messages, { role = "tool", id = s.phase.calls[s.phase.index].id, content = res.feedback })
		s.phase.index += 1
		if native_submission then
			-- Native control data is captured before hooks; never execute trailing calls.
			-- Close their protocol slots so rejected phase data can continue this transcript.
			for i = s.phase.index, #s.phase.calls do
				table.insert(s.messages, { role = "tool", id = s.phase.calls[i].id,
					content = "Skipped: stage.submit ended this reply; this call was not executed." })
			end
			s.phase = { kind = "submitted" }
			return s, { kind = "submission", submission = native_submission }
		end
		return next_call(s)
	elseif event.kind == "model_error" then
		if event.error:find("budget_exceeded", 1, true) then
			s.report.stopped = event.error
			return finish(s)
		end
		if s.phase.kind == "compact" then
			if s.phase.overflow then error("compaction failed after context overflow: " .. s.phase.overflow, 0) end
			s.compact_failures += 1
			return ready(s, "skip")
		end
		local recovery = hooks.emit("model_error", { error = event.error, messages = s.messages })
		if recovery.retry and not s.retried then
			s.retried = true
			return ready(s)
		end
		if event.safe_context_rejection and compact.enabled and not s.retried and compact.overflow(event.error) then
			s.retried = true
			return ready(s, "force", event.error)
		end
		error(if s.retried then "the model call failed again after one compact-and-retry: " .. event.error else event.error, 0)
	elseif event.kind == "model_reply" then
		if s.phase.kind == "compact" then
			local text = event.reply.text or ""
			local archive_cut = s.phase.cut
			local archive_enabled = hx.model_info().history == true
			if text:match("^%s*$") then
				if s.phase.overflow then error("compaction returned no summary after: " .. s.phase.overflow, 0) end
				s.compact_failures += 1
			else
				local kept = table.move(s.messages, s.phase.cut, #s.messages, 1, {})
				s.previous_summary = text
				s.messages = { { role = "user", content = compact.wrap(s.task_text, text, archive_enabled) } }
				table.move(kept, 1, #kept, 2, s.messages)
				s.last_tokens = 0
			end
			local next_state, command = ready(s, "skip")
			if archive_enabled and not text:match("^%s*$") then command.archive = {kind="compaction", cut=archive_cut} end
			return next_state, command
		end
		assert(s.phase.kind == "model", "unexpected model reply")
		local resp = event.reply
		s.report.turns += 1
		s.last_tokens = (resp.input or 0) + (resp.cache_read or 0) + (resp.cache_write or 0) + (resp.output or 0)
		local reply = hooks.emit("after_model", { text = resp.text or "", finish = resp.finish, usage = {
			input = resp.input, output = resp.output, cache_read = resp.cache_read, cache_write = resp.cache_write } })
		local text, calls = reply.text, resp.calls or {}
		if #calls == 0 then
			if text:match("^%s*$") or resp.finish == "length" then
				if options.submissions then
					s.report.truncated_turns += 1
					table.insert(s.messages, { role = "assistant", content = text:sub(1, 1500) })
					s.phase = { kind = "candidate" }
					return s, { kind = "candidate", text = text }
				end
				s.bad += 1
				s.report.truncated_turns += 1
				if s.bad > 3 then s.report.stopped = "protocol: repeated empty or truncated replies"; return finish(s) end
				table.insert(s.messages, { role = "assistant", content = text:sub(1, 1500) })
				table.insert(s.messages, { role = "user", content = prompt.nudge_cut })
				return ready(s)
			end
			if not options.submissions and s.report.code_turns == 0 and s.no_call < 2 then
				s.no_call += 1
				s.report.truncated_turns += 1
				table.insert(s.messages, { role = "assistant", content = text, raw = resp.raw })
				table.insert(s.messages, { role = "user", content = prompt.nudge_no_call })
				return ready(s)
			end
			local fin = hooks.emit("before_finish", { action = "accept", text = text, turn = s.report.turns })
			local follow = if fin.action == "continue" then fin.message else nil
			if not follow and options.recheck ~= false and recheck.enabled and not s.recheck_asked then
				s.recheck_asked = true
				follow = recheck.message
			end
			if follow then
				table.insert(s.messages, { role = "assistant", content = text, raw = resp.raw })
				table.insert(s.messages, { role = "user", content = follow })
				return ready(s)
			end
			if options.submissions then
				table.insert(s.messages, { role = "assistant", content = text, raw = resp.raw })
				s.phase = { kind = "candidate" }
				return s, { kind = "candidate", text = text }
			end
			s.report.finished = true
			s.report.final_answer = text
			return finish(s)
		end
		s.bad = 0
		s.retried = false
		table.insert(s.messages, { role = "assistant", content = text, calls = calls, raw = resp.raw })
		s.phase = { kind = "run", calls = calls, index = 1 }
		return next_call(s)
	elseif event.kind == "next_call" then
		return next_call(s)
	end

	if s.phase.kind == "finished" then return finish(s) end
	local phase = s.phase
	assert(phase.kind == "ready", "unexpected controller event")
	if s.report.turns >= s.max_turns then
		s.report.stopped = "max_turns: " .. s.max_turns
		return finish(s)
	end
	local req = hooks.emit("before_model", { system = prompt.system(options), messages = s.messages, tools = prompt.tools })
	local info = hx.model_info()
	local window = if type(info.context_window) == "number" then info.context_window else compact.context_window
	local max_output = info.max_output or 16384
	if compact.enabled and s.compact_failures < 2 and phase.compact ~= "skip" and (phase.compact == "force" or s.last_tokens >= compact.trigger(window, max_output)) then
		local limit = compact.trigger(window, max_output)
		local cut = compact.find_cut(s.messages, 2, math.min(compact.keep_recent_tokens, math.floor((if limit > 0 then limit else window) / 4)), compact.scale(s.last_tokens, req.system, s.messages))
		if cut then
			s.phase = { kind = "compact", cut = cut, overflow = phase.overflow }
			return s, { kind = "model", request = compact.request(s.messages, cut, s.previous_summary), purpose = "compact" }
		end
		if phase.overflow then error(phase.overflow, 0) end
	end
	s.phase = { kind = "model" }
	return s, { kind = "model", request = req, purpose = "agent" }
end

-- Explicit worker protocol. Stopped states cannot be resumed through continue.
function M.advance(s, event, options)
	local state, outcome = step(s, event, options)
	if outcome.kind == "candidate" or outcome.kind == "submission" then return state, outcome end
	if outcome.kind == "finish" then
		return state, { kind = "stopped", reason = state.report.stopped or "worker stopped", report = state.report }
	end
	return state, { kind = "command", command = outcome }
end

function M.continue(s, message)
	assert(s.phase.kind == "candidate" or s.phase.kind == "submitted", "cannot continue a stopped or active worker")
	table.insert(s.messages, { role = "user", content = message })
	return ready(s)
end

function M.step(s, event)
	return step(s, event, {})
end

return M
