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
```

## The idea

- **The host is a kernel, not a framework.** It gives Luau a model call, a shell, file access and JSON. That is
  all. There is no plugin system, package format, permission prompt or configuration file.
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
hx [options] [TASK...]              run the agent; without a task, read it from stdin or start a prompt
hx [options] --run FILE [ARGS...]   run a Luau script with the host primitives (FILE - reads stdin)
hx --init [--full]                  write .hx/loop.luau, which starts from the built-in loop (--full: a copy)
hx --judge KIT [options] [TASK...]  run the agent with a judge: only what it submits and the kit confirms counts
hx --help | --version
```

A task is any words at all: `hx run the tests` asks the model. Options go before the task; from the first
word that is not an option on, everything is the task, and `--` ends the options early. After `--run FILE`,
every word goes to the script, even one that starts with `-`. A value can follow its option as the next word or
after `=`: `--ro /opt/sdk` and `--ro=/opt/sdk` are the same (the shell does not expand `~` after `=`).

| Option | Meaning |
|---|---|
| `--judge KIT` | judge the model's work with the kit in directory `KIT`, which must lie outside the wall (see "The judge") |
| `--model NAME` | the model to ask; overrides `HX_MODEL`. `opus`, `sonnet` and `haiku` are the current Claude models |
| `--ro PATH` | also allow reading `PATH` |
| `--rw PATH` | also allow writing `PATH` |
| `--keep-env NAME` | keep a secret-looking variable in the environment of commands |
| `--unix-sockets` | before Landlock ABI 9, let commands create unix sockets (see "The wall") |
| `--no-sandbox` | run without Landlock; the model is told it is not confined |
| `-q`, `--quiet` | leave out progress lines; the answer, warnings and errors still show |
| `--no-input` | never prompt: with no task and stdin a terminal, fail instead of waiting |
| `-h`, `--help` / `--version` | print help or the version to stdout |

| Variable | Meaning | Default |
|---|---|---|
| `HX_MODEL` | model name; `opus`, `sonnet` and `haiku` stand for `claude-opus-5-5`, `claude-sonnet-5-5` and `claude-haiku-5-5` | `deepseek-flash`, or `claude-opus-5-5` when `ANTHROPIC_API_KEY` is set and `DEEPSEEK_API_KEY` is not |
| `HX_API_URL` | Anthropic Messages endpoint | `https://api.anthropic.com/v1/messages` for a `claude-*` model, `https://api.deepseek.com/anthropic/v1/messages` otherwise |
| `HX_KEY_ENV` | name of the variable holding the API key | `ANTHROPIC_API_KEY` for a `claude-*` model, `DEEPSEEK_API_KEY` otherwise |
| `HX_EFFORT` | how hard the model thinks: `low`, `medium`, `high`, `xhigh` or `max`, sent as `output_config.effort`; any other value is refused before the model is asked | `medium` on the Claude models below, the endpoint's otherwise (`high` on DeepSeek) |
| `HX_TOOLS_URL` | base of DeepSeek's Search, Fetch and OCR endpoints, which the built-in `web` and `ocr` extensions call with `DEEPSEEK_API_KEY`, whatever the model | `https://api.deepseek.com/v1/tools` |
| `HX_EXA_URL` | Exa's search endpoint, which the built-in `exa` extension calls with `EXA_API_KEY`, whatever the model | `https://api.exa.ai/search` |

The model's words go to stdout and progress lines go to stderr, so `hx "..." > answer.txt` captures the answer.
Exit status: 0 done, 1 the run failed, 2 bad usage or `hx` cannot start here, 130 interrupted (Ctrl-C), 143
terminated. A misspelt option is refused with the closest real one suggested; nothing is sent to the model.

At the prompt, each line is a message and a pasted message of many lines is one message (in a terminal that
supports bracketed paste); Ctrl-C stops the current turn and Ctrl-D leaves.

## The wall

| | Allowed |
|---|---|
| Write | the project directory (where you start `hx`), a private `$TMPDIR` that is removed on exit, `--rw` paths |
| Read | the above, plus `/usr /bin /sbin /lib* /etc /opt /proc /sys /nix/store`, your git configuration, trust roots (below), `--ro` paths |
| Devices | `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom` |
| Everything else | cannot be opened: the rest of your home directory, other terminals, other projects |

