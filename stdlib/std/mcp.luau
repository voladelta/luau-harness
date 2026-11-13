-- MCP servers (configured by the user in .harness/mcp.toml) bound as Luau functions: mcp.SERVER.TOOL{ ...args }.
-- Signatures are generated from each tool's JSON schema and shown to the model. Zero cost when no servers are configured.
local M = {}

local function safe(name: string): string
	return (name:gsub("[^%w_]", "_"))
end

-- JSON schema -> Luau-ish type text, for the prompt.
local function ty(schema, depth: number): string
	if type(schema) ~= "table" then
		return "any"
	end
	if schema.enum then
		local parts = {}
		for _, v in schema.enum do
			table.insert(parts, if type(v) == "string" then string.format("%q", v) else tostring(v))
		end
		return table.concat(parts, " | ")
	end
	local t = schema.type
	if type(t) == "table" then
		t = t[1]
	end
	if t == "string" then
		return "string"
	elseif t == "number" or t == "integer" then
		return "number"
	elseif t == "boolean" then
		return "boolean"
	elseif t == "array" then
		return "{" .. ty(schema.items, depth + 1) .. "}"
	elseif t == "object" or schema.properties then
		if depth >= 2 or not schema.properties then
			return "table"
		end
		local req = {}
		for _, k in schema.required or {} do
			req[k] = true
		end
		local keys = {}
		for k in schema.properties do
			table.insert(keys, k)
		end
		table.sort(keys)
		local parts = {}
		for _, k in keys do
			table.insert(parts, k .. (if req[k] then "" else "?") .. ": " .. ty(schema.properties[k], depth + 1))
		end
		return "{ " .. table.concat(parts, ", ") .. " }"
	end
	return "any"
end

-- At most n characters of s, cut on a character boundary (a split UTF-8 sequence would make the prompt invalid).
local function clip(s: string, n: number): string
	local cut = utf8.offset(s, n + 1)
	return if cut then s:sub(1, cut - 1) else s
end

-- How the model reaches a key: `.key` when it is an identifier, else `["key"]`.
local function path(key: string): string
	return if key:match("^[%a_][%w_]*$") then "." .. key else "[" .. string.format("%q", key) .. "]"
end

-- e.g.  mcp.demo.add{ a: number, b: number }. `key` is the binding the tool got (its safe name unless that collided).
function M.sig(server: string, tool, key: string?): string
	local args = ty(tool.inputSchema or { type = "object" }, 0)
	if args == "table" or args == "{  }" then
		args = ""
	end
	return "mcp" .. path(safe(server)) .. path(key or safe(tool.name)) .. "{" .. (args:match("^{ (.*) }$") or args) .. "}"
end

-- tools/call result -> a Luau value: text when it is one text block, otherwise the content table; errors raise.
function M.unwrap(res, label: string)
	local texts = {}
	for _, c in res.content or {} do
		if c.type == "text" then
			table.insert(texts, c.text)
		end
	end
	if res.isError then
		error("mcp tool error (" .. label .. "): " .. table.concat(texts, "\n"), 3)
	end
	if res.structuredContent ~= nil then
		return res.structuredContent
	end
	if #texts == 1 and #(res.content or {}) == 1 then
		return texts[1]
	end
	return if #texts > 0 then table.concat(texts, "\n") else res.content
end

function M.install(ctx)
	local servers = hx.mcp_servers()
	if #servers == 0 then
		return
	end
	local mcp = {}
	local lines = {}
	for _, server in servers do
		-- Names that differ only in characters `safe` replaces would overwrite each other: the first keeps the name.
		if mcp[safe(server)] ~= nil then
			hx.log("mcp server " .. server .. " skipped: its name collides with another server's as " .. safe(server))
			continue
		end
		local ok, listed = pcall(__mcp_list, server)
		if not ok then
			hx.log("mcp server " .. server .. " unavailable: " .. tostring(listed))
			continue
		end
		if listed.truncated then
			hx.log("mcp server " .. server .. " lists more tools than hx follows; only the first " .. #listed.tools .. " are bound")
		end
		local bound = {}
		local keys = {}
		for _, tool in listed.tools do
			if type(tool.name) ~= "string" or bound[tool.name] ~= nil then
				hx.log("mcp server " .. server .. ": skipped a tool with a missing or repeated name " .. tostring(tool.name))
				continue
			end
			local label = server .. "." .. tool.name
			bound[tool.name] = function(args)
				return M.unwrap(__mcp_call(server, tool.name, args or {}), label)
			end
			keys[tool] = tool.name
		end
		for _, tool in listed.tools do
			local key = keys[tool]
			local alias = if key then safe(key) else nil
			if alias and alias ~= key then
				if bound[alias] == nil then
					bound[alias] = bound[key]
					keys[tool] = alias
				else
					hx.log("mcp tool " .. server .. "." .. key .. " is reachable only as mcp." .. safe(server) .. '["' .. key .. '"]: ' .. alias .. " is another tool's name")
				end
			end
		end
		for _, tool in listed.tools do
			if keys[tool] then
				local desc = (tool.description or ""):match("^[^\n]*") :: string
				table.insert(lines, "- " .. M.sig(server, tool, keys[tool]) .. (if desc ~= "" then "  -- " .. clip(desc, 120) else ""))
			end
		end
		mcp[safe(server)] = bound
	end
	hx.expose("mcp", mcp)
	if #lines > 0 then
		ctx.hooks.on("before_model", function(req)
			req.system = req.system .. "\n\n# MCP tools (user-configured; call with one table argument, e.g. mcp.server.tool{ a = 1 })\n" .. table.concat(lines, "\n")
			return req
		end)
	end
end

return M
