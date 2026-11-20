# Usage

```
hx [options] [TASK...]              run the agent; without a task, read it from stdin or start a prompt
hx [options] --run FILE [ARGS...]   run a Luau script with the host primitives (FILE - reads stdin)
hx --init [--full]                  write .hx/loop.luau, which starts from the built-in loop (--full: a copy)
hx --judge KIT [options] [TASK...]  run the agent with a judge: only what it submits and the kit confirms counts
hx --rpi [options] [TASK...]        run the agent as a lead: it plans, its agents implement, it verifies
hx --help | --version
```

A task is any words at all: `hx run the tests` asks the model. Options go before the task; from the first
word that is not an option on, everything is the task, and `--` ends the options early. After `--run FILE`,
every word goes to the script, even one that starts with `-`. A value can follow its option as the next word or
after `=`: `--ro /opt/sdk` and `--ro=/opt/sdk` are the same (the shell does not expand `~` after `=`).

With a task on the command line, stdin is not read: it stays there for the model's code, which reads a line of
it with `host.input`. Text that goes with an instruction goes on stdin with it:
`{ echo review this change; git diff; } | hx`.

| Option | Meaning |
|---|---|
| `--judge KIT` | judge the model's work with the kit in directory `KIT`, which must lie outside the wall (see [The judge](judge.md)) |
| `--model NAME` | the model to ask; overrides `HX_MODEL`. `opus`, `sonnet` and `haiku` are the current Claude models |
| `--rpi` | lead the task by the rpi workflow (see [Agents](agents.md)): the model is `opus` unless `--model` names another, whatever `HX_MODEL` says |
| `--ro PATH` | also allow reading `PATH` |
| `--rw PATH` | also allow writing `PATH` |
| `--resume LOG` | go on from a conversation saved in `.hx/log/`: `LOG` is the file, by its name there or its path, or `last` for the newest that is not an agent's (see [The loop](loop.md)) |
| `--endpoint NAME=URL` | one more endpoint for `host.post(NAME, body)`, for an extension of your own. Its key is read from `NAME_API_KEY`, upper-cased with `-` as `_` (`my-api` has `MY_API_API_KEY`), taken out of the environment like the other keys and sent as a Bearer token to that URL only. May be given more than once |
| `--keep-env NAME` | keep a secret-looking variable in the environment of commands |
| `--unix-sockets` | before Landlock ABI 9, let commands create unix sockets (see [The wall](wall.md)) |
| `--no-sandbox` | run without Landlock; the model is told it is not confined |
| `--built-in-loop` | run the loop built into hx and leave `.hx/loop.luau` unread. Without it, a `.hx/loop.luau` that does not load stops hx with exit status 2 |
| `-q`, `--quiet` | leave out progress lines; the answer, warnings and errors still show |
| `--no-input` | never prompt: with no task and stdin a terminal, fail instead of waiting |
| `-h`, `--help` / `--version` | print help or the version to stdout |

| Variable | Meaning | Default |
|---|---|---|
| `HX_MODEL` | model name; `opus`, `sonnet` and `haiku` stand for `claude-opus-5-5`, `claude-sonnet-5-5` and `claude-haiku-5-5` | `deepseek-flash`, or `claude-opus-5-5` when `ANTHROPIC_API_KEY` is set and `DEEPSEEK_API_KEY` is not |
| `HX_API_URL` | Anthropic Messages endpoint | `https://api.anthropic.com/v1/messages` for a `claude-*` model, `https://api.deepseek.com/anthropic/v1/messages` otherwise |
| `HX_KEY_ENV` | name of the variable holding the API key | `ANTHROPIC_API_KEY` for a `claude-*` model, `DEEPSEEK_API_KEY` otherwise |
| `HX_EFFORT` | how hard the model thinks: `low`, `medium`, `high`, `xhigh` or `max`, sent as `output_config.effort`; any other value is refused before the model is asked. DeepSeek has three levels: it takes `medium` and `xhigh` for `high` | `medium` on the Claude models below, the endpoint's otherwise (`high` on DeepSeek) |
| `HX_TOOLS_URL` | base of DeepSeek's Search, Fetch and OCR endpoints, which the built-in `web` and `ocr` extensions call with `DEEPSEEK_API_KEY`, whatever the model | `https://api.deepseek.com/v1/tools` |
| `HX_EXA_URL` | Exa's search endpoint, which the built-in `exa` extension calls with `EXA_API_KEY`, whatever the model | `https://api.exa.ai/search` |
| `HX_DEEPSEEK_URL` | DeepSeek's Messages endpoint, which a DeepSeek model under a session on a Claude model is asked through, an agent say, with `DEEPSEEK_API_KEY` | `https://api.deepseek.com/anthropic/v1/messages` |
| `HX_ANTHROPIC_URL` | Anthropic's Messages endpoint, which a Claude model under a session on another model is asked through, with `ANTHROPIC_API_KEY` | `https://api.anthropic.com/v1/messages` |

The model's words go to stdout and progress lines go to stderr, so `hx "..." > answer.txt` captures the answer.
Exit status: 0 done, 1 the run failed, 2 bad usage or `hx` cannot start here, 130 interrupted (Ctrl-C), 143
terminated. A misspelt option is refused with the closest real one suggested; nothing is sent to the model.

At the prompt, each line is a message and a pasted message of many lines is one message (in a terminal that
supports bracketed paste); Ctrl-C stops the current turn and Ctrl-D leaves.
