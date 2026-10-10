-- Opt-in pack (--pack std/authoring): lets the agent extend the harness, always through human approval.
local M = {}

local DOCS = [==[
# Extending yourself (optional)
- deftool(name, source, { doc, signature, caps = { fs = {"read"}, proc = {"sh"} }, tests })  -> module
  `source` is Luau that returns a table of functions. It runs with ONLY the capabilities you list in `caps` (default none)
  and is usable at once as tools.NAME. `tests` is a Luau test file: local m = require("NAME"); test.case("x", function() test.eq(m.f(1), 2) end).
- promote(name) -> { ok, message, output }   stages the tool and runs its tests. It does NOT activate it: the user must review and run `hx accept NAME`.
- propose_module("std/printer", source, { reason })  stages a replacement for a harness module. It must pass an eval gate and the user must run `hx upgrade accept`.
You cannot approve, install or activate anything yourself.
]==]

function M.install(ctx)
	ctx.hooks.on("before_model", function(req)
		req.system = req.system .. "\n\n" .. DOCS
		return req
	end)
end

return M
