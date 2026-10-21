<p align="center">
  <img src="docs/banner.png" alt="hx, the moon imp mascot, tries to fly a git push paper plane out of a sandbox while a crossing guard stops it" width="100%">
</p>

# hx

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange.svg)
![Sandbox: macOS](https://img.shields.io/badge/sandbox-macOS-lightgrey.svg)

A headless coding-agent harness in which the model writes Luau instead of calling tools one at a time. Rust owns effects and recovery; Luau supplies composition and workflow decisions.

```bash
cargo install --git https://github.com/voladelta/luau-harness && hx exec 'print(#search("fn main", {glob = "*.rs"}))'
```

## TL;DR

**The problem:** tool-calling agents spend one model turn per action. A turn re-sends the conversation, so a task that needs 12 searches, reads and edits costs 12 round trips.

**The solution:** hx gives the model one tool, `luau(code)`. The code runs in a persistent live VM, so one turn can search, read the hits, edit two files and run the tests. A Rust supervisor owns authorization, the journal, budgets and recovery. A separate pure Luau controller returns workflow commands. Modules and approved packages are frozen for each run.

| Feature | What it does |
|---|---|
| Code mode | One `luau` tool; globals persist across turns; `read`, `write`, `edit`, `sh`, `search` (ripgrep engine, embedded) |
| Sandbox (macOS) | `sh` runs under Seatbelt: writes confined to the project and a temp dir, no network, no API keys in the environment |
| Approval gate | `git push`, publish, `ssh` and other external commands need the user's approval; nothing the model writes can approve |
| RPI workflow | `--rpi` runs research → design → implementation → verification with up to three repair cycles; `--manual` adds durable owner review gates |
| Autoresearch | `--ar` improves a score with fresh agent episodes over the editable paths in `.harness/ar.toml`, keeping the held-out best |
| Recovery | `--resume` restores explicit saved data and controller state into a fresh VM; old effectful chunks are never replayed |
| Budgets | Shared admission thresholds reserve each request's priced upper bound (the whole remaining allowance when no price is known); an absolute deadline includes downtime |
| Compaction | On by default (`std/compact`): from 256k tokens at the latest (earlier for a smaller `--context-window`), older history becomes a structured summary; recent messages and Luau globals stay; one compact-and-retry on a context-overflow error |
| Recheck | Opt-in (`std/recheck`): the system prompt warns that the first final answer will be checked, and before it is accepted the model is sent back once to name the root cause, list the other cases it implies, test each and re-check the task's requirements. It adds turns when enabled and can hurt build tasks; enable for debugging tasks with `require("std/recheck").enabled = true` in `.harness/init.luau` |
| Overridable harness | Pure controller, prompt, printer and hooks are Luau in `stdlib/std/`; overrides apply to new runs |
| Extensions | Immutable approved packages, isolated method calls with copied data, MCP servers, agent-staged tools and module replacements that wait for your review |
| Any provider | Responses API or Chat Completions (`completions`); any compatible base URL (OpenRouter by default) |

## Quick example

`hx exec` runs Luau in one session without a model or API key, so you can see what the model sees:

```bash
hx exec 'hits = search("fn main", {glob = "*.rs"})' 'print(#hits, hits[1].file)'
```

```text
-- ops=1 error=false
1	src/main.rs
-- ops=0 error=false
```

A real turn from the model looks like this, and does in one round trip what a tool-calling agent does in four:

```luau
print(edit("src/config.py", "timeout = 5", "timeout = 30"))
write("tests/test_config.py", [==[
from src.config import load

def test_timeout():
    assert load()["timeout"] == 30
]==])
print(sh("python -m pytest -q 2>&1 | tail -15").stdout)
```

## Installation

Requires Rust 1.85 or newer (edition 2024). The sandbox requires macOS.

```bash
git clone https://github.com/voladelta/luau-harness
cd luau-harness
cargo install --path .
```

Or build without installing; the binary is `target/release/hx`:

```bash
cargo build --release
```

## Quick start

1. Set an API key. By default hx calls OpenRouter and reads `OPENROUTER_API_KEY` (override with `--base-url` and `--key-env`).

   ```bash
   export OPENROUTER_API_KEY=...
   ```

2. Write the task to a file:

   ```bash
   echo "Fix the failing test in tests/test_tax.py" > task.md
   ```

3. Run it in the project directory:

   ```bash
   hx run --workspace . --prompt-file task.md --model anthropic/claude-sonnet-5.5 --max-cost 0.50
   ```

hx prints a JSON report on stdout (turns, Luau errors, `finished`, `final_answer`, tokens, cost). The event log is written to `.harness/log/`.

Any model on OpenRouter works the same way, for example `--model deepseek/deepseek-v4.1-flash`. For another OpenAI-compatible provider, pass its URL and key variable:

```bash
hx run --prompt-file task.md --model MODEL --base-url https://api.example.com/v1 --key-env EXAMPLE_API_KEY
```

## Commands

| Command | What it does |
|---|---|
| `hx run` | Run an agent task; prints a JSON report |
| `hx review LOG --gate ID --decision approve` | Record an RPI review decision; resume separately to continue |
| `hx run --resume LOG` | Restore the saved manifest and latest checkpoint; no repeated launch flags |
| `hx run --branch LOG --model M` | Start a new run using current approved configuration and inherited saved data |
| `hx recover LOG OP --decision complete\|abandon --yes` | Record an owner's decision about an uncertain mutation; never executes it |
| `hx recover LOG OP --decision billing --tokens N --cost USD --yes` | Settle an uncertain model request's reservation using asserted usage |
| `hx revoke LOG` | Stop admitting further operations for a run |
| `hx exec CHUNK...` | Run Luau chunks in one session (no API key) |
| `hx test PKG_DIR` | Run a package's `tests/*.luau`; exits 1 on failure |
| `hx approve PKG` | Lock a project package's or skill's hash and grant the capabilities it requests |
| `hx skills` | List skills in `.agents/skills` and `~/.agents/skills` and whether each is approved |
| `hx trust [--yes]` | Review this workspace's `.harness` config and `hx.lock`, then trust it |
| `hx staged` / `hx accept NAME` / `hx reject NAME` | Review what the agent staged (tools as `NAME`, module replacements as `std/MODULE`), then activate or discard it |
| `hx rollback MODULE` | Undo the last accepted replacement of a harness module |
| `hx log verify FILE` | Check a log's hash chain and recovery accounting; exit 0 ok, 1 invalid, 3 unknown outcomes |

`hx run` flags:

| Flag | Default | Meaning |
|---|---|---|
| `--prompt-file F` | | Task text (not needed with `--resume` if the log recorded it) |
| `--rpi` | off | Activate the bundled RPI workflow in automatic mode |
| `--manual` | off | With `--rpi`, pause for research/design reviews and final acceptance |
| `--ar` | off | Autoresearch over `.harness/ar.toml`; needs `--prompt-file` (the program) and `--max-cost` (the whole search budget) |
| `--model M` | `HX_MODEL` | Model id |
| `--api A` | `responses` (`HX_API`) | `responses` or `completions` (Chat Completions) |
| `--base-url U` | `HX_BASE_URL` | API base URL; defaults to OpenRouter |
| `--key-env E` | `OPENROUTER_API_KEY` | Env var holding the API key |
| `--reasoning R` | | Reasoning effort |
| `--max-turns N` | 200 | Turn limit; the report says `stopped: "max_turns: N"` when it is hit |
| `--max-output N` | 16384 | Max output tokens per turn |
| `--context-window N` | 1000000 (assumed) | The model's context window, used by `std/compact`: compaction starts at min(~95% of it, 256k); give a smaller model's window so it compacts before an overflow |
| `--max-cost USD` `--max-tokens N` `--max-wall SECS` | none | Admission thresholds and absolute deadline; flags override `policy.toml` for a new run |
| `--provider-only A,B` | | OpenRouter only: route to these providers, no fallbacks |
| `--pack A,B` | | Extra packs: `std/authoring` (agent-written tools), `std/vision` (`view()` shows images to a vision model) |
| `--trace FILE` | | Write a readable trace |

Global flags: `--workspace DIR` (default `.`), `--yolo` (no sandbox, open network, no path limits and no approval gate; `.harness/` and `hx.lock` stay read-only; prints a warning), `--net` / `--no-net`, `--approval prompt` (ask on the terminal instead of refusing). Every command takes `--help`.

## Research, design and implementation

Use `--rpi` to carry a task through separate research-questions, research, system-design, program-design, implementation and verification stages. Each stage starts with a fresh agent session and receives the relevant immutable documents. Verification failures trigger repair and fresh verification, with at most three repair cycles across the run. This is a bundled Luau workflow adapted from [HumanLayer RPI](stdlib/std/rpi/PROVENANCE.md); activate it with `--rpi`, not `--pack std/rpi`.

Use `--ar` when the task has a score to optimize. `.harness/ar.toml` names the editable paths, the evaluator commands and the read-only roots they need; each evaluator prints a JSON line with `score` (and optionally `valid`, `se`, `report`):

```toml
editable = ["ar/strategy.pine"]
eval = "$HOME/grader/eval search --file ar/strategy.pine"    # the agent sees these scores
select = "$HOME/grader/eval select --file ar/strategy.pine"  # held out: only the controller sees these
grader = ["~/grader/bin", "~/grader/search"]                 # read-only for both evaluators
holdout = ["~/grader/select"]                                # read-only for select only
min_delta = 0.05
```

The committed tree is frozen at launch. Every episode is a fresh child run in a throwaway copy of it, and `ar.eval("hypothesis")` scores a snapshot of the editable files in another throwaway copy, under a sandbox with no credentials or network. Only the editable paths carry between episodes; the host keeps the best state, and when an agent stops a fresh one continues from it with the ledger until the budget is spent. At the end, `select` picks among the seed and every improvement. See [docs/runtime-v2.md](docs/runtime-v2.md#autoresearch-workflow).

```bash
hx run --rpi --prompt-file task.md --model MODEL
hx run --rpi --manual --prompt-file task.md --model MODEL
```

> **Repository Requirement:** `--rpi` strictly requires the workspace to be an initialized Git repository (with a `.git` root) and an appropriate `.gitignore`. The harness uses Git's inventory (`git ls-files --exclude-standard`) to track source modifications without being invalidated by compiled build outputs (`build/`, `target/`, `*.o`). Unversioned directories will be rejected at launch with an explicit error.

Automatic mode makes reasonable choices and proceeds. It pauses when essential information, an owner action or mandatory manual validation is missing. Manual mode also pauses after research questions, research, system design and program design, and for final acceptance. Review pauses return `finished:false` and the gate identity. Paused resume performs no model work until you record a valid decision:

```bash
hx review .harness/log/RUN.jsonl --gate GATE_ID --decision approve
hx run --resume .harness/log/RUN.jsonl
```

Use `--decision revise --input-file feedback.md` to revise a reviewed document, or `--decision answer --input-file answers.md` for an input gate. A validation input gate can also allow `--decision waive --input-file reason.md`; the waiver applies only to that gate's listed checks and stays distinct from passing checks. Use the actions allowed by the reported gate. `hx review` records the decision; it does not execute the task. Final acceptance needs no model call.

Documents appear under `.agents/artifacts/rpi-RUN_ID/`. They are views of journal-backed artifacts; edit requests belong in review feedback. Resume restores frozen workflow mode, sources and budgets, so do not repeat `--rpi` or `--manual` with `--resume`. A branch chooses its workflow anew and does not inherit approvals. All stages share the run's limits; review downtime counts toward its deadline.

Research and design stages can inspect source and approved read-only web tools but cannot execute shell or change the project. Implementation, verification and repair use the run's ordinary permissions. Verification checks exact planned commands, successful receipts and a bounded source inventory; this does not establish a hermetic build or protect against every concurrent external writer. See [runtime details](docs/runtime-v2.md#rpi-workflow).

## How it works

```text
            model ── one tool: luau(code) ──┐
                                            ▼
 ┌──────────────────────────────────────────────────────────────┐
 │ Persistent live agent VM · isolated package invocations     │
 │ Pure Luau controller: step(saved state, event) → commands   │
 │ Frozen modules and grants · copied data at VM boundaries    │
 └──────────────────────────────┬───────────────────────────────┘
                                │ read write edit sh search agent
 ┌──────────────────────────────▼───────────────────────────────┐
 │ Rust supervisor, trusted, not modifiable by the model        │
 │   principals · policy · Seatbelt · approvals · reservations  │
 │   single-writer journal · receipts · checkpoints · adapters  │
 └──────────────────────────────────────────────────────────────┘
```

Every admitted effect passes a Rust capability check and lands in the journal. Control hooks can deny or transform commands; they cannot grant authority. A controller-hook failure stops its transition. Observer failures are diagnostic.

Inside the VM the model also has `agent(task)`, `batch{...}` for independent reads/searches/read-only subagents, `durable.get/set` for saved data (taught in the recovery notice, since the restored conversation covers most of it), and `vars()` to list live variables. The full API is in [prompts/system.md](prompts/system.md); ownership, guarantees and migration details are in [Runtime v2](docs/runtime-v2.md).

## Configuration

All configuration lives in `.harness/` in the workspace. The model can read it but never write it.

`.harness/policy.toml` (like all workspace config, used only after `hx trust`, see below):

```toml
[fs]
read = ["/extra/read/root"]     # extra read roots for the sandbox
write = ["/extra/write/root"]   # extra write roots

[net]
allow = false                   # all-or-nothing; Seatbelt cannot filter by host

[gpu]
allow = false                   # true lets `sh` use Metal (Skia/wgpu renderers such as fframes)

[approval]
mode = "deny"                   # "deny" (headless default) or "prompt"
ask = ["terraform apply"]       # extra command substrings that need approval
allow = ["git push origin"]     # pre-approved (substring; irreversible commands need an exact match)

[budget]
max_cost = 0.50
max_tokens = 600000
max_wall_secs = 900

[packs]
enable = []
```

`.harness/mcp.toml`:

```toml
[server.notes]                  # local server, runs under its own Seatbelt profile
command = "python3"
args = ["server.py"]
read_only_tools = ["read_*"]    # these run freely; every other tool needs approval
env = ["NOTES_DIR"]             # the only env vars passed through
net = false

[server.remote]                 # remote Streamable HTTP server (https, or http on localhost)
url = "https://mcp.example.com/mcp"
bearer_env = "REMOTE_TOKEN"     # or headers_env = {"x-api-key" = "VAR"}
```

MCP tools bind as `mcp.SERVER.tool{...}`. Approve a non-read-only tool with `allow = ["mcp notes.add"]` in `policy.toml`.

## Extending

| Kind | Where | How it becomes active |
|---|---|---|
| Module override | `.harness/override/std/<mod>.luau` | On a new run; `require("base:std/<mod>")` wraps the original. Invalid controller code stops the run |
| Local tool | `.harness/tools/NAME.lua` or `NAME.luau` | `hx trust`; loads on a new run as `NAME.method(...)` after validation and credential checks |
| Tool package | `.harness/pkgs/NAME/` with `hx.toml`, `kind = "tool"` | `hx approve NAME`; model calls `tools.NAME.fn(...)` |
| Skill | `.harness/pkgs/NAME/` with `SKILL.md`, `kind = "skill"` | `hx approve NAME`; model calls `skill("NAME")` (or finds it with `skills("words")`; skills are not listed in the prompt) |
| Agent skill | `.agents/skills/NAME/SKILL.md` or `~/.agents/skills/NAME/SKILL.md` (no `hx.toml`; project shadows global) | `hx approve NAME` (`hx skills` lists them); its directory becomes read-only readable |
| MCP server | `.harness/mcp.toml` | On the next run |
| Agent-written tool | `deftool(...)` then `promote(name)` (with `--pack std/authoring`) | Staged without execution; you review, run `hx test DIR`, accept it, and start a new run |
| Agent-written module replacement | `propose_module("std/MODULE", source, {reason})` (with `--pack std/authoring`) | Staged; `hx accept std/MODULE` shows the reason, a structural check (it loads and keeps every export) and the diff, then makes it the override. `hx rollback std/MODULE` undoes it |

**Approvals.** `hx.lock` is the single approval record: per package, the approved version, its content hash and the capabilities granted to it (`[package.grants]`, where `"*"` grants every scope). Workspaces from before this format kept grants in `.harness/grants.toml`, which hx no longer reads: run `hx approve NAME` again for each package that requests capabilities.

**Workspace trust.** `.harness/{policy,mcp}.toml`, `.harness/init.luau`, `.harness/override/`, `.harness/tools/` and `hx.lock` live in the repo, so whoever wrote the repo could widen the sandbox, start MCP commands or pre-approve packages and skills. hx ignores that config until you review it with `hx trust`, which records its hash in `~/.config/hx/trust.toml` (outside the repo, protected from the model). Any edit makes it untrusted again; `hx approve`/`accept`/`rollback` keep an already trusted workspace trusted. A workspace without such files needs nothing. `HX_TRUST=1` in hx's environment trusts the workspace for one launch (tests, throwaway copies).

A package that is unapproved, or changed since approval, is invisible to a new run. Each approved package call uses a fresh VM and dispatch-time capability checks. Imports are restricted to that package; function references and host handles cannot cross its boundary. Accepted changes apply to future runs, with no hot reload. Nothing the agent can call approves source. Every module accept and rollback is appended to `.harness/upgrades.jsonl`.

### Single-file tools

Drop a file in `.harness/tools/`, then review it with `hx trust`. Its file name is the namespace, using lowercase letters, digits and underscores, starting with a letter. A file returns a definition:

```lua
-- .harness/tools/greeting.lua
return {
    tools = {
        hello = {
            signature = "hello(name: string): string",
            description = "Greet someone by name.",
            run = function(name) return "Hello, " .. name end,
        },
    },
}
```

The model can call `greeting.hello("Ada")`. All methods need a signature, description and `run` function. Successful registrations contribute their signatures to the model prompt. Invalid definitions, missing credentials, reserved names and namespace collisions reject the whole file with a warning; other tools still load. Both `.lua` and `.luau` use Luau syntax. Defining the same namespace in both files rejects both.

Registration is pure and limited to one second and 16 MiB. Each method call runs the frozen source in a fresh isolated VM with copied arguments and results; mutable locals do not persist between calls. For filesystem, shell or agent access, declare `capabilities = { fs = {"read", "write"}, proc = {"sh"}, llm = {"agent"} }`, requesting only what you need. Workspace trust grants those declarations; the existing path, sandbox, budget and approval checks still apply. Local tools cannot import privileged harness modules.

HTTP tools declare named, fixed endpoints in `http`, then call `http("endpoint", args)` inside a method. Each endpoint accepts `url`, `method` (default `GET`), `secret` (optional credential name), `header` (default `Authorization`, value sent verbatim), `bearer` (default `false`; sends `Bearer <secret>`, so store the bare token), `raw` (default `false`), and `read_only` (default `false`). A `{name}` placeholder fills one whole path segment (`/videos/{job_id}`, not `/v{id}`) from the argument of that name, percent-encoded; the origin is fixed. Remaining GET arguments become encoded query parameters; other methods send JSON. The return value is decoded JSON, or for `raw` endpoints `{text, media_type}` (UTF-8 bodies) or `{base64, media_type}` (audio, image, video and other bytes). Tool code also gets `json.encode`, `json.decode` and `base64.decode`, whose bytes can go straight to `write`. HTTP errors raise with their status and a bounded response excerpt. Requests have a 120-second timeout, an 8 MiB response limit, no retries and no redirects.

Endpoint declarations grant access to those URLs independently of shell `--net`. Mark endpoints `read_only = true` only when they read data; other endpoints use the existing approval gate, matched as `http NAMESPACE.ENDPOINT`. HTTP calls use the supervisor's authorization, run deadline and journal. Credentials are attached by Rust and never returned to tool code or saved in the run manifest. Credentials echoed in responses are redacted before journaling.

### TinyFish search and fetch

Install the included [TinyFish tool](examples/tools/tinyfish.lua):

```sh
mkdir -p .harness/tools
cp examples/tools/tinyfish.lua .harness/tools/tinyfish.lua
export TINYFISH_API_KEY='your-key'
hx trust
hx exec 'print(tinyfish.search{query="Luau embedding", include_domains="luau.org"})'
hx exec 'local r=tinyfish.fetch{urls={"https://luau.org"}}; print(r.results, r.errors)'
```

The adapter uses TinyFish's documented [Search API](https://docs.tinyfish.ai/search-api/reference) and [Fetch API](https://docs.tinyfish.ai/fetch-api/reference), preserves their response objects, and supports the documented filter and extraction options. Fetch accepts 1–10 URLs; check `errors` even when some results succeed.

Credential lookup checks, in order:

1. A nonblank environment variable.
2. `.harness/secrets.toml` in the workspace.
3. `$XDG_CONFIG_HOME/hx/secrets.toml`, or `~/.config/hx/secrets.toml`.

Secrets files contain top-level string assignments, for example `TINYFISH_API_KEY = "your-key"`. Blank values fall through to the next source. Without a usable key, the entire `tinyfish` registration fails and its methods stay unavailable. Secrets files are blocked from sandboxed reads, searches and shell commands; keep them out of version control. Keys can rotate without changing workspace trust; they are resolved again at registration and request time, including on resume.

### OpenRouter multi-modal generation and model discovery

Install the included [OpenRouter tool](examples/tools/openrouter.lua):

```sh
mkdir -p .harness/tools
cp examples/tools/openrouter.lua .harness/tools/openrouter.lua
export OPENROUTER_API_KEY='your-key'
hx trust
hx exec 'print(openrouter.credits())'
hx exec 'print(openrouter.help{model="qwen/qwen-image-2.1-turbo"})'
hx exec 'print(openrouter.models{output_modalities="video"})'
hx exec 'print(openrouter.text{model="deepseek/deepseek-chat-v3.1", prompt="Hello OpenRouter!"})'
```

- **Authentication enforcement**: Registration strictly requires `OPENROUTER_API_KEY` in the environment or secrets file; without it, registration is refused and `openrouter` is `nil`.
- **Credits balance**: `openrouter.credits()` queries `/api/v1/credits` and returns `{ total_credits, total_usage, remaining_credits }`.
- **24-hour cache for models & documentation**:
  - `openrouter.models{...}` discovers, filters, and searches 400+ models from `/api/v1/models` and caches the catalog locally for 24 hours (`force=true` bypasses).
  - `openrouter.help{model="..."}` fetches on-demand model guides (`https://openrouter.ai/{provider}/{model-id}/llms.txt`) and root documentation (`https://openrouter.ai/docs/llms.txt`) with a 24-hour local cache (`force=true` bypasses).
- **Multi-modal generation**:
  - `openrouter.text{model, prompt|messages}`: Chat completions text generation.
  - `openrouter.image{model, prompt, ...}`: Text-to-image generation; `save_to` writes every image (`name.png`, `name-2.png`, ...), returns the paths as a list and drops the base64 from the result. A failed write raises.
  - `openrouter.sound{model, input, ...}`: Text-to-speech audio synthesis; `save_to` writes the audio and omits base64 from the result. A failed write raises.
  - `openrouter.video{model, prompt, ...}` and `openrouter.poll_video{job_id}`: Video generation submission and job polling.

## Security model

- `sh` runs under macOS Seatbelt: writes only to the project and a per-run temp dir, reads from an allowlist (project, system dirs, detected toolchains, approved skill dirs), no network unless `[net] allow = true`, a minimal environment with no API keys, its own process group killed when the command ends, and a per-user process cap.
- `read`, `write`, `edit` and `search` check the canonical path against the same roots. Writes never go through symlinks.
- `.harness/` and `hx.lock` are read-only to the model.
- Shell commands are classed read / local write / external / irreversible. The classifier is a tripwire for obvious commands such as `git push`; the sandbox is the real boundary.

## Checkpoints and resume

`hx run --resume LOG` restores the frozen manifest, the latest committed controller state and `durable` data into fresh VMs; it never reruns old chunks, and ordinary globals are lost. Interrupted local `write`/`edit` and sandboxed offline `sh` settle themselves from the evidence; other uncertain effects and unbounded model reservations wait for `hx recover`. `--branch LOG` starts a new run with new settings and the inherited saved data. The log stores file contents and command output as blobs in `.harness/log/blobs/`, so treat it as sensitive. The full rules are in [Runtime v2](docs/runtime-v2.md).

## Development

Every check uses `target/release/hx`, and none needs an API key (model runs use a scripted mock).

```bash
scripts/all_checks.sh        # builds, then runs every offline suite; five-minute deadline
scripts/all_checks.sh --jobs 2  # reduce concurrent suites (default: 4)
hx test stdlib/std           # Luau unit tests
cargo test --release
```

The runner prints each suite's elapsed time and a total. Independent suites run concurrently; suites sharing mock ports run sequentially, and the process-cap sandbox suite runs alone. Mock startup waits for readiness instead of sleeping. Stalled suites fail with a diagnostic and their process groups are stopped; reaching the five-minute budget fails the run rather than skipping checks. Live provider calls stay outside this runner.

| Path | Contents |
|---|---|
| `src/` | Rust supervisor, pure controller boundary, VMs, isolated packages, batches, policy, journal/checkpoints, MCP, model adapters and CLI |
| `stdlib/std/` | The built-in `std` package (loop, prompt, printer, hooks, feedback, MCP binding), embedded in the binary |
| `prompts/system.md` | System prompt |
| `scripts/` | Check suites and mock servers |

## Limitations

- **macOS only for sandboxing.** The sandbox uses `sandbox-exec`, which Apple has deprecated. There is no Linux sandbox; on other systems you would need `--yolo`, which also removes the path limits and the approval gate, and there `sh` can write `.harness/` (on macOS `--yolo` still blocks that with Seatbelt). hx has not been tested on Linux.
- **Network is all-or-nothing.** Seatbelt cannot filter by host.
- **The process cap is per user** (`RLIMIT_NPROC`), so it bounds a fork bomb rather than isolating it.
- **Only Chat Completions was verified live** (OpenRouter and Enclave); the Responses API is tested against a mock.
- **Compaction and finish review are heuristics.** Summary quality and review quality depend on the model. Context size is configured, not detected. Resume restores it automatically. Disable compaction with `require("std/compact").enabled = false` in `.harness/init.luau` before starting a new run.
- **No benchmark.** hx ships without an eval suite; its checks cover behavior, not task success or token efficiency.
- **Recovery is explicit.** Arbitrary globals, closures and scratch files are not durable. A killed command's unrecorded output is lost; its external effect may already have happened. There is no exactly-once remote execution or rollback guarantee.
- **Budgets are admission thresholds.** Request-byte allowances are not exact token counts, provider costs may be unavailable, and cancellation cannot stop remote billing already accepted. Runtime v2 has scripted validation, not new live-provider or performance measurements.

## License

MIT. See [LICENSE](LICENSE).
