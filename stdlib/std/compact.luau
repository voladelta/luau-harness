-- Default pack (installed by std/loop; off with `require("std/compact").enabled = false` in .harness/init.luau): keeps a
-- long run inside the model's context window.
-- When the context reaches the trigger (see M.trigger, measured by M.measure), the next model call is preceded by one
-- summarizer call dispatched by the Rust supervisor. That call replays the agent's own request prefix (system prompt, tools,
-- the task message and the history to summarize) and appends one instruction, so the summarizer sees exact messages and
-- the provider can reuse its cached prefix. A validated summary (M.accept) replaces the older messages; the task message
-- and the most recent messages stay verbatim; the Luau session (globals) is untouched. A context-overflow error from the
-- model bounds the window for one compaction that keeps as little as fits under that bound, then a retry.
-- Below the trigger the pack adds nothing to the requests. It never writes files.
-- Config (set in .harness/init.luau, which runs before packs install):
--   require("std/compact").keep_recent_tokens = 8000
-- The window comes from --context-window, else M.context_window. Compaction starts at M.trigger (never above
-- M.max_trigger_tokens, so a 1M window does not mean resending ~1M tokens a turn).
local M = {}

M.enabled = true
M.context_window = 1000000 -- used only when --context-window is not given
M.max_trigger_tokens = 256000 -- compaction starts at this context size at the latest
M.reserve_tokens = 16384 -- headroom kept free for the next reply and the summarizer's output (prime-agent/pi default)
M.keep_recent_tokens = 64000 -- recent history kept verbatim (clamped to a quarter of the trigger)
M.max_passes = 2 -- compactions allowed between two agent replies

local IMAGE_TOKENS = 1200

local INSTRUCTION = [==[Stop working on the task for this one reply. Write a context checkpoint of the conversation above: another model will continue the work from it, the task message and the most recent messages, which stay verbatim. Do not call any tool. Output only the checkpoint, in this EXACT format:

## Goal
[What the user is trying to accomplish; can be multiple items if the task expanded.]

## Constraints & Preferences
- [Constraints, preferences or requirements the user stated, or "(none)"]

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
- [Data, examples or references needed to continue, or "(none)"]

Rules:
- Keep each section concise. Preserve exact file paths, function names, commands, error messages and numbers.
- Capture user instructions and corrections faithfully. This checkpoint request is not one of them: do not mention it.
- If the conversation contains a [compaction-summary] block, it is an earlier checkpoint: carry forward what is still true, drop what is stale, and merge everything into this one checkpoint.
- Luau scope for Critical Context: compaction keeps the running VM. Distinguish user globals assigned without local from chunk-local variables and local functions; do not list locals or builtins as live user state. Record useful globals and what they hold based on the code above, noting any uncertainty. For a needed past local, record how to retrieve or recompute its value instead of implying the name is available. The continuing agent can inspect vars(), a name-to-value map of live user globals.]==]

local SUMMARY_PREFIX = "[compaction-summary]\n\nThe conversation history before this point was compacted into the following summary.\nThe retained messages below are authoritative; this summary may lag behind them.\nFiles on disk are current; the file listing in the task above may be out of date.\n\n<summary>\n"
local SUMMARY_SUFFIX = "\n</summary>\nLuau scope is unchanged: only globals remain accessible across calls. Check vars() before reusing uncertain names; retrieve or recompute past locals."

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

local function estimate_all(system_chars: number, messages): number
	local n = math.ceil(system_chars / 4)
	for i = 1, #messages do
		n += M.estimate(messages[i])
	end
	return n
end

-- One measurement for the whole pack. An anchor says the provider counted `tokens` for the system prompt, tools and
-- messages[1 .. count]; later messages (tool results, notes) are estimates times `scale`, which turns chars/4 into the
-- provider's tokens.
-- M.anchor: taken at an agent reply from its usage; `messages` is the request it answered, `count` covers the reply too.
-- The scale is the prompt count over the estimate of the same request (`system_chars`: its system prompt as sent),
-- clamped to [0.5, 4]; 1 without usage.
function M.anchor(system_chars: number, messages, prompt_tokens: number, output_tokens: number)
	local est = estimate_all(system_chars, messages)
	local scale = if prompt_tokens > 0 and est > 0 then math.clamp(prompt_tokens / est, 0.5, 4) else 1
	return { tokens = prompt_tokens + output_tokens, count = #messages + 1, scale = scale }
end

-- An estimated anchor for a history without a provider count (after a compaction or a recovery), keeping the scale.
function M.reanchor(a, system_chars: number, messages)
	return { tokens = math.ceil(estimate_all(system_chars, messages) * a.scale), count = #messages, scale = a.scale }
end

-- Context size of the next request in provider tokens.
function M.measure(a, messages): number
	local n = a.tokens
	for i = a.count + 1, #messages do
		n += M.estimate(messages[i]) * a.scale
	end
	return n
end

-- Index of the first message to keep and the summarizer's prefix size, or nil when no cut works. Walks back from the end and returns the first assistant
-- message (a tool result is never cut away from its call) at which the kept messages reach `keep_tokens` and the
-- summarizer's prefix (`total` minus the kept messages) is at most `max_prefix`. keep_tokens = 0 keeps as little as fits.
-- messages[1] (the task) is never summarized, so the cut is at least 3.
function M.find_cut(messages, keep_tokens: number, scale: number, total: number, max_prefix: number): (number?, number?)
	local kept = 0
	for i = #messages, 3, -1 do
		kept += M.estimate(messages[i]) * scale
		if messages[i].role == "assistant" and kept >= keep_tokens and total - kept <= max_prefix then
			return i, total - kept
		end
	end
	return nil
end

-- The summarizer request: the agent's own request prefix (same system prompt and tools, messages[1 .. cut-1] unchanged)
-- plus the instruction. Pure; the supervisor performs the call and checkpoints its result.
function M.request(system: string, tools, messages, cut: number)
	local prefix = table.move(messages, 1, cut - 1, 1, {})
	table.insert(prefix, { role = "user", content = INSTRUCTION })
	-- replay: the supervisor sends it under the system prompt of the last agent request as sent (VM hooks applied)
	return { system = system, tools = tools, messages = prefix, max_tokens = math.floor(M.reserve_tokens * 4 / 5), replay = true }
end

function M.wrap(task, summary, history)
	local notice = if history then '\nMessages removed by recorded compactions are available in this root lane: history({q="literal text"}), then history({id="entry-id"}). Print entry.text; follow next (resend q) or next_offset for more. Un-compacted retired RPI stages and other lanes are outside this archive.\n' else ""
	return task .. "\n\n" .. SUMMARY_PREFIX .. summary .. SUMMARY_SUFFIX .. notice
end

-- A summarizer reply is a candidate: it replaces messages[1 .. cut-1] only if it is complete, text only, and smaller
-- than what it replaces. Returns the new first message, or nil and the reason.
function M.accept(reply, task: string, messages, cut: number, history: boolean): (string?, string?)
	local text = reply.text or ""
	if reply.finish == "length" then return nil, "truncated" end
	if #(reply.calls or {}) > 0 then return nil, "tool call" end
	if text:match("^%s*$") then return nil, "empty" end
	local first = M.wrap(task, text, history)
	local replaced = 0
	for i = 1, cut - 1 do
		replaced += M.estimate(messages[i])
	end
	if M.estimate({ content = first }) >= replaced then return nil, "not smaller" end
	return first, nil
end

function M.overflow(message)
	local lower = message:lower()
	for _, pattern in OVERFLOW do if lower:find(pattern, 1, true) then return true end end
	return false
end

return M
