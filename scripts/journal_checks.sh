#!/usr/bin/env bash
# Journal checks: event log (hash chain, write-ahead intents), approval gate, crash consistency. No API key needed.
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
cleanup() { pkill -f "sleep 4242" 2>/dev/null; kill $MOCK 2>/dev/null; }
MOCK=
newws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; printf '#!/bin/bash\necho deployed > deployed.txt\n' > $W/deploy.sh; chmod +x $W/deploy.sh; }
lastlog() { ls -t "$(logs $W)"/*.jsonl | head -1; }

# --- 0. the log is independent of the policy flag, and admin commands do not open a session
newws n
$H exec --workspace $W --yes 'write("a.txt", "hi")' >/dev/null 2>&1
check "--yes still writes a log" "$(ls "$(logs $W)"/*.jsonl 2>/dev/null | wc -l | tr -d ' ')" "1"
$H staged --workspace $W >/dev/null
check "admin commands open no log session" "$(ls "$(logs $W)"/*.jsonl | wc -l | tr -d ' ')" "1"

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

# --- 2. tampering is detected
python3 - "$L" "$T/tampered.jsonl" <<'PY'
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
L=$(lastlog); check "refusal is logged as a denied call, with no intent (approval comes first)" "$(python3 - "$L" <<'PY'
import json,sys
rs=[json.loads(l) for l in open(sys.argv[1])]
i=[r for r in rs if r["kind"]=="intent" and r["tool"]=="sh"]; c=[r for r in rs if r["kind"]=="call" and r.get("denied")]
print("yes" if not i and c and c[0]["class"]=="external" and c[0]["tool"]=="sh" else "no")
PY
)$($H log verify $L >/dev/null; echo $?)" "yes0"
newws c; printf '[approval]\nask = ["./deploy.sh"]\nallow = ["./deploy.sh"]\n' > $W/.harness/policy.toml
$H exec --workspace $W 'sh("./deploy.sh")' >/dev/null 2>&1
check "user pre-approval in policy.toml lets it run" "$([ -e $W/deployed.txt ] && echo ran || echo not-run)" "ran"
check "the intent records who approved it" "$(python3 -c "
import json,sys
print([r.get('approval') for r in map(json.loads, open('$(lastlog)')) if r['kind']=='intent' and r['tool']=='sh'])")" "['policy-allow']"
newws c2; printf '[approval]\nallow = ["git push origin"]\n' > $W/.harness/policy.toml
out=$($H exec --workspace $W 'print(pcall(sh, "git push -o \"git push origin\" nowhere main"))' 2>&1 | head -1)
check "an allow entry inside a quoted argument approves nothing" "$(echo "$out" | grep -c 'approval_required')" "1"
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
report=$(OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY)
L=$(lastlog); check "run log has llm + chunk + write records, chain intact" "$(python3 - "$L" <<'PY'
import json,sys,collections
rs=[json.loads(l) for l in open(sys.argv[1])]; c=collections.Counter((r["kind"],r.get("tool")) for r in rs)
print("yes" if c[("llm",None)]==2 and c[("chunk_begin",None)]==1 and c[("checkpoint",None)]>=3 and c[("intent","write")]==1 and c[("session",None)]==1 else str(dict(c)))
PY
)$($H log verify $L | head -1 | grep -c intact)" "yes1"
# Checkpoints and the manifest are trees of shared blobs: a missing one is damage verify reports, not a hash-chain matter.
check "stored state verifies" "$($H log verify $L | grep -c 'stored state: .* blobs verified')" "1"
shared=$(python3 - "$L" <<'PY'
import json,sys,pathlib
log=pathlib.Path(sys.argv[1]); rs=[json.loads(l) for l in open(log)]
root=json.loads((log.parent/"blobs"/next(r["blob"] for r in rs if r["kind"]=="manifest")).read_text())
refs=[]
def walk(v):
    if isinstance(v,dict):
        if list(v)==["$hx.blob"]: refs.append(v["$hx.blob"])
        else: [walk(x) for x in v.values()]
    elif isinstance(v,list): [walk(x) for x in v]
walk(root); print(log.parent/"blobs"/refs[0])
PY
)
mv "$shared" $T/held; $H log verify $L >$T/verify.out; rc=$?; mv $T/held "$shared"
check "a missing shared blob fails verify (exit 1)" "$(grep -c 'INVALID STORED STATE' $T/verify.out)$rc" "11"

# --- 6. run state lives outside the workspace: listed by `hx runs`, out of reach of shell commands, old journals still work
check "the report names the journal in the state directory, none in the workspace" "$(echo "$report" | python3 -c "import json,sys;print(json.load(sys.stdin)['log'])")$([ -e $W/.harness/log ] && echo IN-WORKSPACE)" "$L"
ID=$(basename $L .jsonl)
check "hx runs lists the run with its path" "$($H runs --workspace $W | grep -c "^$L  run  go")" "1"
check "a run id works where LOG is expected" "$($H log verify --workspace $W $ID | head -1 | grep -c intact)" "1"
if unconfined $W; then echo "skip shell confinement of the journal (Landlock unavailable)"; else
  before=$(cksum < $L)
  out=$($H exec --workspace $W --yes "print(sh('cat $L | wc -c; echo forged >> $L; ls $(dirname $L)/blobs').code)" 2>&1)
  check "a shell command can neither read nor append to the journal" "$(echo "$out" | head -1)$(cksum < $L | grep -c "$before")" "21"
  check "...nor can file tools read it" "$($H exec --workspace $W --yes "print((pcall(read, '$L')))" 2>&1 | head -1)" "false"
fi
mkdir -p $W/.harness/log; cp -R "$(logs $W)"/. $W/.harness/log/; OLD=$W/.harness/log/$ID.jsonl
out=$($H log verify $OLD 2>&1); check "an old in-workspace journal still verifies, with a note" "$(echo "$out" | grep -c 'intact')$(echo "$out" | grep -c 'lives in the workspace and is not protected from shell commands')" "11"
out=$(OPENROUTER_API_KEY=x $H run --workspace $W --resume $OLD 2>&1)
check "...and resumes, with the note" "$(echo "$out" | grep -c 'not protected from shell commands')$(echo "$out" | tail -1 | python3 -c "import json,sys;print(json.load(sys.stdin)['log'] == sys.argv[1])" $OLD)" "1True"

finish
