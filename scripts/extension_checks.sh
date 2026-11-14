#!/usr/bin/env bash
# Extension checks, no API key: skills, tool packages, no self-approval, frozen extensions with live session state. Build first: cargo build --release
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
cleanup() { kill $MOCK 2>/dev/null; pkill -f "sleep 4243" 2>/dev/null; }
MOCK=
W=$T/ws; mkdir -p $W/.harness/pkgs/release $W/.harness/pkgs/slug
x() { $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }
# the owner's own trust store, not the per-launch HX_TRUST=current the other checks use
owner() { HX_TRUST= $H "$@"; }
xo() { owner exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }

# --- the user installs a skill and a tool package (plain files; nothing is active until the workspace is trusted)
cat > $W/.harness/pkgs/release/hx.toml <<'TOML'
[package]
name = "release"
version = "0.1.0"
kind = "skill"
description = "how to cut a release"
TOML
printf -- '---\nname: release\ndescription: how to cut a release\n---\nSteps: 1. bump version 2. tag it.\n' > $W/.harness/pkgs/release/SKILL.md
echo 'return { checklist = function() return { "bump", "tag" } end }' > $W/.harness/pkgs/release/init.luau
cat > $W/.harness/pkgs/slug/hx.toml <<'TOML'
[package]
name = "slug"
version = "0.1.0"
kind = "tool"
description = "slugify text"
[tool]
signature = "slug.run(s: string): string"
doc = "lowercase, dashes for non-alphanumerics"
TOML
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "-")) end }' > $W/.harness/pkgs/slug/init.luau

check "untrusted packages: exec refuses, naming them" "$(xo 'print(tools)' | grep -c 'not trusted (.harness/pkgs/release, .harness/pkgs/slug)')" "1"
check "untrusted packages: --ignore-config loads neither" "$(xo --ignore-config 'print(tools.slug, pcall(skill, "release"))' | head -1 | grep -c '^nil	false.*no installed skill')" "1"
out=$(owner trust --workspace $W --yes)
check "the trust review lists a package with its kind, capabilities and file count" "$(echo "$out" | grep -c '^slug  *package, kind tool, version 0.1.0, no capabilities, 2 files (.harness/pkgs/slug)$')" "1"
check "tool package: tools.slug.run works once trusted" "$(xo 'print(tools.slug.run("Hello World"))' | head -1)" "hello-world"
out=$(x 'local s = skill("release") print(s.text) print(s.module.checklist()[1], s.name)')
check "skill: text without frontmatter" "$(echo "$out" | sed -n 1p)" "Steps: 1. bump version 2. tag it."
check "skill: runnable Luau module" "$(echo "$out" | sed -n 3p)" "bump	release"
B=$T/bws; mkdir -p $B/.harness/pkgs/broken  # keep this broken package isolated from the other fixtures
printf '[package]\nname = "broken"\nversion = "0.1.0"\nkind = "skill"\ndescription = "a skill whose module does not compile"\n' > $B/.harness/pkgs/broken/hx.toml
printf -- '---\nname: broken\ndescription: a skill whose module does not compile\n---\nStill readable.\n' > $B/.harness/pkgs/broken/SKILL.md
echo 'return { go = function() return 1 end' > $B/.harness/pkgs/broken/init.luau
out=$($H exec --workspace $B 'local s = skill("broken") print(s.text) print(pcall(function() return s.module.go() end))' 2>&1 | grep -v '^-- ops=')
check "skill with a broken module: instructions still load" "$(echo "$out" | sed -n 1p)" "Still readable."
check "skill with a broken module: using it raises the load error" "$(echo "$out" | grep -c 'its Luau module failed to load: syntax error')" "1"
echo 'return { run = function(s) return "tampered" end }' > $W/.harness/pkgs/slug/init.luau
check "a tampered package untrusts the workspace, and the refusal names it" "$(xo 'print(tools.slug)' | grep -c 'changed since you trusted it: .harness/pkgs/slug (changed)')" "1"
check "hx trust names what changed before asking" "$(owner trust --workspace $W </dev/null 2>&1 | grep -c '^changed since you last trusted it: .harness/pkgs/slug (changed)$')" "1"
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "-")) end }' > $W/.harness/pkgs/slug/init.luau
check "restoring the trusted content brings it back" "$(xo 'print(tools.slug.run("A B"))' | head -1)" "a-b"

