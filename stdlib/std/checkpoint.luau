-- Checkpoint pack: before the first final answer is accepted, send the model back once to test the cases its fix implies.
-- Opt-in (`--pack std/checkpoint`, or `[packs] enable = ["std/checkpoint"]` in .harness/policy.toml); it costs extra
-- turns on every run. On Terminal-Bench 4.0 mvcc-lsm-compaction (Sonnet 5.5, medium) it lifted the solved rate from
-- 1/20 to 13/20; the wording below is the one that was measured, so change it only with a new measurement.
local M = {}

M.enabled = true
M.message = "Before this answer is accepted: state the root cause you fixed in one sentence, then list the other situations where that same cause could fire beyond the reported one (other inputs, orderings, repeats, sizes and edge values). Write a deterministic test for each, run all the checks, and fix the code if any of them fail. Then re-read the task and confirm that every requirement it states still holds; fix anything that no longer does. Then give your final answer."

local installed = false

function M.install(ctx)
	if installed or not M.enabled then
		return
	end
	installed = true
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
