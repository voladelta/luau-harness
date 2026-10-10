-- Prompt assembly: the system prompt, the first user message, and the nudges the loop sends.
local M = {}

function M.system(): string
	return hx.asset("std/system.md")
end

-- Task plus a file listing so the model need not spend a turn on `ls`.
function M.task_message(task: string): string
	return task .. "\n\n[files in the working directory]\n" .. hx.tree()
end

M.nudge_cut = "Your last reply was cut off or empty, so nothing ran. Reply again with ONE complete ```luau block (keep it shorter: split big edits across turns), or a plain-text final answer if you are done."
M.nudge_no_block = "No ```luau block was found in your reply, so nothing ran. Reply with ONE ```luau block that starts the work."

return M
