-- Arm "review": the one-shot checkpoint reworded for calculation and data-analysis answers, where the code-defect
-- wording ("state the root cause you fixed") gets dismissed (Opus 5.5 medium on foodstuff: "nothing was broken, this is a
-- calculation"). Asks for an expert's review: account for every provided input, compare results with the range a
-- practitioner expects, and weigh alternative conventions. Generic wording; no task hints.
local checkpoint = require("std/checkpoint")
checkpoint.message = "Before this answer is accepted, review it the way an expert in this field would review a colleague's"
	.. " result. First, list every quantity the inputs provide, including totals, percentages and derived columns, and say"
	.. " how each one entered your result or why it correctly does not; data prepared for a calculation is usually there"
	.. " to be used. Second, for each reported value and each key intermediate value, state the range a practitioner"
	.. " would expect for this kind of measurement or system, and whether yours falls in it. A value outside that range"
	.. " means an upstream choice is probably wrong: find it, consider the alternatives, and recompute. Third, for each"
	.. " step where the field has more than one convention, name the alternatives and pick the one that best fits the"
	.. " task wording and the structure of the data. Fix anything this review finds, then re-read the task, confirm every"
	.. " requirement and the exact output format, and give your final answer."
checkpoint.headsup = "- When you first try to finish, you will be asked to account for every provided input, compare each"
	.. " result with the range a domain expert would expect, and justify each method choice against its alternatives."
	.. " Plan so that your first answer already meets this."

return {}
