-- Recheck pack: before the first final answer is accepted, send the model back once to test the cases its fix implies.
-- Optional pack (installed by std/loop; enable with `require("std/recheck").enabled = true` in .harness/init.luau); it
-- costs extra turns on every run. The wording below was tuned against measured runs, so change it with care.
-- The pack also puts a heads-up in the system prompt's Finishing section, so the first answer is planned to meet the
-- check instead of being sent back.
local M = {}

M.enabled = false
M.message = "Before this answer is accepted: state the root cause you fixed in one sentence, then list the other situations where that same cause could fire beyond the reported one (other inputs, orderings, repeats, sizes and edge values). Write a deterministic test for each, run all the checks, and fix the code if any of them fail. Then re-read the task and confirm that every requirement it states still holds; fix anything that no longer does. Then give your final answer."
M.headsup = "- When you first try to finish, you will be asked to state the root cause you fixed, list the other situations where it could fire, and show a passing test for each. Plan so that your first answer already meets this."

return M
