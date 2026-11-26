local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local T = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = T.check, T.raises

local calls, answers = {}, {}
host.post = function(name, request, options)
	table.insert(calls, { name = name, body = request, request = json.decode(request), options = options })
	local answer = table.remove(answers, 1) or { 500, "script ran out" }
	return answer[1], answer[2]
end
host.sleep = function() end
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

T.done("request")
