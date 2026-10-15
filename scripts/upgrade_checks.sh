#!/usr/bin/env bash
# Harness-module replacement, no API key: the agent stages -> the user reviews and accepts -> rollback.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
R=$PWD; H=$R/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/proj; mkdir -p $W/.harness
x() { $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }
stage() { x --pack std/authoring "local r = propose_module('$1', [==[$(cat $2)]==], { reason = '${3:-}' }) print(r.ok)" | head -1; }
BASEPRINT=$(x 'print({1,2})' | head -1)

# --- a replacement printer that wraps the built-in one
cat > $T/printer_v2.luau <<'LUA'
local base = require("base:std/printer")
local M = {}
for k, v in base do M[k] = v end
function M.top(v) return "[v2] " .. base.top(v) end
return M
LUA
# --- the agent can only STAGE
check "agent: propose_module stages a replacement" "$(stage std/printer $T/printer_v2.luau 'tag outputs')$([ -f $W/.harness/staged/std/printer/new.luau ] && echo yes)" "trueyes"
check "agent: the replacement is NOT active" "$(x 'print({1,2})' | head -1)" "$BASEPRINT"
check "agent: cannot write the override itself" "$(x 'print(pcall(write, ".harness/override/std/printer.luau", "return {}"))' | head -1 | grep -c protected)" "1"
check "agent: cannot accept from the staging area" "$(x 'print(pcall(write, ".harness/staged/std/printer/new.luau", "return {}"))' | head -1 | grep -c protected)" "1"
check "agent: only built-in std modules can be replaced" "$(x --pack std/authoring 'print(propose_module("std/nosuch", "return {}", {}).ok, propose_module("other/x", "return {}", {}).ok)' | head -1)" "false	false"
check "hx staged lists it" "$($H staged --workspace $W)" "std/printer"

# --- the user reviews: reason, structural check, diff
out=$($H accept std/printer --workspace $W </dev/null 2>&1)
check "review: reason, a passing check and the diff" "$(echo "$out" | grep -c -e 'reason: tag outputs' -e 'check: loads, returns a table, keeps every export' -e '^+function M.top')" "3"
check "review: without a terminal, accept needs --yes" "$(echo "$out" | grep -c 'pass --yes')$([ -d $W/.harness/staged/std/printer ] && echo kept)" "1kept"

# --- a broken replacement is shown as failing and cannot be accepted
echo 'return {}' > $T/bad1.luau; stage std/printer $T/bad1.luau >/dev/null
out=$($H accept std/printer --yes --workspace $W 2>&1)
check "drops exports: review says so and accept refuses" "$(echo "$out" | grep -c 'drops exports that other modules use')$(x 'print({1,2})' | head -1)" "2$BASEPRINT"
echo 'this is not (( luau' > $T/bad2.luau; stage std/printer $T/bad2.luau >/dev/null
check "a syntax error is refused" "$($H accept std/printer --yes --workspace $W 2>&1 | grep -c 'does not compile')" "2"
echo 'return 42' > $T/bad3.luau; stage std/printer $T/bad3.luau >/dev/null
check "a non-table module is refused" "$($H accept std/printer --yes --workspace $W 2>&1 | grep -c 'must return a table')" "2"
$H reject std/printer --workspace $W >/dev/null
check "reject discards it" "$($H staged --workspace $W)" "nothing staged"

# --- accept is the user's
stage std/printer $T/printer_v2.luau 'tag outputs' >/dev/null
out=$($H accept std/printer --yes --workspace $W 2>&1 | tail -1)
check "accept: activates the module and says how to undo it" "$(echo "$out" | grep -c 'activated std/printer (version 1); undo with `hx rollback std/printer`')" "1"
check "accepted replacement is live (no rebuild)" "$(x 'print({1,2})' | head -1)" "[v2] $BASEPRINT"
check "accept clears the staging" "$($H staged --workspace $W)" "nothing staged"
check "the previous state is saved for rollback" "$(ls $W/.harness/versions/std__printer | wc -l | tr -d ' ')" "1"

# --- staged against a module that changed since is refused
stage std/printer $T/printer_v2.luau 'again' >/dev/null
$H rollback std/printer --workspace $W >/dev/null
check "a replacement staged against a since-changed module is refused" "$($H accept std/printer --yes --workspace $W 2>&1 | grep -c 'changed since this replacement was staged')$(x 'print({1,2})' | head -1)" "1$BASEPRINT"
check "the audit trail records accept and rollback" "$(python3 -c "import json;print([json.loads(l)['action'] for l in open('$W/.harness/upgrades.jsonl')])")" "['accept', 'rollback']"
check "rollback with nothing left says so" "$($H rollback std/printer --workspace $W 2>&1 | grep -c 'nothing to roll back')" "1"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
