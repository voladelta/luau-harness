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
--   require("std/compact").trigger_ratio = 0.5
-- The window comes from --context-window, else M.context_window; give a smaller model's window, or it overflows first.
-- The ratios are the DeepSeek harness defaults (compaction-basic), chosen by the DeepSeek team.
local M = {}

M.enabled = true
M.context_window = 1000000 -- used only when --context-window is not given
M.trigger_ratio = 0.8 -- compaction starts at this share of the window
M.keep_ratio = 0.16 -- recent history kept verbatim: at most this share of the window less the reply; the newest exchange always
M.reserve_tokens = 65536 -- most headroom kept free for the summarizer's output, reasoning included (DeepSeek harness default)

-- Image budget of one request, set below the common provider limits (about 100 images, 32 MB per request). Going over
-- it compacts like token pressure does, and the kept tail carries at most half of it, so image compactions stay rare.
M.max_images = 50
-- base64 characters, as sent: twice what std/vision lets one turn add, so the half a compaction keeps always fits a turn
M.max_image_bytes = 2 * require("std/vision").MAX_BYTES_PER_TURN

local IMAGE_TOKENS = 1200 -- an image without dimensions

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
- Carry recorded data (values, scores, IDs, results) forward exactly, item by item. Never replace it with a total, count or other figure you compute here: arithmetic in a checkpoint goes unchecked, and the data it replaces is lost.
- Capture user instructions and corrections faithfully. This checkpoint request is not one of them: do not mention it.
- If the conversation contains a [compaction-summary] block, it is an earlier checkpoint: carry forward what is still true, drop what is stale, and merge everything into this one checkpoint.
- Luau scope for Critical Context: compaction keeps the running VM. Distinguish user globals assigned without local from chunk-local variables and local functions; do not list locals or builtins as live user state. Record useful globals and what they hold based on the code above, noting any uncertainty. For a needed past local, record how to retrieve or recompute its value instead of implying the name is available. The continuing agent can inspect vars(), a name-to-value map of live user globals.]==]

local IMAGE_RULE = "\n- Images above are not kept after this checkpoint: record what each one showed that still matters, with its path."

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

-- Headroom for the summarizer's output: an eighth of the window, from 16384 tokens up to M.reserve_tokens, so a
-- reasoning summarizer is not cut off on a large window and a small window keeps room for history.
function M.reserve(window: number): number
	return math.min(M.reserve_tokens, math.max(16384, math.floor(window / 8)))
end

-- Context size at which to compact: M.trigger_ratio of the window, but always leaving room for the reply plus the
-- reserve. <= 0 means the window is too small for compaction (the pack stays off).
function M.trigger(window: number, max_output: number): number
	return math.min(math.floor(window * M.trigger_ratio), window - max_output - math.max(M.reserve(window), 4096))
end

-- Recent history kept verbatim at most (see M.find_cut).
function M.keep(window: number, max_output: number): number
	return math.floor((window - max_output) * M.keep_ratio)
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
	local n = math.ceil(chars / 4)
	for _, img in msg.images or {} do n += M.image_tokens(img) end
	return n
end

-- Vision tokens of one image: about one per 750 pixels (Anthropic's rule; OpenAI's tiles land close), so a 1024x576
-- frame costs about 790, up to the 1600 a provider's own downscaling caps it at. The anchor's scale corrects the bias
-- against the provider's own count.
function M.image_tokens(img): number
	if type(img.width) == "number" and type(img.height) == "number" then return math.min(1600, math.ceil(img.width * img.height / 750)) end
	return IMAGE_TOKENS
end

-- Placeholder for an image a request does not carry.
function M.image_note(img): string
	return "[image " .. tostring(img.path or "?") .. " " .. tostring(img.width or "?") .. "x" .. tostring(img.height or "?")
		.. " not shown; view() it again to look]"
end

-- Image count and base64 bytes carried by messages[from ..].
function M.images(messages, from: number?): (number, number)
	from = from or 1
	local count, bytes = 0, 0
	for i = from, #messages do
		for _, img in messages[i].images or {} do
			count += 1
			bytes += #(img.data or "")
		end
	end
	return count, bytes
end

function M.images_over(messages): boolean
	local count, bytes = M.images(messages, 1)
	return count > M.max_images or bytes > M.max_image_bytes
end

-- The first message a compaction may keep so that the tail carries at most half the image budget; never later than the
-- newest assistant message, so the newest exchange is always kept. 1 when every image fits.
function M.image_floor(messages): number
	local newest = 1
	for i = #messages, 1, -1 do
		if messages[i].role == "assistant" then newest = i; break end
	end
	local count, bytes = 0, 0
	for i = #messages, 1, -1 do
		for _, img in messages[i].images or {} do
			count += 1
			bytes += #(img.data or "")
		end
		if count > M.max_images / 2 or bytes > M.max_image_bytes / 2 then return math.min(i + 1, newest) end
	end
	return 1
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

-- Provider tokens of the system prompt and the task message: what no compaction can remove.
function M.fixed(a, system_chars: number, task): number
	return math.ceil((system_chars / 4 + M.estimate(task)) * a.scale)
