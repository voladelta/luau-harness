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

Each checkpoint has a Rust-owned lifecycle: `running` carries opaque controller state and durable data; `awaiting_review` carries a durable owner gate; `finished` carries the validated final report and durable data. A `finish` command commits terminal status independently of the controller's private fields. Resuming a finished run returns its saved report even after its execution deadline. An open owner gate is stored only in the `awaiting_review` state; while it is open no workflow stage is active, so no stage effect can be admitted. Workflow runs add optional frozen configuration and execution-frame fields. A checkpoint (and the run manifest) is stored as a tree of content-addressed blobs: every subtree whose JSON encoding exceeds 1 KiB is its own blob, referenced from its parent, so a transition writes only what changed (a new message, the list node that references it, the path to the root) and an unchanged message, image or module source is stored once, across checkpoints and across runs sharing a blob store. All blobs of a tree are fsynced, with their directory, before the synced record naming its root, so the record commits the whole checkpoint or nothing. Loading verifies each blob's hash. A binary resumes only its own journal format; earlier formats have no migration.

## Durable data

```luau
local findings = { file = "src/config.rs", timeout = 30 }
durable.set("findings", findings)
findings.timeout = 99
assert(durable.get("findings").timeout == 30)
```

Values are copied at both boundaries. The accepted values are finite numbers, strings, booleans, null, contiguous arrays and string-keyed objects. Functions, userdata/host handles, arbitrary metatables, cycles and mixed-key tables are rejected. Native arrays carry a private inert tag so empty JSON arrays remain arrays through subsequent calls and checkpoints; a plain Luau `{}` denotes an object. A missing key returns nil, and `durable.set(key, nil)` deletes the key. Checkpoints store object keys that start with `$` with one more `$`, so any key round-trips and saved data can never name another blob of the run store. A chunk that returns a Lua error is still completed; its saved-data changes can commit with its error result. A process interrupted before checkpoint commit loses those changes. Filesystem and remote effects are not rolled back.

Copies stop during traversal at 64 levels, 262144 nodes or a conservative 16 MiB encoded-size bound. Shared subtrees count for every copied occurrence. The entire durable store has the same limits, checked against a running total so an update validates only its own value; rejected updates preserve its previous contents.

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

Reads return at most 4096 UTF-8-safe text bytes (`bytes` can lower this). Offsets are zero-based byte offsets; offsets inside a character fail. Serialized responses are capped at 12 KiB, including escaping. List snippets are at most 200 bytes. A search examines at most 128 entries and charges at most 64 MiB of checkpoint source bytes per call: an entry reads only the blobs on its path in the checkpoint tree (never an image or provider field), and each blob is charged its size once per call, including on a cache hit. A page ends before an entry that would exceed the allowance. Follow `next` until absent; `scan_complete=false` means more candidates remain, even if the page has no matches. Cursors bind the run, query and initial archive frontier.

The archive covers only recorded root compactions in this run, including earlier RPI stages and before resume. It excludes live messages, un-compacted remainders of retired stages, child lanes, branch-parent journals and native operation receipts. Children and isolated package VMs have no `history` global. No supported archive records returns `status="unavailable"`; an available archive without matches returns `status="ok"` with empty entries. Legacy journals remain resumable without backfilling history or changing their frozen modules and notices.

Successful compaction emits an internal `next` command with `archive={kind="compaction",cut=old_cut}`. Rust validates the source conversation, summary and retained suffix before commit. The same synced checkpoint record commits both the new state and a versioned archive descriptor referencing the preceding root checkpoint. Entry IDs derive from that record's sequence and source message ordinal, so identical messages remain distinct and IDs survive resume. Custom controllers that discard context without this directive do not create archive entries.

Checkpoint trees remain the only authoritative bodies. A disposable descriptor index is rebuilt from the verified journal and grows with recorded compactions; a cache holds at most 8 MiB of encoded source bodies. Retrieval runs in a blocking worker with deadline checks, read authority and workflow admission. Missing, corrupt or individually oversized blobs (over 64 MiB) fail explicitly. Lookup never replays historical chunks.

