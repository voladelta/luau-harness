---
name: hx
description: Use, extend or change hx, the Luau coding-agent host. Use when asked to hand hx a task, script it with `hx --run`, judge a change with `hx --judge`, add an extension, skill, project function or sub-agent under `.hx/`, or work on hx's own source.
---
# hx

hx is a coding-agent host for one project at a time: a Luau VM, one tool (`lua`) that runs Luau, and one
Landlock wall around all of it. The agent loop is a Luau file, so the harness grows by files written into the
project.

You are in one of two places:

- **Outside**: in a shell, as a person or another agent such as Claude Code. You start `hx` as a command.
- **Inside**: you are the model hx runs. You have the `lua` tool, and your code calls `bash`, `read`, `write`,
  `edit`, `skill` and `ext`.

The files under `.hx/` mean the same whoever writes them, so extending hx is the same work from either place.

## What to make

Take the first row that does the job.

| The need | Make | Takes effect |
|---|---|---|
| A chore in code that will come up again | an extension, `.hx/ext/NAME.luau` | at the next `ext("NAME")` |
| A procedure to follow, as text | a skill, `.hx/skills/NAME/SKILL.md` | at the next `skill("NAME")` |
| Notes every session should read | `.hx/prompt.md` | next session |
| A function the model always has, a changed setting or prompt, a sub-agent | `.hx/loop.luau` | next session |
| A script with hx's primitives and wall, and no model | `hx --run FILE` | at once |
| A new primitive, another wall, a built-in extension | hx's own source | after a build |

The system prompt is built once, when a session starts: an extension or skill written now can be called now, and
is listed from the next session on. Commit `.hx/`; `.hx/log/` keeps itself out of git.

## Running hx, from outside

```
hx [options] TASK...                  the agent; every word from the first that is not an option is the task
hx [options] < task.txt               with no task words, the task is all of stdin
hx [options] --run FILE [ARGS...]     a Luau script with the host primitives; FILE - reads the script from stdin
hx --init [--full]                    write .hx/loop.luau (see "The loop")
hx --judge KIT [options] TASK...      the agent, with a judge (see "The judge")
```

- Start it in the project directory. It refuses `/`, the home directory and any directory that holds it.
- It needs a key in the environment: `DEEPSEEK_API_KEY`, or `ANTHROPIC_API_KEY` for a `claude-*` model.
  `HX_KEY_ENV=NAME` reads it from another variable. `--model opus|sonnet|haiku|NAME` or `HX_MODEL` picks the
  model. `--run` needs no key.
- The answer goes to stdout, progress and errors to stderr: `hx -q "..." > answer.txt`. Exit status: 0 done, 1 the
  run failed, 2 bad usage or hx cannot start here, 130 interrupted, 143 terminated.
- When no person is at the terminal, pass `--no-input` and the task as words or on stdin. Stdin is read only when
  there are no task words: `git diff | hx review this` sends the words alone, and
  `{ echo review this change; git diff; } | hx` sends both. A task takes minutes, so give the command a long
  timeout or run it in the background.
- The wall: hx writes only the project directory and its own `$TMPDIR`, and reads those plus the system
  directories. A toolchain in the home directory needs `--ro PATH` or `--rw PATH`. Variables that look like
  secrets are taken out of the environment of commands; `--keep-env NAME` keeps one. The network is open.
  `--no-sandbox` takes the wall away: do not pass it unless the user asked.
- Nothing asks before a file changes, and git is the undo: commit first, then read `git diff`. For a task that
  reads text nobody vouches for, run hx in a clone and pull from it, since hooks and `.git/config` do not show
  in a diff.
- To see what a session did, read its log: where a `.hx` directory exists, each session's conversation is saved
  as JSON in `.hx/log/TIME-ID.json`, and each sub-agent's beside it as `TIME-ID-sub-N.json`. A project with no
  `.hx` directory keeps no log.
- `hx --help` has every option and variable.

## From inside a session

- An extension or a skill you write is there at once: call it in the session that wrote it. A change to
  `.hx/loop.luau` or `.hx/prompt.md` waits for the next session.
- Commands you start do not have the API key, so a nested `hx TASK` cannot reach the model. To hand work to
  another conversation, add a sub-agent function to the loop (below). A nested `hx --run FILE` works when the `hx`
  binary is inside the wall, as one installed under `/usr` is.
- `host.default_loop` is the built-in loop's source, the reference for everything `M` has:
  `host.write(host.env("TMPDIR") .. "/loop.luau", host.default_loop)`, then `read` or `rg` it. From outside,
  `echo 'print(host.default_loop)' | hx --run -` prints it.

## Extensions

`.hx/ext/NAME.luau`, where NAME is letters, digits, `_` and `-`. The first line is a comment that says what it
does and how to call it: that line is all a later session's prompt shows. The module returns a table of
functions.

