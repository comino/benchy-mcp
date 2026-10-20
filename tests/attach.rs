//! Tables that arrive after the seal (docs/HOST-PROTOCOL.md): attached read-only from the one
//! directory DuckDB may still open, which is emptied again by every attach.

use benchy_mcp::result::Cell;
use benchy_mcp::{Caps, QueryResult, Workspace};
use std::path::{Path, PathBuf};

fn unique(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "benchy-attach-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A DuckDB file holding table `name` with `rows` rows, as a loader would write it.
fn host_file(dir: &Path, name: &str, rows: u32, tag: &str) -> PathBuf {
    let f = dir.join(format!("{name}-{tag}.duckdb"));
    let c = duckdb::Connection::open(&f).unwrap();
    c.execute_batch(&format!(
        "CREATE TABLE \"{name}\" AS SELECT i AS id, '{tag}-' || i AS v FROM range({rows}) t(i)"
    ))
    .unwrap();
    drop(c);
    f
}

fn text(results: &[QueryResult]) -> String {
    let mut s = String::new();
    for r in results {
        for row in &r.rows {
            for c in row {
                if let Cell::Text(t) = c {
                    s.push_str(t);
                    s.push(' ');
                }
            }
        }
    }
    s
}

fn count(ws: &mut Workspace, table: &str) -> String {
    let out = ws
        .work(&format!("SELECT count(*) FROM {table}"), &Caps::default())
        .unwrap_or_else(|e| panic!("count {table}: {e}"));
    text(&out).trim().to_string()
}

#[test]
fn an_attached_table_joins_a_running_session() {
    let dir = unique("join");
    let mut ws = Workspace::open_sealed(&dir, "1GB").expect("opens sealed");
    ws.work("CREATE TABLE mine AS SELECT 42 AS answer", &Caps::default())
        .expect("the agent works before anything is attached");
    let t = ws
        .attach("readings", &host_file(&dir, "readings", 3, "a"))
        .expect("attaches");
    assert_eq!(t.rows, 3);
    assert_eq!(count(&mut ws, "readings"), "3");
    assert_eq!(
        count(&mut ws, "mine"),
        "1",
        "the agent's table is still there"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replacing_an_attachment_keeps_the_agents_tables() {
    let dir = unique("replace");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    ws.attach("readings", &host_file(&dir, "readings", 3, "old"))
        .unwrap();
    ws.work(
        "CREATE TABLE mine AS SELECT * FROM readings",
        &Caps::default(),
    )
    .unwrap();
    ws.attach("readings", &host_file(&dir, "readings", 5, "new"))
        .expect("replaces");
    assert_eq!(count(&mut ws, "readings"), "5", "the new data");
    assert_eq!(count(&mut ws, "mine"), "3", "the agent's copy stays");
    let out = ws
        .work(
            "SELECT count(*) FROM duckdb_databases() WHERE database_name LIKE 'benchy\\_%' ESCAPE '\\'",
            &Caps::default(),
        )
        .unwrap();
    assert_eq!(text(&out).trim(), "1", "the old generation is detached");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_agent_cannot_change_a_hosts_table() {
    let dir = unique("readonly");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    ws.attach("readings", &host_file(&dir, "readings", 3, "a"))
        .unwrap();
    for sql in [
        "INSERT INTO readings VALUES (9, 'x')",
        "DELETE FROM readings",
        "UPDATE readings SET v = 'x'",
        "DROP TABLE readings",
        "ALTER TABLE readings ADD COLUMN w INTEGER",
        "MERGE INTO readings USING (SELECT 9 AS id) s ON readings.id = s.id \
         WHEN NOT MATCHED THEN INSERT VALUES (s.id, 'x')",
        "DETACH benchy_1",
        "ATTACH ':memory:' AS benchy_1",
        "USE benchy_1",
        "CREATE TABLE benchy_1.main.planted (x INTEGER)",
    ] {
        assert!(
            ws.work(sql, &Caps::default()).is_err(),
            "the agent ran: {sql}"
        );
    }
    assert_eq!(count(&mut ws, "readings"), "3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nothing_is_left_to_read_in_the_attach_directory() {
    // The directory an engine attaches from is allowed to DuckDB, and an allowed directory is
    // open to every file reader, not only ATTACH. So nothing may stay in it.
    let dir = unique("empty");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    let file = host_file(&dir, "readings", 3, "a");
    ws.attach("readings", &file).unwrap();
    assert!(!file.exists(), "the attached file is unlinked");
    let d = dir.display();
    let f = file.display();
    for sql in [
        format!("SELECT count(*) FROM glob('{d}/*')"),
        format!("SELECT count(*) FROM read_blob('{d}/*')"),
        format!("SELECT count(*) FROM read_text('{d}/*')"),
    ] {
        let got = ws
            .work(&sql, &Caps::default())
            .map(|r| text(&r).trim().to_string())
            .unwrap_or_else(|_| "0".into());
        assert_eq!(got, "0", "{sql} found something");
    }
    for sql in [
        format!("SELECT * FROM read_duckdb('{f}')"),
        format!("SELECT * FROM '{f}'"),
    ] {
        assert!(
            ws.work(&sql, &Caps::default()).is_err(),
            "{sql} read the attached file"
        );
    }
    assert_eq!(count(&mut ws, "readings"), "3", "while the table serves");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_attach_changes_nothing() {
    let dir = unique("failed");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    ws.attach("readings", &host_file(&dir, "readings", 3, "a"))
        .unwrap();
    // A file without the table it is attached as, and a file that is not DuckDB's at all.
    let wrong = host_file(&dir, "other", 5, "b");
    assert!(ws.attach("readings", &wrong).is_err());
    let junk = dir.join("junk.duckdb");
    std::fs::write(&junk, "not a database").unwrap();
    assert!(ws.attach("readings", &junk).is_err());
    assert_eq!(count(&mut ws, "readings"), "3", "the bench is as it was");
    assert!(
        !wrong.exists() && !junk.exists(),
        "refused files are not left behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attached_file_brings_its_table_and_nothing_else() {
    // An attached catalog goes on the agent's search path, and DuckDB resolves a function there
    // before its own: a macro named `lower` in the file stands in for `lower`, measured. A loader
    // writes one table and nothing else, so a file holding anything more is not what a loader
    // wrote, and is refused before the agent can see it.
    let dir = unique("extras");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    for extra in [
        "CREATE MACRO lower(x) AS 'shadowed'",
        "CREATE MACRO rows_of() AS TABLE SELECT 1 AS x",
        "CREATE VIEW v AS SELECT 1 AS x",
        "CREATE TABLE other (x INTEGER)",
        "CREATE SEQUENCE s",
        "CREATE TYPE mood AS ENUM ('ok')",
        "CREATE SCHEMA elsewhere",
        "CREATE INDEX i ON readings (id)",
    ] {
        let f = host_file(&dir, "readings", 3, "x");
        let c = duckdb::Connection::open(&f).unwrap();
        c.execute_batch(extra).unwrap();
        drop(c);
        assert!(ws.attach("readings", &f).is_err(), "attached with: {extra}");
        assert!(!f.exists(), "a refused file is not left behind");
        let out = ws.work("SELECT lower('X') AS l", &Caps::default()).unwrap();
        assert_eq!(text(&out).trim(), "x", "after: {extra}");
    }
    assert!(
        ws.work("SELECT * FROM readings", &Caps::default()).is_err(),
        "nothing was attached"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn anything_else_in_one_file_of_a_batch_refuses_that_file_alone() {
    // The rest of the check is asked once a batch too: one listing of each kind — tables, views,
    // sequences, indexes, types, schemas — and one of columns, for every catalog of the batch
    // (docs/PERFORMANCE.md R2-5). Each catalog is still judged on its own: the file that holds
    // anything besides its table, or not its table under its name, is refused, and the files
    // around it are attached with their own rows and columns.
    let dir = unique("batch-extras");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    for (n, extra) in [
        "CREATE VIEW v AS SELECT 1 AS x",
        "CREATE TABLE other (x INTEGER)",
        "CREATE SEQUENCE s",
        "CREATE TYPE mood AS ENUM ('ok')",
        "CREATE SCHEMA elsewhere",
        "CREATE INDEX i ON middle (id)",
        "ALTER TABLE middle RENAME TO renamed",
    ]
    .into_iter()
    .enumerate()
    {
        let middle = host_file(&dir, "middle", 2, "m");
        let c = duckdb::Connection::open(&middle).unwrap();
        c.execute_batch(extra).unwrap();
        drop(c);
        let (first, last) = (format!("first{n}"), format!("last{n}"));
        let tables = vec![
            (first.clone(), host_file(&dir, &first, 3, "a")),
            ("middle".to_string(), middle),
            (last.clone(), host_file(&dir, &last, 4, "b")),
        ];
        let answers = ws.attach_all(&tables);
        assert!(answers[1].is_err(), "attached with: {extra}");
        let first_table = answers[0].as_ref().expect("the first file of the batch");
        let last_table = answers[2].as_ref().expect("the last file of the batch");
        assert_eq!((first_table.rows, last_table.rows), (3, 4), "{extra}");
        assert_eq!(
            first_table.columns,
            [
                ("id".to_string(), "BIGINT".to_string()),
                ("v".to_string(), "VARCHAR".to_string())
            ],
            "{extra}"
        );
        assert_eq!(count(&mut ws, &first), "3", "{extra}");
        assert_eq!(count(&mut ws, &last), "4", "{extra}");
        assert!(
            ws.work("SELECT * FROM middle", &Caps::default()).is_err(),
            "after: {extra}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_function_in_one_file_of_a_batch_refuses_that_file_alone() {
    // Several tables are attached in one request, and the check for functions is asked once for
    // all of them (docs/history/PERFORMANCE-rounds.md, round 1 item 5). It must stay as strict as
    // one table at a time: the file that brings a macro is refused before any catalog of the batch
    // reaches the search path, and the files around it are attached.
    let dir = unique("batch");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    let shadowing = host_file(&dir, "middle", 2, "m");
    let c = duckdb::Connection::open(&shadowing).unwrap();
    c.execute_batch("CREATE MACRO lower(x) AS 'shadowed'")
        .unwrap();
    drop(c);
    let tables = vec![
        ("first".to_string(), host_file(&dir, "first", 3, "a")),
        ("middle".to_string(), shadowing.clone()),
        ("last".to_string(), host_file(&dir, "last", 4, "b")),
    ];
    let answers = ws.attach_all(&tables);
    assert_eq!(answers.len(), 3);
    assert!(answers[0].is_ok(), "{:?}", answers[0].as_ref().err());
    let refused = answers[1]
        .as_ref()
        .expect_err("the file with a macro was attached");
    assert!(
        refused.to_string().contains("more than that one table"),
        "{refused}"
    );
    assert!(answers[2].is_ok(), "{:?}", answers[2].as_ref().err());
    for (_, f) in &tables {
        assert!(!f.exists(), "{f:?} was left behind");
    }
    let out = ws.work("SELECT lower('X') AS l", &Caps::default()).unwrap();
    assert_eq!(text(&out).trim(), "x", "the macro reached the search path");
    assert_eq!(count(&mut ws, "first"), "3");
    assert_eq!(count(&mut ws, "last"), "4");
    assert!(
        ws.work("SELECT * FROM middle", &Caps::default()).is_err(),
        "the refused table is on the bench"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn replacing_an_attachment_drops_the_results_that_could_hold_its_rows() {
    // A host that narrows what a session may see does it by replacing the table. A result the
    // agent read before still held the rows that were taken away, and could be paged again.
    let dir = unique("replace-results");
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    ws.attach("readings", &host_file(&dir, "readings", 3, "old"))
        .unwrap();
    let before = ws.work("SELECT * FROM readings", &Caps::default()).unwrap();
    let id = before[0].result_id.clone();
    let caps = benchy_mcp::result::Caps::default();
    assert!(ws.page(&id, 0, &caps).is_some(), "retained");
    ws.attach("readings", &host_file(&dir, "readings", 1, "new"))
        .unwrap();
    assert!(
        ws.page(&id, 0, &caps).is_none(),
        "the old rows can still be read by id"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
