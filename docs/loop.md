# The loop

`lua/loop.luau` is the whole agent, in one file. It speaks the Anthropic Messages format and sends thinking blocks
back exactly as they came. Thinking is always on and is not a setting of hx's: `HX_EFFORT` is the one control.
Every `claude-*` request carries `thinking: {type: "adaptive"}`; DeepSeek's models think by default and get no
thinking field. Claude requests use the latest API version, `anthropic-version: 2026-10-21`; DeepSeek's endpoint
gets `2023-06-01`. It gives the model one tool, `lua`, which runs the
code it is sent in the project directory and returns what the code prints and returns. Globals persist from one
call to the next. The code acts through these functions:

| Function | Does |
|---|---|
| `bash(command, timeout?)` | runs a command and returns its output, then `[exit N]`; kills it after a timeout. The status line is for reading: output used as data comes from `host.sh`, whose line in the tool's description offers `split = true` to keep stderr out of it and asks for it to be parsed in Luau rather than in a python or jq pipeline |
| `read(path, offset?, limit?)` | returns a range of lines of a file, numbered, and where to go on; `\r\n` line ends are shown as plain ones |
| `write(path, content)` | creates or overwrites a file. It refuses one that changed since the conversation last read or wrote it, and says to read it again (see [Agents](agents.md)) |
| `edit(path, old, new, all?)` | replaces an exact piece of text that occurs once; in a file with `\r\n` line ends, text written with `\n` ends is found too, and what goes in gets `\r\n` ends |
| `see(path)` | puts a PNG, JPEG, GIF or WebP file into the call's result as an image block, after the text, for a model that takes images: at most `M.image_bytes` (5 MiB) each and `M.images_per_call` (8) a call. `M.vision = false` turns it away for a model that takes none |
| `skill(name?)` | a skill's instructions, by name; with no name, or a name no skill has, the list of skills to pick from (see [Skills](skills.md)) |
| `ext(name)` | an extension's table of functions |
| `agents(tasks)` | gives each task to an agent, all at work at the same time, and returns their reports (see [Agents](agents.md)) |

`host.*` and `json.*` are there too, for working on raw data. The tool's description asks the model to do a
task's logic and data in Luau, to run programs for what programs do, joining them through `host.sh`'s list form
and `input`, and to write a script in another language only for a library neither Luau nor the system's tools
have. The system prompt asks the model, when the wall denies a program the task needs, to name the path for
`--ro`. It asks it to do the task as its instructions say and, when the task is unclear or would take a step
it did not ask for, to stop and end its reply with a question; never to sleep or poll for something outside its
control, such as a market or a deploy, but to report and ask whether to wait.

A function that fails raises, and the error goes back to the model with what the code printed before it. Only
the result is clipped, once: past `M.output_limit` it keeps the start and the end, and the whole of it goes to a
file in `$TMPDIR` whose path it names. A function returns everything, so code can work on an output of any size.

Code from the model and from extensions sees `host`, `json` and the standard libraries through read-only views:
an assignment such as `host.sh = nil` raises "host is read-only" instead of breaking every later call, and the
text such code passes to `host.log` and `host.input` is made printable, so it cannot send an escape sequence to
the terminal. `_G` there is the code's own globals, and a chunk loaded with `host.load` and no environment gets
globals of its own. `for key, value in view` iterates a view; `pairs` and `next` see nothing in one. Digging does
not get past them: a view is frozen, so `rawset` and `setmetatable` raise on it; the standard libraries, `json`,
the metatable strings share, and `host.extensions`, `host.skills`, `host.args` and `host.endpoints` are read-only
tables for everyone; and such code has no `getfenv` or `setfenv`, which would hand it the loop's globals. The
loop, a project's `.hx/loop.luau` and `--run` scripts use the real host, whose own fields they can replace.

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
starts as `host.model`, the model `--model` or `HX_MODEL` chose with its alias resolved. A conversation made with
`M.Conversation.new(model)` is asked on that model instead, each request through the endpoint `host.route` gives
for it; `M.request(system, messages, compaction, model)` takes the model as its last argument, and a replacement
has to pass it on.

The system prompt shows the project's layout, so the model's first call need not be a listing: the files git
lists, tracked or new and not ignored, as a tree of at most `M.layout_lines` (60) lines, directories first. With
`M.layout_lines = 20`, this repository is:

