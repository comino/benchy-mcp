//! REQ-2 layer 2: the locked engine — the boundary that actually stops file and network reads.
//!
//! Of the cases in `corpus/escape.toml`, this layer alone refuses most — and, critically, **all
//! five** the statement allowlist alone lets through. `the_corpus_records_why_the_engine_is_the_\
//! boundary` measures that on every run. It is the primary mechanism, not the backstop.
//!
//! Order matters. `temp_directory` cannot be changed once external access is off, and nothing
//! can be changed after `lock_configuration`.

use duckdb::Connection;

/// Directories DuckDB derives from `HOME`. Pointed at a path that does not exist, so a leak
/// through `duckdb_settings()` names nothing real. Path disclosure survives both SQL-level
/// mechanisms — see [`crate::lockdown::NOTE_PATH_DISCLOSURE`].
const NOWHERE: &str = "/benchy-none";

/// Neither the statement allowlist nor the locked engine can stop `PRAGMA database_list`,
/// `duckdb_databases()`, `duckdb_settings()` or `current_setting()` — all four succeeded in both
/// configurations when measured. No SQL-level mechanism will. The mitigation is that the path be
/// uninteresting: it names this session's own temporary workspace and nothing else
/// (`path_disclosure_names_only_our_own_private_workspace`).
pub const NOTE_PATH_DISCLOSURE: &str =
    "catalog functions disclose the attached path; keep the path uninteresting";

/// The whole lock at once, on one thread: for what runs no agent SQL in a process of its own (a
/// loader's helpers job) and for the tests.
pub fn apply(conn: &Connection, memory_limit: &str) -> duckdb::Result<()> {
    prepare(conn, memory_limit, None)?;
    lock(conn, 1)
}

/// Everything of the lock but its last step, [`lock`], with `allowed` still open to DuckDB
/// afterwards — the one directory a sealed engine attaches from. It has to be named before
/// external access is turned off: DuckDB refuses to change `allowed_directories` once it is.
///
/// Apart from [`lock`] because of Landlock, which confines the thread that asks and the threads
/// it starts afterwards, never one already running (`sandbox::alone`). DuckDB starts its workers
/// as it opens, and `threads = 1` here sends them away: measured on DuckDB 1.5.5, kernel 6.8,
/// four cores, three workers beside the opening thread, gone within 300 ms of the `SET`. So the
/// engine is confined alone between the two, and the threads agent SQL runs on are [`lock`]'s.
pub fn prepare(
    conn: &Connection,
    memory_limit: &str,
    allowed: Option<&std::path::Path>,
) -> duckdb::Result<()> {
    if let Some(dir) = allowed {
        let d = format!("{}/", dir.display()).replace('\'', "''");
        conn.execute_batch(&format!("SET allowed_directories = ['{d}']"))?;
    }
    for stmt in [
        // must precede enable_external_access
        "SET temp_directory = ''",
        "SET home_directory = '/benchy-none'",
        "SET secret_directory = '/benchy-none'",
        "SET extension_directory = '/benchy-none'",
        // the boundary itself
        "SET enable_external_access = false",
        "SET autoinstall_known_extensions = false",
        "SET autoload_known_extensions = false",
        // One thread until the process is confined: see above. What agent SQL runs on is
        // [`lock`]'s to set.
        "SET threads = 1",
    ] {
        conn.execute_batch(stmt)?;
    }

    conn.execute_batch(&format!(
        "SET memory_limit = '{}'",
        memory_limit.replace('\'', "''")
    ))?;

    // Determinism: a result must not depend on the host's zone.
    //
    // GLOBAL, because `TimeZone` is per connection: a plain `SET` reaches this connection alone,
    // and the agent's, opened before it, kept the host's zone (measured on DuckDB 1.5.5 in
    // Europe/Berlin). A global value reaches every connection without one of its own, opened
    // before or after; the lock below keeps any connection from setting one
    // (REQ-10 `the_agent_connection_is_utc_on_a_host_that_is_not`). The seal proves it on the
    // agent's connection (`Workspace::verify_utc`).
    //
    // `TimeZone` is an ICU setting. The official library carries ICU, but a library linked
    // through DUCKDB_LIB_DIR may not — one built from the crates.io sources cannot. So this is
    // best-effort, and when it fails we prove UTC is the default rather than assume it: without
    // ICU, DuckDB has no time-zone machinery to be wrong about.
    if conn.execute_batch("SET GLOBAL TimeZone = 'UTC'").is_err() {
        let icu: i64 = conn
            .query_row(
                "SELECT count(*) FROM system.main.duckdb_extensions() \
                 WHERE extension_name = 'icu' AND loaded",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        time_zone_refused(icu)?;
    }

    debug_assert_eq!(NOWHERE, "/benchy-none");
    Ok(())
}

/// The lock's last step: the threads agent SQL runs on, and then nothing [`prepare`] set, nor
/// this, can be changed. Where the process confines itself it comes after: DuckDB starts the
/// workers `threads` asks for at the `SET`, from the thread that sets it, and Landlock confines
/// every thread a confined one starts (REQ-7 `the_engines_threads_are_started_after_it_is_confined`;
/// set before the confinement, the engine refuses to serve, `4 threads were running`).
///
/// Measured on DuckDB 1.5.5: `SET threads` is taken with external access off, and refused once
/// the configuration is locked, as `SET GLOBAL threads`, `RESET threads` and `external_threads`
/// are, so the agent cannot raise its own (`Workspace::verify_locked` proves it). DuckDB's
/// memory budget is one for the database whatever the threads; neither bounds planning — see
/// `Caps::memory_limit`.
pub fn lock(conn: &Connection, threads: usize) -> duckdb::Result<()> {
    conn.execute_batch(&format!("SET threads = {threads}"))?;
    // nothing above can be changed after this
    conn.execute_batch("SET lock_configuration = true")
}

/// What a refused `SET TimeZone` means, given how many loaded extensions are ICU. With ICU loaded
/// it is a real failure, not an absent feature, and a silent pass would make results
/// host-dependent. Apart from [`prepare`] because the official library always carries ICU
/// and always accepts the setting, so there it is never reached
/// (REQ-10 `a_refused_time_zone_fails_only_where_icu_is_loaded`).
fn time_zone_refused(icu_loaded: i64) -> duckdb::Result<()> {
    if icu_loaded > 0 {
        return Err(duckdb::Error::InvalidParameterName(
            "ICU is loaded but the time zone could not be pinned to UTC".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Results are in UTC whatever the host's zone. A library without ICU has no zone to set
    /// and UTC is its only one; with ICU loaded, a refusal to set UTC stops the engine rather
    /// than leaving results host-dependent. The shipped library always has ICU and always
    /// accepts, so this is tested at the decision.
    #[test]
    fn a_refused_time_zone_fails_only_where_icu_is_loaded() {
        assert!(
            super::time_zone_refused(0).is_ok(),
            "no ICU: UTC is DuckDB's only zone"
        );
        assert!(
            super::time_zone_refused(1).is_err(),
            "ICU loaded and refusing is a failure"
        );
    }
}
