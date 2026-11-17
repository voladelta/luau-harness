# Spatial questions

DuckDB's spatial extension, and Overture Maps as free global data for places, buildings, roads and boundaries.

## 1. What the question needs

| The question | Data | Functions |
|---|---|---|
| X near Y, with no file from the user | Overture Maps | bbox filter, `ST_Distance_Spheroid` |
| How far from A to B | the user's data, or Overture | `ST_Distance_Spheroid` |
| Which points fall in which polygons | the user's files | `ST_Contains` |
| Analyse a GeoJSON, Shapefile or GPX file | the user's file | `ST_Read`, measurements |
| Density or hotspots | the user's or Overture | H3 cells |
| Convert to GeoJSON or GeoPackage | the user's file | `COPY TO (FORMAT GDAL)` |
| Count buildings or roads in an area | Overture Maps | bbox filter, aggregates |

For real places with no file given, read `overture.md`. For function signatures, read `spatial-functions.md`.

## 2. Write and run the query

```bash
duckdb -agent -init .duckdb-skills/state.sql <<'SQL'
INSTALL spatial; LOAD spatial;
SET geometry_always_xy = true;
SETUP
QUERY;
SQL
```

- `SETUP` for Overture: `LOAD httpfs; CREATE SECRET (TYPE S3, PROVIDER config, REGION 'us-west-2');`. For H3:
  `INSTALL h3 FROM community; LOAD h3;`.
- `geometry_always_xy = true` makes every function read coordinates as longitude, latitude. Without it the
  spheroid functions take latitude first and the distances are wrong.
- Distances on the earth: `ST_Distance_Spheroid`, in metres. `ST_Distance` on longitude and latitude is in degrees
  and means nothing as a distance.
- The spheroid functions take `POINT_2D`, not `GEOMETRY`, and a `GEOMETRY('OGC:CRS84')` column does not cast.
  Rebuild the point: `ST_Point(ST_X(geometry), ST_Y(geometry))::POINT_2D`.
- A CSV with latitude and longitude columns: `ST_Point(longitude, latitude)`, longitude first.

## 3. Report

- Tables as they are; distances in km when large and m when small.
- Spatial results can be written for a map: `COPY (...) TO 'result.geojson' WITH (FORMAT GDAL, DRIVER 'GeoJSON');`.
- For density, describe where the dense cells are, and offer the GeoJSON.

## When it fails

- A missing extension: `INSTALL spatial; LOAD spatial;`, or `INSTALL h3 FROM community; LOAD h3;`.
- Nothing found in Overture: widen the bbox, check the category's spelling, or try a broader category.
- `INTERNAL Error: Vector::Reference used on vector of different type`: an `ORDER BY ... LIMIT` over the
  bbox-filtered Overture scan. Filter in a `MATERIALIZED` CTE first, as `overture.md` shows.
