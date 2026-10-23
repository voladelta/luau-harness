# Runtime v2: explicit authority and recovery

The agent uses a persistent Luau VM for an ordinary live run. RPI stages each use a fresh agent VM. Rust owns the frozen run manifest, operation admission, journal, budgets and checkpoints. Recovery creates a fresh agent VM; it never executes historical chunks to rebuild memory.

```text
controller.step(saved state, event) → new state, command
                                      │
                         Rust supervisor validates and dispatches
                                      │
                  model request / agent chunk / final report
                                      │
                    principal → admission → durable intent
                                      │
                         executor → recorded receipt
                                      │
                       next controller checkpoint
```

## State and ownership

| State | Owner | Lifetime |
| --- | --- | --- |
| Policy, exact source bytes (package manifests and their capabilities included), model settings | Frozen manifest | One run |
| Operation IDs, receipts and budget reservations | Supervisor journal | Across restarts |
| Conversation, counters, finish-review state, pending calls | Serializable controller state | Across restarts |
| Agent globals and functions | Agent VM | Until the process exits |
| `durable` data | Supervisor checkpoint | Commits after a completed controller transition/chunk |
| Package locals and module caches | Isolated invocation VM | One invocation |

Controller transitions and package invocations start in fresh VMs. Mutable captured locals cannot become an additional workflow state store. The controller has pure builtins, frozen module imports, model metadata and diagnostic output; it cannot read files, execute processes, approve operations or invoke a provider.

Each checkpoint has a Rust-owned lifecycle: `running` carries opaque controller state and durable data; `awaiting_review` carries a durable owner gate; `finished` carries the validated final report and durable data. A `finish` command commits terminal status independently of the controller's private fields. Resuming a finished run returns its saved report even after its execution deadline. An open owner gate is stored only in the `awaiting_review` state; while it is open no workflow stage is active, so no stage effect can be admitted. Workflow runs add optional frozen configuration and execution-frame fields. A binary resumes only its own journal format; earlier formats have no migration.

## Durable data

```luau
local findings = { file = "src/config.rs", timeout = 30 }
durable.set("findings", findings)
findings.timeout = 99
assert(durable.get("findings").timeout == 30)
```

Values are copied at both boundaries. The accepted values are finite numbers, strings, booleans, null, contiguous arrays and string-keyed objects. Functions, userdata/host handles, arbitrary metatables, cycles and mixed-key tables are rejected. Native arrays carry a private inert tag so empty JSON arrays remain arrays through subsequent calls and checkpoints; a plain Luau `{}` denotes an object. A missing key returns nil. A chunk that returns a Lua error is still completed; its saved-data changes can commit with its error result. A process interrupted before checkpoint commit loses those changes. Filesystem and remote effects are not rolled back.

Copies stop during traversal at 64 levels, 262144 nodes or a conservative 16 MiB encoded-size bound. Shared subtrees count for every copied occurrence. The entire durable store has the same limits; rejected updates preserve its previous contents.

## Compaction history

Root agents can retrieve the public conversation removed by successful, recorded compactions:

```luau
local page = history({q="earlier failure", limit=5})
for _, entry in page.entries do print(entry.id, entry.snippet) end
-- Continue searches with the same q and before=page.next, even on an empty page.
local entry = history({id="h:428:m:17"})
print(entry.text)
-- Continue long entries with offset=entry.next_offset.
```

Search is a case-sensitive literal match over complete rendered entries. Listing defaults to five entries (maximum twenty), examines newest entries first and presents each page chronologically. Summary entries are labeled `kind="compaction"` and contain the bare summarizer output. Message entries preserve saved content and tool-call inputs; provider replay/reasoning fields and media payloads are excluded. This is the saved public transcript, so output shortened before entering the conversation cannot be recovered.

