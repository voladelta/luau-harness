# Luau Harness Plan

Oct 10, 2026 · @Vola

## Bottom line

**Small trusted core, everything else grows.** We build a Rust binary that hosts a persistent Luau VM. The model works by writing code against typed tools. The Rust core holds only what must be trusted: sandbox, policy, approvals, event log, model client. Everything else is Luau or a plugin, including the agent loop itself, so users and the agent can reshape the harness without a rebuild.

The bet: on long multi-file tasks we match Claude Code and Codex on pass rate at **60% or less of their tokens per solved task**, same model. Developer work ships first. Sysadmin and marketing ops ship later as packs built on the same extension system.

The load-bearing assumption is that models write good enough Luau to batch work. Phase 0 tests that within two weeks and has a kill gate. If it fails, we change the language or the prompt before building anything else.

Status: plan v2, revised after a pressure test (see the last two sections). No code exists yet. Nothing here is implemented or measured.

## Contract

**Outcome.** A headless agent harness (CLI plus a JSON event stream) where the model acts by running Luau in a persistent VM. It serves developers, sysadmins and marketing ops. It is extensible at four levels without rebuilding the binary.

**Acceptance criteria.** Each one is checked by the eval harness or a scripted scenario, never by inspection alone.

| ID | Criterion | How it is checked |
| --- | --- | --- |
| AC1 | Dev: pass rate at or above baseline, same model. Median tokens per solved task at most 0.6x baseline on multi-file tasks, at most 0.9x across the whole suite | Dev suite vs `claude -p` and `codex exec` |
| AC2 | Sysadmin: pass rate at or above baseline, zero policy violations | Ops suite in disposable containers, plus a sandbox-escape suite |
| AC3 | Marketing ops: data from two or more sources to a traceable analysis, an article draft and a rendered short video, end to end | Marketing suite with fixture data and mock APIs |
| AC4 | A user adds a skill, a Luau tool package, an MCP server and a WASM plugin with no rebuild. An agent-written tool is tested and promoted after approval. Reload keeps VM state | Scripted extension scenario |
| AC5 | Harness modules (loop, printing, compaction, edit gate) are overridable per project. Agent-proposed changes pass an eval gate and approval before activation, with one-command rollback | Scripted self-upgrade scenario |
| AC6 | After a kill during a task, resume rebuilds VM state and does not repeat any side effect | Crash test that kills the process at every tool-call boundary |

**Invariants.**

- No tool reaches files, network, processes or secrets outside the capabilities it was granted. `sh()` always runs inside an OS sandbox.
- Replay never re-executes a side effect. It returns logged results.
- External or irreversible effects (deploy, send, publish, push, delete outside the workspace) need explicit user approval. Nothing the agent writes can approve on its own behalf.
- Extensions cannot widen their own capabilities. Only the user grants capabilities.
- The trusted core (policy, approval gate, sandbox, event log) is not self-modifiable.

**Exclusions for v1.** IDE plugins and GUI (a minimal TUI is allowed), multi-user server, hosted cloud, model training, Windows sandboxing.

## Architecture

The Rust core is the only part that must be trusted, so it stays small. Everything above it is Luau source or a sandboxed plugin, and any of it can be replaced per project.

&#91;embedded content: harness layers · L0 trusted, L1 to L4 replaceable\]

The model only ever touches L1 to L4 through Luau. Every call down into L0 passes the capability check and lands in the event log.

**Key decisions.**

- **Embedding:** `mlua` with the `luau`, `async`, `serialize` and `send` features. Luau `sandbox(true)` after bindings load, `set_interrupt` for time budgets, `set_memory_limit` for memory.
- **The loop lives in Luau (L2).** Prompt assembly, output printing, compaction, the edit gate and model routing are Luau modules with hooks. Upgrading the harness means editing these, not the binary.
- **Hooks can deny or transform, never approve.** `before_effect` hooks may block an action. Approval is only granted by the core gate after a user decision.
- **Primitives are few and native:** `fs`, `proc`, `net`, `search`, `regex`, `json`, `hash`, `time`, `llm`. Every tool the model sees is Luau built on these, so capability checks happen in one place.
- **Search is ripgrep's engine, embedded.** The `search` primitive uses the `grep` and `ignore` crates directly: no `rg` binary, no subprocess, no output parsing. It respects `.gitignore` and skips binaries by default, which keeps `node_modules`-style noise out of the context. The walker starts only from granted roots and never follows symlinks out of them. Hits stop at a cap. No PCRE2: lookarounds return a typed `unsupported_regex` error with a hint. Not a WASM plugin, because search is a hot path that needs native threads and file access.
- **Plugins:** WASM components (wasmtime) for native-speed code, MCP for external services, subprocess for big tools like ffmpeg. No Rust dylibs: the ABI is unstable and they run unsandboxed.
- **Analysis engine:** DuckDB behind `sql()`. Models write SQL far better than any custom dataframe API. It ships in the full build only; the slim build omits it.
- **Model client:** provider-agnostic (Anthropic, OpenAI, OpenAI-compatible local). Needed for fair baselines and for cheap sub-model calls.