## Dispatch and uncertainty

Every mutating effect records a synced intent before its executor runs. Approval comes first: path permissions and the approval gate for shell commands, MCP calls and HTTP tools are asked before the intent is written, so a refusal, a Ctrl-C at the prompt or the run deadline while it is open leaves a definite non-effect (a `call` receipt marked `denied` or with its error), never an uncertain operation. Each intent names who approved it (`approval`: `not-required`, `policy-allow`, `auto` or `prompt`), and `hx log verify` rejects one that does not. Outcomes include full argument hashes and result receipts. A global mutation permit serializes mutations; sealed read batches may run concurrently. Subagents and packages retain or reduce their caller's authority.

There is at most one application-level dispatch attempt for an operation ID. A provider that answers without a generation is classified: a 4xx or 503 means nothing was generated, so its reservation is released; a 408, 5xx or malformed 200 body gets one new request under a new operation ID, and a 429 that generated nothing up to eight, backing off 2, 4, 8 … s when the provider names no delay (honouring `Retry-After`, capped at 30 s). Each class keeps its own allowance, so a server error after waited-out rate limits still gets its new request. A failed attempt that may have been billed (a lost response, a 500, 502 or 504) is charged its reserved tokens, recorded as a `billing` record with `"source": "hx"`; a lost response then gets one new request under a new operation ID. A reservation left open by a crash is charged the same way on resume or branch. Underlying services may have their own internal behavior; hx does not establish exactly-once remote execution.

An intent without an outcome is uncertain. Local `write` and `edit` settle themselves on resume or branch: each journals the hash of the content it is about to produce (`expect`) and replaces the file atomically (temp file, fsync, rename), so the file holds either its old content or exactly that. hx records a `resolution` with `"by": "hx"` and its evidence: `complete` when the file's hash matches, `abandon` when no `expect` was journaled or the file holds its previous content. An unreadable file is left for the owner.

Shell commands, MCP calls and HTTP tools can act outside the workspace. Only a failure after the request may have reached the other side without a definite answer (a lost or unreadable response, a timeout after sending, a connection reset mid-response, an HTTP 5xx) leaves the operation uncertain. A JSON-RPC error, an HTTP 4xx and a failure before sending (a refused connection, a stdio server that already exited) are ordinary errors. If interrupted, resume allows inspection but blocks new mutations until the owner reconciles them:

```bash
hx log verify LOG
hx recover LOG OP --decision complete --yes
hx recover LOG OP --decision abandon --yes
```

`LOG` is the run's journal: the report's `log` field, the path the recovery notice and the refusal name, or a run id from `hx runs`.

Record a decision based on the actual external state; it dispatches nothing and is final. After `abandon`, the resumed agent sees the receipts and can issue the operation again if it is still needed.

Journal or blob failure is fatal to effect admission, including when Luau catches the returned error. A separate OS lock allows only one writer/resumer. Hash chains and blob hashes detect corruption; they are not authentication against an owner who can rewrite the entire run store. The run store is out of the agent's reach: it lives outside the workspace in `$XDG_STATE_HOME/hx` (default `~/.local/state/hx`, one directory per workspace), sealed from file tools in every mode and, on Linux, neither readable nor writable by shell commands; no write root may overlap it. In the workspace, a shell command could append to its own journal (even from a process left running after hx exits) or read its manifest.

## Resume and branch

```bash
hx run --workspace PROJECT --resume LOG
```

The manifest restores provider/model settings, context/output limits, extra packs, budgets, policy (including autopilot and approval mode) and exact module/package bytes, including the standard library and system prompt. Repeating these flags on resume is an error. Credential values are never stored; the named environment variables must still be available. A rebuilt binary can resume a run of the same journal format (it prints a note): the workflow's Luau comes from the manifest, and old chunks are never re-executed. Workspace configuration edits do not alter an existing run. Temporary file-tool path grants are recorded as `path_grant` journal entries and restored on resume. They are shared by the run's descendants and never copied to a new branch.

