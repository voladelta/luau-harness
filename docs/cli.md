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
| `hx test PKG_DIR` | Run a package's `tests/*.luau` inside its boundary, with only the capabilities its `hx.toml` requests; exits 1 on failure |
| `hx approve PKG` | Lock a project package's or skill's hash and grant the capabilities it requests |
| `hx skills` | List skills in `.agents/skills` and `~/.agents/skills` and whether each is approved |
| `hx trust [--yes]` | Review this workspace's `.harness` config and `hx.lock`, print its hash, then trust it; runs refuse untrusted config |
| `hx staged` / `hx accept NAME` / `hx reject NAME` | Review what the agent staged (tools as `NAME`, module replacements as `std/MODULE`), then activate or discard it |
| `hx rollback MODULE` | Undo the last accepted replacement of a harness module |
| `hx log verify LOG` | Check a log's hash chain and the session its records name, the blobs it references and its recovery accounting; exit 0 ok, 1 invalid, 3 unknown outcomes |
| `hx paths` | Print where hx keeps this workspace's files, one `name: path` per line: `log`, `ar` (autoresearch runs), `secrets`, and the `trust` store |

`LOG` is a run name, such as `1794591703-21202`, for that run's journal in the workspace's log directory, or a path to a journal (`./NAME.jsonl` for one in the current directory). `hx run --ar --resume RUN` takes a run name in the workspace's autoresearch directory, or a path.

## `hx run` flags

Supply one quoted positional prompt or `--prompt-file`, with flags before or after the prompt. Use `--` before a literal prompt starting with `--`. `--branch` inherits its task unless a new prompt is supplied; `--resume` restores its saved task, approval mode, autopilot setting and path grants, without repeated launch flags. `--yes` is also accepted by `hx exec`.

