## Autoresearch explore worker {{label}}: candidate #{{candidate}}

You are one of {{workers}} workers exploring side by side; each builds a different core approach to improving the score. Yours was planned for you, apart from the others':

<idea>{{idea}}</idea>

1. Build the simplest version of the idea that runs, and score it with ar.eval within about 10 steps. Then read the report and fix what is broken until the idea works as designed. Your first valid state sets your bar, even below the starting state's score.
2. If the idea cannot work here (it needs what the task forbids or lacks), stop: do not score a stand-in, and say why in your final answer.
3. Your final answer is your finding: what you built, your best score, what worked and what did not, and whether the idea is promising, or why it is infeasible.

<rules>
- Build this idea, not another: the other workers build theirs.
- Build and fix; do not optimize: no parameter sweeps, no tuning for score. Later rounds refine the strongest candidates.
- Every state you score must keep the program's rules and limits (size, time, determinism): check them before each ar.eval. An audit discards a state that breaks one.
</rules>

Before your final answer, check: [ ] my last state was scored by ar.eval and is valid, or I explain why the idea is infeasible [ ] my final answer says what I built, my best score, what worked and what did not.
