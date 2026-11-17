# Agents

One model working down a list is slow where the list could be split. `agents` splits it: the session's model, the
**lead**, gives each task to an agent, the agents work at the same time, and the lead gets their reports.

```lua
return agents({
	{ name = "parser", task = "In src/parse.c, make parse_line accept CRLF line ends. ... Check with make test-parse." },
	{ name = "docs", task = "In README.md, document the --strict flag. ..." },
})
```

```
[agent parser: done in 142 s]
Changed src/parse.c: parse_line strips a trailing \r. make test-parse: 14 passed.

[agent docs: done in 38 s]
README.md has a "Strict mode" section.
```

An agent is a model with a conversation of its own. Each run of one is a child process: a fork of hx as it is when
the task is sent (`host.spawn`), so it stands behind the same wall, in the same directory, with the same
functions, extensions and skills, and needs no key passed to it. It runs one turn and sends back its
conversation, which the lead's process keeps. The lead's own conversation holds tasks and reports and none of
the agents' steps, which is what keeps a long job inside its context.

- **The model.** Agents run on `M.agent_model`: `deepseek-flash` when `DEEPSEEK_API_KEY` is set, whatever the
  lead's model, and the lead's own model otherwise. Their tokens are counted with the session's, on the line of
  their model.
- **The prompt.** An agent gets the session's system prompt and `M.agent_prompt` after it: that it is one of
  several, to change only what its task gives it, to leave git's index and branches alone, to check its own work
  narrowly, and to end with a report. It knows nothing of the lead's conversation, so the task has to carry what
  it needs; the `agents` line in the tool's description tells the lead so.
- **Sending work back.** A task for a name that already has an agent is the lead's next message in that agent's
  conversation, with everything the agent did still there. A new name is a new agent.
- **Waiting.** The call returns when every agent has reported, or after `M.agent_wait` (270) seconds with the
  reports so far and a line for each agent still at work; `agents()` with no tasks waits again. The wait is short
  on purpose: the lead's next request then falls inside the five minutes Claude keeps its prompt cache, and well
  inside `M.call_seconds`. A report is given once. An agent at work takes no second task until it has reported.
- **How many.** `M.agent_limit` (8) agents work at once; more wait their turn. One run of an agent may take
  `M.agent_seconds` (1800), and is then stopped and reported as stopped, with its conversation intact.
- **What is shared.** Files, and nothing else. What an agent sets in the VM, globals and loaded extensions, is its
  own and gone with its run; what its commands left running is killed when the run ends. Agents write the one
  project directory, so two that edit the same file overwrite each other: the lead is told to give each task its
  own files, and hx does not check that it did.
- **What an agent lacks.** It has no `agents` function, so agents do not multiply, and no `submit` under
  `--judge`: the lead submits. Its words go to stderr as progress lines headed by its name, `[parser] bash make
  test-parse`, and only the lead's words are the answer on stdout.
- **Stopping.** Ctrl-C reaches the agents as it reaches hx, and each ends its run; an agent stopped that way keeps
  the conversation it had before the task. When hx exits, every agent still at work is killed with what it
  started. A lead that answers while agents are at work would end them that way, so it is told once, and its next
  answer stands.

## The rpi workflow

`hx --rpi TASK` runs the task by **rpi**: research, plan, implement. The lead is Opus 5.5, unless `--model`
names another, and its agents are `deepseek-flash`'s, when both keys are set: the expensive model reads, decides
and judges, and the cheap ones type.

1. **Research.** The lead reads the code the task touches, and runs the project's build and tests once to know
   where it starts. It changes nothing.
2. **Plan.** It splits the change into tasks that each own their files, fixes the seams between them itself
   (names, arguments, formats), orders what depends on what, and gives each task a check narrow enough to run
   while the others are half way.
3. **Implement.** It sends a round of tasks with one `agents` call. Each task names the goal, the files, the
   seams, what the lead learned that the agent would otherwise look up again, and the check.
4. **Verify.** It reads the whole diff against the plan and runs the whole build and tests itself. What fails
   goes back to the agent that wrote it, under its name, with the failing output; then it verifies again.

The procedure is a skill, `rpi`, built into hx (`lua/skills/rpi/SKILL.md`). `--rpi` puts its text into the lead's
system prompt, so it holds for every message of the session, is cached with the prompt and outlasts a compaction;
the agents' prompt does not carry it. Without the flag the skill is listed like any other, and a session on any
model follows it when asked: `hx "use the rpi skill to ..."`. A project's own `.hx/skills/rpi/SKILL.md` takes its
place in both cases, so a project can say how its own work is to be split and checked.

`--rpi` is one case of a **workflow**: a skill that leads the whole session rather than one the model picks.
`M.workflow` names it (`host.workflow`, which is `"rpi"` with the flag), and a project's `.hx/loop.luau` can set
it to any skill, to start every session there under that skill. A workflow whose skill is missing stops hx
rather than running without it.
