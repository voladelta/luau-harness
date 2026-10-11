-- Opt-in pack (--pack std/compact): keeps a long run inside the model's context window.
-- When the last reply's context reaches the threshold (see M.threshold), the next model call is preceded by one summarizer
-- call (through hx.llm: logged, budgeted, replayable). Older messages are replaced by a structured summary; the task message
-- and the most recent messages stay verbatim; the Luau session (globals) is untouched. A context-overflow error from the model
-- triggers one compaction and a retry of the same turn (the loop's model_error event).
-- Below the threshold the pack adds nothing to the requests. It never writes files.
-- Config (set in .harness/init.luau, which runs before packs install):
--   require("std/compact").keep_recent_tokens = 8000
-- The window comes from --context-window, else M.context_window.
local M = {}

M.context_window = 128000 -- used only when --context-window is not given
M.reserve_tokens = 16384 -- headroom kept free for the next reply and the summarizer (prime-agent/pi default)
M.keep_recent_tokens = 20000 -- recent history kept verbatim (clamped to a quarter of the threshold)

local MAX_FAILURES = 2
local TRUNCATE_ABOVE, TRUNCATE_HEAD, TRUNCATE_TAIL = 2000, 1500, 500
local IMAGE_TOKENS = 1200

local SUMMARIZER_SYSTEM = [==[You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.]==]

local SUMMARY_PROMPT = [==[The conversation above is a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Dead Ends
- [Approaches tried and abandoned, with the reason, so they are not retried]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages.]==]

local UPDATE_PROMPT = [==[The conversation above contains NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Dead Ends
- [Approaches tried and abandoned, with the reason] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages.]==]

local VM_NOTE = "Note: the Luau session keeps running after this summary: every global variable and helper function defined so far stays available (vars() lists them). The calls that defined them won't appear above, so record in the summary any names worth remembering so they are reused instead of redefined."

local SUMMARY_PREFIX = "[compaction-summary]\n\nThe conversation history before this point was compacted into the following summary.\nThe retained messages below are authoritative; this summary may lag behind them.\nFiles on disk are current; the file listing in the task above may be out of date.\n\n<summary>\n"
local SUMMARY_SUFFIX = "\n</summary>"

-- lowercase substrings of the error text that mean "the request did not fit the context window"
local OVERFLOW = {
	"context_length_exceeded",
	"maximum context length",
	"context length",
	"context window",
	"prompt is too long",
	"input is too long",
	"too many tokens",
	"exceeds the context",
	"reduce the length",
}

-- Context size at which to compact: 95% of the window, but always leaving room for the reply plus the reserve.
-- <= 0 means the window is too small for compaction (the pack stays off).
function M.threshold(window: number, max_output: number, reserve: number): number
	return math.min(math.floor(window * 0.95), window - max_output - math.max(reserve, 4096))
end

-- Rough size of one message in tokens (chars/4). An assistant message's `raw` (the provider-native message the loop echoes
-- back) is counted instead of its text and calls when present, because it also carries the reasoning blocks.
function M.estimate(msg): number
	local chars = 0
	if type(msg.raw) == "string" and msg.raw ~= "" then
		chars = #msg.raw
	else
		chars = #(msg.content or "")
		for _, c in msg.calls or {} do
			local code = if type(c.input) == "table" then c.input.code else nil
			chars += #(if type(code) == "string" then code else tostring(c.error or ""))
		end
	end
	return math.ceil(chars / 4) + #(msg.images or {}) * IMAGE_TOKENS
end

