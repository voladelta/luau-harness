#!/usr/bin/env bash
# LLM client checks (also: loop nudge on garbled tool-call markup): each wire format (chat completions, responses) completes a run against a scripted
# server, and an openrouter-looking URL does not restrict the upstream provider. No API key needed.
set -u
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $M 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
MOCK_REPLIES='["```luau\nprint(7)\n```","all done"]' MOCK_BY_LEN=1 MOCK_LOG=$T/req.log python3 scripts/mock_llm.py 8795 & M=$!
wait_for_port 8795 || exit 1; echo go > $T/p.txt
for api in openai responses; do
  W=$T/$api; mkdir -p $W/.harness; : > $T/req.log
  echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
  out=$(K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --api $api --base-url http://127.0.0.1:8795/openrouter --key-env K 2>&1 | tail -1)
  check "$api: run finishes with the model's answer" "$(python3 -c "import sys,json;r=json.loads(sys.argv[1]);print(r['finished'], r['final_answer'], r['turns'])" "$out" 2>/dev/null)" "True all done 2"
  check "$api: request carries no provider restriction" "$(grep -c '"provider"' $T/req.log | tr -d ' ')" "0"
done
# --- caching: markers per API, cache key, provider pin
runq() { # api model extra... -> request log of one run
  local api=$1 model=$2; shift 2; W=$T/c-$RANDOM; mkdir -p $W/.harness; : > $T/req.log
  echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
  K=x $H run --workspace $W --prompt-file $T/p.txt --model $model --api $api --base-url http://127.0.0.1:8795 --key-env K "$@" >/dev/null 2>&1
}
has() { grep -c "$1" $T/req.log | tr -d ' ' | sed 's/^0$/no/; s/^[1-9][0-9]*$/yes/'; }
runq openai any/model;              check "chat completions: cache_control markers, any model" "$(has cache_control)" "yes"
runq openai any/model;              check "chat completions: prompt_cache_key sent" "$(has prompt_cache_key)" "yes"
runq responses any/model;           check "responses: no markers (automatic caching)" "$(has cache_control)" "no"
runq responses any/model;           check "responses: prompt_cache_key sent" "$(has prompt_cache_key)" "yes"
runq openai any/model --provider-only anthropic,google; check "--provider-only pins providers" "$(has allow_fallbacks)" "yes"
check "unknown api is refused" "$(K=x $H run --workspace $T/openai --prompt-file $T/p.txt --model m --api anthropic --base-url http://127.0.0.1:8795 --key-env K 2>&1 | grep -c 'unknown --api')" "1"
# --- native tool calling: the loop answers every call id, per wire format; bad calls get an error result, not a crash
mockrun() { # replies-json api -> "finished|final_answer|turns|code_turns|truncated_turns"
  MOCK_REPLIES="$1" MOCK_BY_LEN=1 MOCK_LOG=$T/tool.log python3 scripts/mock_llm.py 8796 & local m=$!; wait_for_port 8796 || return 1; : > $T/tool.log
  W=$T/m-$RANDOM; mkdir -p $W/.harness
  echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
  local o; o=$(K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --api ${2:-openai} --base-url http://127.0.0.1:8796 --key-env K 2>&1 | tail -1)
  kill $m 2>/dev/null; wait $m 2>/dev/null
  python3 -c "import sys,json;r=json.loads(sys.argv[1]);print('|'.join(str(r[k]) for k in ('finished','final_answer','turns','code_turns','truncated_turns')))" "$o" 2>/dev/null
}
TWO='[{"text":"","calls":[{"code":"print(1)"},{"code":"print(2)"}]},"all done"]'
for api in openai responses; do
  check "$api: two parallel calls both run, run finishes" "$(mockrun "$TWO" $api)" "True|all done|2|2|0"
  case $api in
    openai) want='"role": "tool"' ;; responses) want='function_call_output' ;;
  esac
  check "$api: results go back in the native tool-result shape (one per call)" "$(grep -o "$want" $T/tool.log | wc -l | tr -d ' ')" "2"
done
check "invalid tool arguments: error result, run continues" "$(mockrun '[{"calls":[{"name":"luau","arguments":"{\"code\": \"print(1"}]},"```luau\nprint(2)\n```","all done"]')" "True|all done|3|1|1"
check "unknown tool name: error result, run continues" "$(mockrun '[{"calls":[{"name":"bash","arguments":"{}"}]},"```luau\nprint(2)\n```","all done"]')" "True|all done|3|1|0"
check "plain prose after a code turn finishes (no nudge)" "$(mockrun '["```luau\nprint(7)\n```","The fix is in place."]')" "True|The fix is in place.|2|1|0"
check "prose before any code is nudged to start" "$(mockrun '["I will look.","```luau\nprint(7)\n```","done"]')" "True|done|3|1|1"
check "the luau tool is declared in the request" "$(grep -c '"luau"' $T/tool.log | tr -d ' ' | sed 's/^[1-9][0-9]*$/yes/')" "yes"
echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
