# Security

benchy-mcp is an experimental prototype. It has **not been independently security-audited**, and
it changes often. Its containment is the point of it, so a report that breaks it is welcome, but
do not rely on it as your only boundary for data that must not leak; the README's "Limits"
section says what it does not cover.

## Threat model

One page, for a reporter and for triage. [REQUIREMENTS.md](REQUIREMENTS.md) is normative; each
claim here names the requirement, and the test, that holds it.

**Trusted.** The **operator**, who writes the config: its sources, credentials, ceilings, SQL
helpers (REQ-18) and exec connectors (REQ-5). The **host**, which starts one benchy per session,
routes the right user to it and fetches that user's data (REQ-13); it may lower the operator's
limits for its session and never raise them (`hello`'s `limits`, including `threads`). benchy
cannot check the routing: the guarantee starts at the connection.

**Untrusted.** The **agent**, whose SQL and tool calls are all it can send, and the **data**:
files, ClickHouse and HTTP responses, a connector's Arrow output and a host's attached streams,
any of which may carry a prompt injection aimed at the agent.

**What benchy defends**, against both:

- **Confidentiality.** Agent SQL reads no file and opens no connection outside the session's
  workspace; one session cannot see or change another's data, scratch or results, and no tool or
  host-protocol parameter can name a path, workspace, session, URL or database (REQ-1
  `cross_session_canary`, `schema_has_no_addressing`); no credential reaches the engine, the
  agent, a result or an error (REQ-5 `credentials_never_cross`,
  `the_engine_inherits_no_environment_and_no_descriptor`).
- **Integrity of the loaded tables**, attached read-only and enforced by DuckDB, not by reading
  statement text (REQ-9 `loaded_is_readonly`, `the_agent_cannot_change_a_hosts_table`), and of
  **benchy's own catalog reads**: each names `system.main`, so no object of the agent's changes
  what the attach check, `describe` or the hints read (REQ-9
  `benchys_catalog_reads_cannot_be_shadowed`), and an attached file brings its one table and
  nothing else (REQ-2 `an_attached_file_brings_its_table_and_nothing_else`).
- **Availability.** A wall deadline, DuckDB's memory budget and a ceiling on the engine's and each
  loader's resident memory (REQ-7, REQ-8), capped results (REQ-10), and admission control before
  a fetch (REQ-12). A runaway loses its query and the agent's scratch, not the bench.

**The containment layers**, in the order they are relied on:

1. **Lockdown** (`src/lockdown.rs`), the primary boundary: DuckDB's external access off,
   extensions off, the configuration locked, and each proven at the seal by trying to undo it
   (REQ-2 `the_corpus_records_why_the_engine_is_the_boundary`,
   `the_table_functions_left_to_the_lock_are_refused_by_it`).
2. **The guard** (`src/guard.rs`): a statement allowlist that keeps the lock from being undone and
   refuses `ATTACH`, `COPY` and `EXPORT`, and a **table-function allowlist** read from the batch's
   text before any of it is prepared. Every table function DuckDB ships is classified as the
   agent's, left to the lock (the file and network readers) or refused, and one it does not know
   is refused; `query()` and `json_execute_serialized_sql` are refused wherever they are written,
   since SQL run from a string cannot be read before it runs (REQ-2
   `every_table_function_duckdb_has_is_classified`, `a_refused_call_runs_nothing_of_its_batch`,
   `the_table_function_allowlist_does_what_the_corpus_says`). The guard alone is not enough:
   file-reading table functions are typed `SELECT`, and only the lock stops them.
3. **The Landlock sandbox** (`src/sandbox.rs`, Linux only), load-bearing: the engine may read
   only its workspace, a file loader its one input, and a ClickHouse or HTTP loader its staging
   directory, the resolver's files and TCP to its source's port (REQ-3
   `a_network_loader_reaches_only_its_source_and_its_staging`). Landlock confines only threads
   started after it, so the engine confines itself while it is its only thread, and the workers
   `--threads` asks for are started after that (REQ-3
   `a_thread_already_running_is_never_confined_as_if_it_were_not`, REQ-7
   `the_engines_threads_are_started_after_it_is_confined`). Without Landlock benchy refuses to
   serve unless started with `--allow-unsandboxed` (REQ-3
   `a_missing_sandbox_is_refused_without_the_flag`). **The network is confined only from Linux
   6.7** (Landlock ABI 4): below it the posture is `partial`, files confined and TCP not, and
   `hello`, `doctor` and the log say so; `--require-full-sandbox` refuses to start there (REQ-3
   `a_kernel_without_tcp_rules_is_reported_as_partial`).
4. **Process separation and SIGKILL.** The agent's SQL runs in an engine process of its own, so a
   query still being planned, which no interrupt reaches, is stopped by killing it; the server
   rebuilds the bench without reading a source again (REQ-8 `planner_blowup_killed`,
   `an_engine_past_its_memory_ceiling_is_killed_even_while_planning`). Each source is parsed by a
   loader of its own.
5. **Connector output is untrusted** (`src/exec.rs`, `src/ipc.rs`): checked against the schema
   the operator declared, never the connector's; a stream without its end marker is a failure; a
   compressed buffer's stated size is checked against the byte bound before it is decoded, and
   zstd is refused (REQ-6 `a_surprise_column_is_refused_rather_than_dropped`,
   `a_stream_without_its_end_marker_is_refused`,
   `a_compressed_buffer_counts_what_it_decodes_to_and_may_not_lie`).

**Not a security boundary: the correctness of the agent's own answers.** The agent can mislead
itself: a CTE or scratch table it writes, a wrong join, a result it misreads. The rules that keep
a name meaning one thing are correctness aids: no object of the agent's may take a bench table's,
a built-in's, a parser word's, a system view's or a helper's name (REQ-9
`an_agent_object_cannot_shadow_a_host_object_or_builtin`), and a helper macro may not read a
table, so a CTE of the caller's cannot stand in for one (REQ-18
`a_helper_macro_that_reads_a_table_is_refused_at_startup`). A gap in them is an ordinary bug,
not an escape, unless it also crosses one of the boundaries above.

