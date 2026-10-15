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
//! layer stops is that lock being *removed* — `SET`, `RESET`, `INSTALL`, `LOAD` — plus `ATTACH`,
//! `COPY` and `EXPORT`.

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
            for i in 0..n {
                let mut prep: ffi::duckdb_prepared_statement = ptr::null_mut();
                let ok = ffi::duckdb_prepare_extracted_statement(self.con, ex, i, &mut prep)
                    == ffi::duckdb_state_DuckDBSuccess;
                // A statement that will not bind yet is not refused here: it may depend on an
                // object an earlier statement in the batch creates. It is classified again when
                // it is prepared to run, where that object exists.
                let verdict = if ok {
                    let t = ffi::duckdb_prepared_statement_type(prep);
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
                            index: i as usize + 1,
                            kind: k,
                        };
                    }
                }
            }
            ffi::duckdb_destroy_extracted(&mut ex);
            Verdict::Allow(kinds)
        }
    }
}
