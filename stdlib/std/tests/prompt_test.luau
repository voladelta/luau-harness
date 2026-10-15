local prompt = require("std/prompt")

test.case("runtime access is a named section independent of custom prose", function()
	local custom = "My custom instructions."
	local text = prompt.sandbox(custom, true, false)
	test.ok(text:sub(1, #custom) == custom)
	test.ok(text:find("# Runtime access", 1, true) ~= nil)
	test.ok(text:find("sh` has network access", 1, true) ~= nil)
	local offline = prompt.sandbox(custom, false, true)
	test.ok(offline:find("not available to `sh`", 1, true) ~= nil)
	test.ok(offline:find("MCP tools listed below work", 1, true) ~= nil)
end)