The latest committed controller checkpoint and durable data are restored into fresh VMs. An interrupted chunk's code and operation receipts are supplied to a recovery turn; pending historical tool calls are closed with an interruption result. The old chunk is not re-executed. Receipts reference content-addressed blobs in the protected log directory. Ordinary globals, closures and scratch files are not restored. A finished run returns its saved report without another provider call.

To choose current approved extensions, new policy, a new binary or other settings, explicitly branch:

```bash
hx run --workspace PROJECT --branch LOG \
  --model MODEL --key-env OPENROUTER_API_KEY
```

A branch inherits committed durable data and the task unless a positional prompt or `--prompt-file` supplies a new task. It starts a new conversation and budget under a new manifest. Reconcile uncertain effects first. Previous receipts remain in the parent journal; branching does not undo the parent's workspace changes.

The branch's parent identity and inherited data are committed in its manifest, so recovery before the first controller checkpoint retains the seed data. Revocation blocks execution while still permitting the owner to reconcile effect receipts, then branch.

## Extensions and capabilities

Approved package bytes and manifests are captured together at launch. A captured package's hash must match its approval, and its capabilities are the ones its approved manifest requests. Symlink-containing extension trees are refused. Skills' supporting files are copied into the protected run store (`registry/SESSION/` beside the journal), the one part of it the run's file tools and shell commands may read, never write. Runtime resolution does not reread mutable workspace modules.

Each package method call gets a fresh VM and a Rust principal. Native effects check that principal at dispatch. Imports are restricted to the package's own frozen files; privileged controller modules and other packages cannot be imported. Arguments/results cross as data, never function references. A read-only descendant cannot regain write authority by calling a more privileged package. `proc.sh` permits shell actions within the externally supplied process environment; it is broader than `fs.write` alone.

Single-file tools in `.harness/tools/NAME.{lua,luau}` use the same invocation boundary. Workspace trust covers their source and grants their declared capabilities; no package manifest or lock entry is needed. Registration evaluates definitions without effects under a one-second CPU deadline and a 16 MiB memory limit. A namespace is exposed only after every method, signature, endpoint and required credential validates. Invalid files produce diagnostics without activating partial namespaces. Source, method metadata and fixed HTTP endpoint grants are frozen in the registry; successful registrations contribute signatures to the model prompt.

Local HTTP endpoints carry an explicit read-only classification (default: external). Requests use the supervisor's authorization, journal, deadline and mutation gate. Endpoint declarations grant network access to their fixed URLs subject to the environment's network access; redirects and automatic retries are disabled. Rust resolves named credentials from the environment, workspace secrets, then user secrets, and attaches them to headers. Secrets are not part of the frozen registry and may rotate; resume checks them again. File tools cannot read `secrets.toml`; on Linux, Landlock keeps shell commands and MCP servers from reading `.harness/secrets.toml` and `~/.config/hx/`. Credential echoes in JSON responses and HTTP error excerpts are redacted before receipts are written.

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

The complete batch is validated and copied before dispatch. It accepts at most 32 independent read/search/read-only-agent operations. Results are published together in input order; search results also carry a `truncated` flag. Each read or search runs in its own VM under a read-only principal and may wait for an owner's path approval like its unbatched form; each counts in the chunk's `ops`. An agent operation records the same intent and receipt as `agent()`. Mutating shells, writable subagents, shared globals and arbitrary closures are excluded. `parallel{function ...}` is removed.

## Workflow customization

Override `std/loop` with `init(config)` and `step(state, event) → state, command`. Rust accepts only known commands and supplies no ambient effect authority to the controller. Rust reads only the commands and the state's `messages` array (for the trace and to attach image notifications to an agent request); the rest of the state belongs to the controller. A failed model call arrives as a `model_error` event whose `error_kind` says what happened (`budget_exceeded`, `context_rejected`, `images_rejected`, `provider` or `unanswered`); its `error` text is for display, so branch on the kind. The old imperative `hx.run`, `hx.llm` and `hx.tree` helpers have been removed. Controller state must pass the same data-only boundary as durable values.

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

