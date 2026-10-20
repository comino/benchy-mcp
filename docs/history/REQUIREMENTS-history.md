# Requirements: history

Narrative moved out of [REQUIREMENTS.md](../../REQUIREMENTS.md) so that it states what holds and
the tests that prove it. The history of each test (how a gap was found, what it did before, the
measurement of the bug) is in that test's doc comment; what is here is the prose that belonged
to no single test, kept verbatim. References to docs/PERFORMANCE.md name its archive,
[PERFORMANCE-rounds.md](PERFORMANCE-rounds.md).

## REQ-9 — Scratch is free; loaded data is not

From the paragraph on taken names, the cases that showed an agent object standing in for a host
table or a built-in, measured before names were checked:

`CREATE TEMP TABLE readings` hid the attached `readings`, `CREATE TEMP MACRO lower` replaced
`lower`, and a macro named `duckdb_columns` answered the host's next attach with columns the agent
made up.

## REQ-14 — Describe tells the shape of the data

The profile paragraph as it stood before the history split, with its mechanism and
measurements in full (R2-2, R2-4, R3-2 and R4-2 name sections of the archived performance
rounds):

The profile is computed **once, by the loader**, and kept in the table's comment, so `describe`
only renders text and a rebuilt engine does not profile again. It is built from one scan of
aggregates, then one pass over the rows that DuckDB streams to the loader, which adds up each
series — its count, extent, phase and two checksums of its timestamps, and whether its times came
in order, rising or falling, and its runs of one value — each time, and the rows that are 0 or
null. The checksums alone
prove nothing: three gaps hidden by three repeats can leave them as they were. A series that fits
them is proven regular, without ordering it, only when its times came in order, so none repeats;
one that fits but came in no order is settled by an exact count of its distinct times, and
bounded windows run only over what failed. Measured
on a 4-core ARM box under heavy load from other work, in CPU time (user and system) and
instructions, median of four alternating runs: 0.22 s and 1.1 billion instructions on the
840,960-row meter data, whose load took 0.84 s and 5.0 billion; 0.74 s and 4.0 billion on five
million rows of `examples/capacity.py`'s shape, whose load took 6.0 s and 34 billion.
`describe` itself takes about 1 ms either way.
A stream is not read back for it: the scan's aggregates, DuckDB's own distinct counts and the
pass are kept from its batches as they go into the table, on a thread beside the insert, and only
the bounded queries (the step, gaps, and the runs of a table of many series) read the table. On
10,000,000 rows that took 12.5–15% off the whole load's instructions, and reading the batches in
place another 9–11% (PERFORMANCE-rounds.md, R2-2). The distinct counts are kept only when the first
batch's times repeat: they choose the key only then, and if the rest of the stream repeats its
times after all, they are read from the table. A file, and an HTTP body DuckDB reads itself, is
kept the same way from one scan as Arrow, which replaces the scan and the pass that each read
the table back: 6–13% fewer instructions for 1,000,000 rows (R2-4). That scan reads what the
table is written from — a staged file's stage, or a Parquet file a second time — on a thread
beside the write, so it is off the path to the first `describe`: 8% sooner for 5,000,000 rows
of a four-column CSV (R3-2). Every CSV and JSON file is staged (R4-2).

## REQ-15 — Origin-blind

Why the write stays on one thread, as the requirement recorded it in full:

The layout includes where each segment lies: `pragma_storage_info` gives the agent every
segment's `block_id` and `block_offset`, and those are the same too. That is why the write stays
on one thread, though it is most of the path to the first `describe` at 10,000,000 rows
(PERFORMANCE-rounds.md, R4-1). Measured with DuckDB 1.5.5, a write on four threads, split into
fixed groups of row groups each committed in order, makes the same row groups, segments and
compression from every source; but DuckDB numbers blocks in the order its threads write them,
and the blocks segments lie in changed from one load to the next (4 placements in 10 loads of
one table). Through DuckDB's own parallel insert the partial blocks are also packed by which
thread wrote which row group, and the block count changed too (24, 25 or 26 blocks in 30 loads).
The one arrangement found that keeps the blocks — a transaction per row group, compressed as it
commits, in order — compresses on one thread at a time again: 10,000,000 rows took 3.9 s where
one thread takes 2.9 s, each commit a sync of the file (2.4 s against 2.9 s with the file in
memory).
