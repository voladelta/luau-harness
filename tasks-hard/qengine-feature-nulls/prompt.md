This is a small Python SQL query engine (`qengine/`, tests in `tests/`, run with `python3 -m unittest discover -s tests -t .`). Tables hold Python values and NULL is stored as `None`, but the engine does not implement SQL NULL semantics: comparisons with NULL return plain booleans or crash, aggregates choke on NULLs, and ORDER BY fails on NULL values.

Implement standard SQL NULL semantics:

1. New predicates `expr IS NULL` and `expr IS NOT NULL`. They bind like a comparison operator (`a + 1 IS NULL` means `(a + 1) IS NULL`, `NOT a IS NULL` means `NOT (a IS NULL)`) and always return TRUE or FALSE, never NULL.
2. Comparisons (`= != <> < <= > >=`), arithmetic (`+ - * / %`) and unary minus return NULL when any operand is NULL. NULL takes priority over errors: `NULL / 0` is NULL, while `1 / 0` still raises the existing division-by-zero error.
3. `AND`, `OR` and `NOT` use three-valued logic: `FALSE AND NULL` is FALSE, `TRUE AND NULL` is NULL, `TRUE OR NULL` is TRUE, `FALSE OR NULL` is NULL, `NOT NULL` is NULL. Results are `True`, `False` or `None`.
4. `WHERE`, `HAVING` and `JOIN ... ON` keep a row only when the condition is TRUE; NULL rejects the row just like FALSE. A LEFT JOIN row whose ON condition is NULL for every right row is padded with NULLs.
5. The scalar functions `UPPER`, `LOWER`, `LENGTH`, `ABS` and `ROUND` return NULL if any argument is NULL. `COALESCE` keeps its current behaviour.
6. Aggregates: `COUNT(*)` counts rows. `COUNT(expr)`, `SUM`, `AVG`, `MIN` and `MAX` ignore NULL values (with `DISTINCT`, NULLs are removed as well). When there are no non-NULL values (including an empty group), `SUM`, `AVG`, `MIN` and `MAX` return NULL and `COUNT` returns 0.
7. `GROUP BY` puts all rows with NULL keys into one group, and `DISTINCT` treats NULLs as equal.
8. `ORDER BY`: NULL sorts after every non-NULL value in ascending order and before every non-NULL value with `DESC`. Rows that tie keep their previous relative order, as now.
9. `format.render` shows NULL as `NULL`.

Update or add tests as you see fit.
