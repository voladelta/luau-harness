local loop = assert(host.load(host.default_loop, "=loop"))()
local Conversation = loop.Conversation

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check = checks.check

local legal = {
	idle = { say = true, restart = true, resume = true },
	ready = { say = true, reply = true, restart = true, resume = true },
	owing = { answer = true },
}

local next_id = 0
local function random_reply()
	local content = {}
	if math.random() < 0.1 then
		return content
	end
	if math.random() < 0.5 then
		table.insert(content, { type = "thinking", thinking = "", signature = "s" })
	end
	if math.random() < 0.6 then
		table.insert(content, { type = "text", text = "words" })
	end
	for _ = 1, math.random(0, 3) do
		next_id += 1
		table.insert(content, { type = "tool_use", id = "call_" .. next_id, name = "bash", input = {} })
	end
	if #content == 0 then
		table.insert(content, { type = "text", text = "words" })
	end
	return content
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
	resume = function(talk)
		talk:resume({ type = "compaction", content = "summary", signature = "s" }, "go on")
	end,
}
local names = { "say", "reply", "answer", "restart", "resume" }

math.randomseed(20261114)
local talk = Conversation.new()
local seen = { idle = 0, ready = 0, owing = 0 }
for step = 1, 5000 do
	local state = talk:state()
	seen[state] += 1
	local name = names[math.random(#names)]
	local before = json.encode(talk.messages)
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
		check(("step %d: a refused %s changes nothing"):format(step, name), json.encode(talk.messages), before)
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
check("a second say joins the first message", json.encode(t.messages), '[{"content":[{"text":"task","type":"text"},{"text":"more","type":"text"}],"role":"user"}]')
check("an empty reply records nothing", #t:reply({}), 0)
check("and leaves it ready", t:state(), "ready")
local calls = t:reply({ { type = "text", text = "x" }, { type = "tool_use", id = "a", name = "bash", input = {} }, { type = "tool_use", id = "b", name = "bash", input = {} } })
check("reply returns its calls", #calls, 2)
check("tool calls are owed", t:state(), "owing")
local ok = pcall(t.answer, t, function(call)
	if call.id == "b" then
		error("interrupted", 0)
	end
	return "fine", false
end)
check("an answer that raises", ok, false)
check("records nothing, the calls are still owed", t:state(), "owing")
t:abandon("interrupted by the user")
check("abandon answers every call", json.encode(t.messages[3]), '{"content":[{"content":"error: interrupted by the user","is_error":true,"tool_use_id":"a","type":"tool_result"},{"content":"error: interrupted by the user","is_error":true,"tool_use_id":"b","type":"tool_result"}],"role":"user"}')
t:say("go on")
check("text after results joins them, behind", t.messages[3].content[3].text, "go on")
check("sound", t:problem(), nil)

local fork = t:fork()
fork:say("only in the fork")
check("a fork does not change the original", #t.messages[3].content, 3)
check("the fork has it", #fork.messages[3].content, 4)

local function broken(name, messages, fragment)
	local c = Conversation.new()
	c.messages = messages
	local problem = c:problem()
	if not problem or not problem:find(fragment, 1, true) then
		check(name, problem, fragment)
	end
end
local use = { type = "tool_use", id = "a", name = "bash", input = {} }
local result = { type = "tool_result", tool_use_id = "a", content = "x" }
broken("starts with the model", { { role = "assistant", content = { { type = "text", text = "x" } } } }, "where the user should speak")
broken("two user messages", { { role = "user", content = "a" }, { role = "user", content = "b" } }, "where the assistant should speak")
broken("empty reply", { { role = "user", content = "a" }, { role = "assistant", content = {} } }, "empty reply")
broken("unanswered call", { { role = "user", content = "a" }, { role = "assistant", content = { use } }, { role = "user", content = "b" } }, "unanswered")
broken("result behind text", { { role = "user", content = "a" }, { role = "assistant", content = { use } }, { role = "user", content = { { type = "text", text = "t" }, result } } }, "behind other content")
broken("result for no call", { { role = "user", content = { result } } }, "was not made")
broken("result for another call", { { role = "user", content = "a" }, { role = "assistant", content = { use } }, { role = "user", content = { { type = "tool_result", tool_use_id = "z", content = "x" } } } }, "was not made")

checks.done("conversation")
