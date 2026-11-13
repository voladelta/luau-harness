#!/usr/bin/env bash
# Journal checks: event log (hash chain, write-ahead intents), approval gate, crash consistency. No API key needed.
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
cleanup() { pkill -f "sleep 4242" 2>/dev/null; kill $MOCK 2>/dev/null; }
MOCK=
newws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; printf '#!/bin/bash\necho deployed > deployed.txt\n' > $W/deploy.sh; chmod +x $W/deploy.sh; }
lastlog() { ls -t "$(logdir $W)"/*.jsonl | head -1; }

# --- 0. the log is independent of the policy flag, and admin commands do not open a session
newws n
$H exec --workspace $W --yes 'write("a.txt", "hi")' >/dev/null 2>&1
check "--yes still writes a log" "$(ls "$(logdir $W)"/*.jsonl 2>/dev/null | wc -l | tr -d ' ')" "1"
$H staged --workspace $W >/dev/null
check "admin commands open no log session" "$(ls "$(logdir $W)"/*.jsonl | wc -l | tr -d ' ')" "1"
check "the log lives in hx's state directory, not the workspace" "$(logdir $W | grep -c "^$XDG_STATE_HOME/hx/workspaces/n-[0-9a-f]\{16\}/log$")$([ -e $W/.harness/log ] && echo IN-TREE)" "1"
check "hx paths names the log, ar, secrets and trust locations" "$($H paths --workspace $W | cut -d: -f1 | tr '\n' ' ')" "log ar secrets trust "

# --- 0b. --yes automatically grants ordinary paths and commands, but .harness/ and hx.lock stay read-only (through file tools)
newws y
O=$T/autopilot-outside.txt
check "--yes writes outside the workspace" "$($H exec --workspace $W --yes "write('$O', 'x')" >/dev/null 2>&1; cat $O 2>/dev/null)" "x"
check "--yes automatically approves external commands" "$($H exec --workspace $W --yes 'sh("./deploy.sh # git push")' >/dev/null 2>&1; cat $W/deployed.txt 2>/dev/null)" "deployed"
check "--yes write() to .harness refused" "$($H exec --workspace $W --yes 'print((pcall(write, ".harness/policy.toml", "x")))' 2>/dev/null | head -1)" "false"
check "--yes write() to hx.lock refused" "$($H exec --workspace $W --yes 'print((pcall(write, "hx.lock", "x")))' 2>/dev/null | head -1)" "false"
ln -s .harness $W/link
check "--yes write() via symlink into .harness refused" "$($H exec --workspace $W --yes 'print((pcall(write, "link/policy.toml", "x")))' 2>/dev/null | head -1)" "false"
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

# --- 2. tampering is detected (copies keep the journal's name: the name is its session)
N=$(basename $L); mkdir -p $T/tampered $T/deleted $T/substituted
python3 - "$L" "$T/tampered/$N" <<'PY'
import json,sys
lines=open(sys.argv[1]).read().splitlines()
for i,line in enumerate(lines):
    record=json.loads(line)
    if record.get("kind")=="intent" and record.get("tool")=="write":
        record["tool"]="wrote"
        lines[i]=json.dumps(record,separators=(",",":"))
        assert lines[i]!=line and i+1<len(lines), "tamper a record covered by the next record's hash"
        break
else:
    raise AssertionError("no write intent to tamper with")
open(sys.argv[2],"w").write("\n".join(lines)+"\n")
PY
$H log verify $T/tampered/$N >/dev/null 2>&1; check "edited record breaks the chain (exit 1)" "$?" "1"
python3 - "$L" "$T/deleted/$N" <<'PY'
import sys
l=open(sys.argv[1]).read().splitlines(); del l[2]; open(sys.argv[2],"w").write("\n".join(l)+"\n")
PY
$H log verify $T/deleted/$N >/dev/null 2>&1; check "deleted record breaks the chain (exit 1)" "$?" "1"
cp $L $T/substituted/$N; check "an intact journal verifies under its own name" "$($H log verify $T/substituted/$N | head -1 | grep -c intact)" "1"
cp $L $T/substituted/other-run.jsonl; out=$($H log verify $T/substituted/other-run.jsonl 2>&1); rc=$?
check "a journal under another run's name is refused (exit 1)" "$rc$(echo "$out" | grep -c 'belongs to session')" "11"
check "hx log verify takes a bare run name" "$($H log verify --workspace $W ${N%.jsonl} | head -1 | grep -c intact)" "1"
# The journal is outside every root: a shell command can neither truncate it, nor put another run's journal in its place.
if ! unconfined $W; then
  OTHER=$(ls "$(logdir $W)"/*.jsonl | grep -v "$N" | head -1)
  [ -n "$OTHER" ] || { $H exec --workspace $W 'print(1)' >/dev/null; OTHER=$(ls -t "$(logdir $W)"/*.jsonl | head -1); }
  before=$(wc -l < $L)
  out=$($H exec --workspace $W "local a = sh('head -n 1 $L > t && mv t $L'); local b = sh('cp $OTHER $L'); local c = sh('rm -f ${L%.jsonl}.lock; touch ${L%.jsonl}.revoked'); print(a.code ~= 0, b.code ~= 0, c.code ~= 0)" 2>/dev/null | head -1)
  check "confined shell cannot truncate, replace or unlock a journal" "$(echo "$out" | tr '\t' ' ')" "true true true"
  check "...and the journal is whole and still its own" "$(wc -l < $L)$($H log verify $L | head -1 | grep -c intact)$([ -e ${L%.jsonl}.revoked ] && echo REVOKED)" "${before}1"
  check "file tools cannot read or write the journal" "$($H exec --workspace $W --yes "print(pcall(read, '$L'), pcall(write, '$L', 'x'))" 2>/dev/null | head -1 | cut -f1,2 | tr '\t' ' ')" "false false"
fi
printf '[fs]\nread = ["%s"]\n' "$XDG_STATE_HOME" > $W/.harness/policy.toml
check "a read root that holds hx's state is refused" "$($H exec --workspace $W 'print(1)' 2>&1 | grep -c "overlaps hx's state directory")" "1"
rm $W/.harness/policy.toml

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
# --- 4. crash consistency: killed mid-effect => unknown outcome, never silently lost
newws g
$H exec --workspace $W 'sh("touch child-ready; sleep 4242; touch survived")' >/dev/null 2>&1 & P=$!
wait_for_file "$W/child-ready" || exit 1; kill -9 $P 2>/dev/null; wait $P 2>/dev/null; pkill -f "sleep 4242" 2>/dev/null
L=$(lastlog); out=$($H log verify $L); rc=$?
check "killed mid-effect: chain intact" "$(echo "$out" | head -1 | grep -c intact)" "1"
check "killed mid-effect: reported as UNKNOWN OUTCOME (exit 3)" "$(echo "$out" | grep -c 'UNKNOWN OUTCOME.*sh')$rc" "13"
check "the interrupted command did not complete" "$([ -e $W/survived ] && echo DONE || echo not-done)" "not-done"

# --- 5. model/LLM and chunk activity is logged end-to-end (scripted model)
newws h
echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
MOCK_REPLIES='["```luau\nwrite(\"x.txt\",\"1\")\n```","done"]' serve MP python3 scripts/mock_llm.py 0 || exit 1; MOCK=$SERVED
echo go > $T/p.txt
OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY >/dev/null
L=$(lastlog); check "run log has llm + chunk + write records, chain intact" "$(python3 - "$L" <<'PY'
import json,sys,collections
rs=[json.loads(l) for l in open(sys.argv[1])]; c=collections.Counter((r["kind"],r.get("tool")) for r in rs)
print("yes" if c[("llm",None)]==2 and c[("chunk_begin",None)]==1 and c[("checkpoint",None)]>=3 and c[("intent","write")]==1 and c[("session",None)]==1 else str(dict(c)))
PY
)$($H log verify $L | head -1 | grep -c intact)" "yes1"

# --- 6. resume refuses a journal that fails verification, and one from outside the workspace's log directory
cp $L $T/elsewhere.jsonl
out=$(OPENROUTER_API_KEY=x $H run --workspace $W --resume $T/elsewhere.jsonl 2>&1); rc=$?
check "resume refuses a journal outside the log directory" "$([ $rc -ne 0 ] && echo refused)$(echo "$out" | grep -c "not in this workspace's log directory")" "refused1"
python3 - "$L" <<'PY'
import json,sys
lines=open(sys.argv[1]).read().splitlines()
last=json.loads(lines[-1])
grant={"seq":last["seq"]+1,"ts_ms":last["ts_ms"],"session":last["session"],"kind":"path_grant","prev":"0"*64,"grant":{"path":"/","write":True}}
open(sys.argv[1],"a").write(json.dumps(grant,separators=(",",":"))+"\n")
PY
out=$(OPENROUTER_API_KEY=x $H run --workspace $W --resume $(basename $L .jsonl) 2>&1); rc=$?
check "resume (by run name) refuses a forged record" "$([ $rc -ne 0 ] && echo refused)$(echo "$out" | grep -c 'hash chain broken')" "refused1"

finish