Toolchains installed in your home directory need a flag, for example
`hx --ro ~/.rustup --ro ~/.cargo --rw ~/.cargo/registry "..."`. The flags are on the command line on purpose:
nothing inside the project, which the model can write, can widen the wall.

Behind a TLS-intercepting proxy the trust roots often live in your home directory. The file or directory named
by each of `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`, `REQUESTS_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`,
`GIT_SSL_CAINFO`, `CARGO_HTTP_CAINFO`, `AWS_CA_BUNDLE` and `NIX_SSL_CERT_FILE`, when it is an absolute path, is
readable inside, so `curl`, `git`, `pip` and `npm` started by the model trust what you trust. These come from your
environment, which the model cannot change. `hx`'s own request uses `SSL_CERT_FILE`, or else `CURL_CA_BUNDLE`.

Also in force:

- `hx` refuses to start in `/`, in your home directory or in any directory that contains it, such as `/home`,
  where the wall would leave your home directory open. It refuses a `--ro` or `--rw` path of that kind too.
- No setuid program can raise privileges (`no_new_privs`), so `sudo` does not work inside.
- On Linux 6.12 or later, confined processes cannot signal processes outside the sandbox or reach their
  abstract unix sockets.
- Unix sockets by path are walled off too. From Landlock ABI 9 the kernel refuses a connection to a socket whose
  path the rules above do not make readable. Before ABI 9 Landlock cannot, so on x86-64 and aarch64 a seccomp
  filter refuses to create unix sockets (a stream `socketpair` still works, a datagram one does not, because
  it could still send to any path; `hx` itself falls back where it would use a unix socket, such as for name
  resolution). The filter also refuses `io_uring`, which can create sockets
  without the socket call, and every call from 32-bit code. That breaks tools that open unix sockets by path:
  the docker CLI, tmux, some test suites; and 32-bit programs. `--unix-sockets` leaves the filter out, and `hx`
  then prints a note at startup.
- The API key is read once and removed from the environment, together with every variable whose name ends in
  `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD` or `CREDENTIALS` (`MY_API_KEY`, `OPENAI_APIKEY`, `PGPASSWORD`),
  or in `_PAT` or `_PWD` (`GITHUB_PAT`, `MYSQL_PWD`; `PWD` and `OLDPWD` stay). `--keep-env NAME` keeps one. Luau
  code never sees the key: the host adds it to requests for the configured endpoint only, and commands cannot
  read it out of the host's memory. `DEEPSEEK_API_KEY` is kept the same way for DeepSeek's tool endpoints, and is
  sent only to `HX_TOOLS_URL/search`, `/fetch` and `/ocr`; `EXA_API_KEY` too, sent only to `HX_EXA_URL`.
- The VM has a memory ceiling of 1 GiB, and every process a command started is killed when `hx` exits, a
  daemon that called `setsid` included: `hx` is a child subreaper, so all of them stay its descendants.

### What the wall does not do

Know these before you rely on it.

- **With `--unix-sockets` on a kernel before Landlock ABI 9, or on another architecture than x86-64 or
  aarch64, unix sockets by path are reachable.** A confined command cannot list `/run/user/1000`, but it can
  connect to a socket there. That puts the D-Bus session bus and systemd's user socket (both can start programs
  outside the sandbox), `ssh-agent`, Wayland and a Docker socket within reach. `hx` prints a note at startup when
  this is so. To close it then, run `hx` as a separate Unix user that owns nothing but the project.
- **The network is open.** The model can send anywhere what it can read. The wall limits what it can read.
- **What the model writes, you may later run.** A changed `Makefile`, test, `.git/hooks` script or `.git/config`
  runs with your full access when you use it outside `hx`. Hooks and `.git/config` do not show in `git diff`,
  and a `core.fsmonitor` or pager set there runs on your next `git status`, the very command you would review
  with. When the task reads text you do not trust, run `hx` in a clone (`git clone project project-hx`) and
  `git pull` from it: commits cross over, hooks and configuration do not.
- **Secrets stored in the project are readable**, as are secrets in variables with unusual names.
- **A process outlives `hx` when `hx` itself is killed with SIGKILL, or crashes.** Every other way out kills
  what the model started, leaving at once on a second Ctrl-C included. Such a process stays confined, but it is
  not killed. A process stuck in uninterruptible I/O (a dead network mount) is sent SIGKILL but dies only when
  the I/O returns; `hx` waits a second for it, then leaves without it. A command that times out is treated the
  same way: `host.sh` returns after that second, with `signal` 9.
