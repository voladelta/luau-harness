local failures = 0

local function fail(name: string, got: any, want: string)
	failures += 1
	print(("FAIL %s\n  got:  %s\n  %s"):format(name, tostring(got), want))
end

local T = {}

function T.check(name: string, got: any, want: any)
	if got ~= want then
		fail(name, got, "want: " .. tostring(want))
	end
end

function T.has(name: string, text: any, fragment: string)
	if not tostring(text):find(fragment, 1, true) then
		fail(name, text, "want it to contain: " .. fragment)
	end
end

function T.lacks(name: string, text: any, fragment: string)
	if tostring(text):find(fragment, 1, true) then
		fail(name, text, "want it not to contain: " .. fragment)
	end
end

function T.raises(name: string, fragment: string, fn: (...any) -> ...any, ...: any)
	local ok, err = pcall(fn, ...)
	T.check(name .. " raises", ok, false)
	if not ok then
		T.has(name .. " message", err, fragment)
	end
end

function T.done(label: string)
	if failures > 0 then
		error(("%s: %d check(s) failed"):format(label, failures), 0)
	end
	print(label .. ": ok")
end

return T
