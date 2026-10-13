#!/bin/bash
# usage: posctl.sh <task>
# POSITIVE CONTROL for a verifier. Builds the workspace exactly as run_task.sh does, applies the
# task's own reference solution (solution/solve.sh, which expects /app + /solution), then grades
# through the identical path run_task.sh uses. A trustworthy verifier must return reward=1.
#
# "untouched workspace grades 0" is NOT sufficient to trust a verifier: a totally broken verifier
# also grades 0. This is the missing check.
#
# Outcomes:
#   PASS            reward=1 -> verifier recognises a correct solution; screens on it are meaningful
#   FAIL-GRADE      solve.sh succeeded but reward!=1 -> verifier is broken; any screen result is void
#   FAIL-SOLVE      solve.sh itself failed in the verifier image -> inconclusive, read solve.log
set -u
TB4_HOME=${TB4_HOME:-$HOME/.cache/hx-tb4}
TASK=$1; T=$TB4_HOME/repo/tasks/$TASK; SHORT=${TASK%%-*}; IMG=tb4-$SHORT-verifier:latest
R=$TB4_HOME/posctl/$SHORT
SRC=$T/environment/app; [ -d $SRC ] || SRC=$T/environment

if [ ! -d $T/solution ]; then echo "== POSCTL $TASK SKIP reason=no-solution-dir"; exit 0; fi

# 1. workspace, identical to run_task.sh
rm -rf $R; mkdir -p $R/ws
(cd $SRC && tar cf - --exclude Dockerfile .) | (cd $R/ws && tar xf -)

# restore image-provided /app/node_modules that the /app bind mount would otherwise shadow
if [ ! -d $R/ws/node_modules ] && container run --rm "$IMG" sh -c 'test -d /app/node_modules' >/dev/null 2>&1; then
  container run --rm -v $R/ws:/out "$IMG" cp -a /app/node_modules /out/ >/dev/null 2>&1
fi

# 2. apply the reference solution inside the verifier image (solve.sh assumes /app and /solution)
t0=$(date +%s)
container run --rm -v $R/ws:/app -v $T/solution:/solution:ro "$IMG" \
  bash /solution/solve.sh > $R/solve.log 2>&1
src=$?
solve_wall=$(( $(date +%s) - t0 ))

# 3. grade through run_task.sh's exact path, same excludes
rm -rf $R/grade; mkdir -p $R/grade/app $R/grade/logs
(cd $R/ws && find . -mindepth 1 -maxdepth 1 -not -name build -print | tar cf - \
  --exclude .harness --exclude .venv --exclude __pycache__ --files-from - ) | (cd $R/grade/app && tar xf -)
container run --rm -v $R/grade/app:/app -v $R/grade/logs:/logs "$IMG" \
  bash /tests/test.sh > $R/grade.log 2>&1
grc=$?
reward=$(cat $R/grade/logs/verifier/reward.txt 2>/dev/null)

verdict=FAIL-GRADE
[ "$reward" = "1" ] || [ "$reward" = "1.0" ] && verdict=PASS
if [ "$verdict" != "PASS" ] && [ $src -ne 0 ]; then verdict=FAIL-SOLVE; fi

echo "== POSCTL $TASK $verdict reward=${reward:-none} solve_exit=$src grade_exit=$grc solve_wall=${solve_wall}s | $(grep -E 'passed|failed|error|not found' $R/grade.log | tail -2 | tr '\n' ' ' | cut -c1-160)"
