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

Each checkpoint has a Rust-owned lifecycle: `running` carries opaque controller state and durable data; `awaiting_review` carries a durable owner gate; `finished` carries the validated final report and durable data. A `finish` command commits terminal status independently of the controller's private fields. Resuming a finished run returns its saved report even after its execution deadline. An open owner gate is stored only in the `awaiting_review` state; while it is open no workflow stage is active, so no stage effect can be admitted. Workflow runs add optional frozen configuration and execution-frame fields. A checkpoint stores each message of the controller state's `messages` array as its own content-addressed blob and lists the message hashes in blobs of 64, so an unchanged message is written once however many checkpoints follow; the rest of the controller state is stored as it is. These blobs are synced before the checkpoint record that names them. A binary resumes only its own journal format; earlier formats have no migration.

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

Root agents can read back the public conversation removed by successful, recorded compactions, as plain Luau data. The system prompt does not describe `history`; each compaction summary of a root run ends with a notice that does, so the model learns of it once the archive has entries:

```luau
for _, c in history() do -- {id, messages, summary, stage?}, oldest first
  for i, m in history(c.id) do -- {role, text, code?}, in order
    if m.text:find("earlier failure", 1, true) then print(c.id, i, m.role, m.text:sub(1, 300)) end
  end
end
local some = history(2, {from = 10, to = 20})
```

`history()` lists the compactions: an id from 1, how many messages each removed, the summary that replaced them and, in a workflow, the stage it ran in. `history(id)` returns that compaction's removed messages, and `history(id, {from = i, to = j})` messages `i..j` of them. A message keeps its role and text; the code of its tool calls (or their input as JSON, for a tool without code) is in `code`, and a tool result is a `tool` message whose text is the output. Provider replay/reasoning fields and media payloads are left out. This is the saved public transcript, so output shortened before entering the conversation cannot be recovered. A call returns at most the data boundary (`hx.limits.DATA_BYTES`); a longer answer is an error naming the range that fits.

The archive covers only recorded root compactions in this run, including earlier RPI stages and before resume. It excludes live messages, un-compacted remainders of retired stages, child lanes, branch-parent journals and native operation receipts. Children and isolated package VMs have no `history` global. A run without a compaction lists none.

Successful compaction emits an internal `next` command with `archive={kind="compaction",cut=old_cut}`. Rust validates the source conversation, summary and retained suffix before commit. The same synced checkpoint record commits both the new state and a versioned archive descriptor referencing the preceding root checkpoint, whose messages are the archive: nothing else is stored. `history` reads the verified journal's checkpoint records when it is called, in a blocking worker under the run deadline, with read authority and workflow admission; a missing or corrupt source fails explicitly. Custom controllers that discard context without this directive do not create archive entries.

## Dispatch and uncertainty

Every mutating effect records a synced intent before its executor runs. Outcomes include full argument hashes and result receipts; a shell command's outcome also names who approved it (`approval`), and `hx log verify` rejects one that does not. A global mutation permit serializes mutations; sealed read batches may run concurrently. Subagents and packages retain or reduce their caller's authority. Starting a subagent (`agent`) is not itself a mutation: it needs `llm.agent` and the stage's permission, and records a `call` receipt, while every effect the subagent makes records its own intent and receipt. A crash during a subagent therefore leaves only the subagent's own uncertain effects, if any.

There is at most one application-level dispatch attempt for an operation ID. A provider that answers without a generation is classified: a 4xx or 503 means nothing was generated, so its reservation is released; a 408, 5xx or malformed 200 body gets one new request under a new operation ID, and a 429 that generated nothing up to eight, backing off 2, 4, 8 … s when the provider names no delay (honouring `Retry-After`, capped at 30 s). A failed attempt that may have been billed (a lost response, a 3xx, a 500, 502 or 504) is charged its reserved tokens, recorded as a `billing` record with `"source": "hx"`; a lost response then gets one new request under a new operation ID. A reservation left open by a crash is charged the same way on resume or branch. Underlying services may have their own internal behavior; hx does not establish exactly-once remote execution.

