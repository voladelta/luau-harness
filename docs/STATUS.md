# hx status

What exists, how to verify it, and what is still missing. `PLAN.md` is the plan; this file is the ground truth about the code.

## Layout
- `src/` Rust core (L0): `vm.rs` (Luau VM, sessions, `require`, `hx` API), `prims.rs` (read/write/edit/sh/search),
  `effects.rs` (every effect live or replayed: write-ahead intents, recorded results, resume queues),
  `policy.rs` (sandbox, path policy, approval gate), `eventlog.rs` (hash-chained log), `modules.rs` (resolver, manifest,
  lockfile, grants), `llm.rs` / `host.rs` (model client, usage, subagents), `main.rs` (CLI).
- `stdlib/std/` the built-in `std` package (L2, Luau): `loop`, `prompt`, `extract`, `printer`, `feedback`, `hooks`.
  Embedded in the binary; override any module per project with `.harness/override/std/<mod>.luau`
  (`require("base:std/<mod>")` wraps the original). A broken override falls back to the built-in with a warning.
- `prompts/system.md` the system prompt (also an overridable asset).
- `eval/` runner, report, task generators; `tasks/` (15 frozen, hashed in `tasks/MANIFEST.sha256`), `tasks-unseen/` (5).

## CLI
`hx run --workspace D --prompt-file F --model M [--max-cost USD --max-tokens N --max-wall SECS] [--api anthropic|openai|responses --base-url U --key-env E --reasoning R] [--approval prompt]`
`hx run --workspace D --model M ... --resume .harness/log/<session>.jsonl` (continue a crashed run; the task is read from the log)
`hx exec --workspace D 'luau chunk' ...` (no API key) · `hx test PKG_DIR` · `hx approve PKG` · `hx log verify FILE`
The sandbox is ON by default; `--no-sandbox` is an explicit opt-out.

## Checks (all need `cargo build --release` first; none needs an API key)
| Script | Covers | Result |
|---|---|---|
| `hx test stdlib/std` | printer, extractor, mcp, hooks unit tests (Luau) | 24 pass |
| `cargo test --release` | Rust unit tests: mcp glob, path normalize, command classes, protected paths | 3 pass |
| `scripts/p2_checks.sh` | printer override without rebuild, broken-override fallback, parallel, vars, lock/grants gate, hooks, subagents, hot reload, failed lock check stays failed | 16/16 |
| `scripts/sandbox_checks.sh` | writes outside project, ../, symlinks, protected `.harness`, env scrubbing, network, process cap, timeout kills group | 35/35 |
| `scripts/p1_checks.sh` | hash-chained log, write-ahead intents, tamper detection, approval gate, crash => unknown outcome, log kept under --no-sandbox | 19/19 |
| `scripts/budget_checks.sh` | cost/token/wall budgets (flags and policy.toml), shared with subagents, graceful stop | 10/10 |
| `scripts/caps_checks.sh` | runtime package capabilities: only granted primitives exist, `getfenv`/`loadstring`/`_G`/`require` escapes closed | 5/5 |
| `scripts/llm_checks.sh` | wire formats (chat completions, responses, anthropic messages), no provider pin, unknown `--api` refused | 7/7 |
| `scripts/all_checks.sh` | runs every suite above | |
| `scripts/p4_checks.sh` | skills, tool packages, prompt index, `deftool`/`promote`/`hx accept`, no self-approval, hot reload keeps session | 30/30 |
| `scripts/mcp_checks.sh` | MCP: binding, schema signatures, approval gate, sandboxed server (fs/net/env), prompt index | 19/19 |
| `scripts/mcp_resume_check.py` | crash after every record of a run that calls MCP tools: no call is ever repeated on the server | 16/16 |
| `scripts/upgrade_checks.sh` | harness-module upgrades: propose, structural check, eval gate (scripted model), accept, rollback, tamper, force | 23/23 |
| `scripts/resume_checks.py` | AC6: SIGKILL after EVERY log record of a scripted run, resume, assert no repeated side effect, state rebuilt, logs intact; changed harness => `replay diverged` | 36/36 |

## Security model (what the model can and cannot do)
- `sh` runs under macOS Seatbelt: writes only to the project and a per-run temp dir, reads from an allowlist
  (project, system dirs, detected toolchains), no network unless `[net] allow = true` in `.harness/policy.toml`,
  minimal environment (no API keys), own process group killed when the command ends, per-user process cap.
