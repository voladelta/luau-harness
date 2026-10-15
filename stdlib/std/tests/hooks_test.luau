local hooks = require("std/hooks")

test.case("handlers transform the payload in registration order", function()
	hooks.clear()
	hooks.on("e", function(p) return p .. "a" end)
	hooks.on("e", function(p) return p .. "b" end)
	test.eq(hooks.emit("e", ""), "ab")
end)

test.case("a deny stops the chain and is returned", function()
	hooks.clear()
	local ran = false
	hooks.on("before_run", function() return { deny = "no" } end)
	hooks.on("before_run", function() ran = true end)
	test.eq(hooks.emit("before_run", {code="print(1)"}).deny, "no")
	test.ok(not ran, "later handler must not run after a deny")
end)

test.case("finish contracts validate returned and in-place changes", function()
	for _, handler in {
		function() return {deny="refused"} end,
		function(p) p.action=nil; p.deny="refused" end,
	} do
		hooks.clear()
		hooks.on("before_finish", handler)
		test.ok(not pcall(hooks.emit, "before_finish", {action="accept"}))
	end
end)

test.case("a failing control handler stops the transition", function()
	hooks.clear()
	local ran = false
	hooks.on("e", function() error("boom") end)
	hooks.on("e", function(p) ran = true; return p + 1 end)
	local ok, err = pcall(hooks.emit, "e", 1)
	test.ok(not ok and tostring(err):find("control hook failed", 1, true) ~= nil)
	test.ok(not ran)
end)

test.case("observer failures do not stop control flow or mutate its result", function()
	hooks.clear()
	hooks.observe("e", function(p) p.value = 99; error("optional observer") end)
	test.eq(hooks.emit("e", { value = 1 }).value, 1)
end)

test.case("clear(event) drops only that event", function()
	hooks.clear()
	hooks.on("a", function() return 1 end)
	hooks.on("b", function() return 2 end)
	hooks.clear("a")
	test.eq(hooks.emit("a", 0), 0)
	test.eq(hooks.emit("b", 0), 2)
end)
