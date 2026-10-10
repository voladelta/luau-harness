# hx status

What exists, how to verify it, and what is still missing. `PLAN.md` is the plan; this file is the ground truth about the code.

## Layout
- `src/` Rust core (L0): `vm.rs` (Luau VM, sessions, `require`, `hx` API), `prims.rs` (read/write/edit/sh/search),
  `effects.rs` (every effect live or replayed: write-ahead intents, recorded results, resume queues),
  `policy.rs` (sandbox, path policy, approval gate), `eventlog.rs` (hash-chained log), `modules.rs` (resolver, manifest,
  lockfile, grants), `llm.rs` / `host.rs` (model client, usage, subagents), `main.rs` (CLI).
- `stdlib/std/` the built-in `std` package (L2, Luau): `loop`, `prompt` (system prompt, the `luau` tool schema, nudges), `printer`, `feedback`, `hooks`.
  Embedded in the binary; override any module per project with `.harness/override/std/<mod>.luau`
  (`require("base:std/<mod>")` wraps the original). A broken override falls back to the built-in with a warning.
- `prompts/system.md` the system prompt (also an overridable asset).
- `eval/` runner, report, task generators; `tasks/` (15 frozen, hashed in `tasks/MANIFEST.sha256`), `tasks-unseen/` (5).

## CLI
`hx run --workspace D --prompt-file F --model M [--max-cost USD --max-tokens N --max-wall SECS] [--api anthropic|openai|responses --base-url U --key-env E --reasoning R] [--provider-only A,B] [--approval prompt]`
`hx run --workspace D --model M ... --resume .harness/log/<session>.jsonl` (continue a crashed run; the task is read from the log)
`hx exec --workspace D 'luau chunk' ...` (no API key) · `hx test PKG_DIR` · `hx approve PKG` · `hx log verify FILE`
The sandbox is ON by default; `--no-sandbox` is an explicit opt-out.

## Checks (all need `cargo build --release` first; none needs an API key)
| Script | Covers | Result |
|---|---|---|
| `hx test stdlib/std` | printer, mcp, hooks unit tests (Luau) | 17 pass |
| `cargo test --release` | Rust unit tests: mcp glob, path normalize, command classes, protected paths | 3 pass |
| `scripts/p2_checks.sh` | printer override without rebuild, broken-override fallback, parallel, vars, lock/grants gate, hooks, subagents, hot reload, failed lock check stays failed | 16/16 |
| `scripts/sandbox_checks.sh` | writes outside project, ../, symlinks, protected `.harness`, env scrubbing, network, process cap, timeout kills group | 35/35 |
| `scripts/p1_checks.sh` | hash-chained log, write-ahead intents, tamper detection, approval gate, crash => unknown outcome, log kept under --no-sandbox | 19/19 |
| `scripts/budget_checks.sh` | cost/token/wall budgets (flags and policy.toml), shared with subagents, graceful stop | 10/10 |
| `scripts/caps_checks.sh` | runtime package capabilities: only granted primitives exist, `getfenv`/`loadstring`/`_G`/`require` escapes closed | 5/5 |
| `scripts/llm_checks.sh` | wire formats (chat completions, responses, anthropic messages), cache markers/key per API (caching is always on), `--provider-only`, bad `--api` refused, native tool calls on all three wire formats: parallel calls answered one result each, invalid arguments and unknown tool names get an error result, prose before any code is nudged | 26/26 |
| `scripts/cache_stats.py LOG` | per-turn prompt-cache hit ratio from an event log (turn 1 ~0%, later turns high) | tool |
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

## Haiku 5.5 (high effort) on hx vs DeepSeek vs Claude Code (2026-10-10)
Claude Code runs on the subscription (`BASELINE_AUTH=subscription`; `--bare` cannot use it, it never reads OAuth). `subscription-lean`
adds `--system-prompt` + `--tools Bash,Read,Edit,Write` (~3.6k prompt tokens instead of ~23.7k). `EVAL_EFFORT=high` sets `--effort`.

| Frozen suite | Pass | Fresh tok/task | Turns | $/task | Wall |
|---|---|---|---|---|---|
| hx DeepSeek (3 runs x 15) | 45/45 | 5.8k | 3.8 | 0.0041 | 12s |
| hx Haiku high (3 runs x 15) | 44/45 | 9.1k | 3.3 | 0.0031 | 24s |
| Claude Code lean, Haiku high (1 run) | 15/15 | 21.9k | 11.9 | 0.0078 (API-equivalent) | 42s |
| Claude Code heavy, Haiku high (1 run) | 15/15 | 24.9k | 13.0 | 0.0093 (API-equivalent) | - |

