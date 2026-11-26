# Development

Where each part of hx lives:

| File | Holds |
|---|---|
| `src/main.cpp` | process startup, private `$TMPDIR`, wiring the judge, sandbox and VM together |
| `src/cli.cpp` | CLI options, `--init`, model/endpoint resolution and scrubbing secrets from the environment |
| `src/policy.cpp` | path trust and Landlock rule construction for the agent and the judge (`WallPolicy`) |
| `src/vm.cpp` | Luau VM lifecycle, `/proc/meminfo` memory budgeting, library freezing and chunk execution |
| `src/judge.cpp` | the judge's process for `--judge`: checking the kit, forking, its wall, hanging up at the end |
| `src/sandbox.cpp` | Landlock; `Sandbox` is the proof that confinement was decided, and a `Host` cannot be built without it |
| `src/host.cpp` | the `host` table and the VM: stops and time limits at safepoints, `host.try`, `host.timed`, `host.load` and the small primitives |
| `src/host_proc.cpp` | signals, waiting until one arrives, and killing every process hx started |
| `src/host_sh.cpp` | `host.sh`: forking the command behind its wall, its limits, and both ends of its output |
| `src/host_http.cpp` | `host.post`: requests to the endpoints, each key to its own URL only |
| `src/host_files.cpp` | `host.read`, `host.write`, `host.list`, `host.walk` and `host.stat` |
| `src/host_judge.cpp` | `host.judge`: the agent's end of the channel to the judge |
| `src/host_spawn.cpp` | `host.spawn` and `host.wait`: children of hx that run a Luau function in a copy of the VM, which the loop's agents are |
| `src/host_internal.h`, `src/sys.h` | what the `host*.cpp` files share, and the small system helpers every file uses |
| `src/json.cpp` | JSON between Lua values and text, on yyjson's reader and writer |
| `lua/loop.luau` | the agent |
| `lua/judge.luau` | the judge, which `--judge` runs in a process of its own |
| `lua/ext/*.luau` | the built-in extensions, one per file; each file in `lua/ext/` is built in under its name |
| `.claude/skills/hx/SKILL.md` | the built-in skill `hx`, which is also this repository's own skill for hx and for Claude Code |
| `lua/skills/*/SKILL.md` | the other built-in skills, one directory each, for the model hx runs alone: `rpi` |

Development build, with AddressSanitizer, UBSan and warnings as errors:

```
cmake --preset dev && cmake --build --preset dev && ctest --preset dev
clang-format --dry-run --Werror src/*.cpp src/*.h
clang-tidy -p build-dev --quiet src/*.cpp
luau-analyze lua/*.luau lua/ext/*.luau tests/*.luau     # prints nothing when all is well; nor with --solver=old
```

The Luau tests are `tests/*_test.luau`, and share their checks in `tests/check.luau`. One runs by hand from a
scratch directory, with absolute paths, as `hx -q --ro /path/to/tests --run /path/to/tests/tools_test.luau`: the
`--ro` lets the test read `check.luau` through the wall. The directory a test starts in is its project: `ctest`
gives each an empty one of its own under `test-projects/` in the build directory, so they can run side by side
(`ctest -j`), and a test that writes a `.hx` directory and removes it refuses to start where there is one already.

`tests/agent_test.py` drives the whole binary against a stand-in model server, a fresh project and home directory
for each test. Besides the loop, the wall and the judge, it runs the examples of the `hx` skill as they are
written there, the extension, the loop check and the sub-agent, so a change that breaks what the skill tells a
model to type fails a test.

The Luau files in `lua/` are `--!strict`; an extension carries that on its second line, because its first is the
description the system prompt shows. `.luaurc` names the globals the host provides. When `luau-analyze` is
installed, `ctest` runs it over every Luau file as the test `analyze`, and anything it prints fails the test.

`tests/live_test.py` runs the agent against the real endpoint: making and editing files, reading a line range,
noticing a failed command, following a long output to its file, compacting a Vietnamese conversation into English
checkpoints that keep a fact, giving up a status polled between sleeps, reporting when out of replies,
following a skill (one listed in the prompt, one listed by name only, one found by searching, and one in the home
directory that only `--ro` makes reachable), and leading agents: two at work side by side, with a task sent back
to the one that did the work, and `--rpi` planning a change that its agents write. It
costs tokens, so ctest
leaves it out: `python3 tests/live_test.py build/hx` with `GEMINI_API_KEY` set, and `HX_MODEL=carbon` for a model
other than Neon.
