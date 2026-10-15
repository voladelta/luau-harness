#!/usr/bin/env bash
# Run every check suite (none needs an API key). Builds the release binary first, so the suites never test a stale one.
cd "$(dirname "$0")/.."
cargo build --release --quiet || exit 1
H=target/release/hx; rc=0
run_check() {
  local name=$1 out summary; shift
  out=$("$@" 2>&1)
  if [ $? -ne 0 ]; then rc=1; echo "$out"; fi
  summary=$(echo "$out" | grep -E '[0-9]+ passed' | tail -1)
  printf '%-16s %s\n' "$name:" "${summary:-$(echo "$out" | tail -1)}"
}
run_check "std unit tests" "$H" test stdlib/std
for s in harness_checks journal_checks sandbox_checks budget_checks caps_checks extension_checks; do
  run_check "$s" scripts/$s.sh
done
for s in mcp_checks upgrade_checks llm_checks vision_checks compact_checks trust_checks; do run_check "$s" scripts/$s.sh; done
run_check redesign python3 scripts/redesign_checks.py
exit $rc
