local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check = checks.check

check("scalars", json.encode({ true, false, 1, -2.5, "x" }), '[true,false,1,-2.5,"x"]')
check("integers have no exponent", json.encode(1e15), "1000000000000000")
check("large floats round-trip", json.decode(json.encode(1.7976931348623157e308)), 1.7976931348623157e308)
check("sorted keys", json.encode({ b = 1, a = 2, c = { z = 1, y = 2 } }), '{"a":2,"b":1,"c":{"y":2,"z":1}}')
check("empty table is an object", json.encode({}), "{}")
check("marked empty table is an array", json.encode(json.array()), "[]")
check("json.array marks an existing table", json.encode(json.array({ 1 })), "[1]")
check("null", json.encode({ json.null, 1 }), "[null,1]")
check("nil at the top", json.encode(nil), "null")
check("escapes", json.encode('a"b\\c\n\t\1'), '"a\\"b\\\\c\\n\\t\\u0001"')
check("valid utf-8 passes", json.encode("héllo 😀"), '"héllo 😀"')
check("invalid utf-8 is replaced", json.encode("a\255b\192\128c"), '"a\239\191\189b\239\191\189\239\191\189c"')
check("truncated utf-8 is replaced", json.encode("x\240\159"), '"x\239\191\189\239\191\189"')
check("surrogate bytes are replaced", json.encode("\237\160\128"), '"\239\191\189\239\191\189\239\191\189"')
check("control characters use uppercase hex", json.encode("\11"), '"\\u000B"')
check("del is not escaped", json.encode("\127"), '"\127"')
check("1e20 is written out", json.encode(1e20), "100000000000000000000.0")
check("1e21 uses an exponent", json.encode(1e21), "1e21")
check("2^53 keeps its .0", json.encode(2 ^ 53), "9007199254740992.0")
check("1e-7 uses an exponent", json.encode(1e-7), "1e-7")
check("1e-6 is written out", json.encode(0.000001), "0.000001")
check("5e-324", json.encode(5e-324), "5e-324")
check("one third", json.encode(1 / 3), "0.3333333333333333")
check("minus zero", json.encode(-0.0), "0")

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
check("minus zero is zero", 1 / json.decode("-0"), math.huge)
check("minus zero point zero keeps its sign", 1 / json.decode("-0.0"), -math.huge)
check("the largest uint64 is a double", json.decode("18446744073709551615"), 2 ^ 64)
check("a huge integer is a double", json.decode("123456789012345678901234567890"), 1.2345678901234568e29)
check("2^53+1 rounds down", json.decode("9007199254740993"), 9007199254740992)
check("0.1 is exact", json.decode("0.1"), 0.1)
check("4.9e-324 is exact", json.decode("4.9e-324"), 4.9e-324)
check("the later duplicate wins", json.decode('{"a":1,"a":2}').a, 2)
check("a null duplicate leaves the first", json.decode('{"a":1,"a":null}').a, 1)
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

local function rejects(text, fragment)
	local result, err = json.decode(text)
	check("reject " .. text, result, nil)
	if not tostring(err):find(fragment, 1, true) then
		check("reject message " .. text, err, fragment)
	end
end
rejects("", "input length is 0")
rejects(" ", "input data is empty")
rejects("{", "unexpected end of data")
rejects('{"a" 1}', "unexpected character, expected ':' after key")
rejects("[1 2]", "unexpected character, expected ',' or ']'")
rejects("[1,]", "trailing comma is not allowed")
rejects('"abc', "unexpected end of data")
rejects('"\\x"', "invalid escaped sequence in string")
rejects('"\\u12"', "invalid escaped sequence in string")
rejects('"\\ud83dx"', "no low surrogate in string")
rejects('"\\ude00"', "invalid high surrogate in string")
rejects("tru", "unexpected end of data")
rejects("1 2", "unexpected content after document")
rejects("-", "unexpected end of data")
rejects("1e999", "number is infinity when parsed as double")
rejects("nan", "invalid literal")
rejects("[1]\0", "unexpected content after document")
rejects(string.rep("[", 500), "depth limit exceeded")
rejects(nest_arrays(201), "depth limit exceeded (at byte 202)")
rejects(nest_objects(201), "depth limit exceeded")
check("an error names its position", select(2, json.decode("[1 2]")), "unexpected character, expected ',' or ']' (at byte 4)")

local nested = { b = { 1, {} }, a = { x = json.array() }, c = "s" }
check("indent writes as Python's json.dumps(indent=2, sort_keys=True)", json.encode(nested, { indent = 2 }),
	'{\n  "a": {\n    "x": []\n  },\n  "b": [\n    1,\n    {}\n  ],\n  "c": "s"\n}')
check("an indent of 0 is the compact form", json.encode(nested, { indent = 0 }), json.encode(nested))
check("indent keeps a scalar as it is", json.encode("x", { indent = 4 }), '"x"')
check("indented text decodes to the same", json.encode(json.decode(json.encode(nested, { indent = 3 }))), json.encode(nested))
checks.raises("a negative indent", "indent is a whole number of spaces from 0 to 16", json.encode, {}, { indent = -1 })
checks.raises("a fractional indent", "indent is a whole number", json.encode, {}, { indent = 1.5 })

checks.done("json")
