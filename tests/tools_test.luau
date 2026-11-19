local loop = assert(host.load(host.default_loop, "=loop"))()
local dir = host.env("TMPDIR") .. "/"

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = checks.check, checks.raises

local tools = {}
for _, fn in loop.functions do
	tools[fn.name] = fn.run
end

local function lua(code)
	return loop.tools[1].run({ code = code })
end

assert(not host.stat(".hx"), "tools_test writes .hx and removes it: run it from a scratch directory, not from a project")

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
host.sh(("seq 1 1000 > %sseq.txt"):format(dir))
local function numbers(from, to)
	local lines = {}
	for i = from, to do
		table.insert(lines, ("%d: %d"):format(i, i))
	end
	return table.concat(lines, "\n")
end
loop.output_limit = 300
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
tools.write(dir .. "crlf.txt", "one\r\ntwo\r\nthree\n")
check("edit across \\r\\n ends", tools.edit(dir .. "crlf.txt", "one\ntwo", "1\n2"), ("replaced 1 occurrence in %scrlf.txt"):format(dir))
check("keeps the file's ends", host.read(dir .. "crlf.txt"), "1\r\n2\r\nthree\n")
check("read shows \\r\\n ends as plain ones", tools.read(dir .. "crlf.txt"), "1: 1\n2: 2\n3: three")
tools.write(dir .. "crlf2.txt", "one\r\ntwo\r\nthree\r\n")
tools.edit(dir .. "crlf2.txt", "two", "2\n2b")
check("new text takes the file's \\r\\n ends", host.read(dir .. "crlf2.txt"), "one\r\n2\r\n2b\r\nthree\r\n")
tools.edit(dir .. "crlf2.txt", "2b\r\n", "x\r\ny\n")
check("and keeps them when it has them already", host.read(dir .. "crlf2.txt"), "one\r\n2\r\nx\r\ny\r\nthree\r\n")
tools.write(dir .. "lf.txt", "one\ntwo\n")
tools.edit(dir .. "lf.txt", "two", "2\n2b")
check("a file with \\n ends keeps them", host.read(dir .. "lf.txt"), "one\n2\n2b\n")

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

check("printable keeps text, newlines and tabs", loop.printable("a\tb\nc é"), "a\tb\nc é")
check("printable drops escape sequences' control bytes", loop.printable("\27]52;c;aGk=\7x\27[2Jy\r\0\127z"), "]52;c;aGk=x[2Jyz")
check("printable drops the marks that reorder text", loop.printable("a\226\128\174b\226\129\166c\194\155d"), "abcd")
check("and one that what it drops would leave behind",
	loop.printable("a\226\128\0\174b\226\194\155\128\174c\226\226\128\174\129\166d"), "abcd")
local nested, began = ("\226\128"):rep(200000) .. ("\174"):rep(200000), os.clock()
check("however deep they nest", loop.printable(nested), "")
check("in one pass", os.clock() - began < 1, true)

local function failed(code)
	return select(2, lua(code))
end
check("lua returns", lua("return 1 + 1, 'x', {a = 1}"), '2\nx\n{"a":1}')
check("lua prints", lua("print('a', 1) print('b')"), "a\t1\nb")
check("lua with nothing to show", lua("local x = 1"), "[no output]")
host.write(dir .. "walked/a/b.luau", "x")
check("the lua tool has host.walk", lua(("return table.concat(host.walk(%q), ' ')"):format(dir .. "walked")), "a/ a/b.luau")
check("and host.stat", lua(("return host.stat(%q).type"):format(dir .. "walked/a")), "dir")
check("and ext('text')", lua("return (ext('text').replace('1.5', '.', ','))"), "1,5")
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

for _, assignment in { "host.sh = nil", "json.encode = nil", "_G.host.sh = nil", "string.format = nil",
	"host.load('host.sh = nil')()" } do
	local result, broke = lua(assignment)
	check(assignment .. " fails", broke, true)
	check(assignment .. " says why", result:find("is read-only", 1, true) ~= nil, true)
end
for _, digging in { "rawset(host, 'sh', nil)", "rawset(string, 'format', nil)", "host.extensions.bytes = ''",
	"host.skills.hx = ''", "host.args[1] = 'x'", "host.endpoints.exa = true",
	"getmetatable('').__index.upper = function() return 'x' end", "getmetatable('').__index = {}",
	"setmetatable(host, {})", "getmetatable(host).__index = nil", "getfenv(bash).host.sh = nil",
	"setfenv(bash, {})" } do
	local _, broke = lua(digging)
	check(digging .. " fails", broke, true)
end
check("strings still have their methods", ("still"):upper(), "STILL")
check("and the loop's own host is whole", type(host.sh), "function")
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

