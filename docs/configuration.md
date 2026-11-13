# Configuration

All configuration lives in `.harness/` in the workspace; run state (journals, blobs, autoresearch runs) does not: it lives in `$XDG_STATE_HOME/hx` (default `~/.local/state/hx`, see [the command reference](cli.md#report-and-logs)). File tools can read the configuration but refuse writes; on Linux, shell commands and MCP servers are [confined](#shell-confinement) to the same file roots. Workspace config is used only after you trust it with `hx trust` (see [Workspace trust](#workspace-trust)).

| File | Purpose |
|---|---|
| `.harness/policy.toml` | File-tool roots, approval gate, budgets, packs |
| `.harness/mcp.toml` | MCP servers |
| `.harness/init.luau` | Toggles for `std` modules such as compaction and recheck |
| `.harness/ar.toml` | Autoresearch setup (see [workflows](workflows.md#harnessartoml)) |
| `.harness/override/`, `.harness/tools/`, `.harness/pkgs/` | Extensions (see [Extending](extending.md)) |
| `.harness/secrets.toml` | Credentials for tools (see [credentials](extending.md#credentials)) |
| `hx.lock` | Approved packages and their content hashes |

## `policy.toml`

```toml
[fs]
read = ["/extra/read/root"]     # extra read roots for file tools
write = ["/extra/write/root"]   # extra write roots

[approval]
mode = "prompt"                 # default; "deny" refuses missing permissions without asking
ask = ["terraform apply"]       # extra command substrings that need approval
allow = ["git push origin"]     # pre-approved: leading words of each external command; irreversible ones need an exact match

[budget]
max_tokens = 600000
max_wall_secs = 900

[packs]
enable = []
```

The workspace, private temp directory and `[fs]` roots are standing file-tool permissions. With `mode = "prompt"`, other file-tool paths ask on the owner's controlling terminal. A write grant covers the requested file's parent directory and its descendants, with read access; a read grant covers the requested file, or the subtree when a directory is requested. Home has no implicit grant. Refusal or a missing terminal leaves the operation unexecuted. Grants are shared by descendants and journaled for resume, and a new run starts with fresh grants.

Shell commands are classed on their shell words, not substrings: each simple command of a pipeline or list (and the body of `$(...)`, backticks and `bash -c`) is classed read / local write / external / irreversible, and the command takes the highest class. A `git push` with `--force`, `--force-with-lease`, `-f`, `--delete`, `--mirror` or a `+`/`:` refspec anywhere in its arguments, `rm -r` of `/`, `~` or `$HOME`, `mkfs` and `dd` are irreversible. An `allow` entry covers a command when its words are the command's first words, and it must cover each external command in a chain: `allow = ["git push origin"]` approves `git push origin main` but not `git push originx`, `git push -o "git push origin" x` or `git push origin main && curl -X POST ...`. The classifier decides when to ask; it is approval UX, not a guard (see [shell confinement](#shell-confinement)). Approval is asked before the operation's intent is journaled, and the intent records who approved it (`approval`: `not-required`, `policy-allow`, `auto` or `prompt`).

`hx run "TASK" --yes` automatically approves ordinary missing path permissions and external actions for one run. It never approves an irreversible command: that still needs an exact `allow` entry or the owner's answer on the terminal, and is refused without one. It overrides the configured approval mode, changes no configuration files and keeps the same scrubbed child environment. Protected configuration, sealed owner state, credential-read restrictions, symlink-write checks, capabilities and budgets still apply. Use `--approval deny` for strict refusal or `--approval prompt` to override the file; neither flag combines with `--yes`. Explicit manual workflow reviews and uncertain-operation reconciliation still need owner decisions.

Budgets are admission thresholds: each request reserves its upper bound in tokens (request bytes plus the output cap), and the deadline is absolute, so downtime counts. There is no dollar budget; an unknown key such as `max_cost` is an error, as is any unknown section or key elsewhere in `policy.toml`. `hx run` flags override these values for a new run.

## `mcp.toml`

```toml
[server.notes]                  # local server, runs directly
command = "python3"
args = ["server.py"]
read_only_tools = ["read_*"]    # these run freely; every other tool needs approval
env = ["NOTES_DIR"]             # the only env vars passed through

[server.remote]                 # remote Streamable HTTP server (https, or http on localhost)
url = "https://mcp.example.com/mcp"
bearer_env = "REMOTE_TOKEN"     # or headers_env = {"x-api-key" = "VAR"}
```

A local server is [confined](#shell-confinement) like a shell command, to the file policy at its start. Network and GPU access are controlled by the environment running hx; hx has no network switches.

MCP tools bind as `mcp.SERVER.tool{...}` (a name that is not an identifier, or whose safe form collides with another tool's, as `mcp.SERVER["name"]`). Approve a non-read-only tool with `allow = ["mcp notes.add"]` in `policy.toml`, or every tool of a server with `"mcp notes.*"`; entries match the tool's identity exactly, never its arguments. A remote server's header credentials are scrubbed from its errors and results before they are journaled; a local server's output is not filtered, so pass it only the variables it needs. A local server that exits is started again on the next call.

## Shell confinement

On Linux 5.13 or newer, every shell command, local MCP server and autoresearch evaluator starts under a Landlock ruleset built from the run's policy at that moment:

| | Shell commands and MCP servers | Evaluators |
|---|---|---|
| Read | Read roots (workspace, private temp dir, `[fs]` roots, approved grants, toolchains, approved skills), `/usr`, `/bin`, `/sbin`, `/etc`, `/dev`, `/opt`, `/lib*`, `/proc`, `/sys` | Their scratch copy, the same system and toolchain directories, `grader` roots (and `holdout` for `select`) |
| Write | Write roots (workspace, private temp dir, `[fs] write`, approved write grants), `/dev/null` and similar devices | Their scratch copy |
| Never readable | `.harness/secrets.toml`, grader and held-out roots in an episode, the run state (`~/.local/state/hx`; only the run's own copy of its approved skills' files is readable) and the trust store (`~/.config/hx`) | |
| Never writable | The run state and the trust store: no write root may overlap them | |

A grant approved mid-run applies to the next command. Write roots must stay clear of the run state and the trust store, because hiding a directory removes reads only: a `[fs] write` root that contains either (`write = ["~"]`) or lies inside one is refused at launch, as is a workspace that contains them, and a resumed run whose frozen roots now do (after `XDG_STATE_HOME` moved). They are refused rather than carved, since carving a write root takes file creation away from every directory on the way down (`~` itself). A file-tool write outside the roots normally grants its directory; where that directory overlaps them (`~/notes.txt`), the grant is that one file. A confined process cannot reach hx's own `/proc` entries, so it cannot read hx's environment. A credential file a command copies cannot be read; one it moves or hard-links is found by its inode and stays hidden for the rest of the run. When a hidden file sits inside a read root, the directories on the way to it (usually the workspace root and `.harness`, or `~/.local/state` under a `read = ["~"]` root) are granted entry by entry, so a file created directly in one of them is readable from the next command on; their entries can be listed by name, never read.

What stays open:

- Network and GPU access: use your container or sandbox.
- Protected configuration inside a write root (`.harness/`, `hx.lock`) is still writable by shell commands; Landlock cannot remove write access from part of a writable tree without breaking file creation in it. Edited config is no longer trusted. This is why run state lives outside the workspace: journals, blobs and autoresearch runs used to sit in `.harness/`, where a command could append to its own journal, read its manifest or rewrite what `--ar --resume` trusts.
- Credential files other than `.harness/secrets.toml` (`id_rsa`, `.netrc` and the rest that file tools refuse by name) are readable by shell commands when they sit inside a read root.

Without Landlock (an older kernel, or a container whose seccomp filter blocks it) hx runs children unconfined, prints one note on stderr per run and records `"confinement": "off: ..."` in the journal's session record; it never fails a command for lack of Landlock.

## `init.luau`: compaction and recheck

**Compaction** (`std/compact`) is on by default. At 80% of the context window (1M is assumed unless `--context-window` gives the model's own, or OpenRouter's model list does: without the flag, hx reads it there once at launch and the run's manifest keeps it), older history becomes a structured summary; the task message, recent messages and Luau globals stay. The summarizer call replays the agent's own request prefix, so it sees exact messages and can hit the provider's prompt cache. Its output, reasoning included, may use up to four fifths of a reserve kept free below the trigger: an eighth of the window, from 16k tokens up to 64k. A summary that is truncated, calls a tool or is not smaller than what it replaces is rejected and the history is kept; after failed summaries compaction waits 1, 2, 4 … (at most 16) turns before trying again. Images count too: older ones are summarized before they pass 8 MB of base64 (half the 16 MiB the controller state, history included, may hold). When the provider rejects a request as over the context window (a 400, 413 or 422, or an error in place of the streamed response; the fallback when the window is wrong), even during that wait, hx treats the rejected size as an upper bound on the window for one compaction, which keeps as little recent history as fits under it, and retries. Disable it for new runs:

```luau
require("std/compact").enabled = false
```

**Recheck** (`std/recheck`) is opt-in. The system prompt warns that the first final answer will be checked; before it is accepted, the model is sent back once to name the root cause, list the other cases it implies, test each and re-check the task's requirements. It adds turns and can hurt build tasks; enable it for debugging tasks:

```luau
require("std/recheck").enabled = true
```

## Workspace trust

`.harness/{policy,mcp,ar}.toml`, `.harness/init.luau`, `.harness/override/`, `.harness/tools/`, `.harness/versions/` (what `hx rollback` restores) and `hx.lock` live in the repo, so whoever wrote the repo could widen file-tool access, start MCP commands, run evaluator commands or pre-approve packages and skills. Trust covers the exact content of all of them. Until you review the config with `hx trust`, which records its hash in `~/.config/hx/trust.toml` (outside the repo, protected from the model), `hx run` and `hx exec` refuse to start:

```text
Error: this workspace's config is not trusted (.harness/policy.toml); review it and run `hx trust`, or pass --ignore-config to run without it
```

- `--ignore-config` runs without any of this workspace's config, trusted or not.
- Any edit makes the workspace untrusted again.
- `hx run --resume` is never refused: a resumed run uses only its frozen manifest.
- `hx approve`, `hx accept` and `hx rollback` keep an already trusted workspace trusted.
- A workspace without such files needs nothing.
- `HX_TRUST=HASH` in hx's environment trusts exactly that config for one launch (throwaway copies, scripts); `hx trust` prints the hash. `HX_TRUST=1`, which trusted any config, is deprecated: it still works in this release and prints a note.

## Approvals and `hx.lock`

`hx.lock` is the single approval record: per package, the approved version and its content hash. The hash covers every file of the package, `hx.toml` included, so approving a package grants exactly the capabilities its manifest requests, and a manifest that asks for more no longer matches its approval. A package or skill containing a symlink cannot be approved.

Content hashes start with `v2:` and frame every path and file content with its length. Earlier versions of hx wrote unframed hashes, under which one file's bytes could be split into several files without changing the hash; hx no longer accepts them. A package locked with an old hash is not loaded (requiring it reports that it was approved by an older hx), and `hx skills` shows `re-approve (old hash)`: review it and run `hx approve NAME` again. Re-approving rewrites `hx.lock`, which keeps an already trusted workspace trusted.

Older lock files may still carry `[package.grants]` tables or a `.harness/grants.toml`; hx ignores both.
