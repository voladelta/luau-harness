#!/usr/bin/env bash
# Vision pack checks (scripted model, no API key): view() queues images and the next request carries them, for
# Responses; completed resume does not reread images; without the pack nothing changes.
set -u
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $M 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
W=$T/ws; mkdir -p $W
ffmpeg -v error -f lavfi -i testsrc=size=1600x900 -frames:v 1 $W/frame.png
REPLIES='["```luau\nlocal r = view(\"frame.png\")\nprint(r[1].width, r[1].height)\n```","done"]'
MOCK_REPLIES="$REPLIES" MOCK_BY_LEN=1 MOCK_LOG=$T/req.jsonl serve MP python3 scripts/mock_llm.py 0 || exit 1; M=$SERVED
echo go > $T/p.txt
run() { OPENROUTER_API_KEY=x $H run --workspace $1 --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$MP --key-env OPENROUTER_API_KEY "${@:2}" 2>/dev/null; }
img_parts() { python3 -c "
import json
b=[json.loads(x) for x in open('$T/req.jsonl')][-1]
msgs=b['input']
n=0
for m in msgs:
    c=m.get('content')
    if isinstance(c,list): n+=sum(1 for p in c if p.get('type') == 'input_image')
print(n)"; }

run $W --pack std/vision --trace $T/tr1.txt >/dev/null
check "view reports the scaled size" "$(grep -A1 '### RESULT' $T/tr1.txt | sed -n 2p)" "1024	576"
check "responses: the next request carries one image" "$(img_parts)" "1"
check "responses: image is a PNG data URL" "$(tail -1 $T/req.jsonl | grep -c 'data:image/png;base64,')" "1"
check "the prompt documents view()" "$(tail -1 $T/req.jsonl | grep -c 'view(path')" "1"
: > $T/req.jsonl; run $W >/dev/null
check "without the pack: no view, no prompt line" "$(tail -1 $T/req.jsonl | grep -c 'view(path')" "0"
check "without the pack: no images sent" "$(img_parts)" "0"
check "event log has no pixels anywhere" "$(cat $W/.harness/log/*.jsonl | grep -c 'iVBORw0KGgo')" "0"
ffmpeg -v error -f rawvideo -pix_fmt rgb24 -s 2000x2000 -i /dev/urandom -frames:v 1 $W/noise.png  # ~16 MB of base64
check "one turn takes at most half the image byte budget" "$($H exec --workspace $W --pack std/vision 'print(pcall(view, "noise.png", {max = 2000}))' 2>&1 | grep -c 'over the 8 MB')" "1"
check "at most 4 images per turn across calls" "$($H exec --workspace $W --pack std/vision 'view({"frame.png","frame.png","frame.png"})' 'print(pcall(view, {"frame.png","frame.png"}))' 2>&1 | grep -c 'per turn')" "1"

# Completed recovery returns the saved report and captured conversation; it does not reread old images.
LOG=$(grep -l '"tool":"view"' $W/.harness/log/*.jsonl | head -1)
resume() { OPENROUTER_API_KEY=x $H run --workspace $W --resume $LOG --trace $T/$1 >$T/$1.out 2>$T/$1.err; }
before=$(wc -l < $T/req.jsonl | tr -d ' ')
resume tr2.txt; rc=$?
check "completed image run resumes without another model call" "$rc $([ "$before" = "$(wc -l < $T/req.jsonl | tr -d ' ')" ] && echo no-call)" "0 no-call"
ffmpeg -v error -y -f lavfi -i testsrc=size=800x600 -frames:v 1 $W/frame.png
resume tr3.txt; rc=$?
check "changed original image does not alter committed history" "$rc $([ "$before" = "$(wc -l < $T/req.jsonl | tr -d ' ')" ] && echo no-call)" "0 no-call"

# Image budget: passing it compacts; the summarizer sees the older image, the next request keeps only the newest.
kill $M 2>/dev/null
nth_imgs() { python3 -c "
import json
b=[json.loads(x) for x in open('$T/$1')][$2]
print(sum(1 for m in b['input'] if isinstance(m.get('content'),list) for p in m['content'] if p.get('type')=='input_image'))"; }
nth_has() { python3 -c "
import json
print(int('$3' in json.dumps([json.loads(x) for x in open('$T/$1')][$2])))"; }
VIEW='```luau\nview(\"frame.png\")\n```'
budget_run() {  # replies log init-lines
    W2=$T/$2; rm -rf $W2; mkdir -p $W2/.harness; cp $W/frame.png $W2/
    printf 'require("std/recheck").enabled = false\n%s\n' "$3" > $W2/.harness/init.luau
    MOCK_REPLIES="$1" MOCK_LOG=$T/$2.jsonl serve MP python3 scripts/mock_llm.py 0 || exit 1; M=$SERVED
    run $W2 --pack std/vision >/dev/null; echo $? > $T/$2.rc; kill $M 2>/dev/null
}
budget_run "[\"$VIEW\",\"$VIEW\",\"SUM\",\"done\"]" budget 'require("std/compact").max_image_bytes = 1'
check "budget: run finishes" "$(cat $T/budget.rc)" "0"
check "budget: the summarizer sees the older image" "$(nth_imgs budget.jsonl 2)" "1"
check "budget: the summarizer is told images are not kept" "$(nth_has budget.jsonl 2 'not kept after this checkpoint')" "1"
check "budget: the next request carries only the newest image" "$(nth_imgs budget.jsonl 3) $(nth_has budget.jsonl 3 'compaction-summary')" "1 1"

# An image rejection (413) compacts without a token bound; the summarizer gets placeholders, then the agent retries.
budget_run "[\"$VIEW\",\"$VIEW\",{\"status\":413,\"error\":\"Payload Too Large\"},\"SUM\",\"done\"]" reject ''
check "413: run finishes after compact-and-retry" "$(cat $T/reject.rc)" "0"
check "413: the summarizer gets no images, only placeholders" "$(nth_imgs reject.jsonl 3) $(nth_has reject.jsonl 3 '[image frame.png 800x600 not shown')" "0 1"
check "413: the retry carries the newest image" "$(nth_imgs reject.jsonl 4) $(nth_has reject.jsonl 4 'compaction-summary')" "1 1"
echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
