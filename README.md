<p align="center">
  <img src="docs/banner.png" alt="hx, the moon imp mascot, tries to fly a git push paper plane out of a sandbox while a crossing guard stops it" width="100%">
</p>

# hx

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)
![Sandbox: macOS](https://img.shields.io/badge/sandbox-macOS-lightgrey.svg)

A headless coding-agent harness in which the model writes Luau instead of calling tools one at a time. Rust owns effects and recovery; Luau supplies composition and workflow decisions.

```bash
cargo install --git https://github.com/voladelta/luau-harness && hx exec 'print(#search("fn main", {glob = "*.rs"}))'
```

## TL;DR

**The problem:** tool-calling agents spend one model turn per action. A turn re-sends the conversation, so a task that needs 12 searches, reads and edits costs 12 round trips.

**The solution:** hx gives the model one tool, `luau(code)`. The code runs in a persistent live VM, so one turn can search, read the hits, edit two files and run the tests. A Rust supervisor owns authorization, the journal, budgets and recovery. A separate pure Luau controller returns workflow commands. Modules and approved packages are frozen for each run.

| Feature | What it does |
|---|---|
| Code mode | One `luau` tool; globals persist across turns; `read`, `write`, `edit`, `sh`, `search` (ripgrep engine, embedded) |
| Sandbox (macOS) | `sh` runs under Seatbelt: writes confined to the project and a temp dir, no network, no API keys in the environment |
| Approval gate | `git push`, publish, `ssh` and other external commands need the user's approval; nothing the model writes can approve |
| Recovery | `--resume` restores explicit saved data and controller state into a fresh VM; old effectful chunks are never replayed |
| Budgets | Shared admission thresholds reserve each request's priced upper bound (the whole remaining allowance when no price is known); an absolute deadline includes downtime |
| Compaction | On by default (`std/compact`): from 256k tokens at the latest (earlier for a smaller `--context-window`), older history becomes a structured summary; recent messages and Luau globals stay; one compact-and-retry on a context-overflow error |
| Checkpoint | On by default (`std/checkpoint`): the system prompt warns that the first final answer will be checked, and before it is accepted the model is sent back once to name the root cause, list the other cases it implies, test each and re-check the task's requirements; off with `require("std/checkpoint").enabled = false` in `.harness/init.luau` |
| Overridable harness | Pure controller, prompt, printer and hooks are Luau in `stdlib/std/`; overrides apply to new runs |
| Extensions | Immutable approved packages, isolated method calls with copied data, MCP servers, staged proposals requiring your review |
| Any provider | OpenAI Chat Completions or Responses API; any compatible base URL (OpenRouter by default) |
| Claude plan | `--api claude-sdk --model haiku\|sonnet\|opus`: drives the `claude` CLI over stream-json (as the Agent SDK does), so calls use your Claude login and plan limits, no API key; hx's `luau` tool is a native tool of an in-process SDK MCP server, so tool calls are structured (no text protocol); images are sent as base64 blocks |

## Quick example

`hx exec` runs Luau in one session without a model or API key, so you can see what the model sees:

```bash
hx exec 'hits = search("fn main", {glob = "*.rs"})' 'print(#hits, hits[1].file)'
```

```text
-- ops=1 error=false
1	src/main.rs
-- ops=0 error=false
```

A real turn from the model looks like this, and does in one round trip what a tool-calling agent does in four:

```luau
print(edit("src/config.py", "timeout = 5", "timeout = 30"))
write("tests/test_config.py", [==[
from src.config import load

def test_timeout():
    assert load()["timeout"] == 30
]==])
print(sh("python -m pytest -q 2>&1 | tail -15").stdout)
```

## Results

These measurements describe the earlier heap-replay runtime. Runtime v2 has not been benchmarked; the tables are retained as historical evidence, not performance claims for this redesign.

Frozen suite: 15 multi-file Python tasks (bug fixes, features, refactors over five small codebases), graded by hidden tests. Fresh tokens are input + cache writes + output. Measured 2026-10-10.

| System | Model | Pass | Fresh tokens/task | Turns | $/task |
|---|---|---|---|---|---|
| hx | Haiku 5.5, medium effort | 45/45 (3 runs) | 6.4k | 3.5 | 0.0019 |
| hx | DeepSeek V4.1 Flash | 45/45 (3 runs) | 6.4k | 4.5 | 0.0041 |
| Claude Code, lean prompt | Haiku 5.5, medium effort | 15/15 (1 run) | 13.4k | 10.8 | 0.0045 (API-equivalent) |

Caveats:
- The Claude Code baseline is one run per task; identical hx runs varied by up to 1.34x in fresh tokens.
- On Sonnet 5.5 at low effort the gap is smaller: 4.7k vs 5.7k fresh tokens per task, because Sonnet needs few turns either way.
- Part of the system prompt was tuned after reading frozen-task traces. On a separate unseen suite of 5 tasks, hx passed 5/5 on both models (DeepSeek 6.1k, Haiku 5.1k fresh tokens per task).
- Claude Code ran without a sandbox; hx ran with it.
- Re-checked 2026-10-13 on the Claude plan (`--api claude-sdk`, after output tokens became exact): 5 bug tasks, Haiku 5.5 medium, one run each. Both passed 5/5; medians per task were 3.7k vs 7.0k fresh tokens, 770 vs 2,074 output tokens, 3 vs 6 turns, hx first.

Rerun with `python3 eval/run.py` (see `--help`).

### Checkpoint on hard tasks

Terminal-Bench 4.0 tasks, run on the macOS host with hx on Sonnet 5.5 at medium effort (Claude plan), graded by each task's own tests in its verifier container. "Checkpoint" is the `std/checkpoint` pack (opt-in when these runs were made, on by default since). Runs the Claude plan's usage limit cut off are excluded, not counted as failures.

| Task | Without checkpoint | With checkpoint |
|---|---|---|
| mvcc-lsm-compaction | 1/20 solved, 0.76 of tests | 13/20 solved, 0.92 of tests (two-tailed Fisher p=0.0001) |
| risk-scorer-replay | 0/1 solved (3/5 tests) | 2/2 solved |
| hof-topology-interpenetration | 0/1 solved (37/38 tests) | 2/2 solved |

Caveats:
- mvcc is the only pre-registered comparison (20 runs per arm, interleaved); risk and hof are leads from a few runs. Setup, method and task notes: `eval/tb4/README.md`. Opus 5.5 at low and medium effort solved mvcc 0/12, in both hx and Claude Code.
- The checkpoint does not help when a task hinges on a convention its text does not state: production-planning failed 18/20 tests with and without it.
- The checkpoint adds turns, and so cost, on every run, including easy ones.
- It can hurt build tasks: on react-lead-form (Sonnet 5.5 medium) base solved 6/6 and the checkpoint 1/6, through fragile tests left in the project and late edits that broke the main path. Turn it off for such work.
- With the heads-up line in the system prompt (added 2026-10-14), Opus 5.5 low solved mvcc 5/5 (pre-registered; the check alone never got Opus to a solve).

## Installation

Requires Rust 1.85 or newer (edition 2024). The sandbox requires macOS.

```bash
git clone https://github.com/voladelta/luau-harness
cd luau-harness
cargo install --path .
```

Or build without installing; the binary is `target/release/hx`:

```bash
cargo build --release
```

## Quick start

1. Set an API key. By default hx calls OpenRouter and reads `OPENROUTER_API_KEY` (override with `--base-url` and `--key-env`).

   ```bash
   export OPENROUTER_API_KEY=...
   ```

2. Write the task to a file:

   ```bash
   echo "Fix the failing test in tests/test_tax.py" > task.md
   ```

3. Run it in the project directory:

   ```bash
   hx run --workspace . --prompt-file task.md --model anthropic/claude-sonnet-5.5 --max-cost 0.50
   ```

hx prints a JSON report on stdout (turns, Luau errors, `finished`, `final_answer`, tokens, cost). The event log is written to `.harness/log/`.

Any model on OpenRouter works the same way, for example `--model deepseek/deepseek-v4.1-flash`. For another OpenAI-compatible provider, pass its URL and key variable:

```bash
hx run --prompt-file task.md --model MODEL --base-url https://api.example.com/v1 --key-env EXAMPLE_API_KEY
```

With a Claude subscription and the `claude` CLI logged in, no API key is needed:

```bash
hx run --prompt-file task.md --api claude-sdk --model sonnet
```

## Commands

| Command | What it does |
|---|---|
| `hx run` | Run an agent task; prints a JSON report |
| `hx run --resume LOG` | Restore the saved manifest and latest checkpoint; no repeated launch flags |
| `hx run --branch LOG --model M` | Start a new run using current approved configuration and inherited saved data |
| `hx recover LOG OP --decision complete\|abandon --yes` | Record an owner's decision about an uncertain mutation; never executes it |
| `hx recover LOG OP --decision billing --tokens N --cost USD --yes` | Settle an uncertain model request's reservation using asserted usage |
| `hx revoke LOG` | Stop admitting further operations for a run |
| `hx exec CHUNK...` | Run Luau chunks in one session (no API key) |
| `hx test PKG_DIR` | Run a package's `tests/*.luau`; exits 1 on failure |
| `hx approve PKG` | Lock a project package's or skill's hash and grant the capabilities it requests |
| `hx skills` | List skills in `.agents/skills` and `~/.agents/skills` and whether each is approved |
| `hx trust [--yes]` | Review this workspace's `.harness` config and `hx.lock`, then trust it |
| `hx staged` / `hx accept NAME` / `hx reject NAME` | Review agent-promoted tools |
| `hx upgrade propose\|list\|show\|check\|accept\|rollback` | Harness-module upgrade flow |
| `hx log verify FILE` | Check a log's hash chain; exit 0 ok, 1 tampered, 3 unknown outcomes |

`hx run` flags:

| Flag | Default | Meaning |
|---|---|---|
| `--prompt-file F` | | Task text (not needed with `--resume` if the log recorded it) |
| `--model M` | `HX_MODEL` | Model id |
| `--api A` | `openai` (`HX_API`) | `openai` (Chat Completions), `responses`, or `claude-sdk` (the `claude` CLI and your Claude plan; `--model haiku\|sonnet\|opus`) |
| `--base-url U` | `HX_BASE_URL` | API base URL; defaults to OpenRouter |
| `--key-env E` | `OPENROUTER_API_KEY` | Env var holding the API key |
| `--reasoning R` | | Reasoning effort |
| `--max-turns N` | 200 | Turn limit; the report says `stopped: "max_turns: N"` when it is hit |
| `--max-output N` | 16384 | Max output tokens per turn |
| `--context-window N` | 1000000 (assumed) | The model's context window, used by `std/compact`: compaction starts at min(~95% of it, 256k); give a smaller model's window so it compacts before an overflow |
| `--max-cost USD` `--max-tokens N` `--max-wall SECS` | none | Admission thresholds and absolute deadline; flags override `policy.toml` for a new run |
| `--provider-only A,B` | | OpenRouter only: route to these providers, no fallbacks |
| `--pack A,B` | | Extra packs: `std/authoring` (agent-written tools), `std/vision` (`view()` shows images to a vision model) |
| `--trace FILE` | | Write a readable trace |

Global flags: `--workspace DIR` (default `.`), `--yolo` (no sandbox, open network, no path limits and no approval gate; `.harness/` and `hx.lock` stay read-only; prints a warning), `--net` / `--no-net`, `--approval prompt` (ask on the terminal instead of refusing). Every command takes `--help`.

## How it works

```text
            model ── one tool: luau(code) ──┐
                                            ▼
 ┌──────────────────────────────────────────────────────────────┐
 │ Persistent live agent VM · isolated package invocations     │
 │ Pure Luau controller: step(saved state, event) → commands   │
 │ Frozen modules and grants · copied data at VM boundaries    │
 └──────────────────────────────┬───────────────────────────────┘
                                │ read write edit sh search agent
 ┌──────────────────────────────▼───────────────────────────────┐
 │ Rust supervisor, trusted, not modifiable by the model        │
 │   principals · policy · Seatbelt · approvals · reservations  │
 │   single-writer journal · receipts · checkpoints · adapters  │
 └──────────────────────────────────────────────────────────────┘
```

Every admitted effect passes a Rust capability check and lands in the journal. Control hooks can deny or transform commands; they cannot grant authority. A controller-hook failure stops its transition. Observer failures are diagnostic.

Inside the VM the model also has `agent(task)`, `batch{...}` for independent reads/searches/read-only subagents, `durable.get/set` for saved data (taught in the recovery notice, since the restored conversation covers most of it), and `vars()` to list live variables. The full API is in [prompts/system.md](prompts/system.md); ownership, guarantees and migration details are in [Runtime v2](docs/runtime-v2.md).

## Configuration

All configuration lives in `.harness/` in the workspace. The model can read it but never write it.

`.harness/policy.toml` (like all workspace config, used only after `hx trust`, see below):

```toml
[fs]
read = ["/extra/read/root"]     # extra read roots for the sandbox
write = ["/extra/write/root"]   # extra write roots

[net]
allow = false                   # all-or-nothing; Seatbelt cannot filter by host

[gpu]
allow = false                   # true lets `sh` use Metal (Skia/wgpu renderers such as fframes)

[approval]
mode = "deny"                   # "deny" (headless default) or "prompt"
ask = ["terraform apply"]       # extra command substrings that need approval
allow = ["git push origin"]     # pre-approved (substring; irreversible commands need an exact match)

[budget]
max_cost = 0.50
max_tokens = 600000
max_wall_secs = 900

[packs]
enable = []
```

`.harness/mcp.toml`:

```toml
[server.notes]                  # local server, runs under its own Seatbelt profile
command = "python3"
args = ["server.py"]
read_only_tools = ["read_*"]    # these run freely; every other tool needs approval
env = ["NOTES_DIR"]             # the only env vars passed through
net = false

[server.remote]                 # remote Streamable HTTP server (https, or http on localhost)
url = "https://mcp.example.com/mcp"
bearer_env = "REMOTE_TOKEN"     # or headers_env = {"x-api-key" = "VAR"}
```

MCP tools bind as `mcp.SERVER.tool{...}`. Approve a non-read-only tool with `allow = ["mcp notes.add"]` in `policy.toml`.

## Extending

| Kind | Where | How it becomes active |
|---|---|---|
| Module override | `.harness/override/std/<mod>.luau` | On a new run; `require("base:std/<mod>")` wraps the original. Invalid controller code stops the run |
| Tool package | `.harness/pkgs/NAME/` with `hx.toml`, `kind = "tool"` | `hx approve NAME`; model calls `tools.NAME.fn(...)` |
| Skill | `.harness/pkgs/NAME/` with `SKILL.md`, `kind = "skill"` | `hx approve NAME`; model calls `skill("NAME")` (or finds it with `skills("words")`; skills are not listed in the prompt) |
| Agent skill | `.agents/skills/NAME/SKILL.md` or `~/.agents/skills/NAME/SKILL.md` (no `hx.toml`; project shadows global) | `hx approve NAME` (`hx skills` lists them); its directory becomes read-only readable |
| MCP server | `.harness/mcp.toml` | On the next run |
| Agent-written tool | `deftool(...)` then `promote(name)` (with `--pack std/authoring`) | Staged without execution; you review, run `hx test DIR`, accept it, and start a new run |
| Harness-module upgrade | `hx upgrade propose MODULE FILE` | `python3 eval/gate.py --proposal ID --model M` must pass, then `hx upgrade accept ID` |

**Workspace trust.** `.harness/{policy,grants,mcp}.toml`, `.harness/init.luau`, `.harness/override/` and `hx.lock` live in the repo, so whoever wrote the repo could widen the sandbox, start MCP commands or pre-approve packages and skills. hx ignores that config until you review it with `hx trust`, which records its hash in `~/.config/hx/trust.toml` (outside the repo, protected from the model). Any edit makes it untrusted again; `hx approve`/`accept`/`upgrade accept` keep an already trusted workspace trusted. A workspace without such files needs nothing. `HX_TRUST=1` in hx's environment trusts the workspace for one launch (tests, throwaway eval copies).

A package that is unapproved, or changed since approval, is invisible to a new run. Each approved package call uses a fresh VM and dispatch-time capability checks. Imports are restricted to that package; function references and host handles cannot cross its boundary. Accepted changes apply to future runs, with no hot reload. Nothing the agent can call approves source. Every upgrade accept and rollback is appended to `.harness/upgrades.jsonl`; `hx upgrade rollback MODULE` undoes one.

## Security model

- `sh` runs under macOS Seatbelt: writes only to the project and a per-run temp dir, reads from an allowlist (project, system dirs, detected toolchains, approved skill dirs), no network unless `[net] allow = true`, a minimal environment with no API keys, its own process group killed when the command ends, and a per-user process cap.
- `read`, `write`, `edit` and `search` check the canonical path against the same roots. Writes never go through symlinks.
- `.harness/` and `hx.lock` are read-only to the model.
- Shell commands are classed read / local write / external / irreversible. The classifier is a tripwire for obvious commands such as `git push`; the sandbox is the real boundary.

## Checkpoints and resume

`hx run --resume LOG` restores frozen settings and the latest committed controller state and `durable` data into fresh VMs. Ordinary globals and functions are lost. It supplies receipts from interrupted work to a recovery turn and never reruns old chunks to reconstruct memory.

- An effect with an intent but no outcome is uncertain. `write` and `edit` replace files atomically, so resume settles them from the file's hash. A sandboxed `sh` in a run without network could only change the workspace and temp dir, so resume settles it as `unknown_local` and the recovery turn inspects the workspace. For `sh` under `--yolo` or with network, and for MCP calls, new mutations are blocked until the owner records a decision with `hx recover`.
- Resume uses the original manifest, including the standard library, even if workspace modules or the binary changed; `--branch LOG` selects new configuration explicitly.
- An OS lock prevents simultaneous resume.
- A lost response is never redispatched. When its reservation was a real bound (a priced model, or no dollar threshold) hx charges it at that bound and sends one new request; otherwise it keeps its reservation until owner billing reconciliation. A 4xx or 503 answer releases its reservation, and a 408, 429, 5xx or malformed 200 gets one more request.
- The log stores file contents and command output as blobs in `.harness/log/blobs/`. Treat it as sensitive.

## Development

Build first: every check uses `target/release/hx`, and none needs an API key (model runs use a scripted mock).

```bash
cargo build --release
scripts/all_checks.sh        # every suite
hx test stdlib/std           # Luau unit tests
cargo test --release
```

| Path | Contents |
|---|---|
| `src/` | Rust supervisor, pure controller boundary, VMs, isolated packages, batches, policy, journal/checkpoints, MCP, model adapters and CLI |
| `stdlib/std/` | The built-in `std` package (loop, prompt, printer, hooks, feedback, MCP binding), embedded in the binary |
| `prompts/system.md` | System prompt |
| `eval/` | Eval runner, report, upgrade gate, task generators |
| `tasks/`, `tasks-unseen/` | 15 frozen tasks (hashed in `tasks/MANIFEST.sha256`) and 5 unseen tasks |
| `tasks-hard/` | 9 harder tasks over four larger codebases (300 to 860 lines each), scored by the share of hidden tests passed |
| `scripts/` | Check suites and mock servers |

## Limitations

- **macOS only for sandboxing.** The sandbox uses `sandbox-exec`, which Apple has deprecated. There is no Linux sandbox; on other systems you would need `--yolo`, which also removes the path limits and the approval gate, and there `sh` can write `.harness/` (on macOS `--yolo` still blocks that with Seatbelt). hx has not been tested on Linux.
- **Network is all-or-nothing.** Seatbelt cannot filter by host.
- **The process cap is per user** (`RLIMIT_NPROC`), so it bounds a fork bomb rather than isolating it.
- **Only Chat Completions was verified live** (OpenRouter and Enclave); the Responses API is tested against a mock.
- **Compaction and finish review are heuristics.** Summary quality and review quality depend on the model. Context size is configured, not detected. Resume restores it automatically. Disable compaction with `require("std/compact").enabled = false` in `.harness/init.luau` before starting a new run.
- **Small benchmark.** The published results cover 20 small Python tasks, which every tested configuration passes, so they measure efficiency rather than quality. A harder 9-task suite (`tasks-hard/`) exists but has no published results yet, and results on large repositories are not measured.
- **Recovery is explicit.** Arbitrary globals, closures and scratch files are not durable. A killed command's unrecorded output is lost; its external effect may already have happened. There is no exactly-once remote execution or rollback guarantee.
- **Budgets are admission thresholds.** Request-byte allowances are not exact token counts, provider costs may be unavailable, and cancellation cannot stop remote billing already accepted. Runtime v2 has scripted validation, not new live-provider or performance measurements.

## License

MIT. See [LICENSE](LICENSE).
