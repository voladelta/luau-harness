#!/bin/bash
# usage: run_task.sh <task> <arm: base|nock|cc|<arms/x>> <model> <effort> [n=1]
# base = hx defaults, which include std/checkpoint since 2026-10-14; nock = without it (the old base); checkpoint = base
# Runs hx (arm cc: Claude Code, `claude -p`, as the comparison harness) on the macOS host in a copy of the task's environment, then grades with the task's own tests/test.sh inside
# its verifier image (tb4-<short>-verifier) with the workspace mounted at /app. Prints one "==" line and one reward line.
# A run that died (plan or rate limit, API error, hx error) prints "== VOID ..." and is not graded: it is not a failure.
# Layout: $TB4_HOME/repo (terminal-bench clone), $TB4_HOME/venvs/<short> (optional host Python), $TB4_HOME/runs,
# $TB4_HOME/.env (optional: OPENROUTER_API_KEY, RIVER_API_KEY, ENCLAVE_API_KEY). Copy this script before a batch: bash reads it as it runs.
set -u
TB4_HOME=${TB4_HOME:-$HOME/.cache/hx-tb4}
HX_REPO=$(cd "$(dirname "$0")/../.." && pwd)
TASK=$1; A=$2; M=$3; E=$4; N=${5:-1}; T=$TB4_HOME/repo/tasks/$TASK; SHORT=${TASK%%-*}; R=$TB4_HOME/runs/$SHORT-$A-$M-$E-$N
[ -d "$T" ] || { echo "no task $T"; exit 1; }
SRC=$T/environment/app; [ -d $SRC ] || SRC=$T/environment
[ -e $R ] && [ -z "${TB4_OVERWRITE:-}" ] && { echo "run $R exists: pick another n (TB4_OVERWRITE=1 replaces it)"; exit 3; }
rm -rf $R; mkdir -p $R/ws; (cd $SRC && tar cf - --exclude Dockerfile .) | (cd $R/ws && tar xf -); mkdir -p $R/ws/.harness
# the task text with /app rewritten to the workspace; the canary line goes (contamination checks look for it)
sed '/harbor-canary/d; s#-C /app#-C .#g; s#`/app/`#the current directory#g; s#`/app/#`#g; s#`/app`#the current directory#g; s# /app/# #g' $T/instruction.md > $R/prompt.md
printf '\nThe task directory /app is the current directory; use relative paths.\n' >> $R/prompt.md
if [ -d $TB4_HOME/venvs/$SHORT ]; then cp -R $TB4_HOME/venvs/$SHORT $R/ws/.venv; printf 'A Python with the task dependencies is at .venv/bin/python (use it for running code and tests).\n' >> $R/prompt.md; fi
case $A in
  base) PACK= ;;
  checkpoint) PACK="--pack std/checkpoint" ;;
  cc) PACK=; rmdir $R/ws/.harness ;;  # Claude Code on the Claude plan, project settings only (none), no hx
  # any other arm: eval/tb4/arms/<arm>.md replaces hx's system prompt (std/system.md) for the run, and/or
  # eval/tb4/arms/<arm>.luau becomes the workspace's .harness/init.luau (hooks); <md arm>ck = that prompt + std/checkpoint;
  # eval/tb4/arms/<arm>.prompt is appended to the task prompt.
  # Generic wording only, no task hints.
  *) PACK=; AD="$HX_REPO/eval/tb4/arms"; found=; B=$A
     # <arm>ck with no files of its own: arm <arm> plus std/checkpoint
     if [ "${A%ck}" != "$A" ] && ! ls "$AD/$A".* >/dev/null 2>&1 && ls "$AD/${A%ck}".* >/dev/null 2>&1; then
       B=${A%ck}; PACK="--pack std/checkpoint"
     fi
     if [ -f "$AD/$B.md" ]; then
       mkdir -p $R/ws/.harness/override/std && cp "$AD/$B.md" $R/ws/.harness/override/std/system.md; found=1
     fi
     if [ -f "$AD/$B.pre" ]; then  # <arm>.pre: prepended above the task prompt, first thing read
       { cat "$AD/$B.pre"; printf '\n'; cat $R/prompt.md; } > $R/prompt.pre && mv $R/prompt.pre $R/prompt.md; found=1
     fi
     if [ -f "$AD/$B.prompt" ]; then  # <arm>.prompt: appended to the task prompt, last thing read before starting
       { printf '\n'; cat "$AD/$B.prompt"; } >> $R/prompt.md; found=1
     fi
     if [ -f "$AD/$B.luau" ]; then
       cp "$AD/$B.luau" $R/ws/.harness/init.luau
       mkdir -p $R/ws/.harness/override/arms && cp "$AD"/*.luau $R/ws/.harness/override/arms/  # require("arms/x")
       found=1
     fi
     [ -n "$found" ] || { echo "unknown arm $A"; exit 1; } ;;
esac
CAP=
case $M in
  ds41f)   MID=deepseek/deepseek-v4.1-flash;    PROV=openrouter ;;
  glm53f)  MID=z-ai/glm-5.3-flash;              PROV=openrouter ;;
  rds41f)  MID=deepseek-ai/DeepSeek-V4.1-Flash; PROV=river ;;
  rglm53f) MID=zai-org/GLM-5.3-Flash;           PROV=river ;;
  orhaiku) MID=anthropic/claude-haiku-5.5;      PROV=openrouter; CAP=2 ;;  # Haiku 5.5 via OpenRouter, not the plan
  eds41f)  MID=cyberouter/deepseek-v4.1-flash;  PROV=enclave ;;  # DeepSeek V4.1 Flash via Enclave (check against River)
  eglm53f) MID=cyberouter/glm-5.3-flash;        PROV=enclave ;;  # GLM 5.3 Flash via Enclave (check against River)
  *)       MID=$M;                              PROV=claude ;;  # haiku | sonnet | opus on the Claude plan
esac
CAP=${CAP:-0.25}; RE=; [ "$E" != none ] && RE="--reasoning $E"
case $PROV in
  openrouter) API="--api openai --base-url https://openrouter.ai/api/v1 --key-env OPENROUTER_API_KEY --max-cost $CAP $RE" ;;
  river)      API="--api openai --base-url https://api.river.ai/v1 --key-env RIVER_API_KEY --max-cost $CAP $RE" ;;
  enclave)    API="--api openai --base-url https://router.enclave.ai/v1 --key-env ENCLAVE_API_KEY --max-cost $CAP $RE" ;;
  claude)     API="--api claude-sdk --reasoning $E --max-output 64000" ;;  # streamed: 128K allowed; thinking counts toward it
esac
[ -f $TB4_HOME/.env ] && . $TB4_HOME/.env
t0=$(date +%s)
if [ $A = cc ]; then
  (cd $R/ws && env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL claude -p "$(cat $R/prompt.md)" --setting-sources project --disable-slash-commands --strict-mcp-config --model $M --effort $E --output-format json --dangerously-skip-permissions --no-session-persistence > $R/result.json 2> $R/stderr.log)
else
(cd $HX_REPO && HX_SDK_TRACE=$R/sdk.log HX_TRUST=1 ./target/release/hx run --workspace $R/ws --prompt-file $R/prompt.md --model $MID $API $PACK --trace $R/trace.txt --max-wall 3600 --max-tokens 8000000 > $R/result.json 2> $R/stderr.log)
fi
rc=$?; wall=$(( $(date +%s) - t0 ))
reason=
if grep -q 'plan usage limit reached' $R/stderr.log; then reason=rate-limit
elif grep -qiE 'API (unavailable after retries|429)|rate.?limit' $R/stderr.log; then reason=rate-limit
elif grep -qiE 'API 402|insufficient.?credits|insufficient.?quota|quota' $R/stderr.log; then reason=credits
elif grep -qiE 'decoding response body|API [0-9]{3}|API request failed' $R/stderr.log; then reason=api-error
elif [ -f $R/trace.txt ] && grep -q '^project init failed' $R/trace.txt; then reason=setup-init  # a broken arm hook: not the model's run
elif grep -q 'safeguards flagged' $R/stderr.log; then reason=refusal  # model safety classifier (e.g. a [bio] false positive): not a failure
elif grep -q 'runtime error:' $R/stderr.log; then reason=hx-error
elif [ ! -s $R/result.json ]; then reason=empty-result
elif [ $A = cc ] && python3 -I -c "import json,sys;sys.exit(0 if json.load(open('$R/result.json')).get('is_error') else 1)" 2>/dev/null; then
  reason=cc-error; grep -qiE 'limit|429' $R/result.json && reason=rate-limit
fi
if [ -n "$reason" ]; then echo "== VOID $TASK $A $M $E #$N reason=$reason exit=$rc wall=${wall}s"; exit 0; fi
echo "== $TASK $A $M $E #$N exit=$rc wall=${wall}s $(python3 -I -c "import json;r=json.load(open('$R/result.json'));print('calls',r.get('llm_calls',r.get('num_turns')),'out',r.get('output_tokens',r.get('usage',{}).get('output_tokens')),'cost',r.get('cost_usd',r.get('total_cost_usd')))" 2>&1 | tail -1)"
# grade a copy of the workspace without hx's own files; restore node_modules the /app mount would shadow
rm -rf $R/grade; mkdir -p $R/grade/app $R/grade/logs; (cd $R/ws && find . -mindepth 1 -maxdepth 1 -not -name build -print | tar cf - --exclude .harness --exclude .claude --exclude .venv --exclude __pycache__ --files-from - ) | (cd $R/grade/app && tar xf -)
if [ ! -d $R/grade/app/node_modules ] && container run --rm tb4-$SHORT-verifier:latest sh -c 'test -d /app/node_modules' >/dev/null 2>&1; then container run --rm -v $R/grade/app:/out tb4-$SHORT-verifier:latest cp -a /app/node_modules /out/ >/dev/null 2>&1; fi
RES=; [ -d $R/grade/app/results ] && RES="-v $R/grade/app/results:/results"  # tasks that write /results (roy, biped, gsea): the sandbox wrote ./results
container run --rm -v $R/grade/app:/app $RES -v $R/grade/logs:/logs tb4-$SHORT-verifier:latest bash /tests/test.sh > $R/grade.log 2>&1
echo "reward=$(cat $R/grade/logs/verifier/reward.txt 2>/dev/null) | $(grep -E 'passed|failed|gate failed|error' $R/grade.log | tail -3 | tr '\n' ' ' | cut -c1-200)"
