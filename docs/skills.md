# Skills

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
`~/.claude/skills/`, then among the skills built into it (below), and the first skill of a name wins. The two in your home directory are outside the wall, so
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

## Built in: hx and rpi

Two skills come with hx, and every session's prompt lists them, marked `(built in)`. Built-in skills come after
every skill directory, so a project's own skill of the same name takes the place of one. Having no directory, a
built-in skill is one text with no files beside it.

`rpi` is the workflow `hx --rpi` leads by (see [Agents](agents.md)). Its text is `lua/skills/rpi/SKILL.md`, and every
`lua/skills/NAME/SKILL.md` is built in under its name: that directory is for skills meant for the model hx runs
and for nobody else.

`hx` is on using and extending hx itself: starting it from a shell, scripts with `--run`, the judge, agents and
the rpi workflow, and what to make for which need, from an extension or a skill to a function or a sub-agent in
`.hx/loop.luau` to a change in hx's source. The prompt's paragraph on extending hx points to it, because a model
that is not pointed there takes that paragraph for all there is to know and loads nothing.

Its text is `.claude/skills/hx/SKILL.md` in this repository, compiled into the binary. Claude Code looks there
too, so when it works on hx it follows the same file. To give the skill to Claude Code everywhere, so that it can
hand a task to hx or write an extension for it in any project, write it out of the binary, from any project
directory:

```
mkdir -p ~/.claude/skills/hx
echo 'print(host.skills.hx)' | hx --run - > ~/.claude/skills/hx/SKILL.md
```
