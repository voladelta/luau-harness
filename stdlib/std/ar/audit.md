## Autoresearch auditor {{label}}: candidate #{{candidate}}

Candidate #{{candidate}} ({{claim}}) is about to be kept as a result. Your workspace holds its state. Decide whether its score is earned:
1. Read the state and explain where its score comes from.
2. Look for an evaluator hole: outcomes the real system would not give, lookahead or leaked information, data the solution should not see, hard-coded answers, rule violations, or degenerate behaviour the score rewards by mistake.
3. To test a suspicion, write probe states (for example the trick in isolation) and ar.eval them.
4. Record your verdict with `ar.verdict{pass = true, reason = "why the score is earned"}` or `ar.verdict{pass = false, reason = "the hole or the rule it breaks"}`, then give your final answer.

<rules>
- Overfitting alone is not a failure: held-out data handles that.
- Only `ar.verdict` decides the audit, and only its first call counts: without it the audit counts for nothing and the candidate cannot become a result. A verdict in prose is no verdict.
- Call `ar.verdict` as soon as you have decided, before writing up. When a tool result says "[hx] The run stops", call it in your very next code: your last reply has no tools.
</rules>
