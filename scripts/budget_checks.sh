#!/usr/bin/env bash
# Budget checks at the command line. No API key needed. Token and wall budgets in a run, descendants sharing them and
# effects cut off by the deadline are tested in-process: src/tests/budget.rs.
cd "$(dirname "$0")/.."
source scripts/check_helpers.sh
echo go > $T/p.txt
ws() { W=$T/$1; rm -rf $W; mkdir -p $W/.harness; }

ws b; printf '[budget]\nmax_cost = 1\n' > $W/.harness/policy.toml
check "policy.toml max_cost is refused, not ignored" "$(OPENROUTER_API_KEY=x $H run --workspace $W --prompt-file $T/p.txt --model m 2>&1 | grep -c 'unknown field `max_cost`')" "1"
check "--max-cost is an unknown option" "$($H run --workspace $W --prompt-file $T/p.txt --model m --max-cost 1 2>&1 | grep -c 'unknown option --max-cost')" "1"

# CPU/wait accounting has a fast Rust regression; arbitrary coroutine scheduling is unavailable.
ws e
check "model cannot create arbitrary coroutines" "$($H exec --workspace $W 'print(type(coroutine))' | head -1)" "nil"
check "parallel API is absent" "$($H exec --workspace $W 'print(parallel == nil)' 2>&1 | head -1)" "true"

finish
