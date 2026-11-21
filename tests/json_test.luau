local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check = checks.check

check("scalars", json.encode({ true, false, 1, -2.5, "x" }), '[true,false,1,-2.5,"x"]')
check("a whole number has no fraction and no exponent", json.encode({ 1763386800000, 1e15, -3 }),
	"[1763386800000,1000000000000000,-3]")
for _, n in { 0, 2 ^ 53, 2 ^ 53 + 2, 1e20, 1e21, 1e-7, 0.1, 1 / 3, -2.5e-9, 5e-324, 1.7976931348623157e308 } do
	check(("%.17g comes back the same number"):format(n), json.decode(json.encode(n)), n)
end
check("sorted keys", json.encode({ b = 1, a = 2, c = { z = 1, y = 2 } }), '{"a":2,"b":1,"c":{"y":2,"z":1}}')
check("empty table is an object", json.encode({}), "{}")
check("marked empty table is an array", json.encode(json.array()), "[]")
local existing = { 1 }
check("json.array returns the supplied table", json.array(existing), existing)
existing[1] = nil
check("the supplied table stays an array when emptied", json.encode(existing), "[]")
check("null", json.encode({ json.null, 1 }), "[null,1]")
check("nil at the top", json.encode(nil), "null")
check("escapes", json.encode('a"b\\c\n\t\1'), '"a\\"b\\\\c\\n\\t\\u0001"')
check("valid utf-8 passes", json.encode("héllo 😀"), '"héllo 😀"')
check("invalid utf-8 is replaced", json.encode("a\255b\192\128c"), '"a\239\191\189b\239\191\189\239\191\189c"')
check("truncated utf-8 is replaced", json.encode("x\240\159"), '"x\239\191\189\239\191\189"')
check("surrogate bytes are replaced", json.encode("\237\160\128"), '"\239\191\189\239\191\189\239\191\189"')
check("a command's output with bytes that are no text still encodes to JSON that decodes",
	json.decode(json.encode({ content = host.sh("printf 'ok \\377\\200 \\033[1m'").out })).content, "ok \239\191\189\239\191\189 \27[1m")

local function fails(name, value, fragment)
	local ok, err = pcall(json.encode, value)
	check(name .. " raises", ok, false)
	if not ok and not tostring(err):find(fragment, 1, true) then
		check(name .. " message", err, fragment)
	end
end
fails("nan", 0 / 0, "nan or inf")
fails("function", { f = print }, "cannot encode a function")
fails("mixed table", { 1, 2, x = 3 }, "neither an array")
fails("number keys", { [1.5] = 1 }, "neither an array")
fails("sparse marked array", json.array({ [3] = 1 }), "keys 1..n")
fails("a hole and a string key", { 1, 2, nil, 4, x = "dropped" }, "neither an array")
fails("a hole and a fractional key", { 1, 2, nil, 4, [1.5] = "dropped" }, "neither an array")
fails("a marked array with a string key", json.array({ 1, 2, x = 3 }), "keys 1..n")
fails("a marked array with a hole and a string key", json.array({ 1, 2, nil, 4, x = 5 }), "keys 1..n")
check("an array built in reverse", json.encode({ [3] = "c", [2] = "b", [1] = "a" }), '["a","b","c"]')
local cycle = {}
cycle.self = cycle
fails("cycle", cycle, "too deep")

local big = {}
for i = 1, 20000 do
	big[i] = { id = i, name = "item " .. i .. " é", score = i / 7, tags = { "a", "b" } }
end
local big_text = json.encode(big)
check("a large document round-trips", json.encode(json.decode(big_text)), big_text)

big[1000].bad = print
fails("a function deep in a big table", big, "cannot encode a function")
check("encode still works after a failed encode", json.encode({ 1, 2 }), "[1,2]")

