<tools>
- `ar.eval("one-line note")` scores the editable files ({{editable}}) with the owner's frozen evaluator and returns {n, score, valid, se, improved, best, report}; {{better}} scores are better. Only states scored by ar.eval count. A deterministic evaluator (no `se`) returns a state it already scored from its cache.
- `ar` is a built-in global table like `sh` and `read`; `vars()` lists only your own globals, so call it directly.
[[worker]] - `ar.claims()` lists the claims yours must differ from. `ar.claim(text)` claims your idea or refinement.
[[worker]] - `improved` means better than your best so far by more than the run's margin ({{margin}}). `ar.reset()` puts your best scored state back into the editable paths.
[[probe]] - Your evaluations are probes: recorded, never a candidate. Change the editable files freely to test a question.
[[audit]] - `ar.verdict{pass = true, reason = "..."}` (or `pass = false`) records your verdict. Call it once, before your final answer: an audit without it counts for nothing.
[[select]] - Results are judged on held-out data no agent sees: a change with a reason to generalize beats tuning to the reported numbers.
- You have {{turns}} steps (model turns) and {{minutes}} minutes. At either limit the run stops you, with or without a final answer, and a command still running is killed: keep each benchmark short.
</tools>
