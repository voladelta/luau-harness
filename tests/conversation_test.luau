local loop = assert(host.load(host.default_loop, "=loop"))()
local Conversation = loop.Conversation

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check = checks.check

local legal = {
	idle = { say = true, restart = true },
	ready = { say = true, reply = true, restart = true },
	owing = { answer = true },
}

local next_id = 0
local function random_reply()
	local steps = {}
	if math.random() < 0.1 then
		return steps
	end
	if math.random() < 0.5 then
		table.insert(steps, { type = "thought", signature = "s" })
	end
	if math.random() < 0.6 then
		table.insert(steps, { type = "model_output", content = { { type = "text", text = "words" } } })
	end
	for _ = 1, math.random(0, 3) do
		next_id += 1
		table.insert(steps, { type = "function_call", id = "call_" .. next_id, name = "bash", arguments = {} })
	end
	return steps
end

local operations = {
	say = function(talk)
		talk:say("text")
	end,
	reply = function(talk)
		talk:reply(random_reply())
	end,
	answer = function(talk)
		talk:answer(function(call)
			return "result of " .. call.id, math.random() < 0.3
		end)
	end,
	restart = function(talk)
		talk:restart("summary")
	end,
}
local names = { "say", "reply", "answer", "restart" }

math.randomseed(20261114)
local talk = Conversation.new()
local seen = { idle = 0, ready = 0, owing = 0 }
for step = 1, 5000 do
	local state = talk:state()
	seen[state] += 1
	local name = names[math.random(#names)]
	local before = json.encode(talk.steps)
	local ok, err = pcall(operations[name], talk)
	if legal[state][name] then
		if not ok then
			check(("step %d: %s while %s should work"):format(step, name, state), err, nil)
		end
		local problem = talk:problem()
		if problem then
			check(("step %d: history after %s while %s"):format(step, name, state), problem, nil)
			break
		end
	else
		check(("step %d: %s while %s raises"):format(step, name, state), ok, false)
		check(("step %d: %s while %s names the state"):format(step, name, state), tostring(err):find("cannot " .. name .. " while " .. state, 1, true) ~= nil, true)
		check(("step %d: a refused %s changes nothing"):format(step, name), json.encode(talk.steps), before)
	end
	if math.random() < 0.05 then
		talk:abandon("stopped")
		check(("step %d: nothing is owed after abandon"):format(step), talk:state() ~= "owing", true)
		check(("step %d: history after abandon"):format(step), talk:problem(), nil)
	end
end
check("the walk visited every state", seen.idle > 100 and seen.ready > 100 and seen.owing > 100, true)

local t = Conversation.new()
check("starts idle", t:state(), "idle")
t:say("task")
check("say makes it ready", t:state(), "ready")
t:say("more")
check("a second say joins the first step", json.encode(t.steps), '[{"content":[{"text":"task","type":"text"},{"text":"more","type":"text"}],"type":"user_input"}]')
check("an empty reply records nothing", #t:reply({}), 0)
check("and leaves it ready", t:state(), "ready")
local calls = t:reply({ { type = "model_output", content = { { type = "text", text = "x" } } },
	{ type = "function_call", id = "a", name = "bash", arguments = {} }, { type = "function_call", id = "b", name = "bash", arguments = {} } })
check("reply returns its calls", #calls, 2)
check("tool calls are owed", t:state(), "owing")
local ok = pcall(t.answer, t, function(call)
	if call.id == "b" then
		error("interrupted", 0)
	end
	return "fine", false
end)
check("an answer that raises", ok, false)
check("records nothing, the calls are still owed", #t.steps, 4)
t:abandon("interrupted by the user")
check("abandon answers every call", json.encode({ t.steps[5], t.steps[6] }), '[{"call_id":"a","is_error":true,"name":"bash","result":[{"text":"error: interrupted by the user","type":"text"}],"type":"function_result"},{"call_id":"b","is_error":true,"name":"bash","result":[{"text":"error: interrupted by the user","type":"text"}],"type":"function_result"}]')
check("answered calls leave it ready", t:state(), "ready")
t:say("go on")
check("text after results is the user's next step", json.encode(t.steps[7]), '{"content":[{"text":"go on","type":"text"}],"type":"user_input"}')
t:reply({ { type = "thought", signature = "s" }, { type = "model_output", content = { { type = "text", text = "done" } } } })
check("a reply without calls leaves it idle", t:state(), "idle")
t:say("and then")

local looked = loop.Conversation.new()
looked:say("look")
looked:reply({ { type = "function_call", id = "a", name = "lua", arguments = { code = "see('x.png')" } } })
local picture = { type = "image", data = "AA==", mime_type = "image/png" }
looked:answer(function()
	return "[image 1]", false, { picture }
end)
check("an answer with images is the text, then the images", json.encode(looked.steps[3]),
	json.encode({ type = "function_result", call_id = "a", name = "lua", result = { { type = "text", text = "[image 1]" }, picture } }))
check("and the conversation is sound", looked:problem(), nil)
check("sound", t:problem(), nil)

local fork = t:fork()
fork:say("only in the fork")
check("a fork does not change the original", #t.steps[#t.steps].content, 1)
check("the fork has it", #fork.steps[#fork.steps].content, 2)

local function broken(name, steps, fragment)
	local c = Conversation.new()
	c.steps = steps
	local problem = c:problem()
	if not problem or not problem:find(fragment, 1, true) then
		check(name, problem, fragment)
	end
end
local said = { type = "user_input", content = { { type = "text", text = "a" } } }
local use = { type = "function_call", id = "a", name = "bash", arguments = {} }
local result = { type = "function_result", call_id = "a", name = "bash", result = { { type = "text", text = "x" } } }
broken("starts with the model", { { type = "model_output", content = { { type = "text", text = "x" } } } }, "where the user should speak")
broken("not a step", { said, "words" }, "is not a step")
broken("unanswered call", { said, use, said }, "unanswered")
broken("result for no call", { said, result }, "was not made")
broken("a call answered twice", { said, use, result, result }, "was not made")
broken("result for another call", { said, use, { type = "function_result", call_id = "z", name = "bash", result = {} } }, "was not made")

checks.done("conversation")