The `std/loop.advance` adapter returns commands, prose candidates, native submissions or terminal stops. Only candidates and submission corrections may continue through `std/loop.continue`; a terminal worker cannot be revived. RPI disables the optional recheck heuristic explicitly. Plain prose never completes an RPI stage. Three submission correction attempts stop that stage honestly; only a refusal by `stage.submit` itself counts (the host flags it), not other chunk errors. A restarted stage (an answered input gate, an owner revision, a redesign or a fresh verification) gets its cause in a `<restart-cause>` block plus the documents behind it: its draft, the reviewed document, or the failing verification and repair record. A repair packet presents only the latest verification as the rejection. Root turn counts and all existing admission budgets span fresh workers.

### Submission and handoff

During a root stage, the agent can call `stage.submit` inside the Luau tool. The payload contains outcome (`complete`, `needs_input` or `blocked`), the full Markdown document, summary, evidence and stage-specific fields. Program design supplies a structured, nonempty check plan, and a workspace stage that submits checks is refused as a correctable error; verification cites actual numeric shell operation IDs and explicit findings. Descendants and isolated package invocations cannot submit for the root.

The host validates the generic payload and successful-chunk provenance, stores document bytes in a content-addressed blob and gives the controller a submission ID. Effects after submission and a second submission in that chunk are refused; remaining calls from the same model reply are discarded. A chunk error invalidates its prepared submission. Printed text, hook output and saved data cannot manufacture native provenance.

The pure controller returns one generic `boundary`: seal a submission and start a stage, pause at a gate, or finish. The host commits the new controller state, execution frame, artifact references, lifecycle and pending delivery together. Administrative delivery is incorporated before admitting new work. A crash before commit leaves the previous boundary authoritative; a crash after commit restores the pending delivery. Recovery never replays the old chunk.

A new stage replaces the root agent VM and worker transcript. The host supplies verified artifact documents for the declared input refs, workspace metadata and the immutable ordered owner-input ledger. Latest explicit owner feedback takes precedence over earlier assumptions. The stage conversation is kept once, as the outer controller's `messages`, which the host extends with images; the nested worker borrows it for each step, so saved state and its 16 MiB bound hold no second copy. Stage-local state and hooks reset; run accounting and user durable data remain separate.

### Stage permissions

An `inspect` stage ceiling applies during research and design, transitively to child agents and packages. It permits existing authorized reads, searches, views, agents and MCP listing; HTTP/MCP calls must have a trusted read classification. It denies shell, project writes/edits and module promotion regardless of command text. This ceiling intersects existing principals and grants; batch descendants keep their stricter read-only policy. Workspace stages retain ordinary policy. Artifact control writes are confined host bookkeeping rather than expanded project authority.

### Owner gates and immutable documents

Manual mode gates research questions, research, system design, program design and final acceptance. Automatic mode omits those routine reviews but still creates real input gates for essential missing information or mandatory human validation. Pauses report `finished:false`; resuming an unanswered gate makes no provider call. Review decisions are owner CLI actions:

```bash
hx review LOG --gate GATE_ID --decision approve
hx review LOG --gate GATE_ID --decision revise --input-file feedback.md
hx review LOG --gate GATE_ID --decision answer --input-file answers.md
hx review LOG --gate GATE_ID --decision waive --input-file reason.md
hx run --resume LOG
```

Each gate declares its allowed decisions and exact artifact/check scope. Revision creates a new artifact version and invalidates downstream dependencies. An input answer resumes the relevant work; an arbitrary answer does not establish a passing check. Explicit waivers remain separate from passing evidence. Final acceptance commits without another model call, including after the execution deadline, subject to the host's source freshness and revocation checks. Resuming executable work still requires the run's existing budgets and deadline. Review decisions are recorded and consumed durably; recording one never dispatches work.

Artifacts are immutable host records with dependency refs and content hashes. `.agents/artifacts/rpi-RUN_ID/` contains readable projections, starting with `01-request.md`. Missing or edited projections are regenerated from canonical blobs and never become approved inputs. A projection failure does not change canonical authority. Resume restores the frozen workflow and rejects new launch configuration. A branch starts a new workflow with distinct artifacts and no inherited approvals.

### Verification scope

