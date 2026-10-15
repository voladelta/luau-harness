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
| Recheck | On by default (`std/recheck`): the system prompt warns that the first final answer will be checked, and before it is accepted the model is sent back once to name the root cause, list the other cases it implies, test each and re-check the task's requirements. It adds turns on every run and can hurt build tasks; off with `require("std/recheck").enabled = false` in `.harness/init.luau` |
| Overridable harness | Pure controller, prompt, printer and hooks are Luau in `stdlib/std/`; overrides apply to new runs |
| Extensions | Immutable approved packages, isolated method calls with copied data, MCP servers, agent-staged tools and module replacements that wait for your review |
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
| `hx staged` / `hx accept NAME` / `hx reject NAME` | Review what the agent staged (tools as `NAME`, module replacements as `std/MODULE`), then activate or discard it |
| `hx rollback MODULE` | Undo the last accepted replacement of a harness module |
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
| Agent-written module replacement | `propose_module("std/MODULE", source, {reason})` (with `--pack std/authoring`) | Staged; `hx accept std/MODULE` shows the reason, a structural check (it loads and keeps every export) and the diff, then makes it the override. `hx rollback std/MODULE` undoes it |

**Approvals.** `hx.lock` is the single approval record: per package, the approved version, its content hash and the capabilities granted to it (`[package.grants]`, where `"*"` grants every scope). Workspaces from before this format kept grants in `.harness/grants.toml`, which hx no longer reads: run `hx approve NAME` again for each package that requests capabilities.

**Workspace trust.** `.harness/{policy,mcp}.toml`, `.harness/init.luau`, `.harness/override/` and `hx.lock` live in the repo, so whoever wrote the repo could widen the sandbox, start MCP commands or pre-approve packages and skills. hx ignores that config until you review it with `hx trust`, which records its hash in `~/.config/hx/trust.toml` (outside the repo, protected from the model). Any edit makes it untrusted again; `hx approve`/`accept`/`rollback` keep an already trusted workspace trusted. A workspace without such files needs nothing. `HX_TRUST=1` in hx's environment trusts the workspace for one launch (tests, throwaway copies).

A package that is unapproved, or changed since approval, is invisible to a new run. Each approved package call uses a fresh VM and dispatch-time capability checks. Imports are restricted to that package; function references and host handles cannot cross its boundary. Accepted changes apply to future runs, with no hot reload. Nothing the agent can call approves source. Every module accept and rollback is appended to `.harness/upgrades.jsonl`.

## Security model

- `sh` runs under macOS Seatbelt: writes only to the project and a per-run temp dir, reads from an allowlist (project, system dirs, detected toolchains, approved skill dirs), no network unless `[net] allow = true`, a minimal environment with no API keys, its own process group killed when the command ends, and a per-user process cap.
- `read`, `write`, `edit` and `search` check the canonical path against the same roots. Writes never go through symlinks.
- `.harness/` and `hx.lock` are read-only to the model.
- Shell commands are classed read / local write / external / irreversible. The classifier is a tripwire for obvious commands such as `git push`; the sandbox is the real boundary.

## Checkpoints and resume

`hx run --resume LOG` restores the frozen manifest, the latest committed controller state and `durable` data into fresh VMs; it never reruns old chunks, and ordinary globals are lost. Interrupted local `write`/`edit` and sandboxed offline `sh` settle themselves from the evidence; other uncertain effects and unbounded model reservations wait for `hx recover`. `--branch LOG` starts a new run with new settings and the inherited saved data. The log stores file contents and command output as blobs in `.harness/log/blobs/`, so treat it as sensitive. The full rules are in [Runtime v2](docs/runtime-v2.md).

## Development

Every check uses `target/release/hx`, and none needs an API key (model runs use a scripted mock).

```bash
scripts/all_checks.sh        # builds, then runs every suite
hx test stdlib/std           # Luau unit tests
cargo test --release
```

| Path | Contents |
|---|---|
| `src/` | Rust supervisor, pure controller boundary, VMs, isolated packages, batches, policy, journal/checkpoints, MCP, model adapters and CLI |
| `stdlib/std/` | The built-in `std` package (loop, prompt, printer, hooks, feedback, MCP binding), embedded in the binary |
| `prompts/system.md` | System prompt |
| `scripts/` | Check suites and mock servers |

## Limitations

- **macOS only for sandboxing.** The sandbox uses `sandbox-exec`, which Apple has deprecated. There is no Linux sandbox; on other systems you would need `--yolo`, which also removes the path limits and the approval gate, and there `sh` can write `.harness/` (on macOS `--yolo` still blocks that with Seatbelt). hx has not been tested on Linux.
- **Network is all-or-nothing.** Seatbelt cannot filter by host.
- **The process cap is per user** (`RLIMIT_NPROC`), so it bounds a fork bomb rather than isolating it.
- **Only Chat Completions was verified live** (OpenRouter and Enclave); the Responses API is tested against a mock.
- **Compaction and finish review are heuristics.** Summary quality and review quality depend on the model. Context size is configured, not detected. Resume restores it automatically. Disable compaction with `require("std/compact").enabled = false` in `.harness/init.luau` before starting a new run.
- **No benchmark.** hx ships without an eval suite; its checks cover behavior, not task success or token efficiency.
- **Recovery is explicit.** Arbitrary globals, closures and scratch files are not durable. A killed command's unrecorded output is lost; its external effect may already have happened. There is no exactly-once remote execution or rollback guarantee.
- **Budgets are admission thresholds.** Request-byte allowances are not exact token counts, provider costs may be unavailable, and cancellation cannot stop remote billing already accepted. Runtime v2 has scripted validation, not new live-provider or performance measurements.

## License

MIT. See [LICENSE](LICENSE).
