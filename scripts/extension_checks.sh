#!/usr/bin/env bash
# Extension checks, no API key: skills, tool packages, agent-written tools (deftool/promote/accept),
# no self-approval, frozen extensions with live session state. Build first: cargo build --release
set -u
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $MOCK 2>/dev/null; pkill -f "sleep 4243" 2>/dev/null' EXIT
MOCK=
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/ws; mkdir -p $W/.harness/pkgs/release $W/.harness/pkgs/slug
x() { $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }
# agent-written tools are an opt-in pack: xt runs with it
xt() { x --pack std/authoring "$@"; }

# --- the user installs a skill and a tool package (plain files; nothing is active until approved)
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

check "an unapproved tool is not visible" "$(x 'print(tools.slug)' | head -1)" "nil"
check "an unapproved skill is not visible" "$(x 'print(pcall(skill, "release"))' | head -1 | grep -c 'no installed skill')" "1"
$H approve release --workspace $W >/dev/null; $H approve slug --workspace $W >/dev/null
check "tool package: tools.slug.run works after approval" "$(x 'print(tools.slug.run("Hello World"))' | head -1)" "hello-world"
out=$(x 'local s = skill("release") print(s.text) print(s.module.checklist()[1], s.name)')
check "skill: text without frontmatter" "$(echo "$out" | sed -n 1p)" "Steps: 1. bump version 2. tag it."
check "skill: runnable Luau module" "$(echo "$out" | sed -n 3p)" "bump	release"
B=$T/bws; mkdir -p $B/.harness/pkgs/broken  # keep this broken package isolated from the other fixtures
printf '[package]\nname = "broken"\nversion = "0.1.0"\nkind = "skill"\ndescription = "a skill whose module does not compile"\n' > $B/.harness/pkgs/broken/hx.toml
printf -- '---\nname: broken\ndescription: a skill whose module does not compile\n---\nStill readable.\n' > $B/.harness/pkgs/broken/SKILL.md
echo 'return { go = function() return 1 end' > $B/.harness/pkgs/broken/init.luau; $H approve broken --workspace $B >/dev/null
out=$($H exec --workspace $B 'local s = skill("broken") print(s.text) print(pcall(function() return s.module.go() end))' 2>&1 | grep -v '^-- ops=')
check "skill with a broken module: instructions still load" "$(echo "$out" | sed -n 1p)" "Still readable."
check "skill with a broken module: using it raises the load error" "$(echo "$out" | grep -c 'its Luau module failed to load: syntax error')" "1"
echo 'return { run = function(s) return "tampered" end }' > $W/.harness/pkgs/slug/init.luau
check "a tampered tool package disappears (hash no longer matches the approval)" "$(x 'print(tools.slug)' | head -1)" "nil"
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "-")) end }' > $W/.harness/pkgs/slug/init.luau
check "restoring the approved content brings it back" "$(x 'print(tools.slug.run("A B"))' | head -1)" "a-b"

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

# --- Definitions stay inactive; promotion never executes unapproved source or tests.
check "authoring remains opt-in" "$(x 'print(type(deftool),type(promote))' | head -1)" "nil	nil"
out=$(xt 'local d=deftool("peek", [==[write("unapproved.txt","bad"); return {}]==], {caps={fs={"write"}}}); print(d.active,tools.peek)')
check "definition does not execute or activate" "$(echo "$out" | head -1) $([ -e $W/unapproved.txt ] && echo bad || echo safe)" "false	nil safe"
check "definitions do not pollute vars" "$(xt 'deftool("t1","return {}"); x=1; print(vars().tools,vars().deftool,vars().x)' | head -1)" "nil	nil	1"
TOOL='deftool("shout", [==[return {run=function(s) return s:upper() .. "!" end}]==], {
 signature="shout.run(s: string): string",doc="shouts",tests=[==[
 local m=require("shout");test.case("shouts",function() test.eq(m.run("hi"),"HI!") end)
 ]==]})'
