-- Arm "plan3": the plan nudge revised from the rpi planning guides and writing-for-agents. Leading words: requirement,
-- check, assumption, key decision. Same trigger as "plan" (once, at the first file change); only the text differs.
-- Generic wording; no task hints.
local lever = require("arms/lever")
lever.PLAN = "\n[harness] Before you change files, write a short plan:\n"
	.. "1. Requirements: list each requirement that the task states. For each one, write the check that will show it "
	.. "is met. The check must fail on a wrong result, not only on a crash.\n"
	.. "2. Assumptions: list separately each thing you assume that the task does not state.\n"
	.. "3. Key decision: find the one decision that the other work depends on. Write two or three ways to decide it, "
	.. "choose one, and give the reason.\n"
	.. "Then change the code for one requirement, run its check, and continue with the next requirement."
lever.install({ plan = true })
