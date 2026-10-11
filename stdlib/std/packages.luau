-- Surfaces what the USER installed (approved tool and skill packages) to the model; costs nothing when there are none.
--   tools.NAME.fn(...)     tool packages (kind = "tool"), loaded on first use
--   skill("NAME")          -> { name, text, module, dir, files }: a skill's SKILL.md (frontmatter stripped), its Luau module
--                          if any, and where its other files are (agent skills from .agents/skills or ~/.agents/skills too)
--   skills(query?)         -> { {name, description} }: search approved skills by name and description (best first).
--                          Skills are NOT listed in the prompt: the model searches or loads one when the user names it.
--   deftool / promote      (the std/authoring pack) agent-written tools: usable at once with only the capabilities they
--                          declare; activated for later sessions only when the user reviews them and runs `hx accept`
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

-- Approved skills matching `query` (words in name and description; a name hit counts more), best first.
-- An empty query lists every approved skill by name.
local function search_skills(query: string, limit: number)
	local words = {}
	for w in query:lower():gmatch("[%w]+") do
		if #w > 1 then
			table.insert(words, w)
		end
	end
	local hits = {}
	for _, p in hx.packages() do
		if p.kind == "skill" then
			local name, desc = p.name:lower(), (p.description or ""):lower()
			local score = if #words == 0 then 1 else 0
			for _, w in words do
				if name:find(w, 1, true) or (#w - #name <= 2 and w:find(name, 1, true)) then -- "crafts" -> craft, not "videos" -> vid
					score += 3
				end
				if desc:find(w, 1, true) then
					score += 1
				end
			end
			if score > 0 then
				local d = p.description or ""
				table.insert(hits, { name = p.name, description = if #d > 240 then d:sub(1, 240) .. "…" else d, score = score })
			end
		end
	end
	table.sort(hits, function(a, b)
		return if a.score == b.score then a.name < b.name else a.score > b.score
	end)
	local out = {}
	for i = 1, math.min(limit, #hits) do
		table.insert(out, { name = hits[i].name, description = hits[i].description })
	end
	return out
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

	hx.expose("skills", function(query: string?)
		return search_skills(query or "", 8)
	end)

	hx.expose("skill", function(name: string)
		name = name:gsub("^/", "") -- "/fframes-video" as the user typed it
		local p = find(name)
		if not p or p.kind ~= "skill" then
			local near = {}
			for _, h in search_skills(name, 3) do
				table.insert(near, h.name)
			end
			error("no installed skill named " .. string.format("%q", name) .. (if #near > 0 then "; closest: " .. table.concat(near, ", ") else ""), 2)
		end
		local text = strip_frontmatter(read(p.dir .. "/SKILL.md"))
		local ok, mod = pcall(require, name)
		local module = if ok then mod else nil
		if not ok and not tostring(mod):find("module not found", 1, true) then
			-- the skill ships a module that does not load: keep the instructions usable and raise the load error where the
			-- module is used, instead of a bare nil that hides why
			local why = "skill " .. string.format("%q", name) .. ": its Luau module failed to load: " .. tostring(mod)
			module = setmetatable({}, {
				__index = function(_, k)
					if type(k) == "string" then
						error(why, 2)
					end
					return nil
				end,
			})
		end
		return { name = name, text = text, module = module, dir = p.dir, files = p.files }
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
		local tool_lines, n_skills = {}, 0
		for _, p in hx.packages() do
			if p.kind == "tool" then
				table.insert(tool_lines, "- tools." .. (if p.signature ~= "" then p.signature else p.name) .. (if p.doc ~= "" then "  -- " .. p.doc else ""))
			elseif p.kind == "skill" then
				n_skills += 1
			end
		end
		if #tool_lines + n_skills == 0 then
			return nil -- nothing installed: the prompt is unchanged
		end
		local out = { "# Installed extensions (set up by the user)" }
		if #tool_lines > 0 then
			table.insert(out, "Tools, already available:")
			for _, l in tool_lines do
				table.insert(out, l)
			end
		end
		if n_skills > 0 then
			-- no catalog: listing every skill costs tokens on every turn and tempts the model into irrelevant ones
			table.insert(out, 'Skills (instructions the user installed): when the user names one (e.g. "/fframes-video"), load it with skill("name") -> { text, dir, files }; '
				.. 'to check whether one fits, skills("words") searches names and descriptions. Paths in text are relative to dir: read(dir .. "/" .. file).')
		end
		req.system = req.system .. "\n\n" .. table.concat(out, "\n")
		return req
	end)
end

return M
