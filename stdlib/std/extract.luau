-- Pull the Luau chunk out of a model reply.
-- Fences follow CommonMark: an opening line of 3+ backticks (+ info string) closes at a line made only of at least
-- as many backticks, so a ``` inside a line of code does not end the block. Also accepts the tool-call markup some
-- models leak (DeepSeek's <｜DSML｜ parameter name="code"> ...) and an unclosed fence whose tail is such markup.
local M = {}

local DSML = "｜DSML｜"

-- returns array of { info = string, body = string, closed = boolean }
function M.fences(text: string)
	local out = {}
	local open = nil
	for line in (text .. "\n"):gmatch("(.-)\n") do
		local trimmed = line:match("^%s*(.-)%s*$") :: string
		local ticks = #(trimmed:match("^`+") or "")
		if open == nil then
			if ticks >= 3 and not trimmed:sub(ticks + 1):find("`", 1, true) then
				open = { n = ticks, info = trimmed:sub(ticks + 1):match("^%s*(.-)%s*$"), body = {} }
			end
		elseif ticks >= open.n and #trimmed == ticks then
			table.insert(out, { info = open.info, body = table.concat(open.body, "\n"), closed = true })
			open = nil
		else
			table.insert(open.body, line)
		end
	end
	if open then
		table.insert(out, { info = open.info, body = table.concat(open.body, "\n"), closed = false })
	end
	return out
end

local function isluau(info: string): boolean
	return info == "luau" or info == "lua"
end

-- returns code, multiple  (code == nil when the reply holds no runnable block)
function M.code(text: string): (string?, boolean)
	local all = M.fences(text)
	local closed = {}
	for _, f in all do
		if f.closed and isluau(f.info) then
			table.insert(closed, f.body)
		end
	end
	if #closed > 0 then
		return closed[1], #closed > 1
	end
	local i = text:find(DSML .. " parameter name=", 1, true)
	if i then
		local gt = text:find(">", i, true)
		if gt then
			local body = text:sub(gt + 1)
			local e = body:find("</" .. DSML .. " parameter>", 1, true)
			local code = (if e then body:sub(1, e - 1) else body):match("^%s*(.-)%s*$") :: string
			if code ~= "" then
				return code, false
			end
		end
	end
	-- Claude-style function-call markup: invoke name="luau" with a parameter name="code" element (any namespace prefix).
	-- Needs the closing tag: a cut-off call is not runnable.
	for name, body in text:gmatch('<[%w:]*parameter name="(%w+)"[^>]*>(.-)</[%w:]*parameter>') do
		if name == "code" or name == "command" or name == "script" or name == "luau" then
			local code = body:match("^%s*(.-)%s*$") :: string
			if code ~= "" then
				return code, false
			end
		end
	end
	for _, f in all do
		if not f.closed and isluau(f.info) then
			local e = f.body:find("</" .. DSML, 1, true) or f.body:find("<" .. DSML, 1, true)
			if e then
				local code = f.body:sub(1, e - 1):match("^%s*(.-)%s*$") :: string
				if code ~= "" then
					return code, false
				end
			end
		end
	end
	return nil, false
end

return M
