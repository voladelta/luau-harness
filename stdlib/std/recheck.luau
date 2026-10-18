-- Recheck pack: before the first final answer is accepted, send the model back once to test the cases its fix implies.
-- Optional pack (installed by std/loop; enable with `require("std/recheck").enabled = true` in .harness/init.luau); it
-- costs extra turns on every run. The wording below was tuned against measured runs, so change it with care.
-- The pack also puts a heads-up in the system prompt's Finishing section, so the first answer is planned to meet the
-- check instead of being sent back.
local M = {}

M.enabled = false
M.message = "Before this answer is accepted: state the root cause you fixed in one sentence, and confirm that your solution strictly obeys the task contract, prompt, and plan/design. Do NOT explore or add unwanted features, speculative extras, or unrequested refactors. Do NOT fix unrelated bugs. Verify that all required checks pass and every stated requirement holds. Then give your final answer."
M.headsup = "- When you first try to finish, you will be asked to state the root cause you fixed and confirm that you strictly adhered to the task contract and design without adding unwanted features or wandering out of scope."

return M
