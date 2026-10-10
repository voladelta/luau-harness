#!/usr/bin/env bash
# LLM client checks: each wire format (chat completions, responses, anthropic messages) completes a run against a scripted
# server, and an openrouter-looking URL does not restrict the upstream provider. No API key needed.
set -u
cd "$(dirname "$0")/.."
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $M 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
MOCK_REPLIES='["```luau\nprint(7)\n```","all done"]' MOCK_BY_LEN=1 MOCK_LOG=$T/req.log python3 scripts/mock_llm.py 8795 & M=$!
sleep 1; echo go > $T/p.txt
for api in openai responses anthropic; do
  W=$T/$api; mkdir -p $W/.harness; : > $T/req.log
  out=$(K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --api $api --base-url http://127.0.0.1:8795/openrouter --key-env K 2>&1 | tail -1)
  check "$api: run finishes with the model's answer" "$(python3 -c "import sys,json;r=json.loads(sys.argv[1]);print(r['finished'], r['final_answer'], r['turns'])" "$out" 2>/dev/null)" "True all done 2"
  check "$api: request carries no provider restriction" "$(grep -c '"provider"' $T/req.log | tr -d ' ')" "0"
done
check "unknown api is refused" "$(K=x $H run --workspace $T/openai --prompt-file $T/p.txt --model m --api bogus --base-url http://127.0.0.1:8795 --key-env K 2>&1 | grep -c 'unknown --api')" "1"
echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
