This is a small Python diff/patch tool (`diffy/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`). `apply.apply` only applies a patch when every hunk matches exactly at the line its header names, so it fails as soon as the file has drifted a little. Add GNU-patch-style tolerant application.

**API.** In `diffy/apply.py` add `apply_fuzzy(patch, lines, fuzz=2)` returning an `ApplyResult` with `.lines` (the patched lines) and `.hunks`, one `HunkResult` per hunk in patch order with these fields:
- `status`: `"applied"` or `"failed"`
- `line`: 1-based line number in the original file where the hunk's old side starts as placed (for a failed hunk: its expected position, defined below, plus 1)
- `offset`: the placed start minus the start given by the hunk header (0 for a failed hunk)
- `fuzz`: the fuzz level used (0 for a failed hunk)

`fuzz < 0` raises `ValueError`. `apply.apply` stays as it is.

**Placement rules.** All positions refer to the original file. A hunk's *header start* is the 0-based index its header names (the `old_start` the parser already computes).
1. Hunks are placed in patch order. A hunk's *expected position* is its header start plus the offset of the most recently applied hunk (0 if none has applied yet).
2. Fuzz level `f` ignores the first `min(f, L)` and the last `min(f, T)` lines of the hunk's old side, where `L` and `T` are the numbers of context lines before the hunk's first change and after its last change. Only the remaining old-side lines are compared with the file. The ignored lines are neither checked nor modified in the output. Their counterparts on the new side are dropped too, so the file keeps its own version of those lines.
3. Try levels `0, 1, ..., fuzz` in order. At each level, try candidate starts (where the hunk's full old side would begin) from the nearest to the expected position outwards. At equal distance the later candidate goes first (`p, p+1, p-1, p+2, p-2, ...`). A candidate is valid only if the compared lines lie entirely inside the file and begin no earlier than the end of the lines replaced by the previously applied hunk. The first candidate whose compared lines equal the file's lines wins, so an exact match anywhere beats a fuzzy match nearby. A hunk with nothing to compare (for example a pure insertion) is placed at the nearest valid candidate.
4. A hunk that matches nowhere is `"failed"`: it changes nothing and does not affect the expected position or the placement limit of later hunks, which are still tried.

**Report.** Add `report(result)` returning a list of GNU-patch-style messages, one for each hunk that did not apply cleanly at its header position, in hunk order (hunks are numbered from 1):
- `Hunk #2 succeeded at 14 (offset 3 lines).`, `Hunk #1 succeeded at 7 (offset -1 line).` (singular for ±1)
- `Hunk #3 succeeded at 40 with fuzz 1 (offset -2 lines).`, `Hunk #1 succeeded at 2 with fuzz 2.` (no offset part when the offset is 0)
- `Hunk #4 FAILED at 51.`

The line numbers are `HunkResult.line`. A hunk applied with offset 0 and fuzz 0 gets no message.

**CLI.** `python3 -m diffy.cli apply PATCH FILE [--fuzz N]` (default fuzz 2) now uses `apply_fuzzy`. It prints the report lines to stdout and writes the result (all hunks that applied) to FILE. If any hunk failed, it also writes `FILE.rej` with the `---`/`+++` header lines of the patch followed by the failed hunks, formatted exactly as `unified.format_hunk` formats them (original headers), and exits with status 1. Otherwise it exits with 0 and writes no `.rej` file. Parse errors still print an error and exit 1.

Update or add tests as you see fit.
