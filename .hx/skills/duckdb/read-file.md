# Read a data file

For data, not source code. If the user named a file without a path, find it:
`find "$PWD" -name 'NAME' -not -path '*/.git/*' | head -1`.

## 1. Read it

One command defines `read_any`, which picks the reader by the file's extension, and shows the schema, the row
count and a sample:

```bash
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
CREATE OR REPLACE MACRO read_any(file_name) AS TABLE
  WITH json_case AS (FROM read_json_auto(file_name))
     , csv_case AS (FROM read_csv(file_name))
     , parquet_case AS (FROM read_parquet(file_name))
     , avro_case AS (FROM read_avro(file_name))
     , blob_case AS (FROM read_blob(file_name))
     , spatial_case AS (FROM st_read(file_name))
     , excel_case AS (FROM read_xlsx(file_name))
     , sqlite_case AS (FROM sqlite_scan(file_name, (SELECT name FROM sqlite_master(file_name) LIMIT 1)))
     , ipynb_case AS (
         WITH nb AS (FROM read_json_auto(file_name))
         SELECT cell_idx, cell.cell_type, array_to_string(cell.source, '') AS source, cell.execution_count
         FROM nb, UNNEST(cells) WITH ORDINALITY AS t(cell, cell_idx)
         ORDER BY cell_idx
     )
  FROM query_table(
    CASE
      WHEN regexp_matches(file_name, '\.(json|jsonl|ndjson|geojson|geojsonl|har)$', 'i') THEN 'json_case'
      WHEN regexp_matches(file_name, '\.(csv|tsv|tab|txt)$', 'i') THEN 'csv_case'
      WHEN regexp_matches(file_name, '\.(parquet|pq)$', 'i') THEN 'parquet_case'
      WHEN regexp_matches(file_name, '\.avro$', 'i') THEN 'avro_case'
      WHEN regexp_matches(file_name, '\.(xlsx|xls)$', 'i') THEN 'excel_case'
      WHEN regexp_matches(file_name, '\.(shp|gpkg|fgb|kml)$', 'i') THEN 'spatial_case'
      WHEN regexp_matches(file_name, '\.ipynb$', 'i') THEN 'ipynb_case'
      WHEN regexp_matches(file_name, '\.(db|sqlite|sqlite3)$', 'i') THEN 'sqlite_case'
      ELSE 'blob_case'
    END
  );

DESCRIBE FROM read_any('PATH');
SELECT count() AS row_count FROM read_any('PATH');
FROM read_any('PATH') LIMIT 20;
SQL
```

Replace `PATH`. For a remote file, put the lines for its protocol before the macro:

| Protocol | Before the macro |
|---|---|
| `https://`, `http://` | `LOAD httpfs;` |
| `s3://` | `LOAD httpfs; CREATE SECRET (TYPE S3, PROVIDER credential_chain);` |
| `gs://`, `gcs://` | `LOAD httpfs; CREATE SECRET (TYPE GCS, PROVIDER credential_chain);` |
| `az://`, `azure://`, `abfss://` | `LOAD httpfs; LOAD azure; CREATE SECRET (TYPE AZURE, PROVIDER credential_chain);` |

Private buckets need credentials hx keeps out: see `s3-explore.md`.

## 2. When it fails

- An extension is missing (spatial, excel, sqlite): put `INSTALL spatial; LOAD spatial;` (or `excel`,
  `sqlite_scanner`) before the macro and run again.
- The wrong reader, or a parse error: call the right `read_*` function directly instead of `read_any`, with
  options such as `read_csv('PATH', delim=';', header=true)`.

## 3. Answer

From the schema, row count and sample, answer the question; with none, describe the column types, the row count
and anything notable. For more questions about the same data, go on with `query.md`; to keep a large file as a
table, `attach-db.md`.