## Extensibility

Everything that extends the harness is a **package**: one format with a manifest, declared capabilities, types and tests. The standard library is itself a package, so the extension system is used from day one rather than bolted on.

| Kind | What it is | Runs as | Typical author |
| --- | --- | --- | --- |
| Skill | `SKILL.md` plus optional Luau, tests and examples. An executable recipe, not just instructions | Luau in the VM | User or agent |
| Tool package | Luau functions over primitives, with `.d.luau` types | Luau in the VM | User or agent |
| MCP server | External service; its tools are auto-bound as Luau functions with types generated from JSON schemas | Separate process | Third party |
| WASM plugin | Native-speed code (parsers, codecs, tree-sitter grammars) | wasmtime, capability-scoped | Developer |
| Harness module | The loop, printing, compaction, edit gate, routing | Luau, L2 | User or agent, behind the eval gate |

**Manifest sketch.**

```toml
[package]
name = "mktg.ga4"
version = "0.1.0"

[capabilities]          # requested; granted by the user at install
net = ["analyticsdata.googleapis.com"]
secrets = ["GA4_TOKEN"]
fs.write = ["$WORKSPACE/data/**"]

[exports]
module = "init.luau"
types = "ga4.d.luau"

[tests]
files = ["tests/*.luau"]
```

Installed packages are pinned in a lockfile with content hashes. A package whose hash changes needs re-approval of its capabilities.

**Agent-built tools.** The agent writes `deftool("fix_import", fn, {types = ...})` in a session and uses it right away. `promote("fix_import")` writes it as a package under `.harness/pkgs/`, generates types, and runs its tests. It becomes available in later sessions only after the user approves.

**Upgrading the harness itself.** The agent may propose a diff to an L2 harness module, for example a better truncation policy. The flow:

1. The diff is staged, not active.
2. The smoke eval suite runs with the current module and with the staged one.
3. The user sees the diff and the metric delta, then approves or rejects.
4. On approval, the module is versioned and activated. `hx harness rollback` restores the previous version.

**Hot reload.** Reloading a package swaps its module table in the running VM. User variables survive. Closures that captured old functions keep the old code until they are rebuilt; this is documented, not hidden.

**Out of reach for self-modification:** the policy engine, the approval gate, the sandbox and the event log. Those live in Rust and change only with a new release.

## Persona packs

Each persona is a pack: a bundle of packages, skills and a default policy. All three share the same core, so a win in one pack (better printing, cheaper compaction) helps the others.

### Developer (ships first)

- **Tools:** `read`, `edit`, `changeset` with `:diff`, `:commit` and `:rollback`, `grep` (ripgrep), `symbols` and `refs` (tree-sitter), `sh`, `git`, test adapters that turn cargo, npm and pytest output into tables.
- **Edge over bash agents:** the edit gate. Every commit compares LSP diagnostics before and after, and rejects edits that introduce new errors. `try{plan_a, plan_b}` runs approaches in separate git worktrees and keeps the one that passes tests with the smallest diff.
- **Default policy:** writes inside the repo, network to package registries, no push without approval.

### Sysadmin

- **Tools:** `host("web-1")` handles that run commands over SSH under a per-host policy. Parsers for `ps`, `df`, `systemctl`, and JSON output from `journalctl`, `docker` and `kubectl`. An inventory kept as a VM variable.
- **Edge:** plan and apply. Mutating calls in ops mode are recorded into a change plan instead of running. The user approves the plan once, then it applies with per-step results and stops at the first failure. This replaces dozens of per-command prompts, which cause approval fatigue.
- **Safety:** secrets are redacted by the print layer before anything reaches the model. Read-only commands run freely; mutations go through the plan.

### Marketing ops

- **Data:** MCP connectors (analytics, ads, CRM, sheets) plus `http` and `csv`. Everything lands in DuckDB, and the model analyzes with `sql()`.
- **Traceable numbers:** every figure in an output carries the SQL query and source rows it came from. `cite(q)` inserts a reference, so an article's claims can be checked.
- **Writing:** `llm` with a style profile; outputs to Markdown, with `.docx` via pandoc.
- **Charts:** Vega-Lite specs rendered to SVG and PNG by a plugin.
- **Video:** `video.compose{clips, captions, music, voice}` builds an ffmpeg pipeline in the sandbox. TTS and image generation go through MCP or HTTP providers the user configures.
- **Safety:** publishing, sending and ad-spend changes always need approval.

