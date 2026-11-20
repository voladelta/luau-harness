-- binary data as text: hex, unhex, base64, unbase64, sha256(data) -> 32 bytes, sha512, sha1, md5, hmac(algo, key, data) with algo "sha256", "sha512", "sha1" or "md5"
--!strict

local M = {}

local function check(fname: string, value: unknown, position: number, name: string, kind: string, optional: boolean?)
	if type(value) == kind or value == nil and optional then
		return
	end
	error(("bytes.%s: argument %d (%s) must be a %s, not %s"):format(fname, position, name, kind, type(value)), 0)
end

function M.hex(data: string): string
	check("hex", data, 1, "data", "string")
	return (data:gsub(".", function(c: string): string
		return ("%02x"):format(c:byte())
	end))
end

function M.unhex(text: string): string
	check("unhex", text, 1, "text", "string")
	local digits = text:gsub("%s", ""):gsub("^0[xX]", "")
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

local K512 = {
	0x428a2f98, 0xd728ae22, 0x71374491, 0x23ef65cd, 0xb5c0fbcf, 0xec4d3b2f, 0xe9b5dba5, 0x8189dbbc,
	0x3956c25b, 0xf348b538, 0x59f111f1, 0xb605d019, 0x923f82a4, 0xaf194f9b, 0xab1c5ed5, 0xda6d8118,
	0xd807aa98, 0xa3030242, 0x12835b01, 0x45706fbe, 0x243185be, 0x4ee4b28c, 0x550c7dc3, 0xd5ffb4e2,
	0x72be5d74, 0xf27b896f, 0x80deb1fe, 0x3b1696b1, 0x9bdc06a7, 0x25c71235, 0xc19bf174, 0xcf692694,
	0xe49b69c1, 0x9ef14ad2, 0xefbe4786, 0x384f25e3, 0x0fc19dc6, 0x8b8cd5b5, 0x240ca1cc, 0x77ac9c65,
	0x2de92c6f, 0x592b0275, 0x4a7484aa, 0x6ea6e483, 0x5cb0a9dc, 0xbd41fbd4, 0x76f988da, 0x831153b5,
	0x983e5152, 0xee66dfab, 0xa831c66d, 0x2db43210, 0xb00327c8, 0x98fb213f, 0xbf597fc7, 0xbeef0ee4,
	0xc6e00bf3, 0x3da88fc2, 0xd5a79147, 0x930aa725, 0x06ca6351, 0xe003826f, 0x14292967, 0x0a0e6e70,
	0x27b70a85, 0x46d22ffc, 0x2e1b2138, 0x5c26c926, 0x4d2c6dfc, 0x5ac42aed, 0x53380d13, 0x9d95b3df,
	0x650a7354, 0x8baf63de, 0x766a0abb, 0x3c77b2a8, 0x81c2c92e, 0x47edaee6, 0x92722c85, 0x1482353b,
	0xa2bfe8a1, 0x4cf10364, 0xa81a664b, 0xbc423001, 0xc24b8b70, 0xd0f89791, 0xc76c51a3, 0x0654be30,
	0xd192e819, 0xd6ef5218, 0xd6990624, 0x5565a910, 0xf40e3585, 0x5771202a, 0x106aa070, 0x32bbd1b8,
	0x19a4c116, 0xb8d2d0c8, 0x1e376c08, 0x5141ab53, 0x2748774c, 0xdf8eeb99, 0x34b0bcb5, 0xe19b48a8,
	0x391c0cb3, 0xc5c95a63, 0x4ed8aa4a, 0xe3418acb, 0x5b9cca4f, 0x7763e373, 0x682e6ff3, 0xd6b2b8a3,
	0x748f82ee, 0x5defb2fc, 0x78a5636f, 0x43172f60, 0x84c87814, 0xa1f0ab72, 0x8cc70208, 0x1a6439ec,
	0x90befffa, 0x23631e28, 0xa4506ceb, 0xde82bde9, 0xbef9a3f7, 0xb2c67915, 0xc67178f2, 0xe372532b,
	0xca273ece, 0xea26619c, 0xd186b8c7, 0x21c0c207, 0xeada7dd6, 0xcde0eb1e, 0xf57d4f7f, 0xee6ed178,
	0x06f067aa, 0x72176fba, 0x0a637dc5, 0xa2c898a6, 0x113f9804, 0xbef90dae, 0x1b710b35, 0x131c471b,
	0x28db77f5, 0x23047d84, 0x32caab7b, 0x40c72493, 0x3c9ebe0a, 0x15c9bebc, 0x431d67c4, 0x9c100d4c,
	0x4cc5d4be, 0xcb3e42b6, 0x597f299c, 0xfc657e2a, 0x5fcb6fab, 0x3ad6faec, 0x6c44198c, 0x4a475817,
}

