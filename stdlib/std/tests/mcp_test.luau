local mcp = require("std/mcp")

test.case("signatures from a JSON schema: required, optional, enums, arrays", function()
	local tool = {
		name = "search-notes",
		inputSchema = {
			type = "object",
			properties = { query = { type = "string" }, limit = { type = "integer" }, kind = { enum = { "a", "b" } }, tags = { type = "array", items = { type = "string" } } },
			required = { "query" },
		},
	}
	test.eq(mcp.sig("my server", tool), 'mcp.my_server.search_notes{kind?: "a" | "b", limit?: number, query: string, tags?: {string}}')
end)

test.case("a tool without arguments", function()
	test.eq(mcp.sig("s", { name = "ping", inputSchema = { type = "object" } }), "mcp.s.ping{}")
	test.eq(mcp.sig("s", { name = "ping" }), "mcp.s.ping{}")
end)

test.case("nested objects are shown one level deep, then as table", function()
	local tool = { name = "x", inputSchema = { type = "object", properties = { opts = { type = "object", properties = { deep = { type = "object", properties = { z = { type = "string" } } } } } } } }
	test.eq(mcp.sig("s", tool), "mcp.s.x{opts?: { deep?: table }}")
end)

test.case("results: one text block becomes a string", function()
	test.eq(mcp.unwrap({ content = { { type = "text", text = "hi" } } }, "s.t"), "hi")
	test.eq(mcp.unwrap({ content = { { type = "text", text = "a" }, { type = "text", text = "b" } } }, "s.t"), "a\nb")
	test.eq(mcp.unwrap({ content = {}, structuredContent = { n = 1 } }, "s.t").n, 1)
end)

test.case("results: isError raises with the server's message", function()
	local ok, err = pcall(mcp.unwrap, { isError = true, content = { { type = "text", text = "boom" } } }, "s.t")
	test.eq(ok, false)
	test.ok(tostring(err):find("mcp tool error (s.t): boom", 1, true), tostring(err))
end)