An intent without an outcome is uncertain. Local `write` and `edit` settle themselves on resume or branch: each journals an `expect` record (the file's hash before, `null` when it does not exist, the hash of the content it is about to produce, and the name of its temporary file) and replaces the file atomically (temporary file, fsync, rename), so the file holds either its old content or exactly the new. hx records a `resolution` with `"by": "hx"` and its evidence: `complete` when the file's hash is the new one, `abandon` when no `expect` was journaled or the file's hash is still the one before. A file that holds anything else, or cannot be read, was changed by something other than this write and is left for the owner. A temporary file left by a crash before the rename is removed when the operation is resolved, by hx or by the owner.

Shell commands and MCP calls can act outside the workspace. If interrupted, resume allows inspection but blocks new mutations until the owner reconciles them:

```bash
hx log verify RUN
hx recover RUN OP --decision complete --yes
hx recover RUN OP --decision abandon --yes
```

Record a decision based on the actual external state; it dispatches nothing and is final. After `abandon`, the resumed agent sees the receipts and can issue the operation again if it is still needed.

Journal or blob failure is fatal to effect admission, including when Luau catches the returned error. A separate OS lock allows only one writer/resumer.

The journal lives outside the workspace, in the workspace's log directory under hx's state directory (`$XDG_STATE_HOME/hx/workspaces/NAME-ID/log/`, see `hx paths`), with its blobs, its writer lock and its revocation marker. No root of the model's contains that directory: file tools refuse it and, with Landlock, shell commands and MCP servers can neither read nor write it. That, not the chain, is what keeps the model from truncating, editing or replacing its own journal, deleting a lock or a `.revoked` marker, or putting another run's journal in its place. `RUN` above is the run's name (its journal's file name without `.jsonl`), or a path to the journal.

Each record's `prev` is the SHA-256 of the previous line, and each record names its session, which is also the journal's file name. `hx log verify` and every reopen (resume, branch, `hx review`, `hx recover`) check both, and a log that fails is refused; `hx log verify` also checks that every blob a record names, and every message blob a checkpoint names, exists and has the content its name hashes. A resumed run must be in the workspace's log directory. What the chain detects:

- A record edited, inserted or deleted anywhere but at the end, and a gap in the sequence numbers, by anyone who did not recompute the chain after it: a damaged disk, a stray edit.
- One run's journal standing in for another's: copied over it, or renamed to its name. Its records name another session.
- A missing or altered blob.

What it does not detect:

- A rewrite by someone who recomputes the hashes: the chain has no secret. Only the journal's location keeps the model from writing it, so without Landlock, where shell commands run unconfined and can write anywhere the user can, a journal can be rewritten undetectably; so can the owner, or another process of the same user outside hx.
- Records removed from the end of a journal: what remains is a valid chain.

Journals that an earlier version chained with a per-user key, or kept in `.harness/log`, are not read; start a new run.

## Resume and branch

```bash
hx run --workspace PROJECT --resume RUN
```

The manifest restores provider/model settings, context/output limits, extra packs, budgets, policy (including autopilot and approval mode) and exact module/package bytes, including the standard library and system prompt. Repeating these flags on resume is an error. Credential values are never stored; the named environment variables must still be available. A rebuilt binary can resume a run of the same journal format (it prints a note): the workflow's Luau comes from the manifest, and old chunks are never re-executed. Workspace configuration edits do not alter an existing run. Temporary file-tool path grants are recorded as `path_grant` journal entries and restored on resume. They are shared by the run's descendants and never copied to a new branch.

The latest committed controller checkpoint and durable data are restored into fresh VMs. An interrupted chunk's code and operation receipts are supplied to a recovery turn; pending historical tool calls are closed with an interruption result. The old chunk is not re-executed. Receipts reference content-addressed blobs in the protected log directory. Ordinary globals, closures and scratch files are not restored. A finished run returns its saved report without another provider call.

To choose current approved extensions, new policy, a new binary or other settings, explicitly branch:

```bash
hx run --workspace PROJECT --branch RUN \
  --model MODEL --key-env OPENROUTER_API_KEY
```

A branch inherits committed durable data and the task unless a positional prompt or `--prompt-file` supplies a new task. It starts a new conversation and budget under a new manifest. Reconcile uncertain effects first. Previous receipts remain in the parent journal; branching does not undo the parent's workspace changes.

The branch's parent identity and inherited data are committed in its manifest, so recovery before the first controller checkpoint retains the seed data. Revocation blocks execution while still permitting the owner to reconcile effect receipts, then branch: branching opens the parent for reconciliation only, settles what the evidence settles and charges lost model responses their reserved tokens, even for a revoked run, and writes nothing else to it.

## Extensions and capabilities

Approved package bytes and manifests are captured together at launch. A captured package's hash must match its approval, and its capabilities are the ones its approved manifest requests. Extension trees containing symlinks are refused (a symlinked package or skill directory is followed; its content is what was approved). Skills' supporting files are copied into the protected run store. Runtime resolution does not reread mutable workspace modules.

Each package method call gets a fresh VM and a Rust principal. Native effects check that principal at dispatch. Imports are restricted to the package's own frozen files; privileged controller modules and other packages cannot be imported. Arguments/results cross as data, never function references. A read-only descendant cannot regain write authority by calling a more privileged package. `proc.sh` permits shell actions within the externally supplied process environment; it is broader than `fs.write` alone.

