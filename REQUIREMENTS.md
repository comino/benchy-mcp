# Requirements

What benchy is held to. Each requirement names the tests that prove it, and
`scripts/check_requirements.py` fails CI when a named test does not exist, when a planned one
already does, or when a comment cites a test that is not there. That keeps this file from naming
evidence that is missing; whether a test proves its bullet is for review to judge. A test marked
*(planned)* is a known gap: the requirement is not met yet, and the bullet says what happens today.

A named test is only evidence if it would fail without the thing it guards. The guards that
matter most were mutation-checked when they were written: remove the guard, watch the test go
red.

## Two postures

- **Hosted.** A host runs one benchy per session, decides what data that session may see, and
  fetches it with the user's own credentials. The model never sees a credential, a path or
  another session. This is the posture the containment is built for.
- **Drop-in.** One person, one machine, their own data. The agent may already have a shell and
  can usually edit benchy's config itself, so the config is not a security boundary there. What
  still matters is that results stay bounded and that SQL cannot wander the filesystem.

## REQ-1 — Isolation

One session, one process, one workspace. Nothing the agent can pass names a workspace, a path or
a session: access is a capability the connection holds, never a name the model can say. That is
affordable only because a workspace is cheap — a process, a few megabytes, a few milliseconds.

- `cross_session_canary` — two live sessions over the same data; a canary in one session's scratch
  is invisible from the other through every catalog view.
- `schema_has_no_addressing` — no tool parameter, at any depth, can name a path, file, directory,
  workspace, session, database, URL or host.
- `neighbour_unaffected` — a runaway in one session leaves the other correct and responsive.
  Separate processes still share the CPU: the isolation is of state and failure, not throughput.
- `path_disclosure_names_only_our_own_private_workspace` — catalog functions that disclose a
  database path disclose only this session's private, temporary workspace.
- `benches_opened_together_never_share_a_directory` — a workspace's name does not come from the
  clock alone: macOS reads it to the microsecond, and two benches opened together collided.

Nothing outlives the session. Everything it keeps on disk — the store of what was loaded, each
engine's workspace, a loader's staging — is in one directory of its own, created fresh with mode
0700.

- `a_session_leaves_nothing_behind` — the directory is gone once the client closes the pipe.
- `a_stopped_server_leaves_nothing_behind` — and once the server is stopped by SIGTERM, which is
  how clients commonly stop one, and which runs no destructors.
- `a_killed_server_leaves_no_data_behind` — SIGKILL gives the server no chance to act. The engine
  notices on its own and exits, and the session's warden — a process that does nothing but wait
  for the server to go — removes the session's directory, all of it, which the confined engine
  could not: removing the directory itself needs a right in the shared temporary directory.
- `a_killed_server_takes_a_running_connector_with_it` — killed mid-load, the connector and the
  processes in its group are stopped by its loader, which notices on its own, and the scratch it
  was given is removed with the session — for good, though the connector writes into it until it
  is stopped: the warden looks again.
- `the_warden_removes_only_a_session_directory` — a binary on PATH can be run by hand, and the
  warden is forked from its server, so there is no `--warden` to run: given any directory, one
  that looks like a session included, the binary refuses the argument and leaves it.
- `only_a_session_directory_is_the_wardens_to_remove` — the warden checks it was handed a
  directory benchy could have made as a session before it removes anything.
- `the_warden_holds_nothing_of_the_servers` — forked from the server (not started from the binary
  again, which spent about 21 M instructions linking DuckDB), before the config is read, it
  zeroes the environment it was forked with: a credential the server was given is not in its
  environment nor in any page of its memory, and it has one thread and no descriptor but the
  server's pipe and `/dev/null`.
- `the_warden_is_forked_only_from_one_thread` — a fork copies only the calling thread, and a lock
  another one held stays held in the copy; starting the warden from a process with a second
  thread is refused.
- `a_signal_to_the_warden_is_not_the_server_going` — a signal interrupts its wait on the server's
  pipe; an interrupted read is not an end of file, and the session stays.
- `a_load_that_times_out_leaves_no_connector_and_no_scratch` — the same when a load outlasts
  `--load-timeout`: the supervisor stops the loader's process group, connector and all.
- `a_connector_starts_with_no_signal_blocked` — the server blocks SIGTERM, SIGINT and SIGHUP on
  every thread so that one can wait for them, and a mask survives fork and exec; a Python
  connector started with all three blocked and never saw the signal asking it to stop.
- `a_failed_rebuild_tells_the_agent_no_path` — a rebuild that fails says so; the reason, which
  names the operator's files, goes to the operator's log.

## REQ-2 — Sealed SQL

The query engine cannot reach the filesystem or the network. The **locked engine** is the
boundary: external access off, extension installing and loading off, configuration locked — and
proven at startup by trying to undo it. The statement allowlist's job is to stop that lock being
removed (`SET`, `RESET`, `INSTALL`, `LOAD`) and to refuse `ATTACH`, `COPY` and `EXPORT` —
though under the lock DuckDB already refuses to prepare the corpus's file forms of `COPY` and
`EXPORT`, before the allowlist sees them. The allowlist alone is not sufficient: file-reading
table functions are typed `SELECT`, and only the engine lock stops them.

- `nothing_in_the_corpus_reaches_outside_the_workspace` — every case in `corpus/escape.toml` runs,
  and none returns a canary planted outside the workspace.
- `a_denied_statement_says_what_is_allowed` — a refusal names what is allowed.
- `a_refused_statement_stops_the_batch_before_anything_runs` — nothing after a refusal runs.
- `file_readers_are_select_typed_so_layer_one_cannot_stop_them` — records, rather than hopes, that
  the engine lock is what stops file readers.
- `the_recorded_division_of_labour_still_holds` — each case's classification still matches the
  corpus, so an engine upgrade that re-types a statement fails a test instead of silently
  changing the posture.
- `the_corpus_records_why_the_engine_is_the_boundary` — measured, not recorded: every case runs on
  a freshly locked engine with no allowlist in front and fails or succeeds as the corpus says, and
  each of the five cases the allowlist lets through reaches the canary without the lock and is
  refused with it.
- `checked_per_statement_before_run` — each statement is checked again immediately before it
  runs, so one that depends on an earlier statement in the same batch — `CREATE TABLE t …;
  PREPARE p AS SELECT … FROM t` — is refused once it can be classified. A dependent `COPY` never
  gets that far: under the lock DuckDB will not prepare it.
- `a_dynamic_pivot_still_runs` — legitimate dependent SQL still runs: DuckDB splits a dynamic
  `PIVOT` into a `CREATE` plus a statement that depends on it.
- `a_pivot_into_a_table_runs` — `CREATE TABLE … AS PIVOT`, alone or wrapped in a SELECT, and an
  INSERT from a PIVOT run. DuckDB 1.5.5 runs each as a TRANSACTION, the pivot's `CREATE TYPE`,
  the statement and a TRANSACTION, and the first was refused as a `TRANSACTION` the agent never
  wrote. Neither TRANSACTION of that shape runs; one the agent writes is still refused, around
  its own CREATE (undone), around a PIVOT (which DuckDB does not wrap), alone or left open.
  Over a table made earlier in the same call the pivot's type does not bind up front, and the
  shape cannot be told from `COPY FROM DATABASE`'s: refused, and the refusal says to make that
  table in a call of its own first.
- `each_statement_kind_is_named_as_duckdb_types_it` — a refusal names the kind it refused, and
  the corpus records each case's kind, so every kind the classifier can meet under the lock is
  pinned. `MERGE` and `UPDATE EXTENSIONS` were both refused as "UNKNOWN". So are the typings that
  leave the guard's other names unreachable: `ANALYZE` is typed VACUUM, `SET VARIABLE` SET and
  `COPY FROM DATABASE` TRANSACTION, and `COPY`, `EXPORT` and `EXECUTE` never bind.
- `merge_is_ordinary_dml` — `MERGE` works on the agent's own tables like `INSERT` and `UPDATE`,
  and its source reads files no more than theirs can.
- `explain_cannot_smuggle_a_refused_statement` — `EXPLAIN ANALYZE` runs what it explains and is
  typed as EXPLAIN, so EXPLAIN is not allowed: `EXPLAIN ANALYZE SET VARIABLE` ran before.
- `the_workspace_file_cannot_be_read_as_a_file` — turning external access off puts an attached
  database's path on DuckDB's allowed list, so `read_blob` read the workspace file's raw bytes.
  The file is unlinked once attached; DuckDB reads through its handle.
- `nothing_is_left_to_read_in_the_attach_directory` — the one directory DuckDB may still open
  after the lock is the one tables are attached from, and an allowed directory is open to every
  file reader, not only `ATTACH`. Each file is unlinked as soon as it is attached, whatever
  happened, so there is never anything in it to read.
- `an_attached_file_brings_its_table_and_nothing_else` — an attached catalog goes on the agent's
  search path, and DuckDB resolves a function there before its own: a macro named `lower` in the
  file stood in for `lower`, measured. A file holding anything but its one table — a macro, a
  view, a sequence, a type, a schema, an index, a second table — is refused before the agent can
  see it.
- `a_function_in_one_file_of_a_batch_refuses_that_file_alone` — the tables a session starts with
  are attached in one request, and the engine lists DuckDB's functions once for all of them; the
  file that brings a macro is refused before any catalog of the batch reaches the search path,
  and the files around it are attached.
- `anything_else_in_one_file_of_a_batch_refuses_that_file_alone` — the rest of the check is one
  listing of each kind for the whole batch too, and each catalog is still judged on its own: the
  file with a view, a second table, a sequence, a type, a schema or an index, or without its
  table under its name, is refused, and the files around it come with their own rows and columns.

## REQ-3 — Sealed process

On Linux, Landlock confines every process that parses data. The engine starts sealed and
empty, loads nothing, and may read only its own workspace and delete within its session's
directory; each file is parsed by a loader of its own, which may read its one input and write
only its staging directory. A built-in source's loader (ClickHouse, HTTP) may write only its staging
directory, read only the resolver's files, and, from Linux 6.7, connect only to its source's
port. So neither a
compromised engine nor a parser subverted by its input can read other files, or truncate any
outside a loader's own staging directory. A connector's
loader is not confined: a connector reaches whatever it fetches from. macOS has no equivalent here: there, containment is the SQL layer alone, and
benchy refuses to serve until the operator says so with `--allow-unsandboxed`.

The TCP rules need Landlock ABI 4, Linux 6.7. On an older kernel every ruleset is enforced
partially: the files are confined as above, and the engine and every confined loader may open a
TCP connection anywhere. benchy serves there, since the filesystem boundary holds and DuckDB's
own network access is off (REQ-2), and says so: `hello` reports the posture as `partial` with
`network` false, the operator's log says the network is not confined, and a server started with
`--require-full-sandbox` does not start.

- `a_thread_already_running_is_never_confined_as_if_it_were_not` — Landlock confines only the
  threads started after it, so the engine and every file loader confine themselves as their only
  thread and start the rest from the confined one. With another thread running, the check
  refuses, and the process with it.
- `a_thread_that_stays_past_the_grace_is_refused` — the check waits a grace for a thread that is
  finishing, and no longer.
- `a_thread_that_finishes_within_the_grace_is_waited_for` — one that goes within the grace does
  not stop the process confining itself.
- `a_process_already_alone_is_confined_at_once` — and a process with nothing to wait for does not
  wait: every engine and every file loader asks.
- `a_connector_that_never_lets_go_cannot_stall_or_unconfine_the_engine` — a connector's threads
  live and die in its loader. One stuck writing to a connector whose stdin a stray process holds
  was once still running when the engine sealed; now it neither stalls the load nor leaves a
  thread in the engine.
- `a_loaders_output_is_checked_before_it_is_kept` — the supervisor that moves a loader's output
  into the store is not confined, so it takes nothing on a loader's word: the table's name is the
  job's, and the file is the one in the staging directory the job named — a plain file with no
  other name, its write-ahead log merged, never a link to one elsewhere.
