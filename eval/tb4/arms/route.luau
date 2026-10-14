-- Arm "route": pick the finish check by task type, as pstack's poteto-mode routes a task to a playbook first.
-- A task that comes with a reproducible failure (crash report, reproducer, incident, regression, traceback) gets the
-- measured std/checkpoint text word for word (mvcc: 9/10 vs base 0/10). Any other task gets a light "prove it works"
-- check instead, because the root-cause checkpoint hurt build tasks (react-lead-form 1/6 vs base 6/6: fragile tests left
-- in the project's suite, late edits that broke the main path). Same moment and delivery: one user turn before the
-- first final answer. Generic wording; no task hints.
local hooks = require("std/hooks")
local checkpoint = require("std/checkpoint")

local LIGHT = "Before this answer is accepted: run the task's own example or main command on the real inputs and "
	.. "confirm that every output the task asks for is present and correct. Do not add tests to the project or change "
	.. "code that already works unless that check fails. Then give your final answer."

local DEFECT = { "crash", "reproduc", "incident", "regression", "root cause", "traceback", "stack trace" }

local function text_of(content: any): string
	if type(content) == "string" then
		return content
	end
	local parts = {}
	if type(content) == "table" then
		for _, p in content do
			if type(p) == "table" and type(p.text) == "string" then
				table.insert(parts, p.text)
			end
		end
	end
	return table.concat(parts, "\n")
end

local kind = nil
hooks.on("before_model", function(req)
	if kind == nil and req.messages and req.messages[1] then
		local task = string.lower(text_of(req.messages[1].content))
		kind = "build"
		for _, w in DEFECT do
			if string.find(task, w, 1, true) then
				kind = "defect"
				break
			end
		end
		hx.log("### ROUTE " .. kind)
	end
	return nil
end)

local asked = false
hooks.on("before_finish", function(_)
	if asked then
		return nil
	end
	asked = true
	local msg = if kind == "defect" then checkpoint.message else LIGHT
	hx.log("### NUDGE (" .. tostring(kind) .. ")\n" .. msg)
	return { continue_with = msg }
end)
