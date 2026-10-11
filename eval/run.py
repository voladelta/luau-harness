#!/usr/bin/env python3
"""P0 eval runner: frozen tasks x {hx, claude} x N runs -> results.jsonl. Script graders only.

ASSUMED task layout (tasks/ was not present when this was written; adjust TASK_* if the frozen format differs):
  tasks/<id>/prompt.md   the task statement given verbatim to both agents
  tasks/<id>/repo/       optional starting snapshot, copied into a fresh workspace per run
  tasks/<id>/grade.sh    run with cwd = workspace; exit 0 = pass
Tasks are never modified; each run works in a throwaway copy.
"""
import argparse, json, os, shutil, subprocess, sys, tempfile, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TASK_PROMPT, TASK_REPO, TASK_GRADE = "prompt.md", "repo", "grade.sh"
HX = ROOT / "target" / "release" / "hx"

def hx_endpoint(model):
    if os.environ.get("EVAL_HX_BASE_URL"):  # tests: a scripted model server
        return ["--api", "openai", "--base-url", os.environ["EVAL_HX_BASE_URL"], "--key-env", "EVAL_HX_KEY"]
    """Enclave models (cyberouter/*) use Enclave; anything else goes through OpenRouter."""
    extra = ["--reasoning", os.environ["EVAL_REASONING"]] if os.environ.get("EVAL_REASONING") else []
    if model.startswith("claude-sdk/"):  # the claude CLI on the user's plan (no key); hx:claude-sdk/haiku
        return ["--api", "claude-sdk"] + extra
    if model.startswith("cyberouter/"):
        return ["--api", "openai", "--base-url", "https://router.enclave.ai/v1", "--key-env", "ENCLAVE_API_KEY"] + extra
    return ["--api", "openai", "--base-url", "https://openrouter.ai/api/v1", "--key-env", "OPENROUTER_API_KEY"] + extra

def run_hx(ws, prompt_file, model, timeout, trace):
    t0 = time.time()
    # runaway guard: a single task should never need more than this (typical run: $0.005, 15k tokens, 15s)
    limits = ["--max-cost", os.environ.get("EVAL_MAX_COST", "0.30"), "--max-tokens", os.environ.get("EVAL_MAX_TOKENS", "600000"), "--max-wall", os.environ.get("EVAL_MAX_WALL", "900")]
    p = subprocess.run([str(HX), "run", "--workspace", ws, "--prompt-file", str(prompt_file),
                        "--model", model.removeprefix("claude-sdk/"), "--trace", str(trace)] + limits + (["--pack", os.environ["EVAL_PACKS"]] if os.environ.get("EVAL_PACKS") else []) + (["--max-output", os.environ["EVAL_MAX_OUTPUT"]] if os.environ.get("EVAL_MAX_OUTPUT") else []) + hx_endpoint(model),
                       capture_output=True, text=True, timeout=timeout,
                       env={**os.environ, "HX_TRUST": "1"})  # throwaway copy we built (gate overrides), so its config is ours
    if p.returncode != 0:
        return {"harness_error": p.stderr[-500:]}
    r = json.loads(p.stdout.strip().splitlines()[-1])
    r["tokens_total"] = r["input_tokens"] + r["cache_creation_tokens"] + r["cache_read_tokens"] + r["output_tokens"]
    r["tokens_fresh"] = r["input_tokens"] + r["cache_creation_tokens"] + r["output_tokens"]
    r["wall_secs"] = time.time() - t0
    return r

BASELINE_AUTH = os.environ.get("BASELINE_AUTH", "subscription")  # or "subscription"

def model_for_claude(model):
    return model

