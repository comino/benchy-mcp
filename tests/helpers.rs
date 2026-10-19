//! REQ-18: SQL helpers, the views and macros an operator supplies in the config, which the agent
//! calls like built-ins and cannot change. Driven through the shipped binary, with the bundled
//! data and the example helpers (`examples/helpers.sql`).

mod common;

use benchy_mcp::result::Cell;
use benchy_mcp::{Bench, Caps, Workspace};
use common::{Server, BIN, PLATFORM};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/data");
const EXAMPLE: &str = include_str!("../examples/helpers.sql");

/// A config naming `helpers`, in a directory of its own, and that directory.
fn config(helpers: &str) -> (PathBuf, PathBuf) {
    let dir = common::unique("helpers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("helpers.sql"), helpers).unwrap();
    let toml = dir.join("benchy.toml");
    std::fs::write(&toml, "[helpers]\nfile = \"helpers.sql\"\n").unwrap();
    (toml, dir)
}

/// A server over the bundled data with `helpers`, and the config's directory.
fn start(helpers: &str, args: &[&str]) -> (Server, PathBuf) {
    let (toml, dir) = config(helpers);
    let mut all = vec!["--config", toml.to_str().unwrap()];
    all.extend_from_slice(args);
    (Server::start(Path::new(DATA), &all), dir)
}

/// What the server says when it does not start with `helpers`: it must exit, and not succeed.
fn refused(helpers: &str) -> String {
    let (toml, dir) = config(helpers);
    let out = Command::new(BIN)
        .arg(DATA)
        .args(["--config", toml.to_str().unwrap()])
        .args(PLATFORM)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "it started with {helpers:?}: {err}");
    err
}

fn ok(s: &mut Server, sql: &str) -> String {
    let (err, body, _) = s.query(sql);
    assert!(!err, "{sql}: {body}");
    body
}

fn err(s: &mut Server, sql: &str) -> String {
    let (err, body, _) = s.query(sql);
    assert!(err, "{sql} ran: {body}");
    body
}

fn describe(s: &mut Server) -> String {
    let v = s.call("tools/call", json!({"name": "describe", "arguments": {}}));
    v["result"]["content"]
        .as_array()
        .map(|cs| cs.iter().filter_map(|c| c["text"].as_str()).collect())
        .unwrap_or_default()
}

/// What the helpers answer, the same before and after anything the agent tries.
fn answers(s: &mut Server) {
    assert!(
        ok(s, "SELECT round(dewpoint(20, 50), 2) AS d").contains("9.26"),
        "the scalar macro"
    );
    let hours = ok(
        s,
        "SELECT hours FROM hours_above('lab', 3, '2026-01-01', '2026-01-02')",
    );
    assert!(hours.contains("0.5"), "the table macro: {hours}");
    let hourly = ok(
        s,
        "SELECT meter, peak_kw, readings FROM hourly ORDER BY meter",
    );
    assert!(
        hourly.contains("lab") && hourly.contains("8.0") && hourly.contains("office"),
        "the view: {hourly}"
    );
}

#[test]
fn a_helper_from_config_serves_read_only() {
    let (mut s, dir) = start(EXAMPLE, &[]);
    answers(&mut s);

    // `describe` names each, with its signature and the comment above it, and nothing of where
    // it came from (REQ-15).
    let d = describe(&mut s);
    for line in [
        "hourly (meter, hour, mean_kw, peak_kw, readings) — view: Readings per meter per hour",
        "dewpoint(t, rh) — macro: Dew point in °C",
        "hours_above(series_id, threshold, start_at, end_at) — table macro: Hours one meter",
    ] {
        assert!(d.contains(line), "{line} is not in: {d}");
    }
    assert!(
        !d.contains("helpers.sql") && !d.contains(dir.to_str().unwrap()),
        "{d}"
    );

    // Read-only, by DuckDB: the catalog they live in is attached READ_ONLY.
    for sql in [
        "DROP MACRO dewpoint",
        "DROP MACRO TABLE hours_above",
        "DROP VIEW hourly",
    ] {
        let e = err(&mut s, sql);
        assert!(e.contains("read-only"), "{sql}: {e}");
    }
    answers(&mut s);
    let _ = std::fs::remove_dir_all(dir);

    // And by DuckDB alone: the same helpers on a bench in this process, where no sandbox runs.
    // (In the engine, Landlock would refuse the write a writable attach needs, so there the
    // file is not even opened writable.)
    let (mut ws, dir) = unsandboxed();
    for sql in [
        "DROP MACRO dewpoint",
        "DROP VIEW hourly",
        "ALTER VIEW hourly RENAME TO h2",
    ] {
        let e = ws.work(sql, &Caps::default()).expect_err(sql);
        assert!(e.to_string().contains("read-only"), "{sql}: {e}");
    }
    let out = ws
        .work("SELECT round(dewpoint(20, 50), 2) AS d", &Caps::default())
        .expect("still there");
    assert_eq!(out[0].rows[0][0], Cell::Text("9.26".into()));
    drop(ws);
    let _ = std::fs::remove_dir_all(dir);
}

