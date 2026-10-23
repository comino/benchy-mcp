# An excerpt of the eval's public meter data

`readings.parquet` is seven of the 24 meters `eval/fetch_uci.py` cuts, every reading of 2014,
columns `ts,meter,kw` as there: 245,280 rows, 379 KB. Four carry the quirks real meter data
has, none of them planted, and three read all year (`the_public_meter_data_shows_its_quirks`,
tests/shape.rs):

| meter | why it is here |
|---|---|
| MT_006, MT_008, MT_018 | read all year, as most meters do; the largest three, so that the totals across meters are as steady as across all 24, and the clock changes are the only moments every meter shares |
| MT_106 | reads 0 until 2014-01-14 |
| MT_109 | reads 0 until 2014-02-18 |
| MT_223 | reads 0 all year |
| MT_347 | starts on 2014-02-28, pauses for weeks, and stops on 2014-12-18 |

Every meter reads 0 for the hour lost in March, and holds two hours in one in October.

It was cut from `eval/uci/readings.csv` (SHA-256
`fa23f39e3cf1533ec878e2394d7e74f7f3295051e909d49a839c66d6fdcb588e`, as `fetch_uci.py` checks)
with the DuckDB 1.5.5 CLI:

```sql
COPY (SELECT * FROM read_csv('eval/uci/readings.csv')
      WHERE meter IN ('MT_006', 'MT_008', 'MT_018', 'MT_106', 'MT_109', 'MT_223', 'MT_347')
      ORDER BY meter, ts)
  TO 'tests/fixtures/uci/readings.parquet' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 19);
```

## Licence

Data: Artur Trindade, *ElectricityLoadDiagrams20112014*, UCI Machine Learning Repository (2015),
https://doi.org/10.24432/C58C86, licensed CC BY 4.0
(https://creativecommons.org/licenses/by/4.0/). Modified: 7 of the 370 clients, 2014 only, one
row per meter and interval, decimal commas written as points, stored as Parquet.
