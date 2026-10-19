# Performance — the ingest baseline, and where the time goes

benchy does one job: get time-series data into a sealed DuckDB workspace before the agent's first
call. This file is how that job is measured, what it costs on `main` today, and the ranked list of
what to make cheaper. Every optimisation is judged by the harness below, before and after.

The short commit hashes below (`b660bcd`, `22ff87e`, …) name builds from before the public
history began, which was squashed into its first commit; they do not resolve in this repository.
They are kept as labels that tell apart the builds one measurement compared, all of them on
DuckDB 1.5.5.

## The harness

```sh
cargo build --release
python3 bench/ingest.py ./target/release/benchy-mcp --runs 5 \
    --clickhouse /path/to/clickhouse --json before.json       # the numbers
python3 bench/ingest.py ./target/release/benchy-mcp --runs 1 --no-perf \
    --clickhouse /path/to/clickhouse --record scratch/rec      # where they go
python3 bench/compare.py before.json after.json               # a change, judged
```

[`bench/ingest.py`](../bench/ingest.py) loads one synthetic table — `ts` (a timestamp every
minute), `meter` (500 series), `kw` (a double) — every way benchy can take it in:

| scenario | how the rows arrive |
|---|---|
| `csv-1m`, `parquet-1m`, `ndjson-1m` | a folder, `benchy-mcp <dir>`: DuckDB's own readers |
| `exec-1m`, `exec-10m` | an exec connector that `cat`s an Arrow IPC stream (64 Ki-row batches), so the connector costs next to nothing and the time is benchy's |
| `exec_series-1m`, `exec_series-10m` | the same, with all 500 meters reading every minute (`ts` advances every 500 rows), so the times repeat and the profile cuts the table into 500 series; in every other scenario each time appears once, and the table is one series |
| `exec_intkey-1m`, `exec_intkey-10m` | exec_series' rows with `meter` an `int64` (1000 + i % 500), so the key the profile tallies is an integer (R3-10) |
| `exec_lz4-1m`, `exec_lz4-10m` | exec's rows with each batch's buffers LZ4-compressed, as ClickHouse's `ArrowStream` sends them by default (R2-6) |
| `ch-1m`, `ch-10m` | the built-in ClickHouse source, its `compression` unset, against a server the script starts on port 8133 (`--ch-port`) with its data under `scratch/`, and stops by its PID. The server is on loopback, so that is `none` since round 1's item 3, and was `zstd` for round 1's baseline |
| `ch_zstd-10m`, `ch_lz4-10m`, `ch_none-10m` (any size) | the same, with `compression` set |
| `host-1m` | `benchy-mcp host`, the host attaching the same Arrow stream |
| `host2-1k`, `host8-1k`, … (any N, any size) | the same stream attached as N tables, in one request where the binary speaks host protocol 2, one request each where it speaks only 1 (R2-5) |
| `hostspool-1m`, `hoststream-1m` (any size) | a host that fetches the stream itself — read from the data file 1 MiB at a time, paced to `--host-fetch-mbps` when given — and spools it to a file in `/dev/shm` (tmpfs, which is memory, as a container's disk often is) before attaching the file; or writes it into a pipe it passes with a streamed attach (host protocol 3). Timed from the spawn, the fetch included; `host.spool_mb` is what was spooled (R5-H1) |
| `hostchannel-1m` (any size) | the same fetch written into the session's stream channel, a pipe benchy inherits (`--stream-channel`, host protocol 5), with plain pipes for the protocol (R5-H2) |
| `csv-1k`, `exec-1k`, `host-1k`, … | any of the above at 1,000 rows, not run by default: next to nothing to load, so what is left is what every session pays before its first `describe` |

[`bench/gen_data.py`](../bench/gen_data.py) writes the data once (pyarrow, through `uv`), under
`scratch/bench/`. Each run starts benchy with `BENCHY_TRACE_TIMINGS=1`, sends one `describe`,
and ends the session. A warm-up run before each scenario fills the page cache and is not counted.

**What is reported, per run:**

- **instructions and cycles** (user space) of the whole process tree, from `perf stat`. The
  primary metric. It needs `perf` and `kernel.perf_event_paranoid` ≤ 1; without them the harness
  says so and reports CPU time only.
- **CPU time** (user + system) and **peak RSS** (`ru_maxrss`, the same as `VmHWM`) for each
  process — server, engine, loader, and the connector the loader reaped — from each process's own
  `getrusage`, written to the trace as it exits (or, for the engine, after each request).
- **stages**: every span the binary traces (`src/trace.rs`), with its wall time and the CPU it cost
  the process, all threads — except a stream's `stream.insert` and `stream.watch`, and a file's
  `write_table` and `file.watch` (R3-2), which run side by side on a thread each and report their
  own thread's CPU (`trace::span_thread`); process starts, from the parent's `spawn` mark to the
  child's `main`;
  bytes fetched; blocks written.
- **cold start**: from spawning benchy to the answer to the first `describe`. Wall time.
- **ClickHouse's own CPU**, from `/proc` for the server's process tree.

`--record` runs each scenario once more under `perf record -k CLOCK_MONOTONIC`, one sample per
1,000,000 user-space instructions. The trace is on the same clock, and each line carries its
thread, so each sample is put in the stage that was running on its thread when it was taken (on
the process's main thread, for a thread that traces no stages of its own, such as DuckDB's
workers): `scratch/rec/<scenario>.txt` lists
the instructions by process, by stage, by library, and the top symbols of every stage that costs
a tenth of the run. Build with `CARGO_PROFILE_RELEASE_STRIP=false cargo build --release` first,
or benchy's own code shows as one unnamed lump.

**The binary's side.** `BENCHY_TRACE_TIMINGS=1` turns on `src/trace.rs` in the server, which
passes `--trace-timings` to the engine and each loader (they inherit no environment). Every line
goes to stderr, the operator's log, as `benchy-trace {json}`; no reply carries it
(`timings_reach_the_operator_log_from_every_process_and_never_the_agent`). Unset, nothing is
written (`without_the_variable_nothing_is_traced`) and a span costs one relaxed atomic load.

### Reading the numbers on a shared machine

Every number here was measured on a 4-core Arm Neoverse-N1 virtual machine with 7.5 GB of
memory, shared with other work: load average 10 to 20 during round 1's baseline, and over 100 at
times; 1.3 to 7 during round 2's, 1.7 to 7.7 during round 3's, 1.0 to 7.8 during round 4's, 2.6 to
6.9 during round 5's, 1.2 to 2.9 during round 6's.

- **Wall time is noise there.** A stage's wall time was often five times its CPU time. Cold-start
  times are given as measured; compare them only on a quiet machine, or with the old and the new
  binary interleaved in one session, as round 2's baseline was.
- **Take the minimum of N runs**, the run least disturbed. The harness prints the median and the
  spread, (max − min) / min, beside it.
- **Instructions are the steadiest metric, not a perfect one.** A stream is written on one thread,
  and its instructions spread 0–4% over five runs. A file is read by DuckDB on every core, and
  idle workers spin while they wait, so the same load counted 5–26% more instructions on a busier
  run. A change to a file path has to beat that spread, or be judged on the stage's own count
  from `--record`.

## The query path: no-shadowing

REQ-9's no-shadowing check runs after every `CREATE` and `ALTER` the agent sends
(`an_agent_object_cannot_shadow_a_host_object_or_builtin`). Measured with `perf stat -e
instructions:u` over `benchy-mcp host`, one attached table, 200 calls of each kind, three runs of
each binary interleaved, the session's fixed cost (no calls) subtracted; `main` at `69a43a3`
against the change:

| Each call | Before | After | Why |
|---|---|---|---|
| `SELECT count(*) … WHERE …` | 1.22 M | 1.20 M | Within the noise: a SELECT pays one comparison of its statement type. |
| `CREATE TEMP TABLE … AS …; DROP …` | 1.75 M | 4.1 M | Its own transaction and one listing of the agent's tables and views. |
| `CREATE TEMP VIEW …; DROP …` | 5.55 M | 3.9 M | A view returns no rows, so it used to list every type to rule out a dynamic `PIVOT`; one that made a table or view no longer does. |
| `CREATE MACRO …; DROP …` | 6.9 M | 119 M | `duckdb_functions()` lists every built-in before it filters (R4-4); paid only by a `CREATE` that made no table, view or type. |

The ENUM a kept object spells (`an_object_that_keeps_an_enum_in_its_sql_is_refused`), the
pivot types told from the agent's (`a_type_the_agent_names_is_never_a_pivots`), and the memory
ceiling read once more as each request is answered
(`an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once`). Measured the same way, over
`benchy-mcp examples/data` through MCP, three runs each, medians, the session with no call
subtracted; `main` at `cdae07a` against the change, 200 calls of each kind (50 of the PIVOT and
the macro). Each session's fixed cost varied by about 100 M between runs, so about 0.5 M a call
at 200 calls and 2 M at 50 is noise:

| Each call | Before | After | Why |
|---|---|---|---|
| `SELECT count(*) … WHERE …` | 1.3 M | 1.2 M | Within the noise: one read of `statm`, 5 µs, as the engine answers. |
| `CREATE TEMP TABLE … AS …; DROP …` | 10.2 M | 12.5 M | The SQL of the agent's tables, read for an ENUM in a DEFAULT, generated column or CHECK. |
| `CREATE TEMP VIEW …; DROP …` | 9.3 M | 16.7 M | The SQL of the agent's views: `duckdb_views()` writes out the SQL of DuckDB's own 48 views before it filters, about 2 ms, so it is read only after a CREATE that made a view. |
| `CREATE MACRO …; DROP …` | 125 M | 136 M | The SQL of tables, views and indexes, read in full after a CREATE that made no table, view or type; the macro's own body is one more column of the listing it already made. |
| A dynamic `PIVOT` | 21 M | 24 M | Within the noise: its type's name sought in the batch's text; the types are listed once after the batch, the DROP's own error saying which stayed (a second listing had cost 6 M). |

What a `CREATE MACRO` pays, and why caching cannot take it away. The set of names DuckDB itself
takes, its built-ins and the names its parser reads itself
(`a_name_the_parser_reads_itself_is_taken`), is read once per engine, on the first `CREATE` that may have made a function, and kept:
nothing loads after the lock, so it cannot change. The built-ins are one listing of
`duckdb_functions()`, about 120 M instructions; the parser's names one parse of each of 337
keywords and rewritten names, about 50 M (measured in-process, 20 runs, DuckDB 1.5.5). What every
`CREATE MACRO` still pays, about 125 M and 30 ms here, is the listing of the *agent's own*
functions, to learn the name it just made: `duckdb_functions()` is the only listing of a
catalog's macros, and it builds every built-in's row before it filters (item 5 below). Its cost
does not depend on how many functions the agent has.

The larger taken set (`a_system_view_name_is_taken`, `a_table_a_helper_reads_stays_taken_after_a_detach`)
costs a `CREATE` nothing it can measure. Over `benchy-mcp host`, no table, `perf stat -e
instructions:u` across every process, `cdae07a` against the change: each of 200 `CREATE TEMP
TABLE … AS …; DROP …` pairs 10.36 M before and 10.35 M after (three runs, medians). The 47
system view names are one listing of `duckdb_views()` over the `system` catalog, read on the
first `CREATE` or `ALTER` and kept for the engine's life: the first pair went from 48.6 M to 53.4 M over a
session with none (nine runs, medians). The tables the helpers read are a set the server sends
with the helpers; checking it is a hash lookup per name the agent has.

Checking before execution instead was not built: the C API gives a prepared statement's type
but not the name it creates, and `json_serialize_sql` serializes SELECT alone, so the name
would have to come from a second SQL parser of benchy's own. That parser would be what the
escaped and quoted spellings are aimed at, and it would never see the `CREATE TYPE` DuckDB adds
for a dynamic `PIVOT`. After execution the catalog names the object as DuckDB will resolve it.

### The empty-result note

An empty result whose query quotes a string gets a note saying what the text columns it names
hold (REQ-11). It read each such column whole, `SELECT DISTINCT … ORDER BY 1 LIMIT 21`, even a
column of unique values that can never give a note, and it ran after the watchdog, so it could
carry a batch past the server's hard deadline
(`the_empty_result_note_stays_inside_the_deadline`). Now each column's first 2,048 values are
read first, and a column with more than 20 distinct among them is not read further; the rest
runs under an interrupt at what is left of the deadline, at most 0.5 s.

Measured over `benchy-mcp <dir>` through MCP, one CSV of 3,000,000 rows with eight columns of
unique text and a three-valued `kind`, `perf stat -e instructions:u` across every process, the
session with no call subtracted, medians of three runs, `2e5400a` against the change. The
session's own load varied by about 15 G between runs, so the after column is read over 100
calls and is good to about 0.1 G a call:

| Each call | Before | After |
|---|---|---|
| `SELECT c1, …, c8, kind FROM w WHERE id = -1 AND kind = 'K0'` (a note on `kind`) | 11 G | 0.1 G |

At `--deadline 3 --hard-grace 1` the same query with `c1 <> 'zz'` in place of the filter on
`kind` killed the engine at 4.2 s before; it now answers in 20 ms, and after 2.25 s of other
work in the same batch the batch finishes and the agent's tables stay. A query that quotes no
string, or names no text column, pays nothing: the note returns before it reads.

## Helpers

REQ-18's helpers. Measured with `perf stat -e instructions:u` over a whole `benchy-mcp
examples/data` session, every process of it, five runs each, medians; the fixed cost is a
session with no query, a query's the difference to one with 100 of them; `main` at `60a0133`
against the change, and the change with the three example helpers (`examples/helpers.sql`):

| | `main` | no helpers | 3 helpers |
|---|---|---|---|
| A session, start to end, no query | 662 M | 660 M | 1,080 M |
| A query over `meters` | 1.75 M | 1.61 M | 1.6–1.8 M |
| Wall time to the first `describe` | | 377 ms | 580 ms |

A session without helpers pays nothing: no job runs, no catalog is attached, and a query pays one
comparison against an empty set. With them, startup pays about 420 M instructions and 200 ms:
the helpers job, a process with a DuckDB of its own, makes them and lists every function once
to check what it made and which names are DuckDB's (`duckdb_functions()` builds a row for each
built-in before it filters, about 115 M, R4-4); the engine lists them once more as it attaches
the file, as it checks a table's. Reading the built-ins before making the helpers was a second
listing, 88 M, and is not done. A helper costs a query nothing over its SQL written out:
`dewpoint(20, 50)` 5.9 M a call, the same expression inline 6.0 M.

Finding the tables the helpers read (REQ-18 `a_table_a_helper_reads_stays_taken_after_a_detach`)
adds one `json_serialize_plan` per view to the listing of views, and one `json_serialize_sql` of
each helper macro's body to the listing of functions the job already makes, computed for the
helpers' rows alone: no second `duckdb_functions()`. That body is now read for whether it reads
a table at all (REQ-18 `a_helper_macro_that_reads_a_table_is_refused_at_startup`), from the
same column. A session over `examples/data` with the
three example helpers, start to end with no query, every process: 1,252 M at `cdae07a`, 1,264 M
with the change (seven runs each, medians; the runs spread over 1,226–1,481 M).

### No check of the agent's SQL for a helper's CTE

0.2.0 read every batch, beside helper macros, for a CTE named like a table a macro reads, and
parsed one with a WITH and such a name (about 0.67 M a call, measured then). A helper macro now
reads no table, so that check is gone (REQ-18
`a_helper_macro_that_reads_a_table_is_refused_at_startup`). Measured with `perf stat -e
instructions:u` over a whole `benchy-mcp examples/data` session, every process, 1,000 calls,
five runs, medians, a session with no call subtracted; the 0.2.0 binary with its example helpers
(`hours_above` read `meters`) and with this change's (no macro reads a table), against the
change:

| | 0.2.0, its helpers | 0.2.0, these helpers | change |
|---|---|---|---|
| A session, start to end, no call | 1,515 M | 1,312 M | 1,336 M |
| `WITH d AS (SELECT * FROM meters) SELECT dewpoint(20, 50) …` | 6.59 M | 5.92 M | 5.98 M |
| `SELECT count(*) FROM meters WHERE …` | 1.24 M | 1.43 M | 1.36 M |

A batch that calls a helper macro beside a WITH naming a table a macro read paid about 0.6 M for
its parse, and no longer does; the rest is within the ±0.15 M a call and ±0.15 G a session these
sessions vary by. The check that a macro reads no table adds nothing measurable to startup: it
reads the column of `json_serialize_sql` the job already computed.

## The profile's claims, made true: what it costs

Four `describe` claims were stated as facts and were not: an integer reading taken for the
series key, "none missing or repeated" on checksums that three gaps and three repeats leave as
they were, anomaly counts cut at 200,001 rows, and past 200,000 series an extent and an "every
series" of the series examined (REQ-14). Measured against the previous commit, on a 4-core
aarch64 box under load 4–8 from other work. Whole sessions (load, profile, one `describe`),
`perf stat -e instructions:u`, three runs each; the profile's own stages from
`BENCHY_TRACE_TIMINGS`, CPU of every thread, the least of three:

| table | instructions, before → after | profile CPU, before → after |
|---|---:|---:|
| the eval's meter data (840,960 rows, 24 series), in time order | 5.21–5.63 → 5.20–5.85 G | 40 → 39 ms |
| the same, newest first | 5.23–5.30 → 5.22–5.83 G | 38 → 40 ms |
| the same, shuffled row by row | — | 257 → 578 ms (`profile.settle` 334 ms) |
| 500 integer device ids × 2,000 times, with an integer CO₂ and RSSI | 36.5–36.8 → 5.90–6.35 G | 5,988 → 252 ms (`profile.key` 95 ms) |

- **The tally's order check.** A series' times that came in order, oldest or newest first, have
  no repeat, which the checksums cannot prove; the tally keeps that with a compare each way a
  row, beside the extent it already keeps. `bench/ingest.py`, five runs: the watch thread
  33.4 → 35.8 ms CPU at exec_series-1m and 36.8 → 38.8 ms at exec_intkey-1m, about 2 ms per
  million rows; whole-session instructions +0.5% / +0.7% / +2.3% at exec_series-1m, csv-1m and
  exec_intkey-1m, inside their 6–15% spread. Tracked only rising, the meter data written newest
  first cost 1.5–1.9 G more, in the exact count below.
- **`profile.settle`.** A series that fits its checksums but whose times came in no order is
  settled by an exact `count(DISTINCT t)` per series. Free for a table in time order either way;
  334 ms for the meter data shuffled row by row, more than the rest of its profile. `SELECT
  DISTINCT k, t` counted per key was slower, 382 ms.
- **`profile.key`.** Where two columns of a kind could be the series key, each is asked how many
  of the first slice of time's rows (the step's sample, about 50,000) follow the row before in
  their series by the most common distance. About 32 ms each at 1M rows; nothing when only one
  column could be the key, as in every `bench/ingest.py` scenario. The wrong key had cost
  5.8 s in `profile.gaps` alone.
- **`profile.gaps`** counts per series in DuckDB rather than listing the anomalies in order:
  the same window, one aggregate over its anomalous rows.

## Infinite timestamps kept on load: 21 instructions a row

A zone-less timestamp is converted with `make_timestamptz(epoch_us(ts))` (src/canonical.rs),
chosen over `timezone('UTC', ts)` because it skips ICU's calendar. `epoch_us` of `infinity` and
`-infinity` is NULL, so both were loaded as nulls from Parquet, Arrow and connector sources
(REQ-15 `an_infinite_timestamp_is_kept_from_every_source`). Now the conversion is
`coalesce(make_timestamptz(epoch_us(ts)), CAST(ts AS TIMESTAMPTZ))`, so ICU's cast handles only
the rows `epoch_us` returned nothing for: the infinities and the nulls.

Measured with DuckDB 1.5.5 alone (its Python package), on one thread: the eval CSV's 840,960
timestamps, converted into a table ten times, three runs of each, minus the same run with no
conversion. The spread was under 1%.

| conversion | per load of the CSV | per row |
|---|---|---|
| `make_timestamptz(epoch_us(ts))`, before | 29 M | 35 |
| `coalesce(…, CAST(ts AS TIMESTAMPTZ))`, taken | 47 M | 56 |
| `CASE WHEN isfinite(ts) THEN … ELSE CAST(ts AS TIMESTAMPTZ) END` | 50 M | 60 |
| `timezone('UTC', ts)` | 1,269 M | 1,509 |

The change costs 18 M instructions per load of that CSV. A whole session that loads it and asks
`describe` counts 5.2–5.5 G both before and after (five runs of each, interleaved), so the
difference is lost in a file load's 3–5% spread. The fast path is still about 27 times cheaper
than ICU's. A nanosecond column's check for digits finer than a microsecond now skips an
infinity (`isfinite(ts) AND …`). `epoch_ns` of `infinity` returns the sentinel itself, which had
the column refused.

## An NDJSON infinity refused: about 1.5 ms of a sniffed JSON load

DuckDB 1.5.5's JSON reader reads `infinity` and `-infinity` in a column it sniffed as a timestamp
or a date as 1900-01-01 00:00:00 (any case, padded, or `\u`-escaped), so such a file loaded with
that time. It is now refused (REQ-15 `an_ndjson_infinity_is_refused`). A byte search of the file
for the word would have cost a pass over it on every load and still had to allow for every case
and escape the reader takes. The check is instead on the table written: whether a timestamp or
date column holds 1900-01-01 00:00:00, a filter on a constant that the row groups' statistics
answer, and, only where one does, a second read of the file (`read_json_objects`) for an
infinity in that column's text, so that a genuine 1900-01-01 loads. A file whose timestamps were
read as text (R3-1) is not checked: that read never takes an infinity.

Measured on this machine (aarch64, shared), with the loader's `file.infinity` span:

| file | read | `file.infinity` CPU |
|---|---|---|
| the harness's 1,000-row NDJSON (`ndjson-1k`, `http-ndjson-1k`) | sniffed | 1.7–1.8 ms (min of five), of the loader's 128–144 ms |
| its 1,000,000 rows as a `.json` file | sniffed | 1.4 ms, of the read's 777 ms |
| its 1,000,000 rows as NDJSON (`ndjson-1m`) | as text | not run |

The cost does not grow with the file: the statistics skip every row group. Whole-session
instructions (`bench/ingest.py`, five runs each, before and after) moved within their spread on
this machine: `ndjson-1k` 498–543 M before and 530–759 M after, `http-ndjson-1k` 507–615 M and
513–619 M, `ndjson-1m` 5,635–6,836 M and 5,697–5,836 M. A file that does hold a genuine
1900-01-01 timestamp pays the second read, about what parsing the file once more costs.

## R6-W. Wide rows on one thread

A CSV or JSON file's stage is held to 256 MiB and spills past it (R4, "Memory and disk"). A stage
of very wide rows did not: DuckDB ran out of memory under the cap, and the stage was filled again
without it and held whole. A 676 MB CSV of 300 `DOUBLE` columns took its loader to 2,090 MB, and
since R6-L1 such a file fails at the 1 GB loader ceiling. The owner decided that wide stages
must spill like narrow ones and load under the ceiling.

**What holds it.** Measured with DuckDB 1.5.5 alone (its Python package), each stage a `CREATE
TABLE … AS SELECT * FROM read_csv_auto(…)` into an attached in-memory database under
`memory_limit = '256MiB'`, as `source::staged` fills it, `duckdb_memory()` polled every 50 ms;
CSVs of random two-decimal numbers, 413 MB unless named:

| file | 4 threads | 2 threads | 1 thread | 4 threads, no cap |
|---|---|---|---|---|
| 300 columns, 400,000 rows, 827 MB | out of memory after 6.2 s: 211 MB of table, 30 MB of CSV buffers | out of memory | 13.3 s, 365 MB | 6.9 s, 1,816 MB |
| its first 25,000 rows, 52 MB | out of memory | | 2.2 s, 261 MB | |
| 200 columns, 689 MB | out of memory | out of memory | 11.8 s, 374 MB | |
| 100 columns | 9.8 s, 347 MB | 8.8 s | 7.4 s, 369 MB | 2.9 s, 975 MB |
| 60 columns | 6.7 s | 8.6 s | 6.3 s | 2.1 s, 1,019 MB |
| 30 columns | 7.3 s | 7.9 s | 5.3 s | 2.1 s, 944 MB |
| the harness's 10M rows, 3 columns, 403 MB | 1.4 s | | 4.5 s | |

Not a row group a thread, as `staged_within`'s comment had it, but a block (256 KiB) of every
column for each thread appending to the stage, whatever rows it holds: the first 52 MB of the
300-column file ran out on four threads too. Keeping the file's order merges the threads'
collections, two at once: 200 columns ran out on two threads where 75 MB of blocks would have
fit. Without the order (`preserve_insertion_order = false`, which REQ-15 cannot have) the
300-column file ran out after 1.7 s with 138 MB of table and nothing spilled. The JSON reader
parses a vector of 2,048 rows at once: an NDJSON of 5,000 objects of 2,000 keys, which DuckDB
reads as one `MAP` (163 MB), took 448 MB of allocations on four threads uncapped and ran out on
four under the cap; on one, 5.9 s and 349 MB.

**Tried, and why not:**

- **A higher cap on every thread:** the 300-column file at 512 MiB on four threads, 32.6 s and
  932 MB; at 768 MiB, 27.7 s and 1,008 MB; on two threads, out of memory at 384 MiB and 32.3 s at
  512 MiB. Many threads spilling merge what they spilled.
- **Smaller stage blocks or row groups:** `ATTACH ':memory:' (BLOCK_SIZE 16384)` still appends in
  256 KiB blocks, and ran out; `(ROW_GROUP_SIZE 8192)` ran out after spilling 455 MB.
- **A file-backed stage:** ran out the same way.
- **Two threads:** slower than one and than four whenever the stage spills (above).
- **One thread, staged:** fits the cap, but the write from the stage then needs the table's row
  group and the stage's blocks in one pool: for the 300-column file 512 MiB was not enough, and
  at 640 MiB the write took 19.2 s more and 764 MB; 200 one-digit columns needed 320 MiB.
- **One thread, straight into the table** under the cap, as Parquet is read (`read_into`): the
  300-column file in 20.2 s within 373 MB; 400,000 rows of 200 one-digit columns in 7.0 s
  (staged: 5.3 + 3.7 s, needing 320 MiB); 100 columns in 10.7 s (staged: 6.5 + 9.8 s). Taken
  for a CSV. Not for a JSON file: `read_into` asks the reader for its types first, and the
  2,000-key file's sniff took 8 s every time, three of them where one stage sniffs once (36 s
  against 16 s in benchy). A JSON file is staged on one thread.

**The change.** `Width::of` reads a file's first 64 KiB, counts the first line's separators and
the lines' mean length, and charges each thread two 256 KiB blocks a column and three times a
vector of 2,048 such lines; past half the cap between them the file is read on one thread from
the start (`a_file_is_read_on_one_thread_where_its_first_line_is_wide`). DuckDB's sniffer would
cost a second read: a `DESCRIBE` of a 1,000-row CSV is 462 M instructions (R2-3). A CSV goes
straight into its table, watched and checkpointed under the cap; a JSON file, or a narrowed body
(whose rows only a stage counts before the narrowing drops any), through a stage on one thread
held to the cap plus what the JSON reader holds of a vector of its lines on one thread (2,048
times the line and 32 bytes a key: 168 MB for 2,000 one-digit keys, whose sniff alone peaked at
140, 216 and 231 MB at 1,000, 2,500 and 5,000 lines; held to the cap alone it ran out,
`a_wide_json_file_is_read_on_one_thread_within_its_vector`). Its write and checkpoint are held to
the cap alone: written under the larger cap, 10,000 such objects took DuckDB alone to 746 MB,
under 256 MiB 558 MB, and 568 MB at 20,000; the table's checkpoint as the bench closed, under no
cap, took 20,000 from 573 to 787 MB, and under the cap stayed at 603 MB. A stage judged narrow
that runs out of memory is read on one thread next, and one that runs out there too without the
cap, the operator told each time. The table is the same file every way
(`a_table_read_on_one_thread_or_staged_is_the_same_file`; the layout tests below 20 times each
under load).

**Wide loads**, benchy, `aab0f17` against the change, loader peak RSS (`ru_maxrss`), first
`describe` and the loader's CPU, runs interleaved:

| file | before | after |
|---|---|---|
| CSV of 300 `DOUBLE` columns, 400,000 rows, 827 MB (2 runs) | 2,117–2,281 MB, 25.5–30.2 s, CPU 32–40 s | **338–343 MB**, 23.2–29.3 s, CPU 23–28 s |
| NDJSON of 5,000 objects of 2,000 random numbers, 163 MB (2 runs) | 635–648 MB, 21.3–22.4 s | **549 MB**, 17.8–18.4 s |
| NDJSON of 2,000 one-digit keys, 5,000 objects (90 MB) | 878 MB, 20.5 s | 522–534 MB, 16.4–16.9 s |
| the same, 2,500 / 10,000 / 20,000 objects | — / 1,125 MB, 39.3 s / — | 420 MB / 554–578 MB, 33.5 s / 614 MB, 64.8 s |
| CSV of 200 one-digit columns, 200,000 / 400,000 / 600,000 / 800,000 rows (3 runs) | 1,007 / 1,371 MB / — / — | 328–345 / 357–377 / 361–395 / 350–375 MB |

The wide CSV's peak no longer grows with the file. The 2,000-key NDJSON's grows, much less, up to
a row group of its table: the write of 122,880 rows of 2,000 `MAP` entries holds what DuckDB does
not count against its limit, the same from DuckDB alone (above), and so it is bounded by the
width of a row, not the length of the file. Its time is DuckDB's JSON sniff, which reads the
whole sample (20,480 objects) of 2,000 keys on one thread before the stage: 8.2 s of the 16.4 s
at 5,000 objects, 32 s of the 65 s at 20,000, as before the change.

**Narrow loads unchanged**, `aab0f17` against the change, 5 runs each interleaved (ndjson-1m 10
more), minimum / median:

| scenario | instructions | first `describe` | loader peak RSS |
|---|---|---|---|
| csv-1m | 5.415 / 5.442 → 5.420 / 5.434 G (+0.1%) | 651 / 720 → 646 / 697 ms | 115 → 118 MB |
| ndjson-1m | 5.574 / 5.623 → 5.607 / 5.724 G; again 5.578 / 5.692 → 5.554 / 5.620 G | 661 / 675 → 688 / 717 ms; again 640 / 669 → 634 / 681 ms | 142 → 143 MB |
| csv-10m | 49.33 / 49.46 → 49.18 / 49.30 G (−0.3%) | 4.18 / 4.31 → 4.10 / 4.40 s | 385–434 → 387–466 MB |
| ndjson-10m | 50.71 / 50.82 → 50.75 / 50.92 G (+0.1%) | 4.24 / 4.56 → 4.08 / 4.26 s | 351–358 → 350–357 MB |

A narrow file pays a 64 KiB read and one `current_setting('threads')`, inside the noise. Left
as it was: a stage of 30 to 100 columns that spills a lot is sooner on one thread than on four
(above), but slower where it fits; whether to move the line is a measurement for another round.

## R6-L1. A memory bound for loaders

The engine has a resident-memory ceiling; a loader had none. What loaders peak at, and what
grows one without bound, measured on DuckDB 1.5.5 with four threads, `ru_maxrss` of each process
and `/proc` of the whole tree every 25 ms:

| load | loader peak | |
|---|---|---|
| the harness's 10M-row loads (round 6 baseline below) | 94–112 MB ClickHouse, Parquet, Arrow, connector; 351–410 MB CSV, NDJSON | bounded: a stream goes in batch by batch, a file's stage is held to 256 MiB |
| a 10,000,000-row Parquet file in one row group, 56 MB | 118 MB | the same as in default row groups (118 MB): DuckDB reads a row group in pieces |
| an NDJSON of 2,000 objects nested 60 deep | 57 MB | |
| a 2 GB CSV body, gzip (4.8 MB) or zstd (168 KB), over HTTP | 563 / 611 MB, refused at the source's row cap | bounded: decoded into a spool on disk as it arrives, the zstd window held to 32 MiB, then staged as a file is |
| an NDJSON of 5,000 objects of 2,000 keys, 188 MB | **630 MB** | grew with the file: its stage did not fit 256 MiB and was held whole; on one thread since R6-W, 549 MB |
| a CSV of 300 `DOUBLE` columns, 400,000 rows, 676 MB | **2,090 MB** | the same; on one thread since R6-W, 338–343 MB (827 MB of it) |
| a connector that holds 800 MB before it writes, itself or in a process it starts | **800 MB** in the connector, the loader 43 MB | nothing bounded it |

**The bound.** The supervisor reads each loader's resident memory (`statm`, 5 µs) every 50 ms
while it waits for the loader's answer, and past `--loader-memory-ceiling` (`[loader]
memory_ceiling`; a host may lower it from protocol 6) kills it with its connectors' process
groups; the load is `E_LIMIT` and attaches nothing. For an exec source's loader every process
under it counts too, found through the kernel's `/proc/<pid>/task/<tid>/children` lists, which
6.8.0-138-generic has (item 8 below said it did not); a full `/proc` scan costs 3.8 ms here
(270 processes, 3.1 ms of it in the kernel), so where the lists are missing the tree is scanned
once a second instead. The default, 1 GB, is 2.4 times the largest bench peak; the 676 MB wide
CSV and a connector that holds 800 MB then failed with a message that says to narrow the source
or raise the ceiling. The wide CSV loads within it since R6-W; the connector still fails.

**Cost**, `a0d7155` against the change, 5 runs each, minima (load 1.9–3.2); csv-10m measured a
second time, the binaries interleaved:

| scenario | instructions | CPU, all processes | server CPU | first `describe`, min / median |
|---|---|---|---|---|
| ch-1m | 2.18 G → 2.17 G (−0.5%) | 510 → 531 ms | 19.9 → 21.6 ms | 469 / 510 → 469 / 530 ms |
| csv-1m | 5.43 G → 5.43 G (+0.2%) | 1.01 → 1.00 s | 19.9 → 21.2 ms | 662 / 733 → 650 / 683 ms |
| ndjson-1m | 5.59 G → 5.59 G (−0.1%) | 1.12 → 1.16 s | 19.6 → 20.6 ms | 662 / 688 → 675 / 683 ms |
| parquet-1m | 2.47 G → 2.48 G (+0.5%) | 574 → 565 ms | 19.3 → 19.7 ms | 445 / 461 → 440 / 516 ms |
| exec-1m | 2.03 G → 2.03 G (+0.2%) | 523 → 522 ms | 25.4 → 27.3 ms | 491 / 551 → 490 / 536 ms |
| exec-10m | 16.30 G → 16.32 G (+0.2%) | 3.31 → 3.31 s | 24.1 → 39.4 ms | 2.99 / 3.50 → 2.94 / 2.97 s |
| csv-10m | 48.93 G → 49.35 G (+0.9%); again 49.19 G → 49.24 G (+0.1%) | 8.16 → 8.04 s | 19.6 → 31.3 ms; again 20.6 → 33.3 ms | 4.95 / 5.22 → 4.18 / 4.36 s |

Within the noise everywhere but the server's own CPU, which the watch costs about 1.5 ms per
second of load (a wake-up and a read every 50 ms; 4–5 ms a second for an exec loader, whose
threads' `children` lists are read too): 0.15–0.5% of a core while a load runs, nothing once it
is done. Loader peak RSS did not move (csv-10m 420 → 388 MB, the spread of its spill).

## Baseline: round 6, `b660bcd`

`main` at `b660bcd`, after the 18 PRs since round 5's baseline (the engine's memory ceiling and its watch, the no-shadowing and ENUM checks and the catalog read
through `system.main`, the agent's connection kept in UTC, the memory limit put back as it was
read, the profile's distinct counts held to the values and its keys in their type's order, the
operator's helpers, host protocols 3 to 5), against round 5's baseline binary, `69a43a3`
(`2a8e24d`'s code), 2026-10-14. Both built from their own commits and run interleaved run by run
(`69a43a3`, `b660bcd`, then the reverse order, …) in the same session on the same data,
ClickHouse on its own port pair (8167 and 9067); load average 1.2 to 2.9. Minima of 5 runs per
scenario; the first `describe` also as the median. `hoststream` needs host protocol 3, which
`69a43a3` does not speak: its baseline is `2b6a95d` (the PR that added it). `hostchannel`
(protocol 5) has no baseline. A host session's server writes no usage line, so its peak RSS
is not in the table.

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| ch-1m | 2.18 G → 2.17 G (−0.2%) | 508 → 526 ms | 64 → 64 MB | 525 / 564 → 531 / 586 ms (+1% / +4%) |
| ch-10m | 17.26 G → 17.28 G (+0.2%) | 2.86 → 2.85 s | 96 → 94 MB | 2.67 / 2.81 → 2.65 / 2.74 s (−1% / −2%) |
| exec-1m | 2.02 G → 2.04 G (+1.0%) | 517 → 533 ms | 59 → 60 MB | 562 / 602 → 603 / 610 ms (+7% / +1%) |
| exec-10m | 16.32 G → 16.32 G (−0.1%) | 3.28 → 3.23 s | 102 → 103 MB | 3.10 / 3.56 → 2.95 / 3.61 s (−5% / +1%) |
| csv-1m | 5.42 G → 5.43 G (+0.2%) | 1.01 → 1.01 s | 114 → 115 MB | 715 / 767 → 727 / 790 ms (+2% / +3%) |
| csv-10m | 49.15 G → 49.11 G (−0.1%) | 8.28 → 8.13 s | 392 → 388 MB | 4.53 / 4.82 → 4.41 / 4.56 s (−3% / −5%) |
| ndjson-1m | 5.56 G → 5.55 G (−0.1%) | 1.10 → 1.16 s | 142 → 142 MB | 727 / 734 → 748 / 758 ms (+3% / +3%) |
| ndjson-10m | 50.63 G → 50.81 G (+0.4%) | 8.55 → 8.78 s | 356 → 351 MB | 4.33 / 4.68 → 4.41 / 4.77 s (+2% / +2%) |
| parquet-1m | 2.46 G → 2.46 G (+0.2%) | 579 → 582 ms | 62 → 62 MB | 532 / 536 → 523 / 564 ms (−2% / +5%) |
| parquet-10m | 20.24 G → 20.26 G (+0.1%) | 3.52 → 3.46 s | 114 → 112 MB | 2.79 / 3.13 → 2.73 / 3.31 s (−2% / +6%) |
| http-json-1m | 5.65 G → 5.65 G (0.0%) | 1.19 → 1.21 s | 69 → 69 MB | 748 / 759 → 755 / 788 ms (+1% / +4%) |
| hostspool-1m | 2.08 G → 2.08 G (+0.2%) | 447 → 450 ms | 59 → 60 MB | 535 / 543 → 547 / 559 ms (+2% / +3%) |
| hostspool-10m | 16.36 G → 16.38 G (+0.1%) | 2.72 → 2.79 s | 104 → 104 MB | 2.74 / 2.86 → 2.82 / 3.04 s (+3% / +6%) |
| hoststream-1m (from `2b6a95d`) | 2.08 G → 2.08 G (−0.2%) | 485 → 497 ms | 60 → 60 MB | 597 / 614 → 576 / 622 ms (−4% / +1%) |
| hoststream-10m (from `2b6a95d`) | 16.36 G → 16.36 G (0.0%) | 2.97 → 3.05 s | 104 → 102 MB | 3.12 / 3.16 → 3.23 / 3.46 s (+3% / +10%) |
| hostchannel-1m | 2.08 G | 487 ms | 59 MB | 578 / 646 ms |
| hostchannel-10m | 16.36 G | 3.02 s | 105 MB | 3.13 / 3.23 s |

The engine peaked at 44.6 MB before and 45.0 MB after in every scenario (medians); the server at
18 MB in both.

What the table says:

- **Nothing regressed the ingest path.** Instructions are within ±0.4% at every size but one, and
  that one, exec-1m at +1.0% by its minimum, is +0.6% by its median, inside its own 2.5% spread.
  Its first `describe` minimum (+7%) was its baseline's one fast run: run again, 15 runs of each
  interleaved, it read 560 / 576 → 551 / 586 ms (−2% / +2%), at the same instructions. The 10M
  host scenarios that moved by their medians, run again with 9 runs of each: hostspool-10m
  2.74 / 2.81 → 2.70 / 2.99 s, hoststream-10m 3.12 / 3.37 → 3.05 / 3.27 s, both within ±0.1%
  in instructions. Read the rest as this machine's spread.
- **The CPU that moved, moved without instructions.** exec-1m's `stream.insert` took 15 to 27 ms
  more CPU by its median in two sessions, and the 10M-row streams' `stream.watch` 15–17% more by
  its minimum; `perf stat` over 9 runs of exec-1m each then counted 1.22 G cycles before and
  1.14 G after, for the same 2.04 G instructions. The watch's one change is a sum per batch per key column
  (`a_distinct_count_never_exceeds_the_values`), and it runs beside the insert, off the path.
- **The stream channel costs what a streamed attach costs.** hostchannel matches hoststream
  within 0.1% in instructions at both sizes, and its first `describe` is within the spread: 578 /
  646 against 576 / 622 ms at 1M rows, 3.13 / 3.23 against 3.23 / 3.46 s at 10M.

### Every session's fixed part, since round 5

The 1k-row sessions, where next to nothing loads, show what every session pays before its first
`describe`; 9 runs of each, interleaved, medians:

| scenario | instructions | first `describe`, min / median |
|---|---:|---:|
| exec-1k | 414.5 M → 422.4 M (+7.9 M) | 250 / 260 → 250 / 262 ms |
| csv-1k | 958.4 M → 967.3 M (+8.9 M) | 370 / 385 → 363 / 384 ms |
| host-1k | 475.5 M → 476.5 M (+1.0 M) | 255 / 270 → 256 / 264 ms |
| ch-1k | 416.2 M → 420.1 M (+3.8 M) | 230 / 240 → 234 / 249 ms |

So a session pays 1 to 9 M instructions more, 0.2 to 2%, and no time that five or nine runs
resolve. Where it goes, from `--record` (10 runs of exec-1k on each binary, built with symbols;
the counts are per run):

- **The agent's own tables and views, read before an attach** (REQ-9
  `an_attached_name_is_taken_too`): 8.6 M and 3.6 to 5.2 ms of the engine's `engine.attach`
  outside its traced stages, where it was 0.2 ms. The catalog listings right after it
  (`attach.listings`) cost 6.7 M and 3 ms less, so the attach as a whole is within 2 M and
  2 ms of before. **Measured, nothing to take:** skipping that read until the agent has sent a
  batch (it owns nothing before) took the 5 ms out of it and put 3.8 ms back on `attach.listings`
  (the first read of DuckDB's catalog functions in an engine pays for them, whichever it is), and
  the session's instructions did not move: exec-1k 427.3 → 429.9 M, exec-1m 2,041 → 2,055 M, the
  engine's attach 56.3 → 56.5 ms (9 runs each, medians).
- **The agent's connection in UTC** (`a_zone_other_than_utc_stops_the_engine`): `SET GLOBAL
  TimeZone` and its check at the seal, 2.8 to 4.6 M in `open_sealed` and under 1 ms. A guarantee
  of results that do not depend on the host's zone.
- **The profile of a table with no time column** (`a_distinct_count_never_exceeds_the_values`):
  over `examples/data`, whose `rooms` table has none, the loaders' `profile` 7.3 M more, the count
  of each key column read beside its estimate and, at up to 100,000 rows, a distinct count made
  exact where the estimate reached it. A table with a time column pays nothing of it.

### The query path, since round 5

Over a whole `benchy-mcp examples/data` session through MCP, every process, `perf stat -e
instructions:u`, 5 runs of each interleaved, medians, the session with no call subtracted; 200
calls of the SELECT, 50 of each pair:

| | `69a43a3` | `b660bcd` |
|---|---:|---:|
| A session, start to end, no call | 661 M | 681 M |
| `SELECT count(*) FROM meters WHERE …` | 1.35 M | 1.30 M |
| `CREATE TEMP TABLE … AS …; DROP …` | 2.41 M | 13.1 M |
| `CREATE TEMP VIEW …; DROP …` | 6.8 M | 17.0 M |
| `CREATE MACRO …; DROP …` | 6.9 M | 139 M |

A SELECT, the call an agent makes most, costs what it did; the memory watch's read of `statm` as
each request is answered (`an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once`) is not
measurable in it. Each `CREATE` pays for REQ-9's no-shadowing check and the ENUM rule, the costs
"The query path" above measured PR by PR (over other sessions, so its steps do not sum to these
exactly): a table +10.7 M, a view +10.2 M, a macro +132 M, the last the listing of the agent's functions that
`duckdb_functions()` builds every built-in's row for (R4-4, R5-3). The session's +20 M is the
fixed part above, paid by `examples/data`'s two tables.

## Baseline: round 5, `2a8e24d`

`main` at `2a8e24d` (round 4 merged: R4-2, R4-1 measured and not done, R4-3), against
round 4's baseline binary, `57e8901` (`5993d6b` plus a docs-only change), and round 1's start,
`22ff87e`, 2026-10-12. All three built from their own commits and run interleaved run by run
(`57e8901`, `2a8e24d`, `22ff87e`, then the reverse order, …) in the same session on the same
data, ClickHouse on its own port pair; load average 2.6 to 6.9 (14.7 at the first run). Minima of
5 runs per scenario; the first `describe` also as the median. `22ff87e` ran only the scenarios
the totals below need, and cannot load JSON pages (its HTTP source was a config and a pager, no
loader yet).

`main` as merged passed `BENCHY_CLICKHOUSE_BIN=… scripts/ci.sh` with no ClickHouse test skipped
(`clickhouse` 16, `origin` 9; 386 tests passed over 36 suites, counting the ClickHouse step's
second run, one ignored: `regenerate_the_shipped_example_stream`), and `benchy-mcp doctor
--probe` said this machine enforces REQ-3. Under load, 20 runs each with four busy loops beside
them, load average 8.8 to 19.4: `tests/origin.rs` (9 tests) 20 of 20, the `layout` lib tests
(`a_files_layout_is_the_same_on_every_load`,
`a_keys_words_are_read_alike_from_every_string_layout`) 20 of 20, and
`json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order` 20 of 20. No flake.

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| ch-1m | 2.17 G → 2.16 G (−0.1%) | 508 → 504 ms | 61 → 61 MB | 487 / 536 → 474 / 540 ms (−3% / +1%) |
| ch-10m | 17.28 G → 17.27 G (0.0%) | 2.90 → 2.91 s | 94 → 93 MB | 2.75 / 3.13 → 2.71 / 2.93 s (−1% / −6%) |
| exec-1m | 2.02 G → 2.03 G (+0.5%) | 532 → 555 ms | 57 → 57 MB | 504 / 574 → 515 / 553 ms (+2% / −4%) |
| exec-10m | 16.32 G → 16.31 G (−0.1%) | 3.34 → 3.32 s | 100 → 100 MB | 3.05 / 3.31 → 3.06 / 3.08 s (0% / −7%) |
| csv-1m | 5.45 G → 5.43 G (−0.4%) | 1.06 s → 996 ms | 112 → 112 MB | 764 / 783 → 661 / 707 ms (−13% / −10%) |
| csv-10m | 49.02 G → 48.74 G (−0.6%) | 8.11 → 7.79 s | 410 → 399 MB | 4.42 / 4.67 → 4.07 / 4.61 s (−8% / −1%) |
| ndjson-1m | 5.55 G → 5.61 G (+0.9%) | 1.23 → 1.20 s | 139 → 140 MB | 748 / 776 → 713 / 807 ms (−5% / +4%) |
| **ndjson-10m** | 42.26 G → 50.75 G (+20.1%) | 6.97 → 8.67 s (+24%) | **131 → 352 MB** | **6.88 / 7.62 → 4.52 / 5.20 s (−34% / −32%)** |
| parquet-1m | 2.46 G → 2.46 G (0.0%) | 606 → 591 ms | 59 → 59 MB | 491 / 507 → 467 / 497 ms (−5% / −2%) |
| parquet-10m | 20.27 G → 20.25 G (−0.1%) | 4.10 → 3.47 s | 110 → 111 MB | 3.24 / 3.30 → 2.69 / 3.16 s (−17% / −4%) |
| **http-json-1m** | 5.08 G → 5.66 G (+11.5%) | 1.08 → 1.18 s (+9%) | 61 → 66 MB | **1.16 / 1.18 s → 763 / 791 ms (−34% / −33%)** |

What the table says:

- **Round 4 moved exactly the two paths it changed**, by what R4-2 and R4-3 measured on their own:
  a 10M-row NDJSON file 34% sooner for a fifth more instructions and the loader at the stage's
  cap (R4-2), JSON pages 34% sooner for 11% more instructions (R4-3). Instructions are within
  ±1% everywhere else: nothing regressed.
- **The rest is the machine.** csv-1m (−13%) and parquet-10m (−17%) moved by their minima with
  no change to their path and their instructions flat; their medians moved 1–10%. Read them as
  the spread of five runs at this load, not as a win.

Since round 1's start, `22ff87e`, in the same session (minima of 5; the first `describe` minimum
/ median):

| scenario | instructions | first `describe` |
|---|---:|---:|
| ch-10m | 34.08 G → 17.27 G (**−49.3%**) | 4.56 / 4.93 → 2.71 / 2.93 s (**−40% / −41%**) |
| exec-10m | 27.81 G → 16.31 G (**−41.4%**) | 4.61 / 5.23 → 3.06 / 3.08 s (**−34% / −41%**) |
| csv-10m | 54.69 G → 48.74 G (**−10.9%**) | 4.78 / 5.65 → 4.07 / 4.61 s (**−15% / −18%**) |
| ndjson-10m | 64.07 G → 50.75 G (**−20.8%**) | 5.25 / 6.11 → 4.52 / 5.20 s (**−14% / −15%**) |
| parquet-1m | 3.66 G → 2.46 G (**−32.6%**) | 811 / 824 → 467 / 497 ms (**−42% / −40%**) |
| http-json-1m | not loadable at `22ff87e` | 1.25 / 1.34 s on `35ad7db` (round 3's start, round 4's session) → 763 / 791 ms |

`22ff87e` read a 10M-row NDJSON file faster than `57e8901` did (5.25 against 6.88 s): it read
every file through a stage on every thread, and R2-1 then bounded JSON's stage to 64 MiB; R4-2
gave it back.

### Where the time to the first `describe` goes, on `2a8e24d`

Stage wall times are minima of the five runs above; instructions from `--record` (one run each,
the binary built with symbols). A thread that traces no stage of its own is counted in the stage
its process's main thread was in, so arrow-json's decoder threads show under `stream.insert`.

- **ch-10m (2.71 s).** DuckDB's write, `stream.insert`, 2.18 s on one thread (80%), 11.8 G of the
  loader's 13.9 G sampled: `StringStats::Update` 15%, `memcpy` 8%, `DictFSSTCompressionState`
  7%, `StringAppend` 6%, `Utf8Proc::Analyze` 5%, ALP, bitpacking. Beside it the fetch and decode
  (`builtin.stream`, 0.54 G) and the watch (0.97 G, 194 ms). Then benchy's on the path:
  `builtin.estimate` 39 ms (the count asked of ClickHouse before the stream), the profile 38 ms
  (`profile.step` 0.44 G, 35 ms), the commit 38 ms, the checkpoint 20 ms, the engine's attach and
  check 49 ms. By library, the loader is 76% DuckDB, 10% libc, 13% benchy (all of it off the
  path, but the profile's step).
- **csv-10m (4.07 s).** DuckDB's reader on four threads 1.50 s (`file.read`, 25.6 G), its write
  on one 2.26 s (12.4 G), the watch beside it 617 ms (3.5 G, its own thread); the profile 26 ms,
  the attach 53 ms. The loader is 90% DuckDB, 7% libc, 2% benchy.
- **ndjson-10m (4.52 s).** The same shape: read 1.60 s (27.0 G), written 2.45 s (12.5 G), watch
  720 ms beside it (4.0 G), `file.describe` 82 ms, profile 33 ms, attach 54 ms. 90% DuckDB.
- **http-json-1m (763 ms).** The load 649 ms: the fetch thread, `http.fetch`, 583 ms wall —
  requests to the harness's server (a Python thread in the harness) and the scan
  (`Scan::skip`, about 0.54 G, 12% of the loader's instructions), the first page sniffed by
  DuckDB — while the load's thread inserts (311 ms) and two decoder threads run arrow-json
  (`TapeDecoder::decode` 25% of the loader, its struct, timestamp and number decoders about 15%
  more). The loader is 59% benchy (arrow-json and the scan), 31% DuckDB; the path is the fetch
  thread, not the decode.
- **Every session's fixed part** (the same in each scenario above): the server's start 27–35 ms,
  the loader's 15 ms and its DuckDB open 23–26 ms, the checkpoint 12–26 ms, the engine's attach
  and check 49–56 ms (`attach.functions` 27–31 ms, 115 M); the engine's start and open (15 + 26
  ms) overlap the load.

### Round 5: what is left, ranked by measured headroom in benchy's own code

R4-1 (a parallel write) is closed by the owner's decision: the write stays on one thread so the
blocks stay the same on every load (REQ-15). It is not on this list, and is not to be proposed
again unless DuckDB changes how it numbers blocks.

**What is left is small.** On every 10M-row path, DuckDB's own read and write are 85–95% of the
time to the first `describe`, and benchy's code on the path is 100–150 ms of 2.7–4.5 s (3–5%).
On JSON pages the path is the fetch thread. A session's floor is about 200 ms (round 4's 1k
sessions: 185–240 ms), of which about 50–70 ms is reachable from benchy (R5-2 to R5-4). No item
below is worth a round of its own; R5-1 is worth a measurement.

1. **R5-1. JSON pages: the fetch thread is the path — 583 ms of http-json-1m's 763 ms.** Since
   R4-3 the decode is off the path; what is on it is the requests and the scan on one thread.
   The scan is about 0.54 G (a table lookup a byte); how much of the 583 ms is the harness
   server's own pace is not measured. *First measure* the fetch thread's time in the request
   against the scan (a span each). If the scan is a large share: scan on a decoder thread and
   send only the cursor back, or a SIMD skip over string bodies. Estimate −50 to −200 ms
   (−7 to −26%) at 1M rows against this harness, and nothing where a real API's latency sets the
   pace. Plus R4-3 (b), the RFC 3339 fast path for UTC timestamps: about −0.25 G of CPU on the
   decoder threads, off the path.
2. **R5-2. `profile.step` from the watch (R3-6, R4-5).** 24–35 ms and 0.25–0.44 G at 10M rows
   on the path (ch-10m 442 M, 35 ms), 19 ms at 1M, 25 M a table. Keep the exact first value and
   the sample window's times in the `Watch`. Estimate −20 to −35 ms per table (≈1%) at 10M rows,
   −15 ms at 1k (≈7%); certain in size, small.
3. **R5-3. The engine's attach check lists every function (R4-4).** 27–31 ms and 115 M of every
   session; no probe that keeps every escape case refused is known. Up to −30 ms a session
   (−13% at 1k), if one is found.
4. **R5-4. Process starts by a zygote (R4-5).** 21 M and about 15 ms per engine or loader started,
   the loader's on the path. Its confinement and "holds nothing" must hold as the warden's do
   (`the_warden_holds_nothing_of_the_servers`). Estimate −15 ms a session.
5. **R5-5. benchy's CPU off the path (R4-6, unchanged).** The watch (0.97 G at ch-10m, 3.5–4.0 G
   at csv-10m and ndjson-10m on their own thread), Arrow's offset validation (about 0.3 G per
   10M rows), Parquet's second read: CPU only, none of it moves the first `describe`.

**Measured, nothing to take: ClickHouse's estimate.** `builtin.estimate`, 39 ms on ch-10m's path,
is the count (and the text columns' average length) asked of ClickHouse before the stream. It
stays before the fetch: REQ-12 (`estimate_before_fetch`) is that an over-budget load costs the
source one count, not a scan, so it cannot overlap the stream it admits.

**DuckDB's, and out of benchy's reach:** the write on one thread (2.2–2.5 s at 10M rows; 80% of a
stream's path), its readers (1.5–1.6 s for a 10M-row CSV or NDJSON file on four threads), its
per-row string statistics and compression, and a session's opens and checkpoint. What would
change that is DuckDB numbering blocks in row-group order whatever thread writes them (R4-1),
or a cheaper way to list one catalog's functions (R5-3).

### R5-H1. A host's attach took a file: every load spooled first — done, streamed through a pipe

A host that fetches data with its own credentials had to spool every load to a file before it
could attach it, since `attach` took a path. In a container that file is often on tmpfs, which is
memory: six sessions loading 240 MB each held 1.4 GB staged before anything attached. And the
load could not start until the fetch had finished.

**Three ways to stream, compared.** All three keep the loader's read the same — the stream goes
into its table by the path a file's takes, so benchy's own instructions do not move — and differ
in what else the stream crosses:

- *Binary frames on the protocol's stdin.* Every byte crosses the supervisor: one more read and
  write of it. Measured as one more process relaying the 10M-row stream (229 MiB) through pipes,
  `cat` in the middle: 145–151 ms of CPU and 212–214 M kernel instructions, every load, before any
  framing. And it ties `ping` to the stream: the reader that answers a ping must first read every
  frame ahead of it, which it can only hand to a loader as fast as the loader takes them, so a
  ping sent behind a megabyte of stream waits for a loader to read that megabyte. Not taken.
- *A pipe benchy makes and hands back.* A pipe has no name to hand back over a byte stream; a
  FIFO has one, a path, which anything that can reach it can write to (capability, not name), or
  the descriptor goes back over a socket, which is the third way with the roles reversed. Not
  taken.
- *The read end of the host's pipe, passed with the request (`SCM_RIGHTS`).* No byte crosses the
  supervisor or touches a file, the loader holds the pipe as descriptor 3 and is confined to its
  staging directory alone, and `ping` is answered by the reader, which sees only the request.
  Taken: host protocol 3, `"stream": true` (docs/HOST-PROTOCOL.md). It needs the session's stdin
  to be a Unix socket.

**Measured** on `69a43a3` plus the change (5 runs, minima; the machine at load 5.7, so wall times
spread 27–65%; instructions spread 0–2%). `host` attaches a file already there; `hostspool`
fetches it into `/dev/shm` first and attaches the file; `hoststream` fetches it into the pipe:

| | host-1m | hostspool-1m | hoststream-1m | host-10m | hostspool-10m | hoststream-10m |
|---|---|---|---|---|---|---|
| first `describe`, min (median) | 0.66 s (0.86) | 0.80 s (0.91) | 0.59 s (0.83) | 3.49 s (3.52) | 3.73 s (4.43) | 3.71 s (4.04) |
| instructions (user, whole tree) | 2.09 G | 2.08 G | 2.10 G | 16.35 G | 16.38 G | 16.38 G |
| loader peak RSS | 57.9 MB | 58.1 MB | 58.2 MB | 103.3 MB | 101.1 MB | 103.0 MB |
| spooled by the host (tmpfs) | — | 22.9 MiB | 0 | — | 228.9 MiB | 0 |
| written by benchy (the loader's table) | 4.3 MB | 4.3 MB | 4.3 MB | 40.8 MB | 40.9 MB | 40.9 MB |

With the fetch paced to 100 MB/s, as a network fetch arrives (`--host-fetch-mbps 100`, 3 runs,
load 6.7), the spool's real cost shows: it waits for the whole fetch before the load starts, and
the stream loads while it arrives.

| | hostspool-1m | hoststream-1m | hostspool-10m | hoststream-10m |
|---|---|---|---|---|
| first `describe`, min (median) | 1.30 s (1.36) | 1.22 s (1.27) | 6.19 s (6.47) | 3.85 s (4.06) |
| instructions | 2.11 G | 2.11 G | 16.41 G | 16.35 G |

So: the same instructions and the same loader memory, no spool at all where a file held 229 MiB
per 10M-row load (in memory, on tmpfs, until the attach answered), and the first `describe` 38%
sooner at 10M rows when the fetch takes as long as a network's (−2.3 s; the load overlaps the
fetch, so the session waits for the slower of the two rather than both). The loader still writes
its table, 40.9 MB at 10M rows: that is the bench, not a spool. The budget is charged the same
bytes as the file form's size (`a_host_streams_a_table_without_a_file`), and the table is the
file form's, block for block (`the_physical_layout_says_nothing_of_where_data_came_from`).

### R5-H2. A host that cannot pass a descriptor could not stream — done, a stream channel

R5-H1's stream needs the read end of a pipe sent with the request (`SCM_RIGHTS`), so the session's
stdin must be a Unix socket and the host must be able to send a descriptor inside its own framing.
Node.js has no public API for that (`child.send()` wraps a handle in Node's own IPC framing, and
`process.binding('pipe_wrap')` is deprecated and blocked under `--permission`), and Java has none.
Such a host spooled every load, R5-H1's cost.

**The channel.** The host starts `benchy-mcp host` with one more inherited pipe — `stdio[3]` in
Node.js, `pass_fds` in Python — named by `--stream-channel`, and writes its streams into it back
to back; an attach with `"stream": "channel"` takes the next whole one (host protocol 5). The
supervisor hands each loader a copy of the pipe as its descriptor 3 and never reads it, so, as in
R5-H1, no byte crosses the supervisor, and the loader is confined to its staging directory alone.

**Not reading past a stream.** A pipe cannot be read back, so the loader must stop at the stream's
end-of-stream marker exactly, where the next stream starts. It costs nothing measurable: the
message reader (`src/ipc.rs`) already reads one message at a time by its stated lengths, so a
reader below it (`ipc::Framed`) only holds each read to the part of the message it is in — the
prefix, the metadata, the body the metadata states — and copies and parses each message's
metadata, a few hundred bytes, as it passes. The read calls are the same as the descriptor
form's. A buffered reader that read ahead would have needed the overshoot handed to the next
loader, through the supervisor.

**A refused stream.** Refused part-way — a type, a limit, its name — the stream is still in the
channel, and the next attach would read its middle. Of the two ways to keep the channel's state
deterministic, reading the rest past and declaring the channel broken, the first is taken: the
loader reads on to the marker, following the messages without decoding them, at most
`attach_bytes` more bytes and within `--load-timeout`, and the host's next attach is unaffected. A
table refused before its loader would start (its name, the count of tables) gets a loader that
only does that, confined the same way. Measured: the 10M-row stream (229 MiB), refused at its
schema, was read past and answered in 0.19–0.20 s from the attach (3 runs), the loader's start
included, and the next stream then loaded. Where the place is lost all the same — the channel
ended before a marker, what came is not Arrow, more than `attach_bytes` left, the load timed out
— the channel is closed, so the host's writer gets `EPIPE`, and every later channel table is
`E_CHANNEL` (`a_channel_that_loses_its_place_is_closed_and_says_so`). Declaring every refusal
fatal to the channel would have been simpler, and would have cost a host its channel for a typo
in a table's name.

**Measured** on `ccc8e63` plus the change, against the descriptor form in the same runs (minima;
load 3–5; instructions spread 0–3%, wall time 9–132%):

| | hoststream-1m | hostchannel-1m | hoststream-10m | hostchannel-10m |
|---|---|---|---|---|
| first `describe`, 5 runs, min (median) | 0.58 s (0.59) | 0.65 s (0.86) | 3.41 s (3.94) | 3.22 s (3.41) |
| instructions (user, whole tree), 5 runs | 2.08 G | 2.10 G | 16.37 G | 16.38 G |
| instructions, 9 more runs at 1m | 2.10 G | 2.08 G | | |
| loader peak RSS | 59.3 MB | 59.3 MB | 103.9 MB | 101.9 MB |
| loader CPU | 377 ms | 365 ms | 2.80 s | 2.76 s |
| bytes the loader read (the budget's) | 24.0 MB | 24.0 MB | 240.0 MB | 240.0 MB |

With the fetch paced to 100 MB/s (`--host-fetch-mbps 100`, 3 runs):

| | hoststream-1m | hostchannel-1m | hoststream-10m | hostchannel-10m |
|---|---|---|---|---|
| first `describe`, min (median) | 0.60 s (0.61) | 0.59 s (0.65) | 3.67 s (4.23) | 3.09 s (3.40) |
| instructions | 2.08 G | 2.09 G | 16.38 G | 16.36 G |

So the channel costs what the descriptor form costs: the same instructions within the runs'
noise (the 1m pair swapped order between the two sets of runs), the same loader memory and CPU,
the same bytes, and the first `describe` no later. The table is the file form's, block for block
(`the_physical_layout_says_nothing_of_where_data_came_from` now loads it through a channel too).

## Baseline: round 4, `5993d6b`

`main` at `5993d6b` (merged: R3-1 to R3-5, R3-10, R3-11, the 256 MiB stage, storage v1.3.0),
against round 3's baseline binary, `35ad7db` (`99b5155` plus a docs-only change), 2026-10-12. Both
built from their own commits and run interleaved run by run (the old, then the new, then the new,
then the old, …) in the same sessions on the same data, ClickHouse on its own port pair; load
average 1.0 to 7.8. Minima of 10 runs per 1M scenario, 6 per 10M, 15 per 1k; the first `describe`
also as the median. `main` as merged passed `BENCHY_CLICKHOUSE_BIN=… scripts/ci.sh` with no
ClickHouse test skipped (`clickhouse` 16, `origin` 9), and `benchy-mcp doctor --probe` said this
machine enforces REQ-3. The layout tests
(`a_json_file_is_laid_out_alike_staged_or_read_straight_in`,
`a_files_layout_is_the_same_on_every_load`,
`the_physical_layout_says_nothing_of_where_data_came_from`,
`an_http_table_has_the_physical_layout_of_a_file_table`) passed 150 and 50 runs in a row at load
average 5 to 12, two loops at once: the Dictionary/FSST flake is gone with v1.3.0.
`a_trivial_query_does_not_pay_for_the_watchdog`, which failed once in round 3 at 10.08 ms
against its fixed 10 ms, failed 11 runs of 20 with 16 busy loops beside it; the test now judges it
against a refused statement's round trip in the same run instead, and it then passed 20 of 20
there and still fails on the old 20 ms tick.

| scenario | instructions | CPU, all processes | loader peak RSS | loader wrote | first `describe`, min / median |
|---|---:|---:|---:|---:|---:|
| csv-1m | 5.80 G → 5.41 G (−6.6%) | 1.13 → 1.03 s (−8.9%) | 117 → 114 MB | 4.3 → 4.3 MB | 832 / 876 → 673 / 747 ms (−19% / −15%) |
| parquet-1m | 2.73 G → 2.46 G (−9.6%) | 638 → 607 ms (−4.9%) | 59 → 62 MB | 4.3 → 4.3 MB | 645 / 701 → 518 / 591 ms (−20% / −16%) |
| ndjson-1m | 6.57 G → 5.56 G (−15.3%) | 1.19 → 1.15 s (−3.3%) | 143 → 142 MB | 4.3 → 4.3 MB | 836 / 927 → 703 / 780 ms (−16% / −16%) |
| exec-1m | 2.41 G → 2.02 G (−16.2%) | 618 → 543 ms (−12.1%) | 57 → 59 MB | 4.3 → 4.3 MB | 574 / 608 → 498 / 535 ms (−13% / −12%) |
| exec_series-1m | 2.75 G → 2.24 G (−18.6%) | 665 → 595 ms (−10.6%) | 58 → 60 MB | 4.3 → 4.3 MB | 580 / 643 → 494 / 552 ms (−15% / −14%) |
| exec_intkey-1m | 1.96 G → 1.53 G (−21.9%) | 534 → 483 ms (−9.6%) | 57 → 57 MB | 4.3 → 4.3 MB | 415 / 433 → 403 / 429 ms (−3% / −1%) |
| exec_lz4-1m | 2.49 G → 2.11 G (−15.2%) | 596 → 514 ms (−13.7%) | 58 → 60 MB | 4.3 → 4.3 MB | 574 / 621 → 493 / 541 ms (−14% / −13%) |
| ch-1m | 2.53 G → 2.18 G (−14.0%) | 596 → 513 ms (−13.8%) | 62 → 64 MB | 9.5 → 9.5 MB | 566 / 608 → 468 / 520 ms (−17% / −14%) |
| host-1m | 2.48 G → 2.08 G (−16.0%) | 549 → 462 ms (−15.8%) | 58 → 59 MB | 4.3 → 4.3 MB | 559 / 618 → 469 / 549 ms (−16% / −11%) |
| exec-10m | 19.87 G → 16.32 G (−17.9%) | 4.19 → 3.38 s (−19.3%) | 102 → 103 MB | 40.8 → 40.8 MB | 3.92 / 4.39 → 3.11 / 3.57 s (−21% / −19%) |
| exec_series-10m | 22.71 G → 18.00 G (−20.8%) | 4.41 → 3.31 s (−25.0%) | 104 → 105 MB | 40.8 → 40.8 MB | 3.94 / 4.15 → 3.25 / 3.63 s (−18% / −13%) |
| exec_intkey-10m | 14.91 G → 10.87 G (−27.1%) | 2.78 → 2.22 s (−20.1%) | 102 → 95 MB | 40.8 → 40.8 MB | 2.37 / 2.69 → 2.26 / 2.39 s (−5% / −11%) |
| exec_lz4-10m | 20.77 G → 17.20 G (−17.2%) | 3.86 → 2.94 s (−23.9%) | 101 → 101 MB | 40.8 → 40.8 MB | 4.01 / 4.60 → 3.13 / 3.68 s (−22% / −20%) |
| ch-10m | 20.52 G → 17.28 G (−15.8%) | 3.50 → 2.89 s (−17.5%) | 92 → 97 MB | 33.1 → 33.4 MB | 3.27 / 3.60 → 2.61 / 2.90 s (−20% / −19%) |
| **csv-10m** (new: the memory bound) | 50.33 G → 48.89 G (−2.9%) | 8.35 → 8.00 s (−4.3%) | **512 → 384 MB (−25%)** | **40.8 → 138.6 MB** | 5.72 / 5.97 → 4.39 / 5.60 s (−23% / −6%) |
| http-csv-1m | 5.84 G → 5.45 G (−6.7%) | 1.18 → 1.12 s (−4.6%) | 116 → 116 MB | 4.3 → 42.7 MB | 1.20 / 1.34 s → 819 / 957 ms (−32% / −29%) |
| http-csvdecl-1m | 5.66 G → 5.32 G (−6.0%) | 1.13 → 1.10 s (−2.9%) | 115 → 116 MB | 4.3 → 42.7 MB | 1.17 / 1.31 s → 848 / 893 ms (−28% / −32%) |
| http-parquet-1m | 2.77 G → 2.48 G (−10.6%) | 696 → 637 ms (−8.5%) | 61 → 62 MB | 10.9 → 10.9 MB | 709 / 756 → 511 / 605 ms (−28% / −20%) |
| http-ndjson-1m | 6.62 G → 5.63 G (−14.9%) | 1.40 → 1.38 s (−1.1%) | 172 → 143 MB (−17%) | 4.3 → 64.6 MB | 1.44 / 1.49 s → 938 ms / 1.07 s (−35% / −28%) |
| http-arrow-1m | 2.42 G → 2.04 G (−15.9%) | 556 → 482 ms (−13.2%) | 58 → 60 MB | 4.3 → 4.3 MB | 545 / 584 → 456 / 497 ms (−16% / −15%) |
| **http-json-1m** | 5.44 G → 5.07 G (−6.8%) | 1.13 → 1.09 s (−3.6%) | 61 → 63 MB | 4.9 → 4.9 MB | **1.25 / 1.34 → 1.24 / 1.34 s (−1% / 0%)** |
| csv-1k | 981 M → 961 M (−2.0%) | 360 → 353 ms | 51 → 49 MB | 0.5 → 0.5 MB | 334 / 391 → 310 / 388 ms (−7% / −1%) |
| parquet-1k | 488 M → 472 M (−3.3%) | 227 → 237 ms | 49 → 48 MB | 0.5 → 0.5 MB | 208 / 225 → 203 / 221 ms (−2% / −2%) |
| ndjson-1k | 514 M → 487 M (−5.3%) | 237 → 242 ms | 48 → 47 MB | 0.5 → 0.5 MB | 218 / 236 → 219 / 240 ms (0% / +2%) |
| exec-1k | 441 M → 407 M (−7.7%) | 231 → 222 ms | 47 → 45 MB | 0.5 → 0.5 MB | 241 / 286 → 230 / 274 ms (−5% / −4%) |
| exec_series-1k | 434 M → 413 M (−4.9%) | 223 → 223 ms | 47 → 45 MB | 0.5 → 0.5 MB | 214 / 290 → 215 / 320 ms (0% / +11%) |
| exec_intkey-1k | 429 M → 409 M (−4.6%) | 205 → 206 ms | 44 → 44 MB | 0.6 → 0.6 MB | 246 / 280 → 232 / 274 ms (−5% / −2%) |
| exec_lz4-1k | 437 M → 412 M (−5.8%) | 215 → 215 ms | 47 → 46 MB | 0.5 → 0.5 MB | 212 / 254 → 221 / 264 ms (+4% / +4%) |
| ch-1k | 436 M → 407 M (−6.6%) | 223 → 206 ms | 48 → 46 MB | 0.5 → 0.5 MB | 217 / 247 → 216 / 232 ms (−1% / −6%) |
| host-1k | 491 M → 464 M (−5.5%) | 187 → 196 ms | 48 → 46 MB | 0.5 → 0.5 MB | 232 / 252 → 238 / 264 ms (+3% / +4%) |
| host8-1k | 1.57 G → 1.52 G (−2.8%) | 855 → 816 ms | 48 → 47 MB | 4.4 → 4.4 MB | 517 / 567 → 491 / 584 ms (−5% / +3%) |
| http-csv-1k | 976 M → 966 M (−1.0%) | 366 → 379 ms | 52 → 50 MB | 0.5 → 0.6 MB | 363 / 405 → 366 / 418 ms (+1% / +3%) |
| http-csvdecl-1k | 566 M → 554 M (−2.1%) | 257 → 266 ms | 52 → 50 MB | 0.5 → 0.6 MB | 266 / 308 → 275 / 323 ms (+4% / +5%) |
| http-parquet-1k | 501 M → 477 M (−4.7%) | 230 → 237 ms | 49 → 48 MB | 0.6 → 0.6 MB | 241 / 273 → 220 / 267 ms (−9% / −2%) |
| http-ndjson-1k | 517 M → 490 M (−5.2%) | 222 → 244 ms | 49 → 47 MB | 0.5 → 0.6 MB | 225 / 264 → 225 / 274 ms (0% / +4%) |
| http-arrow-1k | 436 M → 404 M (−7.2%) | 204 → 207 ms | 48 → 47 MB | 0.5 → 0.5 MB | 188 / 206 → 185 / 202 ms (−2% / −2%) |
| http-json-1k | 472 M → 451 M (−4.6%) | 221 → 223 ms | 48 → 47 MB | 0.6 → 0.6 MB | 201 / 244 → 202 / 242 ms (0% / −1%) |

The declared files, new in round 3 (R3-5), on `5993d6b` alone (the old binary refuses their
config), beside the undeclared runs above: csvdecl-1m 5.27 G, 1.03 s CPU, 113 MB, first
`describe` 724 / 949 ms; ndjsondecl-1m 5.03 G, 1.04 s, 141 MB, 734 / 852 ms; csvdecl-1k 544 M
(csv-1k 961 M), 259 ms CPU, 262 / 316 ms; ndjsondecl-1k 498 M, 244 ms, 225 / 254 ms.

ClickHouse's own CPU to serve ch-10m: 0.96 → 0.93 s (2.39 s at round 1's start, which asked for
zstd). The engine stays at 44 MB and about 100 ms of CPU, the server at 18–25 ms, in every
scenario.

Since round 1's start, `22ff87e`, run interleaved three ways with both (`22ff87e`, `35ad7db`,
`5993d6b`) in the same sessions, load average 3.0 to 3.9, the same minima:

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| ch-10m | 34.06 G → 17.28 G (**−49.3%**) | 5.45 → 2.89 s (−47.0%) | 110 → 97 MB | 4.70 / 5.21 → 2.61 / 2.90 s (**−44% / −44%**) |
| exec-10m | 27.81 G → 16.32 G (**−41.3%**) | 5.17 → 3.38 s (−34.6%) | 115 → 103 MB | 4.53 / 4.81 → 3.11 / 3.57 s (**−31% / −26%**) |
| parquet-1m | 3.67 G → 2.46 G (**−32.8%**) | 820 → 607 ms (−26.0%) | 101 → 62 MB | 875 / 964 → 518 / 591 ms (**−41% / −39%**) |
| csv-1m | 6.20 G → 5.41 G (**−12.7%**) | 1.20 → 1.03 s (−14.5%) | 116 → 114 MB | 924 ms / 1.03 s → 673 / 747 ms (**−27% / −27%**) |
| ndjson-1m | 6.98 G → 5.56 G (**−20.2%**) | 1.27 → 1.15 s (−9.9%) | 142 → 142 MB | 966 ms / 1.08 s → 703 / 780 ms (**−27% / −28%**) |

In the same sessions `35ad7db` read ch-10m 20.52 G, exec-10m 19.87 G, parquet-1m 2.73 G, csv-1m
5.80 G, ndjson-1m 6.57 G: of the 16.8 G taken off ch-10m since round 1, rounds 1 and 2 took
13.5 G and round 3 3.2 G.

What the tables say beyond the wins:

- **No regression in instructions anywhere.** Every scenario fell, 1k sessions by 1–8%.
- **The first `describe` fell 13–35% at 1M and 10M rows, except JSON pages** (http-json-1m,
  1.24 s, unchanged): their fetch is decoded by arrow-json on one thread (R4-3), which no
  round-3 item touched. An integer key moved 3–5%: its write is half a text key's, and its
  watch was already beside it.
- **A 10M-row CSV is held to its stage's cap.** The loader peaked at 384 MB where `35ad7db` took
  512 MB, and wrote 139 MB of spill to its staging directory, removed with the stage. The first
  `describe` came sooner anyway (−23% minimum), the stage's spill outweighed by R3-2's watch
  beside the write and v1.3.0's cheaper write (`write_table` 2.91 → 2.38 s).
- **A stream's loader peaks 1–5 MB higher** with v1.3.0 (its change measured 2 MB); exec_intkey-10m 7 MB
  lower.
- **Several 1k sessions cost 2–10% more CPU for fewer instructions**, inside the 1k spread except
  http-ndjson-1k: the loader's CPU 107 → 123 ms (minimum), the reader threads a spooled body
  starts (R3-3 said about 12 ms). The first `describe` moved within ±5% everywhere at 1k.
- **The 5M-row CSV of docs/CAPACITY.md**, `35ad7db` and `5993d6b` interleaved, two runs each,
  load average 1.4 to 2.4: 3.82 and 4.19 s → 3.52 and 3.30 s to load; session peak 442 and
  445 MB → 466 and 485 MB, the watch beside the write reading back what the stage spilled
  (R3-2 measured +47 MB).

### Where the instructions go, on `5993d6b`

From `--record` (one run each, the binary built with symbols), in instructions:

| stage | csv-1m | parquet-1m | ndjson-1m | http-ndjson-1m | http-json-1m | exec-10m | exec_series-10m | exec_intkey-10m | ch-10m | csv-1k | exec-1k | host8-1k |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| reading a file or body (`file.read`, `file.describe`) | 2.81 G | | 3.15 G | 3.41 G | | | | | | 476 M | | |
| writing the table (`write_table`; `stream.insert` + `stream.commit`; `loader.load`) | 1.34 G | 1.56 G | 1.35 G | 1.28 G | 1.23 G | 14.02 G | 13.13 G | 6.67 G | 14.53 G | 66 M | 7 M | 62 M |
| the profile kept as rows go in (`stream.watch`, `file.watch`) | 300 M | 424 M | 338 M | 332 M | 97 M | 987 M | 2.43 G | 2.47 G | 991 M | 2 M | | |
| the profile read from the table (`profile.*`) | 105 M | 100 M | 96 M | 88 M | 110 M | 292 M | 211 M | 264 M | 468 M | 37 M | 44 M | 348 M |
| the fetch: pipe, HTTP, decode, checks (`exec.stream`, `builtin.stream`, `http.*`, `arrow.stream`) | | | | 107 M | 2.50 G | 360 M | 348 M | 12 M | 547 M | | 1 M | 7 M |
| the engine's attach check (`attach.*`) | 172 M | 172 M | 172 M | 172 M | 172 M | 174 M | 173 M | 174 M | 173 M | 172 M | 173 M | 207 M |
| opening DuckDB (`bench.open`, `open_sealed`) and the checkpoint (`bench.finish`) | 122 M | 118 M | 120 M | 129 M | 123 M | 136 M | 142 M | 137 M | 134 M | 119 M | 126 M | 532 M |
| starting processes (outside any stage) | 63 M | 63 M | 63 M | 63 M | 64 M | 66 M | 64 M | 65 M | 68 M | 63 M | 62 M | 228 M |
| **the loader, all of it** | **4.64 G** | **2.17 G** | **5.02 G** | **5.30 G** | **4.01 G** | **15.76 G** | **16.21 G** | **9.51 G** | **16.63 G** | **664 M** | **137 M** | **1.06 G** |
| **every process** | **4.91 G** | **2.44 G** | **5.29 G** | **5.59 G** | **4.29 G** | **16.04 G** | **16.50 G** | **9.80 G** | **16.91 G** | **938 M** | **418 M** | **1.44 G** |

And on the path to the first `describe` (stage wall times from the baseline runs, minima):

- **A 10M-row stream** (ch-10m, 2.61 s): DuckDB's write, `stream.insert`, 2.14 s on one thread,
  82% of the path; the fetch and decode run beside it, the watch beside them (212 ms); then the
  profile's queries 42 ms, the checkpoint 22 ms, the engine's attach and check 51 ms.
- **A 1M-row CSV** (673 ms): DuckDB's reader on four threads 239 ms, the write on one 221 ms
  (the watch beside it, 54 ms), the profile 17 ms, the checkpoint 12 ms, the attach 50 ms, and
  the processes' starts and opens.
- **A 1M-row Parquet file** (518 ms): the write, the file read straight into it, 262 ms (the
  second read and watch beside it, 96 ms), the attach 52 ms.
- **A 10M-row CSV** (4.39 s): read 1.67 s on four threads, written 2.38 s on one.
- **A 1k-row session** (exec-1k, 230 ms): the server's start 26 ms, the loader's 16 ms, its
  DuckDB open 27 ms, the connector 23 ms, the profile 19 ms, the checkpoint 13 ms, then the
  engine's attach and check 56 ms (`attach.functions` 31 ms of it); the engine's own start and
  open, 15 + 27 ms, overlap the load.

### What is DuckDB's, and what is benchy's

By library, over every process (`--record`, the same runs):

| scenario | DuckDB (`libduckdb.so`) | libc (mostly DuckDB's `memcpy`, `malloc`) | the dynamic linker | benchy's own code (with the Arrow crates) |
|---|---:|---:|---:|---:|
| ch-10m | 77.3% | 10.7% | 0.4% | 11.2% |
| exec-10m | 78.9% | 9.3% | 0.4% | 11.0% |
| exec_series-10m | 70.8% | 9.1% | 0.4% | 19.3% |
| exec_intkey-10m | 66.0% | 7.4% | 0.7% | 25.4% |
| parquet-1m | 80.2% | 11.5% | 2.9% | 4.3% |
| csv-1m | 88.8% | 7.2% | 1.4% | 2.0% |
| ndjson-1m | 85.5% | 10.2% | 1.2% | 2.0% |
| http-ndjson-1m | 83.2% | 12.0% | 1.3% | 2.6% |
| http-json-1m | 36.5% | 10.1% | 1.6% | **50.5%** (arrow-json) |
| exec-1m | 74.2% | 11.0% | 3.8% | 9.2% |
| csv-1k | 67.9% | 20.5% | 7.2% | under 0.1% |
| exec-1k | 54.1% | 23.0% | 16.5% | 0.7% |
| host8-1k | 54.0% | 21.9% | 16.6% | 1.0% |

The rest is `libstdc++` (DuckDB's, 0.2–5.6%) and samples perf could not place.

benchy's own share rose on the streams (9 → 11%) only because DuckDB's write fell (v1.3.0);
in instructions it is about the same: 1.9 G at ch-10m. Per scenario, what is benchy's:

- **Streams, one series** (ch-10m, exec-10m): 1.8–1.9 G of 16–17 G. The watch 0.99 G (99
  instructions a row), Arrow decoding and its checks 0.36–0.55 G (arrow-data's offset validation
  about 0.32 G, R3-8), and the batches into DuckDB's vectors (`BatchScan::func`, about 0.4 G).
  **None of it is on the path to `describe`**: the watch has its own thread, and the fetch and
  decode run beside the write.
- **Streams of 500 series** (exec_series-10m): 3.2 G, the watch 2.43 G (243 a row) on its own
  thread, 496 ms beside a 2.33 s insert. **Integer keys** (exec_intkey-10m): 2.5 G, the watch
  2.47 G, 575 ms beside a 1.44 s insert (the write is half as long without a text column), so
  inside it since R3-10.
- **Files**: 2–4%. The watch beside the write, 300–424 M per 1M rows of which `Watch::add` is
  about 100 M and the rest DuckDB converting its vectors to Arrow and, for Parquet, the second read.
- **JSON pages**: half of the instructions, and the path: arrow-json's tape decoder on the
  fetch's one thread (R4-3).
- **1k sessions**: under 1%. A session's fixed cost is DuckDB's: the engine's attach check
  (172 M, 41% of exec-1k), two DuckDB opens and a checkpoint (126 M, 30%), and three process
  starts (62 M, 15%), 85% of each the dynamic linker resolving `libduckdb.so`'s symbols.

**So benchy's own code is now small everywhere but JSON pages**, and off the path to `describe`
everywhere but JSON pages. What remains of a load's wall time is DuckDB's: its write, on one
thread by design (REQ-15), 82% of a 10M-row stream's path and a third to a half of a 1M-row
file's; its readers; and a session's opens, checkpoint and attach check. The Round 4 list below
is therefore mostly about how benchy drives DuckDB.

### Memory and disk, checked again

- **A CSV past its stage's cap**: csv-10m's loader peaked at 384 MB (512 MB on `35ad7db`) and
  spilled 139 MB to its staging directory, removed with the stage; the peak does not grow with
  the file (`a_large_csv_is_not_held_in_memory_while_it_is_staged`).
- **A JSON file read straight in**: 133 MB at 10M rows (633 MB of NDJSON), DuckDB's buffers.
  Staged since R4-2, as a CSV: 351 MB, spilling 134 MB.
- **Streams**: 95–105 MB at 10M rows, whatever the source.
- **An HTTP CSV or NDJSON body**: spooled whole, 42.7 and 64.6 MB at 1M rows, up to the source's
  `max_bytes`, removed when the load ends.
- **Not bounded by a constant, flagged as in round 3:** the stage's spill (bounded by the input,
  and by DuckDB's `max_temp_directory_size`, 90% of the free disk) and an HTTP body's spool
  (bounded by `max_bytes`, which the operator sets). Nothing found this round grows with the
  input without a bound. R4-2 gives every JSON file a stage, held to the same cap (R4-1, which
  would have given more loads one, is not done).

## Round 4: the backlog, ranked

Ranked by what each could take off the first `describe`, since the owner ranks it first, then
by instructions, then by how sure the estimate is. "R4-n" names an item of this list; estimates
are read off the profile above or measured on their own, as said. Items carried from round 3
keep their old name in brackets.

### R4-1. The write runs on one thread — 82% of a 10M-row stream's path, a third to a half of a file's (measured: not done, it would break REQ-15)

`src/source.rs`, `write_table` (`SET threads = 1`, then `CREATE TABLE … AS`), and a stream's
`Ingest` (`src/source.rs`, `stream.insert`): DuckDB's write — compression analysis, statistics,
the uncompressed local segment read back to compress (R2-9) — is 14.0–14.5 G and 2.14 s of
ch-10m's 2.61 s; 221 ms of csv-1m's 673 ms, 262 ms of parquet-1m's 518 ms, 2.38 s of
csv-10m's 4.39 s. It is on one thread because a parallel write lays the file out by how the input
was split among threads, which would tell a CSV from a Parquet file (REQ-15,
`the_physical_layout_says_nothing_of_where_data_came_from`).

Measured with DuckDB 1.5.5 alone (Python package, storage v1.3.0, the harness's rows staged in
memory, then `CREATE TABLE … AS` into a file):

| rows | the stage filled from | 1 thread | 4 threads | layout on 4 threads |
|---:|---|---:|---:|---|
| 1M | Parquet, CSV, Arrow | 332–337 ms, 9 row groups, 4,468,736 B, the same three ways | 171–210 ms | Parquet: as on 1 thread; CSV: 10 row groups, 4,206,592 B; Arrow: 9, 4,206,592 B |
| 10M | Parquet | 2.63–2.73 s, 82 row groups, 42,741,760 B | **0.87–0.89 s (−67%)** | 90 row groups, 42,217,472 B: the same on 2, 3, 4 and 8 threads, in six runs, and with six busy loops beside it |
| 10M | CSV, Arrow | 2.62–2.75 s, 82 row groups, the same | 0.90–0.94 s | 101 row groups (CSV), 87 (Arrow): different from Parquet's |

So a parallel write is three times as fast at 10M rows and its layout does not depend on the
thread count or the scheduling, but it does depend on **how the stage was filled**: DuckDB's
batch insert keeps the stage's row-group boundaries, and those are where each reader's threads
split the input. One thread's write is the same whatever filled the stage.

- **Idea, to be measured in benchy first:** make the stage canonical, then write it on every
  thread. Every source fills a stage whose row groups are full and in order — a stream already
  arrives in order on one thread; a file read on every thread is copied once on one thread into
  a second, full-row-group stage, or appended in order — and the write reads that stage with
  `threads` at the machine's count. Every origin then makes the 90-row-group layout of the
  table above, which the layout tests must show (they would fail today if only files wrote in
  parallel). A stream would need a stage too (held to `STAGE_MEMORY`, spilling past it, as a
  CSV's is), where it now goes into its table batch by batch.
- **Estimate:** the write's wall time divided by about 2.5–3 on four cores, less the stage's
  fill: ch-10m and exec-10m about −1.0 to −1.3 s first `describe` (−40%), csv-10m about −1.5 s,
  csv-1m and parquet-1m −80 to −150 ms; the instructions up by the stage's fill and copy (to be
  measured); a stream's loader memory up to the stage's cap (about 380 MB at 10M rows, as
  csv-10m) and a spill to disk bounded by the input. Not certain: whether a canonical stage can
  be had cheaply enough, and the 1-core case (`threads = 1` writes the 82-row-group layout, so
  the layout would differ between machines, not between origins).

**Measured, 2026-10-12: not done.** Every parallel write found makes the same row groups, segments
and compression from every source, but none keeps the blocks: DuckDB 1.5.5 numbers a block when
a thread writes it, so the `block_id` of a segment, which the agent reads from
`pragma_storage_info` as it reads the counts, depends on which thread finished first, and
changed from one load of the same file to the next. REQ-15 is the same layout on every load, and
it stays on one thread. The layout tests now compare every segment's `block_id` and
`block_offset` too (they passed before this round, so nothing about the write changed), and they
fail on a parallel write that the counts alone let through: with `write_table`'s `SET threads =
1` taken out, `a_files_layout_is_the_same_on_every_load` loaded one NDJSON file twice and got
the same segments, sizes and compression with two `ts` segments in each other's blocks, and
`an_http_table_has_the_physical_layout_of_a_file_table` found a CSV file and the same file over
HTTP alike in everything but their blocks. On the write as it is, the three passed 60 runs of 60, two loops at
once with two busy loops beside them, at load average 5 to 11.

*DuckDB alone* (the Python package, 1.5.5, storage v1.3.0, `write_buffer_row_group_count = 1`,
256 MiB, the harness's 10,000,000 rows, four busy cores, load 2 to 6; one to three runs each):

| how the write is fed | the write | what the agent can see |
|---|---:|---|
| one thread, as now, from Parquet, a CSV's stage or a stream | 2.6–3.4 s (once 4.2 s) | 82 row groups, 163 blocks, 40.7 MiB; block for block the same from every source and on every run |
| DuckDB's parallel insert from the stage as the reader filled it | 0.87–0.94 s | 101 row groups from CSV, 90 from Parquet, 87 from Arrow (above) |
| the same, from a stage copied once on one thread so its row groups are full and in order (copy 1.3–1.8 s), or from DuckDB's Arrow scan of batches cut to 122,880 rows | 0.9–1.8 s (Arrow: a stream 1.3–1.6 s, Parquet 1.2–1.6 s, a CSV read through a second connection 4.4 s) | 82 row groups and 164 blocks from every source; but see the packing below |
| the Arrow scan of the stream's own 65,536-row batches | 1.6–2.0 s | 87 row groups, 152 blocks, and different segments from one run to the next |
| **fixed groups**: `threads = 1`, four connections on four threads of benchy's own, each appending a group of 4 row groups (491,520 rows) in a transaction of its own, committed in group order | **1.3–1.4 s** from a stream, 1.6–1.8 s from a CSV's stage (`rowid` ranges) | 82 row groups and 164 blocks from every source, on every run; **the blocks the segments lie in differ from run to run** |
| one row group per transaction, compressed as it commits, in order | 3.9 s (2.4 s with the file in memory) | the same blocks on every run: compression is on one thread at a time again, and each commit syncs the file |

Why, from DuckDB 1.5.5's source: its parallel insert (`physical_batch_insert.cpp`) gives each
thread an optimistic writer whose partial-block manager takes the tails of every row group that
thread compresses (`OptimisticDataWriter::Merge`, `PartialBlockManager::Merge`), so the packing
of partial blocks follows the threads' schedule: the 840,000-row layout fixture took 23 blocks on
one thread, 24 on two to four and 25 on eight, and 13 row groups whose tails differ in size took
24, 25 or 26 blocks over 30 runs on four threads (25 or 26 on two). With fixed groups each
transaction packs only its own tails, flushed when it commits (`LocalStorage::Flush`), so the
count is the same every time (20 runs of 20); but a full block takes its number
(`GetFreeBlockId`) when its thread writes it, and four placements came out of ten runs. A
placement that follows the threads' timing would not only be noise: how far the groups overlap
depends on how fast the rows arrive, which a stream from a network and a file on disk need not
share (reasoned from the above, not measured).

*In benchy*, the fixed groups prototyped for a stream's `Ingest` and a staged CSV's write (not
merged), against `main` at `57e8901`, both built from their commits, interleaved run by run,
ClickHouse on its own port pair, load average 2.7 to 6.2; minima, and the first `describe` also as
the median; 6 runs per 10M scenario, 9 per 1M:

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| exec-10m | 16.31 → 16.34 G (+0.2%) | 3.42 → 3.28 s (−4.1%) | 102 → 138 MB | 3.60 / 3.79 → 2.75 / 3.03 s (−24% / −20%) |
| exec_series-10m | 17.99 → 18.03 G (+0.2%) | 3.48 → 3.49 s | 106 → 140 MB | 3.42 / 3.82 → 2.68 / 2.95 s (−22% / −23%) |
| ch-10m | 17.30 → 17.30 G | 2.99 → 3.43 s (+15%) | 96 → 131 MB | 2.98 / 3.27 → 2.85 / 3.20 s (−4% / −2%) |
| csv-10m | 49.80 → 50.45 G (+1.3%) | 8.29 → 8.76 s (+5.7%) | 395 → 412 MB | 4.50 / 5.63 → 3.37 / 5.36 s (−25% / −5%) |
| csv-1m | 5.43 → 5.47 G (+0.7%) | 1.06 → 1.09 s | 115 → 117 MB | 847 / 970 → 653 / 703 ms (−23% / −28%) |
| exec-1m | 2.03 → 2.06 G (+1.6%) | 521 → 545 ms | 60 → 76 MB | 540 / 592 → 457 / 502 ms (−15% / −15%) |
| exec_series-1m | 2.23 → 2.26 G (+1.5%) | 601 → 590 ms | 60 → 76 MB | 561 / 593 → 463 / 534 ms (−17% / −10%) |
| ch-1m | 2.18 → 2.20 G (+1.1%) | 513 → 555 ms (+8%) | 64 → 76 MB | 490 / 569 → 530 / 581 ms (+8% / +2%) |
| parquet-1m (not changed: the noise) | 2.47 → 2.47 G | 598 → 588 ms | 62 → 62 MB | 512 / 585 → 464 / 515 ms (−9% / −12%) |

So what a parallel write would buy, at this load: about a fifth off a 10M-row connector's first
`describe`, little for ClickHouse, whose server shares the four cores and whose fetch then sets
the pace, a quarter off csv-10m's best run and 5% off its median, and 35 MB more in a stream's
loader (four row groups in flight); parquet-1m, whose path the prototype did not touch, moved 9–12%
by the machine alone. With the prototype, `an_http_table_has_the_physical_layout_of_a_file_table`
passed on the counts, row groups and segments of a CSV file and its HTTP body, and failed on
their blocks.

- **What would let it be done:** DuckDB numbering a table's blocks in row-group order, however
  many threads write them (at commit, say), or a way to write a row group into blocks given in
  advance. With that, the fixed groups above are the design: no stage, memory bounded by the
  groups in flight, and the same layout from every source on every load and on every machine
  (the groups do not depend on the thread count). A narrowed stream would need its rows filtered
  before they are grouped, or a group would be short where rows were dropped.

### R4-2. A JSON file over 64 MiB is read straight in, on one thread — 6.6 s for 10M rows (done: every JSON file staged)

`src/source.rs`, `STAGE_JSON_UP_TO` (64 MiB, R2-1): a larger JSON or NDJSON file, and an HTTP
body spooled above it (R3-3), is read by DuckDB's JSON reader on the write's one thread, and its
watch runs after the write (`watched`). Measured with the harness's rows as a 10M-row NDJSON file
(633 MB), three runs on `5993d6b`: first `describe` 6.63 / 7.54 s (minimum / median),
`write_table` 5.79 s on one thread, then `file.watch` 486 ms; 42.3 G instructions; loader
133 MB. The same rows as a CSV, staged: 4.39 s.

The 64 MiB bound was set to bound the stage's memory (the stage held the whole table). Since
the stage cap (round 4) a stage is held to `STAGE_MEMORY` and spills past it, so the bound no longer buys memory
that grows with the file.

- **Idea:** stage every JSON file as a CSV is (raise `STAGE_JSON_UP_TO` to no limit, or drop it),
  read on every thread, the watch beside the write.
- **Estimate:** a 10M-row NDJSON file's first `describe` from 6.6 s to about csv-10m's 4.4 s (−2.2
  s, −33%); the loader's peak from 133 MB to the cap's (384 MB at csv-10m) and a
  spill of about 140 MB, bounded by the file; instructions about the same (a staged NDJSON file took
  4% fewer at 1M rows in R2-1; the spill cost a CSV 4.5% more at 10M). The trade is the owner's rule
  (first `describe` over load-time memory, memory bounded), and
  `a_json_file_is_laid_out_alike_staged_or_read_straight_in` keeps the layout.
- **Done (2026-10-12, owner's decision).** `STAGE_JSON_UP_TO` is gone: a JSON or NDJSON file, and
  an HTTP NDJSON body once spooled, is staged whatever its size, read on every thread into a stage
  held to `STAGE_MEMORY` (256 MiB) and spilling past it, and watched beside the write
  (`a_json_file_is_staged_whatever_its_size`). R3-1's timestamps read as text, and R3-5's declared
  read, are what fill the stage, as before. With no JSON read straight in, the layout test that
  compared the two ways (`a_json_file_is_laid_out_alike_staged_or_read_straight_in`) had nothing
  left to compare, and is gone; a staged JSON file's layout against every other source's is
  `the_physical_layout_says_nothing_of_where_data_came_from`'s, as before. Measured with
  `bench/ingest.py`, `57e8901` and the change interleaved run by run, minimum / median, load
  average 2 to 5; 3 runs each at 10M, 6 at 1M, 10 at 1k:

  | scenario | first `describe` | instructions | loader CPU | loader peak RSS | loader wrote |
  |---|---:|---:|---:|---:|---:|
  | ndjson-10m | 6,665 / 6,884 → **4,587 / 5,525 ms (−31% / −20%)** | 42.25 → 51.10 G (+21%) | 6.58 → 8.71 s | 132 → 351 MB | 41 → 175 MB |
  | ndjsondecl-10m | 6,520 / 6,953 → **3,810 / 4,369 ms (−42% / −37%)** | 40.25 → 48.19 G (+20%) | 6.40 → 7.74 s | 132 → 337 MB | 41 → 163 MB |
  | http-ndjson-10m | 8,068 / 8,568 → **5,839 / 6,004 ms (−28% / −30%)** | 42.73 → 51.68 G (+21%) | 7.63 → 9.61 s | 134 → 355 MB | 644 → 831 MB |
  | ndjson-1m, ndjsondecl-1m, http-ndjson-1m | unchanged, staged before (697 / 761 → 709 / 753 ms, …) | within ±1.6% | | 141–143 MB | |
  | ndjson-1k, http-ndjson-1k, ndjsondecl-1k | unchanged (197 / 218 → 201 / 223 ms, …) | within ±1% | | 48 MB | |

  The first `describe` came as estimated, for about a fifth more instructions where the estimate
  said about the same: profiled (`--record`, ndjson-10m), the parse into the stage on four
  threads costs 28.1 G where the parse and write on one cost 33.1 G, and the write from the stage
  14.1 G more, the stage's own appends, statistics and spill (`StringAppend`, `StringStats::Update`,
  zstd for the spilled blocks) being the difference, as a CSV's stage costs; the watch, scanning
  the stage's converted `SELECT` rather than the table, 2.8 → 3.8 G, off the path. The loader's peak
  is the stage's cap, as csv-10m's (384 MB), and the spill (134 MB at ndjson-10m) is removed with
  the stage. What is left on the path is the write's 2.7 s on one thread, which R4-1 found no
  way to parallelize that keeps the blocks.

### R4-3. JSON pages are decoded on the fetch's one thread — 1.02 s of http-json-1m's 1.24 s (R3-9, carried; done: (a), −40%)

`src/sources/http/load.rs`, `pages` and `PageDecoder::decode`: each page is fetched, then decoded
by arrow-json, then the next page's URL taken from it, all on one thread. `http.fetch` 2.50 G and
1.02 s wall for 867 ms of CPU; arrow-json's `TapeDecoder::decode` 46% of it, its struct decoder
9%, timestamps about 11% (`TimestampParser::new` 5.0%, `NaiveDate::from_ymd_opt` 3.0%,
`TimestampParser::date` 1.8%, …). http-json-1m answers in 1.24 s where the same rows as an
NDJSON body take 0.94 s and as Arrow 0.46 s; it was the one 1M scenario round 3 did not move.

- **Ideas:** (a) find the page's cursor or next link without decoding the rows (a scan for the
  cursor's path that skips values), fetch the next page at once, and decode pages on two or
  three threads, joined in page order into the ingest; (b) the RFC 3339 fast path for UTC
  timestamps (R3-9's).
- **Estimate:** (a) the fetch bounded by the network and the cursor scan instead of the decode:
  about −0.5 s first `describe` (−40%) at 1M rows; instructions about the same plus the scan;
  (b) −0.25 G (−5%).
- **Done, (a) (2026-10-12).** Three kinds of thread now share a paged load
  (`src/sources/http/load.rs`, `pages`): a fetch thread asks for each page, scans it for its
  rows and its cursor without decoding them (`src/sources/http/skim.rs`), tells the pager, and
  asks for the next at once; decoder threads (DuckDB's thread count less two, one to three: two
  here) decode the pages with arrow-json; the load's own thread takes the batches into the table
  in page order, so the table is the one a single thread wrote (REQ-15). At most four pages wait
  between the fetch and the table. Each page's decode must read the rows and cursor its scan read,
  or the source fails; a page the scan cannot read plainly (a duplicate or escaped key on the way,
  a row that is not an object, a cursor that is an object) is decoded by the fetch before it goes
  on, as before. A fault still comes in page order, after every page before it went in
  (`json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order`,
  `a_pages_scan_reads_its_rows_and_cursor_as_its_decode_does`). Measured with `bench/ingest.py`,
  `57e8901` and the change interleaved run by run, minimum / median, load average about 4:

  | scenario | first `describe` | instructions | loader CPU | loader peak RSS |
  |---|---:|---:|---:|---:|
  | http-json-1m (12 runs) | 1,131 / 1,174 → **675 / 740 ms (−40% / −37%)** | 5.07 → 5.65 G (+11%) | 937 → 1,061 ms | 63 → 69 MB |
  | http-json-1k (15 runs) | 180 / 192 → 186 / 198 ms (within the noise) | 444 → 440 M | 102 → 103 ms | 49 MB |

  The estimate held. What is on the path now: the load's thread inserting (about 0.35 s at 1M
  rows, `stream.insert`), and beside it the fetch thread's requests (about 0.33 s against this
  harness's server, a Python thread) and its scan (about 0.1 s); the decode is off it (2.25 G on
  two threads, which wait for neither). A first try kept the fetch and the scan on the load's
  thread with the insert, and took nothing off (1,241 → 1,299 ms): that thread was the path,
  so the client is shared with a fetch thread of its own (its counts are atomics now). The scan
  is a table lookup a byte: 601 M instructions for the harness's 63 MB of pages, against 742 M
  with a `match` a byte. The +0.58 G is the scan and the hand-offs. Two decoder threads and three
  measured the same within the noise (980 / 1,500 ms with three, 1,067 / 1,393 with two, load
  average 6–7), so the third is left to a machine with more cores.
- **DuckDB's JSON reader, measured again (DuckDB 1.5.5 alone, the harness's 100 pages spooled one
  to a line, `unnest(data)` with the columns declared, into an in-memory table):** 2.48 G and
  512 ms on one thread, 882 ms on four, where round 0 measured 13–17 G (docs/HTTP-SOURCE.md).
  Now cheaper than arrow-json's decode and benchy's insert together (4.2 G), but it reads only
  once the pages are fetched and spooled, and its table is still to be written: about 1.1 s in
  sequence against 0.68 s pipelined, with the pinned schema and the strict integer cast to redo
  in SQL. arrow-json kept; worth measuring again if R4-1 makes a staged write cheap.

### R4-4. The engine's attach check lists every function — 115 M and 28–31 ms of every session (R3-7, carried)

`src/workspace.rs`, `catalogs_with_functions`: `duckdb_functions()` builds a row for every
built-in overload to find the macros of the attached catalogs. 115 M of every session, 27% of
exec-1k's instructions and 31 ms of its 230 ms to `describe`; with `attach.listings` (55 M,
19–21 ms), the check is 41% of a 1k session's instructions. Measured again in DuckDB 1.5.5 alone:
29.0 / 30.0 ms (minimum / median of 15) whether it selects `DISTINCT database_name` or
`count(*)`; `duckdb_schemas()` and `duckdb_tables()` filtered the same way 0.6 ms;
`information_schema` has no `routines` view in DuckDB.

- **Idea, still to be found:** a probe for an attached catalog's macros that does not enumerate
  the built-ins and still refuses every case in `corpus/escape.toml` and the attach tests. None
  is known; the obvious ones are measured above.
- **Estimate:** up to −115 M and −28 to −31 ms per session (−12% of a 1k session's first
  `describe`), if such a probe exists.

### R4-5. A session's other fixed costs — 230 ms to `describe` at 1k rows, 1% of it benchy's

What exec-1k's 230 ms is, besides R4-4: the server's start (26 ms from spawn to `main`), the
loader's (16 ms), the loader's DuckDB open (`bench.open`, 52 M, 27 ms), the profile's queries
(19 ms; `profile.step` 25 M a table, R3-6), the checkpoint (`bench.finish`, 13 ms), the engine's
attach (56 ms with R4-4). host8-1k (491 ms) pays the open eight times (419 M, eight loaders).

- **Process starts, measured:** 21–23 M instructions each, 85% of it the dynamic linker
  (`do_lookup_x`, `_dl_relocate_object`, `_dl_lookup_symbol_x`) resolving `libduckdb.so`'s
  symbols (`LD_DEBUG=statistics`: 26,994 relocations looked up, 18,294 more from its cache).
  Binding lazily does not help: the binary patched to drop `BIND_NOW`, 23.23 → 23.08 M (−0.7%).
  Linking DuckDB statically is no way out either: the release's `libduckdb_static.a` has no ICU,
  JSON or Parquet extension (`nm`), the same gap as the `bundled` build. **Idea:** start the
  engine and the loader from a process forked at the top of `main`, before the config is read or
  DuckDB exists (as the warden is since R3-11), instead of `exec`: −21 M each (−10% of exec-1k)
  and the loader's 16 ms off the path. Their confinement must hold as tested, and the forked
  process hold nothing of the server's, as `the_warden_holds_nothing_of_the_servers` shows of the
  warden.
- **`profile.step` from the watch (R3-6):** 0.21–0.47 G at 10M rows, 25 M a table, 18–42 ms on
  the path at 1M and 10M. Keep the exact first value and the sample window's times in the `Watch`
  (R3-6's idea). Estimate −0.2 to −0.45 G at 10M, −10 to −40 ms first `describe` per table,
  −190 M at host8-1k.
- **One loader for a host's N tables:** host8-1k opens DuckDB eight times. A host attach
  carries no credential, so one loader could write the N tables of one request: −7 opens
  (−365 M, −24% of host8-1k). Isolation per source (REQ-5) is about credentials, which a host's
  Arrow file has none of; to be checked against REQ-5 and REQ-17 before measuring.

### R4-6. benchy's own CPU, off the path — measured, ranked last

None of these moves the first `describe` today: each runs beside the write or the fetch.

- **The keyed tally's series lookup (R3-4's follow-up).** exec_series-10m's watch: 2.43 G, 243
  instructions a row, against 99 for one series; R3-4's exact count put finding the series at 114.
  On its own thread (496 ms beside a 2.33 s insert). An idea: a dictionary-encoded key is a series
  by its index, found once a batch rather than once a row. The ClickHouse source asks for a
  `LowCardinality` column as plain strings today (`output_format_arrow_low_cardinality_as_dictionary
  = 0`, `src/sources/clickhouse.rs`, `settings`), and a connector's dictionary is cast to its values
  before the write (`src/source.rs`); asking for the dictionary and casting only for the write would
  let the watch find a series by index. Estimate −1 G at exec_series-10m (−6%) where the key is a
  dictionary, less the cast, nothing otherwise; CPU only, and fewer bytes on the wire. With R4-1
  staging streams, the watch could also read the stage, as a file's does.
- **The one-series watch at +3% (R3-4):** 987 M at exec-10m against 961 M on `99b5155`, 26 M
  per 10M rows (0.16%). Not worth chasing.
- **Reading DuckDB's chunks in place for a file's watch (R3-2 b):** the conversion to Arrow is
  about 165 M per 1M rows of the CSV (`StringVector::AddStringOrBlob`, `ArrowVarcharData::Append`,
  `NextPowerOfTwo`, `ArrowBuffer::reserve`, `memcpy`: 55% of `file.watch`) and about 150 M of
  Parquet's. −0.15 G per 1M rows of a file (−3% of csv-1m), CPU on the watch's thread.
- **Parquet's second read (R3-2):** the file read again beside the write, its decode about 0.11 G
  per 1M rows (`RawUncompress` 18% of `file.watch`, `RleBpDecoder`, the column reader). With R4-1's
  stage the watch could read the stage instead; on its own, nothing to take that is not on the path.
- **Arrow's per-offset UTF-8 validation (R3-8):** about 0.32 G per 10M rows (2.1% of exec-10m's
  main thread). DuckDB's `StringStats::Update` analyses every string again
  (`Utf8Proc::Analyze`, 0.74 G at exec-10m), so the bytes are checked twice; benchy's check is the
  one that could go, with the malformed-stream tests R3-8 asks for. −0.3 G, −2%.

### Measured, nothing to take

- **A text key costs half of a stream's write.** exec_intkey-10m (the same rows, `meter` an
  integer) writes in 6.67 G where exec-10m's text key takes the write to 14.02 G:
  `StringStats::Update` 9.8%, `DictFSSTCompressionState::CompressInternal` 8.6%,
  `UncompressedStringStorage::StringAppend` 7.0% and `StringScanPartial` 4.6%,
  `Utf8Proc::Analyze` 5.3%, `Hash<string_t>` 3.4% of exec-10m's `stream.insert`. That is DuckDB's
  per-row work on the column's type, which origin-blindness fixes (R2-9 measured every setting).
- **Lazy binding, and a static DuckDB:** R4-5.

### Is another round worth it?

Rounds 1 to 3 took 49% of ch-10m's instructions and 44% of its first `describe`, 41% and 31% of
exec-10m's, 33% and 41% of parquet-1m's. What is left of benchy's own code is off the path,
except JSON pages (R4-3). Another round of tuning benchy's own code would move CPU, not
latency: R4-6 adds up to about 1.3 G at 10M rows of 500 series and 0.3 G of one.

The wall time that remains is how benchy drives DuckDB, and three items are worth a round:
R4-1 (the write on one thread: up to −40% at 10M rows, if a canonical stage keeps REQ-15 — measured,
no parallel write does: see R4-1),
R4-2 (−33% for a large JSON file, nearly certain) and R4-3 (−40% for JSON pages). After them
a session's floor is DuckDB's: two opens, a checkpoint, the attach check and three process
starts, about 200 ms, of which R4-4 and R4-5 could take 50–70 ms. Past that, the work is DuckDB's
own per row (statistics, compression analysis, string handling) and would need DuckDB to change:
a C API table function that reports batch indices (so a stream could be written in parallel in
order without a stage), blocks numbered in row-group order however many threads write them (R4-1),
and a way to list one catalog's functions.

## Baseline: round 3, `99b5155`

`main` at `99b5155` (R2-1 to R2-8 merged), against round 2's baseline binary, `d4afdaa`
(`ef3a0cf` plus a docs-only change), 2026-10-11. Both built from their own commits and run
interleaved run by run (the old, then the new, then the new, then the old, …) in the same
sessions on the same data, load average 1.7 to 7.7. Minima of 10 runs per 1M scenario, 6 per
10M, 15 per 1k; the first `describe` also as the median. `main` as merged passed
`BENCHY_CLICKHOUSE_BIN=… scripts/ci.sh` with no ClickHouse test skipped, and
`benchy-mcp doctor --probe` said this machine enforces REQ-3.

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| csv-1m | 6.21 G → 5.80 G (−6.7%) | 1.14 → 1.06 s (−7.3%) | 117 → 117 MB | 776 / 864 → 762 / 796 ms (−2% / −8%) |
| parquet-1m | 3.15 G → 2.72 G (−13.4%) | 712 → 598 ms (−16.0%) | 71 → 60 MB | 613 / 647 → 562 / 635 ms (−8% / −2%) |
| ndjson-1m | 6.79 G → 6.56 G (−3.4%) | 1.19 → 1.20 s (+1.0%) | 99 → 143 MB (R2-1) | 1.12 / 1.22 s → 860 / 950 ms (−24% / −22%) |
| exec-1m | 2.67 G → 2.42 G (−9.6%) | 615 → 577 ms (−6.3%) | 70 → 58 MB | 654 / 735 → 620 / 685 ms (−5% / −7%) |
| exec_series-1m | 2.98 G → 2.72 G (−8.6%) | 667 → 646 ms (−3.3%) | 71 → 57 MB | 658 / 738 → 584 / 664 ms (−11% / −10%) |
| exec_lz4-1m | 2.83 G → 2.49 G (−12.1%) | 663 → 599 ms (−9.6%) | 71 → 58 MB | 637 / 676 → 541 / 589 ms (−15% / −13%) |
| ch-1m | 2.77 G → 2.53 G (−8.8%) | 622 → 576 ms (−7.4%) | 73 → 62 MB | 597 / 616 → 532 / 595 ms (−11% / −3%) |
| host-1m | 2.72 G → 2.46 G (−9.7%) | 574 → 531 ms (−7.6%) | 71 → 57 MB | 599 / 645 → 536 / 556 ms (−10% / −14%) |
| exec-10m | 23.03 G → 19.87 G (−13.7%) | 4.61 → 3.99 s (−13.4%) | 116 → 101 MB | 4.41 / 4.66 → 3.67 / 3.98 s (−17% / −15%) |
| exec_series-10m | 24.90 G → 22.69 G (−8.9%) | 4.68 → 4.38 s (−6.2%) | 117 → 101 MB | 4.65 / 4.83 → 3.95 / 4.26 s (−15% / −12%) |
| exec_lz4-10m | 24.76 G → 20.78 G (−16.1%) | 4.56 → 3.96 s (−13.3%) | 116 → 102 MB | 4.37 / 4.77 → 3.92 / 4.31 s (−10% / −10%) |
| ch-10m | 23.72 G → 20.54 G (−13.4%) | 3.84 → 3.43 s (−10.5%) | 102 → 94 MB | 3.68 / 4.15 → 3.29 / 3.43 s (−11% / −17%) |
| http-csv-1m | 6.25 G → 5.84 G (−6.5%) | 1.23 → 1.12 s (−8.8%) | 115 → 116 MB | 1.11 / 1.20 → 1.08 / 1.12 s (−3% / −6%) |
| http-csvdecl-1m | 6.26 G → 5.66 G (−9.5%) | 1.23 → 1.12 s (−9.2%) | 116 → 112 MB | 1.11 / 1.22 → 1.07 / 1.18 s (−4% / −4%) |
| http-parquet-1m | 3.18 G → 2.77 G (−12.9%) | 707 → 647 ms (−8.5%) | 71 → 61 MB | 672 / 733 → 645 / 712 ms (−4% / −3%) |
| http-ndjson-1m | 7.04 G → 6.63 G (−5.7%) | 1.39 → 1.36 s (−2.7%) | 173 → 172 MB | 1.42 / 1.58 → 1.41 / 1.59 s (−1% / 0%) |
| http-arrow-1m | 2.69 G → 2.42 G (−9.9%) | 596 → 552 ms (−7.4%) | 71 → 58 MB | 581 / 629 → 505 / 586 ms (−13% / −7%) |
| http-json-1m | 5.70 G → 5.46 G (−4.3%) | 1.18 → 1.12 s (−5.3%) | 71 → 61 MB | 1.28 / 1.34 → 1.21 / 1.30 s (−6% / −3%) |
| csv-1k | 1.00 G → 986 M (−1.7%) | 384 → 374 ms (−2.7%) | 52 → 52 MB | 354 / 400 → 352 / 389 ms (−1% / −3%) |
| parquet-1k | 507 M → 488 M (−3.7%) | 231 → 221 ms (−4.0%) | 50 → 50 MB | 221 / 240 → 205 / 235 ms (−7% / −2%) |
| ndjson-1k | 553 M → 514 M (−7.0%) | 234 → 229 ms (−1.9%) | 49 → 49 MB | 203 / 212 → 189 / 207 ms (−7% / −2%) |
| exec-1k | 447 M → 438 M (−2.0%) | 223 → 218 ms (−2.0%) | 49 → 49 MB | 203 / 222 → 205 / 219 ms (+1% / −1%) |
| exec_series-1k | 446 M → 437 M (−2.0%) | 221 → 219 ms (−1.1%) | 48 → 49 MB | 214 / 224 → 205 / 217 ms (−4% / −3%) |
| exec_lz4-1k | 448 M → 442 M (−1.2%) | 222 → 211 ms (−5.1%) | 48 → 48 MB | 230 / 251 → 192 / 242 ms (−17% / −4%) |
| ch-1k | 447 M → 439 M (−1.6%) | 214 → 210 ms (−1.9%) | 49 → 49 MB | 218 / 243 → 222 / 237 ms (+2% / −2%) |
| host-1k | 506 M → 500 M (−1.2%) | 193 → 195 ms (+1.1%) | 49 → 49 MB | 246 / 264 → 251 / 268 ms (+2% / +2%) |
| host8-1k | 2.49 G → 1.59 G (−36.2%) | 1.10 s → 849 ms (−23.1%) | 49 → 49 MB | 1.17 / 1.34 s → 429 / 547 ms (−63% / −59%) |
| http-csv-1k | 995 M → 982 M (−1.3%) | 371 → 356 ms (−4.0%) | 54 → 53 MB | 344 / 371 → 332 / 372 ms (−3% / 0%) |
| http-csvdecl-1k | 993 M → 562 M (−43.4%) | 363 → 250 ms (−31.3%) | 53 → 53 MB | 343 / 357 → 233 / 252 ms (−32% / −30%) |
| http-parquet-1k | 510 M → 501 M (−1.8%) | 243 → 240 ms (−1.3%) | 51 → 51 MB | 216 / 231 → 216 / 229 ms (0% / −1%) |
| http-ndjson-1k | 524 M → 507 M (−3.2%) | 234 → 233 ms (−0.3%) | 50 → 50 MB | 213 / 228 → 212 / 225 ms (0% / −1%) |
| http-arrow-1k | 443 M → 435 M (−1.8%) | 200 → 207 ms (+3.6%) | 49 → 49 MB | 185 / 198 → 180 / 195 ms (−3% / −2%) |
| http-json-1k | 484 M → 477 M (−1.5%) | 221 → 218 ms (−1.3%) | 50 → 50 MB | 198 / 212 → 197 / 214 ms (−1% / +1%) |

ClickHouse's own CPU to serve ch-10m: 0.96 → 0.82 s. The engine stays at 44 MB and the server at
18 ms of CPU in every scenario.

Since round 1's start, `22ff87e`, run interleaved with both in the same sessions, the same
minima (10 runs at 1M, 6 at 10M):

| scenario | instructions | CPU, all processes | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| ch-10m | 34.10 G → 20.54 G (−39.8%) | 5.41 → 3.43 s (−36.6%) | 109 → 94 MB | 4.69 / 5.46 → 3.29 / 3.43 s (−30% / −37%) |
| exec-10m | 27.79 G → 19.87 G (−28.5%) | 5.38 → 3.99 s (−25.9%) | 114 → 101 MB | 4.53 / 5.24 → 3.67 / 3.98 s (−19% / −24%) |
| parquet-1m | 3.65 G → 2.72 G (−25.5%) | 758 → 598 ms (−21.1%) | 101 → 60 MB | 771 / 879 → 562 / 635 ms (−27% / −28%) |
| csv-1m | 6.25 G → 5.80 G (−7.2%) | 1.17 → 1.06 s (−9.3%) | 117 → 117 MB | 904 / 927 → 762 / 796 ms (−16% / −14%) |
| ndjson-1m | 7.00 G → 6.56 G (−6.4%) | 1.27 → 1.20 s (−5.7%) | 142 → 143 MB | 956 ms / 1.11 s → 860 / 950 ms (−10% / −15%) |

What the tables say beyond the wins:

- **No regression in the harness.** Every scenario's instructions fell or held; the 1k
  sessions moved within their spread (±2%).
- **A wider file's first `describe` is later, by 4%** (R3-2 below). `examples/capacity.py`'s
  CSV (four columns, two of them text) read 4.08 s at 5,000,000 rows on `ef3a0cf` and 4.50 s now.
  Interleaved with `d4afdaa`, five runs each: 3,620 / 4,075 → 3,779 / 4,195 ms (minimum /
  median, +4.4% / +2.9%) for 3.9% fewer instructions. The profile took 184 ms, its scan and pass
  on every thread; since R2-4 the `Watch` reads the table once on the load's one thread, after the
  write, in 333 ms, and the rest of the profile 18 ms. At 1M rows of the harness's three columns
  it is a wash: 54 ms of profile before, 45 + 12 ms now.
- **An HTTP CSV or NDJSON body still answers later than the same file**: 1.08 s against 762 ms
  for the CSV, 1.41 s against 860 ms for the NDJSON (R3-3).
- **The HTTP CSV loader's memory (R2-11)** is lower since R2-8, and still above the file's:
  131 MB median (116–132) against 117 MB, where `d4afdaa` had 148 MB (115–149).
- **An NDJSON file's loader is at 143 MB** (162 MB on two runs of ten), from 99 MB: R2-1's stage,
  bounded by its 64 MiB threshold, as the owner decided.

### Where the instructions go, on `99b5155`

From `--record` (one run each, the binary built with symbols), in instructions:

| stage | csv-1m | parquet-1m | ndjson-1m | http-ndjson-1m | http-json-1m | exec-10m | exec_series-10m | ch-10m | csv-1k | exec-1k | host8-1k |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| reading a file or body (`file.read`) | 3.15 G | | 3.93 G | 3.98 G | | | | | 483 M | | |
| writing the table (`write_table`; `stream.insert` + `stream.commit`; `loader.load`) | 1.79 G | 1.94 G | 1.84 G | 1.78 G | 1.81 G | 17.91 G | 17.64 G | 18.18 G | 62 M | 6 M | 72 M |
| the profile kept as rows go in (`stream.watch`, `file.watch`) | 306 M | 305 M | 306 M | 304 M | 94 M | 961 M | 3.86 G | 966 M | 1 M | | 1 M |
| the profile read from the table (`profile.*`) | 86 M | 89 M | 104 M | 109 M | 107 M | 271 M | 247 M | 468 M | 43 M | 34 M | 410 M |
| the fetch: pipe, HTTP, decode, checks (`exec.stream`, `builtin.stream`, `http.fetch`, `arrow.stream`) | | | | 58 M | 3.00 G | 353 M | 359 M | 544 M | | 1 M | 7 M |
| the engine's attach check (`attach.*`) | 173 M | 172 M | 171 M | 173 M | 172 M | 173 M | 172 M | 174 M | 173 M | 172 M | 210 M |
| opening DuckDB (`bench.open`, `open_sealed`) and the checkpoint (`bench.finish`) | 119 M | 134 M | 122 M | 121 M | 120 M | 133 M | 130 M | 139 M | 128 M | 129 M | 607 M |
| starting processes (outside any stage) | 84 M | 84 M | 84 M | 83 M | 85 M | 84 M | 88 M | 86 M | 85 M | 84 M | 249 M |
| **the loader, all of it** | **5.42 G** | **2.43 G** | **6.27 G** | **6.32 G** | **5.09 G** | **19.59 G** | **22.20 G** | **20.26 G** | **679 M** | **133 M** | **1.21 G** |
| **every process** | **5.71 G** | **2.73 G** | **6.56 G** | **6.62 G** | **5.39 G** | **19.89 G** | **22.50 G** | **20.56 G** | **978 M** | **431 M** | **1.61 G** |

### What is DuckDB's, and what is benchy's

By library, over every process (`--record`, the same runs):

| scenario | DuckDB (`libduckdb.so`) | libc (mostly DuckDB's `memcpy`, `malloc`) | the dynamic linker | benchy's own code |
|---|---:|---:|---:|---:|
| ch-10m | 82.5% | 7.6% | 0.4% | 9.2% |
| exec-10m | 82.3% | 8.3% | 0.4% | 8.7% |
| exec_series-10m | 70.8% | 7.8% | 0.4% | 20.6% |
| parquet-1m | 82.1% | 9.9% | 3.3% | 3.7% |
| csv-1m | 88.6% | 7.2% | 1.6% | 1.8% |
| ndjson-1m | 89.6% | 6.6% | 1.4% | 1.6% |
| http-json-1m | 39.8% | 9.0% | 1.7% | 48.6% (arrow-json) |
| exec-1k | 57.8% | 17.0% | 20.2% | 0.7% |

Split by who decides the work:

- **The load itself, DuckDB's**: writing the table (17.6–18.2 G of a 10M-row stream, 85–89%;
  1.8–1.9 G per 1M rows of a file) and DuckDB's readers (CSV 3.15 G, NDJSON 3.93 G per 1M rows).
  Every setting that touches it was measured in round 2 (R2-9); nothing left to take short of
  changing DuckDB. FSST stays on (owner's decision).
- **DuckDB work benchy asks for**, and could ask for less of: the profile's queries
  (`profile.*`: 0.25–0.47 G at 10M, 26 M per 1k table), the file watch's conversion to Arrow
  (about 0.2 G per 1M file rows), the engine's attach check (172 M per session), the opens and
  the checkpoint (120–140 M per session), and how a reader is asked (an NDJSON file's timestamps:
  about 1.2 G per 1M rows, R3-1; a CSV's sniffing: about 0.4 G per table, R3-5). About 4% of
  ch-10m, 24% of parquet-1m, 23% of ndjson-1m, and 77% of an exec-1k session.
- **benchy's own code**: the profile kept as rows go in (`Watch::add`: 1.0 G of one series, 3.9 G
  of 500 series, per 10M rows), Arrow decoding and its checks (0.35–0.54 G per 10M), the batches
  into DuckDB's vectors (`BatchScan::func`, about 0.4 G per 10M), and JSON pages (arrow-json,
  2.6 G per 1M rows).

**The headroom in benchy's own code**: at 10M rows of one series about 1.9 G of 20.5 G (9%), of
which R3-8 could take about 0.3 G (1.3%), and the one-series watch, at 98 instructions a row since
R2-2, little more; at 500 series 4.6 G of 22.5 G (20%), of
which R3-4 could take about 1.8 G (8%); for files at 1M rows under 0.1 G. Past that, the wins are
in what benchy asks DuckDB to do — how it reads (R3-1, R3-5), what it queries (R3-6, R3-7) and on
which thread (R3-2, R3-3) — and in the fixed cost of a session: at 1k rows, benchy's own code is
0.7% of the instructions, and the rest is DuckDB's opens, the attach check and process starts.

## Round 3: the backlog, ranked

Ranked by what each could take off the scenarios above — first `describe` before instructions,
since the owner ranks it first — then by how sure the estimate is. "R3-n" names an item of this
list. Every estimate is read off the profile or measured on its own, as said; judge each change
with `bench/compare.py`, before and after, interleaved.

### R3-1. An NDJSON body's timestamps are parsed by format — about 1.2 G per 1M rows (done: −0.64 G)

`src/source.rs` (`reader_for`, `read_json_auto`), the same for an HTTP NDJSON or JSON body.
DuckDB's JSON reader detects a timestamp format while sniffing and parses every value with it:
`StrpTimeFormat::Parse` is 28% of an NDJSON file's `file.read` (3.93 G at 1M rows), and the
sniffed type is `TIMESTAMP`, which benchy then converts. Measured with DuckDB 1.5.5 alone (its
Python package, one thread, `perf stat`, less an empty connection) on the harness's 1M-row
NDJSON:

| reader | instructions | time, one thread |
|---|---:|---:|
| `read_json_auto(f)`, what a file gets | 3.94 G | 605–638 ms |
| `read_json(f, columns = {ts: 'VARCHAR', …})`, then `ts::TIMESTAMPTZ` | 2.70 G (−31%) | 410–425 ms |
| `read_json(f, columns = {ts: 'TIMESTAMPTZ', …})` | 2.72 G (−31%) | 410–420 ms |

- **Ideas:** (a) undeclared: sniff as now (`DESCRIBE`), then read with the sniffed `columns`, each
  sniffed timestamp column as `VARCHAR` and cast, which takes DuckDB's ISO fast path; if any value
  does not cast (`TRY_CAST` nulls where the text was not null), fall back to the sniffed reader, so
  a format only the sniffer reads still loads. The table must be the same, byte for byte
  (`the_physical_layout_says_nothing_of_where_data_came_from`, the origin tests). (b) declared
  (R3-5): `columns` with the declared types, no sniff at all.
- **Estimate:** −1.2 G per 1M rows (−18% of ndjson-1m and http-ndjson-1m); first `describe` about
  −90 ms for the file (its read, 310 ms on four threads, a third shorter) and about −250 ms for
  an HTTP body (756 ms on one thread, until R3-3).

**Done (2026-10-12) for NDJSON files**, as (b) more than (a): the file is sniffed once
(`DESCRIBE`), then read with the sniffed types as `columns`, each top-level `TIMESTAMP` as
`VARCHAR`, and cast (`src/source.rs`, `text_timestamps`). `columns` turns the sniffer off, so the
read does not sniff a second time; (a)'s second read, the sniffer told no format would match,
sniffed again for 0.15 G and took most of the gain. DuckDB alone, one thread, at 1M rows:
3.94 G as before, 3.06 G this way, the `DESCRIBE` included; 237 → 222 M at 20,000 rows.

- **The cast is held to what is cheap to check.** A per-row check of the text's shape cost what
  the cast saved (a regular expression +1.0 G, `LIKE` +0.8 G per 1M rows). Instead: cast to
  `TIMESTAMP_NS`, which applies an offset as the sniffer's `%z` does (a cast to `TIMESTAMP` drops
  it) and fails outside 1677–2262, where a two-digit year read by `%y` would land; text of 19
  bytes or more (`epoch`, which the sniffer's formats read as 1900-01-01, and a date alone fail
  it); whole microseconds. A value that fails any of these has the file read the sniffed way, to
  its table or its refusal: never a null, never a rounded time. Checked differentially against
  the sniffed read on 951 generated files, each odd value at a vector's first row and inside one:
  no value read differently. A value the sniffer's formats refuse and the cast reads exactly —
  an offset in a vector that began without one, `24:00:00`, a field without its leading zero —
  now loads at the instant its text says, where the sniffed read refused the file.
- **Where `columns` would read otherwise, the file is read as before**: a `DATE` column (cast,
  not read by format: `26-01-02` the year 26), a struct (a key missing from the sample dropped,
  not refused), timestamps in a list, two keys differing in case (`Id_1`), and `.json` files,
  whose format the sniffer finds.
- **An undeclared HTTP NDJSON body takes the same path**, now that R3-3 spools it to a file
  (`source::load_read`); a declared one names its `columns` already. **Not done: JSON pages**,
  decoded by arrow-json, not DuckDB's reader: their timestamps are R3-9's.

- **Files under 1 MiB are read as before** (`TEXT_STAMPS_FROM`): the `DESCRIBE` costs about what
  the read's own sniff did, plus a second plan and open of the file, and at 1,000 rows the session
  took 26 M more instructions (+5%) that way; at 20,000 rows (1.3 MB) DuckDB alone read it in 15 M
  fewer.

Against `edd72a4` (R3-3 and the stage cap merged), interleaved, twelve runs each, minimum /
median, load average about 4:

| scenario | first `describe` | instructions | cycles | loader peak RSS |
|---|---:|---:|---:|---:|
| ndjson-1m | 860 / 960 → **801 / 891 ms (−7% / −7%)** | 6.56 → **5.93 G (−9.5%)** | −3.0% | 143 → 142 MB |
| http-ndjson-1m | 994 / 1,037 → **924 / 1,024 ms (−7% / −1%)** | 6.61 → **6.02 G (−9.0%)** | −1.0% | 144 → 143 MB |
| ndjson-1k, http-ndjson-1k | unchanged (read as before) | within ±2% | | 50 MB |

The loader's `file.read` went from 296 to 150 ms (−50%); the `DESCRIBE` before it takes about
80 ms on one thread, which is why the first `describe` moves less than the read. The CPU of all
processes is unchanged (±1%): fewer instructions, at a lower rate per cycle. The gain is about
half the estimate: the cast and its checks cost about 0.6 G per 1M rows themselves.

### R3-2. A file's profile runs after its write, on one thread — +4% first `describe` at 5M rows (done: (a); (b) not taken)

`src/source.rs`, `watched` (R2-4), called once `write_table` has written the table. `file.watch`
is 305 M per 1M rows of the harness's file, of which `Watch::add` is 98 M; the rest is DuckDB
converting its vectors to Arrow (`ArrowBuffer::reserve`, `ValidityMask::CopySel`,
`ArrowVarcharData::Append`, `NextPowerOfTwo`: about 0.2 G, 7% of parquet-1m). It is on the path to
`describe`: 45 ms at 1M rows of three columns, 333 ms at 5M rows of capacity.py's four, where
the scan and pass it replaced took 184 ms on every thread (the baseline above: +4.4%).

- **Ideas:** (a) beside the write: a staged file (a CSV, a JSON file of up to 64 MiB) has its
  stage before `write_table` starts, so a second connection can scan the stage's converted
  `SELECT` (the one the write reads) into the `Watch` on a thread of its own while the write runs
  on its one; the stage is in memory and uncompressed, so the scan costs less than the table's.
  A Parquet file, read straight in, has no stage: its watch stays after the write, or reads the
  file a second time beside it (Parquet's decode is about 0.3 G per 1M rows; measure). (b) read
  DuckDB's chunks in place instead of Arrow (`duckdb_fetch_chunk`, vectors as they are), as the
  streams' `Held::of` reads Arrow in place: −0.2 G per 1M rows.
- **Done, (a)** (`Beside`): the watch scans the statement the table is written from — a staged
  file's stage, and a Parquet file read a second time — on a second connection and a thread of
  its own, started after the loader is confined, while `write_table` writes on its one; it is
  joined before the stage is detached, and used only if its columns are the table's. It runs in
  the loader's time zone, which a narrowed body's window is read in. A JSON file over 64 MiB,
  read straight in, is still watched after its write: parsed twice it would cost about 3.9 G per
  1M rows to take some 45 ms off the path. `write_table` now reports its own thread's CPU, since
  the process's counts the watch beside it. `describe` is the same, byte for byte
  (`a_files_profile_is_kept_beside_its_write`, which also compares the profile the bench records
  with reading the table, and a narrowed body in a zone other than a fresh connection's).
- **Measured** against `edd72a4` (R3-3's spooled bodies and the 256 MiB stage merged),
  interleaved run by run, 10 runs each, load average 4.2 to 5.3; first `describe` minimum /
  median:

  | scenario | instructions | CPU, all | loader peak RSS | first `describe`, min / median |
  |---|---:|---:|---:|---:|
  | capacity.py's CSV, 5M rows, four columns | +1.2% | +1.9% | 317 → 364 MB | 4,213 / 4,590 → 3,862 / 4,270 ms (−8% / −7%) |
  | csv-1m | +0.2% | −0.4% | 115 → 115 MB | 771 / 841 → 723 / 780 ms (−6% / −7%) |
  | ndjson-1m | +0.7% | −0.2% | 143 → 143 MB | 865 / 904 → 809 / 860 ms (−6% / −5%) |
  | http-csv-1m | +0.2% | +1.1% | 116 → 116 MB | 867 / 945 → 812 / 897 ms (−6% / −5%) |
  | parquet-1m | +4.0% | +7.0% | 61 → 61 MB | 560 / 597 → 524 / 546 ms (−6% / −9%) |
  | http-parquet-1m | +3.1% | +3.2% | 62 → 61 MB | 627 / 696 → 594 / 660 ms (−5% / −5%) |

  - **Memory, a CSV staged past the cap:** the 5M-row CSV's stage spills past `STAGE_MEMORY`.
    With the watch reading it beside the write, the loader peaks 47 MB higher (317 → 364 MB,
    minima). The peak still does not grow with the file:
    `a_large_csv_is_not_held_in_memory_while_it_is_staged` gave 404 MB at 12,000,000 rows,
    26 MB over 6,000,000, within the 390–413 MB measured for `edd72a4`. The owner ranks
    first-`describe` latency over load-time memory.
  - **Parquet:** the second read costs +0.11 to +0.12 G per 1M rows, less than the 0.3 G
    estimated. Its first `describe` was sooner in every session.
  - **Against `eed4c92`** (R3-1 and R3-5 merged too), 10–12 runs, load average 4.3 to 6.9:
    - ndjson-1m 1,055 / 1,139 → 973 / 1,081 ms (−8% / −5%);
    - csv-1m 938 / 1,026 → 897 / 995 ms (−4% / −3%);
    - the 5M-row CSV, in two sessions: 4,124 / 4,840 → 4,251 / 4,873 ms (+3% / +1%), and
      4,671 / 5,273 → 4,336 / 5,084 ms (−7% / −4%). At this load that is inside the noise;
      `write_table` was no slower, and the watch took 467 ms beside it.
  - **Before R3-3 and the stage cap**, against `35ad7db`, load average 4.3 to 5.5:
    - the 5M-row CSV: 4,374 / 4,982 → 3,801 / 4,521 ms (−13% / −9%), and 4,354 / 4,492 →
      3,780 / 4,086 ms in a second session;
    - ndjson-1m −10% / −7%, csv-1m −2% / −7%;
    - parquet-1m −9% / −5%, and in three sessions its minimum always sooner.
  - **The watch's own wall time** grows a little beside the write (csv-1m 46 → 54 ms, the
    5M-row CSV 356 → 443 ms), and the write's by 1–3%.
- **(b), not taken.** Off the path to `describe`, the Arrow conversion is CPU on the watch's own
  thread, about 0.2 G per 1M rows. Reading DuckDB's vectors in place needs a second `Held` over
  `duckdb_string_t` and DuckDB's validity masks, beside the Arrow one the streams use, for a CPU
  win that no longer delays anything. Left as a candidate if loader CPU, not latency, becomes the
  target.

### R3-3. An HTTP CSV or NDJSON body is read on one thread — +0.3 s and +0.55 s at 1M rows (done: spooled)

`src/sources/http/load.rs` and `src/source.rs`, `load_read`: a body reaches DuckDB through a
FIFO, which its readers parse on one thread (`file.read`: 756 ms wall for 752 ms of CPU, NDJSON;
526 ms for 564 ms, CSV), where a file is parsed on four (310 ms and 242 ms). First `describe`:
http-csv-1m 1.08 s against csv-1m's 762 ms; http-ndjson-1m 1.41 s against ndjson-1m's 860 ms.
Instructions are the same within 1%.

- **Idea:** spool the body to a file in the loader's staging directory (the network child may
  write there already: `doctor --probe`), counted against `max_bytes` as now, then read the file
  as a file is read, staged on every thread (R2-1's rule, including its 64 MiB threshold for
  JSON). A spooled body can also be asked for its types first, which a FIFO cannot (R2-3's "not
  done").
- **Estimate:** −0.3 s (http-csv-1m) and −0.55 s (http-ndjson-1m) first `describe`, to within
  about the write of the spool (63 MB to the page cache, tens of ms) of the file's; instructions
  about the same. **Disk:** one body at a time, up to the source's `max_bytes` (256 MiB by
  default; whatever the operator raises it to), removed once read. Bounded by configuration, not
  by a constant: flagged, as the owner asked.
- **Done (2026-10-12, owner's decision).** `Where::spooled` writes the decoded body to
  `http-body.csv` or `http-body.ndjson` in the staging directory through the reader that counts
  and caps it, checks it was read whole (truncation, the cap, the codec, the budget charged
  either way), then reads it as that file: CSV staged, NDJSON staged up to 64 MiB and straight in
  above, narrowed staged (`a_spooled_body_is_staged_or_read_straight_in_as_its_file_is`). The
  spool goes when the load ends, loaded or not
  (`a_csv_or_ndjson_body_is_spooled_to_staging_within_max_bytes_and_removed`). Measured with
  `bench/ingest.py`, `35ad7db` and the change interleaved run by run, ten runs each, load
  average 4 to 5, minimum / median:

  | scenario | first `describe` | instructions | CPU, all | loader peak RSS | loader wrote |
  |---|---:|---:|---:|---:|---:|
  | http-csv-1m | 1,088 / 1,215 → **881 / 1,023 ms (−19% / −16%)** | 5.83 → 5.82 G | 1.12 → 1.13 s | 116 → 117 MB | 4.3 → 42.7 MB |
  | http-ndjson-1m | 1,274 / 1,438 → **981 / 1,109 ms (−23% / −23%)** | 6.62 → 6.62 G | 1.30 → 1.29 s | 173 → 145 MB | 4.3 → 64.6 MB |
  | csv-1m (unchanged path) | 786 / 897 → 815 / 897 ms | 5.80 → 5.82 G | 1.09 → 1.10 s | 117 → 118 MB | 4.3 → 4.3 MB |
  | ndjson-1m (unchanged path) | 889 / 999 → 867 / 1,015 ms | 6.57 → 6.59 G | 1.21 → 1.19 s | 143 → 143 MB | 4.3 → 4.3 MB |
  | http-csv-1k | 343 / 368 → 343 / 378 ms | 971 → 992 M | 352 → 367 ms | 53 → 54 MB | 0.55 → 0.59 MB |
  | http-ndjson-1k | 191 / 239 → 213 / 242 ms | 512 → 515 M | 212 → 237 ms | 50 → 50 MB | 0.55 → 0.61 MB |

  An HTTP body is now within 66 ms (CSV) and 114 ms (NDJSON) of its file at the minimum, of
  which the spool's write is 44 and 65 ms (`http.spool`). Overlapping the download with the
  parse could take back at most that, and only by reading a file still being written, which
  DuckDB's readers would take for a whole one at its current end: not done. A 1k-row NDJSON
  body costs about 12 ms more loader CPU, its reader's threads started, as the file's are.

### R3-4. The keyed tally — 386 instructions a row at 500 series; split out the hot fields (done)

`src/profile.rs`, `Tally::add` (inlined into `Watch::add`) and `same`. `stream.watch` is 3.86 G of
exec_series-10m (17%). It runs on its own thread (631 ms wall beside the insert's 3.08 s), so
this is CPU, not latency — until the key is an integer (R3-10). From `perf annotate`, by source
line: finding the row's series 25% (`same` alone 17.6% of the stage, called twice a row: the
last row's series misses in a fixed cycle of meters, the next one hits); `Series::note` (the
`i128` sums of distances and the gcd's check) about 15%; the per-row `Option` updates of
`first`/`last` and of each measure's `min`/`max` through `duck_cmp`'s NaN handling about 15%;
`Doubles::get`'s `Option` 5%; the per-time totals (`Times`) about 10%.

- **Ideas:** a `Hot` array of plain fields per series — `rows`, `timed`, `first`/`last` as `i64`
  with sentinels, `t0`, `gcd`, the sums as `i64` folded into the `i128` once a batch (or on the
  overflow check), each measure's `nulls`/`zeros`/`min`/`max` in one flat `Vec` with a NaN count
  beside it — apart from the cold `Series` (key `String`, notes, phase). Find the series by the
  key's length and first word kept in `Hot` (no `String` to chase), and try the next series first
  when the last row hit the next one (a fixed cycle then costs one compare, not two).
- **Estimate:** 386 → about 200 instructions a row: −1.8 G at exec_series-10m (−8%), watch CPU
  631 → about 330 ms. `the_profile_kept_as_rows_arrive_is_the_profile_of_the_table` and
  `a_kept_distinct_count_of_a_tallied_key_is_duckdbs` keep `describe` the same.
- **Done** (`Tally`, `Hot`, `HotMeasure`, `Words`). A batch is added a column at a time:
  1. Each row's series is found first, into an array of indices, by the key's `Words`, its
     length and first sixteen bytes. They are read where they lie: two loads of the string
     array's values, masked, or the view itself. They are compared with the words kept in
     `Hot`, and the text only for a longer key. The next series is tried first when the last
     row's came after the one before, so a fixed cycle costs one compare a row.
  2. Then the rows are added to their series' plain fields: `rows`, `timed`, `first`/`last`
     with sentinels, `t0`, `gcd` and the `i128` sums.
  3. Then each measure's column goes into one flat array of `nulls`/`zeros`/`min`/`max`. `min`
     starts as NaN and `max` as −∞, so `duck_cmp`'s order needs no `Option`.
  4. Then the per-time totals, a run of one time at a time, added up apart and into its slot
     once it ends.

  Beside the tally:
  - A time is hashed for its distinct count only when it is not the row before's, once the
    first batch has shown that times repeat. Where they do not, that check alone cost 7
    instructions a row.
  - The table-wide range is kept in locals with the same sentinels.
  - A column whose validity bitmap has no null is read as having none.

  Kept as the `i128` it was, rather than folded from `i64`: a distance squared overflows `i64`
  past 35 days in milliseconds. New tests guard what the words and the runs could get wrong:
  - `a_cycle_of_series_costs_one_key_compare_a_row`: written first, it failed at 39,460
    compares for 20,000 rows;
  - `a_tally_tells_apart_keys_alike_in_their_first_words`;
  - `a_keys_words_are_read_alike_from_every_string_layout`;
  - `a_tallys_totals_per_time_count_every_row_of_every_time`.

  Each fails with its guard removed: the cycle order, the tail compare, a mask, the view's
  shift, the last run's close.
- **Measured.** The watch's own instructions, counted by the kernel for its thread around
  `Watch::add` over exec_series-1m's batches (exact; minimum of three passes):
  - 398 → 285 a row (−28%). Broken down: finding the series 114, the times into their series
    44, the measure 28, the per-time totals 40, the rest of the watch 88.
  - On a one-series stream (exec-1m's), 98 → 101 a row (+3%). Not found which change costs it.

  End to end against `edd72a4`, interleaved, 10 runs at 1M and 6 at 10M, load 3.9 to 4.9:

  | scenario | instructions | CPU, all | `stream.watch` CPU | loader peak RSS | first `describe`, min / median |
  |---|---:|---:|---:|---:|---:|
  | exec_series-10m | 22.70 → 21.58 G (−4.9%) | −3.4% | 622 → 370 ms (−40%) | 104 → 102 MB | 3,936 / 4,426 → 4,060 / 4,342 ms |
  | exec_series-1m | 2.74 → 2.61 G (−4.6%) | −4.7% | 58 → 36 ms (−39%) | 58 → 58 MB | 602 / 667 → 600 / 660 ms |
  | exec-1m | 2.41 → 2.42 G (+0.5%) | −3.3% | 13.7 → 15.1 ms | 58 → 58 MB | 610 / 684 → 622 / 674 ms |

  −1.1 G at exec_series-10m, short of the −1.8 G estimated: finding the series still costs 114
  instructions a row, about half of it reading the key's words through the batch's offsets. The
  watch runs beside the insert, so the first `describe` does not move.

### R3-5. CSV files are still sniffed: let a file source be declared (done)

`src/source.rs`, `reader_for`, and `src/config.rs`: a `type = "file"` source takes `path` and
`timezone`, not `schema` or `delimiter` (`inert_keys`), so every CSV file is sniffed: 0.42 G and
about 120 ms per table whatever its size (http-csv-1k 982 M and 332 ms; the same body declared,
http-csvdecl-1k, 562 M and 233 ms), 43% of a 1k-row CSV session.

- **Should a file source accept a declared schema?** Yes, as an operator's choice, the way an
  HTTP source does: `schema` (and `delimiter` for a CSV) on a `type = "file"` source, read through
  the declared-CSV reader R2-3 built (`types` plus the stated dialect, not `auto_detect = false`,
  for the reasons measured there), and for NDJSON with `columns` (R3-1 b). The declaration is the
  operator's, never the model's, and the table is the same as sniffed (origin-blind); a file that
  does not match fails naming the field. `benchy-mcp <dir>` keeps sniffing, since it has no config.
- **Estimate:** −0.42 G and about −120 ms per declared CSV table (csv-1k 986 M → about 560 M,
  352 → about 235 ms); −1.6 G per 1M rows of a declared NDJSON file (its sniff and R3-1).

**Done (2026-10-12).** A `type = "file"` source takes `[[source.schema]]` and, for a CSV or TSV,
`delimiter` (docs/SOURCES.md, "Files"). It is read by the reader a declared HTTP body is
(`source::reader_declared`, R2-3's, now shared): a CSV or TSV with `types` and its dialect
stated, an NDJSON or JSON file with `columns`, a Parquet file pinned, and held to the declaration
as a body is (`Hold`). `benchy-mcp <dir>`, and a file named without a schema, sniff as before.
New scenarios `csvdecl-*` and `ndjsondecl-*`: the harness's files as a declared file source,
each declared as its sniffer types it (`ts` a `timestamptz` to the CSV sniffer, a `timestamp`
to the JSON one). Against the same files undeclared, interleaved, ten runs each, minimum /
median:

| scenario | first `describe` | instructions | CPU, all processes | loader peak RSS |
|---|---:|---:|---:|---:|
| csv-1k → csvdecl-1k | 355 / 389 → **220 / 268 ms (−38% / −31%)** | 988 → **573 M (−42%)** | 376 → 253 ms | 53 → 53 MB |
| csv-1m → csvdecl-1m | 900 / 1,038 → 776 / 933 ms (−14% / −10%) | 5.82 → 5.62 G (−3.4%) | 1.17 → 1.08 s | 118 → 114 MB |
| ndjson-1k → ndjsondecl-1k | 227 / 254 → 217 / 245 ms | 518 → 507 M (−2.3%) | 245 → 244 ms | 50 → 50 MB |
| ndjson-1m → ndjsondecl-1m | 883 / 1,093 → **789 / 943 ms (−11% / −14%)** | 6.59 → **5.36 G (−18.7%)** | 1.26 → 1.12 s | 143 → 142 MB |

ndjsondecl-1m is −1.23 G, not −1.6 G: R3-1's parse by format is gone (no sniff, the values cast
by the reader), but the cast itself costs about 0.6 G per 1M rows. Declared `timestamptz`
instead, an NDJSON column is cast through ICU, whose start cost ndjsondecl-1k about 130 M more
(655 M); declared as the JSON sniffer types it, `timestamp`, it does not.

### R3-6. `profile.step` reads the table — 0.25–0.47 G at 10M rows, 26 M per 1k table

Carried to round 4 as part of R4-5.


`src/profile.rs`, `measure_step`: a `min()` over the whole time column, then a window query over
the first `STEP_SAMPLE_ROWS` worth of time. ch-10m's table is ordered by `(meter, ts)`, so the
time filter cannot skip row groups and scans everything (`BitpackingScanPartial` 34%,
`FilterSelectionSwitch` 17%, `MinMaxState` 14% of the stage's 463 M). 2.3% of ch-10m, 1.1% of
exec_series-10m, 3.3% of parquet-1m, and 13% of host8-1k (212 M for eight 1k-row tables, 112 ms
of their loaders).

- **Idea:** the `Watch` sees every time already. Keep the exact first raw value (microseconds, not
  the milliseconds it keeps now), so the `min()` needs no query, and the times within the sample
  window per series (bounded by `STEP_SAMPLE_ROWS`) so the mode of their steps is found without
  the window query. A table without a `Seen` keeps the queries.
- **Estimate:** −0.2 to −0.45 G at 10M rows (1–2%), −89 M at parquet-1m (3%), −26 M per 1k table
  (−13% of host8-1k); 10–35 ms of each load.

### R3-7. The engine's attach check lists every function — 115 M and about 28 ms per session

Carried to round 4 as R4-4, measured again.


`src/workspace.rs`, `catalogs_with_functions`: `SELECT DISTINCT database_name FROM
duckdb_functions() WHERE database_name IN (…)`. DuckDB has no filter pushdown into
`duckdb_functions()`: it builds a row of `Value`s for every built-in overload (`Vector::SetValue`,
`Value::LIST`, `malloc`/`free`) to find the macros of the attached catalogs. 27% of an exec-1k
session's instructions, 27 ms of its 205 ms to `describe`; paid once per batch since round 1's
item 5.

- **Idea, to be measured first:** a probe that does not enumerate the built-ins — for example,
  the attached catalog's own entries through `duckdb_schemas()`/`duckdb_dependencies()`, or a
  macro's name tried against each attached catalog — that still refuses every case in
  `corpus/escape.toml` and the attach tests. Nothing found yet does; the item is the 115 M.
- **Estimate:** up to −115 M (−27% at exec-1k) and −27 to −31 ms per session, if such a probe exists.

### R3-8. Arrow validates each string offset through a generic iterator — 0.3 G per 10M rows

Carried to round 4 under R4-6.


`src/ipc.rs` and `src/exec.rs`, `StreamDecoder`: arrow-data's `validate_each_offset` with
`validate_utf8` is 1.4–1.6% of exec-10m, exec_series-10m and ch-10m (33 instructions a row for one
text column); `from_utf8` over the values is 0.1%.

- **Idea:** decode with arrow's validation off (`StreamDecoder::with_skip_validation`, `unsafe`)
  and make benchy's own check, which must refuse everything arrow's refuses: buffers' lengths,
  offsets in bounds and non-decreasing, the values valid UTF-8 once over the whole buffer, and no
  offset inside a character (`byte & 0xC0 != 0x80`, a loop the compiler vectorises). Connector
  output is untrusted (docs/EXEC-CONNECTOR-PROTOCOL.md): this needs the malformed-stream tests
  to fail with it removed, one per check.
- **Estimate:** −0.27 G per 10M rows (−1.3%) of every Arrow stream: exec, ClickHouse, a host's
  attach, HTTP Arrow.

### R3-9. JSON pages — `http.fetch` 3.0 G and 1.03 s on one thread (R2-12, carried)

Carried to round 4 as R4-3.


`src/sources/http/load.rs`, `PageDecoder`: arrow-json's tape decoder 46% of the stage, its
timestamp parsing about 11% (`TimestampParser::new` 4.5%, `NaiveDate::from_ymd_opt` 2.5%,
`TimestampParser::date` 2.1%, …). The fetch is on the path to `describe` (http-json-1m 1.21 s).

- **Ideas:** a fast path for RFC 3339 times in UTC; decode page *i* on a second thread while page
  *i + 1* is requested (the cursor is read from the page first).
- **Estimate:** −0.3 G (−6% of http-json-1m); the overlap, up to the network's share of the 1.03 s.

### R3-10. Integer keys are formatted to text, a row at a time (done)

`src/profile.rs`, `Watch::add`, `Values::Int`: every row's key is written into a buffer with
`write!` so the tally can find it as text. Measured with a stream made for this (not a harness
scenario yet): exec_series-10m's rows with `meter` an `int64` (1000 + i % 500), against the same rows with text keys, one binary, four runs
each, minima: the watch thread's CPU 940 ms against 619 ms (+52%, about +2 G at the watch's rate);
the insert 1.21 s against 2.95 s, so with an integer key the watch is nearly as long as the
insert and close to the path to `describe` (2.07 s).

- **Idea:** tally an integer key by value (the series found by one integer compare, the hash map
  keyed by `i128`), and write a key's text once, when its series begins.
- **Estimate:** −2 G per 10M rows with an integer key (−13% of that load), and the watch off the
  path to `describe` again.
- **Done** (`int_words`, `Tally::find`), on R3-4's `Words`.
  - An integer key's words are its value, with a length no text has, so a row's series is found
    by one compare of two words.
  - A key not seen before is looked up by value, in a map keyed by `i128`, and written as text
    once, when its series begins, as `CAST(… AS VARCHAR)` writes it. A null in the key column
    is still the text that stands for one.
  - Its distinct count was always counted as an integer, and still is
    (`a_kept_distinct_count_of_a_tallied_key_is_duckdbs`).
  - `an_integer_key_is_written_as_text_once_a_series` was written first and failed with 2,000
    keys written for 2,400 rows. It checks the series' text for negative values, values wider
    than 32 bits, an unsigned counter past `i64::MAX`, and a null.
  - The stream is now a harness scenario, `exec_intkey-1m` and `exec_intkey-10m`.
- **Measured.** The watch's own instructions, counted by the kernel for its thread over
  exec_intkey-1m's batches (exact): 713 a row on `main`, 599 with R3-4, 310 with R3-10.

  End to end against `edd72a4`, interleaved, 10 runs at 1M and 6 at 10M, load 4.2 to 5.8:

  | scenario | instructions | CPU, all | `stream.watch` CPU | loader peak RSS | first `describe`, min / median |
  |---|---:|---:|---:|---:|---:|
  | exec_intkey-10m | 14.92 → 10.86 G (−27%) | −18% | 926 → 364 ms (−61%) | 101 → 101 MB | 2,128 / 2,508 → 2,044 / 2,330 ms (−4% / −7%) |
  | exec_intkey-1m | 1.96 → 1.55 G (−21%) | −15% | 95 → 44 ms (−53%) | 57 → 58 MB | 446 / 487 → 428 / 480 ms |

  Against R3-4 alone at 10M: 13.75 → 10.87 G (−21%), and the watch's CPU 801 → 372 ms. The
  watch is again well inside the insert (488 ms of wall time beside 1.31 s, where it took
  990 ms), so it is off the path to `describe`.

### R3-11. Starting processes — 21 M instructions each, 84 M a session (R2-10, carried; done for the warden: −18 to −26 M)

Four processes a session — server, warden, engine, loader — and one more per source: 84 M,
19.5% of exec-1k; host8-1k 249 M. 95% of each is the dynamic linker resolving `libduckdb.so`'s
symbols, fixed by the release library (the `bundled` build that would avoid it is ruled out).

- **Idea:** start the warden by `fork` alone at the top of `main`, before any thread or DuckDB
  exists, instead of `fork` and `exec` of the same binary: it maps and links nothing again, and
  closes everything but its pipe. −21 M a session (−5% at exec-1k), off the path to `describe`
  (the warden starts beside the engine). The warden's guarantees
  (`supervisor::run_warden`: holds nothing, survives a SIGKILLed server) must hold as tested.

**Done (2026-10-12).** `supervisor::start_warden` forks, and the child runs the warden in the
same image: no `exec`, no second link of `libduckdb.so`. Round 1's reason not to (item 7: a forked
warden holds a copy of the server's memory) is met by when and how it forks: right after the
session directory is made, before the config is read and its `${VAR}` credentials resolved,
before DuckDB or any thread exists (refused otherwise, `the_warden_is_forked_only_from_one_thread`);
the child zeroes every environment string it was forked with, closes every descriptor but the
pipe and `/dev/null`, and names itself `benchy-warden`. `the_warden_holds_nothing_of_the_servers`
scans its environment and every readable page of its memory for a credential the server was
given, and fails with the zeroing removed. `--warden` is no longer a mode of the binary. The
SIGKILL, SIGTERM and signal tests are unchanged and pass.

Against `35ad7db`, interleaved, fifteen runs each, minimum / median:

| scenario | instructions | first `describe` |
|---|---:|---:|
| exec-1k | 436 → 418 M (−4.1%) | 206 / 220 → 215 / 227 ms |
| csv-1k | 982 → 960 M (−2.3%) | 320 / 353 → 323 / 347 ms |
| host-1k | 496 → 470 M (−5.2%) | 220 / 240 → 207 / 230 ms |
| parquet-1k | 482 → 462 M (−4.0%) | 195 / 209 → 189 / 204 ms |

The server's `warden.spawn` went from 0.8 to 0.4 ms. The warden started beside the engine
already, so the first `describe` moves within its spread.

### R3-12. Measured, not taken: R2-1's pipe, again, at a quieter moment

R2-1 found that reading an NDJSON or Parquet file on a thread of its own, its chunks referenced
into the write, was no sooner, and asked whether a quieter machine would differ. Re-measured: the
experiment's branch (`0031dc6`, on `d4afdaa`), interleaved with `d4afdaa` (straight in) and
`99b5155` (staged), load average 1.7 to 4.3, 12 runs each (10 for Parquet), minimum / median:

| ndjson-1m | first `describe` | instructions | CPU, all | loader peak RSS |
|---|---:|---:|---:|---:|
| straight in (`d4afdaa`) | 1,100 / 1,170 ms | 6.80 G | 1.20 s | 98 MB |
| the pipe (`0031dc6`) | 1,040 / 1,170 ms (−5% / 0%) | 7.03 G (+3%) | 1.34 s (+12%) | 159 MB |
| staged (`99b5155`, taken) | 846 / 952 ms (−23% / −19%) | 6.56 G (−4%) | 1.18 s | 142 MB |

parquet-1m with the pipe: 595 / 652 → 558 / 677 ms, +1.5% instructions, +5% CPU, 68 → 79 MB.
The pipe stays dropped.

### A loader's file in storage version v1.3.0 — the layout made deterministic, and 6–18% fewer instructions

Found while chasing a flaky layout test (`a_json_file_is_laid_out_alike_staged_or_read_straight_in`
failed once under load: one row group's `m` was FSST where the other load's was Dictionary).
DuckDB 1.5.5 in its default storage version, v1.0.0, analyses a text segment for both dictionary
and FSST and estimates FSST's size from a random quarter of its vectors
(src/storage/compression/fsst.cpp: `ANALYSIS_SAMPLE_SIZE = 0.25`, a `RandomEngine` constructed
with no seed, so seeded from `getrandom`; `setseed` and every setting reach only the client's
own engine). At a near tie the method changed between loads of the same rows: 1 in 30 loads of a
300,000-row column of seven two-byte values, in DuckDB alone. From storage version 5 (v1.3.0) both
are replaced by DICT_FSST — a dictionary whose strings FSST compresses, so FSST stays on — whose
analysis samples nothing: 30 of 30 loads the same.

`storage_compatibility_version` must be set as the file is created: a `SET` after `duckdb_open`
applies only to databases attached later, which is why R2-9's measurement of `'latest'` found the
same segments and compression. Now set in the config `Bench::open_in` opens the file with. Against
`eed4c92`, interleaved, ten runs each (six at 10M), minimum / median:

| scenario | first `describe` | instructions | cycles | loader peak RSS |
|---|---:|---:|---:|---:|
| csv-1m | 819 / 1,110 → 797 / 872 ms | 5.77 → 5.42 G (−6.0%) | −5.0% | 115 → 115 MB |
| parquet-1m | 581 / 723 → 516 / 628 ms | 2.70 → 2.35 G (−13.0%) | −11.3% | 61 → 62 MB |
| ndjson-1m | 872 / 1,083 → 773 / 921 ms | 5.96 → 5.57 G (−6.4%) | −10.9% | 142 → 143 MB |
| exec-1m | 592 / 692 → 521 / 565 ms | 2.40 → 2.03 G (−15.2%) | −14.1% | 58 → 60 MB |
| exec_series-1m | 612 / 735 → 525 / 641 ms | 2.71 → 2.35 G (−13.2%) | −12.3% | 58 → 60 MB |
| exec-10m | 4.22 / 4.40 → **3.20 / 3.52 s (−24% / −20%)** | 19.88 → **16.32 G (−17.9%)** | −20.6% | 101 → 102 MB |
| exec-1k | 238 / 295 → 216 / 290 ms | 416 → 415 M | −3.7% | 49 → 47 MB |

The write no longer analyses each text segment twice. The file is the same size (the harness's
1M rows 4,468,736 bytes, the eval's meter data 7,352,320, either way), and queries over the text
column as fast or faster (DuckDB alone, four threads, minimum of seven: a `GROUP BY meter` 10.1 →
8.1 ms, an equality filter 3.7 → 2.5 ms). The engine, the same DuckDB, attaches the file as
before. A stream's loader peaks 2 MB higher.

**Free text, the case FSST was kept on for** (round 1, item 1: unique URLs and UUIDs, 39 MB with
FSST against 96 MB without). That fixture is not in the repository; rebuilt to its description:
1,000,000 rows of a timestamp, a unique URL (`https://shop.example.com/catalog/<5 kinds>/item-<i>
?utm_source=newsletter&utm_campaign=c<i % 997>`), a UUID-shaped md5, and a medium-cardinality
`customer` (50,000 values of `customer-NNNNN-<region>`). Table size, DuckDB alone, one thread,
and queries on four threads, minimum of seven:

| storage | url + uid table | customer table | `url LIKE '%x%'` | `GROUP BY url` | `uid LIKE '%ab%'` | `customer LIKE '%x%'` | `GROUP BY customer` |
|---|---:|---:|---:|---:|---:|---:|---:|
| v1.0.0, FSST on (`eed4c92`) | 40.6 MB | 5.5 MB | 38.5 ms | 221 ms | 50.0 ms | 22.1 ms | 98.0 ms |
| v1.0.0, FSST off | 134.8 MB | 23.9 MB | 21.0 ms | 156 ms | 34.8 ms | 11.8 ms | 70.6 ms |
| **v1.3.0, DICT_FSST (this)** | **41.7 MB (+2.7%)** | **5.5 MB** | 31.6 ms | 151 ms | 40.9 ms | 17.6 ms | 89.4 ms |

DICT_FSST keeps free text as small as FSST did, within 3%, and medium-cardinality text the
same; the queries are 9–32% faster. Loaded by benchy (the four columns as one Parquet file in a
folder), interleaved, ten runs each, minimum / median: the loader wrote 42.0 → 44.3 MB (+5%), its
peak 141 / 144 → 133 / 136 MB (−6%), 10.37 → 9.60 G instructions (−7.5%), but 5.66 → 6.17 G
cycles (+9%): `write_table` 1,668 → 1,805 ms, the file's profile scan 220 → 264 ms, the first
`describe` 2,253 / 2,342 → 2,429 / 2,516 ms (+8% / +7%). Compressing unique strings into
DICT_FSST, and decompressing them for the profile, runs at fewer instructions per cycle. So a
table of mostly free text loads about 8% later; time series, whose text is a key, sooner.

### Memory and disk that grow with the input

- **A CSV file's stage is held to 256 MiB** (done 2026-10-12, the owner's decision). It was
  staged whole, whatever its size: its loader's peak was the in-memory stage (715 MB for the
  10M-row, 475 MB CSV in docs/CAPACITY.md, 1.5 times the file). DuckDB is now held to
  `STAGE_MEMORY` while a stage is filled and written (`source::staged_within`), and the stage's
  blocks spill past it to `bench.duckdb.tmp` in the loader's staging directory, which the
  Landlock grants already cover; DuckDB removes them when the stage is detached. Every stage
  goes through it: a CSV file, a JSON file of up to 64 MiB (R2-1, which fits), an HTTP CSV or
  NDJSON body, a narrowed HTTP Parquet body. The table is the same file either way
  (`a_table_read_on_one_thread_or_staged_is_the_same_file`).

  **Which cap, and how.** Measured first with DuckDB 1.5.5 alone (its Python package, four
  threads, the harness's 10M-row CSV): an in-memory stage under `memory_limit` spills and
  stays flat past the cap (peak after the read 371 MB at 6M and 368 MB at 12M rows of a CSV
  DuckDB holds at 6.6 times its size). A file-backed stage, the other option, compresses every
  block as it fills: at 10M rows, under the same 256 MB, it peaked at 334 MB against 462 MB
  in memory, for 8.95 s of CPU against 7.27–8.11 s; uncapped, 9.16 s against 7.81 s, its
  buffer pool still growing with the input. Compression costs about what a lower in-memory cap
  does (128 MB in memory: 272–340 MB, 8.25–8.37 s), so the stage stays in memory, which is the
  cheaper of the two at any cap, and the cap was chosen with benchy: csv-10m, `35ad7db`
  against each cap, two runs each, interleaved:

  | cap | instructions | CPU, all | loader peak RSS | first `describe` | loader wrote |
  |---|---:|---:|---:|---:|---:|
  | none (`35ad7db`) | 50.4 G | 8.08 s | 510 MB | 6.52 s | 41 MB |
  | 128 MiB | 62.5 G (+24%) | 10.77 s (+33%) | 300 MB | 7.38 s (+13%) | 215–237 MB |
  | 256 MiB (taken) | 52.3 G (+4%) | 8.82 s (+9%) | 454 MB | 6.80 s (+4%) | 131–140 MB |

  At 128 MiB the buffer manager evicts under four readers at once, and costs a quarter more
  instructions; 256 MiB is the cap. Measured again, `35ad7db` and 256 MiB interleaved run by
  run, load average 4 to 7, minimum / median:

  | scenario | runs | instructions | CPU, all | loader peak RSS | first `describe` | loader wrote |
  |---|---:|---:|---:|---:|---:|---:|
  | csv-10m | 4 | 50.32 → 52.57 G (+4.5%) | 8.18 → 8.61 s (+5.2%) | **513 → 406 MB (−21%)** | 6.16 / 7.97 → 7.01 / 8.52 s (+14% / +7%) | 41 → 129–139 MB |
  | csv-1m | 20 | 5.81 → 5.82 G (+0.1%) | 1.09 → 1.13 s | 117 → 115 MB | 975 / 1,289 → 832 / 1,300 ms | 4.3 → 4.3 MB |
  | ndjson-1m | 10 | 6.60 → 6.64 G (+0.6%) | 1.26 → 1.27 s | 143 → 143 MB | 955 / 1,344 → 926 / 1,138 ms | 4.3 → 4.3 MB |
  | http-csv-1m | 10 | 5.82 → 5.84 G (+0.3%) | 1.15 → 1.15 s | 115 → 115 MB | 1,246 / 1,484 → 1,224 / 1,346 ms | 4.3 → 4.3 MB |
  | http-ndjson-1m | 20 | 6.62 → 6.63 G (+0.1%) | 1.34 → 1.35 s | 173 → 167 MB | 1,327 / 2,002 → 1,462 / 1,965 ms | 4.3 → 4.3 MB |
  | csv-1k | 20 | 979 → 991 M (+1.3%) | 355 → 366 ms | 52 → 52 MB | 324 / 571 → 360 / 570 ms | 0.5 → 0.5 MB |
  | ndjson-1k | 20 | 519 → 506 M (−2.6%) | 228 → 241 ms | 49 → 49 MB | 229 / 341 → 243 / 394 ms | 0.5 → 0.5 MB |

  A stage under the cap does not spill and writes nothing more; at 1M and 1k rows the
  differences are the machine's (the loader's own `loader.load` medians: csv-1k 362 → 361 ms,
  ndjson-1k 177 → 173 ms). At 10M rows the cap costs 4.5% more instructions and 5% more CPU
  for a fifth less memory, and the first `describe` comes later by the spill: the owner's
  decision bounds memory here, the one place it trades against latency. The harness's CSV is
  narrow, three columns that DuckDB holds in about 280 MB, so its stage barely passes the cap;
  the loader's peak past it stays where it is as the file grows
  (`a_large_csv_is_not_held_in_memory_while_it_is_staged`: 6M and 12M rows of a CSV DuckDB
  holds at 6.6 times its size took the loader to 373–400 and 390–413 MB, where held whole they
  took it to 740 and 1,372 MB).

  **Disk:** the spill, at most the stage less the cap: about 90–100 MB at csv-10m, bounded by
  the input (a file's size, or an HTTP body's `max_bytes`) and by DuckDB's
  `max_temp_directory_size` (90% of the free disk).

  **Where the cap could not hold.** A stage of very wide rows on every thread ran DuckDB out of
  memory under the cap rather than spilling, and was filled again without it, held whole. What
  holds it, measured since (R6-W): not a row group per thread but a block of every column for
  each thread appending, and two while the threads' collections are merged in order, and the
  JSON reader's vector of 2,048 parsed rows. Such a file is now read on one thread, under the
  cap.
- **An HTTP CSV or NDJSON body is spooled to disk** since R3-3, up to the source's `max_bytes`,
  and read as its file is: what is said of a file's stage here holds for it.

Everything else a load holds is bounded by a constant: the keyed tally's series
(`series_max`), its times (`TIMES_MAX`), its kept rows (`WATCH_FLAT_MAX`), a stream's queue of
one batch.

## Baseline: round 2, `ef3a0cf`

`main` at `ef3a0cf` (round 1 merged), against round 1's baseline binary, `22ff87e` (`7ec65e2`,
streaming ingest, plus the tracing), 2026-10-11. Both were built from their own commits and
measured in the same sessions on the same data, the old and the new interleaved, so the machine's
load fell on both alike (load average 1.3 to 7). Minima: of 10 runs per 1M scenario, 6 per 10M
and 15 per 1k. Cold start is wall time, from spawning benchy to the answer to `describe`.

| scenario | instructions | CPU, all processes | loader peak RSS | cold start, min |
|---|---:|---:|---:|---:|
| csv-1m | 6.20 G → 6.20 G (0%) | 1.13 → 1.17 s (+3%) | 116 → 117 MB | 863 ms → 774 ms (−10%) |
| parquet-1m | 3.65 G → 3.17 G (−13%) | 0.75 → 0.68 s (−10%) | 100 → 70 MB | 757 ms → 565 ms (−25%) |
| ndjson-1m | 6.99 G → 6.77 G (−3%) | 1.25 → 1.17 s (−6%) | 142 → 99 MB | **947 ms → 1.11 s (+17%)** |
| exec-1m | 3.19 G → 2.65 G (−17%) | 0.72 → 0.61 s (−16%) | 68 → 70 MB | 748 ms → 647 ms (−13%) |
| exec_series-1m | 3.57 G → 2.95 G (−17%) | 0.81 → 0.69 s (−15%) | 68 → 71 MB | 855 ms → 671 ms (−21%) |
| ch-1m | 3.80 G → 2.79 G (−27%) | 0.78 → 0.62 s (−21%) | 77 → 73 MB | 801 ms → 607 ms (−24%) |
| host-1m | 3.22 G → 2.72 G (−15%) | 0.67 → 0.57 s (−14%) | 68 → 71 MB | 721 ms → 597 ms (−17%) |
| exec-10m | 27.77 G → 23.07 G (−17%) | 5.21 → 4.41 s (−16%) | 113 → 116 MB | 4.44 s → 4.50 s (+1%) |
| exec_series-10m | 30.76 G → 24.91 G (−19%) | 5.87 → 4.82 s (−18%) | 112 → 116 MB | 5.34 s → 4.85 s (−9%) |
| ch-10m | 34.06 G → 23.68 G (−30%) | 5.33 → 3.92 s (−26%) | 108 → 103 MB | 4.59 s → 3.81 s (−17%) |
| csv-1k | 1.01 G → 996 M (−2%) | 0.37 → 0.36 s (−2%) | 52 → 53 MB | 443 ms → 329 ms (−26%) |
| parquet-1k | 527 M → 501 M (−5%) | 0.24 → 0.22 s (−8%) | 50 → 50 MB | 319 ms → 190 ms (−40%) |
| ndjson-1k | 547 M → 552 M (+1%) | 0.24 → 0.24 s (−2%) | 49 → 50 MB | 320 ms → 202 ms (−37%) |
| exec-1k | 467 M → 447 M (−4%) | 0.22 → 0.22 s (+1%) | 49 → 49 MB | 320 ms → 206 ms (−36%) |
| exec_series-1k | 470 M → 447 M (−5%) | 0.23 → 0.22 s (−5%) | 49 → 49 MB | 332 ms → 211 ms (−36%) |
| ch-1k | 468 M → 440 M (−6%) | 0.22 → 0.21 s (−5%) | 49 → 49 MB | 317 ms → 195 ms (−39%) |
| host-1k | 532 M → 502 M (−6%) | 0.19 → 0.19 s (0%) | 48 → 49 MB | 321 ms → 222 ms (−31%) |

ClickHouse's own CPU to serve the load fell with the loopback default (`none` for zstd): 2.41 →
0.91 s at 10M rows, 0.29 → 0.13 s at 1M. The engine stays at 44–45 MB and the server about
20 ms of CPU in every scenario.

What the table says beyond the wins:

- **The NDJSON file's first `describe` came later** (R2-1 below): reading it straight into its
  table parses it on one thread. Confirmed against `b0e46ae`, the commit before that change,
  interleaved, twelve runs each: 833 → 1,099 ms (minimum), 914 → 1,143 ms (median).
- **A 10M-row connector load got no sooner** though its CPU fell 16%: the profile kept as rows
  go in (`stream.watch`) runs on the insert's thread, where the scan it replaced ran on four.
- **A stream's loader peaks 2–4 MB higher**: the profile holds a batch's columns as it reads them.
  Since R2-2 the watch reads the batch in place, on a thread of its own: exec-10m's first
  `describe` 4.88 → 4.08 s (minima), and no copies, though the batch its queue holds stands in
  their place.

The HTTP source did not exist at `22ff87e`; against the numbers its branch measured (round 1's
HTTP table, below), minima of 10 runs:

| scenario | instructions | loader CPU | loader peak RSS | cold start, min (1k) |
|---|---:|---:|---:|---:|
| http-arrow-1m | 3.21 G → 2.69 G (−16%) | 0.59 → 0.48 s | 70 → 72 MB | 180 ms |
| http-parquet-1m | 3.69 G → 3.17 G (−14%) | 0.76 → 0.58 s | 103 → 73 MB | 199 ms |
| http-csv-1m | 6.28 G → 6.23 G (−1%) | 1.25 → 1.11 s | 119 → 126–149 MB (R2-11) | 326 ms (479 before round 1's item 6) |
| http-ndjson-1m | 7.05 G → 7.01 G (−1%) | 1.49 → 1.27 s | 177 → 172 MB | 206 ms |
| http-json-1m | 6.31 G → 5.72 G (−9%) | 1.20 → 1.04 s | 72 → 74 MB | 188 ms (336 before item 6) |

### Where the instructions go, on `ef3a0cf`

From `--record`, one run each, in instructions:

| stage | csv-1m | parquet-1m | exec-1m | http-json-1m | exec-10m | exec_series-10m | ch-10m | csv-1k | exec-1k |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| reading a file into its stage (`file.read`) | 3.59 G | | | | | | | 0.48 G | |
| writing the table: `write_table`; `stream.insert` + `stream.commit`; a direct read's commit (`loader.load`) | 1.81 G | 1.94 G | 1.79 G | 1.80 G | 17.86 G | 17.75 G | 17.88 G | 69 M | 6 M |
| the profile kept as rows go in (`stream.watch`) | | | 0.33 G | 0.34 G | 3.23 G | 6.08 G | 3.21 G | | |
| the profile read from the table (`profile.*`) | 0.78 G | 0.83 G | 0.11 G | 86 M | 0.96 G | 0.20 G | 1.18 G | 42 M | 33 M |
| the fetch: pipe, HTTP, decode, checks (`exec.stream`, `builtin.stream`, `http.fetch`) | | | 35 M | 3.00 G | 0.37 G | 0.36 G | 0.55 G | | |
| the engine's attach check (`attach.functions` + `attach.inspect`) | 0.18 G | 0.18 G | 0.18 G | 0.18 G | 0.18 G | 0.18 G | 0.18 G | 0.18 G | 0.18 G |
| opening DuckDB: loader `bench.open`, engine `open_sealed` | 0.11 G | 0.11 G | 0.12 G | 0.13 G | 0.11 G | 0.12 G | 0.12 G | 0.11 G | 0.11 G |
| starting four processes (outside any stage) | 85 M | 84 M | 85 M | 85 M | 84 M | 85 M | 85 M | 84 M | 84 M |
| **the loader, all of it** | **6.26 G** | **2.85 G** | **2.36 G** | **5.32 G** | **22.52 G** | **24.50 G** | **22.91 G** | **0.68 G** | **0.13 G** |
| **every process** | **6.57 G** | **3.16 G** | **2.67 G** | **5.63 G** | **22.83 G** | **24.82 G** | **23.23 G** | **0.99 G** | **0.44 G** |

At 10M rows, three quarters of a stream load is DuckDB writing the table; at 1k rows, two
fifths of a session is the engine's attach check, and a CSV file's sniffing is half of it. By
library, a ch-10m loader's instructions are 73.8% `libduckdb.so`, 17.7% benchy (the profile kept
as rows go in, and the batches into DuckDB's vectors) and 8.2% libc.

## Round 2: the backlog, ranked

Every item below was done or carried into round 3's list, except R2-11 (the HTTP CSV loader's
memory: 131 MB median against the file's 117 MB on `99b5155`, from 148 MB). Ranked by what each could take off the scenarios above, then by how sure the estimate is; a
regression first. "R2-n" names an item of this list; "item n" elsewhere in the repository is
round 1's, below. Every estimate is read off the profile or measured on its own, as said; judge
each change with `bench/compare.py`, before and after.

### R2-1. An NDJSON file's first `describe`: +17% to +32% since it was read straight in (done: staged up to 64 MiB)

`src/source.rs`, `reads_twice` and `read_into`. Round 1 reads an NDJSON or JSON file straight into
its table with one `CREATE TABLE … AS SELECT` on one thread, which keeps the physical layout
the same for every origin but also makes DuckDB's JSON reader parse on that one thread; staged,
it parsed on four. Against `b0e46ae`, interleaved, twelve runs each: first `describe` 833 →
1,099 ms (minimum), 914 → 1,143 ms (median), for 2.6% fewer instructions, 5% less CPU and a loader
of 99 MB instead of 143 MB. (That change reported "as soon, −1%", which these runs do not reproduce.)
Parquet is unaffected: its reader is cheap, and it got 25% sooner.

**Done (2026-10-11): a JSON or NDJSON file of up to 64 MiB is staged; a larger one is read
straight in.** Decided 2026-10-11 by the owner: first-describe latency wins over bounded
load-time memory (≤ 55 MB). A staged file's profile is still kept from one scan of its table
(R2-4). Against `bd8cbd3` (R2-3, R2-4 merged), interleaved, ten runs each, minimum / median,
load average 1 to 2:

| ndjson-1m | first `describe` | instructions | loader CPU | loader peak RSS |
|---|---:|---:|---:|---:|
| straight in (`bd8cbd3`) | 1,083 / 1,116 ms | 6.38 G | 1,004 ms | 89 MB |
| **staged (taken, up to 64 MiB)** | **876 / 898 ms (−19% / −20%)** | 6.56 G (+3%) | 1,093 ms (+9%) | 143 MB |

parquet-1m is unchanged (566 / 612 → 552 / 584 ms, 2.73 G, 60 MB); so was ndjson-1k, measured
against `e09f20a` (268 / 268 ms, the same instructions and memory). The ways that were not taken, measured earlier against `d4afdaa` the same way, as
changes from straight in:

| ndjson-1m | first `describe` | instructions | loader CPU | loader peak RSS |
|---|---:|---:|---:|---:|
| staged | −23% / −16% | +3% | +4% | 99 → 142 MB |
| read on a thread of its own, its chunks referenced into the write | no sooner | +3% | +15% | 103 MB |
| the same, the reader on every thread | no sooner | +18% | +42% | 161 MB |
| read into Arrow on a thread of its own, through a stream's `Ingest` | no sooner | +1% | +12% | 105 MB |

- **The pipe did not pay** (re-measured at a quieter moment for round 3: R3-12, the same verdict). Reading on a second thread while the write takes its vectors (by
  reference, through a table function, no copy) should overlap the parse (about 430 ms of the
  write's 720 ms) with the write. Traced, the write waited on the reader for most of the load,
  and the reader, the same instructions, took twice as long as in the write's own thread: every
  vector crosses cores, at 15% more CPU. On every thread, the reader's ordered buffer held 60 MB
  and its idle workers spun. Not adopted; on a quiet machine with two free cores it may differ.
- **Parallel straight in breaks the layout.** `CREATE TABLE … AS` with the reader on every thread
  writes the same row groups but leaves 7 free blocks of 23 (`pragma_database_size()`), which the
  agent can see.
- **The threshold bounds the memory.** The stage holds the table, about 0.85 times the file, so
  64 MiB costs at most about 55 MB more (89 → 143 MB for the 63 MB harness file); above it the
  loader stays as it is.
- **Parquet re-checked** (against `d4afdaa`): staged, it was no sooner, with 17% more instructions and 101 MB instead
  of 70 MB. It stays straight in.
- **Found on the way:** a file that failed part-way while read straight in reported "Current
  transaction is aborted" instead of DuckDB's reason, since giving the loader its threads back
  failed in the aborted transaction and was reported first. Fixed
  (`a_file_that_fails_midway_leaves_no_table_and_says_why`).

### R2-2. `stream.watch` — 3.2 G per 10M rows of one series, 6.1 G of 500 series; done

`src/profile.rs`, `Watch::add` (68–77% of the stage) and `Times::at` (13% with a keyed tally);
`src/source.rs`, `held` and its iterators (15–20%), which copy each batch's columns into
`Vec<Option<_>>` before the watch reads them. 14% of exec-10m and ch-10m, 24.5% of
exec_series-10m, and on the insert's thread, so all of it is on the path to `describe` (the
table above: exec-10m's cold start did not fall with its CPU).

- **Ideas:** read the Arrow arrays in place: the values slice and the validity bitmap, the time
  unit's scale applied in the loop, no `Option` vectors. For a keyed tally, keep the last key's
  slot, since a meter's rows arrive in runs or in a fixed cycle, and hash each key once for both
  the lookup and the HyperLogLog. Then feed the watch from a second thread with the same
  `RecordBatch` (an `Arc`), so it overlaps the insert. **Estimate:** −1.5 G (−7%) at 10M rows of
  one series, −3 G (−12%) at 500 series; with the thread, up to 0.7 s of wall time at 10M.
  `the_profile_kept_as_rows_arrive_is_the_profile_of_the_table` keeps `describe` the same.
- **Done.** Measured on its own first with a micro-benchmark of `Watch::add` (1M rows, 64 Ki-row
  batches, `perf stat`, instructions a row): one series 326 → 98, 500 series 606 → 404. Where it
  went:
  - `Held::of` reads each column where it lies, values and validity bitmap; numeric columns not
    sent as doubles are converted into buffers kept from batch to batch. A microsecond timestamp
    is read without the scaling's multiply and divide, and the extent kept in locals.
  - One series keeps no tally: its rows, extent and measures are the scan's aggregates already,
    so only the sums of its times are added up (`Series::note`).
  - The key candidates' distinct counts were 66 of one series' 164 instructions a row, and are
    asked for only when times repeat. A first batch whose times do not repeat stops counting
    them; if the rest of the stream repeats its times after all, the profile reads the counts
    from the table (`profile.distinct`), as it reads the pass then.
  - A key a tally keeps a series for is hashed once per series, when the series begins, not once
    a row (`a_kept_distinct_count_of_a_tallied_key_is_duckdbs`); the tally tries the series after
    the last one before its hash map, which a fixed cycle of meters always hits; a time already
    seen on the last row is found without a hash; and the last bytes of a short string are read
    without a call to `memcpy`.
  - The watch runs on a thread of its own (`source::Watcher`), started with the first batch,
    after the loader has confined itself, fed the same `RecordBatch` through a queue of one, and
    joined when the stream ends. Measured against the same code on the insert's thread, 10M rows,
    min of five: instructions the same (±0.1%), CPU the same within the spread, loader peak 0–2 MB
    higher (the batch in the queue), cold start exec-10m 4.04 → 4.17 s, exec_series-10m 4.36 →
    3.92 s (minima; medians within the spread). Kept: the watch is off the path to `describe`,
    and the more series, the more it overlaps.
- **After**, against `e09f20a` (with R2-5 and R2-8), the four binaries — `e09f20a`, then this
  item, then R2-7, then R2-6 on top — interleaved, minima of four runs per 10M scenario and five
  per 1M, load average 2–6:

  | scenario | instructions | `stream.watch` CPU | loader peak RSS | cold start, min |
  |---|---:|---:|---:|---:|
  | exec-1m | 2.65 → 2.42 G (−8.7%) | 68 → 13 ms | 54.9 → 55.0 MB | 712 → 670 ms |
  | exec_series-1m | 2.96 → 2.75 G (−7.1%) | 114 → 57 ms | 55.0 → 55.4 MB | 756 → 675 ms |
  | ch-1m | 2.79 → 2.54 G (−9.0%) | 48 → 15 ms | 59.1 → 59.4 MB | 677 → 629 ms |
  | exec-10m | 23.03 → 20.59 G (−10.6%) | 670 → 160 ms | 98.0 → 98.3 MB | 4.88 → 4.08 s |
  | exec_series-10m | 25.03 → 22.73 G (−9.2%) | 1,004 → 539 ms | 102.5 → 98.7 MB | 5.15 → 3.93 s |
  | ch-10m | 23.72 → 21.25 G (−10.4%) | 464 → 136 ms | 93.5 → 91.1 MB | 3.82 → 3.61 s |

  `stream.watch` is now the watch thread's own CPU; before, it was the process's while the watch
  ran, the same thing then. The 2–4 MB the watch's copies added to a loader's peak are not
  visible against `e09f20a`: R2-8 took the write buffer's 11–16 MB, and the batch the queue holds
  (about 1.5 MB at 64 Ki rows) stands where the copies stood. Loader peaks are within ±2 MB run
  to run.

### R2-3. CSV sniffing — 0.34–0.47 G per CSV table, whatever its size (done for a declared CSV)

`src/source.rs`, `reader_for` (`read_csv_auto`), and `src/sources/http/load.rs`, `reader_of`.
DuckDB's sniffer costs the same for 1,000 rows as for 1,000,000: 47% of a 1k-row CSV session's
instructions and about 120 ms of its first `describe` (csv-1k 329 ms, exec-1k 206 ms), 5.4% at
1M rows. Measured with DuckDB 1.5.5 alone (its Python package, `perf stat`, less an empty
connection), on the harness's 1k-row CSV:

| reader | instructions |
|---|---:|
| `read_csv_auto(f)`, what a file gets | 472 M |
| `read_csv_auto(f, types = {…})`, what a declared HTTP CSV gets | 473 M |
| `delim = ','` stated | 451 M |
| `escape = '"'` stated | 128 M |
| `quote = '"', escape = '"'` stated | 69 M |
| `columns = {…}, header = true, auto_detect = false` | 14 M |

At 1M rows: 3.20 G with the sniffer, 2.86 G declared. The `types` a declared HTTP CSV passes
today do not stop the sniffer.

- **Ideas:** a declared CSV (HTTP today; a file source if it gains a schema) is read with
  `auto_detect = false`, `header = true` and its delimiter, a config field defaulting to `,`.
  For an undeclared CSV, stating RFC 4180's quote and escape would take 400 M off, but would
  misread a file escaped with backslashes; that needs a corpus of real CSVs first.
  **Estimate:** −0.4 G and about 120 ms per declared CSV table.

**Done (2026-10-11) for a declared HTTP CSV: its dialect is stated, not sniffed.** It is read
`read_csv_auto(…, types = {…}, delim = …, quote = '"', escape = '"')`, `delim` from the new
`delimiter` field, `,` unless set. Not `auto_detect = false`: that reads columns by position,
and a body whose header came in another order loaded one column's values into another without
a word (measured: `ts` and `meter` swapped loaded `NULL`s and timestamps as text), and it loses
the sniffer's date formats (`01/02/2026` read as a date). With the dialect stated the sniffer only
reads the header, by which the declaration is matched, and the formats: the same instructions
in DuckDB alone, 66 M against 462 M for the 1k-row CSV (`auto_detect = false`: 13 M). A body in
another dialect fails loudly: another delimiter is `E_SCHEMA` naming `delimiter`, quotes
escaped with a backslash are refused (`a_declared_csv_is_read_in_its_stated_dialect`).

New scenario `http-csvdecl-*`: the CSV with its three columns declared. Against `e09f20a`,
interleaved, ten runs each, minimum / median:

| scenario | first `describe` | instructions | loader CPU | loader peak RSS |
|---|---:|---:|---:|---:|
| http-csvdecl-1k | 357 / 383 → **236 / 251 ms (−34%)** | 0.99 → **0.57 G (−43%)** | 246 → 132 ms (−46%) | 53 → 54 MB |
| http-csvdecl-1m | 1,149 / 1,223 → 1,135 / 1,248 ms | 6.24 → 6.05 G (−3%) | 1,089 → 1,068 ms | 123 → 126 MB |
| http-csv-1k (undeclared, unchanged) | 354 → 368 ms | 0.99 → 0.98 G | 253 → 256 ms | 54 → 54 MB |

- **Not done: a CSV straight into its table now that a declared one is not sniffed.** A body
  comes through a FIFO, which can be read once, so it cannot be asked for its types first; and
  a file has no declaration, so it is still sniffed. Nothing to retry until a file source can be
  declared.
- **Not done: `check` printing the sniffed dialect.** One field, `delimiter`, is left to the
  operator; a wrong one fails the load naming it. `check` already prints the schema to pin.
- **An undeclared CSV** (every file, and an HTTP CSV without a schema) is sniffed as before;
  `delimiter`, when set on an undeclared HTTP CSV, is stated to the sniffer too.

### R2-4. Files are profiled from their table — 0.78–0.83 G per 1M rows (done)

`src/profile.rs`, `compute`, after `src/source.rs` writes a file's table: `profile.scan` and
`profile.pass` each read the table back and decompress it (`memcpy` 22%, `GreaterThan<double>`
18–19%). 26% of parquet-1m's instructions and 12% of csv-1m's; about ten times that at 10M.

- **Idea:** give files the streams' `Watch`: one scan of the written table as Arrow
  (`query_arrow`) through `Watch::add` replaces the scan and the pass, the pass step-free as for
  streams. **Estimate:** −0.3 to −0.5 G at 1M rows (−10 to −15% of parquet-1m), the same
  `describe` (`the_profile_kept_as_rows_arrive_is_the_profile_of_the_table`).

**Done (2026-10-11).** Once a file's table is written (`src/source.rs`, `watched`), the columns
the `Watch` needs are scanned once, streamed as Arrow a DuckDB vector at a time, and read with
`Held::of` as a stream's batches are; the `Seen` goes to `compute` with the table. The same for
an HTTP body DuckDB reads itself (`load_read`). `describe` is the same, byte for byte
(`a_files_profile_is_kept_from_one_scan_and_is_the_profile_of_the_table`). Against `cd26314`
(the watch reading arrays in place), interleaved, ten runs each, minima:

| scenario | instructions | loader CPU | loader peak RSS | first `describe`, min / median |
|---|---:|---:|---:|---:|
| parquet-1m | 3.12 → **2.72 G (−12.7%)** | 563 → 483 ms (−14%) | 60 → 61 MB | 566 / 612 → 563 / 607 ms |
| csv-1m | 6.21 → 5.79 G (−6.7%) | 1,011 → 963 ms (−5%) | 118 → 117 MB | 756 / 817 → 775 / 847 ms |
| ndjson-1m | 6.79 → 6.38 G (−6.0%) | 1,065 → 1,019 ms (−4%) | 90 → 90 MB | 1,084 / 1,149 → 1,088 / 1,160 ms |
| http-parquet-1m | 3.13 → 2.75 G (−12.3%) | 564 → 513 ms (−9%) | 61 → 61 MB | 567 / 649 → 590 / 632 ms |
| csv-1k | 0.98 → 0.99 G | 238 → 250 ms | 53 → 54 MB | 321 / 348 → 328 / 340 ms |

The first `describe` is no sooner: the scan it replaced ran on every thread, the watch on the
load's one after the write. Against `e09f20a`, before that, the same change took 5% off
parquet-1m and 2–3% off csv-1m and ndjson-1m: most of the win is `cd26314`'s faster `Watch::add`.

### R2-5. A host's attach — 280 M instructions and about 120 ms per table; done: 162 M and 52 ms

`src/host.rs` (`attach`, one table per request) and `src/workspace.rs`, `Workspace::attach_all`.
A host session attaching N 1k-row tables, minimum of five (`perf stat`, whole tree): N = 1:
501 M, 2: 777 M, 4: 1,333 M, 8: 2,466 M; each attach after the first 120 ms. Each pays a loader's
start and DuckDB open (about 90 M), its profile (about 30 M) and the engine's check
(`attach.functions` 115 M, `attach.inspect` 67 M). The CLI path already pays the listing once
per batch (round 1, item 5); a host cannot batch.

- **Idea:** an `attach` call that takes several streams, loaded by parallel loaders and checked
  in one `attach_all`, so the listing is paid once. A host-protocol change (docs/HOST-PROTOCOL.md).
  **Estimate:** −115 M (about 40%) per table after the first in a batch.
- **Done: host protocol 2, `attach` with `tables`.** One request lists several tables; their
  loaders run side by side (`run_loaders`, as many as the machine has cores, at most four), and
  every table that loaded is attached in one request to the engine. Each is answered on its own,
  and held to the count of tables and the budget in order (`Room`). Protocol 1 is served as
  before. And the rest of the engine's check is made once a batch too
  (`Workspace::catalog_listings`): one listing each of tables, views, sequences, indexes, types,
  schemas and columns for every catalog of the batch, where each table had a query over six
  listings, one over its columns and one for its comment. From `--record`, `attach.inspect` was
  277 M for eight tables; `attach.listings` and `attach.inspect` are now 82 M for eight and 56 M
  for one, against 67 M. That holds for a folder's tables and a rebuild's as well.
- **After**, `bench/ingest.py --scenarios host-1k,host2-1k,host4-1k,host8-1k` (`host<N>` attaches
  N tables, in one request where the binary speaks protocol 2), against `d4afdaa`, the two run
  back to back, minimum of five, load average 6–7:

  | tables | instructions | CPU, all processes | engine CPU | cold start, min |
  |---:|---:|---:|---:|---:|
  | 1 | 508 M → 487 M (−4.0%) | 202 → 189 ms | 107 → 95 ms | 253 → 244 ms |
  | 2 | 800 M → 653 M (−18.4%) | 326 → 273 ms | 144 → 108 ms | 454 → 273 ms |
  | 4 | 1,382 M → 978 M (−29.2%) | 585 → 455 ms | 218 → 108 ms | 715 → 403 ms |
  | 8 | 2,484 M → 1,626 M (−34.6%) | 1,104 → 819 ms | 375 → 116 ms | 1,159 → 607 ms |

  Each table after the first: 282 M → 162 M instructions (−43%), 129 → 52 ms to `describe`.
  Loader peak RSS unchanged (48–49 MB); host-1m unchanged (2.73 G, within ±0.5%).
- **What a table after the first still costs** is its loader, about 145 M of the 162 M: from
  `--record` of host8-1k, per loader, `bench.open` 56 M (DuckDB's open, its catalog of built-in
  functions and types), `profile.step` 28 M and `profile` 14 M (the profile's queries on a
  1,000-row table, much of it DuckDB's worker threads spinning for work: `ExecuteForever`, CAS
  and swap), `bench.finish` 15 M (the checkpoint), and 20 M outside any stage, mostly the dynamic
  linker resolving `libduckdb.so`'s symbols (`do_lookup_x`, 16.7% of the loaders'
  instructions). The engine's part is about 5 M. Not taken here: the profile is R2-2's and
  R2-7's, and a loader's start is R2-10's.

### R2-6. Arrow LZ4 bodies are decoded twice — 1.7 G more per 10M rows; done: 0.8 G taken

`src/ipc.rs`, `count_compressed`: each LZ4 buffer is decoded once to check its claimed size,
then again by arrow-ipc. New scenario `exec_lz4-*` (bench/gen_data.py writes the same rows with
LZ4 bodies, as ClickHouse's `ArrowStream` sends them by default): 24.77 G against exec-10m's
23.07 G (+7.4%), 2.85 G against 2.65 G at 1M.

- **Idea:** decode each buffer once, into a buffer of its claimed size (read to one byte past
  it, as now), and hand arrow the decoded body as an uncompressed message. **Estimate:** −0.8 G
  per 10M rows (−3.5%) for a connector that passes ClickHouse's bodies through.
- **Done.** `ipc::next` decodes each LZ4 buffer into the message's new body as it checks the
  claim, prefixed `-1` — the format's mark for a buffer stored uncompressed, which arrow slices
  without a copy — and rewrites the buffers' places and the body's length in the metadata, both
  fixed-size values there (`a_compressed_message_reaches_arrow_decoded_once`). The bound is
  unchanged: a claim past what is left is refused on the claim, and a buffer is decoded one byte
  past its claim at most (`a_compressed_buffer_counts_what_it_decodes_to_and_may_not_lie`). In
  the same interleaved session as R2-2: exec_lz4-10m 21.60 → 20.78 G (−3.8%), loader CPU 4.05 →
  3.73 s, loader peak 99.9 → 98.8 MB; exec_lz4-1m 2.60 → 2.50 G (−3.8%); every other scenario
  the same. An LZ4 stream now costs 0.9 G more than the same rows uncompressed at 10M
  (20.78 G against 19.90 G), the one decode.
- **zstd bodies stay refused.** The same 10M rows with zstd Arrow bodies are 76.8 MB, against
  103.7 MB with LZ4 and 41.5 MB for the whole stream compressed by HTTP zstd, which the
  ClickHouse and HTTP sources already read (round 1, item 3). Only a connector or host that
  cannot compress its transport would gain; not before one asks.

### R2-7. `profile.runs` for one series — 0.72 G per 10M rows; done

`src/profile.rs`: the runs of repeated values are still read from the table for a one-series
table (3% of exec-10m and ch-10m). **Idea:** keep each measure's longest run in the one-series
tally as rows arrive, as the keyed tally keeps its passes. **Estimate:** −0.7 G (−3%).

- **Done.** The watch keeps one series' rows of 0 or null with their times as it counts them, up
  to the keyed tally's bound (`WATCH_FLAT_MAX`), and `runs_of` finds the runs from them, as for
  a keyed tally (`the_profile_kept_as_rows_arrive_is_the_profile_of_the_table`, a series with
  runs of 0 and of null, read without a query). In the same session as R2-2: exec-10m 20.59 →
  19.87 G (−3.5%), ch-10m 21.25 → 20.53 G (−3.4%), exec_lz4-10m −0.72 G; at 1M rows nothing to
  take (the runs' query runs only when a series has a day's worth of zero or null rows). Loader
  peak within ±1 MB.

### R2-8. Loader memory — DuckDB's write buffer, 11–16 MB; done

`src/workspace.rs`, `Bench::open_in`. DuckDB buffers five row groups of a bulk insert before
writing them (`write_buffer_row_group_count`). Set to 1 in every loader, three runs each:
instructions within ±0.6%, loader peak parquet-1m 72 → 59 MB, exec-1m 71 → 56 MB, exec-10m 116 →
100 MB, ch-10m 103 → 92 MB; the table files the same size (ch-10m within 1 MB).

- **Idea:** set it, after the layout tests and `pragma_storage_info` show the same segments.
  **Estimate:** −10 to −20% of a loader's peak, every source alike.
- **Done: every loader sets it** (`Bench::open_in`, `a_loader_writes_each_row_group_as_it_fills`).
  Against `d4afdaa`, back to back, minimum of three:

  | scenario | instructions | loader peak RSS | written by the loader |
  |---|---:|---:|---:|
  | csv-1m | −0.8% | 117.0 → 117.1 MB | 4.3 → 4.3 MB |
  | parquet-1m | −0.7% | 71.6 → 59.6 MB (−17%) | 4.3 → 4.3 MB |
  | exec-1m | 0.0% | 70.3 → 57.1 MB (−19%) | 4.3 → 4.3 MB |
  | host-1m | +0.1% | 70.5 → 57.1 MB (−19%) | 4.3 → 4.3 MB |
  | ch-1m | −0.2% | 73.0 → 61.2 MB (−16%) | 9.5 → 9.5 MB |
  | exec-10m | +0.1% | 116.4 → 100.9 MB (−13%) | 40.8 → 40.8 MB |
  | ch-10m | 0.0% | 104.2 → 93.6 MB (−10%) | 32.9 → 33.6 MB (+2%) |

  A CSV's loader does not move: its peak is the in-memory stage the reader fills.
- **The layout.** `the_physical_layout_says_nothing_of_where_data_came_from` and
  `an_http_table_has_the_physical_layout_of_a_file_table` pass. On the eval's 840,960 meter
  rows, loaded from CSV, Parquet and an Arrow stream: `pragma_storage_info` is the same for all
  three, and the same as before but for block ids and offsets: 7 row groups, 62 segments, the same
  compression for each (ALP-RD, RLE, Dictionary, Constant). Each row group's segments now start a
  block of their own, where they used to share the previous one's last block, so the file is
  29 blocks (7.3 MiB) where it was 28 (7.0 MiB).
- **Queries as fast.** Seven agent queries over the meter data (count, per-meter aggregates,
  hourly `time_bucket`, a daily average of one meter, a `lag` window, `quantile_cont`, a month's
  range), minimum of nine each, through the MCP server: 368 and 392 ms in all before, 340 and
  378 ms after, in two interleaved rounds. Every query within the noise.

### R2-9. DuckDB's own write — 17.6–17.9 G per 10M rows; its settings measured, nothing to take

The write is 74–78% of a 10M-row stream load. Inside `stream.insert` at ch-10m:
`StringStats::Update` 12.3%, the dictionary compressor's analysis and lookups 12.5%,
`Hash<string_t>` 5.4%, `GreaterThan<double>` 5.4% (the doubles' statistics),
`Utf8Proc::Analyze` 4.1%, `UncompressedStringStorage::StringAppend` 4.0% and
`StringScanPartial` 3.0% (rows go into the transaction's local storage uncompressed and are read
back to be compressed), ALP 3.8%, FSST 3.6%, bitpacking 3.5%, `memcpy` 7.2%, and
`__aarch64_ldadd8_acq_rel` 2.4% (the release library is built with outline atomics).

Every setting in `duckdb_settings()` that touches compression or statistics, measured in every
loader's `Bench::open_in` with the harness:

| setting | instructions | table files | verdict |
|---|---:|---|---|
| `storage_compatibility_version = 'latest'` (from `v0.10.2`) | −0.5% (exec-10m, ch-10m, exec_series-10m), inside the spread; measured again on top of R2-8, three runs: exec-10m −0.1%, exec-1m −0.2%, host-1m +0.1%, csv-1m +2.4%, parquet-1m +1.9% | same size; on the meter data the same segments and compression, the header still `v1.0.0+`, and 6,341 bytes of one block of `kw`'s ALP-RD segments written differently | nothing to take, and a different file for it; not taken. (Set with `SET` after the open, it never applied to the file: see "A loader's file in storage version v1.3.0", round 3, where set at the open it took 6–18% off) |
| `write_buffer_row_group_count = 1` | ±0.6% | same size (ch-10m +2%) | taken: R2-8 |
| `threads = 1` for the whole load | csv-1k −1.5%, csv-1m −0.1% | same | nothing: the write is on one thread already |
| `disabled_compression_methods` | FSST off −4.9% (round 1) | 2.5× for free text | kept on (round 1, item 1) |
| `force_compression` | not run | one method for every column | changes the layout |

No setting skips the statistics or the analysis. What is left is DuckDB's: built from source
(R2-10) with LSE atomics for this CPU, the 2.4% of `ldadd`; anything more is a change to DuckDB.

### R2-10. Starting processes — 21 M instructions and about 10 ms each

Four per session (server, warden, engine, loader) and one per further source: 84 M, 19% of an
exec-1k session; 95% of it the dynamic linker resolving `libduckdb.so` (round 1, item 7).

- **Ideas:** a warden without DuckDB, which only waits and removes a directory: −21 M per
  session (−5% at 1k), for a second binary to build and find. DuckDB built from source with its
  extensions (ICU, Parquet, JSON, `core_functions`) as a static library with hidden visibility:
  about −80 M (−18% at 1k) and the atomics of R2-9, for a much longer build.

### R2-11. The HTTP CSV loader's memory — 126 to 149 MB against 117 MB from a file

The same 1M-row CSV, over HTTP and from a file. Across runs the HTTP loader peaks at 126 or
143–149 MB (one session: 126, 126, 143, 148, 149; another: 149 five times); round 1's 119 MB was
measured on an earlier revision of the HTTP branch, and `1f5b792` as merged already shows 149 MB.
Sampled every 2 ms: both paths reach 115 MB reading the body into its stage; the file path then
writes at 106 MB at most, the HTTP path climbs to 149 MB in `write_table`. DuckDB alone (its
Python package, the same stage and write, a FIFO against a file) peaks at 130 MB either way, so it
is not the CSV reader on a pipe.

- **Next:** DuckDB's allocator statistics at the start of `write_table` in both paths. Memory
  only, about 30 MB, HTTP CSV only.

### R2-12. JSON pages — `http.fetch` 3.0 G per 1M rows

`src/sources/http/load.rs`, `PageDecoder`: arrow-json's tape decoder is 46% of the stage and its
timestamp parsing about 12% (`TimestampParser::new`, chrono's `from_ymd_opt`, …). It is already
cheaper than DuckDB reading the same rows as NDJSON (5.72 G against 6.77 G in all). **Idea:** a
fast path for RFC 3339 times in UTC. **Estimate:** −0.3 G (−6% of http-json-1m).

## Round 1: the backlog as it was done

Round 1's baseline (`7ec65e2`), where its instructions went, and the items as they were done and
measured, kept as the record that code comments and other documents cite ("item n").

### Round 1's baseline

`main` at `7ec65e2` (streaming ingest) plus this tracing, 2026-10-11. Five runs per 1M
scenario, three per 10M; minima.

| scenario | instructions | CPU, all processes | loader CPU | loader peak RSS | cold start (min / median) |
|---|---:|---:|---:|---:|---:|
| csv-1m | 6.34 G | 1.24 s | 1.11 s | 117 MB | 1.22 / 1.31 s |
| parquet-1m | 3.72 G | 0.82 s | 0.68 s | 102 MB | 0.93 / 1.12 s |
| ndjson-1m | 7.25 G | 1.51 s | 1.36 s | 154 MB | 1.84 / 2.23 s |
| exec-1m | 3.19 G | 0.76 s | 0.59 s | 69 MB | 1.31 / 1.46 s |
| ch-1m | 3.82 G | 0.83 s | 0.68 s | 78 MB | 1.60 / 1.94 s |
| host-1m | 3.21 G | 0.72 s | 0.59 s | 67 MB | 1.98 / 2.36 s |
| exec-10m | 27.88 G | 5.54 s | 5.12 s | 113 MB | 12.3 / 12.7 s |
| ch-10m | 34.20 G | 5.67 s | 5.54 s | 108 MB | 6.2 / 6.5 s |

Around every load, whatever its size: the engine 105–122 ms of CPU and 41–45 MB, the server
about 25 ms and 17 MB, the `cat` connector 24 ms (1M) and 233 ms (10M) at 8 MB. ClickHouse itself
spent 0.31 s of CPU serving 1M rows and 2.26 s serving 10M (zstd, 22.9 and 228.9 MiB of Arrow).
The loader wrote a 4.3 MB table file for 1M rows and 33–41 MB for 10M.

#### HTTP (REQ-17)

The `http-*` scenarios fetch the same 1M rows from a server in the harness's own process (so it
is in no count here), as each format the HTTP source reads; `http-json-1m` is the NDJSON's rows as
cursor-paged JSON, 10,000 to a page. Three runs each, minima, measured on the HTTP source's branch
(2026-10-11), against the file baselines measured in the same session:

| scenario | instructions | loader CPU | loader peak RSS | its file | file instructions | file loader CPU |
|---|---:|---:|---:|---|---:|---:|
| http-arrow-1m | 3.21 G | 0.59 s | 70 MB | exec-1m | 3.21 G | 0.60 s |
| http-parquet-1m | 3.69 G | 0.76 s | 103 MB | parquet-1m | 3.70 G | 0.82 s |
| http-csv-1m | 6.28 G | 1.25 s | 119 MB | csv-1m | 6.36 G | 1.12 s |
| http-ndjson-1m | 7.05 G | 1.49 s | 177 MB | ndjson-1m | 7.05 G | 1.33 s |
| http-json-1m | 6.31 G | 1.20 s | 72 MB | ndjson-1m | 7.05 G | 1.33 s |

Each HTTP format costs what its file costs, in instructions; CSV and NDJSON spend 0.1–0.2 s more
loader CPU moving the body through the pipe DuckDB reads (kernel and writer thread, which
`instructions:u` does not count). docs/HTTP-SOURCE.md ("As built") has why each path is the one
it is.

#### Where the instructions went

From `--record`, one run each, in instructions:

| stage | csv-1m | parquet-1m | exec-1m | ch-1m | exec-10m | ch-10m |
|---|---:|---:|---:|---:|---:|---:|
| reading the file into a staging table (`file.read`) | 3.19 G | 1.25 G | | | | |
| the single-threaded write of the table (`write_table`) | 1.74 G | 1.74 G | | | | |
| batches into the table (`stream.insert` + `stream.commit`) | | | 1.92 G | 1.99 G | 19.20 G | 19.54 G |
| the profile `describe` renders (`profile`) | 0.83 G | 0.86 G | 0.85 G | 0.88 G | 7.73 G | 7.92 G |
| the fetch itself: HTTP, zstd, Arrow decode and checks | | | 0.04 G | 0.56 G | 0.36 G | 6.21 G |
| the engine attaching the table (`attach.inspect`) | 0.19 G | 0.20 G | 0.19 G | 0.19 G | 0.20 G | 0.19 G |
| opening DuckDB: loader `bench.open`, engine `open_sealed` | 0.11 G | 0.12 G | 0.12 G | 0.12 G | 0.16 G | 0.11 G |
| starting four processes (dynamic linking, mostly) | 0.09 G | 0.09 G | 0.09 G | 0.09 G | 0.09 G | 0.09 G |
| **the loader, all of it** | 6.03 G | 4.01 G | 2.90 G | 3.53 G | 27.41 G | 33.78 G |

By library, 89% of a 10M exec loader's instructions are in `libduckdb.so`, 9% in libc (mostly
`memcpy`) and 1.6% in benchy; for ch-10m, benchy's
share was 18%, nearly all of it the zstd decoder (item 3 has since replaced it).

Round 1's list was ranked by what each could take off a 10M-row stream load (34 G instructions
from ClickHouse, 28 G from a connector) and a 1M-row file load, then by how sure the estimate is.
Each estimate is an upper bound read off the profile; measure before and after with
`bench/compare.py`.

### 1. Writing the batches: compression analysis and statistics — up to 19 G per 10M rows (done in part)

`src/source.rs`, `Ingest::insert` and `Ingest::finish`: 57% of a 10M ClickHouse load. Inside it:
`StringStats::Update` 11.6%, the dictionary compressor's analysis and lookups 10%,
`Utf8Proc::Analyze` 4.8%, `Hash<string_t>` 4.5%, ALP for the doubles 3.3%, FSST analysis and
compression about 3%, bitpacking analysis 1.8%, `memcpy` 9%. DuckDB analyses every compression
method for every segment of every column, then compresses with the winner.

**Done (2026-10-11), on `1f5b792`.** Measured with the harness, five runs per 1M scenario and
three per 10M, minima, against `1f5b792` itself (so libzstd and loopback `none` are in both):

| scenario | instructions before | after | change | loader CPU | loader peak RSS |
|---|---:|---:|---:|---:|---:|
| exec-1m | 3.17 G | 3.05 G | −3.7% | −3.2% | 69 MB, same |
| ch-1m | 3.30 G | 3.17 G | −3.8% | −2.2% | 71 MB, same |
| host-1m | 3.22 G | 3.11 G | −3.4% | −7.5% | 68 MB, same |
| http-arrow-1m | 3.20 G | 3.07 G | −3.9% | −8.5% | 69 MB, same |
| http-json-1m | 6.25 G | 6.09 G | −2.5% | −1.9% | 70 MB, same |
| exec-10m | 27.81 G | 26.42 G | −5.0% | −12.4% | 114 MB, same |
| ch-10m | 28.51 G | 27.08 G | −5.0% | −9.3% | 103 MB, same |

The files and the HTTP bodies DuckDB reads itself (`csv-1m`, `parquet-1m`, `ndjson-1m`,
`http-csv-1m`, `http-parquet-1m`, `http-ndjson-1m`) do not take this path; their changes, −4.0%
to +0.1%, are their spread. The tables written are the same, byte for byte in size.

- **Each batch goes into DuckDB's vectors directly** (`fill` in `src/source.rs`): fixed-width
  columns copied as they lie, validity 64 rows at a time, and each string written without the
  C API's UTF-8 check (`duckdb_unsafe_vector_assign_string_element_len`), inline when it is 12
  bytes or less. duckdb-rs's conversion made one C call per value, checked each string's UTF-8
  that arrow had already checked, and set nulls one call each. −4.6% (exec-10m) and −3.7%
  (ch-10m) on its own. Strings are still checked once more by DuckDB as it writes the column's
  statistics (`StringStats::Update`); that one is inside the write and stays.
- **The INSERT is prepared once per stream.** −0.3% on its own, inside the spread: planning the
  statement per 64 Ki-row batch was cheap. Kept, since a connector that sends small batches pays
  it per batch. One `INSERT … SELECT` pulling every batch would need the fetch on another thread,
  for the same nothing.
- **Not adopted: gathering to a row group** (`GATHER_ROWS` 122,880). It pairs the 64 Ki-row
  batches: −0.1% (exec-10m) and −0.8% (ch-10m) of instructions, inside the spread, and 5 MB more
  peak memory for the copy.
- **Not adopted: fewer compression candidates.** `disabled_compression_methods` with FSST off
  took 4.9% (exec-10m) and 4.3% (ch-10m) of instructions off, and wrote the same file for these
  tables and the eval's meter data (dictionary for the meter, RLE or bitpacking for the time,
  ALP or ALPRD for the readings). But 1,000,000 rows of unique, compressible text (a URL per row,
  a UUID per row) then took 96 MB instead of 39 MB, 2.5 times the disk and the engine's memory
  for any table with free text, for queries 20–35% faster on that text. Chimp, Patas, ZSTD,
  Roaring and DICT_FSST off saved nothing (−0.3% to +0.1%). **Decided 2026-10-11: kept on.**
  Keeping free-text tables small, on disk and in the engine's memory, matters more than about 5%
  of a load's instructions. Revisit if FSST could be turned off only for the columns the
  dictionary already wins.

### 2. The profile — 7.7–7.9 G per 10M rows, 0.8–0.9 G per 1M (done for streams)

`src/profile.rs`: 23–29% of every load, run after the table is written, as one pass that
scans it back. Its top symbols are `GreaterThan<double>` 13%, `Equals<double>` 8%, `memcpy`
10%, `DatePart` 6%, `IsNan` 6.6%, min/max and `SUM` aggregates; the scan decompresses what
the write just compressed (ALP, bitpacking).

**Done for streams (2026-10-11).** Measured on `1f5b792` with item 1, against item 1 alone. A stream's profile is kept from
its batches as they go into the table (`profile::Watch`, fed by `Ingest::insert`): the scan's
counts, extents, ranges, nulls and zeros; its distinct counts, as DuckDB's own
`approx_count_distinct` — the same 64-register HyperLogLog over the same hashes, ported and
checked against DuckDB from none to 250,000 values (`a_kept_distinct_count_is_duckdbs`), so every
choice it drives is the same; and the pass, step-free (each series keeps its first time, the gcd
of every distance from it, and the sums of the distances and their squares, from which the phase
and the checksums follow once the step is known). Only the bounded queries still read the table:
the step's sample, the gaps of irregular series, the runs and the moments a tally could not
keep. `describe` is byte for byte the same
(`the_profile_kept_as_rows_arrive_is_the_profile_of_the_table`, tests/shape.rs, the meter data).

Minima of five runs at 1M and three at 10M:

| scenario | instructions before | after | change | loader CPU | loader peak RSS |
|---|---:|---:|---:|---:|---:|
| exec-1m | 3.05 G | 2.69 G | −11.8% | −13.7% | +1.9 MB |
| ch-1m | 3.17 G | 2.81 G | −11.4% | −10.4% | +1.5 MB |
| host-1m | 3.11 G | 2.75 G | −11.6% | −9.3% | +1.9 MB |
| http-arrow-1m | 3.07 G | 2.69 G | −12.4% | −12.2% | +2.4 MB |
| http-json-1m | 6.09 G | 5.73 G | −6.0% | −4.6% | +2.6 MB |
| exec_series-1m | 3.44 G | 2.98 G | −13.3% | −17.7% | +2.0 MB |
| exec-10m | 26.42 G | 23.04 G | −12.8% | −11.5% | +3.2 MB |
| ch-10m | 27.08 G | 23.70 G | −12.5% | −12.8% | +0.8 MB |
| exec_series-10m | 29.37 G | 24.95 G | −15.0% | −17.7% | +4.4 MB |

What it took, measured on the way:

- **One tally, not every one.** Keeping the one-series tally and a keyed tally per candidate for
  every row cost what the scan it replaced did (`stream.watch` 6.0 G at exec-10m, the loader's
  total +0.6%). The first batch now chooses: times that repeat keep the keyed tallies, else the
  one-series one; the other cut, if the profile makes it after all, is read from the table.
  Counted exactly on that batch: with 64 registers, the HyperLogLog's estimate of 65,536 distinct
  times fell under 95% of them on one ClickHouse batch and chose wrong.
- **One division per row for the gcd**, not Euclid's loop, when the distance is a multiple of
  what is kept, as it nearly always is; and the batch's null bitmap read directly, where
  `Array::is_valid` through `dyn Array` was a call per value.
- **What is left** is `stream.watch` at about 3.2 G per 10M rows, nearly all of it in
  `Watch::add`: the hash lookup per row of a keyed tally, the two HyperLogLogs, and the columns
  copied into `Option` vectors per batch. Reading the Arrow arrays in place would take some of
  it.
- **Files** are still profiled from their table: they are written by DuckDB, not by `Ingest`.

Not done:

- **Push the aggregates down to ClickHouse**, which has the data already sorted and indexed:
  most of the profile's cost moves to the server, which is usually not the machine benchy runs on.
- **HyperLogLog for the distinct counts**: they already are DuckDB's; exact counts would change
  what the heuristics choose at their edges, and need memory a 10M-row table of unique times
  cannot spare.
- **Parallelise the pass**: wall time only; the instructions stay.
- **Profile lazily on the first `describe`**: it moves the cost rather than removing it, and
  `describe` is usually the first call.

### 3. zstd decoding in pure Rust — done: 4.4 G off a 10M-row zstd load, 5.9 G off the default

`src/sources/clickhouse.rs` read the response through `ruzstd`: 90% of `builtin.stream`
(6.21 G at 10M), 16% of the whole load. Two changes, measured on `main` at `8c5ee38`
(the ClickHouse loader confined) before and after, 2026-10-11.

**The decoder.** [`spike/decompress`](../spike/decompress) decodes one ClickHouse 26.8
`ArrowStream` response of the 10M-row table (240 MB of Arrow), saved with `curl`, through each
candidate the way the loader reads a body (a 64 KiB `BufReader`, 64 KiB reads). Instructions
from `perf stat -e instructions:u`, identical in each of three runs; CPU is the best of three;
each build alone, clean, 2 jobs, on a machine at load 10–20:

| body | on the wire | decoder | instructions | CPU | cold build | build's peak RSS | text size |
|---|---:|---|---:|---:|---:|---:|---:|
| (none: read, decode nothing) | 240.0 MB | — | ~0 | 0.05 s | 1.2 s | 122 MB | 337 KB |
| zstd | 41.5 MB | ruzstd 0.9.0 (what benchy had) | 5.99 G | 0.75 s | 7.5 s | 203 MB | +119 KB |
| zstd | 41.5 MB | **libzstd 1.5.7** (`zstd` 0.13.3, C, built by `cc`) | **1.64 G** | **0.29 s** | 75 s | 241 MB | +93 KB |
| LZ4 frame (`Accept-Encoding: lz4`) | 111.2 MB | lz4_flex 0.13 | 0.95 G | 0.16 s | 5.6 s | 164 MB | +17 KB |
| Arrow's LZ4 bodies | 123.1 MB | arrow-ipc, over reading the plain stream | +0.70 G | +0.09 s | | | |
| gzip | 72.2 MB | flate2 1.1 + miniz_oxide | 4.61 G | 0.66 s | 10.0 s | 169 MB | +47 KB |
| gzip | 72.2 MB | flate2 1.1 + zlib-rs 0.6 | 4.53 G | 0.53 s | 11.4 s | 212 MB | +58 KB |
| deflate (zlib) | 72.2 MB | miniz_oxide / zlib-rs | 3.93 / 3.67 G | 0.60 / 0.46 s | | | |

libzstd decodes in 27% of ruzstd's instructions and 39% of its CPU. Adopted, in
[`src/decompress.rs`](../src/decompress.rs), which the HTTP source will share: its C build
needs `cc`, which `ring` already needs, so no new toolchain; it reads no file, so the confined
loader is unaffected; and it is bounded the same way: output only as the reader pulls
(`a_bomb_is_decoded_only_as_far_as_it_is_read`), a window of at most 32 MiB
(`a_zstd_window_past_32_mib_is_refused`). The release binary shrank 64 KB. Arrow's own LZ4 costs
the loader two passes (src/ipc.rs checks each buffer's claimed size by decoding it once more), so
about 1.4 G. For gzip, when the HTTP source needs it: zlib-rs, 20% less CPU than miniz_oxide for
the same instructions.

ClickHouse's own CPU to send the same 10M rows, from `/proc` (best of five): none 0.65 s,
Arrow's LZ4 0.77 s, zstd 2.29 s, gzip 2.92 s, LZ4 over HTTP 4.13 s.

**The default, by where the server is.** With both ends on one machine, zstd costs ClickHouse
about 1.6 s of CPU to compress 10M rows and benchy 0.3 s to decode them, to save moving bytes
that loopback moves for almost nothing. So an unset `compression` is now `none` for a server on
loopback and `zstd` anywhere else (`an_unset_compression_follows_where_the_server_is`). A private
address is not treated as near: on 1 Gbit/s, `none`'s 240 MB take about 2 s where zstd's 42 MB
take 0.3 s.

bench/ingest.py, three runs each, minima; CPU is benchy's processes and ClickHouse's together:

| scenario | before: instructions | after: instructions | before: CPU, all | after: CPU, all | ClickHouse's share, after |
|---|---:|---:|---:|---:|---:|
| ch_zstd-10m | 34.34 G | 29.96 G (−12.8%) | 8.52 s | 7.73 s | 2.47 s |
| ch_lz4-10m | 29.92 G | 29.85 G | 6.71 s | 6.63 s | 1.09 s |
| ch_none-10m | 28.54 G | 28.52 G | 6.10 s | 5.97 s | 0.91 s |
| ch-10m, the default on loopback | 34.34 G (zstd) | 28.47 G (none, −17%) | 8.52 s | 5.97–7.08 s | 0.95 s |
| ch_zstd-1m | 3.83 G | 3.49 G (−8.8%) | 1.09 s | 1.07 s | 0.30 s |
| ch-1m, the default on loopback | 3.83 G (zstd) | 3.32 G (none, −13%) | 1.09 s | 0.94 s | 0.15 s |

The instructions are benchy's; the CPU column moves with the machine's load (the default's 7.08 s
run read the same 28.5 G instructions as `none`'s 5.97 s). What is left of a ClickHouse load is
the write (item 1) and the profile (item 2).

### 4. Files: a staging table, then a single-threaded write — about 1.7 G per 1M rows (done for Parquet and JSON)

`src/source.rs`, `load_file_in`: DuckDB reads the file into an in-memory staging table with all
its threads (`file.read`: CSV 3.19 G, NDJSON 3.93 G, Parquet 1.25 G per 1M rows), then
`write_table` converts it and writes the table on one thread (1.74 G, the same for every format).
The staging table costs its own string appends and statistics: Parquet's read is 1.25 G here
against 0.60 G for DuckDB's Parquet reader alone on similar data (`spike/src/probe7_http_decode.rs`).

**Done (2026-10-11), on `b0e46ae` (items 1 and 2 in).** A Parquet file, an NDJSON or JSON file,
and an HTTP Parquet body (staged as a file) are read straight into the write: `DESCRIBE` the
reader for its types, then one `CREATE TABLE … AS SELECT <conversion> FROM <reader>` on one
thread, in a transaction (`source::read_into`). A held body is read to one row past its cap and
checked after; a refusal rolls back and leaves no blocks
(`a_body_read_into_its_table_is_held_and_refused_whole`). The layout is unchanged
(`the_physical_layout_says_nothing_of_where_data_came_from`,
`an_http_table_has_the_physical_layout_of_a_file_table`). Five runs, minima, against items 1
and 2:

| scenario | instructions before | after | change | loader CPU | loader peak RSS | first `describe` |
|---|---:|---:|---:|---:|---:|---:|
| parquet-1m | 3.69 G | 3.15 G | −14.4% | −10.9% | 102 → 72 MB | −26% |
| ndjson-1m | 7.01 G | 6.78 G | −3.2% | −13.1% | 143 → 99 MB | −1% |
| http-parquet-1m | 3.71 G | 3.26 G | −12.2% | −15.8% | 103 → 74 MB | −10% |

Not read straight in, measured or by necessity:

- **A CSV or TSV file**: read straight in, it cost 2.6% *more* instructions and its first
  `describe` came 21% later (972 → 1,177 ms). DuckDB parses a CSV file on every thread, and a
  write on one thread gives that up; its types are also sniffed twice. It stays staged.
- **CSV and NDJSON over HTTP** arrive through a FIFO, which can be read once: asking it for its
  types would consume it. They were already read on one thread. Staged; unchanged.
- **A narrowed HTTP body** is charged the rows the reader produced before the narrowing drops
  any, which only the stage counts. Staged.
- **Through `Ingest`** (DuckDB's reader to Arrow, Arrow back into a vector) would copy every
  value twice to save one; not tried.
- The CSV reader's own time (`BaseScanner::Process`, date and time parsing) is DuckDB's; the
  sniffer's `ColumnCountResult` pass is 4%. Stating the CSV's types in the config would skip it.

### 5. The engine's attach check — 0.19 G and about 60 ms per table; done: paid once per batch

`src/workspace.rs`, `Workspace::attached_table`: one query over `duckdb_tables()`,
`duckdb_views()`, `duckdb_functions()`, `duckdb_sequences()`, `duckdb_indexes()`,
`duckdb_types()` and `duckdb_schemas()` filtered to the attached catalog. `duckdb_functions()`
builds a row for every built-in function before the filter (`ExtractFunctionData`,
`Vector::SetValue`, `LogicalType` copies dominate the engine's profile). 70% of the engine's
instructions, 5–6% of a 1M load, and on the cold-start path for every table.

- **Asking the attached catalog alone is not possible in DuckDB 1.5.** `duckdb_functions()`
  registers neither filter nor projection pushdown (its source: every one of its 22 columns is
  filled for each of ~3,000 built-in functions and their overloads, then filtered), the
  `information_schema` views are built on it, and no other table function lists a catalog's
  macros — the only kind of function a DuckDB file can carry. Measured with DuckDB 1.5:
  `duckdb_functions()` filtered to one catalog took 230–360 ms on the loaded machine against
  3–10 ms for each of the other six listings.
- **Done: one listing per batch.** `Workspace::attach_all` attaches and checks every file of a
  batch on its own, then asks `duckdb_functions()` once for all their catalogs, before any of
  them reaches the search path; the server attaches every table a session starts with, and every
  table of a rebuild, in one request. As strict as before:
  `an_attached_file_brings_its_table_and_nothing_else` holds, and
  `a_function_in_one_file_of_a_batch_refuses_that_file_alone` refuses the file with a macro
  and attaches the ones around it. `the_tables_a_session_starts_with_are_attached_in_one_request`
  is the test for the batching.
- One table pays the same as before: `attach.inspect` is 24–28 ms of the engine's CPU and the
  listing (`attach.functions`) the other ~30 ms. Each table after the first saves the listing:
  a folder of four 100-row CSVs, interleaved runs, from spawning benchy to `describe`,
  1,750 → 1,395 M instructions and 774 → 662 ms (minimum of 11; before item 6, 1,826 M and
  934 ms).
- A host attached one table per request, and still paid per table, until host protocol 2 (R2-5).

### 6. Waiting for the engine to open — done: about 70 ms off every cold start, and the first load overlaps the open

`src/engine.rs`, `Watch::stop`: the engine's `Open` took about 102 ms of wall time in every run,
of which `open_sealed` is 25–37 ms and `confine` 0.1 ms. The rest was `Watch::stop` joining a
thread that slept in 100 ms steps. The server waited for `Open` before it started the first
loader (`main.rs`, `Supervisor::start`), so all of it was on the cold-start path. No CPU was spent.

- **Done.** The watch waits on a condvar, and stopping it wakes it
  (`stopping_the_watch_does_not_wait_out_its_poll`): the engine's `Open` went from 100–102 ms
  to 26–31 ms of wall time in every scenario (minimum of 8 runs).
- **Done.** `Supervisor::start` sends `Open` and returns; the first loader runs while the engine
  opens, and the first attach waits for the answer (`the_first_loader_starts_while_the_engine_opens`).
  The engine is a process of its own, so it still seals and confines with no other thread
  (`sandbox::alone`). Since every table of a session is attached in one request (item 5), every
  loader runs while the engine opens, and an engine that cannot open stops startup once the
  loads are done rather than before them, with the same message. A host's session still waits for `Open` before it
  answers `hello`, which reports the engine and its sandbox.
- Measured with the two together against `1f5b792`, interleaved runs on a shared 4-core
  machine (load 4), a folder of one 100-row CSV, from spawning benchy to the answer to
  `describe`: 351 → 232 ms (minimum of 21), 372 → 253 ms (median). From `bench/ingest.py`
  (minimum of 8, two rounds of 4 interleaved): csv-1k 479 → 333 ms, exec-1k 349 → 223 ms,
  host-1k 357 → 226 ms, ch-1k 343 → 213 ms, http-csv-1k 479 → 331 ms, http-json-1k
  336 → 204 ms; instructions within the spread. At 1M rows the cold start is the load's, and
  the runs were too noisy to see 100 ms in.

### 7. Starting processes — about 21 M instructions and 10 ms each; measured, nothing taken (the warden's taken in round 3, R3-11)

Every process benchy started — server, warden, engine, one loader per source — spent about
21 M instructions before `main`, 95% of it in the dynamic linker resolving `libduckdb.so`'s
symbols (`do_lookup_x`, `_dl_relocate_object`; 45,250 relative and 27,192 symbol relocations,
`LD_DEBUG=statistics`). `benchy-mcp --help` costs 21.6 M instructions; `true` costs 0.13 M.

- **Static linking is not possible with the release download.** Its `libduckdb_static.a`
  (75 MB) is DuckDB's core alone: it holds none of the extensions the shared library carries —
  no `icu_duckdb_cpp_init`, no Parquet reader, no JSON, no `core_functions` — and a program
  linked against it fails on an undefined `duckdb::ExtensionHelper::LoadAllExtensions`
  (1.5.5, aarch64). Without ICU a TIMESTAMPTZ column loses `date_trunc`, `time_bucket` and
  interval arithmetic, which is why the `bundled` feature was rejected (CONTRIBUTING.md); a
  static link would need DuckDB built from source with its extensions, the build `bundled`
  cannot do. The libduckdb-sys build script would take `DUCKDB_STATIC=1` with
  `DUCKDB_DOWNLOAD_LIB`; it links `duckdb_static` from the same download, and fails the same way.
- **Lazy binding does not help.** A C program linked to `libduckdb.so` costs 20.94 M
  instructions to start with `-z now` and 20.94 M with `-z lazy`: the cost is the library's own
  ~26,700 symbol relocations — C++ vtables and typeinfo with default visibility — which the
  loader resolves eagerly either way. Only DuckDB built with hidden visibility would remove them.
- **The warden by `fork` instead of `exec`** would save its 21 M (about 4% of a 1k-row session),
  but a forked warden holds a copy of the server's memory — its environment, the parsed config,
  any credential in either — where it held nothing (CONTRIBUTING.md: "the warden: nothing").
  Not taken in round 1. **Taken in round 3 (R3-11):** the warden is now forked, not exec'd,
  right after the session directory exists, before the config is read or any thread or DuckDB
  exists, and zeroes its environment, so it still holds nothing of the server's
  (`the_warden_holds_nothing_of_the_servers`); −18 to −26 M a session. The engine and the loaders
  are still exec'd, 21 M each; a clean "zygote" that forks them is R4-5's idea.

### 8. Smaller items

- **Done: `kill_with_connectors`** (`src/supervisor.rs`) read `/proc/<pid>/stat` for every
  process on the machine (about 400 here) after every loader and engine stop, to find connector
  groups. Only an exec source's loader starts connectors, so only that one is looked through now;
  the engine and file, Arrow and built-in loaders are killed without it
  (`a_load_that_times_out_leaves_no_connector_and_no_scratch` fails if an exec loader is
  skipped). The server's CPU per session, minimum of 8: csv-1k 24.2 → 18.8 ms, ch-1k
  24.2 → 18.5 ms, http-json-1k 24.8 → 19.9 ms, ch-1m 25.7 → 19.8 ms; an exec session still
  scans once (25.6 → 24.8 ms). Tracking the connector's
  process group directly needs `/proc/<pid>/task/*/children` (`CONFIG_PROC_CHILDREN`). That was
  written down as missing here; 6.8.0-138-generic has it, and the loader memory watch uses it
  (R6-L1). Using it for this kill too is not done yet.
- **Explained: the host server's 82 M instructions** against 25 M elsewhere were its own
  in-memory DuckDB, opened to prove table names (`host::check_name`): DuckDB's whole start, about
  50 M instructions and 22 ms of CPU. It is still needed; it now opens on a thread of its own while
  the engine opens (`host.prover` in the trace: 22 ms of CPU, off the path to `hello`).
- **Done: the host attach checks the session's budget before the rows go in**
  (`loader.rs`, `arrow`). The bytes are known before a batch is read; the rows are counted as
  each batch arrives, and the stream stops at the batch that passes what is left
  (`a_host_attach_stops_at_the_budget_not_after_reading_it_all`). A refused attach used to pay
  for the whole insert first.
- **`Bench::open_in`** costs 50–60 M instructions per loader and `bench.finish` 12–24 M:
  DuckDB's own open and checkpoint. Nothing in either is benchy's: `open_in` is `duckdb_open`
  and three `SET`s, `finish` is `duckdb_close`.
- **The exec path's own code** — the pipe, Arrow decode and its checks — is 0.36 G per 10M rows,
  1.3% of the load. Not worth touching before the items above.

## Build profile

Measured on `a8ffc3e`, before streaming ingest, with the same harness (`Cargo.toml` keeps the
numbers beside `[profile.release]`). Not adopted:

| option | what it changed | cost |
|---|---|---|
| `lto = "fat"`, `codegen-units = 1` | ClickHouse fetch −2% instructions; process start −0.6%; binary 14.2 → 10.8 MB; whole sessions −2.5% to +5.4%, inside the spread | build 4 → 26 min, 2.6 GB peak memory |
| the same with `-C target-cpu=native` (Neoverse-N1) | ClickHouse fetch −5% instructions, −10% CPU; sessions −2% to +6%, inside the spread | the binary needs this CPU's features; 41 min to build |
| `panic = "abort"` | not built | `src/ipc.rs` catches arrow's panic on a hostile schema to refuse the stream (`a_hostile_schema_is_refused_not_a_panic`); aborting would crash the loader instead |

More than 85% of a load runs in DuckDB's prebuilt library, which no Rust flag reaches. Revisit LTO
if benchy's own share grows — with the zstd decoder replaced (item 3), it has shrunk instead.

## Tools

This file was made with `perf` (`perf stat`, `perf record`). The harness needs
`kernel.perf_event_paranoid` ≤ 1 for counters, and falls back to `getrusage` CPU time without
them. On aarch64 there is no LBR, and neither benchy (stripped, no frame pointers) nor DuckDB's
release library unwinds cheaply, so the profiles here are flat (self) profiles split by the
trace's stages rather than call graphs. For call graphs: `valgrind --tool=callgrind` on one
loader, or a build with `-C force-frame-pointers=yes` and `perf record --call-graph=fp` (DuckDB's
frames will still be missing).