/// The example helpers, made by the helpers job's own code and put on a sealed bench in this
/// process, where no sandbox runs, so what keeps them is DuckDB's read-only catalog alone.
#[test]
fn a_write_to_a_helper_is_told_it_is_a_helper() {
    // DuckDB refuses it as a write to a read-only catalog, which the loaded tables' hint read as a
    // loaded table: `DROP MACRO dewpoint` was told "CREATE TABLE my_dewpoint AS SELECT * FROM
    // dewpoint". A helper is told it is one.
    let (mut ws, dir) = unsandboxed();
    for (sql, name) in [
        ("DROP MACRO dewpoint", "dewpoint"),
        ("DROP MACRO TABLE hours_above", "hours_above"),
        ("DROP VIEW hourly", "hourly"),
        ("ALTER VIEW Hourly RENAME TO h2", "hourly"),
    ] {
        let e = ws.work(sql, &Caps::default()).expect_err(sql).to_string();
        assert!(
            e.contains(&format!("`{name}` is a helper the operator provides"))
                && e.contains(&format!("`my_{name}`")),
            "{sql}: {e}"
        );
        assert!(!e.contains("CREATE TABLE my_"), "{sql}: {e}");
    }
    drop(ws);
    let _ = std::fs::remove_dir_all(dir);
}

fn unsandboxed() -> (Workspace, PathBuf) {
    let dir = common::unique("helpers-lib");
    std::fs::create_dir_all(&dir).unwrap();
    let defs = benchy_mcp::helpers::parse(EXAMPLE).unwrap();
    let bench = Bench::open_in(&dir.join("stage")).unwrap();
    let col = |n: &str, t: &str| (n.to_string(), t.to_string());
    let tables = vec![(
        "meters".to_string(),
        vec![
            col("observed_at", "TIMESTAMP WITH TIME ZONE"),
            col("meter", "VARCHAR"),
            col("power_kw", "DOUBLE"),
        ],
    )];
    let built = benchy_mcp::helpers::build(&bench, &defs, &tables, "1GB");
    assert!(built.broken.is_empty(), "{:?}", built.broken);
    let file = bench.finish().unwrap();
    let ws_dir = dir.join("ws");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let mut ws = Workspace::open_sealed(&ws_dir, "1GB").unwrap();
    let at = ws_dir.join("helpers.duckdb");
    std::fs::rename(file, &at).unwrap();
    ws.put_helpers(&at, built.shown, &built.reads, &built.macro_reads)
        .expect("put on the bench");
    (ws, dir)
}

#[test]
fn a_helper_that_shadows_a_builtin_is_refused_at_startup() {
    // DuckDB's own catalog says what is built in, read through `system.main`: a scalar function,
    // a table function and a built-in view, and any spelling of one.
    for (sql, name) in [
        ("CREATE MACRO lower(x) AS x", "lower"),
        ("CREATE MACRO Range(a) AS TABLE SELECT a", "Range"),
        (
            "CREATE VIEW duckdb_tables AS SELECT 1 AS x",
            "duckdb_tables",
        ),
    ] {
        let e = refused(sql);
        assert!(
            e.contains(&format!("helper `{name}`")) && e.contains("built-in"),
            "{sql}: {e}"
        );
    }
    // And benchy's own names: the prefix of its catalogs, and a table's name.
    let e = refused("CREATE MACRO benchy_1() AS 1");
    assert!(e.contains("benchy_"), "{e}");
    let e = refused("CREATE VIEW meters AS SELECT 1 AS x");
    assert!(e.contains("helper `meters`") && e.contains("table"), "{e}");
    // A helper that reads a table no source provides is a mistake, not a wait.
    let e = refused("CREATE VIEW v AS SELECT * FROM meterz");
    assert!(
        e.contains("helper `v` reads `meterz`, which is no table of this session's"),
        "{e}"
    );
}

