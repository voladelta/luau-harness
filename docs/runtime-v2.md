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
| Policy, exact source bytes, package grants, model settings | Frozen manifest | One run |
| Operation IDs, receipts and budget reservations | Supervisor journal | Across restarts |
| Conversation, counters, finish-review state, pending calls | Serializable controller state | Across restarts |
| Agent globals and functions | Agent VM | Until the process exits |
| `durable` data | Supervisor checkpoint | Commits after a completed controller transition/chunk |
| Package locals and module caches | Isolated invocation VM | One invocation |

Controller transitions and package invocations start in fresh VMs. Mutable captured locals cannot become an additional workflow state store. The controller has pure builtins, frozen module imports, model metadata and diagnostic output; it cannot read files, execute processes, approve operations or invoke a provider.

Each checkpoint has a Rust-owned lifecycle: `running` carries opaque controller state and durable data; `awaiting_review` carries a durable owner gate; `finished` carries the validated final report and durable data. A `finish` command commits terminal status independently of the controller's private fields. Resuming a finished run returns its saved report even after its execution deadline. Workflow runs add optional frozen configuration and execution-frame fields. Existing format-4 runs without those fields retain the ordinary worker path and frozen sources; earlier experimental formats have no migration.

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

There is at most one application-level dispatch attempt for an operation ID. A provider that answers without a generation is classified: a 4xx or 503 means nothing was generated, so its reservation is released; a 408, 429, 5xx or malformed 200 body gets one new request under a new operation ID (honouring `Retry-After`, capped at 30 s). A failed attempt that may have been billed (a lost response, a 500, 502 or 504) is charged at its reservation when that reservation has a price bound (`"reserved_cost": {"kind": "upper_bound", "usd": amount}`), recorded as a `billing` record with `"source": "hx"`; a lost response then gets one new request under a new operation ID. An unpriced reservation stays open as uncertain billing until the owner reconciles it, even without a dollar threshold; zero reserved dollars do not mean a request was free. Underlying services may have their own internal behavior; hx does not establish exactly-once remote execution.

An intent without an outcome is uncertain. Local `write` and `edit` settle themselves on resume or branch: each journals the hash of the content it is about to produce (`expect`) and replaces the file atomically (temp file, fsync, rename), so the file holds either its old content or exactly that. hx records a `resolution` with `"by": "hx"` and its evidence: `complete` when the file's hash matches, `abandon` when no `expect` was journaled or the file holds its previous content. An unreadable file is left for the owner.

An interrupted `sh` that ran under Seatbelt with the network off, and was not an approval-gated command, could only change the workspace and the run's temp dir. hx records `unknown_local` (`"by": "hx"`): it may have run partly or fully, and the recovery turn inspects the workspace before repeating it.

Other mutations (`sh` under `--yolo` or with network, MCP calls) can act outside the workspace, so resume allows inspection but blocks new mutations until the owner reconciles them:

```bash
hx log verify .harness/log/RUN.jsonl
hx recover .harness/log/RUN.jsonl OP --decision complete --yes
hx recover .harness/log/RUN.jsonl OP --decision abandon --yes
```

Record a decision based on the actual external state; it dispatches nothing and is final. After `abandon`, the resumed agent sees the receipts and can issue the operation again if it is still needed.

Uncertain model calls with an admission allowance (`"reserved_cost": {"kind": "allowance", "usd": amount}`) retain their reservations. The amount controls admission and is not evidence of a charge. After checking provider usage, the owner can settle one explicitly:

```bash
hx recover .harness/log/RUN.jsonl OP --decision billing --tokens 1234 --cost 0.02 --yes
```

That command records asserted usage and releases the reservation; it never sends a provider request.

Journal or blob failure is fatal to effect admission, including when Luau catches the returned error. A separate OS lock allows only one writer/resumer. Hash chains and blob hashes detect corruption; they are not authentication against an owner who can rewrite the entire run store.

## Resume and branch

```bash
hx run --workspace PROJECT --resume PROJECT/.harness/log/RUN.jsonl
```

