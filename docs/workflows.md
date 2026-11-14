# Workflows: RPI and autoresearch

hx ships a bundled Luau workflow on top of the plain agent loop, and a second program, `hx-ar`, built on hx and installed beside it.

| Workflow | Command | Use it when |
|---|---|---|
| RPI | `hx run --rpi` | A task benefits from separate research, design, implementation and verification, optionally with owner review gates |
| Autoresearch | `hx-ar` | A score must stay out of the agent's hands: a private evaluator, held-out data, or a metric an agent could game |

## RPI: research, design, implement, verify

`--rpi` carries a task through separate research-questions, research, system-design, program-design, implementation and verification stages. Each stage starts with a fresh agent session and receives the relevant immutable documents. Verification failures trigger repair and fresh verification, with at most three repair cycles across the run.

This is a bundled Luau workflow adapted from [HumanLayer RPI](../stdlib/std/rpi/PROVENANCE.md). Activate it with `--rpi`, not `--pack std/rpi`.

```bash
hx run --rpi --prompt-file task.md --model MODEL
hx run --rpi --manual --prompt-file task.md --model MODEL
```

> **Git required.** `--rpi` requires the workspace to be an initialized Git repository (with a `.git` root) and an appropriate `.gitignore`. The harness uses Git's inventory (`git ls-files --exclude-standard`) to track source modifications without being invalidated by build outputs (`build/`, `target/`, `*.o`). Unversioned directories, and projects the inventory cannot read (a file over 32 MiB, a non-UTF-8 path, a symlink leading outside), are rejected at launch with an explicit error.

### Automatic and manual mode

Automatic mode makes reasonable choices and proceeds. It pauses when essential information, an owner action or mandatory manual validation is missing.

Manual mode also pauses after research questions, research, system design and program design, and for final acceptance.

Review pauses return `finished:false` and the gate identity. A paused resume performs no model work until you record a valid decision:

```bash
hx review RUN --gate GATE_ID --decision approve
hx run --resume RUN
```

`RUN` is the run's name, its journal's file name without `.jsonl` (the report's `log` field is the journal's path; `hx paths` prints the directory), or a path to the journal.

| Decision | Use |
|---|---|
| `approve` | Accept the reviewed document |
| `revise --input-file feedback.md` | Revise a reviewed document |
| `answer --input-file answers.md` | Answer an input gate |
| `waive --input-file reason.md` | Validation input gates only: waive that gate's listed checks; the waiver stays distinct from passing checks |

Use the actions the reported gate allows. `hx review` records the decision; it does not execute the task. Final acceptance needs no model call. Review and acceptance reports include the frozen `check_plan`, and the program-design document ends with a generated "Frozen check plan" section: approve the plan verification will enforce, not only the prose.

If the project changes after you approve final acceptance but before `--resume`, the resume discards that approval and reports `decision_rejected`; the gate stays open, so record `revise` to repair and verify again. Revising at acceptance does not use up one of the three repair cycles.

### Documents, resume and permissions

Documents appear under `.agents/artifacts/rpi-RUN_ID/`. They are views of journal-backed artifacts; edit requests belong in review feedback. If they cannot be written, hx warns and the run continues.

Resume restores the frozen workflow mode, sources and budgets, so do not repeat `--rpi` or `--manual` with `--resume`. A branch chooses its workflow anew and does not inherit approvals. All stages share the run's limits; review downtime counts toward its deadline.

Research and design stages can inspect source and approved read-only web tools but cannot execute shell commands or change the project. Implementation, verification and repair use the run's ordinary permissions.

Verification checks exact planned commands, successful receipts and a bounded source inventory. Build outputs ignored by Git are covered only when a check lists them in `inputs`; a directory there covers every file below it. This does not establish a hermetic build or protect against every concurrent external writer. See [runtime details](runtime-v2.md#rpi-workflow).

## Autoresearch

`hx-ar` improves a score over the editable paths in `.harness/ar.toml`. Use it when the score must stay out of the agent's hands. When the agent can check its work from files in the workspace, a plain run with the same budget did as well in our tests.

```bash
hx-ar --prompt-file program.md --model MODEL
hx-ar --resume RUN                          # continue an interrupted search (RUN: its name, in `ar/` of the
                                            # `state` directory `hx paths` prints)
```

`hx-ar` is not part of the harness core: each episode is an ordinary `hx run`, and the agent there reaches `hx-ar` only through an MCP server that `hx-ar` serves on loopback for the run. `hx-ar` alone takes snapshots, runs the evaluators, judges claims and writes the run's journal.

### `.harness/ar.toml`

`.harness/ar.toml` names the editable paths, the evaluator commands and the read-only roots they need. Like the rest of `.harness/`, it is covered by `hx trust`, and `hx-ar` refuses to start until it is trusted. Each evaluator prints a JSON line with `score` (and optionally `valid`, `se`, `report`):

```toml
editable = ["ar/strategy.pine"]
eval = "$HOME/grader/eval search --file ar/strategy.pine"    # the agent sees these scores
select = "$HOME/grader/eval select --file ar/strategy.pine"  # held out: only the controller sees these
grader = ["~/grader/bin", "~/grader/search"]                 # read-only for both evaluators
holdout = ["~/grader/select"]                                # read-only for select only
min_delta = 0.05
```

`select` must reach the shift the result has to survive (other years, symbols or input ranges) and have enough cases to tell candidates apart. No `grader` or `holdout` root may overlap the workspace or hx's own configuration and state directories (`hx paths`; `~` itself contains them), since model-edited code runs in the evaluators that read those roots.

### How a search runs

1. **Freeze.** The committed tree is frozen at launch. Every episode is a fresh child `hx run` in a throwaway copy of it. `mcp.ar.eval{note = "hypothesis"}` asks `hx-ar` to score a snapshot of the editable files in another throwaway copy, with a scrubbed environment, confined by Landlock to its copy and the `grader` roots; network confinement must be supplied externally. Only the editable paths carry between episodes.
2. **Research.** A research episode writes a brief.
3. **Workers.** Eight workers (six after a provider rate limit) run side by side. Each derives its own idea from a host seed with [String Seed of Thought](https://pub.sakana.ai/ssot/) and claims it with `mcp.ar.claim`, first come, first served, so no two pursue the same core approach. `mcp.ar.claims{}` lists what is taken; a placeholder claim is refused. Workers implement and fix their idea, then return without tuning it; depth comes from rounds.
4. **Gate.** The three most promising candidates pass to the next rounds, even raw ones below the seed. Candidates that beat the seed on search and are not worse on `select` are audited for evaluator holes; only those can become results. With a noisy evaluator (one that reports `se`), a state's score is the mean of its evaluations, and both comparisons allow for the combined standard error.
5. **Rounds.** Refinement continues while it moves the chosen candidates; exploring starts again when it doesn't. Two rounds in a row without progress (a new candidate, or a gain above both `min_delta` and the evaluator's standard error) end the run.

Episodes are limited in model turns, not dollars. The result is a portfolio of distinct good candidates, best search score first.

The held-out score is only a veto. Ranking on it too would select on it every round until it is as optimistic as the search score.

See [Runtime v2](runtime-v2.md#autoresearch-workflow).