local started = os.time()
check("a time limit ends a sleep", select(2, host.timed(0.2, function() host.sleep(30) end)), "timeout")
check("a time limit ends a command", select(2, host.timed(0.2, function() return host.sh("sleep 30") end)), "timeout")
check("an inner limit cannot outlast the outer one", select(2, host.timed(0.2, function() return host.timed(30, function() host.sleep(30) end) end)), "timeout")
check("none of that waited", os.time() - started < 5, true)
check("a limit that is not reached changes nothing", select(2, host.timed(5, function() return host.sh("echo ok").out end)), "ok\n")
check("a negative sleep", select(2, host.timed(5, function() host.sleep(-1); return "slept" end)), "slept")
check("a negative timeout", select(2, host.timed(5, function() return host.sh("sleep 30", { timeout = -1 }).timed_out end)), true)
check("a negative timeout outside a limit", host.sh("sleep 30", { timeout = -1 }).timed_out, true)
check("a negative limit", select(2, host.timed(-1, function() while true do end end)), "timeout")
check("nan is no time at all", select(2, host.timed(5, function() return host.sh("sleep 30", { timeout = 0 / 0 }).timed_out end)), true)
raises("a command with a NUL byte", "contains a NUL byte", host.sh, "echo a\0b")
check("a list is a program and its arguments, each passed as it is", host.sh({ "printf", "%s|", "it's", "a b", "$HOME", "`x`", 7 }).out, "it's|a b|$HOME|`x`|7|")
raises("an empty list", "the command list is empty", host.sh, {})
raises("a list with a table in it", "command[2] is a table", host.sh, { "echo", {} })
raises("a list with a NUL byte", "command[2] contains a NUL byte", host.sh, { "echo", "a\0b" })
check("input goes to stdin", host.sh({ "base64", "-d" }, { input = "aGVsbG8=" }).out, "hello")
check("all of a large input", host.sh("wc -c", { input = string.rep("x", 3000000) }).out, "3000000\n")
check("an empty input is an empty stdin", host.sh("cat; echo end", { input = "" }).out, "end\n")
check("a command may stop reading its input", host.sh("head -c 2", { input = string.rep("y", 3000000) }).out, "yy")
raises("an input that is not text", "input is the text", host.sh, "cat", { input = {} })
do
	local long = host.sh("head -c 3000000 /dev/zero | tr '\\0' a")
	check("an output of megabytes is kept whole, for code to work on", #long.out, 3000000)
	check("so nothing is omitted", long.omitted, nil)
	local endless = host.sh("head -c 20000000 /dev/zero | tr '\\0' a")
	local marker = ("\n[... %d bytes of output omitted ...]\n"):format(endless.omitted or 0)
	check("an output past 16 MiB is cut in the middle", endless.out:find(marker, 1, true) ~= nil, true)
	check("and omitted counts the bytes that are gone", #endless.out - #marker + endless.omitted, 20000000)
end
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
check("with its memory back", gcinfo() < 64 * 1024, true)

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
host.write(".hx/ext/_x.luau", "-- shared code\nreturn { two = function() return 2 end }\n")
check("a helper extension loads", loop.ext("_x").two(), 2)
local prompt = loop.system_prompt()
check("the prompt lists the other extensions", prompt:find("- greet: greets", 1, true) ~= nil, true)
check("but not a helper", prompt:find("_x", 1, true), nil)
host.sh("rm -rf .hx")

local real_sh = host.sh
local function with_rg(found: boolean): string
	host.sh = function(command: string, options: any): any
		if command == "command -v rg" then
			return { out = "", code = found and 0 or 1 }
		end
		return real_sh(command, options)
	end
	local text = loop.system_prompt()
	host.sh = real_sh
	return text
end
check("with rg, the prompt prefers it to grep",
	with_rg(true):find("use rg through bash rather than plain grep", 1, true) ~= nil, true)
check("without rg, the prompt does not name it", with_rg(false):find("ripgrep", 1, true), nil)

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
replies = { calls(same), calls(same), calls("return subagent()"), calls(same), done, calls(same), done }
local talk = loop.Conversation.new()
talk:say("the task")
host.write(".hx/here", "")
loop.turn("system", talk)
check("the script was played to the end", #replies, 0)
local logged = {}
for _, name in host.list(".hx/log") or {} do
	if name:match("%.json$") then
		table.insert(logged, name)
	end
end
check("a sub-agent's conversation is logged, and only its", #logged, 1)
check("under the session's name and its number", (logged[1] or ""):match("%-sub%-1%.json$"), "-sub-1.json")
local sub = json.decode(host.read(".hx/log/" .. (logged[1] or "")) or "{}")
check("with the system prompt it ran on", sub.system, "system")
check("from its task", sub.messages and sub.messages[1].content, "a sub-task")
check("to its last reply", sub.messages and #sub.messages, 4)
host.sh("rm -rf .hx")
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

checks.done("tools")