- **The model can edit `.hx/loop.luau` and its extensions.** That is the point, and it shows in `git diff`.
  If the loop no longer loads and returns a `main` function, the built-in one runs instead.
- **Nothing asks before a file in the project is changed or deleted.** Git is the undo.

## The loop

`lua/loop.luau` is the whole agent, in one file. It speaks the Anthropic Messages format, asks for adaptive
thinking, and sends thinking blocks back exactly as they came. It gives the model one tool, `lua`, which runs the
code it is sent in the project directory and returns what the code prints and returns. Globals persist from one
call to the next. The code acts through these functions:

| Function | Does |
|---|---|
| `bash(command, timeout?)` | runs a command and returns its output, then `[exit N]`; kills it after a timeout. The status line is for reading: output used as data comes from `host.sh(command).out` |
| `read(path, offset?, limit?)` | returns a range of lines of a file, numbered, and where to go on; `\r\n` line ends are shown as plain ones |
| `write(path, content)` | creates or overwrites a file |
| `edit(path, old, new, all?)` | replaces an exact piece of text that occurs once; in a file with `\r\n` line ends, text written with `\n` ends is found too, and what goes in gets `\r\n` ends |
| `skill(name)` | a skill's instructions, by name; for a name no skill has, the list of skills to pick from (see "Skills") |
| `ext(name)` | an extension's table of functions |

`host.*` and `json.*` are there too, for working on raw data. A function that fails raises, and the error goes
back to the model with what the code printed before it. Only the result is clipped, once: past `M.output_limit`
it keeps the start and the end, and the whole of it goes to a file in `$TMPDIR` whose path it names. A function
returns everything, so code can work on an output of any size.

Code from the model and from extensions sees `host`, `json` and the standard libraries through read-only views:
an assignment such as `host.sh = nil` raises "host is read-only" instead of breaking every later call, and the
text such code passes to `host.log` and `host.input` is made printable, so it cannot send an escape sequence to
the terminal. `_G` there is the code's own globals, and a chunk loaded with `host.load` and no environment gets
globals of its own. `for key, value in view` iterates a view; `pairs` and `next` see nothing in one. The views
stop mistakes, not code that digs with `rawset` or `debug` on purpose, and `host.extensions` and `host.args` are
still the tables themselves. The loop, a project's `.hx/loop.luau` and `--run` scripts use the real host.

`hx --init` writes `.hx/loop.luau` from `lua/overlay.luau`:

```lua
local M = host.load(host.default_loop, "=loop")()
M.max_replies = 100
table.insert(M.functions, { name = "check", usage = "check() -> text: runs the tests", run = function() ... end })
return M
```

Every setting at the top of the loop is a field of `M`, and so are the parts a project may want to replace:
`M.functions`, `M.tools` (the tools the model is offered: `lua` alone, unless a project adds one the model should
call directly rather than from code), `M.skill_dirs`, `M.system_prompt`, `M.layout`, `M.request` (one request to
the model), `M.turn` (replies and tool calls until the model is done), `M.compact`, `M.compact_prompt` and
`M.resume_prompt`. The loop always calls them through `M`, so a replacement takes effect everywhere. `M.model`
starts as `host.model`, the model `--model` or `HX_MODEL` chose with its alias resolved.

The system prompt shows the project's layout, so the model's first call need not be a listing: the files git
lists, tracked or new and not ignored, as a tree of at most `M.layout_lines` (60) lines, directories first. This
repository fits whole; in 15 lines it is:

```
lua/
  ext/ (4 files)
  judge.luau
  loop.luau
  overlay.luau
src/ (18 files)
tests/ (9 files)
.clang-format
.clang-tidy
.gitignore
.luaurc
CMakeLists.txt
CMakePresets.json
LICENSE
README.md
```

Directories are opened level by level, each one whose entries still fit, and one that stays closed is a line
with the number of files below it. When the top level alone does not fit, the tree is its first entries and how
many more there are. Where git lists no files (no repository, or one whose `.git` is above the project directory
and so outside the wall), the tree is the project directory's own entries, one level. It is made once per run,
like the list of skills, so it does not upset the prompt cache, and it says that it shows the start of the
session. It needs nothing but git: no tree command has to be installed. `M.layout_lines = 0` leaves it out,
`M.tree(paths, limit)` draws any list of paths the same way, and a project that wants another picture replaces
`M.layout`.

