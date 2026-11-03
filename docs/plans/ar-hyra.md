# Plan: four autoresearch changes from Hyra 2.0

Status: planned, not started. Written against `main` at `deff054`. This file is the whole task record: a fresh session
needs it, the files it links, and nothing from the conversation that produced it.

## Activity

- **Next activity:** implement, phase by phase, in the order below. Starting it needs the owner's go in the new session;
  this file does not authorize commits or pushes by itself.
- **Source:** the owner asked for a sound, elegant design for Hyra 2.0 ideas
  (<https://hy.tencent.com/research/hyra2>, published 2026-11-01) applied to `hx run --ar`, then for this plan.

## Background: what Hyra 2.0 showed that applies here

1. **Same budget before judging.** Hyra's "spiral-staircase" robot hand was 11% *slower* than the best published design
   before tuning, and 28.6% faster after both got the same policy fine-tuning budget. The best raw design was not the
   best design after tuning.
2. **Independent review sees evidence first.** Hyra starts a fresh review worker that first sees only the original
   problem and the evidence (code version, results, logs), not the executor's conclusions.
3. **Experience is kept with its evidence and applicability.** Their shared knowledge base grades entries as
   observation (seen once), verified (independently reproduced) and convention (human-confirmed).
4. **When a problem stays unsolved, name the bottleneck** (model ability, search efficiency, or verification and
   feedback speed) and make it the next target.

## Contract

### Outcomes

1. **Probation.** Every explore round is two batches: the explore batch `round{n}`, then a probation batch `round{n}p`
   that gives one exploit worker to each candidate claimed in round `n`. The gate for round `n` runs once, after both
   batches. The gate therefore never ranks an idea that nobody refined.
2. **Evidence-only audits.** Auditor tasks contain no `<findings>` block. They contain the candidate's ledger table.
3. **Stall diagnosis.** `report.json` gains `diagnosis: {counts, likely}`, and the orchestrator prints one stderr line
   `hx ar: diagnosis: <likely> (...)`.
4. **Digest of earlier runs.** At launch, earlier finished runs with an identical spec are summarized into
   `launch.json` field `prior`. Only the research episode sees it, as a `<prior_runs>` block.

### Acceptance criteria

Each is checked by the check named in its phase.

- A1. In every run, each `planned` journal record for batch `round{n}` with role `explore` is followed, before the
  `gate` record of round `n`, by a `planned` record for batch `round{n}p`. That batch has one `exploit` plan per probation
  candidate of round `n` (defined in Phase 2), capped at the round's width, and no other plans.
- A2. After an explore round that comes after an exploit round (a re-explore), the new round's candidates have exploit
  plans in `round{n}p`. This fixes today's behavior, where a run can stop right after a re-explore and those ideas get no
  refinement.
- A3. Every `episodes/a*.md` (auditor task) contains `<ledger>` and does not contain `<findings>`. Explore and exploit
  tasks keep `<findings>` as today.
- A4. `report.json` has `diagnosis.likely`, one of `harness`, `validity`, `holes`, `noise`, `generalization`,
  `unclaimed`, `search`, chosen by the rules in Phase 3. `diagnosis.counts` holds every count those rules read.
- A5. A second run in a workspace with a finished first run, and with an identical spec, has a non-empty
  `launch.json["prior"]`. Its `episodes/r0-research.md` contains `<prior_runs>` with exactly that text. A run whose spec
  differs in any field gets `prior == ""` and no `<prior_runs>` block.
- A6. The `prior` text contains no held-out (`select`) score and does not say which candidates the held-out veto
  rejected.
- A7. `cargo test --release --quiet` and `python3 scripts/ar_checks.py` pass. `python3 scripts/all_checks.py` passes
  before the last commit.

### Invariants

These must stay true. Existing checks cover most of them; don't weaken those checks.

- The held-out score is only a veto. No agent-visible text (tasks, briefs, digest) carries a `select` score or whether
  a candidate was vetoed.
- A resumed run replays from its journal and journals nothing new for finished work. `scenario_00`'s replay check
  must still pass.
- The run terminates: the stop rule ("two rounds in a row without progress") is unchanged and still guarantees an end.
- Episodes are limited by `TURNS`, not dollars. No new `ar.toml` key and no new CLI flag. `Spec` keeps
  `deny_unknown_fields`.
- Runs that started before this change can still be resumed: new `Launch` fields use `#[serde(default)]`.
- Only snapshots of editable paths leave an episode. The run directory and earlier runs stay sealed from agents
  (`sealed()` in `src/ar.rs` already seals all of `.harness/ar`).

### Exclusions

These were decided against. Do not build them in this task.

- A separate model for auditors (a `--audit-model` flag or similar).
- Ranking candidates against the leaders' scores at the same refinement depth. One probation refinement is the chosen
  level of fairness.
- Journaling claim rejections. The `unclaimed` diagnosis uses an existing proxy instead.
- A separate cross-run knowledge store (Hyra's "Books"). The digest is derived from earlier run directories and stored
  nowhere else.
- Hyra's persistent task queue, wake-ups, three agent tiers and Research Board.

## Map of the code you will touch

- `src/ar.rs`: the orchestrator. Key places, by function (line numbers are as of `deff054`):
  - `Orchestrator::run` (~657-760): closes unfinished episodes, then runs the round loop. Each round plans batch
    `round{n}` (explore from the seed when `explore` is true, otherwise exploit split across the last gate's `chosen`
    as `chosen[w % chosen.len()]`), runs it, then calls `gate(round)`. Progress, `moved`, `quiet` and `explore` are
    computed after the gate. The function then writes `report.json`.
  - `Orchestrator::task` (~474-558): builds every episode's prompt. `context` gets `<brief>` and `<findings>` for every
    non-research role (~479-482). The exploit arm builds a `<ledger>` table inline (~530-536).
  - `Orchestrator::gate` / `candidates` (~562-635): ranking, audits, `Record::Gate`.
  - `Orchestrator::state` (~428-451): findings come from every finished episode whose label starts with `r` and not
    `r0`.
  - Resume (~664-681): research and audit episodes (labels `r0-*` and `a*`) count as crashed when interrupted.
  - `orchestrate` (~811-847): launch builds the `Launch` and writes `launch.json`. Resume reads it.
  - `Launch`, `Plan`, `Candidate`, `Record`, `Row`, `Claim` structs (~148-260).
- `src/ar/episode.rs`: `ar.eval`, `ar.claim` and the claim judge. Not expected to change.
- `src/ar/files.rs`: snapshots, copies, the evaluator. The digest reader can live here or in a new `src/ar/prior.rs`.
- `scripts/ar_checks.py`: offline integration checks. The real CLI runs against a scripted mock provider (`scripted`,
  `climbing` and others) and a toy task: `x.txt` holds an integer, search scores `-(x-7)^2`, held-out scores `-(x-5)^2`.
  Scenarios run four at a time.
- Docs that describe this behavior and must change with it: `docs/runtime-v2.md` section "Autoresearch workflow",
  `docs/workflows.md` section "Autoresearch" ("How a search runs"), and the module comment at the top of `src/ar.rs`.

**Build and check commands** (from the repository root):

```bash
cargo build --release                 # ar_checks.py runs target/release/hx through scripts/hx_trusted
cargo test --release --quiet
python3 scripts/ar_checks.py          # about 2-4 minutes
python3 scripts/all_checks.py         # every offline suite, about 5 minutes
```

## Phases

Each phase is one verifiable unit: code, docs and checks land together in one commit. Do them in this order. Phases
1-3 don't depend on each other. Phase 4 reads the probation batches that Phase 2 creates.

### Phase 1: evidence-only audits

**Outcome:** A3.

**Change:**
- In `task()`, choose inputs by role instead of giving `<findings>` to every non-research role:

  | Role | brief | findings | ledger of its candidate |
  |---|---|---|---|
  | Research | no | no | no |
  | Explore | yes | yes | no |
  | Exploit | yes | yes | yes |
  | Audit | yes | **no** | **yes** |

  Factor the inline exploit ledger into `fn ledger_table(rows: &[Row], idea: u32) -> String` (same format, same
  `SHOWN` cap) and use it for exploit and audit. Auditors keep the claim text and `<starting_state>`.
- Docs: in `docs/runtime-v2.md` "Episodes", say that auditors get the brief and the candidate's ledger, not the
  findings, so their verdict isn't anchored on the workers' own account.

**Checks (owner: implementer):**
- In `scenario_00` of `scripts/ar_checks.py`, assert that every `run/episodes/a*.md` contains `<ledger>` and not
  `<findings>`, and that `run/episodes/r2-w1.md` contains `<findings>`.
- The scripted auditor fails the candidate whose claim is `go(3)` by matching `({go(3)})` in the task text. That must
  keep working, so the claim must stay in the auditor's task.
- `cargo test --release --quiet`, then `python3 scripts/ar_checks.py`.

### Phase 2: probation batch

**Outcome:** A1, A2.

**Definitions:**
- *Probation candidates of round n:* the explore claims (`scope == "explore"`) whose `worker` label belongs to round `n`,
  meaning the label up to its first `-` is `r{n}` (retries like `r3-w2.2` count). Of those, keep the ones that have a
  valid improved ledger row (`r.idea == claim.n && r.valid && r.improved`) and whose best such score differs from the
  seed's search score. A candidate scoring exactly the seed is the seed in other bytes, which the gate already drops.
  Rank by best score using `spec.better`, best first, and take at most `self.width()?`.
- *Probation plan:* `Plan { label: "r{n}p-w{k}" (k from 1), role: Exploit, idea: claim.n, start: <snap of its best
  row>, best: <its best score>, seed: None }`.

**Change:**
- Add a pure function, so it can be unit-tested without a provider:
  `fn probation(spec: &Spec, rows: &[Row], claims: &[Claim], round: usize, seed_search: Option<f64>, width: usize) -> Vec<Plan>`.
  It reads only the ledger and claims. It must **not** call `candidates()` or `select()`: that would score
  pre-probation states on held-out data, an extra look at the holdout for nothing.
- In `run`'s round loop: when this round's batch is an explore batch and no gate is journaled yet for this round, then
  after `run_batch("round{n}")`, plan batch `round{n}p` with `probation(...)` and run it. Plan it **even when it is
  empty**, so a resumed run sees the decision. Then call `gate(round)`. Reuse the batch if `planned("round{n}p")`
  already exists (resume). Compute `seed_search` with `self.known(&rows, &seed).0`.
- Leave alone: `explore`, `moved`, `quiet`, progress and the stop rule. The loop's `plans` variable must still hold the
  `round{n}` plans, so that `explore = !moved && plans.first().role == Exploit` behaves as today.
- These already handle `r{n}p-*` labels correctly; don't change them, but re-read them to confirm:
  - findings in `state()` include them (label starts with `r`, not `r0`);
  - resume treats them as workers, not answer-only (not `r0-*`, not `a*`);
  - the explore `taken` filter in `task()` reads only explore-scope claims (probation claims are scope `c{n}`).
- Docs: in "How a search runs" (`docs/workflows.md`) and "Manager loop" (`docs/runtime-v2.md`), describe the probation
  batch as part of an explore round, why it exists (raw scores of unrefined ideas don't predict refined ones), and its
  cost: up to `2 × width` episodes per explore round. Update the module comment in `src/ar.rs`.

**Checks (owner: implementer):**
- Rust unit tests for `probation()`, in `src/ar.rs` `mod tests`:
  - claims from rounds 1 and 2: only the asked round's claims are used;
  - a claim with only invalid rows, or with no improved row, gets no plan;
  - a claim whose best equals the seed's score gets no plan;
  - more candidates than `width`: the best `width` by `spec.better`, best first, labels `r{n}p-w1..`;
  - for `direction = "min"`, the lowest score ranks first;
  - a retried worker label (`r1-w3.2`) counts as round 1.
- `scripts/ar_checks.py`:
  - `scenario_00` asserts the exact sequence of batch roles (`roles == {"round1": "explore", "round2": "exploit", ...}`)
    and the gate numbering. Those expectations will change. Rewrite them to the new sequence: run the scenario, read
    the journal, and write down the sequence you observe **only after checking that each difference comes from
    probation**. Do not delete an assertion; replace it with its equivalent under the new schedule. Explain the new
    sequence in the comment above it.
  - Add the A1 invariant as a helper that runs over the journal of every scenario run that reaches a gate (at least
    `scenario_00`, `scenario_01` and `scenario_fallback`).
  - A2: in `scenario_00`, the re-explore round (round 4 today) must have a `round{n}p` batch whose plan ideas equal
    that round's probation candidates. The toy makes them valid and below the seed (x = 40 + w).
  - The replay check in `scenario_00` (resume of a finished run journals nothing) must pass unchanged.
- Fix every other scenario whose expectations move because of the extra batch, under the same rule: equivalent
  assertion, never a deleted one.
- `cargo test --release --quiet`, then `python3 scripts/ar_checks.py`.

### Phase 3: stall diagnosis

**Outcome:** A4.

**Change:**
- Add a pure function: `fn diagnose(spec: &Spec, records: &[Record], rows: &[Row], claims: &[Claim], seed_search: Option<f64>) -> Value`.
  It returns `{"counts": {...}, "likely": "<label>"}`. It is called in `run` when `report.json` is built, and its line
  is printed to stderr before the report is written.
- Counts. Every count comes from the journal, ledger and claims; nothing comes from model text:
  - `episodes`: `finished` records. `crashed`: those with `crashed: true`. `rate_limited`: those with `rate_limits > 0`.
  - `evals`: ledger rows with `idea != 0` (worker evals; probes excluded). `invalid`: those with `valid == false`.
  - `audits_failed`: `audited` records with `pass == false`.
  - `vetoed`: candidates in the last `gate` record that beat the seed on search by more than `min_delta` and are not
    `good`.
  - `noisy`: candidates in the last gate's `chosen` whose best snap has a ledger row with `se >= min_delta` (only when
    `se` is reported).
  - `workers`: finished episodes whose plan role is explore or exploit (labels `r{n}-*` and `r{n}p-*`, not `r0-*`).
    `unclaimed`: those whose label (and every retry of the plan) has no row in `claims`.
- `likely` is the first rule that holds, checked in this order:

  | likely | Holds when | Message suggests |
  |---|---|---|
  | `harness` | `crashed + rate_limited > episodes / 2` | provider or configuration |
  | `validity` | `evals > 0 && invalid > evals / 2` | program rules unclear or evaluator brittle |
  | `holes` | `audits_failed > 0` | fix the evaluator before searching more |
  | `noise` | `noisy > 0` | the evaluator can't resolve `min_delta`: more cases or a lower `min_delta` |
  | `generalization` | `vetoed > 0` | the search set misses the shift the result must survive |
  | `unclaimed` | `workers > 0 && unclaimed > workers / 2` | ideas exhausted, or the claim judge is too strict |
  | `search` | otherwise | ideas are valid but weak |

  Use integer comparisons exactly as written, so `>` against `/ 2` means "more than half".
- The diagnosis is owner-only. It is never put into an agent task or into `prior` (Phase 4), because `vetoed` reveals
  the held-out veto.
- Docs: in `docs/runtime-v2.md` "Final result", document `diagnosis` with this table.

**Checks (owner: implementer):**
- Rust unit tests: build `Vec<Record>`, `Vec<Row>` and `Vec<Claim>` in memory for each row of the table, plus one case
  where two rules hold (the earlier one wins). `Record`, `Row`, `Claim` and `Candidate` are private structs in
  `src/ar.rs`, so the tests live in its `mod tests`.
- `scripts/ar_checks.py`: `scenario_00` (one audit fails by design) asserts `report["diagnosis"]["likely"] == "holes"`.
  The run that ends with ideas below the seed (the `hopeless` provider) asserts `"search"`. Both assert that every
  count key is present.
- `cargo test --release --quiet`, then `python3 scripts/ar_checks.py`.

### Phase 4: digest of earlier runs

**Outcome:** A5, A6.

**Change:**
- `Launch` gains `#[serde(default)] prior: String`.
- At launch only (the `(Some(..), None)` arm of `orchestrate`, before `launch.json` is written), compute
  `prior = digest(&workspace.join(".harness/ar"), &spec)` in a new `src/ar/prior.rs`. Resume never recomputes it.
- `digest`:
  - Lists the subdirectories of `.harness/ar`, newest first. Names are `{unix_secs}-{pid}.noindex`; sort on the
    leading integer.
  - Keeps a run only if it has `report.json` (it finished) and its `launch.json` spec equals the current spec, compared
    as `serde_json::to_value(&spec)`. Skip any run that can't be read or parsed; the digest never fails a launch.
  - Writes, per kept run:
    - a header: run name, `launch.commit`, the seed's search score (`seed.json["score"]`), and the number of rounds
      (`gate` records);
    - one line per explore claim: claim number, claim text (clipped to about 200 characters), depth (the number of
      exploit plans with that `idea` in the run's `planned` records), best valid search score from its `ledger.jsonl`
      rows, and **status**, taken from that run's records:
      - `result`: its idea is in `report.json["portfolio"]`;
      - `below seed after {depth} refinements`: no valid state, or its best search score doesn't beat the seed's by
        more than `min_delta`;
      - `not kept`: anything else. This deliberately merges "vetoed on held-out", "lower rank" and "unaudited";
    - "Evaluator holes found": every `audited` record with `pass == false`, with that candidate's claim and the
      recorded `reason`.
  - Adds the newest kept run's brief (the `final_answer` of the last finished attempt of plan `r0-research`, read from
    `episodes/<label>.report.json` as `Orchestrator::answer` does), labeled
    `Brief from run <name> (an earlier run; check it before relying on it)`.
  - Clips the whole text to `PRIOR_CHARS = 8_000`, the newest run first, using the existing `clip()`.
  - Never reads or writes `select`, `Selected` records, `good` or a candidate's `select` field.
- In `task()` for `Role::Research` only: when `launch.prior` is non-empty, add `<prior_runs>\n{prior}\n</prior_runs>`
  after the program. Add one rule to the research instructions: earlier runs with the same evaluator are summarized in
  `<prior_runs>`; check what you rely on; carry known evaluator holes into Traps; an idea that stayed below the seed
  after 0 or 1 refinements is not ruled out.
- Docs: in `docs/runtime-v2.md`, add "Earlier runs" (what is included, the identical-spec rule, why veto information is
  left out) and add `prior` to the `launch.json` description under "State and recovery". Add one sentence to
  `docs/workflows.md`.

**Checks (owner: implementer):**
- Rust unit tests for `digest` against a temporary `.harness/ar` holding hand-written run directories:
  - spec mismatch → `""`;
  - unfinished run (no `report.json`) → skipped;
  - a corrupt `journal.jsonl` → that run is skipped, others still appear;
  - statuses `result`, `below seed after N refinements`, `not kept`, and one failed-audit reason;
  - newest first, and the size cap holds;
  - a fixture whose `select` scores are distinctive numbers (for example `-12345.678`): neither the numbers nor the
    word "veto" appear in the output (A6).
- `scripts/ar_checks.py`: a new scenario runs the toy task twice in one workspace (a fast scripted provider). Assert:
  - the second run's `launch.json["prior"]` is non-empty and appears verbatim inside `<prior_runs>` in
    `episodes/r0-research.md`;
  - it names the first run's claims;
  - the first run's `-(x-5)^2` held-out scores (from its `journal.jsonl` `selected` records) don't appear in it.
  Then change `timeout` in `ar.toml`, commit, and run a third time: `prior == ""` and no `<prior_runs>`.
- `cargo test --release --quiet`, `python3 scripts/ar_checks.py`, then `python3 scripts/all_checks.py`.

## Decisions and why

- **Probation is a second batch inside the explore round, not a scheduling rule.** A rule like "candidates nobody has
  refined always get a worker" would break the `moved` test: leaders that got no workers "didn't move", which triggers a
  false re-explore, and fixing that needs a second rule. With the two-batch shape, the gate only ever sees refined
  ideas, and progress, `moved`, stopping and audits stay as they are. It also fixes A2 for free.
- **Auditors drop findings but keep the claim and the brief.** The claim says what was attempted, not whether it worked.
  The research episode wrote the brief before any worker ran.
- **The digest is derived, not stored separately.** Earlier run directories are the single source of truth. The
  identical-spec rule is what limits old lessons to runs where they apply. "Convention" (Hyra's human-confirmed tier)
  is already the owner's `program.md`.
- **"Not kept" hides the veto.** Telling later runs which ideas the held-out set rejected would let them steer around
  it, and over runs the held-out score would become as optimistic as the search score.
- **Diagnosis is owner-only.** It reads `vetoed`.

## Open question

The design flagged this; it doesn't block any phase.

- **Do auditors miss holes that workers admitted in their findings?** Settle it with an optional live check (needs a
  provider key; owner: the repository owner). Use an evaluator with a planted hole (for example, it rewards a
  hard-coded output that the program forbids). Run the audit with and without `<findings>` and compare how often it
  catches the hole. If dropping findings costs catches, give auditors a short evidence-only finding (what the worker
  changed, without its verdict) instead of none. This is not part of A1-A7.

## Risks

- **Cost.** An explore round runs up to `2 × width` episodes. This is the price of not discarding raw ideas. The cap is
  `self.width()` per batch.
- **One refinement is not full fairness.** A candidate with one refinement still competes against leaders with several.
  Ranking at equal depth is the next step if lockbox comparisons show it matters (see Exclusions).
- **A stale earlier brief could anchor research.** Mitigated: it's labeled, only research sees it, and each explore
  worker still derives its idea from its own seed.

## State

- Branch `plan/ar-hyra`, created from `main` at `deff054`. Only this file is added. No code changes yet.
- Self-containment: see the commit that adds this file.

## Next

1. In the new session, check out `plan/ar-hyra`, read this file, and confirm `main` hasn't moved under the touched
   functions (`git log deff054..origin/main -- src/ar.rs src/ar scripts/ar_checks.py docs`). If it has, re-check the
   line references above before editing.
2. With the owner's go, do Phase 1, commit, then Phases 2, 3 and 4, one commit each, running each phase's checks first.
