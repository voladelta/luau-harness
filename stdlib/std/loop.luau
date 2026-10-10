-- The agent loop (L2). The model replies with one ```luau block; the harness runs it in the persistent session and
-- feeds the printed output back. Replace this module per project (.harness/override/std/loop.luau) to change the loop.
local extract = require("std/extract")
local feedback = require("std/feedback")
local hooks = require("std/hooks")
local prompt = require("std/prompt")

local M = {}

local function odd_fences(text: string): boolean
	local _, n = text:gsub("```", "")
	return n % 2 == 1
end

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
	local bad, no_block = 0, 0

	while rep.turns < cfg.max_turns do
		local req = hooks.emit("before_model", { system = prompt.system(), messages = messages })
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
		local text = reply.text
		hx.log("### MODEL (turn " .. rep.turns .. ")\n" .. text)

		local code, multi = extract.code(text)
		if text:match("^%s*$") or resp.finish == "length" or (code == nil and odd_fences(text)) then
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
		bad = 0

		if code == nil then
			if (rep.code_turns == 0 or text:find("<invoke", 1, true) or text:find("</invoke", 1, true) or text:find("<parameter", 1, true) or text:find("</parameter", 1, true) or text:find("```luau", 1, true)) and no_block < 2 then
				-- A "final answer" before any code ran, or one that still carries unextractable code markup, is a malformed reply: nudge.
				no_block += 1
				rep.truncated_turns += 1
				table.insert(messages, { role = "assistant", content = text })
				hx.log("### HARNESS\n" .. prompt.nudge_no_block)
				table.insert(messages, { role = "user", content = prompt.nudge_no_block })
				continue
			end
			rep.finished = true
			rep.final_answer = text
			break
		end

		table.insert(messages, { role = "assistant", content = text })
		local pre = hooks.emit("before_run", { code = code })
		local fb
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
			fb = feedback.compose(res, pre.code, multi)
		end
		hx.log("### RESULT\n" .. fb)
		table.insert(messages, { role = "user", content = fb })
	end
	return rep
end

return M
