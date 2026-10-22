-- Turn the result of running a chunk into the text the model sees.
local printer = require("std/printer")

local M = {}

-- Offending source line for `chunk:N:` errors plus hints for known Luau traps.
function M.context(code: string, msg: string): string
	local out = ""
	local n = tonumber(msg:match("chunk:(%d+):"))
	if n then
		local i = 0
		for line in (code .. "\n"):gmatch("(.-)\n") do
			i += 1
			if i == n then
				out ..= "  at line " .. n .. ": " .. line:gsub("%s+$", ""):sub(1, 160) .. "\n"
				break
			end
		end
	end
	if msg:find("syntax error", 1, true) and code:find("[[", 1, true) then
		out ..= '  hint: if a [[ ]] string holds text containing ]] (e.g. code like d[x["k"]]), use [==[ ... ]==] instead\n'
	end
	return out
end

-- One budget for the whole result: the error (itself capped) is kept whole, printed output and values share the rest.
function M.compose(res, code: string): string
	local err = ""
	if res.error then
		err = printer.cut("error: " .. res.error .. "\n" .. M.context(code, res.error), printer.OUTPUT_CAP // 3, "error")
	end
	local text = res.output
	if #res.values > 0 then
		local shown = {}
		for _, v in res.values do
			table.insert(shown, printer.top(v))
		end
		text ..= "=> " .. table.concat(shown, "\t") .. "\n"
	end
	text = printer.cap(text, printer.OUTPUT_CAP - (utf8.len(err) or #err)) .. err
	if text:match("^%s*$") then
		text = "(no output)\n"
	end
	return text
end

return M
