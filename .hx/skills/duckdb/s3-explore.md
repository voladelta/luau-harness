# Explore object storage

S3, Cloudflare R2, GCS, MinIO and other S3-compatible stores, read in place without downloading.

## 1. Credentials

Every command starts with `LOAD httpfs;` and, unless the bucket is public, one secret:

| Provider | URL | Secret |
|---|---|---|
| AWS S3 | `s3://` | `CREATE SECRET (TYPE S3, PROVIDER credential_chain);` |
| Cloudflare R2 | `r2://`, or `s3://` with an R2 endpoint | `CREATE SECRET (TYPE R2, PROVIDER credential_chain);` |
| GCS | `gs://`, `gcs://` | `CREATE SECRET (TYPE GCS, PROVIDER credential_chain);` |
| MinIO and others | `s3://` with an endpoint | `CREATE SECRET (TYPE S3, KEY_ID '...', SECRET '...', ENDPOINT '...', USE_SSL true);` |

- R2's endpoint is `ACCOUNT_ID.r2.cloudflarestorage.com`; write `r2://bucket/path` as `s3://bucket/path` with the
  R2 secret.
- A public bucket (Overture Maps, AWS Open Data) needs no secret, only its region:
  `CREATE SECRET (TYPE S3, PROVIDER config, REGION 'us-west-2');`.
- hx takes variables that look like secrets (`AWS_SECRET_ACCESS_KEY` and the like) out of every command's
  environment, and `~/.aws` is outside its wall, so `credential_chain` finds nothing. For a private bucket, hx must
  be started with `--keep-env AWS_ACCESS_KEY_ID --keep-env AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`,
  `AWS_REGION`), or with `--ro ~/.aws`. When neither was done, say so rather than try keys from elsewhere.

## 2. What the URL is

A bucket or a directory (no extension, or a trailing `/`): list it, reading only names and sizes. Never select
`content`, which downloads every file.

```bash
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
LOAD httpfs;
SECRET
SELECT filename, (size / 1048576)::DECIMAL(10, 1) AS size_mb, last_modified
FROM read_blob('URL/*')
ORDER BY filename
LIMIT 50;
SQL
```

A file or a glob: show its schema, rows and a sample. Name the reader: `FROM 'URL'` picks one by the extension,
and a glob that ends in `/*` has none.

```bash
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
LOAD httpfs;
SECRET
DESCRIBE FROM read_parquet('URL');
FROM read_parquet('URL') LIMIT 20;
SQL
```

For Parquet, take the row count from the files' footers, which reads no data:

```sql
SELECT file_name, num_rows FROM parquet_file_metadata('URL');
```

Not `sum(row_group_num_rows)` over `parquet_metadata`: it has a row per column of each row group, so the sum is
the row count times the number of columns. The sizes are in the listing above.

## 3. Answer

Write the SQL the question needs (`query.md`). Filters on Parquet are pushed down to the remote files, so
filtering a large remote dataset reads only the parts it needs.

## When it fails

- 403, access denied: the credentials above.
- 404, no such bucket: the URL and the region.
- A listing that times out: narrow the glob to a prefix.