**macOS is for development only.** It has no OS sandbox: containment there is the SQL layer
alone, benchy serves only with `--allow-unsandboxed`, and the memory ceilings are not enforced.
There is no Windows build.

**Known limits, out of scope.** The server's own input is not bounded: an MCP request read from
stdin has no size cap, and a host's queued requests no count cap (each host frame is capped).
The engine and loader ceilings bound those processes, not the parent server; use a whole-session
limit such as Docker's `--memory`. An exec connector runs unconfined with its process's rights
and its source's credentials, trusted by the operator who configured it. In the drop-in posture
the agent can often edit the config, so the config is no boundary there. Other processes of the
same user can read a connector's environment through `/proc`; benchy does not defend against its
own user.

**Triage.** A report is an **escape**, handled privately as below, when untrusted input — the
agent's SQL or tool calls, or data — reads or writes outside the session's workspace, reaches
the network, sees or changes another session, reaches a credential, changes a loaded table or
what benchy reads from the catalog, or gets past a ceiling the README documents, on Linux with
the sandbox enforced. Anything else is an ordinary **bug**: a wrong answer the agent could have
avoided, a shadowing or naming gap with no crossing, anything on macOS or with
`--allow-unsandboxed`, what a connector does, and the limits above.

## Reporting a vulnerability

Report privately, through GitHub's private vulnerability reporting: the repository's
**Security** tab, then **Report a vulnerability**. Please do not open a public issue, pull
request or discussion for anything below until a fix is released.

Say which version (`benchy-mcp --version`), the platform and what `benchy-mcp doctor` prints, and
the smallest config, data and SQL or host-protocol frames that show it. A case for
`corpus/escape.toml` is the ideal form for a SQL escape. This is a one-maintainer project: expect a
first reply within about a week. A confirmed report gets a failing test, a fix in a release, and
credit in CHANGELOG.md unless you would rather not.

## What counts

A security bug here is a way across a boundary the project claims to hold
([REQUIREMENTS.md](REQUIREMENTS.md) is normative), for example:

- **Containment escapes.** SQL, a data file or a connector's output that reads or writes the
  filesystem or the network, undoes DuckDB's lockdown, gets past the statement guard, or runs
  outside the Landlock sandbox on Linux (REQ-2, REQ-3, REQ-6).
- **Cross-session access.** One session seeing or changing another session's data, scratch
  tables, results or files, or a tool or host-protocol parameter that names a path, workspace,
  session, URL or database (REQ-1, REQ-13).
- **Credential exposure.** A source credential or an exec connector's environment reaching the
  engine, the agent, a result, an error message or a log (REQ-5, REQ-15).
- **The release itself.** A bundle, image or install path that ships something other than what
  the release workflow built, such as a DuckDB library that is not DuckDB's release file; or a
  bundle's SBOM, or the release's `compat.json`, that does not say what it ships.

Not security bugs, though still worth an ordinary issue: anything on native macOS, which has no
OS sandbox and serves only with `--allow-unsandboxed`; what an exec connector does, since it runs
with its process's rights and is trusted by the operator who configured it; the drop-in posture,
where the agent can already edit the config; and resource exhaustion beyond the ceilings the
README documents.

## Supported versions

Only the latest release is supported: fixes go into a new release, not into older ones. The
`main` branch may be ahead of it.
