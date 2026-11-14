# Configuration

Workspace configuration lives in `.harness/` in the workspace, with the workspace's agent skills in `.agents/skills/`. File tools can read both but refuse writes; shell commands and MCP servers are [confined](#shell-confinement) to the same file roots. Workspace config is used only after you trust it with `hx trust` (see [Workspace trust](#workspace-trust)).

| File | Purpose |
|---|---|
| `.harness/policy.toml` | File-tool roots, approval gate, budgets, packs |
| `.harness/mcp.toml` | MCP servers |
| `.harness/init.luau` | Toggles for `std` modules such as compaction and recheck |
| `.harness/ar.toml` | `hx-ar`'s autoresearch setup (see [workflows](workflows.md#harnessartoml)); trust covers every `.harness/*.toml` |
| `.harness/override/`, `.harness/tools/`, `.harness/pkgs/` | Extensions (see [Extending](extending.md)) |
| `.agents/skills/` | The workspace's agent skills |

What hx treats as authority or as a secret is kept outside the workspace, in its own directories, which no root of the model's may contain:

| Location | Holds |
|---|---|
| `$XDG_CONFIG_HOME/hx/` (default `~/.config/hx/`) | `trust.toml` (the trust store), `secrets.toml` (your credentials for tools), and per workspace `workspaces/NAME-ID/secrets.toml` (that workspace's credentials, see [credentials](extending.md#credentials)) |
| `$XDG_STATE_HOME/hx/` (default `~/.local/state/hx/`) | Per workspace, the state directory `workspaces/NAME-ID/`: `log/` (event logs, blobs, lock files, revocation markers, each run's materialized skills), and the files of programs built on hx, such as `ar/` (`hx-ar`'s runs) |

`NAME` is the workspace directory's name with unusual characters replaced, `ID` the first 16 hex digits of the SHA-256 of its canonical path. `hx paths` prints the four that matter (`log`, `state`, `secrets`, `trust`). hx refuses a workspace, an `[fs]` root or a path grant that contains either directory or lies inside one (`[fs] write = ["~"]` with the default locations, for example): Landlock grants whole trees, so such a root would hand the model the trust store, your credentials and the journals. A `.harness/secrets.toml` or `.harness/log` left by an earlier version is not read; hx says so on stderr at each launch with the new location, and moving the file is up to you. Until a leftover `.harness/secrets.toml` is moved, shell commands can still read it where it is.

## `policy.toml`

```toml
[fs]
read = ["/extra/read/root"]     # extra read roots for file tools (relative to the workspace; ~ is your home)
write = ["/extra/write/root"]   # extra write roots; a root that does not exist is an error
sealed = ["~/grader/holdout"]   # never readable or writable by file tools, in any mode, --yes included

[approval]
mode = "prompt"                 # default; "deny" refuses missing permissions without asking
ask = ["terraform apply"]       # extra command substrings that need approval
allow = ["git push origin"]     # pre-approved (leading words of each external command; irreversible ones need an exact match)

[budget]
max_tokens = 600000
max_wall_secs = 900

[packs]
enable = []                     # std/vision (as --pack); any other name is an error
```

The workspace, private temp directory and `[fs]` roots are standing file-tool permissions. With `mode = "prompt"`, other file-tool paths ask on the owner's controlling terminal. A write grant covers the requested file only, with read access (shell commands may then change that file, not create its siblings); a read grant covers the requested file, or the subtree when a directory is requested. A directory that is `/`, contains your home directory, or contains protected configuration, credentials or sealed state is never granted, in any mode: add a narrower `[fs]` root instead. Home has no implicit grant. A `sealed` path is refused to file tools in every mode and is never granted; it may not lie inside a read root (that would put it within shell commands' reach), so a policy that seals one there is refused. Refusal or a missing terminal leaves the operation unexecuted. Grants are shared by descendants and journaled for resume, and a new run starts with fresh grants.