# --- the prompt shows installed extensions (and nothing when there are none)
MOCK_REPLIES='["done"]' MOCK_BY_LEN=1 MOCK_LOG=$T/req.jsonl serve MP python3 scripts/mock_llm.py 0 || exit 1; MOCK=$SERVED
echo go > $T/p.txt
run() { OPENROUTER_API_KEY=x $H run --workspace $1 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY >/dev/null 2>&1; }
system_of_last() { python3 -c "
import json
l=[json.loads(x) for x in open('$T/req.jsonl')][-1]
print(l['instructions'])"; }
run $W
s=$(system_of_last)
check "prompt lists the tool with its signature" "$(echo "$s" | grep -c 'tools.slug.run(s: string): string  -- lowercase, dashes')" "1"
check "prompt does not list skills (no catalog)" "$(echo "$s" | grep -c 'how to cut a release')" "0"
check "prompt says how to search and load skills" "$(echo "$s" | grep -c 'skills(\"words\") searches')" "1"
E=$T/empty; mkdir -p $E; run $E
check "no extensions installed: the prompt is unchanged" "$(system_of_last | grep -c 'Installed extensions')" "0"

# --- file tools cannot change installed extensions
check "cannot write into .harness/pkgs" "$(x 'print(pcall(write, ".harness/pkgs/evil/hx.toml", "x"))' | head -1 | grep -c protected)" "1"
check "cannot write into .agents/skills" "$(x 'print(pcall(write, ".agents/skills/evil/SKILL.md", "x"))' | head -1 | grep -c protected)" "1"
check "no accept/approve function exists for the agent" "$(x 'print(type(accept), type(approve), type(hx), type(require))' | head -1)" "nil	nil	nil	nil"

# --- extensions stay frozen when a user edits and re-trusts a tool during a session
( x 'counter = 7
print(tools.slug.run("A B"))
sh("touch extension-ready; while [ ! -e .harness/proceed ]; do sleep 0.01; done", { timeout = 5 })' 'print(counter, tools.slug.run("A B"))' > $T/hot.out ) &
HP=$!
wait_for_file "$W/extension-ready" || exit 1
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "_")) end }' > $W/.harness/pkgs/slug/init.luau
owner trust --workspace $W --yes >/dev/null
touch "$W/.harness/proceed"
wait $HP
check "frozen extensions: original source runs before the edit" "$(sed -n 1p $T/hot.out)" "a-b"
check "frozen run: session variable and original source survive trust changes" "$(grep -v '^$' $T/hot.out | tail -1)" "7	a-b"

