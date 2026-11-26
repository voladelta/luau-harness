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
broken("call without id", { said, { type = "function_call", name = "bash", arguments = {} } }, "without an id")
broken("call with empty id", { said, { type = "function_call", id = "", name = "bash", arguments = {} } }, "without an id")
broken("call with non-string id", { said, { type = "function_call", id = 123, name = "bash", arguments = {} } }, "without an id")
broken("duplicate open call id", { said, use, use }, "repeats tool call id a")
broken("result without call_id", { said, use, { type = "function_result", name = "bash", result = {} } }, "was not made")

local saved_session_bytes = loop.session_image_bytes
loop.session_image_bytes = 20
local media_talk = Conversation.new()
media_talk:say("watch")
local img1 = { type = "image", data = string.rep("A", 10), mime_type = "image/png" }
local doc1 = { type = "document", data = string.rep("B", 10), mime_type = "application/pdf" }
local img2 = { type = "image", data = string.rep("C", 10), mime_type = "image/png" }
local img3 = { type = "image", data = string.rep("D", 12), mime_type = "image/png" }

media_talk:reply({ { type = "function_call", id = "m1", name = "lua", arguments = {} } })
media_talk:answer(function()
	return "first", false, { img1 }
end)
local step1 = media_talk.steps[3]

media_talk:reply({ { type = "function_call", id = "m2", name = "lua", arguments = {} } })
media_talk:answer(function()
	return "second", false, { doc1 }
end)
local step2 = media_talk.steps[5]
check("within session_image_bytes, earlier step stays untouched", media_talk.steps[3], step1)
check("within session_image_bytes, earlier media remains", media_talk.steps[3].result[2], img1)

local media_fork = media_talk:fork()

media_talk:reply({ { type = "function_call", id = "m3", name = "lua", arguments = {} } })
media_talk:answer(function()
	return "text only", false
end)
check("text-only turn leaves earlier media untouched", media_talk.steps[3], step1)

media_talk:reply({ { type = "function_call", id = "m4", name = "lua", arguments = {} } })
media_talk:answer(function()
	return "fourth", false, { img2 }
end)
check("exceeding session_image_bytes evicts the oldest media part first",
	json.encode(media_talk.steps[3].result[2]),
	json.encode({ type = "text", text = "[earlier image omitted to stay within the request size limit]" }))
check("eviction stops once back within limit, keeping newer earlier step untouched", media_talk.steps[5], step2)
check("newest media part stays", media_talk.steps[9].result[2], img2)
check("eviction keeps conversation sound", media_talk:problem(), nil)
check("eviction leaves prior fork intact", media_fork.steps[3].result[2], img1)

media_talk:reply({ { type = "function_call", id = "m5", name = "lua", arguments = {} } })
media_talk:answer(function()
	return "fifth", false, { img3 }
end)
check("evicts oldest remaining document part when needed",
	json.encode(media_talk.steps[5].result[2]),
	json.encode({ type = "text", text = "[earlier document omitted to stay within the request size limit]" }))
check("evicts across multiple steps until within limit",
	json.encode(media_talk.steps[9].result[2]),
	json.encode({ type = "text", text = "[earlier image omitted to stay within the request size limit]" }))
check("newest oversized-history media part stays", media_talk.steps[11].result[2], img3)
check("conversation stays sound after multi-step eviction", media_talk:problem(), nil)
check("prior fork keeps both original media parts",
	media_fork.steps[3].result[2] == img1 and media_fork.steps[5].result[2] == doc1, true)
check("prior fork stays sound", media_fork:problem(), nil)
loop.session_image_bytes = saved_session_bytes

checks.done("conversation")
