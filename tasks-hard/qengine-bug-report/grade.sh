#!/usr/bin/env bash
# cwd = workspace, TASK_DIR = this task's directory. Hidden tests live outside the workspace.
set -u
export PYTHONDONTWRITEBYTECODE=1
fail=0
PYTHONPATH=. python3 -m unittest discover -s "$TASK_DIR/hidden" -p 'test_hidden.py' >/tmp/grade.$$ 2>&1 || { tail -15 /tmp/grade.$$; fail=1; }
python3 - "$TASK_DIR/hidden/test_hidden.py" /tmp/grade.$$ <<'PY'
import re, sys
total = len(re.findall(r"^    def test_", open(sys.argv[1]).read(), re.M))
out = open(sys.argv[2]).read()
m = re.search(r"^Ran (\d+) test", out, re.M)
bad = set(re.findall(r"^(?:FAIL|ERROR): (\w+)", out, re.M))
print(f"SCORE {total - len(bad) if m and int(m.group(1)) == total else 0}/{total}")
PY
rm -f /tmp/grade.$$

exit $fail
