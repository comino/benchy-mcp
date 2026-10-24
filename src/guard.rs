//! REQ-2 layer 1: the statement allowlist.
//!
//! Two checks. The batch is classified **up front**, so a plainly refused statement stops the
//! batch before anything runs. Then every statement is checked again **immediately before it
//! runs**, in [`crate::workspace::Workspace::work`], because the up-front pass cannot bind a
//! statement that depends on an object an earlier statement in the same batch creates:
//! `CREATE TABLE t …; PREPARE p AS SELECT … FROM t` only becomes classifiable once the `CREATE`
//! has run. The up-front pass cannot simply refuse what it cannot bind, either — DuckDB splits a
//! dynamic `PIVOT` into a `CREATE` plus a statement referencing a type that does not exist until
//! the `CREATE` has run.
//!
//! This layer does **not** stop file reads, and cannot. `read_text`, `read_blob` and `sniff_csv`
//! are typed `SELECT`; five cases in the escape corpus reach a canary outside the workspace
//! through this layer alone. Stopping them is [`crate::lockdown`]'s job. What this
//! layer stops is that lock being *removed* — `SET`, `RESET`, `INSTALL`, `LOAD` — plus `ATTACH`.
//! `COPY` and `EXPORT` it would refuse too, but under the lock they never reach it: the locked
//! engine cannot prepare them (`locked = "unprepared"` in `corpus/escape.toml`), so they are
//! refused when their turn comes, not up front, and the statements before them in the batch keep
//! their effects.
//!
//! A statement's type does not say what its table functions do. `enable_logging()`,
//! `enable_profiling()`, `checkpoint()` and the like are called in a `SELECT`, and none of them
//! reaches outside, so neither the type nor the lock stops them. [`Calls`] is the second half of
//! this layer: an allowlist of the table functions DuckDB has, checked against the batch's text
//! before anything in it is prepared.

// A bindgen enum constant is prefixed with its type name. An unprefixed name is not a constant
// at all: it is a binding pattern that matches everything, silently allowing the whole allowlist
// through. That is not hypothetical — it happened, and reported `ATTACH` as `SELECT`.
#![deny(unreachable_patterns)]

use duckdb::ffi;
use std::ffi::{CStr, CString};
use std::ptr;

/// What the agent may run inside its own workspace.
///
/// `SELECT` to read, and full freedom over objects it created itself — REQ-9's playground.
/// Everything else is refused before execution.
///
/// Not `EXPLAIN`: `EXPLAIN ANALYZE` runs the statement it explains, and DuckDB types the whole
/// as EXPLAIN, so allowing it let refused kinds through (`explain_cannot_smuggle_a_refused_\
/// statement`).
pub(crate) fn is_allowed(t: ffi::duckdb_statement_type) -> bool {
    matches!(
        t,
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_INSERT
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_UPDATE
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DELETE
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DROP
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ALTER
            | ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_MERGE_INTO
    )
}