local function add64(...: number): (number, number)
	local hi, lo = 0, 0
	local args = { ... }
	for i = 1, #args, 2 do
		hi += args[i]
		lo += args[i + 1]
	end
	return (hi + lo // 2 ^ 32) % 2 ^ 32, lo % 2 ^ 32
end

local function rotr64(hi: number, lo: number, n: number): (number, number)
	if n >= 32 then
		hi, lo, n = lo, hi, n - 32
	end
	if n == 0 then
		return hi, lo
	end
	local lshift, rshift, bor = bit32.lshift, bit32.rshift, bit32.bor
	return bor(rshift(hi, n), lshift(lo, 32 - n)), bor(rshift(lo, n), lshift(hi, 32 - n))
end

local function shr64(hi: number, lo: number, n: number): (number, number)
	local lshift, rshift, bor = bit32.lshift, bit32.rshift, bit32.bor
	return rshift(hi, n), bor(rshift(lo, n), lshift(hi, 32 - n))
end

function M.sha512(data: string): string
	check("sha512", data, 1, "data", "string")
	local band, bnot, bxor = bit32.band, bit32.bnot, bit32.bxor
	local padded = data
		.. "\128"
		.. string.rep("\0", (111 - #data) % 128)
		.. string.pack(">I8I8", 0, #data * 8)
	local h = {
		0x6a09e667, 0xf3bcc908, 0xbb67ae85, 0x84caa73b, 0x3c6ef372, 0xfe94f82b, 0xa54ff53a, 0x5f1d36f1,
		0x510e527f, 0xade682d1, 0x9b05688c, 0x2b3e6c1f, 0x1f83d9ab, 0xfb41bd6b, 0x5be0cd19, 0x137e2179,
	}
	local w = table.create(160, 0)
	for block = 1, #padded, 128 do
		for i = 1, 32 do
			w[i] = string.unpack(">I4", padded, block + (i - 1) * 4) :: number
		end
		for t = 17, 80 do
			local ah, al = w[2 * t - 31], w[2 * t - 30]
			local r1h, r1l = rotr64(ah, al, 1)
			local r8h, r8l = rotr64(ah, al, 8)
			local s7h, s7l = shr64(ah, al, 7)
			local bh, bl = w[2 * t - 5], w[2 * t - 4]
			local r19h, r19l = rotr64(bh, bl, 19)
			local r61h, r61l = rotr64(bh, bl, 61)
			local s6h, s6l = shr64(bh, bl, 6)
			w[2 * t - 1], w[2 * t] = add64(
				w[2 * t - 33], w[2 * t - 32],
				bxor(r1h, r8h, s7h), bxor(r1l, r8l, s7l),
				w[2 * t - 15], w[2 * t - 14],
				bxor(r19h, r61h, s6h), bxor(r19l, r61l, s6l)
			)
		end
		local v = table.clone(h)
		for t = 1, 80 do
			local eh, el = v[9], v[10]
			local a1h, a1l = rotr64(eh, el, 14)
			local a2h, a2l = rotr64(eh, el, 18)
			local a3h, a3l = rotr64(eh, el, 41)
			local chh = bxor(band(eh, v[11]), band(bnot(eh), v[13]))
			local chl = bxor(band(el, v[12]), band(bnot(el), v[14]))
			local t1h, t1l = add64(
				v[15], v[16],
				bxor(a1h, a2h, a3h), bxor(a1l, a2l, a3l),
				chh, chl,
				K512[2 * t - 1], K512[2 * t],
				w[2 * t - 1], w[2 * t]
			)
			local ah, al = v[1], v[2]
			local b1h, b1l = rotr64(ah, al, 28)
			local b2h, b2l = rotr64(ah, al, 34)
			local b3h, b3l = rotr64(ah, al, 39)
			local mah = bxor(band(ah, v[3]), band(ah, v[5]), band(v[3], v[5]))
			local mal = bxor(band(al, v[4]), band(al, v[6]), band(v[4], v[6]))
			local t2h, t2l = add64(bxor(b1h, b2h, b3h), bxor(b1l, b2l, b3l), mah, mal)
			v[15], v[16] = v[13], v[14]
			v[13], v[14] = v[11], v[12]
			v[11], v[12] = v[9], v[10]
			v[9], v[10] = add64(v[7], v[8], t1h, t1l)
			v[7], v[8] = v[5], v[6]
			v[5], v[6] = v[3], v[4]
			v[3], v[4] = v[1], v[2]
			v[1], v[2] = add64(t1h, t1l, t2h, t2l)
		end
		for i = 1, 16, 2 do
			h[i], h[i + 1] = add64(h[i], h[i + 1], v[i], v[i + 1])
		end
	end
	local out = {}
	for _, word in h do
		table.insert(out, string.pack(">I4", word))
	end
	return table.concat(out)
end

function M.sha1(data: string): string
	check("sha1", data, 1, "data", "string")
	local band, bnot, bor, bxor, lrotate = bit32.band, bit32.bnot, bit32.bor, bit32.bxor, bit32.lrotate
	local padded = data .. "\128" .. string.rep("\0", (55 - #data) % 64) .. string.pack(">I8", #data * 8)
	local h = { 0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0 }
	local w = table.create(80, 0)
	for block = 1, #padded, 64 do
		for i = 1, 16 do
			w[i] = string.unpack(">I4", padded, block + (i - 1) * 4) :: number
		end
		for i = 17, 80 do
			w[i] = lrotate(bxor(w[i - 3], w[i - 8], w[i - 14], w[i - 16]), 1)
		end
		local a, b, c, d, e = h[1], h[2], h[3], h[4], h[5]
		for i = 1, 80 do
			local f, k
			if i <= 20 then
				f, k = bor(band(b, c), band(bnot(b), d)), 0x5a827999
			elseif i <= 40 then
				f, k = bxor(b, c, d), 0x6ed9eba1
			elseif i <= 60 then
				f, k = bor(band(b, c), band(b, d), band(c, d)), 0x8f1bbcdc
			else
				f, k = bxor(b, c, d), 0xca62c1d6
			end
			a, b, c, d, e = (lrotate(a, 5) + f + e + k + w[i]) % 2 ^ 32, a, lrotate(b, 30), c, d
		end
		for i, v in { a, b, c, d, e } do
			h[i] = (h[i] + v) % 2 ^ 32
		end
	end
	return string.pack(">I4I4I4I4I4", table.unpack(h))
end

local MD5_K = table.create(64, 0)
for i = 1, 64 do
	MD5_K[i] = math.floor(math.abs(math.sin(i)) * 2 ^ 32)
end
local MD5_S = {
	7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
	5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
	4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
	6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
}

function M.md5(data: string): string
	check("md5", data, 1, "data", "string")
	local band, bnot, bor, bxor, lrotate = bit32.band, bit32.bnot, bit32.bor, bit32.bxor, bit32.lrotate
	local padded = data .. "\128" .. string.rep("\0", (55 - #data) % 64) .. string.pack("<I8", #data * 8)
	local h = { 0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476 }
	local m = table.create(16, 0)
	for block = 1, #padded, 64 do
		for i = 1, 16 do
			m[i] = string.unpack("<I4", padded, block + (i - 1) * 4) :: number
		end
		local a, b, c, d = h[1], h[2], h[3], h[4]
		for i = 0, 63 do
			local f, g
			if i < 16 then
				f, g = bor(band(b, c), band(bnot(b), d)), i
			elseif i < 32 then
				f, g = bor(band(d, b), band(bnot(d), c)), (5 * i + 1) % 16
			elseif i < 48 then
				f, g = bxor(b, c, d), (3 * i + 5) % 16
			else
				f, g = bxor(c, bor(b, bnot(d))), (7 * i) % 16
			end
			local sum = (a + f + MD5_K[i + 1] + m[g + 1]) % 2 ^ 32
			a, b, c, d = d, (b + lrotate(sum, MD5_S[i + 1])) % 2 ^ 32, b, c
		end
		for i, v in { a, b, c, d } do
			h[i] = (h[i] + v) % 2 ^ 32
		end
	end
	return string.pack("<I4I4I4I4", table.unpack(h))
end

local HASHES: { [string]: { hash: (string) -> string, block: number } } = {
	sha256 = { hash = M.sha256, block = 64 },
	sha512 = { hash = M.sha512, block = 128 },
	sha1 = { hash = M.sha1, block = 64 },
	md5 = { hash = M.md5, block = 64 },
}

function M.hmac(algo: string, key: string, data: string): string
	check("hmac", algo, 1, "algo", "string")
	check("hmac", key, 2, "key", "string")
	check("hmac", data, 3, "data", "string")
	local chosen = HASHES[algo]
	if not chosen then
		error(("bytes.hmac: %q is not sha256, sha512, sha1 or md5"):format(algo), 0)
	end
	if #key > chosen.block then
		key = chosen.hash(key)
	end
	key ..= string.rep("\0", chosen.block - #key)
	local inner = key:gsub(".", function(c: string): string
		return string.char(bit32.bxor(c:byte(), 0x36))
	end)
	local outer = key:gsub(".", function(c: string): string
		return string.char(bit32.bxor(c:byte(), 0x5c))
	end)
	return chosen.hash(outer .. chosen.hash(inner .. data))
end

return M