- `read/write/edit/search` check the canonical path against the same roots; writes never go through symlinks.
- `.harness/` and `hx.lock` are read-only to the model. Grants, overrides, init, policy and the log live there.
- Effects are classed read / local_write / external / irreversible. External and irreversible shell commands (git push,
  publish, ssh, ...) need the user's approval from `policy.toml` or an interactive prompt; hooks can deny, never approve.
  This classifier is a tripwire for obvious commands; the sandbox (no network, confined writes) is the real boundary.

## Replay and resume
Every effect (read, write, edit, sh, search, subagent, os.time/os.clock/os.date/math.random, the file listing) is logged with
its full result (strings as content-addressed blobs in `.harness/log/blobs/`, so the log contains file contents and command
output: treat it as sensitive). `hx run --resume LOG` re-runs the same Luau loop with the model's logged replies and every
logged effect result returned instead of executed, which rebuilds the exact session state, then continues live.
- An effect with an intent but no outcome is an UNKNOWN OUTCOME: it is never re-run; the model receives
  `unknown_outcome` and decides (the log then records it as resolved).
- Replay is checked: each call must match the logged tool + argument hash and each model request the logged hash, so a
  changed prompt/module/printer between crash and resume stops with `replay diverged` instead of replaying wrong results.
- `parallel{}` branches replay from their own queues; `agent()` is replayed as one effect.
- Not covered: a crash mid-LLM-call just repeats that call (no side effect); there is no `hx resume` for a log whose
  task record was never written (pass --prompt-file); stdout of a killed `sh` is lost (unknown outcome).

## Known gaps (not done)
- A package's code sees only the primitives it requested and the user granted (`hx approve`); the `net` capability has no runtime
  effect because there is no network primitive (the sandbox blocks the network for `sh`). Packages can still `require` any `std/*` module.
- Seatbelt cannot filter by host; `[net] allow` is all-or-nothing. `sandbox-exec` is deprecated by Apple.
- Focus is macOS; Linux sandboxing (Landlock/bubblewrap), MCP/WASM plugins, `deftool`/`promote` (P4+).
- The process cap uses RLIMIT_NPROC, which is per user, so it bounds a bomb rather than isolating it.

## Latest eval (DeepSeek V4.1 Flash on hx, sandbox on; baselines = Claude Code)
| | pass | fresh tokens vs lean Sonnet | vs heavy (subscription) | cost vs lean |
|---|---|---|---|---|
| 15 frozen tasks × 3 | 45/45 | 0.67x (bug 0.62, feature 0.76, refactor 0.54) | 0.57x | 0.08x |
| 5 unseen tasks × 3 | 14/15 | 0.73x | 0.56x | 0.09x |
| all 20 | 59/60 | 0.69x (total tokens incl. cache 0.90x) | | 0.08x |
Caveats: DeepSeek vs Sonnet (different models); lean baseline is 1 run/task; the "finishing" prompt section was written
after reading frozen-task traces (the unseen suite did not improve: 0.72x -> 0.73x), so treat the frozen gain as partly tuned.
Claude Code ran unsandboxed. System prompt revisions so far: 1 `]]` gotcha, 2 `edit` primitive, 3 sandbox/tmpdir, 4 finishing.

Latest rerun after the effects/log/replay layer (same prompt, sandbox on): 60/60 pass over the 20 tasks, fresh tokens 0.64x
lean Sonnet, cost 0.08x. Real-model crash demo: a run killed mid-edit resumed from its log, surfaced `unknown_outcome`,
finished and passed the grader.

Budgets: `[budget] max_cost / max_tokens / max_wall_secs` in `.harness/policy.toml` or the `--max-*` flags (flags win). A run that hits
one stops gracefully (`stopped: "budget_exceeded: ..."` in the report, a `budget` record in the log); subagents share the budget.
Tokens = input + cache writes + output. Cost needs a provider that reports it (OpenRouter, Enclave). `eval/run.py` passes
$0.30 / 600k tokens / 900s by default as a runaway guard.

