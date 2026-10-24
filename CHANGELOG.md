# Changelog

What changed in benchy-mcp. The version is the one in `Cargo.toml`, which is what
`benchy-mcp --version` reports. This is a work in progress prototype under heavy development:
frequent breaking changes are expected, with no stable API or backward-compatibility guarantee.

## Unreleased

- **Security: a macro of the agent's could blind the check that keeps built-in names taken.**
  benchy's own SQL, run on connections that see the agent's objects, called some functions
  unqualified, and DuckDB 1.5.5 resolves an unqualified call, an operator such as `||`, LIKE or
  `count(*)` included, to the agent's macro of that name first. The check run after every
  `CREATE` listed the agent's macros with `bool_or(...)`, so `CREATE MACRO bool_or(x) AS
  unnest([]::BOOLEAN[])` made the listing empty: that macro was kept, and every built-in's name
  was free after it. A macro `abs` then answered for DuckDB's `abs` in every query, an operator's
  helper that calls `abs` included, and macros under `~~`, `regexp_matches`, `starts_with` or
  `count_star` blinded the ENUM checks, the dropping of a dynamic PIVOT's type and the attach
  check's counts. The listing was unqualified since 0.1.0, and the attack reproduced on 0.3.1. Since the built-in names were
  read in the same scan (#63), an engine's first `CREATE` could also keep the empty listing as
  DuckDB's names for the engine's life, even after the macro was dropped. Every function benchy's
  own SQL calls is now named through `system.main` (`system.main.concat` in place of `||`,
  `system.main.contains` in place of LIKE), held by a test that reads each query with DuckDB's own
  parser, and a listing of DuckDB's names that lacks one of those functions refuses the statement
  and is not kept (`a_macro_of_the_agents_cannot_blind_the_check_that_keeps_a_helpers_builtins`,
  `benchys_internal_sql_names_every_function_by_its_catalog`).
- **Dependencies:** tokio 1.53.3 (RUSTSEC-2026-0386: `AsyncFd` constructors were unsound), toml 1.1
  (TOML 1.1; the config's rules are unchanged: unknown fields are still errors), libc 0.2.190,
  serde_json 1.0.154.
- **The load timeout covers handing a loader its job** (#61). The job was written to the
  loader's stdin before the timeout and the memory watch started, so a job larger than the pipe
  holds (64 KiB on Linux), such as an attach stating many series columns, to a loader that did
  not read it left startup or the attach waiting for as long as the loader stayed so: no reply
  after 30 s past a 2 s load timeout. The job is now written on the thread that reads the
  loader's answer, and the load fails `E_DEADLINE` at the timeout, with the loader killed and
  nothing attached (`a_job_the_loader_does_not_read_is_held_to_the_load_timeout`).
- **A helper macro that calls a function nothing defines stops startup** (#62). DuckDB 1.5.5
  checks only part of a macro's body when it is made: a call in a SELECT with a FROM, a table
  macro's included, or in a lambda was made with no such function anywhere, and once the agent
  made a macro of that name, the operator's helper called the agent's code. A helper macro now
  calls only DuckDB's functions and other helpers, by name alone (not `s.f(x)` or `x.f()`, which
  DuckDB binds to a schema the agent may make), read from the function listing the helpers job
  already makes; the refusal names the helper and what it calls. Helper views were not affected:
  DuckDB binds one when it is made, in the helpers job, and a waiting one when its table arrives.
- **An unknown column or table is told the columns** (#27). `SELECT temp FROM readings` was told
  the bench's table names and to run `describe` for their columns; it is now told the columns of
  the tables the statement reads, `readings(series_id, t, n)`, and an unknown table the columns
  of what is here. A qualified reference, `SELECT r.temp FROM readings r`, had no hint at all; it
  is now told that `r` is `readings`, with its columns, and so is a column of a view, a CTE or a
  subquery. Helper views and the agent's own tables are listed like loaded ones. The list is
  held to 1,024 bytes: a table cut short ends `… [+N columns])`, past 12 tables `(+N more)`, and
  the hint then names `DESCRIBE <table>`. These hints name no tool, so a host whose agent has no
  `describe` is not sent to one; there is no protocol change. The columns come from what the
  attach kept, or a `SELECT *` prepared and not run, and a failing query costs what it did on a
  loaded table (docs/PERFORMANCE.md).
- **A host's error replies carry `detail`, facts beside the model's message** (#26). A host
  no longer parses prose: `E_REBUILT` says why the engine went (`reason`: `hard_deadline`,
  `memory_ceiling`, `engine_died` or `pivot_type`) and what went with it (`lost`: `results`,
  `agent_tables`); `E_DEADLINE` says `deadline` or `load_timeout`, with nothing lost; a batch
  that fails at a statement gives that statement and the earlier results' `result_ids` and
  `results`, in a query reply's shape; `E_LIMIT` names the limit as `hello` does and the value it
  held the table to; `E_VERSION` lists the versions benchy speaks. docs/HOST-PROTOCOL.md has
  the table of which code carries what. Additive: every field and message is as before, and no
  new protocol version, since replies only grow. The model's message after a rebuild now says
  the earlier results are gone too, and past the memory ceiling no longer tells the agent to ask
  the operator to start benchy with a larger `--memory-ceiling`, which it cannot do: it is told to
  use less, or to say that the work needs more.
- **A helper comment `describe` cuts is told to the operator** (#60). `describe` cuts a helper's
  comment at 200 bytes and its whole line (signature, kind and comment) at 320, and marked the cut
  only for the agent, which never sees what was cut. Startup now says on stderr, beside
  `benchy: N helper(s): …`, which helper is cut and by how many bytes, in `benchy-mcp <dir>` and
  `benchy-mcp host` alike, and a waiting view whose line passes 320 once its columns are known is
  told when the attach makes it (`a_cut_helper_comment_or_line_is_told_to_the_operator`). Told,
  not refused: a long comment reaches nothing it should not, and a deployment that started
  before still starts. No reply carries it. The README's "SQL helpers" now gives both caps; a
  signature of up to 82 bytes keeps a whole comment.
- **DuckDB's built-in function names are read once per engine, in the scan the shadowing check
  makes anyway** (#63). The first `CREATE MACRO` of an engine listed `duckdb_functions()` twice,
  once for DuckDB's names and once for the agent's, and each listing builds every built-in's row:
  it now costs 185 M instructions rather than 282 M (`benchy-mcp host`, medians of seven). Every
  later `CREATE MACRO` costs what it did, about 137 M, since learning the name the agent made
  still takes that one scan, and a session's helpers cost what they did at startup: the engine's
  listing as it attaches their file checks the file, not DuckDB's names. The check is as strict
  as before; docs/PERFORMANCE.md has the numbers.
- **Tests that guard what REQUIREMENTS.md cites** (#22). REQ-10's retention limits (2,000 rows,
  1 MiB, the last 32 results) and the 512-byte `max_bytes` floor each have a test that fails if
  the limit moves either way. REQ-10 now says that the row reaching 1 MiB is kept. The retention
  rule moved into `src/result.rs`, which the weekly mutation tests now examine. The meter-data
  quirks test runs on seven committed meters of the eval's public data instead of only when it
  was fetched; `hello`'s `benchy` and `sandbox` are checked for their values; and
  `scripts/check_requirements.py` fails on a cited test that is ignored, or built only on
  Linux, unless that is said.

## [0.3.1] — 2026-10-21

Two TLS advisories fixed in the HTTP loader's dependencies, and the weekly cargo-mutants gate green
again. The only other behaviour change: the guard's text scan refuses a batch if it ever stops
moving on, which no input makes it do.

- **TLS for the HTTP loader takes the fixes for RUSTSEC-2026-0381 and RUSTSEC-2026-0382**:
  `rustls-webpki` 0.103.16 and `webpki-roots` 1.0.10, so certificate checks no longer accept the
  distrusted intermediates those advisories name.
- **The guard's text scan fails closed if it stops moving on** (#64). A step of the scan that
  reads a batch for refused table functions now has to move past at least one byte; one that
  does not refuses the batch, as an unparseable one is, instead of looping. No input makes it
  stop: it is there so that a slip in the scan's arithmetic is a refusal a test sees at once, not
  a hang. The weekly cargo-mutants run on 0.3.0 had 22 such hangs, and five mutants no test told
  apart; each now has a test that fails on it: a host asking for exactly a limit it has, a memory
  ceiling at DuckDB's own budget, every statement type named on its own (which retires the
  exclusion of the names no SQL reaches), a helpers job's `ok`, the reason a refused channel
  stream lost its place, and the tables of one attach sharing what the budget has left.

## [0.3.0] — 2026-10-20

Hardening of the SQL boundary, a `describe` that is right about mixed and series-sorted data,
opt-in engine threads, and release metadata. Still an experimental prototype: **some of these
change what a session refuses or what an operator's config may hold**, so read "Changed
behaviour" before upgrading. The host protocol is now 7 (`limits.threads`); 1–6 still work.

Most important:

- **A table function that changes the engine is refused** (#49). `SELECT * FROM
  enable_logging(storage='file', …)` broke the session for good; every table function DuckDB
  ships is now classified, and `query()` and `json_execute_serialized_sql` are refused
  everywhere.
- **A helper macro may not read a table** (#58). Write it as a view; the CTE checks it needed are
  gone, and with them the false refusal of `query (` inside a string (#35) and a check that let
  a batch through unread (#36).
- **A `SELECT` with no `LIMIT` reads only the rows it keeps** (#53): a `SELECT *` over 10M rows
  costs 23 M instructions instead of 0.63 G, and no longer puts the engine past its ceiling.
- **`describe` judges each series at its own step** (#50), says alike notes once, shows every
  table's shape first, notes a value held too long (#51), and reads times to the microsecond
  (#15).
- **`--threads N`** runs agent SQL on more threads, started after the sandbox confines the
  engine (#56).
- **Each release has `compat.json`, `--version --json` and per-bundle SBOMs** (#57), and a host
  can read the timeouts and `--print-limits` (#29).

Changed behaviour:

- **The agent is told times are UTC** (#52). The session is pinned to UTC, and neither the
  instructions nor a refused `SET TimeZone` said so. Both now say "Times are UTC. For local
  calendar periods, convert first, e.g. `date_trunc('day', timezone('Europe/Berlin', ts))`.",
  and a `SET` or `RESET` of `TimeZone` gets that instead of the statements allowed
  (`the_agent_is_told_times_are_utc_and_how_to_bucket_local_days`). The instructions are 115
  bytes longer. `dialect` recommends `time_bucket` for buckets below a day only: on a
  TIMESTAMPTZ column, one engine thread, `time_bucket` of a day took 10.05 s of CPU over
  1,000,000 rows where `date_trunc('day', …)` took 0.30 s, and a month 20.7 s against 0.31 s;
  by the hour `time_bucket` is the faster, 0.10 s against 0.29 s.
- **`describe` judges each series at its own step** (#50). On a table of series every 5, 10
  and 15 minutes, every complete 10- and 15-minute series was read at the table's 5 minutes and
  reported broken ("1014: 8735 gaps, 17470 rows missing"). A series is now judged at the step
  its times keep — the common divisor of their distances, or, where it breaks that, the distance
  at least half its rows keep — and its gaps, runs of 0 and late start or early end at that step.
  The time column's line says how many series keep each step (`series have a row every 5 minutes
  (52 series), 10 minutes (49) or 15 minutes (48)`), the line on regular series how many of each
  are regular, and a series off the most common step says its own. One cadence reads as before.
  (`each_series_is_judged_at_its_own_step`)
- **Alike notes are said once** (#50): series whose notes are the same are one line, by their
  keys, an integer key's consecutive values as a range (`1090–1099 (10 series): kw is 0 in every
  row`). And every table's own lines — time column, key, cadence, columns — get the `describe`
  budget before any table's notes about single series, so a small table beside one with many
  notes keeps its shape (`every_tables_shape_comes_before_any_tables_notes`).
- **A value held too long is noted** (#51): `'r1': temp is 21.5 from … to … (21 days)`, for a
  run of one value, not 0 or null, lasting a day or more by the same rule as a run of 0 (a gap
  breaks it) and holding ten times as many rows as the series' runs of one value do on average,
  so a status code or a setpoint that holds each value for days is not noted. Only tables with a
  series key are examined. No configuration. The loader's pass counts the runs as it goes, and
  reads a series again only when its longest run could be one to note
  (`a_stuck_value_inside_a_series_is_noted`, `the_pass_misses_no_value_held`). The line that
  lists what is noted is 58 bytes longer.
- **An NDJSON/JSON file with `epoch` in a timestamp or date column is refused** (#37), as one
  with `infinity` has been since 0.2.0. DuckDB's sniffed JSON read took `epoch` as
  1900-01-01 00:00:00, where a CSV and a declared JSON column read it as 1970-01-01. Declare the
  column's type in `[[source.schema]]` to load it as 1970
  (`an_ndjson_epoch_is_refused_where_a_csv_reads_1970`).

- **A helper macro may not read a table** (#58). DuckDB expands a macro inside the caller's
  query, where a CTE of the caller's named like a table the macro reads stands in for it; 0.2.0
  guarded that by reading every batch for such a CTE. Now a helper macro whose body reads a
  table, a helper view, or a table function other than `range`, `generate_series` and `unnest`,
  or calls a helper macro that does, stops startup with a message naming it
  (`a_helper_macro_that_reads_a_table_is_refused_at_startup`). A macro over a table not attached
  yet no longer waits; it is refused the same way. Views and macros that compute from their
  arguments are unchanged. **To migrate**, write a table-reading macro as a view and let the
  agent filter it: `CREATE MACRO hours_above(id, kw) AS TABLE … FROM meters m WHERE m.meter =
  id …` becomes `CREATE VIEW reading_spans AS SELECT meter, observed_at, power_kw, lead(…) … FROM
  meters`, and the agent writes the `WHERE meter = 'lab'` and the sum. `examples/helpers.sql`
  does so.
- With that, the agent's SQL is no longer checked against the helpers: a CTE named like a table
  a helper reads is never refused. This fixes the refusal of `query (` inside a string literal
  (#35, `a_string_that_looks_like_sql_runs_beside_helpers`) and removes the check that let a
  batch through unread when its parser could not be prepared (#36). `query()` and
  `json_execute_serialized_sql` are refused everywhere now, helpers or not (#49, below).
- **A `SELECT` with no `LIMIT` no longer builds its whole result.** It is streamed and read only
  as far as the 2,000 rows that are kept, so a `SELECT *` over 10,000,000 rows costs 23 M
  instructions instead of 0.63 G, and the engine's peak memory is 51–60 MiB instead of
  402–424 MiB (45 MiB idle). A wider result, or a smaller `--memory`, had put the engine past
  its ceiling, which cost the agent its tables (#53). The reply still says `of 2000+`. Every
  other kind of statement runs to completion. Changed behaviour: an error DuckDB would raise
  only in rows past the ones read, such as a cast that fails at row 5,000,000, is no longer
  raised; the answer is the rows kept. An error in the rows read still fails the query.
- **A `SELECT` over a table function that changes the engine is refused** (#49).
  `SELECT * FROM enable_logging(storage='file', …)` left every later query of the session
  failing, `enable_profiling()` wrote every plan tree to the operator's log, and
  `checkpoint()`, `force_checkpoint()` and `truncate_duckdb_logs()` ran. The statement
  allowlist now has a second half, an allowlist of DuckDB's table functions checked on the
  batch's text before any of it is prepared: each of DuckDB 1.5.5's 93 is the agent's, left to
  the engine lock (the file and network readers), or refused, and a test fails when a DuckDB
  brings one that is not classified. On a DuckDB other than the one measured, any table
  function not classified is refused. A string literal's text is not read, so a refused name
  inside one (`SELECT 'checkpoint()'`) runs, as #35 has it.

  `query('…')` and `json_execute_serialized_sql` are refused with or
  without helper macros, since the table functions in their string cannot be read before it
  runs; `query_table('name')` still works. The engine's own workings (`duckdb_logs`,
  `duckdb_memory`, `duckdb_extensions`, `duckdb_secrets` and the like) and DuckDB's test
  functions are refused too.

Added:

- **A release has an SBOM and a compat.json** (#57). Each bundle has a CycloneDX SBOM,
  `benchy-mcp-<platform>.cdx.json`, attested of the bundle: every crate the release builds for
  the platform, with its Cargo.lock checksum and whether it is in the binary, and DuckDB's library
  with the SHA-256 of the file shipped and of the pinned release file it came from
  (`packaging/sbom.py`). `compat.json` says what a pin bump changes — `benchy`, `duckdb`,
  `host_protocols`, `platforms`, `full_sandbox_min_landlock_abi` and `min_glibc`, the last read
  from the Linux bundles' ELF version needs — and `benchy-mcp --version --json` prints the same,
  less `min_glibc`, from the binary (`version_json_says_what_a_pin_bump_checks`). Both are in
  `SHA256SUMS`; each smoke test holds its platform's binary to compat.json
  (docs/HOST-PROTOCOL.md, Versions).
- **A host can read the timeouts and the limits of a flag set** (#29). `hello` has
  `timeouts: {load_timeout_secs, hard_grace_secs}` beside the caps
  (`hello_says_how_long_a_load_and_the_hard_grace_may_take`), and `benchy-mcp host [flags]
  --print-limits` prints what `hello` would report to a session agreeing on the newest protocol,
  and starts nothing (`print_limits_says_what_hello_would`).

Faster:

- **The engine can run agent SQL on more than one thread** (#56). Use `--threads N` or
  `[engine] threads = N`; the default is still 1, so on a shared machine a runaway takes one
  core. A host may lower the operator's number in `hello`, from host protocol 7
  (`limits.threads`), and never raise it. An engine already open on more threads is then
  rebuilt on the host's before its first call. On four cores, a per-meter hourly mean over
  10,000,000 rows took 0.77 → 0.25 s, and a per-meter percentile 0.65 → 0.21 s. That cost
  12–19% more instructions and about 70 MiB more peak memory. The engine is sealed in a new
  order: everything but the lock, on one thread, then the confinement, then `SET threads = N`
  and `lock_configuration`. DuckDB starts the workers at that `SET`, from a confined thread,
  so they are confined too. A test puts a C function on the bench to prove it from each
  worker. The agent still cannot change the threads: the lock refuses `SET threads`, and the
  seal proves that before it serves. Host protocol 7 adds the field; a host on 6 sees nothing
  new.
  (`the_engines_threads_are_started_after_it_is_confined`,
  `a_host_lowers_the_engines_threads_and_cannot_raise_them`)
- **A table sorted by series, then time, is no longer profiled twice** (#54). Its first batches
  look like one series whose times do not repeat, and the watch chose the one series there, so
  the pass and the key's distinct counts were read from the table again after the write, on
  one thread. Now the keyed tallies wait, beside the one series, for the first candidate value
  that changes, up to 1,000,000 rows, and a series that begins back in time is taken for a cut
  into series. 10,000,000 rows of 500 meters from Parquet in that order: 27.46 → 23.16 G
  instructions, the profile's CPU 1,189 → 112 ms, the first `describe` 3.5 → 2.6 s; in time
  order nothing changes. A table of one series beside a constant text column pays +1.3% for
  the wait. `bench/ingest.py` gains `parquet_bytime` and `parquet_bymeter`, the same table in
  either order.

Fixed:

- **A page cuts values at its own cell cap** (#18). A host's `page` may lower `max_cell_bytes`
  for the call, and that cap was applied to the column names alone: `SELECT repeat('x', 200)`
  paged at 16 came back whole, without TRUNCATED. Each value is now cut to the page's cap, and
  one cut already when it was kept says how much of the value is missing
  (`a_page_cuts_values_at_its_own_cell_cap`).
- **`page`'s `sql` keeps every string literal whole** (#16). Comments were stripped by a scan
  that knew `'…'` and `"…"` alone, so `SELECT $$a -- b$$ AS y, 'q' AS z` came back as
  `SELECT $$a`, and `SELECT $$a/*b*/c$$ AS x` as `SELECT $$a c$$ AS x`. The batch is now split
  and its comments found by the lexer the table-function check uses, held to DuckDB 1.5.5's:
  `'…'`, `E'…'`, `$tag$…$tag$`, quoted names, `--` and nested `/* */`. A statement with a `;`
  in a dollar-quoted string, which was given no `sql`, is given its own
  (`page_sql_keeps_every_literal_whole`).
- **The trace's peak memory is in KiB on macOS too** (#55). `maxrss_kb` and
  `children_maxrss_kb` were `ru_maxrss` as getrusage(2) gives it, which is KiB on Linux and
  bytes on macOS, where a process at about 513 MB was traced as `546504704`; `bench/ingest.py`
  read it as KiB. It is now converted there, and the name stays
  (`peak_memory_is_said_in_kib_on_every_platform`).
- **A failed `hello` or `ping` reply ends the session** (#17). The reader answers these itself
  and ignored a failed write, so a host that had closed its end of the replies kept its
  session, engine and warden running for as long as its stdin stayed open. A second `hello`'s
  refusal counts too (`a_reply_the_reader_cannot_write_ends_the_session`).
- **The hard deadline covers writing a request to the engine** (#10). A request larger than the
  engine's stdin pipe holds (64 KiB on Linux) was written before the deadline started, so an
  engine that did not read it, stopped or wedged, left a 512 KiB query without a reply after
  90 s past a 1 s deadline. The request is now written on the thread that reads the reply, and
  the engine is killed and rebuilt at the hard deadline as for any other request
  (`a_request_the_engine_does_not_read_is_held_to_the_hard_deadline`).
- **A request mixing channel and descriptor tables no longer stalls on one CPU** (#11). With one
  loader at a time (a pod with `cpu: 1`), every channel table ran before the descriptor tables
  after the first, so `[channel a, descriptor b, channel c]`, written in that order, waited for
  `c` while the writer was blocked on `b`'s full pipe, until the load timeout (300 s by
  default) failed `c` and closed the channel. The tables now load in the order of the list
  (`a_channel_and_a_descriptor_load_in_request_order_on_one_cpu`).
- `a_descriptor_is_taken_only_by_a_streamed_attach` no longer fails now and then (#48). The
  server closed the refused request's descriptor as it should. The read end that stayed open
  was a copy in a process another test of the same binary was starting, not yet exec'd. The
  check now runs while no test of that binary starts a process.
- **`describe` reads times to the microsecond** (#15). The profile read them as milliseconds:
  rows 500 µs apart were "a row every 1 millisecond", half of them "a time already present",
  and the extent ended at `.009` where the last row was at `.0095`. Now they are `a row every
  500 microseconds`, a duration under a millisecond is said in microseconds, and a far time
  such as 9999-12-31 23:59:59.999999 is said exactly
  (`times_finer_than_a_millisecond_are_profiled_as_they_are`). No measurable cost
  (docs/PERFORMANCE.md).
- **What an Arrow stream decodes to is one total against its byte bound** (#14). Its bodies,
  decompressed, and what its dictionary and view columns unpack to were each held to
  `attach_bytes` (or a connector's `max_bytes`) on its own, so a stream with some of each loaded
  at up to twice the bound: 818 KB of plain text and 804 KB unpacked attached under 1 MiB. Now
  they are added up, and such a stream is refused as `E_LIMIT`
  (`an_attachments_plain_and_unpacked_bytes_share_one_bound`).
- **An NDJSON infinity in a column renamed for case is refused** (#38). Of two keys that differ
  only in case, DuckDB renames one (`Id_1`), and the check that refuses an infinity read as
  1900-01-01 looked for a key of that name, found none, and let the file load
  (`an_ndjson_infinity_is_refused_in_a_column_renamed_for_case`). JSON pages, decoded by
  arrow-json, already refused an infinity or `epoch`, and now have a test that says so
  (`a_json_pages_infinity_or_epoch_is_refused_not_read_as_1900`).

## [0.2.0] — 2026-10-17

Fixes from a review of 0.1.0. Still an experimental prototype: **some of these change what a
session says or refuses**, so read "Changed behaviour" before upgrading a pinned integration.

Most important:

- **An empty result could cost the agent its tables.** The note that explains an empty result
  read whole text columns after the deadline; on a large table the engine was killed. It is now
  bounded by what is left of the deadline (#2).
- **`describe` no longer states false facts:** a measure taken for the series key (#3), "none
  missing or repeated" over hidden gaps (#8), counts capped at 200,001 shown as exact (#5), and
  past 200,000 series a cut time range and "every series" (#6).
- **`infinity` timestamps are kept**, not loaded as nulls, and render as `infinity` (#4).
- **The release's DuckDB library is checked** against a pinned SHA-256, and the Rust toolchain
  is pinned (#19, #21).
- **Below Linux 6.7 the network is not confined**, and benchy now says so: `hello` has a
  structured `sandbox_posture`, `doctor` says `partial`, and `--require-full-sandbox` refuses to
  start there (#25).

Changed behaviour:

- Files whose names start with `.` in a data directory are skipped (#7).
- An NDJSON/JSON file with `infinity` in a timestamp or date column is refused instead of loaded
  as 1900-01-01.
- `CREATE TABLE … AS PIVOT` and writes that read a PIVOT now run (#1).
- With helper macros: `query()` and `json_execute_serialized_sql` are refused, and a CTE named
  like a helper's table is refused only beside a helper call (#9, #31).
- `describe` text changed where it was wrong (series key, regularity, counts, large tables).
- `doctor --probe` can now report `partial` (exit status 0).

Known limits, unchanged from 0.1.0: no OS sandbox on native macOS; no Windows binary; connectors
run with their process's rights; no independent security audit; engine and loader ceilings do not
cap the parent server or its queued requests. Report a security problem privately: SECURITY.md.

### Details

- DuckDB's release library is checked against a pinned SHA-256 before it is linked or shipped.
  `packaging/duckdb_lib.py` fetches the release file for CI, the release builds and the
  Dockerfile, so libduckdb-sys never downloads it unchecked there; `package.sh` checks it again,
  pins DuckDB's LICENSE too, takes the exact DuckDB version from Cargo.lock, and writes a
  `SHA256SUMS` of every file into the bundle, whose README names the DuckDB release file and its
  SHA-256. Building from source: run `python3 packaging/duckdb_lib.py fetch` before `cargo build`.
- The Rust compiler is pinned to 1.98.1 in `rust-toolchain.toml`, used by local builds, CI, the
  release and the Dockerfile alike, with `rust-version` in Cargo.toml; `scripts/check_toolchain.py`
  keeps them in step.
- SECURITY.md (private vulnerability reporting), issue and pull request templates, Dependabot for
  crates and actions, and `cargo deny check` in CI. The macOS CI job also runs on pull requests.
  `packaging/container-smoke.sh` stages its fixtures under `$HOME` on macOS, where Colima does not
  share `/var/folders`.
- `describe` no longer takes an integer reading, such as CO₂ in ppm or an RSSI, for the series
  key when an integer device id is beside it: where two columns of a kind could be the key, the
  one whose values cut the first slice of time into series that keep a step wins.
- A series is called regular ("none missing or repeated") only when that is proven: its count,
  extent and checksums matched a series with three gaps hidden by three repeats. Times that
  came in order, rising or falling, prove it; otherwise an exact count of its distinct times does.
- Gaps, repeats and rows off the step are counted per series exactly. They were counted from a
  list cut at 200,001 rows, which understated them as if exact and left the series after the
  cut without notes.
- Past 200,000 series the time column's range is the table's, the key gets no range, and the
  line on regular series says which series were examined, never "every series".
- Keep `infinity` and `-infinity` timestamps on load. A zone-less or zoned timestamp from
  Parquet, a host's Arrow stream or a connector used to become a null, even in a column
  declared non-nullable. A nanosecond one used to be refused as finer than a microsecond.
  Results now show an infinite DATE, TIMESTAMP (any precision) or TIMESTAMPTZ as `infinity` or
  `-infinity`, both at the top level and in lists and structs. Before, a result showed the year
  294247 or an `ERROR: Cast error` cell and was still marked complete. A TIMESTAMPTZ inside a
  list or struct now shows its UTC instant.
- An empty result's note no longer reads whole every text column the query names. A column
  whose first values already hold too many for a note is not read further, and the note gets
  what is left of the batch's deadline, at most 0.5 s. On a large table of unique text it had
  carried a batch past the hard deadline: the engine was killed, the agent's own tables were
  lost, and the error blamed planning.
- `CREATE TABLE … AS PIVOT`, and an INSERT, UPDATE, DELETE or MERGE that reads a PIVOT, now
  run. DuckDB wraps each in a transaction of its own, which was refused as the agent's
  `TRANSACTION`. A transaction the agent writes is still refused, and a refused `TRANSACTION`
  now says what to do when the PIVOT reads a table made earlier in the same call.
- **A folder's dotfiles are skipped** (REQ-4 `a_hidden_file_in_a_directory_is_not_a_table`).
  `.secret.csv` became a table `secret`, and the AppleDouble `._meters.csv` macOS writes beside
  `meters.csv` stopped startup as a second `meters`. Every name starting with `.` is now skipped
  and named in the operator's log; a dotfile named with `--source` or in a config still loads
  (`a_hidden_file_named_explicitly_still_loads`).
- **Docs say what is true.** Only a macro is refused a built-in function's name: `CREATE TABLE
  range` works (DIALECT.md, the `query` tool's description). COPY and EXPORT are refused by the
  locked engine at prepare, not by the allowlist up front (`src/guard.rs`). HOST-PROTOCOL.md lists
  every `sandbox` value `hello` reports, and that Node.js may destroy a closed channel, or hand a
  write's error to its callback, before any `'error'` event. CAPACITY.md says which parts are
  `capacity.py`'s output. A helper's comment is its `--` lines joined into one, at most 200 bytes.
  The README's Limits measure how a long string's overshoot of `--memory` grows with its rows.
  Commit hashes and issue numbers from before the public history began no longer read as links
  into this repository.
- **SQL run from a string no longer gets past the helper CTE rule**
  (`sql_run_from_a_string_is_refused_beside_helper_macros`). From a host's report:
  `query('WITH meters AS (…) SELECT * FROM total_kw(''m0'')')` returned the CTE's value, built
  with `||` or `replace()` too, and a view over it kept doing so. DuckDB parses that string only
  when it runs, so no look beforehand can read it. With helper macros that read a table, a batch
  that may call `query` or `json_execute_serialized_sql` (which reached a helper by its catalog's
  name) is refused before anything runs, by any spelling. `query_table`, which reads only a
  table named in its string, stays, and so does `query()` in a session without helper macros.
  The escape corpus records both on the locked engine.
- **A CTE named like a helper's table is refused only beside a helper call**
  (`a_cte_named_like_a_helpers_table_is_refused_only_beside_a_helper_call`). From a host whose
  helpers read `hourly`: every `WITH hourly AS (…)` was refused, though only a helper macro
  expanded in the same query reads the CTE. Now the rule applies to a batch that names a helper
  macro, a macro of the agent's that calls one at any depth (measured: DuckDB 1.5.5 expands
  both in the caller's query), or `view`, `macro` or `function` (a view or macro keeps the CTE in
  its body, and reads it through any macro it calls once that one is replaced by a helper call,
  measured). A view the batch calls is bound on its own and does not count. The refusal names
  the call and says the name is fine elsewhere, so the model renames once.

### The sandbox's posture, for a host to read (#25)

- **`hello` reports `sandbox_posture`** (`hello_says_how_the_engine_is_confined`):
  `{"posture": "full" | "partial" | "none" | "unavailable", "landlock_abi", "filesystem",
  "network", "reason"}`, beside the prose of `sandbox`, which stays. A host gated its sessions on
  `sandbox` being the words "fully enforced", which a rewording would break. A reply field, so
  sent in every protocol version, and no new version.
- **A kernel without TCP rules is said to be one** (`a_kernel_without_tcp_rules_is_reported_as_partial`).
  Before Linux 6.7 (Landlock ABI 4) every ruleset is enforced partially: files confined, and the
  engine and every confined loader free to open any TCP connection. benchy still serves there, as
  before, but `hello` says `"network": false`, the operator's log says the network is not
  confined, and the README, REQUIREMENTS.md and CONTRIBUTING.md no longer claim the ClickHouse and
  HTTP loaders are held to their source's port on such a kernel.
- **`--require-full-sandbox`**, for a server and `host`
  (`a_session_that_must_be_fully_sandboxed_refuses_to_start_otherwise`): start only where the
  engine's posture is `full` with TCP rules, and otherwise say why and exit, a server before any
  source loads. Stricter than `--allow-unsandboxed`, and wins over it.
- **`--require-memory-ceilings`**, for a server and `host`
  (`a_session_that_must_have_memory_ceilings_refuses_to_start_without_them`): start only where the
  engine's and the loaders' memory ceilings are enforced, which is Linux.
- **`doctor --probe --json`'s keys are documented**, in the README, and pinned
  (`the_doctor_json_has_the_documented_keys`).
- **An NDJSON or JSON file holding `infinity` in a timestamp or date column is refused**
  (`an_ndjson_infinity_is_refused`). DuckDB's JSON reader reads `infinity` and `-infinity` there
  as 1900-01-01 00:00:00, and the file loaded with that time. The refusal names the file and the
  column and says what to do: null or a real timestamp, the column's type declared in
  `[[source.schema]]` (which keeps the infinity), or CSV or Parquet. A genuine 1900-01-01 still
  loads. The check costs about 1.5 ms of CPU for a file read the sniffed way, whatever its size,
  and nothing for one whose timestamps are read as text (docs/PERFORMANCE.md).
- **`doctor --probe` says "partial" below Landlock ABI 4** (`doctor_without_tcp_rules_says_partial`).
  Where every check that counts held but the kernel has no TCP rules, the verdict is `partial`
  (PARTIAL in prose), with the same `reason` as `hello`'s `sandbox_posture`, rather than
  `enforced` and VERIFIED. The exit status stays 0, since benchy serves there without a flag;
  only `not_enforced` is non-zero.

## [0.1.0] — 2026-10-16

First public packaged release, under the MIT license. **This is an experimental prototype,
not ready for production. Expect frequent breaking changes; pin the version you integrate with.**

- Load files, ClickHouse, HTTP sources and Arrow connectors into a private DuckDB workspace.
  Explore through the `describe`, `query`, `get_result` and `dialect` MCP tools, or the host protocol.
- Native bundles for Linux x86_64/aarch64 and macOS Apple silicon/Intel include DuckDB,
  example data and third-party licenses. The installer checks SHA-256; release assets carry
  GitHub build attestations.
- Docker images at `ghcr.io/comino/benchy-mcp:0.1.0` for Linux amd64/arm64 run as a non-root
  user and speak MCP on stdio. Tags `0.1` and `latest` follow the newest release in their series.
  The default Dockerfile also builds directly from source.
- Release publication requires the Linux, ClickHouse and macOS tests, native bundle smoke
  tests, and real MCP/host checks inside the containers. Container checks keep Docker's default
  seccomp profile and exercise Landlock with a read-only root filesystem.
- Fix a parallel test fixture race found by macOS CI. Update rustls to 0.23.46.
- Handle a stream write's callback error in the Node.js host example, before its asynchronous
  `error` event arrives.

Known limits: no OS sandbox on native macOS; no Windows binary; connectors run with their
process's rights; no independent security audit. Engine and loader ceilings do not cap the
parent server or queued requests: use an external whole-session memory limit, such as Docker's
`--memory`. See the README's Docker examples and Limits before using the prototype.

## Prototype development history

The development before the first public release is in
[docs/history/CHANGELOG-prototype.md](docs/history/CHANGELOG-prototype.md).
