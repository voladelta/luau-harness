local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, raises = checks.check, checks.raises

local path = loop.ext("path")

check("the system prompt lists path", loop.system_prompt():find("- path (built in): POSIX paths", 1, true) ~= nil, true)

local posixpath = json.decode([==[
{"": ["", "", "", "."], "/": ["/", "", "", "/"], "//": ["//", "", "", "//"], "///": ["///", "", "", "/"], "a": ["", "a", "", "a"], "a/": ["a", "", "", "a"], "a/b": ["a", "b", "", "a/b"], "a/b/": ["a/b", "", "", "a/b"], "/a/b": ["/a", "b", "", "/a/b"], "//a/b": ["//a", "b", "", "//a/b"], "a//b": ["a", "b", "", "a/b"], "./a": [".", "a", "", "a"], "../a": ["..", "a", "", "../a"], "a/../..": ["a/..", "..", "", ".."], "/..": ["/", "..", "", "/"], "/a/./b/../c": ["/a/./b/..", "c", "", "/a/c"], "a/b.tar.gz": ["a", "b.tar.gz", ".gz", "a/b.tar.gz"], ".bashrc": ["", ".bashrc", "", ".bashrc"], "a/.bashrc": ["a", ".bashrc", "", "a/.bashrc"], "file.": ["", "file.", ".", "file."], "a..b": ["", "a..b", ".b", "a..b"], "...": ["", "...", "", "..."], "/x.y/z": ["/x.y", "z", "", "/x.y/z"], ".a.b": ["", ".a.b", ".b", ".a.b"]}
]==])
for given, want in posixpath do
	check(("dirname %q, as posixpath has it"):format(given), path.dirname(given), want[1])
	check(("basename %q"):format(given), path.basename(given), want[2])
	check(("ext %q"):format(given), path.ext(given), want[3])
	check(("normalize %q"):format(given), path.normalize(given), want[4])
end

check("join", path.join("a", "b"), "a/b")
check("join after a slash", path.join("a/", "b"), "a/b")
check("join from a root", path.join("a", "/b", "c"), "/b/c")
check("join on nothing", path.join("", "b"), "b")
check("join nothing on", path.join("a", ""), "a/")

check("relative across", path.relative("/a/b/c", "/a/d"), "../b/c")
check("relative to itself", path.relative("/a", "/a"), ".")
check("relative to the root", path.relative("/a/b", "/"), "a/b")
check("relative up to the root", path.relative("/", "/a/b"), "../..")
check("relative to the working directory", path.relative(host.cwd .. "/x/y"), "x/y")
check("relative from a relative path", path.relative("x/y", "x"), "y")
raises("relative of nothing", "path.relative: argument 1 (path) is empty", path.relative, "")
raises("join of a number", "path.join: argument 2 (path) must be a string, not number", path.join, "a", 1)

checks.done("path")