## Same-model check (hx on Sonnet 5.5, medium reasoning, 1 run x 20 tasks) vs Claude Code on Sonnet
- hx+Sonnet: 20/20 pass (2 initial failures were a harness gap: Claude-style `<invoke>/<parameter>` markup was not extracted;
  fixed, reran, both pass). Fresh tokens 0.63x of lean Claude Code (paired median), i.e. the harness effect at a FIXED model
  matches the DeepSeek result (0.64x).
- Cost: median per task 0.61x of lean Claude Code, but TOTAL $1.65 vs $0.81 (2x): 6 of 20 tasks hit $0.21-0.24 because medium
  reasoning produced ~17k output tokens (Claude Code lean ran without extra reasoning). DeepSeek-hx total for the same 20: $0.07.
- Spent $1.70 of OpenRouter credit on this run.

## Decisions
- The `dev` pack (changeset edit gate, test adapters, git, tree-sitter symbols) was built, measured and purged. Full pack: 1.21x MORE
  fresh tokens than the plain harness (extra verification turns, git misuse errors); the light variant was neutral (1.05x). The
  benchmark tasks are small and well-tested, so plain `edit` + a test run is already efficient. If a larger suite ever shows value,
  start from git history rather than from this branch of ideas. The general pack mechanism (`--pack`, `hx.expose`) stays.
- Preferences if test adapters return: uv for Python, bun for JS, never npm.

## P4: open extension system (done except WASM)
All of it is opt-in or costs nothing when unused (no packages and no mcp.toml => the prompt is byte-for-byte unchanged).
- **Packages** (`.harness/pkgs/NAME/`, manifest `hx.toml`, `kind = "tool" | "skill"`): installed by the user, locked by hash and
  granted capabilities with `hx approve NAME`; anything unapproved or modified since approval is invisible to the model.
  Tools: `tools.NAME.fn(...)`. Skills: `skill("NAME")` -> `{ text, module }` (SKILL.md without frontmatter, plus runnable Luau).
  The prompt lists installed tools (signature + doc) and skills (description) only.
- **Agent-written tools** (`--pack std/authoring` adds the docs): `deftool(name, source, {doc, signature, caps, tests})` is usable at
  once but runs with ONLY the capabilities it declares (default none); `promote(name)` stages it under `.harness/staged/` after its
  tests pass in a fresh process; only the user's `hx accept NAME` (review, lock, grant) activates it. Nothing the agent can call approves.
- **Hot reload**: `refresh_extensions()` drops cached tool modules; session variables survive.
- **MCP** (stdio): servers in the user-owned `.harness/mcp.toml` run under their own Seatbelt profile (network and extra write
  roots per server, env scrubbed except what the user lists). Tools bind as `mcp.SERVER.tool{...}` with signatures generated
  from their JSON schemas. Tools matching `read_only_tools` run freely; every other tool is an *external* effect that needs
  approval (`allow = ["mcp server.tool"]` in policy.toml). Calls and `tools/list` are recorded effects: replayed on resume, never repeated.
- **Harness-module upgrades** (`hx upgrade propose|list|show|check|accept|rollback`; agent side: `propose_module`): a proposal must
  load, return a table and keep every export; `python3 eval/gate.py --project DIR --proposal ID --tasks ... --model ...` runs the
  current module, the candidate and an A/A control, and records a verdict bound to the candidate's sha256; `hx upgrade accept`
  needs a passing verdict for exactly that candidate (or an explicit `--force`, which is logged). Every accept/rollback is appended
  to `.harness/upgrades.jsonl`; rollback is one command. The A/A control matters: with 4 tasks x 3 runs the same module differed by
  1.34x in fresh tokens between two runs, so small gates can only catch large regressions. Use more tasks/runs for fine margins.
- **WASM plugins: dropped on purpose.** Luau packages already are the sandboxed, capability-limited, hash-locked plugin format. Native
  speed goes into the Rust core as a primitive (trusted, logged, replayable); third-party code is MCP; big binaries are `sh`. Measured
  cost of wasmtime (minimal): about +9 MB binary (11.8 -> ~20 MB), ~69 s clean build, ~114 crates.

Real-model checks: a DeepSeek run authored, tested and promoted `slugify` in 2 turns ($0.002); the user's `hx accept` made it
available to a new session. Regression eval after all of P4: 60/60, fresh tokens 1.01x of before.