local value = json.decode(' {"a": [1, 2.5e2, -3, true, false, null], "b": {"c": "d"}, "n": null, "e": [], "o": {}} ')
check("number", value.a[2], 250)
check("negative", value.a[3], -3)
check("null in an array", value.a[6], json.null)
check("array length", #value.a, 6)
check("nested", value.b.c, "d")
check("null member is absent", value.n, nil)
check("empty array survives", json.encode(value.e), "[]")
check("empty object survives", json.encode(value.o), "{}")
check("top-level null", json.decode("null"), json.null)
check("escapes", json.decode('"\\"\\\\\\/\\b\\f\\n\\r\\t\\u0041"'), '"\\/\b\f\n\r\tA')
check("surrogate pair", json.decode('"\\ud83d\\ude00"'), "😀")
check("round trip", json.encode(json.decode('{"x":[1,null,{"y":[]}],"z":{}}')), '{"x":[1,null,{"y":[]}],"z":{}}')
check("a bom is skipped", json.decode("\239\187\191{\"a\":1}").a, 1)
check("invalid utf-8 passes through", json.decode('"\255\192"'), "\255\192")
check("an escaped nul is kept", #json.decode('"a\\u0000b"'), 3)
check("an escaped nul round-trips", json.encode(json.decode('"a\\u0000b"')), '"a\\u0000b"')
check("a u64 past 2^53 is the nearest double, not an error", json.decode('{"lamports":18446744073709551615}').lamports, 2 ^ 64)
check("raw control characters in a string are accepted", json.decode('"a\nb"'), "a\nb")

local function nest_arrays(n)
	return string.rep("[", n) .. string.rep("]", n)
end

local function nest_objects(n)
	return string.rep('{"a":', n) .. "1" .. string.rep("}", n)
end

local function nesting(v)
	local count = 0
	while type(v) == "table" do
		count += 1
		v = v[1] or v.a
	end
	return count
end

check("200 nested arrays decode", nesting(json.decode(nest_arrays(200))), 200)
check("200 nested objects decode", nesting(json.decode(nest_objects(200))), 200)
check("200 nested arrays round-trip", json.encode(json.decode(nest_arrays(200))), nest_arrays(200))

local function rejects(name, text)
	local ok, result, err = pcall(json.decode, text)
	check(name .. " does not raise", ok, true)
	check(name .. " is no value", result, nil)
	check(name .. " says where it went wrong", tostring(err):match("%(at byte %d+%)$") ~= nil, true)
end
rejects("the output of a command that printed nothing", "")
rejects("only a newline", "\n")
rejects("a document cut short", '{"results": [{"title": "Lua')
rejects("an error page where JSON was expected", "<html><body>502 Bad Gateway</body></html>")
rejects("JSON lines, which decode a line at a time", '{"a":1}\n{"a":2}\n')
rejects("a trailing comma", "[1,]")
rejects("a Python dict printed, not dumped", "{'a': 1}")
rejects("the NaN Python's json.dumps writes", '{"score": NaN}')
rejects("a number too large for a double", "1e999")
rejects("half of a surrogate pair", '"\\ud83d"')
rejects("a missing colon", '{"a" 1}')
rejects("201 nested arrays", nest_arrays(201))
rejects("201 nested objects", nest_objects(201))
check("the position is the byte that is wrong, counted from 1", select(2, json.decode("[1 2]")):match("%(at byte %d+%)$"), "(at byte 4)")

local nested = { b = { 1, {} }, a = { x = json.array() }, c = "s" }
check("indent writes as Python's json.dumps(indent=2, sort_keys=True)", json.encode(nested, { indent = 2 }),
	'{\n  "a": {\n    "x": []\n  },\n  "b": [\n    1,\n    {}\n  ],\n  "c": "s"\n}')
check("an indent of 0 is the compact form", json.encode(nested, { indent = 0 }), json.encode(nested))
check("indent keeps a scalar as it is", json.encode("x", { indent = 4 }), '"x"')
check("indented text decodes to the same", json.encode(json.decode(json.encode(nested, { indent = 3 }))), json.encode(nested))
checks.raises("a negative indent", "indent is a whole number of spaces from 0 to 16", json.encode, {}, { indent = -1 })
checks.raises("a fractional indent", "indent is a whole number", json.encode, {}, { indent = 1.5 })

checks.done("json")
