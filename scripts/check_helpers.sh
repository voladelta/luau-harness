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