```
.claude/
  skills/
    hx/
      SKILL.md
.hx/
  skills/
    duckdb/ (13 files)
docs/ (9 files)
lua/ (12 files)
src/ (19 files)
tests/ (16 files)
.clang-format
.clang-tidy
.gitignore
.luaurc
AGENTS.md
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
repeats on its own and leaves the outer turn's counts alone. The model's globals are shared by both. Such a
sub-agent runs inside the session's process, while the session waits for it; the `agents` function runs several
at once, each in a process of its own (see [Agents](agents.md)).

hx does not bring a search tool: the model searches through `bash`. Where `rg` is on the `PATH`, the system
prompt says that ripgrep is installed and to use it rather than plain grep, since it is faster and skips what
`.gitignore` names; where it is not, the prompt does not name it.

Some habits of the model are handled in the loop rather than left to the prompt: a function call that gets the same
result for the 3rd, 5th and 8th time in a task, whatever ran in between, gets a reminder to stop and report, and to
try something else only when it knows an action that will change the result (results are compared by length and a
hash, so a long one is not kept); a reply with nothing in it is retried like a busy server; a function called by
its name as if it were a tool gets an error that says it is a function and how code sent to `lua` calls it; a
task that runs out of replies gets one more, to report where it stands; and a model that answers while its agents
are still at work is told once that its answer would end them half way.
For notes to the model that need no code, write `.hx/prompt.md`.

## Claude models

`M.claude` lists what the loop adds for the Claude models it knows, which are the 5.5 family: hx is not for the
models before it. Any other model, DeepSeek's included, gets the plain request. A `claude-*` model newer than the
list gets adaptive thinking and the system prompt as a block with `cache_control`, and nothing else, until it is
added; only a listed one gets the features below. The settings follow Anthropic's guidance for these models:

| | Opus 5.5 | Sonnet 5.5 | Haiku 5.5 |
|---|---|---|---|
| `output_config.effort`, unless `HX_EFFORT` is set | `medium` | `medium` | `medium` |
| `fallbacks: "default"`: a declined request runs again on a model picked for why it was declined, in the same round trip | yes | yes | no (Haiku has none) |
| `thinking.display: "updates"`: notes between tool calls come back and show as progress lines | yes | yes | no |
| `thinking.block_binding: drop_block`: a thinking block sent back after a changed history is dropped rather than failing the request | yes | yes | yes |
| `compaction: {type: "summarize"}`: past `M.compact_at` prompt tokens, the API writes the summary (on-demand compaction) | yes | yes | yes |

The betas these need go in the `anthropic-beta` header, which Luau sets with `host.post(endpoint, body, {beta = "a,b"})`. Thinking
is adaptive and cannot be turned off on these models; effort is the control. A reply a fallback model finished
carries a `fallback` block; the loop drops what the declining model wrote before it, as the API requires, and a
refusal is logged with its category.

Compaction on these models is the API's on-demand kind: the loop sends the conversation as it stands, with the same
system prompt, tools, thinking and effort, plus `compaction` with `M.compact_prompt` as its `instructions`. The
signed `compaction` block that comes back replaces every message it summarized; it goes first in the next request,
exactly as it came, followed by `M.resume_prompt` and, when the model's code has set globals, their names
(`M.resume_text`): they outlast the compaction, and the checkpoint may not say so. If no block comes back, the conversation is kept and compacted
after a later reply. Any other model, DeepSeek's included, is asked for the summary as an ordinary reply.

Prompt caching follows each provider's docs. Every model gets a prefix that only grows: the system prompt is built
once per run, the tool description never changes, JSON keys are sorted, and the history is only appended to. A
Claude model gets two cache breakpoints: one on the system prompt, so tools and system are still read from the
cache after a compaction, and the top-level automatic one, which moves forward with the conversation. DeepSeek
caches every prefix by itself and ignores `cache_control`. The line at the end of a run gives the replies, the
tokens in, those read from the cache and written to it, and the tokens out. A run that used several models, a
sub-agent on another say, gets such a line for each, headed by the model's name: their prices differ, so one sum
would not say what the run cost.

When a `.hx` directory exists, each session's conversation is saved to `.hx/log/` as JSON, after every reply that
called tools and its results as well as at the end of a turn, so a run killed half-way leaves its log too. Whether
the directory exists is looked at with each save, so a session that makes `.hx` itself keeps its log. A
sub-agent's conversation, one whose turn runs inside another turn, is saved the same way beside the session's,
under the session's name with `-sub-1`, `-sub-2` and so on; so is each agent's, in the order the agents were
first given a task. The logs hold file contents, so that directory
carries a `.gitignore` that keeps all of it out of git.

## The conversation is a state machine

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
