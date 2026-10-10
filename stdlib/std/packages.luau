-- Surfaces what the USER installed (approved tool and skill packages) to the model; costs nothing when there are none.
--   tools.NAME.fn(...)     tool packages (kind = "tool"), loaded on first use
--   skill("NAME")          -> { name, text, module }: a skill's SKILL.md (frontmatter stripped) and its Luau module, if any
--   deftool / promote      agent-written tools: usable at once with only the capabilities they declare; activated for
--                          later sessions only when the user reviews them and runs `hx accept`
--   refresh_extensions()   hot reload: drop cached tool modules so edited/newly accepted packages are picked up;
--                          session variables are untouched
local M = {}

local function strip_frontmatter(text: string): string
	local body = text:match("^%-%-%-\n.-\n%-%-%-\n(.*)$")
	return body or text
end

local function find(name: string)
	for _, p in hx.packages() do -- always fresh: a package accepted mid-session shows up on the next call
		if p.name == name then
			return p
		end
	end
	return nil
end

function M.install(ctx)
	local tools = setmetatable({}, {
		__index = function(t, k)
			local p = find(k)
			if p and p.kind == "tool" then
				local m = require(k)
				rawset(t, k, m)
				return m
			end
			return nil
		end,
	})
	hx.expose("tools", tools)

	hx.expose("skill", function(name: string)
		local p = find(name)
		if not p or p.kind ~= "skill" then
			error("no installed skill named " .. string.format("%q", name), 2)
		end
		local text = strip_frontmatter(read(".harness/pkgs/" .. name .. "/SKILL.md"))
		local ok, mod = pcall(require, name)
		return { name = name, text = text, module = if ok then mod else nil }
	end)

	hx.expose("refresh_extensions", function()
		local n = 0
		for _, p in hx.packages() do
			if rawget(tools, p.name) ~= nil or p.kind == "skill" then
				reload(p.name)
				rawset(tools, p.name, nil)
				n += 1
			end
		end
		return "refreshed " .. n .. " extension(s)"
	end)

	ctx.hooks.on("before_model", function(req)
		local tool_lines, skill_lines = {}, {}
		for _, p in hx.packages() do
			if p.kind == "tool" then
				table.insert(tool_lines, "- tools." .. (if p.signature ~= "" then p.signature else p.name) .. (if p.doc ~= "" then "  -- " .. p.doc else ""))
			elseif p.kind == "skill" then
				table.insert(skill_lines, "- " .. p.name .. (if p.description ~= "" then ": " .. p.description else ""))
			end
		end
		if #tool_lines + #skill_lines == 0 then
			return nil -- nothing installed: the prompt is unchanged
		end
		local out = { "# Installed extensions (set up by the user)" }
		if #tool_lines > 0 then
			table.insert(out, "Tools, already available:")
			for _, l in tool_lines do
				table.insert(out, l)
			end
		end
		if #skill_lines > 0 then
			table.insert(out, 'Skills: call skill("name") to get { text, module } (text = instructions, module = runnable Luau, if any):')
			for _, l in skill_lines do
				table.insert(out, l)
			end
		end
		req.system = req.system .. "\n\n" .. table.concat(out, "\n")
		return req
	end)
end

return M
