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
| [RPI workflow](docs/workflows.md#rpi-research-design-implement-verify) | `--rpi` runs research → design → implementation → verification with up to three repair cycles; `--manual` adds owner review gates |
| [Autoresearch](docs/workflows.md#autoresearch) | `--ar` improves a score from a held-out evaluator over the paths you allow, with parallel workers and audited gates |
| [Recovery](docs/cli.md#checkpoints-and-resume) | `--resume` restores saved data and controller state into a fresh VM; old effectful chunks are never replayed |
| [Budgets](docs/configuration.md#policytoml) | Cost, token and wall-clock admission thresholds; each request reserves its priced upper bound |
| [Compaction](docs/configuration.md#initluau-compaction-and-recheck) | On by default: from 256k tokens at the latest, older history becomes a structured summary; Luau globals stay |
| [Extensions](docs/extending.md) | Override any `std` module; add single-file tools, approved packages, skills and MCP servers |
| Any provider | Responses API or Chat Completions; any compatible base URL (OpenRouter by default) |

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
cargo install --git https://github.com/voladelta/luau-harness
```

Or from a clone; `cargo build --release` builds without installing (the binary is `target/release/hx`):

```bash
git clone https://github.com/voladelta/luau-harness
cd luau-harness
cargo install --path .
```

## Quick start

1. Set an API key. By default hx calls OpenRouter and reads `OPENROUTER_API_KEY`.

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

hx prints a JSON report on stdout (turns, Luau errors, `finished`, `final_answer`, tokens, cost) and writes the event log to `.harness/log/`.

Any OpenRouter model works the same way. For another OpenAI-compatible provider, pass its URL and key variable:

```bash
hx run --prompt-file task.md --model MODEL --base-url https://api.example.com/v1 --key-env EXAMPLE_API_KEY
```

## Commands

| Command | What it does |
|---|---|
| `hx run` | Run an agent task (`--rpi`, `--ar`, `--resume LOG`, `--branch LOG`) |
| `hx exec CHUNK...` | Run Luau chunks in one session (no API key) |
| `hx review` / `hx recover` / `hx revoke` | Record an RPI review decision, settle an uncertain operation, stop a run |
| `hx trust` | Review this workspace's `.harness` config and `hx.lock`, then trust it |
| `hx approve` / `hx skills` / `hx test` | Approve packages and skills, list skills, run a package's tests |
| `hx staged` / `hx accept` / `hx reject` / `hx rollback` | Review and activate what the agent staged; undo a module replacement |
| `hx log verify FILE` | Check a log's hash chain and recovery accounting |

Every flag and exit code is in the [command reference](docs/cli.md).

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

Every admitted effect passes a Rust capability check and lands in the journal. Control hooks can deny or transform commands; they cannot grant authority.

Inside the VM the model also has `agent(task)`, `batch{...}` for independent reads/searches/read-only subagents, `durable.get/set` for saved data, and `vars()` to list live variables. The full API is in [prompts/system.md](prompts/system.md); ownership, guarantees and recovery rules are in [Runtime v2](docs/runtime-v2.md).

## Security model

- `sh` runs under macOS Seatbelt: writes only to the project and a per-run temp dir, reads from an allowlist (project, system dirs, detected toolchains, approved skill dirs), no network unless `[net] allow = true`, a minimal environment with no API keys, its own process group killed when the command ends, and a per-user process cap.
- `read`, `write`, `edit` and `search` check the canonical path against the same roots. Writes never go through symlinks.
- `.harness/` and `hx.lock` are read-only to the model. Workspace config is ignored until you [trust](docs/configuration.md#workspace-trust) it.
- Shell commands are classed read / local write / external / irreversible. The classifier is a tripwire for obvious commands such as `git push`; the sandbox is the real boundary.

## Documentation

| Page | Covers |
|---|---|
| [Command reference](docs/cli.md) | Every command and flag, the run report, resume and branch |
| [Workflows](docs/workflows.md) | `--rpi` stages and review gates; `--ar` setup and how a search runs |
| [Configuration](docs/configuration.md) | `policy.toml`, `mcp.toml`, compaction and recheck, workspace trust, `hx.lock` |
| [Extending](docs/extending.md) | Module overrides, single-file and HTTP tools, packages, skills, credentials, the bundled example tools |
| [Runtime v2](docs/runtime-v2.md) | State ownership, dispatch and uncertainty, durable data, recovery guarantees |

## Development

Every check uses `target/release/hx`, and none needs an API key (model runs use a scripted mock).

```bash
scripts/all_checks.sh            # builds, then runs every offline suite; five-minute deadline
scripts/all_checks.sh --jobs 2   # fewer concurrent suites (default: 4)
hx test stdlib/std               # Luau unit tests
cargo test --release
```

Independent suites run concurrently; suites sharing mock ports run sequentially, and the process-cap sandbox suite runs alone. Stalled suites fail with a diagnostic, and reaching the five-minute budget fails the run rather than skipping checks. Live provider calls stay outside this runner.

| Path | Contents |
|---|---|
| `src/` | Rust supervisor, pure controller boundary, VMs, isolated packages, batches, policy, journal/checkpoints, MCP, model adapters and CLI |
| `stdlib/std/` | The built-in `std` package (loop, prompt, printer, hooks, feedback, MCP binding), embedded in the binary |
| `prompts/system.md` | System prompt |
| `examples/tools/` | Example single-file tools (TinyFish, OpenRouter, SSoT) |
| `scripts/` | Check suites and mock servers |

## Limitations

- **macOS only for sandboxing.** The sandbox uses `sandbox-exec`, which Apple has deprecated. There is no Linux sandbox; on other systems you would need `--yolo`, which also removes the path limits and the approval gate, and there `sh` can write `.harness/` (on macOS `--yolo` still blocks that with Seatbelt). hx has not been tested on Linux.
- **Network is all-or-nothing.** Seatbelt cannot filter by host.
- **The process cap is per user** (`RLIMIT_NPROC`), so it bounds a fork bomb rather than isolating it.
- **Only Chat Completions was verified live** (OpenRouter and Enclave); the Responses API is tested against a mock.
- **Compaction and finish review are heuristics.** Summary and review quality depend on the model. Context size is configured, not detected; resume restores it. Disabling compaction is in [configuration](docs/configuration.md#initluau-compaction-and-recheck).
- **No benchmark.** hx ships without an eval suite; its checks cover behavior, not task success or token efficiency.
- **Recovery is explicit.** Arbitrary globals, closures and scratch files are not durable. A killed command's unrecorded output is lost; its external effect may already have happened. There is no exactly-once remote execution or rollback guarantee.
- **Budgets are admission thresholds.** Request-byte allowances are not exact token counts, provider costs may be unavailable, and cancellation cannot stop remote billing already accepted. Runtime v2 has scripted validation, not new live-provider or performance measurements.

## License

MIT. See [LICENSE](LICENSE).
