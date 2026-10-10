#!/usr/bin/env bash
# Harness-module upgrade flow (AC5), no API key: propose -> check -> eval gate -> accept -> rollback.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
R=$PWD; H=$R/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $MOCK 2>/dev/null' EXIT
MOCK=
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/proj; mkdir -p $W/.harness
x() { $H exec --workspace $W "$@" 2>&1 | grep -v "^-- ops="; }
BASEPRINT=$(x 'print({1,2})' | head -1)

# --- a candidate printer that wraps the built-in one
cat > $T/printer_v2.luau <<'LUA'
local base = require("base:std/printer")
local M = {}
for k, v in base do M[k] = v end
function M.top(v) return "[v2] " .. base.top(v) end
return M
LUA
# --- the agent can only PROPOSE
out=$(x --pack std/authoring "local r = propose_module('std/printer', [==[$(cat $T/printer_v2.luau)]==], { reason = 'tag outputs' }) print(r.ok, r.id)")
ID=$(echo "$out" | head -1 | awk '{print $2}')
check "agent: propose_module stages a proposal" "$(echo "$out" | head -1 | awk '{print $1}')$([ -f $W/.harness/proposals/$ID/new.luau ] && echo yes)" "trueyes"
check "agent: the proposal is NOT active" "$(x 'print({1,2})' | head -1)" "$BASEPRINT"
check "agent: cannot write the override itself" "$(x 'print(pcall(write, ".harness/override/std/printer.luau", "return {}"))' | head -1 | grep -c protected)" "1"
check "agent: only built-in std modules can be proposed" "$(x --pack std/authoring 'print(propose_module("std/nosuch", "return {}", {}).ok, propose_module("other/x", "return {}", {}).ok)' | head -1)" "false	false"
check "hx upgrade list shows it" "$($H upgrade list --workspace $W | grep -c "$ID  std/printer  tag outputs")" "1"

# --- structural check
check "check: a valid candidate loads and keeps every export" "$($H upgrade check $ID --workspace $W 2>&1 | head -1)" "loads, returns a table, keeps every export"
echo 'return {}' > $T/bad1.luau; B1=$($H upgrade propose std/printer $T/bad1.luau --workspace $W)
check "check: a candidate that drops exports is rejected" "$($H upgrade check $B1 --workspace $W 2>&1 | grep -c 'drops exports that other modules use')" "1"
echo 'this is not (( luau' > $T/bad2.luau; B2=$($H upgrade propose std/printer $T/bad2.luau --workspace $W)
check "check: a syntax error is rejected" "$($H upgrade check $B2 --workspace $W 2>&1 | grep -c 'does not compile')" "1"
echo 'return 42' > $T/bad3.luau; B3=$($H upgrade propose std/printer $T/bad3.luau --workspace $W)
check "check: a non-table module is rejected" "$($H upgrade check $B3 --workspace $W 2>&1 | grep -c 'must return a table')" "1"

# --- the gate is required
check "accept without a gate report is refused" "$($H upgrade accept $ID --yes --workspace $W 2>&1 | grep -c 'eval gate has not been run')" "1"

# --- run the gate with a scripted model on two tiny tasks
mkdir -p $T/tasks/t1/repo $T/tasks/t2/repo; for t in t1 t2; do echo "make out.txt" > $T/tasks/$t/prompt.md; echo keep > $T/tasks/$t/repo/keep.txt; printf '#!/bin/bash\ntest -f out.txt\n' > $T/tasks/$t/grade.sh; done
MOCK_REPLIES='["```luau\nwrite(\"out.txt\", \"1\")\n```","done"]' MOCK_BY_LEN=1 python3 scripts/mock_llm.py 8798 & MOCK=$!
sleep 1
export EVAL_HX_BASE_URL=http://127.0.0.1:8798 EVAL_HX_KEY=x
g() { python3 eval/gate.py --project $W --proposal $1 --tasks $T/tasks --runs 2 --model m --quiet 2>&1; }
g $ID > $T/gate1.out; rc=$?
check "gate: a harmless candidate passes" "$rc$(python3 -c "import json;print(json.load(open('$W/.harness/proposals/$ID/gate.json'))['pass'])")" "0True"
check "gate: the report is bound to this exact candidate" "$(python3 -c "
import json,hashlib
g=json.load(open('$W/.harness/proposals/$ID/gate.json')); print(g['candidate_sha']==hashlib.sha256(open('$W/.harness/proposals/$ID/new.luau','rb').read()).hexdigest(), g['current']['passed'], g['candidate']['passed'])")" "True 4 4"

# --- accept is the user's
echo "$(printf 'x')" > /dev/null
out=$($H upgrade accept $ID --yes --workspace $W 2>&1 | tail -1)
check "accept: activates the module and says how to undo it" "$(echo "$out" | grep -c 'activated std/printer (version 1)')" "1"
check "accepted candidate is live (no rebuild)" "$(x 'print({1,2})' | head -1)" "[v2] $BASEPRINT"
check "the previous state is saved for rollback" "$(ls $W/.harness/versions/std__printer | wc -l | tr -d ' ')" "1"
check "rollback: one command restores the built-in module" "$($H upgrade rollback std/printer --workspace $W | grep -c 'built-in')$(x 'print({1,2})' | head -1)" "1$BASEPRINT"
check "the audit trail records accept and rollback" "$(python3 -c "import json;print([json.loads(l)['action'] for l in open('$W/.harness/upgrades.jsonl')])")" "['accept', 'rollback']"

# --- a harmful candidate is stopped by the gate
cat > $T/loop_bad.luau <<'LUA'
local M = {}
function M.run(cfg) return { turns = 0, code_turns = 0, luau_errors = 0, truncated_turns = 0, ops_per_turn = {}, finished = true, final_answer = "" } end
return M
LUA
BAD=$($H upgrade propose std/loop $T/loop_bad.luau --reason "simplify" --workspace $W)
check "harmful candidate: passes the structural check (it is valid Luau)" "$($H upgrade check $BAD --workspace $W 2>&1 | head -1)" "loads, returns a table, keeps every export"
g $BAD > $T/gate2.out; rc=$?
check "harmful candidate: the gate FAILS it (pass rate collapses)" "$rc$(python3 -c "import json;g=json.load(open('$W/.harness/proposals/$BAD/gate.json'));print(g['pass'], g['candidate']['passed'])")" "1False 0"
check "harmful candidate: accept refuses" "$($H upgrade accept $BAD --yes --workspace $W 2>&1 | grep -c 'gate FAILED')" "1"
check "harmful candidate: nothing became active" "$([ -e $W/.harness/override/std/loop.luau ] && echo ACTIVE || echo inactive)" "inactive"

# --- tampering after the gate
echo '-- tampered' >> $W/.harness/proposals/$ID/new.luau
check "a candidate edited after the gate cannot be accepted" "$($H upgrade accept $ID --yes --workspace $W 2>&1 | grep -c 'modified after it was proposed')" "1"

# --- force is explicit and recorded
FORCED=$($H upgrade propose std/printer $T/printer_v2.luau --reason "forced" --workspace $W)
check "force accepts without a gate and is recorded as such" "$($H upgrade accept $FORCED --yes --force --workspace $W 2>&1 | grep -c activated)$(grep -c 'accept(force)' $W/.harness/upgrades.jsonl)" "11"
$H upgrade rollback std/printer --workspace $W >/dev/null

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
