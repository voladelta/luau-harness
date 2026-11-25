local loop = assert(host.load(host.default_loop, "=loop"))()

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, raises = checks.check, checks.has, checks.raises

local search = loop.ext("search")

host.sh({ "rm", "-rf", "repo", "plain" })
host.sh({ "mkdir", "-p", "repo/src", "repo/.hx", "repo/out", "plain/sub" })
host.write("repo/.gitignore", "out\n")
host.write("repo/src/a.luau", "alpha\nbeta gamma\n")
host.write("repo/src/gone.luau", "beta\n")
host.write("repo/b.txt", "Beta\n1.5\n1x5\n")
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
check("a glob without a slash matches nested files in git", joined(search.files("*.luau", "repo")), "repo/.hx/n.luau repo/src/a.luau")
host.write("repo/root.luau", "root\n")
check("a glob without a slash matches root and nested files in git", joined(search.files("*.luau", "repo")),
	"repo/.hx/n.luau repo/root.luau repo/src/a.luau")
host.sh({ "rm", "-f", "repo/root.luau" })
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
		check(by .. "a pattern is an extended regex", tostring(search.grep("1.5|nothing", { dir = "repo" })), "repo/b.txt:2: 1.5\nrepo/b.txt:3: 1x5")
		check(by .. "unless fixed", tostring(search.grep("1.5", { dir = "repo", fixed = true })), "repo/b.txt:2: 1.5")
		check(by .. "nothing found", tostring(search.grep("no such text", { dir = "repo" })), "[no match]")
		local some = search.grep("e", { dir = "plain/..", limit = 1 })
		check(by .. "the limit holds", #some, 1)
		has(by .. "and says there is more", tostring(some), "[more than these 1 match;")
		raises(by .. "a pattern that does not parse", "search.grep: ", search.grep, "(", { dir = "repo" })
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
			local by = ripgrep and "rg" or "grep"
			check(by .. ": a file it cannot read does not lose the matches", #search.grep("gamma", { dir = "repo/src" }), 1)
			check(by .. ": a file it cannot read does not break a zero-match search",
				tostring(search.grep("no-such-pattern", { dir = "repo/src" })), "[no match]")
		end
	end
end
host.sh({ "rm", "-f", "repo/src/locked.luau" })
assert(host.write("plain/long.txt", "needle" .. string.rep("x", 20 * 1024 * 1024) .. "\nneedle\n"))
assert(host.write("plain/a\nb.txt", "needle\n"))
for _, ripgrep in { true, false } do
	if not ripgrep or host.sh("command -v rg").code == 0 then
		search.ripgrep = ripgrep
		local by = ripgrep and "rg: " or "grep: "
		local unusual = search.grep("needle", { dir = "plain", glob = "a*.txt" })
		assert(unusual[1] and unusual[1].path == "plain/a\nb.txt", by .. "a newline is part of the filename")
		local long = search.grep("needle", { dir = "plain", glob = "long.txt", limit = 1 })
		assert(long[1] and long[1].text == "needle" .. string.rep("x", 494) .. " ...", by .. "a long line must return its bounded preview")
		check(by .. "the next match still marks the limited result", long.more, true)
		search.text_limit = 20 * 1024 * 1024
		raises(by .. "clipped output is never silently parsed", "shell output was clipped", search.grep, "needle",
			{ dir = "plain", glob = "long.txt" })
		search.text_limit = 500
	end
end
search.ripgrep = nil

raises("an empty pattern", "search.grep: argument 1 (pattern) is empty", search.grep, "")
raises("a pattern that is not text", "search.grep: argument 1 (pattern) must be a string, not nil", search.grep)
has("the lua tool shows matches as lines", loop.tools[1].run({ code = 'return ext("search").grep("gamma", { dir = "repo" })' }),
	"repo/src/a.luau:2: beta gamma")

assert(host.write("repo/src/conflicted.txt", "merge-token one\nmerge-token two\n"))
local hash = host.sh({ "git", "hash-object", "-w", "src/conflicted.txt" }, { cwd = "repo" }).out:match("%x+")
assert(hash)
local stages = {}
for stage = 1, 3 do
	table.insert(stages, ("100644 %s %d\tsrc/conflicted.txt\n"):format(hash, stage))
end
assert(host.sh({ "git", "update-index", "--index-info" }, { cwd = "repo", input = table.concat(stages) }).code == 0)
check("an unmerged path is listed once", joined(search.files("src/conflicted.txt", "repo")), "repo/src/conflicted.txt")
search.ripgrep = false
local conflict = search.grep("merge-token", { dir = "repo", glob = "src/conflicted.txt", limit = 2 })
check("grep reads an unmerged path once, in line order", tostring(conflict),
	"repo/src/conflicted.txt:1: merge-token one\nrepo/src/conflicted.txt:2: merge-token two")
check("duplicate index stages do not count as extra matches", conflict.more, nil)

host.sh({ "ln", "-s", "src", "repo/dirlink" })
check("add tracked directory symlink", host.sh({ "git", "-C", "repo", "add", "dirlink" }).code, 0)
for _, ripgrep in { true, false } do
	if not ripgrep or host.sh("command -v rg").code == 0 then
		search.ripgrep = ripgrep
		local by = ripgrep and "rg" or "grep"
		check(by .. ": a tracked directory symlink does not break a zero-match search",
			tostring(search.grep("no-such-pattern", { dir = "repo" })), "[no match]")
	end
end
search.ripgrep = nil
host.sh({ "git", "-C", "repo", "rm", "-fq", "dirlink" })

checks.done("search")
