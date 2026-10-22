#!/usr/bin/env bash
# std/compact pack checks (scripted model, no API key): threshold compaction, the replayed summarizer request, merged
# summaries, globals surviving, summary validation, overflow compact-and-retry, subagents. The mock's usage numbers
# (prompt_tokens) decide when the threshold is crossed.
# Window 40000, max output 1000 => threshold min(38000, 40000-1000-16384) = 22616; a reply with prompt_tokens 23000 crosses it,
# and the summarizer prefix still fits under 40000-16384 = 23616.
set -u
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); MP=""; trap 'rm -rf "$T"; kill $MP 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
echo "TASKTEXT-SCRIPTED" > $T/p.txt

# reply builders (C: a luau call with ~2000 chars of padding, so a summary is smaller than what it replaces; S: plain text; E: HTTP 400)
cat > $T/r.py <<'PY'
import json
PAD = " -- " + "p" * 2000
def C(code, tok=10): return {"text": "", "calls": [{"code": code + PAD}], "prompt_tokens": tok}
def S(text): return {"text": text}
def E(msg): return {"status": 400, "error": msg}
def dump(*r): print(json.dumps(list(r)))
PY
cat > $T/q.py <<'PY'
import json
def text(c): return c if isinstance(c, str) else "".join(p.get("text", "") for p in (c or []) if isinstance(p, dict))
def load(f): return [json.loads(l) for l in open(f)]
def is_sum(b): return "Write a context checkpoint" in text(b["input"][-1].get("content"))
def msgs(b): return b["input"]
def sums(f): return [b for b in load(f) if is_sum(b)]
def conv(b): return json.dumps(b["input"])
PY
R() { PYTHONPATH=$T python3 -c "from r import *; $1"; }   # R 'dump(C("x=1"), S("done"))' -> JSON list
Q() { PYTHONPATH=$T python3 -c "from q import *; $1"; }