- `doctor_says_in_json_what_each_mechanism_enforces` — `benchy-mcp doctor --probe --json` prints
  one JSON object and nothing else, for a host to check at container start: an overall `verdict`
  (`enforced`, `partial` or `not_enforced`), the exit status agreeing with it, the Landlock ABI, and each
  mechanism — the engine, a file loader, a network loader — with its posture, whether it is
  enforced, and every check it made: what the child could do, and what it must (`null` where the
  right is not Landlock's to withhold on this kernel). Where the restriction cannot be applied it
  still prints JSON, saying so.
- `the_doctor_json_has_the_documented_keys` — that JSON's keys, at the top, in each mechanism and
  in each check, are exactly the ones the README's table lists, so a host can rely on them and a
  key added or dropped fails until the table says so.
- `doctor_without_landlock_says_not_enforced` — on a kernel without Landlock (ABI 0), or below
  the minimum ABI, every check is above what the kernel can refuse and none counts, so nothing
  checked once passed as `enforced`. Now that verdict is `not_enforced`, in prose and JSON alike,
  with a `reason`, and the exit status is non-zero; tested on the verdict with the ABI given.
  Each entry of `mechanisms` says the same: its `enforced` is true only where at least one of its
  checks counted and every one that counted held, so at ABI 0 none is, where each once said
  `true` beside the verdict (a host's report), and one failing check marks its own entry alone.
- `doctor_probes_the_restriction_the_server_applies` — `benchy-mcp doctor --probe` has one child
  apply the engine's own restriction to a session laid out like the server's and another a file
  loader's, then checks each right they grant and withhold. The engine: read in the workspace,
  not outside it, no new files, not the store's contents, delete within the session. A loader:
  read its input and nothing else, write its staging directory and nowhere else, not the store's
  contents. Neither may open a TCP connection. A third applies a network loader's: read the
  resolver's files and resolve a name, write its staging directory and nowhere else, connect to
  its source's port and no other, listen on none. Where the kernel can refuse truncate(2) (ABI 3,
  Linux 6.2), each is also checked for it: the engine truncates nothing, a loader only what is in
  its staging directory. Where a restriction cannot be applied, it says so and fails.
- `a_confined_process_cannot_truncate_what_it_was_not_granted` — every ruleset handles the
  filesystem rights of the kernel's own ABI, up to V5 (`Refer`, `Truncate`, `IoctlDev`), not
  V1's alone. Handling V1's left truncate(2) by path unhandled, so a confined engine or loader
  could empty any file this user can write. Now the engine can truncate nothing, and a file or
  network loader only what is beneath its staging directory: not its input, not the workspace,
  nothing outside. Measured on kernel 6.8 (ABI 4): every posture still fully enforced.
- `every_ruleset_builds_for_every_landlock_abi` — the handled rights follow the kernel's ABI, and
  landlock refuses a rule that grants a right its ruleset does not handle, so a grant of
  `Truncate` once left every loader unconfined, and refusing to run, on kernels before 6.2
  (ABI 1 and 2). Each grant is now cut to what its ruleset handles, and the engine's, a file
  loader's and a network loader's rulesets are built and filled for every ABI from V1 to V5,
  whatever kernel the test runs on.
- `the_truncate_probes_count_only_where_landlock_can_refuse_it` — below ABI 3 the doctor reports
  the truncate probes and does not count them, as it does TCP below ABI 4.
- `a_network_loader_reaches_only_its_source_and_its_staging` — a built-in source's loader
  (ClickHouse; HTTP's is `an_http_loader_is_confined_before_its_first_request`) runs only
  benchy's code but parses a remote server's bytes, so it is confined too, before its first request: read-write its staging directory, read the
  resolver's files (`/etc/resolv.conf`, `/etc/hosts`, `/etc/nsswitch.conf`, `/etc/host.conf`,
  `/etc/gai.conf`), and, where the kernel has Landlock's network rights (6.7+), TCP-connect only
  to its source's port and listen on none. Reached by name, it still loads; where it cannot be
  confined it refuses without `--allow-unsandboxed`; and `doctor --probe` measures each right it
  grants and withholds.
- `a_missing_sandbox_is_refused_without_the_flag` — where the restriction cannot be applied, the
  engine refuses to serve unless the server was started with `--allow-unsandboxed`, and says what
  is missing. Tested for real: Landlock allows sixteen stacked layers, and the test fills them.
- `a_loader_that_cannot_be_confined_refuses_without_the_flag` — and so does a file loader, whose
  parser reads whatever the file holds; the same flag is the only way past it.
- `the_tcp_probe_counts_only_where_landlock_can_refuse_it` — Landlock refuses TCP from ABI 4
  (Linux 6.7). On an older kernel the filesystem is confined all the same and the network is not
  Landlock's to refuse, and the doctor says so rather than failing; and only `EACCES` is Landlock's
  refusal — `EPERM` is somebody else's.
- `a_kernel_without_tcp_rules_is_reported_as_partial` — below ABI 4 the posture is `partial`,
  reported with the filesystem confined, the network not, and why; only `full` at ABI 4 or later
  reports `network` true, and only it passes `--require-full-sandbox`. Tested on the posture with
  the ABI given: no kernel before 6.7 is to be had here.
- `doctor_without_tcp_rules_says_partial` — and `doctor --probe` says the same of that kernel:
  where every check that counts held and the TCP checks could not, its verdict is `partial` and
  PARTIAL in prose, with `sandbox_posture`'s reason, where it said `enforced` and VERIFIED with
  the network unconfined. Its exit status is 0, as for `enforced`: benchy serves under it without
  a flag, since the filesystem boundary holds, and only `not_enforced`, where it refuses to, is
  non-zero. Tested on the verdict with the ABI given.
- `hello_says_how_the_engine_is_confined` — `hello`'s `sandbox_posture` says how Landlock
  confines the engine in fixed words and booleans (`posture`, `landlock_abi`, `filesystem`,
  `network`, `reason`), beside the prose of `sandbox`, which a host once matched word for word:
  `full` here, and `unavailable` with both false where Landlock's layers are used up and the
  operator served anyway.
- `a_session_that_must_be_fully_sandboxed_refuses_to_start_otherwise` — `--require-full-sandbox`
  starts a server or a host session only where the engine's posture is `full` with TCP rules, and
  wins over `--allow-unsandboxed`. Short of that it says why and exits, a server before any source
  loads, since a network loader would otherwise reach its source under the posture refused.

## REQ-4 — Sources

Data is on the bench before the agent's first call, and there is no load tool: loading is not the
agent's job, so it costs the agent no steps. Three tiers exist — files, built-in
sources (ClickHouse, REQ-16; HTTP, REQ-17) and exec connectors.

- `a_directory_becomes_a_bench` — every supported file in a directory becomes a table named after
  it.
- `one_failed_source_does_not_cost_the_others` — a session starts with what loaded. A table that
  could not be loaded is listed in `describe` as not available, by name and nothing more, and a
  query naming it is told it is not available this session rather than that there is no such
  table; the operator's log has the reason. Only when no source loads does the server exit,
  non-zero.
- `json_and_ndjson_load_too` — JSON and NDJSON load alongside CSV, TSV and Parquet.
- `an_unsupported_file_says_what_is_supported` — an unreadable format lists the readable ones,
  whether it arrived in a directory or named with `--source`.
- `a_name_collision_is_refused_with_both_paths` — two files that would make the same table are
  refused, naming both.
- `table_names_are_plain_identifiers` — a file name becomes a table name the agent can type, by
  the rules the README states.
- `a_skipped_file_is_named_in_the_operator_log` — a folder is read at its top level; each
  subdirectory, hidden file and file of another kind is named in the operator's log, with the
  reason, and nothing of it reaches the agent.
- `a_hidden_file_in_a_directory_is_not_a_table` — a folder's dotfiles, any name starting with
  `.`, are skipped: `.secret.csv` is no table, and macOS's AppleDouble `._meters.csv` beside
  `meters.csv` does not stop startup as a second `meters`.
- `a_hidden_file_named_explicitly_still_loads` — a dotfile named with `--source` or in a config
  loads: the operator chose it.
- `the_help_example_names_the_binary_by_absolute_path` — `--help`'s JSON example gives the
  command as an absolute path, since desktop apps do not read `PATH`.
- `the_catalogue_is_bounded` — the catalogue is in the agent's context on every turn, so it is
  capped and deterministically ordered.
- `the_catalogue_carries_no_path` — nor does it disclose where anything came from.
- `an_exec_connector_puts_rows_on_the_bench` — any program that writes an Arrow stream to stdout
  is a source.
- `an_exec_table_appears_in_the_catalogue` — a connector's table is listed like any other.
- `narrowing_holds_on_the_served_path` — a server narrows in a loader process; a narrowing that
  stopped reaching it would leave every row on the bench while the in-process tests stayed green.
- `config_rejects_what_it_cannot_apply` — unknown keys, missing schemas, a column declared twice
  (DuckDB does not tell `id` from `ID`), unresolvable variables and secrets outside `env` stop
  startup. Nothing is warned about and dropped.
- `a_key_that_does_nothing_is_refused` — nor ignored: a key that cannot apply to its kind of
  source, such as `max_rows` on a file, stops startup instead of reading as a cap.
- `a_file_source_may_be_declared` — a file source takes `[[source.schema]]`, and a CSV's or TSV's
  `delimiter`, as the operator's choice (R3-5), checked at startup: a delimiter on another kind
  of file or longer than a character, a column declared twice or outside the vocabulary, a
  stated shape naming an undeclared column, and an unknown key stop it. Through its loader the
  file loads as declared, and only the declared columns.
- `a_declared_file_loads_as_the_sniffed_file_does` — a declared CSV in its stated dialect, its
  header in another order and with a column more, a TSV, an NDJSON file read with its `columns`
  and a Parquet file pinned each make the table the sniffed read makes, in a zone other than UTC.
- `a_declared_file_that_does_not_match_fails_naming_why` — a CSV split on another delimiter
  names `delimiter`; a missing column, a value not of its type, a Parquet column of another type
  and a null in a `nullable = false` column each name the column; none leaves a table.
- `a_rebuild_reads_nothing_again` — a bench rebuilt after a hard kill attaches what the session
  stored: neither the config nor any source is read again, whatever has happened to them
  since.
- `loading_never_downloads_an_extension` — a reader the build lacks fails loudly at load time.
  DuckDB's default of installing a known extension on first use is off before anything loads.
- `every_advertised_format_loads_in_the_shipped_build` — every format the README lists loads in
  the build a stranger gets, with the library `cargo build` fetches.
- `a_host_attaches_a_table_during_a_session` — a host can add a dataset at any time, read-only,
  without restarting the session (REQ-13).
- `an_attached_table_joins_a_running_session` — a table attached after the seal is queryable at
  once, beside the agent's own.
- `replacing_an_attachment_keeps_the_agents_tables` — attaching a name again replaces the data,
  keeps what the agent made from the old data, and detaches the old catalog.
- `replacing_an_attachment_drops_the_results_that_could_hold_its_rows` — replacing is how a host
  takes rows away, so a result read before cannot be paged again afterwards.
- `a_failed_attach_changes_nothing` — a file without the table it is attached as, or not a
  database at all, is refused, and the bench is as it was.

## REQ-5 — Credentials stay out of the engine

The process that runs agent SQL holds data, never credentials. Connectors hold the credentials
and do the I/O, each run by a loader process of its own, and each gets only the environment its
operator listed.

- `the_connector_sees_only_its_own_environment` — a connector's environment is constructed, not
  inherited: its own variables, no other source's, and no `HOME` unless listed.
- `entity_ids_travel_in_the_document_not_in_argv` — narrowing values, which a host may take from
  its own users, reach a connector in its input document, never as arguments.
- `a_writable_connector_is_refused` — a connector anyone else can write is a credential handover.
- `a_personal_group_may_write_the_connector` — anyone else means another owner, everyone, or a
  shared group. A personal group — the user's own, with nobody else in it — is not anyone else,
  and under its default umask of 0002 a fresh clone makes every connector group-writable.
- `the_personal_group_is_recognised` — that lookup agrees with `id` and `getent`.
- `credentials_never_cross` — the server resolves `${VAR}` and hands the result to the loader
  that runs that source's connector, and to no other process. The engine never holds it: not in
  its environment, not in its arguments, nowhere in its memory — scanned, next to a string the
  scan must find.
- `the_engine_inherits_no_environment_and_no_descriptor` — whatever a client passed the server,
  a credential in its environment or an open file, does not reach the engine: its environment is
  built, and empty, and every descriptor past stdio is closed before it starts.

Out of scope: other processes of the same user. They can read a connector's environment through
`/proc`, as they can read the operator's config; benchy does not defend against its own user.

## REQ-6 — Connector output is untrusted

A connector is a third party, and its output heads for the model. It is validated against what
the operator declared, never trusted. A declared column accepts what it holds exactly — a
narrower integer, `float32` for `float64`, a timestamp at any precision, a string in any layout —
and never a type that could change a value (REQ-15).

- `a_surprise_column_is_refused_rather_than_dropped` — the schema is the operator's declaration,
  not the connector's; an extra column is refused, never dropped.
- `a_connector_that_breaks_its_declaration_is_refused_when_served` — the same through the loader
  process a server uses: startup stops, naming the column.
- `a_dictionary_encoded_column_is_unpacked_within_the_byte_bound` — pandas sends a categorical
  as a dictionary, ClickHouse a LowCardinality column when asked to. It is unpacked into its
  values; unpacked, though, a dictionary has no size bound the stream's size implies, so its
  unpacked size counts against `max_bytes` as the stream is read, and passing it refuses the
  load.
- `a_view_column_is_unpacked_within_the_byte_bound` — so is a string or binary view, which is 16
  bytes on the wire whatever it points at: any number of views may name the same block, and 2,000
  views of one 64 KiB block, a stream of 114 KB, loaded as 131 MB under a `max_bytes` of 1 MiB.
  Each view counts the length it states.
- `a_large_dictionary_costs_each_batch_only_its_keys` — a dictionary is sent once and every batch
  may pick from it, so what a key picks is read from the dictionary's offsets: a batch costs its
  rows, not the dictionary. Re-reading every value per batch took 15.7 s of CPU for a 5.2 MB
  stream of one-row batches over a million-entry dictionary.
- `a_compressed_stream_is_read_within_the_byte_bound` — ClickHouse sends LZ4-compressed bodies by
  default, and LZ4 halves what a remote fetch moves, so they are read. What bounds memory is what a
  stream decodes to, so that is what `max_bytes` bounds: a stream that fits on the wire and not
  decoded is refused. zstd is refused, naming the setting that turns it off.
- `a_compressed_buffer_counts_what_it_decodes_to_and_may_not_lie` — a buffer states what it
  decompresses to, and arrow allocates that and reads past it before checking. The statement is
  checked first, by decompressing the buffer and stopping one byte past it, so a bomb costs what
  it claimed and no more.
- `a_compressed_message_reaches_arrow_decoded_once` — what the check decoded is what arrow gets:
  each buffer stored, prefixed `-1`, and its place and the body's length rewritten in the
  message's metadata, so LZ4 runs once, not once to check and once in arrow.
- `a_claim_past_what_is_left_is_refused_before_it_is_decoded` — and a claim past what the stream
  has left is refused on the claim, without decompressing anything to check it.
- `a_hostile_schema_is_refused_not_a_panic` — arrow's schema conversion panics on a type it
  cannot build, such as an integer 7 bits wide; that is refused as a bad stream, where it used to
  crash `exec-test`.
- `our_refusal_is_not_hidden_by_the_connector_dying_of_it` — when benchy refuses a stream before
  the connector has finished writing it, it stops reading, and the connector dies of the broken
  pipe — SIGPIPE, curl's exit 23, or an error event about it. Benchy's refusal is the verdict;
  what the connector did next goes to the operator's log beside it.
- `a_not_null_column_carrying_nulls_is_refused` — nullability is checked in the data, not taken
  from the Arrow field flag.
- `a_stream_without_its_end_marker_is_refused` — a stream cut at a message boundary parses as a
  valid shorter one; only the end-of-stream marker tells finished from stopped.
- `a_stream_that_fails_after_its_rows_leaves_no_table` — a stream's batches go into the table as
  they arrive, and the verdict on it comes after the last: its end-of-stream marker, the
  connector's exit status, a late `error` event. The table is written in one transaction that
  commits only on that verdict, so a stream refused once its rows are in leaves no table, and the
  next load goes on as if it had not been tried.
- `a_stream_must_end_at_its_end_marker` — and the marker ends the input: a second stream written
  after the first, as a connector restarting its writer per page does, was dropped without a word.
  After the marker one byte more refuses the load, whether it comes before the pipe closes or
  before the connector exits — a helper left holding the pipe is not waited for
  (`a_finished_connector_leaves_nothing_running`). A stream of exactly `max_bytes` still loads.
- `an_empty_stream_is_an_empty_table_but_no_stream_at_all_is_a_failure` — no rows is a complete
  stream with zero batches; empty stdout is a failure.
- `the_row_cap_refuses_rather_than_truncating` — over a cap the load is refused, never shortened.
- `narrowing_is_reapplied_even_when_the_connector_promised_to_do_it` — entity narrowing is
  re-applied after ingest, so a buggy connector costs bytes rather than answers.
- `time_narrowing_is_reapplied_too` — and so is the time window, half-open: its start is in, its
  end is out, and a row with no timestamp is out.
- `a_relative_window_is_sent_as_rfc3339` — `last = "30d"` reaches the connector as RFC 3339,
  with a `Z`, as the protocol promises.
- `a_row_with_no_entity_is_outside_the_narrowing` — a row that cannot be shown to be inside is
  outside.
- `narrowed_rows_never_reach_the_file` — an excluded row is never written: each batch is filtered
  as it is inserted, so only admitted rows reach the workspace file. Deleting them afterwards left
  them in the file, readable as bytes and visible in `pragma_storage_info`'s statistics.
- `a_connector_that_hangs_does_not_hang_the_host` — `describe` is bounded by its timeout.
- `a_connector_that_hangs_mid_fetch_is_bounded_too` — and so is `fetch`, by its deadline.
- `noise_on_stderr_is_not_an_error` — stderr lines that are not control events are log text.
- `a_fetch_is_bounded_by_one_deadline_not_two` — after the stream ends, the wait for the
  connector to exit gets what is left of the deadline, not a new one.
- `a_finished_connector_leaves_nothing_running` — once a fetch ends, the connector's process
  group is stopped, helpers it left behind included.
- `an_error_after_a_flood_of_logging_still_counts` — however much a connector logs first, its
  first error event is the verdict, and the operator log keeps the end of stderr as well as the
  start.
- `a_connectors_stderr_line_is_bounded` — a line is kept to its first 64 KiB and the rest of it
  read and dropped: a connector that never wrote a newline grew one string without bound. Bytes
  that are not UTF-8 are log text, and the lines after them still arrive.
- `a_connector_error_reaches_the_operator_not_the_agent` — a connector's message, which may carry
  URLs and account ids, and its hint go to the operator's log, with the code. The agent is told
  only that the table is not available.
- `an_unclassified_code_does_not_reach_the_sentence_lookup` — an unknown code is taken as
  `E_INTERNAL`, not echoed.
- `a_connector_must_tolerate_keys_it_has_never_seen` — every input document carries a reserved
  key, so a connector that breaks on an unknown key breaks immediately.
- `exec_test_catches_the_mistakes_it_exists_to_catch` — `benchy-mcp exec-test` names the fix for
  each failure it reports.
- `exec_test_names_a_fix_for_every_failure` — every failure code the host can produce comes with
  advice; the test finds the codes in the source, so a new one without advice fails it.
- `exec_test_passes_the_connector_we_ship` — the shipped example passes its own checker.
- `exec_test_skips_what_a_failed_fetch_makes_meaningless` — one mismatch is one failure: the
  checks that need a fetch say they were skipped, instead of failing with unrelated advice.
- `a_type_mismatch_names_the_conversion` — an integer or text where a timestamp is declared, a
  zone-less timestamp for an instant, an unsigned 64-bit or a float for an integer: each names
  the producer-side fix, ClickHouse's `toDateTime64(col, 6, 'UTC')` among them.
- `an_oversized_request_starts_no_connector` — a request over the 1 MiB document cap is refused
  before any connector starts, for `estimate` and `fetch` alike.
- `a_hostile_entity_list_starts_no_connector` — so are entity ids over the bounds: more than
  1024, longer than 512 bytes, or carrying a NUL or a control character.

## REQ-7 — Ceilings

Every query batch runs under a wall deadline, DuckDB's memory budget and a ceiling on the
engine's resident memory (REQ-8), and what comes back is capped. Capacity is measured, not claimed: `examples/capacity.py` produces `docs/CAPACITY.md` up to its
ClickHouse section, whose table `examples/capacity_clickhouse.py` prints.

- `a_deadline_of_zero_is_refused_at_startup` — at zero every query would fail at once, and the
  caps a host reads from `hello` could not be handed back.
- `ceilings_enforced` — the row cap, the byte cap, the memory budget and the deadline are each
  exceeded in turn; each gives a named error or a stated truncation, and the server answers
  afterwards.
- `a_deadline_holds_between_statements` — the watchdog interrupted once, and DuckDB clears a
  pending interrupt when the next statement starts, so a deadline that fell between two
  statements was lost and the batch ran on. No statement now starts past the deadline, and the
  interrupt repeats until the batch ends.
- `a_batch_that_finishes_is_not_killed_by_its_own_deadline` — finishing just in time counts. A
  batch is judged by its statements, not by whether the watchdog fired; sixty batches that end
  around their deadline each either finished, leaving their table, or were stopped, leaving
  none.
- `a_trivial_query_does_not_pay_for_the_watchdog` — the deadline costs a trivial query nothing:
  in 60 pairs, each `SELECT 1` against a statement refused before the watchdog starts, the lower
  quartile of what `SELECT 1` cost more stays under 5 ms. The load is in both halves of a pair,
  so a busy machine does not fail it (a fixed 10 ms bound failed 11 runs of 20 with 16 busy
  loops beside it on four cores; this, none), and the old 20 ms tick still does (20.3 ms).
- `running_out_of_memory_says_how_to_use_less` — the memory error names the fix.
- `a_large_stream_is_not_held_in_memory_while_it_loads` — a loader does not hold a stream whole.
  Collected into a list of batches and then copied into an in-memory staging table, a 112 MB
  Arrow stream grew its loader by 321 MB; each batch now goes into the table as it arrives, and
  the same stream grows it by about 68 MB, DuckDB's own buffers. The test bounds the loader's peak
  over a one-row load's by the stream's size.
- `a_large_csv_is_not_held_in_memory_while_it_is_staged` — a loader does not hold a CSV's stage
  whole. It is held to 256 MiB and spills past it to the staging directory: 6,000,000 and
  12,000,000 rows of a CSV DuckDB holds at 6.6 times its size take the loader to about 400 MB
  both, where held whole they took it to 740 and 1,372 MB. The test bounds the larger file's
  peak over the smaller's by how much the CSV grew, and finds nothing of the spill left behind.
- `a_wide_csv_is_not_held_in_memory_while_it_is_staged` — a loader does not hold a wide CSV
  whole either. Each thread filling a stage pins a block of every column, so a stage of wide rows
  on every thread ran out of memory under the cap and was staged again without it: a 676 MB CSV
  of 300 columns took its loader to 2,090 MB. A file whose first line is that wide is now read on
  one thread, straight into its table under the cap: 200,000 and 600,000 rows of 200 columns
  take the loader to 328–345 and 361–395 MB, where staged on every thread and then without the
  cap 200,000 and 400,000 rows took it to 1,007 and 1,371 MB. The test bounds the larger file's
  peak over the smaller's by how much the CSV grew.
- `a_file_is_read_on_one_thread_where_its_first_line_is_wide` — which files are read on one
  thread is judged from their first 64 KiB, not by DuckDB's sniffer: two blocks of 256 KiB a
  column and three times a vector of 2,048 of its lines, for each thread, past half the cap. The
  harness's three-column CSV and NDJSON stay on every thread, on 64 threads too; 100 columns of
  a CSV, or a JSON object of 2,000 keys, go to one.
- `a_wide_json_file_is_read_on_one_thread_within_its_vector` — a JSON file of wide objects is
  staged on one thread at once, held to the cap and what DuckDB's JSON reader holds of a vector
  of its lines; held to the cap alone, 2,100 objects of 2,000 keys ran out of memory on one
  thread too. A JSON file is staged rather than read straight in, whose `DESCRIBE` sniffs it
  again: 8 s for 5,000 such objects. Its write is held to the cap alone, and its checkpoint too.
  Measured: 5,000 objects of 2,000 keys (163 MB) took the loader to 549 MB, where on every thread
  and then without the cap they took it to 635–648 MB; 10,000 of one-digit keys to 554–578 MB
  against 1,125 MB, and 20,000 to 614 MB. It grows with the rows up to a row group of the
  table, whose 40,000,000 `MAP` entries at 20,000 rows DuckDB writes partly outside its limit
  (DuckDB 1.5.5 alone, 568–603 MB), so it is bounded by the width of a row, not the file's length.
- `a_table_read_on_one_thread_or_staged_is_the_same_file` — staged under the cap or without
  it, read straight in on one thread, or staged on one thread for a narrowed body, the table is
  the same file (REQ-15 `the_physical_layout_says_nothing_of_where_data_came_from`); a stage that
  runs out of memory is read on one thread, and one that runs out there too, without the cap, as
  every stage that ran out was before, rather than failing; and the bench's memory limit and
  threads are what they were.
- `a_memory_limit_put_back_reads_as_it_did` — DuckDB shows its memory limit cut to one decimal,
  and that text set again does not always read back the same (`12.7 GiB` reads `12.6 GiB` on
  1.5.5), so a limit put back from it shrank with each staged table on a machine whose default
  was `12.7 GiB`. Put back, every limit reads as it did, again and again, and the buffer pool has
  it too. `RESET memory_limit` is not used: on 1.5.5 it leaves the buffer pool at the cap.

## REQ-8 — Survivable failure

A runaway query loses the query, not the bench. The in-process interrupt stops execution; query
*planning* never checks it, so the engine is a separate process that the server kills and
rebuilds past a hard deadline, or past a ceiling on its resident memory. DuckDB's memory budget
does not bound planning either, and in a container one engine's growth is every session's OOM
kill. So is one loader's: each is read the same way while it loads, with the connector it runs.

- `a_runaway_batch_loses_the_batch_not_the_bench` — a runaway is interrupted at its deadline and
  the agent's tables survive.
- `planner_blowup_killed` — a query that takes seconds just to plan is stopped by killing the
  engine; the bench is rebuilt and the lost scratch is stated.
- `an_engine_past_its_memory_ceiling_is_killed_even_while_planning` — a query whose planning
  grows the engine past `--memory-ceiling` (nested `MAP`s, which took an engine with a 64 MB
  budget to 229 MB) is killed within moments, long before its deadline: `E_REBUILT`, saying it
  was the memory ceiling and how to use less, the data back and the scratch gone. The server
  reads the engine's resident memory every 50 ms while it answers a request, and never between
  requests: 0 ms of CPU over 10 s idle, 20 ms over a 10 s query.
- `an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once` — the server reads the engine's
  resident memory once more as it answers, and an engine past its ceiling then is killed and
  rebuilt there, the request answered `E_REBUILT`, rather than left past it, idle, for the next
  request to be killed for. Requests quicker than the 50 ms watch kept 5 MB tables each, and the
  27th left an idle engine at 194.3 MB against a 192 MB ceiling. Between requests the engine runs
  nothing, so nothing is watched while it idles: the read costs 5 µs a request. There is no lower
  "idle" ceiling: what an engine keeps between requests is its tables, inside DuckDB's budget,
  and memory its allocator gives back by itself (a hash aggregate at a 512 MB budget left 274 MB,
  120 MB a second later); what DuckDB's budget does not count, an ENUM's values, the agent
  cannot keep (REQ-9).
- `a_memory_ceiling_the_engine_cannot_live_under_is_refused_at_startup` — a ceiling below
  `--memory`, where every query DuckDB would refuse as out of memory would kill the engine
  instead, and a size that does not parse, for `--memory` too: refused at startup, by the server
  and by `host`. Unset, the ceiling is `--memory` and half again, and at least 256 MiB over it:
  at a 1 GB budget, legitimate queries were resident at up to 1,260 MB.
- `a_memory_ceiling_that_cannot_be_enforced_is_not_reported` — the ceiling is enforced by reading
  the engine's resident memory from `/proc`, so on Linux only. Elsewhere `hello`'s
  `limits.memory_ceiling` is `null`, `unenforced.memory_ceiling` says why, and `doctor` says it
  is not enforced, rather than a number nothing applies (an engine with a 128 MB ceiling reached
  243 MB on macOS). On Linux the reply is unchanged. Tested with the platform given.
- `a_session_that_must_have_memory_ceilings_refuses_to_start_without_them` — with
  `--require-memory-ceilings` a server or a host session starts only where both the engine's and
  the loaders' memory ceilings are enforced: on Linux it changes nothing, and elsewhere it says why
  and exits, rather than leave a host to check `hello` for numbers.
  `ceilings_that_cannot_be_enforced_are_refused_when_required` holds the check to each ceiling
  unenforced, with the platform given.
- `a_loader_past_its_memory_ceiling_is_killed_and_attaches_nothing` — a loader is read from
  outside every 50 ms while it runs, and past `--loader-memory-ceiling` (default 1 GB) it is
  killed: the table is not loaded, nothing of it is attached, the operator's log says it was the
  loader memory ceiling, what was held and what to do, and the other sources load as before. The
  agent hears only that the table is unavailable. Unbounded, a 676 MB CSV of 300 columns (too
  wide for its stage on every thread) took its loader to 2,090 MB, and now reads on one thread
  within 338–343 MB (`a_wide_csv_is_not_held_in_memory_while_it_is_staged`); the harness's
  10M-row loads peak at 94–410 MB, and the watch costs them nothing measurable
  (docs/PERFORMANCE.md).
- `a_connectors_memory_counts_against_its_loaders_ceiling` — an exec connector, and every process
  it starts, counts with its loader: a connector that held 100 MB itself, and one whose child held
  200 MB, each passed a 150 MB ceiling and was killed with its whole process group; before, a
  connector that held 800 MB was bounded by nothing. Found through the kernel's list of each
  thread's children (15 ms of the server's CPU over a 3.3 s load); where the kernel has no such list, through every
  process's `stat`, once a second.