The manifest restores provider/model settings, context/output limits, extra packs, budgets, policy and exact module/package bytes, including the standard library and system prompt. Repeating these flags on resume is an error. Credential values are never stored; the named environment variables must still be available. A rebuilt binary can resume a run of the same journal format (it prints a note): the workflow's Luau comes from the manifest, and old chunks are never re-executed. Workspace configuration edits do not alter an existing run.

The latest committed controller checkpoint and durable data are restored into fresh VMs. An interrupted chunk's code and operation receipts are supplied to a recovery turn; pending historical tool calls are closed with an interruption result. The old chunk is not re-executed. Receipts reference content-addressed blobs in the protected log directory. Ordinary globals, closures and scratch files are not restored. A finished run returns its saved report without another provider call.

To choose current approved extensions, new policy, a new binary or other settings, explicitly branch:

```bash
hx run --workspace PROJECT --branch PROJECT/.harness/log/RUN.jsonl \
  --model MODEL --api completions --key-env OPENROUTER_API_KEY
```

A branch inherits committed durable data and the task unless `--prompt-file` supplies a new task. It starts a new conversation and budget under a new manifest. Reconcile uncertain effects and model billing first. Previous receipts remain in the parent journal; branching does not undo the parent's workspace changes.

The branch's parent identity and inherited data are committed in its manifest, so recovery before the first controller checkpoint retains the seed data. Revocation blocks execution while still permitting the owner to reconcile effect receipts and billing, then branch.

## Extensions and capabilities

Approved package bytes, manifests and grants are captured together at launch. A captured package's hash must match its approval. Symlink-containing extension trees are refused. Skills' supporting files are copied into the protected run store. Runtime resolution does not reread mutable workspace modules.

Each package method call gets a fresh VM and a Rust principal. Native effects check that principal at dispatch. Imports are restricted to the package's own frozen files; privileged controller modules and other packages cannot be imported. Arguments/results cross as data, never function references. A read-only descendant cannot regain write authority by calling a more privileged package. `proc.sh` permits shell actions within the run's OS policy; it is broader than `fs.write` alone.

Single-file tools in `.harness/tools/NAME.{lua,luau}` use the same invocation boundary. Workspace trust covers their source and grants their declared capabilities; no package manifest or lock entry is needed. Registration evaluates definitions without effects under a one-second CPU deadline and a 16 MiB memory limit. A namespace is exposed only after every method, signature, endpoint and required credential validates. Invalid files produce diagnostics without activating partial namespaces. Source, method metadata and fixed HTTP endpoint grants are frozen in the registry; successful registrations contribute signatures to the model prompt.

Local HTTP endpoints carry an explicit read-only classification (default: external). Requests use the supervisor's authorization, journal, deadline and mutation gate. Endpoint declarations grant network access to their fixed URLs independently of shell network policy; redirects and automatic retries are disabled. Rust resolves named credentials from the environment, workspace secrets, then user secrets, and attaches them to headers. Secrets are not part of the frozen registry and may rotate; resume checks them again. Sandboxed code cannot read `secrets.toml`. Credential echoes in JSON responses and HTTP error excerpts are redacted before receipts are written.

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

`hx run --ar --prompt-file PROGRAM --max-cost B` searches for a better score under `.harness/ar.toml` (covered by `hx trust`). It requires Git and a committed workspace apart from the editable paths. `--rpi`, `--branch`, `--trace`, `--max-tokens` and `--max-wall` are refused. All other run flags pass through to the episodes. `hx run --ar --resume .harness/ar/RUN` continues an interrupted search with its frozen launch settings.

