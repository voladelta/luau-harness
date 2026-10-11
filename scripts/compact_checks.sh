#!/usr/bin/env bash
# std/compact pack checks (scripted model, no API key): threshold compaction, the summarizer request, update summaries,
# globals surviving, failure handling, overflow compact-and-retry, resume of logs that contain a compaction or an overflow,
# subagents. The mock's usage numbers (prompt_tokens) decide when the threshold is crossed.
# Window 40000, max output 1000 => threshold min(38000, 40000-1000-16384) = 22616; a reply with prompt_tokens 30000 crosses it.
set -u
cd "$(dirname "$0")/.."
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); MP=""; trap 'rm -rf "$T"; kill $MP 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
echo "TASKTEXT-SCRIPTED" > $T/p.txt

# reply builders (C: a luau call with ~200 chars of padding so tiny histories have something to cut; S: plain text; E: HTTP 400)
cat > $T/r.py <<'PY'
import json
PAD = " -- " + "p" * 200
def C(code, tok=10): return {"text": "", "calls": [{"code": code + PAD}], "prompt_tokens": tok}
def S(text): return {"text": text}
def E(msg): return {"status": 400, "error": msg}
def dump(*r): print(json.dumps(list(r)))
PY
cat > $T/q.py <<'PY'
import json
def text(c): return c if isinstance(c, str) else "".join(p.get("text", "") for p in (c or []) if isinstance(p, dict))
def load(f): return [json.loads(l) for l in open(f)]
def is_sum(b): return "context summarization assistant" in text(b["messages"][0]["content"])
def msgs(b): return b["messages"][1:]  # without the system message
def sums(f): return [b for b in load(f) if is_sum(b)]
def conv(b): return text(b["messages"][1]["content"])
PY
R() { PYTHONPATH=$T python3 -c "from r import *; $1"; }   # R 'dump(C("x=1"), S("done"))' -> JSON list
Q() { PYTHONPATH=$T python3 -c "from q import *; $1"; }