## Phases

Seven phases, each proven by its own checks before the next starts. The eval harness starts in P0 and grows with every phase, so each claim is measured, not assumed.

1. **P0, spike and kill gate.** Minimal Rust loop with `mlua` and Luau; `read`, `write`, `sh`, `grep`; smart printing; 15 dev tasks.
   - Check: same model against `claude -p` on those tasks. Measure Luau error rate per turn, turns, tokens per solved task.
   - Gate: tokens per solved task at most 0.8x baseline and Luau errors in under 15% of turns. Miss either and we revise the prompt, examples or language before P1.
2. **P1, trusted core.** Policy engine, capability grants, approval gate, OS sandbox (Landlock or bubblewrap on Linux, Seatbelt on macOS), event log with intent and outcome records, replay and resume, budgets, provider-agnostic model client.
   - Checks: sandbox-escape suite (writes outside grants, network to ungranted hosts, fork bombs, symlink tricks); crash test killing the process at every tool-call boundary (AC6); budget tests for time, memory and infinite loops.
3. **P2, loop in Luau and packages.** Move the loop into L2 modules with hooks. Package format, manifest, capabilities, lockfile. The standard library becomes the first package. Add `vars()`, `agent()` subagents and `parallel{}`.
   - Checks: override the printing module in one project without a rebuild; eval parity with the P0 loop (no regression over 2 points of pass rate).
4. **P3, developer pack.** Changeset with edit gate, tree-sitter symbols, LSP diagnostics, test adapters, git, `try{}` worktree branching.
   - Check: full dev suite against both baselines. This is AC1.
5. **P4, open extension system.** MCP auto-binding with generated types, WASM plugins, executable skills, `deftool` and `promote`, hot reload, harness-module upgrade with eval gate and rollback.
   - Checks: the AC4 and AC5 scenario scripts.
6. **P5, sysadmin pack.** Host handles, parsers, plan and apply, secret redaction.
   - Checks: ops suite in disposable containers and VMs. This is AC2.
7. **P6, marketing pack.** DuckDB `sql()`, connectors, `cite`, charts, docs output, `video.compose`.
   - Check: marketing suite end to end. This is AC3.

P5 and P6 depend only on P4 and can run in parallel with separate owners. Everything earlier is one line of work.

## Evals and baselines

We win only if the same model, on the same tasks, under the same limits, solves as much for less. Every comparison runs our harness against `claude -p` and `codex exec` in headless mode.

| Suite | Size | Source | Environment |
| --- | --- | --- | --- |
| Dev | 40 own tasks plus a Terminal-Bench subset | Multi-file refactors, bug hunts, feature adds in real repos | Pinned repo snapshots |
| Ops | 20 tasks | Service config, log triage, disk and process issues, rollouts | Disposable containers and VMs |
| Marketing | 15 tasks | Fetch, analyze, write, chart, render video | Fixture data, mock APIs |
| Smoke | 10 tasks drawn from the above | Fast regression check | Used by the harness-upgrade gate |

**Metrics, all per task:** pass or fail, total tokens, model turns, wall-clock time, cost, and policy violations. The headline number is **pass rate at equal cost**, so no harness wins by simply spending more.

**Rules.** Same model and version for both sides. Three runs per task, report the median. Graders are scripts (tests pass, file state matches, numbers match source), with a model grader only for article quality, and that one is labeled. SWE-bench Verified is a sanity check, not the target; it is close to saturated and does not reward batching.

**Known handicap.** Each vendor's models are tuned on their own harness, so our early scores are expected to trail. Tool names and argument shapes copy what models already know (`read(path, {offset, limit})`, `edit(path, old, new)`, `sh(cmd)`). We innovate in what tools return and how they compose.

## Failure and recovery

The event log is the source of truth. VM state is rebuilt from it, and no side effect is ever repeated on replay.

**Effect classes.** Each primitive declares one, and the class decides how replay and approval treat it.

| Class | Examples | Approval | On replay |
| --- | --- | --- | --- |
| pure | `json`, `regex`, `hash` | none | re-executed |
| read | `read`, `grep`, `http GET` | none | logged result returned |
| local\_write | `write`, `commit`, `sh` in the workspace | by policy | logged result returned |
| external | `send`, `publish`, `host:apply` | always | never re-executed |
| irreversible | `push`, delete outside workspace, ad spend | always, named | never re-executed |

