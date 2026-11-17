# Spatial functions

```sql
INSTALL spatial; LOAD spatial;
SET geometry_always_xy = true;
INSTALL h3 FROM community; LOAD h3;
```

## Reading

| Format | Read with |
|---|---|
| GeoJSON, Shapefile, GeoPackage, FlatGeobuf, KML, GPX | `ST_Read('file')` (GDAL; 50 and more formats) |
| GeoParquet | `FROM 'file.parquet'` |
| OSM PBF | `ST_ReadOSM('region.osm.pbf')`, tags as a MAP column |
| CSV with latitude and longitude | `SELECT *, ST_Point(lng, lat) AS geom FROM 'file.csv'` |

## Writing

```sql
COPY (FROM ...) TO 'out.geojson' WITH (FORMAT GDAL, DRIVER 'GeoJSON');
COPY (FROM ...) TO 'out.gpkg' WITH (FORMAT GDAL, DRIVER 'GPKG');
COPY (FROM ...) TO 'out.shp' WITH (FORMAT GDAL, DRIVER 'ESRI Shapefile');
COPY (FROM ...) TO 'out.fgb' WITH (FORMAT GDAL, DRIVER 'FlatGeobuf');
```

## Functions

| Kind | Functions |
|---|---|
| Build | `ST_Point(x, y)`, `ST_MakeEnvelope(xmin, ymin, xmax, ymax)`, `ST_GeomFromText(wkt)`, `ST_GeomFromGeoJSON(text)`, `ST_MakeLine(points)` |
| Distance | `ST_Distance(a, b)` planar; `ST_Distance_Spheroid(a, b)` metres; `ST_DWithin(a, b, d)`; `ST_DWithin_Spheroid(a, b, metres)` |
| Relations | `ST_Contains`, `ST_Within`, `ST_Intersects`, `ST_Covers`, `ST_Disjoint`, `ST_Touches` |
| Measure | `ST_Area`, `ST_Area_Spheroid` (m²), `ST_Length`, `ST_Length_Spheroid` (m), `ST_Perimeter`, `ST_NPoints` |
| Transform | `ST_Transform(g, 'EPSG:from', 'EPSG:to')`, `ST_Centroid`, `ST_Buffer(g, d)`, `ST_Simplify(g, tol)`, `ST_ConvexHull`, `ST_Union(a, b)`, `ST_Intersection`, `ST_Difference`, `ST_FlipCoordinates` |
| Aggregate | `ST_Extent_Agg`, `ST_Union_Agg`, `ST_Collect(list)` |
| Read | `ST_X`, `ST_Y`, `ST_GeometryType`, `ST_AsText`, `ST_AsGeoJSON`, `ST_XMin`, `ST_XMax`, `ST_YMin`, `ST_YMax` |

Every `_Spheroid` function takes `POINT_2D` and not `GEOMETRY`:
`ST_Point(ST_X(geometry), ST_Y(geometry))::POINT_2D`.

## H3 cells

| Resolution | Edge | For |
|---|---|---|
| 3 | 59 km | regions |
| 5 | 8 km | cities |
| 7 | 1.2 km | neighbourhoods |
| 9 | 174 m | blocks |
| 11 | 25 m | buildings |

`h3_latlng_to_cell(lat, lng, res)` gives a `UBIGINT` cell, `h3_cell_to_latlng(cell)` a list `[lat, lng]`,
`h3_cell_to_boundary_wkt(cell)` the hexagon as WKT, and `h3_grid_disk(cell, k)` the cells within k rings. These
take latitude first.

```sql
SELECT h3_latlng_to_cell(lat, lng, 7) AS hex, count() AS n, h3_cell_to_boundary_wkt(hex) AS boundary
FROM points
GROUP BY hex
ORDER BY n DESC;
```

## Patterns

```sql
SELECT a.name, b.name, ST_Distance_Spheroid(a.geom, b.geom) AS dist_m
FROM places a, places b
WHERE a.name < b.name;

SELECT p.name, z.zone
FROM points p, zones z
WHERE ST_Contains(z.geom, p.geom);

SELECT a.name, b.name AS nearest, ST_Distance_Spheroid(a.geom, b.geom) AS dist_m
FROM places a
CROSS JOIN LATERAL (
    SELECT name, geom FROM targets b
    ORDER BY ST_Distance_Spheroid(a.geom, b.geom)
    LIMIT 3
) b;
```
