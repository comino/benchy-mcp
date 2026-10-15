//! REQ-9 `an_agent_object_cannot_shadow_a_host_object_or_builtin`: the agent creates what it
//! likes, under any name that is not already taken.
//!
//! DuckDB resolves the connection's temporary catalog before any search path, and the agent's own
//! schema before the host's catalogs, so an agent object named like a host table or a built-in
//! function stands in for it, measured on DuckDB 1.5.5: `CREATE TEMP TABLE readings` hid the
//! attached `readings`, and `CREATE TEMP MACRO lower` replaced `lower`. Such an object is refused
//! after the statement runs, and undone with the statement's transaction.

mod common;

use benchy_mcp::result::Cell;
use benchy_mcp::{source, Caps, QueryResult, WorkError, Workspace};
use std::path::{Path, PathBuf};

fn unique(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "benchy-shadow-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A sealed bench with `meters` loaded: two rows, 6 kW in all.
fn bench() -> Workspace {
    let d = unique("bench");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("meters.csv"),
        "observed_at,meter,power_kw\n2026-01-01T00:00:00Z,lab,2.0\n2026-01-01T00:15:00Z,lab,4.0\n",
    )
    .unwrap();
    let mut b = common::bench();
    source::load_dir(&mut b, &d).expect("loads");
    let _ = std::fs::remove_dir_all(&d);
    b.seal("1GB").expect("seals")
}

/// A DuckDB file holding table `name`, as a loader writes it.
fn host_file(dir: &Path, name: &str, rows: u32) -> PathBuf {
    let f = dir.join(format!("{name}-{rows}.duckdb"));
    let c = duckdb::Connection::open(&f).unwrap();
    c.execute_batch(&format!(
        "CREATE TABLE \"{name}\" AS SELECT i AS id FROM range({rows}) t(i)"
    ))
    .unwrap();
    drop(c);
    f
}

