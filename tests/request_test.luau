local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local T = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, lacks, raises = T.check, T.has, T.lacks, T.raises

local calls, answers, slept = {}, {}, {}
host.post = function(name, request, options)
	table.insert(calls, { name = name, body = request, request = json.decode(request), options = options })
	local answer = table.remove(answers, 1) or { 500, "script ran out" }
	return answer[1], answer[2], answer[3]
end
host.sleep = function(seconds)
	table.insert(slept, seconds)
end
host.quiet = true

local thought = '{"signature":"c2lnbmVk","summary":[],"type":"thought"}'
local output = '{"content":[{"text":"Looking.","type":"text"}],"type":"model_output"}'
local call = '{"arguments":{},"id":"call_1","name":"luau","type":"function_call"}'
local function interaction(status: string, steps: { string }): { any }
	return { 200, ('{"status":%q,"steps":[%s],"usage":{"total_input_tokens":100,"total_cached_tokens":40,'
		.. '"total_output_tokens":7,"total_thought_tokens":5}}'):format(status, table.concat(steps, ",")) }
end
local function hi(): { any }
	return { { type = "user_input", content = { { type = "text", text = "hi" } } } }
end

loop.effort = "xhigh"
loop.tools = { { name = "luau", description = "run code", parameters = { type = "object" }, run = function() end } :: any }

answers = { interaction("requires_action", { thought, output, call }) }
local reply = loop.request("be brief", hi(), "gemini-4-carbon")

local sent = calls[1].request
check("the session's endpoint is asked", calls[1].name, "session")
check("a hard prompt gets fifteen minutes", calls[1].options.timeout, 900)
check("the model", sent.model, "gemini-4-carbon")
check("nothing is stored", sent.store, false)
check("the system prompt", sent.system_instruction, "be brief")
check("the model keeps its own output limit", sent.generation_config.max_output_tokens, nil)
check("xhigh is high on a model without it", sent.generation_config.thinking_level, "high")
check("a function tool", json.encode(sent.tools[1]),
	json.encode({ type = "function", name = "luau", description = "run code", parameters = { type = "object" } }))
