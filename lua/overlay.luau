-- This project's hx loop. It starts from the loop built into hx and changes only what this project needs, so
-- the rest keeps up with hx. host.default_loop holds the built-in loop's source: read it to see what a part
-- does before replacing it. If this file raises while loading, or returns no table with a `main` function, hx
-- says why and runs the built-in loop.

local M = host.load(host.default_loop, "=loop")()

-- The model, and the other settings at the top of the built-in loop. Reading HX_MODEL first keeps
-- `hx --model NAME` and the variable in charge:
-- M.model = host.env("HX_MODEL") or "deepseek-v4-pro"

-- A tool of this project's own. run(args) returns the text the model sees; raising reports an error to it.
-- table.insert(M.tools, {
-- 	name = "check",
-- 	description = "Run the project's tests. Returns their output and exit status.",
-- 	parameters = { type = "object", properties = { timeout = { type = "number", description = "seconds" } } },
-- 	run = function(args)
-- 		local result = host.sh("make check", { timeout = args.timeout or 600 })
-- 		return result.out .. "[exit " .. tostring(result.code) .. "]"
-- 	end,
-- })

-- A part replaced, here by wrapping it. The others that can be replaced are M.request, M.turn and M.compact.
-- local built_in_prompt = M.system_prompt
-- function M.system_prompt()
-- 	return built_in_prompt() .. "\n\nRun `make check` before you report."
-- end

return M
