# Capacity — measured, not claimed

REQ-7: capacity is measured, not claimed. **No capacity claim belongs in any benchy
document that is not output from this file.** Regenerate it on your own hardware — these
numbers describe the machine below and nothing else:

```sh
python3 examples/capacity.py ./target/release/benchy-mcp --out docs/CAPACITY.md
```

- **Machine** — Linux aarch64, 4 cores, 7.5 GB RAM
- **Measured** — 2026-10-14, on `b660bcd` (round 6's baseline, docs/PERFORMANCE.md), the
  load-time profile (REQ-14) included; load average 1.2 to 1.7. Run interleaved in the same
  minutes with `69a43a3`, round 5's baseline, twice each: `b660bcd` read 0.95 and 0.93 s at
  1,000,000 rows and 3.35 and 3.32 s, 463 and 474 MB at 5,000,000, where `69a43a3` read 1.03 and
  1.07 s, and 3.62 and 3.28 s, 480 and 469 MB; the table is `b660bcd`'s first run. The table
  before it, on `5993d6b` (2026-10-12), read 0.88 s and 3.52 s, 466 MB.
- **Shape** — four columns: an ISO timestamp, a 500-value string key, a double, a short string.
- **Defaults** — `--max-rows 200`, `--max-bytes 8192`, `--deadline 20`, `--memory 1GB`.

| rows | csv on disk | load | load rate | `count(*)` | `GROUP BY` | window | peak RSS |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 100,000 | 4 MB | 0.50 s | 0.20 M rows/s | 2 ms | 16 ms | 13 ms | 123 MB |
| 1,000,000 | 43 MB | 0.95 s | 1.05 M rows/s | 2 ms | 20 ms | 24 ms | 200 MB |
| 5,000,000 | 217 MB | 3.35 s | 1.49 M rows/s | 2 ms | 66 ms | 92 ms | 463 MB |

## Reading this

**Load** is the cost you pay once, before the agent's first call: a loader process parses
the CSV, infers its types and writes a DuckDB file, which the engine then attaches. It is the
only number that grows with your data, and it is what the agent never has to spend a step on.
The parse runs on every core and the write into the file on one, so the same rows make the
same file, block for block, whatever they came from (REQ-15; a parallel write was measured and
left out, docs/PERFORMANCE.md R4-1).

**Query times are the full MCP round trip** — JSON in, plan, execute, cap, JSON out. `count(*)`
stays flat because DuckDB answers it from metadata. The two that do real work scale with the
data and stay small. What does *not* grow is the answer: capped at 200 rows either way. That
is the product — the reason to put data on a bench instead of into a context window.

**Peak RSS is the whole session's** — the server, its engine and the loader that parsed the
file — sampled every 20 ms: the loader has exited by the time the queries run, and its moment
is often the highest. It tracks the data rather than the configured memory budget, so size
the OS ceiling from this column
and not from `--memory`. It is also the noisiest column here: repeated runs of the same
workload differ by a fifth or more, because the allocator retains pages. Size from the top
of a few runs, not from one.

## What the first run of this found

History, not a measurement of this run. The first version of this table reported **0.022 s
for `count(*)` at 100k rows, at 1M and at 5M**. A number that does not move with the work is
not measuring the work. The deadline
watchdog polled in 20 ms sleeps and was joined at the end of every batch, so every query —
`SELECT 1` included — waited out the rest of a tick. Replacing the poll with a condvar took a
trivial query from 22.2 ms to 1.9 ms, on every call an agent makes.

That is what a benchmark is for. It was not written to find that, and the bug had been
shipping happily through a green test suite.

## What this does not measure

- **Parquet**, which loads without the parse and the type inference, so it will be faster.
- **Concurrency.** One bench per process by design (REQ-1). `neighbour_unaffected` shows a
  runaway neighbour leaves this bench correct and responsive; the CPU is still shared.
- **The ceiling.** Every size here fits; none of them found the edge. The honest statement is
  that 5,000,000 rows of this shape cost 463 MB and 3.3 s, not that
  5,000,000 rows is a limit.

## A ClickHouse table, six ways

The same rows from a ClickHouse server, put on the bench by the built-in source (REQ-16) with
each `compression`, by an exec connector that pipes `curl`'s `ArrowStream` through, and from CSV
and Parquet exports of them. Output of

```sh
python3 examples/capacity_clickhouse.py ./target/release/benchy-mcp \
    --url http://127.0.0.1:8123 --meters eval/uci/readings.csv --rows 10000000 --runs 3
```

- **Machine** — the one above, with ClickHouse 26.8 on the same machine (so the network is
  loopback), and other work on it: load average 2.6 to 4.5 on 4 cores. Three runs of each path,
  the median of each figure.
- **Measured** — 2026-10-12, on `5993d6b`: batches written as they arrive, zstd decoded by
  libzstd, the profile kept as a stream's rows go in on a thread of its own and a file's beside
  its write, each row group written as it fills, the loader's file in storage version v1.3.0, a
  Parquet file read straight into its table, a CSV's stage held to 256 MiB. The table this one
  replaces (on `35ad7db`) was measured at load average 5 to 7, so the times here are lower by
  more than the code took off; the CPU is the fairer comparison: 6.2 to 6.5 s then, 4.6 to 5.4 s
  now, at 10,000,000 rows of a stream or a Parquet file.
- **Tables** — the eval's public meter data (`DateTime`, `LowCardinality(String)`, `Float32`),
  and 10,000,000 rows of `examples/capacity.py`'s shape made by ClickHouse with `numbers()`.
- **Time** is from starting benchy to the answer to the first `describe`: fetch, load, profile,
  seal. **CPU** is user and system time of the server and every process it reaped — the
  loader, a connector and `curl`, the engine — as the kernel counted it. **Peak RSS, session**
  is sampled as above; **largest process** is the kernel's high-water mark for the one process
  that reached the most, at load the loader.

| table | rows | path | start → first `describe` | CPU | peak RSS, session | peak RSS, largest process | bytes moved |
|---|---:|---|---:|---:|---:|---:|---:|
| meters | 840,960 | built-in, `compression = "zstd"` | 0.65 s | 0.62 s | 131 MB | 68 MB | 2.1 MB on the wire |
| meters | 840,960 | built-in, `compression = "lz4"` | 0.58 s | 0.65 s | 136 MB | 72 MB | 10.8 MB on the wire |
| meters | 840,960 | built-in, `compression = "none"` | 0.56 s | 0.61 s | 128 MB | 66 MB | 21.9 MB on the wire |
| meters | 840,960 | exec connector (`curl`) | 0.59 s | 0.65 s | 137 MB | 65 MB | 21.9 MB on the wire |
| meters | 840,960 | CSV file | 0.90 s | 1.16 s | 170 MB | 108 MB | 34.6 MB on disk |
| meters | 840,960 | Parquet file | 0.59 s | 0.68 s | 133 MB | 72 MB | 1.1 MB on disk |
| synthetic | 10,000,000 | built-in, `compression = "zstd"` | 5.11 s | 5.17 s | 184 MB | 120 MB | 63.6 MB on the wire |
| synthetic | 10,000,000 | built-in, `compression = "lz4"` | 5.04 s | 5.25 s | 190 MB | 127 MB | 164.5 MB on the wire |
| synthetic | 10,000,000 | built-in, `compression = "none"` | 4.39 s | 4.58 s | 180 MB | 117 MB | 372.4 MB on the wire |
| synthetic | 10,000,000 | exec connector (`curl`) | 5.07 s | 5.35 s | 189 MB | 117 MB | 372.4 MB on the wire |
| synthetic | 10,000,000 | CSV file | 6.84 s | 11.97 s | 545 MB | 484 MB | 474.9 MB on disk |
| synthetic | 10,000,000 | Parquet file | 4.16 s | 5.17 s | 181 MB | 120 MB | 47.5 MB on disk |

**Reading it.** A stream, from ClickHouse or a connector, and a Parquet file are the cheapest
ways onto the bench: 4.6 to 5.4 s of CPU at 10,000,000 rows, with loaders of the same size; a CSV
file costs 12.0 s and a loader four times the size: 484 MB, where it was 715 MB (11.3 s) with its
stage held in memory whole. zstd moves 5.9
times fewer bytes than none on the synthetic table and 10 times fewer on the meter data.
Decoding it (C libzstd) cost benchy 5.17 against 4.58 s of CPU here, where on `35ad7db` the order
was the other way (6.17 against 6.48 s): the difference is inside what three runs on a shared
machine resolve. What zstd costs is ClickHouse's CPU to compress (docs/PERFORMANCE.md, round 1 item 3), which is why an unset
`compression` is `none` for a server on loopback and zstd for any other. The table above sets
`compression` on every built-in row.

**Memory.** Each batch of a stream goes into its table as it arrives, and the loader never
holds the stream whole: its peak grows slowly with the rows, 65 to 72 MB at 840,960 rows and 117
to 127 MB at 10,000,000, which is DuckDB's own buffers (11–16 MB less than on `ef3a0cf`, since each
row group is written as it fills, R2-8). Until 2026-10-11 a stream was held as a
list of Arrow batches until it ended, then copied into an in-memory staging table, then written.
Measured the same way on the same machine, the 10,000,000-row built-in loads then took 963 to
1,022 MB for the session, 940 to 953 MB in the loader, and 10.6 to 12.6 s of CPU; the meter data
186 to 196 MB and 0.93 to 1.07 s (`a_large_stream_is_not_held_in_memory_while_it_loads`). A CSV
file is still read by DuckDB's own reader into a staging table and written from it, and so is a
JSON file, of any size since R4-2 (up to 64 MiB before, R2-1); since 2026-10-12 that stage is held to 256 MiB of memory and
spills past it to the loader's staging directory, so the loader's peak no longer grows with the
file past the cap (`a_large_csv_is_not_held_in_memory_while_it_is_staged`), for about a tenth
more CPU where it spills (4.5% more instructions at the harness's 10M-row CSV,
docs/PERFORMANCE.md), and the harness's 10M-row CSV now peaks at 384 MB in the loader where it
took 512 MB; a 10M-row NDJSON file, 351 MB, spilling 134 MB, where read straight in it took
132 MB and 2 s longer to its first `describe` (docs/PERFORMANCE.md R4-2). A Parquet file is read
straight into its table, without the stage, and its loader peaked at 584 MB at 10,000,000 rows before that
(docs/PERFORMANCE.md, round 1 item 4). A paged JSON source holds at most four pages between its
fetch and its table, besides the one each side has (R4-3): its loader peaked at 69 MB at
1,000,000 rows of 10,000-row pages, 63 MB when it read a page at a time.
