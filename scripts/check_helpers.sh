# Shared setup for the bash check suites: `cd "$(dirname "$0")/.." && source scripts/check_helpers.sh` at the top.
# It gives each suite a fresh trust store and scratch directory $T (removed on exit, after the suite's own `cleanup`),
# $H (hx; scripts/hx_trusted trusts the config a suite writes, per launch, under HX_TRUST=current), $R (the repository
# root), and check/finish for the pass/fail tally. Build first: scripts/all_checks.py builds once for every suite.
set -u
R=$PWD
T=$(mktemp -d); T=$(cd "$T" && pwd -P)
export HX_TRUST=current XDG_CONFIG_HOME=$T/config  # suites write .harness config on purpose; trust_checks.sh tests trust
H=$R/scripts/hx_trusted
pass=0; fail=0
cleanup() { :; }  # a suite redefines it to stop its mock servers and processes
trap 'cleanup; rm -rf "$T"' EXIT

# check NAME GOT WANT
check() { if [ "$2" = "$3" ]; then echo "ok   $1"; pass=$((pass+1)); else echo "FAIL $1: got [$2] want [$3]"; fail=$((fail+1)); fi; }
# The summary line all_checks.py reads, and the suite's exit status.
finish() { echo "$pass passed, $fail failed"; [ $fail -eq 0 ]; }

# Readiness deadlines for integration checks, replacing fixed startup sleeps.
# serve VAR CMD...: run a mock server in the background on a free port (CMD passes port 0; the server writes the
# bound port to $PORT_FILE once listening). Sets VAR to the port and SERVED to the server's pid.
serve() {
  local var=$1 file; shift; file=$(mktemp -u)
  PORT_FILE=$file "$@" & SERVED=$!
  wait_for_file "$file" || return 1
  printf -v "$var" %s "$(cat "$file")"; rm -f "$file"
}

wait_for_file() {
  python3 - "$1" <<'PY'
from pathlib import Path
import sys, time
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    if Path(sys.argv[1]).exists():
        sys.exit(0)
    time.sleep(0.01)
sys.exit("test synchronization file did not appear within 5s: " + sys.argv[1])
PY
}

# unconfined DIR: did hx runs in workspace DIR record that Landlock is unavailable (children run unconfined)?
unconfined() {
  grep -qh '"confinement":"off' "$1"/.harness/log/*.jsonl 2>/dev/null
}
