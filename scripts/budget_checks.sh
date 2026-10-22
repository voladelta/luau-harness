#!/usr/bin/env bash
# Budget checks (scripted model: every call costs 10+5 tokens). No API key needed.
set -u
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
export HX_TRUST=1 XDG_CONFIG_HOME=$(mktemp -d)  # these suites write .harness config on purpose; trust_checks.sh tests trust
H=$PWD/target/release/hx
T=$(mktemp -d); T=$(cd "$T" && pwd -P); trap 'rm -rf "$T"; kill $M1 $M2 2>/dev/null' EXIT
pass=0; fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
CHUNKS='["```luau\nprint(1)\n```","```luau\nprint(2)\n```","```luau\nprint(3)\n```","```luau\nprint(4)\n```","```luau\nprint(5)\n```","done"]'
MOCK_REPLIES="$CHUNKS" MOCK_BY_LEN=1 serve P1 python3 scripts/mock_llm.py 0 || exit 1; M1=$SERVED
SUB='["```luau\nlocal r = agent(\"sub\")\nprint(r.answer)\n```","```luau\nprint(1)\n```","```luau\nprint(2)\n```","```luau\nprint(3)\n```","done","done"]'
MOCK_REPLIES="$SUB" serve P2 python3 scripts/mock_llm.py 0 || exit 1; M2=$SERVED
echo go > $T/p.txt
run() { # port, workspace, extra flags...
  local port=$1 ws=$2; shift 2
  OPENROUTER_API_KEY=x $H run --workspace $ws --prompt-file $T/p.txt --model m --base-url http://127.0.0.1:$port --key-env OPENROUTER_API_KEY "$@" 2>/dev/null | tail -1
}
field() { python3 -c "import sys,json;r=json.loads(sys.stdin.read());print(r.get('$1'))"; }
ws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; }

ws a; out=$(run $P1 $W --max-tokens 40)
check "token budget stops the run after 3 calls" "$(echo "$out" | field llm_calls)" "3"
check "stopped reason names the token limit" "$(echo "$out" | field stopped | grep -c 'token admission threshold')" "1"
check "run is reported unfinished (not a crash)" "$(echo "$out" | field finished)" "False"
check "budget stop is logged" "$(grep -h -c '"kind":"budget"' $W/.harness/log/*.jsonl)" "1"

ws b; printf '[budget]\nmax_cost = 1\n' > $W/.harness/policy.toml
check "policy.toml max_cost is refused, not ignored" "$(OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m 2>&1 | grep -c 'unknown field `max_cost`')" "1"
check "--max-cost is an unknown option" "$($H run --workspace $W --prompt-file $T/p.txt --model m --max-cost 1 2>&1 | grep -c 'unknown option --max-cost')" "1"

ws c; out=$(run $P1 $W --max-wall 0)
check "wall budget of 0 blocks the first call" "$(echo "$out" | field llm_calls)$(echo "$out" | field stopped | grep -c 'deadline')" "01"

ws d; printf '[budget]\nmax_tokens = 20\n' > $W/.harness/policy.toml; out=$(run $P1 $W)
check "policy.toml [budget] applies" "$(echo "$out" | field llm_calls)" "2"
ws e; printf '[budget]\nmax_tokens = 20\n' > $W/.harness/policy.toml; out=$(run $P1 $W --max-tokens 1000)
check "a launch flag beats policy.toml" "$(echo "$out" | field finished)" "True"

ws f; out=$(run $P2 $W --max-tokens 40)
check "subagent shares the parent's budget (3 calls total, then both stop)" "$(echo "$out" | field llm_calls)$(echo "$out" | field stopped | grep -c budget_exceeded)" "31"

ws g; out=$(run $P1 $W)
check "no budget: run completes normally" "$(echo "$out" | field finished)$(echo "$out" | field stopped)" "TrueNone"

# CPU/wait accounting has a fast Rust regression; arbitrary coroutine scheduling is unavailable.
check "model cannot create arbitrary coroutines" "$($H exec --workspace $W 'print(type(coroutine))' | head -1)" "nil"
check "parallel API is absent" "$($H exec --workspace $W 'print(parallel == nil)' 2>&1 | head -1)" "true"

echo "$pass passed, $fail failed"; [ $fail -eq 0 ]