-- Factor that turns estimates into real tokens: the provider's count for the last request over the estimate of the whole
-- request (system prompt + messages). Clamped to [0.5, 4]; 1 when there is no usage yet.
function M.scale(last_tokens: number, system: string, messages): number
	local est = math.ceil(#(system or "") / 4)
	for _, m in messages do
		est += M.estimate(m)
	end
	if last_tokens <= 0 or est <= 0 then
		return 1
	end
	return math.clamp(last_tokens / est, 0.5, 4)
end

-- Index of the first message to keep, or nil when there is nothing to summarize. Walks back from the end until the kept
-- messages add up to `keep_tokens` (estimates times `scale`, see M.scale), then moves forward to an assistant message: a
-- tool result is never cut away from the call it answers (results follow their assistant message). messages[1 .. start-1]
-- are never summarized.
function M.find_cut(messages, start: number, keep_tokens: number, scale: number?): number?
	local k = scale or 1
	local sum, i = 0, #messages
	while i >= start do
		sum += M.estimate(messages[i]) * k
		if sum >= keep_tokens then
			break
		end
		i -= 1
	end
	if i < start then
		return nil -- everything after `start` fits in the kept budget
	end
	while i <= #messages and messages[i].role ~= "assistant" do
		i += 1
	end
	if i > #messages or i <= start then
		return nil
	end
	return i
end

local function is_continuation(s: string, i: number): boolean
	local b = string.byte(s, i)
	return b ~= nil and b >= 0x80 and b < 0xC0
end

local function truncate(s: string): string
	if #s <= TRUNCATE_ABOVE then
		return s
	end
	local head_end, tail_start = TRUNCATE_HEAD, #s - TRUNCATE_TAIL + 1
	while head_end > 0 and is_continuation(s, head_end + 1) do
		head_end -= 1 -- never split a UTF-8 character
	end
	while is_continuation(s, tail_start) do
		tail_start += 1
	end
	return s:sub(1, head_end) .. "\n…[truncated]…\n" .. s:sub(tail_start)
end

-- messages[from .. to] as plain text for the summarizer. Tool results are numbered like the calls they answer.
function M.serialize(messages, from: number, to: number): string
	local numbers, n = {}, 0
	for i = from, to do
		for _, c in messages[i].calls or {} do
			n += 1
			numbers[c.id] = n
		end
	end
	local parts = {}
	for i = from, to do
		local m = messages[i]
		if m.role == "user" then
			local text = truncate(m.content or "")
			for _ = 1, #(m.images or {}) do
				text ..= "\n[image]"
			end
			table.insert(parts, "[User]: " .. text)
		elseif m.role == "assistant" then
			if (m.content or "") ~= "" then
				table.insert(parts, "[Assistant]: " .. truncate(m.content))
			end
			if m.calls and #m.calls > 0 then
				local lines = {}
				for _, c in m.calls do
					local code = if type(c.input) == "table" and type(c.input.code) == "string" then c.input.code else "(invalid arguments) " .. tostring(c.error or "")
					table.insert(lines, "#" .. tostring(numbers[c.id]) .. " " .. tostring(c.name) .. "(code=" .. truncate(code) .. ")")
				end
				table.insert(parts, "[Assistant tool calls]: " .. table.concat(lines, "\n"))
			end
		elseif m.role == "tool" then
			local num = numbers[m.id]
			table.insert(parts, "[Tool result" .. (if num then " #" .. num else "") .. "]: " .. truncate(m.content or ""))
		end
	end
	return table.concat(parts, "\n\n")
end

local function limits()
	local info = if hx.model_info then hx.model_info() else {}
	return info.context_window or M.context_window, info.max_output or 16384
end

function M.install(ctx)
	local task_text -- the first user message (task + file listing), re-sent verbatim after every compaction
	local prev -- the previous summary, once a compaction happened
	local last_tokens = 0 -- context size reported by the last reply
	local system = "" -- the system prompt of the last request (for M.scale)
	local failures, disabled = 0, false

	-- Replace messages[2 .. cut-1] by a summary, in place. Returns true when the messages were replaced.
	local function compact(messages): boolean
		local window, max_output = limits()
		task_text = task_text or messages[1].content
		-- keep a quarter of the threshold at most, so a compaction always frees most of the room up to the next one
		local limit = M.threshold(window, max_output, M.reserve_tokens)
		local keep = math.min(M.keep_recent_tokens, math.floor((if limit > 0 then limit else window) / 4))
		local cut = M.find_cut(messages, 2, keep, M.scale(last_tokens, system, messages))
		if not cut then
			hx.log("### COMPACT SKIPPED\nnothing old enough to summarize (" .. #messages .. " messages)")
			return false
		end
		local body = "<conversation>\n" .. M.serialize(messages, 2, cut - 1) .. "\n</conversation>\n\n"
		if prev then
			body ..= "<previous-summary>\n" .. prev .. "\n</previous-summary>\n\n" .. UPDATE_PROMPT
		else
			body ..= SUMMARY_PROMPT
		end
		body ..= "\n\n" .. VM_NOTE
		local ok, resp = pcall(hx.llm, {
			system = SUMMARIZER_SYSTEM,
			messages = { { role = "user", content = body } },
			max_tokens = math.floor(M.reserve_tokens * 4 / 5),
		})
		local summary = if ok then (resp.text or ""):match("^%s*(.-)%s*$") else ""
		if not ok or summary == "" then
			failures += 1
			disabled = failures >= MAX_FAILURES
			hx.log("### COMPACT FAILED\n" .. (if ok then "the summarizer returned no text" else tostring(resp)) .. "\nfailure " .. failures .. " of " .. MAX_FAILURES .. (if disabled then "; compaction is off for the rest of the run" else ""))
			return false
		end
		prev = summary
		local kept = table.move(messages, cut, #messages, 1, {})
		table.clear(messages)
		table.insert(messages, { role = "user", content = task_text .. "\n\n" .. SUMMARY_PREFIX .. summary .. SUMMARY_SUFFIX })
		table.move(kept, 1, #kept, 2, messages)
		hx.log("### COMPACT\nsummarized " .. (cut - 2) .. " messages, kept " .. #kept .. ", context was " .. last_tokens .. " tokens, summary " .. #summary .. " chars")
		last_tokens = 0 -- the next reply reports the new size
		return true
	end

	ctx.hooks.on("after_model", function(payload)
		local u = payload.usage
		if u then
			last_tokens = (u.input or 0) + (u.cache_read or 0) + (u.cache_write or 0) + (u.output or 0)
		end
		return payload
	end)

	ctx.hooks.on("before_model", function(req)
		system = req.system or ""
		if not disabled and last_tokens > 0 then
			local window, max_output = limits()
			local limit = M.threshold(window, max_output, M.reserve_tokens)
			if limit > 0 and last_tokens >= limit then
				compact(req.messages)
			end
		end
		return req
	end)

	ctx.hooks.on("model_error", function(ev)
		if disabled then
			return ev
		end
		local err = string.lower(ev.error or "")
		for _, pattern in OVERFLOW do
			if err:find(pattern, 1, true) then
				if compact(ev.messages) then
					return { retry = true }
				end
				break
			end
		end
		return ev
	end)
end

return M
