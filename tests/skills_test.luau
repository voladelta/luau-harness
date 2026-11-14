-- Skills: front matter, discovery, loading by name, search and the system prompt's index.
-- Run with: hx --run tests/skills_test.luau

local loop = assert(host.load(host.default_loop, "=loop"))()
host.quiet = true

local failures = 0
local function check(name, got, want)
	if got ~= want then
		failures += 1
		print(("FAIL %s\n  got:  %s\n  want: %s"):format(name, tostring(got), tostring(want)))
	end
end
local function has(name, text, fragment)
	if not tostring(text):find(fragment, 1, true) then
		failures += 1
		print(("FAIL %s\n  got:  %s\n  want it to contain: %s"):format(name, tostring(text), fragment))
	end
end
local function lacks(name, text, fragment)
	if tostring(text):find(fragment, 1, true) then
		failures += 1
		print(("FAIL %s\n  got:  %s\n  want it not to contain: %s"):format(name, tostring(text), fragment))
	end
end

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

-- Search ------------------------------------------------------------------------------------------------------

local found = loop.skill("excel spreadsheets")
has("a search names no skill", found, 'No skill is named "excel spreadsheets"')
has("words in a description match", found, "- xlsx: Create and edit Excel")
lacks("unrelated skills are left out", found, "release-notes")

found = loop.skill("relase notes")
check("a misspelt name finds the skill first", found:match("\n%- ([%w%-_]+):"), "release-notes")

found = loop.skill("merge pdf")
check("the name's words rank first", found:match("\n%- ([%w%-_]+):"), "pdf-tools")
has("and the other pdf skill follows", found, "- pdf_tools:")

found = loop.skill("only when")
lacks("hidden skills are not searched", found, "secret")

found = loop.skill("kubernetes")
has("no match says so", found, 'No skill is named "kubernetes" or matches it.')
has("and lists the skills when there are few", found, "- xlsx:")
lacks("but not hidden ones", found, "- secret:")

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
found = loop.skill("long words that match nothing else")
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

loop.skill_index_names = 3
prompt = loop.system_prompt()
has("very many: only the count", prompt, "There are 6; skill(words) searches")

loop.skill_dirs = { missing }
lacks("no skills, no part of the prompt", loop.system_prompt(), "Skills are instructions")

host.sh(("rm -rf '%s'"):format(root))

if failures > 0 then
	error(failures .. " failed", 0)
end
print("skills: all passed")
