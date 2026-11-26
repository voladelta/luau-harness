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
			local found = search.grep("gamma", { dir = "repo/src" })
			check(by .. ": a file it cannot read does not lose the matches", #found, 1)
			check(by .. ": and is named among them", tostring(found),
				"repo/src/a.luau:2: beta gamma\n[could not read 1 file: repo/src/locked.luau]")
			check(by .. ": a file it cannot read does not break a zero-match search, and is named",
				tostring(search.grep("no-such-pattern", { dir = "repo/src" })),
				"[no match]\n[could not read 1 file: repo/src/locked.luau]")
			raises(by .. ": a pattern that does not parse still raises beside it", "search.grep: ", search.grep, "(",
				{ dir = "repo/src" })
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
raises("non-boolean fixed option", "search.grep: argument 2 (options.fixed) must be a boolean, not string",
	search.grep, "x", { fixed = "false" :: any })
raises("non-boolean ignore_case option", "search.grep: argument 2 (options.ignore_case) must be a boolean, not string",
	search.grep, "x", { ignore_case = "true" :: any })
raises("files in missing dir", "search.files: no-such-dir: No such file or directory", search.files, nil, "no-such-dir")
raises("files on regular file", "search.files: repo/b.txt is not a directory", search.files, nil, "repo/b.txt")
for _, ripgrep in { true, false } do
	if not ripgrep or host.sh("command -v rg").code == 0 then
		search.ripgrep = ripgrep
		raises("grep in missing dir", "search.grep: no-such-dir: No such file or directory", search.grep, "x", { dir = "no-such-dir" })
		raises("grep on regular file", "search.grep: repo/b.txt is not a directory", search.grep, "x", { dir = "repo/b.txt" })
	end
end
search.ripgrep = nil
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

local real_sh = host.sh
local function with_locale(locale: string, fn: () -> ())
	host.sh = function(command: any, options: any): any
		local merged = if options then table.clone(options) else {}
		local env = if merged.env then table.clone(merged.env) else {}
		env.LANG = locale
		env.LC_ALL = locale
		env.LC_MESSAGES = locale
		merged.env = env
		return real_sh(command, merged)
	end
	local ok, err = pcall(fn)
	host.sh = real_sh
	if not ok then
		error(err, 0)
	end
end

local locales = { "C.UTF-8" }
if host.sh("LC_ALL=en_US.UTF-8 locale charmap 2>/dev/null").out:find("UTF-8", 1, true) then
	table.insert(locales, "en_US.UTF-8")
end

assert(host.write("repo/src/locale.txt", "alpha\nmixed\nBeta\nÉlan\ná\nzebra\n"))
host.write("repo/src/locked.luau", "beta\n")
host.sh({ "chmod", "000", "repo/src/locked.luau" })
local can_lock = not host.read("repo/src/locked.luau")

for _, locale in locales do
	with_locale(locale, function()
		search.ripgrep = false
		check("grep (" .. locale .. "): [A-Z] matches only ASCII uppercase",
			tostring(search.grep("[A-Z]", { dir = "repo/src", glob = "locale.txt" })),
			"repo/src/locale.txt:3: Beta")
		if can_lock then
			local found = search.grep("gamma", { dir = "repo/src" })
			check("grep (" .. locale .. "): unreadable file does not lose matches", #found, 1)
			check("grep (" .. locale .. "): unreadable file is named among matches", tostring(found),
				"repo/src/a.luau:2: beta gamma\n[could not read 1 file: repo/src/locked.luau]")
			check("grep (" .. locale .. "): unreadable file is named on zero-match",
				tostring(search.grep("no-such-pattern", { dir = "repo/src" })),
				"[no match]\n[could not read 1 file: repo/src/locked.luau]")
		end
		if host.sh("command -v rg").code == 0 then
			search.ripgrep = true
			check("rg (" .. locale .. "): [A-Z] matches only ASCII uppercase",
				tostring(search.grep("[A-Z]", { dir = "repo/src", glob = "locale.txt" })),
				"repo/src/locale.txt:3: Beta")
			check("rg (" .. locale .. "): keeps Unicode case folding",
				tostring(search.grep("élan", { dir = "repo/src", glob = "locale.txt", ignore_case = true })),
				"repo/src/locale.txt:4: Élan")
		end
	end)
end

search.ripgrep = nil
host.sh({ "rm", "-f", "repo/src/locale.txt", "repo/src/locked.luau" })

host.sh({ "rm", "-rf", "wt" })
host.sh({ "mkdir", "-p", "wt/sub/locked_dir" })
assert(host.write("wt/.git", "gitdir: /nope\n"))
assert(host.write("wt/sub/item.txt", "ancestor-token\n"))
assert(host.write("wt/sub/locked.txt", "ancestor-token\n"))
assert(host.write("wt/sub/locked_dir/hidden.txt", "ancestor-token\n"))
host.sh({ "chmod", "000", "wt/.git", "wt/sub/locked.txt", "wt/sub/locked_dir" })
if not host.read("wt/.git") and host.sh("command -v rg").code == 0 then
	search.ripgrep = true
	check("rg: ignores unreadable ancestor .git file while keeping target unreadable",
		tostring(search.grep("ancestor-token", { dir = "wt/sub" })),
		"wt/sub/item.txt:1: ancestor-token\n[could not read 2 files: wt/sub/locked.txt, wt/sub/locked_dir]")
	check("rg: ignores unreadable ancestor .git file on zero-match",
		tostring(search.grep("no-such-pattern", { dir = "wt/sub" })),
		"[no match]\n[could not read 2 files: wt/sub/locked.txt, wt/sub/locked_dir]")
	local abs_sub = (host.sh({ "pwd" }, { cwd = "wt/sub" }).out:gsub("\n$", ""))
	check("rg: ignores unreadable ancestor .git file for absolute dir",
		tostring(search.grep("ancestor-token", { dir = abs_sub })),
		abs_sub .. "/item.txt:1: ancestor-token\n[could not read 2 files: " .. abs_sub .. "/locked.txt, " .. abs_sub .. "/locked_dir]")
	check("rg: ignores unreadable ancestor .git file on zero-match for absolute dir",
		tostring(search.grep("no-such-pattern", { dir = abs_sub .. "//" })),
		"[no match]\n[could not read 2 files: " .. abs_sub .. "/locked.txt, " .. abs_sub .. "/locked_dir]")
	search.ripgrep = nil
end
host.sh({ "chmod", "755", "wt/sub/locked_dir" })
host.sh({ "chmod", "644", "wt/.git", "wt/sub/locked.txt" })
host.sh({ "rm", "-rf", "wt" })

checks.done("search")
