# hx

A small coding-agent host for one project at a time: a Luau VM, five tools, and one kernel-enforced wall.

The host is about 2,000 lines of C++. The agent loop is one Luau file, and your project can change any part of
it. The model can extend the harness by writing Luau modules into the project.

```
cd my-project
export DEEPSEEK_API_KEY=...
hx "add a --json flag to the export command and test it"
```

## The idea

- **The host is a kernel, not a framework.** It gives Luau a model call, a shell, file access and JSON. That is
  all. There is no plugin system, package format, permission prompt or configuration file.
- **The loop belongs to the project.** `hx init` writes `.hx/loop.luau`, a short file that loads the loop built
  into hx and changes what this project needs: the model, the prompt, the tools, the retry and compaction
  rules. What it leaves alone keeps up with hx. `hx init --full` copies the whole built-in loop instead, and
  the project owns all of it.
- **One wall, built by the kernel.** Before anything runs, the process confines itself with Landlock. The VM,
  the file tools and every command the model starts inherit the same rules, so no tool needs a path check and
  no command needs to be judged by its text.
- **Extensions have no more power than the model.** An extension is Luau running in the same confined process.
  It can do nothing the model's own shell could not, so nothing needs to be approved, staged or trusted.

## Build

Needs Linux 5.13 or later (Landlock), a C++20 compiler, CMake 3.28, Ninja, libcurl headers, and `bash` at run
time.

```
cmake --preset release
cmake --build --preset release
ctest --preset release            # needs python3 and a kernel with Landlock
cmake --install build             # optional
```

The first configure downloads three pinned dependencies: [Luau](https://github.com/luau-lang/luau) (the VM and
compiler), [RapidJSON](https://github.com/Tencent/rapidjson) and [{fmt}](https://github.com/fmtlib/fmt) (both
header-only).

## Use

```
hx [options] [TASK...]            run the agent; without a task, read it from stdin or start a prompt
hx [options] run FILE [ARGS...]   run a Luau script with the host primitives
hx init                           make this project own its loop
```

| Option | Meaning |
|---|---|
| `--ro PATH` | also allow reading `PATH` |
| `--rw PATH` | also allow writing `PATH` |
| `--keep-env NAME` | keep a secret-looking variable in the environment of commands |
| `--no-sandbox` | run without Landlock; the model is told it is not confined |

| Variable | Meaning | Default |
|---|---|---|
| `HX_API_URL` | Anthropic Messages endpoint | `https://api.deepseek.com/anthropic/v1/messages` |
| `HX_KEY_ENV` | name of the variable holding the API key | `DEEPSEEK_API_KEY` |
| `HX_MODEL` | model name | `deepseek-flash` |

The model's words go to stdout and progress lines go to stderr, so `hx "..." > answer.txt` captures the answer.
At the prompt, Ctrl-C stops the current turn and Ctrl-D leaves.

## The wall

| | Allowed |
|---|---|
| Write | the project directory (where you start `hx`), a private `$TMPDIR` that is removed on exit, `--rw` paths |
| Read | the above, plus `/usr /bin /sbin /lib* /etc /opt /proc /sys /nix/store`, your git configuration, `--ro` paths |
| Devices | `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom` |
| Everything else | cannot be opened: the rest of your home directory, other terminals, other projects |

Toolchains installed in your home directory need a flag, for example
`hx --ro ~/.rustup --ro ~/.cargo --rw ~/.cargo/registry "..."`. The flags are on the command line on purpose:
nothing inside the project, which the model can write, can widen the wall.

Also in force:

- `hx` refuses to start in `/` or in your home directory, where the wall would protect nothing.
- No setuid program can raise privileges (`no_new_privs`), so `sudo` does not work inside.
- On Linux 6.12 or later, confined processes cannot signal processes outside the sandbox or reach their
  abstract unix sockets.
- The API key is read once and removed from the environment, together with every variable whose name ends in
  `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD` or `CREDENTIALS`. Luau code never sees the key: the host adds
  it to requests for the configured endpoint only, and commands cannot read it out of the host's memory.
- The VM has a memory ceiling of 1 GiB, and every process a command started is killed when `hx` exits, a
  daemon that called `setsid` included: `hx` is a child subreaper, so all of them stay its descendants.

### What the wall does not do

Know these before you rely on it.

- **Unix sockets by path are reachable on kernels before Landlock ABI 9.** A confined command cannot list
  `/run/user/1000`, but it can connect to a socket there. That puts the D-Bus session bus (which can start
  programs outside the sandbox), `ssh-agent` and a Docker socket within reach. `hx` prints a note at startup
  when the kernel cannot refuse this. With ABI 9 the connection is refused unless the socket's path is
  readable under the rules above. To close it on an older kernel, run `hx` as a separate Unix user that owns
  nothing but the project.
- **The network is open.** The model can send anywhere what it can read. The wall limits what it can read.
- **What the model writes, you may later run.** A changed `Makefile`, test, or `.git/hooks` script runs with
  your full access when you run it outside `hx`. Git hooks do not show in `git diff`.
- **Secrets stored in the project are readable**, as are secrets in variables with unusual names.
- **A process outlives `hx` when `hx` dies without its exit path:** killed with SIGKILL, or leaving at once on a
  second signal that came before the first was handled. Such a process stays confined, but it is not killed.
- **The model can edit `.hx/loop.luau` and its extensions.** That is the point, and it shows in `git diff`.
  If the loop no longer loads and returns a `main` function, the built-in one runs instead.
- **Nothing asks before a file in the project is changed or deleted.** Git is the undo.

## The loop

`lua/loop.luau` is the whole agent: about 700 lines. It speaks the Anthropic Messages format, asks for adaptive
thinking, and sends thinking blocks back exactly as they came. The five tools it defines:

