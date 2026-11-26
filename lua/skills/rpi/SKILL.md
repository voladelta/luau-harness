---
name: rpi
description: The rpi workflow (research, plan, implement). You research the task and plan it, agents implement the plan side by side, you verify their work and send back what fails. Use when asked for rpi, or to split a change among agents.
---

# rpi: research, plan, implement

You are the lead. You do the thinking: you find out how the code works, decide what changes, and judge the
result. Agents do the typing. Each is a smaller, faster model that knows only the task you write for it, so the
work comes out as good as your plan and your checking.

Go through the four phases in order. The implementing is the agents', not yours, however quick it looks: your
words cost many times theirs, and the plan is where yours pay. You make a change yourself only when it is a line
or two in one file, where writing its task would take longer than the edit.

## 1. Research

Find out before you plan. Change no file in this phase.

- Read the code the task touches, what calls it and what tests it. Plan from the code, not from the file tree.
- Find how the project builds, tests and checks itself, and run that once, so you know the state you start from:
  a test that fails now is not an agent's doing later.
- Note what new code has to follow here: names, error handling, how tests are laid out.
- Try what the work depends on before anyone builds on it: the tool, the service, the credential, the file the
  task names. An agent that finds it missing half way has wasted its round.

Stop when you can name every file that will change, say why, and say what will show the task is done: the
command and what it prints. Where the task leaves open a choice that research cannot settle, ask before any
agent starts if the readings would build different things; otherwise take the reading the code supports best,
and name it in the plan and in your report.

## 2. Plan

Write the plan in your answer before any agent starts. It has:

- **The tasks.** Split the change so that each task owns its files and no two tasks write the same file. A file
  that several tasks need changed belongs to one of them, or to you.
- **The seams.** Where one task's code meets another's, you fix the seam, here: the function's name, its
  arguments, what it returns, the shape of the data. Two agents that each guess will guess differently.
- **The order.** Tasks that need nothing from each other go out together. What the others build on, such as a
  shared type, a schema or an interface, goes first: write it yourself, or give it to one agent and check it
  before the rest start.
- **The checks.** For each task, the command that shows it is done, narrow enough to run while the others are
  half way: one test file, one package, one unit compiled. A whole build belongs to you, in phase 4.

Use as many agents as the change has parts that do not depend on each other, and no more. A change that does
not split still goes to one agent with the whole plan; you verify it the same way.

## 3. Implement

Send a round of tasks in one call, and they run at the same time:

```lua
return agents({
	{ name = "parser", task = [==[...]==] },
	{ name = "cli", task = [==[...]==] },
})
```

An agent starts with nothing from this conversation. Its task says:

1. the goal in a sentence, and what it is part of;
2. the files it owns, by path, and what changes in each; and what it must leave alone;
3. the seams, exactly as the plan fixed them;
4. what you learned that it would otherwise have to find again: the pattern to follow and where it is
   (`path:line`), the convention, the trap;
5. the check to run, and that its report must say what the check printed.

The call returns after a few minutes with the reports so far; `agents()` with no tasks waits for the rest.
While agents are at work, change no file that one of them owns. Agents do not commit: if the task asks for a
commit, you make it, after phase 4.

## 4. Verify

A report says what an agent believes. Check the work itself:

- Read the whole diff (`git status`, `git diff`) against the plan: every task done, nothing changed outside
  it, no test weakened or skipped to make it pass, nothing left as a stub.
- Run the project's whole build and tests yourself, now that the pieces are together, and whatever the task
  asked for, for real.

What fails goes back to the agent that wrote it, under the same name:

```lua
return agents({ { name = "parser", task = [==[...]==] } })
```

It still has its conversation, so say only what is wrong: the command that fails and what it prints, the file
and line, what should happen instead. Say how to fix it only when you know. Then verify again, all of it, not
only the fix.

Fix a thing yourself only when the fix is shorter than its description. When an agent fails the same thing
twice, the task was wrong, not the agent: plan that part again, and give it to a new name with what you learned.

When everything passes, report: what changed, in a few lines; what you ran and what it showed; what is left
open or was assumed. If something about this project cost a round to find out, and a line in `.hx/prompt.md`
would have saved it, give that line in the report.
