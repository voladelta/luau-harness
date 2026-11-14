-- json.encode / json.decode. Run with: hxm run tests/json_test.luau

local failures = 0
local function check(name, got, want)
	if got ~= want then
		failures += 1
		print(("FAIL %s\n  got:  %s\n  want: %s"):format(name, tostring(got), tostring(want)))
	end
end

-- Encoding
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
local cycle = {}
cycle.self = cycle
fails("cycle", cycle, "too deep")

-- Decoding
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
check("lone high surrogate", json.decode('"\\ud83dx"'), "\239\191\189x")
check("lone low surrogate", json.decode('"\\ude00"'), "\239\191\189")
check("high then non-low", json.decode('"\\ud83d\\u0041"'), "\239\191\189A")
check("raw newline in a string", json.decode('"a\nb"'), "a\nb")
check("round trip", json.encode(json.decode('{"x":[1,null,{"y":[]}],"z":{}}')), '{"x":[1,null,{"y":[]}],"z":{}}')

local function rejects(text, fragment)
	local result, err = json.decode(text)
	check("reject " .. text, result, nil)
	if not tostring(err):find(fragment, 1, true) then
		check("reject message " .. text, err, fragment)
	end
end
rejects("", "unexpected end")
rejects("{", "expected a string key")
rejects('{"a" 1}', "expected ':'")
rejects("[1 2]", "expected ',' or ']'")
rejects("[1,]", "invalid value")
rejects('"abc', "unterminated string")
rejects('"\\x"', "invalid escape")
rejects('"\\u12"', "\\u escape")
rejects("tru", "invalid value")
rejects("1 2", "trailing")
rejects("-", "invalid number")
rejects("1e999", "invalid number")
rejects("nan", "invalid value")
rejects(string.rep("[", 500), "too deep")

if failures > 0 then
	error(failures .. " json check(s) failed", 0)
end
print("json: ok")