# --- agent skills: <ws>/.agents/skills (part of the workspace config, so trusted with it) and ~/.agents/skills (the
# user's own files: no approval, and no file tool or grant can write them)
A=$T/aws; FH=$T/home; mkdir -p $A/.agents/skills/vid $FH/.agents/skills/vid/references $FH/.agents/skills/craft/references
xa() { HOME=$FH $H exec --workspace $A "$@" 2>&1 | grep -v "^-- ops="; }
xao() { HOME=$FH owner exec --workspace $A "$@" 2>&1 | grep -v "^-- ops="; }
printf -- '---\nname: vid\ndescription: project copy\n---\nproject vid\n' > $A/.agents/skills/vid/SKILL.md
printf -- '---\nname: vid\ndescription: global copy\n---\nglobal vid\n' > $FH/.agents/skills/vid/SKILL.md
printf -- '---\nname: craft\ndescription: >\n  plan and cut\n  short videos\n---\nRead references/beats.md first.\n' > $FH/.agents/skills/craft/SKILL.md
echo "beat list" > $FH/.agents/skills/craft/references/beats.md
check "hx skills lists project then user skills with their state" "$(HOME=$FH owner skills --workspace $A | awk '{print $1, $2, $3}' | tr '\n' ';')" "vid not trusted;craft user skill;vid shadowed by;"
check "a workspace skill needs trust: exec refuses, naming it" "$(xao 'print(1)' | grep -c 'not trusted (.agents/skills/vid)')" "1"
check "--ignore-config: user skills load without approval, the workspace's do not" "$(xao --ignore-config 'print(skill("craft").name, skill("vid").text)' | head -1)" "craft	global vid"
HOME=$FH owner trust --workspace $A --yes >/dev/null
out=$(xao 'local s = skill("craft") print(s.text) print(s.files[1], s.files[2], s.module) print(read(s.dir .. "/" .. s.files[2]))')
check "agent skill: text without frontmatter" "$(echo "$out" | sed -n 1p)" "Read references/beats.md first."
check "agent skill: files listed, no module" "$(echo "$out" | sed -n 3p)" "SKILL.md	references/beats.md	nil"
check "agent skill: reference file readable via dir" "$(echo "$out" | sed -n 4p)" "beat list"
check "agent skill: readable from sh in its frozen copy" "$(xa "print(sh('cat ' .. skill('craft').dir .. '/references/beats.md').stdout)" | head -1)" "beat list"
check "agent skill: the live directory is not a read root" "$(xa "print(pcall(read, '$FH/.agents/skills/craft/SKILL.md'))" | head -1 | grep -c 'outside the allowed roots')" "1"
check "user skills: a file tool cannot write them" "$(xa "print(pcall(write, '$FH/.agents/skills/craft/x.md', 'x'))" | head -1 | grep -c protected)" "1"
check "user skills: not even under --yes, which grants single files" "$(HOME=$FH $H exec --yes --workspace $A "print(pcall(write, '$FH/.agents/skills/craft/SKILL.md', 'planted'))" 2>&1 | head -1 | grep -c protected)$(grep -c planted $FH/.agents/skills/craft/SKILL.md)" "10"
check "skills(query) finds by description, best first" "$(xa 'for _, h in skills("short videos") do print(h.name) end' | head -1)" "craft"
check "skills() with no query lists every skill" "$(xa 'print(#skills())' | head -1)" "2"
check "skill(\"/name\") as the user types it" "$(xa 'print(skill("/craft").name)' | head -1)" "craft"
check "unknown skill suggests the closest" "$(xa 'print(pcall(skill, "crafts"))' | head -1 | grep -c 'closest: craft')" "1"
check "project .agents/skills shadows the user's" "$(xao 'print(skill("vid").text)' | head -1)" "project vid"
echo "edited" >> $A/.agents/skills/vid/SKILL.md
check "an edited workspace skill untrusts the workspace, and the refusal names it" "$(xao 'print(1)' | grep -c 'changed since you trusted it: .agents/skills/vid (changed)')" "1"
check "hx skills reports the change" "$(HOME=$FH owner skills --workspace $A | grep -c '^vid  *changed since trust')" "1"
echo "secret" > $T/secret.txt; mkdir -p $FH/.agents/skills/linky; printf -- '---\ndescription: links out\n---\nx\n' > $FH/.agents/skills/linky/SKILL.md
ln -s $T/secret.txt $FH/.agents/skills/linky/leak.md
check "a skill containing a symlink is not loaded" "$(xa 'print(pcall(skill, "linky"))' | head -1 | grep -c 'no installed skill')" "1"
check "hx skills reports it" "$(HOME=$FH owner skills --workspace $A | grep -c '^linky .*error: .*may not contain symlinks')" "1"
check "a symlink inside a skill is not readable (read)" "$(xa "print(pcall(read, '$FH/.agents/skills/linky/leak.md'))" | head -1 | grep -c 'outside the allowed roots')" "1"
mkdir -p $A/.harness/pkgs/vid; printf '[package]\nversion = "1.0.0"\nkind = "skill"\ndescription = "pkg copy"\n' > $A/.harness/pkgs/vid/hx.toml; printf 'pkg vid\n' > $A/.harness/pkgs/vid/SKILL.md
check ".harness/pkgs shadows an agent skill of the same name" "$(xa 'print(skill("vid").text)' | head -1)" "pkg vid"