mock() { MOCK_REPLIES="$1" MOCK_LOG="$2" python3 scripts/mock_llm.py $3 & MP=$!; sleep 1; }  # replies-json log port
unmock() { kill $MP 2>/dev/null; wait $MP 2>/dev/null; MP=""; }
newws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; echo 'require("std/compact").keep_recent_tokens = 40' > $W/.harness/init.luau; }
run() { # workspace port trace extra-flags...   (stdout/stderr in $T/out, $T/err; status is hx's)
  OPENROUTER_API_KEY=x $H run --workspace $1 --prompt-file $T/p.txt --model m --api openai --base-url http://127.0.0.1:$2 --key-env OPENROUTER_API_KEY \
    --context-window 40000 --max-output 1000 --trace $3 "${@:4}" > $T/out 2> $T/err
}
field() { python3 -c "import sys,json;r=json.loads(open('$T/out').read().strip().splitlines()[-1]);print(r.get('$1'))" 2>/dev/null; }
lastlog() { ls -t $1/.harness/log/*.jsonl | head -1; }

# ---- 1. A1: without the pack, and with the pack below the threshold, requests are identical ----
THREE=$(R 'dump(C("a=1"), C("b=2"), S("done"))')
newws a1; mock "$THREE" $T/a1.plain 8810; run $W 8810 $T/a1.tr1; unmock
newws a1b; mock "$THREE" $T/a1.pack 8810; run $W 8810 $T/a1.tr2 --pack std/compact; unmock
check "A1 pack below the threshold: request bodies identical to a run without the pack" "$(python3 - <<PY
import json
def norm(f):
    out = []
    for l in open(f):
        b = json.loads(l); b.pop("prompt_cache_key", None); out.append(b)  # the session id differs per run
    return out
a, b = norm("$T/a1.plain"), norm("$T/a1.pack")
print(len(a) == 3 and a == b)
PY
)" "True"

# ---- 2. A2 + 3. A4: one summarizer request, then summary + kept tail; globals survive ----
S2=$(R 'dump(C("x = 42 -- TURN-ONE-MARKER"), C("y = 1", 30000), S("SUMMARY-ONE"), {"text": "", "calls": [{"code": "print(x)"}]}, S("done"))')
newws a2; mock "$S2" $T/a2.req 8811; run $W 8811 $T/a2.tr --pack std/compact; rc=$?; unmock
A2W=$W
check "A2 run with a compaction finishes" "$rc $(field finished)" "0 True"
check "A2 exactly one summarizer request, 5 requests in all" "$(Q 'r = load("'$T'/a2.req"); print(len(r), len(sums("'$T'/a2.req")))')" "5 1"
check "A2 summarizer request: system prompt, one user message, no tools" "$(Q 'b = load("'$T'/a2.req")[2]; print(is_sum(b), len(msgs(b)), msgs(b)[0]["role"], "tools" not in b)')" "True 1 user True"
check "A2 summarizer sees the serialized history, not the task message" "$(Q 'c = conv(load("'$T'/a2.req")[2]); print("<conversation>" in c, "TURN-ONE-MARKER" in c, "TASKTEXT-SCRIPTED" in c, "Dead Ends" in c, "<previous-summary>" in c)')" "True True False True False"
check "A2 next request: task + summary block, then an assistant message, tool results answer calls in the tail" "$(Q '
b = load("'$T'/a2.req")[3]; m = msgs(b)
ids = {c["id"] for x in m if x["role"] == "assistant" for c in x.get("tool_calls", [])}
tools = [x for x in m if x["role"] == "tool"]
first = text(m[0]["content"])
print(m[0]["role"], "TASKTEXT-SCRIPTED" in first, "[compaction-summary]" in first, "SUMMARY-ONE" in first, "</summary>" in first, m[1]["role"], len(tools) > 0 and all(t["tool_call_id"] in ids for t in tools))')" "user True True True True assistant True"
check "A2 the dropped history is gone from the next request" "$(Q 'print("TURN-ONE-MARKER" in json.dumps(load("'$T'/a2.req")[3]))')" "False"
check "A2 trace records the compaction" "$(grep -c '^### COMPACT$' $T/a2.tr)" "1"
check "A4 a global set before the compaction is readable after it" "$(awk '/^### COMPACT$/{s=1} s&&/^### RESULT/{getline; print; exit}' $T/a2.tr)" "42"

# ---- 4. A3: the second compaction updates the first summary and serializes only newer messages ----
S4=$(R 'dump(C("x = 42 -- TURN-ONE-MARKER"), C("y = 1", 30000), S("SUMMARY-ONE"), C("print(x) -- TURN-THREE-MARKER"), C("z = 2 -- TURN-FOUR-MARKER", 30000), S("SUMMARY-TWO"), S("done"))')
newws a3; mock "$S4" $T/a3.req 8812; run $W 8812 $T/a3.tr --pack std/compact; rc=$?; unmock
check "A3 run with two compactions finishes" "$rc $(field finished) $(Q 'print(len(sums("'$T'/a3.req")))')" "0 True 2"
check "A3 second summarizer request carries <previous-summary>, the UPDATE prompt and only newer messages" "$(Q '
c = conv(sums("'$T'/a3.req")[1])
print("<previous-summary>\nSUMMARY-ONE\n</previous-summary>" in c, "Update the existing structured summary" in c, "TURN-ONE-MARKER" not in c, "TURN-THREE-MARKER" in c)')" "True True True True"
check "A3 the final request holds the second summary only" "$(Q '
t = text(msgs(load("'$T'/a3.req")[-1])[0]["content"]); print("SUMMARY-TWO" in t, "SUMMARY-ONE" in t)')" "True False"

# ---- 5. A5: a failed summarizer leaves messages alone; two failures switch the pack off ----
S5=$(R 'dump(C("a=1"), C("b=2"), C("c=3", 30000), S(""), C("d=4", 30000), S(""), C("e=5", 30000), S("done"))')
newws a5; mock "$S5" $T/a5.req 8813; run $W 8813 $T/a5.tr --pack std/compact; rc=$?; unmock
check "A5 failing summarizer: run finishes, history never replaced" "$rc $(field finished) $(grep -c 'compaction-summary' $T/a5.req)" "0 True 0"
check "A5 two failures logged, then no third summarizer request" "$(grep -c '^### COMPACT FAILED$' $T/a5.tr) $(Q 'print(len(sums("'$T'/a5.req")))')" "2 2"
check "A5 a summarizer API error counts as a failure too" "$(
S5b=$(R 'dump(C("a=1"), C("b=2"), C("c=3", 30000), E("boom"), C("d=4", 30000), S("done"))'); newws a5b; mock "$S5b" $T/a5b.req 8813; run $W 8813 $T/a5b.tr --pack std/compact; rc=$?; unmock
echo "$rc $(field finished) $(grep -c '^### COMPACT FAILED$' $T/a5b.tr)")" "0 True 1"

# ---- 6. A6: a context-overflow error compacts once and retries the same turn ----
S6=$(R 'dump(C("a=1"), C("b=2"), E("maximum context length exceeded"), S("SUMMARY-OV"), C("print(a) -- AFTER"), S("done"))')
newws a6; mock "$S6" $T/a6.req 8814; run $W 8814 $T/a6.tr --pack std/compact; rc=$?; unmock
A6W=$W; A6REQ=$T/a6.req
check "A6 overflow then compaction then retry: run finishes" "$rc $(field finished) $(field turns)" "0 True 4"
check "A6 the retry carries the summary; the failed request did not" "$(Q 'r = load("'$T'/a6.req"); print(len(r), "compaction-summary" in json.dumps(r[2]), "SUMMARY-OV" in json.dumps(r[4]))')" "6 False True"
S6b=$(R 'dump(C("a=1"), C("b=2"), E("maximum context length exceeded"), S("SUMMARY-OV"), E("context_length_exceeded again"), S("never used"))')
newws a6b; mock "$S6b" $T/a6b.req 8814; run $W 8814 $T/a6b.tr --pack std/compact; rc=$?; unmock
check "A6 a second overflow on the same turn fails the run and says so" "$([ $rc -ne 0 ] && echo fail) $(grep -c 'failed again after one compact-and-retry' $T/err) $(Q 'print(len(load("'$T'/a6b.req")))')" "fail 1 5"
S6c=$(R 'dump(C("a=1"), C("b=2"), E("maximum context length exceeded"), S(""), S("never used"))')
newws a6c; mock "$S6c" $T/a6c.req 8814; run $W 8814 $T/a6c.tr --pack std/compact; rc=$?; unmock
check "A6 overflow with a failed compaction ends the run with the API error" "$([ $rc -ne 0 ] && echo fail) $(grep -c 'maximum context length exceeded' $T/err)" "fail 1"
newws a6d; mock "$S6c" $T/a6d.req 8814; run $W 8814 $T/a6d.tr; rc=$?; unmock
check "A6 without the pack an overflow fails as before (no summarizer call)" "$([ $rc -ne 0 ] && echo fail) $(Q 'print(len(load("'$T'/a6d.req")))')" "fail 3"

# ---- 6b. real-model shape: reasoning (echoed back in raw) dominates the context, and keep_recent is the default ----
S6r=$(R 'R1 = "r" * 6000; dump(*[dict(C("t%d = %d" % (i, i), 30000 if i == 8 else 3000 * i), reasoning=R1) for i in range(1, 9)], S("SUMMARY-R"), C("print(t1)", 2000), S("done"))')
W=$T/a6r; mkdir -p $W/.harness; echo 'require("std/compact").keep_recent_tokens = 5000' > $W/.harness/init.luau
mock "$S6r" $T/a6r.req 8814; run $W 8814 $T/a6r.tr --pack std/compact; rc=$?; unmock
check "6b reasoning-heavy turns: the compaction summarizes most of the history, keeping a short tail" "$rc $(field finished) $(grep -A1 '^### COMPACT$' $T/a6r.tr | tail -1 | sed -E 's/summarized ([0-9]+) messages, kept ([0-9]+),.*/\1 \2/')" "0 True 12 4"

# ---- 7. A7: kill the run after chosen records, resume: replay must not diverge ----
resume_case() { # name crash-seq replies-json   (runs the scenario, SIGKILLs it after record #seq, resumes against the remaining replies)
  newws $1; mock "$3" $T/$1.c.req 8815; HX_TEST_CRASH_AFTER_RECORD=$2 run $W 8815 $T/$1.c.tr --pack std/compact; local crashed=$?; unmock
  local log=$(lastlog $W)
  local rest=$(python3 - "$log" "$3" <<'PY'
import json, sys
llm = [json.loads(l) for l in open(sys.argv[1]) if l.strip().endswith("}")]
llm = [r for r in llm if r["kind"] == "llm"]
n = len(llm) - (1 if llm and "error" in llm[-1] else 0)  # a trailing recorded failure is asked again on resume
print(json.dumps(json.loads(sys.argv[2])[n:]))
PY
)
  mock "$rest" $T/$1.r.req 8815; run $W 8815 $T/$1.r.tr --pack std/compact --resume $log; local rc=$?; unmock
  $H log verify $log > $T/verify 2>&1; local vrc=$?
  echo "$([ $crashed -ne 0 ] && echo killed) $rc $(field finished) $(grep -c 'replay diverged' $T/err) $vrc"
}
seq_of() { python3 - "$1" "$2" <<'PY'  # log, n -> seq of the n-th llm record
import json, sys
print([json.loads(l)["seq"] for l in open(sys.argv[1]) if '"kind":"llm"' in l][int(sys.argv[2]) - 1])
PY
}
LOG2=$(lastlog $A2W)
check "A7 killed right after the summarizer's llm record, resumed" "$(resume_case r7a $(seq_of $LOG2 3) "$S2")" "killed 0 True 0 0"
check "A7 killed after the first model call following the compaction, resumed" "$(resume_case r7b $(seq_of $LOG2 4) "$S2")" "killed 0 True 0 0"
check "A7 killed mid-run before the compaction (summarizer re-issued), resumed" "$(resume_case r7c $(seq_of $LOG2 2) "$S2")" "killed 0 True 0 0"
LOG6=$(lastlog $A6W)
check "A7b overflow run, killed after the recorded failure, resumed" "$(resume_case r7d $(seq_of $LOG6 3) "$S6")" "killed 0 True 0 0"
check "A7b overflow run, killed after the summarizer's record, resumed" "$(resume_case r7e $(seq_of $LOG6 4) "$S6")" "killed 0 True 0 0"
check "A7b overflow run, killed after the retry, resumed" "$(resume_case r7f $(seq_of $LOG6 5) "$S6")" "killed 0 True 0 0"
check "A7b resuming the resumed r7d log again replays all of it (the re-issued failure supersedes the recorded one)" "$(
LOG=$(lastlog $T/r7d)
mock '["unused"]' $T/r7d2.req 8815; run $T/r7d 8815 $T/r7d2.tr --pack std/compact --resume $LOG; rc=$?; unmock
echo "$rc $(field finished) $(grep -c 'replay diverged' $T/err) $(Q 'print(len(load("'$T'/r7d2.req")))' 2>/dev/null || echo 0)")" "0 True 0 0"
check "A7b a resumed run that differs at a trailing recorded failure reports replay diverged" "$(
S7h=$(R 'dump(C("a=1"), E("invalid request: unsupported value"), S("live answer"))')
newws r7h; mock "$S7h" $T/r7h.c.req 8815; run $W 8815 $T/r7h.c.tr; c=$?; unmock; LOG=$(lastlog $W)
echo 'require("std/hooks").on("before_model", function(r) if #r.messages > 1 then r.system ..= "\nCHANGED" end return r end)' >> $W/.harness/init.luau
mock "$(R 'dump(S("live answer"))')" $T/r7h.r.req 8815; run $W 8815 $T/r7h.r.tr --resume $LOG; rc=$?; unmock
echo "$([ $c -ne 0 ] && echo failed) $([ $rc -ne 0 ] && echo fail) $(grep -c 'replay diverged' $T/err)")" "failed fail 1"
check "A7b the error is a normal llm record in the log (error text, no reply)" "$(python3 - <<PY
import json
r = [json.loads(l) for l in open("$LOG6") if '"kind":"llm"' in l]
print(len(r), "maximum context length exceeded" in r[2].get("error", ""), "reply" in r[2])
PY
)" "6 True False"
check "A7b resuming a complete overflow log replays all of it (no live call)" "$(
cp -r $A6W $T/r7g; LOG=$(lastlog $T/r7g)
mock '["unused"]' $T/r7g.req 8815; run $T/r7g 8815 $T/r7g.tr --pack std/compact --resume $LOG; rc=$?; unmock
echo "$rc $(field finished) $(grep -c 'replay diverged' $T/err) $(Q 'print(len(load("'$T'/r7g.req")))' 2>/dev/null || echo 0)")" "0 True 0 0"

# ---- 8. A8: a subagent compacts its own history ----
S8=$(R 'dump({"text": "", "calls": [{"code": "local r = agent(\"sub\")\nprint(r.answer)"}]}, C("a=1 -- CHILD-MARK"), C("b=2 -- CHILD-MARK"), C("c=3 -- CHILD-MARK", 30000), S("CHILD-SUMMARY"), S("child done"), S("parent done"))')
newws a8; mock "$S8" $T/a8.req 8816; run $W 8816 $T/a8.tr --pack std/compact; rc=$?; unmock
check "A8 subagent crossing the threshold sends a summarizer request with the child's history" "$rc $(field finished) $(Q 's = sums("'$T'/a8.req"); print(len(s), "CHILD-MARK" in conv(s[0]), "agent(" in conv(s[0]))')" "0 True 1 True False"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
