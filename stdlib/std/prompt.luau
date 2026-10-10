-- Prompt assembly: the system prompt, the tool the model calls, the first user message, and the nudges the loop sends.
local M = {}

function M.system(): string
	return hx.asset("std/system.md")
end

-- The one tool the model has. Everything else (read, write, edit, sh, search, ...) is a function inside the Luau it sends.
M.tools = {
	{
		name = "luau",
		description = "Run Luau code in the persistent session. Returns what the code printed, the value of the last expression (after =>) and any error with traceback. Global variables persist across calls.",
		parameters = {
			type = "object",
			properties = { code = { type = "string", description = "The Luau source to run." } },
			required = { "code" },
		},
	},
}

-- Task plus a file listing so the model need not spend a turn on `ls`.
function M.task_message(task: string): string
	return task .. "\n\n[files in the working directory]\n" .. hx.tree()
end

M.nudge_cut = "Your last reply was cut off or empty, so nothing ran. Call the luau tool again with shorter code (split big edits across calls), or reply in plain text if you are done."
M.nudge_no_call = "You have not run any code yet. Call the luau tool to start the work."
M.cut_call = "error: your tool call was cut off before the code was complete, so nothing ran. Send shorter code (split big edits across calls)."

return M
