# Requirements

What benchy is held to. Each requirement names the tests that prove it, and
`scripts/check_requirements.py` fails CI when a named test does not exist, when a planned one
already does, or when a comment cites a test that is not there. That keeps this file from naming
evidence that is missing; whether a test proves its bullet is for review to judge. A test marked
*(planned)* is a known gap: the requirement is not met yet, and the bullet says what happens today.

A named test is only evidence if it would fail without the thing it guards. The guards that
matter most were mutation-checked when they were written: remove the guard, watch the test go
red.

Each bullet says what its test proves. How a gap was found, what happened before it was closed
and the measurements behind it are in that test's doc comment, and the longer narrative that
belongs to no one test is in [docs/history/REQUIREMENTS-history.md](docs/history/REQUIREMENTS-history.md).
The threat model these requirements serve is in [SECURITY.md](SECURITY.md#threat-model).

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

- `cross_session_canary` — a canary in one session's scratch is invisible from another session
  through every catalog view.
- `schema_has_no_addressing` — no tool parameter, at any depth, can name a path, file, directory,
  workspace, session, database, URL or host.
- `neighbour_unaffected` — a runaway in one session leaves the other correct and responsive (the
  isolation is of state and failure, not CPU).
- `path_disclosure_names_only_our_own_private_workspace` — catalog functions that disclose a
  database path disclose only this session's private, temporary workspace.
- `benches_opened_together_never_share_a_directory` — benches opened at the same moment get
  directories of their own.

Nothing outlives the session. Everything it keeps on disk — the store of what was loaded, each
engine's workspace, a loader's staging — is in one directory of its own, created fresh with mode
0700.

- `a_session_leaves_nothing_behind` — the directory is gone once the client closes the pipe.
- `a_stopped_server_leaves_nothing_behind` — and once the server is stopped by SIGTERM.
- `a_killed_server_leaves_no_data_behind` — and after a SIGKILL: the engine exits, and the
  session's warden removes the whole directory.
- `a_killed_server_takes_a_running_connector_with_it` — killed mid-load, the connector and its
  process group are stopped, and its scratch is removed for good.
- `the_warden_removes_only_a_session_directory` — the binary takes no argument that makes it
  remove a directory, whatever directory it is given.
- `only_a_session_directory_is_the_wardens_to_remove` — the warden removes only a directory
  benchy could have made as a session.
- `the_warden_holds_nothing_of_the_servers` — the warden has no credential in its environment or
  memory, one thread, and no descriptor but the server's pipe and `/dev/null`.
- `the_warden_is_forked_only_from_one_thread` — forking the warden from a process with a second
  thread is refused (Linux only: its threads are counted in /proc).
- `a_signal_to_the_warden_is_not_the_server_going` — a signal to the warden does not end the
  session.
- `a_load_that_times_out_leaves_no_connector_and_no_scratch` — a load past `--load-timeout`
  stops the loader's process group, connector and all.
- `a_connector_starts_with_no_signal_blocked` — a connector starts with no signal blocked, so the
  signal asking it to stop reaches it.
- `a_failed_rebuild_tells_the_agent_no_path` — a failed rebuild says so to the agent; the reason,
  which names the operator's files, goes to the operator's log.

## REQ-2 — Sealed SQL

The query engine cannot reach the filesystem or the network. The **locked engine** is the
boundary: external access off, extension installing and loading off, configuration locked — and
proven at startup by trying to undo it. The statement allowlist's job is to stop that lock being
removed (`SET`, `RESET`, `INSTALL`, `LOAD`) and to refuse `ATTACH`, `COPY` and `EXPORT`,
though under the lock DuckDB already refuses to prepare the file forms of `COPY` and `EXPORT`.
The allowlist alone is not sufficient: file-reading table functions are typed `SELECT`, and only the
engine lock stops them.

A statement's type does not say what its table functions do, either: `enable_logging()`,
`enable_profiling()` and `checkpoint()` are called in a `SELECT`, reach nothing outside, and so
pass both the statement allowlist and the lock. The second half of the allowlist is one of
DuckDB's table functions (`guard::TABLE_FUNCTIONS`): each is the agent's, left to the lock (the
file and network readers), or refused by name, checked on the batch's text before any of it is
prepared. A table function DuckDB adds is refused until it is classified. `query()` and
`json_execute_serialized_sql` are refused with them, since SQL run from a string cannot be read
before it runs.

- `nothing_in_the_corpus_reaches_outside_the_workspace` — no case in `corpus/escape.toml` returns
  a canary planted outside the workspace.
- `a_denied_statement_says_what_is_allowed` — a refusal names what is allowed.
- `a_refused_statement_stops_the_batch_before_anything_runs` — nothing after a refusal runs.
- `file_readers_are_select_typed_so_layer_one_cannot_stop_them` — file readers pass the
  statement allowlist; the engine lock is what stops them.
- `the_recorded_division_of_labour_still_holds` — each corpus case is classified as the corpus
  records, so a re-typed statement fails a test.
- `the_corpus_records_why_the_engine_is_the_boundary` — on a locked engine with no allowlist each
  case fails or succeeds as recorded, and each case the allowlist lets through reaches the canary
  only without the lock.
- `checked_per_statement_before_run` — each statement is checked again just before it runs, so
  one that depends on an earlier statement of the batch is still refused.
- `a_dynamic_pivot_still_runs` — a dynamic `PIVOT`, which DuckDB splits into dependent
  statements, runs.
- `a_pivot_into_a_table_runs` — `CREATE TABLE … AS PIVOT` and `INSERT … PIVOT` run; a transaction
  the agent writes is still refused, and a pivot over a table made in the same call is refused
  with what to do instead.
- `each_statement_kind_is_named_as_duckdb_types_it` — a refusal names the kind DuckDB types the
  statement as, and every kind the classifier can meet under the lock is pinned.
- `merge_is_ordinary_dml` — `MERGE` works on the agent's own tables like `INSERT` and `UPDATE`,
  and reads no files.
- `a_table_function_that_changes_the_engine_is_refused` — `enable_logging`, `enable_profiling`,
  `checkpoint`, `force_checkpoint` and `truncate_duckdb_logs` are refused, and the session goes on.
- `a_refused_call_runs_nothing_of_its_batch` — nothing of a batch with a refused call runs,
  wherever the call is written: in a CTAS, an `INSERT`, an `UPDATE`, a view, a macro, `query('…')`.
- `a_refused_function_is_refused_however_it_is_written` — in any case, quoted, qualified, or with
  a comment or white space before its `(`.
- `a_refused_name_in_a_string_literal_is_only_a_string` — a refused name inside a string
  literal (`'…'`, `E'…'`, dollar-quoted) is not refused.
- `a_refused_call_beside_a_literal_is_refused` — a refused call after a literal, or after what
  only looks like one, is refused; an unterminated literal, quoted name or comment is read on.
- `a_scan_that_does_not_move_on_stops` — a step of the text's scan that does not move forward
  stops it, and the batch is refused rather than scanned for ever.
- `the_literals_the_check_skips_are_duckdbs` — each literal the check skips, and each call it
  reads, is one by DuckDB 1.5.5's own parse.
- `what_ends_a_name_that_is_not_called` — a column, alias or table named like a refused
  function, or a longer name containing one, is not refused.
- `every_table_function_duckdb_has_is_classified` — each of the shipped DuckDB's table functions
  and table macros is classified exactly once, and no refused name is also a scalar, aggregate
  or macro.
- `an_engine_it_was_not_measured_on_reads_its_own_catalog` — on another DuckDB, an unclassified
  table function is refused, and an unreadable catalog refuses every batch.
- `the_table_functions_left_to_the_lock_are_refused_by_it` — each file and network reader left
  to the lock is refused by the locked engine.
- `the_pragmas_typed_select_are_the_measured_ones` — the PRAGMAs DuckDB rewrites to a SELECT are
  pinned, and the two that do more than read the catalog are stopped.
- `the_agents_table_functions_still_run` — `range`, `generate_series`, `unnest`, `json_each`,
  `summary`, `query_table`, `histogram`, the catalog's functions, views and PRAGMAs, `SUMMARIZE`
  and `DESCRIBE` run.
- `the_table_function_allowlist_does_what_the_corpus_says` — the table-function allowlist alone
  refuses a corpus case exactly when the corpus records `functions = "refuse"`.
- `explain_cannot_smuggle_a_refused_statement` — `EXPLAIN` is refused, since `EXPLAIN ANALYZE`
  runs what it explains.
- `the_workspace_file_cannot_be_read_as_a_file` — `read_blob` of the workspace file's path reads
  nothing.
- `nothing_is_left_to_read_in_the_attach_directory` — each file in the attach directory is
  unlinked as soon as it is attached, whatever happened.
- `an_attached_file_brings_its_table_and_nothing_else` — an attached file holding anything but
  its one table is refused before the agent can see it.
- `a_function_in_one_file_of_a_batch_refuses_that_file_alone` — in a batch attach, the file that
  brings a macro is refused before any catalog reaches the search path; the others attach.
- `anything_else_in_one_file_of_a_batch_refuses_that_file_alone` — likewise a file with a view,
  a second table, a sequence, a type, a schema or an index, or without its table.

## REQ-3 — Sealed process

On Linux, Landlock confines every process that parses data. The engine starts sealed and
empty, loads nothing, and may read only its own workspace and delete within its session's
directory; each file is parsed by a loader of its own, which may read its one input and write
only its staging directory. A built-in source's loader (ClickHouse, HTTP) may write only its
staging directory, read only the resolver's files, and, from Linux 6.7, connect only to its
source's port. So neither a compromised engine nor a parser subverted by its input can read
other files, or truncate any outside a loader's own staging directory. A connector's loader is
not confined: a connector reaches whatever it fetches from. macOS has no equivalent: there,
containment is the SQL layer alone, and benchy refuses to serve without `--allow-unsandboxed`.

The TCP rules need Landlock ABI 4, Linux 6.7. On an older kernel every ruleset is enforced
partially: the files are confined as above, and the engine and every confined loader may open a
TCP connection anywhere. benchy serves there, since the filesystem boundary holds and DuckDB's
own network access is off (REQ-2), and says so: `hello` reports the posture as `partial` with
`network` false, the operator's log says the network is not confined, and a server started with
`--require-full-sandbox` does not start.

- `a_thread_already_running_is_never_confined_as_if_it_were_not` — a process confines itself
  only as its one thread; with another running, it refuses.
- `a_thread_that_stays_past_the_grace_is_refused` — a thread still running after the grace is
  refused.
- `a_thread_that_finishes_within_the_grace_is_waited_for` — one that ends within the grace is
  waited for.
- `a_process_already_alone_is_confined_at_once` — a process already alone confines at once.
- `a_connector_that_never_lets_go_cannot_stall_or_unconfine_the_engine` — a connector that never
  finishes reading its input neither stalls the load nor leaves a thread in the engine.
- `a_loaders_output_is_checked_before_it_is_kept` — the store keeps only the job's table name and
  a plain file in the job's staging directory, never a link.
- `doctor_says_in_json_what_each_mechanism_enforces` — `doctor --probe --json` prints one JSON
  object: a verdict that agrees with the exit status, the ABI, and each mechanism's checks.
- `the_doctor_json_has_the_documented_keys` — that JSON has exactly the keys the README lists.
- `doctor_without_landlock_says_not_enforced` — below the minimum ABI the verdict is
  `not_enforced` with a reason and a non-zero exit, and no mechanism is `enforced`.
- `doctor_probes_the_restriction_the_server_applies` — `doctor --probe` applies the engine's, a
  file loader's and a network loader's own restrictions and checks each right they grant and
  withhold.
- `a_confined_process_cannot_truncate_what_it_was_not_granted` — the engine can truncate
  nothing, and a loader only what is beneath its staging directory.
- `every_ruleset_builds_for_every_landlock_abi` — every ruleset builds for every ABI from V1 to
  V5.
- `the_truncate_probes_count_only_where_landlock_can_refuse_it` — below ABI 3 the truncate
  probes are reported and not counted.
- `a_network_loader_reaches_only_its_source_and_its_staging` — a ClickHouse loader may use only
  its staging directory, the resolver's files and (from Linux 6.7) TCP to its source's port.
- `a_missing_sandbox_is_refused_without_the_flag` — where the engine cannot be confined, it
  refuses to serve without `--allow-unsandboxed`, and says what is missing.
- `a_loader_that_cannot_be_confined_refuses_without_the_flag` — and so does a file loader.
- `the_tcp_probe_counts_only_where_landlock_can_refuse_it` — below ABI 4 the TCP probe is not
  counted, and only `EACCES` counts as Landlock's refusal.
- `a_kernel_without_tcp_rules_is_reported_as_partial` — below ABI 4 the posture is `partial`;
  only `full` reports `network` true and passes `--require-full-sandbox`.
- `doctor_without_tcp_rules_says_partial` — below ABI 4 `doctor --probe` says `partial`, with
  exit status 0.
- `hello_says_how_the_engine_is_confined` — `hello`'s `sandbox_posture` gives the posture in
  fixed words and booleans.
- `a_session_that_must_be_fully_sandboxed_refuses_to_start_otherwise` — `--require-full-sandbox`
  starts a server or host session only at `full`, overrides `--allow-unsandboxed`, and a server
  exits before any source loads.

## REQ-4 — Sources

Data is on the bench before the agent's first call, and there is no load tool: loading is not the
agent's job, so it costs the agent no steps. Three tiers exist — files, built-in sources
(ClickHouse, REQ-16; HTTP, REQ-17) and exec connectors.

- `a_directory_becomes_a_bench` — every supported file in a directory becomes a table named
  after it.
- `one_failed_source_does_not_cost_the_others` — a session starts with what loaded and lists
  the rest as not available, the reason in the operator's log; it exits only when nothing loads.
- `json_and_ndjson_load_too` — JSON and NDJSON load alongside CSV, TSV and Parquet.
- `an_unsupported_file_says_what_is_supported` — an unreadable format lists the readable ones.
- `a_name_collision_is_refused_with_both_paths` — two files that would make the same table are
  refused, naming both.
- `table_names_are_plain_identifiers` — a file name becomes a table name the agent can type.
- `a_skipped_file_is_named_in_the_operator_log` — each skipped entry of a folder is named in the
  operator's log, and not to the agent.
- `a_hidden_file_in_a_directory_is_not_a_table` — a folder's dotfiles are skipped.
- `a_hidden_file_named_explicitly_still_loads` — a dotfile named with `--source` or in a config
  loads.
- `the_help_example_names_the_binary_by_absolute_path` — `--help`'s JSON example names the binary
  by absolute path.
- `the_catalogue_is_bounded` — the catalogue is capped and deterministically ordered.
- `the_catalogue_carries_no_path` — the catalogue discloses no origin.
- `an_exec_connector_puts_rows_on_the_bench` — any program that writes an Arrow stream to stdout
  is a source.
- `an_exec_table_appears_in_the_catalogue` — a connector's table is listed like any other.
- `narrowing_holds_on_the_served_path` — narrowing applies on the server's loader path.
- `config_rejects_what_it_cannot_apply` — unknown keys, missing schemas, a column declared twice
  in any case, unresolvable variables and secrets outside `env` stop startup.
- `a_key_that_does_nothing_is_refused` — a key that cannot apply to its kind of source stops
  startup.
- `a_file_source_may_be_declared` — a file source's `[[source.schema]]` and `delimiter` are
  checked at startup, and the file loads as declared, with only the declared columns.
- `a_declared_file_loads_as_the_sniffed_file_does` — a declared CSV, TSV, NDJSON or Parquet file
  makes the table its sniffed read makes.
- `a_declared_file_that_does_not_match_fails_naming_why` — a declared file that does not match
  fails naming the delimiter or the column, and leaves no table.
- `a_rebuild_reads_nothing_again` — a rebuild after a hard kill reads neither the config nor any
  source again.
- `loading_never_downloads_an_extension` — a reader the build lacks fails at load time; nothing
  is installed on first use.
- `every_advertised_format_loads_in_the_shipped_build` — every format the README lists loads in
  the shipped build.
- `a_host_attaches_a_table_during_a_session` — a host can add a dataset at any time, read-only,
  without restarting the session (REQ-13).
- `an_attached_table_joins_a_running_session` — a table attached after the seal is queryable at
  once.
- `replacing_an_attachment_keeps_the_agents_tables` — attaching a name again replaces its data
  and keeps the agent's tables.
- `replacing_an_attachment_drops_the_results_that_could_hold_its_rows` — replacing a table drops
  the results that could hold its old rows.
- `a_failed_attach_changes_nothing` — a refused attach leaves the bench as it was.

## REQ-5 — Credentials stay out of the engine

The process that runs agent SQL holds data, never credentials. Connectors hold the credentials
and do the I/O, each run by a loader process of its own, and each gets only the environment its
operator listed.

- `the_connector_sees_only_its_own_environment` — a connector's environment is only its own
  listed variables.
- `entity_ids_travel_in_the_document_not_in_argv` — narrowing values reach a connector in its
  input document, never as arguments.
- `a_writable_connector_is_refused` — a connector that another owner, everyone or a shared group
  can write is refused: that is a credential handover.
- `a_personal_group_may_write_the_connector` — a personal group, with nobody else in it, may
  write it.
- `the_personal_group_is_recognised` — that lookup agrees with `id` and `getent` (Linux only: macOS has no
  `getent`).
- `credentials_never_cross` — a resolved `${VAR}` reaches only its connector's loader; it is
  nowhere in the engine's environment, arguments or memory.
- `the_engine_inherits_no_environment_and_no_descriptor` — the engine starts with an empty
  environment and no descriptor past stdio.

Out of scope: other processes of the same user. They can read a connector's environment through
`/proc`, as they can read the operator's config; benchy does not defend against its own user.

## REQ-6 — Connector output is untrusted

A connector is a third party, and its output heads for the model. It is validated against what
the operator declared, never trusted. A declared column accepts what it holds exactly — a
narrower integer, `float32` for `float64`, a timestamp at any precision, a string in any layout —
and never a type that could change a value (REQ-15).

- `a_surprise_column_is_refused_rather_than_dropped` — the schema is the operator's: an extra
  column is refused, never dropped.
- `a_connector_that_breaks_its_declaration_is_refused_when_served` — through a server's loader
  process too: startup stops, naming the column.
- `a_dictionary_encoded_column_is_unpacked_within_the_byte_bound` — a dictionary is unpacked
  into its values, its unpacked size counted against `max_bytes` as the stream is read.
- `a_view_column_is_unpacked_within_the_byte_bound` — so is a string or binary view: each counts
  the length it states.
- `a_large_dictionary_costs_each_batch_only_its_keys` — a batch costs its keys, not the
  dictionary.
- `a_compressed_stream_is_read_within_the_byte_bound` — LZ4 bodies are read, and `max_bytes`
  bounds what they decode to; zstd is refused, naming the setting that turns it off.
- `a_compressed_buffer_counts_what_it_decodes_to_and_may_not_lie` — a buffer's stated size is
  checked before arrow sees it, decoding at most one byte past it.
- `a_compressed_message_reaches_arrow_decoded_once` — the checked buffers reach arrow decoded,
  so LZ4 runs once.
- `a_claim_past_what_is_left_is_refused_before_it_is_decoded` — a claim past what the stream has
  left is refused before anything is decompressed.
- `a_hostile_schema_is_refused_not_a_panic` — a type arrow cannot build (an integer 7 bits wide)
  is a bad stream, not a panic.
- `our_refusal_is_not_hidden_by_the_connector_dying_of_it` — benchy's refusal is the verdict; the
  connector's death of the broken pipe that follows goes to the operator's log beside it.
- `a_not_null_column_carrying_nulls_is_refused` — nullability is checked in the data, not taken
  from the Arrow field flag.
- `a_stream_without_its_end_marker_is_refused` — only the end-of-stream marker tells a finished
  stream from one cut at a message boundary.
- `a_stream_that_fails_after_its_rows_leaves_no_table` — the table commits only on the stream's
  verdict, so a stream refused after its rows leaves no table.
- `a_stream_must_end_at_its_end_marker` — one byte after the marker refuses the load, and a
  helper left holding the pipe is not waited for; a stream of exactly `max_bytes` loads.
- `an_empty_stream_is_an_empty_table_but_no_stream_at_all_is_a_failure` — no rows is a complete
  stream; empty stdout is a failure.
- `the_row_cap_refuses_rather_than_truncating` — over a cap the load is refused, never shortened.
- `narrowing_is_reapplied_even_when_the_connector_promised_to_do_it` — entity narrowing is
  re-applied after ingest.
- `time_narrowing_is_reapplied_too` — and so is the time window, half-open; a row with no
  timestamp is out.
- `a_relative_window_is_sent_as_rfc3339` — `last = "30d"` reaches the connector as RFC 3339 with
  a `Z`.
- `a_row_with_no_entity_is_outside_the_narrowing` — a row that cannot be shown to be inside is
  outside.
- `narrowed_rows_never_reach_the_file` — an excluded row is never written to the workspace file.
- `a_connector_that_hangs_does_not_hang_the_host` — `describe` is bounded by its timeout.
- `a_connector_that_hangs_mid_fetch_is_bounded_too` — and `fetch` by its deadline.
- `noise_on_stderr_is_not_an_error` — stderr lines that are not control events are log text.
- `a_fetch_is_bounded_by_one_deadline_not_two` — the wait for the connector's exit gets what is
  left of the fetch's deadline.
- `a_finished_connector_leaves_nothing_running` — once a fetch ends, the connector's process
  group is stopped.
- `an_error_after_a_flood_of_logging_still_counts` — the first error event is the verdict however
  much came first, and the log keeps both ends of stderr.
- `a_connectors_stderr_line_is_bounded` — a line is kept to its first 64 KiB; non-UTF-8 bytes are
  log text, and later lines still arrive.
- `a_connector_error_reaches_the_operator_not_the_agent` — the connector's message and hint go to
  the operator's log with the code; the agent hears only that the table is unavailable.
- `an_unclassified_code_does_not_reach_the_sentence_lookup` — an unknown code is taken as
  `E_INTERNAL`, not echoed.
- `a_connector_must_tolerate_keys_it_has_never_seen` — every input document carries a reserved
  key.
- `exec_test_catches_the_mistakes_it_exists_to_catch` — `exec-test` names the fix for each
  failure it reports.
- `exec_test_names_a_fix_for_every_failure` — every failure code in the source comes with advice.
- `exec_test_passes_the_connector_we_ship` — the shipped example passes its own checker.
- `exec_test_skips_what_a_failed_fetch_makes_meaningless` — one mismatch is one failure; the
  checks that need a fetch say they were skipped.
- `a_type_mismatch_names_the_conversion` — each type mismatch names the producer-side fix, such
  as ClickHouse's `toDateTime64(col, 6, 'UTC')`.
- `an_oversized_request_starts_no_connector` — a request over the 1 MiB document cap starts no
  connector, for `estimate` and `fetch` alike.
- `a_hostile_entity_list_starts_no_connector` — nor do entity ids past the bounds: more than
  1024, longer than 512 bytes, or with a NUL or control character.

## REQ-7 — Ceilings

Every query batch runs under a wall deadline, DuckDB's memory budget and a ceiling on the
engine's resident memory (REQ-8), and what comes back is capped. Capacity is measured, not
claimed: `examples/capacity.py` produces `docs/CAPACITY.md` up to its ClickHouse section, whose
table `examples/capacity_clickhouse.py` prints.

Agent SQL runs on one thread unless the operator gives the engine more (`--threads N`, or
`[engine] threads`), so on a shared machine a runaway takes one core. A host may lower the
operator's number for its session and never raise it, and the agent cannot change it: the
configuration is locked after it is set. The threads are started after the engine is confined,
so each of them is (REQ-3).

- `the_engine_runs_agent_sql_on_the_threads_it_is_given` — one thread by default; with
  `--threads 4` a grouped aggregate keeps more than one core busy (Linux only: the engine's
  CPU is read from /proc).
- `the_engines_threads_are_the_operators_to_set_once` — `--threads` or `[engine] threads`, never
  both, never 0.
- `a_host_lowers_the_engines_threads_and_cannot_raise_them` — from protocol 7 a host may lower
  the threads, rebuilding the engine; raising them, 0, or sending them earlier ends the session.
- `the_engines_threads_are_started_after_it_is_confined` — every worker is refused a file outside
  the bench and a TCP connection; threads started before the confinement stop the engine
  (Linux only, as Landlock is).
- `a_deadline_of_zero_is_refused_at_startup` — at zero every query would fail at once.
- `ceilings_enforced` — the row cap, byte cap, memory budget and deadline each give a named error
  or a stated truncation, and the server answers afterwards.
- `a_deadline_holds_between_statements` — no statement starts past the deadline.
- `a_batch_that_finishes_is_not_killed_by_its_own_deadline` — a batch near its deadline either
  finished, leaving its table, or was stopped, leaving none.
- `a_trivial_query_does_not_pay_for_the_watchdog` — the watchdog adds under 5 ms to `SELECT 1`
  (lower quartile of 60 paired runs).
- `running_out_of_memory_says_how_to_use_less` — the memory error names the fix.
- `a_large_stream_is_not_held_in_memory_while_it_loads` — a loader's peak over a one-row load's
  is bounded by the stream's size.
- `a_large_csv_is_not_held_in_memory_while_it_is_staged` — a CSV's stage is held to 256 MiB and
  spills: the peak grows less than the file, and no spill is left.
- `a_wide_csv_is_not_held_in_memory_while_it_is_staged` — a CSV with a wide first line is read on
  one thread under the cap: the peak grows less than the file.
- `a_file_is_read_on_one_thread_where_its_first_line_is_wide` — judged from the first 64 KiB:
  one thread where two 256 KiB blocks a column and three 2,048-line vectors, per thread, pass
  half the cap. The harness's three-column files stay on every thread.
- `a_wide_json_file_is_read_on_one_thread_within_its_vector` — a wide JSON file is staged on one
  thread, its peak bounded by a row's width, not the file's length.
- `a_table_read_on_one_thread_or_staged_is_the_same_file` — however a table is staged or read it
  is the same file (REQ-15 `the_physical_layout_says_nothing_of_where_data_came_from`); a stage
  out of memory falls back rather than failing, and the bench's limit and threads are restored.
- `a_memory_limit_put_back_reads_as_it_did` — a memory limit put back after a stage reads as it
  did, in the buffer pool too.

## REQ-8 — Survivable failure

A runaway query loses the query, not the bench. The in-process interrupt stops execution; query
*planning* never checks it, so the engine is a separate process that the server kills and
rebuilds past a hard deadline, or past a ceiling on its resident memory. DuckDB's memory budget
does not bound planning either, and in a container one engine's growth is every session's OOM
kill. So is one loader's: each is read the same way while it loads, with the connector it runs.

- `a_runaway_batch_loses_the_batch_not_the_bench` — a runaway is interrupted at its deadline and
  the agent's tables survive.
- `a_request_the_engine_does_not_read_is_held_to_the_hard_deadline` — a request the engine never
  reads is answered `E_REBUILT` at the hard deadline.
- `planner_blowup_killed` — a query that takes seconds to plan is stopped by killing the engine;
  the bench is rebuilt and the lost scratch stated.
- `an_engine_past_its_memory_ceiling_is_killed_even_while_planning` — planning past
  `--memory-ceiling` is killed within moments: `E_REBUILT`, naming the ceiling, scratch gone.
  Memory is read every 50 ms during a request, never between (0 ms of CPU idle).
- `an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once` — read once more as a request is
  answered (5 µs), an engine past its ceiling is rebuilt then, not left idle past it (Linux
  only, where the ceiling is read from /proc).
- `a_memory_ceiling_the_engine_cannot_live_under_is_refused_at_startup` — a ceiling below
  `--memory`, or a size that does not parse, stops startup. Unset, the ceiling is `--memory` and
  half again, at least 256 MiB over (legitimate queries reached 1,260 MB at a 1 GB budget).
- `a_memory_ceiling_that_cannot_be_enforced_is_not_reported` — off Linux `limits.memory_ceiling`
  is `null`, `unenforced.memory_ceiling` says why, and `doctor` says it is not enforced.
- `a_session_that_must_have_memory_ceilings_refuses_to_start_without_them` —
  `--require-memory-ceilings` starts a session only where both ceilings are enforced
  (`ceilings_that_cannot_be_enforced_are_refused_when_required` for each).
- `a_loader_past_its_memory_ceiling_is_killed_and_attaches_nothing` — a loader past
  `--loader-memory-ceiling` (1 GB by default; the harness's 10M-row loads peak at 94–410 MB) is
  killed, attaches nothing and tells the operator why; the agent hears the table is unavailable.
  The watch costs loads nothing measurable (docs/PERFORMANCE.md, R6-L1).
- `a_connectors_memory_counts_against_its_loaders_ceiling` — a connector and every process it
  starts count with its loader, and are killed with its process group.
- `a_loader_memory_ceiling_is_set_in_one_place_and_must_be_a_size` — flag or config, never both,
  and a size that parses.
- `a_loader_memory_ceiling_that_cannot_be_enforced_is_not_reported` — off Linux it is `null` in
  `hello` (from protocol 6) with the reason, and `doctor` says so.
- `a_load_that_keeps_failing_is_not_retried_forever` — it fails once, after `--load-timeout`,
  and the server exits.
- `a_job_the_loader_does_not_read_is_held_to_the_load_timeout` — the load timeout covers handing
  the loader its job too: a 300 KiB job, more than the loader's stdin pipe holds, to a loader
  that never reads it fails `E_DEADLINE` at the timeout, attaches nothing, and the session serves
  on.
- `an_engine_that_dies_at_every_attach_is_not_rebuilt_forever` — a rebuild that loses its engine
  counts as failed, and a lost engine is noticed when it goes.
- `result_ids_never_outlive_their_workspace` — after a rebuild, ids from the lost workspace stop
  resolving, and none is reused.
- `result_ids_continue_across_a_rebuild` — through the host protocol, a rebuilt engine numbers
  on, and the old ids answer `E_NO_RESULT` to `page`.
- `the_server_answers_while_a_query_runs` — a ping during a long query is answered at once.

## REQ-9 — Scratch is free; loaded data is not

- `loaded_is_readonly` — loaded data is attached read-only, so DuckDB enforces it, not a reading
  of the statement text.
- `the_agent_cannot_change_a_hosts_table` — nor its catalog: no write, detach, attach over its
  name, or create in it.
- `scratch_is_free` — the agent's own tables, views and macros persist and are fully mutable.
- `the_agent_keeps_no_enum` — DuckDB holds an ENUM's values outside its budget, so a
  `CREATE TYPE`, or a table or view with an ENUM column at any depth, is undone (about 2 ms with
  100 scratch tables); an ENUM in a query that keeps nothing works.
- `a_pivot_keeps_no_enum_after_its_batch` — a dynamic PIVOT's ENUM works for its statement and is
  dropped after the batch.
- `an_object_that_keeps_an_enum_in_its_sql_is_refused` — nor may an ENUM be spelled in a
  DEFAULT, generated column, CHECK, index, or view or macro body, string literals included.
- `a_spoofed_pivot_enum_cannot_be_pinned` — a type named like a pivot's, cast to in a DEFAULT,
  refuses the batch, and nothing of either stays.
- `a_pivot_type_that_cannot_be_dropped_spoils_the_engine` — a pivot type that cannot be dropped
  after its batch gets the engine rebuilt, `E_REBUILT` with why.

The agent creates what it likes, temporary or not, under any name that is not already taken.
DuckDB resolves the connection's temporary catalog before any search path and the agent's own
schema before the host's catalogs, so an agent object named like a host table or a built-in
would stand in for it. After every `CREATE` and `ALTER`, inside the statement's own transaction,
benchy lists the agent's catalogs and undoes the statement if it took a name that is taken.
Names compare as DuckDB compares identifiers, ASCII case folded. Taken are: the tables on the
bench, DuckDB's built-in functions, macros and table functions, the names DuckDB's parser reads
itself, DuckDB's system views, and the operator's helpers and every table they read (REQ-18).
Built-in type names need no check: DuckDB refuses `CREATE TYPE integer` itself.

- `an_agent_object_cannot_shadow_a_host_object_or_builtin` — any object created or renamed into
  a taken name, by any spelling, is refused and undone; every name still means what it meant.
- `a_name_the_parser_reads_itself_is_taken` — `coalesce`, `if`, `try_cast` and the like are
  taken; a keyword the parser reads as a name, such as `data`, stays free.
- `a_refused_name_stops_the_batch_where_it_stands` — the statements before it keep what they did;
  nothing after it runs.
- `an_attached_name_is_taken_too` — a table attached after the seal is held to the rule, and an
  attach under a name the agent holds is refused with `E_NAME`.
- `an_attach_under_the_agents_own_name_is_refused` — through the host protocol too; the agent's
  table stays.
- `a_system_view_name_is_taken` — every view of DuckDB's `system` catalog, in every schema (47 on
  1.5.5), is taken; a table function's name stays free.
- `a_dynamic_pivot_is_not_a_shadow` — DuckDB's own `CREATE TYPE` for a dynamic `PIVOT` passes and
  answers once.
- `a_spoofed_pivot_type_gets_nothing_past_the_guard` — even taken for a pivot's, an agent's type
  would get nothing past the guard: every statement is still classified and every name checked.
- `a_type_the_agent_names_is_never_a_pivots` — a type is a pivot's only if named as DuckDB names
  one, in the temporary catalog, and absent from the batch's SQL.
- `benchys_catalog_reads_cannot_be_shadowed` — benchy's catalog reads name `system.main`, so no
  planted macro changes the attach check, `describe`, the hints or pivot detection.
- `the_agent_cannot_forge_what_the_host_reads_from_the_catalog` — through the host protocol, an
  attach answers with the real columns.
- `the_corpus_shadows_are_refused_and_undone` — every shadowing case in `corpus/escape.toml`.

## REQ-10 — Bounded results

The reason to put data on a bench instead of into a context window is that what comes back is
small. Results are capped in rows and bytes, truncation is always stated, and a result can be
fetched again by id.

- `results_are_capped_and_say_so` — a capped result says it is not all the rows.
- `a_batch_returns_every_result_not_only_the_last` — each statement in a batch answers.
- `a_failed_batch_returns_what_ran_before_it` — the statements before a failure keep their
  effects and results, after the error, which comes first.
- `a_result_can_be_fetched_again_by_id` — a result is addressable after the fact.
- `two_sessions_never_issue_the_same_result_id` — ids carry a random 32-bit prefix of the
  session's own (`3fa9c1b2-7`), resolve only where issued, and name nothing (REQ-1).
- `a_timestamptz_arrives_intact` — a value is never silently blank.
- `nested_types_render_rather_than_vanishing` — lists, structs and maps render.
- `every_common_type_renders_as_something_a_reader_can_use` — one sweep across the common types.
- `an_infinite_time_renders_as_duckdb_writes_it` — infinite dates and timestamps render as
  `infinity` or `-infinity`, at the top level and nested.
- `cap_measured_on_the_wire` — the whole JSON-escaped response stays under the byte cap and says
  it was cut, across nine shapes at two caps.
- `the_response_budget_holds_for_awkward_shapes` — the same at the renderer, for escaped
  multi-byte text and 40 results.
- `every_tool_response_obeys_the_byte_cap` — every tool, `get_result`, `describe` and `dialect`
  included.
- `a_full_page_is_not_called_truncated` — exactly the row cap is complete; one more row is not.
- `a_partial_result_is_never_presented_as_complete` — a read that stops early says so, even when
  every row kept fits (tested at the renderer).
- `a_truncated_result_can_be_paged` — `get_result` with an offset reads on without re-running.
- `a_result_keeps_its_first_2000_rows` — 2,000 rows are kept whole; of 2,001, the first 2,000.
- `a_result_keeps_rows_until_they_reach_1_mib` — rows are kept until they come to 1 MiB, a value
  counting its text and 3 bytes: the row that reaches it is the last kept, so a result can hold
  up to a row more than 1 MiB.
- `the_last_32_results_are_kept` — the 32nd result from the last can be paged, the 33rd cannot.
- `the_response_cap_is_at_least_512_bytes` — `--max-bytes 511` is refused at startup; at 512 a
  cut response fits and says it was cut.
- `a_host_lowers_max_bytes_to_512_and_no_further` — a host's `caps.max_bytes` on a query or a
  page may be 512, and not 511.
- `a_page_cuts_values_at_its_own_cell_cap` — a page's lower `max_cell_bytes` cuts each value as
  the query would have, saying TRUNCATED.
- `a_query_without_a_limit_reads_no_further_than_it_keeps` — a `SELECT` is read only as far as
  its rows are kept: a trillion-row `range` answers `of 2000+`. Other statements run to the end.
- `a_query_without_a_limit_holds_no_more_than_it_keeps` — such a `SELECT` over ten million rows
  grows the engine's peak by megabytes, not by the result (Linux only).
- `an_error_past_the_rows_kept_is_not_raised` — the price, accepted (reading on cost 0.88 G and
  178 ms over 10M rows): an error only past the ~1 MB DuckDB buffers is not raised.
- `an_error_in_the_rows_kept_still_fails_the_query` — an error in the rows read fails the
  statement, also in a later fetch.
- `a_deadline_while_rows_are_read_is_the_deadline` — a deadline in a fetch is the deadline, not
  the first rows called all of them.
- `every_common_type_is_streamed` — a `SELECT` is streamed only where its column types read as
  Arrow; every common type, BIT, BIGNUM and TIME WITH TIME ZONE included, does.
- `a_page_past_the_end_says_so` — an offset past the last row is an empty page saying where the
  rows end, not an error.
- `a_value_arrow_cannot_carry_faithfully_says_so` — BIT, BIGNUM and TIME WITH TIME ZONE render
  correctly or as a marker naming `::VARCHAR`, never a wrong value.
- `time_zone_conversion_works_in_the_shipped_build` — converting to a named zone works in the
  shipped build, sealed.
- `a_refused_time_zone_fails_only_where_icu_is_loaded` — results are UTC; with ICU loaded, a
  refusal to set UTC stops the engine (tested at the decision).
- `the_agent_connection_is_utc_on_a_host_that_is_not` — the agent's connection is UTC on a host
  in another zone, sealed from a bench or empty.
- `every_bench_connection_is_utc_on_a_host_that_is_not` — so is every connection to a loader's
  bench.
- `a_zone_other_than_utc_stops_the_engine` — an agent connection found in another zone, or with
  ICU and no zone, is a startup failure.
- `the_agent_is_told_times_are_utc_and_how_to_bucket_local_days` — the instructions, and a
  refused `SET`/`RESET TimeZone`, say times are UTC and how to bucket local days; the example
  `date_trunc('day', timezone('Europe/Berlin', ts))` gives Berlin's days.

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
- `an_unknown_table_lists_the_columns_of_what_is_here` — with each table's columns.
- `an_unknown_column_names_the_columns_of_the_tables_it_reads` — an unknown column is told the
  columns of the tables, views and the agent's own objects the statement names, and of no
  others.
- `a_qualified_unknown_column_resolves_its_alias` — `r.temp`, `t.temp` and a CTE's `c.temp`
  are told too, the alias said as its table, that table first.
- `an_unknown_column_of_a_helper_view_lists_its_columns` — a helper view is listed like a table.
- `the_columns_a_hint_lists_are_bounded` — at most 1,024 bytes of tables and columns: a cut
  table ends `… [+N columns])`, past 12 tables `(+N more)`, and the hint says the SQL that shows
  the rest.
- `a_hosts_sql_error_gives_the_columns_and_sends_its_agent_to_no_tool` — these hints name no
  tool, so a host whose agent has no `describe` is not sent to one.
- `a_correct_query_carries_no_hint` — a success is never padded.
- `empty_results_say_what_exists` — an empty result whose filter used an unknown value says what
  the column holds.
- `a_non_empty_result_never_carries_a_note` — and only then.
- `the_empty_result_note_stays_inside_the_deadline` — the note gets what is left of the
  deadline, at most 0.5 s, and skips a column whose first 2,048 values already hold more than it
  lists; past that the result goes back without a note.
- `mentions_survives_a_name_that_starts_with_a_multibyte_letter` — the note's name matching
  does not panic on a name that starts with `Ü`.
- `a_write_to_a_loaded_table_says_how_to_make_your_own` — a write or DDL on a loaded table is
  told it is read-only and how to copy it, naming no internal catalog (REQ-15).
- `an_export_is_told_results_come_back_through_query` — `COPY … TO` and `EXPORT` are told there
  is no file output, apart from a file read.
- `a_dynamic_pivot_answers_once` — only the query of a dynamic `PIVOT` answers, the statements
  after it keep their numbers, and no empty-result note is added.
- `the_dialect_reference_is_written_for_the_agent` — `dialect` serves `docs/DIALECT.md` without
  its maintainers' blocks: no requirement, test or source file is named to the agent.

## REQ-12 — Admission control at the source

Nothing may let the model do unbounded work to a system benchy reads from.

- `estimate_before_fetch` — a connector's `estimate` is asked first, and over budget the fetch
  is refused, saying what was estimated and what was left.
- `an_inband_estimate_cancels_a_fetch_already_running` — an estimate sent during a fetch stops it.
- `a_cancelled_fetch_is_killed_if_it_ignores_the_request` — a connector that ignores SIGTERM is
  killed after the cancel grace.
- `reload_does_not_amplify` — a rebuilt bench re-attaches what the session stored instead of
  asking the source again.
- `an_empty_result_survives_a_rebuild` — including a source that returned nothing.
- `an_operator_declared_window_is_parsed_and_refuses_what_it_cannot_apply` — an operator can
  bound what a source may return at all.
- `exactly_what_is_left_fits_and_one_more_does_not` — the budget's edge is exact, in rows and
  in bytes.
- `session_budget_enforced` — the pass budget bounds what is read, not only what was estimated.
- `the_tables_of_one_attach_share_what_the_budget_has_left` — in one host `attach` of several
  tables, each is held to the rows the ones before it left, to the row.
- `a_configured_session_budget_counts_every_fetch` — in a server, each loader's reported rows
  and bytes are charged: exactly the `[budget]` loads, and the next source is refused, naming it.
- `a_failed_fetch_still_spends_what_it_read` — a fetch that fails is charged what it read before
  it is reported, so the next source gets only what is left.
- `a_sources_own_cap_is_not_called_the_sessions_budget` — a refusal names the cap that was
  passed, a source's own `max_bytes` or the session's budget.
- `a_session_budget_that_admits_nothing_is_refused` — a `[budget]` ceiling of zero, or an
  unknown key, stops startup; one left out keeps its default.
- `the_store_is_private_to_the_session` — what was loaded is kept, for rebuilds, in the
  session's own directory: created fresh, owned by the user running benchy, mode 0700.

## REQ-13 — The host protocol

A host puts data on a session's bench over `benchy-mcp host`
([docs/HOST-PROTOCOL.md](docs/HOST-PROTOCOL.md)). The pipes are the capability, and so is a
descriptor a request carries; requests are strict, and a refusal changes nothing.

- `the_host_protocol_refuses_what_it_does_not_know` — an unknown call or field, a cap raised or
  lowered past what can say it cut, a request without an id, a second `hello`.
- `a_session_that_does_not_open_properly_ends` — whatever is wrong with the first request.
- `a_request_is_read_exactly_as_sent` — a key given twice is refused; the caps `hello` reports
  can be handed back, each lowered.
- `an_id_in_use_is_answered_once` — a request reusing an id still being answered is refused with
  `"id": null`.
- `a_host_that_closes_its_input_still_gets_its_answers` — closed stdin means no more requests:
  everything already sent is answered.
- `a_host_that_has_gone_is_not_worked_for` — once a reply cannot be written, the session ends.
- `a_reply_the_reader_cannot_write_ends_the_session` — so does one the reader writes itself
  (`hello`, `ping`, a second `hello`'s refusal).
- `a_request_larger_than_a_request_can_be_ends_the_session` — a request over a megabyte ends the
  session.
- `hello_says_what_the_session_is` — the version agreed, the engine, the sandbox, the caps and
  the limits.
- `hello_says_how_long_a_load_and_the_hard_grace_may_take` — `timeouts`: the load timeout and the
  hard grace, beside the caps and not in them, so the caps can still be handed back in a `query`.
- `print_limits_says_what_hello_would` — `host --print-limits` prints the caps, limits and
  timeouts `hello` reports for the same flags, refuses what a session would refuse, and starts
  nothing.
- `version_json_says_what_a_pin_bump_checks` — `--version --json`, which a release's
  `compat.json` repeats, names exactly the protocol versions `hello` agrees to, the version and
  DuckDB library `hello` and `doctor` name, and the Landlock ABI the full posture needs.
- `a_host_may_lower_the_memory_ceiling_but_never_raise_it` — protocol 3's `limits.memory_ceiling`
  lowers the operator's engine ceiling and the lower one is enforced; raising it, going below
  DuckDB's budget, or an older protocol ends the session as `E_REQUEST`.
- `a_host_lowers_the_loader_ceiling_and_cannot_raise_it` — from protocol 6, `hello` reports
  `limits.loader_memory_ceiling` and a host may lower it; an attach past it is `E_LIMIT` and
  attaches nothing; raised, 0 or before protocol 6 ends the session as `E_REQUEST`.
- `a_bad_attachment_attaches_nothing` — a stream cut short or followed by another, not Arrow, a
  symlink, a type outside the vocabulary, a duplicate column: `E_DATA`, no path, old table kept.
- `an_attach_name_the_agent_could_not_type_is_refused` — refused, not rewritten; an unreserved
  keyword such as `data` is a fine name.
- `a_detach_drops_the_results_that_could_hold_its_rows` — and a detach takes effect, even if the
  engine is lost doing it.
- `ping_is_answered_while_a_query_runs` — a ping never waits behind a call.
- `a_rebuild_restores_the_attachments` — after a lost engine the host gets `E_REBUILT`, every
  attached table is back, and the agent's own are gone.
- `a_session_holds_at_most_its_limit_of_tables` — replacing a table does not count as adding one.
- `several_tables_attach_in_one_request` — protocol 2's `tables`: each answered as its own attach,
  in order, with one engine request for all.
- `the_loaders_of_one_attach_run_at_once` — on more than one core, a request's loaders run side
  by side.
- `a_host_streams_a_table_without_a_file` — protocol 3: a table comes through a pipe sent with the
  `attach`, no file on either side, costing the budget what its file would.
- `ping_is_answered_while_a_stream_is_attached` — an attach waiting for its writer does not hold
  up a ping, and a non-blocking read end is read blocking.
- `a_streamed_attach_keeps_every_rule_of_the_file_form` — each refusal of the file form, with its
  code; the old table keeps serving and the writer is told nobody reads.
- `a_descriptor_is_taken_only_by_a_streamed_attach` — a descriptor anywhere else, or a stream
  without exactly one, is `E_REQUEST` with every descriptor closed; a non-pipe, non-socket,
  non-file descriptor is `E_DATA`.
- `a_stream_needs_a_socket_to_come_over` — on pipes a streamed attach is refused, saying so.
- `a_host_streams_tables_back_to_back_over_one_channel` — protocol 5's `--stream-channel`: each
  `"stream": "channel"` attach takes the next whole stream, in order, charged its bytes.
- `ping_is_answered_while_a_channel_attach_waits` — an attach waiting on the channel does not
  hold up a ping, and a non-blocking channel is read blocking.
- `a_refused_stream_leaves_the_channel_at_the_next_one` — a stream refused for its content, a
  limit or its name is drained to its end marker; the next attach takes the next stream.
- `a_channel_that_loses_its_place_is_closed_and_says_so` — where a stream's end cannot be found,
  the channel is closed (`EPIPE` for the writer), later channel tables are `E_CHANNEL`, and the
  rest of the session serves.
- `a_refused_request_that_names_the_channel_closes_it` — a request refused whole that names a
  channel table closes the channel and says so, so no stream is taken under another name.
- `a_stream_channel_is_one_the_operator_names` — only the descriptor `--stream-channel` names is
  a channel, and a bad one stops the start.
- `a_channel_and_a_descriptor_serve_one_session` — both streamed forms in one session and in one
  request.
- `a_channel_and_a_descriptor_load_in_request_order_on_one_cpu` — with one CPU, a request's
  tables load in list order, so one writer's channel and descriptor streams all load (Linux
  only: one CPU is had with `sched_setaffinity`).
- `a_channel_reader_stops_at_each_streams_end_marker` — the channel's reader reads nothing past
  a stream's marker, whatever size its reads are.
- `a_refused_stream_is_drained_to_its_marker_within_a_bound` — exactly the bound drains, a byte
  more loses the place, and so do an early end or bytes that are no message.
- `limits_are_lowered_by_the_host_and_raised_only_by_the_operator` — the operator sets attachment,
  session and table limits (flag or `host --config`, never both, never 0); a host only lowers
  them, in protocol 3's `hello`.
- `a_query_reply_counts_the_rows_of_each_result` — `results` gives each result's rows held,
  whether complete, rows shown and whether the text was cut.
- `a_version_refusal_lists_the_versions_in_its_detail` — `E_VERSION`'s `detail.protocols` is
  every version benchy speaks.
- `a_failed_batch_says_what_ran_in_its_detail` — a batch that fails gives the statement it
  stopped at, and the earlier results' `result_ids` and `results` in a query reply's shape, in
  `detail`, and still in the message; past the deadline, `reason` and an empty `lost` too.
- `a_rebuild_says_in_its_detail_why_and_what_was_lost` — `E_REBUILT`'s `detail.reason`
  (`hard_deadline`, `engine_died`, `memory_ceiling` with `held` and `ceiling`) and `lost`
  (`results`, `agent_tables`); the message tells the model the results went, and past the
  ceiling to say the work needs more rather than to restart benchy itself (Linux only, where
  the ceiling is read from /proc).
- `a_lost_engine_says_why_in_one_word` — every way the engine is lost, `pivot_type` included,
  has a reason of its own and loses the same.
- `an_attach_past_a_limit_names_it_in_its_detail` — `E_LIMIT`'s `detail.limit` names the limit
  as `hello` does, for rows, bytes, a file, a stream and the table count, and `value` the bound
  it was held to.
- `a_load_past_its_timeout_says_why_in_its_detail` — an attach past `--load-timeout` is
  `E_DEADLINE` with `reason: load_timeout` and nothing lost.
- `a_page_says_the_sql_of_its_result` — `page` returns the agent's own statement of its result,
  never a helper's body, cut at `max_bytes`, left out where the batch cannot be split as DuckDB
  splits it.
- `page_sql_keeps_every_literal_whole` — that statement drops comments only: a `--`, `/*` or `;`
  inside any literal form or quoted name is kept.
- `texts_name_the_tools_as_the_host_names_them` — protocol 4's `tool_names` renames the agent's
  tools in every text; without it, benchy's names are used.
- `tool_names_are_known_tools_plain_names_and_protocol_4` — an unknown tool, a name not plain or
  over 64 bytes, a clash, or protocol 3 ends the session as `E_REQUEST`. A name labels prose
  and addresses nothing.
- `a_long_tool_name_keeps_every_text_within_its_budget` — a 64-byte name keeps every footer
  within the byte cap.
- `a_host_states_the_shape_of_what_it_attaches` — protocol 3's `time_column` and `series` make
  `describe` say "is"; a missing or non-time column is refused before a row is written.
- `one_bad_table_of_an_attach_is_refused_alone` — with its own code; the tables around it attach,
  and one it was to replace keeps serving.
- `an_attach_of_several_tables_is_held_to_the_limit_in_order` — the table count is held as if
  attached one after another; replacing is not adding.
- `the_tables_of_one_attach_are_held_to_the_budget_in_order` — the budget is held in list order:
  the first to pass what is left is `E_LIMIT`; exactly what is left fits.
- `an_attach_request_that_is_not_well_formed_attaches_nothing` — a malformed `tables` request is
  refused whole as `E_REQUEST` before anything is read.
- `a_host_attach_past_what_the_session_has_left_is_refused_as_a_limit` — `E_LIMIT` before
  anything is written, in rows or bytes; exactly what is left fits.
- `a_host_attach_stops_at_the_budget_not_after_reading_it_all` — the attach stops at the batch
  that passes the budget, before writing it or reading the rest.
- `a_loader_that_cannot_read_its_job_says_so` — a loader's error reaches the operator or host as
  its own words.
- `the_budget_moves_by_what_was_attached_and_nothing_else` — by the rows counted and bytes
  measured; a refused attach costs nothing, and `describe` says what is left.
- `a_dictionary_encoded_attachment_is_unpacked_within_the_bound` — its values load, and one that
  would unpack past the bound is `E_LIMIT` before unpacking.
- `a_view_attachment_is_unpacked_within_the_bound` — view columns count by the lengths their
  views state, before unpacking.
- `an_attachments_plain_and_unpacked_bytes_share_one_bound` — decoded bodies and unpacked
  dictionary and view columns are one total against `attach_bytes`.
- `a_detach_brings_a_bench_that_could_not_rebuild_back` — detaching the table that stops rebuilds
  earns a fresh try.
- `a_killed_host_leaves_nothing_behind_even_without_an_engine` — after a failed rebuild the
  warden still removes the session.
- `a_message_names_no_path_of_the_sessions_or_the_hosts` — no reply names a session or host path.
- `a_short_path_does_not_garble_a_message` — only an absolute path is replaced.
- `columns_duckdb_tells_apart_attach` — duplicate columns are judged by ASCII case, as DuckDB
  does: `Ä` and `ä` are two.
- `a_host_session_leaves_nothing_behind` — closing the pipe stops the engine and removes the
  session's directory.

## REQ-14 — Describe tells the shape of the data

Every step benchy saved an agent in the eval was a step of discovery (eval/REPORT.md). So one
`describe` says what an analyst would look for first in a table of time series: which column is
time, its range and cadence, which column tells the series apart and the step each series keeps,
and which series are dead, start late, stop, pause, skip rows or repeat them, or hold one value
far longer than they otherwise do — and which moments every series shares, such as the hour a
clock change zeroes or doubles. What is a heuristic says "looks like"; an operator can state the
time column and the series key per source instead (`time_column`, `series`), and a host per
attachment. Units are never guessed.

The profile is computed **once, by the loader**, and kept in the table's comment, so `describe`
only renders text and a rebuilt engine does not profile again. It comes from one scan of
aggregates and one pass over the rows as they are written (a stream's batches, a file's scan
beside its write), adding up each series' count, extent, phase, two checksums of its times,
their order and its runs of one value. The checksums alone prove nothing: a series is proven
regular by them only when its times came in order, otherwise by an exact count of its distinct
times, and bounded queries read the table only for what failed. It costs a fraction of the load
(0.22 s of 0.84 s on the public meter data; [the measurement in
full](docs/history/REQUIREMENTS-history.md#req-14--describe-tells-the-shape-of-the-data)), and
`describe` about 1 ms.
The agent's own tables are named in `describe` but not profiled: they change with every
statement, and the agent made them.

- `describe_tells_the_time_column_its_range_and_cadence` — the first temporal column, its first
  and last time, and the step most series follow.
- `describe_tells_the_series_key` — the guessed key, its values, how many series are regular,
  and each numeric column's range, nulls and zeros.
- `an_integer_reading_is_not_taken_for_the_series_key` — between integer candidates, the one
  whose series keep a step wins over the one with the most values.
- `a_small_table_still_finds_its_series_key` — too few rows for the first rule, the key is the
  column that, with the time, tells apart at least 80% of the rows.
- `repeated_times_with_no_key_are_said_to_be_unassessed` — repeated times with no key are said so,
  with the exact distinct count, and no gaps or repeats are claimed.
- `describe_names_duckdbs_own_types` — `double`, `bigint`, `varchar`, `timestamptz`, through the
  library and MCP alike.
- `counts_are_said_in_good_grammar` — "1 row", "1 of 2 series has", "4 times"; never "(s)".
- `a_long_line_is_elided_between_items_never_inside_one` — a long line is shortened between items,
  never inside a timestamp or a value.
- `describe_names_a_dead_series_a_late_start_a_stop_and_a_pause` — runs of 0 or null of a day or
  more, at the start, the end or between; a series 0 throughout.
- `describe_names_gaps_and_repeated_times` — gaps and repeats, where, including a gap hidden by as
  many repeats.
- `each_series_is_judged_at_its_own_step` — series at 5, 10 and 15 minutes are each judged at their
  own step; notes alike in several series are said once, by their keys.
- `times_finer_than_a_millisecond_are_profiled_as_they_are` — times are profiled in microseconds:
  rows 500 µs apart are `a row every 500 microseconds`.
- `an_ordered_series_is_regular_however_its_checksums_wrap` — an in-order series is proven
  regular without the sum that can pass an i128.
- `every_tables_shape_comes_before_any_tables_notes` — every table's own lines get room before any
  table's per-series notes.
- `a_stuck_value_inside_a_series_is_noted` — a value held a day or more, ten times the series'
  average run, is noted like a run of 0; a gap breaks it, and a mode-like column is not noted.
- `the_pass_misses_no_value_held` — the loader's pass finds every value held the query finds.
- `three_gaps_hidden_by_three_repeats_are_found` — series whose checksums match a regular one are
  not called regular, in any row order.
- `anomalies_past_any_limit_are_counted_exactly` — gaps, repeats and rows off the step are counted
  exactly: 300,000 repeats are said as 300,000.
- `describe_names_a_moment_every_series_shares` — a time when every series reads 0, or totals
  double; a clock-change day is said to be one.
- `the_public_meter_data_shows_its_quirks` — on seven meters of the eval's public data,
  committed: the dead meter, late starters, pauses, the stop and both clock changes.
- `a_stated_shape_wins_over_the_heuristic` — a stated time column and key are used and not
  called guesses.
- `a_stated_column_that_is_not_there_refuses_the_load` — a missing stated column, or a non-time
  time column, fails the load.
- `a_stated_shape_must_name_declared_columns` — an exec source's stated shape is checked against
  its schema at startup.
- `a_table_without_time_gets_its_columns_summarised` — ranges, and a small text column's values.
- `a_distinct_count_never_exceeds_the_values` — no estimate exceeds the column's values; one that
  reaches them is exact up to 100,000 rows, "about" above.
- `an_integer_series_key_is_listed_in_number_order` — keys are listed in their type's order;
  past 200,000 series the key gets no range.
- `many_series_keep_the_tables_own_extent` — past 200,000 series the extent is the table's, and
  the series not examined are said to be so.
- `the_shape_is_summarised_never_cut_silently` — what does not fit is counted or said to be
  missing, always within the byte budget.
- `the_pass_finds_what_the_queries_find` — the loader's pass and the fallback queries find the
  same shared moments and runs of 0 or null.
- `the_profile_kept_as_rows_arrive_is_the_profile_of_the_table` — a stream's profile kept as it
  arrives equals the profile read from its table, byte for byte.
- `a_stream_sorted_by_series_is_tallied_by_its_key` — rows sorted by series, then time, are
  tallied by their key without reading back, and give the table's profile byte for byte.
- `a_stream_sorted_by_series_says_what_its_rows_in_time_order_say` — sent by series or by time,
  the same rows give the same profile.
- `a_files_profile_is_kept_from_one_scan_and_is_the_profile_of_the_table` — a Parquet, CSV or
  NDJSON file's one-scan profile equals the table's, byte for byte.
- `a_files_profile_is_kept_beside_its_write` — a staged or Parquet file is profiled beside its
  write; a time-window body is watched as narrowed, in the loader's zone.
- `a_kept_distinct_count_is_duckdbs` — distinct counts kept as rows arrive equal
  `approx_count_distinct` of the table.
- `a_kept_distinct_count_of_a_tallied_key_is_duckdbs` — a tallied key, hashed once per series,
  still counts as DuckDB counts.
- `a_cycle_of_series_costs_one_key_compare_a_row` — meters in a fixed cycle cost one key compare a
  row (285 instructions a row against 398 with two; docs/history/PERFORMANCE-rounds.md, R3-4).
- `a_tally_tells_apart_keys_alike_in_their_first_words` — keys differing only past their
  sixteenth byte, or by a trailing NUL, are different series.
- `a_keys_words_are_read_alike_from_every_string_layout` — a string, large string, view or slice
  gives a key the same words.
- `a_tallys_totals_per_time_count_every_row_of_every_time` — per-time totals count every row.
- `an_integer_key_is_written_as_text_once_a_series` — an integer key becomes text once a series,
  as `CAST(… AS VARCHAR)` writes it (27% fewer instructions at 10M rows;
  docs/history/PERFORMANCE-rounds.md, R3-10).
- `placing_a_series_matches_counting_its_steps` — phase and step checksums from early sums equal
  counting each index.
- `describe_does_not_scan_the_table` — on two million rows `describe` takes milliseconds, the
  same every time.
- `the_same_data_gives_the_same_shape_from_csv_and_parquet` — the same rows, from another format in
  another order, describe identically.
- `the_shape_reaches_the_agent_through_the_engine` — profiled by a loader, carried in the file,
  read at attach, rendered by MCP `describe`.
- `a_hosts_describe_carries_the_shape_of_what_it_attached` — the same for an Arrow stream a host
  attaches, through the host protocol's `describe`.

## REQ-15 — Origin-blind

The agent cannot tell where data came from. The same data makes the same table — the same
column types, the same `describe`, the same answer to every query — whether it was a CSV, a
Parquet or NDJSON file, an Arrow stream a host attached from a file, through a pipe or through
its stream channel, or a connector's fetch. At runtime the agent hears about tables, never about
sources, connectors, files or paths: those, and every reason a load failed, are the operator's
(`a_connector_error_reaches_the_operator_not_the_agent`).

One mapping is applied to every source as it is loaded (`src/canonical.rs`): every integer to
`BIGINT` (`UBIGINT` to `HUGEINT`), every float to `DOUBLE`, every timestamp to `TIMESTAMP WITH
TIME ZONE` at microseconds; a string in any layout is `VARCHAR`; dates stay `DATE` and decimals
keep their precision. A zone-less timestamp is read in its source's `timezone`, UTC unless the
operator names one. A timestamp finer than a microsecond is refused rather than rounded. Only
top-level columns are mapped.

The physical layout is the same too. A file is written to its table in one pass on one thread,
straight from its reader (Parquet) or from a stage the reader filled (CSV, JSON), and a stream
batch by batch as it arrives, in order, on one thread. So the same rows in the same order give
the same file size, blocks, row groups, segments and compression on every load, whatever they
came from. A loader's file is written in storage version v1.3.0, whose DICT_FSST is chosen
without sampling (DuckDB 1.5.5's default chooses from a random sample). Rows in another order
are other data as far as the layout goes.

The layout includes each segment's `block_id` and `block_offset`, which the agent can read, so
the write stays on one thread though it is most of the path to the first `describe` at
10,000,000 rows (docs/PERFORMANCE.md, R4-1): measured on DuckDB 1.5.5, every parallel write
tried moved blocks between loads, and the one that kept them took 3.9 s where one thread takes
2.9 s.

- `the_same_data_looks_the_same_from_every_source` — one fixture from every kind of source, in
  differing source types, answers `describe` and six queries, `typeof` among them, byte for byte
  the same.
- `a_stated_shape_looks_the_same_from_every_source` — a time column and series key stated in a
  file source's config, a host's attach or a connector's config give the same `describe`,
  saying "is" where a heuristic says "looks like".
- `the_physical_layout_says_nothing_of_where_data_came_from` — `pragma_database_size()` and
  `pragma_storage_info` answer the same for 840,000 rows from all six sources, down to every
  segment's block and offset.
- `a_loader_writes_each_row_group_as_it_fills` — every loader writes each row group of a bulk
  insert as it fills, not five at once, and every source is still written the same way.
- `a_files_layout_is_the_same_on_every_load` — eleven loads of one file make the same file,
  segments, compression, blocks and offsets; no text compression is chosen from a sample.
- `a_json_file_is_staged_whatever_its_size` — a JSON or NDJSON file of any size is staged and
  read on every thread.
- `a_json_files_timestamps_read_as_text_load_as_the_sniffer_reads_them` — an NDJSON file's
  timestamps, read as text and cast, make the sniffed read's table; a value the cast would read
  otherwise has the file read the sniffed way. Never a null, never a rounded time.
- `an_ndjson_bodys_timestamps_read_as_text_load_as_the_sniffer_reads_them` — the same for an
  undeclared HTTP NDJSON body, spooled and read as a file.
- `a_json_file_whose_columns_the_text_read_would_change_is_read_as_it_was` — a file whose columns
  the text read would change, or one under 1 MiB, is read the sniffed way, once.
- `a_file_that_fails_midway_leaves_no_table_and_says_why` — a file that goes bad after 60,000
  good rows fails in DuckDB's words, staged or read straight in, and leaves no table.
- `the_catalog_says_nothing_of_where_data_came_from` — the catalog views and the search path name
  no file, path, connector or process id, only the session's own random directory.
- `a_stream_may_be_narrower_than_its_declaration_never_wider` — a connector may send a type the
  declared one holds exactly, loaded as the declared canonical type; a wider one is refused,
  naming the column.
- `a_zone_less_timestamp_is_utc_unless_its_source_names_a_zone` — a zone-less timestamp is UTC,
  or in the zone its source's `timezone` names; an unknown zone stops startup.
- `a_zone_less_timestamp_is_read_in_its_sources_zone` — the conversion itself, at the SQL.
- `an_infinite_timestamp_is_kept_from_every_source` — `infinity` and `-infinity` load as
  themselves from every source, unit and zone, never as a null; `describe`'s extents omit them.
- `an_ndjson_infinity_is_refused` — an infinity in a sniffed JSON timestamp or date column is
  refused, naming the file, the column and the fixes (null, a declared type, CSV or Parquet); a
  genuine 1900-01-01 loads.
- `an_ndjson_infinity_is_refused_in_a_column_renamed_for_case` — the same in a column DuckDB
  renamed for a key that differs from another only in case.
- `an_ndjson_epoch_is_refused_where_a_csv_reads_1970` — `epoch` in a sniffed JSON column is
  refused as an infinity is; in a declared column it reads as 1970, as from a CSV.
- `clickhouse_looks_the_same_as_a_file` — ClickHouse's own `ArrowStream` (LZ4, zoned
  `DateTime64`), fetched by `clickhouse local` as a connector, answers as the CSV does; skipped,
  saying so, without `BENCHY_CLICKHOUSE_BIN`.
- `a_data_value_in_describe_is_a_quoted_literal` — every data value in `describe` is a SQL
  literal and every odd column name a quoted identifier, with control, format and separator
  characters spelled out as `\u{…}`.
- `a_hosts_describe_quotes_data_values_and_names_duckdbs_types` — the same through the host
  protocol's `describe`, with DuckDB's type names.
- `the_clickhouse_source_looks_the_same_as_a_file` — the built-in ClickHouse source (REQ-16)
  answers byte for byte as the CSV does (Linux only, as the ClickHouse tests are).

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

- `clickhouse_types_map_to_the_canonical_vocabulary` — every ClickHouse type converts to the
  bench's canonical type (REQ-15), `UInt64` to HUGEINT, `Nullable` and `LowCardinality` to what
  they wrap.
- `unsupported_clickhouse_types_become_json_text_or_are_dropped` — containers load as JSON text,
  oversized integers and decimals as exact text; aggregate states and intervals are left out,
  saying how to load them.
- `identifiers_are_quoted_whatever_they_hold` — no character in a name ends its identifier.
- `values_travel_as_query_parameters_never_in_the_sql` — the window and entity ids are query
  parameters, never SQL text.
- `a_time_window_must_say_its_offset` — a window without an offset is refused at startup.
- `a_clickhouse_url_cannot_carry_a_password` — a user or password in the URL is refused, naming
  where they go instead.
- `a_multi_frame_zstd_body_is_read_to_its_end` — a multi-frame zstd body is read whole; one cut
  short fails.
- `a_bomb_is_decoded_only_as_far_as_it_is_read` — a zstd body is decoded only as far as it is
  read, so a bomb stops at the cap.
- `a_zstd_window_past_32_mib_is_refused` — a frame asking for a window over 32 MiB is refused.
- `an_unset_compression_follows_where_the_server_is` — without `compression`, a loopback server
  is asked for nothing compressed and any other for zstd; a set value is used everywhere.
- `a_clickhouse_table_loads_with_its_schema_read_and_mapped` — every kind of column loads as the
  bench's type with its exact value; an aggregate state is left out.
- `the_same_rows_from_csv_and_clickhouse_describe_and_answer_identically` — the same rows from a
  CSV and from a ClickHouse table of other types give the same types, `describe` and rows.
- `the_window_and_the_allowlist_are_pushed_down_as_parameters` — ClickHouse returns only rows in
  the window and the allowlist, and hostile ids arrive intact.
- `only_the_pinned_or_named_columns_are_fetched` — ClickHouse reads no column it was not asked for.
- `a_pinned_schema_that_does_not_match_refuses_the_load` — a pinned column that is missing, of
  another type, or null where pinned non-null refuses the load.
- `an_over_budget_clickhouse_load_is_refused_before_transfer` — over the source's cap or the
  session's budget, the load is refused on the estimate, before a row is sent.
- `a_compressed_response_is_decoded_and_bounded` — the cap counts the decoded Arrow, not the bytes
  on the wire.
- `clickhouse_failures_map_to_fault_codes` — a wrong password is `E_AUTH` (not in the message),
  nothing listening `E_UNAVAILABLE`, an unknown table `E_CONFIG`, a slow query a timeout.
- `a_clickhouse_response_is_held_to_what_a_connectors_stream_is` — views or a zstd body past the
  cap, and an exception after a whole stream, are refused, and what was read is charged.
- `a_clickhouse_error_reaches_the_operator_not_the_agent` — ClickHouse's messages go to the
  operator log with the mapped code; the agent hears only that the table is not available.
- `a_failed_clickhouse_fetch_is_charged_when_served` — a failed fetch is charged what it read
  before the next source is fetched.
- `clickhouse_credentials_stay_in_the_loader` — the password signs in from benchy's environment
  and is in neither the engine's environment, arguments nor memory, nor the operator log.
- `a_read_only_clickhouse_user_loads_what_any_user_loads` — a `readonly = 1` user loads the same
  columns and rows as any user; a profile it cannot change that makes unreadable Arrow refuses
  the load before a row, naming the setting.
- `check_names_the_fix_for_each_clickhouse_failure` — `benchy-mcp check` prints the mapped schema
  and the estimate, names the fix for each failure, and says the steps after it were skipped.
- `a_clickhouse_source_refuses_what_it_cannot_apply` — a key or combination it cannot apply stops
  startup; one source listing two tables is two tables.

## REQ-17 — HTTP is a built-in source

`type = "http"` puts a URL on the bench without a connector: a body of Arrow IPC, Parquet, CSV or
NDJSON, gzip- or zstd-compressed or not, or a JSON API that answers in pages, the rows at a path
in each ([docs/SOURCES.md](docs/SOURCES.md#http); the design and its measurements,
[docs/HTTP-SOURCE.md](docs/HTTP-SOURCE.md)). Each format takes its fastest measured path into
DuckDB and ends where a file or a stream ends, so an HTTP table is the file's table (REQ-15). The
fetch runs in a network loader of its own (REQ-3), holding that source's credential and no
other's (REQ-5).

- `every_http_config_the_design_shows_is_accepted` — every format and paging style, written as the
  design shows it, loads.
- `an_http_source_refuses_what_it_cannot_apply` — a key, paging style, parameter, header or
  `delimiter` it cannot apply stops startup.
- `an_http_secret_comes_only_from_env_through_auth` — a secret anywhere but `env` read through
  `auth` is refused, and no refusal repeats it.
- `a_repeated_cursor_fails_the_source` — a cursor the API sent before fails the source, saying
  nothing was loaded.
- `an_api_that_ignores_its_page_parameter_fails_the_source` — a page identical to the one before
  is a loop, and fails.
- `offset_paging_advances_by_the_rows_received_and_ends_only_at_an_empty_page` — offset paging
  advances by the rows received, and only an empty page ends it.
- `more_pages_than_max_pages_fails_rather_than_truncates` — past `max_pages` the source fails.
- `a_next_url_on_another_origin_is_refused` — a next URL on another origin is refused, naming
  only where it pointed.
- `link_header_next_is_followed_on_the_same_origin` — RFC 8288 `rel="next"`, resolved against
  the page it came from.
- `every_paging_style_loads_every_page` — each paging style loads every row, asking no more often
  than it needs.
- `a_declared_csv_is_read_in_its_stated_dialect` — a declared CSV is read in its stated dialect,
  not sniffed, and matched by its header; a body in another dialect is refused.
- `every_http_format_loads_as_the_same_table_as_its_file` — REQ-15's fixture in every HTTP format
  answers `describe`, `typeof` and six queries as the CSV file does.
- `an_http_table_has_the_physical_layout_of_a_file_table` — 840,000 rows in each format have the
  CSV file's layout and catalog, naming nothing of the server.
- `a_body_read_into_its_table_is_held_and_refused_whole` — a Parquet body past its cap or its
  pins is refused whole and leaves no block in the bench file; one that fits loads.
- `an_http_error_reaches_the_operator_not_the_agent` — status, message, URL and host go to the
  operator log, an echoed key masked; the agent hears only that the table is not available.
- `http_credentials_stay_in_the_loader` — the token is in neither the engine's environment,
  arguments nor memory; an echoed key is masked in the log (Linux only: the engine is read
  through /proc).
- `an_http_loader_is_confined_before_its_first_request` — the HTTP loader is Landlocked before
  its first answer (REQ-3), and still loads through its staging directory.
- `a_failed_page_fails_the_source_with_no_partial_table` — a failed, cut or rowless page, or a
  cut CSV or NDJSON body, leaves no table.
- `a_csv_or_ndjson_body_is_spooled_to_staging_within_max_bytes_and_removed` — a CSV or NDJSON
  body is spooled to a file within `max_bytes`, refused at the cap, and removed after the load.
- `a_failed_http_fetch_is_charged_when_served` — what a failed fetch read is charged before the
  next source is fetched.
- `an_http_body_is_held_to_max_bytes_as_it_decompresses` — a compressed body is refused at
  `max_bytes` decoded, having decoded no more than the cap and a byte.
- `a_parquet_body_over_max_rows_is_refused_from_its_footer` — a Parquet body over `max_rows` is
  refused on its footer, before a row group is read.
- `a_later_json_page_with_a_wider_type_fails_the_source` — a later page whose values do not fit
  the first page's sniffed type fails, naming the column.
- `a_json_pages_infinity_or_epoch_is_refused_not_read_as_1900` — `infinity`, `-infinity` or
  `epoch` in a JSON page's time column fails the source, naming the column.
- `json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order` — pages are fetched a few
  ahead of their decode, enter the table in page order, and a failed decode fails the source.
- `a_pages_scan_reads_its_rows_and_cursor_as_its_decode_does` — the pager's scan and the decode
  read the same rows and cursor; a page the scan cannot read plainly is decoded first.
- `the_narrowing_is_pushed_down_as_the_operators_parameters` — window, entities and columns
  travel in the operator's parameters, and are re-applied to an API that ignores them.
- `a_refused_pushdown_parameter_fails_naming_the_parameters_sent` — a 400 to pushdown parameters
  fails at once with `E_CONFIG`, naming them, never retried without them.
- `retries_back_off_and_honour_retry_after_within_the_deadline` — 429 and 503 are retried with
  `Retry-After` or jittered backoff within `deadline_secs`; other 4xx but 408 are not retried.
- `retry_after_is_seconds_or_an_http_date` — `Retry-After` in either of RFC 9110's forms.
- `a_redirect_off_the_origin_carries_no_credential` — a redirect is followed with the credential
  on the origin, without it to a `redirect_hosts` entry, and nowhere else.
- `check_shows_the_request_the_schema_to_pin_and_the_first_pages_rows` — `benchy-mcp check`
  prints the masked request, the first page's rows and the schema to pin, from one page.

## REQ-18 — Helpers the operator provides

An operator, or an integrator deploying benchy for a host, may give the agent SQL helpers: views,
scalar macros and table macros, in the config's `[helpers]` (a SQL file, or the SQL inline), for
`benchy-mcp <dir>`/`--config` and `benchy-mcp host --config` alike. They are fixed for the
session: read at startup, and no MCP tool or host-protocol call adds, changes or removes one.
They are SQL the agent could write itself; what they save is the steps it takes to write it.

**A helper macro reads no table.** DuckDB 1.5.5 expands a macro, scalar or table, inside the
caller's query, so a table its body names binds to a CTE of the caller's of that name, with no
error; a view is bound on its own and reads the table (measured). So what reads a table is a
view, and a macro is a formula over its arguments, or a table macro over its arguments alone.
A parameterised recipe that reads a table is a view plus the filter the agent writes.

**A helper macro calls only DuckDB and the helpers.** DuckDB 1.5.5 checks only part of a macro's
body for the functions it calls when it is made, and binds the rest where it is called, so a
helper calling a name nothing had called the agent's macro once the agent made one of that name,
and a CTE of the agent's could then stand in for a table that macro read (measured). Every
function a helper macro calls is DuckDB's or a helper's, named alone, and so taken from the agent.

A helpers job (`Job::Helpers`, a loader confined to its staging directory and locked down as the
engine is) makes the helpers in a file of their own against empty stand-ins of the bench's
tables, since DuckDB binds a body when it is created; the engine attaches that file `READ_ONLY`
between the agent's schema and the tables, and DuckDB binds each helper again on every query. A
helper names tables unqualified (every attach renames a table's catalog), and no object of
the agent's may take a table's name (REQ-9) or the name of a table a helper reads, so the name
can only mean the host's table. After a detach a helper stays made, and fails when called until
that table is attached again.

- `a_helper_from_config_serves_read_only` — config helpers answer like built-ins, `describe` lists
  them with signature and comment, and DuckDB's read-only catalog refuses a `DROP`.
- `a_helper_that_shadows_a_builtin_is_refused_at_startup` — a helper named like a built-in, a
  table or with the `benchy_` prefix, or reading a table no source provides, stops startup.
- `a_helper_named_like_a_parser_keyword_is_refused_at_startup` — a macro helper named like a word
  DuckDB's parser reads itself stops startup; a view under such a name is allowed.
- `an_agent_cannot_replace_or_shadow_a_helper` — a helper's name is taken in every namespace and
  spelling, and DuckDB refuses writes to the helpers' catalog.
- `a_write_to_a_helper_is_told_it_is_a_helper` — a refused `DROP` or `ALTER` of a helper is said
  of a helper, with the hint to make one's own under another name.
- `helpers_survive_a_rebuild` — a rebuilt engine gets the helpers before its tables, as read-only
  and as taken as before.
- `a_helper_cannot_reach_outside_the_bench` — a helper that reads a file or the network, needs an
  extension, keeps an ENUM, or is not `CREATE VIEW` or `CREATE MACRO` stops startup.
- `a_helper_macro_that_reads_a_table_is_refused_at_startup` — a helper macro whose body reads a
  table, by DuckDB's own parse and failing closed, stops startup, naming it and saying to write a
  view.
- `a_macro_body_benchy_cannot_read_reads_a_table` — parser output benchy cannot read counts as
  reading a table.
- `a_helper_macro_that_calls_an_unknown_function_is_refused_at_startup` — a helper macro that
  calls a function neither DuckDB nor a helper has, or one named by a schema or catalog (`s.f(x)`,
  `x.f()`), stops startup, naming the helper and the call; plain DuckDB makes such a macro and
  then calls the agent's macro of that name.
- `a_helper_view_never_calls_the_agents_function` — a helper view calling an unknown function
  stops startup, and a schema and macro of the agent's named like a view's `meter.upper()` leave
  its answer as it was.
- `a_waiting_view_never_calls_the_agents_function` — through the host protocol too: a helper
  macro is checked before any table is attached, and a waiting view is bound by the helpers job
  when its table arrives, where a macro of the agent's never answers for a function it lacks.
- `a_table_free_helper_macro_and_a_view_serve` — views and table-free macros serve, and no CTE of
  the agent's changes their answers.
- `a_string_that_looks_like_sql_runs_beside_helpers` — beside helpers, a string literal runs as
  it does without them; `query()` is still refused (REQ-2).
- `a_lateral_macro_call_equals_the_per_id_call` — a macro reading its arguments once in a
  FROM-less CTE gives through `LATERAL` what per-id calls give, as docs/DIALECT.md advises.
- `a_view_over_a_missing_table_errors_until_attached` — a helper view over a table not attached
  waits and says for what, until the attach makes it; only a view waits.
- `a_host_cannot_attach_under_a_helpers_name` — an attach under a helper's name, made or
  waiting, is refused with `E_NAME`.
- `a_table_a_helper_reads_stays_taken_after_a_detach` — every table a helper reads, found from
  DuckDB's bound plan, stays taken from the agent for the session, detached or not.
- `a_helper_that_names_a_table_in_a_string_is_refused_at_startup` — a helper that reads the
  bench through a table named in a string stops startup.
- `the_table_a_waiting_helper_needs_is_taken` — the table a waiting helper waits for is taken
  from the start.
- `the_tables_helpers_read_stay_taken_across_a_rebuild` — every engine the server starts takes
  the tables the helpers have read, a detached one too.
- `a_table_the_agent_had_first_stays_its_own` — a table the agent had before a waiting helper
  named it stays the agent's, and the host's attach of it is refused until the agent drops it.
- `a_helper_is_a_create_view_or_macro_with_a_plain_name` — the config's SQL is split as DuckDB
  splits it, and each statement checked for kind and name before anything runs.
- `helper_sql_splits_where_duckdb_would` — quotes, comments and dollar quotes hold their `;`,
  and the `--` lines directly above a statement are its comment.
- `a_cut_helper_comment_or_line_is_told_to_the_operator` — a comment `describe` cuts at 200
  bytes, or a line it cuts at 320, is named on stderr with the bytes cut, by `benchy-mcp <dir>`
  and `benchy-mcp host` alike, and the session starts; a helper under both caps is not named.
- `a_cut_is_told_from_the_first_byte_past_a_cap` — a comment of 200 bytes and a line of 320 are
  whole and not named; one byte more is, with the bytes `describe` keeps, a character across the
  cap kept whole or not at all.
- `a_waiting_view_whose_line_is_cut_once_made_is_told_to_the_operator` — a waiting view whose
  line passes 320 bytes only once its table's columns are in it is named on stderr when the
  attach makes it, once.

A helper costs nothing to a session that has none: no job runs and no catalog is attached.
Measured in docs/history/PERFORMANCE-rounds.md, "Helpers".