Shell commands are classed on their shell words, not substrings: each simple command of a pipeline or list (and the body of `$(...)`, backticks and `bash -c`) is classed read / local write / external / irreversible, and the command takes the highest class. A `git push` with `--force`, `--force-with-lease`, `-f`, `--delete`, `--mirror` or a `+`/`:` refspec anywhere in its arguments, `rm -r` of `/`, `~` or `$HOME`, `mkfs` and `dd` are irreversible. Shell reserved words (`if`, `then`, `do`, `{`, `!`, ...) are skipped, and after a wrapper (`sudo`, `env`, `timeout`, `nice`, `xargs`, `find -exec`, ...) or among `git`'s global options every word up to the one that can be the program is tried as the program, so `for b in a; do git push --force; done`, `timeout 5s git push --force` and `git --git-dir .git push --force` are irreversible like the bare command; `command -v` and `command -V` run nothing. Where the classifier cannot tell what will run, the command is external: a program (or `git` subcommand) the shell substitutes (`$x`, `"$(...)"`, a backtick, `$'...'` quoting). A wrapper whose program may start more than 32 words after it could hide anything, so it is irreversible. `time -p CMD` is classed as `CMD`, `env -S 'CMD'` and `env --split-string='CMD'` as the command in the string, and `find` by the command after each `-exec`, `-execdir`, `-ok` and `-okdir`, however long the expression before it. An `allow` entry covers a simple command when the command's words start with the entry's words, and it must cover each external command in a chain: `allow = ["git push origin"]` approves `git push origin main` but not `sudo git push origin main`, `echo x "git push origin"; ssh host` or `git push origin main && curl -X POST ...`. The classifier decides when to ask; it is approval UX, not a guard (see [shell confinement](#shell-confinement)). The journal records who approved each shell command (`approval` on its outcome: `not-required`, `policy-allow`, `auto` or `prompt`).

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
timeout_secs = 300              # either kind: how long one call may wait for its answer (default 60)
```

A local server is [confined](#shell-confinement) like a shell command, to the file policy at its start, and runs in its own process group: when hx drops the connection (the server stopped answering, the run ended, hx was stopped by a signal) it kills the whole group, so the real server behind a wrapper such as `npx` or `uvx` ends too. A process that detaches into its own session is not killed. Network and GPU access are controlled by the environment running hx; hx has no network switches.

An unknown key (a typo such as `bearer-env`) is an error. A server that exits is started again on the next call. A call whose answer does not arrive within `timeout_secs` (60 seconds unless the server sets it) is abandoned; when the tool is not read-only, its outcome is unknown and the run's later effects wait until you settle it (`hx recover`), so give a server whose tools take longer a longer deadline.

MCP tools bind as `mcp.SERVER.tool{...}`; a name that is not a Luau identifier also gets an alias with `_` for each other character (`a.b` as `a_b`) unless another tool already has that name. Approve a non-read-only tool with `allow = ["mcp notes.add"]` in `policy.toml`, or every tool of a server with `"mcp notes.*"` (a server whose name contains a dot cannot be pre-approved). Approval is decided on the server and tool name alone: the prompt shows the call's arguments, escaped, but they are never matched against `allow`. HTTP tool endpoints work the same way with `http NAMESPACE.ENDPOINT`.

## Shell confinement

On Linux 5.13 or newer (Landlock), every shell command and local MCP server starts under a Landlock ruleset built from the run's policy at that moment, and every `hx-ar` evaluator under one of its own:

| | Shell commands and MCP servers | Evaluators |
|---|---|---|
| Read | Read roots (workspace, private temp dir, `[fs]` roots, approved grants, toolchain directories), `/usr`, `/bin`, `/sbin`, `/etc`, `/opt`, `/lib*`, `/proc`, `/sys`, `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, `/dev/shm` | Their scratch copy, the same system and toolchain directories, `grader` roots (and `holdout` for `select`) |
| Write | Write roots (workspace, private temp dir, `[fs] write`, approved write grants), `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/shm` | Their scratch copy |
| Also readable | The run's materialized skills (in its log directory), so a skill's supporting files are there | |