**Write-ahead rule.** Before any `external` or `irreversible` effect, the core writes an *intent* record and flushes it. After the effect it writes an *outcome* record. On resume, an intent with no outcome is an **unknown outcome**. It is shown to the user and never retried automatically.

**Worked failures.**

- **Crash after a local write, before its log record.** The file is on disk but the log does not show it. Resume replays to the last logged call. The next `edit` on that file fails its hash check, and the model re-reads the file. Nothing is lost or duplicated.
- **Crash during `host:apply` step 3 of 5.** Steps 1 and 2 have outcomes. Step 3 has only an intent. Resume marks step 3 unknown, stops, and shows the user steps 3 to 5 with a check command for step 3. The user decides.
- **Model times out mid-turn.** The chunk is cancelled at the next interrupt. Effects that completed are logged; the rest never started. The model receives the partial result and the timeout error.
- **Plugin crashes.** A WASM trap or an MCP disconnect becomes a typed Luau error `{kind = "plugin_failed"}`. The VM survives.

**Determinism.** Time, randomness and environment reads go through primitives and are logged, so replay sees the same values. Guarantee level: specified, `UNPROVEN` until the P1 crash test passes.

## Pressure test and revisions

Version 1 of this plan was reviewed against its own load-bearing assumptions. Seven changes made it into version 2.

| # | v1 | Problem found | v2 |
| --- | --- | --- | --- |
| 1 | Build all core, then measure | The whole plan rests on models writing Luau well enough to batch. Untested, it could sink months of work | P0 spike with a numeric kill gate before anything else |
| 2 | 0.6x tokens on the whole dev suite | Counterexample: debugging tasks need the model to read each result before deciding the next step, so batching saves little there | 0.6x on multi-file tasks, 0.9x across the suite, reported per category |
| 3 | Extension system in P4, packs before it | Packs built before the format exists get rewritten later, and the format goes untested | Package format in P2; the standard library is the first package |
| 4 | Custom dataframe API for marketing | Models have seen almost no examples of a new API; SQL is everywhere in training data | DuckDB behind `sql()` |
| 5 | Native plugins as Rust dylibs | Unstable ABI, no sandbox, crashes take down the VM | WASM components, MCP, subprocess |
| 6 | Agent may edit any harness module | The agent could weaken its own guardrails | Policy, approval gate, sandbox and log stay in Rust and out of reach |
| 7 | Per-command approval for ops | Dozens of prompts per task lead users to approve blindly | Plan and apply: one approval per change plan |

Not independently reviewed. This pass was a self-review by the author of the plan; a fresh reviewer should go over P1 and the failure model before implementation starts.

## Open questions and risks

None of the open questions blocks P0. Each has a working assumption so work can start.

- [ ] **Model providers for v1?** Assumed: Anthropic, OpenAI and OpenAI-compatible local endpoints.
- [ ] **Distribution?** Assumed: a slim build (no DuckDB, no tree-sitter grammars) and a full build. Grammars could also load as WASM plugins to keep one build.
- [ ] **Open source or not?** Undecided. It decides whether a public package registry makes sense and how much plugin supply-chain work P4 needs.
- [ ] **Who supplies TTS and image generation for video?** Assumed: user-configured providers through MCP or HTTP; we ship none.
- [ ] **Test infrastructure for the ops suite.** Needs disposable VMs, not only containers, for systemd and kernel-level tasks. Needed before P5.

| Risk | Effect | Mitigation |
| --- | --- | --- |
| Luau fluency is worse than expected | Errors eat the token savings | P0 kill gate; cheat sheet and worked examples in the prompt; switch to Starlark or QuickJS if the gate fails twice |
| Models don't batch even when they can | Code mode becomes one-call-per-turn bash with extra steps | Worked examples that batch; measure calls per turn in P0 |
| Co-training gap vs vendor harnesses | Early scores trail | Familiar tool names and shapes; compare per category, not only overall |
| Three personas spread the team thin | Nothing ships well | Dev pack first; ops and marketing only after P4 |
| Plugin supply chain | A malicious package exfiltrates data | Capability grants, hashes in the lockfile, re-approval on change |
| Approval fatigue | Users click yes without reading | Plan and apply; effect classes so reads never prompt |
| Replay drift from unlogged nondeterminism | Resume rebuilds the wrong state | All time, randomness and environment reads through logged primitives; crash test in P1 |

**Next step:** P0. It needs a Rust workspace, a 15-task dev set and API access for one model shared by both sides.