check("the steps go as they are", json.encode(sent.input), json.encode(hi()))
check("the reply's status", reply.status, "requires_action")
check("and its steps", #reply.steps, 3)
check("its usage", (reply.usage :: any).total_cached_tokens, 40)

local steps = hi()
table.move(reply.steps, 1, #reply.steps, 2, steps)
answers = { interaction("completed", { output }) }
loop.request("be brief", steps, "gemini-4-neon")
check("neon thinks as hard as asked", calls[2].request.generation_config.thinking_level, "xhigh")
check("the model's steps go back as it sent them", calls[2].body:find(table.concat({ thought, output, call }, ","), 1, true) ~= nil, true)

loop.effort = nil
answers = { interaction("completed", { output }) }
loop.request("s", hi())
check("with no model named, the session's", calls[3].request.model, host.model)
check("no effort leaves the model its own thinking level", calls[3].request.generation_config, nil)

answers = { interaction("completed", { thought }), interaction("completed", { thought, output }) }
reply = loop.request("s", hi())
check("a thought with nothing after it is an empty reply, asked for again", #reply.steps, 2)
check("in a second request", #calls, 5)

answers = { interaction("incomplete", { thought }) }
check("a reply cut off in thought is kept", loop.request("s", hi()).status, "incomplete")

answers = { { 200, '{"status":"failed","steps":[],"errors":[{"code":"internal","message":"it broke"}]}' } }
raises("a failed interaction", "it broke", loop.request, "s", hi())

answers = { { 200, '{"content":[{"type":"text","text":"hi"}]}' } }
raises("a reply without steps", "cannot read", loop.request, "s", hi())

answers = { { 400, '{"error":{"code":"invalid_request","message":"bad signature"}}' } }
raises("an error is not retried", "bad signature", loop.request, "s", hi())
check("one request each", #calls, 9)

answers = { { 503, "busy" }, interaction("completed", { output }) }
check("a busy endpoint is asked again", #loop.request("s", hi()).steps, 1)

local compact_talk = loop.Conversation.new()
compact_talk:say("start")
answers = {
	interaction("completed", {
		'{"type":"model_output","content":[{"type":"text","text":"part "},{"type":"text","text":"one"},{"type":"text","text":""}]}',
		'{"type":"model_output","content":[{"type":"text","text":""}]}',
		'{"type":"model_output","content":[{"type":"text","text":"part two"}]}',
	}),
}
loop.compact("s", compact_talk)
check("multi-part model_output joins chunks within a step and steps with a newline",
	compact_talk.steps[1].content[1].text, "part one\npart two\n\n" .. loop.resume_text())

answers = { interaction("requires_continuation", { thought }) }
check("requires_continuation normalizes to incomplete", loop.request("s", hi()).status, "incomplete")

answers = { interaction("requires_continuation", { output }) }
check("requires_continuation with output normalizes to incomplete", loop.request("s", hi()).status, "incomplete")

answers = { { 200, '{"status":"failed","steps":[],"errors":[]}' } }
local empty_err_ok, empty_err = pcall(loop.request, "s", hi())
check("failed with empty errors raises", empty_err_ok, false)
has("failed with empty errors falls back to unknown error", empty_err, "unknown error")
lacks("failed with empty errors does not format nil", empty_err, "nil")

answers = { { 200, '{"status":"failed","steps":[]}' } }
raises("failed without errors falls back to unknown error", "unknown error", loop.request, "s", hi())

answers = { { 200, '{"status":"failed","steps":[],"error":{"code":"quota","message":"quota exceeded"}}' } }
raises("failed with singular error object extracts error.message", "quota exceeded", loop.request, "s", hi())

answers = { { 400, '{"error":{"code":"quota_exceeded"}}' } }
raises("400 with error.code only extracts code", "(400): quota_exceeded", loop.request, "s", hi())

answers = { { 400, '{"error":{},"message":{}}' } }
local ok_empty_obj, err_empty_obj = pcall(loop.request, "s", hi())
check("400 with empty error objects fails", ok_empty_obj, false)
lacks("400 with empty error objects never formats table address", err_empty_obj, "table:")

answers = { { 400, '{"error":{"code":"invalid_request","message":"bad signature"}}' } }
local ok_400, err_400 = pcall(loop.request, "s", hi())
check("400 fails", ok_400, false)
check("400 extracts structured error.message", err_400, "the model request failed (400): bad signature")

answers = { { 403, '{"error":{"code":403,"message":"permission denied"}}' } }
local ok_403, err_403 = pcall(loop.request, "s", hi())
check("403 fails", ok_403, false)
check("403 extracts structured error.message", err_403, "the model request failed (403): permission denied")

answers = { { 400, '{"error":"flat error"}' } }
raises("400 extracts flat error string", "(400): flat error", loop.request, "s", hi())

answers = { { 403, '{"message":"top message"}' } }
raises("403 extracts top-level message", "(403): top message", loop.request, "s", hi())

slept = {}
answers = {
	{ 500, "err1" },
	{ 502, "err2" },
	{ 503, "err3" },
	{ 429, "err4" },
	interaction("completed", { output }),
}
check("retries up to 4 transient failures", #loop.request("s", hi()).steps, 1)
check("slept 4 times", #slept, 4)
for attempt = 1, 4 do
	local low, high = 2 ^ (attempt - 1), 2 ^ attempt
	check(("attempt %d backoff within [%g, %g]"):format(attempt, low, high),
		slept[attempt] >= low and slept[attempt] <= high, true)
end

slept = {}
local logs = {}
local real_log = host.log
host.quiet = false
host.log = function(line)
	table.insert(logs, line)
end
answers = {
	{ 429, '{"error":{"message":"slow down"}}', 3 },
	{ 429, '{"error":{"message":"slow down"}}', 200 },
	interaction("completed", { output }),
}
check("retry_after succeeds", #loop.request("s", hi()).steps, 1)
host.quiet = true
host.log = real_log
check("respects retry_after", slept[1], 3)
check("caps retry_after at 120", slept[2], 120)
has("formats retry_after with %g s", logs[1], "retrying in 3 s")

T.done("request")