A project function may run a `lua` call inside the running one (`M.tools[1].run`), or a whole `M.turn` on another
`Conversation`, as a sub-agent. The nested call keeps the outer call's printed lines, and the nested turn counts
repeats on its own and leaves the outer turn's counts alone. The model's globals are shared by both.

hx does not bring a search tool: the model searches through `bash`. Where `rg` is on the `PATH`, the system
prompt says that ripgrep is installed and to use it rather than plain grep, since it is faster and skips what
`.gitignore` names; where it is not, the prompt does not name it.

Some habits of the model are handled in the loop rather than left to the prompt: a function call that gets the same
result for the 3rd, 5th and 8th time in a task, whatever ran in between, gets a reminder to stop and report, and to
try something else only when it knows an action that will change the result (results are compared by length and a
hash, so a long one is not kept); a reply with nothing in it is retried like a busy server; and a task that runs out of replies gets one more, to report where it stands.
For notes to the model that need no code, write `.hx/prompt.md`.

### Claude models

`M.claude` lists what the loop adds for the Claude models it knows; any other model, DeepSeek's included, gets the
plain request. Any `claude-*` model, listed or not, gets the system prompt as a block with `cache_control`; only a
listed one gets the features below. The settings follow Anthropic's guidance for these models:

| | Opus 5.5 | Sonnet 5.5 | Haiku 5.5 |
|---|---|---|---|
| `output_config.effort`, unless `HX_EFFORT` is set | `medium` | `medium` | `medium` |
| `fallbacks: "default"`: a declined request runs again on a model picked for why it was declined, in the same round trip | yes | yes | no (Haiku has none) |
| `thinking.display: "updates"`: notes between tool calls come back and show as progress lines | yes | yes | no |
| `thinking.block_binding: drop_block`: a thinking block sent back after a changed history is dropped rather than failing the request | yes | yes | yes |
| `compaction: {type: "summarize"}`: past `M.compact_at` prompt tokens, the API writes the summary (on-demand compaction) | yes | yes | yes |

The betas these need go in the `anthropic-beta` header, which Luau sets with `host.llm(body, {beta = "a,b"})`. Thinking
is adaptive and cannot be turned off on these models; effort is the control. A reply a fallback model finished
carries a `fallback` block; the loop drops what the declining model wrote before it, as the API requires, and a
refusal is logged with its category.

Compaction on these models is the API's on-demand kind: the loop sends the conversation as it stands, with the same
system prompt, tools, thinking and effort, plus `compaction` with `M.compact_prompt` as its `instructions`. The
signed `compaction` block that comes back replaces every message it summarized; it goes first in the next request,
exactly as it came, followed by `M.resume_prompt`. If no block comes back, the conversation is kept and compacted
after a later reply. Any other model, DeepSeek's included, is asked for the summary as an ordinary reply.

Prompt caching follows each provider's docs. Every model gets a prefix that only grows: the system prompt is built
once per run, the tool description never changes, JSON keys are sorted, and the history is only appended to. A
Claude model gets two cache breakpoints: one on the system prompt, so tools and system are still read from the
cache after a compaction, and the top-level automatic one, which moves forward with the conversation. DeepSeek
caches every prefix by itself and ignores `cache_control`. The line at the end of a run gives the tokens read
from the cache and written to it.

When a `.hx` directory exists, each session's conversation is saved to `.hx/log/` as JSON, after every reply that
called tools and its results as well as at the end of a turn, so a run killed half-way leaves its log too. Whether
the directory exists is looked at with each save, so a session that makes `.hx` itself keeps its log. The logs
hold file contents, so that directory carries a `.gitignore` that keeps all of it out of git.

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

## The judge

