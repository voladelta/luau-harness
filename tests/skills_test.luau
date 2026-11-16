-- Skills: front matter, discovery, loading by name, the list for an unknown name, the system prompt's index, and
-- the skills built into hx.
-- Run with: hx --ro tests --run tests/skills_test.luau

local loop = assert(host.load(host.default_loop, "=loop"))()
host.quiet = true

-- hx has skills built in (host.skills), which every project gets. The checks of directories are made without
-- them; the last section puts them back.
local built_in = host.skills
host.skills = {}

-- check, has, lacks and the report at the end are shared, in check.luau beside this file.
local here = (debug.info(1, "s"):gsub("[^/]*$", ""))
local checks = assert(host.load(assert(host.read(here .. "check.luau")), "=" .. here .. "check.luau"))()
local check, has, lacks = checks.check, checks.has, checks.lacks

-- Front matter ------------------------------------------------------------------------------------------------

local fields, body = loop.skill_front_matter("---\nname: pdf-tools\ndescription: Reads PDFs.\n---\n# Body\n")
check("name", fields.name, "pdf-tools")
check("description", fields.description, "Reads PDFs.")
check("body", body, "# Body\n")

fields = loop.skill_front_matter('---\nname: "quoted \\"x\\""\ndescription: \'it\'\'s\'\n---\n')
check("double quotes", fields.name, 'quoted "x"')
check("single quotes", fields.description, "it's")

fields = loop.skill_front_matter("---\nname: a\ndescription: >\n  Folded over\n  two lines.\nlicense: MIT\n---\nx")
check("folded block", fields.description, "Folded over two lines.")
check("the key after a block", fields.license, "MIT")

fields = loop.skill_front_matter("---\ndescription: |\n  line one\n  line two\n---\n")
check("literal block", fields.description, "line one\nline two")

fields = loop.skill_front_matter("---\ndescription: starts here\n  and goes on\nname: n\n---\n")
check("a plain value continued", fields.description, "starts here and goes on")
check("the key after it", fields.name, "n")

fields, body = loop.skill_front_matter("\239\187\191---\r\nname: crlf\r\n---\r\nbody\r\n")
check("a BOM and \\r\\n line ends", fields.name, "crlf")
check("the body of such a file", body, "body\n")

fields, body = loop.skill_front_matter("no front matter")
check("no front matter, no fields", next(fields), nil)
check("no front matter, all body", body, "no front matter")

fields = loop.skill_front_matter("---\nname: nested\nmetadata:\n  author: me\n---\n")
check("a nested map does not leak a key", fields.author, nil)

-- Discovery ---------------------------------------------------------------------------------------------------

local root = (host.env("TMPDIR") or "/tmp") .. "/hx-skills-test-" .. tostring(os.clock()):gsub("%.", "")
local first, second, missing = root .. "/one/", root .. "/two/", root .. "/none/"
loop.skill_dirs = { first, second, missing }

check("no skills", loop.skill("anything"):find("There are no skills", 1, true) ~= nil, true)

local function skill(dir, name, front, text)
	assert(host.write(dir .. name .. "/SKILL.md", "---\n" .. front .. "\n---\n" .. (text or "Do the thing.\n")))
end
skill(first, "pdf-tools", "name: pdf-tools\ndescription: Read, split and merge PDF files.",
	"# PDF\n\nRun scripts/split.py.\n")
skill(first, "xlsx", "name: xlsx\ndescription: Create and edit Excel spreadsheets, with formulas and charts.")
skill(first, "release-notes", "name: release-notes\ndescription: Write release notes from the git log.")
skill(first, "secret", "name: secret\ndescription: Only when the user asks.\ndisable-model-invocation: true")
skill(first, "no-name", "description: The directory names it.")
skill(second, "xlsx", "name: xlsx\ndescription: A second xlsx, which loses to the first.")
skill(second, "pdf_tools", "name: pdf_tools\ndescription: Another PDF skill.")
assert(host.write(second .. "not-a-skill.md", "a file, not a directory"))
assert(host.write(second .. "empty/README.md", "a directory without SKILL.md"))

local skills = loop.skills()
local names = {}
for _, s in skills do
	table.insert(names, s.name)
end
check("found in order, the first of a name wins", table.concat(names, " "),
	"no-name pdf-tools release-notes secret xlsx pdf_tools")
check("the first xlsx wins", skills[5].description, "Create and edit Excel spreadsheets, with formulas and charts.")
check("a skill without a name takes its directory's", skills[1].name, "no-name")
check("hidden is read", skills[4].hidden, true)
check("the path", skills[2].path, first .. "pdf-tools/SKILL.md")

-- Loading -----------------------------------------------------------------------------------------------------

local loaded = loop.skill("xlsx")
has("an exact name loads", loaded, "[skill xlsx, in " .. first .. "xlsx/.")
has("the body is there", loaded, "Do the thing.")
lacks("the front matter is not", loaded, "description:")

lacks("a skill with no other files lists none", loaded, "Its files:")
assert(host.write(first .. "pdf-tools/scripts/split.py", "print('split')\n"))
assert(host.write(first .. "pdf-tools/reference/a/b/too-deep.md", "deep"))
loaded = loop.skill("pdf-tools")
has("a skill's files are listed by their paths", loaded, "Its files: " .. first .. "pdf-tools/scripts/split.py]")
lacks("three directories deep at most", loaded, "too-deep.md")
host.sh(("rm -rf '%spdf-tools/reference'"):format(first))

