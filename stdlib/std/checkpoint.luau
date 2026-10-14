-- Checkpoint pack: before the first final answer is accepted, send the model back once to test the cases its fix implies.
-- Opt-in (`--pack std/checkpoint`, or `[packs] enable = ["std/checkpoint"]` in .harness/policy.toml); it costs extra
-- turns on every run. On Terminal-Bench 4.0 mvcc-lsm-compaction (Sonnet 5.5, medium) it lifted the solved rate from
-- 1/20 to 13/20; the wording below is the one that was measured, so change it only with a new measurement.
-- The pack also puts a heads-up in the system prompt's Finishing section, so the first answer is planned to meet the
-- check: with it, Opus 5.5 low solved mvcc 5/5 pre-registered (6/7 with the pilot and a later check run), where the
-- check alone never got Opus to a solve.
local M = {}

M.enabled = true
M.message = "Before this answer is accepted: state the root cause you fixed in one sentence, then list the other situations where that same cause could fire beyond the reported one (other inputs, orderings, repeats, sizes and edge values). Write a deterministic test for each, run all the checks, and fix the code if any of them fail. Then re-read the task and confirm that every requirement it states still holds; fix anything that no longer does. Then give your final answer."
M.headsup = "- When you first try to finish, you will be asked to state the root cause you fixed, list the other situations where it could fire, and show a passing test for each. Plan so that your first answer already meets this."

-- Insert the heads-up after the real-check Finishing bullet (where it was measured), or append it to a custom prompt.
function M.with_headsup(system: string): string
	if string.find(system, M.headsup, 1, true) then
		return system
	end
	local anchor = "\n- Do not re-read a file"
	local at = string.find(system, anchor, 1, true)
	if at then
		return string.sub(system, 1, at) .. M.headsup .. string.sub(system, at)
	end
	return system .. "\n\n" .. M.headsup
end

local installed = false

function M.install(ctx)
	if installed or not M.enabled then
		return
	end
	installed = true
	ctx.hooks.on("before_model", function(req)
		if type(req.system) == "string" then
			req.system = M.with_headsup(req.system)
		end
		return req
	end)
	local asked = false
	ctx.hooks.on("before_finish", function(_)
		if asked then
			return nil
		end
		asked = true
		return { continue_with = M.message }
	end)
end

return M
