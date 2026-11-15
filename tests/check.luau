-- What the Luau tests share: checks that count their failures, and the report at the end. Not a test itself (its
-- name does not end in _test, so CMake does not run it as one). `hx --run` has no require, so a test loads it from
-- beside its own file, whose path `debug.info(1, "s")` gives:
--
--   local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
--   local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
--   ...
--   checks.done("tools")
--
-- The tests run where they may write, not in the source tree, which is then outside the wall: give hx `--ro` for
-- this directory, as CMake does, or the read of this file is refused.

local failures = 0

local function fail(name: string, got: any, want: string)
	failures += 1
	print(("FAIL %s\n  got:  %s\n  %s"):format(name, tostring(got), want))
end

local T = {}

-- got must equal want.
function T.check(name: string, got: any, want: any)
	if got ~= want then
		fail(name, got, "want: " .. tostring(want))
	end
end

-- text must contain fragment.
function T.has(name: string, text: any, fragment: string)
	if not tostring(text):find(fragment, 1, true) then
		fail(name, text, "want it to contain: " .. fragment)
	end
end

-- text must not contain fragment.
function T.lacks(name: string, text: any, fragment: string)
	if tostring(text):find(fragment, 1, true) then
		fail(name, text, "want it not to contain: " .. fragment)
	end
end

-- fn(...) must raise an error that contains fragment.
function T.raises(name: string, fragment: string, fn: (...any) -> ...any, ...: any)
	local ok, err = pcall(fn, ...)
	T.check(name .. " raises", ok, false)
	if not ok then
		T.has(name .. " message", err, fragment)
	end
end

-- Ends the test: raises when a check failed, so that hx exits 1, and otherwise says that all passed.
function T.done(label: string)
	if failures > 0 then
		error(("%s: %d check(s) failed"):format(label, failures), 0)
	end
	print(label .. ": ok")
end

return T
