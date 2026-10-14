-- Arm "checkpoint5": the measured std/checkpoint text word for word, plus two sentences against the harms seen on
-- 2026-10-14 when the checkpoint ran on build tasks (react-lead-form 0/2 vs base 2/2, interleaved-vigenere 0/1):
-- (1) situation tests left in the project's own suite that only pass on this machine (process.chdir in vitest workers,
-- an "unwritable" path that root can write), and the grader runs that suite in a clean container as root;
-- (2) last-minute edge-case edits that broke the main path (empty output). Same moment and delivery as std/checkpoint.
-- Generic wording; no task hints.
local hooks = require("std/hooks")

local CHECK = "Before this answer is accepted: state the root cause you fixed in one sentence, then list the other "
	.. "situations where that same cause could fire beyond the reported one (other inputs, orderings, repeats, sizes and "
	.. "edge values). Write a deterministic test for each, run all the checks, and fix the code if any of them fail. "
	.. "Keep a new test in the project only if the task asks for tests; otherwise run it from tmpdir(). Any test you keep "
	.. "must pass on a clean machine, as any user (including root), from any working directory. "
	.. "Then re-read the task and confirm that every requirement it states still holds; fix anything that no longer does. "
	.. "If you changed code during this step, run the task's own example or main command again and confirm its output is "
	.. "still complete and correct. Then give your final answer."

local asked = false
hooks.on("before_finish", function(_)
	if asked then
		return nil
	end
	asked = true
	hx.log("### NUDGE\n" .. CHECK)
	return { continue_with = CHECK }
end)
