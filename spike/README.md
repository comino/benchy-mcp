# M0 spike

Throwaway probes that answer requirement-blocking questions. **Not product code.**

```sh
cd spike
export DUCKDB_LIB_DIR=$PWD/vendor DUCKDB_INCLUDE_DIR=$PWD/vendor
cargo build --release
LD_LIBRARY_PATH=$PWD/vendor ./target/release/probe1_stmt_types
LD_LIBRARY_PATH=$PWD/vendor ./target/release/probe2_interrupt
LD_LIBRARY_PATH=$PWD/vendor ./target/release/probe3_arrow_stream
```

Probes 1–4 and 7 need `LD_LIBRARY_PATH`; probe 7 also needs the eval's meter CSV
(`probe7_http_decode gen eval/uci/readings.csv <dir>` writes its fixtures); probe 6 has no DuckDB dependency. Binaries are not
committed — a repository people clone should not carry build artifacts. Each probe's finding is
in the table below, and the source here is what reproduces it.

`vendor/` holds the official prebuilt `libduckdb-linux-arm64` 1.5.5 and is git-ignored — fetch it
with `gh release download v1.5.5 --repo duckdb/duckdb --pattern 'libduckdb-linux-arm64.zip'`.
The main build links the same library, fetched by `cargo build` itself.

| Probe | Question | Answer |
|---|---|---|
| 1 | Can we get per-statement types for REQ-2's allowlist? | Yes, including inside multi-statement batches |
| 2 | Does interrupt save the bench from a runaway? | Yes in execution; **no in planning** — hence REQ-8's second layer |
| 3 | Does Arrow ingest stream without a whole-stream copy? | Yes — 2.45 M rows/s, 1.31× peak |
| 4 | Does either REQ-2 mechanism stop the escape corpus alone? | **No** — the allowlist leaks 5 of 40; only the locked engine holds |
| 6 | Does Landlock actually hold, and do open fds survive it? | Yes — V1–V4 fully enforced; open fds survive, everything else denied |
| 7 | Which decoder is fastest for each format an HTTP source sends? | DuckDB's readers for Parquet, CSV and NDJSON (CSV and NDJSON from a pipe at no extra cost); arrow-json for JSON pages. Numbers in docs/HTTP-SOURCE.md |
| 8 | Which decoder for a compressed HTTP body? (`decompress/`, a package of its own) | C libzstd: 1.64 G instructions for a 10M-row ClickHouse zstd response against ruzstd's 5.99 G; gzip on flate2 + zlib-rs, 20% less CPU than miniz_oxide. Numbers in docs/history/PERFORMANCE-rounds.md (round 1 item 3) |

Probe 8 is `decompress/`, a package of its own with each decoder behind a feature, so each builds
alone: `cd decompress && cargo build --release --features czstd` (or `ruzstd`, `lz4`, `gz-miniz`,
`gz-zlibrs`, `arrow`), then `probe8_decompress <codec> <body>` on a response saved with `curl`.

Probe 5 checked a Postgres wire encoding for a connector tier benchy no longer has, and is gone.