Reads return at most 4096 UTF-8-safe text bytes (`bytes` can lower this). Offsets are zero-based byte offsets; offsets inside a character fail. Serialized responses are capped at 12 KiB, including escaping. List snippets are at most 200 bytes. A search examines at most 128 entries and charges at most 64 MiB of checkpoint source bytes per call, including cache hits. Follow `next` until absent; `scan_complete=false` means more candidates remain, even if the page has no matches. Cursors bind the run, query and initial archive frontier.

The archive covers only recorded root compactions in this run, including earlier RPI stages and before resume. It excludes live messages, un-compacted remainders of retired stages, child lanes, branch-parent journals and native operation receipts. Children and isolated package VMs have no `history` global. No supported archive records returns `status="unavailable"`; an available archive without matches returns `status="ok"` with empty entries. Legacy journals remain resumable without backfilling history or changing their frozen modules and notices.

Successful compaction emits an internal `next` command with `archive={kind="compaction",cut=old_cut}`. Rust validates the source conversation, summary and retained suffix before commit. The same synced checkpoint record commits both the new state and a versioned archive descriptor referencing the preceding root checkpoint. Entry IDs derive from that record's sequence and source message ordinal, so identical messages remain distinct and IDs survive resume. Custom controllers that discard context without this directive do not create archive entries.

Checkpoint blobs remain the only authoritative bodies. A disposable descriptor index is rebuilt from the verified journal and grows with recorded compactions; a cache holds at most 8 MiB of encoded source bodies. Retrieval runs in a blocking worker with deadline checks, read authority and workflow admission. Missing, corrupt or individually oversized sources (over 64 MiB) fail explicitly. Lookup never replays historical chunks.

## Dispatch and uncertainty

Every mutating effect records a synced intent before its executor runs. Outcomes include full argument hashes and result receipts. A global mutation permit serializes mutations; sealed read batches may run concurrently. Subagents and packages retain or reduce their caller's authority.

There is at most one application-level dispatch attempt for an operation ID. A provider that answers without a generation is classified: a 4xx or 503 means nothing was generated, so its reservation is released; a 408, 5xx or malformed 200 body gets one new request under a new operation ID, and a 429 that generated nothing up to eight, backing off 2, 4, 8 … s when the provider names no delay (honouring `Retry-After`, capped at 30 s). A failed attempt that may have been billed (a lost response, a 500, 502 or 504) is charged its reserved tokens, recorded as a `billing` record with `"source": "hx"`; a lost response then gets one new request under a new operation ID. A reservation left open by a crash is charged the same way on resume or branch. Underlying services may have their own internal behavior; hx does not establish exactly-once remote execution.

An intent without an outcome is uncertain. Local `write` and `edit` settle themselves on resume or branch: each journals the hash of the content it is about to produce (`expect`) and replaces the file atomically (temp file, fsync, rename), so the file holds either its old content or exactly that. hx records a `resolution` with `"by": "hx"` and its evidence: `complete` when the file's hash matches, `abandon` when no `expect` was journaled or the file holds its previous content. An unreadable file is left for the owner.

Shell commands and MCP calls can act outside the workspace. If interrupted, resume allows inspection but blocks new mutations until the owner reconciles them:

```bash
hx log verify .harness/log/RUN.jsonl
hx recover .harness/log/RUN.jsonl OP --decision complete --yes
hx recover .harness/log/RUN.jsonl OP --decision abandon --yes
```

Record a decision based on the actual external state; it dispatches nothing and is final. After `abandon`, the resumed agent sees the receipts and can issue the operation again if it is still needed.

Journal or blob failure is fatal to effect admission, including when Luau catches the returned error. A separate OS lock allows only one writer/resumer. Hash chains and blob hashes detect corruption; they are not authentication against an owner who can rewrite the entire run store.

## Resume and branch

```bash
hx run --workspace PROJECT --resume PROJECT/.harness/log/RUN.jsonl
```

