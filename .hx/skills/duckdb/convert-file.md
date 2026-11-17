# Convert a file

The output path is the one the user gave, or the input's name with `.parquet` when they gave none. Its extension
picks the format:

| Extension | Format clause |
|---|---|
| `.parquet`, `.pq` | none |
| `.csv` | `(FORMAT csv, HEADER)` |
| `.tsv` | `(FORMAT csv, HEADER, DELIMITER '\t')` |
| `.json` | `(FORMAT json, ARRAY true)` |
| `.jsonl`, `.ndjson` | `(FORMAT json, ARRAY false)` |
| `.xlsx` | `(FORMAT xlsx, HEADER true)`, after `LOAD excel;`; without `HEADER` the column names are lost |
| `.geojson` | `(FORMAT GDAL, DRIVER 'GeoJSON')`, after `INSTALL spatial; LOAD spatial;` |
| `.gpkg` | `(FORMAT GDAL, DRIVER 'GPKG')`, after loading spatial |
| `.shp` | `(FORMAT GDAL, DRIVER 'ESRI Shapefile')`, after loading spatial |

```bash
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
LOADS
COPY (FROM 'INPUT') TO 'OUTPUT' FORMAT_CLAUSE;
SQL
```

- `LOADS` are the extensions both the input and the output need. A remote input takes the lines for its protocol
  from the table in `read-file.md`.
- When the input's reader needs options, name it: `COPY (FROM read_csv('INPUT', delim=';')) TO ...`.
- Partitioned output (Parquet or CSV only): add `PARTITION_BY (col)` to the clause, and the output is a directory.
- Compression for Parquet: add `CODEC 'zstd'`.

Report the input and its format, the output, its format and its size from `ls -lh`, and the row count:
`SELECT count() FROM 'OUTPUT';`. When the input fails to parse, look at it with `read-file.md` first.