**Episodes.** Each episode is a fresh child `hx run` (a flush: a new conversation, the workspace restored to the lineage's best state). Its task is the program, the protocol, and the lineage's ledger: its ancestors' rows up to each fork point and its own rows, never a sibling's. The child gets `ar.eval(note)`. That call refuses to run when anything outside the editable paths differs from Git HEAD (ignored files and `.harness/` excepted). Otherwise it stores a content-addressed snapshot of the editable files, runs the `eval` command, appends a ledger row and returns `{n, score, valid, se, improved, best, report}`. A result improves the best only by more than `max(min_delta, se)`. When an episode stops with at least 15% of its allowance left, a fresh child continues the same lineage, up to four attempts, and an attempt without an eval ends the episode.

**Schedule.** Three rounds of four branches start at the seed and then fork from the leader's best state. Each branch gets 6% of the budget. After each round, the branch whose best state scores best on `select` becomes the leader and continues for an equal share of the remainder. At the end, the three round winners' final states compete on `select` once more, and the winner is restored into the workspace. Costs are the children's reported `cost_usd`. A child that dies without a report is charged what its journal recorded, with unsettled requests at their reservation, or its whole allowance when no journal is found. A child that dies before any eval (usually provider rate limits) is retried after 2, 4 and 8 minutes.

The schedule is a fixed plan of fork, episode and selection steps. `HX_AR_SCHEDULE=single` replaces it with one lineage that gets the whole budget; it exists to compare against fork-and-flush at equal cost.

**Authority.** Agents never call `select`. `.harness/ar` is sealed: in every mode, including `--yolo`, `read`, `search`, `view` and `sh` can neither read nor write it, so sibling snapshots and held-out scores stay out of reach. Evaluators are owner commands that run outside the agent sandbox with the workspace as cwd. Agents can write anything in the workspace, including ignored build outputs, so keep the grader and its data outside it.

**State and recovery.** `.harness/ar/RUN/` holds `launch.json` (spec, program, child flags, budget, schedule), `journal.jsonl`, `ledger.jsonl`, `snaps/`, `episodes/` (task, stderr and report per child) and `report.json`. Each attempt is journaled as started before its child runs and finished after. On resume, completed steps are skipped, finished attempts count their reported cost, and an attempt that never finished is charged its whole allowance. Kill the orchestrator's process group, not only the orchestrator, so no child keeps writing.

## Budgets and validation

One journal-backed ledger accounts for the root and all descendants. Admission atomically reserves an allowance; successful receipts settle it. Under a dollar threshold, launch looks up the model's price in the endpoint's `/models` list (OpenRouter publishes it) and freezes it in the manifest. Each request then reserves its full upper bound, even when it exceeds the remaining allowance: every request byte as an input token at the highest of the prompt and cache-write prices, plus the full output cap, plus any per-request fee, at the highest pricing tier the request could reach. OpenRouter's `overrides` tiers count: time-of-day tiers always, long-context tiers only once the request's bytes reach their `min_prompt_tokens`, since a request has no more tokens than bytes. A model with variable pricing (a negative price, as `openrouter/auto` lists) is unpriced. The lookup needs no key. Concurrent subagents are admitted side by side while bounds fit. A subagent on a different model, or a run whose endpoint has no price, falls back to reserving the whole remaining allowance per request; other requests wait for it to settle instead of failing. Only settled usage and unbounded uncertain reservations stop admission. Token allowances use request bytes plus the configured output cap. Settled token usage includes ordinary input, cache reads, cache writes and output; reasoning is already included in output. Both limits are documented admission thresholds, not exact tokenizer or billing ceilings. Reservations from uncertain requests survive restarts.

A 4xx answer, including a context rejection, releases its reservation, so a definite context rejection can trigger one compaction and a new request under any threshold. Transport loss and failed 5xx attempts are charged at bounded reservations and otherwise retain them.

The absolute run deadline includes downtime. It is checked before admission and around every controller transition, including controller-only loops. Controller execution and queued model admission also observe it. It interrupts local VM/process/model work where possible. Cancellation cannot revoke a request already accepted by a remote service.

MCP responses have a 16 MiB received-byte limit per request across stdio, HTTP JSON, SSE events and HTTP error bodies. Ignored notifications count toward the same allowance. Oversized responses close the connection; a mutation whose response is lost remains uncertain and is never automatically redispatched.

Run `cargo test`, `hx test stdlib/std`, `python3 scripts/redesign_checks.py`, and `python3 scripts/rpi_checks.py` after building. The acceptance suite uses local scripted providers, injects crashes at actual journal boundaries, resumes repeatedly, checks dispatch counts and committed state, and exercises capabilities, concurrency, revocation, corruption and deadlines. It does not prove model-answer correctness, physical power-loss durability, exhaustive filesystem races, or live-provider behavior.