The manifest restores provider/model settings, context/output limits, extra packs, budgets, policy (including autopilot and approval mode) and exact module/package bytes, including the standard library and system prompt. Repeating these flags on resume is an error. Credential values are never stored; the named environment variables must still be available. A rebuilt binary can resume a run of the same journal format (it prints a note): the workflow's Luau comes from the manifest, and old chunks are never re-executed. Workspace configuration edits do not alter an existing run. Temporary file-tool path grants are recorded as `path_grant` journal entries and restored on resume. They are shared by the run's descendants and never copied to a new branch.

The latest committed controller checkpoint and durable data are restored into fresh VMs. An interrupted chunk's code and operation receipts are supplied to a recovery turn; pending historical tool calls are closed with an interruption result. The old chunk is not re-executed. Receipts reference content-addressed blobs in the protected log directory. Ordinary globals, closures and scratch files are not restored. A finished run returns its saved report without another provider call.

To choose current approved extensions, new policy, a new binary or other settings, explicitly branch:

```bash
hx run --workspace PROJECT --branch PROJECT/.harness/log/RUN.jsonl \
  --model MODEL --key-env OPENROUTER_API_KEY
```

A branch inherits committed durable data and the task unless a positional prompt or `--prompt-file` supplies a new task. It starts a new conversation and budget under a new manifest. Reconcile uncertain effects first. Previous receipts remain in the parent journal; branching does not undo the parent's workspace changes.

The branch's parent identity and inherited data are committed in its manifest, so recovery before the first controller checkpoint retains the seed data. Revocation blocks execution while still permitting the owner to reconcile effect receipts, then branch.

## Extensions and capabilities

Approved package bytes and manifests are captured together at launch. A captured package's hash must match its approval, and its capabilities are the ones its approved manifest requests. Symlink-containing extension trees are refused. Skills' supporting files are copied into the protected run store. Runtime resolution does not reread mutable workspace modules.

Each package method call gets a fresh VM and a Rust principal. Native effects check that principal at dispatch. Imports are restricted to the package's own frozen files; privileged controller modules and other packages cannot be imported. Arguments/results cross as data, never function references. A read-only descendant cannot regain write authority by calling a more privileged package. `proc.sh` permits shell actions within the externally supplied process environment; it is broader than `fs.write` alone.

Single-file tools in `.harness/tools/NAME.{lua,luau}` use the same invocation boundary. Workspace trust covers their source and grants their declared capabilities; no package manifest or lock entry is needed. Registration evaluates definitions without effects under a one-second CPU deadline and a 16 MiB memory limit. A namespace is exposed only after every method, signature, endpoint and required credential validates. Invalid files produce diagnostics without activating partial namespaces. Source, method metadata and fixed HTTP endpoint grants are frozen in the registry; successful registrations contribute signatures to the model prompt.

Local HTTP endpoints carry an explicit read-only classification (default: external). Requests use the supervisor's authorization, journal, deadline and mutation gate. Endpoint declarations grant network access to their fixed URLs subject to the environment's network access; redirects and automatic retries are disabled. Rust resolves named credentials from the environment, workspace secrets, then user secrets, and attaches them to headers. Secrets are not part of the frozen registry and may rotate; resume checks them again. File tools cannot read `secrets.toml`; the external sandbox must restrict shell access. Credential echoes in JSON responses and HTTP error excerpts are redacted before receipts are written.

Package initializers must be pure. Put file/process work inside exported methods. Pass invocation state as explicit data and save results in the agent's `durable` store when needed.

`hx approve` and `hx accept` affect future runs. The old `parallel`, `refresh_extensions()` and `reload()` APIs have been removed. `deftool()` records an inactive source definition; `promote()` syntax-checks and stages it without executing unapproved source or tests. The owner reviews, runs `hx test DIR`, accepts it, and starts a new run or branch.

`hx revoke LOG` writes a protected revocation marker. The supervisor stops admitting operations and refuses subsequent resume. It cannot undo already accepted external work.