- `a_loader_memory_ceiling_is_set_in_one_place_and_must_be_a_size` — `--loader-memory-ceiling` or
  `[loader] memory_ceiling`, never both, and a size that does not parse stops startup.
- `a_loader_memory_ceiling_that_cannot_be_enforced_is_not_reported` — as the engine's: off Linux
  `hello` (from protocol 6) reports `limits.loader_memory_ceiling` as `null` with
  `unenforced.loader_memory_ceiling` saying why, and `doctor` says it is not enforced.
- `a_load_that_keeps_failing_is_not_retried_forever` — a load that cannot finish fails once,
  after `--load-timeout`, says why, and the server exits instead of loading again.
- `an_engine_that_dies_at_every_attach_is_not_rebuilt_forever` — a rebuild that loses its engine
  counts as a failed rebuild. It used to recover by rebuilding again, from inside the rebuild,
  where the count of failures never rose. And a lost engine is noticed when it goes, not at the
  attach's timeout.
- `result_ids_never_outlive_their_workspace` — after a rebuild, ids from the lost workspace stop
  resolving, and no new result reuses one.
- `result_ids_continue_across_a_rebuild` — through the host protocol: the engine a rebuild
  starts numbers on from the last id the session issued, so several results after it reuse none
  of the ids before it, and each of those answers `E_NO_RESULT` to `page`.
- `the_server_answers_while_a_query_runs` — a query that takes its whole deadline does not stall
  the server: a ping sent meanwhile is answered at once.

## REQ-9 — Scratch is free; loaded data is not

- `loaded_is_readonly` — enforced by DuckDB, since loaded data is attached read-only, not by
  inspecting statement text, which a computed name would get past.
- `the_agent_cannot_change_a_hosts_table` — nor the catalog it lives in: the agent can neither
  write to it, detach it, attach over its name, nor create in it.
- `scratch_is_free` — the agent's own tables, views and macros persist between calls and are
  fully mutable.
- `the_agent_keeps_no_enum` — DuckDB holds an ENUM's values outside its memory budget (an ENUM
  type of 3,000,000 values left an engine with a 1 GB budget holding 698 MB that
  `duckdb_memory()` called 0), so the agent keeps none: `CREATE TYPE` of any kind, in any schema,
  and a table or view with an ENUM column, nested in a list, struct or map or not, by CREATE,
  CREATE OR REPLACE or ALTER, are undone in the same post-statement check as a taken name, a
  table they would have replaced still there; a type named as a pivot's gets no column. An ENUM
  in a query that keeps nothing still works. The check adds one listing of the agent's columns
  to each CREATE or ALTER, about 2 ms with 100 scratch tables; a query pays nothing.
- `a_pivot_keeps_no_enum_after_its_batch` — a dynamic PIVOT's own ENUM type works for its
  statement, and is dropped once the batch is over, so pivots cannot pile ENUMs up either.
