# Attach a database

Opens a `.duckdb` file (or SQLite, Postgres, another DuckDB database) and adds it to `state.sql`, so every later
command with `-init` has its tables.

## 1. Resolve the path

```bash
P="$(realpath -m 'GIVEN_PATH')"
test -f "$P" && echo exists
```

When the file does not exist, DuckDB creates it at the first write. Do that only when the task asks for a new
database; otherwise report the missing file.

## 2. Check it opens

```bash
duckdb -agent "$P" -readonly -c "PRAGMA version;"
```

A failure means the file is not a DuckDB database or is damaged: report the error and stop. For a SQLite file,
run `INSTALL sqlite;` once, and in step 4 add `LOAD sqlite;` before the ATTACH line and `TYPE sqlite` to its
options.

## 3. Look at the schema

```bash
duckdb -agent "$P" -readonly -c "SELECT table_name, estimated_size, column_count FROM duckdb_tables() ORDER BY table_name;"
```

For each table that matters, up to 20: `DESCRIBE name; SELECT count() FROM name;`.

## 4. Add it to state.sql

The alias is the file's name without its extension (`sales_2024.duckdb` is `sales_2024`). When another ATTACH in
`state.sql` already uses that alias for another path, add a suffix.

```bash
OPTS=""
grep -qF "'$P'" .duckdb-skills/state.sql || printf "ATTACH IF NOT EXISTS '%s' AS %s %s;\nUSE %s;\n" "$P" ALIAS "$OPTS" ALIAS >> .duckdb-skills/state.sql
```

Set `OPTS='(READ_ONLY)'` when the task only reads the database, `'(TYPE sqlite)'` for SQLite, or both in one
list: `'(TYPE sqlite, READ_ONLY)'`. A database outside the project is behind hx's wall:
it can be read only when hx was started with `--ro DIR`, and written only with `--rw DIR`.

## 5. Check and report

```bash
duckdb -agent -init .duckdb-skills/state.sql -c "SHOW TABLES;"
```

Report the path, the alias, and each table with its column and row counts, or that the database is empty.
Queries go on with `query.md`.
