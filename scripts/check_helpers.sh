# Readiness deadlines for integration checks, replacing fixed startup sleeps.
wait_for_port() {
  python3 - "$1" <<'PY'
import socket, sys, time
deadline = time.monotonic() + 5
while time.monotonic() < deadline:
    try:
        with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=0.1):
            sys.exit(0)
    except OSError:
        time.sleep(0.01)
sys.exit("mock server did not become ready within 5s")
PY
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
