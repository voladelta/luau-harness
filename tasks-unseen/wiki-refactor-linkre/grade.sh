#!/usr/bin/env bash
# cwd = workspace, TASK_DIR = this task's directory. Hidden tests live outside the workspace.
set -u
export PYTHONDONTWRITEBYTECODE=1
fail=0
PYTHONPATH=. python3 -m unittest discover -s "$TASK_DIR/hidden" -p 'test_hidden.py' >/tmp/grade.$$ 2>&1 || { tail -15 /tmp/grade.$$; fail=1; }
rm -f /tmp/grade.$$
n=$(grep -rnF '\]\(' wiki/ --include=*.py | wc -l); if [ "$n" -ne 1 ]; then echo "link pattern defined $n times"; fail=1; fi
if grep -rn '<a href=' wiki/ --include=*.py | grep -v '^wiki/links.py'; then echo "anchor markup outside links.py"; fail=1; fi
exit $fail
