local loop = assert(host.load(host.default_loop, "=loop"))()
host.quiet = true

local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, lacks = checks.check, checks.has, checks.lacks

local paths = { "README.md", "src/main.c", "src/util/text.c", "src/util/list.c", "docs/guide.md", ".gitignore" }

check("all of it, when it fits: directories first, then files, each by name", loop.tree(paths, 60), table.concat({
	"docs/",
	"  guide.md",
	"src/",
	"  util/",
	"    list.c",
	"    text.c",
	"  main.c",
	".gitignore",
	"README.md",
}, "\n"))

check("a level is opened before the one below it", loop.tree(paths, 8), table.concat({
	"docs/",
	"  guide.md",
	"src/",
	"  util/ (2 files)",
	"  main.c",
	".gitignore",
	"README.md",
}, "\n"))

check("a directory that does not fit stays closed, and a smaller one after it still opens", loop.tree(paths, 6),
	table.concat({ "docs/", "  guide.md", "src/ (3 files)", ".gitignore", "README.md" }, "\n"))

check("only the top level", loop.tree(paths, 4),
	table.concat({ "docs/ (1 file)", "src/ (3 files)", ".gitignore", "README.md" }, "\n"))

check("less than the top level: its first entries, and how many more", loop.tree(paths, 3),
	table.concat({ "docs/ (1 file)", "src/ (3 files)", ".gitignore", "... and 1 more" }, "\n"))

local big = {}
for i = 1, 500 do
	table.insert(big, ("vendor/lib/file%03d.c"):format(i))
end
table.insert(big, "app/main.c")
table.insert(big, "Makefile")
check("a large directory is one line, whatever is in it", loop.tree(big, 60),
	table.concat({ "app/", "  main.c", "vendor/", "  lib/ (500 files)", "Makefile" }, "\n"))

check("a path that ends in / is a directory with nothing known to be in it",
	loop.tree({ "build/", "notes.txt", "src/" }, 60), "build/\nsrc/\nnotes.txt")

check("a path listed twice in a row counts once", loop.tree({ "a/x", "a/x", "b" }, 1), "a/ (1 file)\n... and 1 more")

check("no paths, no tree", loop.tree({}, 60), "")

local tree = loop.tree({ "odd\nname\27[2J.txt" }, 60)
check("a line break in a name is a space, and an escape character goes", tree, "odd name[2J.txt")

assert(host.write("hx-layout-test/file.txt", "x"))
loop.layout_lines = 100000
has("the layout names what is in the project directory", loop.layout(), "hx-layout-test/")
has("the prompt has the layout", loop.system_prompt(), "hx-layout-test/")
has("and says when it was made", loop.system_prompt(), "as this session starts")

loop.layout_lines = 0
check("no lines, no layout", loop.layout(), nil)
lacks("and none in the prompt", loop.system_prompt(), "hx-layout-test")

loop.layout = function()
	return "Files: just the one."
end
has("a layout the project replaced", loop.system_prompt(), "Files: just the one.")

host.sh("rm -rf hx-layout-test")

checks.done("layout")