Program design freezes check IDs, exact commands, cwd, timeout and requirement mappings. Verification evidence must identify successful same-stage shell receipts matching those invocations. Failed exit status, timeout, substituted commands, missing receipts and source changes cannot satisfy a planned check. Re-verification requires fresh receipts. Source/check inventory observations bind verification and final acceptance to the declared bounded project scope: Git tracked and nonignored files where available, or a bounded file walk, with explicit check inputs. Runtime logs and artifact projections are excluded. Submodules and nested Git repositories appear as `unobserved:repository` entries: their presence counts, their files do not. Launch observes the inventory once and refuses entries it cannot observe; if one appears later, verification fails with that reason instead of the process. Each observation is stored once as a blob; shell records, checkpoints, prompts and the acceptance gate carry its digest and blob reference. A verify-stage shell intent reuses the previous record's observation when no other effect intent intervened, so each check hashes the project once. This is an observation of that inventory, not a hermetic build or a guarantee against all external races; ignored build outputs and external dependencies require deliberate check/input coverage.

Adapted phase instructions and the upstream MIT notice are embedded with the binary; [provenance](../stdlib/std/rpi/PROVENANCE.md) records the pinned source and adaptation. There is no startup download or independent plugin installation. Trusted overrides affect new runs while generic host lifecycle, policy and provenance checks remain enforced.

## Autoresearch workflow

`hx run --ar --prompt-file PROGRAM` searches for a better score under `.harness/ar.toml` (covered by `hx trust`). It requires Git and a committed workspace apart from the editable paths. `--rpi`, `--branch`, `--trace`, `--max-tokens`, `--max-wall` and `--max-turns` are refused: episodes are limited in model turns and wall-clock time, and the run ends by its own progress rule or `max_rounds`. All other run flags pass through to the episodes, and the model flags also to the orchestrator's own idea calls. `hx run --ar --resume RUN` continues a stopped search with its frozen launch settings, and `--resume RUN --finish` writes its result from the last gate without starting anything (`RUN` is the run directory or its id; `hx runs` lists both).

```toml
editable = ["ar/strategy.pine"]                  # files or directories; the only thing that leaves an episode
eval = "$HOME/grader/eval search --file ar/strategy.pine"
select = "$HOME/grader/eval select --file ar/strategy.pine"   # optional, held out
grader = ["~/grader/bin", "~/grader/search"]     # owner data roots, sealed from agents
holdout = ["~/grader/select"]                    # held-out data: readable only by the select evaluator
direction = "max"                                # or "min"
timeout = 120                                    # seconds per evaluation (default 600)
min_delta = 0.05                                 # the smallest lift that counts (default 0)
episode_secs = 900                               # wall-clock cap per episode (default 900, at least 60)
workers = 8                                      # episodes side by side per round (1 to 32, default 8)
max_rounds = 20                                  # rounds at most (default 20)
eval_jobs = 4                                    # evaluators at once across the run (default: half the CPUs)
```

**Isolation.** At launch, HEAD's tree is frozen into the run's `base` (without `.harness`), byte for byte from the object store, so export attributes do not apply and submodules are not included. On Linux, Landlock keeps every episode and evaluator in its own throwaway copy; network access is up to the environment.