out=$(xt "$TOOL"$'\nlocal r=promote("shout"); print(r.ok,r.tested); print(r.message)')
check "promotion stages without execution" "$(echo "$out" | head -1)" "true	false"
check "promotion explains owner testing and acceptance" "$(echo "$out" | grep -c 'hx accept shout')" "1"
check "staged source is not installed" "$([ -d $W/.harness/staged/shout ] && [ ! -d $W/.harness/pkgs/shout ] && echo yes || echo no)" "yes"
check "a new session cannot see staged source" "$(x 'print(tools.shout)' | head -1)" "nil"
check "owner can run staged tests" "$($H test $W/.harness/staged/shout --workspace $W | tail -1)" "1 passed, 0 failed"
$H accept shout --yes --workspace $W >/dev/null 2>&1
check "accepted source is available to a new run" "$(x 'print(tools.shout.run("ok"))' | head -1)" "OK!"
out=$(xt 'deftool("bad","return {}",{tests=[==[write("unapproved-test.txt","bad")]==]}); local r=promote("bad"); print(r.ok,r.tested)')
check "unapproved tests are not executed by promotion" "$(echo "$out" | head -1) $([ -e $W/unapproved-test.txt ] && echo bad || echo safe)" "true	false safe"
out=$(xt 'deftool("untested","return {}"); local r=promote("untested"); print(r.ok,r.message)')
check "promotion still requires submitted tests" "$(echo "$out" | head -1 | grep -c 'must have tests')" "1"

# --- the agent cannot approve or install anything itself
check "cannot write into .harness/pkgs" "$(x 'print(pcall(write, ".harness/pkgs/evil/hx.toml", "x"))' | head -1 | grep -c protected)" "1"
check "cannot write into .harness/staged" "$(x 'print(pcall(write, ".harness/staged/evil/init.luau", "x"))' | head -1 | grep -c protected)" "1"
check "no accept/approve function exists for the agent" "$(x 'print(type(accept), type(approve), type(hx), type(require))' | head -1)" "nil	nil	nil	nil"
check "sh cannot activate a tool either" "$(x 'sh("mkdir -p .harness/pkgs/evil && echo x > .harness/pkgs/evil/hx.toml")' >/dev/null; [ -e $W/.harness/pkgs/evil ] && echo CREATED || echo blocked)" "blocked"

# --- extensions stay frozen when a user edits and re-approves a tool during a session
( x 'counter = 7
print(tools.slug.run("A B"))
sh("touch extension-ready; while [ ! -e .harness/proceed ]; do sleep 0.01; done", { timeout = 5 })' 'print(counter, tools.slug.run("A B"))' > $T/hot.out ) &
HP=$!
wait_for_file "$W/extension-ready" || exit 1
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "_")) end }' > $W/.harness/pkgs/slug/init.luau
$H approve slug --workspace $W >/dev/null
touch "$W/.harness/proceed"
wait $HP
check "frozen extensions: original source runs before the edit" "$(sed -n 1p $T/hot.out)" "a-b"
check "frozen run: session variable and original source survive approval changes" "$(grep -v '^$' $T/hot.out | tail -1)" "7	a-b"

