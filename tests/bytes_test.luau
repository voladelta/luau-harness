local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = checks.check, checks.raises

local bytes = loop.ext("bytes")
local all = {}
for i = 0, 255 do
	table.insert(all, string.char(i))
end
local every = table.concat(all)

check("the system prompt lists bytes with no key", loop.system_prompt():find("- bytes (built in): binary data", 1, true) ~= nil, true)

check("hex", bytes.hex("\0\1\127\255"), "0001" .. "7fff")
check("unhex takes upper case, a 0x and spaces", bytes.unhex("0x00 01 7F ff"), "\0\1\127\255")
check("unhex takes leading whitespace before 0x", bytes.unhex(" \t0X00 01 7F ff"), "\0\1\127\255")
check("hex and back", bytes.unhex(bytes.hex(every)), every)
raises("an odd number of digits", "even number of hex digits", bytes.unhex, "abc")
raises("a letter that is not hex", "even number of hex digits", bytes.unhex, "zz")

check("base64 with no padding", bytes.base64("abc"), "YWJj")
check("base64 with one =", bytes.base64("ab"), "YWI=")
check("base64 with two", bytes.base64("a"), "YQ==")
check("base64 of nothing", bytes.base64(""), "")
check("base64 of every byte, as Python writes it", bytes.base64(every):sub(1, 40), "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwd")
check("which is 344 long", #bytes.base64(every), 344)
check("and back", bytes.unbase64(bytes.base64(every)), every)
check("unbase64 without padding and across lines", bytes.unbase64("YW\nI"), "ab")
check("unbase64 takes the URL alphabet", bytes.unbase64("-_8"), bytes.unbase64("+/8"))
raises("a character that is not base64", '"*" at 3 is not base64', bytes.unbase64, "YW*j")
raises("one symbol left over", "cut short", bytes.unbase64, "YWJjZ")

check("base58", bytes.base58("hello world"), "StV1DL6CwTryKyV")
check("leading zero bytes are 1s", bytes.base58("\0\0\x28\x7f\xb4\xcd"), "11233QC4")
check("a key of zeros", bytes.base58(string.rep("\0", 32)), string.rep("1", 32))
local wsol = "So11111111111111111111111111111111111111112"
check("a Solana address decodes to 32 bytes", #bytes.unbase58(wsol), 32)
check("which are these", bytes.hex(bytes.unbase58(wsol)), "069b8857feab8184fb687f634618c035dac439dc1aeb3b5598a0f00000000001")
check("and back", bytes.base58(bytes.unbase58(wsol)), wsol)
check("every byte and back", bytes.unbase58(bytes.base58(every)), every)
local wide = string.rep("\1", 8200)
check("base58 handles inputs larger than the call stack", bytes.unbase58(bytes.base58(wide)), wide)
check("nothing", bytes.base58(""), "")
raises("a 0 is not base58", '"0" at 2 is not base58', bytes.unbase58, "10")

check("sha256 of nothing", bytes.hex(bytes.sha256("")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
check("sha256 of abc", bytes.hex(bytes.sha256("abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
check("sha256 of several blocks", bytes.hex(bytes.sha256(string.rep("a", 1000))), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3")
check("sha256 against the system's", bytes.hex(bytes.sha256(every)), host.sh({ "sha256sum" }, { input = every }).out:match("^%x+"))

check("a u128 past 2^53, exactly", bytes.uint(string.rep("\255", 16)), "340282366920938463463374607431768211455")
check("little-endian, from an offset", bytes.uint("xx" .. every:sub(2, 17) .. "yy", 3, 16), "21345817372864405881847059188222722561")
check("a u64 at its offset", bytes.uint("\0\1\0\0\0\0\0\0\0", 2, 8), "1")
check("zero", bytes.uint("\0\0\0\0"), "0")
check("an i64 of -2", bytes.int("\254" .. string.rep("\255", 7)), "-2")
check("the least i8", bytes.int("\128"), "-128")
check("a positive int", bytes.int("\127"), "127")
check("an i32 inside a record", bytes.int("ab\255\255\255\255cd", 3, 4), "-1")
raises("a field past the end", "bytes 5 to 12 are not inside the 8 bytes given", bytes.uint, "12345678", 5, 8)
raises("a width of no bytes", "not inside", bytes.uint, "12", 1, 0)
raises("data that is not text", "bytes.uint: argument 1 (data) must be a string, not nil", bytes.uint)

local function sums(algo: string, data: string): string
	local file = host.env("TMPDIR") .. "/bytes-sum"
	host.write(file, data)
	return (host.sh({ algo .. "sum", file }).out:match("^%x+"))
end
for _, length in { 0, 3, 55, 56, 63, 64, 111, 112, 127, 128, 1000 } do
	local data = string.rep("abc\0\255", 200):sub(1, length)
	for _, algo in { "sha1", "sha256", "sha512", "md5" } do
		check(("%s of %d bytes, as %ssum has it"):format(algo, length, algo), bytes.hex(bytes[algo](data)), sums(algo, data))
	end
end

check("hmac sha256, RFC 4231 case 1", bytes.hex(bytes.hmac("sha256", string.rep("\11", 20), "Hi There")),
	"b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
check("hmac sha512 with a key longer than the block, RFC 4231 case 6",
	bytes.hex(bytes.hmac("sha512", string.rep("\170", 131), "Test Using Larger Than Block-Size Key - Hash Key First")),
	"80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f352"
		.. "6b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598")
check("hmac md5", bytes.hex(bytes.hmac("md5", "key", "The quick brown fox jumps over the lazy dog")),
	"80070713463e7749b90c2dc24911e275")
check("hmac sha1", bytes.hex(bytes.hmac("sha1", "key", "The quick brown fox jumps over the lazy dog")),
	"de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9")
raises("hmac with an unknown hash", 'bytes.hmac: "sha3" is not sha256, sha512, sha1 or md5', bytes.hmac, "sha3", "k", "d")
raises("sha512 of nothing given", "bytes.sha512: argument 1 (data) must be a string, not nil", bytes.sha512)

checks.done("bytes")
