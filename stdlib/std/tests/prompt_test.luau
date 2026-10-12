local prompt = require("std/prompt")

local base = "# Sandbox\n- `/tmp`, your home directory, and the network are not available. `sh` has a minimal environment.\n"

test.case("the sandbox line follows the network policy and MCP", function()
	test.eq(prompt.sandbox(base, true, false), "# Sandbox\n- `/tmp` and your home directory are not available. `sh` has network access. `sh` has a minimal environment.\n")
	test.eq(prompt.sandbox(base, false, false), "# Sandbox\n- `/tmp`, your home directory, and the network are not available to `sh`. `sh` has a minimal environment.\n")
	test.ok(prompt.sandbox(base, false, true):find("not available to `sh`. The MCP tools listed below work.", 1, true) ~= nil)
	test.ok(prompt.sandbox(base, true, true):find("network access. The MCP tools listed below work.", 1, true) ~= nil)
	test.eq(prompt.sandbox("no such line", true, true), "no such line") -- an overridden system.md is left alone
end)