def run_claude(ws, prompt, model, timeout, trace):
    t0 = time.time()
    # --bare: no user CLAUDE.md / hooks / plugins, so the baseline is the stock agent.
    env = dict(os.environ)
    flags = ["--bare"]
    if BASELINE_AUTH == "openrouter":  # lean stock agent, paid; same provider and model as hx
        env.update(ANTHROPIC_BASE_URL="https://openrouter.ai/api", ANTHROPIC_AUTH_TOKEN=os.environ["OPENROUTER_API_KEY"], ANTHROPIC_API_KEY="")
    else:  # subscription / subscription-lean login: --bare skips OAuth, so trim context with flags (still ~20k tokens heavier)
        for k in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"): env.pop(k, None)
        flags = ["--setting-sources", "project", "--disable-slash-commands", "--strict-mcp-config"]
        if BASELINE_AUTH == "subscription-lean":  # ~3.6k prompt instead of ~23.7k; minimal system prompt, 4 core tools
            flags += ["--system-prompt", "You are a coding agent. Complete the task by editing files in the working directory.", "--tools", "Bash,Read,Edit,Write"]
    p = subprocess.run(["claude", "-p", prompt, *flags, "--model", model_for_claude(model), "--output-format", "json",
                        *(["--effort", os.environ["EVAL_EFFORT"]] if os.environ.get("EVAL_EFFORT") else []),
                        "--dangerously-skip-permissions", "--no-session-persistence"],
                       cwd=ws, capture_output=True, text=True, timeout=timeout, env=env)
    Path(trace).write_text(p.stdout + "\n" + p.stderr)
    try:
        j = json.loads(p.stdout)
    except Exception:
        return {"harness_error": (p.stdout + p.stderr)[-500:]}
    u = j.get("usage", {})
    r = {"model": model, "turns": j.get("num_turns"), "cost_usd": j.get("total_cost_usd"),
         "input_tokens": u.get("input_tokens", 0), "output_tokens": u.get("output_tokens", 0),
         "cache_creation_tokens": u.get("cache_creation_input_tokens", 0),
         "cache_read_tokens": u.get("cache_read_input_tokens", 0), "is_error": j.get("is_error")}
    r["tokens_total"] = sum(r[k] for k in ("input_tokens", "output_tokens", "cache_creation_tokens", "cache_read_tokens"))
    r["tokens_fresh"] = r["input_tokens"] + r["output_tokens"] + r["cache_creation_tokens"]
    r["wall_secs"] = time.time() - t0
    return r

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", default=str(ROOT / "tasks"))
    ap.add_argument("--claude-model", default="sonnet")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--systems", default="hx:cyberouter/glm-5.3-flash,hx:cyberouter/deepseek-v4.1-flash,claude",
                    help="comma list of hx:<model> and claude")
    ap.add_argument("--only", default="")
    ap.add_argument("--out", default=str(ROOT / "eval" / "results"))
    ap.add_argument("--timeout", type=int, default=1200)
    ap.add_argument("--resume", default="", help="results dir to scan; skip (task,system,run) triples already there")
    a = ap.parse_args()
    a.tasks = str(Path(a.tasks).resolve())
    if not Path(a.tasks).is_dir(): sys.exit(f"tasks dir not found: {a.tasks}")
    tasks = sorted(d for d in Path(a.tasks).iterdir() if d.is_dir() and (d / TASK_PROMPT).exists())
    if a.only: tasks = [t for t in tasks if t.name in a.only.split(",")]
    if not tasks: sys.exit(f"no tasks found under {a.tasks} (expected <id>/{TASK_PROMPT})")
    out = Path(a.out); out.mkdir(parents=True, exist_ok=True)
    results = out / "results.jsonl"
    done = set()
    if a.resume:
        for fp in Path(a.resume).glob("**/results.jsonl"):
            done |= {(r["task"], r["system"], r["run"]) for r in map(json.loads, fp.read_text().splitlines())}
    for task in tasks:
        for run in range(1, a.runs + 1):
            for system in a.systems.split(","):
                if (task.name, system, run) in done: continue
                ws = tempfile.mkdtemp(prefix=f"p0-{task.name}-")
                if (task / TASK_REPO).is_dir(): shutil.copytree(task / TASK_REPO, ws, dirs_exist_ok=True)
                # EVAL_OVERRIDE="std/printer=/abs/file.luau;std/loop=/abs/other.luau": per-arm module overrides (upgrade gate)
                for spec in filter(None, os.environ.get("EVAL_OVERRIDE", "").split(";")):
                    mod, src = spec.split("=", 1); pkg, m = mod.split("/", 1)
                    dest = Path(ws) / ".harness" / "override" / pkg / f"{m}.luau"; dest.parent.mkdir(parents=True, exist_ok=True); shutil.copy(src, dest)
                tag = system.replace("/", "_").replace(":", "-")
                trace = out / f"{task.name}.{tag}.{run}.trace"
                prompt = (task / TASK_PROMPT).read_text()
                try:
                    r = (run_hx(ws, task / TASK_PROMPT, system.split(":", 1)[1], a.timeout, trace) if system.startswith("hx:")
                         else run_claude(ws, prompt, a.claude_model, a.timeout, trace))
                except subprocess.TimeoutExpired:
                    r = {"harness_error": "timeout"}
                g = subprocess.run(["bash", str(task / TASK_GRADE)], cwd=ws, env={**os.environ, "TASK_DIR": str(task.resolve())},
                                   capture_output=True, text=True, timeout=600)
                r.update(task=task.name, system=system, run=run, passed=(g.returncode == 0), grader_tail=(g.stdout + g.stderr)[-300:])
                with results.open("a") as f: f.write(json.dumps(r) + "\n")
                print(f"{task.name} {system} #{run}: {'PASS' if r['passed'] else 'FAIL'} tokens={r.get('tokens_total')} turns={r.get('turns')} cost={r.get('cost_usd')}", flush=True)
                shutil.rmtree(ws, ignore_errors=True)

if __name__ == "__main__":
    main()
