-- Hook registry. A hook may transform a payload (return the new payload) or deny (return { deny = "reason" }).
-- Hooks never approve anything: approval belongs to the Rust gate, which no Luau code can reach.
local M = {}
M.state = {} -- controller supplies this serialized store on every transition; use it instead of captured counters

local handlers: { [string]: { (any) -> any } } = {}
local observers = {}
local function clone(value, seen, depth)
	if type(value) ~= "table" then return value end
	seen = seen or {}
	depth = depth or 0
	assert(depth <= 64, "observer data nesting exceeds 64 levels")
	if seen[value] then return seen[value] end
	local copy = {}
	seen[value] = copy
	for key, item in value do copy[key] = clone(item, seen, depth + 1) end
	return copy
end

-- Events fired by std/loop:
--   before_model { system, messages }  -> transformed request
--   after_model  { text, finish, usage = { input, cache_read, cache_write, output } } -> transformed reply
--   model_error  { error, messages }   -> { retry = true } asks for the same turn again (once per turn; messages may be
--                                         changed in place first). Fired when the model call fails, except budget stops.
--   before_finish { action, text, turn } -> { action = "continue", message = "..." } turns the final answer back into a turn: the
--                                         message is sent as the next user turn and the loop goes on
--   before_run   { code }              -> transformed code, or { deny = "why" }
--   after_run    { output, values, error, ops } -> transformed result of the chunk
function M.on(event: string, fn: (any) -> any)
	handlers[event] = handlers[event] or {}
	table.insert(handlers[event], fn)
end

function M.observe(event: string, fn: (any) -> ())
	observers[event] = observers[event] or {}
	table.insert(observers[event], fn)
end

function M.clear(event: string?)
	if event then
		handlers[event] = nil
	else
		handlers = {}
		observers = {}
	end
end

local function validate(event, out)
	if event == "before_finish" then
		assert(type(out) == "table" and (out.action == "accept" or (out.action == "continue" and type(out.message) == "string")), "before_finish must return {action='accept'} or {action='continue', message=...}")
	elseif event == "before_model" then
		assert(type(out) == "table" and type(out.system) == "string" and type(out.messages) == "table", "before_model must return a request")
	elseif event == "before_run" then
		assert(type(out) == "table" and (type(out.code) == "string" or type(out.deny) == "string"), "before_run must return {code=...} or {deny=...}")
	end
end

-- Validate effective payloads, including in-place mutations with no returned replacement.
function M.emit(event: string, payload: any): any
	for _, fn in handlers[event] or {} do
		local ok, out = pcall(fn, payload)
		if not ok then
			error("control hook failed in " .. event .. ": " .. tostring(out), 0)
		elseif out ~= nil then
			payload = out
		end
		validate(event, payload)
		if event == "before_run" and payload.deny then return payload end
	end
	validate(event, payload)
	for _, fn in observers[event] or {} do
		local ok, err = pcall(function() fn(clone(payload)) end)
		if not ok then hx.log("observer error in " .. event .. ": " .. tostring(err)) end
	end
	return payload
end

return M
