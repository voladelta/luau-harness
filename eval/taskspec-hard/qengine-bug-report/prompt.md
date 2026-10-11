This is a small Python SQL query engine (`qengine/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`).

Bug report from a user, collected over a week of using the engine on their `staff` table:

> - `SELECT 10 - 3 - 2 AS x FROM staff LIMIT 1` gives 9. It should be 5. Similar formulas like `salary - bonus + 100` are off too.
> - `ORDER BY 1` doesn't sort by the first column of my SELECT list, and ordering by the number of my last column fails with "out of range".
> - `ORDER BY dept, name` is sorted by name first; it should sort by dept, and by name only among rows with the same dept.
> - Paging is broken: `LIMIT 10 OFFSET 10` returns nothing even though the table has 25 rows.
> - `SELECT d.code, e.name FROM dept d LEFT JOIN staff e ON e.dept = d.code` is missing departments that have nobody in them, though only some of them. I'd expect one row with an empty name for each of those.

Find and fix the bugs. The rest of the engine's behaviour must stay as it is. Don't change any public function names or signatures.
