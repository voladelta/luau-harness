local loop = assert(host.load(host.default_loop, "=loop"))()
local dir = host.env("TMPDIR") .. "/"

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, lacks, raises = checks.check, checks.has, checks.lacks, checks.raises

local agents
for _, fn in loop.functions do
	if fn.name == "agents" then
		agents = fn.run
	end
end

mark = "nobody's"
local systems = {}
loop.system_prompt = function()
	table.insert(systems, "built")
	return "the session's prompt"
end
local function call(steps, code)
	return { status = "requires_action", steps = { { type = "function_call", id = "c" .. #steps, name = "lua", arguments = { code = code } } } }
end
local function says(words)
	return { status = "completed", steps = { { type = "model_output", content = { { type = "text", text = words } } } } }
end
loop.request = function(system, steps, model)
	local last = steps[#steps]
	if last.type == "function_result" then
		return says("ran: " .. last.result[1].text)
	end
	local told = last.content[#last.content].text
	local command = told:match("^run: (.*)$")
	if command then
		local code = ("return bash(%q)"):format(command)
		return call(steps, code)
	end
	local code = told:match("^lua: (.*)$")
	if code then
		return call(steps, code)
	end
	local words = told:match("^say: (.*)$")
	if told == "count" then
		words = ("%d steps"):format(#steps)
	elseif told == "functions" then
		local names = {}
		for name in loop.tools[1].description():gmatch("\n%- ([%w_]+)%(") do
			table.insert(names, name)
		end
		words = table.concat(names, " ") .. " on " .. tostring(model) .. " under: " .. system
	elseif told == "global" then
		words = "mark was " .. tostring(mark)
		mark = "the agent's"
	elseif told == "fail" then
		error("the model request failed (500): no", 0)
	end
	return says(words or "what?")
end

check("with no agents, it says so", agents(), "You have no agents yet: give agents a list of tasks.")

raises("tasks that are no table", "argument 1 (tasks) must be a table, not string", agents, "do it")
raises("a table that is no list", "tasks is a list", agents, { parser = "do it" })
raises("a task without a name", 'task 2 must be a table { name = "...", task = "..." }', agents,
	{ { name = "a", task = "say: a" }, { task = "say: b" } })
raises("a name that is no name", "a name is up to 40 letters, digits, _ and -", agents, { { name = "a b", task = "say: a" } })
raises("an empty task", "the task for a is empty", agents, { { name = "a", task = " \n" } })
raises("one name twice", "a is given two tasks in one call", agents, { { name = "a", task = "say: 1" }, { name = "a", task = "say: 2" } })
check("a call that fails sends nothing", agents(), "You have no agents yet: give agents a list of tasks.")

host.sh({ "mkfifo", dir .. "meet" })
loop.agent_seconds = 5
local reports = agents({
	{ name = "first", task = ("run: cat %smeet; echo one > %sone.txt; echo made one"):format(dir, dir) },
	{ name = "second", task = ("run: echo two > %stwo.txt; : > %smeet; echo made two"):format(dir, dir) },
})
loop.agent_seconds = 1800
lacks("two agents that meet at a fifo work side by side", reports, "stopped at")
check("their reports come back in the order they were named", (reports:gsub("in %d+ s", "in N s")),
	"[agent first: done in N s]\nran: made one\n[exit 0]\n\n[agent second: done in N s]\nran: made two\n[exit 0]")
check("the first did its work", host.read(dir .. "one.txt"), "one\n")
check("and the second", host.read(dir .. "two.txt"), "two\n")
check("the agents' prompt was built once", #systems, 1)

reports = agents({ { name = "first", task = "count" }, { name = "third", task = "count" } })
has("the agent sent more has what it did", reports, "[agent first: done in 0 s]\n5 steps")
has("a new one starts empty", reports, "[agent third: done in 0 s]\n1 steps")
has("one task alone", agents({ name = "third", task = "say: again" }), "again")
check("a report is given once", agents(),
	"No agent is at work, and you have every report. Your agents so far: first, second, third.")

loop.agent_model = "some-model"
local told = agents({ { name = "first", task = "functions" } })
has("an agent has the functions", told, "bash read write edit see skill ext on some-model")
lacks("but not agents", told, "ext agents")
has("its prompt is the session's and the agents' paragraph", told,
	"under: the session's prompt\n\nYou are one of several agents at work in this directory")
check("the lead keeps its model", loop.model, host.model)
check("and its functions", loop.functions[#loop.functions].name, "agents")

mark = "the lead's"
has("an agent sees the lead's globals", agents({ { name = "first", task = "global" } }), "mark was the lead's")
check("and does not change them", mark, "the lead's")
has("nor does its next run see what its last one set", agents({ { name = "first", task = "global" } }), "mark was the lead's")

local failed = agents({ { name = "second", task = "fail" } })
has("a failed turn says why", failed, "[agent second: failed after 0 s: the model request failed (500): no]\n(it said nothing)")
has("and the agent goes on from there", agents({ { name = "second", task = "count" } }), "5 steps")

loop.agent_seconds = 1
local late = agents({ { name = "slow", task = "run: sleep 20; echo never" } })
has("a run past its time is stopped", late, "[agent slow: stopped at the 1 s one run may take]")
loop.agent_seconds = 1800
has("and the agent can still be sent more", agents({ { name = "slow", task = "count" } }), "4 steps")

local killed = agents({ { name = "slow", task = "run: kill -9 $PPID" } })
has("a killed agent is reported", killed, "[agent slow: stopped after 0 s (the child ended without a result")
has("and has the conversation it had before", agents({ { name = "slow", task = "count" } }), "6 steps")

host.write(dir .. "lead.txt", "the lead's")
host.write(dir .. "kept.txt", "old")
local wrote = agents({
	{ name = "writer", task = ("lua: write(%q, 'a') edit(%q, 'a', 'b') write(%q, 'new') return 'ok'"):format(dir .. "w1.txt", dir .. "w1.txt", dir .. "kept.txt") },
	{ name = "clasher", task = ("lua: host.sleep(0.05) host.write(%q, 'theirs') return write(%q, 'mine')"):format(dir .. "other.txt", dir .. "other.txt") },
	{ name = "reader", task = ("lua: host.sleep(0.05) host.write(%q, 'theirs') read(%q) return write(%q, 'mine')"):format(dir .. "read.txt", dir .. "read.txt", dir .. "read.txt") },
})
has("a report names the files written, each once", wrote, ("[agent writer: done in 0 s]\n[wrote, with write and edit: %sw1.txt, %skept.txt]\nran: ok"):format(dir, dir))
has("a file written since the task began, by another, is not overwritten", wrote, "other.txt was written while agents share this directory, and not by your write or edit")
check("and stays as it was", host.read(dir .. "other.txt"), "theirs")
has("once read, it can be", wrote, ("ran: wrote 4 bytes to %sread.txt"):format(dir))
check("a file from before the task is the agent's to overwrite", host.read(dir .. "kept.txt"), "new")
host.write(dir .. "w1.txt", "changed between its tasks")
has("an agent sent back still knows what it wrote, and does not overwrite a change made since",
	agents({ name = "writer", task = ("lua: return write(%q, 'stale')"):format(dir .. "w1.txt") }),
	"w1.txt has changed since you last read or wrote it")
check("so the change stays", host.read(dir .. "w1.txt"), "changed between its tasks")
lacks("a report of an agent that wrote nothing names no files", agents({ name = "writer", task = "say: idle" }), "[wrote")

host.sh({ "mkdir", "-p", ".hx" })
local logged = agents({ name = "logged", task = "say: hello" })
host.sh({ "rm", "-rf", ".hx" })
lacks("outside a turn there is no conversation file to name", logged, "[its conversation:")

local began = os.clock()
local patience = loop.agent_wait
loop.agent_wait = 0.3
agents({ { name = "long", task = "run: sleep 30; echo never" } })
loop.agent_wait = 10
local stopped = agents({ { name = "long", stop = true } })
has("an agent at work can be stopped", stopped, "[agent long: stopped as you asked, after")
check("without waiting for its command", os.clock() - began < 10, true)
has("and has the conversation it had before", agents({ { name = "long", task = "count" } }), "1 steps")
raises("an idle agent has nothing to stop", "long is not at work, so there is nothing to stop", agents, { { name = "long", stop = true } })
raises("nor has one that does not exist", "nobody is not at work", agents, { name = "nobody", stop = true })
loop.agent_wait = patience
raises("a stop takes no task", "task 1 has a task and stop = true", agents, { { name = "long", stop = true, task = "count" } })

loop.agent_limit = 1
loop.agent_wait = 0.3
local busy = agents({ { name = "first", task = "run: sleep 1; echo a" }, { name = "second", task = "run: sleep 1; echo b" } })
has("the first is at work", busy, "[agent first: still at work, 0 s so far]")
has("the second waits for its place", busy, "[agent second: waiting for one of the 1 places at work]")
has("and the lead is told how to wait", busy, "agents() with no tasks waits for the agents still at work.")
raises("an agent at work takes no second task", "first is still at work on its last task", agents, { { name = "first", task = "count" } })
raises("nor does one that waits", "second is still at work on its last task", agents, { { name = "second", task = "count" } })
has("one that waits is stopped before it begins", agents({ { name = "second", stop = true } }), "[agent second: stopped as you asked, before it began]")
agents({ { name = "second", task = "run: sleep 1; echo b" } })
loop.agent_wait = 10
local rest = agents()
has("waiting gives the first report", rest, "ran: a\n[exit 0]")
has("and the second, which ran after it", rest, "ran: b\n[exit 0]")
lacks("with nobody left at work", rest, "still at work")

local previous_request = loop.request
local previous_seconds = loop.agent_seconds
host.sh({ "mkdir", "-p", ".hx" })
loop.agent_seconds = 0.2
loop.request = function(system, steps, model)
	if steps[#steps].type == "user_input" and steps[#steps].content[1].text == "sleepy-task" then
		host.sleep(5)
	end
	return previous_request(system, steps, model)
end
local lead_talk = loop.Conversation.new()
lead_talk:say('lua: return agents({ name = "sleepy", task = "sleepy-task" })')
loop.turn("the session's prompt", lead_talk)
loop.agent_seconds = previous_seconds
loop.request = previous_request
local final_text = lead_talk.steps[#lead_talk.steps].content[1].text
local saved_log = final_text:match("%[its conversation: (%.hx/log/[^%]]+%.json)%]")
check("a timed-out agent inside a turn names its conversation file", saved_log ~= nil, true)
local saved = saved_log and json.decode(host.read(saved_log) or "{}")
check("and its log holds the message it timed out on", saved and saved.steps and saved.steps[1] and saved.steps[1].content[1].text, "sleepy-task")
host.sh({ "rm", "-rf", ".hx" })

checks.done("agents")
