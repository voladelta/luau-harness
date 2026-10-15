-- Default pack (installed by std/loop; off with `require("std/compact").enabled = false` in .harness/init.luau): keeps a
-- long run inside the model's context window.
-- When the last reply's context reaches the threshold (see M.trigger), the next model call is preceded by one summarizer
-- command dispatched by the Rust supervisor. Older messages are replaced by a structured summary; the task message
-- and the most recent messages stay verbatim; the Luau session (globals) is untouched. A context-overflow error from the model
-- triggers one compaction and a retry of the same turn (the loop's model_error event).
-- Below the threshold the pack adds nothing to the requests. It never writes files.
-- Config (set in .harness/init.luau, which runs before packs install):
--   require("std/compact").keep_recent_tokens = 8000
-- The window comes from --context-window, else M.context_window. Compaction starts at M.trigger (never above
-- M.max_trigger_tokens, so a 1M window does not mean resending ~1M tokens a turn); a model whose real window is smaller
-- than the assumed one hits an overflow error first, which compacts and retries once.
local M = {}

M.enabled = true
M.context_window = 1000000 -- used only when --context-window is not given
M.max_trigger_tokens = 256000 -- compaction starts at this context size at the latest
M.reserve_tokens = 16384 -- headroom kept free for the next reply and the summarizer (prime-agent/pi default)
M.keep_recent_tokens = 64000 -- recent history kept verbatim (clamped to a quarter of the trigger)

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

-- The context size that starts a compaction: M.threshold, capped at M.max_trigger_tokens. <= 0 means off.
function M.trigger(window: number, max_output: number): number
	local t = M.threshold(window, max_output, M.reserve_tokens)
	return if t > 0 then math.min(t, M.max_trigger_tokens) else t
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

-- Pure request construction. The supervisor performs the model call and checkpoints its result.
function M.request(messages, cut, previous)
	local body = "<conversation>\n" .. M.serialize(messages, 2, cut - 1) .. "\n</conversation>\n\n"
	if previous then body ..= "<previous-summary>\n" .. previous .. "\n</previous-summary>\n\n" .. UPDATE_PROMPT
	else body ..= SUMMARY_PROMPT end
	return { system = SUMMARIZER_SYSTEM, messages = { { role = "user", content = body .. "\n" .. VM_NOTE } },
		max_tokens = math.floor(M.reserve_tokens * 4 / 5) }
end

function M.wrap(task, summary) return task .. "\n\n" .. SUMMARY_PREFIX .. summary .. SUMMARY_SUFFIX end

function M.overflow(message)
	local lower = message:lower()
	for _, pattern in OVERFLOW do if lower:find(pattern, 1, true) then return true end end
	return false
end

return M
