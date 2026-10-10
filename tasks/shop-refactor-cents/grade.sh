#!/usr/bin/env bash
# cwd = workspace, TASK_DIR = this task's directory. Hidden tests live outside the workspace.
set -u
export PYTHONDONTWRITEBYTECODE=1
fail=0
PYTHONPATH=. python3 -m unittest discover -s "$TASK_DIR/hidden" -p 'test_hidden.py' >/tmp/grade.$$ 2>&1 || { tail -15 /tmp/grade.$$; fail=1; }
rm -f /tmp/grade.$$
if grep -rnE '\bfloat\b|round\(|[0-9]\.[0-9]' shop/ --include=*.py; then echo "float arithmetic remains in shop/"; fail=1; fi
exit $fail
