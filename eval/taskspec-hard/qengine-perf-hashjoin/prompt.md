This is a small Python SQL query engine (`qengine/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Joins are too slow: `executor.join` evaluates the ON condition for every pair of left and right rows, so joining two 4,000-row tables takes far too long. Joining tables of a few thousand rows each on a key must take well under a second.

Make equality joins run in roughly linear time with a hash join. That covers any join whose ON condition is an equality between an expression that uses only the newly joined (right) table and an expression that uses only the tables joined before it, either alone or combined with other conditions through `AND` (in either operand order, at any nesting depth of `AND`, with qualified or unqualified column names).

Query results must stay exactly as they are now, for every query that currently succeeds: the same columns, the same rows, and the same row order (left rows in their order; for each left row, its matching right rows in right-table order; LEFT JOIN padding unchanged). This includes:

- ON conditions that cannot use a hash join (no usable equality, `OR`, `NOT`, equalities whose sides both use the right table, and so on);
- the extra conditions next to the equality;
- values that compare equal in the current engine although they look different, such as `1`, `1.0` and `True`, and `NULL`, which currently equals `NULL`.

Update or add tests as you see fit.
