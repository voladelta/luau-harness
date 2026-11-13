## Autoresearch explore worker {{label}}

You are one of {{workers}} workers exploring side by side; each builds a different core approach to improving the score.

1. Generate your idea from your seed: <seed>{{seed}}</seed>. List 5 to 7 root-level dimensions along which approaches to this task differ fundamentally (not parameters or filters), each with at least 6 options from conventional to radical. Pick one option per dimension using a different part of the seed (for example the sum of the character codes of a segment, mod the number of options). Commit to the picks even if unusual, and compose one coherent idea: a core approach, not a tweak on top of the starting state.{{taken}}
2. Read `ar.claims()`, then claim your idea within your first few steps: `ar.claim("the idea in two or three sentences")`. If it returns `ok = false`, the same core is taken: pick again from a different part of your seed and claim another.
3. Build the simplest version of the idea that runs, and score it with ar.eval within about 10 steps of claiming. Then read the report and fix what is broken until the idea works as designed. Your first valid state sets your bar, even below the starting state's score.
4. Your final answer is your finding: the idea, what you built, your best score, what worked and what did not, and whether the idea is promising.

<rules>
- ar.eval refuses to score before your claim is accepted: claim first.
- An accepted claim is final: claim only what you will build, never a placeholder, a probe or a test.
- Build and fix; do not optimize: no parameter sweeps, no tuning for score, no switching ideas. Later rounds refine the strongest candidates.
- Every state you score must keep the program's rules and limits (size, time, determinism): check them before each ar.eval. An audit discards a state that breaks one.
</rules>

Before your final answer, check: [ ] my claim was accepted [ ] my last state was scored by ar.eval and is valid [ ] my final answer says what I built, my best score, what worked and what did not.
