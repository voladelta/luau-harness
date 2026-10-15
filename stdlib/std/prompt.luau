-- Prompt assembly: system sections, the model tool schema, and controller nudges.
local M = {}

-- The sandbox section must match the run: with network on, say so, and never tell the model the listed MCP tools are
-- unreachable (they run in the harness, outside the sandbox, either way).
function M.sandbox(text: string, net: boolean, has_mcp: boolean): string
	local line = if net then "`/tmp` and your home directory are not available. `sh` has network access."
		else "`/tmp`, your home directory, and the network are not available to `sh`."
	if has_mcp then
		line ..= " The MCP tools listed below work."
	end
	return text .. "\n\n# Runtime access\n" .. line
end

function M.system(): string
	local sections = { M.sandbox(hx.asset("std/system.md"), hx.net, #hx.mcp_servers() > 0) }
	local checkpoint = require("std/checkpoint")
	if checkpoint.enabled then table.insert(sections, "# Finish review\n" .. checkpoint.headsup) end
	if hx.prompt_contributions and hx.prompt_contributions ~= "" then table.insert(sections, hx.prompt_contributions) end
	return table.concat(sections, "\n\n")
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

M.nudge_cut = "Your last reply was cut off or empty, so nothing ran. Call the luau tool again with shorter code (split big edits across calls), or reply in plain text if you are done."
M.nudge_no_call = "You have not run any code yet. Call the luau tool to start the work."
M.cut_call = "error: your tool call was cut off before the code was complete, so nothing ran. Send shorter code (split big edits across calls)."

return M
