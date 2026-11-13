# Configuration

All configuration lives in `.harness/` in the workspace. File tools can read it but refuse writes; on Linux, shell commands and MCP servers are [confined](#shell-confinement) to the same file roots. Workspace config is used only after you trust it with `hx trust` (see [Workspace trust](#workspace-trust)).

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
read = ["/extra/read/root"]     # extra read roots for file tools (relative to the workspace; ~ is your home)
write = ["/extra/write/root"]   # extra write roots; a root that does not exist is an error

[approval]
mode = "prompt"                 # default; "deny" refuses missing permissions without asking
ask = ["terraform apply"]       # extra command substrings that need approval
allow = ["git push origin"]     # pre-approved (leading words of each external command; irreversible ones need an exact match)

[budget]
max_tokens = 600000
max_wall_secs = 900

[packs]
enable = []
```

The workspace, private temp directory and `[fs]` roots are standing file-tool permissions. With `mode = "prompt"`, other file-tool paths ask on the owner's controlling terminal. A write grant covers the requested file only, with read access (shell commands may then change that file, not create its siblings); a read grant covers the requested file, or the subtree when a directory is requested. A directory that is `/`, contains your home directory, or contains protected configuration, credentials or sealed state is never granted, in any mode: add a narrower `[fs]` root instead. Home has no implicit grant. Refusal or a missing terminal leaves the operation unexecuted. Grants are shared by descendants and journaled for resume, and a new run starts with fresh grants.

Shell commands are classed on their shell words, not substrings: each simple command of a pipeline or list (and the body of `$(...)`, backticks and `bash -c`) is classed read / local write / external / irreversible, and the command takes the highest class. A `git push` with `--force`, `--force-with-lease`, `-f`, `--delete`, `--mirror` or a `+`/`:` refspec anywhere in its arguments, `rm -r` of `/`, `~` or `$HOME`, `mkfs` and `dd` are irreversible. Shell reserved words (`if`, `then`, `do`, `{`, `!`, ...) are skipped, and after a wrapper (`sudo`, `env`, `timeout`, `nice`, `xargs`, `find -exec`, ...) or among `git`'s global options every later word is tried as the program, so `for b in a; do git push --force; done`, `timeout 5s git push --force` and `git --git-dir .git push --force` are irreversible like the bare command. An `allow` entry covers a simple command when the command's words start with the entry's words, and it must cover each external command in a chain: `allow = ["git push origin"]` approves `git push origin main` but not `sudo git push origin main`, `echo x "git push origin"; ssh host` or `git push origin main && curl -X POST ...`. The classifier decides when to ask; it is approval UX, not a guard (see [shell confinement](#shell-confinement)). The journal records who approved each shell command (`approval` on its outcome: `not-required`, `policy-allow`, `auto` or `prompt`).

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

An unknown key (a typo such as `bearer-env`) is an error. A server that exits is started again on the next call.

MCP tools bind as `mcp.SERVER.tool{...}`; a name that is not a Luau identifier also gets an alias with `_` for each other character (`a.b` as `a_b`) unless another tool already has that name. Approve a non-read-only tool with `allow = ["mcp notes.add"]` in `policy.toml`, or every tool of a server with `"mcp notes.*"` (a server whose name contains a dot cannot be pre-approved). Approval is decided on the server and tool name alone: the prompt shows the call's arguments, escaped, but they are never matched against `allow`. HTTP tool endpoints work the same way with `http NAMESPACE.ENDPOINT`.

## Shell confinement

On Linux 5.13 or newer, every shell command, local MCP server and autoresearch evaluator starts under a Landlock ruleset built from the run's policy at that moment:

| | Shell commands and MCP servers | Evaluators |
|---|---|---|
| Read | Read roots (workspace, private temp dir, `[fs]` roots, approved grants, toolchain directories), `/usr`, `/bin`, `/sbin`, `/etc`, `/opt`, `/lib*`, `/proc`, `/sys`, `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, `/dev/shm` | Their scratch copy, the same system and toolchain directories, `grader` roots (and `holdout` for `select`) |
| Write | Write roots (workspace, private temp dir, `[fs] write`, approved write grants), `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/shm` | Their scratch copy |
| Never readable | `.harness/secrets.toml`, sealed state (`.harness/ar`, grader and held-out roots in an episode), the trust store (`~/.config/hx`) | |

A grant approved mid-run applies to the next command. A confined process cannot reach hx's own `/proc` entries, so it cannot read hx's environment. It has no terminal (`/dev/tty`, `/dev/pts`), so it cannot draw over an approval prompt or read your keystrokes. On Linux 6.12 or newer (Landlock ABI 6) it also cannot signal processes or connect to abstract Unix sockets outside its own command. A credential file a command copies cannot be read; one it moves or hard-links, or a file it moves out of sealed state, is known by its inode: file tools refuse it under any name, and later commands of the run cannot read it. When a hidden file sits inside a read root, the directories on the way to it (usually the workspace root and `.harness`) are granted entry by entry, so a file created directly in one of them is readable from the next command on.

What stays open:

- Network and GPU access: use your container or sandbox.
- Protected configuration inside a write root (`.harness/`, `hx.lock`) is still writable by shell commands; Landlock cannot remove write access from part of a writable tree without breaking file creation in it. Edited config is no longer trusted, and the journal's hash chain shows tampering.
- Pathname Unix sockets (the user's D-Bus or systemd socket, `docker.sock`) are reachable before Landlock ABI 9 (Linux 7.1); Landlock cannot confine them on older kernels.
- `.git/config` and `.git/hooks` are refused to file tools but writable by shell commands, like the rest of the workspace.
- Credential files other than `.harness/secrets.toml` (`id_rsa`, `.netrc` and the rest that file tools refuse by name) are readable by shell commands when they sit inside a read root.

Without Landlock (an older kernel, or a container whose seccomp filter blocks it) hx runs children unconfined, prints one note on stderr per run and records `"confinement": "off: ..."` in the journal's session record; it never fails a command for lack of Landlock.

## `init.luau`: compaction and recheck

**Compaction** (`std/compact`) is on by default. At 80% of the context window (1M is assumed unless `--context-window` gives the model's own, or OpenRouter's model list does: without the flag, hx reads it there once at launch and the run's manifest keeps it), older history becomes a structured summary; the task message, recent messages and Luau globals stay. The summarizer call replays the agent's own request prefix, so it sees exact messages and can hit the provider's prompt cache. Its output, reasoning included, may use up to four fifths of a reserve kept free below the trigger: an eighth of the window, from 16k tokens up to 64k. A summary that is truncated, calls a tool or is not smaller than what it replaces is rejected and the history is kept. After two failed or rejected summaries in a row, compaction under token pressure pauses for the next eight agent replies (`retry_turns`); a context overflow and the image budget still compact during the pause. On a context-overflow error (recognized from the provider's error text, the fallback when the window is wrong) hx treats the rejected size as an upper bound on the window for one compaction, which keeps as little recent history as fits under it, and retries. Disable it for new runs:

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
- `HX_TRUST=HASH` in hx's environment trusts exactly that config for one launch (throwaway copies, scripts); `hx trust` prints the hash.

## Approvals and `hx.lock`

`hx.lock` is the single approval record: per package, the approved version and its content hash. The hash covers the package's `hx.toml`, so approving a package grants exactly the capabilities its manifest requests, and a manifest that asks for more no longer matches its approval.

The hash is SHA-256 over every file of the package (entries starting with `.` excluded), each as its length-prefixed relative path followed by its length-prefixed content, in path order. `hx approve`, `hx skills`, `hx accept` and a run's launch read a package through the same function, so they agree on what was approved. Locks written by hx versions before this encoding no longer match: run `hx approve NAME` again for each package and skill.

A run builds its extensions from the config bytes trust checked, `hx.lock` included, never from a second read of the files. `hx.lock` and the module overrides `hx accept` and `hx rollback` write are replaced atomically.

Older lock files may still carry `[package.grants]` tables or a `.harness/grants.toml`; hx ignores both.