- Each episode is a fresh child `hx run` in a throwaway copy of `base` holding the state it starts from, with no Git history and no `.harness` config, and its temp directory and journal inside that copy. It never touches the run directory: `ar.eval`, `ar.reset` and `ar.verdict` are requests to the orchestrator over a channel only the child process holds (no shell command inherits it), and the orchestrator scores, records and answers. When the child ends, the orchestrator keeps its journal with the run and deletes the copy; nothing but snapshots of the editable paths carries over.
- `ar.eval(note)` snapshots the editable files (symlinks and special files skipped; at most 16 MiB, 10,000 files and 32 levels, or that eval is refused), opening each entry without following links, and the orchestrator stores each file's mode and bytes (bytes once per content, shared across snapshots) and runs `eval` in a second throwaway tree holding exactly those files, executable bits included: an overlay over `base` where the system allows it, else a copy. A process the agent leaves running cannot change what is scored. Each snapshot is scored once per evaluator; asking again returns the recorded score.
- Evaluators run with a scrubbed environment. With Landlock each reads only its copy, the system and toolchain directories and its `grader` roots (the `select` evaluator also its `holdout` roots), and writes only its copy. Every evaluator and episode shell command runs in fresh user, pid, mount and IPC namespaces (evaluators also with a loopback-only network): it sees only its own processes in a private `/proc`, gets a private `/dev/shm`, and everything it leaves running dies with it, so candidate code next to held-out data cannot hand anything to a later agent. Where user namespaces are forbidden, each gets a cgroup killed whole when it ends and Landlock signal and socket scoping, without `/dev/shm`; with neither, they refuse to run. A failed evaluator's output never reaches the agent, only how it failed (exit status, signal, timeout); the owner finds the output in the run's `failures/`. At most `eval_jobs` evaluators run at once, so evaluations side by side do not skew each other's timings. When editable code runs inside the evaluator, it can print its own result line, so such an evaluator must run candidates in a subprocess and parse their output itself.
- The run directory is in the state directory, which every policy seals from file tools in every mode, including `--yes`, and which confined shell commands neither read nor write; only the orchestrator writes it. Each child's policy also seals the `grader` and `holdout` roots and its own journal; its record of the episode leaves out the `select` command.
- Without Landlock none of this confinement holds, so a spec with `select`, `grader` or `holdout` refuses to launch or resume unless the owner passes `--ar-unconfined`.

**Manager loop.** Before any episode the host scores the seed with the search evaluator (and the select evaluator, with `select`); a seed either cannot score stops the run. A research episode then writes the brief every later episode gets. Rounds of `workers` episodes follow, each ended by a gate:

- *Ideas.* Before a round, the orchestrator plans each worker's idea with the episodes' model: one String Seed of Thought call per worker, each from its own OS seed, side by side, then one call that compares the batch with itself and with the ideas already taken. A duplicate is sampled again with everything accepted listed as taken, at most twice; then it is accepted. These calls are admitted and journaled like any run's model calls. A worker cannot change its idea; one that finds it cannot work says so in its final answer.
- *Explore.* Each worker builds and fixes its planned idea but does not tune it; each idea is a candidate. Ideas of earlier rounds are taken while their candidate holds a valid state.
- *Gate.* A pure function of what is journaled. Each state of a candidate counts once; only the seed's own bytes are excluded, so a state that ties the seed's score is a candidate's state like any other. A state is rejected by a failed audit or by the held-out veto (worse than the seed on `select`, or invalid there). The candidate's frontier is its best state no check rejected; it is good when it beats the seed by the margin and passed the veto. The held-out score is computed, side by side for all candidates, only for frontiers that beat the seed, and a veto moves the frontier back exactly like a failed audit. The result is the best state that beats the seed by the margin, passed the veto and passed an audit. Candidates rank good ones first, then by search score, one per distinct frontier, and the top three go on. The margin is `min_delta`, or the standard error of the difference of the two scores (√(se₁² + se₂²)) when the evaluator reports `se` and that is larger; a worker's `improved` and the progress rule use the same margin.
- *Audits.* A good frontier among the top three is audited when its candidate has no result yet (it just became good, or its earlier state failed); a candidate with a result is audited again only at the run's last gate, when its frontier beats that result by the margin. An auditor reads the state, looks for an evaluator hole with probe evaluations and records its verdict with `ar.verdict{pass, reason}`, once. An auditor that never calls it gives no verdict: its state stays unaudited and cannot become a result.
- *Exploit.* Workers are split across the chosen candidates, each starting from its frontier with the candidate's evaluations and the reasons checks rejected its states (never the held-out score), and each building a refinement planned as ideas are, distinct among those planned for the same candidate and starting state. When an exploit round moves no chosen frontier, the next round explores again.
- *Progress.* A round makes progress when it chooses a new good candidate or lifts a chosen frontier above its score at the last progress by more than the margin, so small steps add up across rounds. Two rounds in a row without progress, or `max_rounds`, end the run; then the last gate runs its audits.