The workspace, private temp directory and configured file-tool roots need no confirmation. Other paths ask for a temporary grant; recognized external commands, HTTP actions and MCP tools ask unless pre-approved. No controlling terminal means refusal. `--yes` grants ordinary requests automatically: commands, and single files outside the roots. It never grants a directory (that needs your answer on the terminal) nor an irreversible command (force push, `rm -rf ~`, `mkfs`, `dd`), while retaining protected paths, credential-read restrictions, capabilities, budgets and the scrubbed environment. It does not answer manual workflow review gates or reconcile uncertain operations. Shell commands are [confined](configuration.md#shell-confinement) to the file-tool roots. The former `--yolo` option has been removed.

hx always uses the Responses API. Requests go to `/responses` under the configured base URL.

| Flag | Default | Meaning |
|---|---|---|
| `--prompt-file F` | | Read the task from a file instead of a positional prompt |
| `--ignore-config` | off | Run without this workspace's `.harness` config and `hx.lock`; without it, untrusted config makes `hx run` and `hx exec` exit with an error (see [workspace trust](configuration.md#workspace-trust)). Also accepted by `hx exec` |
| `--approval prompt\|deny` | `policy.toml`, else `prompt` | Override the configured approval mode; cannot combine with `--yes`. Also accepted by `hx exec` and `hx test` |
| `--yes` | off | Autopilot: automatically approve ordinary missing file-tool permissions and external actions for this run; irreversible commands still need an exact `allow` entry or a terminal answer |
| `--rpi` | off | Activate the bundled [RPI workflow](workflows.md#rpi-research-design-implement-verify) in automatic mode |
| `--manual` | off | With `--rpi`, pause for research/design reviews and final acceptance |
| `--ar` | off | [Autoresearch](workflows.md#autoresearch) over `.harness/ar.toml`; needs a prompt or `--prompt-file` (the program); runs until progress stops |
| `--model M` | `HX_MODEL` | Model id |
| `--base-url U` | `HX_BASE_URL` | Responses API base URL; defaults to OpenRouter |
| `--key-env E` | `OPENROUTER_API_KEY` | Env var holding the API key |
| `--reasoning R` | | Reasoning effort |
| `--max-turns N` | 200 | Turn limit; the report says `stopped: "max_turns: N"` when it is hit |
| `--max-output N` | 65536 | Max output tokens per turn, reasoning included; with a known context window, the default is at most a quarter of it and at most the provider's listed output cap |
| `--context-window N` | 1000000 (assumed) | The model's context window, used by `std/compact`: compaction starts at 80% of it. Without the flag, a run on OpenRouter (the default base URL) reads the model's `context_length` and `top_provider.max_completion_tokens` from `/models` once at launch; the manifest keeps them for resume. When that fails (another provider, an unlisted model, no network), hx says so on stderr and assumes the default; give a smaller model's window so it compacts before an overflow |
| `--max-tokens N` `--max-wall SECS` | none | Admission thresholds and absolute deadline; flags override `policy.toml` for a new run |
| `--provider-only A,B` | | OpenRouter only: route to these providers, no fallbacks |
| `--pack A,B` | | Extra packs: `std/authoring` (agent-written tools), `std/vision` (`view()` shows images to a vision model); any other name is an error (compaction and recheck are set in `.harness/init.luau`, RPI with `--rpi`). Also accepted by `hx exec` |
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

Flags and positional operands may come in any order; `--` ends the flags. A command refuses a flag it does not use.

Environment: `HX_MODEL` and `HX_BASE_URL` set defaults for the matching `run` flags; `HX_DEBUG=1` prints the full error chain on failure; `HX_TRUST=HASH` trusts exactly the config with that hash for one launch (see [workspace trust](configuration.md#workspace-trust)).

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Done. `hx run` exits 0 whenever it prints a report, finished or not; read `finished` and `stopped` in it |
| 1 | An error (printed on stderr); `hx test`: a test failed; `hx log verify`: a broken chain, a bad blob or invalid recovery accounting |
| 2 | A usage error: an unknown command or flag, a missing or surplus operand, a malformed value |
| 3 | `hx log verify`: effects or model requests with an unknown outcome |
| 129, 130, 143 | Stopped by SIGHUP, SIGINT or SIGTERM; running commands, MCP servers and child runs are stopped too |
| 141 | stdout was closed before hx finished writing (`hx run ... \| head -1`); nothing more is written |

## Report and logs

`hx run` prints a JSON report on stdout: turns, Luau errors, `finished`, `final_answer`, tokens and `log`, the path of the run's event log. Event logs live outside the workspace, in hx's state directory: `$XDG_STATE_HOME/hx/workspaces/NAME-ID/log/` (default `~/.local/state/hx/...`; `NAME` is the workspace directory's name, `ID` 16 hex digits of the SHA-256 of its canonical path). `hx paths` prints the directory. The log stores file contents and command output as blobs in its `blobs/` directory, so treat it as sensitive. A `.harness/log`, `.harness/ar` or `.harness/secrets.toml` left in a workspace by an earlier version is not read; hx says so at each launch and names the new location.

## Checkpoints and resume

`hx run --resume LOG` restores the frozen manifest, the latest committed controller state and `durable` data into fresh VMs. It never reruns old chunks, and ordinary globals are lost.

Runs created before per-run approvals or before shell outcomes recorded their approval source use an older journal format, and journals an earlier version chained with a per-user key do not verify; start a new run with this version. A run resumes only from the workspace's log directory.

Interrupted local `write`/`edit` settle themselves from the evidence when the file holds its content from before or after the write. Interrupted shell commands require owner reconciliation. An interrupted subagent leaves nothing to reconcile beyond its own effects. Model requests whose response was lost are charged their reserved tokens on resume or branch, including a revoked run's; they are not operations for `hx recover`. Other uncertain effects wait for `hx recover`.

`--branch LOG` starts a new run with new settings and the inherited saved data.

The full rules are in [Runtime v2](runtime-v2.md#resume-and-branch).