loaded = loop.skill("Release_Notes")
has("case, - and _ do not matter", loaded, "[skill release-notes,")

loaded = loop.skill("secret")
has("a hidden skill loads by name", loaded, "[skill secret,")

loaded = loop.skill("pdf-tools")
has("an exact name wins over a near one", loaded, "[skill pdf-tools,")
loaded = loop.skill("pdf_tools")
has("and so does the other", loaded, "[skill pdf_tools,")
loaded = loop.skill("PDFTools")
has("names alike are listed, not guessed", loaded, "Several skills have a name like")
has("with each path", loaded, second .. "pdf_tools/SKILL.md")

-- An unknown name ----------------------------------------------------------------------------------------------

local found = loop.skill("excel spreadsheets")
has("an unknown name says so", found, 'No skill is named "excel spreadsheets".')
has("and lists the skills to pick from", found, "- xlsx: Create and edit Excel")
has("all of them", found, "- release-notes: Write release notes")
lacks("but not hidden ones", found, "- secret:")
lacks("nor a second skill of a name", found, "A second xlsx")

loop.skill_listed = 2
found = loop.skill("kubernetes")
has("past M.skill_listed, the rest are counted", found, "and 3 more")
check("and not listed", select(2, found:gsub("\n%- ", "")), 2)
loop.skill_listed = 50

-- Through the function the model calls, which checks its argument.
local fn
for _, f in loop.functions do
	if f.name == "skill" then
		fn = f
	end
end
check("skill is a function", fn ~= nil, true)
has("the function trims", fn.run("  xlsx \n"), "[skill xlsx,")
check("a table is refused", pcall(fn.run, { name = "xlsx" }), false)
check("an empty name is refused", pcall(fn.run, "  "), false)
has("the lua tool reaches it", loop.tools[1].run({ code = "return skill('xlsx')" }), "[skill xlsx,")

-- A long description is cut in listings, on a character boundary.
skill(first, "long", "name: long\ndescription: " .. string.rep("é", 400))
found = loop.skill("a name no skill has")
has("a long description is cut", found, string.rep("é", loop.skill_description_limit) .. " ...")

-- The system prompt -------------------------------------------------------------------------------------------

local prompt = loop.system_prompt()
has("the prompt lists skills", prompt, "- pdf-tools: Read, split and merge PDF files.")
lacks("but not hidden ones", prompt, "- secret:")
has("and says how to load one", prompt, "skill(name)")

loop.skill_index_full = 2
prompt = loop.system_prompt()
has("many skills: names only", prompt, "There are 6: long, no-name, pdf-tools, release-notes, xlsx, pdf_tools.")
lacks("no descriptions then", prompt, "Read, split and merge")

loop.skill_index_full = 0
prompt = loop.system_prompt()
has("however many, still their names", prompt, "There are 6: long, no-name")
has("and how to see what they are for", prompt, "lists them with their descriptions")

loop.skill_dirs = { missing }
lacks("no skills, no part of the prompt", loop.system_prompt(), "Skills are instructions")
lacks("and no pointer to a skill that is not there", loop.system_prompt(), 'skill("hx")')

-- Built in ----------------------------------------------------------------------------------------------------

-- A built-in skill is the text of a SKILL.md, with no directory.
loop.skill_index_full = 30
host.skills = {
	xlsx = "---\nname: xlsx\ndescription: The built-in xlsx, which loses to a project's.\n---\nBuilt-in sheets.\n",
	tour = "---\ndescription: Shows a newcomer round.\n---\nStart at the door.\n",
}
check("with no directory, the built-in skills are the skills", #loop.skills(), 2)
has("the prompt lists one, marked", loop.system_prompt(), "- tour (built in): Shows a newcomer round.")
check("one without a name takes the name it is built in under, and loads with no directory", loop.skill("tour"),
	"[skill tour, built into hx. Every path in it is the project's.]\n\nStart at the door.")
has("case does not matter here either", loop.skill("Tour"), "Start at the door.")
has("an unknown name lists them, marked", loop.skill("nothing"), "- xlsx (built in): The built-in xlsx")

loop.skill_dirs = { first, second, missing }
names = {}
for _, s in loop.skills() do
	table.insert(names, s.name)
end
check("they come after every directory, and a directory's skill of a name takes the place of one",
	table.concat(names, " "), "long no-name pdf-tools release-notes secret xlsx pdf_tools tour")
has("the project's xlsx is the one that loads", loop.skill("xlsx"), "[skill xlsx, in " .. first .. "xlsx/.")
lacks("and the one that is listed", loop.skill("nothing"), "- xlsx (built in)")

-- The skill hx is built with, on using and extending hx itself.
host.skills = built_in
loop.skill_dirs = { missing }
fields = loop.skill_front_matter(built_in.hx or "")
check("hx is built in, under its own name", fields.name, "hx")
check("its description fits a listing uncut", (utf8.len(fields.description or "") or 0) <= loop.skill_description_limit,
	true)
check("and is there", (fields.description or "") ~= "", true)
loaded = loop.skill("hx")
has("it loads", loaded, "[skill hx, built into hx.")
check("and fits a tool result unclipped", #loaded < loop.output_limit, true)
has("the prompt lists it", loop.system_prompt(), "- hx (built in): ")
has("and points to it where it says how hx is extended", loop.system_prompt(),
	'skill("hx") says which of these a need calls for')

host.sh(("rm -rf '%s'"):format(root))

checks.done("skills")
