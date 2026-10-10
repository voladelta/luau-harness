-- Hook registry. A hook may transform a payload (return the new payload) or deny (return { deny = "reason" }).
-- Hooks never approve anything: approval belongs to the core gate (P1), which no Luau code can reach.
local M = {}

local handlers: { [string]: { (any) -> any } } = {}

-- Events fired by std/loop:
--   before_model { system, messages }  -> transformed request
--   after_model  { text, finish }      -> transformed reply
--   before_run   { code }              -> transformed code, or { deny = "why" }
--   after_run    { result }            -> transformed result
function M.on(event: string, fn: (any) -> any)
	handlers[event] = handlers[event] or {}
	table.insert(handlers[event], fn)
end

function M.clear(event: string?)
	if event then
		handlers[event] = nil
	else
		handlers = {}
	end
end

-- Run every handler in registration order. Stops at the first deny.
function M.emit(event: string, payload: any): any
	for _, fn in handlers[event] or {} do
		local ok, out = pcall(fn, payload)
		if not ok then
			hx.log("hook error in " .. event .. ": " .. tostring(out))
		elseif type(out) == "table" and out.deny then
			return out
		elseif out ~= nil then
			payload = out
		end
	end
	return payload
end

return M
