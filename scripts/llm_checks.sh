#!/usr/bin/env bash
# LLM client checks (also: loop nudge on garbled tool-call markup): each wire format (chat completions, responses, anthropic messages) completes a run against a scripted
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
# --- caching: markers per API, cache key, provider pin
runq() { # api model extra... -> request log of one run
  local api=$1 model=$2; shift 2; W=$T/c-$RANDOM; mkdir -p $W/.harness; : > $T/req.log
  K=x $H run --workspace $W --prompt-file $T/p.txt --model $model --api $api --base-url http://127.0.0.1:8795 --key-env K "$@" >/dev/null 2>&1
}
has() { grep -c "$1" $T/req.log | tr -d ' ' | sed 's/^0$/no/; s/^[1-9][0-9]*$/yes/'; }
runq anthropic claude-x;            check "anthropic: cache_control markers" "$(has cache_control)" "yes"
runq openai any/model;              check "chat completions: cache_control markers, any model" "$(has cache_control)" "yes"
runq openai any/model;              check "chat completions: prompt_cache_key sent" "$(has prompt_cache_key)" "yes"
runq responses any/model;           check "responses: no markers (automatic caching)" "$(has cache_control)" "no"
runq responses any/model;           check "responses: prompt_cache_key sent" "$(has prompt_cache_key)" "yes"
runq anthropic claude-x --cache off; check "no way to turn caching off: --cache is not a flag" "$(has cache_control)" "yes"
runq openai any/model --provider-only anthropic,google; check "--provider-only pins providers" "$(has allow_fallbacks)" "yes"
runq anthropic claude-x --provider-only anthropic; check "provider pin is chat-completions only" "$(has allow_fallbacks)" "no"
check "unknown api is refused" "$(K=x $H run --workspace $T/openai --prompt-file $T/p.txt --model m --api bogus --base-url http://127.0.0.1:8795 --key-env K 2>&1 | grep -c 'unknown --api')" "1"
# --- malformed tool-call markup after a code turn is nudged, not taken as the final answer; bare <invoke> code is run
mockrun() { # replies-json -> report fields "finished|final_answer|turns|code_turns|truncated_turns"
  MOCK_REPLIES="$1" MOCK_BY_LEN=1 python3 scripts/mock_llm.py 8796 & local m=$!; sleep 1
  W=$T/m-$RANDOM; mkdir -p $W/.harness
  local o; o=$(K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --api openai --base-url http://127.0.0.1:8796 --key-env K 2>&1 | tail -1)
  kill $m 2>/dev/null; wait $m 2>/dev/null
  python3 -c "import sys,json;r=json.loads(sys.argv[1]);print('|'.join(str(r[k]) for k in ('finished','final_answer','turns','code_turns','truncated_turns')))" "$o" 2>/dev/null
}
check "garbled </invoke> after a code turn: nudged, run continues" "$(mockrun '["```luau\nprint(7)\n```","<\n<parameter name=\"cmd\">I will fix it</parameter>\n</invoke>\nelu","```luau\nprint(8)\n```","all done"]')" "True|all done|4|2|1"
check "bare <invoke-luau> raw code is extracted and run" "$(mockrun '["<invoke-luau>\nprint(9)\n</invoke-luau>","all done"]')" "True|all done|2|1|0"
check "plain prose after a code turn still finishes (no nudge)" "$(mockrun '["```luau\nprint(7)\n```","The fix is in place."]')" "True|The fix is in place.|2|1|0"
echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
