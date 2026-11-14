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
for _, tool in loop.tools do
	tools[tool.name] = tool.run
end

local function raises(name, fn, args, fragment)
	local ok, err = pcall(fn, args)
	check(name .. " raises", ok, false)
	if not ok and not tostring(err):find(fragment, 1, true) then
		check(name .. " message", err, fragment)
	end
end

-- Every tool has a schema that names its required arguments as a JSON array.
for _, tool in loop.tools do
	local schema = json.encode(tool.parameters)
	check(tool.name .. " schema lists required", schema:find('"required":%[') ~= nil, true)
end

-- write, read
check("write", tools.write({ file_path = dir .. "a/b/file.txt", content = "one\ntwo\nthree\n" }), ("wrote 14 bytes to %sa/b/file.txt"):format(dir))
check("read", tools.read({ file_path = dir .. "a/b/file.txt" }), "1: one\n2: two\n3: three")
check("read a range", tools.read({ file_path = dir .. "a/b/file.txt", offset = 2, limit = 1 }), "2: two\n[lines 2-2 of 3; offset=3 goes on]")
raises("read past the end", tools.read, { file_path = dir .. "a/b/file.txt", offset = 9 }, "past the end")
raises("read a missing file", tools.read, { file_path = dir .. "nope" }, "No such file")
raises("read a directory", tools.read, { file_path = dir .. "a" }, "is a directory")
raises("read without a path", tools.read, {}, 'argument "file_path" must be a string')
tools.write({ file_path = dir .. "empty", content = "" })
check("read an empty file", tools.read({ file_path = dir .. "empty" }), "[empty file]")
-- A long file comes back as whole lines up to the byte limit, and says where to go on.
host.sh(("seq 1 1000 > %sseq.txt"):format(dir))
local function numbers(from, to)
	local lines = {}
	for i = from, to do
		table.insert(lines, ("%d: %d"):format(i, i))
	end
	return table.concat(lines, "\n")
end
loop.output_limit = 100
check("read stops at the byte limit", tools.read({ file_path = dir .. "seq.txt" }), numbers(1, 17) .. "\n[lines 1-17 of 1000; offset=18 goes on]")
check("read goes on from the next offset", tools.read({ file_path = dir .. "seq.txt", offset = 37, limit = 3 }), numbers(37, 39) .. "\n[lines 37-39 of 1000; offset=40 goes on]")
check("read the last lines", tools.read({ file_path = dir .. "seq.txt", offset = 999 }), numbers(999, 1000) .. "\n[lines 999-1000 of 1000]")
tools.write({ file_path = dir .. "wide.txt", content = "short\n" .. string.rep("x", 250) .. "\nafter\n" })
check("read stops before a line over the limit", tools.read({ file_path = dir .. "wide.txt" }), "1: short\n[lines 1-1 of 3; offset=2 goes on]")
check("a line over the limit comes back cut, with a note", tools.read({ file_path = dir .. "wide.txt", offset = 2 }), "2: " .. string.rep("x", 100) .. "\n[line 2 is 250 bytes; only the first 100 are shown]\n[lines 2-2 of 3; offset=3 goes on]")
check("and the next offset moves past it", tools.read({ file_path = dir .. "wide.txt", offset = 3 }), "3: after\n[lines 3-3 of 3]")
tools.write({ file_path = dir .. "one.txt", content = string.rep("y", 250) })
check("a file of one long line", tools.read({ file_path = dir .. "one.txt" }), "1: " .. string.rep("y", 100) .. "\n[line 1 is 250 bytes; only the first 100 are shown]")
loop.output_limit = 30000
check("a file within the limits comes back whole", tools.read({ file_path = dir .. "seq.txt", limit = 5000 }), numbers(1, 1000))

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
tools.write({ file_path = dir .. "e.txt", content = "alpha beta\nalpha gamma\n" })
check("edit", tools.edit({ file_path = dir .. "e.txt", old_string = "beta", new_string = "BETA" }), ("replaced 1 occurrence in %se.txt"):format(dir))
check("edit result", host.read(dir .. "e.txt"), "alpha BETA\nalpha gamma\n")
raises("edit an ambiguous match", tools.edit, { file_path = dir .. "e.txt", old_string = "alpha", new_string = "x" }, "occurs 2 times")
check("an ambiguous edit changes nothing", host.read(dir .. "e.txt"), "alpha BETA\nalpha gamma\n")
check("edit all", tools.edit({ file_path = dir .. "e.txt", old_string = "alpha", new_string = "%1", replace_all = true }), ("replaced 2 occurrences in %se.txt"):format(dir))
check("edit is literal, not a pattern", host.read(dir .. "e.txt"), "%1 BETA\n%1 gamma\n")
raises("edit a missing text", tools.edit, { file_path = dir .. "e.txt", old_string = "zzz", new_string = "x" }, "not found")
raises("edit with an empty old", tools.edit, { file_path = dir .. "e.txt", old_string = "", new_string = "x" }, "must not be empty")
raises("edit that changes nothing", tools.edit, { file_path = dir .. "e.txt", old_string = "BETA", new_string = "BETA" }, "change nothing")
raises("a missing text says how to recover", tools.edit, { file_path = dir .. "e.txt", old_string = "zzz", new_string = "x" }, "read the file again")
raises("line numbers from read are named", tools.edit, { file_path = dir .. "e.txt", old_string = "1: %1 BETA", new_string = "x" }, "without the line numbers")
-- A file with \r\n line ends is edited with the \n ends models write, and keeps its own.
tools.write({ file_path = dir .. "crlf.txt", content = "one\r\ntwo\r\nthree\n" })
check("edit across \\r\\n ends", tools.edit({ file_path = dir .. "crlf.txt", old_string = "one\ntwo", new_string = "1\n2" }), ("replaced 1 occurrence in %scrlf.txt"):format(dir))
check("keeps the file's ends", host.read(dir .. "crlf.txt"), "1\r\n2\r\nthree\n")
check("read shows \\r\\n ends as plain ones", tools.read({ file_path = dir .. "crlf.txt" }), "1: 1\n2: 2\n3: three")

