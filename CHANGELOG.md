# Changelog

What changed in benchy-mcp. The version is the one in `Cargo.toml`, which is what
`benchy-mcp --version` reports. This is a work in progress prototype under heavy development:
frequent breaking changes are expected, with no stable API or backward-compatibility guarantee.

## Unreleased

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
  (`the_doctor_json_has_the_documented_keys`). The README says that its verdict counts what the
  kernel can refuse, so below ABI 4 `enforced` comes with the network unconfined.
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

The entries below record development before the first public release. Later release notes
supersede earlier packaging decisions.

### Public prototype status

- Make the prototype status and expected breaking changes prominent in the README, contributor
  guide and bundle documentation. The project remains MIT-licensed and is not ready for production.

### A CTE cannot stand in for a helper's table in any statement

- **The CTE rule holds in UPDATE, DELETE, MERGE, INSERT and PIVOT too**
  (`a_cte_in_any_statement_cannot_stand_in_for_a_table_a_helper_macro_reads`). From a host's
  report: `MERGE … USING (WITH meters AS …)` made a helper macro count the CTE's rows and wrote
  that count into the agent's table, `ok: true`; the documented gap for UPDATE and DELETE was
  wider, since every WITH-led subquery of a statement other than a SELECT went unread. Now each
  WITH is given to DuckDB with the text after it, and DuckDB's own syntax-error position says
  where its query ends (a `)`, `RETURNING`, `ON CONFLICT`); a text DuckDB reads whole but not as
  SELECTs is cut at a `;` or ahead of the INSERT, UPDATE, DELETE, MERGE or PIVOT the WITH leads.
  Covered: MERGE's USING, ON, conditions and actions; UPDATE's SET, FROM and WHERE; DELETE's
  USING and WHERE; INSERT's VALUES, BY NAME, RETURNING and ON CONFLICT; PIVOT and UNPIVOT; a
  WITH ahead of any of them. No second parser. A batch that would need more than 64 parses is
  refused rather than let through unread; a whole WITH query inside a string of such a statement,
  under a taken name, is refused too.
- **A statement with non-ASCII text no longer kills the engine** when helper macros are loaded
  and it has a WITH and a taken name (`UPDATE m SET note = 'é, done with meters'`): the old
  fallback sliced the text inside a multi-byte character and panicked, the engine was rebuilt,
  and the agent's own tables were gone. Words are now found in bytes.
- **Measured** (docs/PERFORMANCE.md, "A CTE named like a table a helper macro reads"): nothing
  without helper macros, nothing for a statement without a WITH and a matching name, and about
  0.35–0.4 M instructions more a call for one with both.
- docs/HOST-PROTOCOL.md, docs/DIALECT.md and REQUIREMENTS.md no longer say a WITH inside an
  UPDATE or DELETE is unread, and name what is covered.

### Wide files load within the stage's cap

- **A file of wide rows is read on one thread, under the cap** (REQ-7
  `a_wide_csv_is_not_held_in_memory_while_it_is_staged`). Each thread filling a stage pins a
  block of every column, and DuckDB's JSON reader holds a vector of 2,048 parsed rows, so a stage
  of wide rows on every thread ran out of memory under its 256 MiB cap and was staged again
  without it, held whole; once loaders had a memory ceiling, such a file failed at its 1 GB. Now a file
  whose first 64 KiB show rows that wide (`a_file_is_read_on_one_thread_where_its_first_line_is_wide`)
  is read on one thread from the start: a CSV straight into its table, a JSON file through a
  stage held to the cap and its reader's vector
  (`a_wide_json_file_is_read_on_one_thread_within_its_vector`), its write and checkpoint to the
  cap. A stage that still runs out of memory is read on one thread next, and only then without
  the cap. The table is the same file every way
  (`a_table_read_on_one_thread_or_staged_is_the_same_file`, renamed from
  `a_stage_held_to_its_memory_cap_makes_the_same_table_or_goes_without_it`).
- **Measured** (docs/PERFORMANCE.md, R6-W): a CSV of 300 `DOUBLE` columns (827 MB) 2,117–2,281 →
  338–343 MB, first `describe` 25.5–30.2 → 23.2–29.3 s, loader CPU 32–40 → 23–28 s; an NDJSON
  of 2,000 keys an object (163 MB) 635–648 → 549 MB, 21.3–22.4 → 17.8–18.4 s; 10,000 objects of
  one-digit keys, 1,125 → 554–578 MB. The harness's narrow loads are unchanged: csv-1m, ndjson-1m, csv-10m
  and ndjson-10m within ±0.6% instructions, peaks within ±3 MB.

### A memory bound for loaders (host protocol 6)

- **A loader past its memory ceiling is killed, and its load attaches nothing** (REQ-8
  `a_loader_past_its_memory_ceiling_is_killed_and_attaches_nothing`). The engine had a ceiling;
  the loaders had none, and in a container one runaway load was every session's OOM kill.
  Measured unbounded: a 676 MB CSV of 300 columns, too wide for its 256 MiB stage, took its loader
  to 2,090 MB; a 188 MB NDJSON of 2,000 keys an object, 630 MB; a connector that held 800 MB
  before writing a row, any amount. The supervisor now reads each loader's resident memory every
  50 ms while it runs and kills it past `--loader-memory-ceiling` (or `[loader] memory_ceiling`;
  default 1 GB, against 10M-row peaks of 94–410 MB). The load fails `E_LIMIT`, saying what was
  held and what to do: narrow the source or raise the ceiling.
- **A connector counts against its loader's ceiling** (`a_connectors_memory_counts_against_its_loaders_ceiling`),
  with every process it starts, found through the kernel's `children` lists; the kill takes the
  connector's whole group. Where the kernel has no such list, a `/proc` scan once a second.
- **A host may lower it** (`a_host_lowers_the_loader_ceiling_and_cannot_raise_it`): host protocol 6
  reports `limits.loader_memory_ceiling` and takes it in `hello`'s `limits`, never raised. Not
  reported before 6, since a host may send `limits` back as it got them. A channel stream whose
  loader is killed loses its place and closes the channel. Off Linux the ceiling is reported as
  `null` with why, and `doctor` says so.
- **Cost:** nothing measurable on the ingest bench (docs/PERFORMANCE.md, R6-L1).

### A new user's pass, and the docs held to the code

- **Claude Code 2.1 lists the tools** (`a_client_without_initialize_can_list_the_tools`). It
  speaks MCP 2026-07-28 without `initialize`, where a list result must carry `ttlMs` and
  `cacheScope`; without them the Quickstart ended in "Connected · tools fetch failed". The tool
  list now says an hour, private.
- **"stdout" only for a connector.** A host's stream, or a ClickHouse or HTTP body, whose first
  message is no Arrow schema was refused as "no readable Arrow stream on stdout"
  (`a_bad_attachment_attaches_nothing`).
- **The README's Options block is `--help`'s**, checked by
  `the_readme_shows_the_options_as_help_prints_them`; it lacked `--stream-channel` and `check`'s
  HTTP half. `--help` registers the binary by its full path, as the README says to.
- **Docs**: HOST-PROTOCOL has a table of what each of versions 1 to 5 adds and how a version is
  agreed, and says a host serves DIALECT.md itself, under benchy's tool names; the README, SOURCES,
  HTTP-SOURCE, EXEC-CONNECTOR-PROTOCOL, REQUIREMENTS, CONTRIBUTING, `examples/` and
  `examples/benchy.toml` say what the code does where they did not (the HTTP loader is confined
  as ClickHouse's is; `[engine]` in a host's config; a CSV declaration is DuckDB's `types`;
  the HTTP admission; the redirect port; a root-owned connector; broken links to a README
  section that no longer exists).

### A host's re-review: doctor's entries, and macOS test hygiene

- **Each `doctor --json` mechanism says what was checked** (`doctor_without_landlock_says_not_enforced`).
  At ABI 0 every entry of `mechanisms` said `"enforced": true` beside a `not_enforced` verdict.
  An entry is enforced now only where at least one of its checks counted and every one held.
- **A distinct count too low stays an estimate.** 50 values on 50 rows read "about 47
  distinct". Counting an estimate near the column's values exactly was measured and not taken:
  a 50,000-row table of unique names paid 7% more to load, for a cosmetic gain (`src/profile.rs`).
