local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, raises = checks.check, checks.has, checks.raises

local search = loop.ext("search")

check("the system prompt lists search", loop.system_prompt():find("- search (built in): find files and text", 1, true) ~= nil, true)

host.sh({ "rm", "-rf", "repo", "plain" })
host.sh({ "mkdir", "-p", "repo/src", "repo/.hx", "repo/out", "plain/sub" })
host.write("repo/.gitignore", "out\n")
host.write("repo/src/a.luau", "alpha\nbeta gamma\n")
host.write("repo/src/gone.luau", "beta\n")
host.write("repo/b.txt", "Beta\n1.5\n")
host.write("repo/.hx/n.luau", "beta\n")
host.write("repo/out/built.luau", "beta\n")
host.write("plain/sub/c.txt", "beta\n")
host.write("plain/d.luau", "x\n")
check("a repository to search", host.sh("git init -q && git add -A && rm src/gone.luau", { cwd = "repo" }).code, 0)

local function joined(paths: { string }): string
	return table.concat(paths, " ")
end

check("files are what git lists, less what is ignored or deleted", joined(search.files(nil, "repo")),
	"repo/.gitignore repo/.hx/n.luau repo/b.txt repo/src/a.luau")
check("a glob picks among them", joined(search.files("**/*.luau", "repo")), "repo/.hx/n.luau repo/src/a.luau")
check("a directory inside the repository", joined(search.files(nil, "repo/src")), "repo/src/a.luau")
check("where git lists nothing, every file", joined(search.files(nil, "plain")), "plain/d.luau plain/sub/c.txt")
check("with the glob", joined(search.files("**/*.txt", "plain")), "plain/sub/c.txt")
check("in a repository a glob that matches nothing is nothing, not what git ignores", joined(search.files("**/built.luau", "repo")), "")

for _, ripgrep in { true, false } do
	if not ripgrep or host.sh("command -v rg").code == 0 then
		search.ripgrep = ripgrep
		local by = ripgrep and "rg: " or "grep: "
		local found = search.grep("beta", { dir = "repo/src" })
		check(by .. "one match", #found, 1)
		check(by .. "its path", found[1].path, "repo/src/a.luau")
		check(by .. "its line", found[1].line, 2)
		check(by .. "its text, with no line end", found[1].text, "beta gamma")
		check(by .. "it prints as lines", tostring(found), "repo/src/a.luau:2: beta gamma")
		check(by .. "hidden directories are searched", tostring(search.grep("beta", { dir = "repo/.hx" })), "repo/.hx/n.luau:1: beta")
		check(by .. "case can be ignored", #search.grep("beta", { dir = "repo", glob = "*.txt", ignore_case = true }), 1)
		check(by .. "a pattern is a regex", #search.grep("1.5|nothing", { dir = "repo", glob = "*.txt" }), 1)
		check(by .. "unless fixed", tostring(search.grep("1.", { dir = "repo", fixed = true })), "repo/b.txt:2: 1.5")
		check(by .. "nothing found", tostring(search.grep("no such text", { dir = "repo" })), "[no match]")
		local some = search.grep("e", { dir = "plain/..", limit = 1 })
		check(by .. "the limit holds", #some, 1)
		has(by .. "and says there is more", tostring(some), "[more than these 1 match;")
		raises(by .. "a pattern that does not parse", "search.grep: ", search.grep, "(", { dir = "repo" })
	end
end
for _, ripgrep in { true, false } do
	if not ripgrep or host.sh("command -v rg").code == 0 then
		search.ripgrep = ripgrep
		local by = ripgrep and "rg: " or "grep: "
		check(by .. "skips what .gitignore names", #search.grep("beta", { dir = "repo", glob = "*.luau" }), 2)
	end
end
search.ripgrep = false
check("grep: a glob with a directory keeps to it", tostring(search.grep("beta", { dir = "repo", glob = "src/*.luau" })),
	"repo/src/a.luau:2: beta gamma")
host.write("repo/src/locked.luau", "beta\n")
host.sh({ "chmod", "000", "repo/src/locked.luau" })
if not host.read("repo/src/locked.luau") then
	for _, ripgrep in { true, false } do
		if not ripgrep or host.sh("command -v rg").code == 0 then
			search.ripgrep = ripgrep
			check((ripgrep and "rg" or "grep") .. ": a file it cannot read does not lose the matches", #search.grep("gamma", { dir = "repo/src" }), 1)
		end
	end
end
host.sh({ "rm", "-f", "repo/src/locked.luau" })
search.ripgrep = nil

raises("an empty pattern", "search.grep: argument 1 (pattern) is empty", search.grep, "")
raises("a pattern that is not text", "search.grep: argument 1 (pattern) must be a string, not nil", search.grep)
has("the lua tool shows matches as lines", loop.tools[1].run({ code = 'return ext("search").grep("gamma", { dir = "repo" })' }),
	"repo/src/a.luau:2: beta gamma")

checks.done("search")