-- bash
check("bash", tools.bash({ command = "echo out; echo err >&2; exit 3" }), "out\nerr\n[exit 3]")
check("bash adds a newline before the status", tools.bash({ command = "printf x" }), "x\n[exit 0]")
check("bash with no output", tools.bash({ command = "true" }), "[exit 0]")
check("bash timeout", tools.bash({ command = "echo start; sleep 30", timeout = 0.3 }), "start\n[timed out and was killed]")
check("bash signal", tools.bash({ command = "kill -9 $$" }), "[killed by signal 9]")
check("bash has no terminal on stdin", tools.bash({ command = "cat" }), "[exit 0]")
check("a background job does not hold the call open", tools.bash({ command = "sleep 30 & echo started", timeout = 5 }), "started\n[exit 0]")
loop.output_limit = 100
local clipped = tools.bash({ command = "seq 1 1000" })
check("long output keeps its start", clipped:sub(1, 4), "1\n2\n")
check("long output keeps its end", clipped:find("1000\n[exit 0]", 1, true) ~= nil, true)
check("long output says what is missing", clipped:find("bytes omitted", 1, true) ~= nil, true)
local spill = clipped:match("the whole output is in (%S+)")
check("long output names the file that holds all of it", spill ~= nil, true)
check("and the file holds all of it", spill and host.read(spill), host.sh("seq 1 1000").out)
loop.output_limit = 30000

-- printable
check("printable keeps text, newlines and tabs", loop.printable("a\tb\nc é"), "a\tb\nc é")
check("printable drops escape sequences' control bytes", loop.printable("\27]52;c;aGk=\7x\27[2Jy\r\0\127z"), "]52;c;aGk=x[2Jyz")

-- lua
check("lua returns", tools.lua({ code = "return 1 + 1, 'x', {a = 1}" }), '2\nx\n{"a":1}')
check("lua prints", tools.lua({ code = "print('a', 1) print('b')" }), "a\t1\nb")
check("lua with nothing to show", tools.lua({ code = "local x = 1" }), "[no output]")
tools.lua({ code = "counter = 41" })
check("lua globals persist", tools.lua({ code = "counter += 1 return counter" }), "42")
check("lua globals stay out of the harness", counter, nil)
raises("lua syntax error", tools.lua, { code = "return +" }, "lua:1")
raises("lua runtime error", tools.lua, { code = "error('boom')" }, "boom")
loop.lua_seconds = 0.2
raises("lua time limit", tools.lua, { code = "while true do end" }, "timeout")
raises("the limit holds inside pcall", tools.lua, { code = "while true do pcall(function() while true do end end) end" }, "timeout")
loop.lua_seconds = 60

-- the host's limits
local started = os.time()
check("a time limit ends a sleep", select(2, host.timed(0.2, function() host.sleep(30) end)), "timeout")
check("a time limit ends a command", select(2, host.timed(0.2, function() return host.sh("sleep 30") end)), "timeout")
check("an inner limit cannot outlast the outer one", select(2, host.timed(0.2, function() return host.timed(30, function() host.sleep(30) end) end)), "timeout")
check("none of that waited", os.time() - started < 5, true)
check("a limit that is not reached changes nothing", select(2, host.timed(5, function() return host.sh("echo ok").out end)), "ok\n")
-- A negative duration is an empty one, inside a limit or not.
check("a negative sleep", select(2, host.timed(5, function() host.sleep(-1) return "slept" end)), "slept")
check("a negative timeout", select(2, host.timed(5, function() return host.sh("sleep 30", { timeout = -1 }).timed_out end)), true)
check("a negative timeout outside a limit", host.sh("sleep 30", { timeout = -1 }).timed_out, true)
check("a negative limit", select(2, host.timed(-1, function() while true do end end)), "timeout")
check("nan is no time at all", select(2, host.timed(5, function() return host.sh("sleep 30", { timeout = 0 / 0 }).timed_out end)), true)
raises("a command with a NUL byte", host.sh, "echo a\0b", "contains a NUL byte")
raises("a path with a NUL byte", host.read, "a\0b", "contains a NUL byte")
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
check("ext from the lua tool", tools.lua({ code = "return ext('greet').hello('y')" }), "hi y")
host.write(".hx/ext/twice.luau", "-- uses another extension\nreturn { hello = function(n) return ext('greet').hello(n) .. '!' end }\n")
check("an extension can use another", loop.ext("twice").hello("z"), "hi z!")
host.write(".hx/ext/broken.luau", "return {")
raises("a broken extension", loop.ext, "broken", "ext/broken")
raises("a missing extension", loop.ext, "nope", "no extension nope")
raises("a path as an extension name", loop.ext, "../x", "letters, digits")
host.sh("rm -rf .hx")

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
