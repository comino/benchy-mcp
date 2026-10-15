// REQ-2 hazard: an unprefixed constant is a *binding pattern*, not a constant pattern,
// so it matches everything and silently allows the whole allowlist through.
#![deny(unreachable_patterns)]
// REQ-2: can we get per-statement types for the allowlist, through the C API?
use duckdb::ffi::*;
use std::ffi::{CStr, CString};
use std::ptr;

fn type_name(t: duckdb_statement_type) -> &'static str {
    match t {
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT => "SELECT",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_INSERT => "INSERT",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_UPDATE => "UPDATE",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DELETE => "DELETE",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE => "CREATE",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DROP => "DROP",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ALTER => "ALTER",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPLAIN => "EXPLAIN",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ATTACH => "ATTACH",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DETACH => "DETACH",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_COPY => "COPY",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPORT => "EXPORT",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_LOAD => "LOAD",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_PRAGMA => "PRAGMA",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SET => "SET",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CALL => "CALL",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_TRANSACTION => "TRANSACTION",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_PREPARE => "PREPARE",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXECUTE => "EXECUTE",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_VACUUM => "VACUUM",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ANALYZE => "ANALYZE",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_MULTI => "MULTI",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_RELATION => "RELATION",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_LOGICAL_PLAN => "LOGICAL_PLAN",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE_FUNC => "CREATE_FUNC",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_VARIABLE_SET => "VARIABLE_SET",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXTENSION => "EXTENSION",
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_COPY_DATABASE => "COPY_DATABASE",
        _ => "INVALID/UNKNOWN",
    }
}

unsafe fn types_of(con: duckdb_connection, sql: &str) -> Result<Vec<String>, String> {
    let c = CString::new(sql).unwrap();
    let mut extracted: duckdb_extracted_statements = ptr::null_mut();
    let n = duckdb_extract_statements(con, c.as_ptr(), &mut extracted);
    if n == 0 {
        let e = duckdb_extract_statements_error(extracted);
        let msg = if e.is_null() { "no statements".into() } else { CStr::from_ptr(e).to_string_lossy().into_owned() };
        duckdb_destroy_extracted(&mut extracted);
        return Err(msg);
    }
    let mut out = Vec::new();
    for i in 0..n {
        let mut prep: duckdb_prepared_statement = ptr::null_mut();
        // NOTE: prepare BINDS the statement — it fails on unknown tables, so the guard
        // must run against a workspace where our tables already exist.
        let st = duckdb_prepare_extracted_statement(con, extracted, i, &mut prep);
        if st != duckdb_state_DuckDBSuccess {
            let e = duckdb_prepare_error(prep);
            let msg = if e.is_null() { "prepare failed".to_string() } else { CStr::from_ptr(e).to_string_lossy().into_owned() };
            out.push(format!("<prepare failed: {}>", msg.lines().next().unwrap_or("")));
            duckdb_destroy_prepare(&mut prep);
            continue;
        }
        out.push(type_name(duckdb_prepared_statement_type(prep)).to_string());
        duckdb_destroy_prepare(&mut prep);
    }
    duckdb_destroy_extracted(&mut extracted);
    Ok(out)
}

fn main() {
    unsafe {
        let mut db: duckdb_database = ptr::null_mut();
        let mut con: duckdb_connection = ptr::null_mut();
        let path = CString::new(":memory:").unwrap();
        assert_eq!(duckdb_open(path.as_ptr(), &mut db), duckdb_state_DuckDBSuccess);
        assert_eq!(duckdb_connect(db, &mut con), duckdb_state_DuckDBSuccess);

        let v = duckdb_library_version();
        println!("duckdb {}\n", CStr::from_ptr(v).to_string_lossy());

        // a table so that SELECT/UPDATE/DROP bind
        let setup = CString::new("CREATE TABLE loaded(a INTEGER)").unwrap();
        let mut r: duckdb_result = std::mem::zeroed();
        duckdb_query(con, setup.as_ptr(), &mut r);
        duckdb_destroy_result(&mut r);

        let cases = [
            ("SELECT 1", "allow"),
            ("SELECT a FROM loaded WHERE a > 1", "allow"),
            ("CREATE TEMP TABLE scratch AS SELECT 1 AS x", "allow"),
            ("UPDATE loaded SET a = 2", "allow"),
            ("DROP TABLE loaded", "allow"),
            ("EXPLAIN SELECT 1", "allow"),
            ("ATTACH '/etc/passwd' AS evil", "REFUSE"),
            ("COPY loaded TO '/tmp/x.csv'", "REFUSE"),
            ("EXPORT DATABASE '/tmp/dump'", "REFUSE"),
            ("INSTALL httpfs", "REFUSE"),
            ("LOAD httpfs", "REFUSE"),
            ("SET memory_limit='9GB'", "REFUSE"),
            ("PRAGMA version", "REFUSE?"),
            ("PRAGMA database_list", "REFUSE?"),
            ("CALL pragma_version()", "REFUSE"),
            ("BEGIN TRANSACTION", "REFUSE"),
            ("SELECT 1; DROP TABLE loaded", "multi"),
            ("SELECT 1; ATTACH '/etc/passwd' AS e", "multi w/ refuse"),
            ("PIVOT loaded ON a USING count(*)", "dynamic pivot"),
        ];
        let w = cases.iter().map(|c| c.0.len()).max().unwrap();
        for (sql, expect) in cases {
            match types_of(con, sql) {
                Ok(t) => println!("{:<w$}  ->  {:<28} [{}]", sql, t.join(" , "), expect, w = w),
                Err(e) => println!("{:<w$}  ->  PARSE ERROR: {:<15} [{}]", sql, e.lines().next().unwrap_or(""), expect, w = w),
            }
        }
        duckdb_disconnect(&mut con);
        duckdb_close(&mut db);
    }
}