Unseen (5 tasks, 1 run): hx Haiku 5/5 (6.2k), hx DeepSeek 5/5 (7.3k), Claude Code lean Haiku 5/5 (13.6k).
- Haiku medium was bad earlier; high passes everything, so Haiku needs high effort. DeepSeek stays the default (better pass rate, fewer tokens, 2x faster).
- The one Haiku failure (`shop-feature-bogo` #3) was a harness gap, not the model: it wrapped raw code in a bare `<invoke-luau>` tag (no fence, no
  `<parameter>`), the extractor found nothing, and the loop ended the run as "finished" with nothing applied. Fixed: `std/extract` accepts a
  bare invoke body, and `std/loop` nudges (max 2) on replies that still carry `<invoke` or a ```luau fence but extract no code, even after earlier code turns.
  Rerun of that task: 3/3 pass. Other suites green (`scripts/all_checks.sh`).
- NOT re-measured: DeepSeek on the frozen suite after this loop/extractor change (the 45/45 above predates it). Expected unaffected.

### Haiku medium and Sonnet low (2026-10-10, same day)
Frozen suite, 1 run unless noted. Haiku medium (3 runs x 15) passes as well as high and is cheaper; the earlier "bad" medium result was
harness gaps (unextracted markup), not the effort level.

| System | Pass | Fresh tok/task | Turns | $/task | Wall |
|---|---|---|---|---|---|
| hx Haiku medium (3 runs) | 44/45 | 7.6k | 3.2 | 0.0025 | 19s |
| hx Haiku medium (1 run) | 15/15 | 7.1k | 3.1 | 0.0022 | 17s |
| hx Sonnet low (1 run) | 15/15 | 4.7k | 3.4 | 0.0239 | 15s |
| Claude Code lean, Haiku medium | 15/15 | 13.4k | 10.8 | 0.0045 (API-equiv.) | 23s |
| Claude Code lean, Sonnet low | 15/15 | 5.7k | 4.5 | 0.0336 (API-equiv.) | 13s |

- hx saves ~0.5x fresh tokens on Haiku, but only ~0.83x on Sonnet low (Sonnet at low effort needs few turns either way).
- Second Haiku failure (`wiki-bug-bold` #3, medium): a garbled turn-2 reply (prose inside a half-formed `<parameter name="cmd">` tag, closing
  `</invoke>`, stray text). No extractable code, and the nudge only matched an opening `<invoke`, so the run ended "finished" with no edits.
  Fix: `std/loop` now also nudges on `</invoke`, `<parameter` and `</parameter`. Rerun of the task: 5/5. All suites green.
- Hugging Face router (Qwen3.8-27B on Cerebras) was tried and dropped: needs a strict request body (no cache markers / `usage`), got 13/15
  with 3.3x DeepSeek's fresh tokens and 16% Luau errors. That code was not kept.

## Native `luau(code)` tool (2026-10-10) - replaces fenced-block extraction
The model has ONE tool, `luau(code)`, declared on every request (chat completions `tools`, Responses `function` tools, Anthropic `tools`).
The loop runs each call in the persistent session and answers every call id with one tool result; a reply with no call is the final answer
(before any code has run it is nudged, max 2). Invalid or cut-off arguments and unknown tool names get an error result, never a crash.
`std/extract` and all the markup nudges are gone: a model's own tool-call format is parsed by the provider, not by us.
- The provider's raw assistant message (Anthropic content blocks incl. thinking, Chat Completions message incl. `reasoning`, Responses
  output items) is kept on the neutral message and echoed back verbatim; the replay log's reply blob stores `{text, calls, raw}`.
- No forced `tool_choice` (newer Claude models 400 on it): `auto` plus the prompt. The Anthropic Messages path is only tested against
  the mock (no Anthropic API key here); chat completions was verified live on Enclave and OpenRouter.
- `scripts/mock_llm.py` turns a scripted ```luau reply into a native call (objects `{text, calls:[{code}|{name, arguments}]}` for parallel/bad calls).

Frozen suite x3 (45 runs each), fresh tokens per task / turns / $ per task, native tool vs the old extraction:

| Model | Pass | Fresh tok | Turns | $/task |
|---|---|---|---|---|
| hx DeepSeek, native | 45/45 | 6.4k | 4.5 | 0.0041 |
| hx DeepSeek, extraction | 45/45 | 5.8k | 3.8 | 0.0041 |
| hx Haiku 5.5 medium, native | 45/45 | 6.4k | 3.5 | 0.0019 |
| hx Haiku 5.5 medium, extraction | 44/45 | 7.6k | 3.2 | 0.0025 |

Unseen (5, 1 run): DeepSeek 5/5 (6.1k), Haiku medium 5/5 (5.1k). Luau errors per code turn 6-7%, truncated turns 0 (extraction: 8 on Haiku).
DeepSeek's +10% tokens on frozen is inside the ~1.34x noise between identical runs. Haiku medium is now the cheapest setup ($0.0019/task).
Both Haiku extraction failures above (bare `<invoke-luau>`, garbled `<parameter name="cmd">`) cannot occur any more.

## Decisions
- The `dev` pack (changeset edit gate, test adapters, git, tree-sitter symbols) was built, measured and purged. Full pack: 1.21x MORE
  fresh tokens than the plain harness (extra verification turns, git misuse errors); the light variant was neutral (1.05x). The
  benchmark tasks are small and well-tested, so plain `edit` + a test run is already efficient. If a larger suite ever shows value,
  start from git history rather than from this branch of ideas. The general pack mechanism (`--pack`, `hx.expose`) stays.
- Extraction was purged in favour of the native tool (see above); do not reintroduce a text-protocol fallback for models without function calling unless one is actually needed.
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
