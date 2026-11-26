local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local T = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = T.check, T.raises

local calls, answers = {}, {}
host.post = function(name, request, options)
	table.insert(calls, { name = name, request = json.decode(request), options = options })
	local answer = table.remove(answers, 1) or { 500, "script ran out" }
	return answer[1], answer[2]
end
host.sleep = function() end
host.quiet = true
host.route = function(model)
	return model:match("^gemini%-") and "gemini" or "session"
end

local thought = { type = "thought", signature = "c2lnbmVk", summary = { { type = "text", text = "weigh it" } } }
local function interaction(status: string, steps: { any }): { any }
	return { 200, json.encode({
		status = status,
		steps = steps,
		usage = { total_input_tokens = 100, total_cached_tokens = 40, total_output_tokens = 7, total_thought_tokens = 5 },
	}) }
end

loop.max_output = 9000
loop.effort = "xhigh"
loop.tools = { { name = "luau", description = "run code", parameters = { type = "object" }, run = function() end } :: any }

answers = { interaction("requires_action", {
	thought,
	{ type = "model_output", content = { { type = "text", text = "Looking." } } },
	{ type = "function_call", id = "call_1", name = "luau", arguments = { code = "return 1" } },
}) }
local reply = loop.request("be brief", { { role = "user", content = "what is one?" } }, nil, "gemini-4-carbon")

local sent = calls[1].request
check("the model's vendor is asked", calls[1].name, "gemini")
check("no beta header", calls[1].options.beta, nil)
check("the model", sent.model, "gemini-4-carbon")
check("nothing is stored", sent.store, false)
check("the system prompt", sent.system_instruction, "be brief")
check("no Messages fields", sent.messages or sent.system or sent.max_tokens, nil)
check("thinking shares Gemini's output limit, so the loop's is not sent", sent.generation_config.max_output_tokens, nil)
check("a hard prompt gets fifteen minutes", calls[1].options.timeout, 900)
check("effort is held to what the model has", sent.generation_config.thinking_level, "high")
check("a function tool", json.encode(sent.tools[1]),
	json.encode({ type = "function", name = "luau", description = "run code", parameters = { type = "object" } }))
check("the user's words", json.encode(sent.input),
	json.encode({ { type = "user_input", content = { { type = "text", text = "what is one?" } } } }))

check("a call stops for tools", reply.stop_reason, "tool_use")
check("thought, text, call", #reply.content, 3)
check("the summary shows", reply.content[1].thinking, "weigh it")
check("text", reply.content[2].text, "Looking.")
check("the call", json.encode({ reply.content[3].type, reply.content[3].id, reply.content[3].name, reply.content[3].input.code }),
	json.encode({ "tool_use", "call_1", "luau", "return 1" }))
check("uncached input", (reply.usage :: any).input_tokens, 60)
check("cached input", (reply.usage :: any).cache_read_input_tokens, 40)
check("thoughts are output", (reply.usage :: any).output_tokens, 12)

local picture = { type = "image", source = { type = "base64", media_type = "image/png", data = "AA==" } }
answers = { interaction("completed", { { type = "model_output", content = { { type = "text", text = "One." } } } }) }
loop.effort = "xhigh"
reply = loop.request("be brief", {
	{ role = "user", content = "what is one?" },
	{ role = "assistant", content = reply.content },
	{ role = "user", content = {
		{ type = "tool_result", tool_use_id = "call_1", content = { { type = "text", text = "1" }, picture }, is_error = true },
		{ type = "text", text = "go on" },
	} },
}, nil, "gemini-4-neon")
sent = calls[2].request
check("neon thinks as hard as asked", sent.generation_config.thinking_level, "xhigh")
check("the turn goes back step by step", json.encode(sent.input), json.encode({
	{ type = "user_input", content = { { type = "text", text = "what is one?" } } },
	thought,
	{ type = "model_output", content = { { type = "text", text = "Looking." } } },
	{ type = "function_call", id = "call_1", name = "luau", arguments = { code = "return 1" } },
	{ type = "function_result", call_id = "call_1", name = "luau", is_error = true,
		result = { { type = "text", text = "1" }, { type = "image", data = "AA==", mime_type = "image/png" } } },
	{ type = "user_input", content = { { type = "text", text = "go on" } } },
} :: { any }))
check("the answer ends the turn", reply.stop_reason, "end_turn")
check("its text", reply.content[1].text, "One.")

answers = { interaction("completed", { { type = "model_output", content = { { type = "text", text = "ok" } } } }) }
loop.request("s", { { role = "assistant", content = { { type = "thinking", thinking = "x", signature = "claude-signed" } :: any } },
	{ role = "user", content = "hi" } }, nil, "gemini-4-neon")
check("another vendor's thinking stays behind", #calls[3].request.input, 1)

loop.effort = nil
answers = { interaction("incomplete", { { type = "model_output", content = { { type = "text", text = "cut" } } } }) }
check("a cut reply", loop.request("s", { { role = "user", content = "hi" } }, nil, "gemini-4-neon").stop_reason, "max_tokens")
check("no effort leaves the model its own thinking level", calls[#calls].request.generation_config, nil)

answers = { { 200, json.encode({ status = "failed", steps = {}, errors = { { code = "internal", message = "it broke" } } }) } }
raises("a failed interaction", "it broke", loop.request, "s", { { role = "user", content = "hi" } }, nil, "gemini-4-neon")

answers = { { 400, json.encode({ error = { code = "invalid_request", message = "bad signature" } }) } }
raises("an error is not retried", "bad signature", loop.request, "s", { { role = "user", content = "hi" } }, nil, "gemini-4-neon")
check("one request", #calls, 6)

local before = #calls
answers = { { 200, json.encode({ content = { { type = "text", text = "hi" } }, stop_reason = "end_turn" }) } }
loop.effort = nil
reply = loop.request("s", { { role = "user", content = "hi" } }, nil, "deepseek-flash")
sent = calls[before + 1].request
check("other models keep the Messages format", json.encode({ sent.system, sent.messages[1].content, sent.tools[1].input_schema.type, sent.input }),
	json.encode({ "s", "hi", "object", nil } :: { any }))
check("their output limit", sent.max_tokens, 9000)
check("and the host's timeout", calls[before + 1].options.timeout, nil)
check("and its reply", reply.content[1].text, "hi")

T.done("gemini")
