#!/usr/bin/env bash
# Vision pack checks (scripted model, no API key): view() queues images and the next request carries them, for
# Chat Completions and Responses; completed resume does not reread images; without the pack nothing changes.
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
MOCK_REPLIES="$REPLIES" MOCK_BY_LEN=1 MOCK_LOG=$T/req.jsonl python3 scripts/mock_llm.py 8797 & M=$!
wait_for_port 8797 || exit 1; echo go > $T/p.txt
run() { OPENROUTER_API_KEY=x $H run --workspace $1 --prompt-file $T/p.txt --model m --api $2 --base-url http://127.0.0.1:8797 --key-env OPENROUTER_API_KEY "${@:3}" 2>/dev/null; }
img_parts() { python3 -c "
import json
b=[json.loads(x) for x in open('$T/req.jsonl')][-1]
msgs=b.get('messages') or b.get('input')
n=0
for m in msgs:
    c=m.get('content')
    if isinstance(c,list): n+=sum(1 for p in c if p.get('type') in ('image_url','input_image'))
print(n)"; }

run $W completions --pack std/vision --trace $T/tr1.txt >/dev/null
check "view reports the scaled size" "$(grep -A1 '### RESULT' $T/tr1.txt | sed -n 2p)" "1024	576"
check "chat: the next request carries one image" "$(img_parts)" "1"
check "chat: image is a PNG data URL" "$(tail -1 $T/req.jsonl | grep -c 'data:image/png;base64,')" "1"
check "chat: cache marker on the last part" "$(tail -1 $T/req.jsonl | python3 -c "
import json,sys
b=json.load(sys.stdin); m=b['messages'][-1]; print(m['role'], m['content'][-1].get('cache_control') is not None)")" "user True"
check "the prompt documents view()" "$(tail -1 $T/req.jsonl | grep -c 'view(path')" "1"
: > $T/req.jsonl; run $W responses --pack std/vision >/dev/null
check "responses: input_image part" "$(img_parts)" "1"
: > $T/req.jsonl; run $W completions >/dev/null
check "without the pack: no view, no prompt line" "$(tail -1 $T/req.jsonl | grep -c 'view(path')" "0"
check "without the pack: no images sent" "$(img_parts)" "0"
check "event log has no pixels anywhere" "$(cat $W/.harness/log/*.jsonl | grep -c 'iVBORw0KGgo')" "0"
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
echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