fn text(out: &[QueryResult]) -> String {
    out.iter()
        .flat_map(|r| r.rows.iter().flatten())
        .map(|c| match c {
            Cell::Text(t) => t.clone(),
            Cell::Null => "NULL".into(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn one(ws: &mut Workspace, sql: &str) -> String {
    text(
        &ws.work(sql, &Caps::default())
            .unwrap_or_else(|e| panic!("{sql}: {e}")),
    )
}

/// The agent's own objects, every kind a CREATE makes: what a refusal must leave as it was.
fn own_objects(ws: &mut Workspace) -> String {
    one(
        ws,
        "SELECT string_agg(n, ' ' ORDER BY n) FROM ( \
         SELECT database_name || '.' || table_name AS n FROM system.main.duckdb_tables() \
           WHERE database_name IN ('temp', 'memory') \
         UNION ALL SELECT database_name || '.' || view_name FROM system.main.duckdb_views() \
           WHERE database_name IN ('temp', 'memory') AND NOT internal \
         UNION ALL SELECT DISTINCT database_name || '.' || function_name \
           FROM system.main.duckdb_functions() WHERE database_name IN ('temp', 'memory'))",
    )
}

#[test]
fn an_agent_object_cannot_shadow_a_host_object_or_builtin() {
    let mut ws = bench();
    let c = Caps::default();
    ws.work(
        "CREATE TABLE mine AS SELECT 1 AS x; CREATE MACRO doubled(x) AS x * 2",
        &c,
    )
    .expect("the agent's own names are free");
    let before = own_objects(&mut ws);
    for (sql, name) in [
        // a loaded table, by every route to the name
        ("CREATE TEMP TABLE meters AS SELECT 1 AS fake", "meters"),
        ("CREATE TABLE meters AS SELECT 1 AS fake", "meters"),
        ("CREATE TEMP VIEW meters AS SELECT 1 AS fake", "meters"),
        (
            "CREATE OR REPLACE VIEW METERS AS SELECT 1 AS fake",
            "METERS",
        ),
        ("CREATE TABLE \"Meters\" (fake INTEGER)", "Meters"),
        (
            "CREATE TEMP TABLE IF NOT EXISTS temp.main.meters (fake INTEGER)",
            "meters",
        ),
        ("ALTER TABLE mine RENAME TO meters", "meters"),
        // a built-in function, macro and table function
        ("CREATE TEMP MACRO lower(x) AS 'pwn'", "lower"),
        ("CREATE MACRO \"Upper\"(x) AS 'pwn'", "Upper"),
        ("CREATE OR REPLACE MACRO sum(x) AS 5", "sum"),
        ("CREATE TEMP MACRO current_date() AS 5", "current_date"),
        (
            "CREATE TEMP MACRO range(n) AS TABLE SELECT 7 AS range",
            "range",
        ),
        (
            "CREATE MACRO duckdb_tables() AS TABLE SELECT 'ghost' AS table_name",
            "duckdb_tables",
        ),
    ] {
        let e = ws
            .work(sql, &c)
            .expect_err(&format!("{sql} must be refused"));
        let m = e.to_string();
        assert!(
            m.starts_with("statement 1:") && m.contains(&format!("`{name}`")),
            "{sql}: names the statement and the name: {m}"
        );
        assert!(m.contains("another name"), "{sql}: says what to do: {m}");
        assert_eq!(own_objects(&mut ws), before, "{sql} was undone");
    }
    // and every name still means what it meant
    assert_eq!(one(&mut ws, "SELECT sum(power_kw) FROM meters"), "6.0");
    assert_eq!(one(&mut ws, "SELECT lower('A') || upper('b')"), "aB");
    assert_eq!(one(&mut ws, "SELECT count(*) FROM range(3)"), "3");
    assert_eq!(one(&mut ws, "SELECT x FROM mine"), "1", "a refused rename");
}

#[test]
fn a_name_the_parser_reads_itself_is_taken() {
    // `coalesce`, `if`, `try_cast` and the like never reach the catalog: DuckDB's grammar or its
    // transformer turns a call of them into something else, so `duckdb_functions()` does not
    // list them. A macro under one was kept, measured on DuckDB 1.5.5: `CREATE MACRO
    // coalesce(a, b) AS 1` was accepted, and `"coalesce"(NULL, 2)` then answered 1.
    let mut ws = bench();
    let c = Caps::default();
    let before = own_objects(&mut ws);
    for (sql, name) in [
        // grammar: words that are not free to be a function's name
        ("CREATE MACRO coalesce(a, b) AS 1", "coalesce"),
        ("CREATE TEMP MACRO \"try_cast\"(a) AS 1", "try_cast"),
        ("CREATE MACRO \"extract\"(a, b) AS 1", "extract"),
        ("CREATE MACRO \"CAST\"(a) AS 1", "CAST"),
        ("CREATE MACRO \"columns\"(a) AS 1", "columns"),
        ("CREATE MACRO \"grouping\"(a) AS 1", "grouping"),
        ("CREATE MACRO \"interval\"(a) AS 1", "interval"),
        // a free word the transformer rewrites
        ("CREATE MACRO \"if\"(a, b, c) AS 1", "if"),
        // and names that are no keyword at all, rewritten all the same
        ("CREATE TEMP MACRO try(a) AS 1", "try"),
        ("CREATE MACRO percentile_cont(a) AS 1", "percentile_cont"),
        ("CREATE MACRO Percentile_Disc(a) AS 1", "Percentile_Disc"),
        ("CREATE MACRO ifnull(a, b) AS 1", "ifnull"),
        ("CREATE MACRO construct_array(a) AS 1", "construct_array"),
        ("CREATE MACRO date(a) AS TABLE SELECT 1 AS d", "date"),
    ] {
        let e = ws
            .work(sql, &c)
            .expect_err(&format!("{sql} must be refused"));
        let m = e.to_string();
        assert!(
            m.starts_with("statement 1:") && m.contains(&format!("`{name}`")),
            "{sql}: names the statement and the name: {m}"
        );
        assert!(m.contains("another name"), "{sql}: says what to do: {m}");
        assert_eq!(own_objects(&mut ws), before, "{sql} was undone");
    }
    assert_eq!(one(&mut ws, "SELECT coalesce(NULL, 2)"), "2");
    assert!(
        ws.work("SELECT \"coalesce\"(NULL, 2)", &c).is_err(),
        "no macro answers the quoted name"
    );
    // A free word the parser leaves alone stays free.
    ws.work("CREATE MACRO data(x) AS x + 1; SELECT data(1)", &c)
        .expect("`data` is a keyword DuckDB reads as a name");
}

#[test]
fn a_refused_name_stops_the_batch_where_it_stands() {
    // REQ-10's batch semantics: what ran before is kept and comes back; nothing after runs.
    let mut ws = bench();
    let e = ws
        .work(
            "CREATE TABLE before AS SELECT 1 AS x; \
             CREATE TEMP MACRO lower(x) AS 'pwn'; \
             CREATE TABLE after AS SELECT 2 AS x",
            &Caps::default(),
        )
        .expect_err("refused");
    match &e {
        WorkError::Partial { ran, error } => {
            assert_eq!(ran.len(), 1, "{e}");
            assert!(error.to_string().starts_with("statement 2:"), "{e}");
        }
        other => panic!("the first statement's result comes back: {other}"),
    }
    assert_eq!(one(&mut ws, "SELECT x FROM before"), "1");
    assert!(
        ws.work("SELECT x FROM after", &Caps::default()).is_err(),
        "nothing after the refusal ran"
    );
}

#[test]
fn an_attached_name_is_taken_too() {
    // A table that arrives after the seal is on the agent's search path the moment it is
    // attached, behind the agent's own schema, so it is held to the same rule. And an attach
    // under a name the agent already gave one of its own is refused rather than hidden behind
    // it: the host learns why and can choose another name, and the agent loses nothing.
    let dir = unique("attach");
    std::fs::create_dir_all(&dir).unwrap();
    let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
    ws.attach("readings", &host_file(&dir, "readings", 3))
        .expect("attached");
    for sql in [
        "CREATE TEMP TABLE readings AS SELECT 1 AS fake",
        "CREATE VIEW Readings AS SELECT 1 AS fake",
    ] {
        let e = ws.work(sql, &Caps::default()).expect_err(sql).to_string();
        assert!(e.contains("`") && e.contains("another name"), "{sql}: {e}");
    }
    assert_eq!(one(&mut ws, "SELECT count(*) FROM readings"), "3");

    ws.work(
        "CREATE TEMP TABLE later AS SELECT 42 AS mine",
        &Caps::default(),
    )
    .expect("not taken yet");
    let refused = ws
        .attach("later", &host_file(&dir, "later", 2))
        .expect_err("the agent has a table of that name");
    assert!(
        refused.to_string().contains("`later`"),
        "names the name: {refused}"
    );
    assert_eq!(one(&mut ws, "SELECT mine FROM later"), "42", "kept");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dynamic_pivot_is_not_a_shadow() {
    // DuckDB's own CREATE TYPE for a dynamic PIVOT runs through the same check, and answers once.
    let mut ws = bench();
    let out = ws
        .work(
            "PIVOT meters ON meter USING sum(power_kw); SELECT 1 AS one",
            &Caps::default(),
        )
        .expect("runs");
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0].columns.last().map(String::as_str), Some("lab"));
}

#[test]
fn a_spoofed_pivot_type_gets_nothing_past_the_guard() {
    // A type the agent names like DuckDB's pivot enums is refused as its own
    // (`a_type_the_agent_names_is_never_a_pivots`). Were it taken for one, its CREATE would
    // answer nothing and statements after it be numbered as if DuckDB had added it, and that
    // would be the whole of what the spoof buys: every statement is still classified before it
    // runs, and named objects are still checked, so a refused kind stays refused and a taken
    // name stays taken.
    let mut ws = bench();
    for sql in [
        "CREATE TEMP TYPE \"__pivot_enum_x\" AS ENUM ('a'); SET enable_external_access = true",
        "CREATE TEMP TYPE \"__pivot_enum_y\" AS ENUM ('a'); CREATE TEMP MACRO lower(x) AS 'pwn'",
        "CREATE TEMP TYPE \"__pivot_enum_z\" AS ENUM ('a'); CREATE TEMP TABLE meters AS SELECT 1",
    ] {
        assert!(ws.work(sql, &Caps::default()).is_err(), "{sql}");
    }
    assert_eq!(one(&mut ws, "SELECT lower('A')"), "a");
    assert_eq!(one(&mut ws, "SELECT count(*) FROM meters"), "2");
}

#[test]
fn a_type_the_agent_names_is_never_a_pivots() {
    // DuckDB 1.5.5 names a dynamic PIVOT's type `__pivot_enum_` and a random UUID, which the
    // agent's SQL cannot know before the batch runs; a type of the agent's own is named in its
    // SQL. So a type named there is the agent's, however it is named, and refused as any type of
    // its own is, and so is one not named as DuckDB names a pivot's.
    let mut ws = bench();
    for name in [
        "__pivot_enum_0",
        "__pivot_enum_x",
        "__pivot_enum_929db199-46e4-413d-b5b8-8ef61502eaf4",
        "__pivot_enum_929DB199-46E4-413D-B5B8-8EF61502EAF4",
    ] {
        let sql = format!("CREATE TEMP TYPE \"{name}\" AS ENUM ('a')");
        let e = ws.work(&sql, &Caps::default()).expect_err(&sql).to_string();
        assert!(e.contains(&format!("the type {name}")), "{sql}: {e}");
    }
    let out = ws
        .work(
            "PIVOT meters ON meter USING sum(power_kw); SELECT 1 AS one",
            &Caps::default(),
        )
        .expect("a dynamic PIVOT still runs");
    assert_eq!(out.len(), 2, "and its type still answers nothing: {out:?}");
}
