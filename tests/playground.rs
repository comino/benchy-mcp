//! REQ-9: the agent's own objects are free; what was loaded is not.

mod common;

use benchy_mcp::{result::Cell, source, Bench, Caps, Workspace};

fn bench() -> Workspace {
    // Tests run in parallel; each load owns the directory it later removes.
    let d = common::unique("play");
    std::fs::create_dir(&d).expect("create this test's input directory");
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

fn text(ws: &mut Workspace, sql: &str) -> String {
    let out = ws.work(sql, &Caps::default()).expect("runs");
    out[0]
        .rows
        .iter()
        .flat_map(|r| r.iter())
        .map(|c| match c {
            Cell::Text(t) => t.clone(),
            Cell::Null => "NULL".into(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn loaded_is_readonly() {
    // Enforced by DuckDB — the loaded data is attached READ_ONLY — not by inspecting statement
    // text, which a computed name would get past.
    let mut ws = bench();
    for sql in [
        "UPDATE meters SET power_kw = 0",
        "DELETE FROM meters",
        "INSERT INTO meters VALUES ('2026-01-02T00:00:00Z', 'x', 1.0)",
        "DROP TABLE meters",
        "ALTER TABLE meters ADD COLUMN extra INTEGER",
    ] {
        assert!(
            ws.work(sql, &Caps::default()).is_err(),
            "{sql} must fail against loaded data"
        );
    }
    assert_eq!(
        text(&mut ws, "SELECT count(*), sum(power_kw) FROM meters"),
        "2,6.0",
        "and the data is exactly as loaded"
    );
}

#[test]
fn benches_opened_together_never_share_a_directory() {
    // REQ-1. On macOS the clock reads to the microsecond, and two benches opened in the same one
    // got the same directory: an error now, and before that a shared bench.
    // A bench is not Send, so each thread hands back where its bench went. The directories stay
    // on disk, so a name handed out twice would fail the second open with "File exists".
    let opened: Vec<Result<std::path::PathBuf, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                s.spawn(|| {
                    (0..4)
                        .map(|_| {
                            Bench::open()
                                .map(|b| b.dir().to_path_buf())
                                .map_err(|e| e.to_string())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    let mut dirs = std::collections::BTreeSet::new();
    let mut failed = Vec::new();
    for r in opened {
        match r {
            Ok(d) => {
                if !dirs.insert(d.clone()) {
                    failed.push(format!("two benches share {d:?}"));
                }
            }
            Err(e) => failed.push(e),
        }
    }
    for d in &dirs {
        let _ = std::fs::remove_dir_all(d);
    }
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(dirs.len(), 32);
}

#[test]
fn scratch_is_free() {
    // The agent's own tables, views and macros persist between calls and are fully mutable.
    let mut ws = bench();
    let c = Caps::default();
    for sql in [
        "CREATE TABLE mine AS SELECT meter, sum(power_kw) AS kw FROM meters GROUP BY 1",
        "INSERT INTO mine VALUES ('extra', 1.5)",
        "UPDATE mine SET kw = kw * 2 WHERE meter = 'extra'",
        "CREATE VIEW mine_v AS SELECT * FROM mine WHERE kw > 0",
        "CREATE MACRO doubled(x) AS x * 2",
    ] {
        ws.work(sql, &c).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    assert_eq!(
        text(
            &mut ws,
            "SELECT doubled(kw) FROM mine_v WHERE meter = 'extra'"
        ),
        "6.0"
    );
    ws.work("DROP VIEW mine_v; DROP TABLE mine; DROP MACRO doubled", &c)
        .expect("and removable again");
}

/// The types the agent's own catalogs hold, `memory` and `temp`: each `database.type`.
fn kept_types(ws: &mut Workspace) -> String {
    text(
        ws,
        "SELECT coalesce(string_agg(database_name || '.' || type_name, ','), '') \
         FROM system.main.duckdb_types() \
         WHERE NOT internal AND database_name IN ('memory', 'temp')",
    )
}

#[test]
fn the_agent_keeps_no_enum() {
    // DuckDB keeps an ENUM's dictionary outside its memory budget: an ENUM type of 3,000,000
    // values left an engine with a 1 GB budget holding 698 MB that `duckdb_memory()` called 0,
    // and ENUM columns accumulated batch after batch. So the agent keeps none: no type of its
    // own, and no table or view with an ENUM column, nested or not. Each is refused as it runs,
    // and nothing it did stays; a table it would have replaced is still there.
    let mut ws = bench();
    ws.work(
        "CREATE TABLE kept AS SELECT 1 AS n; CREATE TABLE plain (a INTEGER); CREATE SCHEMA mine",
        &Caps::default(),
    )
    .expect("scratch");
    let pivotish = "\"__pivot_enum_00000000-0000-4000-8000-000000000000\"";
    for sql in [
        "CREATE TYPE mood AS ENUM ('calm', 'cross')".to_string(),
        "CREATE TEMP TYPE mood AS ENUM ('calm', 'cross')".to_string(),
        "CREATE TYPE big AS ENUM (SELECT i::VARCHAR FROM range(1000) t(i))".to_string(),
        "CREATE TYPE id AS INTEGER".to_string(),
        "CREATE TYPE mine.mood AS ENUM ('calm')".to_string(),
        "CREATE TABLE mine.e AS SELECT 'a'::ENUM('a', 'b') AS x".to_string(),
        "CREATE TABLE e (x ENUM('a', 'b'))".to_string(),
        "CREATE TEMP TABLE e (x ENUM('a', 'b'))".to_string(),
        "CREATE TABLE e AS SELECT 'a'::ENUM('a', 'b') AS x".to_string(),
        "CREATE TABLE e (y STRUCT(z ENUM('c'))[])".to_string(),
        "CREATE TABLE e (m MAP(VARCHAR, ENUM('c')))".to_string(),
        "CREATE VIEW e AS SELECT 'a'::ENUM('a', 'b') AS x".to_string(),
        "CREATE OR REPLACE TABLE kept AS SELECT 'a'::ENUM('a', 'b') AS n".to_string(),
        "ALTER TABLE plain ADD COLUMN x ENUM('a')".to_string(),
        "ALTER TABLE plain ALTER a TYPE ENUM('1')".to_string(),
        // A type named as a dynamic PIVOT's is the agent's all the same.
        format!("CREATE TEMP TYPE {pivotish} AS ENUM ('a'); CREATE TABLE e (x {pivotish})"),
    ] {
        let e = ws.work(&sql, &Caps::default()).expect_err(&sql).to_string();
        assert!(e.contains("ENUM") && e.contains("VARCHAR"), "{sql}: {e}");
        assert_eq!(kept_types(&mut ws), "", "{sql}: a type stayed");
        let tables = text(
            &mut ws,
            "SELECT string_agg(table_name, ',' ORDER BY table_name) \
             FROM system.main.duckdb_tables() WHERE database_name IN ('memory', 'temp')",
        );
        assert_eq!(tables, "kept,plain", "{sql}: what it made stayed");
        let plain = text(
            &mut ws,
            "SELECT string_agg(column_name || ' ' || data_type, ',') \
             FROM system.main.duckdb_columns() WHERE table_name = 'plain'",
        );
        assert_eq!(plain, "a INTEGER", "{sql}: an ALTER stayed");
        let views = text(
            &mut ws,
            "SELECT count(*) FROM system.main.duckdb_views() WHERE NOT internal",
        );
        assert_eq!(views, "0", "{sql}: a view stayed");
        assert_eq!(
            text(&mut ws, "SELECT n FROM kept"),
            "1",
            "{sql}: kept was replaced"
        );
    }
    // An ENUM that nothing keeps is the agent's to use.
    assert_eq!(
        text(&mut ws, "SELECT 'b'::ENUM('a', 'b')::VARCHAR AS v"),
        "b"
    );
}

#[test]
fn an_object_that_keeps_an_enum_in_its_sql_is_refused() {
    // An ENUM need not be a column's type to be kept: a column's DEFAULT, a generated column, a
    // CHECK, an index, a view's or a macro's body keeps the cast it spells, and DuckDB keeps a
    // table whose DEFAULT casts to a type as long as the type, so no DROP TYPE can free it (a
    // host's review pinned 1,500,000 values a batch so). Each is refused as an ENUM column is,
    // in any schema, and nothing it did stays; the helpers are held to the same rule.
    let mut ws = bench();
    ws.work(
        "CREATE TABLE kept AS SELECT 1 AS n; CREATE TABLE plain (a INTEGER); CREATE SCHEMA mine",
        &Caps::default(),
    )
    .expect("scratch");
    let e = "('a'::ENUM('a', 'b'))::VARCHAR";
    for sql in [
        format!("CREATE TABLE e (c INTEGER DEFAULT length({e}))"),
        format!("CREATE TEMP TABLE e (c INTEGER DEFAULT length({e}))"),
        format!("CREATE TABLE mine.e (c INTEGER DEFAULT length({e}))"),
        format!("CREATE TABLE e (c INTEGER, d VARCHAR GENERATED ALWAYS AS ({e}))"),
        "CREATE TABLE e (c VARCHAR CHECK ((c::ENUM('a'))::VARCHAR <> 'x'))".to_string(),
        "CREATE TABLE e (c INTEGER DEFAULT length(('a'::STRUCT(z ENUM('a')))::VARCHAR))"
            .to_string(),
        format!("CREATE VIEW e AS SELECT {e} AS x"),
        format!("CREATE VIEW mine.e AS SELECT {e} AS x"),
        format!("CREATE MACRO e() AS {e}"),
        format!("CREATE TEMP MACRO e() AS {e}"),
        format!("CREATE MACRO mine.e() AS {e}"),
        format!("CREATE MACRO e() AS TABLE SELECT {e} AS x"),
        "CREATE MACRO e(x) AS x::enum('a')".to_string(),
        format!("CREATE INDEX e ON plain ((length({e}) + a))"),
        format!("ALTER TABLE plain ALTER a SET DEFAULT length({e})"),
        // A string can spell a type DuckDB builds: `json_transform` and `from_json` take their
        // structure as a string, and on 1.5.5 `typeof(json_transform('"a"', '"ENUM(''a'')"'))`
        // is `ENUM('a')`, measured. So the rule reads the SQL whole, string literals included,
        // and a literal that only mentions `ENUM(` (`SELECT 'use ENUM(' AS note`) is refused
        // too: telling the two apart would take knowing which functions read a type from a
        // string, a list that could miss one (a host's report, #70).
        "CREATE TABLE e (c VARCHAR DEFAULT (json_transform('\"a\"', '\"ENUM(''a'')\"')::VARCHAR))"
            .to_string(),
        "CREATE VIEW e AS SELECT from_json('\"a\"', '\"ENUM(''a'')\"')::VARCHAR AS x".to_string(),
    ] {
        let err = ws.work(&sql, &Caps::default()).expect_err(&sql).to_string();
        assert!(
            err.contains("ENUM") && err.contains("VARCHAR"),
            "{sql}: {err}"
        );
        let left = text(
            &mut ws,
            "SELECT (SELECT string_agg(table_name, ',' ORDER BY table_name) \
               FROM system.main.duckdb_tables() WHERE database_name IN ('memory', 'temp')) \
             || '|' || (SELECT string_agg(column_name || ' ' || data_type || ' ' \
               || coalesce(column_default, '-'), ',') \
               FROM system.main.duckdb_columns() WHERE table_name = 'plain') \
             || '|' || (SELECT count(*) FROM system.main.duckdb_views() WHERE NOT internal) \
             || '|' || (SELECT count(*) FROM system.main.duckdb_indexes()) \
             || '|' || (SELECT count(*) FROM system.main.duckdb_functions() \
               WHERE database_name IN ('memory', 'temp'))",
        );
        assert_eq!(
            left, "kept,plain|a INTEGER -|0|0|0",
            "{sql}: what it made stayed"
        );
    }
    // An ENUM in a statement that keeps only its values is the agent's to use.
    ws.work(
        &format!("CREATE TABLE fine AS SELECT {e} AS x; DROP TABLE fine"),
        &Caps::default(),
    )
    .expect("a table of VARCHAR");
}

#[test]
fn a_pivot_keeps_no_enum_after_its_batch() {
    // A dynamic PIVOT runs as a CREATE TYPE of the pivoted values, then the query, and DuckDB
    // keeps the type: one pivot over a column of millions of values, again and again, would pile
    // them up as the agent's own ENUMs would. The pivot works, again and beside a CREATE, and its
    // type is gone once its batch is over.
    let mut ws = bench();
    for sql in [
        "PIVOT meters ON meter USING sum(power_kw)",
        "PIVOT meters ON meter USING sum(power_kw)",
        "CREATE TABLE t AS SELECT 1 AS n; PIVOT meters ON meter USING sum(power_kw); SELECT n FROM t",
    ] {
        let out = ws.work(sql, &Caps::default()).expect(sql);
        let pivot = out.iter().find(|r| r.columns.contains(&"lab".to_string()));
        assert!(pivot.is_some(), "{sql}: {out:?}");
        assert_eq!(kept_types(&mut ws), "", "{sql}: the pivot's type outlived its batch");
    }
}
