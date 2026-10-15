// Does the statement allowlist alone stop the escape corpus? Does the locked engine alone?
// REQ-2 `the_corpus_records_why_the_engine_is_the_boundary` repeats this on every run.
//
// "Stopped" is measured by whether a canary written outside the workspace reaches the result,
// not merely by whether an error was raised.
#![deny(unreachable_patterns)]
use duckdb::ffi::*;
use std::ffi::{CStr, CString};
use std::ptr;

const CANARY: &str = "CANARY-8f31d0a7c4e2";

// REQ-2 layer 1. Everything else is refused before it runs.
fn allowed(t: duckdb_statement_type) -> bool {
    matches!(t,
        duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_INSERT
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_UPDATE
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DELETE
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_DROP
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ALTER
      | duckdb_statement_type_DUCKDB_STATEMENT_TYPE_EXPLAIN)
}

struct Db { db: duckdb_database, con: duckdb_connection }

impl Db {
    unsafe fn open(locked: bool) -> Db {
        let (mut db, mut con) = (ptr::null_mut(), ptr::null_mut());
        let path = CString::new(":memory:").unwrap();
        assert_eq!(duckdb_open(path.as_ptr(), &mut db), duckdb_state_DuckDBSuccess);
        assert_eq!(duckdb_connect(db, &mut con), duckdb_state_DuckDBSuccess);
        let me = Db { db, con };
        me.run("CREATE TABLE loaded(a INTEGER)").ok();
        if locked {
            // order matters: temp_directory cannot be set once external access is off
            for s in ["SET temp_directory = ''",
                      "SET home_directory = '/benchy-none'",
                      "SET secret_directory = '/benchy-none'",
                      "SET extension_directory = '/benchy-none'",
                      "SET enable_external_access = false",
                      "SET autoinstall_known_extensions = false",
                      "SET autoload_known_extensions = false",
                      "SET lock_configuration = true"] {
                if let Err(e) = me.run(s) { eprintln!("lockdown step failed: {s} -> {e}"); }
            }
        }
        me
    }

    // Returns every cell joined, so a canary anywhere in the output is detectable.
    unsafe fn run(&self, sql: &str) -> Result<String, String> {
        let c = CString::new(sql).unwrap();
        let mut r: duckdb_result = std::mem::zeroed();
        if duckdb_query(self.con, c.as_ptr(), &mut r) != duckdb_state_DuckDBSuccess {
            let e = duckdb_result_error(&mut r);
            let msg = if e.is_null() { "error".into() } else { CStr::from_ptr(e).to_string_lossy().into_owned() };
            duckdb_destroy_result(&mut r);
            return Err(msg.lines().next().unwrap_or("").to_string());
        }
        let (rows, cols) = (duckdb_row_count(&mut r), duckdb_column_count(&mut r));
        let mut out = String::new();
        for row in 0..rows.min(50) {
            for col in 0..cols {
                let v = duckdb_value_varchar(&mut r, col, row);
                if !v.is_null() { out.push_str(&CStr::from_ptr(v).to_string_lossy()); out.push(' ');
                                  duckdb_free(v as *mut std::ffi::c_void); }
            }
        }
        duckdb_destroy_result(&mut r);
        Ok(out)
    }

    unsafe fn refused_by_allowlist(&self, sql: &str) -> bool {
        let c = CString::new(sql).unwrap();
        let mut ex: duckdb_extracted_statements = ptr::null_mut();
        let n = duckdb_extract_statements(self.con, c.as_ptr(), &mut ex);
        if n == 0 { duckdb_destroy_extracted(&mut ex); return true; } // unparseable: refused
        let mut refuse = false;
        for i in 0..n {
            let mut p: duckdb_prepared_statement = ptr::null_mut();
            if duckdb_prepare_extracted_statement(self.con, ex, i, &mut p) == duckdb_state_DuckDBSuccess {
                if !allowed(duckdb_prepared_statement_type(p)) { refuse = true; }
            } // a statement that will not bind cannot run either
            duckdb_destroy_prepare(&mut p);
        }
        duckdb_destroy_extracted(&mut ex);
        refuse
    }
}

