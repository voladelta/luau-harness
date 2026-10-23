# Command reference

Every command takes `--help`.

## Commands

| Command | What it does |
|---|---|
| `hx run "TASK" [--yes]` | Run an agent task; prints a JSON report |
| `hx run --resume LOG` | Restore the saved manifest and latest checkpoint; no repeated launch flags |
| `hx run --branch LOG --model M` | Start a new run using current approved configuration and inherited saved data |
| `hx review LOG --gate ID --decision approve` | Record an RPI review decision; resume separately to continue |
| `hx recover LOG OP --decision complete\|abandon --yes` | Record an owner's decision about an uncertain mutation; never executes it |
| `hx revoke LOG` | Stop admitting further operations for a run |
| `hx exec CHUNK...` | Run Luau chunks in one session (no API key) |
| `hx test PKG_DIR` | Run a package's `tests/*.luau`; exits 1 on failure |
| `hx approve PKG` | Lock a project package's or skill's hash and grant the capabilities it requests |
| `hx skills` | List skills in `.agents/skills` and `~/.agents/skills` and whether each is approved |
| `hx trust [--yes]` | Review this workspace's `.harness` config and `hx.lock`, then trust it |
| `hx staged` / `hx accept NAME` / `hx reject NAME` | Review what the agent staged (tools as `NAME`, module replacements as `std/MODULE`), then activate or discard it |
| `hx rollback MODULE` | Undo the last accepted replacement of a harness module |
| `hx log verify FILE` | Check a log's hash chain and recovery accounting; exit 0 ok, 1 invalid, 3 unknown outcomes |

## `hx run` flags

Supply one quoted positional prompt or `--prompt-file`, with flags before or after the prompt. Use `--` before a literal prompt starting with `--`. `--branch` inherits its task unless a new prompt is supplied; `--resume` restores its saved task, approval mode, autopilot setting and path grants, without repeated launch flags. `--yes` is also accepted by `hx exec`.

The workspace, private temp directory and configured file-tool roots need no confirmation. Other paths ask for a temporary grant; recognized external commands, HTTP actions and MCP tools ask unless pre-approved. No controlling terminal means refusal. `--yes` grants ordinary requests automatically while retaining protected paths, credential-read restrictions, capabilities, budgets and the scrubbed environment. It does not answer manual workflow review gates or reconcile uncertain operations. Shell commands run directly under your external sandbox. The former `--yolo` option has been removed.

hx always uses the Responses API. Requests go to `/responses` under the configured base URL.

| Flag | Default | Meaning |
|---|---|---|
| `--prompt-file F` | | Read the task from a file instead of a positional prompt |
| `--yes` | off | Autopilot: automatically approve ordinary missing file-tool permissions and external actions for this run |
| `--rpi` | off | Activate the bundled [RPI workflow](workflows.md#rpi-research-design-implement-verify) in automatic mode |
| `--manual` | off | With `--rpi`, pause for research/design reviews and final acceptance |
| `--ar` | off | [Autoresearch](workflows.md#autoresearch) over `.harness/ar.toml`; needs a prompt or `--prompt-file` (the program); runs until progress stops |
| `--model M` | `HX_MODEL` | Model id |
| `--base-url U` | `HX_BASE_URL` | Responses API base URL; defaults to OpenRouter |
| `--key-env E` | `OPENROUTER_API_KEY` | Env var holding the API key |
| `--reasoning R` | | Reasoning effort |
| `--max-turns N` | 200 | Turn limit; the report says `stopped: "max_turns: N"` when it is hit |
| `--max-output N` | 65536 | Max output tokens per turn, reasoning included; with `--context-window`, the default is at most a quarter of that window |
| `--context-window N` | 1000000 (assumed) | The model's context window, used by `std/compact`: compaction starts at 80% of it; give a smaller model's window so it compacts before an overflow |
| `--max-tokens N` `--max-wall SECS` | none | Admission thresholds and absolute deadline; flags override `policy.toml` for a new run |
| `--provider-only A,B` | | OpenRouter only: route to these providers, no fallbacks |
| `--pack A,B` | | Extra packs: `std/authoring` (agent-written tools), `std/vision` (`view()` shows images to a vision model) |
| `--trace FILE` | | Write a readable trace |

## Owner command flags

| Flag | Commands | Meaning |
|---|---|---|
| `--gate ID` | `review` | The gate to decide |
| `--decision D` | `review`, `recover` | `approve`, `revise`, `answer` or `waive` for a review; `complete` or `abandon` for a recovery |
| `--input-file F` | `review` | Feedback, answers or the waiver reason |
| `--yes` | `recover`, `trust`, `accept` | Confirm without a terminal prompt (required when stdin is not a terminal) |

## Global flags

| Flag | Meaning |
|---|---|
| `--workspace DIR` | Workspace root (default `.`) |
| `--approval prompt\|deny` | Override the configured approval mode (default `prompt`); cannot combine with `--yes` |

Environment: `HX_MODEL` and `HX_BASE_URL` set defaults for the matching `run` flags; `HX_DEBUG=1` prints the full error chain on failure; `HX_TRUST=1` trusts the workspace for one launch (see [workspace trust](configuration.md#workspace-trust)).

## Report and logs

`hx run` prints a JSON report on stdout: turns, Luau errors, `finished`, `final_answer` and tokens. The event log is written to `.harness/log/`. The log stores file contents and command output as blobs in `.harness/log/blobs/`, so treat it as sensitive.

## Checkpoints and resume

`hx run --resume LOG` restores the frozen manifest, the latest committed controller state and `durable` data into fresh VMs. It never reruns old chunks, and ordinary globals are lost.

Runs created before per-run approvals were introduced use an older journal format; start a new run with this version.

Interrupted local `write`/`edit` settle themselves from the evidence. Interrupted shell commands require owner reconciliation. Model requests whose response was lost are charged their reserved tokens. Other uncertain effects wait for `hx recover`.

`--branch LOG` starts a new run with new settings and the inherited saved data.

The full rules are in [Runtime v2](runtime-v2.md#resume-and-branch).
