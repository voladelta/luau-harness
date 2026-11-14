#!/usr/bin/env bash
# Responses successes include "error": null, as real providers do.
# LLM client checks: the Responses API completes a run against a scripted server (request shape, streaming, caching,
# provider pins, the provider's model list, proxies). No API key needed. The loop's handling of replies and failures
# is tested in-process: src/tests/.
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
cleanup() { kill $M 2>/dev/null; }
MOCK_REPLIES='["```luau\nprint(7)\n```","all done"]' MOCK_BY_LEN=1 MOCK_LOG=$T/req.log serve MP python3 scripts/mock_llm.py 0 || exit 1; M=$SERVED
echo go > $T/p.txt
W=$T/responses; mkdir -p $W/.harness; : > $T/req.log
echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
out=$(K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --base-url http://127.0.0.1:$MP/openrouter --key-env K 2>&1 | tail -1)
check "responses: run finishes with the model's answer" "$(python3 -c "import sys,json;r=json.loads(sys.argv[1]);print(r['finished'], r['final_answer'], r['turns'])" "$out" 2>/dev/null)" "True all done 2"
check "responses: request carries no provider restriction" "$(grep -c '"provider"' $T/req.log | tr -d ' ')" "0"
check "responses: the luau tool is declared in the request" "$(grep -c '"name": "luau"' $T/req.log | tr -d ' ')" "2"
check "responses: the chunk's result goes back in the native tool-result shape" "$(grep -o '"type": "function_call_output"' $T/req.log | wc -l | tr -d ' ')" "1"
check "responses: requests ask for a streamed response" "$(grep -c '"stream": true' $T/req.log | tr -d ' ')" "2"
# --- automatic caching, cache key, provider pin
runq() { # model extra... -> request log of one run
  local model=$1; shift; W=$T/c-$RANDOM; mkdir -p $W/.harness; : > $T/req.log
  echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
  K=x $H run --workspace $W --prompt-file $T/p.txt --model $model --base-url http://127.0.0.1:$MP --key-env K "$@" >/dev/null 2>&1
}
has() { grep -c "$1" $T/req.log | tr -d ' ' | sed 's/^0$/no/; s/^[1-9][0-9]*$/yes/'; }
runq any/model;           check "responses: no markers (automatic caching)" "$(has cache_control)" "no"
runq any/model;           check "responses: prompt_cache_key sent" "$(has prompt_cache_key)" "yes"
runq any/model --provider-only anthropic,google; check "--provider-only pins providers" "$(has allow_fallbacks)" "yes"
# --- without --context-window, the window and output cap come from the provider's model list (HX_TEST_MODELS: the
# mock counts as OpenRouter); a failed lookup keeps the defaults and says so
models() { # MOCK_MODELS json, then runq args -> request log and stderr in $T/models.err
  MOCK_REPLIES='["done"]' MOCK_MODELS="$1" MOCK_LOG=$T/req.log serve LP python3 scripts/mock_llm.py 0 || return 1; local m=$SERVED; shift
  W=$T/l-$RANDOM; mkdir -p $W/.harness; : > $T/req.log; echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
  HX_TEST_MODELS=1 K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --base-url http://127.0.0.1:$LP --key-env K "$@" >/dev/null 2>$T/models.err
  kill $m 2>/dev/null; wait $m 2>/dev/null
}
models '{"data":[{"id":"any/model","context_length":40000,"top_provider":{"max_completion_tokens":3000}}]}'
check "the provider's listing sets the output cap" "$(has '"max_output_tokens": 3000')" "yes"
check "...and the window, frozen in the manifest" "$(grep -h '"kind":"manifest"' "$(logdir $W)"/*.jsonl | head -1 | python3 -c "import json,sys,pathlib;r=json.loads(sys.stdin.read());print(json.loads(pathlib.Path('$(logdir $W)/blobs/'+r['blob']).read_text())['model']['context_window'])")" "40000"
models '{"data":[{"id":"any/model","context_length":40000}]}' --context-window 200000
check "--context-window wins over the listing" "$(has '"max_output_tokens": 50000')" "yes"
models '{"data":[]}'
check "an unlisted model keeps the defaults, with a note" "$(has '"max_output_tokens": 65536')$(grep -c 'context window of any/model is unknown' $T/models.err)" "yes1"
K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --base-url http://127.0.0.1:$MP --key-env K >/dev/null 2>$T/other.err
check "another provider: hx says once that the window is unknown" "$(grep -c 'is not OpenRouter.*context window of any/model is unknown' $T/other.err)" "1"
# --- streaming and transport (the loop's handling of each reply and failure: src/tests/)
mockrun() { # replies-json -> "finished|final_answer|turns|code_turns|truncated_turns"
  MOCK_REPLIES="$1" MOCK_BY_LEN=${BYLEN:-1} MOCK_LOG=$T/tool.log serve TP python3 scripts/mock_llm.py 0 || return 1; local m=$SERVED; : > $T/tool.log
  W=$T/m-$RANDOM; mkdir -p $W/.harness
  echo 'require("std/recheck").enabled = false' > $W/.harness/init.luau
  local o; o=$(K=x $H run --workspace $W --prompt-file $T/p.txt --model any/model --base-url http://127.0.0.1:$TP --key-env K 2>&1 | tail -1)
  kill $m 2>/dev/null; wait $m 2>/dev/null
  python3 -c "import sys,json;r=json.loads(sys.argv[1]);print('|'.join(str(r[k]) for k in ('finished','final_answer','turns','code_turns','truncated_turns')))" "$o" 2>/dev/null
}
# a stream that ends before the response completes is a lost response
check "a dropped stream is a lost response: one new request, run finishes" "$(BYLEN=0 mockrun '[{"text":"","calls":[{"code":"print(1)"}],"drop":true},"```luau\nprint(2)\n```","all done"]')" "True|all done|2|1|0"
# a loopback provider is reached directly even with a proxy configured
check "a loopback base URL bypasses HTTP_PROXY" "$(HTTP_PROXY=http://127.0.0.1:9 http_proxy=http://127.0.0.1:9 ALL_PROXY=http://127.0.0.1:9 NO_PROXY= no_proxy= mockrun '["```luau\nprint(7)\n```","all done"]')" "True|all done|2|1|0"
finish
