-- The agent loop (L2). The model has one tool, luau(code): the harness runs each call in the persistent session and returns
-- what it printed. A reply with no tool call is the final answer. Replace this module per project
-- (.harness/override/std/loop.luau) to change the loop.
local feedback = require("std/feedback")
local hooks = require("std/hooks")
local prompt = require("std/prompt")

local M = {}

-- cfg: { task: string, max_turns: number }
-- returns a report: { turns, code_turns, luau_errors, truncated_turns, ops_per_turn, finished, final_answer }
function M.run(cfg)
	local ok, err = pcall(require, "project/init")
	if not ok and not tostring(err):find("no project init", 1, true) then
		hx.log("project init failed: " .. tostring(err)) -- a broken init must not stop the run
	end
	require("std/packages").install({ hooks = hooks })
	require("std/mcp").install({ hooks = hooks })
	for _, name in cfg.packs or {} do
		local pack = require(name)
		pack.install({ hooks = hooks })
	end
	local rep = { turns = 0, code_turns = 0, luau_errors = 0, truncated_turns = 0, ops_per_turn = {}, finished = false, final_answer = "" }
	local messages = { { role = "user", content = prompt.task_message(cfg.task) } }
	hx.log("### TASK\n" .. messages[1].content)
	local bad, no_call = 0, 0

	while rep.turns < cfg.max_turns do
		local req = hooks.emit("before_model", { system = prompt.system(), messages = messages, tools = prompt.tools })
		local called, resp = pcall(hx.llm, req)
		if not called then
			if tostring(resp):find("budget_exceeded", 1, true) then
				rep.stopped = tostring(resp):match("budget_exceeded:[^\n]*") or "budget_exceeded"
				hx.log("### STOPPED\n" .. rep.stopped)
				break
			end
			error(resp, 0)
		end
		rep.turns += 1
		local reply = hooks.emit("after_model", { text = resp.text or "", finish = resp.finish })
		local text, calls = reply.text, resp.calls
		local summary = text
		for _, c in calls do
			summary ..= "\n[tool call " .. c.name .. "]\n" .. (if type(c.input) == "table" and type(c.input.code) == "string" then c.input.code else tostring(c.error))
		end
		hx.log("### MODEL (turn " .. rep.turns .. ")\n" .. summary)
		local cut = resp.finish == "length"

		if #calls == 0 then
			if text:match("^%s*$") or cut then
				-- A truncated or empty reply is not a final answer: ask again instead of ending the run.
				rep.truncated_turns += 1
				bad += 1
				if bad > 3 then
					break
				end
				-- keep only a preview of a cut-off reply: re-sending a runaway (often a repetition loop) costs tokens and can
				-- push the model into repeating itself
				local preview = if text:match("^%s*$") then "(empty reply)" elseif #text > 1500 then text:sub(1, 1500) .. "\n…[reply cut off here]" else text
				table.insert(messages, { role = "assistant", content = preview })
				hx.log("### HARNESS\n" .. prompt.nudge_cut)
				table.insert(messages, { role = "user", content = prompt.nudge_cut })
				continue
			end
			if rep.code_turns == 0 and no_call < 2 then
				-- A "final answer" before any code ran: push the model to start working.
				no_call += 1
				rep.truncated_turns += 1
				table.insert(messages, { role = "assistant", content = text, raw = resp.raw })
				hx.log("### HARNESS\n" .. prompt.nudge_no_call)
				table.insert(messages, { role = "user", content = prompt.nudge_no_call })
				continue
			end
			rep.finished = true
			rep.final_answer = text
			break
		end
		bad = 0

		table.insert(messages, { role = "assistant", content = text, calls = calls, raw = resp.raw })
		-- every call id gets exactly one result, in order
		for _, c in calls do
			local code = if type(c.input) == "table" and type(c.input.code) == "string" then c.input.code else nil
			local fb
			if c.name ~= "luau" then
				fb = "error: unknown tool " .. tostring(c.name) .. "; the only tool is luau(code)."
				table.insert(rep.ops_per_turn, 0)
			elseif code == nil then
				rep.truncated_turns += 1
				fb = if cut then prompt.cut_call else "error: the luau tool needs a string argument `code` (" .. tostring(c.error or "missing") .. ")."
				table.insert(rep.ops_per_turn, 0)
			else
				local pre = hooks.emit("before_run", { code = code })
				if pre.deny then
					fb = "Blocked by a project hook: " .. tostring(pre.deny) .. "\n"
					table.insert(rep.ops_per_turn, 0)
				else
					local res = hx.run(pre.code)
					res = hooks.emit("after_run", res)
					rep.code_turns += 1
					if res.error then
						rep.luau_errors += 1
					end
					table.insert(rep.ops_per_turn, res.ops)
					fb = feedback.compose(res, pre.code)
				end
			end
			hx.log("### RESULT\n" .. fb)
			table.insert(messages, { role = "tool", id = c.id, content = fb })
		end
	end
	return rep
end

return M