end

-- Context size of the next request in provider tokens.
function M.measure(a, messages): number
	local n = a.tokens
	for i = a.count + 1, #messages do
		n += M.estimate(messages[i]) * a.scale
	end
	return n
end

-- Index of the first message to keep and the summarizer's prefix size, or nil when no cut works. Cuts only at an
-- assistant message (a tool result is never cut away from its call) whose summarizer prefix (`total` minus the kept
-- messages) is at most `max_prefix`. Of those, it keeps the most recent history within `keep_tokens`, or else the
-- least: the budget is a ceiling, so one huge exchange is never pinned in the tail, and the context after a compaction
-- is at most the task, the summary and max(keep_tokens, the newest exchange). keep_tokens = 0 keeps as little as fits.
-- messages[1] (the task) is never summarized, so the cut is at least 3; nil when no cut fits. `floor` (M.image_floor) is
-- the earliest cut allowed.
function M.find_cut(messages, keep_tokens: number, scale: number, total: number, max_prefix: number, floor: number?): (number?, number?)
	local kept, best, size = 0, nil, nil
	for i = #messages, math.max(3, floor or 3), -1 do
		kept += M.estimate(messages[i]) * scale
		if messages[i].role == "assistant" and total - kept <= max_prefix then
			if kept > keep_tokens then
				if best then return best, size end
				return i, total - kept
			end
			best, size = i, total - kept
		end
	end
	return best, size
end

-- The summarizer request: the agent's own request prefix (same system prompt and tools, messages[1 .. cut-1] unchanged)
-- plus the instruction. Pure; the supervisor performs the call and checkpoints its result.
-- `reserve`: M.reserve of the window the prefix was cut to fit; the summary may use four fifths of it, the rest absorbs
-- estimation error. `no_images`: send placeholders instead, after the provider rejected the request for its images.
function M.request(system: string, tools, messages, cut: number, reserve: number, no_images: boolean?)
	local prefix = table.move(messages, 1, cut - 1, 1, {})
	local count = M.images(prefix, 1)
	if no_images and count > 0 then
		count = 0 -- the summarizer cannot see them: its rule about images would ask the impossible
		for i, msg in prefix do
			if msg.images and #msg.images > 0 then
				local notes = {}
				for _, img in msg.images do table.insert(notes, M.image_note(img)) end
				local copy = table.clone(msg)
				copy.images = nil
				copy.content = (msg.content or "") .. "\n" .. table.concat(notes, "\n")
				prefix[i] = copy
			end
		end
	end
	table.insert(prefix, { role = "user", content = if count > 0 then INSTRUCTION .. IMAGE_RULE else INSTRUCTION })
	-- replay: the supervisor sends it under the system prompt of the last agent request as sent (VM hooks applied)
	return { system = system, tools = tools, messages = prefix, max_tokens = math.floor(reserve * 4 / 5), replay = true }
end

function M.wrap(task, summary, history)
	local notice = if history then '\nMessages removed by recorded compactions are available in this root lane: history({q="literal text"}), then history({id="entry-id"}). Print entry.text; follow next (resend q) or next_offset for more. Un-compacted retired RPI stages and other lanes are outside this archive.\n' else ""
	return task .. "\n\n" .. SUMMARY_PREFIX .. summary .. SUMMARY_SUFFIX .. notice
end

-- A summarizer reply is a candidate: it replaces messages[1 .. cut-1] only if it finished normally, is text only, and smaller
-- than what it replaces, or removes images (their bytes are what an image budget needs freed). Returns the new first
-- message, or nil and the reason.
function M.accept(reply, task: string, messages, cut: number, history: boolean): (string?, string?)
	local text = reply.text or ""
	if reply.finish ~= nil and reply.finish ~= "stop" then return nil, reply.finish end -- "length", "tool_markup"
	if #(reply.calls or {}) > 0 then return nil, "tool call" end
	if text:match("^%s*$") then return nil, "empty" end
	local first = M.wrap(task, text, history)
	local replaced = 0
	for i = 1, cut - 1 do
		replaced += M.estimate(messages[i])
	end
	if M.estimate({ content = first }) >= replaced and M.images(messages, 1) == M.images(messages, cut) then
		return nil, "not smaller"
	end
	return first, nil
end

-- lowercase substrings of the error text that mean "the request carried more image data than the provider takes"
local IMAGE_OVERFLOW = {
	"api 413 ",
	"too many images",
	"request_too_large",
	"request too large",
	"payload too large",
	"request entity too large",
}

local function matches(message: string, patterns): boolean
	local lower = message:lower()
	for _, pattern in patterns do if lower:find(pattern, 1, true) then return true end end
	return false
end

function M.image_overflow(message): boolean
	return matches(message, IMAGE_OVERFLOW)
end

-- A rejection that a compaction can fix: the request was over the context window or over the image limit.
function M.overflow(message): boolean
	return matches(message, OVERFLOW) or matches(message, IMAGE_OVERFLOW)
end

return M