```lua
-- counts TODO comments per file: count(dir) -> text
return {
	count = function(dir)
		return host.sh(("grep -rc TODO %s | grep -v ':0$'"):format(dir)).out
	end,
}
```

- Call it as `ext("NAME").count("src")`. A changed file is loaded again at its next use.
- It sees what the model's code sees: `bash`, `read`, `write`, `edit`, `skill`, `ext`, `host.*`, `json.*`. Its
  globals are its own, and what it prints goes into the tool result.
- Report failure with `error(message, 0)`. Return text, or a table: a table shows as JSON, unless its metatable
  has `__tostring`.
- A name that starts with `_` is a helper for other extensions and is not listed. `web`, `ocr`, `exa` and `_http`
  are built in; a project's file of the same name takes the place of one.
- Keep it as small as the chore. Try it before you report: from inside, call it; from outside,

```
echo 'local loop = host.load(host.default_loop, "=loop")() print(loop.ext("NAME").count("src"))' | hx -q --run -
```

## Skills

`NAME/SKILL.md` in `.hx/skills/`, `.agents/skills/` or `.claude/skills/`, searched in that order, then in
`~/.agents/skills/` and `~/.claude/skills/` when `--ro` opens them, then among the skills built into hx (this
one). The first skill of a name wins. Claude Code reads `.claude/skills/` too, so a skill both agents should
follow goes there, once.

```
---
name: release-notes
description: Write release notes from the git log. Use when asked for release notes or a changelog entry.
---
Group commits by area, newest first. Run scripts/authors.sh for the credits line.
```

- The description is what a model picks the skill by: say what it does and when to use it, within 250
  characters, the most hx lists.
- Scripts and references go beside `SKILL.md`; `skill(NAME)` names them by their paths from the project.
- hx reads `name`, `description` and `disable-model-invocation: true` (not listed, still loads by name), and
  ignores other fields. A name no skill has, as in `skill("?")`, lists the skills with their descriptions.

## The loop

`hx --init` writes `.hx/loop.luau`, which loads the built-in loop as `M`, changes what the project needs and
returns it. What it leaves alone keeps up with hx; `--full` copies the whole loop instead, which the project then
owns. Change these through `M`:

- Settings: `M.max_replies` (200), `M.max_output` (16000 tokens a reply), `M.compact_at` (150000 prompt tokens),
  `M.output_limit` (30000 bytes of a result), `M.call_seconds` (600), `M.layout_lines` (60), `M.skill_dirs`.
- Parts to replace or wrap: `M.system_prompt`, `M.layout`, `M.request`, `M.turn`, `M.compact`,
  `M.compact_prompt`, `M.functions`, `M.tools`.

A function the model's code can call, here one that hands a task to a sub-agent with a conversation of its own:

```lua
local M = host.load(host.default_loop, "=loop")()

local system -- built at the first use, once, so that every sub-agent reads the same cached prompt
table.insert(M.functions, {
	name = "delegate",
	usage = "delegate(task) -> text: gives the task to a sub-agent with a fresh conversation; returns its last words.",
	run = function(task)
		system = system or M.system_prompt()
		local talk = M.Conversation.new()
		talk:say(task)
		M.turn(system, talk) -- replies and tool calls until the sub-agent is done; raises if it fails
		local words = {}
		for _, block in talk.messages[#talk.messages].content do
			if block.type == "text" then
				table.insert(words, block.text)
			end
		end
		return table.concat(words, "\n")
	end,
})

return M
```

- `usage` is the one line the model reads about a function, in the `lua` tool's description, not in the system
  prompt; `run` returns what the model should see and raises to report failure.
- A sub-agent shares the model's globals and the wall. Its words also go to stdout, its replies are counted in
  the line at the end of the run, and its conversation has a log of its own (see "Running hx").
- A sub-agent runs on another model when `M.model` is set for its turn and put back however the turn ends:
  `local outer = M.model; M.model = "deepseek-flash"; local ok, err = pcall(M.turn, system, talk); M.model = outer`,
  then `error(err, 0)` when `ok` is false. Under a session on a Claude model that is another Claude model
  (`claude-sonnet-5-5`), or a DeepSeek model, which needs `DEEPSEEK_API_KEY` set when hx starts. A session on
  DeepSeek cannot hand a turn to Claude.
- Keep what goes to the model the same from request to request, or the prompt cache is lost: build prompt text
  once, and put nothing that changes (a clock, a counter) into the system prompt or a `usage` line.
- A loop that does not load, or returns no table with a `main` function, is reported on stderr and the built-in
  loop runs instead, so check after every change:

```
echo 'local M = assert(host.load(assert(host.read(".hx/loop.luau")), "=.hx/loop.luau"))()
assert(type(M.main) == "function") print("loop ok")' | hx -q --run -
```

## Scripts: `hx --run`

