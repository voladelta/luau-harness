# Runtime v2: explicit authority and recovery

The agent uses one persistent Luau VM for a live run. Rust owns the frozen run manifest, operation admission, journal, budgets and checkpoints. Recovery creates a fresh agent VM; it never executes historical chunks to rebuild memory.

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

## Durable data

```luau
local findings = { file = "src/config.rs", timeout = 30 }
durable.set("findings", findings)
findings.timeout = 99
assert(durable.get("findings").timeout == 30)
```

Values are copied at both boundaries. The accepted values are finite numbers, strings, booleans, null, contiguous arrays and string-keyed objects. Functions, userdata/host handles, arbitrary metatables, cycles and mixed-key tables are rejected. Native arrays carry a private inert tag so empty JSON arrays remain arrays through subsequent calls and checkpoints; a plain Luau `{}` denotes an object. A missing key returns nil. A chunk that returns a Lua error is still completed; its saved-data changes can commit with its error result. A process interrupted before checkpoint commit loses those changes. Filesystem and remote effects are not rolled back.

Copies stop during traversal at 64 levels, 262144 nodes or a conservative 16 MiB encoded-size bound. Shared subtrees count for every copied occurrence. The entire durable store has the same limits; rejected updates preserve its previous contents.

## Dispatch and uncertainty

Every mutating effect records a synced intent before its executor runs. Outcomes include full argument hashes and result receipts. A global mutation permit serializes mutations; sealed read batches may run concurrently. Subagents and packages retain or reduce their caller's authority.

There is at most one application-level dispatch attempt for an operation ID. The HTTP and Claude adapters do not automatically retry lost generations. Underlying services and the Claude CLI may have their own internal behavior; hx does not establish exactly-once remote execution.

An intent without an outcome is uncertain. Resume allows inspection but blocks new mutations until the owner reconciles it:

```bash
hx log verify .harness/log/RUN.jsonl
hx recover .harness/log/RUN.jsonl OP --decision complete --yes
hx recover .harness/log/RUN.jsonl OP --decision abandon --yes
hx recover .harness/log/RUN.jsonl OP --decision retry --yes
```

Record a decision based on the actual external state. `retry` dispatches nothing: it permits a single subsequent operation with the original tool and argument hash. The new intent records `retry_of`. Other mutations are refused while this exact retry is pending. Before the linked intent is dispatched, the owner can replace that authorization with `complete` or `abandon`. Both assertions remain in the journal.

Uncertain model calls retain budget reservations. After checking provider usage, the owner can settle one explicitly:

```bash
hx recover .harness/log/RUN.jsonl OP --decision billing --tokens 1234 --cost 0.02 --yes
```

That command records asserted usage and releases the reservation; it never sends a provider request.

Journal or blob failure is fatal to effect admission, including when Luau catches the returned error. A separate OS lock allows only one writer/resumer. Hash chains and blob hashes detect corruption; they are not authentication against an owner who can rewrite the entire run store.

## Resume and branch

```bash
hx run --workspace PROJECT --resume PROJECT/.harness/log/RUN.jsonl
```

The manifest restores provider/model settings, context/output limits, extra packs, budgets, policy and exact module/package bytes. Do not repeat these flags on resume. Credential values are never stored; the named environment variables must still be available. The same binary is required. Workspace configuration edits do not alter an existing run.

The latest committed controller checkpoint and durable data are restored into fresh VMs. An interrupted chunk's code and operation receipts are supplied to a recovery turn; pending historical tool calls are closed with an interruption result. The old chunk is not re-executed. Receipts reference content-addressed blobs in the protected log directory. Ordinary globals, closures and scratch files are not restored. A finished run returns its saved report without another provider call.

To choose current approved extensions, new policy, a new binary or other settings, explicitly branch:

```bash
hx run --workspace PROJECT --branch PROJECT/.harness/log/RUN.jsonl \
  --model MODEL --api openai --key-env OPENROUTER_API_KEY
```

A branch inherits committed durable data and the task unless `--prompt-file` supplies a new task. It starts a new conversation and budget under a new manifest. Reconcile uncertain effects and model billing first. Previous receipts remain in the parent journal; branching does not undo the parent's workspace changes.

The branch's parent identity and inherited data are committed in its manifest, so recovery before the first controller checkpoint retains the seed data. Revocation blocks execution while still permitting the owner to reconcile effect receipts and billing, then branch.

## Extensions and capabilities

Approved package bytes, manifests and grants are captured together at launch. A captured package's hash must match its approval. Symlink-containing extension trees are refused. Skills' supporting files are copied into the protected run store. Runtime resolution does not reread mutable workspace modules.

Each package method call gets a fresh VM and a Rust principal. Native effects check that principal at dispatch. Imports are restricted to the package's own frozen files; privileged controller modules and other packages cannot be imported. Arguments/results cross as data, never function references. A read-only descendant cannot regain write authority by calling a more privileged package. `proc.sh` permits shell actions within the run's OS policy; it is broader than `fs.write` alone.

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

Override `std/loop` with `init(config)` and `step(state, event) → state, command`. Rust accepts only known commands and supplies no ambient effect authority to the controller. The old imperative `hx.run`, `hx.llm` and `hx.tree` helpers have been removed. Controller state must pass the same data-only boundary as durable values.

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

## Budgets and validation

One journal-backed ledger accounts for the root and all descendants. Admission atomically reserves an allowance; successful receipts settle it. Under a dollar threshold, one outstanding request reserves the remaining allowance because hx has no universal per-request price bound. Token allowances use request bytes plus the configured output cap. Both limits are documented admission thresholds, not exact tokenizer or billing ceilings. Reservations from uncertain requests survive restarts.

HTTP errors, including context rejection, conservatively retain reservations. With a dollar threshold, automatic compaction can therefore stop at admission until the owner reconciles billing. Without that blocked allowance, a definite context rejection may trigger one compaction and a new request; transport loss is never automatically retried.

The absolute run deadline includes downtime. It is checked before admission and interrupts local VM/process/model work where possible. Cancellation cannot revoke a request already accepted by a remote service.

Run `cargo test`, `hx test stdlib/std`, and `python3 scripts/redesign_checks.py` after building. The acceptance suite uses local scripted providers, injects crashes at actual journal boundaries, resumes repeatedly, checks dispatch counts and committed state, and exercises capabilities, concurrency, revocation, corruption and deadlines. It does not prove model-answer correctness, physical power-loss durability, exhaustive filesystem races, or live-provider behavior.
