# Overture Maps

Free global GeoParquet on S3: places, buildings, roads, boundaries, addresses. No key, no download.

## The release

Releases come about monthly and old ones are removed, so look up the current one each session:

```bash
duckdb -agent -init .duckdb-skills/state.sql -c "LOAD httpfs; SELECT latest FROM read_json('https://stac.overturemaps.org/catalog.json');"
```

The base path is `s3://overturemaps-us-west-2/release/RELEASE/`, and every query starts with:

```sql
INSTALL spatial; LOAD spatial; LOAD httpfs;
SET geometry_always_xy = true;
CREATE SECRET (TYPE S3, PROVIDER config, REGION 'us-west-2');
```

| Data | Path after the base |
|---|---|
| Places | `theme=places/type=place/*` |
| Buildings | `theme=buildings/type=building/*` |
| Roads, rail, water ways | `theme=transportation/type=segment/*` |
| Divisions (a point each) | `theme=divisions/type=division/*` |
| Division boundaries | `theme=divisions/type=division_area/*` |
| Addresses | `theme=addresses/type=address/*` |
| Land, water, land use | `theme=base/type=land/*` and its siblings |

## Filter on bbox first

The files are laid out by bounding box. A filter on `bbox.xmin`, `bbox.xmax`, `bbox.ymin` and `bbox.ymax` reads only
the parts it needs; without one, a query reads the whole world.

In DuckDB 2.0, `ORDER BY ... LIMIT` straight over a bbox-filtered scan fails with `INTERNAL Error: Vector::Reference
used on vector of different type`, whatever it orders by. A plain `LIMIT`, a count or other aggregates work.
For the nearest or the top N, filter in a `MATERIALIZED` CTE, then order:

```sql
WITH p AS MATERIALIZED (
    SELECT names.primary AS name, basic_category, geometry
    FROM read_parquet('s3://overturemaps-us-west-2/release/RELEASE/theme=places/type=place/*')
    WHERE bbox.xmin BETWEEN -73.99 AND -73.98
      AND bbox.ymin BETWEEN 40.745 AND 40.75
      AND basic_category = 'coffee_shop'
)
SELECT name,
       round(ST_Distance_Spheroid(ST_Point(ST_X(geometry), ST_Y(geometry))::POINT_2D,
                                  ST_Point(-73.985, 40.748)::POINT_2D)) AS dist_m
FROM p
ORDER BY dist_m
LIMIT 10;
```

A count needs no CTE:

```sql
SELECT count() AS buildings
FROM read_parquet('s3://overturemaps-us-west-2/release/RELEASE/theme=buildings/type=building/*')
WHERE bbox.xmin BETWEEN -122.75 AND -122.55
  AND bbox.ymin BETWEEN 45.45 AND 45.60;
```

## Columns

The schema changes between releases. When a column is not found, `DESCRIBE FROM read_parquet('PATH')` shows the
current one. Every type has `id`, `geometry` (`GEOMETRY('OGC:CRS84')`), `bbox`, `names.primary`, `sources` and
`version`.

| Type | Columns |
|---|---|
| place | `basic_category` (such as `coffee_shop`, `restaurant`, `hospital`), `taxonomy.primary`, `taxonomy.hierarchy`, `taxonomy.alternates`, `confidence` (0 to 1), `operating_status`, `addresses[1].freeform`, `.locality`, `.postcode`, `.region`, `.country`, `websites`, `phones`, `emails`, `socials`, `brand.names.primary` |
| building | `subtype`, `class`, `height`, `min_height`, `num_floors`, `is_underground`, `roof_shape`, `roof_material`, `facade_material`, `has_parts` |
| segment | `subtype` (road, rail, water), `class` (motorway, primary, residential, ...), `subclass`, `speed_limits`, `road_surface`, `access_restrictions`, `routes`, `connectors` |
| division | `subtype` (country, region, county, locality, ...), `class`, `admin_level`, `country`, `region`, `population`, `parent_division_id`, `hierarchies`, `wikidata` |
| division_area | `division_id`, `subtype`, `admin_level`, `country`, `region`, `is_land`, `is_territorial` |

Older releases had `categories.primary` for places; it is now `basic_category` and `taxonomy`.