#[test]
fn a_helper_that_names_a_table_in_a_string_is_refused_at_startup() {
    // The tables a helper reads are taken from the agent (REQ-18
    // `a_table_a_helper_reads_stays_taken_after_a_detach`), found from DuckDB: a macro's body as
    // its parser reads it, a view's bound plan. A table named in a string, `query_table('meters')`,
    // is in neither, so after a detach the agent's `meters` would be what the helper read. A
    // helper names its tables in FROM.
    for (sql, name, function) in [
        (
            "CREATE MACRO m() AS TABLE SELECT * FROM query_table('meters')",
            "m",
            "query_table",
        ),
        (
            "CREATE MACRO n() AS (SELECT count(*) FROM query('SELECT * FROM meters'))",
            "n",
            "query",
        ),
        (
            "CREATE MACRO Q(t) AS TABLE SELECT * FROM QUERY_TABLE(t)",
            "Q",
            "query_table",
        ),
        (
            "CREATE VIEW cols AS SELECT name FROM pragma_table_info('meters')",
            "cols",
            "pragma_table_info",
        ),
        (
            "CREATE VIEW sample AS SELECT * FROM duckdb_table_sample('meters')",
            "sample",
            "duckdb_table_sample",
        ),
    ] {
        let e = refused(sql);
        assert!(
            e.contains(&format!("helper `{name}`")) && e.contains(&format!("`{function}`")),
            "{sql}: {e}"
        );
    }
    // A view over `query_table` is bound to the table it names, which its plan holds: its table
    // is found, and the view stays allowed.
    let (mut s, dir) = start(
        "CREATE VIEW all_meters AS SELECT * FROM query_table('meters');",
        &[],
    );
    assert!(ok(&mut s, "SELECT count(*) + 900 AS n FROM all_meters").contains("9"));
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_helper_named_like_a_parser_keyword_is_refused_at_startup() {
    // The names DuckDB's parser reads itself, which no catalog lists, so the built-in check above
    // does not see them: a helper macro under one was made and served, and a call by its plain
    // name reached DuckDB's own meaning, never the helper. The same set the agent is held to
    // (REQ-9 `a_name_the_parser_reads_itself_is_taken`), any spelling, scalar or table macro.
    for (sql, name) in [
        ("CREATE MACRO coalesce(a, b) AS 1", "coalesce"),
        ("CREATE MACRO grouping(a) AS 1", "grouping"),
        ("CREATE MACRO Try(a) AS 1", "Try"),
        ("CREATE MACRO percentile_cont(a) AS 1", "percentile_cont"),
        ("CREATE MACRO ifnull(a, b) AS 1", "ifnull"),
        ("CREATE MACRO date(a) AS TABLE SELECT 1 AS d", "date"),
    ] {
        let e = refused(sql);
        assert!(
            e.contains(&format!("helper `{name}`"))
                && e.contains("parser")
                && e.contains("another name"),
            "{sql}: {e}"
        );
    }
    // A free word the parser leaves alone stays free, and a view is read by its name in FROM,
    // where the parser takes these words as names.
    let (mut s, dir) = start(
        "CREATE MACRO data(x) AS x + 1;\nCREATE VIEW coalesce AS SELECT 7 AS x;",
        &[],
    );
    assert!(ok(&mut s, "SELECT data(1) AS d").contains('2'));
    assert!(ok(&mut s, "SELECT x FROM coalesce").contains('7'));
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_agent_cannot_replace_or_shadow_a_helper() {
    let (mut s, dir) = start(EXAMPLE, &[]);
    // Shadowing, by any spelling, temporary or not, in either namespace: refused and undone.
    for sql in [
        "CREATE MACRO dewpoint(t, rh) AS 0",
        "CREATE TEMP MACRO DewPoint(t, rh) AS 0",
        "CREATE MACRO hours_above(a, b, c, d) AS TABLE SELECT 0 AS hours",
        "CREATE TEMP TABLE hourly AS SELECT 1 AS x",
        "CREATE VIEW HOURLY AS SELECT 1 AS x",
        "CREATE TABLE dewpoint AS SELECT 1 AS x",
    ] {
        let e = err(&mut s, sql);
        assert!(
            e.contains("is taken") && e.contains("the operator provides a helper"),
            "{sql}: {e}"
        );
    }
    // Renamed into a helper's name: refused too.
    ok(&mut s, "CREATE TABLE mine AS SELECT 1 AS x");
    let e = err(&mut s, "ALTER TABLE mine RENAME TO hourly");
    assert!(e.contains("is taken"), "{e}");

    // Replaced or changed where it lives, by its qualified name: DuckDB refuses a read-only catalog.
    let at = ok(
        &mut s,
        "SELECT DISTINCT database_name FROM duckdb_functions() WHERE function_name = 'dewpoint'",
    );
    let catalog = at
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|w| w.starts_with("benchy_"))
        .unwrap_or_else(|| panic!("no catalog in {at}"))
        .to_string();
    for sql in [
        format!("CREATE OR REPLACE MACRO {catalog}.main.dewpoint(t, rh) AS 0"),
        format!("CREATE MACRO {catalog}.main.extra() AS 1"),
        format!("CREATE TABLE {catalog}.main.readings AS SELECT 1 AS x"),
        format!("DROP MACRO {catalog}.main.dewpoint"),
        "ALTER VIEW hourly RENAME TO h2".to_string(),
        "COMMENT ON VIEW hourly IS 'changed'".to_string(),
        format!("DETACH {catalog}"),
    ] {
        let e = err(&mut s, &sql);
        assert!(
            e.contains("read-only") || e.contains("not allowed here"),
            "{sql}: {e}"
        );
    }
    answers(&mut s);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn helpers_survive_a_rebuild() {
    // REQ-8: a query that wedges the planner, or swells the engine past its memory ceiling, costs
    // the engine. The rebuilt one is given the helpers before its tables, and they are as
    // read-only and as taken as before, whichever killed it.
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let blowup = format!("WITH {} SELECT m FROM l18", ctes.join(", "));
    let mut kills = vec![(
        &["--deadline", "1", "--hard-grace", "1"][..],
        "hard deadline",
    )];
    // Linux only: the memory ceiling is read from /proc, and elsewhere it is not enforced
    // (`a_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
    if cfg!(target_os = "linux") {
        kills.push((
            &[
                "--deadline",
                "60",
                "--hard-grace",
                "60",
                "--memory",
                "64MB",
                "--memory-ceiling",
                "128MB",
            ][..],
            "memory ceiling",
        ));
    }
    for (args, killed_by) in kills {
        let (mut s, dir) = start(EXAMPLE, args);
        answers(&mut s);
        let e = err(&mut s, &blowup);
        assert!(e.contains("rebuilt") && e.contains(killed_by), "{e}");
        answers(&mut s);
        assert!(describe(&mut s).contains("dewpoint(t, rh) — macro"));
        let e = err(&mut s, "DROP MACRO dewpoint");
        assert!(e.contains("read-only"), "{e}");
        let e = err(&mut s, "CREATE MACRO dewpoint(t, rh) AS 0");
        assert!(e.contains("is taken"), "{e}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn a_helper_cannot_reach_outside_the_bench() {
    // Made under the engine's lock, in a loader confined to its staging directory: one that
    // reads a file or the network when it is made stops startup, refused by the lock itself
    // (Landlock would refuse the file too, in other words), and so does one that needs an
    // extension, which nothing loads.
    for sql in [
        "CREATE VIEW leak AS SELECT * FROM read_text('/etc/hostname')",
        "CREATE MACRO leak() AS TABLE SELECT * FROM read_csv('/etc/passwd')",
        "CREATE MACRO leak() AS (SELECT count(*) FROM glob('/*'))",
        "CREATE VIEW leak AS SELECT * FROM 'https://example.com/x.csv'",
    ] {
        let e = refused(sql);
        assert!(
            e.contains("helper `leak`") && e.contains("disabled by configuration"),
            "{sql}: {e}"
        );
    }
    // Nor does a helper keep an ENUM, as the agent may not (REQ-9 `the_agent_keeps_no_enum`,
    // `an_object_that_keeps_an_enum_in_its_sql_is_refused`): not as a column, and not spelled in
    // its body.
    for sql in [
        "CREATE VIEW leak AS SELECT 'a'::ENUM('a', 'b') AS x",
        "CREATE VIEW leak AS SELECT [{'z': 'a'::ENUM('a')}] AS x",
        "CREATE MACRO leak(x) AS x::ENUM('a', 'b')",
        "CREATE MACRO leak(x) AS x::enum('a', 'b')",
        "CREATE VIEW leak AS SELECT ('a'::ENUM('a', 'b'))::VARCHAR AS x",
        "CREATE MACRO leak() AS TABLE SELECT ('a'::ENUM('a', 'b'))::VARCHAR AS x",
    ] {
        let e = refused(sql);
        assert!(e.contains("helper `leak` keeps an ENUM"), "{sql}: {e}");
    }
    let e = refused("CREATE VIEW leak AS SELECT * FROM sqlite_scan('/tmp/x.db', 't')");
    assert!(
        e.contains("helper `leak`") && e.contains("extension"),
        "{e}"
    );
    // Anything but a CREATE VIEW or CREATE MACRO is not a helper.
    for sql in [
        "ATTACH '/tmp/x.duckdb' AS x",
        "COPY (SELECT 1) TO '/tmp/x.csv'",
        "INSTALL httpfs",
        "SET enable_external_access = true",
        "CREATE TABLE t AS SELECT 1 AS x",
        "CREATE MACRO ok() AS 1; LOAD httpfs",
    ] {
        let e = refused(sql);
        assert!(
            e.contains("not a CREATE") || e.contains("creates"),
            "{sql}: {e}"
        );
    }
    // A macro whose file is its argument is made, since nothing is read until it is called, and
    // then the engine's lock refuses it, as it refuses the agent's own SQL.
    let (mut s, dir) = start(
        "CREATE MACRO read_it(p) AS TABLE SELECT * FROM read_text(p)",
        &[],
    );
    let e = err(&mut s, "SELECT * FROM read_it('/etc/hostname')");
    assert!(
        e.contains("file system operations are disabled") || e.contains("Permission"),
        "{e}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_lateral_macro_call_equals_the_per_id_call() {
    // On DuckDB 1.5.5 a macro's argument expressions are substituted into its body, so a body that
    // names an argument next to a table alias of its own binds the caller's `m.meter` to that
    // alias. The naive macro below gives every meter the same answer through LATERAL; the
    // example's, which reads its arguments once in a FROM-less CTE, gives each its own.
    let naive = "CREATE MACRO naive_above(series_id, threshold) AS TABLE \
                 SELECT count(*) AS n FROM meters m \
                 WHERE m.meter = series_id AND m.power_kw > threshold";
    let (mut s, dir) = start(&format!("{EXAMPLE}\n{naive};"), &[]);
    let per_id: Vec<String> = ["lab", "office", "plant"]
        .iter()
        .map(|id| {
            let out = ok(
                &mut s,
                &format!(
                    "SELECT '{id}' AS meter, hours FROM \
                     hours_above('{id}', 3, '2026-01-01', '2026-01-02')"
                ),
            );
            rows(&out).pop().expect("a row")
        })
        .collect();
    let lateral = rows(&ok(
        &mut s,
        "SELECT m.meter, h.hours FROM rooms m, \
         LATERAL hours_above(m.meter, 3, '2026-01-01', '2026-01-02') h ORDER BY m.meter",
    ));
    assert_eq!(lateral, per_id);
    assert_eq!(
        per_id[0].split_whitespace().last(),
        Some("0.5"),
        "{per_id:?}"
    );

    // The trap itself, so this test says when a DuckDB release changes it.
    let naive_each = rows(&ok(
        &mut s,
        "SELECT m.meter, n.n FROM rooms m, LATERAL naive_above(m.meter, 0) n ORDER BY m.meter",
    ));
    let naive_alone: Vec<String> = ["lab", "office", "plant"]
        .iter()
        .map(|id| {
            let out = ok(
                &mut s,
                &format!("SELECT '{id}' AS meter, n FROM naive_above('{id}', 0)"),
            );
            rows(&out).pop().expect("a row")
        })
        .collect();
    assert_eq!(naive_alone, ["lab 4", "office 4", "plant 0"]);
    assert_eq!(
        naive_each,
        ["lab 8", "office 8", "plant 8"],
        "every meter got the count of all of them: the trap is as measured on DuckDB 1.5.5"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_cte_cannot_stand_in_for_a_table_a_helper_macro_reads() {
    // DuckDB 1.5.5 expands a macro inside the caller's query, so a table its body names binds to
    // a CTE of the caller's of that name: `WITH meters AS (…)` made `hours_above` read the CTE,
    // with no error. A view is bound in its own context and reads the table. So a CTE may not be
    // named like a table (or a helper view) that a helper macro, scalar or table, reads; every
    // other CTE name is the agent's.
    let extra = "\
        CREATE MACRO lab_peak() AS (SELECT max(power_kw) FROM meters WHERE meter = 'lab');\n\
        CREATE MACRO peak_of(m) AS TABLE SELECT max(peak_kw) AS p FROM hourly WHERE meter = m;\n\
        CREATE VIEW room_list AS SELECT * FROM rooms;\n";
    let (mut s, dir) = start(&format!("{EXAMPLE}\n{extra}"), &[]);
    let forged = "SELECT 'lab' AS meter, TIMESTAMPTZ '2026-01-01 00:30:00+00' AS observed_at, \
                  999.0 AS power_kw FROM range(5)";
    let hours = "SELECT hours FROM hours_above('lab', 3, '2026-01-01', '2026-01-02')";
    ok(&mut s, "CREATE TABLE mine (hours DOUBLE)");
    for sql in [
        format!("WITH meters AS ({forged}) {hours}"),
        format!("WITH Meters AS ({forged}) SELECT lab_peak() AS p"),
        format!("WITH \"METERS\" AS ({forged}) SELECT lab_peak() AS x"),
        "WITH hourly AS (SELECT 'lab' AS meter, 1.0 AS peak_kw) SELECT * FROM peak_of('lab')"
            .to_string(),
        format!(
            "WITH a AS (WITH meters AS ({forged}) SELECT * FROM meters) SELECT lab_peak() FROM a"
        ),
        format!("SELECT * FROM (WITH meters AS ({forged}) {hours}) s"),
        format!("SELECT 1 AS x WHERE 1 < (WITH meters AS ({forged}) {hours})"),
        "WITH RECURSIVE meters(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM meters WHERE n < 3) \
         SELECT n, lab_peak() FROM meters"
            .to_string(),
        format!("SELECT 1 AS x; WITH meters AS ({forged}) {hours}"),
        format!("CREATE TABLE forged AS WITH meters AS ({forged}) {hours}"),
        format!("CREATE TABLE ok_first AS SELECT 1 AS x; WITH meters AS ({forged}) {hours}"),
        format!("INSERT INTO mine WITH meters AS ({forged}) {hours}"),
        format!("INSERT INTO mine SELECT * FROM (WITH meters AS ({forged}) {hours}) s"),
        format!("CREATE VIEW forged AS WITH meters AS ({forged}) {hours}"),
        format!("CREATE MACRO forged() AS TABLE WITH meters AS ({forged}) {hours}"),
    ] {
        let e = err(&mut s, &sql);
        assert!(
            e.contains("CTE") && e.contains("helper") && e.contains("another"),
            "{sql}: {e}"
        );
    }
    // Refused before anything ran.
    for t in ["forged", "ok_first"] {
        let e = err(&mut s, &format!("SELECT * FROM {t}"));
        assert!(e.contains("does not exist"), "{t}: {e}");
    }
    let n = ok(&mut s, "SELECT count(*) || '!' AS n FROM mine");
    assert!(n.contains("0!"), "nothing was inserted: {n}");
    answers(&mut s);

    // Every other CTE name, and every other mention of one of these names, is the agent's.
    for sql in [
        "WITH data AS (SELECT * FROM meters) SELECT count(*) AS n FROM data",
        "WITH meters_by_hour AS (SELECT * FROM meters) SELECT count(*) AS n FROM meters_by_hour",
        "WITH rooms AS (SELECT 1 AS x) SELECT * FROM rooms",
        "SELECT 'WITH meters AS (…)' AS note",
        "CREATE TABLE daily AS WITH d AS (SELECT * FROM meters) SELECT count(*) AS n FROM d",
        "INSERT INTO daily WITH d AS (SELECT * FROM meters) SELECT count(*) FROM d",
        "CREATE VIEW per_day AS WITH d AS (SELECT * FROM meters) SELECT count(*) AS n FROM d",
    ] {
        ok(&mut s, sql);
    }
    let _ = std::fs::remove_dir_all(dir);

    // Without helpers, no name is taken from a CTE.
    let mut plain = Server::start(Path::new(DATA), &[]);
    ok(
        &mut plain,
        "WITH meters AS (SELECT 1 AS x) SELECT * FROM meters",
    );
}

#[test]
fn a_cte_in_any_statement_cannot_stand_in_for_a_table_a_helper_macro_reads() {
    // From a host's report: the rule above held for SELECT, but `MERGE … USING (WITH meters
    // AS …)` made a helper macro count the CTE's rows and wrote that count into the agent's table,
    // `ok: true`. Measured on DuckDB 1.5.5, the macro binds to the caller's CTE wherever a
    // WITH-led query can stand: MERGE's USING, ON, conditions and actions; UPDATE's SET, FROM and
    // WHERE; DELETE's USING and WHERE; INSERT's VALUES, and before RETURNING or ON CONFLICT; a
    // PIVOT's source; and a WITH ahead of an INSERT, UPDATE, DELETE, MERGE or PIVOT.
    let extra = "CREATE MACRO total() AS (SELECT count(*) FROM meters);\n";
    let (mut s, dir) = start(&format!("{EXAMPLE}\n{extra}"), &[]);
    let f = "WITH meters AS (SELECT 'X' AS meter FROM range(999))";
    ok(
        &mut s,
        "CREATE TABLE m AS SELECT 1 AS k, 0::BIGINT AS v, '' AS note",
    );
    ok(
        &mut s,
        "CREATE TABLE pk (k INTEGER PRIMARY KEY, v BIGINT); INSERT INTO pk VALUES (1, 0)",
    );
    let real = ok(&mut s, "SELECT total() || '!' AS n");
    assert!(!real.contains("999!"), "the helper reads the bench: {real}");
    for sql in [
        // The host's reproduction.
        format!(
            "MERGE INTO m USING ({f} SELECT 1 k, total() v) s ON m.k = s.k \
             WHEN MATCHED THEN UPDATE SET v = s.v"
        ),
        format!(
            "MERGE INTO m USING (SELECT 1 k) s ON m.k = s.k AND ({f} SELECT total()) = 999 \
             WHEN MATCHED THEN UPDATE SET v = 999"
        ),
        format!(
            "MERGE INTO m USING (SELECT 1 k) s ON m.k = s.k \
             WHEN MATCHED AND ({f} SELECT total()) > 0 THEN UPDATE SET v = 999"
        ),
        format!(
            "MERGE INTO m USING (SELECT 1 k) s ON m.k = s.k \
             WHEN MATCHED THEN UPDATE SET v = ({f} SELECT total())"
        ),
        format!(
            "MERGE INTO m USING (SELECT 2 k) s ON m.k = s.k \
             WHEN NOT MATCHED THEN INSERT VALUES (2, ({f} SELECT total()), '')"
        ),
        format!(
            "{f} MERGE INTO m USING (SELECT 1 k, total() v) s ON m.k = s.k \
             WHEN MATCHED THEN UPDATE SET v = s.v"
        ),
        format!("UPDATE m SET v = ({f} SELECT total())"),
        format!("update m set v = ({} select total())", f.to_lowercase()),
        format!("UPDATE m SET v = s.v FROM ({f} SELECT total() v) s"),
        format!("UPDATE m SET v = 999 WHERE ({f} SELECT total()) = 999"),
        format!("UPDATE m SET v = 999 WHERE k IN (SELECT k FROM ({f} SELECT 1 k, total() t) WHERE t = 999)"),
        format!("{f} UPDATE m SET v = total()"),
        format!("UPDATE m SET note = 'é😀 with', v = ({f} SELECT total())"),
        format!("DELETE FROM m WHERE ({f} SELECT total()) = 999"),
        format!("DELETE FROM m USING ({f} SELECT total() t) s WHERE s.t = 999"),
        format!("{f} DELETE FROM m WHERE total() = 999"),
        format!("INSERT INTO m VALUES (2, ({f} SELECT total()), '')"),
        format!("INSERT INTO m {f} SELECT 2, total(), '' RETURNING k"),
        format!("INSERT INTO pk {f} SELECT 1, total() ON CONFLICT DO UPDATE SET v = excluded.v"),
        format!("INSERT INTO m BY NAME ({f} SELECT 2 k, total() v)"),
        format!("{f} INSERT INTO m SELECT 2, total(), ''"),
        format!("PIVOT ({f} SELECT 'a' AS x, total() AS v) ON x USING sum(v)"),
        format!("{f} PIVOT (SELECT 'a' AS x, total() AS v) ON x USING sum(v)"),
        format!("UNPIVOT ({f} SELECT total() AS v) ON v INTO NAME n VALUE x"),
        // In a batch, after and before other statements.
        format!("SELECT 1 AS x; UPDATE m SET v = ({f} SELECT total())"),
        format!("UPDATE m SET v = 1; {f} UPDATE m SET v = total(); SELECT 1 AS x"),
        format!("UPDATE m SET note = ';'; DELETE FROM m WHERE ({f} SELECT total()) = 999"),
        format!("{f} SELECT total() AS n; CREATE TABLE z AS SELECT 1 AS x"),
    ] {
        let e = err(&mut s, &sql);
        assert!(
            e.contains("CTE") && e.contains("helper") && e.contains("another"),
            "{sql}: {e}"
        );
    }
    // A batch with more WITHs than benchy has DuckDB read is refused, not let through unread:
    // each of these takes two parses, so the forged CTE at the end is past the cap.
    let many = "UPDATE m SET v = (WITH d AS (SELECT 1) SELECT 1); ".repeat(40);
    let e = err(
        &mut s,
        &format!("{many}UPDATE m SET v = ({f} SELECT total())"),
    );
    assert!(e.contains("smaller batches"), "{e}");
    let few = "UPDATE m SET v = (WITH d AS (SELECT 1) SELECT 1); ".repeat(10);
    let e = err(
        &mut s,
        &format!("{few}UPDATE m SET v = ({f} SELECT total())"),
    );
    assert!(e.contains("another"), "{e}");
    ok(&mut s, &format!("{few}UPDATE m SET v = 0"));

    // Refused before anything ran: no statement of a refused batch wrote.
    let m = ok(
        &mut s,
        "SELECT count(*) || ':' || sum(v) || ':' || max(note) || '!' AS t FROM m",
    );
    assert!(m.contains("1:0:!"), "m is as it was made: {m}");
    let p = ok(
        &mut s,
        "SELECT count(*) || ':' || sum(v) || '!' AS t FROM pk",
    );
    assert!(p.contains("1:0!"), "pk is as it was made: {p}");
    let z = err(&mut s, "SELECT * FROM z");
    assert!(z.contains("does not exist"), "{z}");
    answers(&mut s);

    // Every other CTE name in these statements, and every other mention of the names, is the
    // agent's.
    for sql in [
        "UPDATE m SET v = (WITH d AS (SELECT * FROM meters) SELECT count(*) FROM d)",
        "UPDATE m SET note = 'done with meters'",
        // Before the public history began, this killed the engine: the fallback sliced the text inside `é`,
        // and the agent's tables were lost with it.
        "UPDATE m SET note = 'é, done with meters'",
        "DELETE FROM m WHERE k = (WITH d AS (SELECT 5 AS k) SELECT k FROM d)",
        "MERGE INTO m USING (WITH d AS (SELECT 1 k, total() v) SELECT * FROM d) s ON m.k = s.k \
         WHEN MATCHED THEN UPDATE SET v = s.v",
        "WITH d AS (SELECT * FROM meters) INSERT INTO m SELECT 3, count(*), 'with' FROM d",
        "PIVOT (WITH d AS (SELECT 'a' AS x, 1 AS v) SELECT * FROM d) ON x USING sum(v)",
        "SELECT * FROM range(3) WITH ORDINALITY AS t(meters, n)",
    ] {
        ok(&mut s, sql);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sql_run_from_a_string_is_refused_beside_helper_macros() {
    // From a host's report: `query('WITH meters AS (…) SELECT * FROM total_kw(''m0'')')` returned
    // the CTE's value, and a view over it kept doing so. The string is SQL DuckDB parses only
    // when it runs, and it can be built then (`||`, `replace()`), so no parse beforehand reads
    // it. Measured on DuckDB 1.5.5: `query` and `json_execute_serialized_sql` run a string as a
    // query, a helper macro in it reads the string's CTE, and the latter reaches a helper by its
    // catalog's name; `query_table` reads only a table named in the string.
    let extra = "CREATE MACRO total() AS (SELECT count(*) FROM meters);\n";
    let (mut s, dir) = start(&format!("{EXAMPLE}\n{extra}"), &[]);
    let catalog = rows(&ok(
        &mut s,
        "SELECT database_name, 1 AS one FROM duckdb_functions() WHERE function_name = 'total'",
    ))
    .pop()
    .and_then(|r| r.split(' ').next().map(str::to_string))
    .expect("the helpers' catalog");
    let f = "WITH meters AS (SELECT ''X'' AS meter FROM range(999))";
    for sql in [
        format!("SELECT * FROM query('{f} SELECT total() AS n')"),
        format!(
            "SELECT * FROM query('WITH me' || 'ters' || '{} SELECT total() AS n')",
            &f[11..]
        ),
        "SELECT * FROM query(replace('WITH m_ AS (SELECT ''X'' AS meter FROM range(999)) \
         SELECT total() AS n', 'm_', 'meters'))"
            .to_string(),
        format!("CREATE VIEW forged AS SELECT * FROM query('{f} SELECT total() AS n')"),
        format!("CREATE MACRO forged() AS TABLE SELECT * FROM query('{f} SELECT total() AS n')"),
        format!(
            "SELECT * FROM json_execute_serialized_sql(json_serialize_sql('{f} SELECT \
             {catalog}.main.total() AS n'))"
        ),
        format!("select * from QUERY ('{f} SELECT total() AS n')"),
        format!("SELECT * FROM \"Query\"('{f} SELECT total() AS n')"),
        format!("SELECT * FROM system.main.query /* c */ ('{f} SELECT total() AS n')"),
        "SELECT 1 AS x; SELECT * FROM query('SELECT 42 AS x')".to_string(),
    ] {
        let e = err(&mut s, &sql);
        assert!(
            e.contains("string") && e.contains("helper") && e.contains("Nothing ran"),
            "{sql}: {e}"
        );
    }
    let e = err(&mut s, "SELECT * FROM forged");
    assert!(e.contains("does not exist"), "{e}");
    answers(&mut s);
    // A table named in a string is still the agent's to read, and a name that only looks like
    // the function is not a call of it.
    for sql in [
        "SELECT count(*) AS n FROM query_table('meters')",
        "SELECT count(*) AS n FROM query_table(['meters', 'meters'])",
        "SELECT 'query' AS query, 'query_table' AS q, count(*) AS my_query FROM meters",
    ] {
        ok(&mut s, sql);
    }
    let _ = std::fs::remove_dir_all(dir);

    // Without helper macros a string's SQL reads nothing a CTE could take, but the table
    // functions it calls cannot be read before it runs either, so REQ-2's table-function
    // allowlist refuses it there too (`a_refused_call_runs_nothing_of_its_batch`).
    let mut plain = Server::start(Path::new(DATA), &[]);
    let e = err(&mut plain, "SELECT * FROM query('SELECT 42 AS x')");
    assert!(
        e.contains("runs SQL from a string") && e.contains("Nothing ran"),
        "{e}"
    );
    assert!(ok(
        &mut plain,
        "SELECT count(*) AS n FROM query_table('meters')"
    )
    .contains('n'));
}

#[test]
fn a_cte_named_like_a_helpers_table_is_refused_only_beside_a_helper_call() {
    // From a host whose helpers read `hourly`: every `WITH hourly AS (…)` was refused, though only
    // a helper macro expanded in the same query reads the CTE. Measured on DuckDB 1.5.5: a macro
    // of the agent's that calls a helper macro reads the caller's CTE too, a view does not (it
    // is bound on its own), and a view or macro whose body holds the CTE reads it through any
    // macro that body calls, replaced later by one that calls a helper.
    let extra = "CREATE MACRO total() AS (SELECT count(*) FROM meters);\n";
    let (mut s, dir) = start(&format!("{EXAMPLE}\n{extra}"), &[]);
    let f = "WITH meters AS (SELECT 'X' AS meter FROM range(999))";
    ok(&mut s, "CREATE TABLE m AS SELECT 1 AS k, 0::BIGINT AS v");
    // No helper macro is called: the CTE is the agent's.
    for sql in [
        format!("{f} SELECT count(*) AS n FROM meters"),
        "WITH hourly AS (SELECT 1 AS x) SELECT * FROM hourly".to_string(),
        "WITH \"METERS\" AS (SELECT 1 AS x) SELECT * FROM meters".to_string(),
        "WITH RECURSIVE meters(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM meters WHERE n < 3) \
         SELECT * FROM meters"
            .to_string(),
        format!("CREATE TABLE copied AS {f} SELECT count(*) AS n FROM meters"),
        format!("UPDATE m SET v = ({f} SELECT count(*) FROM meters)"),
        format!("{f} SELECT count(*) AS n FROM meters, hourly"),
    ] {
        ok(&mut s, &sql);
    }
    // The agent's macros: one that calls no helper leaves the CTE alone; one that does, at any
    // depth, takes it. Made after a CTE was let through, so what benchy knew of them then is
    // not what it goes by.
    ok(&mut s, "CREATE MACRO twice(x) AS x * 2");
    ok(
        &mut s,
        &format!("{f} SELECT twice(count(*)) AS n FROM meters"),
    );
    ok(&mut s, "CREATE MACRO mine() AS TABLE SELECT total() AS n");
    ok(
        &mut s,
        "CREATE MACRO mine_too() AS TABLE SELECT * FROM mine()",
    );
    ok(&mut s, "CREATE MACRO benign() AS TABLE SELECT 1 AS n");
    ok(&mut s, "CREATE VIEW mine_view AS SELECT * FROM mine()");
    let real = ok(&mut s, "SELECT n || '!' AS n FROM mine()");
    assert!(!real.contains("999!"), "{real}");
    for (sql, names) in [
        (format!("{f} SELECT total() AS n"), "`total`"),
        (format!("{f} SELECT * FROM mine()"), "`mine`"),
        (format!("{f} SELECT * FROM mine_too()"), "`mine_too`"),
        (
            format!("{f} SELECT * FROM benign(), mine_too()"),
            "`mine_too`",
        ),
        (
            format!("UPDATE m SET v = ({f} SELECT n FROM mine())"),
            "`mine`",
        ),
        // A view or macro keeps the CTE in its body: `benign` could be replaced later.
        (
            format!("CREATE VIEW forged AS {f} SELECT * FROM benign()"),
            "view or macro",
        ),
        (
            format!("CREATE MACRO forged() AS TABLE {f} SELECT * FROM benign()"),
            "view or macro",
        ),
        (
            format!("CREATE TEMP VIEW forged AS {f} SELECT 1 AS n"),
            "view or macro",
        ),
    ] {
        let e = err(&mut s, &sql);
        assert!(
            e.contains("CTE") && e.contains(names) && e.contains("another"),
            "{sql}: {e}"
        );
    }
    // A view is bound on its own: its macro reads the table, so the CTE beside it is the agent's.
    let v = ok(&mut s, &format!("{f} SELECT n || '!' AS n FROM mine_view"));
    assert!(!v.contains("999!"), "{v}");
    let e = err(&mut s, "SELECT * FROM forged");
    assert!(e.contains("does not exist"), "{e}");
    answers(&mut s);
    let _ = std::fs::remove_dir_all(dir);
}

/// The data rows of a rendered result, each as its cells joined by a space.
fn rows(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| l.contains('|') && !l.contains("---"))
        .skip(1)
        .map(|l| {
            l.split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}