## Batches

```luau
local results = batch({
    { op = "read", args = { "src/main.rs", { limit = 40 } } },
    { op = "search", args = { "TODO", { glob = "*.rs", max = 20 } } },
    { op = "agent", args = { "Inspect the test coverage", { max_turns = 4 } } },
})
for _, result in results do
    if result.ok then print(result.value) else print(result.error) end
end
```

The complete batch is validated and copied before dispatch. It accepts at most 32 independent read/search/read-only-agent operations. Results are published together in input order; search results also carry a `truncated` flag. Mutating shells, writable subagents, shared globals and arbitrary closures are excluded. `parallel{function ...}` is removed.

## Workflow customization

Override `std/loop` with `init(config)` and `step(state, event) → state, command`. Rust accepts only known commands and supplies no ambient effect authority to the controller. Rust reads only the commands and the state's `messages` array (for the trace and to attach image notifications to an agent request); the rest of the state belongs to the controller. The old imperative `hx.run`, `hx.llm` and `hx.tree` helpers have been removed. Controller state must pass the same data-only boundary as durable values.

Use `hooks.state` inside callbacks for persistent per-hook data:

```luau
local hooks = require("std/hooks")
hooks.on("before_finish", function(payload)
    if hooks.state.my_review_done then return { action = "accept" } end
    hooks.state.my_review_done = true
    return { action = "continue", message = "Check the changed behavior once more." }
end)
```

Control hooks fail the transition on error. Finish hooks return explicit `accept` or `continue` outcomes. Request hooks return validated requests; run hooks return code or an explicit denial. Observers registered with `hooks.observe` receive copies; their failures are diagnostic and their mutations cannot change control flow. Prompts append named runtime-access and finish-review sections instead of editing matching prose.

Compaction and finish review remain heuristics. A successful checkpoint proves a transition was committed, not that the model's reasoning or code is correct.

## RPI workflow

`hx run --rpi` selects the bundled `std/rpi` controller for the root. `--manual` requires `--rpi`; both are launch-only flags. Importing a module or enabling `std/rpi` as a pack does not activate the workflow. Ordinary runs and descendants continue through `std/loop`.

The fixed route is research questions → research → system design → program design and outline → implementation → verification. Failed verification enters repair and then fresh verification. A repair can return to its owning system/program design; it still counts toward the run-wide maximum of three cycles. Stage policy and prompts live in Luau; the host supplies generic boundaries, submission authority, artifact storage and review lifecycle.

The `std/loop.advance` adapter returns commands, prose candidates, native submissions or terminal stops. Only candidates and submission corrections may continue through `std/loop.continue`; a terminal worker cannot be revived. RPI disables the optional recheck heuristic explicitly. Plain prose never completes an RPI stage. Three submission correction attempts stop that stage honestly. Root turn counts and all existing admission budgets span fresh workers.

### Submission and handoff

During a root stage, the agent can call `stage.submit` inside the Luau tool. The payload contains outcome (`complete`, `needs_input` or `blocked`), the full Markdown document, summary, evidence and stage-specific fields. Program design supplies a structured check plan; verification cites actual numeric shell operation IDs and explicit findings. Descendants and isolated package invocations cannot submit for the root.

The host validates the generic payload and successful-chunk provenance, stores document bytes in a content-addressed blob and gives the controller a submission ID. Effects after submission and a second submission in that chunk are refused; remaining calls from the same model reply are discarded. A chunk error invalidates its prepared submission. Printed text, hook output and saved data cannot manufacture native provenance.

The pure controller returns one generic `boundary`: seal a submission and start a stage, pause at a gate, or finish. The host commits the new controller state, execution frame, artifact references, lifecycle and pending delivery together. Administrative delivery is incorporated before admitting new work. A crash before commit leaves the previous boundary authoritative; a crash after commit restores the pending delivery. Recovery never replays the old chunk.