Nothing else is reachable: in particular not hx's configuration and state directories (trust store, credentials, journals, `hx-ar`'s runs), which lie outside every root, nor `sealed` paths (such as an autoresearch episode's `grader` and `holdout` roots), which may not lie inside one. A grant approved mid-run applies to the next command. A confined process cannot reach hx's own `/proc` entries, so it cannot read hx's environment. It has no terminal (`/dev/tty`, `/dev/pts`), so it cannot draw over an approval prompt or read your keystrokes. On Linux 6.12 or newer (Landlock ABI 6) it also cannot signal processes or connect to abstract Unix sockets outside its own command.

What stays open:

- Network and GPU access: use your container or sandbox.
- Protected configuration inside a write root (`.harness/`, `.agents/skills/`) is still writable by shell commands; Landlock cannot remove write access from part of a writable tree without breaking file creation in it. Edited config is no longer trusted.
- Pathname Unix sockets (the user's D-Bus or systemd socket, `docker.sock`) are reachable before Landlock ABI 9 (Linux 7.1); Landlock cannot confine them on older kernels.
- `.git/config` and `.git/hooks` are refused to file tools but writable by shell commands, like the rest of the workspace.
- Credential files that sit inside a read root (`id_rsa`, `.netrc` and the rest that file tools refuse by name) are readable by shell commands. `--yes` grants single files only: it never grants a directory, for reading or writing, so it cannot put one such as `~/.ssh` in a command's reach, and it does not grant writing a hidden file or anything under a hidden directory (`~/.bashrc`, `~/.ssh/authorized_keys`, `~/.config/...`, another repository's `.git/hooks`), where a write becomes code that runs later. Only your answer on the terminal can grant those.

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

Every `.harness/*.toml` (hx's `policy.toml` and `mcp.toml`, and the config of programs built on hx, such as `ar.toml`), `.harness/init.luau`, `.harness/override/`, `.harness/tools/`, the packages in `.harness/pkgs/` and the agent skills in `.agents/skills/` live in the repo, so whoever wrote the repo could widen file-tool access, start MCP commands, run evaluator commands or install a package with capabilities. Trust covers the exact content of all of them. Until you review the config with `hx trust`, which records its hash in the trust store (`trust.toml` in hx's configuration directory, outside the repo and every root of the model's), `hx run` and `hx exec` refuse to start:

```text
Error: this workspace's config is not trusted (.agents/skills/deploy, .harness/policy.toml); review it and run `hx trust`, or pass --ignore-config to run without it
```

`hx trust` prints every config file (control characters escaped), then one line per package and skill with what it is granted: its name, kind, the capabilities its `hx.toml` declares and its file count. An agent skill is listed as having no Luau and no capabilities. Then it prints the hash and asks.

- A package gets exactly the capabilities its trusted `hx.toml` declares; there is no separate approval. A package or skill that cannot be read (a symlink inside it, an invalid name, a broken `hx.toml`) is listed with the reason and never loads.
- Any edit makes the workspace untrusted again, whoever made it: you, or the agent's shell commands, which can write `.harness/` and `.agents/skills/` (file tools cannot). The refusal names what changed since you trusted it, for example `.harness/pkgs/slug (changed), .agents/skills/notes (new)`, and `hx trust` repeats it before asking.
- `--ignore-config` runs without any of this workspace's config, trusted or not.
- `hx run --resume` is never refused: a resumed run uses only its frozen manifest.
- A workspace without such files needs nothing.
- `HX_TRUST=HASH` in hx's environment trusts exactly that config for one launch (throwaway copies, scripts); `hx trust` prints the hash.

The hash is SHA-256 over every config file, and every file of every package and skill (entries starting with `.` excluded, so a skill's `.git` is not part of it), each as its length-prefixed workspace-relative path followed by its length-prefixed content, in path order. A run builds its policy and extensions from the bytes trust checked, never from a second read of the files.

Your own skills in `~/.agents/skills` are not part of any workspace and need no approval: they load in every workspace (a workspace package or skill of the same name shadows one). What keeps the model from changing them: they lie outside every root it can write, file tools refuse writes under `~/.agents/skills` in every mode, and no grant can reach them (`--yes` grants single files, but never a protected path nor a write under a hidden directory, and a directory grant needs your answer on the terminal). Shell commands cannot write them unless your own `policy.toml` adds a write root that contains them.

A `hx.lock` left by an older version is ignored, with one note on stderr; trust covers what it used to approve.
