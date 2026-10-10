# hx

A headless coding-agent harness in which the model writes Luau instead of calling tools one at a time. On the same 15 tasks with the same model, it used about half the fresh tokens of Claude Code.

```bash
cargo install --path . && hx exec 'print(#search("fn main", {glob = "*.rs"}))'
```

## TL;DR

**The problem:** tool-calling agents spend one model turn per action. A turn re-sends the conversation, so a task that needs 12 searches, reads and edits costs 12 round trips.

**The solution:** hx gives the model one tool, `luau(code)`. The code runs in a persistent Luau VM, so one turn can search, read the hits, edit two files and run the tests. A small Rust core does everything that has to be trusted: the sandbox, the approval gate, the event log and the model client. The agent loop, prompt and output printer are Luau modules you can override per project without rebuilding.

| Feature | What it does |
|---|---|
| Code mode | One `luau` tool; globals persist across turns; `read`, `write`, `edit`, `sh`, `search` (ripgrep engine, embedded) |
| Sandbox (macOS) | `sh` runs under Seatbelt: writes confined to the project and a temp dir, no network, no API keys in the environment |
| Approval gate | `git push`, publish, `ssh` and other external commands need the user's approval; nothing the model writes can approve |
| Replay and resume | Every effect is logged in a hash-chained log; `--resume` rebuilds the session and never repeats a side effect |
| Budgets | Cost, token and wall-clock limits, shared with subagents; the run stops cleanly when one is hit |
| Overridable harness | Loop, prompt, printer and hooks are Luau in `stdlib/std/`; replace any module in `.harness/override/std/` |
| Extensions | Tool and skill packages locked by hash, MCP servers (stdio and HTTP), agent-written tools promoted only after your review |
| Any provider | Anthropic Messages, OpenAI Chat Completions or Responses API; any compatible base URL |

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

Rerun with `python3 eval/run.py` (see `--help`).

## Installation

Requires Rust 1.85 or newer (edition 2024). The sandbox requires macOS.

```bash
git clone <!-- TODO: repository URL --> luau-harness
cd luau-harness
cargo install --path .
```

Or build without installing; the binary is `target/release/hx`:

```bash
cargo build --release
```

## Quick start

1. Set an API key. The default key variable depends on `--api`: `ANTHROPIC_API_KEY` for `anthropic` (default), `OPENROUTER_API_KEY` for `openai` and `responses`.

   ```bash
   export ANTHROPIC_API_KEY=...
   ```

2. Write the task to a file:

   ```bash
   echo "Fix the failing test in tests/test_tax.py" > task.md
   ```

3. Run it in the project directory:

   ```bash
   hx run --workspace . --prompt-file task.md --model claude-sonnet-5-5 --max-cost 0.50
   ```

hx prints a JSON report on stdout (turns, Luau errors, `finished`, `final_answer`, tokens, cost). The event log is written to `.harness/log/`.

To use an OpenAI-compatible provider instead:

```bash
hx run --prompt-file task.md --model deepseek/deepseek-v4.1-flash \
  --api openai --base-url https://openrouter.ai/api/v1 --key-env OPENROUTER_API_KEY
```

<!-- TODO: confirm the OpenRouter model id for DeepSeek V4.1 Flash; the evals used it through Enclave as cyberouter/deepseek-v4.1-flash -->

## Commands

| Command | What it does |
|---|---|
| `hx run` | Run an agent task; prints a JSON report |
| `hx run --resume LOG` | Continue a crashed run from its event log |
| `hx exec CHUNK...` | Run Luau chunks in one session (no API key) |
| `hx test PKG_DIR` | Run a package's `tests/*.luau`; exits 1 on failure |
| `hx approve PKG` | Lock a project package's hash and grant the capabilities it requests |
| `hx staged` / `hx accept NAME` / `hx reject NAME` | Review agent-promoted tools |
| `hx upgrade propose\|list\|show\|check\|accept\|rollback` | Harness-module upgrade flow |
| `hx log verify FILE` | Check a log's hash chain; exit 0 ok, 1 tampered, 3 unknown outcomes |

`hx run` flags:

| Flag | Default | Meaning |
|---|---|---|
| `--prompt-file F` | | Task text (not needed with `--resume` if the log recorded it) |
| `--model M` | `HX_MODEL` | Model id |
| `--api A` | `anthropic` (`HX_API`) | `anthropic`, `openai` (Chat Completions) or `responses` |
| `--base-url U` | `HX_BASE_URL` | API base URL; defaults to Anthropic or OpenRouter |
| `--key-env E` | see Quick start | Env var holding the API key |
| `--reasoning R` | | Reasoning effort |
| `--max-turns N` | 30 | Turn limit |
| `--max-output N` | 16384 | Max output tokens per turn |
| `--max-cost USD` `--max-tokens N` `--max-wall SECS` | none | Budgets; flags override `policy.toml` |
| `--provider-only A,B` | | OpenRouter only: route to these providers, no fallbacks |
| `--pack A,B` | | Extra packs (for example `std/authoring`) |
| `--trace FILE` | | Write a readable trace |

Global flags: `--workspace DIR` (default `.`), `--no-sandbox` (disables the sandbox and path policy, prints a warning), `--net` / `--no-net`, `--approval prompt` (ask on the terminal instead of refusing). Every command takes `--help`.

