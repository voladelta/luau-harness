# hx

A small coding-agent host for one project at a time: a Luau VM, one tool that runs Luau, and one kernel-enforced
wall.

The host is a few C++ files. The agent loop is one Luau file, and your project can change any part of
it. The model can extend the harness by writing Luau modules into the project.

```
cd my-project
export DEEPSEEK_API_KEY=...         # or ANTHROPIC_API_KEY=... for Claude
hx "add a --json flag to the export command and test it"
hx --model sonnet "same, on Claude Sonnet 5.5"
hx --rpi "move the config loader to TOML"   # Opus researches and plans, deepseek-flash agents implement
hx --resume last "now add the tests"        # go on from the last conversation saved in .hx/log
```

## The idea

- **The host is a kernel, not a framework.** It gives Luau a POST to the endpoints it holds the keys for, a
  shell, file access and JSON. That is all. There is no plugin system, package format, permission prompt or
  configuration file.
- **The loop belongs to the project.** `hx --init` writes `.hx/loop.luau`, a short file that loads the loop built
  into hx and changes what this project needs: the model, the prompt, the functions, the retry and compaction
  rules. What it leaves alone keeps up with hx. `hx --init --full` copies the whole built-in loop instead, and
  the project owns all of it.
- **One tool, and it is code.** The model acts by sending Luau to its one tool, `lua`. `bash`, `read`, `write`
  and `edit` are functions that code calls, next to the project's own functions and extensions, so a step that
  takes ten calls elsewhere can be one loop here.
- **One wall, built by the kernel.** Before anything runs, the process confines itself with Landlock. The VM,
  the file functions and every command the model starts inherit the same rules, so no function needs a path
  check and no command needs to be judged by its text.
- **A change counts when a judge says so.** With `--judge KIT`, a second process, behind a wall of its own,
  measures what the model submits with a kit the model cannot read, and keeps only gains that hold up twice.
- **A lead and its agents.** The model can give tasks to agents, each a model with a conversation of its own in a
  fork of hx behind the same wall, and they work at the same time. With `--rpi`, Opus leads by the rpi workflow:
  it researches and plans, agents on `deepseek-flash` implement, and it verifies their work and sends back what
  fails.
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
compiler), [yyjson](https://github.com/ibireme/yyjson) and [{fmt}](https://github.com/fmtlib/fmt) (header-only).

## Use

```
hx [options] [TASK...]              run the agent; without a task, read it from stdin or start a prompt
hx [options] --run FILE [ARGS...]   run a Luau script with the host primitives (FILE - reads stdin)
hx --init [--full]                  write .hx/loop.luau, which starts from the built-in loop (--full: a copy)
hx --judge KIT [options] [TASK...]  run the agent with a judge: only what it submits and the kit confirms counts
hx --rpi [options] [TASK...]        run the agent as a lead: it plans, its agents implement, it verifies
hx --help | --version
```

A task is any words at all: `hx run the tests` asks the model. The model's words go to stdout and progress lines to
stderr, so `hx "..." > answer.txt` captures the answer. Every option, variable and exit status is in
[docs/usage.md](docs/usage.md).

## Documentation

| Topic | What it covers |
|---|---|
| [Usage](docs/usage.md) | options, environment variables, exit status, the prompt |
| [The wall](docs/wall.md) | what Landlock lets in, how secrets are kept from the model, and what the wall does not do |
| [The loop](docs/loop.md) | the agent's one tool and its functions, `.hx/loop.luau`, Claude settings, compaction, caching, logs |
| [Agents](docs/agents.md) | `agents()`, which runs tasks side by side, and the `--rpi` workflow |
| [The judge](docs/judge.md) | `--judge KIT`: kits, trials, verdicts and the walls that keep them honest |
| [Extensions](docs/extensions.md) | `.hx/ext/`, and the built-in `bytes`, `text`, `path`, `search`, `http`, `web`, `ocr` and `exa` |
| [Skills](docs/skills.md) | where hx finds skills, how the model loads them, and the built-in `hx` and `rpi` |
| [Host primitives](docs/host.md) | every `host.*` and `json.*` call Luau has |
| [Development](docs/development.md) | the source files, the dev build, linters and tests |

## Limits

- A session's endpoint is fixed at start. It must accept the Anthropic Messages format and an `x-api-key` header:
  the Claude API for a `claude-*` model, DeepSeek's otherwise. The model can change for a turn, a sub-agent's
  say, when `M.model` is set for the turn and put back after it. Under an Opus session a turn then runs on
  Sonnet, with its own model's settings from `M.claude`, or on a DeepSeek model, which is asked through
  DeepSeek's endpoint with `DEEPSEEK_API_KEY` and gets the plain request: Opus leads and `deepseek-flash` does
  the sub-agents' work. The other way round is not there: a session on DeepSeek holds no key of Claude's, and a
  project loop that changes `M.model` to a Claude model there still talks to DeepSeek. The Claude settings have
  been checked against a stand-in server, and `tests/live_test.py` passes against the live API on Opus 5.5 and
  Sonnet 5.5. Haiku 5.5 passes 13 to 15 of its fifteen checks from run to run: told to use the edit function, it
  often tries a tool named `edit` first, which does not exist, is told so, and then goes through `lua`; the file
  still comes out right. More effort does not help it: at `HX_EFFORT=high` two runs passed 13. Anthropic places
  Haiku for high-volume and sub-agent work, and that is where it fits here: under a Sonnet or Opus session, not
  as the session's own model.
- Agents share one project directory. Nothing keeps two of them from writing the same file or from building in
  the same directory at once but the lead's plan; there is no worktree or copy per agent.
- No streaming: a reply appears when it is complete.
- Tool calls in one reply run one after another.
- The whole conversation is encoded again for every request. At 5 MB that takes 16 ms.
- Linux only.
- Numbers are doubles, so integers above 2^53 in JSON lose precision.

## License

MIT. See [LICENSE](LICENSE).