A script gets `host`, `json` and the standard Luau libraries, behind the same wall as the agent, with no model
and no key. Its arguments are `host.args`, and `print` writes to stdout. There is no `require`: load another file
with `host.load(host.read(path), "=" .. path)()`. An error exits 1. A script that returns a table with a `main`
function is a module: `main()` runs, and the number it returns is the exit status.

| Primitive | Returns |
|---|---|
| `host.sh(command, {timeout=120})` | `{out, code, signal, timed_out, seconds}` |
| `host.read(path)`, `host.write(path, data)`, `host.list(dir)` | the value, or `nil, error`: they do not raise. `write` makes missing directories |
| `host.load(source, name, env?)` | a function, or `nil, error` |
| `host.try(fn, ...)`, `host.timed(seconds, fn, ...)` | `true, ...` or `false, error` |
| `host.spawn(fn, ...)` | an id: forks hx, and the child runs `fn(...)` in a copy of the VM while the parent goes on. Only files and the string `fn` returns reach the parent |
| `host.wait(seconds?)` | `id, ok, text` for the first child that has ended (`text` is what `fn` returned or raised), or `nil, running` when none ends in time |
| `host.clip(text, limit)` | `text`, or `head, tail` when it is longer |
| `host.log(text)`, `host.input(prompt)` | a line to stderr; a line from stdin, or `nil` at its end |
| `host.sleep(seconds)`, `host.env(name)` | |
| `host.args`, `host.cwd`, `host.model`, `host.sandboxed`, `host.quiet`, `host.default_loop` | values, not calls |
| `json.encode(value)`, `json.decode(text)`, `json.array(t)`, `json.null` | keys are written sorted |

Ctrl-C raises `"interrupted"` again at every step until the outermost `host.try` or `host.timed` returns, so a
plain `pcall` cannot swallow a stop. Inside `host.timed`, `sh`, `sleep` and the model call give up when the limit
passes.

## The judge

With `--judge KIT` the model only proposes: a judge it cannot read measures each `submit(note)` against the best
version so far and keeps a change only when it wins twice. Use it for tasks with a number to push, where an agent
could otherwise make the number only look better.

- A kit is a directory outside the wall: `run` (required, executable, exits 0 when it worked, timed by hx),
  optional `build`, optional `check DIR` (exits 0 when the output in `DIR/.judge/stdout` is right; a number on its
  last line is the score, higher is better), optional `kit.json`
  (`{"runs": 5, "margin": 0.02, "timeout": 600, "blind": false}`).
- Keep the answers in `check`: the candidate's code can read `run`. Before trusting a kit, try it on the
  unchanged project and on a change known to be bad.
- Verdicts, patches and `ledger.jsonl` go to `~/.local/state/hx/judge/PROJECT-TIME-XXXXXX`; `champion.patch`
  there applies with `git apply`.

## Changing hx itself

In hx's repository, `README.md` is the reference: it describes every behaviour, and a change that alters one
changes its paragraph too.

| To change | Edit |
|---|---|
| The agent: prompt, functions, turns, compaction, skills | `lua/loop.luau` |
| What `hx --init` writes | `lua/overlay.luau` |
| A built-in extension (each file is one, by its name) | `lua/ext/NAME.luau` |
| This skill, which is built into hx | `.claude/skills/hx/SKILL.md` |
| A primitive | `src/host*.cpp`, and the lists in `src/host.h`, `lua/loop.luau` and the README |
| Options, secrets, what the wall lets in | `src/main.cpp`, `src/sandbox.cpp` |
| The judge | `lua/judge.luau`, `src/judge.cpp` |

```
cmake --preset release && cmake --build --preset release && ctest --preset release
cmake --preset dev && cmake --build --preset dev && ctest --preset dev      # ASan, UBSan, warnings are errors
clang-format --dry-run --Werror src/*.cpp src/*.h
clang-tidy -p build-dev --quiet src/*.cpp
luau-analyze lua/*.luau lua/ext/*.luau tests/*.luau                          # prints nothing when all is well
```

- A build needs a C++20 compiler, CMake 3.28, Ninja and libcurl's headers (`libcurl4-openssl-dev` on Debian and
  Ubuntu). The first configure downloads Luau, RapidJSON and {fmt}.
- The Luau sources and this skill are compiled into the binary, so build again after editing one.
- Luau files are `--!strict`. Tests are `tests/NAME_test.luau`, which share `tests/check.luau` and run through
  `hx --run`, and `tests/agent_test.py`, which drives the whole agent against a scripted model. A new behaviour
  gets a check in one of them.
- One runs by hand from a scratch directory: `hx -q --ro /path/to/tests --run /path/to/tests/skills_test.luau`.
- Comments and commit messages say what is true now, in plain sentences: `loop: an edit in a \r\n file keeps its
  line ends`.
