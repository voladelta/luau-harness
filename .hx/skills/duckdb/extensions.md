# Extensions and DuckDB itself

## DuckDB is missing

hx's wall lets in only programs on the user's PATH, and installing one means writing to the home directory, which
the wall forbids. Tell the user to install it outside hx and start hx again:

- Linux: `curl -fsSL https://install.duckdb.org | sh` (it links `~/.local/bin/duckdb`)
- macOS: `brew install duckdb`
- Windows: `winget install DuckDB.cli`

## Install an extension

A name installs from core; `name@repo` from that repository (`magic@community` is `INSTALL magic FROM community`).
Install them all in one call:

```bash
duckdb -agent -init .duckdb-skills/state.sql -c "INSTALL spatial; INSTALL h3 FROM community; LOAD spatial; LOAD h3;"
```

They land in `.duckdb-skills/home/.duckdb/extensions/` because of `state.sql`, so a command without `-init` does
not find them. Common ones: `httpfs` (HTTP, S3, GCS), `azure`, `spatial`, `excel`, `sqlite`, `postgres`, `fts`,
`h3` (community), `ducklake`, `iceberg`, `delta`.

## Update

```bash
duckdb -agent -init .duckdb-skills/state.sql -c "UPDATE EXTENSIONS;"
duckdb -agent -init .duckdb-skills/state.sql -c "UPDATE EXTENSIONS (spatial, httpfs);"
```

To see whether DuckDB itself is current:

```bash
duckdb --version
curl -fsSL https://duckdb.org/data/latest_stable_version.txt
```

When it is older, tell the user the command above that upgrades it; it cannot run inside hx's wall.