pub fn type_name(t: ffi::duckdb_statement_type) -> &'static str {
    match t {
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT => "SELECT",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_INSERT => "INSERT",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_UPDATE => "UPDATE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DELETE => "DELETE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE => "CREATE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DROP => "DROP",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ALTER => "ALTER",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPLAIN => "EXPLAIN",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ATTACH => "ATTACH",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DETACH => "DETACH",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_COPY => "COPY",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPORT => "EXPORT",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_LOAD => "LOAD",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_PRAGMA => "PRAGMA",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SET => "SET",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CALL => "CALL",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_TRANSACTION => "TRANSACTION",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_PREPARE => "PREPARE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXECUTE => "EXECUTE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_VACUUM => "VACUUM",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ANALYZE => "ANALYZE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_MULTI => "MULTI",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_RELATION => "RELATION",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_LOGICAL_PLAN => "LOGICAL_PLAN",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE_FUNC => "CREATE_FUNC",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_VARIABLE_SET => "VARIABLE_SET",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXTENSION => "EXTENSION",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_COPY_DATABASE => "COPY_DATABASE",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_UPDATE_EXTENSIONS => "UPDATE EXTENSIONS",
        ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_MERGE_INTO => "MERGE",
        _ => "UNKNOWN",
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every statement in the batch is permitted.
    Allow(Vec<&'static str>),
    /// Statement `index` (1-based) is `kind`, which is refused. Nothing after it runs.
    Refuse { index: usize, kind: &'static str },
    /// The batch did not parse. Refused, because an unparseable batch cannot be reasoned about.
    Unparseable(String),
}

/// A raw connection used only to classify statements. Classification requires `prepare`, which
/// binds — so this connection must see the same objects the workspace does.
pub struct Classifier {
    con: ffi::duckdb_connection,
}

impl Classifier {
    /// # Safety
    /// `con` must outlive this `Classifier` and must not be used concurrently.
    pub unsafe fn from_raw(con: ffi::duckdb_connection) -> Self {
        Self { con }
    }

    pub fn check(&self, sql: &str) -> Verdict {
        let Ok(c) = CString::new(sql) else {
            return Verdict::Unparseable("statement contains a NUL byte".into());
        };
        unsafe {
            let mut ex: ffi::duckdb_extracted_statements = ptr::null_mut();
            let n = ffi::duckdb_extract_statements(self.con, c.as_ptr(), &mut ex);
            if n == 0 {
                let e = ffi::duckdb_extract_statements_error(ex);
                let msg = if e.is_null() {
                    "no statement".to_string()
                } else {
                    CStr::from_ptr(e).to_string_lossy().into_owned()
                };
                ffi::duckdb_destroy_extracted(&mut ex);
                return Verdict::Unparseable(msg.lines().next().unwrap_or("").to_string());
            }
            let mut kinds = Vec::with_capacity(n as usize);
            // DuckDB 1.5.5 runs a statement that keeps a dynamic PIVOT, `CREATE TABLE … AS PIVOT`
            // or an INSERT, UPDATE, DELETE or MERGE that reads one, as a transaction of its own:
            // a TRANSACTION, the pivot's CREATE TYPE, the statement, a TRANSACTION. So a
            // TRANSACTION before a CREATE passes here, as does the next one; the run holds them
            // to that shape, and runs neither (`Workspace::work`, `a_pivot_into_a_table_runs`).
            // Those and the CREATE after the first are not the agent's, so a refusal does not
            // count them; a second pivot's CREATE TYPE is not told apart here, only when the
            // batch runs.
            let mut opening: Option<usize> = None;
            let mut open: Option<usize> = None;
            let mut added = 0usize;
            for i in 0..n {
                let mut prep: ffi::duckdb_prepared_statement = ptr::null_mut();
                let ok = ffi::duckdb_prepare_extracted_statement(self.con, ex, i, &mut prep)
                    == ffi::duckdb_state_DuckDBSuccess;
                // A statement that will not bind yet is not refused here: it may depend on an
                // object an earlier statement in the batch creates. It is classified again when
                // it is prepared to run, where that object exists.
                let t = if ok {
                    Some(ffi::duckdb_prepared_statement_type(prep))
                } else {
                    None
                };
                if let Some(at) = opening.take() {
                    // Not one that does not bind yet: `COPY FROM DATABASE` is a TRANSACTION and
                    // two statements that do not bind under the lock, as a pivot's CREATE TYPE
                    // over a table made earlier in the batch would be.
                    if t != Some(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE) {
                        ffi::duckdb_destroy_prepare(&mut prep);
                        ffi::duckdb_destroy_extracted(&mut ex);
                        return Verdict::Refuse {
                            index: at + 1,
                            kind: "TRANSACTION",
                        };
                    }
                    open = Some(at);
                    added += 1;
                }
                let transaction =
                    t == Some(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_TRANSACTION);
                let verdict = if transaction && open.take().is_some() {
                    added += 1;
                    Ok("TRANSACTION")
                } else if transaction {
                    opening = Some(i as usize - added);
                    added += 1;
                    Ok("TRANSACTION")
                } else if let Some(t) = t {
                    if is_allowed(t) {
                        Ok(type_name(t))
                    } else {
                        Err(type_name(t))
                    }
                } else {
                    Ok("UNBOUND")
                };
                ffi::duckdb_destroy_prepare(&mut prep);
                match verdict {
                    Ok(k) => kinds.push(k),
                    Err(k) => {
                        ffi::duckdb_destroy_extracted(&mut ex);
                        return Verdict::Refuse {
                            index: i as usize - added + 1,
                            kind: k,
                        };
                    }
                }
            }
            ffi::duckdb_destroy_extracted(&mut ex);
            // A TRANSACTION that ends the batch, or one the batch never closes, wraps nothing.
            if let Some(at) = opening.or(open) {
                return Verdict::Refuse {
                    index: at + 1,
                    kind: "TRANSACTION",
                };
            }
            Verdict::Allow(kinds)
        }
    }
}

/// What one of DuckDB's table functions is to the agent ([`TABLE_FUNCTIONS`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    /// The agent's: it reads its arguments, the agent's tables, the catalog or the session's
    /// settings, and changes nothing.
    Agent,
    /// Reads a file or the network, which the locked engine refuses, every one of them
    /// (`the_table_functions_left_to_the_lock_are_refused_by_it`). Not refused by name: the lock
    /// is the boundary for files, the escape corpus goes on proving it through the whole stack,
    /// and the hint for a file reader goes on naming the tables already on the bench.
    Lock,
    /// Refused by name, because it does what this says.
    Refused(&'static str),
}

const STATE: &str = "changes the engine's settings or storage for the rest of the session";
const STRING: &str = "runs SQL from a string, which benchy cannot read before it runs: not for \
     the table functions it calls, nor, beside the operator's helper macros, for the CTEs it \
     names. Write the query out instead; `query_table('name')` still reads a table named in a \
     string";
const INSIDE: &str = "shows the engine's own workings (its log, memory, files, extensions or \
     connections), not your data";
const SECRETS: &str = "lists secrets, and this engine holds none";
const INTERNAL: &str = "is DuckDB's internal or test machinery";
const UNKNOWN: &str = "is a table function benchy has not classified for this version of DuckDB";
const UNREAD_VIEW: &str = "is a view whose SQL benchy could not read through";

/// The DuckDB [`TABLE_FUNCTIONS`] was measured on. On any other, what is refused is read from
/// that engine's own catalog instead ([`Calls::for_version`]).
pub const MEASURED_ON: &str = "v1.5.5";

/// Every table function and table macro in DuckDB 1.5.5's `system` catalog, 89 and 4, and what
/// each is to the agent. `every_table_function_duckdb_has_is_classified` holds this to the
/// shipped engine's `duckdb_functions()`, so a DuckDB that brings a new one fails a test until it
/// is classified here, and one that runs elsewhere refuses it ([`Calls::for_version`]).
///
/// Refused because the statement allowlist and the lock both let them run, measured on 1.5.5
/// through `query`: `enable_logging(storage='file', …)` left every later statement of the
/// session failing with "Permission Error: Cannot access directory", `disable_logging()`
/// included; `enable_profiling()` wrote every later plan tree to the operator's log;
/// `enable_logging()` in memory let the agent read the engine's log; `checkpoint()`,
/// `force_checkpoint()` and `truncate_duckdb_logs()` ran
/// (`a_table_function_that_changes_the_engine_is_refused`). `query` and
/// `json_execute_serialized_sql` run a string as SQL, which may be built as it runs, so a look at
/// the text cannot read what it calls; with them, every function refused here would be one
/// `||` away. Each side effect above happened when the statement ran, not when it was prepared
/// (measured: `enable_logging` and `enable_profiling` prepared left the settings untouched), but
/// the check is made before the batch is prepared all the same, so nothing refused is even bound.
pub const TABLE_FUNCTIONS: &[(&str, Use)] = &[
    ("arrow_scan", Use::Refused(INTERNAL)),
    ("arrow_scan_dumb", Use::Refused(INTERNAL)),
    ("check_peg_parser", Use::Refused(INTERNAL)),
    ("checkpoint", Use::Refused(STATE)),
    ("disable_logging", Use::Refused(STATE)),
    ("disable_peg_parser", Use::Refused(STATE)),
    ("disable_profiling", Use::Refused(STATE)),
    ("duckdb_approx_database_count", Use::Refused(INSIDE)),
    ("duckdb_columns", Use::Agent),
    ("duckdb_connection_count", Use::Refused(INSIDE)),
    ("duckdb_constraints", Use::Agent),
    ("duckdb_coordinate_systems", Use::Agent),
    ("duckdb_databases", Use::Agent),
    ("duckdb_dependencies", Use::Agent),
    ("duckdb_extensions", Use::Refused(INSIDE)),
    ("duckdb_external_file_cache", Use::Refused(INSIDE)),
    ("duckdb_functions", Use::Agent),
    ("duckdb_indexes", Use::Agent),
    ("duckdb_keywords", Use::Agent),
    ("duckdb_log_contexts", Use::Refused(INSIDE)),
    ("duckdb_logs", Use::Refused(INSIDE)),
    ("duckdb_logs_parsed", Use::Refused(INSIDE)),
    ("duckdb_memory", Use::Refused(INSIDE)),
    ("duckdb_optimizers", Use::Agent),
    ("duckdb_prepared_statements", Use::Agent),
    ("duckdb_profiling_settings", Use::Agent),
    ("duckdb_schemas", Use::Agent),
    ("duckdb_secret_types", Use::Refused(SECRETS)),
    ("duckdb_secrets", Use::Refused(SECRETS)),
    ("duckdb_sequences", Use::Agent),
    ("duckdb_settings", Use::Agent),
    ("duckdb_table_sample", Use::Refused(INSIDE)),
    ("duckdb_tables", Use::Agent),
    ("duckdb_temporary_files", Use::Refused(INSIDE)),
    ("duckdb_types", Use::Agent),
    ("duckdb_variables", Use::Agent),
    ("duckdb_views", Use::Agent),
    ("enable_logging", Use::Refused(STATE)),
    ("enable_peg_parser", Use::Refused(STATE)),
    ("enable_profiling", Use::Refused(STATE)),
    ("force_checkpoint", Use::Refused(STATE)),
    ("generate_series", Use::Agent),
    ("glob", Use::Lock),
    ("histogram", Use::Agent),
    ("histogram_values", Use::Agent),
    ("icu_calendar_names", Use::Agent),
    ("json_each", Use::Agent),
    ("json_execute_serialized_sql", Use::Refused(STRING)),
    ("json_tree", Use::Agent),
    ("parquet_bloom_probe", Use::Lock),
    ("parquet_file_metadata", Use::Lock),
    ("parquet_full_metadata", Use::Lock),
    ("parquet_kv_metadata", Use::Lock),
    ("parquet_metadata", Use::Lock),
    ("parquet_scan", Use::Lock),
    ("parquet_schema", Use::Lock),
    ("pg_timezone_names", Use::Agent),
    // The tables behind the PRAGMAs DuckDB 1.5.5 types SELECT (`PRAGMA table_info('t')` is
    // `SELECT * FROM pragma_table_info('t')`), which the agent may run anyway
    // (`the_pragmas_typed_select_are_the_measured_ones`).
    ("pragma_collations", Use::Agent),
    ("pragma_database_size", Use::Agent),
    ("pragma_metadata_info", Use::Agent),
    ("pragma_platform", Use::Agent),
    ("pragma_show", Use::Agent),
    ("pragma_storage_info", Use::Agent),
    ("pragma_table_info", Use::Agent),
    ("pragma_user_agent", Use::Agent),
    ("pragma_version", Use::Agent),
    ("query", Use::Refused(STRING)),
    // Names a table or view, never a query or a function (measured: `query_table('checkpoint()')`
    // is "Table with name checkpoint() does not exist!"). A name built as it runs can reach the
    // `duckdb_logs` view past [`REFUSED_VIEWS`], and finds the log empty: nothing that turns
    // logging on runs here.
    ("query_table", Use::Agent),
    ("range", Use::Agent),
    ("read_blob", Use::Lock),
    ("read_csv", Use::Lock),
    ("read_csv_auto", Use::Lock),
    ("read_duckdb", Use::Lock),
    ("read_json", Use::Lock),
    ("read_json_auto", Use::Lock),
    ("read_json_objects", Use::Lock),
    ("read_json_objects_auto", Use::Lock),
    ("read_ndjson", Use::Lock),
    ("read_ndjson_auto", Use::Lock),
    ("read_ndjson_objects", Use::Lock),
    ("read_parquet", Use::Lock),
    ("read_text", Use::Lock),
    ("repeat", Use::Agent),
    ("repeat_row", Use::Agent),
    ("seq_scan", Use::Refused(INTERNAL)),
    ("sniff_csv", Use::Lock),
    ("sql_auto_complete", Use::Refused(INTERNAL)),
    ("summary", Use::Agent),
    ("test_all_types", Use::Refused(INTERNAL)),
    ("test_vector_types", Use::Refused(INTERNAL)),
    ("truncate_duckdb_logs", Use::Refused(STATE)),
    ("unnest", Use::Agent),
    ("which_secret", Use::Refused(SECRETS)),
];

/// The views of DuckDB 1.5.5's `system` catalog whose body calls a refused table function,
/// refused wherever they are named: `FROM duckdb_logs` reads `duckdb_logs()` with no call in the
/// text. `every_table_function_duckdb_has_is_classified` holds this to the shipped catalog.
pub const REFUSED_VIEWS: &[(&str, &str)] = &[("duckdb_logs", INSIDE)];

/// The system catalog's table functions and table macros, for an engine this was not measured on.
pub(crate) const CATALOG_FUNCTIONS: &CStr = c"SELECT DISTINCT function_name, '' \
      FROM system.main.duckdb_functions() \
      WHERE database_name = 'system' AND function_type IN ('table', 'table_macro')";
/// The system catalog's views, and the SQL of each.
pub(crate) const CATALOG_VIEWS: &CStr = c"SELECT view_name, coalesce(sql, '') \
      FROM system.main.duckdb_views() WHERE database_name = 'system'";

/// REQ-2: which of DuckDB's table functions the agent may not call, and which system views it
/// may not name, checked on the text of a batch before any of it is prepared.
///
/// On the text, not on DuckDB's parse: `json_serialize_sql` serializes SELECTs alone, so a
/// `CREATE TABLE … AS`, an `INSERT … SELECT`, an `UPDATE … FROM` or a `MERGE … USING` would go
/// unread; and a function inside an agent's view or macro is named in the text that makes it.
/// A call needs its name in the text, since DuckDB has no other spelling of an identifier
/// (`U&"…"` is not implemented on 1.5.5) and folds ASCII case only; the only ways to call one
/// without writing it are SQL run from a string, which is refused here itself, and DuckDB's own
/// rewrites — the PRAGMAs it types SELECT (`the_pragmas_typed_select_are_the_measured_ones`),
/// `SUMMARIZE`, `DESCRIBE`, `SHOW` and a system macro's or view's body. Measured, none of those
/// calls one refused here but `PRAGMA extension_versions`, which reads `duckdb_extensions()` and
/// is refused by the lock, and a system view whose body calls one is refused by its name
/// ([`REFUSED_VIEWS`]). A string literal's text is no call, and is not read ([`code`]): a
/// refused name in one is only a string (#35). Elsewhere the check errs one way only: a refused
/// name in a comment or a quoted name is refused too, and a name joined to another by `$` or a
/// non-ASCII letter, which DuckDB reads as one identifier, is read here as a word of its own.
pub struct Calls {
    /// Folded name to why.
    functions: std::collections::HashMap<String, &'static str>,
    views: std::collections::HashMap<String, &'static str>,
    /// Why this engine's table functions are not known, when they are not: then every batch is
    /// refused.
    unread: Option<String>,
}

impl Calls {
    /// The refusals [`TABLE_FUNCTIONS`] and [`REFUSED_VIEWS`] record.
    pub fn measured() -> Calls {
        Calls {
            functions: TABLE_FUNCTIONS
                .iter()
                .filter_map(|(name, what)| match what {
                    Use::Refused(why) => Some((name.to_string(), *why)),
                    _ => None,
                })
                .collect(),
            views: REFUSED_VIEWS
                .iter()
                .map(|(name, why)| (name.to_string(), *why))
                .collect(),
            unread: None,
        }
    }

    /// The refusals for an engine whose catalog holds the table functions and table macros
    /// `functions` and the views `views` (name, SQL): every function not classified as the
    /// agent's or the lock's is refused, and every view whose body calls a refused one.
    pub fn from_catalog(functions: &[String], views: &[(String, String)]) -> Calls {
        let functions = functions
            .iter()
            .map(|f| f.to_ascii_lowercase())
            .filter_map(
                |f| match TABLE_FUNCTIONS.iter().find(|(name, _)| *name == f) {
                    Some((_, Use::Agent | Use::Lock)) => None,
                    Some((_, Use::Refused(why))) => Some((f, *why)),
                    None => Some((f, UNKNOWN)),
                },
            )
            .collect();
        let mut calls = Calls {
            functions,
            views: Default::default(),
            unread: None,
        };
        let refused: Vec<(String, &'static str)> = views
            .iter()
            .filter_map(|(name, sql)| {
                let found = code(sql).and_then(|c| calls.first(&c.to_ascii_lowercase()));
                match found {
                    Ok(found) => found.map(|(_, why)| (name.to_ascii_lowercase(), why)),
                    // Fails closed: a view whose body the scan could not read through is refused.
                    Err(Stuck) => Some((name.to_ascii_lowercase(), UNREAD_VIEW)),
                }
            })
            .collect();
        calls.views.extend(refused);
        calls
    }

    /// [`Calls::measured`] on the DuckDB it was measured on, and on any other, the refusals its
    /// own catalog gives ([`Calls::from_catalog`]), read by `catalog`. Fails closed: a catalog
    /// that cannot be read refuses every batch.
    pub fn for_version(
        version: &str,
        catalog: impl FnOnce() -> Result<(Vec<String>, Vec<(String, String)>), String>,
    ) -> Calls {
        if version == MEASURED_ON {
            return Calls::measured();
        }
        match catalog() {
            Ok((functions, views)) => Calls::from_catalog(&functions, &views),
            Err(e) => Calls {
                functions: Default::default(),
                views: Default::default(),
                unread: Some(e),
            },
        }
    }

    /// The refusals for the engine `con` belongs to. Read before the agent has made anything,
    /// and only on an engine [`TABLE_FUNCTIONS`] was not measured on: listing
    /// `duckdb_functions()` costs about 0.1 G instructions on DuckDB 1.5.5 (97 M for its
    /// built-ins, measured for #63; `Workspace::attach_all`).
    ///
    /// # Safety
    /// `con` must be a valid connection, not used concurrently.
    pub unsafe fn for_engine(con: ffi::duckdb_connection) -> Calls {
        let version = CStr::from_ptr(ffi::duckdb_library_version())
            .to_string_lossy()
            .into_owned();
        Calls::for_version(&version, || {
            let functions = pairs(con, CATALOG_FUNCTIONS)?
                .into_iter()
                .map(|(f, _)| f)
                .collect();
            Ok((functions, pairs(con, CATALOG_VIEWS)?))
        })
    }

    /// Why the batch `sql` is refused, as the agent is told it, or `None`.
    pub fn check(&self, sql: &str) -> Option<String> {
        if let Some(e) = &self.unread {
            return Some(format!(
                "benchy could not read which table functions this DuckDB has ({e}), so it runs \
                 no query. Nothing ran."
            ));
        }
        let (name, why) = match code(sql).and_then(|c| self.first(&c.to_ascii_lowercase())) {
            Ok(found) => found?,
            Err(Stuck) => {
                return Some(
                    "benchy could not read this batch's text through, so it runs none of it. \
                     Nothing ran."
                        .into(),
                )
            }
        };
        Some(if self.views.contains_key(&name) {
            format!("`{name}` is not available here: it {why}. Nothing ran.")
        } else {
            format!(
                "`{name}(…)` is not available here: it {why}. Nothing ran. The table functions \
                 here are `range`, `generate_series`, `unnest`, `json_each`, `summary`, \
                 `query_table` and the catalog's, such as `duckdb_tables()` and \
                 `duckdb_columns()`; the data is in tables already, which `describe` names."
            )
        })
    }

    /// The first word of the folded text `text` that names a refused view, or calls a refused
    /// function, and why it is refused; [`Stuck`] if the scan stops moving on ([`forward`]).
    fn first(&self, text: &str) -> Result<Option<(String, &'static str)>, Stuck> {
        let b = text.as_bytes();
        let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
        let mut at = 0;
        while at < b.len() {
            // A word, or one byte that is in none. Told apart by `b[at]`, which a scan run past
            // the end cannot read, and not by the word's length: past the end, `b[at..]` is an
            // empty slice, and the scan would go on as if nothing were wrong.
            let start = at;
            let in_word = word(&b[at]);
            at += if in_word {
                b[at..].iter().take_while(|c| word(c)).count()
            } else {
                1
            };
            forward(start, at)?;
            if !in_word {
                continue;
            }
            // Both ends are ASCII bytes, so both are character boundaries.
            let w = &text[start..at];
            if let Some(why) = self.views.get(w) {
                return Ok(Some((w.to_string(), why)));
            }
            if let Some(why) = self.functions.get(w) {
                if calls(&b[at..]) {
                    return Ok(Some((w.to_string(), why)));
                }
            }
        }
        Ok(None)
    }
}

/// What a stretch of a batch's text is, as DuckDB 1.5.5's lexer reads it ([`lex`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    /// Anything else: keywords, names, numbers, operators, white space, a `;`.
    Code,
    /// A quoted name, `"…"`, its quotes with it.
    Name,
    /// A string literal, its quotes and any `E` prefix or dollar tags with it.
    Literal,
    /// A comment: `--` to a line's end, the line break not in it, or `/* … */`, nested.
    Comment,
}

/// `sql` cut into the stretches DuckDB 1.5.5's lexer reads apart
/// (`the_literals_the_check_skips_are_duckdbs`, `a_batch_splits_where_duckdb_splits_it`): a
/// string literal — `'…'`, where `''` is a quote and a backslash is itself; `E'…'`, where a
/// backslash also escapes the character after it; and `$tag$…$tag$`, the tag empty or a name,
/// its case kept — a quoted name (`"…"`, `""` a quote), a comment (`--` to a line's end,
/// `/* … */` nested), and the code around them. Each is found only where DuckDB would start one:
/// not inside another, and not within a word, which DuckDB continues with `$` (`x$a$` is a name)
/// and in which a final `E` is no prefix (`xE'…'` is a plain string).
///
/// Wherever this could part from DuckDB it errs towards reading more as code: a word starting
/// with a digit is read through any `$`, as a name is, so a dollar quote after a number is code;
/// and a literal, a quoted name or a `/*` comment that does not end leaves everything from there
/// on as code, though DuckDB will not parse the batch at all. Every stretch starts and ends at an
/// ASCII byte or at the end, so each is a `&sql[range]`; together they are `sql`, in order, and
/// no two stretches of code are next to each other. [`Stuck`] if a step does not move on
/// ([`forward`]), which no input makes it do.
pub(crate) fn lex(sql: &str) -> Result<Vec<(Part, std::ops::Range<usize>)>, Stuck> {
    let b = sql.as_bytes();
    let mut out: Vec<(Part, std::ops::Range<usize>)> = Vec::new();
    let mut push = |part: Part, r: std::ops::Range<usize>| {
        if let (Part::Code, Some((Part::Code, last))) = (part, out.last_mut()) {
            last.end = r.end;
        } else {
            out.push((part, r));
        }
    };
    // What continues a word in DuckDB's lexer.
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let tag = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let at = |i: usize, s: &[u8]| b[i..].starts_with(s);
    let mut i = 0;
    // Where the step before this one started.
    let mut last = None;
    while i < b.len() {
        if let Some(was) = last {
            forward(was, i)?;
        }
        last = Some(i);
        let start = i;
        // What starts at `i`, and where it ends; `None` for one that does not end.
        let found: Option<(Part, usize)> = match b[i] {
            b'-' if at(i, b"--") => {
                let n = b[i..].iter().position(|c| matches!(c, b'\n' | b'\r'));
                Some((Part::Comment, n.map_or(b.len(), |n| i + n)))
            }
            b'/' if at(i, b"/*") => {
                let mut depth = 0usize;
                let mut j = i;
                loop {
                    if j >= b.len() {
                        break None;
                    }
                    let was = j;
                    if at(j, b"/*") {
                        depth += 1;
                        j += 2;
                    } else if at(j, b"*/") {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break Some((Part::Comment, j));
                        }
                    } else {
                        j += 1;
                    }
                    forward(was, j)?;
                }
            }
            // To the next `"`: a `""` inside closes one quoted name and opens the next, which
            // reads the same as one name holding a quote.
            b'"' => b[i + 1..]
                .iter()
                .position(|c| *c == b'"')
                .map(|n| (Part::Name, i + n + 2)),
            b'\'' => quoted(b, i + 1, false)?.map(|e| (Part::Literal, e + 1)),
            b'e' | b'E' if b.get(i + 1) == Some(&b'\'') => {
                quoted(b, i + 2, true)?.map(|e| (Part::Literal, e + 1))
            }
            b'$' => {
                let mut j = i + 1;
                if b.get(j).is_some_and(|c| tag(*c) && !c.is_ascii_digit()) {
                    j += b[j..].iter().take_while(|c| tag(**c)).count();
                }
                if b.get(j) != Some(&b'$') {
                    push(Part::Code, i..i + 1);
                    i += 1;
                    continue;
                }
                let delim = &b[i..=j];
                let body = j + 1;
                b[body..]
                    .windows(delim.len())
                    .position(|w| w == delim)
                    .map(|n| (Part::Literal, body + n + delim.len()))
            }
            c if word(c) => {
                i += b[i..].iter().take_while(|c| word(**c)).count();
                push(Part::Code, start..i);
                continue;
            }
            _ => {
                push(Part::Code, i..i + 1);
                i += 1;
                continue;
            }
        };
        let end = found.map_or(b.len(), |(_, end)| end);
        push(found.map_or(Part::Code, |(part, _)| part), start..end);
        i = end;
    }
    Ok(out)
}

/// `sql` with the text of each string literal blanked ([`lex`]), for a check that reads names
/// and calls: a comment or a quoted name is left to be read. A literal is blanked to spaces, its
/// quotes and prefix with it, so a name never runs into its neighbour.
fn code(sql: &str) -> Result<String, Stuck> {
    let mut out = sql.as_bytes().to_vec();
    for (part, r) in lex(sql)? {
        if part == Part::Literal {
            out[r].fill(b' ');
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Where the `'…'` literal whose text starts at `from` closes: `''` is a quote, and with
/// `escapes`, as in `E'…'`, a backslash takes the byte after it. `None` if it does not close.
fn quoted(b: &[u8], from: usize, escapes: bool) -> Result<Option<usize>, Stuck> {
    let mut i = from;
    loop {
        let was = i;
        match b.get(i) {
            None => return Ok(None),
            Some(b'\\') if escapes => i += 2,
            Some(b'\'') if b.get(i + 1) == Some(&b'\'') => i += 2,
            Some(b'\'') => return Ok(Some(i)),
            Some(_) => i += 1,
        }
        forward(was, i)?;
    }
}

/// A scan of a batch's text ([`lex`], [`Calls::first`]) that stopped moving on. No input makes
/// it: each step of the scan moves past at least one byte. The check is for a scan that would
/// otherwise loop for ever, as a slip in its arithmetic makes it: each such slip cargo-mutants
/// made hung the suite until its timeout, 22 of them on 0.3.0 (#64). Stopped, the scan fails
/// closed: the batch is refused, as one that does not parse is, and a hint reads nothing in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stuck;

/// [`Stuck`] unless a step of a scan that started at `was` ended past it, at `now`.
fn forward(was: usize, now: usize) -> Result<(), Stuck> {
    if now > was {
        Ok(())
    } else {
        Err(Stuck)
    }
}

/// Whether what follows a name, `rest`, may make it a call: past a `"` that closes it and
/// white space, anything but what certainly ends a name that is not called. A comment, a byte
/// DuckDB may read as white space and this does not, or any other byte this does not know
/// counts as a call; so does a `(`.
fn calls(rest: &[u8]) -> bool {
    let rest = rest.strip_prefix(b"\"").unwrap_or(rest);
    match rest
        .iter()
        .find(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
    {
        None => false,
        Some(c) => !(c.is_ascii_alphanumeric() || b"_,;).=<>'+*|:".contains(c)),
    }
}

/// Two text columns of every row `sql` gives on `con`, as [`Calls::for_engine`] reads them.
unsafe fn pairs(con: ffi::duckdb_connection, sql: &CStr) -> Result<Vec<(String, String)>, String> {
    let mut r: ffi::duckdb_result = std::mem::zeroed();
    if ffi::duckdb_query(con, sql.as_ptr(), &mut r) != ffi::duckdb_state_DuckDBSuccess {
        let e = ffi::duckdb_result_error(&mut r);
        let msg = if e.is_null() {
            "the catalog could not be read".to_string()
        } else {
            CStr::from_ptr(e).to_string_lossy().into_owned()
        };
        ffi::duckdb_destroy_result(&mut r);
        return Err(msg);
    }
    let text = |r: &mut ffi::duckdb_result, col: u64, row: u64| -> String {
        if ffi::duckdb_value_is_null(r, col, row) {
            return String::new();
        }
        let v = ffi::duckdb_value_varchar(r, col, row);
        let s = CStr::from_ptr(v).to_string_lossy().into_owned();
        ffi::duckdb_free(v as *mut std::ffi::c_void);
        s
    };
    let rows = (0..ffi::duckdb_row_count(&mut r))
        .map(|row| (text(&mut r, 0, row), text(&mut r, 1, row)))
        .collect();
    ffi::duckdb_destroy_result(&mut r);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_function_is_refused_however_it_is_written() {
        let calls = Calls::measured();
        for sql in [
            "SELECT * FROM checkpoint()",
            "select * from CHECKPOINT()",
            "SELECT * FROM \"Checkpoint\"()",
            "SELECT * FROM \"checkpoint\" ()",
            "SELECT * FROM system.main.checkpoint /* c */ ()",
            "SELECT * FROM checkpoint\n-- c\n()",
            "SELECT * FROM checkpoint\u{0b}()",
            "SELECT * FROM checkpoint\u{a0}()",
            "SELECT * FROM checkpoint\t\r\n\u{0c}()",
            "SELECT 1; SELECT * FROM checkpoint()",
            "CREATE TABLE t AS SELECT * FROM checkpoint()",
            "INSERT INTO t SELECT * FROM checkpoint()",
            "SELECT (SELECT count(*) FROM checkpoint())",
            "CREATE MACRO m() AS TABLE SELECT * FROM checkpoint()",
            "SELECT * FROM query('SELECT 1')",
            "PRAGMA json_execute_serialized_sql('{}')",
            "SELECT count(*) FROM duckdb_logs",
            "SELECT count(*) FROM system.main.\"DUCKDB_LOGS\"",
            "SELECT * FROM x$checkpoint()",
            "SELECT * FROM écheckpoint()",
            "SELECT * FROM checkpoint[1]",
        ] {
            let e = calls.check(sql);
            assert!(
                e.as_deref().is_some_and(
                    |e| e.contains("is not available here") && e.contains("Nothing ran")
                ),
                "{sql:?}: {e:?}"
            );
        }
    }

    #[test]
    fn what_ends_a_name_that_is_not_called() {
        let calls = Calls::measured();
        for sql in [
            "SELECT checkpoint FROM t",
            "SELECT t.checkpoint, 1 FROM t",
            "SELECT 1 AS checkpoint",
            "SELECT checkpoint.x FROM t AS checkpoint",
            "SELECT 1 WHERE checkpoint=1 OR checkpoint<2 OR checkpoint>0",
            "SELECT checkpoint+1, checkpoint*2, checkpoint||'a', checkpoint::INT FROM t",
            "SELECT \"checkpoint\" FROM t;",
            "SELECT my_checkpoint(), checkpoints(), duckdb_logs_x FROM t",
            "SELECT * FROM range(3), generate_series(1, 2), unnest([1]), duckdb_tables()",
            "SELECT * FROM query_table('t'), json_each('{}'), summary((SELECT 1))",
            "SELECT * FROM read_csv('x.csv'), glob('/*')",
            "SELECT repeat('x', 3), range(3)",
        ] {
            assert_eq!(calls.check(sql), None, "{sql}");
        }
    }

    #[test]
    fn an_engine_it_was_not_measured_on_reads_its_own_catalog() {
        let measured = Calls::measured();
        let same = Calls::for_version(MEASURED_ON, || panic!("the catalog is not read"));
        assert_eq!(same.functions, measured.functions);
        assert_eq!(same.views, measured.views);
        // A function benchy does not know is refused; one it knows keeps its class; a view
        // over a refused function is refused.
        let other = Calls::for_version("v9.9.9", || {
            Ok((
                vec![
                    "brand_new".into(),
                    "Range".into(),
                    "read_csv".into(),
                    "checkpoint".into(),
                ],
                vec![
                    ("fresh_view".into(), "SELECT * FROM brand_new()".into()),
                    ("tables".into(), "SELECT * FROM range(1)".into()),
                ],
            ))
        });
        let e = other.check("SELECT * FROM brand_new()").expect("refused");
        assert!(e.contains("not classified"), "{e}");
        assert!(other.check("SELECT * FROM checkpoint()").is_some());
        assert!(other.check("SELECT * FROM fresh_view").is_some());
        assert_eq!(other.check("SELECT * FROM range(1), tables"), None);
        assert_eq!(other.check("SELECT * FROM read_csv('x')"), None);
        assert_eq!(
            other.check("SELECT * FROM enable_logging()"),
            None,
            "not in it"
        );
        // Fails closed.
        let unread = Calls::for_version("v9.9.9", || Err("no".into()));
        let e = unread.check("SELECT 1").expect("refused");
        assert!(e.contains("(no)") && e.contains("Nothing ran"), "{e}");
    }

    #[test]
    fn a_refused_name_in_a_string_literal_is_only_a_string() {
        // #35: a literal's text is no call. Each form as DuckDB 1.5.5 lexes it, measured
        // (`the_literals_the_check_skips_are_duckdbs`).
        let calls = Calls::measured();
        for sql in [
            "SELECT 'checkpoint()' AS v",
            "SELECT 'it''s checkpoint()' AS v",
            "SELECT 'duckdb_logs' AS v",
            "SELECT x'41', 'a\\', 'checkpoint()' AS v",
            "SELECT E'it\\'s checkpoint()' AS v",
            "SELECT e'a''checkpoint() \\\\' AS v",
            "SELECT E'\\\\\\' checkpoint()' AS v",
            "SELECT $$checkpoint()$$ AS v",
            "SELECT $$it's checkpoint()$$ AS v",
            "SELECT $t$a$$checkpoint()$t$ AS v",
            "SELECT $A$x$a$checkpoint()$A$ AS v",
            "SELECT $é$checkpoint()$é$ AS v",
            "SELECT $_1$checkpoint()$_1$ AS v",
            "SELECT \"x\", $$checkpoint()$$, 'query(1)' AS v",
            "SELECT 'a' AS \"it's\", 'checkpoint()' AS v",
            "SELECT 1 -- it's\n, 'checkpoint()' AS v",
            "SELECT 1 /* it's */, 'checkpoint()' AS v",
            // a lone `-` or `/` starts no comment
            "SELECT 'checkpoint()''' AS v",
            "SELECT 1-1, 'checkpoint()' AS v",
            "SELECT 4/2, 'checkpoint()' AS v",
            // a dollar quote's text may start with what ends its tag
            "SELECT $a$a$ checkpoint() $a$ AS v",
            // in `E'…'`, a backslash after a `''` still escapes
            "SELECT E'a''b\\'checkpoint()' AS v",
        ] {
            assert_eq!(calls.check(sql), None, "{sql}");
        }
    }

    #[test]
    fn a_refused_call_beside_a_literal_is_refused() {
        // Where the text after a literal, or what only looks like one, is code, it is read; and
        // what does not end is read to the end.
        let calls = Calls::measured();
        for sql in [
            // right after each form
            "SELECT 'a'||checkpoint()",
            "SELECT 'it''s', checkpoint()",
            "SELECT E'a\\'b', checkpoint()",
            "SELECT E'a\\\\', checkpoint()",
            "SELECT E'\\'', checkpoint(), 'x'",
            // a refused call whose argument is a literal
            "SELECT * FROM checkpoint('x')",
            "SELECT * FROM checkpoint(E'x')",
            "SELECT * FROM checkpoint($$x$$)",
            "SELECT $$a$$, checkpoint()",
            "SELECT $t$a$t$, checkpoint()",
            // a backslash ends no plain literal, nor one whose `E` ends a word
            "SELECT 'a\\', checkpoint(), 'b'",
            "SELECT xE'a\\', checkpoint(), 'b'",
            "SELECT 1E'a\\', checkpoint(), 'b'",
            "SELECT éE'a\\', checkpoint(), 'b'",
            // `$` inside a word, or after a number, starts no dollar quote
            "SELECT 1 AS x$a$, checkpoint() AS y, 1 AS z$a$",
            "SELECT 1 AS é$a$, checkpoint() AS y, 1 AS z$a$",
            "SELECT 1$a$, checkpoint(), $a$",
            "SELECT $1, checkpoint()",
            "SELECT $1a$, checkpoint(), $1a$",
            // a dollar tag keeps its case
            "SELECT $A$a$a$, checkpoint()",
            // a quote inside a comment or a quoted name opens no literal
            "SELECT 1 -- it's\n, checkpoint() --'",
            "SELECT 1 -- it's\r, checkpoint() --'",
            "SELECT 1 /* it's */, checkpoint() /* ' */",
            "SELECT 1 /* /* */ ' */, checkpoint() /* ' */",
            "SELECT \"a'b\", checkpoint(), 'c'",
            "SELECT \"a\"\"'\", checkpoint(), 'c'",
            "SELECT \"$a$\", checkpoint(), '$a$'",
            // a name in a comment or a quoted name is still read
            "SELECT 1 /* checkpoint() */",
            "SELECT 1 -- checkpoint()",
            "SELECT 1 -- it's checkpoint() '",
            // a word that only starts with `e` starts no `E'…'`
            "SELECT 1 AS ex, checkpoint(), 'a'",
            "SELECT \"checkpoint\"()",
            // what does not end is read to the end
            "SELECT 'checkpoint()",
            "SELECT E'\\'checkpoint()",
            "SELECT $$checkpoint()",
            "SELECT $a$checkpoint()$A$",
            "SELECT \"x, checkpoint()",
            "SELECT 1 /* /* */ checkpoint()",
        ] {
            assert!(calls.check(sql).is_some(), "{sql:?}");
        }
    }

    #[test]
    fn a_scan_that_does_not_move_on_stops() {
        // #64: a step that ends where it started, or before, would be taken again for ever.
        assert_eq!(forward(3, 3), Err(Stuck), "where it started");
        assert_eq!(forward(3, 2), Err(Stuck), "before it");
        assert_eq!(forward(0, 0), Err(Stuck), "at the start of the text");
        assert_eq!(forward(3, 4), Ok(()), "one byte on");
        assert_eq!(forward(3, 9), Ok(()), "a word on");
        // Every form the scan reads, each step moving on.
        for sql in [
            "SELECT * FROM checkpoint()",
            "SELECT 'a''b', E'c\\'d', $$e$$, $t$f$t$, \"g\"\"h\" -- i\n/* j /* k */ */ $1",
            "'",
            "E'",
            "$",
            "/*",
            "",
        ] {
            assert!(lex(sql).is_ok(), "{sql:?}");
        }
    }

    #[test]
    fn the_stretches_are_the_whole_text_in_order() {
        // What `lex` promises its callers: the stretches together are the text, in order, and
        // no two of code are next to each other.
        for sql in [
            "SELECT $",
            "SELECT $1, $a$",
            "x$a$ 'b' E'c' \"d\" $$e$$ -- f\n/* g */ h",
            "SELECT 'unterminated",
            "/* unterminated",
            "$",
            "-",
        ] {
            let parts = lex(sql).unwrap();
            let mut at = 0;
            for (k, (part, r)) in parts.iter().enumerate() {
                assert_eq!(r.start, at, "{sql:?}: {parts:?}");
                assert!(r.end > r.start, "{sql:?}: {parts:?}");
                if k > 0 {
                    assert!(
                        !(*part == Part::Code && parts[k - 1].0 == Part::Code),
                        "{sql:?}: {parts:?}"
                    );
                }
                at = r.end;
            }
            assert_eq!(at, sql.len(), "{sql:?}: {parts:?}");
        }
    }

    #[test]
    fn every_statement_type_has_a_name_of_its_own() {
        // The name is what a refusal says the agent ran (`each_statement_kind_is_named_as_duckdb_\
        // types_it` holds which name DuckDB gives what). Every kind the C API has is named, and
        // no two alike: `MERGE` and `UPDATE EXTENSIONS` were once refused as "UNKNOWN", and an
        // agent told it ran what it did not cannot fix its batch.
        let last = ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_MERGE_INTO;
        let mut seen = std::collections::HashSet::new();
        for t in 1..=last {
            let name = type_name(t);
            assert_ne!(name, "UNKNOWN", "statement type {t}");
            assert!(seen.insert(name), "{name} names two statement types");
        }
        assert_eq!(
            type_name(ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_INVALID),
            "UNKNOWN"
        );
    }

    #[test]
    fn the_refusal_says_why_and_what_to_use() {
        let calls = Calls::measured();
        let e = calls.check("SELECT * FROM enable_logging()").unwrap();
        assert!(
            e.starts_with("`enable_logging(…)` is not available here"),
            "{e}"
        );
        assert!(
            e.contains(STATE) && e.contains("`range`") && e.contains("describe"),
            "{e}"
        );
        let e = calls.check("SELECT * FROM duckdb_logs").unwrap();
        assert_eq!(
            e,
            format!("`duckdb_logs` is not available here: it {INSIDE}. Nothing ran.")
        );
    }
}