- **The ENUM rule still reads string literals.** `SELECT 'use ENUM(' AS note` in a view stays
  refused: a string can spell a type DuckDB builds, `json_transform(…, '"ENUM(''a'')"')` is an
  ENUM on 1.5.5, and ignoring literals would let such a DEFAULT or view keep one. The test now
  holds both forms (`an_object_that_keeps_an_enum_in_its_sql_is_refused`).
- **Tests on macOS**: the tests that need Landlock or the memory ceiling are Linux-only, or keep
  their Linux-only part so (`limits_are_lowered_by_the_host_and_raised_only_by_the_operator`
  expects `null` for an unenforced ceiling, `helpers_survive_a_rebuild` drops its ceiling case);
  two exec tests quote the checkout's path in their connector, which may hold a space. `cargo
  clippy --all-targets --target aarch64-apple-darwin` is clean.

### A host's re-review: a refused request closes the channel, and a CTE cannot stand in for a helper's table

- **A request refused whole that names the stream channel closes it**
  (`a_refused_request_that_names_the_channel_closes_it`). A host may write streams ahead of its
  requests, and a request refused as `E_REQUEST` read none of them, so the next attach took one
  under its own name: `tables` naming `meters` twice was refused, and the next attach of `rooms`
  answered `ok: true` with `meters`' 50 rows. Now any request-level refusal of a request that
  names `"stream": "channel"` — a name twice, a bad or unknown field, a list longer than
  `attachments`, `"channel"` before protocol 5, a key given twice, an id still in use — closes
  the channel, says `"channel": "closed"` and why, and is answered in its turn behind the
  requests before it; every channel attach after it is `E_CHANNEL`. A refusal that names no
  channel table leaves it be. What is served pays nothing: the request is read for the channel
  only once it is refused.
- **A CTE may not be named like a table a helper macro reads**
  (`a_cte_cannot_stand_in_for_a_table_a_helper_macro_reads`). DuckDB expands a macro, table or
  scalar, inside the caller's query, so `WITH meters AS (…)` made `hours_above` read the CTE,
  silently; a view reads the table. Such a CTE, at any depth and by any spelling, is refused
  before anything runs, saying to pick another name. Without helper macros a query pays one test
  of an empty set; with them, a look at its text for a WITH and one of those names, and a
  `json_serialize_sql` only when both are there: about 0.7 M instructions a query that has both,
  within the noise otherwise (docs/PERFORMANCE.md).
- docs/HOST-PROTOCOL.md says when a host may write ahead (always: the worst it gets is a closed
  channel), to check a stream's size against `attach_bytes` before writing it, and that Node.js
  gets `ECONNRESET` rather than `EPIPE` from a closed channel and needs an `'error'` handler,
  which `examples/host-node.mjs` now has and checks.

### Hole A from a host's Linux run: a pivot ENUM pinned, and a ceiling that did not reclaim

- **An object of the agent's keeps no ENUM spelled in its SQL** (a re-review's hole A, REQ-9
  `an_object_that_keeps_an_enum_in_its_sql_is_refused`, `a_spoofed_pivot_enum_cannot_be_pinned`).
  A type named `__pivot_enum_0` passed as a dynamic PIVOT's, a table whose INTEGER column's
  DEFAULT cast to it passed the ENUM-column check, the type could not be dropped after the batch
  and that was swallowed: three batches of 1,500,000 values left an idle engine at 799 MB. A
  DEFAULT, generated column, CHECK, index, view or macro that casts to an ENUM, written out or a
  pivot's type, is now refused and undone, as an ENUM column is. Read from the SQL DuckDB keeps of
  each object; `duckdb_dependencies()` lists nothing of the temporary catalog on DuckDB 1.5.5.
  Helper views are held to the same pattern as helper macros, which they were not.
- **A type the agent names is never a pivot's** (`a_type_the_agent_names_is_never_a_pivots`).
  DuckDB names a dynamic PIVOT's type `__pivot_enum_` and a random UUID; a type is taken for one
  only if it is so named and its name is not in the batch, which was written before the UUID
  was drawn.
- **A pivot type that cannot be dropped rebuilds the engine**
  (`a_pivot_type_that_cannot_be_dropped_spoils_the_engine`). The engine says so with its answer,
  and the server kills and rebuilds it, answering `E_REBUILT` with why, so the memory goes too.
- **The memory ceiling holds as the engine answers, not only while it works** (REQ-8
  `an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once`). Read every 50 ms only while a
  request ran, a quicker request could leave the engine past its ceiling, idle, for the next
  request to be killed for. Its resident memory is now read once more as it answers, 5 µs, and
  past the ceiling the engine is rebuilt there, that call answered `E_REBUILT`. Nothing is
  watched while the engine idles, and no lower idle ceiling was added: docs/HOST-PROTOCOL.md says
  why, and what a host can do instead.
- Costs per call, in docs/PERFORMANCE.md: a SELECT unchanged; a CREATE TABLE +2 M instructions,
  a CREATE VIEW +7 M, a CREATE MACRO +11 M on 125 M, a dynamic PIVOT within the noise.

### Follow-ups to the helpers' tables

- **A helper that names a table in a string stops startup**
  (`a_helper_that_names_a_table_in_a_string_is_refused_at_startup`). The tables a helper reads
  are taken from the agent, found in a macro's body as DuckDB parses it; `query_table('meters')`,
  `query('SELECT … FROM meters')`, `pragma_table_info('meters')` and the like name no table
  there, so after a detach the agent's own `meters` would have been what such a macro read. A
  helper names its tables in FROM. A view over `query` or `query_table` stays allowed: DuckDB
  binds those to the scans they make, and the view's plan holds the table.
- **The `query` tool says which names are taken**: a loaded table's, a helper's, a table's a
  helper reads, a system view's or a built-in function's, as docs/DIALECT.md does.

### Holes B and C from a host's Linux run: names the helpers read, and system views

- **A table a helper reads is taken for the agent, on the bench or not** (the re-review's hole B, REQ-18
  `a_table_a_helper_reads_stays_taken_after_a_detach`). After a host detached `readings`, the
  agent's `CREATE TABLE readings AS SELECT 'FORGED' …` was kept: the helper `per_meter` then
  served the agent's rows, and the host's attach of `readings` again was refused with `E_NAME`.
  The tables come from DuckDB, not from parsing SQL in benchy: the helpers job reads each view's
  bound plan (`json_serialize_plan`) and each macro's body as DuckDB parses it
  (`json_serialize_sql`, one more column of the function listing it already made), kept to the
  tables on the bench. The server keeps them for the session and gives them to every engine it
  starts, a rebuilt one too.
- **The table a waiting helper needs is taken** (the re-review's hole C,
  `the_table_a_waiting_helper_needs_is_taken`). `describe` told the agent `per_meter` "works
  once `readings` is on the bench", and the agent's own `readings` then made the host's attach
  fail. A table DuckDB names only later, as a waiting helper's second missing one, stays the
  agent's if it had it first (`a_table_the_agent_had_first_stays_its_own`); docs/HOST-PROTOCOL.md
  says so, with what a helper does after a detach.
- **DuckDB's system views are taken** (REQ-9 `a_system_view_name_is_taken`). An agent view named
  `sqlite_master`, `duckdb_tables` or `pg_class` was kept and answered for the system view
  wherever the agent named it unqualified. All 47 views of the `system` catalog are taken, listed
  by DuckDB once per engine.
- **`Workspace::reserve_function` is gone**, with the "functions the host reserves" in REQ-9: it
  had no caller outside a test. A host's functions are its helpers, which are taken through the
  helpers (`an_agent_cannot_replace_or_shadow_a_helper`).
- **docs/HOST-PROTOCOL.md's limits**: a host may lower six limits in `hello`, not five
  (`memory_ceiling` is the sixth); `attachments` has no operator setting, and `memory` is not a
  host's to lower.
- **A test that result ids continue across a rebuild** through the host protocol
  (`result_ids_continue_across_a_rebuild`).

### Follow-ups: helper names and test hygiene

- **A helper macro named like a word DuckDB's parser reads itself stops startup**
  (`a_helper_named_like_a_parser_keyword_is_refused_at_startup`). `coalesce`, `try`,
  `percentile_cont` and the rest of the names the agent was already held to (REQ-9) are listed in
  no catalog, so the built-in check did not see them: such a helper was served, and a call by its
  plain name was DuckDB's own. The helpers job reads the same set by the same query, only when a
  helper is a macro. A view under such a name stays allowed: FROM reads it as a name.
- **The test suite leaves nothing in TMPDIR.** One `scripts/ci.sh` run left about 187
  directories, mostly benches opened in-process, which nothing removes outside a session. Each
  test binary now keeps what it makes under one root, removed when it exits. A full run with a
  fresh TMPDIR leaves none. Product behaviour is unchanged.

### Fixes from a host's re-review

- **The agent's connection is in UTC on any host.** `TimeZone` is per connection in DuckDB
  1.5.5, and the lockdown set it on the host-facing connection only: on a host in another zone
  the agent's connection took that zone, so `date_trunc('day', …)` bucketed in it and every
  TIMESTAMPTZ column was "not rendered". The zone is now set globally, before the lock, in the
  engine and in a loader's bench, and the seal proves it on the agent's connection
  (`the_agent_connection_is_utc_on_a_host_that_is_not`).
- **`doctor --probe` no longer says "enforced" without Landlock.** At ABI 0 every check was
  skipped and the verdict passed. Below the minimum ABI the verdict is now `not_enforced`, with a
  `reason` in JSON, and the exit status is non-zero (`doctor_without_landlock_says_not_enforced`).
- **The memory ceiling is not reported where it is not enforced.** Off Linux, `hello` reports
  `limits.memory_ceiling` as `null` with `unenforced.memory_ceiling` saying why, and `doctor`
  says so (`a_memory_ceiling_that_cannot_be_enforced_is_not_reported`). Linux is unchanged.
- **`describe` lists an integer series key by number** (`an_integer_series_key_is_listed_in_number_order`):
  `50 series (1 … 50)`, not `(1 … 9)`. The keys are still held and cut in text order.
- **A distinct count is never more than there are** (`a_distinct_count_never_exceeds_the_values`).
  HyperLogLog said "about 24 distinct" of 20 values. An estimate is now held to its column's
  values, counted in the profile's scan or by the stream, and one that reaches them is made
  exact on a table of up to 100,000 rows: "20 distinct". No load costs more for it.
- **A page past the end says so** (`a_page_past_the_end_says_so`): "no rows at offset 10000:
  result_id … has 500 row(s), at offsets 0 to 499", not "rows 10001–10000 of 500". It stays an
  ok reply, with its `sql`.
- **A staged table no longer shrinks the loader's memory limit**
  (`a_memory_limit_put_back_reads_as_it_did`). The limit was put back from DuckDB's text of it,
  which does not always read back the same: `12.7 GiB` became `12.6 GiB` with each stage, and a
  test that compared them failed on a machine whose default was `12.7 GiB`.
- **The crate and its tests build for macOS again.** `MSG_CMSG_CLOEXEC`, `pipe2`,
  `prctl(PR_SET_NAME)` and `clearenv` are Linux's; off Linux, descriptors are made close-on-exec
  with `fcntl` as they arrive, the warden's environment is emptied by hand, and it keeps the
  server's name. Linux is unchanged. Checked here with `cargo clippy --all-targets --target
  aarch64-apple-darwin -- -D warnings` (CONTRIBUTING says how); not linked or run on macOS.

### Scope: one job, done as fast as it can be

- **One job.** benchy gets time-series data into a sealed DuckDB workspace, as fast and as
  cheaply as it can, and nothing else. The sources are ClickHouse and HTTP (both built
  in), with a folder of files and the exec connector for the rest; the one sink is
  DuckDB. The README, CONTRIBUTING and docs/SOURCES.md say so, and no longer suggest other
  built-in sources.
- **No release.** Building from source is the way in. The installer, the bundles and the release
  workflow are kept, and no longer presented as the main path.
- **Verification is local.** `scripts/ci.sh` runs everything `.github/workflows/ci.yml` runs —
  fmt, clippy, the tests, the ClickHouse tests when `BENCHY_CLICKHOUSE_BIN` is set, the
  requirements check, `verify.py`, `host.py` and the eval's checks — and exits non-zero on any
  failure. GitHub Actions is not relied on.
- **Ingest is measured.** `bench/ingest.py` loads the same table from CSV, Parquet, NDJSON, an
  exec connector, ClickHouse and a host's attach, and reports per run the instructions and cycles
  (`perf stat`), and per process and per stage the CPU time, peak memory and wall time.
  `BENCHY_TRACE_TIMINGS=1` makes the binary write those stage timings to the operator log; unset,
  it writes nothing. docs/PERFORMANCE.md has the method, the baseline and the ranked backlog.

### SQL helpers (REQ-18)

- **An operator or an integrator gives the agent views and macros.** A config's `[helpers]`
  names a SQL file of `CREATE VIEW` and `CREATE MACRO` statements, or holds them inline, for
  `benchy-mcp <dir>`/`--config` and `benchy-mcp host --config` alike. They are fixed for the
  session: no tool or protocol call adds one. `describe` lists each with its signature and the
  `--` comment above it. A host measured helpers like these taking an 8-question eval from 38
  agent steps to 26.
- **Read-only, by DuckDB.** A helpers job, a loader confined to its staging directory and locked
  down as the engine is, makes them in a file of their own against empty stand-ins of the bench's
  tables, since DuckDB 1.5.5 binds a view's or a macro's body when it is created; the engine
  attaches the file `READ_ONLY` between the agent's schema and the tables, and an engine rebuilt
  after the hard deadline or the memory ceiling gets it first. Their names are taken (the no-shadowing check): the agent cannot drop, change, replace or
  shadow one, and a host cannot attach under one.
- **Checked at startup.** A helper named like a DuckDB built-in (read from DuckDB's own catalog),
  with the `benchy_` prefix or like a table, one that reads a file, the network or an extension
  or keeps an ENUM (held to the agent's rule),
  any statement that is not a view or a macro, and a server's helper over a table no source
  provides stop startup with the reason. A host's helper over a table it has not attached yet
  waits for it, says so when called, and is made by that attach.
- **The macro-argument trap is documented and tested.** On DuckDB 1.5.5 a macro's argument is
  substituted into its body, so `FROM rooms m, LATERAL above(m.meter, …)` bound `m` to the macro's
  own alias and gave every meter the count of all of them. docs/DIALECT.md and
  `examples/helpers.sql` read arguments once in a FROM-less CTE.
- **A table macro over a missing table waits like a view** (a re-review,
  `a_table_macro_over_a_missing_table_waits_like_a_view`). DuckDB 1.5.5 does not bind a table
  macro's body where it reads an argument, so one over a table not attached yet was listed as
  ready and failed with a bare Catalog Error. The helpers job now prepares a call of each macro,
  arguments NULL; one that reads a missing table is marked waiting in `describe`, says what it
  waits for when called, and is made by the attach.
- **A `DROP` or `ALTER` of a helper is told it is a helper** (a re-review,
  `a_write_to_a_helper_is_told_it_is_a_helper`), not that it wrote to a loaded table with the
  hint `CREATE TABLE my_dewpoint AS SELECT * FROM dewpoint`.
- **Free when unused.** A session without helpers starts and queries in the same instructions as
  before (660 M against 662 M; a query 1.6 M against 1.75 M, within the noise). Three helpers
  add about 420 M instructions and 200 ms to startup, the helpers job and one listing of every
  function on each side, and nothing to a query over the SQL written out (docs/PERFORMANCE.md,
  "Helpers").

### Hosts

- **A host streams its attachments** (host protocol 3, R5-H1). `attach` took the path of a
  file, so a host that fetches with its own credentials spooled every load first — on tmpfs,
  in memory, in a container: 229 MiB per 10M-row load. Now `"stream": true` and the read end of
  a pipe, sent with the request over a session whose stdin is a Unix socket, put the stream on
  the bench as the host writes it: no byte through the supervisor, no file on either side, the
  loader confined to its staging directory alone, and `ping` answered meanwhile. Every rule of the
  file form holds and the table is the file form's, block for block. With the fetch at 100 MB/s
  the first `describe` came 38% sooner at 10M rows (6.19 → 3.85 s), for the same instructions
  and loader memory. Protocols 1 and 2 are served as before; the file form stays.
  `examples/host.py` streams one table; `bench/ingest.py` has `hostspool-*` and `hoststream-*`.
- **A host that cannot pass a descriptor streams too** (host protocol 5, R5-H2). Node.js and Java
  have no public way to send one, so they spooled. `benchy-mcp host --stream-channel FD` takes one
  more inherited pipe as the session's stream channel: the host writes Arrow streams into it back
  to back, and each attach with `"stream": "channel"` takes the next whole one, a `tables` list in
  its order. Plain pipes for the protocol; no byte through the supervisor; the loader confined as
  before and reading to the stream's end-of-stream marker and not a byte further. Every rule of
  the file form holds and the table is the same, block for block. A refused stream is read past to
  its marker (at most `attach_bytes` more) so the next attach is unaffected; where a stream's end
  cannot be found the channel is closed, that answer says `"channel": "closed"`, and later channel
  attaches are `E_CHANNEL`. Measured against the descriptor form: the same instructions, loader
  memory and bytes, and the first `describe` no later. The descriptor form stays, beside it.
  `examples/host-node.mjs` is a host on Node.js; `examples/host.py` uses the channel too;
  `bench/ingest.py` has `hostchannel-*`.

- **The agent keeps no ENUM** (REQ-9). DuckDB holds an ENUM's values outside its memory budget:
  an ENUM type of 3,000,000 values left an engine with a 1 GB budget holding 698 MB that
  `duckdb_memory()` called 0, and ENUM columns piled up batch after batch. `CREATE TYPE`, and a
  table or view with an ENUM column, nested or not, are now undone in the same check that
  refuses a taken name; a dynamic PIVOT's own type works for its statement and is dropped at the
  end of the batch. A CREATE or ALTER pays one more catalog listing; a query nothing.
- **The operator sets a host's limits, and a host may lower them** (host protocol 3). What one
  attachment and a session may hold was fixed, and a real host's load reaches 240 MB of Arrow
  against the 256 MiB an attachment may be. `benchy-mcp host` takes `--attach-rows`,
  `--attach-bytes`, `--session-rows` and `--session-bytes`, or `--config` with `[attach]` and
  `[budget]` and no sources; a limit set in both places is refused. A host lowers any of the five
  in `hello`'s `limits`, never raises one (`limits_are_lowered_by_the_host_and_raised_only_by_the_operator`).
- **The engine has a memory ceiling** (REQ-8). DuckDB's `--memory` bounds its buffer manager,
  not planning: nested `MAP`s took an engine with a 64 MB budget to 229 MB, `repeat('x',
  600000000)` one with 256 MB to 1,185 MB, an ENUM type of 3,000,000 values one with 1 GB to
  1,065 MB, and in a container one engine's growth is every session's OOM kill. The server now
  reads the engine's resident memory every 50 ms while it answers a request and kills it past
  the ceiling, rebuilding it as past the hard deadline: `E_REBUILT`, with a message that says it
  was the memory and how to use less. It costs nothing between requests and 0.2% of a core during
  one. The operator sets it with `--memory-ceiling` or `[engine] memory_ceiling` (default:
  `--memory` and half again, at least 256 MiB over it); a ceiling below `--memory`, or a size
  that does not parse, is refused at startup. A host lowers it in `hello`'s `limits`, never
  raises it, and `hello` reports `memory` and `memory_ceiling`.
- **A host states the shape of what it attaches** (host protocol 3). `time_column` and `series`
  on `attach`, as an operator's config has them, held to the stream's schema before a row is
  written; `describe` then says "is", not "looks like", and says it the same as for a file or a
  connector with the same stated shape (`a_stated_shape_looks_the_same_from_every_source`).
- **A host reads facts, not prose** (host protocol 4, and two reply fields in every version).
  A host had to parse the footer to learn that a result was empty, keep its own map from result
  to SQL, and watch its agent be sent to `get_result` and `describe` when its own tools are called
  otherwise. Now `query` answers with `results`, one entry a result: the rows benchy holds,
  whether that is all of them, how many the text shows, and whether the text is less than the
  whole result. `page` answers with `sql`, the agent's statement its result came from — never a
  helper's body, cut at `max_bytes`. And in protocol 4, `hello`'s `tool_names` gives the agent's
  tools the host's names: every footer, hint and `describe` line that names a tool uses them.
  The names are plain identifiers and address nothing; without the map, and in MCP mode, the
  texts are as before. A new request field is a new version, so `tool_names` is protocol 4, and a
  host speaking 3 is served the same, with the two reply fields beside what it read before.
- **Result ids are unique across sessions.** Every session numbered from `r1`, so a host whose
  conversations outlive sessions could resolve an old id to a different answer. An id now
  starts with a random prefix of its session's own, `3fa9c1b2-7`; it names nothing another
  session could use, and resolves only where it was issued.
- **`doctor --probe --json`** prints one JSON object for a host to check at container start:
  an overall `verdict`, the exit status agreeing, and each mechanism (the engine, a file loader,
  a network loader) with every check it made. The prose is unchanged.

### Faster ingest

- **Every JSON or NDJSON file is staged, whatever its size** (R4-2), and so is an HTTP NDJSON
  body once spooled: read on every thread into a stage held to 256 MiB and spilling past it,
  then written, as a CSV is. Over 64 MiB one was read straight into its table on the write's one
  thread. At 10,000,000 rows the first `describe` comes 28–42% sooner (ndjson-10m 6.67 → 4.59 s,
  ndjsondecl-10m 6.52 → 3.81 s, http-ndjson-10m 8.07 → 5.84 s, minima), for about a fifth more
  instructions (the stage's own appends and spill) and the loader at the stage's cap, 351 MB
  where it held 132 MB, spilling about 134 MB to its staging directory, removed with the stage.
  1M and 1k rows load as before (`a_json_file_is_staged_whatever_its_size`).
- **JSON pages are fetched ahead of their decode** (R4-3). A paged JSON source was fetched,
  decoded and followed a page at a time on one thread. Now a fetch thread scans each page for
  its rows and cursor without decoding them and asks for the next at once, decoder threads decode
  the pages with arrow-json, and the load takes the batches into the table in page order, so the
  table is the same. At most four pages wait between the fetch and the table. http-json-1m's
  first `describe` 1.13 → 0.68 s (−40%, minima), for 11% more instructions and 6 MB more loader
  memory; 1k rows unchanged. Faults still come in page order, and a page's decode must agree
  with its scan (`json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order`,
  `a_pages_scan_reads_its_rows_and_cursor_as_its_decode_does`).

- **A loader's file is written in storage version v1.3.0** (set as the file is created): text is
  compressed as DICT_FSST, chosen without sampling, so the same rows make the same file on every
  load. DuckDB's default chose between dictionary and FSST from a random sample, and at a near
  tie the layout changed between loads (`a_files_layout_is_the_same_on_every_load`). It is also
  cheaper to write: exec-10m 17.9% fewer instructions and its first `describe` 20–24% sooner,
  the 1M-row scenarios 6–15% fewer; the same file size.

- **A file source may be declared** (R3-5): `[[source.schema]]`, and `delimiter` for a CSV or
  TSV, on a `type = "file"` source, as the operator's choice. It is read as a declared HTTP body
  is — a CSV in its stated dialect, columns matched by the header's names; an NDJSON file with
  its `columns`, not sniffed; a Parquet file pinned — and fails loudly where it does not match.
  A 1,000-row CSV loads with 42% fewer instructions and its first `describe` 38% sooner; a
  1,000,000-row NDJSON file with 19% fewer and 11% sooner. Undeclared files are sniffed as
  before (`a_file_source_may_be_declared`, `a_declared_file_loads_as_the_sniffed_file_does`,
  `a_declared_file_that_does_not_match_fails_naming_why`). `bench/ingest.py` has `csvdecl-*`
  and `ndjsondecl-*`.

- **An HTTP CSV or NDJSON body is spooled, then read on every thread** (R3-3). It went to
  DuckDB through a FIFO, which DuckDB parses on one thread; it is now written to a file in the
  loader's staging directory, held to `max_bytes` as it is written and removed once read, and
  read as the same file is. First `describe` at 1,000,000 rows: http-csv-1m 1,088 → 881 ms,
  http-ndjson-1m 1,274 → 981 ms (minima; medians −16% and −23%), instructions unchanged, the
  NDJSON loader's peak 173 → 145 MB. The disk it takes is one body at a time, at most the
  source's `max_bytes`
  (`a_csv_or_ndjson_body_is_spooled_to_staging_within_max_bytes_and_removed`).
- **A CSV's stage is held to 256 MiB of memory**, and spills past it to the loader's staging
  directory. A CSV file was staged whole in memory, so its loader grew with the file: the
  harness's 10M-row CSV took it to 513 MB, now 406 MB, and past the cap the peak stays put as
  the file grows (`a_large_csv_is_not_held_in_memory_while_it_is_staged`). Every stage is held
  alike: a JSON file (of up to 64 MiB until R4-2, of any size since), an HTTP CSV or NDJSON body. At 1M rows and below nothing
  spills and nothing changes; at 10M rows the spill costs 4.5% more instructions and about 90 MB
  of disk, and the first `describe` comes 7–14% later (docs/PERFORMANCE.md, "Memory and disk").
  A stage too wide to fit the cap was staged without it, as before, and the operator's log said
  so; since, wide rows are read on one thread under the cap ("Wide files load within the stage's
  cap" above; `a_table_read_on_one_thread_or_staged_is_the_same_file`).
- **The warden is forked from the server, not started from the binary again** (R3-11): about
  21 M fewer instructions a session (exec-1k −4.1%, host-1k −5.2%), the dynamic linker's work on
  `libduckdb.so`. It is forked before the config is read, zeroes its copy of the environment and
  closes every descriptor but its pipe, so it holds none of the server's credentials
  (`the_warden_holds_nothing_of_the_servers`). `--warden` is no longer a mode of the binary.
- **An NDJSON file's or HTTP body's timestamps are read as text and cast** (R3-1), not parsed
  with the format DuckDB's sniffer found: the file is sniffed once, then read with those columns.
  ndjson-1m and http-ndjson-1m: 9–9.5% fewer instructions, their first `describe` about 7%
  sooner. Files and bodies under 1 MiB are read as before. The table is the sniffed read's (`a_json_files_timestamps_read_as_text_load_as_the_sniffer_reads_them`); a value
  the cast does not take as the sniffer does has the file read the sniffed way, and a file whose
  columns that read would change — a date, a struct, two keys differing in case — is read as
  before (`a_json_file_whose_columns_the_text_read_would_change_is_read_as_it_was`). A file the
  sniffed read refused for a time the cast reads exactly, such as an offset in a vector that began
  without one, now loads.
- **A file's profile is kept from one scan of its table** (R2-4), as a stream's is from its
  batches, instead of the scan and the pass that each read it back: parquet-1m 12.7% fewer
  instructions and 14% less loader CPU, csv-1m and ndjson-1m 6–7% and 4–5%, an HTTP Parquet body
  12%. `describe` says the same, byte for byte
  (`a_files_profile_is_kept_from_one_scan_and_is_the_profile_of_the_table`).
- **A file's profile is kept beside its write** (R3-2), from what the table is written from — a
  staged file's stage, a Parquet file read a second time — on a thread of its own, instead of
  from the written table on the load's one thread after it. Against the 256 MiB stage, the
  5,000,000-row, four-column CSV of docs/CAPACITY.md answers its first `describe` 8% sooner
  (minimum; 7% median), csv-1m, ndjson-1m and http-csv-1m 5–7%. The CSV's loader, its stage
  spilling, peaks 47 MB higher, and still does not grow with the file. A Parquet file pays 3–4%
  more instructions for its second read, for a first `describe` 5–9% sooner. A JSON file too
  large to stage is still watched after its write (`a_files_profile_is_kept_beside_its_write`).

- **The keyed tally costs 28% fewer instructions a row** (R3-4): what a row changes of its
  series is kept apart in plain fields, the series found by its key's first sixteen bytes, one
  compare a row when meters arrive in a cycle, and a batch added a column at a time. At 500
  series the watch went from 398 to 285 instructions a row: exec_series-10m 4.9% fewer
  instructions and 40% less watch CPU. A stream of one series costs about 3% more in its watch
  (+0.5% of exec-1m). `describe` says the same, byte for byte.

- **An integer key is tallied by its value** (R3-10), and written as text once per series, as
  `CAST(… AS VARCHAR)` writes it, where it was written for every row. 10,000,000 rows of 500
  integer-keyed series (`exec_intkey-10m`, a new harness scenario) take 27% fewer instructions,
  the watch's CPU 926 → 364 ms, and the first `describe` 4–7% sooner, the watch off its path again.
  `describe` says the same, byte for byte (`an_integer_key_is_written_as_text_once_a_series`).

- **zstd is decoded by C libzstd**, not ruzstd: 1.64 G instructions for a 10M-row ClickHouse
  response where ruzstd took 5.99 G, and a 10M-row zstd load 12.8% fewer instructions in all
  (34.3 G to 30.0 G). Decompression lives in `src/decompress.rs`, for the HTTP source to share,
  and stays bounded: decoded only as the reader pulls, so a bomb stops at the cap
  (`a_bomb_is_decoded_only_as_far_as_it_is_read`), with a window of at most 32 MiB
  (`a_zstd_window_past_32_mib_is_refused`). libzstd is built from source by `cc`, which `ring`
  already needed. docs/PERFORMANCE.md, item 3, has the decoders compared, gzip's among them.
- **An unset ClickHouse `compression` follows where the server is**: `none` on loopback, `zstd`
  anywhere else (`an_unset_compression_follows_where_the_server_is`). On one machine zstd cost
  ClickHouse 1.6 s more CPU to send 10M rows than none, and benchy more to decode: a 10M-row load
  from a local server now takes 28.5 G instructions where it took 34.3 G. `bench/ingest.py`
  takes `--ch-port` and `ch_<compression>-<size>` scenarios.
- **A stream's batches go into DuckDB's vectors directly.** Fixed-width columns are copied as
  they lie, validity 64 rows at a time, and strings without DuckDB's per-value UTF-8 check, which
  arrow had already made; short strings are written inline. The `INSERT` each batch goes through
  is prepared once per stream. A 10,000,000-row load costs 5.0% fewer instructions from an exec
  connector or ClickHouse; 1,000,000 rows from a stream (exec, ClickHouse, a host, HTTP Arrow or
  JSON) 2.5–3.9% fewer. Measured and not adopted:
  gathering batches to a row group, and turning compression methods off (docs/PERFORMANCE.md).
- **A stream's profile is kept as its rows go in** (REQ-14), so the table is not read back
  for it: the scan's aggregates, its distinct counts as DuckDB's own `approx_count_distinct`
  gives them (the same HyperLogLog, ported), and the pass, which no longer needs the step to
  add up. `describe` says the same, byte for byte. 10,000,000 rows load with 12.5–15% fewer
  instructions; 1,000,000 with 6–13% fewer; peak memory rises 1–4 MB. `bench/ingest.py` has
  `exec_series-*` scenarios, where 500 series share every minute.
- **A Parquet or JSON file is read straight into its table**, without the in-memory staging
  table it was read into first, and so is an HTTP Parquet body: 14.4% fewer instructions for
  1,000,000 Parquet rows and peak memory 102 MB to 72 MB; NDJSON 3.2% fewer and 143 MB to 99 MB.
  A refused body rolls back. A CSV stays staged: DuckDB parses it on every thread, and read
  straight in on one it took 21% longer.
- **The round, measured together** (docs/PERFORMANCE.md, round 2's baseline): against round 1's
  baseline binary, interleaved in the same sessions, a 10M-row ClickHouse load takes 30% fewer
  instructions and 26% less CPU, a 10M-row connector load 17% and 16%, a 1M-row Parquet file
  13% and 10% with a loader of 70 MB instead of 100 MB, and a session with 1,000 rows answers
  its first `describe` 26–40% sooner. An NDJSON file's first `describe` came 17–32% later, read
  on one thread since it goes straight into its table (R2-1, below). The backlog is re-ranked from
  profiles of this `main`, with the DuckDB settings that touch compression and statistics
  measured; `bench/ingest.py` gains `exec_lz4-*`, a stream with LZ4-compressed Arrow bodies.
- **A host attaches several tables in one request** (host protocol 2, R2-5). `attach` takes
  `tables`, a list of `{"name", "path"}`, and answers each table on its own, in order, held to
  the session's count of tables and budget as if attached one after another; one bad table is
  refused alone. Their loaders run side by side, at most four at once, and the engine checks them
  in one request. A host speaking protocol 1 is served as before. Eight 1,000-row tables:
  2.48 G instructions and 1.16 s to `describe` one request at a time, 1.63 G and 0.61 s in one;
  each table after the first costs 162 M instructions instead of 282 M (−43%) and 52 ms instead
  of 129 ms. `bench/ingest.py` gains `host<N>-<size>`, N tables in one session.
- **The engine checks a batch of attached files with one listing of each kind**, not one query
  per file: the views, sequences, indexes, types, schemas, tables and columns of every catalog
  in the batch at once. Each table after the first is checked for about 4 M instructions instead
  of 35 M, and one table for 56 M instead of 67 M; a folder's tables and a rebuild gain the same. Each catalog is still judged on its own
  (`anything_else_in_one_file_of_a_batch_refuses_that_file_alone`).
- **A loader writes each row group as it fills** (`write_buffer_row_group_count = 1`, R2-8),
  not after DuckDB's five: a stream's or a Parquet file's loader peaks 11–16 MB lower (exec-1m
  70 → 57 MB, exec-10m 116 → 101 MB, ch-10m 104 → 94 MB), with the same instructions, the same
  layout from every source and queries as fast. Each row group now starts a block of its own:
  the eval's meter data is 29 blocks where it was 28. `storage_compatibility_version = 'latest'`
  was measured again on top and not taken: no instructions to gain, and a file written
  differently for it.
- **A stream's profile is kept from its batches as they lie, beside the insert** (R2-2, R2-7).
  The watch reads each Arrow column's values and validity in place instead of copying them,
  keeps no tally for one series beyond the sums of its times, hashes a tallied key once per
  series, and stops counting the key candidates when the first batch's times do not repeat. One
  series' runs of 0 or null are kept as they arrive instead of read from the table. The watch
  runs on a thread of its own, started after the loader is confined. Against the build before it: a
  10M-row connector load takes 13.7% fewer instructions (23.03 → 19.87 G), ClickHouse 13.4%,
  500 series 9.2%; `stream.watch` 670 → 160 ms of CPU for one series. Trace lines carry their
  thread, and `bench/ingest.py --record` places a sample on its own thread's stage.
- **An LZ4 Arrow body is decoded once** (R2-6), not once to check its claims and again in arrow:
  the checked, decoded buffers go to arrow stored, prefixed `-1`. A 10M-row stream with LZ4
  bodies takes 3.8% fewer instructions (21.60 → 20.78 G); the bound on what a stream decodes to
  is unchanged.

- **A declared HTTP CSV states its dialect instead of having it sniffed** (R2-3): `delimiter`
  (new, `,` unless set) and RFC 4180's quote and escape. A 1,000-row declared CSV session takes
  43% fewer instructions and answers its first `describe` 34% sooner (357 ms to 236 ms); 3% fewer
  instructions at 1,000,000 rows. Columns are still matched by the header's names, and a body in
  another dialect fails loudly, naming `delimiter` (`a_declared_csv_is_read_in_its_stated_dialect`).
  A declared CSV column the header lacks is now `E_SCHEMA`, as a missing column is for every other
  format, rather than `E_CONFIG`.
- **A JSON or NDJSON file of up to 64 MiB is staged again** (R2-1): parsed on every thread, a
  1,000,000-row NDJSON file answers its first `describe` 19% sooner (1.08 s to 0.88 s, minimum
  of ten interleaved runs; 20% at the median), for 3% more instructions, 9% more loader CPU and
  a loader of 143 MB instead of 89 MB, as the owner decided: latency over load-time memory. A larger file is read straight in, so that copy stays
  bounded. A file that fails part-way, read straight in, now says why in DuckDB's words; it said
  only "Current transaction is aborted"
  (`a_file_that_fails_midway_leaves_no_table_and_says_why`).
- **Round 2, measured together** (docs/PERFORMANCE.md, round 3's baseline): `main` with R2-1 to
  R2-8, against round 2's start, interleaved: a 10M-row ClickHouse or connector load takes 9–16%
  fewer instructions and answers its first `describe` 10–17% sooner, with loaders 8–16 MB
  smaller; a 1M-row Parquet file 13% fewer, an NDJSON file 24% sooner; eight host tables in one
  request 63% sooner. Since round 1's start, a 10M-row ClickHouse load is 40% fewer instructions
  and 30% sooner. `main` passed `scripts/ci.sh` with every ClickHouse test run. One cost found: a
  wider file's first `describe` is about 4% later at 5,000,000 rows, its profile now read on one
  thread after the write (R3-2). docs/CAPACITY.md and the README's numbers are regenerated, and
  the backlog is re-ranked for round 3, with what is DuckDB's and what is benchy's.
- **Round 3, measured together** (docs/PERFORMANCE.md, round 4's baseline): `main` with round 3
  merged, against round 3's start, interleaved: a 10M-row ClickHouse or connector load takes 16–27% fewer
  instructions and answers its first `describe` 18–22% sooner (5% with an integer key); CSV,
  Parquet and NDJSON files at 1M rows 15–20% sooner, an HTTP CSV, NDJSON or Parquet body 28–35%
  sooner; a 10M-row CSV's loader peaks at 384 MB, not 512 MB. JSON pages are the one 1M-row path that did not move.
  Since round 1's start, a 10M-row ClickHouse load is 49% fewer instructions and 44% sooner.
  `main` passed `scripts/ci.sh` with every ClickHouse test run, and the layout tests passed
  200 runs under load: the Dictionary/FSST flake is gone. docs/CAPACITY.md and the README's
  numbers are regenerated, and the backlog is re-ranked for round 4: benchy's own code is now
  small and off the path to `describe` everywhere but JSON pages, and what is left is DuckDB's
  write on one thread, which a parallel write could cut by two thirds if the layout can be kept
  origin-blind (R4-1).
- **Round 4, measured together** (docs/PERFORMANCE.md, round 5's baseline): `main` with
  round 4 merged, against round 4's start, interleaved: a 10M-row NDJSON file answers its first
  `describe` 34% sooner (R4-2) and JSON pages 34% sooner (R4-3), each for the extra instructions
  its PR measured; every other path within ±1% in instructions. The write stays on one thread
  (R4-1, the owner's decision for REQ-15). Since round 1's start, a 10M-row ClickHouse load is
  49% fewer instructions and 40% sooner, a 10M-row connector 41% and 34%, a 1M-row Parquet file
  33% and 42%. `main` passed `scripts/ci.sh` with every ClickHouse test run, and the origin,
  layout and paged-JSON tests passed 20 runs each under load. What is left of benchy's own code
  on the path to `describe` is 3–5% at 10M rows, and the fetch thread of JSON pages: the
  backlog says so, and that it is small.
- **The host-engine work, measured together** (docs/PERFORMANCE.md, round 6's baseline): `main`
  before the public history began, after the host-engine work, against round 5's baseline, interleaved: every ingest path within
  ±0.4% in instructions at 1M and 10M rows (exec-1m +1.0% by its minimum, +0.6% by its median),
  and no first `describe` later than the machine's spread; the stream channel costs what a
  streamed attach does. A session's fixed part is 1–9 M instructions more, no measurable time:
  the agent's tables read before an attach (REQ-9), the connection set to UTC, and a no-time
  table's distinct counts held to its values (REQ-14). Skipping the first was measured and not
  taken: the catalog's first read moves to the next listing. A SELECT costs what it did; a
  CREATE pays the no-shadowing and ENUM checks measured in their PRs.

### HTTP source, built (REQ-17)

- **`type = "http"` loads.** A URL answering with Arrow IPC, Parquet, CSV or NDJSON, gzip- or
  zstd-compressed, or a JSON API in pages (cursor, offset, page number, `Link`, next URL), each by
  its fastest measured path: Arrow through the connector's validating reader, Parquet staged and
  checked by its footer's row count before a row group is read, CSV and NDJSON through a FIFO into
  DuckDB's own readers exactly as the file is read, JSON pages decoded by arrow-json straight
  into batches, an undeclared schema DuckDB's sniff of the first page, pinned. In instructions,
  each costs what the same file costs, and JSON pages less than the NDJSON file
  (docs/PERFORMANCE.md).
- **Origin-blind.** The fixture over HTTP in every format answers `describe` and every query byte
  for byte as the CSV file does, and 840,000 rows are laid out in storage as the file lays them
  out.
- **Requests.** One deadline for the whole source; retries with `Retry-After` or jittered backoff,
  never past it; same-origin redirects with the credential, `redirect_hosts` (with their ports)
  without it; a 400 to pushed-down parameters fails naming them. Secrets come from `env` through
  `auth`, live in the loader alone, and are masked in every log line and error, as the API echoed
  them included. No failure leaves part of a table, and what a failed fetch read is charged to
  the budget.
- **Confined.** The HTTP loader is the network loader of REQ-3, Landlocked before its first
  request to its staging directory, the resolver and its ports.
- **`benchy-mcp check`** shows the request with the secret masked, the first page's rows and the
  schema read, as `[[source.schema]]` entries to pin. `bench/ingest.py` has `http-*` scenarios.

### Faster to the first `describe`

- **The engine opens about 70 ms sooner.** Its watch for a dead server slept in 100 ms steps,
  and the seal waited out the step; it waits on a condvar now, and the engine's `Open` takes
  26–31 ms instead of 100–102 ms.
- **The first source loads while the engine opens.** The two need nothing from each other until
  the attach. A folder of one small CSV answers `describe` in 232 ms instead of 351 ms
  (minimum of 21, interleaved, on a shared 4-core machine).
- **Stopping a loader or the engine no longer reads every process's `/proc/<pid>/stat`**, except
  for an exec source's loader, the only one that runs connectors: about 5 ms of the server's CPU
  per session on this machine.
- **A host's attach is held to the session's budget as it is read**, not after it was written:
  the bytes before the first batch, the rows batch by batch.
- **A host's server opens the DuckDB that proves table names while the engine opens**, not after.
- **The tables a session starts with are attached in one request**, and so are a rebuild's. The
  engine's check of an attached file lists every function DuckDB has (about 0.13–0.19 G
  instructions); it is now made once for the whole batch, before any table reaches the agent's
  search path, and is as strict as before. Four small tables reach `describe` with 1,395 M
  instructions instead of 1,750 M. Every loader now runs while the engine opens, and an engine
  that cannot open stops startup once the loads are done.
- `bench/ingest.py` runs any scenario at 1,000 rows (`csv-1k`, `exec-1k`, …), the fixed cost of a
  session.
- docs/PERFORMANCE.md items 5–8 say what was done and measured, and why static linking DuckDB
  and a forked warden were not taken.

### HTTP source, designed (REQ-17)

- **[docs/HTTP-SOURCE.md](docs/HTTP-SOURCE.md)** designs `type = "http"`: Arrow, Parquet, CSV and
  NDJSON bodies, gzip or zstd, and JSON APIs paged by cursor, offset, page number, `Link` header
  or next URL. Each format's path was chosen by measuring decoders on the eval's meter data
  (`spike/src/probe7_http_decode.rs`): DuckDB's own readers for Parquet, CSV and NDJSON, the
  last two straight from a pipe; arrow-json for JSON pages, about half the instructions of
  serde_json.
- **A `type = "http"` source is checked in full, then refused** at startup as not built yet,
  rather than starting a session with a source nothing loads. Secrets come only from `env`
  through `auth`; a URL with a password or a query string, a credential in `headers` and
  `${…}` outside `env` are refused.
- **Its open questions are decided** (2026-10-11, as recommended): `max_bytes` counts the
  decompressed body; redirects stay same-origin; a pushed-down parameter the API refuses fails
  the source, naming what was sent; and the network loaders, ClickHouse's and HTTP's, are
  confined by Landlock to their staging directory and their source's port (REQ-3; built for
  ClickHouse, below).
- **Paging is built and tested on its own** (`src/sources/http/paginate.rs`): a repeated cursor
  or URL, a page repeated byte for byte, more than `max_pages` and a next URL on another origin
  each fail the source; nothing is ever truncated.

### Security

- **No agent object can shadow a host table or a built-in** (REQ-9
  `an_agent_object_cannot_shadow_a_host_object_or_builtin`). The agent still creates what it
  likes, temporary or not, but a `CREATE` or `ALTER` that takes a loaded table's name, or a
  built-in function's, macro's or table function's, is refused, undone with its own transaction,
  and told to pick another name. `CREATE TEMP TABLE readings` used to hide the attached
  `readings`, and `CREATE TEMP MACRO lower` to replace `lower`. A host's attach under a name the
  agent already gave one of its own is refused with `E_NAME`. The taken set takes names a host
  provides (`Workspace::reserve_function`), for config-time helpers. A SELECT pays nothing for it
  (docs/PERFORMANCE.md, "The query path: no-shadowing").
- **Names DuckDB's parser reads itself are taken too** (a re-review, REQ-9
  `a_name_the_parser_reads_itself_is_taken`). `coalesce`, `if`, `try`, `try_cast`, `extract`,
  `percentile_cont` and the like never reach the catalog, so `duckdb_functions()` does not list
  them, and `CREATE MACRO coalesce(a, b) AS 1` was kept: `"coalesce"(NULL, 2)` then answered 1.
  DuckDB's parser finds them, once per engine: every keyword that is not unreserved, and each
  unreserved keyword or name DuckDB's transformer rewrites whose call parses to something else.
  The first function the agent makes pays about 50 M instructions more for it; every later one
  the same as before.
- **benchy's catalog reads cannot be shadowed** (`benchys_catalog_reads_cannot_be_shadowed`).
  Every one names `system.main.duckdb_*()`. Unqualified, a macro the agent named `duckdb_columns`
  answered the host's next attach with forged columns, one named `duckdb_tables` failed every
  later attach and put a table that does not exist in `describe`, and one named `duckdb_types`
  made benchy take the agent's next `CREATE` for a dynamic `PIVOT`'s and drop its answer. A type
  the agent names like a dynamic `PIVOT`'s still is taken for one; all that buys it is its own
  `CREATE` answering nothing (`a_spoofed_pivot_type_gets_nothing_past_the_guard`). Eight cases in
  `corpus/escape.toml` record both.

- **Confined processes can no longer truncate what they were not granted** (REQ-3
  `a_confined_process_cannot_truncate_what_it_was_not_granted`). Every Landlock ruleset handled
  only ABI V1's filesystem rights, which left truncate(2) by path unhandled: a compromised engine
  or loader could empty any file this user can write. Each now handles the rights of the kernel's
  own ABI, up to V5 (`Refer`, `Truncate`, `IoctlDev`), and grants `Truncate` only beneath a
  loader's staging directory, and `Refer` and `IoctlDev` nowhere. `doctor --probe` checks
  truncate for the engine and both kinds of loader. On kernel 6.8 every posture is still fully
  enforced, and the cost is nil. Each grant is cut to what its ruleset handles, so a kernel before
  6.2, which has no `Truncate`, still confines every loader
  (`every_ruleset_builds_for_every_landlock_abi`).

- **The ClickHouse loader is confined** (REQ-3
  `a_network_loader_reaches_only_its_source_and_its_staging`). Before its first request, as its
  only thread, it Landlocks itself to its staging directory, the resolver's five files and, on
  Linux 6.7+, TCP to its server's port; it listens on nothing. Where it cannot be confined it
  refuses unless the server was started with `--allow-unsandboxed`. `doctor --probe` measures a
  network loader's restriction right by right. Its cost on a 1M-row load: about 0.2 ms, within the
  run's noise. HTTP will use the same `sandbox::restrict_network_loader`.

- **View columns are bounded by what they unpack to.** A string or binary view is 16 bytes on the
  wire whatever it points at, so 2,000 views of one 64 KiB block — a 114 KB stream — loaded as
  131 MB under a 1 MiB `max_bytes`. Views now count the lengths they state, for connectors, host
  attachments and ClickHouse alike.
- **A failed fetch is charged to the session's budget.** With a partial start, a source that
  failed after reading a megabyte left the whole budget to the next one. What a fetch read now
  counts whether or not it loads.
- **Nothing may follow a stream's end-of-stream marker.** A second stream after the first was
  dropped without a word, and an exception ClickHouse writes after a whole stream left the rows
  before it loaded; both now fail the load.
- **Per-batch work is bounded for large dictionaries.** A million-entry dictionary picked from by
  6,000 one-row batches took 15.7 s of CPU; each batch now costs its keys.
- **A table's file no longer tells its source.** CSV and Parquet were converted in place and every
  table written in parallel, so `pragma_database_size()` and `pragma_storage_info` differed by
  source. Every source is now written the same way, on one thread; files are smaller and a load
  of the eval's meter data takes about 0.1 s longer.
- **Smaller fixes.** A hostile Arrow schema (an Int 7 bits wide) is refused instead of crashing
  `exec-test`; an LZ4 buffer claiming more than is left is refused before it is decompressed; a
  source's own `max_bytes` is no longer reported as the session's budget; a connector's stderr
  line is kept to 64 KiB, and bytes that are not UTF-8 no longer stop stderr being read.
- **Release supply chain.** Every GitHub Action is pinned to a commit; releases carry signed
  build-provenance attestations (`gh attestation verify`); publishing runs in a `release`
  environment. `install.sh` runs nothing if the download is cut short, insists on HTTPS and TLS
  1.2 or later, and on macOS says what running without a sandbox means rather than suggesting
  the flag.

### Time-series analytics

- **Streams are written as they arrive.** Rows from ClickHouse, a connector or a host's Arrow
  attachment go from the decoder through the conversion into their table a batch at a time, in
  one transaction that commits only once the stream is accepted whole. They were collected into a
  list until the stream ended, then copied into an in-memory staging table, then written. At
  10,000,000 rows from ClickHouse the loader's peak went from 940 MB to 144 MB and the load's CPU
  from 12.6 s to 8.6 s; a 3,000,000-row attach went from 15.1 to 11.3 G instructions. The table
  and its file are what they were (REQ-15), a stream refused after its rows went in still leaves
  no table, and `examples/capacity_clickhouse.py` now reports CPU, the loader's own peak and a
  Parquet file.
- **Profiling costs less at load.** The loader works out a table's shape in one pass over the rows
  instead of a `GROUP BY` series, a `GROUP BY` time per measure and a window over the zero rows,
  and counts distinct values only when the answer is used. `describe` says exactly what it said;
  in CPU time the profile went from 0.34 s to 0.22 s on the eval's meter data (load 0.84 s) and
  from 1.06 s to 0.74 s on five million rows (load 6.0 s).
- **Read-only ClickHouse users load.** A user with `readonly = 1`, the usual analytics account,
  was refused because benchy named output settings on every request. It is now sent none, and
  loads the same rows with ClickHouse's own Arrow defaults; ClickHouse is not told the deadline
  then, and a profile that makes zstd Arrow bodies or binary text is refused before the fetch,
  naming the setting. `check` says which way a source will load (docs/SOURCES.md).
- **`describe` tells the shape of the data** (REQ-14). For each table: the time column, its
  range and cadence, the series key, each numeric column's range, nulls and zeros, and per series
  what is wrong with it — dead, late start, early end, runs of 0 or null, gaps, repeated times —
  and the moments every series shares, such as a clock-change hour. The loader profiles the table
  once; `describe` only renders it. `time_column` and `series` state the shape per source instead
  of guessing.
- **ClickHouse is a built-in source** (REQ-16). `type = "clickhouse"` loads tables, or a
  query, over ClickHouse's HTTP interface: the schema read and mapped to the bench's types, the
  time window and entity allowlist pushed down as query parameters, the load estimated before it
  is fetched, zstd on the wire by default (or Arrow's LZ4, or none), and the password in a header
  from a loader that holds it alone. `benchy-mcp check <cfg> <source>` checks one without a
  model. See docs/SOURCES.md.
- **A session budget**: `[budget]` in the config caps the rows and bytes every load in a
  session may fetch together (defaults 50,000,000 rows and 2 GiB).

### Origin-blind (REQ-15)

- **The write stays on one thread, and the layout tests now compare blocks** (R4-1, measured, not
  done). A write on four threads, split into fixed groups of row groups committed in order,
  answered exec-10m's first `describe` 20–24% sooner with the same row groups, segments and
  compression from every source; but DuckDB 1.5.5 numbers blocks in the order its threads write
  them, so the block a segment lies in, which `pragma_storage_info` shows the agent, changed from
  one load to the next, and through DuckDB's own parallel insert so did the block count. The
  layout tests (`the_physical_layout_says_nothing_of_where_data_came_from`,
  `an_http_table_has_the_physical_layout_of_a_file_table`, `a_files_layout_is_the_same_on_every_load`)
  now compare every segment's block and offset, which held before and which the prototype broke
  while the counts held. docs/PERFORMANCE.md has the measurements and what DuckDB would need.
- One type mapping for every source: integers are `BIGINT` (`UBIGINT` is `HUGEINT`), floats
  `DOUBLE`, timestamps `TIMESTAMP WITH TIME ZONE` in microseconds. A zone-less timestamp is read
  as UTC, or in the source's `timezone`. A timestamp finer than a microsecond is refused, not
  rounded.
- The agent hears about tables, never sources: connector errors and hints go to the operator log
  only. The session starts with the sources that loaded, and `describe` lists one that did not as
  not available.
- LZ4-compressed Arrow IPC is accepted, bounded by what it decodes to.
- When benchy refuses a stream mid-load, its refusal is reported, not the broken pipe the
  connector then dies of.

### For agents and operators

- `describe` reads small tables right: the bundled example finds `meter` as its series key and
  says `office` repeats one reading and misses the next. When times repeat and no column tells
  the rows apart, it says so instead of reporting repeats.
- `describe` quotes data: values are SQL literals (`'MT_223'`), odd column names quoted
  identifiers, and control, format and line-separator characters are spelled out, so a value
  worded as an instruction reads as data. Types are DuckDB's own names: `double`, `bigint`,
  `varchar`, `timestamptz`.
- A failed batch returns what ran: the statements before the one that failed keep their effects,
  and their results come back with the error.
- Clearer errors: a write to a loaded table says how to make your own copy; `COPY … TO` and
  `EXPORT` say there is no file output; a dynamic `PIVOT` answers once.
- Folders: each file skipped is named in the operator log. `dialect` serves the reference without
  its maintainers' notes. `exec-test` and `check` skip the checks a failed step makes meaningless,
  and name the conversion for common type mismatches.

### Install and release

- Release bundles for Linux x86_64 and aarch64 and macOS arm64 and x86_64: the binary with
  DuckDB's release library beside it, the licences and example data. A release is smoke-tested
  on machines that built nothing before it is published.
- `install.sh` checks the SHA-256 and installs without root; `BENCHY_DOWNLOAD_BASE` installs a
  bundle from a local directory.
- `benchy-mcp --version` names the DuckDB library; `doctor` names the file it was loaded from.
  Started by hand in a terminal, the server says it is waiting for an MCP client.

### Testing

- The weekly cargo-mutants run over `guard`, `lockdown`, `sandbox`, `admission` and `loader` is a
  gate: a mutant the suite does not catch fails it.

### Eval

- Measured again on main, 2026-10-11, before the public history began: deepseek-v4-flash-0731, gemini-3.8-flash,
  gpt-5-mini and claude-haiku-4.5 took 38–48% fewer steps with benchy than with a shell. Two
  tasks `describe` does not answer join the eleven, and the report shows the five it does answer
  apart. Transcripts also hide the checkout's path and the host name. The 2026-09-30 results are
  in `eval/results/archive/`.

## Initial development baseline

- **The bench.** `benchy-mcp <dir>` loads every `.csv`, `.tsv`, `.parquet`, `.json`, `.ndjson`
  and `.jsonl` in a folder as a table, before the agent's first call, and serves MCP on stdio.
  `--source name=path` names files explicitly.
- **Four MCP tools:** `describe`, `query` (DuckDB SQL, several statements per call), `get_result`
  (the last 32 results, by id and row offset) and `dialect` (the DuckDB reference on demand).
  There is no load tool.
- **Connectors.** `--config benchy.toml` declares exec connectors: any program that writes an
  Arrow stream to stdout, validated against the schema the operator declares.
  `benchy-mcp exec-test` checks one against the contract without a server or a model.
- **The host protocol.** `benchy-mcp host` lets an agent platform attach Arrow streams to a
  sealed session and relay the agent's SQL (`docs/HOST-PROTOCOL.md`).
- **Containment.** DuckDB's external access, extensions and configuration are locked and the
  lock is proven; a statement allowlist keeps it locked; on Linux, Landlock confines the engine
  and every file loader. Each session is its own set of processes and nothing names a path or a
  session. On macOS there is no OS sandbox, and benchy serves only with `--allow-unsandboxed`.
- **Ceilings.** A per-batch deadline, an engine memory budget, row, byte and cell caps on every
  result, and a supervisor that kills a wedged engine and rebuilds it without reading any source
  again.
