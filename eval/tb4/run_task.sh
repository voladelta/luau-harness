#!/bin/bash
# usage: run_task.sh <task> <arm: base|checkpoint> <model> <effort> [n=1]
# Runs hx on the macOS host in a copy of the task's environment, then grades with the task's own tests/test.sh inside
# its verifier image (tb4-<short>-verifier) with the workspace mounted at /app. Prints one "==" line and one reward line.
# A run that died (plan or rate limit, API error, hx error) prints "== VOID ..." and is not graded: it is not a failure.
# Layout: $TB4_HOME/repo (terminal-bench clone), $TB4_HOME/venvs/<short> (optional host Python), $TB4_HOME/runs,
# $TB4_HOME/.env (optional: OPENROUTER_API_KEY, RIVER_API_KEY). Copy this script before a batch: bash reads it as it runs.
set -u
TB4_HOME=${TB4_HOME:-$HOME/.cache/hx-tb4}
HX_REPO=$(cd "$(dirname "$0")/../.." && pwd)
TASK=$1; A=$2; M=$3; E=$4; N=${5:-1}; T=$TB4_HOME/repo/tasks/$TASK; SHORT=${TASK%%-*}; R=$TB4_HOME/runs/$SHORT-$A-$M-$E-$N
[ -d "$T" ] || { echo "no task $T"; exit 1; }
SRC=$T/environment/app; [ -d $SRC ] || SRC=$T/environment
rm -rf $R; mkdir -p $R/ws; (cd $SRC && tar cf - --exclude Dockerfile .) | (cd $R/ws && tar xf -); mkdir -p $R/ws/.harness
# the task text with /app rewritten to the workspace; the canary line goes (contamination checks look for it)
sed '/harbor-canary/d; s#-C /app#-C .#g; s#`/app/`#the current directory#g; s#`/app/#`#g; s#`/app`#the current directory#g; s# /app/# #g' $T/instruction.md > $R/prompt.md
printf '\nThe task directory /app is the current directory; use relative paths.\n' >> $R/prompt.md
if [ -d $TB4_HOME/venvs/$SHORT ]; then cp -R $TB4_HOME/venvs/$SHORT $R/ws/.venv; printf 'A Python with the task dependencies is at .venv/bin/python (use it for running code and tests).\n' >> $R/prompt.md; fi
case $A in
  base) PACK= ;;
  checkpoint) PACK="--pack std/checkpoint" ;;
  *) echo "unknown arm $A"; exit 1 ;;
esac
case $M in
  ds41f)   MID=deepseek/deepseek-v4.1-flash;    PROV=openrouter ;;
  glm53f)  MID=z-ai/glm-5.3-flash;              PROV=openrouter ;;
  rds41f)  MID=deepseek-ai/DeepSeek-V4.1-Flash; PROV=river ;;
  rglm53f) MID=zai-org/GLM-5.3-Flash;           PROV=river ;;
  *)       MID=$M;                              PROV=claude ;;  # haiku | sonnet | opus on the Claude plan
esac
case $PROV in
  openrouter) API="--api openai --base-url https://openrouter.ai/api/v1 --key-env OPENROUTER_API_KEY --max-cost 0.25" ;;
  river)      API="--api openai --base-url https://api.river.ai/v1 --key-env RIVER_API_KEY --max-cost 0.25" ;;
  claude)     API="--api claude-sdk --reasoning $E" ;;
esac
[ -f $TB4_HOME/.env ] && . $TB4_HOME/.env
t0=$(date +%s)
(cd $HX_REPO && HX_SDK_TRACE=$R/sdk.log HX_TRUST=1 ./target/release/hx run --workspace $R/ws --prompt-file $R/prompt.md --model $MID $API $PACK --trace $R/trace.txt --max-wall 3600 --max-tokens 8000000 > $R/result.json 2> $R/stderr.log)
rc=$?; wall=$(( $(date +%s) - t0 ))
reason=
if grep -q 'plan usage limit reached' $R/stderr.log; then reason=rate-limit
elif grep -qiE 'API (unavailable after retries|429)|rate.?limit' $R/stderr.log; then reason=rate-limit
elif grep -qiE 'API 402|insufficient.?credits|insufficient.?quota|quota' $R/stderr.log; then reason=credits
elif grep -qiE 'decoding response body|API [0-9]{3}|API request failed' $R/stderr.log; then reason=api-error
elif grep -q 'runtime error:' $R/stderr.log; then reason=hx-error
elif [ ! -s $R/result.json ]; then reason=empty-result
fi
if [ -n "$reason" ]; then echo "== VOID $TASK $A $M $E #$N reason=$reason exit=$rc wall=${wall}s"; exit 0; fi
echo "== $TASK $A $M $E #$N exit=$rc wall=${wall}s $(python3 -I -c "import json;r=json.load(open('$R/result.json'));print('calls',r.get('llm_calls'),'out',r.get('output_tokens'),'cost',r.get('cost_usd'))" 2>&1 | tail -1)"
# grade a copy of the workspace without hx's own files; restore node_modules the /app mount would shadow
rm -rf $R/grade; mkdir -p $R/grade/app $R/grade/logs; (cd $R/ws && find . -mindepth 1 -maxdepth 1 -not -name build -print | tar cf - --exclude .harness --exclude .venv --exclude __pycache__ --files-from - ) | (cd $R/grade/app && tar xf -)
if [ ! -d $R/grade/app/node_modules ] && container run --rm tb4-$SHORT-verifier:latest sh -c 'test -d /app/node_modules' >/dev/null 2>&1; then container run --rm -v $R/grade/app:/out tb4-$SHORT-verifier:latest cp -a /app/node_modules /out/ >/dev/null 2>&1; fi
container run --rm -v $R/grade/app:/app -v $R/grade/logs:/logs tb4-$SHORT-verifier:latest bash /tests/test.sh > $R/grade.log 2>&1
echo "reward=$(cat $R/grade/logs/verifier/reward.txt 2>/dev/null) | $(grep -E 'passed|failed|gate failed|error' $R/grade.log | tail -3 | tr '\n' ' ' | cut -c1-200)"