# --- an agent skill carries no Luau and no capabilities, whatever files it holds
mkdir -p $A/.agents/skills/sneaky; printf -- '---\ndescription: sneaky\n---\nx\n' > $A/.agents/skills/sneaky/SKILL.md
printf '[package]\nversion = "1"\nkind = "tool"\n[capabilities]\nproc = ["sh"]\n' > $A/.agents/skills/sneaky/hx.toml
echo 'return { run = function() return sh("echo RAN").stdout end }' > $A/.agents/skills/sneaky/init.luau
check "the trust review says an agent skill has no Luau and no capabilities" "$(HOME=$FH owner trust --workspace $A </dev/null 2>&1 | grep -c '^sneaky  *agent skill, no Luau and no capabilities (its hx.toml is an ordinary file), 3 files')" "1"
check "an agent skill's hx.toml grants no tool namespace" "$(xa 'print(tools.sneaky, skill("sneaky").module)' | head -1)" "nil	nil"

# --- a linked skill directory is trusted and loaded by its content; its .git is not part of it
mkdir -p $T/src/linked/.git; printf -- '---\ndescription: linked\n---\nlinked body\n' > $T/src/linked/SKILL.md; echo x > $T/src/linked/.git/HEAD
ln -s $T/src/linked $A/.agents/skills/linked; HOME=$FH owner trust --workspace $A --yes >/dev/null
check "a symlinked skill directory loads once trusted" "$(xao 'print(skill("linked").text)' | head -1)" "linked body"
echo y > $T/src/linked/.git/ORIG_HEAD
check "git activity in a skill does not untrust it" "$(HOME=$FH owner skills --workspace $A | grep -c '^linked .* trusted')" "1"
echo changed >> $T/src/linked/SKILL.md
check "editing a linked skill's content untrusts the workspace" "$(xao 'print(1)' | grep -c '.agents/skills/linked (changed)')" "1"

# --- hx test runs a package's tests inside its boundary
S=$T/sneak; mkdir -p $S/tests
printf '[package]\nname = "sneak"\nversion = "0.1.0"\n' > $S/hx.toml; echo 'return {}' > $S/init.luau
echo 'test.case("escape", function() sh("touch pwned") end) test.case("lib", function() test.eq(require("sneak").x, nil) end)' > $S/tests/t.luau
out=$($H test $S --workspace $W 2>&1)
check "hx test: a test cannot use what the package does not declare" "$(echo "$out" | grep -c 'capability_denied')$(echo "$out" | tail -1)$([ -e $W/pwned ] && echo pwned)" "11 passed, 1 failed"
mkdir -p $W/.harness/bare/tests; echo 'test.case("x", function() end)' > $W/.harness/bare/tests/t.luau
check "hx test: a directory in .harness without hx.toml is refused" "$($H test $W/.harness/bare --workspace $W 2>&1 | grep -c 'not a package')" "1"

# --- only the suite compiled into hx runs as the built-in suite (with the agent's authority)
W3=$T/w3; mkdir -p $W3; cp -r stdlib/std $W3/std
echo 'print("PLANTED TEST RAN"); sh("echo pwned > PWNED")' > $W3/std/tests/zz_test.luau
out=$(cd $W3 && $H test std 2>&1)
check "hx test: a planted copy of std is refused, its test never runs" "$(echo "$out" | grep -c 'calls itself std')$(echo "$out" | grep -c 'PLANTED')$(ls $W3/PWNED $W3/std/PWNED 2>/dev/null | wc -l | tr -d ' ')" "100"
check "hx test stdlib/std runs the built-in suite" "$($H test stdlib/std --workspace $W3 2>&1 | tail -1 | grep -c ' passed, 0 failed')" "1"
finish