fn main() {
    let path = std::env::temp_dir().join("benchy-canary.txt");
    std::fs::write(&path, format!("secret,{CANARY}\n")).unwrap();
    let p = path.display().to_string();

    let corpus: Vec<(String, &str)> = vec![
        (format!("ATTACH '{p}' AS evil"), "attach"),
        (format!("ATTACH DATABASE '{p}' AS evil"), "attach"),
        ("ATTACH ':memory:' AS scratch".into(), "attach"),
        (format!("ATTACH '{p}' AS e (TYPE sqlite)"), "attach"),
        (format!("SELECT * FROM read_csv('{p}')"), "file read"),
        (format!("SELECT * FROM read_csv_auto('{p}')"), "file read"),
        (format!("SELECT * FROM read_text('{p}')"), "file read"),
        (format!("SELECT * FROM read_blob('{p}')"), "file read"),
        (format!("SELECT * FROM read_parquet('{p}')"), "file read"),
        (format!("SELECT * FROM read_json_auto('{p}')"), "file read"),
        (format!("SELECT * FROM '{p}'"), "replacement scan"),
        (format!("SELECT * FROM sniff_csv('{p}')"), "file read"),
        (format!("SELECT * FROM glob('{}/*')", std::env::temp_dir().display()), "glob"),
        (format!("COPY loaded TO '{p}.out'"), "copy out"),
        (format!("COPY loaded FROM '{p}'"), "copy in"),
        ("EXPORT DATABASE '/tmp/benchy-dump'".into(), "export"),
        ("INSTALL httpfs".into(), "extension"),
        ("LOAD httpfs".into(), "extension"),
        ("INSTALL sqlite".into(), "extension"),
        ("SET enable_external_access = true".into(), "unlock"),
        ("SET home_directory = '/home'".into(), "unlock"),
        ("SET extension_directory = '/tmp'".into(), "unlock"),
        ("RESET enable_external_access".into(), "unlock"),
        ("PRAGMA database_list".into(), "path disclosure"),
        ("SELECT * FROM duckdb_databases()".into(), "path disclosure"),
        ("SELECT * FROM duckdb_settings() WHERE name LIKE '%directory%'".into(), "path disclosure"),
        ("SELECT * FROM duckdb_extensions()".into(), "recon"),
        ("SELECT current_setting('home_directory')".into(), "path disclosure"),
        ("SELECT * FROM pragma_database_list()".into(), "path disclosure"),
        ("SELECT getenv('HOME')".into(), "env read"),
        (format!("SELECT * FROM read_csv('file://{p}')"), "file read"),
        ("SELECT * FROM read_csv('https://example.invalid/x.csv')".into(), "network"),
        ("SELECT * FROM read_parquet('s3://bucket/x.parquet')".into(), "network"),
        ("CALL pragma_version()".into(), "call"),
        ("BEGIN TRANSACTION".into(), "transaction"),
        (format!("SELECT 1; ATTACH '{p}' AS e"), "multi-statement"),
        (format!("CREATE TABLE x AS SELECT * FROM read_csv('{p}')"), "ctas wrapping a reader"),
        (format!("SELECT * FROM (SELECT * FROM read_text('{p}'))"), "nested reader"),
        (format!("WITH c AS (SELECT * FROM read_text('{p}')) SELECT * FROM c"), "cte wrapping a reader"),
        (format!("PREPARE p AS SELECT * FROM read_text('{p}')"), "prepare"),
    ];

    unsafe {
        let plain = Db::open(false);   // no lockdown; allowlist is the only layer
        let locked = Db::open(true);   // locked engine; allowlist NOT applied

        println!("canary written to {p}\n");
        println!("{:<62} {:<22} {:>9} {:>9}", "statement", "category", "allowlist", "engine");
        println!("{}", "-".repeat(106));
        let (mut a_stop, mut e_stop, mut leaks) = (0, 0, 0);

        for (sql, cat) in &corpus {
            // layer 1 alone
            let refused = plain.refused_by_allowlist(sql);
            let a_leak = if refused { false } else {
                plain.run(sql).map(|o| o.contains(CANARY)).unwrap_or(false) };
            let a = if refused { "REFUSE" } else if a_leak { "LEAK" } else { "ran" };

            // layer 2 alone
            let e_res = locked.run(sql);
            let e_leak = e_res.as_ref().map(|o| o.contains(CANARY)).unwrap_or(false);
            let e = if e_res.is_err() { "error" } else if e_leak { "LEAK" } else { "ran" };

            if refused { a_stop += 1; }
            if e_res.is_err() { e_stop += 1; }
            if a_leak || e_leak { leaks += 1; }

            let short: String = sql.chars().take(60).collect();
            println!("{:<62} {:<22} {:>9} {:>9}{}", short, cat, a, e,
                     if a_leak || e_leak { "   <-- CANARY REACHED THE RESULT" } else { "" });
        }

        println!("\n{} attempts", corpus.len());
        println!("  allowlist alone refused : {a_stop}/{}", corpus.len());
        println!("  locked engine alone errored: {e_stop}/{}", corpus.len());
        println!("  attempts where the canary reached a result in EITHER single-layer config: {leaks}");
        println!("\nA 'ran' is not a failure: many of these are harmless reads of internal catalogs.");
        println!("The failure condition is LEAK, and the requirement is that neither layer alone leaks.");
    }
    let _ = std::fs::remove_file(&path);
}
