local dir = host.env("TMPDIR") .. "/host-test/"

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = checks.check, checks.raises

host.sh({ "rm", "-rf", dir })
for _, file in { "top.luau", "src/a.cpp", "src/a/b.cpp", "src/a/b/c.luau", ".git/x/y", ".hidden" } do
	host.write(dir .. file, "x")
end
host.sh({ "ln", "-s", "src", dir .. "link" })

local function walk(options)
	local out, err = host.walk(dir, options)
	return out and table.concat(out, " ") or err
end

check("walk lists every entry under a directory, sorted, but .git", walk(),
	".hidden link src/ src/a.cpp src/a/ src/a/b.cpp src/a/b/ src/a/b/c.luau top.luau")
check("a glob with ** matches at the top too", walk({ glob = "**/*.luau" }), "src/a/b/c.luau top.luau")
check("a * stays inside its directory", walk({ glob = "src/*.cpp" }), "src/a.cpp")
check("a glob with no slash matches names at any depth", walk({ glob = "*.cpp" }), "src/a.cpp src/a/b.cpp")
check("depth 1 is the directory's own entries", walk({ depth = 1 }), ".hidden link src/ top.luau")
check("depth 2", walk({ depth = 2, glob = "src/**" }), "src/a.cpp src/a/")
check("a missing directory", select(2, host.walk(dir .. "nope")), dir .. "nope: No such file or directory")
raises("depth 0", "host.walk: depth is a whole number, 1 or more", host.walk, dir, { depth = 0 })

local file = host.stat(dir .. "top.luau")
check("stat of a file", file.type, "file")
check("its size", file.size, 1)
check("its mode, in octal", file.mode, host.sh({ "stat", "-c", "%a", dir .. "top.luau" }).out:gsub("\n", ""))
check("its mtime, in seconds", math.abs(file.mtime - os.time()) < 60, true)
check("stat of a directory", host.stat(dir .. "src").type, "dir")
local link = host.stat(dir .. "link")
check("stat does not follow a link", link.type, "link")
check("and says where it points", link.target, "src")
check("stat of nothing", select(2, host.stat(dir .. "nope")), dir .. "nope: No such file or directory")

check("sh in another directory", host.sh("pwd", { cwd = dir .. "src" }).out, dir .. "src\n")
check("sh with a variable", host.sh("printf %s \"$HX_TEST\"", { env = { HX_TEST = "a b" } }).out, "a b")
check("a number is its text", host.sh("printf %s $HX_TEST", { env = { HX_TEST = 3 } }).out, "3")
check("false takes a variable away", host.sh("printf %s \"${HOME-gone}\"", { env = { HOME = false } }).out, "gone")
check("the rest stays", host.sh("printf %s \"$PATH\"", { env = { HX_TEST = "x" } }).out, host.env("PATH"))
raises("a cwd that is missing", "host.sh: cwd " .. dir .. "nope: No such file or directory", host.sh, "pwd", { cwd = dir .. "nope" })
raises("a cwd that is a file", "is not a directory", host.sh, "pwd", { cwd = dir .. "top.luau" })
raises("a name with =", "env has a name that is not a variable's", host.sh, "true", { env = { ["A=B"] = "x" } })
raises("a value that is a table", "env.A is text, or false to remove it", host.sh, "true", { env = { A = {} } })

checks.done("host")