An agent asked to make a number go up will sometimes make it only look higher: it skips the slow case, moves the
timer, or just says it won. So with `--judge KIT` the model only proposes. A judge that it can neither read nor
change measures each change against the best version so far, the **champion**, and keeps only what it confirms.
The idea is [Thermite](https://github.com/iammrduncan/thermite)'s, cut down to one machine and one process.

```
hx --judge ~/kits/primes "make 'python3 primes.py N' fast for large N, keeping every answer right"
```

A **kit** is the judge's half of the project, a directory with up to four files:

| File | Does |
|---|---|
| `run` | required, executable. Runs in a fresh copy of the built candidate and is timed by hx from outside; it exits 0 when it worked |
| `build` | optional, executable. Runs once per candidate before any run, untimed, at the same path the runs use, so a build that records where it is still runs |
| `check` | optional, executable. Runs as `check DIR` after each run, where `DIR/.judge/stdout` is what the run printed; links in `DIR` are removed first, so the run cannot point the check at the kit. It exits 0 when the output is right. A number on its last line is the score: positive, and higher is better; without one, the measure is the run's time. What it prints when it fails goes to the model, unless `blind` is on |
| `kit.json` | optional: `{"runs": 5, "margin": 0.02, "timeout": 600, "blind": false}`: pairs per round (1 suits a score that does not vary), the smallest relative gain that counts, the seconds one build, run or check may take, and whether the model hears verdicts without numbers and without the candidate's or the check's words, so that it cannot fit what the kit holds back by reading them. An unknown key or a wrong type stops hx before the model is asked anything |

The model gets one more function, `submit(note?)`, and a paragraph in its prompt that says how judging works.
Before the model begins, the judge copies the project and measures it as it is. That is the first champion; if it
does not pass, the first submission that does is. Each submission then goes through the same steps:

1. **Copied.** The judge copies the project as it stands, without `.git` and `.hx`, and writes its patch.
2. **Built**, by the kit's `build`, if there is one. A build that fails is `broken`.
3. **Screened.** The candidate and the champion run in pairs, each pair in a random order, and each run in a fresh
   copy of its build. A run or check that fails is `invalid`. The change is better when the median pair favours it
   by more than the margin, and all pairs but one favour it at all. Otherwise it is `no different` or `worse`.
4. **Confirmed.** A change that looks better is measured again in fresh pairs and judged on those alone: `win`, or
   `not confirmed`.

A trial the judge cannot finish through no fault of the submission, such as a champion that stops passing, is an
`error`; one that the end of the session cuts short is `stopped`. The judge takes one submission at a time. One
can take longer than a `lua` call allows (`M.call_seconds`, 600): `submit` then says the judge is still measuring,
and `submit()`, with no note, waits for the verdict; a new note is refused until it comes. When the session ends, hx prints a summary with the champion's
measure and the start's. The judge's state directory, `$XDG_STATE_HOME/hx/judge/PROJECT-TIME-XXXXXX`
(`~/.local/state` by default), keeps `ledger.jsonl` (every trial: its note, verdict, and each pair's measures),
`champion.patch` and `trials/N.patch` for every trial, whatever its verdict. A patch is against the start, from
git, so it carries binary files, links and modes: `git apply` takes it in the project.

The walls are what make the verdicts worth having:

- **The judge is its own process.** hx forks it before the agent's wall goes up. It confines itself to reading the
  project and the kit and writing its state directory; the agent's wall leaves out the kit and the state
  directory, and hx refuses a kit or state directory inside a path the agent can read. It does not stop you
  handing the agent part of the kit with `--ro`. The two talk over
  one socket, a line each way, through `host.judge`. The agent sends only a note; the judge copies the project
  itself.
- **The candidate cannot read the kit's other files.** `build` and `run` run behind a narrower wall still: their
  copy of the candidate, the system directories and the `--ro` and `--rw` paths. Only the script being run is
  copied in, so `check` and whatever else the kit holds stay out of reach. `check` reads the kit and the run's copy
  and writes nothing that lasts.
- **A candidate leaves nothing running.** What a build or run started dies when it ends, so no process of the
  candidate's can slow the champion's runs or write into an output after the run.
- **The judge is built into hx** (`lua/judge.luau`). A project's loop can change everything about the agent, but
  not the judge.

Limits: the candidate's code can read `run` and what it reads, so keep answers in `check`. Candidates are not cut
off from the network or from SysV IPC, so a helper the agent leaves running can carry results between runs, and
the agent can lower the priority of the judge's runs. A check that accepts a wrong answer lets it win, so try a
kit on the unchanged project and on a known-bad change before trusting it.

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

The model writes extensions with `write` and calls them from its code as `ext("NAME").count("src")`. An
extension can call the same functions the model's code can, and what it prints goes into the tool result with what
that code prints. A changed file is loaded again on its next use. Each
session starts with a list of the project's extensions in the system prompt. The tool itself never changes, so
prompt caching keeps working. Commit `.hx/ext/` and the harness grows with the project.

An extension whose name starts with `_` is a helper for other extensions: `ext()` loads it like any other, and
the system prompt does not list it.

### Built in: web, ocr and exa

Three extensions come with hx, in `lua/ext/`. Two of them, `web` and `ocr`, call DeepSeek's [Search, Fetch](https://api-docs.deepseek.com/guides/search)
and [OCR](https://api-docs.deepseek.com/guides/ocr) APIs with `DEEPSEEK_API_KEY`, whichever model the loop talks
to, so a session on Claude can use them too when that key is set. The system prompt lists them only then. A
project's own `.hx/ext/web.luau` or `.hx/ext/ocr.luau` takes the place of the built-in one.

| Call | Returns |
|---|---|
| `ext("web").search(query, {count?, content?, question?})` | results, best first: `{title, url, snippet, summary, content?, site_name?, date?, rerank_score, authority}`. Printed or returned from `lua`, they read as a numbered list. `query` takes `site:`, `-site:`, `lang:`, `after:`, `before:` and `"phrases"`, up to 128 characters; `count` is 1 to 50 (default 10); `content = true` asks for each page's text |
| `ext("web").fetch(url)` | the page as Markdown, its title first as a heading when the page does not show it |
| `ext("ocr").read(path, purpose?)` | the text of a PDF, Word, PowerPoint or image file, up to 100 MiB and 1,000 pages. A PDF is read with `rich_layout_recognition`, which keeps tables and layout and leaves box tokens in the text; anything else with `text_recognition`. A document of several pages has a `[page N]` line before each |

Requests that fail with 408, 429 or 5xx, or get no answer, are tried three times, Exa's too. The retry loop, the
reading of error bodies and the argument checks are written once, in the helper `_http`, which all three load; a
project's own `.hx/ext/_http.luau` takes its place for all of them. The file for OCR is read by the
host inside the wall, so a file the model cannot read cannot be sent. Search and Fetch cost $0.75 per 1,000
requests, OCR $0.225 per 1,000 pages.

A third, `exa`, calls Exa's [Deep Search](https://exa.ai/docs/search/deep-search) with `EXA_API_KEY`, and is listed
only when that key is set. Deep Search plans several searches, reads what they find, searches again where the
evidence is thin, and writes a grounded answer. It is for questions that take research; a single lookup is quicker
and cheaper through `web`.

| Call | Returns |
|---|---|
| `ext("exa").deep(query, objective, {queries?, schema?, count?})` | `{answer, grounding, results, notices, cost}`: `answer` is text, or a table when `schema` is given; `grounding` lists the citations behind each field; `results` are the sources, best first, `{title, url, date?, author?, highlights}`. Printed or returned from `lua`, it reads as the answer and a numbered list of sources. `objective` says which documents should rank first, which to leave out and what facts to pull; `queries` is up to 10 further starting searches; `schema` is an Exa output schema of type `"object"` (at most 10 properties, 2 levels deep); `count` is 1 to 100 sources (default 10). The search type is always `deep` |

Every request asks for highlights, Exa's excerpts of each page that answer the query, not the whole page. Exa's
notices go to the model with the answer; its warnings go to stderr. Deep Search costs $12 per 1,000 requests up to
10 results, and takes 4 to 15 seconds.

## Skills

A skill is instructions for one kind of task, in the [Agent Skills](https://agentskills.io) format: a directory
with a `SKILL.md` whose front matter gives a `name` and a `description` of when to use it, plus whatever scripts
and references the text points to. Extensions are code the model calls; skills are text the model reads.

```
---
name: release-notes
description: Write release notes from the git log. Use when asked for release notes or a changelog entry.
---
Group commits by area, newest first. Run scripts/authors.sh for the credits line.
```

hx looks in `.hx/skills/`, `.agents/skills/` and `.claude/skills/` in the project, then `~/.agents/skills/` and
`~/.claude/skills/`, and the first skill of a name wins. The two in your home directory are outside the wall, so
they are found only when you open them: `hx --ro ~/.claude/skills "..."`. A skill with
`disable-model-invocation: true` is left out of the prompt and of the list, but still loads by its name. Other
fields, such as `allowed-tools`, `model` and `hooks`, are ignored: there is one tool, and the wall is the same for
everything.

The system prompt lists each skill with its description, up to `M.skill_index_full` (30) of them, and past that
their names only. The list is made once per run, so it does not upset the prompt cache. The model loads a skill
from its code:

| Call | Returns |
|---|---|
| `skill("release-notes")` | the text after the front matter, headed by the skill's directory and its other files by their paths from the project (up to `M.skill_files_shown`, 30), so a script the skill names can be run as it is listed; the work itself stays in the project directory. Case, `-`, `_`, `.` and spaces in the name do not matter, so `skill("Release_Notes")` is the same call; when two skills differ only in those, both are listed with their paths instead |
| `skill("changelog")` | when no skill has that name: the skills as `name: description` lines, up to `M.skill_listed` (50) and then how many more, for the model to pick one. There is no fuzzy search: the model reads descriptions better than a score would |

Scripts in a skill run through `bash`, inside the wall like any other command. A skill in the project can be
written by the model, and a skill in a repository you cloned is text from whoever wrote it, like its README.

## Host primitives

Everything Luau can do to the machine:

| Primitive | Returns |
|---|---|
| `host.llm(body, {timeout=600, beta=})` | `status, text`, or `nil, error`; `beta` is sent as the `anthropic-beta` header. A third value is the seconds a `Retry-After` header asked for, when the reply had one; `host.tool` returns it too |
| `host.tool(name, request, {timeout=300})` | `status, text`, or `nil, error`: a POST to DeepSeek's `search`, `fetch` or `ocr` endpoint with `DEEPSEEK_API_KEY`, or, for `"exa"`, to Exa's search endpoint with `EXA_API_KEY`. A string `request` is sent as JSON; for DeepSeek, a table is sent as a form, whose `file` field is the path of a file to upload |
| `host.sh(command, {timeout=120, read=, write=, hangup=})` | `{out, code, signal, timed_out, seconds}`; `seconds` runs from the fork to the exit. Given `read` or `write`, lists of paths, the command runs behind a narrower wall: the system directories and those paths, inside the wall already in force, and whatever it starts is killed when it ends. With `hangup = true`, the command ends, or does not start, once stdin's writer has hung up, and `host.sh` raises `"hung up"` |
| `host.judge(line?)` | `{lines}, pending`: sends a line to the judge, if given, and returns the answer it owes, waiting until 2 seconds before the `host.timed` limit in force; `pending` is how many it still owes. A line is refused while one is owed |
| `host.read(path)` | the contents, or `nil, error`. A FIFO or a device is read without blocking: waiting for a writer, or for more, ends with the `host.timed` limit in force (`nil, "PATH: timed out"`) or a stop |
| `host.write(path, data)` | `true`, or `nil, error`; creates parent directories. A regular file is replaced by renaming a new one over it, so a failed write leaves the old content (a killed one can leave the new file beside it); the mode stays and a link stays a link, but owner, extended attributes and ACLs are not carried over. A device, a FIFO, a file with other hard links, a dangling link, or a file in a directory that refuses a new file is written in place; waiting for a FIFO's reader, or for room, ends as `host.read`'s waits do |
| `host.list(dir)` | sorted names, directories ending in `/`, or `nil, error` |
| `host.clip(text, limit)` | `text` when it is at most `limit` bytes; otherwise `head, tail`, at most `limit // 2` bytes from each end. Each is cut at a line when that keeps more than half of it, and otherwise never inside a UTF-8 character; `host.sh` cuts a long output the same way |
| `host.load(source, name, env)` | a function, or `nil, error` |
| `host.try(fn, ...)` | `true, ...`, or `false, error`: a `pcall` that is also where a stop ends |
| `host.timed(seconds, fn, ...)` | as `host.try`; the error is `"timeout"` when the limit passed |
| `host.input(prompt)` | a line from stdin, or `nil` at the end |
| `host.log(text)`, `host.sleep(seconds)`, `host.env(name)` | |
| `host.args`, `host.cwd`, `host.interactive`, `host.sandboxed`, `host.default_loop` | |
| `host.quiet` | `true` with `--quiet`: leave out progress lines |
| `host.model` | the model name `host.llm`'s requests are meant for, after `--model`, `HX_MODEL` and the aliases |
| `host.tty` | `true` when stdout is a terminal |
| `host.tools` | `true` when hx has a `DEEPSEEK_API_KEY` for `host.tool` |
| `host.exa` | `true` when hx has an `EXA_API_KEY` for `host.tool("exa")` |
| `host.judged` | `true` when there is a judge for `host.judge` |
| `host.extensions` | the built-in extensions' sources, by name |
| `json.encode(value)`, `json.decode(text)`, `json.array(t)`, `json.null` | |

Two rules hold across all of them:

- **A stop cannot be swallowed.** Ctrl-C raises `"interrupted"` from whichever call it cut short, and again at
  every later step of Luau code and every blocking call, until the outermost `host.try` or `host.timed` returns
  `false, "interrupted"`. That is the only place a stop ends, so a plain `pcall` cannot make it go away: outside
  any `host.try` it ends the run. SIGTERM and SIGHUP raise `"terminated"` the same way.
- **A time limit reaches blocking calls.** Inside `host.timed`, `llm`, `sh` and `sleep` give up when the limit
  passes, and so do `read` and `write` waiting on a FIFO or a device; limits nest.

`json.encode` writes object keys in sorted order, so equal values always produce the same bytes.

## Source

| File | Holds |
|---|---|
| `src/main.cpp` | arguments, secrets, the sandbox rules, starting the VM |
| `src/judge.cpp` | the judge's process for `--judge`: checking the kit, forking, its wall, hanging up at the end |
| `src/sandbox.cpp` | Landlock; `Sandbox` is the proof that confinement was decided, and a `Host` cannot be built without it |
| `src/host.cpp` | the `host` table and the VM: stops and time limits at safepoints, `host.try`, `host.timed`, `host.load` and the small primitives |
| `src/host_proc.cpp` | signals, waiting until one arrives, and killing every process hx started |
| `src/host_sh.cpp` | `host.sh`: forking the command behind its wall, its limits, and both ends of its output |
| `src/host_http.cpp` | `host.llm` and `host.tool`: requests to the endpoints, each key to its own URLs only |
| `src/host_files.cpp` | `host.read`, `host.write` and `host.list` |
| `src/host_judge.cpp` | `host.judge`: the agent's end of the channel to the judge |
| `src/host_internal.h`, `src/sys.h` | what the `host*.cpp` files share, and the small system helpers every file uses |
| `src/json.cpp` | JSON between Lua values and text, on RapidJSON's streaming reader and writer |
| `lua/loop.luau` | the agent |
| `lua/judge.luau` | the judge, which `--judge` runs in a process of its own |
| `lua/ext/*.luau` | the built-in extensions, one per file; each file in `lua/ext/` is built in under its name |

Development build, with AddressSanitizer, UBSan and warnings as errors:

```
cmake --preset dev && cmake --build --preset dev && ctest --preset dev
clang-format --dry-run --Werror src/*.cpp src/*.h
clang-tidy -p build-dev --quiet src/*.cpp
luau-analyze lua/*.luau lua/ext/*.luau tests/*.luau     # prints nothing when all is well; nor with --solver=old
```

The Luau tests are `tests/*_test.luau`, and share their checks in `tests/check.luau`. One runs by hand from a
scratch directory, with absolute paths, as `hx -q --ro /path/to/tests --run /path/to/tests/tools_test.luau`: the
`--ro` lets the test read `check.luau` through the wall.

The Luau files in `lua/` are `--!strict`; an extension carries that on its second line, because its first is the
description the system prompt shows. `.luaurc` names the globals the host provides. When `luau-analyze` is
installed, `ctest` runs it over every Luau file as the test `analyze`, and anything it prints fails the test.

`tests/live_test.py` runs the agent against the real endpoint: making and editing files, reading a line range,
noticing a failed command, following a long output to its file, compacting a Vietnamese conversation into English
checkpoints that keep a fact, giving up a status polled between sleeps, reporting when out of replies, and
following a skill: one listed in the prompt, one listed by name only, one found by searching, and one in the home
directory that only `--ro` makes reachable. It costs tokens, so ctest
leaves it out: `python3 tests/live_test.py build/hx` with `DEEPSEEK_API_KEY` set, or with `ANTHROPIC_API_KEY` and `HX_MODEL=sonnet`
(or `opus`, `haiku`).

## Limits

- One model endpoint per run, which must accept the Anthropic Messages format and an `x-api-key` header: the Claude
  API for a `claude-*` model, DeepSeek's otherwise. The model is fixed at start: a project loop that changes
  `M.model` to a Claude model still talks to the endpoint `--model` or `HX_MODEL` chose. The Claude settings have
  been checked against a stand-in server, and `tests/live_test.py` passes against the live API on Sonnet 5.5; Opus
  and Haiku have not been run live.
- No streaming: a reply appears when it is complete.
- Tool calls in one reply run one after another.
- The whole conversation is encoded again for every request. At 5 MB that takes 16 ms.
- Linux only.
- Numbers are doubles, so integers above 2^53 in JSON lose precision.
