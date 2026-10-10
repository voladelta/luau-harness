#!/usr/bin/env bash
# Run every check suite (none needs an API key). Build first: cargo build --release
cd "$(dirname "$0")/.."
H=target/release/hx; rc=0
echo "std unit tests:  $($H test stdlib/std | tail -1)"; $H test stdlib/std >/dev/null || rc=1
for s in p2_checks p1_checks sandbox_checks budget_checks caps_checks p4_checks; do
  out=$(scripts/$s.sh 2>&1); [ $? -eq 0 ] || { rc=1; echo "$out" | grep FAIL; }
  printf '%-16s %s\n' "$s:" "$(echo "$out" | grep -E 'passed' | tail -1)"
done
for s in mcp_checks upgrade_checks; do out=$(scripts/$s.sh 2>&1); [ $? -eq 0 ] || { rc=1; echo "$out" | grep FAIL; }; printf '%-16s %s\n' "$s:" "$(echo "$out" | grep -E 'passed' | tail -1)"; done
out=$(scripts/mcp_resume_check.py 2>&1); [ $? -eq 0 ] || { rc=1; echo "$out" | grep FAIL; }
printf '%-16s %s\n' "mcp_resume:" "$(echo "$out" | tail -1)"
out=$(scripts/resume_checks.py 2>&1); [ $? -eq 0 ] || { rc=1; echo "$out" | grep FAIL; }
printf '%-16s %s\n' "resume_checks:" "$(echo "$out" | tail -1)"
exit $rc
