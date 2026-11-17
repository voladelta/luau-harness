# Query

Raw SQL runs as given. A question in words becomes SQL first, written with the idioms below.

## 1. Know what you query

- Files: SQL names them directly, `FROM 'data.csv'`, `FROM 'parts/*.parquet'`.
- Attached databases: `grep -i attach .duckdb-skills/state.sql` lists them, and `-init` opens them. When an
  ATTACH there fails, say so and query the files directly.
- Before writing SQL for a question, look at the schema:
  `duckdb -agent -init .duckdb-skills/state.sql -c "SELECT database_name, table_name, estimated_size FROM duckdb_tables();"`,
  then `DESCRIBE tbl;` or `DESCRIBE FROM 'file';` for the ones that matter.

## 2. Run it

```bash
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
QUERY;
SQL
```

- `-agent` already cuts a large result to its first and last rows, so a big table cannot flood the conversation.
  Still add `LIMIT` or an aggregate when only part of the result answers the question.
- A source over a million rows prints an estimate and progress on stderr. For a query that may run long, bound it:
  `SET max_execution_time = 60000;` (milliseconds).
- To check what a query returns without running it: `DESCRIBE <query>`.

## 3. When it fails

- A syntax error: fix the query and run it again.
- `Extension "X" not loaded`: `INSTALL X; LOAD X;` before the query (see `extensions.md`).
- A table that is not found: list them with `FROM duckdb_tables()` and use the right name.
- A file that is not found: `find "$PWD" -name 'NAME' -not -path '*/.git/*'`.
- Anything unclear: search `docs.md` for the error's words.

## 4. Report

Show the result. For a question in words, say in a sentence what the numbers mean.

## Friendly SQL

Prefer DuckDB's own idioms:

- `FROM tbl WHERE x > 10` without `SELECT *`; `count()` without `*`.
- `GROUP BY ALL`, `ORDER BY ALL`.
- `SELECT * EXCLUDE (a, b)`, `SELECT * REPLACE (lower(name) AS name)`, `COLUMNS('re')`, `COLUMNS(*)` with a lambda.
- `UNION ALL BY NAME` for tables whose columns differ in order or number.
- `SELECT x: 42` as an alias; an alias can be used in `WHERE`, `GROUP BY`, `HAVING` and later in the same `SELECT`.
- Trailing commas in a `SELECT` list; `LIMIT 10%`.
- `count() FILTER (WHERE x > 10)`; `GROUPING SETS`, `CUBE`, `ROLLUP`.
- Top N per group: `max(col, 3)`, `arg_max(arg, val, 3)`, `min_by(arg, val, 3)`.
- `DESCRIBE tbl`, `SUMMARIZE tbl` for a statistical profile, `PIVOT` and `UNPIVOT`.
- `SET VARIABLE x = expr`, read with `getvariable('x')`.
- Method calls: `name.trim().lower()`; slicing `s[1:3]`, `list[-1]`; lists `[1, 2, 3]`;
  comprehensions `[x * 2 FOR x IN l]`; `{'a': 1}.a` and `s.*` for structs; `format('{} -> {}', a, b)`.
- Lambdas: `list_transform(l, lambda x: x + 1)`. The arrow form `x -> x + 1` is an error in 2.0.
- `ASOF JOIN` for the nearest earlier row by time; `POSITIONAL JOIN` by row number; `LATERAL` subqueries.
- `CREATE OR REPLACE TABLE t AS SELECT ...`; `INSERT INTO t BY NAME SELECT ...`; `INSERT OR REPLACE`,
  `INSERT OR IGNORE`.