- `an_object_that_keeps_an_enum_in_its_sql_is_refused` — nor does an ENUM stay spelled in what
  an object keeps: a column's DEFAULT, a generated column, a CHECK, an index's expression, a
  view's or a macro's body, nested or not, in any schema, by CREATE or ALTER, written out or
  named as a pivot's type. DuckDB keeps a table whose DEFAULT casts to a type for as long as the
  type, so no DROP TYPE frees it. Read from the SQL DuckDB keeps of each object
  (`duckdb_tables()`, `duckdb_views()`, `duckdb_indexes()`, `duckdb_functions()`): on DuckDB 1.5.5
  `duckdb_dependencies()` lists nothing of the temporary catalog. Only what the statement may
  have made is read: a new table's or an altered one's, a new view's, and everything when it made
  neither; a CREATE TABLE pays about 2 M instructions more, a CREATE VIEW 7 M, a CREATE MACRO
  11 M on its 125 M. The helpers are held to the same pattern. String literals are read too: a
  string can spell a type DuckDB builds (`json_transform(…, '"ENUM(''a'')"')` is an ENUM on
  1.5.5), so such a DEFAULT or view is refused, and so, falsely, is a literal that only mentions
  one (`SELECT 'use ENUM(' AS note`, a host's report): telling them apart would take a list of
  the functions that read a type from a string.
- `a_spoofed_pivot_enum_cannot_be_pinned` — a host's review, through the binary: a type named
  `__pivot_enum_0` of 1,500,000 values and a table whose INTEGER column's DEFAULT cast to it
  passed as a dynamic PIVOT's and an INTEGER column, the type could not be dropped after the
  batch and that went unsaid, and three batches left an idle engine at 799 MB. Now the batch is
  refused, and nothing of either stays, batch after batch.
- `a_pivot_type_that_cannot_be_dropped_spoils_the_engine` — the guard behind those: a dynamic
  PIVOT's type that still cannot be dropped once its batch is over is not passed over. The
  engine says so with its answer, and the server kills and rebuilds it, answering `E_REBUILT`
  with why, so the memory goes with it.

The agent creates what it likes, temporary or not, under any name that is not already taken.
DuckDB resolves the connection's temporary catalog before any search path and the agent's own
schema before the host's catalogs, so an agent object named like a host table or a built-in
stood in for it: `CREATE TEMP TABLE readings` hid the attached `readings`, `CREATE TEMP MACRO
lower` replaced `lower`, and a macro named `duckdb_columns` answered the host's next attach with
columns the agent made up. After every `CREATE` and `ALTER`, inside the statement's own
transaction, benchy lists the agent's catalogs and undoes the statement if it took a name that is
taken. Names compare as DuckDB compares identifiers, ASCII case folded. Taken are: the tables on
the bench, DuckDB's built-in functions, macros and table functions, the names DuckDB's parser
reads itself, DuckDB's system views, and the operator's helpers and every table they read
(REQ-18). Built-in type names need no check: DuckDB refuses `CREATE TYPE integer` itself.

- `an_agent_object_cannot_shadow_a_host_object_or_builtin` — a table, view, macro or table macro,
  temporary or not, created or renamed into a taken name, by any spelling of it, is refused,
  undone, and told to pick another name; every name still means what it meant.
- `a_name_the_parser_reads_itself_is_taken` — `coalesce`, `if`, `try`, `try_cast`, `extract`,
  `percentile_cont` and the like never reach DuckDB's catalog, so no listing of it has them; a
  macro under one answered where the name was quoted (`"coalesce"(NULL, 2)` gave the agent's 1).
  They are found by DuckDB's parser, once per engine: every keyword that is not unreserved, and
  each unreserved keyword, or name its transformer rewrites, whose call parses to something
  other than a call of it. A keyword the parser reads as a name, such as `data`, stays free.
- `a_refused_name_stops_the_batch_where_it_stands` — the statements before it keep what they did
  and answer; nothing after it runs.
- `an_attached_name_is_taken_too` — a table attached after the seal is held to the same rule, and
  an attach under a name the agent already gave one of its own is refused with `E_NAME` rather
  than hidden behind it.
- `an_attach_under_the_agents_own_name_is_refused` — the same, through the host protocol: the
  host learns why and attaches under another name, and the agent's table stays.
- `a_system_view_name_is_taken` — an agent view named `sqlite_master` or `duckdb_tables`
  answered for the system view of that name wherever the agent named it unqualified. Every view
  of DuckDB's `system` catalog, in each of its schemas (`main`, `information_schema`,
  `pg_catalog`; 47 on DuckDB 1.5.5), is taken, as DuckDB lists them, read once per engine. A
  schema of the agent's under a system schema's name, and a view in the system catalog, DuckDB
  refuses itself; a table function's name stays free, since `FROM range` names a table.
- `a_dynamic_pivot_is_not_a_shadow` — DuckDB's own `CREATE TYPE` for a dynamic `PIVOT` passes the
  same check and answers once.
- `a_spoofed_pivot_type_gets_nothing_past_the_guard` — a type the agent names like a dynamic
  `PIVOT`'s is refused as its own (`a_type_the_agent_names_is_never_a_pivots`); were it taken
  for one, all it would buy is its own `CREATE` answering nothing and later statements numbered
  one lower. Every statement is still classified before it runs and every name still checked.
- `a_type_the_agent_names_is_never_a_pivots` — DuckDB 1.5.5 names a dynamic PIVOT's type
  `__pivot_enum_` and a random UUID, which the batch's SQL, written before it was drawn, cannot
  hold; the C API gives no statement's text or origin, so that is what tells them apart. A type
  is a pivot's only if it is so named, in the temporary catalog, and its name is not in the
  batch; DuckDB has no other spelling of an identifier (`U&"…"` is not implemented).
- `benchys_catalog_reads_cannot_be_shadowed` — every catalog read benchy makes names
  `system.main`, so a macro named like a catalog function, planted past the check, changes none
  of them: not the attach check, not an attach's columns, not `describe`, the hints or the
  engine version, and not what counts as a dynamic `PIVOT`.
- `the_agent_cannot_forge_what_the_host_reads_from_the_catalog` — through the host protocol: the
  macros that forged an attach's columns and failed every later attach are refused, and the
  attach answers with the real columns.
- `the_corpus_shadows_are_refused_and_undone` — every shadowing case in `corpus/escape.toml`.

## REQ-10 — Bounded results

The reason to put data on a bench instead of into a context window is that what comes back is
small. Results are capped in rows and bytes, truncation is always stated, and a result can be
fetched again by id.

- `results_are_capped_and_say_so` — a capped result says it is not all the rows.
- `a_batch_returns_every_result_not_only_the_last` — each statement in a batch answers.
- `a_failed_batch_returns_what_ran_before_it` — a batch stops at the statement that fails; the
  ones before it keep their effects, and their results come back after the error, fetchable by
  id, with the error first so no result can push it past the byte cap.
- `a_result_can_be_fetched_again_by_id` — a result is addressable after the fact.
- `two_sessions_never_issue_the_same_result_id` — every session numbered its results from `r1`,
  so a host whose conversations outlive sessions could hand a new session an old id and get a
  different answer. Ids now start with a random prefix of the session's own, `3fa9c1b2-7`: 32
  bits, so a stale id resolves in another session once in four billion tries, and it names no
  path, process or session another could use (REQ-1); an id resolves only where it was issued.
- `a_timestamptz_arrives_intact` — a value is never silently blank.
- `nested_types_render_rather_than_vanishing` — lists, structs and maps render.
- `every_common_type_renders_as_something_a_reader_can_use` — one sweep across the common types.
- `an_infinite_time_renders_as_duckdb_writes_it` — an infinite DATE, TIMESTAMP at every
  precision or TIMESTAMPTZ renders as `infinity` or `-infinity`, at the top level, in a list and
  in a struct. Arrow carries these values as plain numbers. A TIMESTAMPTZ rendered as the year
  294247, and a TIMESTAMP or a DATE as an `ERROR: Cast error` cell, in a result marked complete.
  A TIMESTAMPTZ in a list renders as its instant, where before it rendered as a "not rendered"
  marker.
- `cap_measured_on_the_wire` — the whole response, JSON-escaped as the client receives it, stays
  under the byte cap, and says it was cut. Nine shapes at two caps, among them one 1 MB value,
  200 columns, 60 statements in a batch, and text that escaping inflates sixfold.
- `the_response_budget_holds_for_awkward_shapes` — the same, at the renderer, for escaped
  multi-byte text and a batch of 40 results.
- `every_tool_response_obeys_the_byte_cap` — every tool, not only `query`: `get_result` on an
  empty result whose note quoted 40 KB values came back at 24 times the cap, and `describe` and
  `dialect` were not measured at all. Also a 1,500-column table and a 20,000-byte column name.
- `a_full_page_is_not_called_truncated` — a result of exactly the row cap is complete; one row
  more is not.
- `a_partial_result_is_never_presented_as_complete` — once reading stops early, the result says
  so even when every row kept fits on the page. No SQL has been found that makes conversion fail
  partway, so this is tested at the renderer.
- `a_truncated_result_can_be_paged` — a capped result is read further with `get_result` and an
  offset, without re-running the query; the last page says it is the end. The first 2,000 rows
  (at most 1 MiB) of each of the last 32 results are kept.
- `a_page_past_the_end_says_so` — an offset at or past the last row kept is an empty page whose
  footer says there are no rows at that offset and where the rows end, not a range that runs
  backwards (`rows 10001–10000 of 500`). It is not an error: the result is there, and a host's
  `page` still gets its `sql`.
- `a_value_arrow_cannot_carry_faithfully_says_so` — BIT, BIGNUM and TIME WITH TIME ZONE render
  correctly or as a marker naming the fix (`::VARCHAR`), never as a confident wrong value.
- `time_zone_conversion_works_in_the_shipped_build` — converting to a named zone works in the
  build a stranger gets, with the engine sealed.
- `a_refused_time_zone_fails_only_where_icu_is_loaded` — results are in UTC whatever the host's
  zone. A library without ICU has no zone to set and UTC is its only one; with ICU loaded, a
  refusal to set UTC stops the engine rather than leaving results host-dependent. The shipped
  library always has ICU and always accepts, so this is tested at the decision.
- `the_agent_connection_is_utc_on_a_host_that_is_not` — `TimeZone` is per connection in DuckDB
  1.5.5, and the lockdown set it on the host's connection only, so on a host in Europe/Berlin the
  agent's connection said Europe/Berlin, bucketed days in it and could not render a TIMESTAMPTZ.
  Now the zone is set globally before the lock, which reaches connections opened before and
  after; the seal reads it back on the agent's connection, sealed from a bench and sealed empty,
  and a day's bucket of 23:30 UTC is that day. Run in a process of its own whose zone is
  Asia/Kolkata, checked to have taken.
- `every_bench_connection_is_utc_on_a_host_that_is_not` — so is every connection to a loader's
  bench: its own, a clone, and one opened after it, as the helpers' build does.
- `a_zone_other_than_utc_stops_the_engine` — an agent connection the seal finds in another zone,
  or with ICU loaded and no zone to read, is a startup failure, not a host-dependent result.

## REQ-11 — Teach the dialect where it lands

Short deltas in the instructions, the fuller reference on demand, and a concrete fix in every
error that can have one.

- `instructions_carry_deltas` — the instructions state what is different about this engine.
- `version_not_hardcoded` — and name the version the running engine reports.
- `reference_is_on_demand` — the reference is a tool call, not part of every request.
- `hint_names_the_fix` — a file function is told the data is already loaded, and where.
- `dialect_slips_from_other_engines_each_name_their_duckdb_form` — each mapped slip names its
  DuckDB spelling.
- `an_unknown_table_names_the_ones_that_exist` — including the agent's own.
- `a_scratch_table_is_not_forgotten_by_the_hint` — the named tables include scratch.
- `a_correct_query_carries_no_hint` — a success is never padded.
- `empty_results_say_what_exists` — an empty result whose filter used an unknown value says what
  the column holds.
- `a_non_empty_result_never_carries_a_note` — and only then.
- `the_empty_result_note_stays_inside_the_deadline` — the note ran after the watchdog had
  stopped, a full `SELECT DISTINCT … ORDER BY` per text column the query named, and on ten
  million rows of unique values took long enough to carry a batch past the server's hard
  deadline: the engine was killed, the agent's tables lost, and the error blamed planning. A
  column whose first 2,048 values already hold more than the note lists is not read further,
  and the note gets what is left of the deadline, at most 0.5 s, past which its query is
  interrupted and the result goes back without one. On four million rows, six such columns
  took 8.7 s against a one-second deadline before and now take milliseconds, and the column
  that explains the result still does; a column whose head is one value is stopped at a 300 ms
  deadline.
- `mentions_survives_a_name_that_starts_with_a_multibyte_letter` — the empty-result note's name
  matching stepped one byte past a match, into `Ü`: a panic in the engine, and the agent's tables
  lost with it.
- `a_write_to_a_loaded_table_says_how_to_make_your_own` — `UPDATE`, `INSERT`, `DELETE`, `DROP`
  and `ALTER` on a loaded table are told it is read-only and how to copy it, and the message no
  longer names the bench's internal catalog (REQ-15).
- `an_export_is_told_results_come_back_through_query` — DuckDB refuses `COPY … TO` and `EXPORT`
  in the words it refuses a file read; the statement tells them apart, and an export is told
  there is no file output.
- `a_dynamic_pivot_answers_once` — DuckDB runs a dynamic `PIVOT` as a `CREATE TYPE` and the
  query; only the query answers, the statements after it are numbered as written, and a
  statement that returns no rows by nature gets no empty-result note.
- `the_dialect_reference_is_written_for_the_agent` — `dialect` serves `docs/DIALECT.md` without
  its maintainers' blocks: no requirement, test or source file is named to the agent.

## REQ-12 — Admission control at the source

Nothing may let the model do unbounded work to a system benchy reads from.

- `estimate_before_fetch` — a connector that implements `estimate` is asked first and refused over
  budget before it fetches; the refusal says what was estimated and what was left.
- `an_inband_estimate_cancels_a_fetch_already_running` — an estimate sent during a fetch stops it.
- `a_cancelled_fetch_is_killed_if_it_ignores_the_request` — cancelling sends SIGTERM; a connector
  that ignores it is killed after the cancel grace instead of holding the fetch to its deadline.
- `reload_does_not_amplify` — a rebuilt bench re-attaches what the session stored instead of
  asking the source again.
- `an_empty_result_survives_a_rebuild` — including a source that returned nothing.
- `an_operator_declared_window_is_parsed_and_refuses_what_it_cannot_apply` — an operator can
  bound what a source may return at all.
- `exactly_what_is_left_fits_and_one_more_does_not` — the budget's edge is exact, for rows and for
  bytes.
- `session_budget_enforced` — the pass budget bounds what is actually read, not only what was
  estimated: two sources that each fit their own caps are refused once together they pass it.
- `a_configured_session_budget_counts_every_fetch` — and so it does in a server, where each
  source is fetched by a loader of its own and the session is charged what the loader reports:
  under an operator's `[budget]`, exactly the budget in rows or in bytes loads, and past it the
  second source is refused, naming the budget.
- `a_failed_fetch_still_spends_what_it_read` — a fetch costs its source what it read whether or
  not the load succeeds: a stream cut short, a refusal past a cap or a connector's error is
  charged before it is reported, in a server and in one process alike. With a partial start the
  next source is fetched, and it used to get the whole budget again: three 1 MB sources under
  1.5 MB fetched about 2.5 MB.
- `a_sources_own_cap_is_not_called_the_sessions_budget` — a refusal names the cap that was
  passed: a source's own `max_bytes` is not reported as the session's budget because the rows
  happened to be bounded by it.
- `a_session_budget_that_admits_nothing_is_refused` — a `[budget]` ceiling of zero, or a key the
  table does not know, stops startup; a ceiling left out keeps its default.
- `the_store_is_private_to_the_session` — what was loaded is kept for rebuilds, and a rebuild
  reads it instead of any source, so a file planted there would be served as if loaded. It lives
  in the session's own directory: created fresh, owned by the user running benchy, mode 0700.

## REQ-13 — The host protocol

A host puts data on a session's bench over `benchy-mcp host`
([docs/HOST-PROTOCOL.md](docs/HOST-PROTOCOL.md)). The pipes are the capability, and so is a
descriptor a request carries; requests are strict, and a refusal changes nothing.

- `the_host_protocol_refuses_what_it_does_not_know` — an unknown call or field, a cap raised or
  lowered past what can say it cut, a request without an id, a second `hello`.
- `a_session_that_does_not_open_properly_ends` — whatever is wrong with the first request: not a
  `hello`, no id, a field `hello` does not take, a version list that is not a list.
- `a_request_is_read_exactly_as_sent` — a key given twice is refused rather than one value taken;
  the caps `hello` reports can be handed back, each lowered, the deadline among them.
- `an_id_in_use_is_answered_once` — a request reusing an id still being answered is refused with
  `"id": null`, whatever else is wrong with it, or the host would see two replies with one id.
- `a_host_that_closes_its_input_still_gets_its_answers` — closing stdin says no more requests,
  not "I have gone": a host that writes everything and then reads is answered in full.
- `a_host_that_has_gone_is_not_worked_for` — one that has closed its end of the replies too gets
  nothing more run: the first reply that cannot be written ends the session.
- `a_request_larger_than_a_request_can_be_ends_the_session` — a megabyte, when every frame is
  parsed and a request is SQL or a path.
- `hello_says_what_the_session_is` — the version agreed, the newest both speak, the engine, the
  sandbox, the caps and the limits.
- `a_host_may_lower_the_memory_ceiling_but_never_raise_it` — the operator sets the engine's
  memory ceiling (`--memory-ceiling`, or `[engine] memory_ceiling` in the config); in protocol 3
  `hello`'s `limits.memory_ceiling` lowers it as any limit is lowered, and the lower one is what
  kills the engine. One that raises it, one below DuckDB's own budget, or `limits` in a session
  that agreed an older protocol ends the session as `E_REQUEST`. `hello` reports the budget
  (`limits.memory`) and the ceiling.
- `a_host_lowers_the_loader_ceiling_and_cannot_raise_it` — from protocol 6 `hello` reports
  `limits.loader_memory_ceiling`, the operator's (`--loader-memory-ceiling` or `[loader]
  memory_ceiling`), and a host may lower it; lowered, it is the one enforced: an attach whose
  loader passes it is `E_LIMIT`, saying what happened and what to do, and attaches nothing.
  Raised, set to 0, or given before protocol 6 ends the session as `E_REQUEST`; before 6 it is
  not reported, since a host may send `limits` back as it got them. A channel stream whose loader
  is killed for it loses its place and closes the channel
  (`a_channel_that_loses_its_place_is_closed_and_says_so`).
- `a_bad_attachment_attaches_nothing` — a stream cut short, a stream with another after its
  end-of-stream marker, a file that is not Arrow, a symlink, a type outside the vocabulary, two
  columns with one name: each refused as `E_DATA`, naming no path, and the table already there
  keeps serving.
- `an_attach_name_the_agent_could_not_type_is_refused` — refused, not rewritten, so the host knows
  what the agent types; an unreserved keyword such as `data` is a fine name.
- `a_detach_drops_the_results_that_could_hold_its_rows` — and a detach takes effect, even if the
  engine is lost doing it.
- `ping_is_answered_while_a_query_runs` — calls run one at a time, but a ping never waits behind
  one, and an id still in use is not accepted twice.
- `a_rebuild_restores_the_attachments` — a wedged planner costs the engine: the host is told
  `E_REBUILT`, every attached table is back, and the agent's own are gone.
- `a_session_holds_at_most_its_limit_of_tables` — replacing a table does not count as adding one.
- `several_tables_attach_in_one_request` — in protocol 2, one `attach` lists several tables, each
  answered as an attach of its own, in the order asked, and the engine is asked once for all of
  them: DuckDB's functions are listed once a request.
- `the_loaders_of_one_attach_run_at_once` — the loaders of one request run side by side, on a
  machine with more than one core.
- `a_host_streams_a_table_without_a_file` — in protocol 3, a table can come through a pipe whose
  read end the host sends with the `attach`, alone or beside a file in `tables`, and replace one;
  no file is written on either side, and it costs the budget what its file would.
- `ping_is_answered_while_a_stream_is_attached` — an attach waiting for its host to write does not
  hold up a ping, and a read end the host made non-blocking is read blocking.
- `a_streamed_attach_keeps_every_rule_of_the_file_form` — cut short, another stream after it, not
  Arrow, empty, a type outside the vocabulary, two columns DuckDB cannot tell apart, zstd bodies,
  unpacked past the bound: each refused with the file form's code, the table already there still
  serving, and the host's writer told nobody reads rather than left blocked.
- `a_descriptor_is_taken_only_by_a_streamed_attach` — a descriptor on any other request, a stream
  without one, more than one a stream, a stream beside a path, `stream` not `true`: refused as
  `E_REQUEST`, every descriptor closed before the answer; a descriptor that is not a pipe, a
  socket or a file is `E_DATA`; protocol 2 has no `stream`.
- `a_stream_needs_a_socket_to_come_over` — on pipes no descriptor can come, and a streamed attach
  is refused saying so.
- `a_host_streams_tables_back_to_back_over_one_channel` — in protocol 5, a session started with
  `--stream-channel` takes its streams from one more inherited pipe, its protocol on plain pipes
  as a Node.js host's are: the host writes streams back to back, ahead of the requests or after,
  and each attach with `"stream": "channel"` takes the next whole one, a `tables` list in its
  order with a file between, a stream larger than a pipe holds included, and a replacement; each
  costs the budget its stream's bytes.
- `ping_is_answered_while_a_channel_attach_waits` — an attach waiting on the channel does not
  hold up a ping, and a channel the host made non-blocking is read blocking.
- `a_refused_stream_leaves_the_channel_at_the_next_one` — a stream refused for what the file
  form refuses (a type outside the vocabulary, two columns DuckDB cannot tell apart, zstd, unpacked
  past the bound, a stated column it lacks), for a limit (rows or bytes in an attachment, the
  budget, the count of tables) or for its name is read on to its end-of-stream marker and dropped:
  the next attach takes the next stream, the host's writes all go through, nothing changes.
- `a_channel_that_loses_its_place_is_closed_and_says_so` — where the end of a stream cannot be
  found (the channel ends before the marker, what comes is not Arrow, more is left of a refused
  stream than `attach_bytes`, the load runs past `--load-timeout`), benchy closes the channel:
  the host's next write fails with `EPIPE`, so nothing else benchy started holds it; that table's
  error says `"channel": "closed"` and why, every channel table after it is `E_CHANNEL`, and the
  session serves everything else.
- `a_refused_request_that_names_the_channel_closes_it` — a request refused whole that names a
  table from the channel (a name twice, a bad or unknown field, a table that is not an object,
  one table's fields beside `tables`, more tables than `attachments`, a bad stated shape,
  `"channel"` before protocol 5, a key given twice, an id still in use) closes the channel and
  says `"channel": "closed"`, answered in its turn behind the requests before it: the next
  channel attach is `E_CHANNEL`, never a stream written ahead for the refused request under
  another name. A refused request that names no table from the channel leaves it be, and a
  session without a channel is not told one closed.
- `a_stream_channel_is_one_the_operator_names` — an inherited descriptor is not a channel unless
  `--stream-channel` names it; one that is not open, not readable as a stream, is stdio or is
  named outside `host` stops the start; `stream` takes `true` or `"channel"` and nothing else,
  and `"channel"` not before protocol 5.
- `a_channel_and_a_descriptor_serve_one_session` — both streamed forms in one session and in one
  request.
- `a_channel_reader_stops_at_each_streams_end_marker` — the channel's reader follows the Arrow
  messages as they pass and reads nothing past a stream's marker, whatever size its reads are.
- `a_refused_stream_is_drained_to_its_marker_within_a_bound` — the rest of a refused stream is read
  past to its marker, at most the bound: exactly the bound fits, a byte more does not, and an end
  before the marker or bytes that are no message lose the place.
- `limits_are_lowered_by_the_host_and_raised_only_by_the_operator` — what one attachment and a
  session may hold, and the tables at once: the operator sets them, past the defaults or under,
  on the command line or in `host --config` (`[attach]`, `[budget]`, no sources), never both for
  one limit, never 0; a host lowers them in protocol 3's `hello`, for a file and a stream alike,
  and a limit raised, 0, unknown, or asked in protocol 2 ends the session.
- `a_query_reply_counts_the_rows_of_each_result` — beside the text, `results` says of each result
  the rows benchy holds, whether that is all of them, how many the text shows, and whether the
  text is less than the whole result, so a host acts on an empty or a cut result without parsing
  the footer.
- `a_page_says_the_sql_of_its_result` — `page` returns the statement its result came from, as the
  agent wrote it, one statement of its batch: never a helper's body, cut at `max_bytes`, and left
  out where the batch cannot be split as DuckDB splits it.
- `texts_name_the_tools_as_the_host_names_them` — in protocol 4, `hello`'s `tool_names` renames
  the agent's tools in every text that names one — a result's footer, a hint, `describe` — and
  none names benchy's own; without it, they do.
- `tool_names_are_known_tools_plain_names_and_protocol_4` — a key that is not one of benchy's tools,
  a name that is not plain (a letter, then letters, digits, `_` or `-`, at most 64 bytes), two
  tools under one name, a name that is another of benchy's tools, or `tool_names` in protocol 3
  or older: each ends the session as `E_REQUEST`. A name labels prose and addresses nothing.
- `a_long_tool_name_keeps_every_text_within_its_budget` — a 64-byte name does not push a footer
  or `describe`'s closing line past the byte cap.
- `a_host_states_the_shape_of_what_it_attaches` — in protocol 3, `time_column` and `series` on an
  attach, an entry of `tables` or a stream; `describe` says "is", not "looks like"; a name the
  stream does not have, or a time column that is not a time, is refused before a row is written
  and changes nothing.
- `one_bad_table_of_an_attach_is_refused_alone` — a name the agent could not type, a stream that is
  not one, a file that is not there: each refused with its own code, the tables around it
  attached, and a table it was to replace still serving.
- `an_attach_of_several_tables_is_held_to_the_limit_in_order` — as if attached one after another:
  with room for one more table, the first new name takes it and the next is `E_LIMIT`, and
  replacing is not adding; each table costs the budget what it would alone, a refused one nothing.
- `the_tables_of_one_attach_are_held_to_the_budget_in_order` — each loader of a request is given
  what the budget has left, so the tables are held together after, in order: one that passes
  what the ones before it left is refused as `E_LIMIT`, in rows or in bytes, and exactly what is
  left fits. `the_tables_of_one_attach_are_counted_in_order` does the same for the count.
- `an_attach_request_that_is_not_well_formed_attaches_nothing` — `tables` beside `name`, a list
  that is empty, longer than a session holds or names a table twice, an entry with a field it
  does not take: refused whole, as `E_REQUEST`, before anything is read. Protocol 1 has no
  `tables`.
- `a_host_attach_past_what_the_session_has_left_is_refused_as_a_limit` — an attachment over
  what the session's budget has left, in rows or in bytes, is refused as `E_LIMIT` before
  anything is written; exactly what is left fits.
- `a_host_attach_stops_at_the_budget_not_after_reading_it_all` — the bytes are checked before a
  batch is read, and the rows as each batch arrives: the attach stops at the batch that passes
  what is left, before it is written and before the rest of the stream is read. Checked once the
  whole stream was in its table, a refused attach had paid for every insert first.
- `a_loader_that_cannot_read_its_job_says_so` — a loader's error reaches the operator or host as
  its own words, not as a bare failure.
- `the_budget_moves_by_what_was_attached_and_nothing_else` — by the rows the engine counted and
  the bytes benchy measured, replacements included; a refused attach costs nothing, and
  `describe` says what is left.
- `a_dictionary_encoded_attachment_is_unpacked_within_the_bound` — as a connector's is: its
  values are loaded, and one that unpacks past what an attachment may hold is refused as
  `E_LIMIT` before it is unpacked.
- `a_view_attachment_is_unpacked_within_the_bound` — and so is a string or binary view column,
  counted by the lengths its views state: a file of 130 KB that unpacks to 275 MB is refused.
- `a_detach_brings_a_bench_that_could_not_rebuild_back` — rebuilds that keep failing stop being
  tried, but detaching the table that stops them earns a fresh try.
- `a_killed_host_leaves_nothing_behind_even_without_an_engine` — after a rebuild that failed there
  is no engine to notice a SIGKILL; the warden removes the session all the same.
- `a_message_names_no_path_of_the_sessions_or_the_hosts` — DuckDB names the files it could not
  open, and a loader's error can name its input; neither reaches a reply.
- `a_short_path_does_not_garble_a_message` — only an absolute path is replaced, and the host's
  path is made absolute first: `a` occurs inside ordinary words.
- `columns_duckdb_tells_apart_attach` — duplicate columns are refused as DuckDB sees them: by ASCII
  case, so `Ä` and `ä` are two.
- `a_host_session_leaves_nothing_behind` — closing the pipe stops the engine and removes the
  session's directory.

## REQ-14 — Describe tells the shape of the data

Every step benchy saved an agent in the eval was a step of discovery (eval/REPORT.md). So one
`describe` says what an analyst would look for first in a table of time series: which column is
time, its range and cadence, which column tells the series apart, and which series are dead,
start late, stop, pause, skip rows or repeat them — and which moments every series shares, such
as the hour a clock change zeroes or doubles. What is a heuristic says "looks like"; an operator
can state the time column and the series key per source instead (`time_column`, `series`), and a
host per attachment. Units are never guessed.

The profile is computed **once, by the loader**, and kept in the table's comment, so `describe`
only renders text and a rebuilt engine does not profile again. It is built from one scan of
aggregates, then one pass over the rows that DuckDB streams to the loader, which adds up each
series — its count, extent, phase and two checksums of its timestamps, and whether its times came
in order, rising or falling — each time, and the rows that are 0 or null. The checksums alone
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
place another 9–11% (docs/PERFORMANCE.md, R2-2). The distinct counts are kept only when the first
batch's times repeat: they choose the key only then, and if the rest of the stream repeats its
times after all, they are read from the table. A file, and an HTTP body DuckDB reads itself, is
kept the same way from one scan as Arrow, which replaces the scan and the pass that each read
the table back: 6–13% fewer instructions for 1,000,000 rows (R2-4). That scan reads what the
table is written from — a staged file's stage, or a Parquet file a second time — on a thread
beside the write, so it is off the path to the first `describe`: 8% sooner for 5,000,000 rows
of a four-column CSV (R3-2). Every CSV and JSON file is staged (R4-2).
The agent's own tables are named in `describe` but not profiled: they change with every
statement, and the agent made them.

- `describe_tells_the_time_column_its_range_and_cadence` — the first temporal column, its first
  and last time, and the step most series follow.
- `describe_tells_the_series_key` — the text column with the most values that still leaves each
  series rows to spare, named as a guess; its values; how many series are on their step with
  nothing missing or repeated; each numeric column's range, nulls and zeros.
- `an_integer_reading_is_not_taken_for_the_series_key` — 50 integer device ids beside an integer
  CO₂ reading of 1,600 values and an RSSI of 91: where two columns of a kind could be the key, the
  one that cuts the table's first slice of time into series that keep a step wins over the one
  with the most values, when clearly more of its rows keep it. One query per such column over
  that slice, as the step's sample reads; none when only one column could be the key.
- `a_small_table_still_finds_its_series_key` — the bundled example, eight rows in two series:
  too few for ten rows a series, so the key is the column whose every value has rows at two
  times or more and which, with the time, tells apart at least 80% of the rows, more than the
  time alone does. Counted exactly, over at most a million rows, only when the first rule found
  nothing. It finds what stating the key finds: one gap and one repeat, not four repeats.
- `repeated_times_with_no_key_are_said_to_be_unassessed` — times repeat and no column tells the
  rows apart: `describe` says so, with the exact count of distinct times, and reports no gaps or
  repeats it cannot place.
- `describe_names_duckdbs_own_types` — columns are named with DuckDB's own types, `double`,
  `bigint`, `varchar`, `timestamptz`, in the library and through MCP; `float` and `int` are
  32-bit in DuckDB.
- `counts_are_said_in_good_grammar` — "1 row", "1 of 2 series has", "4 times"; never "(s)".
- `a_long_line_is_elided_between_items_never_inside_one` — a series with ten runs, and a key
  line of twelve long values: each line is shortened where an item ends — first runs, `then …
  then`, the last, and how many in all; the first values and how many more — never inside a
  timestamp or a value.
- `describe_names_a_dead_series_a_late_start_a_stop_and_a_pause` — runs of 0 or null lasting a
  day or more, from the start, to the end, or between; a series that is 0 throughout.
- `describe_names_gaps_and_repeated_times` — rows missing and rows repeated, where, including a
  series whose gap is hidden by as many repeats: its row count is right, and only the checksums
  of its timestamps give it away.
- `three_gaps_hidden_by_three_repeats_are_found` — series whose count, sum and sum of squares of
  step indices are those of a regular series (`{0,1,1,4,5,5,6,6,8,9}`; 2,000 steps missing
  1002, 1003 and 1007 and repeating 1001, 1005 and 1006; `0..20` without 1, 2, 4 and 7 and with
  0, 3, 5 and 6 twice) are not called regular, and their gaps and repeats are counted; the same
  rows backwards, and shuffled so that no series' times come in order, describe the same; and one
  series with no key is not called regular on its checksums.
- `anomalies_past_any_limit_are_counted_exactly` — gaps, repeats and rows off the step are
  counted per series by DuckDB, not listed and counted up to a limit: 300,000 repeats in one
  series are said as 300,000, and the series after it keeps its notes.
- `describe_names_a_moment_every_series_shares` — a time when every series reads 0, or their
  total is twice what it was a day either side; a day clocks change is said to be one.
- `the_public_meter_data_shows_its_quirks` — on the eval's public data, when it has been fetched:
  the dead meter, the late starters, the pauses and the stop, and both clock changes.
- `a_stated_shape_wins_over_the_heuristic` — a stated time column and a stated two-column key are
  used and not called guesses; a guessed key that is not the whole key shows as repeated times.
- `a_stated_column_that_is_not_there_refuses_the_load` — a stated column that is missing, or a
  time column that is not temporal, fails the load rather than being ignored.
- `a_stated_shape_must_name_declared_columns` — an exec source's stated shape is checked against
  its schema at startup; a file source's travels to its loader.
- `a_table_without_time_gets_its_columns_summarised` — ranges, and a small text column's values.
- `a_distinct_count_never_exceeds_the_values` — DuckDB's `approx_count_distinct` is
  HyperLogLog, and said 24 of 20 values. Each estimate is held to its column's values, counted
  in the profile's one scan or, for a stream, as it went in; one that reaches that count is made
  exact ("20 distinct") on a table of up to 100,000 rows, and stays "about" on a larger one.
  Loads of 50,000 and 1,000,000 rows without a time column cost the same as before.
- `an_integer_series_key_is_listed_in_number_order` — the keys are held as text, and were listed
  in text order (`50 series (1 … 9)`); they are listed in the order of the key's type, an
  integer column's by number. Past 200,000 series, where the ones kept are the first in text
  order, the key gets no range.
- `many_series_keep_the_tables_own_extent` — past 200,000 series the time column's range is the
  table's, from the scan, not that of the series examined; and the line on regular series says
  how many of the first 200,000 by key are, and that the series after the last examined were not
  examined and have no notes, never "every series" or "all span the whole range".
- `the_shape_is_summarised_never_cut_silently` — 300 series with notes: the first forty, then
  how many more; with less room, how many lines are not shown; with almost none, that the shape
  is not shown. Always within the byte budget.
- `the_pass_finds_what_the_queries_find` — the shared moments and the runs of 0 or null worked
  out from the loader's one pass are the ones the queries a table too large for it falls back on
  find, on a fixture with each of them and two rows at one time.
- `the_profile_kept_as_rows_arrive_is_the_profile_of_the_table` — what a stream's batches showed
  as they went in gives the profile reading the table gives, byte for byte: one series and many,
  an integer key, a float, an unsigned counter, times before 1970 and a date for time, NaN, a
  negative zero and nulls, a stated key, and the cuts no tally was kept for, which are read; a
  first batch whose times do not repeat before batches whose times do, whose key is chosen from
  counts read from the table; and one series' runs of 0 and of null, kept without a query.
- `a_files_profile_is_kept_from_one_scan_and_is_the_profile_of_the_table` — a Parquet, CSV and
  NDJSON file of seven meters, with a dead one, a late start, a stop into nulls, a pause, an hour
  all 0, NaN, an integer and a date: the scan's aggregates and the pass are kept from one Arrow
  scan of the table, and give the profile reading the table gives, byte for byte.
- `a_files_profile_is_kept_beside_its_write` — a staged CSV and NDJSON file and a Parquet file
  are watched beside their write, and each records the profile reading the table gives; a body narrowed to a time window is watched as narrowed, its
  window read in the loader's zone, not the one a second connection starts in.
- `a_kept_distinct_count_is_duckdbs` — the distinct counts kept as rows arrive are what
  `approx_count_distinct` says of the table, for integers, instants, dates and text, from none to
  250,000 values: the heuristics choose the same either way.
- `a_kept_distinct_count_of_a_tallied_key_is_duckdbs` — a key a tally keeps series for is hashed
  once per series, not once per row; what is counted is still DuckDB's count: a null that began a
  series is not a value and the text that stands for one in a key is, an integer key is counted as
  an integer, and a tally that gives up within a batch leaves the rest of it counted row by row.
- `a_cycle_of_series_costs_one_key_compare_a_row` — a tally finds a row's series by its key's
  length and first sixteen bytes, kept beside the series, and tries the next series first when
  meters arrive in a fixed cycle: one key compare a row, where it took two. The watch of 500
  series costs 285 instructions a row where it cost 398 (docs/PERFORMANCE.md, R3-4).
- `a_tally_tells_apart_keys_alike_in_their_first_words` — keys that differ only past their
  sixteenth byte, or only by a trailing NUL, are different series.
- `a_keys_words_are_read_alike_from_every_string_layout` — a key's words read in place from a
  string array, a large one, a view and a slice are the words of its own bytes.
- `a_tallys_totals_per_time_count_every_row_of_every_time` — the per-time totals, added up a run
  of one time at a time, count every row, the last run's and a time that comes back included.
- `an_integer_key_is_written_as_text_once_a_series` — an integer key is tallied by its value
  and written as text once, when its series begins, as `CAST(… AS VARCHAR)` writes it, where it
  was written for every row: at 10,000,000 rows of 500 integer-keyed series, 27% fewer
  instructions and the watch's CPU 926 → 364 ms (docs/PERFORMANCE.md, R3-10).
- `placing_a_series_matches_counting_its_steps` — a series' phase and the checksums of its step
  indices, worked out once the step is known from sums kept before it was, are what counting each
  index gives.
- `describe_does_not_scan_the_table` — on two million rows `describe` takes milliseconds and is
  the same every time.
- `the_same_data_gives_the_same_shape_from_csv_and_parquet` — origin-blind: the same rows, read
  from another format in another order, describe identically.
- `the_shape_reaches_the_agent_through_the_engine` — profiled by a loader process, carried in the
  file, read by the engine at attach, rendered by MCP `describe`.
- `a_hosts_describe_carries_the_shape_of_what_it_attached` — the same for an Arrow stream a host
  attaches, through the host protocol's `describe`.

## REQ-15 — Origin-blind

The agent cannot tell where data came from. The same data makes the same table — the same
column types, the same `describe`, the same answer to every query — whether it was a CSV, a
Parquet or NDJSON file, an Arrow stream a host attached from a file, through a pipe or through
its stream channel, or a connector's fetch. At runtime the
agent hears about tables, never about sources, connectors, files or paths: those, and every
reason a load failed, are the operator's (`a_connector_error_reaches_the_operator_not_the_agent`).

One mapping is applied to every source as it is loaded (`src/canonical.rs`): every integer to
`BIGINT` (`UBIGINT` to `HUGEINT`), every float to `DOUBLE`, every timestamp to `TIMESTAMP WITH
TIME ZONE` at microseconds; a string in any layout is `VARCHAR`; dates stay `DATE` and decimals
keep their precision. A zone-less timestamp is read in its source's `timezone`, UTC unless the
operator names one. A timestamp finer than a microsecond is refused rather than rounded. Only
top-level columns are mapped. The physical layout is the same too: a file is written to its
table in one pass on one thread, straight from its reader (Parquet) or, for a CSV or JSON file of
any size, from a stage the reader filled, and a stream is written batch by batch as it arrives, in order, on one thread, which fills the same row groups the same way. So the same rows
in the same order give the same file size, blocks, row groups, segments and compression, whatever
they came from, and on every load: a loader's file is written in storage version v1.3.0, whose
text compression (DICT_FSST) is chosen without sampling, where DuckDB 1.5.5's default chose
between dictionary and FSST from a random sample (src/storage/compression/fsst.cpp). Rows in another
order are other data as far as the layout goes: it follows the order the source sent.

The layout includes where each segment lies: `pragma_storage_info` gives the agent every
segment's `block_id` and `block_offset`, and those are the same too. That is why the write stays
on one thread, though it is most of the path to the first `describe` at 10,000,000 rows
(docs/PERFORMANCE.md, R4-1). Measured with DuckDB 1.5.5, a write on four threads, split into
fixed groups of row groups each committed in order, makes the same row groups, segments and
compression from every source; but DuckDB numbers blocks in the order its threads write them,
and the blocks segments lie in changed from one load to the next (4 placements in 10 loads of
one table). Through DuckDB's own parallel insert the partial blocks are also packed by which
thread wrote which row group, and the block count changed too (24, 25 or 26 blocks in 30 loads).
The one arrangement found that keeps the blocks — a transaction per row group, compressed as it
commits, in order — compresses on one thread at a time again: 10,000,000 rows took 3.9 s where
one thread takes 2.9 s, each commit a sync of the file (2.4 s against 2.9 s with the file in
memory).

- `the_same_data_looks_the_same_from_every_source` — one fixture, cut from the eval's public
  meter data, loaded as CSV, as Parquet with nanosecond timestamps and an unsigned integer, as
  NDJSON, attached by a host with second-resolution timestamps and a dictionary-encoded string —
  from a file, and streamed through a pipe — and fetched by a connector sending zoned millisecond
  timestamps and an 8-bit integer: `describe` and six queries, `typeof` among them, answer byte
  for byte the same.
- `a_stated_shape_looks_the_same_from_every_source` — the same stated time column and series
  key, from a file source's config, a host's attach from a file and through a pipe, and a
  connector's config: the same `describe` and answers, saying "is" where a heuristic says "looks
  like".
- `the_physical_layout_says_nothing_of_where_data_came_from` — `pragma_database_size()` and
  `pragma_storage_info`, which the agent can call, answer the same for 840,000 rows from all six
  sources, a host's streamed attach among them. A CSV used to be converted in place, and every table written in parallel: on the eval's
  meter data that left 8.0 MiB in 9 row groups from CSV and 11.5 MiB with 15 free blocks in 8 from
  Parquet. Now each is 7.3 MiB in 7 row groups and 29 blocks, and a server's start took about
  0.1 s longer. (7.0 MiB and 28 blocks until a loader wrote each row group as it filled, below:
  each row group's segments now start a block of their own.) The block and offset of every
  segment are compared too: a parallel write from row-group-sized batches made the same counts,
  row groups and segments from a CSV file and its HTTP body, in different blocks.
- `a_loader_writes_each_row_group_as_it_fills` — every loader holds one row group of a bulk
  insert before writing it, not DuckDB's five: a stream's or a Parquet file's loader peaks
  11–16 MB lower (a CSV's, staged in memory first, does not move), and every source is still
  written the same way.
- `a_files_layout_is_the_same_on_every_load` — the same file loaded eleven times makes the same
  segments and compression, in the same blocks at the same offsets, and a file of the same size
  every time (with the write on every thread, two loads made the same segments with two of them
  in each other's blocks), and no text segment is compressed by a method chosen from
  a random sample. In DuckDB 1.5.5's default storage version the choice between dictionary and
  FSST is made from a random quarter of a segment's vectors, seeded from `getrandom`, which no
  setting reaches: at a near tie 1 load in 30 of a column of seven short values came out FSST,
  and the layout tests failed under load.
- `a_json_file_is_staged_whatever_its_size` — a JSON or NDJSON file is staged, read on every
  thread, however large it is, and watched beside its write. Staged only up to 64 MiB while a
  stage held the whole table in memory, a 10,000,000-row NDJSON file (633 MB) was read straight
  in on the write's one thread: 6.6 s to its first `describe`, 4.6 s staged, its loader at the
  stage's cap (351 MB) where it held 132 MB, and 134 MB spilled to the staging directory.
- `a_json_files_timestamps_read_as_text_load_as_the_sniffer_reads_them` — an NDJSON file's
  timestamp columns are read as text and cast, a third of the read's instructions cheaper than
  the format DuckDB's sniffer found, and the table is the sniffed read's, value for value and
  type for type, staged, in a zone other than UTC: `Z`, offsets, fractions,
  a space for the `T`, nulls, with an odd value past the sniffer's sample at a vector's first row
  and inside one. A value the cast does not take as the sniffer does — nanoseconds, seven digits,
  a date alone, `epoch`, a padded `infinity`, a two-digit year, 12-hour times, a number — has the
  file read the sniffed way, to its table or its refusal (an infinity's,
  `an_ndjson_infinity_is_refused`); never a null, never a rounded time. A
  value the sniffer's formats refuse and the cast reads exactly — an offset in a vector that began
  without one — loads at that instant, where the sniffed read refused the file.
- `an_ndjson_bodys_timestamps_read_as_text_load_as_the_sniffer_reads_them` — the same for an
  undeclared HTTP NDJSON body, spooled to a file and read as that file is, narrowed or not: its
  timestamps read as text, or, for a value the cast does not take, the sniffed way, to the same
  table.
- `a_json_file_whose_columns_the_text_read_would_change_is_read_as_it_was` — that read names the
  sniffed columns, which turns the sniffer off for more than the timestamps; a file it would read
  otherwise is read the sniffed way, once: one with a date (`26-01-02` would be the year 26), a
  struct (a key not in the sample would be dropped, not refused), timestamps in a list (an
  offset dropped), two keys differing in case, a `.json` file, no timestamp at
  all, or one under 1 MiB, where the `DESCRIBE` first costs more than the cast saves.
- `a_file_that_fails_midway_leaves_no_table_and_says_why` — a file whose rows go bad after 60,000
  good ones fails in DuckDB's words, staged or read straight in (as a Parquet file is), and
  leaves no table and the loader its threads. Read straight in, it used to say only that the
  transaction was aborted.
- `the_catalog_says_nothing_of_where_data_came_from` — `duckdb_databases()`, `duckdb_settings()`,
  `duckdb_tables()`, `duckdb_columns()`, `information_schema` and the search path name no file,
  path, connector or process id, and a file's catalog and a connector's are the same once the
  session's own random directory name is set aside. That directory is still named: DuckDB's
  allowed directory must be a real path, and a path inside the session, random, says nothing of
  the source.
- `a_stream_may_be_narrower_than_its_declaration_never_wider` — a connector may send a type every
  value of which the declared one holds exactly, and the table holds the declared column's
  canonical type; a wider integer, an unsigned 64-bit one for a signed one, a float for an
  integer, a zone-less time for an instant, and a nanosecond the bench cannot hold are refused,
  naming the column.
- `a_zone_less_timestamp_is_utc_unless_its_source_names_a_zone` — a file's or a connector's
  zone-less timestamp is read as UTC, or in the zone its source's `timezone` names; a zone the
  engine does not know stops startup.
- `a_zone_less_timestamp_is_read_in_its_sources_zone` — the conversion itself, at the SQL.
- `an_infinite_timestamp_is_kept_from_every_source` — `infinity` and `-infinity` load as
  themselves, never as a null, from a CSV, a Parquet file at every precision, a host's Arrow
  stream at every unit, zone-less and zoned, declared non-nullable, and a connector's. The
  zone-less conversion's `epoch_us` gives NULL for either, and these sources loaded both as
  nulls. A nanosecond column was refused, because `epoch_ns` of an infinity is the sentinel itself.
  `describe`'s extents leave them out.
- `an_ndjson_infinity_is_refused` — DuckDB 1.5.5's JSON reader reads `infinity` and
  `-infinity` in a column it sniffed as a timestamp or a date (any case, padded, `\u`-escaped,
  in a list too) as 1900-01-01 00:00:00, and an NDJSON or JSON file, or an HTTP body read as one,
  loaded with that time. The text cast above never takes them: they are under 19 bytes, so the
  file is read the sniffed way, which gives the same time. Such a file is now refused, naming the
  file and the column and saying what to do: write the value as null or as a real timestamp,
  declare the column's type in `[[source.schema]]`, which reads it as itself, or use CSV or
  Parquet. A genuine 1900-01-01 loads, beside a text column that says `infinity` too.
- `clickhouse_looks_the_same_as_a_file` — ClickHouse's own `ArrowStream`, LZ4 bodies and zoned
  `DateTime64` included, fetched by `clickhouse local` as a connector, against the fixture as CSV.
  It needs the ClickHouse binary, named by `BENCHY_CLICKHOUSE_BIN` as for REQ-16's tests, and
  says it was skipped without it; CI's `clickhouse` job downloads the binary and runs it.
- `a_data_value_in_describe_is_a_quoted_literal` — a value is data, and the agent reads
  `describe` as prose. A series key and a text column holding a value worded as an instruction,
  with a line separator, NEL, an ANSI escape, a bidi override and a quote in it: every appearance
  is a SQL literal with its quote doubled and those characters spelled out as `\u{…}`, and
  nothing in `describe` is a control, format or separator character. A column name that is not
  a plain identifier is a quoted identifier the same way.
- `a_hosts_describe_quotes_data_values_and_names_duckdbs_types` — the same through the host
  protocol's `describe`, with DuckDB's type names.
- `the_clickhouse_source_looks_the_same_as_a_file` — the sixth way: the built-in ClickHouse
  source (REQ-16) reading the fixture from a server table typed `DateTime('UTC')`,
  `LowCardinality(String)` and `UInt8` answers byte for byte as the CSV does.

## REQ-16 — ClickHouse is a built-in source

`type = "clickhouse"` in a config puts ClickHouse tables, or the result of a query, on the bench
without a connector ([docs/SOURCES.md](docs/SOURCES.md)). benchy reads the schema, converts each
column in the SELECT to the one type the bench gives its kind of value, pushes the narrowing down,
asks what the load will cost, and streams ClickHouse's `ArrowStream` answer into the same
validating reader an exec connector's stdout goes through. The fetch runs in a loader that holds
that source's credentials and nobody else's.

The integration tests run against a real ClickHouse server that they start from
`BENCHY_CLICKHOUSE_BIN`; without it they say they were skipped. CI's `clickhouse` job downloads
the official binary, pinned by version and SHA-512.

- `clickhouse_types_map_to_the_canonical_vocabulary` — every integer to BIGINT but `UInt64`,
  which does not fit and is held as HUGEINT, as from every source (REQ-15), every float to DOUBLE, every instant to TIMESTAMPTZ in microseconds and
  UTC, `Date` to DATE, text-like types to VARCHAR, `Nullable` and `LowCardinality` to what they
  wrap; each conversion ends in the ClickHouse type whose Arrow form is the bench's.
- `unsupported_clickhouse_types_become_json_text_or_are_dropped` — containers arrive as JSON text,
  integers past 64 bits and decimals past 38 digits as exact text; aggregate states and intervals
  are not loaded, saying how to load them instead.
- `identifiers_are_quoted_whatever_they_hold` — a backquote, a backslash or a newline in a name
  cannot end the identifier it is in.
- `values_travel_as_query_parameters_never_in_the_sql` — the window and the entity ids are query
  parameters, percent-encoded on the URL; none is in the SQL text, however hostile.
- `a_time_window_must_say_its_offset` — a window without an offset means a different instant to
  each reader, so it is refused at startup.
- `a_clickhouse_url_cannot_carry_a_password` — a URL reaches logs; a user or password in it is
  refused, naming where they go instead.
- `a_multi_frame_zstd_body_is_read_to_its_end` — a zstd stream of several frames is read whole,
  and one cut short fails rather than ending as if complete.
- `a_bomb_is_decoded_only_as_far_as_it_is_read` — a zstd body that would decode to 1 GiB, read
  to a 1 MiB cap, gives the cap and has taken less than one frame's few KiB off the wire: the
  decoder works as the reader pulls, so a bomb is stopped at the cap, never decoded whole.
- `a_zstd_window_past_32_mib_is_refused` — a frame header is the server's to write, and one that
  asks for a 64 MiB window is refused; 32 MiB is within the bound.
- `an_unset_compression_follows_where_the_server_is` — without `compression`, a server on
  loopback (`localhost`, 127.0.0.0/8, ::1) is asked for nothing compressed and any other for
  zstd; what the operator sets is used wherever the server is.
- `a_clickhouse_table_loads_with_its_schema_read_and_mapped` — against the server: every kind of
  column loads as the bench's type with its exact value, a Tokyo `DateTime` the same instant in
  UTC, and an aggregate state is left out.
- `the_same_rows_from_csv_and_clickhouse_describe_and_answer_identically` — origin-blind: the same
  rows from a CSV file and from a ClickHouse table with other types (`DateTime`,
  `LowCardinality(String)`, `Nullable(Float32)`, `UInt16`) give the same column types, the same
  `describe`, profile included, and the same rows.
- `the_window_and_the_allowlist_are_pushed_down_as_parameters` — ClickHouse returns only the rows
  inside the window and the allowlist, and ids a careless connector would splice into SQL arrive
  intact.
- `only_the_pinned_or_named_columns_are_fetched` — ClickHouse reads no column it was not asked for.
- `a_pinned_schema_that_does_not_match_refuses_the_load` — a pinned column that is missing, loads
  as another type, or holds a null it was pinned without, refuses the load.
- `an_over_budget_clickhouse_load_is_refused_before_transfer` — over the source's cap or the
  session's budget, the load is refused on the estimate: ClickHouse counted and sent no row.
- `a_compressed_response_is_decoded_and_bounded` — zstd moves a quarter of the bytes or fewer, and
  the cap counts the decoded Arrow: a cap between the two sizes refuses the load.
- `clickhouse_failures_map_to_fault_codes` — a wrong password is `E_AUTH` and is not in the
  message, nothing listening is `E_UNAVAILABLE`, an unknown table `E_CONFIG` with ClickHouse's own
  code for the operator, and a slow query times out at its deadline.
- `a_clickhouse_response_is_held_to_what_a_connectors_stream_is` — a server is trusted no more
  than a connector, and needs none to test: views that unpack past the cap, a zstd body that
  decodes past it (decoded as it is read, so the read stops at the cap), and an exception written
  after a whole stream are each refused, and what was read is spent on the budget.
- `a_clickhouse_error_reaches_the_operator_not_the_agent` — ClickHouse's message, an error status
  with its exception code or an exception after the stream, names hosts, databases and users; it
  goes to the operator's log with the mapped code, and the agent hears only that the table is not
  available.
- `a_failed_clickhouse_fetch_is_charged_when_served` — in a server, a ClickHouse fetch that fails
  is charged what it read before the next source is fetched.
- `clickhouse_credentials_stay_in_the_loader` — served, the password from benchy's environment
  signs in, and is in neither the engine's environment, arguments nor memory; a wrong one stops
  startup with `E_AUTH`, and neither password reaches the operator log.
- `a_read_only_clickhouse_user_loads_what_any_user_loads` — a `readonly = 1` user, which may
  change no setting, is sent none once it has been refused them, and loads the same columns and
  rows as any user, with each `compression`, LZ4 bodies being ClickHouse's default. A profile
  that makes zstd Arrow bodies or binary text, which a read-only user cannot change, refuses the
  load before ClickHouse sends a row, naming the setting.
- `check_names_the_fix_for_each_clickhouse_failure` — `benchy-mcp check` prints the mapped schema
  and the estimate, says when a user is read-only and which of ClickHouse's settings its load will
  run with, and names the fix for an unknown table, a wrong password, a read-only user's profile
  that makes Arrow benchy does not read, and an address with nothing listening. Each is one
  failure: the steps after it say they were skipped.
- `a_clickhouse_source_refuses_what_it_cannot_apply` — a key of another kind of source, an `env`
  name it would hand to nothing, `tables` with `query`, a URL with a password, a window without
  an offset, columns both pinned and named: each stops startup. One source listing two tables is
  two tables.

## REQ-17 — HTTP is a built-in source

`type = "http"` puts a URL on the bench without a connector: a body of Arrow IPC, Parquet, CSV or
NDJSON, gzip- or zstd-compressed or not, or a JSON API that answers in pages, the rows at a path
in each ([docs/SOURCES.md](docs/SOURCES.md#http); the design and its measurements,
[docs/HTTP-SOURCE.md](docs/HTTP-SOURCE.md)). Each format takes its fastest measured path into
DuckDB and ends where a file or a stream ends, so an HTTP table is the file's table (REQ-15). The
fetch runs in a network loader of its own (REQ-3), holding that source's credential and no
other's (REQ-5).

- `every_http_config_the_design_shows_is_accepted` — every format and every way of paging,
  written as the design shows them, is a source a session loads.
- `an_http_source_refuses_what_it_cannot_apply` — a key of another kind of source, a format or a
  paging style that does not exist, a paging key of another style, a pushdown parameter with no
  narrowing to carry, a parameter set twice or by both the operator and benchy, a header benchy
  sets itself, columns both pinned and named, a cursor with no key to be kept in, a rows path
  inside the cursor's, and a `delimiter` on another format, longer than one character, the quote
  or a line break: each stops startup.
- `an_http_secret_comes_only_from_env_through_auth` — a user, password or query string on the
  URL, a credential header in `headers`, `${…}` outside `env`, an `env` key `auth` does not read
  and an `auth` key `env` does not set are refused, and no refusal repeats the secret.
- `a_repeated_cursor_fails_the_source` — a cursor the API sent before, even two pages back, fails
  the source instead of paging forever; the refusal says nothing was loaded.
- `an_api_that_ignores_its_page_parameter_fails_the_source` — a page byte for byte the one
  before it is a loop.
- `offset_paging_advances_by_the_rows_received_and_ends_only_at_an_empty_page` — a short page
  may be the API's own cap on page size, so only an empty page ends the source.
- `more_pages_than_max_pages_fails_rather_than_truncates` — past `max_pages` the source fails;
  it never ends early with what it had.
- `a_next_url_on_another_origin_is_refused` — a `Link` or body URL on another scheme, host or
  port would carry the source's credentials there; the refusal names only where it pointed.
- `link_header_next_is_followed_on_the_same_origin` — RFC 8288 `rel="next"`, resolved against
  the page it came from.
- `every_paging_style_loads_every_page` — cursor, offset, page number, `Link` and next URL,
  rows at the top of the page or nested, each loads every row, asking no more often than its
  style needs.
- `a_declared_csv_is_read_in_its_stated_dialect` — a declared CSV is read with `delimiter` (`,`
  unless set) and RFC 4180's quote and escape stated, not sniffed, and matched by its header's
  names: a body split on `;` is refused naming `delimiter` until it is set, then loads as the
  comma body does; a day-first date still loads as the file does; quotes escaped with a backslash
  are refused, and leave no table.
- `every_http_format_loads_as_the_same_table_as_its_file` — REQ-15's fixture served as Arrow,
  Parquet, CSV, NDJSON and cursor-paged JSON answers `describe`, `typeof` and six queries byte
  for byte as the same CSV file does.
- `an_http_table_has_the_physical_layout_of_a_file_table` — 840,000 rows in each format, the
  JSON 10,000 to a page: `pragma_database_size()`, `pragma_storage_info` and the catalog views
  answer the same as for the CSV file, and name nothing of the server.
- `a_body_read_into_its_table_is_held_and_refused_whole` — a Parquet body goes straight from
  DuckDB's reader into its table, read to one row past its cap: past the cap, or with a null in a
  column pinned `nullable = false`, it is refused, the write rolls back, and the bench file keeps
  no block of it; one that fits loads every row.
- `an_http_error_reaches_the_operator_not_the_agent` — a status, the API's message, the URL and
  the host go to the operator log, the key the API echoed masked; the agent hears the table is
  not available, and nothing of where it came from or why.
- `http_credentials_stay_in_the_loader` — served, the token signs in, and is in neither the
  engine's environment, arguments nor memory; a wrong key sent as a parameter, which the API
  echoes, is masked in the log.
- `an_http_loader_is_confined_before_its_first_request` — the HTTP loader is the network loader
  of REQ-3: Landlocked before its first answer arrives, it still loads a CSV body through its
  spool and a JSON page through the sniff, both in its staging directory.
- `a_failed_page_fails_the_source_with_no_partial_table` — a page that errors past its retries,
  is cut short, is not JSON or has no rows leaves no table, not the pages before it; a CSV or
  NDJSON body cut short, which DuckDB's reader would take for a whole one, fails too.
- `a_csv_or_ndjson_body_is_spooled_to_staging_within_max_bytes_and_removed` — a CSV or NDJSON
  body is written to a regular file in the loader's staging directory as it arrives (not a pipe,
  which DuckDB reads on one thread), then read as that file is; the spool never holds more than
  `max_bytes`, a body past it is refused without waiting for the rest, and the spool is gone
  after the load, whether it loaded, was cut short or was too large.
- `a_failed_http_fetch_is_charged_when_served` — what a failed fetch read is spent on the
  session's budget before the next source is fetched.
- `an_http_body_is_held_to_max_bytes_as_it_decompresses` — a gzip or zstd body, by
  `Content-Encoding` or by `compression`, that decodes past `max_bytes` is refused at the cap,
  having decoded no more than the cap and a byte.
- `a_parquet_body_over_max_rows_is_refused_from_its_footer` — the footer's row count is checked
  before a row group is read: a body whose row groups are garbage is refused on it, and its
  staged file is gone.
- `a_later_json_page_with_a_wider_type_fails_the_source` — an undeclared JSON source is typed by
  DuckDB's sniff of its first page; a later page that does not fit, `2.5` where the first held
  integers, fails naming the column rather than loading as another type, or as `2`.
- `json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order` — a paged JSON source's next
  page is asked for once a scan has read the page's cursor, while the page decodes on another
  thread, at most a few pages ahead; the rows enter the table in page order whichever page
  decodes first (each row's `rowid` is its place in the source); and a page that fails its
  decode fails the source as that page, with nothing loaded. Decoded a page at a time,
  http-json-1m's first `describe` took 1.13 s; fetched ahead, 0.68 s.
- `a_pages_scan_reads_its_rows_and_cursor_as_its_decode_does` — the scan the pager follows and
  arrow-json's decode, which must agree or the source fails, read the same rows and cursor on
  every page the scan reads plainly: escapes, numeric and boolean cursors, null, missing, under a
  null parent, keys in any order. A page it does not read plainly is decoded before the next is
  asked for.
- `the_narrowing_is_pushed_down_as_the_operators_parameters` — the window, in each
  `time_format`, the entities and the columns travel in the parameters the operator named, and
  are re-applied after ingest to an API that ignores them.
- `a_refused_pushdown_parameter_fails_naming_the_parameters_sent` — a 400 to a request carrying
  pushdown parameters fails at once with `E_CONFIG`, naming them, never retried without them.
- `retries_back_off_and_honour_retry_after_within_the_deadline` — a 429 or 503 is retried after
  `Retry-After` or a growing, jittered wait, never past `deadline_secs`, and a 4xx other than 408
  and 429 is not retried; a server that never answers is held to the deadline.
- `retry_after_is_seconds_or_an_http_date` — `Retry-After` in either of RFC 9110's forms.
- `a_redirect_off_the_origin_carries_no_credential` — a same-origin redirect is followed with
  the credential; one to a host and port in `redirect_hosts` without the `auth` header, the `auth`
  parameter or a secret the API echoed into its URL; any other is refused, naming only where it
  pointed, and nothing is sent there. The loader may connect to those ports alone.
- `check_shows_the_request_the_schema_to_pin_and_the_first_pages_rows` — `benchy-mcp check`
  prints the request as sent with the secret masked, the first page's row count, and the schema
  read as `[[source.schema]]` entries to pin; it asks for one page only.

## REQ-18 — Helpers the operator provides

An operator, or an integrator deploying benchy for a host, may give the agent SQL helpers: views,
scalar macros and table macros, in the config's `[helpers]` (a SQL file, or the SQL inline), for
`benchy-mcp <dir>`/`--config` and `benchy-mcp host --config` alike. They are fixed for the
session: read at startup, and no MCP tool or host-protocol call adds, changes or removes one.
They are SQL the agent could write itself; what they save is the steps it takes to write it.

DuckDB 1.5.5 binds a view's or a macro's body when it is created, so a helper over a table the
bench does not have yet cannot be created then. A helpers job (`Job::Helpers`, a loader confined
to its staging directory and locked down as the engine is) makes them in a file of their own
against empty stand-ins of the bench's tables, and the engine attaches that file `READ_ONLY`
between the agent's own schema and the tables. DuckDB binds each helper again on every query, so
it reads the table that is on the bench when it is called. A helper names tables unqualified:
the catalog a table lives in is renamed by every attach, and no object of the agent's may take a
table's name (REQ-9), nor the name of a table a helper reads, attached or not, so the unqualified
name can only mean the host's table. After a detach a helper stays made, and fails when called
until a table of that name is attached again.

- `a_helper_from_config_serves_read_only` — the example helpers answer like built-ins; `describe`
  lists each with its signature and the comment above it, and nothing of where it came from;
  `DROP` is refused by DuckDB, whose read-only catalog alone keeps them where no sandbox runs.
- `a_helper_that_shadows_a_builtin_is_refused_at_startup` — a helper named like a built-in
  function, table function or view, by DuckDB's own catalog read through `system.main`, like a
  table, or with benchy's `benchy_` prefix, stops startup and says why; so does one that reads a
  table no source of a server's session provides.
- `a_helper_named_like_a_parser_keyword_is_refused_at_startup` — a macro helper named like a word
  DuckDB's parser reads itself (`coalesce`, `try`, `percentile_cont`, …), which no catalog lists,
  stops startup and says why: a call by its plain name would be DuckDB's own. The set is the one
  the agent is held to (REQ-9 `a_name_the_parser_reads_itself_is_taken`), found by DuckDB's
  parser in the helpers job, not a second list. A view under such a name stays allowed: FROM
  reads it as a name.
- `an_agent_cannot_replace_or_shadow_a_helper` — a helper's name is taken in every namespace, by
  any spelling: an object of the agent's created or renamed into it is refused and undone (REQ-9's
  check), and a `CREATE OR REPLACE`, `DROP`, `ALTER`, `COMMENT` or `CREATE` in the helpers'
  catalog by its qualified name is refused by DuckDB.
- `a_write_to_a_helper_is_told_it_is_a_helper` — DuckDB's refusal of a `DROP` or `ALTER` of a
  helper is said of a helper, with the hint to make one's own under another name, not as a write
  to a loaded table with the hint to copy it into a table.
- `helpers_survive_a_rebuild` — an engine rebuilt after the hard deadline or the memory
  ceiling killed it is given the helpers before its tables, as read-only and as taken as before.
- `a_helper_cannot_reach_outside_the_bench` — a helper that reads a file or the network when it
  is made is refused by the engine's lock and stops startup; one that needs an extension does too,
  and so does one that keeps an ENUM, a view with an ENUM column or a macro whose body spells one,
  held to the agent's rule (`the_agent_keeps_no_enum`: DuckDB holds an ENUM's values outside its
  memory budget); any statement but `CREATE VIEW` or `CREATE MACRO` is not a helper; a macro that
  reads a file only when called is refused by the engine's lock then, as the agent's own SQL is.
- `a_lateral_macro_call_equals_the_per_id_call` — on DuckDB 1.5.5 a macro's argument is
  substituted into its body, so a body that names its arguments beside an alias of its own binds
  the caller's `m.meter` to that alias, measured: a naive table macro gave every meter the count
  of all of them through `LATERAL`. Read once in a FROM-less CTE, as the example helpers do, a
  `LATERAL` call equals the per-id calls. docs/DIALECT.md says so to the agent.
- `a_view_over_a_missing_table_errors_until_attached` — through the host protocol: a helper
  over a table the host has not attached waits, `describe` says for what, calling it says so, and
  the attach of that table makes it; detached again, the view says which table it misses.
- `a_table_macro_over_a_missing_table_waits_like_a_view` — DuckDB 1.5.5 does not bind a table
  macro's body where it reads an argument, as a FROM-less CTE of the arguments does, so such a
  macro over a missing table was made and failed when called with a bare Catalog Error. Each
  macro is bound once more as a call, all arguments NULL, prepared and never run; one that reads
  a missing table, or a helper that waits, waits like a view and is made by the attach.
- `a_host_cannot_attach_under_a_helpers_name` — an attach under a helper's name, made or
  waiting, is refused with `E_NAME`.
- `a_table_a_helper_reads_stays_taken_after_a_detach` — through the host protocol, from a host's
  report: after `readings` was detached, the agent's `CREATE TABLE readings AS SELECT 'FORGED'
  …` was kept, the helper over it served the agent's rows, and the host's attach of `readings`
  again got `E_NAME`. Every table a helper reads is taken for the agent for the session, on the
  bench or not, by any spelling, created or renamed into. DuckDB finds them, benchy parses no
  SQL: a view's bound plan (`json_serialize_plan`, before the optimizer), and a macro's body as
  DuckDB's parser reads it (`json_serialize_sql`), kept to the tables on the bench, since a
  macro is bound only where it is called and a call with NULL arguments folds a subquery away.
- `a_cte_cannot_stand_in_for_a_table_a_helper_macro_reads` — from a host's report: DuckDB 1.5.5
  expands a macro, table or scalar, inside the caller's query, so `WITH meters AS (…)` made a
  helper macro over `meters` read the CTE, with no error; a view reads the table. In a batch
  that calls a helper macro (below), a CTE named, by any spelling, like a table or helper view a
  helper macro reads is refused before anything runs, saying to pick another name: at the top,
  nested in a CTE, in a subquery or an expression, `WITH RECURSIVE`, in a batch beside a SELECT,
  and in a CREATE TABLE AS, CREATE VIEW, CREATE MACRO or INSERT, where `json_serialize_sql`
  cannot read the whole statement. Other CTE names, a table only a view reads, the name in a
  string literal, and a session without helpers are not refused.
- `a_cte_in_any_statement_cannot_stand_in_for_a_table_a_helper_macro_reads` — from the same
  host: `MERGE … USING (WITH meters AS …)` made a helper macro count the CTE's 999 rows and
  wrote them into the agent's table. Measured on DuckDB 1.5.5, the macro binds to the caller's
  CTE wherever a WITH-led query can stand, and each is refused before anything in the batch
  runs: MERGE's USING, ON, `WHEN … AND` and actions (UPDATE SET, INSERT VALUES); UPDATE's SET,
  FROM and WHERE; DELETE's USING and WHERE; INSERT's VALUES and BY NAME, and a query before
  RETURNING or ON CONFLICT; a PIVOT's or UNPIVOT's source; a WITH ahead of an INSERT, UPDATE,
  DELETE, MERGE or PIVOT; lower case, after non-ASCII text, and beside other statements in a
  batch, `;` in a string among them. benchy gives DuckDB the text from each WITH and cuts it
  where DuckDB's syntax error says the query ends, or, when DuckDB reads it whole as something
  other than SELECTs, at a `;` or ahead of the statement keyword after the CTEs. Other CTE names
  in these statements, a name in a string (after non-ASCII text too, which killed the engine
  before), and `WITH ORDINALITY` are not refused. A batch that
  would need more than 64 parses is refused rather than let through unread. A whole WITH query
  inside a string of a statement other than SELECT, with a CTE under one of those names, is
  refused too (not tested: a false refusal, not a wrong number).
- `a_cte_named_like_a_helpers_table_is_refused_only_beside_a_helper_call` — from a host whose
  helpers read `hourly`: every `WITH hourly AS (…)` was refused, though only a macro expanded in
  the same query reads the CTE. Measured on DuckDB 1.5.5: a macro of the agent's that calls a
  helper macro reads the caller's CTE too; a view does not, being bound on its own; and a view
  or macro whose body holds the CTE reads it through any macro that body calls, once that macro
  is replaced by one that calls a helper. So the CTE rule above applies only to a batch that
  names, as a word, a helper macro, a macro of the agent's that calls one at any depth (read from
  the catalog's macro bodies), or `view`, `macro` or `function`; every other CTE is the agent's,
  whatever its name. The refusal says which call or word it is, and that the name is fine in a
  batch that calls no helper macro. DuckDB refuses a subquery, a macro's included, in a DEFAULT,
  CHECK, generated column and index expression, so nothing else keeps a body.
- `sql_run_from_a_string_is_refused_beside_helper_macros` — from a host's report: `SELECT * FROM
  query('WITH meters AS (…) SELECT * FROM total_kw(''m0'')')` returned the CTE's value, and a
  view over it kept doing so. DuckDB parses a string's SQL only when it runs, from a string that
  can be built then (`||`, `replace()`), so no look beforehand reads it. With a helper macro that
  reads a table on the bench, a batch that may call `query` or `json_execute_serialized_sql`
  (which reached a helper by its catalog's name, measured) is refused before anything runs, by
  any spelling: quoted, qualified, with a comment before its `(`. `query_table`, which reads only
  a table named in its string, and a column named `query` stay allowed, and so does `query()`
  without helper macros.
- `a_helper_that_names_a_table_in_a_string_is_refused_at_startup` — a table named in a string,
  `query_table('meters')`, `query('SELECT … FROM meters')`, `pragma_table_info('meters')`,
  `duckdb_table_sample('meters')`, is in no parse as a table, so it could not be taken from the
  agent: a helper macro that calls one of DuckDB 1.5.5's table functions that read the bench by a
  string, found in its body as DuckDB parses it, stops startup and says why, as does a view whose
  bound plan still calls one. A view over `query_table` or `query` stays allowed: its plan is
  bound to the scans they make, so its tables are found.
- `the_table_a_waiting_helper_needs_is_taken` — the table a waiting helper waits for is taken
  from the start, so the agent cannot occupy it and the host's attach makes the helper.
- `the_tables_helpers_read_stay_taken_across_a_rebuild` — the server keeps the tables the helpers
  have read and gives them to every engine it starts, so a rebuilt engine takes a detached one
  too.
- `a_table_the_agent_had_first_stays_its_own` — DuckDB names a waiting helper's missing tables one
  at a time; a table it names later that the agent already has stays the agent's, so its later
  statements are not refused for it. The helper waits, reading nothing of the agent's, and the
  host's attach of that table is refused until the agent drops its own; then it is taken.
- `a_helper_is_a_create_view_or_macro_with_a_plain_name` — the config's SQL is split where
  DuckDB would split it and each statement read for its kind and name before anything runs.
- `helper_sql_splits_where_duckdb_would` — quotes, comments and dollar quotes hold their `;`,
  and the `--` lines directly above a statement are its comment.

A helper costs nothing to a session that has none: no job runs and no catalog is attached.
Measured in docs/PERFORMANCE.md, "Helpers".