# --- agent skills: <ws>/.agents/skills and ~/.agents/skills (no hx.toml, locked by `hx approve`, read-only outside the ws)
A=$T/aws; FH=$T/home; mkdir -p $A/.agents/skills/vid $FH/.agents/skills/vid/references $FH/.agents/skills/craft/references
xa() { HOME=$FH $H exec --workspace $A "$@" 2>&1 | grep -v "^-- ops="; }
printf -- '---\nname: vid\ndescription: project copy\n---\nproject vid\n' > $A/.agents/skills/vid/SKILL.md
printf -- '---\nname: vid\ndescription: global copy\n---\nglobal vid\n' > $FH/.agents/skills/vid/SKILL.md
printf -- '---\nname: craft\ndescription: >\n  plan and cut\n  short videos\n---\nRead references/beats.md first.\n' > $FH/.agents/skills/craft/SKILL.md
echo "beat list" > $FH/.agents/skills/craft/references/beats.md
check "hx skills lists project then global, unapproved" "$(HOME=$FH $H skills --workspace $A | awk '{print $1, $2, $3}' | tr '\n' ';')" "vid not approved;craft not approved;"
check "an unapproved agent skill is not visible" "$(xa 'print(pcall(skill, "craft"))' | head -1 | grep -c 'no installed skill')" "1"
check "an unapproved skill's files are not readable" "$(xa "print(pcall(read, '$FH/.agents/skills/craft/SKILL.md'))" | head -1 | grep -c 'outside the allowed roots')" "1"
HOME=$FH $H approve craft --workspace $A >/dev/null; HOME=$FH $H approve vid --workspace $A >/dev/null
out=$(xa 'local s = skill("craft") print(s.text) print(s.files[1], s.files[2], s.module) print(read(s.dir .. "/" .. s.files[2]))')
check "agent skill: text without frontmatter" "$(echo "$out" | sed -n 1p)" "Read references/beats.md first."
check "agent skill: files listed, no module" "$(echo "$out" | sed -n 3p)" "SKILL.md	references/beats.md	nil"
check "agent skill: reference file readable via dir" "$(echo "$out" | sed -n 4p)" "beat list"
check "agent skill: readable from sh (Seatbelt)" "$(xa "print(sh('cat $FH/.agents/skills/craft/references/beats.md').stdout)" | head -1)" "beat list"
check "agent skill: global dir stays read-only" "$(xa "print(pcall(write, '$FH/.agents/skills/craft/x.md', 'x'))" | head -1 | grep -c 'outside the allowed roots')" "1"
check "skills(query) finds by description, best first" "$(xa 'for _, h in skills("short videos") do print(h.name) end' | head -1)" "craft"
check "skills() with no query lists approved names" "$(xa 'print(#skills())' | head -1)" "2"
check "skill(\"/name\") as the user types it" "$(xa 'print(skill("/craft").name)' | head -1)" "craft"
check "unknown skill suggests the closest" "$(xa 'print(pcall(skill, "crafts"))' | head -1 | grep -c 'closest: craft')" "1"
check "project .agents/skills shadows global" "$(xa 'print(skill("vid").text)' | head -1)" "project vid"
echo "secret" > $T/secret.txt; mkdir -p $FH/.agents/skills/linky; printf -- '---\ndescription: links out\n---\nx\n' > $FH/.agents/skills/linky/SKILL.md
ln -s $T/secret.txt $FH/.agents/skills/linky/leak.md; HOME=$FH $H approve linky --workspace $A >/dev/null
check "a symlink inside an approved skill is not readable (read)" "$(xa "print(pcall(read, '$FH/.agents/skills/linky/leak.md'))" | head -1 | grep -c 'outside the allowed roots')" "1"
check "a symlink inside an approved skill is not readable (sh)" "$(xa "print(sh('cat $FH/.agents/skills/linky/leak.md').stdout)" | head -1 | grep -c secret)" "0"
mkdir -p $A/.harness/pkgs/vid; printf '[package]\nversion = "1.0.0"\nkind = "skill"\ndescription = "pkg copy"\n' > $A/.harness/pkgs/vid/hx.toml; printf 'pkg vid\n' > $A/.harness/pkgs/vid/SKILL.md
HOME=$FH $H approve vid --workspace $A >/dev/null
check ".harness/pkgs shadows an agent skill of the same name" "$(xa 'print(skill("vid").text)' | head -1)" "pkg vid"
echo "tampered" >> $FH/.agents/skills/craft/references/beats.md
check "a changed agent skill disappears" "$(xa 'print(pcall(skill, "craft"))' | head -1 | grep -c 'no installed skill')" "1"
check "hx skills reports the change" "$(HOME=$FH $H skills --workspace $A | grep -c 'craft .*changed since approval')" "1"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