mock() { MOCK_REPLIES="$1" MOCK_LOG="$2" serve PORT python3 scripts/mock_llm.py 0 || exit 1; MP=$SERVED; }  # replies-json log; sets PORT
unmock() { kill $MP 2>/dev/null; wait $MP 2>/dev/null; MP=""; }
newws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; printf 'require("std/compact").keep_ratio = 0.001\nrequire("std/recheck").enabled = false\n' > $W/.harness/init.luau; }
off() { echo 'require("std/compact").enabled = false' >> $W/.harness/init.luau; }  # the pack is on by default
run() { # workspace port trace extra-flags...   (stdout/stderr in $T/out, $T/err; status is hx's)
  OPENROUTER_API_KEY=x $H run --workspace $1 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$2 --key-env OPENROUTER_API_KEY \
    --context-window 40000 --max-output 1000 --trace $3 "${@:4}" > $T/out 2> $T/err
}
field() { python3 -c "import sys,json;r=json.loads(open('$T/out').read().strip().splitlines()[-1]);print(r.get('$1'))" 2>/dev/null; }
lastlog() { ls -t $1/.harness/log/*.jsonl | head -1; }

# ---- 1. A1: with the pack off, and with it on below the threshold, requests are identical ----
THREE=$(R 'dump(C("a=1"), C("b=2"), S("done"))')
newws a1; off; mock "$THREE" $T/a1.plain; run $W $PORT $T/a1.tr1; unmock
newws a1b; mock "$THREE" $T/a1.pack; run $W $PORT $T/a1.tr2 --pack std/compact; unmock
check "A1 pack below the threshold: request bodies identical to a run with the pack off" "$(python3 - <<PY
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
S2=$(R 'dump(C("x = 42 -- TURN-ONE-MARKER"), C("y = 1", 23000), S("SUMMARY-ONE"), {"text": "", "calls": [{"code": "print(x)"}]}, S("done"))')
newws a2; mock "$S2" $T/a2.req; run $W $PORT $T/a2.tr --pack std/compact; rc=$?; unmock
A2W=$W
check "A2 run with a compaction finishes" "$rc $(field finished)" "0 True"
check "A2 exactly one summarizer request, 5 requests in all" "$(Q 'r = load("'$T'/a2.req"); print(len(r), len(sums("'$T'/a2.req")))')" "5 1"
check "A2 summarizer request is the previous agent request's prefix plus one user message" "$(Q '
a, b = load("'$T'/a2.req")[1:3]; n = len(msgs(b)) - 1
print(is_sum(b), b["instructions"] == a["instructions"], b["tools"] == a["tools"], msgs(b)[:n] == msgs(a)[:n], msgs(b)[-1]["role"])')" "True True True True user"
check "A2 summarizer sees the task and exact history; the kept tail is not replayed" "$(Q 'c = conv(load("'$T'/a2.req")[2]); print("TURN-ONE-MARKER" in c, "TASKTEXT-SCRIPTED" in c, "Dead Ends" in c, "y = 1" in c)')" "True True True False"
check "A2 next request: task + summary block, then assistant output, tool results answer calls in the tail" "$(Q '
b = load("'$T'/a2.req")[3]; m = msgs(b)
ids = {x["call_id"] for x in m if x.get("type") == "function_call"}
tools = [x for x in m if x.get("type") == "function_call_output"]
first = text(m[0]["content"])
print(m[0]["role"], "TASKTEXT-SCRIPTED" in first, "[compaction-summary]" in first, "SUMMARY-ONE" in first, "</summary>" in first, bool(ids), len(tools) > 0 and all(t["call_id"] in ids for t in tools))')" "user True True True True True True"
check "A2 the dropped history is gone from the next request" "$(Q 'print("TURN-ONE-MARKER" in json.dumps(load("'$T'/a2.req")[3]))')" "False"
check "A2 trace records the compaction" "$(grep -c '^### COMPACT$' $T/a2.tr)" "1"
check "A4 a global set before the compaction is readable after it" "$(awk '/^print\(x\)$/{s=1} s&&/^### RESULT/{getline; print; exit}' $T/a2.tr)" "42"

# ---- 4. A3: the second compaction sees the first summary (in the task message) and merges it ----
S4=$(R 'dump(C("x = 42 -- TURN-ONE-MARKER"), C("y = 1", 23000), S("SUMMARY-ONE"), C("print(x) -- TURN-THREE-MARKER"), C("z = 2 -- TURN-FOUR-MARKER", 23000), S("SUMMARY-TWO"), S("done"))')
newws a3; mock "$S4" $T/a3.req; run $W $PORT $T/a3.tr --pack std/compact; rc=$?; unmock
check "A3 run with two compactions finishes" "$rc $(field finished) $(Q 'print(len(sums("'$T'/a3.req")))')" "0 True 2"
check "A3 second summarizer request carries the first summary, the merge rule and only newer messages" "$(Q '
c = conv(sums("'$T'/a3.req")[1])
print("SUMMARY-ONE" in c, "earlier checkpoint" in c, "TURN-ONE-MARKER" not in c, "TURN-THREE-MARKER" in c)')" "True True True True"
check "A3 the final request holds the second summary only" "$(Q '
t = text(msgs(load("'$T'/a3.req")[-1])[0]["content"]); print("SUMMARY-TWO" in t, "SUMMARY-ONE" in t)')" "True False"

# ---- 5. A5: a failed summarizer leaves messages alone; two failures switch the pack off ----
S5=$(R 'dump(C("a=1"), C("b=2"), C("c=3", 23000), S(""), C("d=4", 23000), S(""), C("e=5", 23000), S("done"))')
newws a5; mock "$S5" $T/a5.req; run $W $PORT $T/a5.tr --pack std/compact; rc=$?; unmock
check "A5 failing summarizer: run finishes, history never replaced" "$rc $(field finished) $(grep -c '<summary>' $T/a5.req)" "0 True 0"
check "A5 two failures logged, then no third summarizer request" "$(grep -c '^### COMPACT FAILED$' $T/a5.tr) $(Q 'print(len(sums("'$T'/a5.req")))')" "2 2"
check "A5 a summarizer API error counts as a failure too" "$(
S5b=$(R 'dump(C("a=1"), C("b=2"), C("c=3", 23000), E("boom"), C("d=4", 23000), S("done"))'); newws a5b; mock "$S5b" $T/a5b.req; run $W $PORT $T/a5b.tr --pack std/compact; rc=$?; unmock
echo "$rc $(field finished) $(grep -c '^### COMPACT FAILED$' $T/a5b.tr)")" "0 True 1"

S5c=$(R 'dump(C("a=1"), C("b=2"), C("c=3", 23000), {"text": "SUMMARY-CUT", "finish": "length"}, C("d=4", 23000), {"text": "x", "calls": [{"code": "print(1)"}]}, S("done"))')
newws a5c; mock "$S5c" $T/a5c.req; run $W $PORT $T/a5c.tr --pack std/compact; rc=$?; unmock
check "A5 truncated and tool-calling summaries are rejected" "$rc $(field finished) $(grep -c '^### COMPACT FAILED$' $T/a5c.tr) $(grep -c '<summary>' $T/a5c.req)" "0 True 2 0"
S5d=$(R 'dump(C("a=1"), C("b=2"), C("c=3", 23000), S("L" * 40000), S("done"))')
newws a5d; mock "$S5d" $T/a5d.req; run $W $PORT $T/a5d.tr --pack std/compact; rc=$?; unmock
check "A5 a summary larger than what it replaces is rejected" "$rc $(field finished) $(grep -c '^### COMPACT FAILED$' $T/a5d.tr) $(grep -c '<summary>' $T/a5d.req)" "0 True 1 0"

# ---- 6. A6: a context-overflow error compacts once (keeping as little as fits) and retries the same turn ----
# reserve 10: the scripted contexts are tiny, so the default summarizer headroom (16384) would leave room for no prefix
S6=$(R 'dump(C("a=1"), C("b=2"), E("maximum context length exceeded"), S("SUMMARY-OV"), C("print(a) -- AFTER"), S("done"))')
newws a6; echo 'require("std/compact").reserve_tokens = 10' >> $W/.harness/init.luau; mock "$S6" $T/a6.req; run $W $PORT $T/a6.tr --pack std/compact; rc=$?; unmock
A6W=$W; A6REQ=$T/a6.req
check "A6 overflow then compaction then retry: run finishes" "$rc $(field finished) $(field turns)" "0 True 4"
check "A6 the overflow compaction keeps as little as fits (here the newest exchange)" "$(Q 'c = conv(sums("'$T'/a6.req")[0]); print("a=1" in c, "b=2" in c)')" "True False"
check "A6 the retry carries the summary; the failed request did not" "$(Q 'r = load("'$T'/a6.req"); print(len(r), "compaction-summary" in json.dumps(r[2]), "SUMMARY-OV" in json.dumps(r[4]))')" "6 False True"
S6b=$(R 'dump(C("a=1"), C("b=2"), E("maximum context length exceeded"), S("SUMMARY-OV"), E("context_length_exceeded again"), S("never used"))')
newws a6b; echo 'require("std/compact").reserve_tokens = 10' >> $W/.harness/init.luau; mock "$S6b" $T/a6b.req; run $W $PORT $T/a6b.tr --pack std/compact; rc=$?; unmock
check "A6 a second overflow on the same turn fails the run and says so" "$([ $rc -ne 0 ] && echo fail) $(grep -c 'failed again after one compact-and-retry' $T/err) $(Q 'print(len(load("'$T'/a6b.req")))')" "fail 1 5"
S6c=$(R 'dump(C("a=1"), C("b=2"), E("maximum context length exceeded"), S(""), S("never used"))')
newws a6c; echo 'require("std/compact").reserve_tokens = 10' >> $W/.harness/init.luau; mock "$S6c" $T/a6c.req; run $W $PORT $T/a6c.tr --pack std/compact; rc=$?; unmock
check "A6 overflow with a failed compaction ends the run with the API error" "$([ $rc -ne 0 ] && echo fail) $(grep -c 'maximum context length exceeded' $T/err)" "fail 1"
newws a6d; off; mock "$S6c" $T/a6d.req; run $W $PORT $T/a6d.tr; rc=$?; unmock
check "A6 with the pack off an overflow fails as before (no summarizer call)" "$([ $rc -ne 0 ] && echo fail) $(Q 'print(len(load("'$T'/a6d.req")))')" "fail 3"

# ---- 6c. on by default: no --pack needed; enabled = false turns it off ----
newws a6e; mock "$S2" $T/a6e.req; run $W $PORT $T/a6e.tr; rc=$?; unmock
check "6c default: compacts without --pack (one summarizer request)" "$rc $(field finished) $(Q 'print(len(sums("'$T'/a6e.req")))')" "0 True 1"
newws a6f; off; mock "$S2" $T/a6f.req; run $W $PORT $T/a6f.tr; rc=$?; unmock
check "6c enabled = false: no summarizer request, no summary in any request" "$(Q 'print(len(sums("'$T'/a6f.req")))') $(grep -c 'compaction-summary' $T/a6f.req)" "0 0"

# ---- 6b. real-model shape: reasoning (echoed back in raw) dominates the context, and a small keep_ratio ----
S6r=$(R 'R1 = "r" * 6000; dump(*[dict(C("t%d = %d" % (i, i), 30000 if i == 8 else 3000 * i), reasoning=R1) for i in range(1, 9)], S("SUMMARY-R"), C("print(t1)", 2000), S("done"))')
W=$T/a6r; mkdir -p $W/.harness; echo 'require("std/compact").keep_ratio = 0.128' > $W/.harness/init.luau
echo 'require("std/recheck").enabled = false' >> $W/.harness/init.luau
mock "$S6r" $T/a6r.req; run $W $PORT $T/a6r.tr --pack std/compact; rc=$?; unmock
check "6b reasoning-heavy turns: the compaction summarizes most of the history, keeping a short tail" "$rc $(field finished) $(grep -A1 '^### COMPACT$' $T/a6r.tr | tail -1 | sed -E 's/summarized ([0-9]+) messages, kept ([0-9]+),.*/\1 \2/')" "0 True 12 4"

# ---- 8. A8: a subagent compacts its own history ----
S8=$(R 'dump({"text": "", "calls": [{"code": "local r = agent(\"sub\")\nprint(r.answer)"}]}, C("a=1 -- CHILD-MARK"), C("b=2 -- CHILD-MARK"), C("c=3 -- CHILD-MARK", 23000), S("CHILD-SUMMARY"), S("child done"), S("parent done"))')
newws a8; mock "$S8" $T/a8.req; run $W $PORT $T/a8.tr --pack std/compact; rc=$?; unmock
check "A8 subagent crossing the threshold sends a summarizer request with the child's history" "$rc $(field finished) $(Q 's = sums("'$T'/a8.req"); print(len(s), "CHILD-MARK" in conv(s[0]), "agent(" in conv(s[0]))')" "0 True 1 True False"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
