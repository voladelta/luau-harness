---
name: duckdb
description: Query, read, convert and explore data with the DuckDB CLI: CSV, Parquet, JSON, Excel, SQLite, .duckdb files, S3 and HTTPS, spatial data and Overture Maps, the DuckDB docs, and past hx sessions. Use for any data file or SQL question.
---

# duckdb

The DuckDB CLI is `duckdb`, version 2.0. Every command runs inside hx's wall: it writes only the project and
`$TMPDIR`, so DuckDB's state, extensions and caches live in `.duckdb-skills/` in the project, never in `~/.duckdb`.

## Before the first query

Run this once a session; it does nothing when the directory is already there:

```bash
D="$PWD/.duckdb-skills"
mkdir -p "$D/home" "$D/docs"
test -f "$D/.gitignore" || printf '*\n' > "$D/.gitignore"
test -f "$D/state.sql" || printf "SET home_directory='%s/home';\n" "$D" > "$D/state.sql"
command -v duckdb
ls "$D"/home/.duckdb/extensions/*/*/httpfs.duckdb_extension >/dev/null 2>&1 ||
  duckdb -agent -init "$D/state.sql" -c "INSTALL httpfs; INSTALL fts; INSTALL spatial; INSTALL excel;"
```

If `duckdb` is not found, read `extensions.md` and stop there. `LOAD x` fails with `Extension ... not found` for
any other extension until `INSTALL x` has run once.

## Every command

```bash
duckdb -agent -init .duckdb-skills/state.sql -c "SQL"
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
QUERY;
SQL
```

- `-agent` gives markdown tables with the column types, cuts a large result to its first and last rows with the
  count, and writes errors as JSON on stderr. An output flag such as `-csv` or `-json` turns it off: use one only
  to write a file another program reads.
- `-init .duckdb-skills/state.sql` keeps extensions under the project, and restores the databases, secrets and
  macros the other files add. Never overwrite `state.sql`: append to it, and check with `grep` first that the line
  is not already there.
- Use a heredoc for SQL longer than a line, so the shell leaves its quotes alone.
- Lambdas are written `lambda x: x + 1`; the old `x -> x + 1` is an error in 2.0.
- When an error is unclear, search the docs (`docs.md`) for its words before guessing.

## What to read

| The task | Read |
|---|---|
| Look at a data file: what is in it, its schema, a sample | `read-file.md` |
| Answer a question in SQL, or run SQL the user gave; the Friendly SQL idioms | `query.md` |
| Turn a file into another format: Parquet, CSV, JSON, Excel, GeoJSON | `convert-file.md` |
| Open a `.duckdb` database and keep it for later queries | `attach-db.md` |
| Data on S3, R2, GCS or MinIO: list a bucket, read remote files | `s3-explore.md` |
| Places, coordinates, distances, GeoJSON, Shapefile, GPX; Overture Maps | `spatial.md` |
| How DuckDB or DuckLake does something; an error to explain | `docs.md` |
| Install or update an extension, or DuckDB itself | `extensions.md` |
| What an earlier hx session decided or left undone | `memories.md` |

Read the file the task names before you write SQL: each one holds steps and checks that were tried on 2.0.

Adapted from github.com/duckdb/duckdb-skills, under the MIT licence in `LICENSE`.
