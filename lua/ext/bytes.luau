-- binary data as text and numbers: hex, unhex, base64, unbase64, base58, unbase58, sha256(data) -> 32 bytes, uint(data, at?, width?) and int(...) -> exact decimal text of a little-endian integer
--!strict

local M = {}

local function check(fname: string, value: unknown, position: number, name: string, kind: string, optional: boolean?)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("bytes.%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

local function convert(digits: { number }, from: number, to: number): { number }
	local out = { 0 }
	for _, digit in digits do
		local carry = digit
		for i = 1, #out do
			carry += out[i] * from
			out[i] = carry % to
			carry //= to
		end
		while carry > 0 do
			table.insert(out, carry % to)
			carry //= to
		end
	end
	return out
end

local function leading(text: string, char: string): number
	local run = text:match("^" .. char .. "*")
	return run and #run or 0
end

function M.hex(data: string): string
	check("hex", data, 1, "data", "string")
	return (data:gsub(".", function(c: string): string
		return ("%02x"):format(c:byte())
	end))
end

function M.unhex(text: string): string
	check("unhex", text, 1, "text", "string")
	local digits = text:gsub("^0[xX]", ""):gsub("%s", "")
	if #digits % 2 ~= 0 or digits:find("%X") then
		error("bytes.unhex: the text is not an even number of hex digits", 0)
	end
	return (digits:gsub("%x%x", function(pair: string): string
		return string.char(tonumber(pair, 16) :: number)
	end))
end

local alphabet64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local value64: { [string]: number } = { ["-"] = 62, ["_"] = 63 }
for i = 1, 64 do
	value64[alphabet64:sub(i, i)] = i - 1
end

function M.base64(data: string): string
	check("base64", data, 1, "data", "string")
	local out = {}
	for i = 1, #data, 3 do
		local a, b, c = data:byte(i, i + 2)
		local n = a * 65536 + (b or 0) * 256 + (c or 0)
		local quad = {}
		for shift = 3, 0, -1 do
			local index = n // 64 ^ shift % 64 + 1
			table.insert(quad, alphabet64:sub(index, index))
		end
		local kept = if c then 4 elseif b then 3 else 2
		table.insert(out, table.concat(quad, "", 1, kept) .. string.rep("=", 4 - kept))
	end
	return table.concat(out)
end

function M.unbase64(text: string): string
	check("unbase64", text, 1, "text", "string")
	local symbols = text:gsub("%s", ""):gsub("=+$", "")
	if #symbols % 4 == 1 then
		error("bytes.unbase64: the text is cut short", 0)
	end
	local out = {}
	for i = 1, #symbols, 4 do
		local group = symbols:sub(i, i + 3)
		local n = 0
		for j = 1, 4 do
			local char = group:sub(j, j)
			local value = if char == "" then 0 else value64[char]
			if not value then
				error(("bytes.unbase64: %q at %d is not base64"):format(char, i + j - 1), 0)
			end
			n = n * 64 + value
		end
		local whole = string.char(n // 65536, n // 256 % 256, n % 256)
		table.insert(out, whole:sub(1, #group - 1))
	end
	return table.concat(out)
end

local alphabet58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
local value58: { [string]: number } = {}
for i = 1, 58 do
	value58[alphabet58:sub(i, i)] = i - 1
end

function M.base58(data: string): string
	check("base58", data, 1, "data", "string")
	local zeros = leading(data, "\0")
	local digits = convert({ data:byte(zeros + 1, -1) }, 256, 58)
	local out = {}
	for i = #digits, 1, -1 do
		table.insert(out, alphabet58:sub(digits[i] + 1, digits[i] + 1))
	end
	local body = if zeros == #data then "" else table.concat(out)
	return string.rep("1", zeros) .. body
end

function M.unbase58(text: string): string
	check("unbase58", text, 1, "text", "string")
	local ones = leading(text, "1")
	local digits = {}
	for i = ones + 1, #text do
		local value = value58[text:sub(i, i)]
		if not value then
			error(("bytes.unbase58: %q at %d is not base58"):format(text:sub(i, i), i), 0)
		end
		table.insert(digits, value)
	end
	local limbs = convert(digits, 58, 256)
	local out = {}
	for i = #limbs, 1, -1 do
		table.insert(out, string.char(limbs[i]))
	end
	local body = if ones == #text then "" else table.concat(out)
	return string.rep("\0", ones) .. body
end

local K = {
	0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
	0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
	0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
	0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
	0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
	0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
	0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
	0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
}

function M.sha256(data: string): string
	check("sha256", data, 1, "data", "string")
	local band, bnot, bxor, rrotate, rshift = bit32.band, bit32.bnot, bit32.bxor, bit32.rrotate, bit32.rshift
	local padded = data .. "\128" .. string.rep("\0", (55 - #data) % 64) .. string.pack(">I8", #data * 8)
	local h = { 0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19 }
	local w = table.create(64, 0)
	for block = 1, #padded, 64 do
		for i = 1, 16 do
			w[i] = string.unpack(">I4", padded, block + (i - 1) * 4) :: number
		end
		for i = 17, 64 do
			local s0 = bxor(rrotate(w[i - 15], 7), rrotate(w[i - 15], 18), rshift(w[i - 15], 3))
			local s1 = bxor(rrotate(w[i - 2], 17), rrotate(w[i - 2], 19), rshift(w[i - 2], 10))
			w[i] = (w[i - 16] + s0 + w[i - 7] + s1) % 2 ^ 32
		end
		local a, b, c, d, e, f, g, hh = h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8]
		for i = 1, 64 do
			local s1 = bxor(rrotate(e, 6), rrotate(e, 11), rrotate(e, 25))
			local choice = bxor(band(e, f), band(bnot(e), g))
			local t1 = hh + s1 + choice + K[i] + w[i]
			local s0 = bxor(rrotate(a, 2), rrotate(a, 13), rrotate(a, 22))
			local majority = bxor(band(a, b), band(a, c), band(b, c))
			hh, g, f, e, d, c, b, a = g, f, e, (d + t1) % 2 ^ 32, c, b, a, (t1 + s0 + majority) % 2 ^ 32
		end
		for i, v in { a, b, c, d, e, f, g, hh } do
			h[i] = (h[i] + v) % 2 ^ 32
		end
	end
	return string.pack(">I4I4I4I4I4I4I4I4", table.unpack(h))
end

local function field(fname: string, data: string, at: number?, width: number?): { number }
	check(fname, data, 1, "data", "string")
	check(fname, at, 2, "at", "number", true)
	check(fname, width, 3, "width", "number", true)
	local first = at or 1
	local size = width or #data - first + 1
	if first < 1 or size < 1 or first % 1 ~= 0 or size % 1 ~= 0 or first + size - 1 > #data then
		error(("bytes.%s: bytes %d to %d are not inside the %d bytes given"):format(fname, first, first + size - 1, #data), 0)
	end
	local big = {}
	for i = first + size - 1, first, -1 do
		table.insert(big, (data:byte(i)))
	end
	return big
end

local function decimal(big: { number }): string
	local limbs = convert(big, 256, 1e7)
	local out = { tostring(limbs[#limbs]) }
	for i = #limbs - 1, 1, -1 do
		table.insert(out, ("%07d"):format(limbs[i]))
	end
	return table.concat(out)
end

function M.uint(data: string, at: number?, width: number?): string
	return decimal(field("uint", data, at, width))
end

function M.int(data: string, at: number?, width: number?): string
	local big = field("int", data, at, width)
	if big[1] < 128 then
		return decimal(big)
	end
	local carry = 1
	for i = #big, 1, -1 do
		local v = 255 - big[i] + carry
		big[i], carry = v % 256, v // 256
	end
	return "-" .. decimal(big)
end

return M
