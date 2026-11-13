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

-- Binding names for a set of names (tools of one server, or servers): every exact name, then each safe alias
-- (`a.b` -> `a_b`) that no exact name or earlier alias already holds. An alias never replaces a binding.
function M.names(list: { string }): { [string]: string }
	local names = {}
	for _, name in list do
		names[name] = name
	end
	for _, name in list do
		local alias = safe(name)
		if names[alias] == nil then
			names[alias] = name
		end
	end
	return names
end

-- How the prompt refers to `name` in a table bound with `names`: `.alias` when the alias is its own, else `["name"]`.
function M.key(names: { [string]: string }, name: string): string
	local alias = safe(name)
	if names[alias] == name then
		return "." .. alias
	end
	return "[" .. string.format("%q", name) .. "]"
end

-- e.g.  mcp.demo.add{ a: number, b: number }; `path` is the callable path (default: the safe aliases).
function M.sig(server: string, tool, path: string?): string
	local args = ty(tool.inputSchema or { type = "object" }, 0)
	if args == "table" or args == "{  }" then
		args = ""
	end
	local callee = path or ("mcp." .. safe(server) .. "." .. safe(tool.name))
	return callee .. "{" .. (args:match("^{ (.*) }$") or args) .. "}"
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
	local server_names = M.names(servers)
	for _, server in servers do
		local ok, tools = pcall(__mcp_list, server)
		if not ok then
			hx.log("mcp server " .. server .. " unavailable: " .. tostring(tools))
			continue
		end
		local by_name = {}
		local list = {}
		for _, tool in tools do
			if type(tool) ~= "table" or type(tool.name) ~= "string" then
				continue
			end
			if by_name[tool.name] == nil then
				table.insert(list, tool.name)
			end
			by_name[tool.name] = tool
		end
		local tool_names = M.names(list)
		local bound = {}
		for _, name in list do
			local tool = by_name[name]
			local label = server .. "." .. name
			bound[name] = function(args)
				return M.unwrap(__mcp_call(server, name, args or {}), label)
			end
			local path = "mcp" .. M.key(server_names, server) .. M.key(tool_names, name)
			local desc = (tool.description or ""):match("^[^\n]*") :: string
			table.insert(lines, "- " .. M.sig(server, tool, path) .. (if desc ~= "" then "  -- " .. desc:sub(1, 120) else ""))
		end
		for alias, name in tool_names do
			bound[alias] = bound[name]
		end
		mcp[server] = bound
	end
	for alias, server in server_names do
		mcp[alias] = mcp[server] -- nil for an unavailable server
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
