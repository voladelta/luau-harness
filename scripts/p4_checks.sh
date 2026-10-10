#!/usr/bin/env bash
# P4 (extension system) checks, no API key: skills, tool packages, agent-written tools (deftool/promote/accept),
# no self-approval, hot reload with session state. Build first: cargo build --release
set -u
cd "$(dirname "$0")/.."
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $MOCK 2>/dev/null; pkill -f "sleep 4243" 2>/dev/null' EXIT
MOCK=
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/ws; mkdir -p $W/.harness/pkgs/release $W/.harness/pkgs/slug
x() { $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }

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
echo 'return { run = function(s) return "tampered" end }' > $W/.harness/pkgs/slug/init.luau
check "a tampered tool package disappears (hash no longer matches the approval)" "$(x 'print(tools.slug)' | head -1)" "nil"
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "-")) end }' > $W/.harness/pkgs/slug/init.luau
check "restoring the approved content brings it back" "$(x 'print(tools.slug.run("A B"))' | head -1)" "a-b"

# --- the prompt shows installed extensions (and nothing when there are none)
MOCK_REPLIES='["done"]' MOCK_BY_LEN=1 MOCK_LOG=$T/req.jsonl python3 scripts/mock_llm.py 8796 & MOCK=$!
sleep 1; echo go > $T/p.txt
run() { OPENROUTER_API_KEY=x $H run --workspace $1 --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8796 --key-env OPENROUTER_API_KEY >/dev/null 2>&1; }
system_of_last() { python3 -c "
import json
l=[json.loads(x) for x in open('$T/req.jsonl')][-1]
m=l['messages'][0]['content']; print(m[0]['text'] if isinstance(m,list) else m)"; }
run $W
s=$(system_of_last)
check "prompt lists the tool with its signature" "$(echo "$s" | grep -c 'tools.slug.run(s: string): string  -- lowercase, dashes')" "1"
check "prompt lists the skill with its description" "$(echo "$s" | grep -c -- '- release: how to cut a release')" "1"
E=$T/empty; mkdir -p $E; run $E
check "no extensions installed: the prompt is unchanged" "$(system_of_last | grep -c 'Installed extensions')" "0"

# --- agent-written tools: usable immediately, only with the capabilities they declare
echo "secret" > $W/data.txt
out=$(x 'deftool("peek", [==[
return { run = function() return read("data.txt") end }
]==], { doc = "reads a file" })
print(pcall(tools.peek.run))')
check "deftool: a tool without declared caps cannot read files" "$(echo "$out" | head -1 | grep -c 'capability_denied')" "1"
out=$(x 'deftool("peek", [==[
return { run = function() return read("data.txt") end }
]==], { caps = { fs = { "read" } } })
print(tools.peek.run())')
check "deftool: declaring fs.read lets it read" "$(echo "$out" | head -1)" "secret"
check "deftool: defined tools do not show up in vars()" "$(x 'deftool("t1", "return {}") x = 1 print(vars().tools, vars().deftool, vars().x)' | head -1)" "nil	nil	1"

TOOL='deftool("shout", [==[
local M = {}
function M.run(s) return s:upper() .. "!" end
return M
]==], { doc = "shouts", signature = "shout.run(s: string): string",
  tests = [==[
local m = require("shout")
test.case("shouts", function() test.eq(m.run("hi"), "HI!") end)
]==] })'
out=$(x "$TOOL"$'\nprint(tools.shout.run("a"))\nlocal r = promote("shout")\nprint(r.ok)\nprint(r.message)')
check "deftool: usable at once in the session" "$(echo "$out" | sed -n 1p)" "A!"
check "promote: tests pass => staged" "$(echo "$out" | sed -n 2p)" "true"
check "promote: tells the agent a human must accept" "$(echo "$out" | sed -n 3p | grep -c 'hx accept shout')" "1"
check "staged but NOT installed" "$([ -d $W/.harness/staged/shout ] && [ ! -d $W/.harness/pkgs/shout ] && echo yes || echo no)" "yes"
check "a new session does not see the staged tool" "$(x 'print(tools.shout)' | head -1)" "nil"
check "hx staged lists it" "$($H staged --workspace $W | head -1)" "shout"
$H accept shout --yes --workspace $W >/dev/null 2>&1
check "after the user runs hx accept, a new session can use it" "$(x 'print(tools.shout.run("ok"))' | head -1)" "OK!"
check "accepted tool is locked in hx.lock" "$(grep -c 'name = "shout"' $W/hx.lock)" "1"

out=$(x 'deftool("bad", "return { run = function() return 1 end }", { tests = [==[
local m = require("bad")
test.case("fails", function() test.eq(m.run(), 2) end)
]==] })
local r = promote("bad") print(r.ok, r.message)')
check "promote: failing tests => refused, nothing staged" "$(echo "$out" | head -1)$([ -d $W/.harness/staged/bad ] && echo STAGED)" "false	the tool's tests failed; nothing was staged"
out=$(x 'deftool("untested", "return {}") local r = promote("untested") print(r.ok, r.message)')
check "promote: no tests => refused" "$(echo "$out" | head -1 | grep -c 'must have tests')" "1"
out=$(x 'deftool("greedy", [==[
return { run = function() return sh("echo x").stdout end }
]==], { tests = [==[
test.case("runs", function() local m = require("greedy") test.eq(m.run(), "x\n") end)
]==] })
local r = promote("greedy") print(r.ok)')
check "promote: a tool that needs a capability it did not declare fails its own tests" "$(echo "$out" | head -1)" "false"

# --- the agent cannot approve or install anything itself
check "cannot write into .harness/pkgs" "$(x 'print(pcall(write, ".harness/pkgs/evil/hx.toml", "x"))' | head -1 | grep -c protected)" "1"
check "cannot write into .harness/staged" "$(x 'print(pcall(write, ".harness/staged/evil/init.luau", "x"))' | head -1 | grep -c protected)" "1"
check "no accept/approve function exists for the agent" "$(x 'print(type(accept), type(approve), type(hx), type(require))' | head -1)" "nil	nil	nil	nil"
check "sh cannot activate a tool either" "$(x 'sh("mkdir -p .harness/pkgs/evil && echo x > .harness/pkgs/evil/hx.toml")' >/dev/null; [ -e $W/.harness/pkgs/evil ] && echo CREATED || echo blocked)" "blocked"

# --- hot reload keeps the session: user edits + re-approves a tool while a session is running
( x 'counter = 7
print(tools.slug.run("A B"))
sh("sleep 4243", { timeout = 4 })' 'print(counter, refresh_extensions(), tools.slug.run("A B"))' > $T/hot.out ) &
HP=$!
sleep 2
echo 'return { run = function(s) return (s:lower():gsub("[^%w]+", "_")) end }' > $W/.harness/pkgs/slug/init.luau
$H approve slug --workspace $W >/dev/null
wait $HP
check "hot reload: before the edit the old code runs" "$(sed -n 1p $T/hot.out)" "a-b"
check "hot reload: session variable survives and the new code is live" "$(grep -v '^$' $T/hot.out | tail -1)" "7	refreshed 2 extension(s)	a_b"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
