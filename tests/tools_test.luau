-- The tools, the extension loader and the reading of pasted messages of the built-in loop. Run with: hx --run tests/tools_test.luau
-- Works in $TMPDIR, which hx creates for the run and removes afterwards.

local loop = assert(host.load(host.default_loop, "=loop"))()
local dir = host.env("TMPDIR") .. "/"

local failures = 0
local function check(name, got, want)
	if got ~= want then
		failures += 1
		print(("FAIL %s\n  got:  %s\n  want: %s"):format(name, tostring(got), tostring(want)))
	end
end

local tools = {}
for _, fn in loop.functions do
	tools[fn.name] = fn.run
end

-- The one tool: returns its result and whether it failed.
local function lua(code)
	return loop.tools[1].run({ code = code })
end

local function raises(name, fragment, fn, ...)
	local ok, err = pcall(fn, ...)
	check(name .. " raises", ok, false)
	if not ok and not tostring(err):find(fragment, 1, true) then
		check(name .. " message", err, fragment)
	end
end

check("there is one tool", #loop.tools, 1)
-- Every tool has a schema that names its required arguments as a JSON array.
for _, tool in loop.tools do
	local schema = json.encode(tool.parameters)
	check(tool.name .. " schema lists required", schema:find('"required":%[') ~= nil, true)
end

-- write, read
check("write", tools.write(dir .. "a/b/file.txt", "one\ntwo\nthree\n"), ("wrote 14 bytes to %sa/b/file.txt"):format(dir))
check("read", tools.read(dir .. "a/b/file.txt"), "1: one\n2: two\n3: three")
check("read a range", tools.read(dir .. "a/b/file.txt", 2, 1), "2: two\n[lines 2-2 of 3; offset=3 goes on]")
raises("read past the end", "past the end", tools.read, dir .. "a/b/file.txt", 9)
raises("read a missing file", "No such file", tools.read, dir .. "nope")
raises("read a directory", "is a directory", tools.read, dir .. "a")
raises("read without a path", "read: argument 1 (path) must be a string, not nil; usage: read(path, offset?, limit?)", tools.read)
raises("named arguments, as a JSON tool takes them", "must be a string, not table", tools.read, { file_path = dir .. "a/b/file.txt" })
tools.write(dir .. "empty", "")
check("read an empty file", tools.read(dir .. "empty"), "[empty file]")
-- A long file comes back as whole lines up to the byte limit, and says where to go on.
host.sh(("seq 1 1000 > %sseq.txt"):format(dir))
local function numbers(from, to)
	local lines = {}
	for i = from, to do
		table.insert(lines, ("%d: %d"):format(i, i))
	end
	return table.concat(lines, "\n")
end
loop.output_limit = 300 -- read keeps 200 bytes of it for the range line
check("read stops at the byte limit", tools.read(dir .. "seq.txt"), numbers(1, 17) .. "\n[lines 1-17 of 1000; offset=18 goes on]")
check("read goes on from the next offset", tools.read(dir .. "seq.txt", 37, 3), numbers(37, 39) .. "\n[lines 37-39 of 1000; offset=40 goes on]")
check("read the last lines", tools.read(dir .. "seq.txt", 999), numbers(999, 1000) .. "\n[lines 999-1000 of 1000]")
tools.write(dir .. "wide.txt", "short\n" .. string.rep("x", 250) .. "\nafter\n")
check("read stops before a line over the limit", tools.read(dir .. "wide.txt"), "1: short\n[lines 1-1 of 3; offset=2 goes on]")
check("a line over the limit comes back cut, with a note", tools.read(dir .. "wide.txt", 2), "2: " .. string.rep("x", 100) .. "\n[line 2 is 250 bytes; only the first 100 are shown]\n[lines 2-2 of 3; offset=3 goes on]")
check("and the next offset moves past it", tools.read(dir .. "wide.txt", 3), "3: after\n[lines 3-3 of 3]")
tools.write(dir .. "one.txt", string.rep("y", 250))
check("a file of one long line", tools.read(dir .. "one.txt"), "1: " .. string.rep("y", 100) .. "\n[line 1 is 250 bytes; only the first 100 are shown]")
loop.output_limit = 30000
check("a file within the limits comes back whole", tools.read(dir .. "seq.txt", nil, 5000), numbers(1, 1000))

-- host.write replaces a file through a temporary one, and the file stays what it was
local w = dir .. "w/"
local function sh(command)
	return host.sh(command:format(w, w)).out
end
host.write(w .. "run.sh", "#!/bin/sh\necho old\n")
sh("chmod 754 %srun.sh")
check("write over an executable", host.write(w .. "run.sh", "#!/bin/sh\necho new\n"), true)
check("it keeps its mode", sh("stat -c %%a %srun.sh"), "754\n")
check("and runs the new content", sh("%srun.sh"), "new\n")
host.write(w .. "new.txt", "x")
check("a new file has the default mode", sh("stat -c %%a %snew.txt"), host.sh("printf '%o\\n' $(( 0666 & ~$(umask) ))").out)
host.write(w .. "real.txt", "old")
sh("ln -s real.txt %slink.txt")
check("write through a link", host.write(w .. "link.txt", "new"), true)
check("the link stays a link", sh("readlink %slink.txt"), "real.txt\n")
check("and the file it names has the content", host.read(w .. "real.txt"), "new")
host.write(w .. "one.txt", "old")
sh("ln %sone.txt %stwo.txt")
check("write to a file with two names", host.write(w .. "one.txt", "new"), true)
check("it is still one file", sh("stat -c %%h %sone.txt"), "2\n")
check("and the other name sees the content", host.read(w .. "two.txt"), "new")
check("write to /dev/null", host.write("/dev/null", "x"), true)
check("which stays a device", host.sh("stat -c %F /dev/null").out, "character special file\n")
sh("ln -s gone.txt %sdangling.txt")
check("write through a dangling link", host.write(w .. "dangling.txt", "made"), true)
check("makes the file it names", host.read(w .. "gone.txt"), "made")
check("no temporary file is left", table.concat(host.list(w), " "), "dangling.txt gone.txt link.txt new.txt one.txt real.txt run.sh two.txt")

-- edit
tools.write(dir .. "e.txt", "alpha beta\nalpha gamma\n")
check("edit", tools.edit(dir .. "e.txt", "beta", "BETA"), ("replaced 1 occurrence in %se.txt"):format(dir))
check("edit result", host.read(dir .. "e.txt"), "alpha BETA\nalpha gamma\n")
raises("edit an ambiguous match", "occurs 2 times", tools.edit, dir .. "e.txt", "alpha", "x")
check("an ambiguous edit changes nothing", host.read(dir .. "e.txt"), "alpha BETA\nalpha gamma\n")
check("edit all", tools.edit(dir .. "e.txt", "alpha", "%1", true), ("replaced 2 occurrences in %se.txt"):format(dir))
check("edit is literal, not a pattern", host.read(dir .. "e.txt"), "%1 BETA\n%1 gamma\n")
raises("edit a missing text", "not found", tools.edit, dir .. "e.txt", "zzz", "x")
raises("edit with an empty old", "must not be empty", tools.edit, dir .. "e.txt", "", "x")
raises("edit that changes nothing", "change nothing", tools.edit, dir .. "e.txt", "BETA", "BETA")
raises("a missing text says how to recover", "read the file again", tools.edit, dir .. "e.txt", "zzz", "x")
raises("line numbers from read are named", "without the line numbers", tools.edit, dir .. "e.txt", "1: %1 BETA", "x")
-- A file with \r\n line ends is edited with the \n ends models write, and keeps its own.
tools.write(dir .. "crlf.txt", "one\r\ntwo\r\nthree\n")
check("edit across \\r\\n ends", tools.edit(dir .. "crlf.txt", "one\ntwo", "1\n2"), ("replaced 1 occurrence in %scrlf.txt"):format(dir))
check("keeps the file's ends", host.read(dir .. "crlf.txt"), "1\r\n2\r\nthree\n")
check("read shows \\r\\n ends as plain ones", tools.read(dir .. "crlf.txt"), "1: 1\n2: 2\n3: three")

-- bash
check("bash", tools.bash("echo out; echo err >&2; exit 3"), "out\nerr\n[exit 3]")
check("bash adds a newline before the status", tools.bash("printf x"), "x\n[exit 0]")
check("bash with no output", tools.bash("true"), "[exit 0]")
check("bash timeout", tools.bash("echo start; sleep 30", 0.3), "start\n[timed out and was killed]")
check("bash signal", tools.bash("kill -9 $$"), "[killed by signal 9]")
check("bash has no terminal on stdin", tools.bash("cat"), "[exit 0]")
check("a background job does not hold the call open", tools.bash("sleep 30 & echo started", 5), "started\n[exit 0]")
check("bash returns all of a long output, for code to work on", #tools.bash("seq 1 1000"), #host.sh("seq 1 1000").out + #"[exit 0]")
loop.output_limit = 100
local clipped = lua("return bash('seq 1 1000')")
check("the result the model is sent keeps its start", clipped:sub(1, 4), "1\n2\n")
check("and its end", clipped:find("1000\n[exit 0]", 1, true) ~= nil, true)
check("and says what is missing", clipped:find("bytes omitted", 1, true) ~= nil, true)
local spill = clipped:match("the whole output is in (%S+)")
check("and names the file that holds all of it", spill ~= nil, true)
check("which holds all of it", spill and host.read(spill), host.sh("seq 1 1000").out .. "[exit 0]")
check("it is clipped once, so it says so once", select(2, clipped:gsub("bytes omitted", "")), 1)
loop.output_limit = 30000

-- printable
check("printable keeps text, newlines and tabs", loop.printable("a\tb\nc é"), "a\tb\nc é")
check("printable drops escape sequences' control bytes", loop.printable("\27]52;c;aGk=\7x\27[2Jy\r\0\127z"), "]52;c;aGk=x[2Jyz")

-- the one tool
local function failed(code)
	return select(2, lua(code))
end
check("lua returns", lua("return 1 + 1, 'x', {a = 1}"), '2\nx\n{"a":1}')
check("lua prints", lua("print('a', 1) print('b')"), "a\t1\nb")
check("lua with nothing to show", lua("local x = 1"), "[no output]")
check("a call that works has not failed", failed("return 1"), false)
lua("counter = 41")
check("lua globals persist", lua("counter += 1 return counter"), "42")
check("lua globals stay out of the harness", _G.counter, nil)
check("lua syntax error", lua("return +"):match("^error: lua:1") ~= nil, true)
check("is a failure", failed("return +"), true)
check("lua runtime error", lua("error('boom')"), "error: lua:1: boom")
check("is a failure", failed("error('boom')"), true)
loop.call_seconds = 0.2
check("lua time limit", lua("while true do end"), "error: timeout")
check("the limit holds inside pcall", lua("while true do pcall(function() while true do end end) end"), "error: timeout")
loop.call_seconds = 600

-- the functions, as the model's code calls them
check("code calls the functions", lua(("write(%q, 'x\\n') return read(%q)"):format(dir .. "f.txt", dir .. "f.txt")), "1: x")
check("a failure keeps what was printed before it",
	lua(("print('first') print(edit(%q, 'nope', 'y')) print('never')"):format(dir .. "f.txt")),
	("first\nerror: edit: old text not found in %sf.txt: read the file again and copy the text exactly"):format(dir))
check("and what ran before it has happened", lua(("write(%q, 'done') error('later')"):format(dir .. "g.txt")), "error: lua:1: later")
check("so the file is there", host.read(dir .. "g.txt"), "done")
check("long brackets need no escaping",
	lua(("write(%q, [==[a \"quoted\" \\path\\ and ]] brackets]==]) return host.read(%q)"):format(dir .. "q.txt", dir .. "q.txt")),
	'a "quoted" \\path\\ and ]] brackets')
lua("read = 'shadowed'")
check("a global named like a function shadows it", lua("return type(read)"), "string")
lua("read = nil")
check("and gives it back once cleared", lua("return type(read)"), "function")

-- The model's code sees the host's tables read-only: an assignment raises and names the reason, and later calls
-- still work, the loop's own included.
for _, assignment in { "host.sh = nil", "json.encode = nil", "_G.host.sh = nil", "string.format = nil",
	"host.load('host.sh = nil')()" } do
	local result, broke = lua(assignment)
	check(assignment .. " fails", broke, true)
	check(assignment .. " says why", result:find("is read-only", 1, true) ~= nil, true)
end
check("bash still works", lua("return bash('echo still')"), "still\n[exit 0]")
check("json still works", lua("return json.encode({1})"), "[1]")
check("and so does the loop's own json", json.encode({ 2 }), "[2]")
check("reads see the host as it is", lua("return host.cwd == " .. ("%q"):format(host.cwd)), "true")
check("a view is the same value each time", lua("return host == _G.host"), "true")
check("its tables can be iterated", lua("local n = 0 for name in host.extensions do n += 1 end return n > 0"), "true")
check("and so can the view", lua("local found = false for name in host do found = found or name == 'sh' end return found"), "true")
check("_G.print is the print that reaches the model", lua("_G.print('through _G')"), "through _G")
check("_G is the code's own globals", lua("_G.via_g = 7 return via_g"), "7")
check("which stay out of the harness", _G.via_g, nil)
check("a chunk the code loads sees the views too", lua("return host.load('return host')() == host"), "true")
check("and has globals of its own", lua("host.load('loaded_global = 1')() return loaded_global"), "nil")
table.insert(loop.functions, { name = "twice", usage = "twice(n) -> number", run = function(n) return 2 * n end })
check("a project function is there at once", lua("return twice(21)"), "42")
check("and the model reads about it", loop.tools[1].description():find("- twice(n) -> number", 1, true) ~= nil, true)
table.remove(loop.functions)
local description = loop.tools[1].description()
for _, name in { "bash(command, timeout?)", "read(path, offset?, limit?)", "write(path, content)", "edit(path, old, new, all?)", "ext(name)", "[==[" } do
	check("the tool describes " .. name, description:find(name, 1, true) ~= nil, true)
end
check("the description is the same each time, for the cache", loop.tools[1].description(), description)
check("ext is described once", select(2, description:gsub("ext%(name%)", "")), 1)

-- the host's limits
local started = os.time()
check("a time limit ends a sleep", select(2, host.timed(0.2, function() host.sleep(30) end)), "timeout")
check("a time limit ends a command", select(2, host.timed(0.2, function() return host.sh("sleep 30") end)), "timeout")
check("an inner limit cannot outlast the outer one", select(2, host.timed(0.2, function() return host.timed(30, function() host.sleep(30) end) end)), "timeout")
check("none of that waited", os.time() - started < 5, true)
check("a limit that is not reached changes nothing", select(2, host.timed(5, function() return host.sh("echo ok").out end)), "ok\n")
-- A negative duration is an empty one, inside a limit or not.
check("a negative sleep", select(2, host.timed(5, function() host.sleep(-1); return "slept" end)), "slept")
check("a negative timeout", select(2, host.timed(5, function() return host.sh("sleep 30", { timeout = -1 }).timed_out end)), true)
check("a negative timeout outside a limit", host.sh("sleep 30", { timeout = -1 }).timed_out, true)
check("a negative limit", select(2, host.timed(-1, function() while true do end end)), "timeout")
check("nan is no time at all", select(2, host.timed(5, function() return host.sh("sleep 30", { timeout = 0 / 0 }).timed_out end)), true)
raises("a command with a NUL byte", "contains a NUL byte", host.sh, "echo a\0b")
raises("a path with a NUL byte", "contains a NUL byte", host.read, "a\0b")
local hog_ok, hog_err = pcall(function()
	local hoard = {}
	for i = 1, 1e7 do
		hoard[i] = string.rep("x", 1e6) .. i
	end
end)
check("a script that eats memory is stopped", hog_ok, false)
check("with an ordinary error", tostring(hog_err):find("not enough memory", 1, true) ~= nil, true)
check("and the harness lives on", json.encode({ 1 }), "[1]")

-- extensions (relative to the working directory, like the real thing)
check("this test runs where it can write", host.write(".hx/ext/greet.luau", "-- greets\nreturn { hello = function(n) return 'hello ' .. n end }\n"), true)
check("ext", loop.ext("greet").hello("x"), "hello x")
check("ext is cached", loop.ext("greet"), loop.ext("greet"))
host.write(".hx/ext/greet.luau", "-- greets\nreturn { hello = function(n) return 'hi ' .. n end }\n")
check("ext reloads a changed file", loop.ext("greet").hello("x"), "hi x")
check("ext from the lua tool", lua("return ext('greet').hello('y')"), "hi y")
host.write(".hx/ext/uses.luau", "-- uses a function\nreturn { run = function() return bash('echo from ext') end }\n")
check("an extension can call the functions", loop.ext("uses").run(), "from ext\n[exit 0]")
host.write(".hx/ext/twice.luau", "-- uses another extension\nreturn { hello = function(n) return ext('greet').hello(n) .. '!' end }\n")
check("an extension can use another", loop.ext("twice").hello("z"), "hi z!")
host.write(".hx/ext/meddle.luau", "-- meddles\nreturn { f = function() host.sh = nil end }\n")
raises("an extension cannot assign to the host either", "host is read-only", loop.ext("meddle").f)
check("so commands still run", tools.bash("echo ok"), "ok\n[exit 0]")
host.write(".hx/ext/broken.luau", "return {")
raises("a broken extension", "ext/broken", loop.ext, "broken")
raises("a missing extension", "no extension nope", loop.ext, "nope")
raises("a path as an extension name", "letters, digits", loop.ext, "../x")
-- A name that starts with _ is a helper other extensions share: it loads, but the model is not told of it.
host.write(".hx/ext/_x.luau", "-- shared code\nreturn { two = function() return 2 end }\n")
check("a helper extension loads", loop.ext("_x").two(), 2)
local prompt = loop.system_prompt()
check("the prompt lists the other extensions", prompt:find("- greet: greets", 1, true) ~= nil, true)
check("but not a helper", prompt:find("_x", 1, true), nil)
host.sh("rm -rf .hx")

-- Nesting. A project function can run a `lua` call inside the running one, or a whole turn on another
-- Conversation, as a sub-agent. Either leaves the outer call's printed lines and the outer turn's repeat counts
-- as it found them.
table.insert(loop.functions, { name = "nested", usage = "nested(code, seconds?) -> text", run = function(code, seconds)
	local outer = loop.call_seconds
	loop.call_seconds = seconds or outer
	local result = loop.tools[1].run({ code = code })
	loop.call_seconds = outer
	return result
end })
check("a nested lua call keeps the outer call's prints",
	lua("print('outer 1') local inner = nested('print(\"inner\")') print('outer 2') return inner"), "outer 1\nouter 2\ninner")
check("and so does one that fails",
	lua("print('outer 1') local inner = nested('error(\"no\", 0)') print('outer 2') return inner"), "outer 1\nouter 2\nerror: no")
check("or runs out of time",
	lua("print('outer 1') local inner = nested('print(1) while true do end', 0.2) print('outer 2') return inner"),
	"outer 1\nouter 2\n1\nerror: timeout")
-- A scripted model: each request takes the next reply.
local replies, next_id = {}, 0
local function calls(code)
	next_id += 1
	return { stop_reason = "tool_use", content = { { type = "tool_use", id = "c" .. next_id, name = "lua", input = { code = code } } } }
end
local done = { stop_reason = "end_turn", content = { { type = "thinking", thinking = "", signature = "s" } } }
local request = loop.request
loop.request = function()
	return assert(table.remove(replies, 1), "the scripted model ran out of replies")
end
table.insert(loop.functions, { name = "subagent", usage = "subagent() -> text", run = function()
	local talk = loop.Conversation.new()
	talk:say("a sub-task")
	loop.turn("system", talk)
	return "the sub-agent is done"
end })
local same = "return bash('echo same')"
-- The outer turn runs the same call twice, then a sub-agent whose turn runs it once, then the call a third time.
replies = { calls(same), calls(same), calls("return subagent()"), calls(same), done, calls(same), done }
local talk = loop.Conversation.new()
talk:say("the task")
loop.turn("system", talk)
check("the script was played to the end", #replies, 0)
local reminders = {}
for _, message in talk.messages do
	for _, block in if type(message.content) == "table" then message.content else {} do
		if block.type == "text" then
			table.insert(reminders, block.text)
		end
	end
end
check("a nested turn neither resets the outer turn's counts nor adds to them", #reminders, 1)
check("so the outer turn's third call is its third",
	(reminders[1] or ""):find('bash("echo same") has now returned the same result 3 times', 1, true) ~= nil, true)
loop.request = request
table.remove(loop.functions)
table.remove(loop.functions)

-- read_message joins the lines of a bracketed paste into one message.
local function message(lines)
	local i = 0
	local got = loop.read_message(function()
		i += 1
		return lines[i]
	end)
	return got, i
end
check("a plain line is a message", message({ "hello", "next" }), "hello")
check("a plain line reads one line", select(2, message({ "hello", "next" })), 1)
check("the end of input", message({}), nil)
check("a paste is one message", message({ "\27[200~first", "second", "third\27[201~", "next" }), "first\nsecond\nthird")
check("a paste reads up to its end", select(2, message({ "\27[200~a", "b\27[201~", "next" })), 2)
check("a paste on one line", message({ "\27[200~one\27[201~", "next" }), "one")
check("typed text around a paste", message({ "before \27[200~a", "b\27[201~ after" }), "before a\nb after")
check("a paste cut short by the end of input", message({ "\27[200~a", "b" }), "a\nb")
check("empty lines in a paste", message({ "\27[200~a", "", "b\27[201~" }), "a\n\nb")

if failures > 0 then
	error(failures .. " tool check(s) failed", 0)
end
print("tools: ok")