A new stage replaces the root agent VM and worker transcript. The host supplies verified artifact documents for the declared input refs, workspace metadata and the immutable ordered owner-input ledger. Latest explicit owner feedback takes precedence over earlier assumptions. The outer controller's `messages` and nested worker transcript are synchronized so image additions survive worker transitions. Stage-local state and hooks reset; run accounting and user durable data remain separate.

### Stage permissions

An `inspect` stage ceiling applies during research and design, transitively to child agents and packages. It permits existing authorized reads, searches, views, agents and MCP listing; HTTP/MCP calls must have a trusted read classification. It denies shell, project writes/edits and module promotion regardless of command text. This ceiling intersects existing principals and grants; batch descendants keep their stricter read-only policy. Workspace stages retain ordinary policy. Artifact control writes are confined host bookkeeping rather than expanded project authority.

### Owner gates and immutable documents

Manual mode gates research questions, research, system design, program design and final acceptance. Automatic mode omits those routine reviews but still creates real input gates for essential missing information or mandatory human validation. Pauses report `finished:false`; resuming an unanswered gate makes no provider call. Review decisions are owner CLI actions:

```bash
hx review .harness/log/RUN.jsonl --gate GATE_ID --decision approve
hx review .harness/log/RUN.jsonl --gate GATE_ID --decision revise --input-file feedback.md
hx review .harness/log/RUN.jsonl --gate GATE_ID --decision answer --input-file answers.md
hx review .harness/log/RUN.jsonl --gate GATE_ID --decision waive --input-file reason.md
hx run --resume .harness/log/RUN.jsonl
```

Each gate declares its allowed decisions and exact artifact/check scope. Revision creates a new artifact version and invalidates downstream dependencies. An input answer resumes the relevant work; an arbitrary answer does not establish a passing check. Explicit waivers remain separate from passing evidence. Final acceptance commits without another model call, including after the execution deadline, subject to the host's source freshness and revocation checks. Resuming executable work still requires the run's existing budgets and deadline. Review decisions are recorded and consumed durably; recording one never dispatches work.

Artifacts are immutable host records with dependency refs and content hashes. `.agents/artifacts/rpi-RUN_ID/` contains readable projections, starting with `01-request.md`. Missing or edited projections are regenerated from canonical blobs and never become approved inputs. A projection failure does not change canonical authority. Resume restores the frozen workflow and rejects new launch configuration. A branch starts a new workflow with distinct artifacts and no inherited approvals.

### Verification scope

Program design freezes check IDs, exact commands, cwd, timeout and requirement mappings. Verification evidence must identify successful same-stage shell receipts matching those invocations. Failed exit status, timeout, substituted commands, missing receipts and source changes cannot satisfy a planned check. Re-verification requires fresh receipts. Source/check inventory observations bind verification and final acceptance to the declared bounded project scope: Git tracked and nonignored files where available, or a bounded file walk, with explicit check inputs. Runtime logs and artifact projections are excluded. This is an observation of that inventory, not a hermetic build or a guarantee against all external races; ignored build outputs and external dependencies require deliberate check/input coverage.

Adapted phase instructions and the upstream MIT notice are embedded with the binary; [provenance](../stdlib/std/rpi/PROVENANCE.md) records the pinned source and adaptation. There is no startup download or independent plugin installation. Trusted overrides affect new runs while generic host lifecycle, policy and provenance checks remain enforced.

## Autoresearch workflow

`hx run --ar --prompt-file PROGRAM` searches for a better score under `.harness/ar.toml` (covered by `hx trust`). It requires Git and a committed workspace apart from the editable paths. `--rpi`, `--branch`, `--trace`, `--max-tokens`, `--max-wall` and `--max-turns` are refused: episodes are limited in model turns, and the run ends by its own progress rule. All other run flags pass through to the episodes. `hx run --ar --resume .harness/ar/RUN` continues an interrupted search with its frozen launch settings.

