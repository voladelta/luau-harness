# Host primitives

Everything Luau can do to the machine:

| Primitive | Returns |
|---|---|
| `host.llm(body, {timeout=600, beta=, endpoint=})` | `status, text`, or `nil, error`; `beta` is sent as the `anthropic-beta` header. A third value is the seconds a `Retry-After` header asked for, when the reply had one; `host.tool` returns it too. The request goes to the session's endpoint, or with `endpoint = "deepseek"` to DeepSeek's Messages endpoint with `DEEPSEEK_API_KEY`; no other name is taken |
| `host.tool(name, request, {timeout=300})` | `status, text`, or `nil, error`: a POST to DeepSeek's `search`, `fetch` or `ocr` endpoint with `DEEPSEEK_API_KEY`, or, for `"exa"`, to Exa's search endpoint with `EXA_API_KEY`. A string `request` is sent as JSON; for DeepSeek, a table is sent as a form, whose `file` field is the path of a file to upload. A file that cannot be sent, one that is missing, outside the wall, not a regular file or larger than 100 MiB, raises, so that a caller's retry loop does not take it for a request that got no answer |
| `host.sh(command, {timeout=120, input=, cwd=, env=, read=, write=, hangup=})` | `{out, code, signal, timed_out, seconds, omitted}`; `seconds` runs from the fork to the exit. `command` is a bash command, or a list of a program and its arguments, which are quoted for bash so that each reaches the program as it is; a number in the list is taken as its text. `input` is text written to the command's stdin, which is otherwise `/dev/null`; a command that stops reading it early is not an error. `cwd` is the directory the command runs in, and one that is missing or not a directory raises. `env` is a table of variables to set for the command, by name: a value is text or a number, and `false` takes the variable away; the rest of hx's environment stays. `out` is stdout and stderr together, all of it up to 16 MiB, and a larger output belongs in a file that `host.read` reads; past that the middle is cut, as `host.clip` cuts, a line in its place says how many bytes are gone, and `omitted` is that number. Given `read` or `write`, lists of paths, the command runs behind a narrower wall: the system directories and those paths, inside the wall already in force, and whatever it starts is killed when it ends. With `hangup = true`, the command ends, or does not start, once stdin's writer has hung up, and `host.sh` raises `"hung up"` |
| `host.spawn(fn, ...)` | an id, counting from 1: forks hx. The child is the process as it is, VM and wall and all, and runs `fn(...)` while the parent goes on. What it changes in the VM is its own: only files, and the string `fn` returns, reach the parent. The child is under no time limit of the parent's (its own `host.timed` works), reads no stdin, has no judge and cannot spawn. A stop ends it, and so does the parent's exit; what its commands left running is killed when `fn` ends. At most 64 children run or wait to be waited for |
| `host.wait(seconds?)` | `id, ok, text` for the first child that has ended, in the order they were spawned: `ok` is whether `fn` returned, `text` what it returned (a string, or nothing for `""`) or the error it raised. A child that was killed is `false` and says so. When none has ended it waits, for at most `seconds` and until 2 seconds before the `host.timed` limit in force, and then, or when there is no child, returns `nil, running`: how many are still at work |
| `host.judge(line?)` | `{lines}, pending`: sends a line to the judge, if given, and returns the answer it owes, waiting until 2 seconds before the `host.timed` limit in force; `pending` is how many it still owes. A line is refused while one is owed |
| `host.read(path)` | the contents, or `nil, error`. A FIFO or a device is read without blocking: waiting for a writer, or for more, ends with the `host.timed` limit in force (`nil, "PATH: timed out"`) or a stop |
| `host.write(path, data)` | `true`, or `nil, error`; creates parent directories. A regular file is replaced by renaming a new one over it, so a failed write leaves the old content (a killed one can leave the new file beside it); the mode stays and a link stays a link, but owner, extended attributes and ACLs are not carried over. A device, a FIFO, a file with other hard links, a dangling link, or a file in a directory that refuses a new file is written in place; waiting for a FIFO's reader, or for room, ends as `host.read`'s waits do |
| `host.list(dir)` | sorted names, directories ending in `/`, or `nil, error` |
| `host.walk(dir, {glob=, depth=})` | every entry under `dir`, as sorted paths relative to it, directories ending in `/`, or `nil, error`. A `.git` directory is left out, links are not followed, and `.gitignore` is not read (`rg --files` honours it). `depth` 1 is `dir`'s own entries. A `glob` with a `/` is matched against the whole path, where `*` and `?` stay inside a directory and `**` is any number of them, more than none at the end; one without a `/` is matched against the name, at any depth. More than 100,000 entries is an error |
| `host.stat(path)` | `{type, size, mtime, mode, target}`, or `nil, error`: `type` is `"file"`, `"dir"`, `"link"`, `"fifo"`, `"socket"` or `"device"`, of the path itself, as a link is not followed; `mtime` is in seconds since the epoch; `mode` is the permission bits as octal text, `"644"`; `target` is where a link points |
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
| `host.workflow` | the skill the session is led by: `"rpi"` with `--rpi`, otherwise `nil` |
| `host.extensions` | the built-in extensions' sources, by name |
| `host.skills` | the built-in skills' `SKILL.md` texts, by name |
| `json.encode(value, {indent=})`, `json.decode(text)`, `json.array(t)`, `json.null` | |

Two rules hold across all of them:

- **A stop cannot be swallowed.** Ctrl-C raises `"interrupted"` from whichever call it cut short, and again at
  every later step of Luau code and every blocking call, until the outermost `host.try` or `host.timed` returns
  `false, "interrupted"`. That is the only place a stop ends, so a plain `pcall` cannot make it go away: outside
  any `host.try` it ends the run. SIGTERM and SIGHUP raise `"terminated"` the same way.
- **A time limit reaches blocking calls.** Inside `host.timed`, `llm`, `sh` and `sleep` give up when the limit
  passes, and so do `read` and `write` waiting on a FIFO or a device; limits nest. `judge` and `wait` return two
  seconds before it, with what they have.

`json.encode` writes object keys in sorted order, so equal values always produce the same bytes. With `indent`,
from 1 to 16 spaces, it writes one member per line, as Python's `json.dumps(value, indent=N, sort_keys=True)` does.
