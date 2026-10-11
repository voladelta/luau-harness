This is a small Python diff/patch tool (`diffy/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`). Its unified diffs are supposed to be interchangeable with GNU `diff -u` and `patch`.

Bug reports:

> - GNU `patch` misplaces or rejects some hunks from `diffy diff`. It happens when one side of a hunk is empty, i.e. the hunk only adds or only removes lines. That is common with `-U 0`, and when creating a file from an empty one.
> - The other way round, `diffy apply` fails or puts the lines in the wrong place for some patches made by GNU `diff -u`, for example a patch that fills an empty file (`@@ -0,0 +1,3 @@`) or a `-U0` patch that inserts lines.
> - With 3 lines of context, two changes separated by exactly 6 unchanged lines come out as two hunks; GNU diff produces a single hunk there. Other gaps look right.

`diffy diff` output must be byte-for-byte what GNU `diff -u` (and Python's `difflib.unified_diff` with the same names and context) produces for the same edit, and `diffy apply` must accept GNU-generated patches. Find and fix the bugs. Don't change any public function names or signatures.
