-- Shared by the plan, reorient and full arms: the high-leverage nudges, each injected into a tool result at a
-- decision moment (not into the system prompt). Generic wording; no task hints.
local M = {}

M.PLAN = "\n[harness] You are about to start changing files. Before going further, answer in a few lines, then act:\n"
	.. "1. Which requirements does the task actually state, and which are you assuming?\n"
	.. "2. Which single decision or fact, settled now, makes the rest simpler or unblocks it? Settle it first.\n"
	.. "3. What is the shortest loop that tells you whether a change worked? Set it up before building more."

M.REORIENT = "\n[harness] Progress has stalled. Before your next call, answer in a few lines, then act:\n"
	.. "1. What did the last results actually show?\n"
	.. "2. Which requirement is behind it, and is it stated by the task or assumed by you? Drop the assumptions.\n"
	.. "3. What can you delete or simplify instead of adding more?\n"
	.. "4. Which single decision or fact, settled now, unblocks the rest? Do that next."

-- opts: { plan = bool, reorient = bool }
function M.install(opts)
	local hooks = require("std/hooks")
	local planned, writes_now = false, false
	local last_err, same, best, stale, idle, fired = nil, 0, -1, 0, 0, 0
	hooks.on("before_run", function(p)
		writes_now = p.code:find("write%(") ~= nil or p.code:find("edit%(") ~= nil
		idle = if writes_now then 0 else idle + 1
		return nil
	end)
	hooks.on("after_run", function(res)
		local out = res.output or ""
		local note = nil
		if opts.plan and writes_now and not planned then
			planned = true
			note = M.PLAN
		end
		if opts.reorient then
			local err = res.error or out:match("[^\n]*FAILED[^\n]*") or out:match("[^\n]*panicked[^\n]*") or out:match("[^\n]*[Ee]rror[^\n]*")
			same = if err and err == last_err then same + 1 else 0
			last_err = err
			local passed = tonumber(out:match("(%d+) passed"))
			if passed then
				if passed > best then
					best, stale = passed, 0
				else
					stale += 1
				end
			end
			if not note and fired < 2 and (same >= 2 or stale >= 5 or idle >= 8) then
				fired += 1
				same, stale, idle = 0, 0, 0
				note = M.REORIENT
			end
		end
		if note then
			hx.log("### NUDGE\n" .. note)
			res.output = out .. note
		end
		return res
	end)
end

return M
