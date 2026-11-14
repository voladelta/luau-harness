# hxm

A small coding-agent host for one project at a time: a Luau VM, five tools, and one kernel-enforced wall.

`hxm` is the minimal take on [`hx`](../README.md). The host is about 1,500 lines of C++. The agent loop is one
Luau file that your project owns and edits. The model can extend the harness by writing Luau modules into the
project.

```
cd my-project
export DEEPSEEK_API_KEY=...
hxm "add a --json flag to the export command and test it"
```

## The idea

- **The host is a kernel, not a framework.** It gives Luau a model call, a shell, file access and JSON. That is
  all. There is no plugin system, package format, permission prompt or configuration file.
- **The loop belongs to the project.** `hxm init` copies the built-in loop to `.hxm/loop.luau`. From then on
  that file is the harness for this project: the prompt, the tools, the retry and compaction rules. Change it by
  editing it.
- **One wall, built by the kernel.** Before anything runs, the process confines itself with Landlock. The VM,
  the file tools and every command the model starts inherit the same rules, so no tool needs a path check and
  no command needs to be judged by its text.
- **Extensions have no more power than the model.** An extension is Luau running in the same confined process.
  It can do nothing the model's own shell could not, so nothing needs to be approved, staged or trusted.

## Build

Needs Linux 5.13 or later (Landlock), a C++20 compiler, CMake 3.28, libcurl headers, and `bash` at run time.

```
cmake -S . -B build -G Ninja
cmake --build build
ctest --test-dir build        # needs python3 and a kernel with Landlock
cmake --install build         # optional
```

The first build downloads Luau 0.747 and compiles its VM and compiler (about 20 seconds).

## Use

```
hxm [options] [TASK...]            run the agent; without a task, read it from stdin or start a prompt
hxm [options] run FILE [ARGS...]   run a Luau script with the host primitives
hxm init                           make this project own its loop
```

| Option | Meaning |
|---|---|
| `--ro PATH` | also allow reading `PATH` |
| `--rw PATH` | also allow writing `PATH` |
| `--keep-env NAME` | keep a secret-looking variable in the environment of commands |
| `--no-sandbox` | run without Landlock |

| Variable | Meaning | Default |
|---|---|---|
| `HXM_API_URL` | chat-completions endpoint | `https://api.deepseek.com/chat/completions` |
| `HXM_KEY_ENV` | name of the variable holding the API key | `DEEPSEEK_API_KEY` |
| `HXM_MODEL` | model name | `deepseek-flash` |

The model's words go to stdout and progress lines go to stderr, so `hxm "..." > answer.txt` captures the answer.
At the prompt, Ctrl-C stops the current turn and Ctrl-D leaves.

## The wall

| | Allowed |
|---|---|
| Write | the project directory (where you start `hxm`), a private `$TMPDIR` that is removed on exit, `--rw` paths |
| Read | the above, plus `/usr /bin /sbin /lib* /etc /opt /proc /sys /dev /nix/store`, your git configuration, `--ro` paths |
| Everything else | cannot be opened, including the rest of your home directory |

Toolchains installed in your home directory need a flag, for example
`hxm --ro ~/.rustup --ro ~/.cargo --rw ~/.cargo/registry "..."`. The flags are on the command line on purpose:
nothing inside the project, which the model can write, can widen the wall.

Also in force:

- `hxm` refuses to start in `/` or in your home directory, where the wall would protect nothing.
- No setuid program can raise privileges (`no_new_privs`), so `sudo` does not work inside.
- On Linux 6.12 or later, confined processes cannot signal processes outside the sandbox.
- The API key is read once and removed from the environment, together with every variable whose name ends in
  `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD` or `CREDENTIALS`. Luau code never sees the key: the host adds
  it to requests for the configured endpoint only.
- Every process group a command started is killed when `hxm` exits.

### What the wall does not do

- **The network is open.** The model can fetch and send anything it can read. The wall limits what it can read.
- **Secrets stored in the project are readable**, as are secrets in variables with unusual names. Use
  `env -i` or a clean shell if you need certainty.
- **A process that calls `setsid` outlives `hxm`.** It stays confined, but it is not killed.
- **The model can edit `.hxm/loop.luau` and its extensions.** That is the point, and it shows in `git diff`.
  If the loop no longer compiles, the built-in one runs instead.
- **Git is the undo.** Nothing asks before the model changes or deletes a file in the project.

## The loop

`lua/loop.luau` is the whole agent: about 500 lines. It speaks the OpenAI chat-completions format. The five
tools it defines:

| Tool | Does |
|---|---|
| `bash` | runs a command, returns its output and exit status, kills it after a timeout |
| `read` | returns a range of lines of a file |
| `write` | creates or overwrites a file |
| `edit` | replaces an exact piece of text that occurs once |
| `lua` | runs Luau inside the harness; this is how extensions are called |

To add a project tool, add an entry to `M.tools` in your `.hxm/loop.luau`. To change the prompt, edit
`system_prompt()`, or put notes for the model in `.hxm/prompt.md`.

When a `.hxm` directory exists, each session's conversation is saved to `.hxm/log/` as JSON.

## Extensions

An extension is a Luau module at `.hxm/ext/NAME.luau`. Its first line is a comment that says what it does, and
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
working. Commit `.hxm/ext/` and the harness grows with the project.

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
| `host.timed(seconds, fn, ...)` | `true, ...`, or `false, error`; the error is `"timeout"` when the limit passed |
| `host.input(prompt)` | a line from stdin, or `nil` at the end |
| `host.log(text)`, `host.sleep(seconds)`, `host.env(name)` | |
| `host.args`, `host.cwd`, `host.interactive`, `host.default_loop` | |
| `json.encode(value)`, `json.decode(text)`, `json.array(t)`, `json.null` | |

`json.encode` writes object keys in sorted order, so equal values always produce the same bytes.

## Limits

- One endpoint, bearer authentication, no streaming: a reply appears when it is complete.
- Linux only.
- Numbers are doubles, so integers above 2^53 in JSON lose precision.