## How it works

```text
            model ── one tool: luau(code) ──┐
                                            ▼
 ┌──────────────────────────────────────────────────────────────┐
 │ Luau, replaceable per project (.harness/override/std/*)      │
 │   std/loop  std/prompt  std/printer  std/hooks  std/mcp ...  │
 │   packages: tools, skills · MCP bindings · agent-written     │
 └──────────────────────────────┬───────────────────────────────┘
                                │ read write edit sh search agent
 ┌──────────────────────────────▼───────────────────────────────┐
 │ Rust core, trusted, not modifiable by the model              │
 │   path policy · Seatbelt sandbox · approval gate · budgets   │
 │   hash-chained event log · replay · model client             │
 └──────────────────────────────────────────────────────────────┘
```

Every call from Luau into the core passes the capability check and lands in the event log. Hooks can deny or transform an action, never approve one.

Inside the VM the model also has `agent(task)` for subagents, `parallel{...}` for concurrent branches and `vars()` to list its session variables. The system prompt with the full API is [prompts/system.md](prompts/system.md).

## Configuration

All configuration lives in `.harness/` in the workspace. The model can read it but never write it.

`.harness/policy.toml`:

```toml
[fs]
read = ["/extra/read/root"]     # extra read roots for the sandbox
write = ["/extra/write/root"]   # extra write roots

[net]
allow = false                   # all-or-nothing; Seatbelt cannot filter by host

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
| Module override | `.harness/override/std/<mod>.luau` | Immediately; `require("base:std/<mod>")` wraps the original; a broken override falls back with a warning |
| Tool package | `.harness/pkgs/NAME/` with `hx.toml`, `kind = "tool"` | `hx approve NAME`; model calls `tools.NAME.fn(...)` |
| Skill | `.harness/pkgs/NAME/` with `SKILL.md`, `kind = "skill"` | `hx approve NAME`; model calls `skill("NAME")` |
| MCP server | `.harness/mcp.toml` | On the next run |
| Agent-written tool | `deftool(...)` then `promote(name)` (with `--pack std/authoring`) | Your `hx accept NAME` |
| Harness-module upgrade | `hx upgrade propose MODULE FILE` | `python3 eval/gate.py --proposal ID --model M` must pass, then `hx upgrade accept ID` |

A package that is unapproved, or changed since approval, is invisible to the model. A package only gets the primitives its manifest requests and you granted. An agent-written tool runs with only the capabilities it declares, and nothing the agent can call approves it. Every upgrade accept and rollback is appended to `.harness/upgrades.jsonl`; `hx upgrade rollback MODULE` undoes one.

## Security model

- `sh` runs under macOS Seatbelt: writes only to the project and a per-run temp dir, reads from an allowlist (project, system dirs, detected toolchains), no network unless `[net] allow = true`, a minimal environment with no API keys, its own process group killed when the command ends, and a per-user process cap.
- `read`, `write`, `edit` and `search` check the canonical path against the same roots. Writes never go through symlinks.
- `.harness/` and `hx.lock` are read-only to the model.
- Shell commands are classed read / local write / external / irreversible. The classifier is a tripwire for obvious commands such as `git push`; the sandbox is the real boundary.

## Replay and resume

Every effect (file reads and writes, `sh`, `search`, subagents, time and random values) is logged with its result. `hx run --resume LOG` reruns the loop with the logged model replies and effect results, which rebuilds the session exactly, then continues live.

- An effect with an intent but no outcome is never re-run. The model receives `unknown_outcome` and decides.
- If the prompt, a module or the printer changed between crash and resume, the run stops with `replay diverged`.
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
| `src/` | Rust core: VM and `hx` API, primitives, effects and replay, policy and sandbox, event log, packages, MCP, model client, CLI |
| `stdlib/std/` | The built-in `std` package (loop, prompt, printer, hooks, feedback, MCP binding), embedded in the binary |
| `prompts/system.md` | System prompt |
| `eval/` | Eval runner, report, upgrade gate, task generators |
| `tasks/`, `tasks-unseen/` | 15 frozen tasks (hashed in `tasks/MANIFEST.sha256`) and 5 unseen tasks |
| `scripts/` | Check suites and mock servers |

## Limitations

- **macOS only for sandboxing.** The sandbox uses `sandbox-exec`, which Apple has deprecated. There is no Linux sandbox; on other systems you would need `--no-sandbox`, which removes the path policy as well. <!-- TODO: confirm behaviour on Linux; untested -->
- **Network is all-or-nothing.** Seatbelt cannot filter by host.
- **The process cap is per user** (`RLIMIT_NPROC`), so it bounds a fork bomb rather than isolating it.
- **The Anthropic Messages path is tested only against a mock.** Chat Completions was verified live on OpenRouter and Enclave.
- **No compaction.** Long tasks are bounded by `--max-turns` and the model's context window.
- **Small benchmark.** 20 small Python tasks; results on large repositories are not measured.
- **Not covered by resume:** a crash mid-model-call repeats that call (no side effect), and the stdout of a killed `sh` is lost.

## License

MIT. See [LICENSE](LICENSE).
