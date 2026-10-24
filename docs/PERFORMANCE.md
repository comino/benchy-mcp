# Performance — how ingest is measured, and what it costs now

benchy does one job: get time-series data into a sealed DuckDB workspace before the agent's first
call. Every change on that path is judged by the harness below, before and after, and the pull
request gives both numbers. This page is the current state. The measurements behind it, rounds 1
to 6 and every change measured since, are archived in
[history/PERFORMANCE-rounds.md](history/PERFORMANCE-rounds.md) (shortened to *rounds* below);
each line here links to its section there. Builds are named by short hash only where the archive
does, and most of those are labels from before the public history, not commits you can check
out. Everything was measured on DuckDB 1.5.5.

## The harness

```sh
python3 packaging/duckdb_lib.py fetch && cargo build --release
python3 bench/ingest.py ./target/release/benchy-mcp --runs 5 \
    --clickhouse /path/to/clickhouse --json before.json       # the numbers
python3 bench/ingest.py ./target/release/benchy-mcp --runs 1 --no-perf \
    --clickhouse /path/to/clickhouse --record scratch/rec      # where they go
python3 bench/compare.py before.json after.json               # a change, judged
```

[`bench/ingest.py`](../bench/ingest.py) loads one synthetic table (`ts` every minute, `meter`
of 500 series, `kw` a double) every way benchy takes data in: a folder (`csv-*`, `parquet-*`,
`ndjson-*`), an exec connector that `cat`s an Arrow stream (`exec-*`, `exec_series-*`, …), the
ClickHouse source (`ch-*`), HTTP (`http-*`) and the host protocol (`host*-*`), each at any size
(`-1k`, `-1m`, `-10m`). At 1,000 rows what is left is what every session pays.
[`bench/gen_data.py`](../bench/gen_data.py) writes the data once under `scratch/bench/`. The
scenario table: [rounds, The harness](history/PERFORMANCE-rounds.md#the-harness).

Each run starts benchy with `BENCHY_TRACE_TIMINGS=1`, sends one `describe` and ends the
session, after one uncounted warm-up. It reports user-space **instructions** of the process tree
from `perf stat` (the primary metric; it needs `kernel.perf_event_paranoid` ≤ 1, and falls back
to CPU time), CPU time and peak RSS per process, every traced stage, the wall time to the first
`describe`, and ClickHouse's own CPU.

`--record` adds `perf record` samples put into the stage running on their thread; build with
`CARGO_PROFILE_RELEASE_STRIP=false` first, or benchy's code is one unnamed lump. Tracing goes to
the operator's log only (`timings_reach_the_operator_log_from_every_process_and_never_the_agent`)
and costs one relaxed atomic load when off (`without_the_variable_nothing_is_traced`).

For a single call rather than a load, measure the whole session with `perf stat -e
instructions:u` over N calls and subtract a session with none, as the query-path figures below
were.

### Reading the numbers on a shared machine

Measured on a 4-core Arm Neoverse-N1 VM with 7.5 GB, shared with other work (load 1 to 20):

- **Wall time is noise.** A stage's wall time was often five times its CPU time. Compare cold
  starts only on a quiet machine, or with the old and new binary interleaved run by run.
- **Take the minimum of N runs**; the harness prints the median and the spread beside it.
- **Instructions are the steadiest metric, not a perfect one.** A stream's spread 0–4% over five
  runs; a file read on every core 5–26%, because idle DuckDB workers spin. A change to a file
  path has to beat that spread, or be judged on its stage's count from `--record`.

## The current baseline

Round 6's baseline is the last full one: every scenario, interleaved against round 5's binary,
minima of five runs, load average 1.2 to 2.9, 2026-10-14 (build label `b660bcd`). Changes since
were each measured against their parent and moved these numbers by less than their spread,
except where the decisions below say otherwise.

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| ch-1m | 2.17 G | 526 ms | 64 MB | 531 / 586 ms |
| ch-10m | 17.28 G | 2.85 s | 94 MB | 2.65 / 2.74 s |
| exec-1m | 2.04 G | 533 ms | 60 MB | 603 / 610 ms |
| exec-10m | 16.32 G | 3.23 s | 103 MB | 2.95 / 3.61 s |
| csv-1m | 5.43 G | 1.01 s | 115 MB | 727 / 790 ms |
| csv-10m | 49.11 G | 8.13 s | 388 MB | 4.41 / 4.56 s |
| ndjson-1m | 5.55 G | 1.16 s | 142 MB | 748 / 758 ms |
| ndjson-10m | 50.81 G | 8.78 s | 351 MB | 4.41 / 4.77 s |
| parquet-1m | 2.46 G | 582 ms | 62 MB | 523 / 564 ms |
| parquet-10m | 20.26 G | 3.46 s | 112 MB | 2.73 / 3.31 s |
| http-json-1m | 5.65 G | 1.21 s | 69 MB | 755 / 788 ms |
| hostspool-10m | 16.38 G | 2.79 s | 104 MB | 2.82 / 3.04 s |
| hoststream-10m | 16.36 G | 3.05 s | 102 MB | 3.23 / 3.46 s |
| hostchannel-10m | 16.36 G | 3.02 s | 105 MB | 3.13 / 3.23 s |

The engine peaked at about 45 MB and the server at 18 MB in every scenario
([rounds, Baseline: round 6](history/PERFORMANCE-rounds.md#baseline-round-6-b660bcd)). Measured
since: exec_series-10m 18.6 G, exec_mixed-1m 2.30 G, parquet_bymeter-10m 23.16 G and 2.56 s.

## The costs a user feels

- **Time to the first `describe`.** 1,000 rows: 250–385 ms (exec-1k 250 / 262 ms, csv-1k
  363 / 384 ms, host-1k 256 / 264 ms, ch-1k 234 / 249 ms). 1M rows: about 0.5–0.8 s. 10M rows:
  2.7–3.6 s from a stream or Parquet, 4.4–4.8 s from CSV or NDJSON. The 10M-row path is mostly
  DuckDB's write on one thread (80% of ch-10m's; see R4-1 below).
- **What every session pays.** 420–970 M instructions at 1,000 rows: process starts (about 21 M
  and 10 ms each), DuckDB's opens, a checkpoint, and the engine's attach check, of which listing
  DuckDB's functions is about 115 M and 28–31 ms. The engine's start overlaps the first load.
  [rounds, Every session's fixed part](history/PERFORMANCE-rounds.md#every-sessions-fixed-part-since-round-5).
- **Memory.** A stream's loader holds 95–105 MB at 10M rows whatever the source; a CSV or JSON
  file's stage is held to 256 MiB and spills past it (csv-10m 384–388 MB, ndjson-10m 351 MB);
  an HTTP CSV or NDJSON body is spooled up to the source's `max_bytes`. Every loader is killed
  past `--loader-memory-ceiling`, 1 GB by default. Capacity per machine: [CAPACITY.md](CAPACITY.md).
- **A query.** A `SELECT` costs about 1.2–1.4 M instructions over the session; one with no
  `LIMIT` is streamed and read only as far as its kept rows (`SELECT *` of 10M rows 23 M and
  7–18 ms, the engine 51–60 MiB, where building it whole was 0.63 G and 402–424 MiB). A `CREATE
  TEMP TABLE` or `VIEW` pays about 10 M for the no-shadowing and ENUM checks, a `CREATE MACRO`
  about 125–139 M and 30 ms (below), the first of an engine 185 M with the parser's names (282 M
  before #63 read DuckDB's own names in the same scan).
- **Helpers.** None: nothing. The three example helpers: about 420 M and 200 ms of startup
  (a session 662 M → 1,080 M, first `describe` 377 → 580 ms); a helper call costs what its SQL
  written out does (`dewpoint(20, 50)` 5.9 M, inline 6.0 M).
- **Engine threads.** One by default. `--threads 4` took an hourly mean over 10M rows from
  0.766 s to 0.248 s and a per-meter percentile from 0.649 s to 0.213 s, for 12–19% more
  instructions and up to 70 MiB more engine memory.

## Standing decisions, each with its measurement

| decision | why, measured | detail |
|---|---|---|
| The write stays on one thread (R4-1) | A parallel write is about three times as fast at 10M rows, but DuckDB 1.5.5 numbers blocks by the thread that wrote them, so `block_id` in `pragma_storage_info` changed from load to load of the same file; REQ-15 needs one layout from every origin | [R4-1](history/PERFORMANCE-rounds.md#r4-1-the-write-runs-on-one-thread--82-of-a-10m-row-streams-path-a-third-to-a-half-of-a-files-measured-not-done-it-would-break-req-15) |
| Every JSON file is staged, whatever its size (R4-2) | ndjson-10m's first `describe` 6.67 → 4.59 s (−31%), for +21% instructions and a loader peak of 132 → 351 MB, bounded by the stage's cap | [R4-2](history/PERFORMANCE-rounds.md#r4-2-a-json-file-over-64-mib-is-read-straight-in-on-one-thread--66-s-for-10m-rows-done-every-json-file-staged) |
| A stage is held to 256 MiB and spills | csv-10m's loader 512 → 384 MB, 139 MB spilled; the peak does not grow with the file | [Memory and disk, checked again](history/PERFORMANCE-rounds.md#memory-and-disk-checked-again) |
| A wide file is read on one thread (R6-W) | Four threads each hold a 256 KiB block of every column; a 300-column CSV ran out under the cap on four threads and took its loader to 2.1–2.3 GB staged uncapped; on one thread 338–343 MB. Narrow loads unchanged within 0.3% | [R6-W](history/PERFORMANCE-rounds.md#r6-w-wide-rows-on-one-thread) |
| Loaders have a memory ceiling, 1 GB by default (R6-L1) | 2.4 times the largest bench peak; the watch reads `statm` (5 µs) every 50 ms, about 1.5 ms of server CPU per second of load; ingest instructions within the noise | [R6-L1](history/PERFORMANCE-rounds.md#r6-l1-a-memory-bound-for-loaders) |
| `CREATE MACRO` costs about 30 ms (R4-4) | `duckdb_functions()` is the only listing of a catalog's macros and builds every built-in's row first, about 115–125 M; no cheaper probe was found that keeps the attach check strict. DuckDB's own names come from that same scan, once per engine: the first `CREATE MACRO` 282 → 185 M (#63) | [The query path: no-shadowing](history/PERFORMANCE-rounds.md#the-query-path-no-shadowing), [R4-4](history/PERFORMANCE-rounds.md#r4-4-the-engines-attach-check-lists-every-function--115-m-and-2831-ms-of-every-session-r3-7-carried), [#63](history/PERFORMANCE-rounds.md#duckdbs-built-ins-read-once-per-engine-in-the-agents-scan-63) |
| No-shadowing is checked after a `CREATE`, not before | The C API does not give the name a statement creates; checking before would need a second SQL parser, the thing escaped spellings aim at. A SELECT pays one comparison | [The query path: no-shadowing](history/PERFORMANCE-rounds.md#the-query-path-no-shadowing) |
| No check of the agent's SQL for a helper's CTE (#58) | Helper macros may not read tables, so the check is gone: about 0.6 M a call off a batch that had a WITH and called a helper; the macro check reads a column the helpers job already computes | [No check of the agent's SQL for a helper's CTE](history/PERFORMANCE-rounds.md#no-check-of-the-agents-sql-for-a-helpers-cte) |
| A `SELECT` is streamed and read only as far as it is kept | 0.63 G → 23 M and 402–424 → 51–60 MiB for an unlimited `SELECT *` of 10M rows; reading on to the end to surface late errors was built and not taken (0.88 G) | [A query read only as far as it is kept](history/PERFORMANCE-rounds.md#a-query-read-only-as-far-as-it-is-kept) |
| A hint's columns are the attach's, or the catalog's, kept (#27) | A loaded table's cost nothing measurable (3.7–3.9 M a failing call before and after). A view's are `duckdb_columns()`'s, never a bind, which ran the view's table-function arguments outside the deadline; read once and kept, a call that lists a helper view 6.5 → 4.2–4.5 M, one of the agent's views 4.8 → 4.3 M; read anew after a `CREATE`, 21.4 → 28.1 M. The agent's are read on its own connection, the only one that sees its temporary catalog, at the same cost (4.4 M a call that lists its view) | [The columns in a hint](history/PERFORMANCE-rounds.md#the-columns-in-an-unknown-columns-or-tables-hint-27) |
| The empty-result note reads at most 2,048 values a column first | 11 G → 0.1 G a call over 3M rows of unique text, and it stays inside the deadline | [The empty-result note](history/PERFORMANCE-rounds.md#the-empty-result-note) |
| Engine threads are opt-in (#56) | Four threads are about three times as fast on a free machine but cost 12–19% more instructions; on a busy one they buy nothing, so one is the default. Workers start after the seal confines | [Engine threads](history/PERFORMANCE-rounds.md#engine-threads-56) |
| Rows sorted by series are tallied by their key (#54) | parquet_bymeter-10m 27.46 → 23.16 G (−15.7%), first `describe` 3.54 → 2.56 s; the tallies wait at most 1M rows, so a one-series table pays +1.3% rather than +8.5% | [Rows sorted by series](history/PERFORMANCE-rounds.md#rows-sorted-by-series-tallied-by-their-key-54) |
| Each series is judged at its own step; a value held is noted (#50, #51) | exec_mixed-1m 7.09 → 2.30 G (−67.5%); about 24 instructions a row elsewhere (+1.3% at exec_series-10m). The ten-times-average rule kept all 74 frozen readings in the public meter data | [Each series at its own step](history/PERFORMANCE-rounds.md#each-series-at-its-own-step-and-a-value-held-what-it-costs) |
| The profile reads microseconds (#15) | Within ±1% everywhere, once the time hash is rotated: unrotated FxHash put a minutely table's times in one bucket in 256 (+39 M at exec-1m) | [The profile in microseconds](history/PERFORMANCE-rounds.md#the-profile-in-microseconds-15-nothing-measurable-once-times-hash-apart) |
| Infinite timestamps are kept (`coalesce` over the fast path) | 21 instructions a row more (35 → 56); `timezone('UTC', ts)` would cost 1,509 a row | [Infinite timestamps](history/PERFORMANCE-rounds.md#infinite-timestamps-kept-on-load-21-instructions-a-row) |
| An NDJSON infinity is refused, checked on the table written | about 1.4–1.8 ms per sniffed JSON load, not growing with the file; a byte search would read the whole file every load | [An NDJSON infinity refused](history/PERFORMANCE-rounds.md#an-ndjson-infinity-refused-about-15-ms-of-a-sniffed-json-load) |
| The profile's claims are exact | The integer key no longer mistaken: 36.5 → 5.9–6.35 G on 500 integer device ids; `profile.settle` 334 ms only for rows in no order | [The profile's claims, made true](history/PERFORMANCE-rounds.md#the-profiles-claims-made-true-what-it-costs) |
| Helpers are made in a job of their own | A session without helpers pays nothing; reading the built-ins before making them was a second listing, 88 M, not done | [Helpers](history/PERFORMANCE-rounds.md#helpers) |
| A host attaches tables in one request, and streams (R2-5, R5-H1, R5-H2) | The attach check once a request: 280 M and 120 ms a table → 162 M and 52 ms; a descriptor or the stream channel needs no spool, and the channel costs what a streamed attach does, within 0.1% | [R2-5](history/PERFORMANCE-rounds.md#r2-5-a-hosts-attach--280-m-instructions-and-about-120-ms-per-table-done-162-m-and-52-ms), [R5-H1](history/PERFORMANCE-rounds.md#r5-h1-a-hosts-attach-took-a-file-every-load-spooled-first--done-streamed-through-a-pipe), [R5-H2](history/PERFORMANCE-rounds.md#r5-h2-a-host-that-cannot-pass-a-descriptor-could-not-stream--done-a-stream-channel) |
| ClickHouse's `compression` unset is none on loopback, zstd elsewhere; zstd decoded by C libzstd (round 1, item 3) | On loopback zstd costs ClickHouse's CPU to compress for nothing; libzstd 1.64 G against ruzstd's 5.99 G for a 10M-row response | [item 3](history/PERFORMANCE-rounds.md#3-zstd-decoding-in-pure-rust--done-44-g-off-a-10m-row-zstd-load-59-g-off-the-default) |
| The warden is forked, not exec'd (R3-11) | −18 to −26 M a session; a process start costs about 21 M and 10 ms | [R3-11](history/PERFORMANCE-rounds.md#r3-11-starting-processes--21-m-instructions-each-84-m-a-session-r2-10-carried-done-for-the-warden-18-to-26-m) |
| No LTO, no `target-cpu=native`, no `panic = "abort"` | Fat LTO −2% on the ClickHouse fetch, inside the spread, for a 4 → 26 min build; more than 85% of a load is DuckDB's prebuilt library; `src/ipc.rs` catches arrow's panic on a hostile schema | [Build profile](history/PERFORMANCE-rounds.md#build-profile) |

What is left on the path is DuckDB's own work per row: statistics, compression analysis and the
write on one thread. Taking more would need DuckDB to change: blocks numbered in row-group order
however many threads write them, and a way to list one catalog's functions
([rounds, Is another round worth it?](history/PERFORMANCE-rounds.md#is-another-round-worth-it)).

On aarch64 there is no LBR and neither benchy nor DuckDB's library unwinds cheaply, so profiles
are flat, split by the trace's stages; for call graphs use `valgrind --tool=callgrind` on one
loader, or `-C force-frame-pointers=yes` with `perf record --call-graph=fp`.