Single-file tools in `.harness/tools/NAME.{lua,luau}` use the same invocation boundary. Workspace trust covers their source and grants their declared capabilities; no package manifest or lock entry is needed. Registration evaluates definitions without effects under a one-second CPU deadline and a 16 MiB memory limit. A namespace is exposed only after every method, signature, endpoint and required credential validates. Invalid files produce diagnostics without activating partial namespaces. Source, method metadata and fixed HTTP endpoint grants are frozen in the registry; successful registrations contribute signatures to the model prompt.

Local HTTP endpoints carry an explicit read-only classification (default: external). Requests use the supervisor's authorization, journal, deadline and mutation gate. Endpoint declarations grant network access to their fixed URLs subject to the environment's network access; redirects and automatic retries are disabled. Rust resolves named credentials from the environment, workspace secrets, then user secrets, and attaches them to headers. Secrets are not part of the frozen registry and may rotate; resume checks them again. Both secrets files are in hx's configuration directory, outside every root: file tools refuse them, and Landlock keeps shell commands and MCP servers from reading or writing them. Literal echoes of a credential (and its JSON-escaped form in error excerpts) in responses and HTTP error excerpts are redacted before receipts are written; other encodings of it are not detected.

Package initializers must be pure. Put file/process work inside exported methods. Pass invocation state as explicit data and save results in the agent's `durable` store when needed.

`hx approve` affects future runs. The old `parallel`, `refresh_extensions()` and `reload()` APIs have been removed. The model cannot define, stage or activate extensions; the owner edits `.harness/`, runs `hx trust`, and starts a new run or branch.

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

The complete batch is validated and copied before dispatch. It accepts at most 32 independent read/search/read-only-agent operations. Each operation goes through the same authorization, journal records and operation count as the single primitive, and agents run read-only. Results are published together in input order: a read's value is the file's bytes, a search's value is its rows and the entry also carries `truncated`, and an agent's value is `{answer, finished, turns}`, as `agent()` returns. Mutating shells, writable subagents, shared globals and arbitrary closures are excluded. `parallel{function ...}` is removed.

## Workflow customization

Override `std/loop` with `init(config)` and `step(state, event) → state, command`. Rust accepts only known commands and supplies no ambient effect authority to the controller. Rust reads only the commands and the state's `messages` array (for the trace and to attach image notifications to an agent request); the rest of the state belongs to the controller. The old imperative `hx.run`, `hx.llm` and `hx.tree` helpers have been removed. Controller state must pass the same data-only boundary as durable values.

Use `hooks.state` inside callbacks for persistent per-hook data; the controller VM is rebuilt for every step, so captured upvalues do not last. The loop saves `hooks.state` in its state after every step, whether a hook changed it in place or assigned a new table:

```luau
local hooks = require("std/hooks")
hooks.on("before_finish", function(payload)
    if hooks.state.my_review_done then return { action = "accept" } end
    hooks.state.my_review_done = true
    return { action = "continue", message = "Check the changed behavior once more." }
end)
```

Control hooks fail the transition on error. Finish hooks return explicit `accept` or `continue` outcomes. Request hooks (`before_model`) run once per agent request, never for a summarizer call, and return validated requests: the system prompt and tools they return apply to that request, while `messages` is the conversation itself, so a message a hook adds or changes stays in the history; run hooks return code or an explicit denial. Observers registered with `hooks.observe` receive copies; their failures are diagnostic and their mutations cannot change control flow. Prompts append named runtime-access and finish-review sections instead of editing matching prose.

Compaction and finish review remain heuristics. A successful checkpoint proves a transition was committed, not that the model's reasoning or code is correct.

## RPI workflow

`hx run --rpi` selects the bundled `std/rpi` controller for the root. `--manual` requires `--rpi`; both are launch-only flags. Importing a module or enabling `std/rpi` as a pack does not activate the workflow. Ordinary runs and descendants continue through `std/loop`.

The fixed route is research questions → research → system design → program design and outline → implementation → verification. Failed verification enters repair and then fresh verification. A repair can return to its owning system/program design; it still counts toward the run-wide maximum of three cycles. Stage routing and prompts live in Luau. The host side (`workflow.rs`, `workflow_runtime.rs`) is specific to RPI: it supplies stage boundaries, submission authority, check receipts, artifact storage and the review lifecycle. An owner revision at final acceptance starts a repair that does not count toward the three cycles.

The `std/loop.advance` adapter returns commands, prose candidates, native submissions or terminal stops. Only candidates and submission corrections may continue through `std/loop.continue`; a terminal worker cannot be revived. RPI disables the optional recheck heuristic explicitly. Plain prose never completes an RPI stage. Three submission corrections stop that stage honestly; a correction is a reply without a valid submission or a chunk in which `stage.submit` refused a payload (the host reports that refusal as a typed `submission_error` flag, never by matching error text). Root turn counts and all existing admission budgets span fresh workers.

### Submission and handoff

