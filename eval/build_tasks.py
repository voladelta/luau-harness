#!/usr/bin/env python3
"""Assemble tasks/<id>/ from eval/domains + eval/taskspec, then (--verify) prove each task is gradeable:
the grader must FAIL on the starting repo and PASS after the reference solution.

taskspec/<id>/{prompt.md, hidden_test.py, task.py}; task.py defines DOMAIN and either MUTATIONS
[(path, good, bad)] (bug tasks: the repo gets `bad`, the solution restores `good`) or solve(h), plus optional CHECK
(extra shell lines for grade.sh, run with cwd = workspace).
Output layout (what run.py expects): tasks/<id>/{prompt.md, repo/, grade.sh, hidden/test_hidden.py}.
"""
import argparse, hashlib, os, shutil, subprocess, sys, tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
_ap = argparse.ArgumentParser()
_ap.add_argument("--verify", action="store_true")
_ap.add_argument("--spec", default="taskspec")
_ap.add_argument("--out", default="tasks")
_args = _ap.parse_args()
SPEC = HERE / _args.spec
OUT = HERE.parent / _args.out
GRADE = """#!/usr/bin/env bash
# cwd = workspace, TASK_DIR = this task's directory. Hidden tests live outside the workspace.
set -u
export PYTHONDONTWRITEBYTECODE=1
fail=0
PYTHONPATH=. python3 -m unittest discover -s "$TASK_DIR/hidden" -p 'test_hidden.py' >/tmp/grade.$$ 2>&1 || { tail -15 /tmp/grade.$$; fail=1; }
rm -f /tmp/grade.$$
%s
exit $fail
"""

class H:
    def __init__(self, ws): self.ws = Path(ws)
    def edit(self, path, old, new, count=1):
        p = self.ws / path; s = p.read_text()
        assert s.count(old) == count, f"{path}: expected {count} of {old!r}, found {s.count(old)}"
        p.write_text(s.replace(old, new))
    def write(self, path, text):
        p = self.ws / path; p.parent.mkdir(parents=True, exist_ok=True); p.write_text(text)

def load(spec):
    ns = {}
    exec((spec / "task.py").read_text(), ns)
    return ns

def make_repo(ns, dest):
    shutil.copytree(HERE / "domains" / ns["DOMAIN"], dest, ignore=shutil.ignore_patterns("__pycache__"))
    for path, good, bad in ns.get("MUTATIONS", []):
        H(dest).edit(path, good, bad)

def build():
    if OUT.exists(): shutil.rmtree(OUT)
    for spec in sorted(SPEC.iterdir()):
        ns = load(spec); d = OUT / spec.name
        make_repo(ns, d / "repo")
        shutil.copy(spec / "prompt.md", d / "prompt.md")
        (d / "hidden").mkdir(); shutil.copy(spec / "hidden_test.py", d / "hidden" / "test_hidden.py")
        (d / "grade.sh").write_text(GRADE % ns.get("CHECK", ""))
    lines = []
    for f in sorted(p for p in OUT.rglob("*") if p.is_file() and "__pycache__" not in p.parts):
        lines.append(f"{hashlib.sha256(f.read_bytes()).hexdigest()}  {f.relative_to(OUT)}")
    (OUT / "MANIFEST.sha256").write_text("\n".join(lines) + "\n")
    print(f"built {len(list(OUT.glob('*/prompt.md')))} tasks; manifest {len(lines)} files")

def grade(task, ws):
    return subprocess.run(["bash", str(task / "grade.sh")], cwd=ws, capture_output=True, text=True,
                          env={**os.environ, "TASK_DIR": str(task)})

def verify():
    bad = 0
    for spec in sorted(SPEC.iterdir()):
        ns = load(spec); task = OUT / spec.name
        with tempfile.TemporaryDirectory() as t:
            a = Path(t) / "start"; shutil.copytree(task / "repo", a)
            r0 = grade(task, a)
            b = Path(t) / "solved"; shutil.copytree(task / "repo", b)
            if ns.get("MUTATIONS"):
                for path, good, badtxt in ns["MUTATIONS"]: H(b).edit(path, badtxt, good)
            else:
                ns["solve"](H(b))
            r1 = grade(task, b)
        ok = r0.returncode != 0 and r1.returncode == 0
        bad += not ok
        print(f"{'ok  ' if ok else 'FAIL'} {spec.name}: start={'fail' if r0.returncode else 'PASS(!)'} solved={'pass' if r1.returncode == 0 else 'FAIL(!)'}")
        if r1.returncode != 0: print("   ", (r1.stdout + r1.stderr)[-600:].replace("\n", "\n    "))
    sys.exit(1 if bad else 0)

if __name__ == "__main__":
    build()
    if _args.verify: verify()