| Tool | Does |
|---|---|
| `bash` | runs a command, returns its output and exit status, kills it after a timeout |
| `read` | returns a range of lines of a file |
| `write` | creates or overwrites a file |
| `edit` | replaces an exact piece of text that occurs once |
| `lua` | runs Luau inside the harness; this is how extensions are called |

`hx init` writes `.hx/loop.luau` from `lua/overlay.luau`:

```lua
local M = host.load(host.default_loop, "=loop")()
M.model = "deepseek-pro"
table.insert(M.tools, { name = "check", description = "...", parameters = {...}, run = function(args) ... end })
return M
```

Every setting at the top of the loop is a field of `M`, and so are the parts a project may want to replace:
`M.system_prompt`, `M.request` (one request to the model), `M.turn` (replies and tool calls until the model
is done) and `M.compact`. The loop always calls them through `M`, so a replacement takes effect everywhere.
For notes to the model that need no code, write `.hx/prompt.md`.

When a `.hx` directory exists, each session's conversation is saved to `.hx/log/` as JSON. The logs hold
file contents, so that directory carries a `.gitignore` that keeps all of it out of git.

### The conversation is a state machine

The Messages API accepts one shape of history only: it starts with the user, roles alternate, and a reply
that calls tools is answered by the next message with exactly one result per call. The loop keeps the history
in a `Conversation` whose state is read off its last message, and each operation is legal in some states only:

| State | The last message is | Legal operations |
|---|---|---|
| `idle` | nothing, or a finished reply | `say` |
| `ready` | the user's | `say`, `reply` |
| `owing` | a reply that called tools | `answer`, `abandon` |

An illegal operation raises and changes nothing, so an interrupt, a cut-off reply or a compaction cannot leave
a history the API would reject. `tests/conversation_test.luau` checks this with a random walk over every
operation in every state.

## Extensions

An extension is a Luau module at `.hx/ext/NAME.luau`. Its first line is a comment that says what it does, and
it returns a table of functions:

```lua
-- counts TODO comments per file
return {
	count = function(dir)
		return host.sh(("grep -rc TODO %s | grep -v ':0$'"):format(dir)).out
	end,
}
```

The model writes extensions with the ordinary `write` tool and calls them through the `lua` tool as
`ext("NAME").count("src")`. A changed file is loaded again on its next use. Each session starts with a list of
the project's extensions in the system prompt. The tool list itself never changes, so prompt caching keeps
working. Commit `.hx/ext/` and the harness grows with the project.

## Host primitives

Everything Luau can do to the machine:

| Primitive | Returns |
|---|---|
| `host.llm(body, {timeout=600})` | `status, text`, or `nil, error` |
| `host.sh(command, {timeout=120})` | `{out, code, signal, timed_out}` |
| `host.read(path)` | the contents, or `nil, error` |
| `host.write(path, data)` | `true`, or `nil, error`; creates parent directories |
| `host.list(dir)` | sorted names, directories ending in `/`, or `nil, error` |
| `host.load(source, name, env)` | a function, or `nil, error` |
| `host.try(fn, ...)` | `true, ...`, or `false, error`: a `pcall` that is also where a stop ends |
| `host.timed(seconds, fn, ...)` | as `host.try`; the error is `"timeout"` when the limit passed |
| `host.input(prompt)` | a line from stdin, or `nil` at the end |
| `host.log(text)`, `host.sleep(seconds)`, `host.env(name)` | |
| `host.args`, `host.cwd`, `host.interactive`, `host.sandboxed`, `host.default_loop` | |
| `json.encode(value)`, `json.decode(text)`, `json.array(t)`, `json.null` | |

Two rules hold across all of them:

- **A stop cannot be swallowed.** Ctrl-C raises `"interrupted"` from whichever call it cut short, and again at
  every later step of Luau code and every blocking call, until the outermost `host.try` or `host.timed` returns
  `false, "interrupted"`. That is the only place a stop ends, so a plain `pcall` cannot make it go away: outside
  any `host.try` it ends the run. SIGTERM and SIGHUP raise `"terminated"` the same way.
- **A time limit reaches blocking calls.** Inside `host.timed`, `llm`, `sh` and `sleep` give up when the limit
  passes, and limits nest.

`json.encode` writes object keys in sorted order, so equal values always produce the same bytes.

## Source

| File | Holds |
|---|---|
| `src/main.cpp` | arguments, secrets, the sandbox rules, starting the VM |
| `src/sandbox.cpp` | Landlock; `Sandbox` is the proof that confinement was decided, and a `Host` cannot be built without it |
| `src/host.cpp` | the `host` primitives, signals, time limits |
| `src/json.cpp` | JSON between Lua values and text, on RapidJSON's streaming reader and writer |
| `lua/loop.luau` | the agent |

Development build, with AddressSanitizer, UBSan and warnings as errors:

```
cmake --preset dev && cmake --build --preset dev && ctest --preset dev
clang-format --dry-run --Werror src/*.cpp src/*.h
clang-tidy -p build-dev --quiet src/*.cpp
```

## Limits

- One endpoint, which must accept the Anthropic Messages format and an `x-api-key` header. The default is
  DeepSeek's; `HX_API_URL=https://api.anthropic.com/v1/messages` with `HX_KEY_ENV=ANTHROPIC_API_KEY` and a
  Claude model in `HX_MODEL` has not been tried.
- No streaming: a reply appears when it is complete.
- Tool calls in one reply run one after another.
- The whole conversation is encoded again for every request. At 5 MB that takes 16 ms.
- Linux only.
- Numbers are doubles, so integers above 2^53 in JSON lose precision.
