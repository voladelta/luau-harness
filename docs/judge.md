# The judge

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
| `check` | optional, executable. Runs as `check DIR` after each run, in the kit's directory, where `DIR/.judge/stdout` is what the run printed; links in `DIR` are removed first, so the run cannot point the check at the kit, and the check does not start in `DIR`, so a module or file the candidate left there is not taken for the check's own. It exits 0 when the output is right. A number on its last line is the score: positive, and higher is better; without one, the measure is the run's time. What it prints when it fails goes to the model, unless `blind` is on |
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
  copy of the candidate, the system directories and the `--ro` and `--rw` paths. The project is not among them: a
  `PATH` entry in the project names the same directory in the copy, so a candidate runs its own `node_modules/.bin`
  and not the one the model is still writing. Only the script being run is
  copied in, so `check` and whatever else the kit holds stay out of reach. `check` reads the kit and the run's copy
  and writes nothing that lasts. What a failed build or run printed goes to the model only from the file hx
  sent it to: a link left in that file's place is not read.
- **A candidate leaves nothing running.** What a build or run started dies when it ends, so no process of the
  candidate's can slow the champion's runs or write into an output after the run.
- **The judge is built into hx** (`lua/judge.luau`). A project's loop can change everything about the agent, but
  not the judge.

Limits: the candidate's code can read `run` and what it reads, so keep answers in `check`. Candidates are not cut
off from the network or from SysV IPC, so a helper the agent leaves running can carry results between runs, and
the agent can lower the priority of the judge's runs. A check that accepts a wrong answer lets it win, so try a
kit on the unchanged project and on a known-bad change before trusting it.
