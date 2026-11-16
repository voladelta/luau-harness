local dir = host.env("TMPDIR") .. "/"

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, raises = checks.check, checks.has, checks.raises

local function wait_all(seconds: number): { [number]: { ok: boolean, text: string } }
	local done = {}
	while true do
		local id, ok, text = host.wait(seconds)
		if not id then
			return done
		end
		done[id] = { ok = ok, text = text }
	end
end

check("with no child, wait returns nil and none running", select("#", host.wait(0)), 2)
check("nil", (host.wait(0)), nil)
check("none running", select(2, host.wait()), 0)

shared = "the parent's"
local returns = host.spawn(function(a, b)
	shared = "the child's"
	return a .. " and " .. b
end, "one", "two")
local nothing = host.spawn(function() end)
local raises_text = host.spawn(function()
	error("boom", 0)
end)
local raises_table = host.spawn(function()
	error({})
end)
local returns_number = host.spawn(function()
	return 5
end)
local big = host.spawn(function()
	return string.rep("x", 3000000)
end)
check("ids count up from 1", returns, 1)
local done = wait_all(10)
check("a returned string", done[returns].text, "one and two")
check("counts as returned", done[returns].ok, true)
check("nothing returned is the empty string", done[nothing].text, "")
check("and is no error", done[nothing].ok, true)
check("an error is its message", done[raises_text].text, "boom")
check("and not a return", done[raises_text].ok, false)
check("an error that is no string says so", done[raises_table].text, "(error is not a string)")
has("a value that is no string is refused", done[returns_number].text, "returned a number")
check("as an error", done[returns_number].ok, false)
check("a long result comes back whole", #done[big].text, 3000000)
check("the parent's globals are its own", shared, "the parent's")

local began = os.clock()
for _ = 1, 4 do
	host.spawn(function()
		host.sleep(1)
	end)
end
check("wait gives up after its seconds", (host.wait(0.1)), nil)
check("and says how many still run", select(2, host.wait(0)), 4)
wait_all(10)
check("four children that sleep a second take about one", os.clock() - began < 3, true)

local ok, inner = host.timed(0.3, function()
	local id = host.spawn(function()
		host.sleep(1)
		local finished, why = host.timed(0.2, function()
			while true do
			end
		end)
		return ("outlived the parent's limit; its own: %s %s"):format(tostring(finished), tostring(why))
	end)
	return id
end)
check("spawned inside a limit", ok, true)
done = wait_all(10)
check("the parent's limit does not end the child, and the child's own limits work", done[inner].text,
	"outlived the parent's limit; its own: false timeout")
local lone = host.spawn(function()
	local spawned, why = pcall(host.spawn, function() end)
	return ("%s|%s|%s"):format(tostring((host.input())), tostring(spawned), tostring(why))
end)
done = wait_all(10)
check("a child reads no stdin and cannot spawn", done[lone].text, "nil|false|host.spawn: a spawned child cannot spawn")

local slow = host.spawn(function()
	host.sleep(4)
end)
began = os.clock()
local in_time, waited, running = host.timed(3, function()
	return host.wait()
end)
check("a wait inside a limit ends before it", in_time, true)
check("with nothing", waited, nil)
check("and the child still running", running, 1)
check("two seconds before the limit", os.clock() - began < 2, true)

local killed = host.spawn(function()
	host.sh("kill -9 $PPID")
	host.sleep(10)
end)
local leaves = host.spawn(function()
	host.sh(("(sleep 300 & echo $! > %sleft.pid) > /dev/null 2>&1"):format(dir))
	return "left one running"
end)
done = wait_all(10)
check("the slow one ended too", done[slow].ok, true)
check("a killed child is not a return", done[killed].ok, false)
has("and says what happened", done[killed].text, "ended without a result")
check("the other returned", done[leaves].text, "left one running")
local pid = (host.read(dir .. "left.pid") or ""):match("%d+")
check("it had started a process", pid ~= nil, true)
check("which is gone", host.sh(("kill -0 %s 2>&1"):format(pid or "0")).code ~= 0, true)

raises("spawn takes a function", "function expected", host.spawn, "not a function")

checks.done("spawn")