```toml
editable = ["ar/strategy.pine"]                  # files or directories; the only thing that leaves an episode
eval = "$HOME/grader/eval search --file ar/strategy.pine"
select = "$HOME/grader/eval select --file ar/strategy.pine"   # optional, held out
grader = ["~/grader/bin", "~/grader/search"]     # owner data roots, sealed from agent file tools
holdout = ["~/grader/select"]                    # held-out data; configure process isolation externally
direction = "max"
timeout = 120
min_delta = 0.05
```

**Isolation.** At launch, HEAD's tree is frozen into the run's `base` (without `.harness`), byte for byte from the object store, so export attributes do not apply and submodules are not included; Git is not used again. Episodes work in separate copies; the external sandbox must restrict access to the owner's workspace:

- Each episode is a fresh child `hx run` in a throwaway copy of `base` that holds the state it starts from. The copy has no Git history and no `.harness` config. When the child ends, its journals are kept in `episodes/` and the copy is deleted. Nothing but snapshots of the editable paths carries over, so commits, ignore rules, hooks, scratch files and logs from one episode never reach a later one or the owner's repository.
- `ar.eval(note)` snapshots the editable files (binary-safe; symlinks, special files and editable paths below a symlinked directory skipped; UTF-8 names; at most 16 MiB, 10,000 files and 32 levels, or that eval is refused), stores the snapshot content-addressed, and runs the `eval` command in a second throwaway copy of `base` holding exactly those bytes. A process the agent leaves running cannot change what is scored. It appends a ledger row and returns `{n, score, valid, se, improved, best, report}`. Explore and exploit workers score only after their claim is accepted. A result improves the worker's best when it beats it by more than its `se` (by anything for a deterministic evaluator), so small steps add up across rounds; `min_delta` is the bar a candidate must clear against the seed. `ar.claims()` lists the claims in the worker's scope (every explore idea, or the refinements of its candidate) as plain text. An evaluator that reports no `se` is treated as deterministic: an eval of a state it already scored validly returns the stored result with `cached: true`, adding no ledger row when it is a probe or the same candidate scored it, and a row for this candidate (judged against the worker's best, without running the evaluator) when only another candidate did. `ar.reset()` puts the worker's best scored state (the state it started from when it has not improved) back into the editable paths, through the same path checks as `write`, so it cannot change anything outside the workspace.
- Evaluators are owner commands run directly with a scrubbed environment (no inherited credentials). Filesystem and network access, including separation between search and held-out data, must be configured externally. The `grader` and `holdout` declarations validate paths and seal them from agent file tools; they do not restrict evaluator or shell processes. When editable code runs inside the evaluator (a Python module it imports, for example), that code can print its own result line; such an evaluator must run candidates in a subprocess and parse their output itself.
- A child reads its episode (role, spec, run directory, starting state) once at launch and records it in its frozen policy, so its manifest says which episode it served.
- The run directory is sealed from file tools: in every mode, including `--yes`, `read`, `search` and `view` cannot read `.harness/ar`, and `write`/`edit` cannot modify it. Each child's policy also seals the owner's `.harness/ar`, `grader` and `holdout` roots. Shell commands run directly and require external confinement. All episode shell commands receive the same minimal environment, including under `--yes`.
- When an episode attempt or an evaluation ends, every process that still has its working directory or an open file in its throwaway copy is killed before the copy is deleted.

**Manager loop.** A run is research, then rounds until progress stops, and returns a portfolio of distinct good candidates rather than one best state: the single best score is the state most likely to be an overfit or an evaluator hole. Before any episode the host scores the seed with the search evaluator (`seed.json`, outside the ledger) and, with `select`, checks that the held-out evaluator scores the seed validly (otherwise the run stops before spending).

- *Research.* One episode studies the task (objective, how the evaluator scores, rules, the seed's weaknesses, traps) and writes the brief every later episode gets. Its evaluations are probes: recorded, never a candidate.
- *Round 1: explore.* Eight workers run side by side from the seed. Each gets its own fresh OS seed and derives one core idea from it with String Seed of Thought (list the root-level dimensions, pick an option per dimension from a part of the seed), then claims it with `ar.claim(idea)` before it may score anything. A claim must describe the idea in at least eight words: an accepted claim is final, so a placeholder such as `"x"` is refused without using it up. Claims are served one at a time in arrival order under the run's claim lock: the model is asked twice, independently, whether the new claim shares the core approach of an earlier one, and the claim is rejected only when both answers name the same earlier claim (a judge that cannot answer accepts). The same two calls also reject a claim that names no concrete approach (a placeholder, a probe or a test of the tool), without using up the worker's claim; workers read `ar.claims()` first instead of claiming to peek. A rejected worker sees every claim and derives another idea. A worker builds its idea faithfully and fixes what is broken, but does not tune it: it returns as soon as the idea works, and later rounds do the refining. Each accepted idea is a candidate with its own best state: its first state improves it even when worse than the seed (the starting state itself never does), and later ones must beat it by more than `se`.
- *Gate.* After each round, a candidate is good when its best state beats the seed's search score by more than `min_delta` and, with `select`, its held-out score is not worse than the seed's. Candidates are ranked good ones first, then by search score, one per distinct state, and the top three go on to the next round; a candidate that scores exactly as the seed on search is the seed in other bytes and never goes on. The held-out score is only a veto, computed only for candidates that beat the seed on search: ranking on it too would select on it every round until it is as optimistic as the search score. Workers return their ideas unpolished, so a candidate below the seed still earns refinement when fewer than three are good; only good candidates can become results. Good ones among the top three that have no verdict for their current state are audited, once per gate, in one batch; the gate then re-ranks, so a failed candidate's place goes to the next strongest, which is audited at a later gate. An auditor episode reads the state, looks for an evaluator hole (fills or outcomes the real system would not give, lookahead, leaked data, rule violations) with probe evaluations, and ends with `VERDICT: PASS` or `VERDICT: FAIL: reason`; a failed candidate never goes on. Only good candidates with a passing audit become results: an auditor that crashed or that ended without a `VERDICT:` line gives no verdict, and its candidate stays unaudited. With no valid candidate after round 1 the run stops. When no candidate of the last gate is good, the result is empty and the workspace keeps the seed.
- *Later rounds: exploit, or explore again.* Eight workers are split across the chosen candidates, each starting from its candidate's best state with the brief and the findings so far. Each claims a refinement, judged against the other refinements of that candidate (the same change to the same part of the solution counts as a duplicate), so workers on one candidate try different things. A worker implements its one refinement, fixes it if broken, and returns; depth comes from rounds, each starting from the best state so far. When an exploit round moves none of the chosen candidates (no new candidate or best state among them), the next round explores again from the seed: new workers see the ideas claimed in earlier rounds and must claim a different core approach; their candidates join the next gate with the earlier ones. Each round runs eight workers, six once any episode of the run has been rate-limited by the provider. A round makes progress when it chooses a new candidate or lifts a chosen one by more than `min_delta` since the last progress, so small refinement steps add up across rounds. After exploring the run refines; it refines again while that moves a chosen candidate at all, and explores again after a refining round that moved none. Two rounds in a row without progress end the run; since every round that makes progress raises a score by more than `min_delta` or adds a candidate, the run ends.
- *Findings.* Every worker ends with a finding (its final answer): what it tried, its best score, what worked. Later rounds and audits see the sixteen most recent findings, each with its worker's claim and best score.

**Episodes.** A child's task is the program, the brief, the findings so far, its role's instructions, the protocol (including that `ar` is a built-in global that `vars()` does not list) and the search evaluator's full report for the state it starts from, up to 12,000 characters; an exploit worker also sees its candidate's ledger. `ar.reset()` restores the worker's own best state, or the state it started from.

**Final result.** The good candidates the last gate chose, best search score first, are written to `portfolio/c<id>/`, and the first goes into the workspace's editable paths. Choosing on `select` makes its numbers optimistic, so compare finished runs on a third tier no run ever touches (a lockbox). The result is only as safe as the select tier's veto: it must reach the shift the result has to survive (other years, symbols or input ranges), and it needs enough cases that candidates differ by more than its noise.

**Limits.** Every episode is limited to 40 model turns, so a run means the same on any model. Stopping the orchestrator (SIGINT, SIGTERM or SIGHUP) stops its episode children, which kill the commands and evaluators they started; a child whose orchestrator dies outright sees its stdin close and stops the same way. An episode that dies before any eval is retried up to twice; when every episode of a batch crashed, the next try waits 2, then 4 minutes, and if the run has no eval yet and episodes keep crashing, it stops with the child's error. A run that ends with no evals exits nonzero.

**State and recovery.** `.harness/ar/RUN/` holds `launch.json` (spec, program, child flags, budget, frozen commit), `base/`, `seed`, `seed.json`, `journal.jsonl` (planned batches, episodes, held-out scores, audits and gates), `ledger.jsonl` (each row names its candidate), `claims.jsonl`, `snaps/`, `portfolio/`, `episodes/` (task, stderr, report and journals per child) and `report.json`. Each batch is journaled whole before any of it starts, and each episode as started (with its scratch directory) before its child runs and finished after. Resume uses the spec frozen in `launch.json` (approved at launch), so later edits to `ar.toml` neither change nor block it. It first closes an episode that never finished with a journaled charge: from the killed child's journals in its leftover scratch directory, which is then removed, or the whole allowance when that directory is gone. Then it runs what was planned but not finished, retrying interrupted workers that had not scored anything and interrupted research and audits (judged by a final answer a killed child never gave), and continues; a retried worker keeps the claim its earlier attempt made, and held-out scores, audit verdicts and gates already journaled are reused. A torn final line in the ledger or journal (a writer killed mid-append) is ignored when read and dropped before the next append. Kill the orchestrator's process group, not only the orchestrator, so no child keeps running.

## Budgets and validation

One journal-backed ledger accounts for the root and all descendants. Admission atomically reserves request bytes plus the configured output cap in tokens; successful receipts settle it. Concurrent subagents are admitted side by side while reservations fit under `max_tokens`; otherwise a request waits for running ones to settle instead of failing. Settled token usage includes ordinary input, cache reads, cache writes and output; reasoning is already included in output. The limit is an admission threshold, not an exact tokenizer or billing ceiling. hx tracks no dollar cost.

A 4xx answer, including a context rejection, releases its reservation, so a definite context rejection can trigger one compaction and a new request under any threshold. Transport loss and failed 5xx attempts are charged their reserved tokens.

The absolute run deadline includes downtime. It is checked before admission and around every controller transition, including controller-only loops. Controller execution and queued model admission also observe it. It interrupts local VM/process/model work where possible. Cancellation cannot revoke a request already accepted by a remote service.

MCP responses have a 16 MiB received-byte limit per request across stdio, HTTP JSON, SSE events and HTTP error bodies. Ignored notifications count toward the same allowance. Oversized responses close the connection; a mutation whose response is lost remains uncertain and is never automatically redispatched.

Run `cargo test`, `hx test stdlib/std`, `python3 scripts/redesign_checks.py`, and `python3 scripts/rpi_checks.py` after building. The acceptance suite uses local scripted providers, injects crashes at actual journal boundaries, resumes repeatedly, checks dispatch counts and committed state, and exercises capabilities, concurrency, revocation, corruption and deadlines. It does not prove model-answer correctness, physical power-loss durability, exhaustive filesystem races, or live-provider behavior.