**Result.** The portfolio is the distinct results of the last gate, best search score first, at most three, written to `portfolio/c<id>/`; the first goes into the workspace's editable paths. With no result the workspace is not touched. `report.json` lists the portfolio and every candidate's decision. Choosing on `select` makes its numbers optimistic, so compare finished runs on a third tier no run touches (a lockbox).

**Limits.** Every episode is limited to 40 model turns and to `episode_secs` of wall-clock time; a watchdog kills a child still running two minutes past that and records it as timed out, with the evals it made. Stopping the orchestrator (SIGINT, SIGTERM or SIGHUP) stops its episode children and the commands they started; a child whose orchestrator dies outright sees its channel close and stops the same way. An evaluation cut short by a stop records nothing. A child that ends with neither a report nor an eval has crashed and is retried, up to three crashes; when every episode of a batch crashed the next try waits 2, then 4 minutes, and with no eval in the run yet repeated crashes stop it with the child's error. A run that ends with no evals exits nonzero.

**State and recovery.** The run directory, `ar/RUN/` in the workspace's state directory (`hx runs` lists it), holds `base`, the copy of hx its episodes run (made when an orchestrator starts, so rebuilding hx mid-run changes nothing), one hash-chained journal and the content-addressed blobs beside it, each episode's task, stderr and journal, `failures/`, the portfolio and `report.json`. The journal holds the frozen launch settings and seed, the planned batches with their ideas, each episode attempt as started and as ended, each score per evaluator and snapshot, each evaluation's candidate, each verdict and each gate's choice; frontiers, results and progress are derived from it, never stored. The blobs hold snapshots, search reports and the episodes' answers. Everything is journaled before it acts: a batch whole before it starts, an episode before its child runs. The journal's lock is the run's, so a second `--resume` of a live run is refused. Resume closes the attempts a stopped orchestrator left open as interrupted, keeps their journals and deletes their copies; an interrupted attempt is never counted as a crash, so a worker that had not scored, a research episode and an auditor without a verdict run again however often the orchestrator was stopped. A request in flight when the orchestrator stopped is lost with it. Scores, verdicts and gates already journaled are reused, so a finished run replays to the same decisions without a model request. Resume takes only a run of this workspace's state directory started by an hx with the same run format; kill the orchestrator's process group, not only the orchestrator, so no child keeps running.

## Budgets and validation

One journal-backed ledger accounts for the root and all descendants. Admission atomically reserves request bytes plus the configured output cap in tokens; successful receipts settle it. Concurrent subagents are admitted side by side while reservations fit under `max_tokens`; otherwise a request waits for running ones to settle instead of failing. Settled token usage includes ordinary input, cache reads, cache writes and output; reasoning is already included in output. The limit is an admission threshold, not an exact tokenizer or billing ceiling. hx tracks no dollar cost.

A 4xx answer, including a context rejection, releases its reservation, so a definite context rejection can trigger one compaction and a new request under any threshold. A context or image rejection that arrives as an error event in place of a streamed 200 response is the same definite rejection: released, and not resent unchanged. Transport loss and failed 5xx attempts are charged their reserved tokens.

The absolute run deadline includes downtime. It is checked before admission and around every controller transition, including controller-only loops. Controller execution and queued model admission also observe it. It interrupts local VM/process/model work where possible. Cancellation cannot revoke a request already accepted by a remote service.

MCP responses have a 16 MiB received-byte limit per request across stdio, HTTP JSON, SSE events and HTTP error bodies. Ignored notifications count toward the same allowance. Oversized responses close the connection; a mutation whose response is lost remains uncertain and is never automatically redispatched.

Run `cargo test`, `hx test stdlib/std`, `python3 scripts/redesign_checks.py`, and `python3 scripts/rpi_checks.py` after building. The acceptance suite uses local scripted providers, injects crashes at actual journal boundaries, resumes repeatedly, checks dispatch counts and committed state, and exercises capabilities, concurrency, revocation, corruption and deadlines. It does not prove model-answer correctness, physical power-loss durability, exhaustive filesystem races, or live-provider behavior.
