# Search the DuckDB docs

Full-text search over the DuckDB and DuckLake documentation and the DuckDB blog, from an index cached in
`.duckdb-skills/docs/`.

## 1. Pick the index and the terms

| Index | URL | Cache file | Versions |
|---|---|---|---|
| DuckDB docs and blog | `https://duckdb.org/data/docs-search.duckdb` | `duckdb-docs.duckdb` | `2.0`, `blog` |
| DuckLake docs | `https://ducklake.select/data/docs-search.duckdb` | `ducklake-docs.duckdb` | `stable` |

- DuckLake, its catalogs or its features: the DuckLake index. Anything else: DuckDB, version `2.0`.
- Background, history or reasons: version `blog`. When unsure, leave the version out.
- From a question, keep the technical words: `how do I find the most frequent value` becomes `most frequent value
  mode`. A function name or keyword (`arg_max`, `GROUP BY ALL`) stays as it is. An error message: its distinctive
  words, without the names from the user's query.

## 2. Fetch the index when it is missing or older than two days

```bash
F=.duckdb-skills/docs/CACHE_FILE
if [ ! -f "$F" ] || [ $(( $(date +%s) - $(stat -c %Y "$F") )) -gt 172800 ]; then
  duckdb -agent -init .duckdb-skills/state.sql -c "
    INSTALL httpfs; INSTALL fts; LOAD httpfs; LOAD fts;
    ATTACH 'URL' AS remote (READ_ONLY);
    ATTACH '$F.tmp' AS tmp;
    COPY FROM DATABASE remote TO tmp;" && mv "$F.tmp" "$F"
fi
```

The DuckDB index is about 15 MB. When the fetch fails, report that the index cannot be reached, and point to
https://duckdb.org/docs or https://ducklake.select/docs.

## 3. Search

```bash
duckdb -agent -readonly -cmd "SET home_directory='$PWD/.duckdb-skills/home'" .duckdb-skills/docs/CACHE_FILE <<'SQL'
LOAD fts;
FROM (
    SELECT chunk_id, page_title, section, breadcrumb, url, version, text,
           fts_main_docs_chunks.match_bm25(chunk_id, 'TERMS') AS score
    FROM docs_chunks
    WHERE version = 'VERSION'
)
WHERE score IS NOT NULL
ORDER BY score DESC
LIMIT 8;
SQL
```

- The score is filtered outside the subquery: DuckDB refuses `WHERE score` beside `match_bm25` in one `SELECT`.
- Drop the `WHERE version` line to search them all.
- With no results, drop the least specific term, or try a single word, and search again.
- `text` is cut at 500 characters in agent mode. For a chunk you need whole, select only its `text` with
  `-json` and the chunk's `chunk_id`.

## 4. Answer

Lead with the answer the chunks give, then the sources, each as `section — page_title` with
`https://duckdb.org` followed by its `url` (DuckLake's under `https://ducklake.select`).
