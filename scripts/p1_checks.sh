#!/usr/bin/env bash
# P1 checks: event log (hash chain, write-ahead intents), approval gate, crash consistency. No API key needed.
set -u
cd "$(dirname "$0")/.."
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; pkill -f "sleep 4242" 2>/dev/null; kill $MOCK 2>/dev/null' EXIT
MOCK=
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
newws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; printf '#!/bin/bash\necho deployed > deployed.txt\n' > $W/deploy.sh; chmod +x $W/deploy.sh; }
lastlog() { ls -t $W/.harness/log/*.jsonl | head -1; }

# --- 1. the log records effects write-ahead and the chain verifies
newws a
$H exec --workspace $W 'write("a.txt", "hi")
edit("a.txt", "hi", "zebra-content-7731")
read("a.txt")
sh("echo ok")
search("zebra")' >/dev/null
L=$(lastlog)
check "log chain intact" "$($H log verify $L | head -1 | grep -c 'intact')" "1"
check "intents precede outcomes (write, edit, sh)" "$(python3 - "$L" <<'PY'
import json,sys
rs=[json.loads(l) for l in open(sys.argv[1])]
ok=True
for t in ("write","edit","sh"):
    i=[r for r in rs if r["kind"]=="intent" and r["tool"]==t]; o=[r for r in rs if r["kind"]=="outcome" and r["tool"]==t]
    ok &= len(i)==1 and len(o)==1 and o[0]["of"]==i[0]["seq"] and i[0]["seq"]<o[0]["seq"]
print("yes" if ok else "no")
PY
)" "yes"
check "reads are logged as calls, file content is not" "$(python3 - "$L" <<'PY'
import json,sys
txt=open(sys.argv[1]).read(); rs=[json.loads(l) for l in txt.splitlines()]
print("yes" if any(r["kind"]=="call" and r["tool"]=="read" for r in rs) and "zebra-content-7731" not in txt else "no")
PY
)" "yes"

# --- 2. tampering is detected
cp $L $T/tampered.jsonl; sed -i '' '3s/"tool": "write"/"tool": "wrote"/;3s/"tool":"write"/"tool":"wrote"/' $T/tampered.jsonl
$H log verify $T/tampered.jsonl >/dev/null 2>&1; check "edited record breaks the chain (exit 1)" "$?" "1"
python3 - "$L" "$T/deleted.jsonl" <<'PY'
import sys
l=open(sys.argv[1]).read().splitlines(); del l[2]; open(sys.argv[2],"w").write("\n".join(l)+"\n")
PY
$H log verify $T/deleted.jsonl >/dev/null 2>&1; check "deleted record breaks the chain (exit 1)" "$?" "1"

# --- 3. approval gate: external effects are refused without the user's approval, and do not run
newws b; printf '[approval]\nask = ["./deploy.sh"]\n' > $W/.harness/policy.toml
out=$($H exec --workspace $W 'print(pcall(sh, "./deploy.sh"))' 2>&1 | head -1)
check "external command refused" "$(echo "$out" | grep -c 'approval_required')" "1"
check "refused command did not run" "$([ -e $W/deployed.txt ] && echo RAN || echo not-run)" "not-run"
L=$(lastlog); check "refusal is logged (intent + denied outcome)" "$(python3 - "$L" <<'PY'
import json,sys
rs=[json.loads(l) for l in open(sys.argv[1])]
i=[r for r in rs if r["kind"]=="intent" and r["tool"]=="sh"]; o=[r for r in rs if r["kind"]=="outcome" and r.get("denied")]
print("yes" if i and o and i[0]["class"]=="external" and o[0]["of"]==i[0]["seq"] else "no")
PY
)" "yes"
newws c; printf '[approval]\nask = ["./deploy.sh"]\nallow = ["./deploy.sh"]\n' > $W/.harness/policy.toml
$H exec --workspace $W 'sh("./deploy.sh")' >/dev/null 2>&1
check "user pre-approval in policy.toml lets it run" "$([ -e $W/deployed.txt ] && echo ran || echo not-run)" "ran"
newws d; printf '[approval]\nask = ["./deploy.sh"]\n' > $W/.harness/policy.toml
out=$($H exec --workspace $W 'print(pcall(sh, "./deploy.sh"))' --approval prompt 2>&1 </dev/null | head -1)
check "prompt mode with no terminal refuses" "$(echo "$out" | grep -c 'approval_required')$([ -e $W/deployed.txt ] && echo RAN)" "1"
newws e; printf '[approval]\nallow = ["git push"]\n' > $W/.harness/policy.toml
out=$($H exec --workspace $W 'print(pcall(sh, "git push --force origin main"))' 2>&1 | head -1)
check "irreversible command is not covered by a substring allow" "$(echo "$out" | grep -c 'approval_required')" "1"
newws f
out=$($H exec --workspace $W 'print(pcall(write, ".harness/policy.toml", "[approval]\nallow = [\"\"]"))' 2>&1 | head -1)
check "the model cannot edit policy.toml to approve itself" "$(echo "$out" | grep -c 'protected')" "1"
out=$($H exec --workspace $W 'print(pcall(sh, "echo \"[approval]\nallow=[\\\"\\\"]\" > .harness/policy.toml; ls ./nonexistent"))' >/dev/null 2>&1; [ -s $W/.harness/policy.toml ] && echo written || echo untouched)
check "sh cannot edit policy.toml either" "$out" "untouched"

# --- 4. crash consistency: killed mid-effect => unknown outcome, never silently lost
newws g
$H exec --workspace $W 'sh("sleep 4242; touch survived")' >/dev/null 2>&1 & P=$!
sleep 2; kill -9 $P 2>/dev/null; wait $P 2>/dev/null; pkill -f "sleep 4242" 2>/dev/null
L=$(lastlog); out=$($H log verify $L); rc=$?
check "killed mid-effect: chain intact" "$(echo "$out" | head -1 | grep -c intact)" "1"
check "killed mid-effect: reported as UNKNOWN OUTCOME (exit 3)" "$(echo "$out" | grep -c 'UNKNOWN OUTCOME.*sh')$rc" "13"
check "the interrupted command did not complete" "$([ -e $W/survived ] && echo DONE || echo not-done)" "not-done"

# --- 5. model/LLM and chunk activity is logged end-to-end (scripted model)
newws h
MOCK_REPLIES='["```luau\nwrite(\"x.txt\",\"1\")\n```","done"]' python3 scripts/mock_llm.py 8769 & MOCK=$!
sleep 1; echo go > $T/p.txt
OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:8769 --key-env OPENROUTER_API_KEY >/dev/null
L=$(lastlog); check "run log has llm + chunk + write records, chain intact" "$(python3 - "$L" <<'PY'
import json,sys,collections
rs=[json.loads(l) for l in open(sys.argv[1])]; c=collections.Counter((r["kind"],r.get("tool")) for r in rs)
print("yes" if c[("llm",None)]==2 and c[("chunk",None)]==1 and c[("intent","write")]==1 and c[("session",None)]==1 else str(dict(c)))
PY
)$($H log verify $L | head -1 | grep -c intact)" "yes1"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