During a root stage, the agent can call `stage.submit` inside the Luau tool. The payload contains outcome (`complete`, `needs_input` or `blocked`), the full Markdown document, summary, evidence and stage-specific fields. Program design supplies a structured check plan; verification cites actual numeric shell operation IDs and explicit findings. Descendants and isolated package invocations cannot submit for the root.

`stage.submit` checks everything the model controls before the boundary, so a bad payload is a refused call the model can correct: the payload shape, the check plan (only program design may submit `checks`; a check's `cwd` and `inputs` must be relative paths inside the workspace), verification receipts, and, for implementation and repair, that a `complete` submission leaves a project verification can observe. It records successful-chunk provenance, stores document bytes in a content-addressed blob and gives the controller a submission ID. Program design's document gets a generated "Frozen check plan" section showing the plan exactly as the host parsed it. Effects after submission and a second submission in that chunk are refused; remaining calls from the same model reply are discarded. A chunk error invalidates its prepared submission. Printed text, hook output and saved data cannot manufacture native provenance.

The pure controller returns one `boundary`: seal a submission and start a stage, pause at a gate, or finish. The host commits the new controller state, execution frame, artifact references, lifecycle and pending delivery together. Administrative delivery is incorporated before admitting new work. A crash before commit leaves the previous boundary authoritative; a crash after commit restores the pending delivery. Recovery never replays the old chunk.

A new stage replaces the root agent VM and worker transcript. The host supplies verified artifact documents for the declared input refs, workspace metadata and the immutable ordered owner-input ledger. Latest explicit owner feedback takes precedence over earlier assumptions. The worker transcript is stored once, in the controller's `messages`; std/rpi hands it to the std/loop worker only while that worker runs, so image additions survive worker transitions. Stage-local state and hooks reset; run accounting and user durable data remain separate.

### Stage permissions

An `inspect` stage ceiling applies during research and design, transitively to child agents and packages. It permits existing authorized reads, searches, views, agents and MCP listing; HTTP/MCP calls must have a trusted read classification. It denies shell, project writes/edits and module promotion regardless of command text. This ceiling intersects existing principals and grants; batch descendants keep their stricter read-only policy. Workspace stages retain ordinary policy. Artifact control writes are confined host bookkeeping rather than expanded project authority.

### Owner gates and immutable documents

Manual mode gates research questions, research, system design, program design and final acceptance. Automatic mode omits those routine reviews but still creates real input gates for essential missing information or mandatory human validation. Pauses report `finished:false`; resuming an unanswered gate makes no provider call. Review decisions are owner CLI actions:

```bash
hx review RUN --gate GATE_ID --decision approve
hx review RUN --gate GATE_ID --decision revise --input-file feedback.md
hx review RUN --gate GATE_ID --decision answer --input-file answers.md
hx review RUN --gate GATE_ID --decision waive --input-file reason.md
hx run --resume RUN
```

Each gate declares its allowed decisions and exact artifact/check scope. Review and acceptance reports carry the frozen `check_plan` (ID, command, cwd, timeout, requirements, inputs). Revision creates a new artifact version and invalidates downstream dependencies. An input answer resumes the relevant work; an arbitrary answer does not establish a passing check. Explicit waivers remain separate from passing evidence. Final acceptance commits without another model call, including after the execution deadline, subject to the host's source freshness and revocation checks. Resuming executable work still requires the run's existing budgets and deadline. Review decisions are recorded and consumed durably, each exactly once; recording one never dispatches work, and a gate accepts one unconsumed decision. An acceptance approval that is stale on resume (the project changed after `hx review` recorded it) is consumed without effect: the resume reports `decision_rejected` and the gate stays open for a new decision, normally `revise`.

Artifacts are immutable host records with content hashes. `.agents/artifacts/rpi-RUN_ID/` contains readable projections, starting with `01-request.md`. Missing or edited projections are regenerated from canonical blobs and never become approved inputs. A projection failure (for example `.agents` being a symlink or read-only) prints a warning and does not stop, block or change the run. Resume restores the frozen workflow and rejects new launch configuration. A branch starts a new workflow with distinct artifacts and no inherited approvals.

### Verification scope

Program design freezes check IDs, exact commands, cwd, timeout and requirement mappings. Verification evidence must identify successful same-stage shell receipts matching those invocations. Failed exit status, timeout, substituted commands, missing receipts and source changes cannot satisfy a planned check. Re-verification requires fresh receipts. Source/check inventory observations bind verification and final acceptance to the declared bounded project scope: Git tracked and nonignored files, with explicit check inputs; a directory input covers every file below it. The workspace must be its own Git repository, and `--rpi` observes the project once at launch, so a project it cannot observe (a file over 32 MiB, a non-UTF-8 path, a symlink leading outside) is refused before any work. Receipts and gates store only the inventory digest. What each stage does with a project it cannot observe: implementation and repair cannot submit `complete` until it can be observed again (they have the shell to fix it). Verification is told why when it starts that way. Its shell commands always run: a receipt taken while the project cannot be observed records `project_observation_error` instead of a digest (the outcome is not observed again) and cannot satisfy a planned check. When verification submits, whatever its outcome, over a project it cannot observe, the host seals a failed verification with that finding (`verification.project_error`), which goes to repair. Runtime logs and artifact projections are excluded. This is an observation of that inventory, not a hermetic build or a guarantee against all external races; ignored build outputs and external dependencies require deliberate check/input coverage.

Adapted phase instructions and the upstream MIT notice are embedded with the binary; [provenance](../stdlib/std/rpi/PROVENANCE.md) records the pinned source and adaptation. There is no startup download or independent plugin installation. Trusted overrides affect new runs while generic host lifecycle, policy and provenance checks remain enforced.

## Autoresearch workflow

`hx-ar --prompt-file PROGRAM` searches for a better score under `.harness/ar.toml` (covered by `hx trust`). It requires Git and a committed workspace apart from the editable paths. `--rpi`, `--branch`, `--trace`, `--max-tokens`, `--max-wall`, `--max-turns` and `--ignore-config` are refused: episodes are limited in model turns, and the run ends by its own progress rule. The model and provider flags, `--pack`, `--approval` and `--yes` pass through to the episodes. `hx-ar --resume RUN` continues an interrupted search with its frozen launch settings; `RUN` is the run's name in the workspace's autoresearch directory (`ar/` in the `state` directory `hx paths` prints), or its path.

`hx-ar` is a second binary built on the hx library and installed beside `hx`; the harness core knows nothing about it. Each episode is an ordinary child `hx run` whose agent reaches `hx-ar` through MCP: `hx-ar` serves the tools `eval`, `claim`, `claims` and `reset` on loopback (Streamable HTTP, plain JSON responses), and the agent calls them as `mcp.ar.eval{note = ...}`, `mcp.ar.claim{idea = ...}`, `mcp.ar.claims{}` and `mcp.ar.reset{}`. `hx-ar` is the only process that holds the run's state: it snapshots episodes' files, runs the evaluators, judges claims and writes the run's journal.

```toml
editable = ["ar/strategy.pine"]                  # files or directories; the only thing that leaves an episode
eval = "$HOME/grader/eval search --file ar/strategy.pine"
select = "$HOME/grader/eval select --file ar/strategy.pine"   # optional, held out
grader = ["~/grader/bin", "~/grader/search"]     # owner data roots, sealed from agent file tools
holdout = ["~/grader/select"]                    # held-out data: readable only by the select evaluator (Landlock)
direction = "max"
timeout = 120
min_delta = 0.05
```

**Isolation.** At launch, HEAD's tree is frozen into the run's `base` (without `.harness`), byte for byte from the object store, so export attributes do not apply and submodules are not included; Git is not used again. Episodes work in separate copies; Landlock keeps their shell commands out of the owner's workspace (network access is up to the environment):

- Each episode is a fresh child `hx run` in a throwaway copy of `base` that holds the state it starts from. The copy has no Git history, and its `.harness` holds only what `hx-ar` writes for the episode: `mcp.toml` names `hx-ar`'s server and the variable holding the episode's token, and `policy.toml` pre-approves the server's tools (`allow = ["mcp ar.*"]`) and seals the `grader` and `holdout` roots (`[fs] sealed`). The child trusts exactly that configuration (`HX_TRUST` set to its hash). When the child ends, its journals are kept in `episodes/` and the copy is deleted. Nothing but snapshots of the editable paths carries over, so commits, ignore rules, hooks, scratch files and logs from one episode never reach a later one or the owner's repository.
- `eval` snapshots the episode's editable files (binary-safe, executable bits kept; symlinks, special files and editable paths below a symlinked directory skipped; every directory and file is opened relative to its already opened parent without following a symlink, so swapping one in while the snapshot is taken reaches nothing outside; UTF-8 names; at most 16 MiB, 10,000 files and 32 levels, or that eval is refused), stores the snapshot content-addressed, and runs the `eval` command in a second throwaway copy of `base` holding exactly those bytes and modes. A process the agent leaves running cannot change what is scored. It journals the evaluation and returns `{n, score, valid, se, improved, best, report}`. Explore and exploit workers score only after their claim is accepted. A state's search score is the mean of all valid evaluations of its snapshot (and of the host's baseline evaluation, for the seed), with the standard error of that mean, `sqrt(sum of se²) / n`, when the evaluator reports `se`. Evaluating the same files again adds a sample to their mean; it is not a new state and never improves. A new state improves the worker's best when its score beats the best state's by more than their combined standard error, `sqrt(se_a² + se_b²)` (by anything for a deterministic evaluator), so small steps add up across rounds; `min_delta` is the bar a candidate must clear against the seed. Evaluations of one snapshot run one at a time. `claims` lists the claims in the worker's scope (every explore idea, or the refinements of its candidate) as plain text. An evaluator that has reported no `se` in the run (the host's baseline evaluation included) is treated as deterministic (once it reports one, every later evaluation runs the evaluator): an eval of a state it already scored validly returns the stored result with `cached: true`, adding no journal record when it is a probe or the same candidate scored it, and a record for this candidate (judged against the worker's best, without running the evaluator) when only another candidate did. `reset` puts the worker's best scored state (the state it started from when it has not improved) back into the editable paths. The agent controls the episode's workspace, so `hx-ar` captures and writes it opening every directory relative to its already opened parent without following a symlink: a link the agent plants is refused, and nothing outside the editable paths is read or changed.
- Evaluators are owner commands that `hx-ar` runs with a scrubbed environment (no inherited credentials). With Landlock each one reads only its scratch copy, the system and toolchain directories and its `grader` roots (the `select` evaluator also its `holdout` roots), and writes only its scratch copy, so the search evaluator cannot read held-out data. A `holdout` root inside a `grader` root, or inside a system or toolchain directory every evaluator reads, would be readable by the search evaluator: such an `ar.toml` is refused, as is one whose `grader` or `holdout` root overlaps the workspace or hx's own configuration or state directory (`grader = ["~"]`, say), since code under test runs in the evaluators. Network access must be configured externally, and without Landlock evaluators run unconfined. The `grader` and `holdout` declarations also seal those roots from agent file tools and shell commands. When editable code runs inside the evaluator (a Python module it imports, for example), that code can print its own result line; such an evaluator must run candidates in a subprocess and parse their output itself.
- An episode's identity (role, candidate, starting state, workspace) lives only in `hx-ar`, under a random bearer token the child gets in its environment (`HX_AR_TOKEN`, named by `bearer_env` in its `mcp.toml`). Shell commands never see the token, since hx scrubs their environment, and with Landlock a confined command cannot read hx's `/proc` entries. A request without a token, with an unknown one, or with the token of an episode whose child has exited is refused. The child journals each call and its result like any MCP call, so its manifest says which server it used.
- The run directory is in hx's state directory, outside every root: in every mode, including `--yes`, file tools refuse it and shell commands cannot reach it. Each episode's `policy.toml` also seals the `grader` and `holdout` roots from file tools, and `ar.toml` may not put them inside a directory the agent's shell commands read (the system and toolchain directories). Shell commands are confined to the child's file policy, which includes none of these. All episode shell commands receive the same minimal environment, including under `--yes`.
- When an episode attempt or an evaluation ends, every process that still has its working directory, its executable or an open file in its throwaway copy is killed (found under `/proc`, repeated until none is left) before the copy is deleted. Throwaway copies live in one directory per process in the system temp directory, locked while that process lives; one left by a process that was stopped or killed is removed, with any process still in it, by the next `hx-ar`.

**Manager loop.** A run is research, then rounds until progress stops, and returns a portfolio of distinct good candidates rather than one best state: the single best score is the state most likely to be an overfit or an evaluator hole. Before any episode the host scores the seed with the search evaluator (journaled apart from the agents' evaluations) and, with `select`, checks that the held-out evaluator scores the seed validly (otherwise the run stops before spending).

- *Research.* One episode studies the task (objective, how the evaluator scores, rules, the seed's weaknesses, traps) and writes the brief every later episode gets. Its evaluations are probes: recorded, never a candidate.
- *Round 1: explore.* Eight workers run side by side from the seed. Each gets its own fresh OS seed and derives one core idea from it with String Seed of Thought (list the root-level dimensions, pick an option per dimension from a part of the seed), then claims it with `mcp.ar.claim{idea = ...}` before it may score anything. A claim must describe the idea in at least eight words: an accepted claim is final, so a placeholder such as `"x"` is refused without using it up. Claims are served one at a time in arrival order: `hx-ar` asks the episodes' model twice, independently, whether the new claim shares the core approach of an earlier one, and the claim is rejected only when both answers name the same earlier claim (a judge that cannot answer accepts). The same two calls also reject a claim that names no concrete approach (a placeholder, a probe or a test of the tool), without using up the worker's claim; workers read `mcp.ar.claims{}` first instead of claiming to peek. A rejected worker sees every claim and derives another idea. A worker builds its idea faithfully and fixes what is broken, but does not tune it: it returns as soon as the idea works, and later rounds do the refining. Each accepted idea is a candidate with its own best state: its first state improves it even when worse than the seed (the starting state itself never does), and later ones must beat it by more than `se`.
- *Gate.* After each round, a candidate's best state is the one with the best search score among the states that improved it, leaving out any state that failed an audit. The candidate is good when that score beats the seed's by more than `min_delta` and by more than their combined standard error and, with `select`, its held-out score is not worse than the seed's by more than their combined standard error (each held-out score is one evaluation, with the `se` the select evaluator reports). Candidates are ranked good ones first, then by search score, one per distinct state, and the top three go on to the next round; a candidate whose best state is the seed's own files never goes on. The held-out score is only a veto, computed only for candidates that beat the seed on search: ranking on it too would select on it every round until it is as optimistic as the search score. Workers return their ideas unpolished, so a candidate below the seed still earns refinement when fewer than three are good; only good candidates can become results. Good ones among the top three that have no verdict for their current state are audited, once per gate, in one batch; the gate then re-ranks, so a failed candidate's place goes to the next strongest, which is audited at a later gate. An auditor episode reads the state, looks for an evaluator hole (fills or outcomes the real system would not give, lookahead, leaked data, rule violations) with probe evaluations, and ends with `VERDICT: PASS` or `VERDICT: FAIL: reason` (markdown emphasis and case aside); a state that failed is never its candidate's best again, and the candidate goes on with its next best state. Only good candidates with a passing audit become results: an auditor that crashed, or whose last `VERDICT:` line is neither `PASS` nor `FAIL`, or that gave none, gives no verdict, and its candidate stays unaudited. With no valid candidate after round 1 the run stops. When no candidate of the last gate is good, the result is empty and the workspace keeps the seed.
- *Later rounds: exploit, or explore again.* Eight workers are split across the chosen candidates, each starting from its candidate's best state with the brief and the findings so far. Each claims a refinement, judged against the other refinements of that candidate (the same change to the same part of the solution counts as a duplicate), so workers on one candidate try different things. A worker implements its one refinement, fixes it if broken, and returns; depth comes from rounds, each starting from the best state so far. When an exploit round moves none of the chosen candidates (no new candidate or best state among them), the next round explores again from the seed: new workers see the ideas claimed in earlier rounds and must claim a different core approach; their candidates join the next gate with the earlier ones. Each round runs eight workers, six once any episode of the run has been rate-limited by the provider. A round makes progress when it chooses a new candidate, or when a chosen candidate's search score beats its score at the last progress by the rule the gate uses against the seed: by more than `min_delta` and by more than the combined standard error of the two estimates. Small refinement steps add up across rounds until they count, and re-scoring unchanged files (which moves a noisy state's mean) counts only when the new mean clears that combined standard error, which a re-scoring of the same bytes rarely does. After exploring the run refines; it refines again while that moves a chosen candidate at all, and explores again after a refining round that moved none. Two rounds in a row without progress end the run. There is no fixed bound on the number of rounds: the run ends once two rounds in a row neither bring a new candidate into the chosen three nor lift a chosen one by more than the evaluator's noise and `min_delta`. With a deterministic evaluator only changed files can lift a score; with a noisy one a re-scoring can, rarely, and each such false lift costs one more round.
- *Findings.* Every worker ends with a finding (its final answer): what it tried, its best score, what worked. Later rounds and audits see the sixteen most recent findings, each with its worker's claim and best score.

**Episodes.** A child's task is the program, the brief, the findings so far, its role's instructions, the protocol (the `mcp.ar` tools) and the search evaluator's full report for the state it starts from, up to 12,000 characters; an exploit worker also sees its candidate's evaluations. `reset` restores the worker's own best state, or the state it started from.

**Calls and lost answers.** `claims` is a read-only tool. `eval`, `claim` and `reset` change state, so the agent's hx treats each as an external effect: if its answer is lost, its outcome is unknown, and the episode's later effects are refused until an owner settles it, which in practice ends the episode. `hx-ar` keeps that rare and harmless. Each episode's `mcp.toml` sets the call deadline (`timeout_secs`) to twice the evaluator's `timeout` plus ten minutes, longer than any call can wait. Every call runs to its end and journals its result whether or not the answer arrives. `claim` and `reset` are idempotent: the same claim again gets the first answer (a different second idea is refused), and `reset` writes the same state again. Repeating `eval` evaluates the same bytes again, which refines their score and never counts as an improvement. When `hx-ar` itself stops, its episodes stop with it (see Limits), so no call is left half done.

**Final result.** The good candidates the last gate chose, best search score first, are written to `portfolio/c<id>/`, and the first goes into the workspace's editable paths: only files whose bytes or executable bit it changed from the seed are written, and only files it removed that still hold the seed's bytes are deleted; files the run never saw (untracked symlinks, special files) and the modes of other files stay as they are. With no result the workspace is not touched. Choosing on `select` makes its numbers optimistic, so compare finished runs on a third tier no run ever touches (a lockbox). The result is only as safe as the select tier's veto: it must reach the shift the result has to survive (other years, symbols or input ranges), and it needs enough cases that candidates differ by more than its noise.

**Limits.** Every episode is limited to 40 model turns, so a run means the same on any model. Stopping `hx-ar` (SIGINT, SIGTERM or SIGHUP) kills the evaluators it is running and stops its episode children, which kill the commands they started. A child whose `hx-ar` dies outright gets SIGTERM from the kernel (`hx-ar` sets `PR_SET_PDEATHSIG` as it starts the child) and stops the same way; an evaluator that `hx-ar` was running when it was killed outright runs until its own timeout, and the next `hx-ar` removes its scratch copy and anything left in it. An episode that dies before any eval is retried up to twice; when every episode of a batch crashed, the next try waits 2, then 4 minutes. When every attempt of every episode of a batch crashed (a provider outage, or misconfiguration), the run stops there with the child's error and exits nonzero without a report; `hx-ar --resume` then gives that batch's unfinished episodes fresh attempts. A run that ends with no evals exits nonzero.

**State and recovery.** The run directory (`RUN` in the workspace's autoresearch directory, `$XDG_STATE_HOME/hx/workspaces/NAME-ID/ar/`) holds `launch.json` (spec, program, child flags, frozen commit), `base/`, `journal.jsonl` (the seed and its baseline score, planned batches, episodes, every evaluation (each names its candidate) and accepted claim, what each claim's judge calls spent, held-out scores, audits and gates), `snaps/`, `portfolio/`, `episodes/` (task, stderr, report and journals per child; each child is an `hx run` that journals in hx's state directory like any run, and its journals are moved here when it ends) and `report.json`. Each batch is journaled whole before any of it starts, and each episode as started (with its scratch directory) before its child runs and finished after. Resume uses the spec frozen in `launch.json` (approved at launch), so later edits to `ar.toml` neither change nor block it. It first closes an episode that never finished: as finished when its child had ended and its report was saved, otherwise as interrupted, with the rate limits the killed child's journals recorded. Then it runs what was planned but not finished, retrying interrupted workers that had not scored anything and interrupted research and audits (judged by a final answer a killed child never gave) without counting them as crashes, and continues; a retried worker keeps the claim its earlier attempt made, and held-out scores, audit verdicts and gates already journaled are reused. `hx-ar` is the journal's only writer: it holds it locked while it runs, so a second `hx-ar` on the same run is refused, and the run's state is a fold over its records. A final record cut off because `hx-ar` was killed while writing it is dropped when the run resumes.

## Budgets and validation

One journal-backed ledger accounts for the root and all descendants. Admission atomically reserves request bytes plus the configured output cap in tokens; successful receipts settle it. Concurrent subagents are admitted side by side while reservations fit under `max_tokens`; otherwise a request waits for running ones to settle instead of failing. Settled token usage includes ordinary input, cache reads, cache writes and output; reasoning is already included in output. The limit is an admission threshold, not an exact tokenizer or billing ceiling. hx tracks no dollar cost.

A 4xx answer, including a context rejection, releases its reservation, so a definite context rejection can trigger one compaction and a new request under any threshold. Transport loss and failed 5xx attempts are charged their reserved tokens.

The absolute run deadline includes downtime. It is checked before admission and around every controller transition, including controller-only loops. Controller execution and queued model admission also observe it. It interrupts local VM/process/model work where possible. Cancellation cannot revoke a request already accepted by a remote service.

MCP responses have a 16 MiB received-byte limit per request across stdio, HTTP JSON, SSE events and HTTP error bodies, and each request has 60 s (the server's `timeout_secs` in `mcp.toml`, when set) from the first byte of its request written to the last byte of its response; a stdio server that stops reading its input is dropped (its process group killed) when the time runs out, and a mutation it may have read in part or whole remains uncertain. Ignored notifications count toward the same allowance. Oversized responses close the connection; a mutation whose response is lost remains uncertain and is never automatically redispatched. A request that certainly did nothing is a definite failure: an unknown server, a failed start or handshake, a refused connection, a write to a server that has exited, a JSON-RPC error, a 4xx or a 503; any other status that is not 2xx (a 3xx, which hx does not follow, or another 5xx) leaves a mutation uncertain. A connection found dead before the request reached it (a server that exited, an expired session) is replaced, and that request is sent once on the new one.

Model responses have a 256 MiB received-byte limit per request across every streamed event, a plain response body and an error body. An error status settles the attempt as above even when its body is cut off at the limit; a stream or success body over the limit is a lost response.

Run `cargo test`, `hx test stdlib/std`, `python3 scripts/redesign_checks.py`, and `python3 scripts/rpi_checks.py` after building. The acceptance suite uses local scripted providers, injects crashes at actual journal boundaries, resumes repeatedly, checks dispatch counts and committed state, and exercises capabilities, concurrency, revocation, corruption and deadlines. It does not prove model-answer correctness, physical power-loss durability, exhaustive filesystem races, or live-provider behavior.
