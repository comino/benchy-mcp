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
        "SELECT coalesce(sum(epoch(held)) FILTER (WHERE power_kw > 3), 0) / 3600 AS hours \
         FROM reading_spans WHERE meter = 'lab' \
         AND observed_at >= '2026-01-01' AND observed_at < '2026-01-02'",
    );
    assert!(hours.contains("0.5"), "the view of spans: {hours}");
    let slots = ok(
        s,
        "SELECT count(*) || '!' AS n FROM hours_of('2026-01-01', '2026-01-02')",
    );
    assert!(slots.contains("24!"), "the table macro: {slots}");
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
        "reading_spans (meter, observed_at, power_kw, held) — view: Each reading with how long",
        "hours_of(start_at, end_at) — table macro: Every hour from start_at",
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
        "DROP MACRO TABLE hours_of",
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
        ("DROP MACRO TABLE hours_of", "hours_of"),
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
    ws.put_helpers(&at, built.shown, &built.reads)
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
fn a_helper_macro_that_reads_a_table_is_refused_at_startup() {
    // DuckDB 1.5.5 expands a macro, scalar or table, inside the caller's query, so a table its
    // body names binds to a CTE of the caller's of that name: `WITH meters AS (…)` made a helper
    // macro over `meters` count the CTE's rows, with no error. A view is bound on its own and
    // reads the table. So a helper macro reads no table: one whose body does, as DuckDB's parser
    // reads it, stops startup, names the helper and says to write it as a view.
    let reads = [
        // a table macro, and the FROM-less CTE of its arguments the agent's own macros use
        (
            "CREATE MACRO of_meter(id) AS TABLE SELECT * FROM meters WHERE meter = id",
            "of_meter",
        ),
        (
            "CREATE MACRO of_meter(id) AS TABLE WITH arg AS (SELECT id AS id) \
             SELECT m.* FROM arg, meters m WHERE m.meter = arg.id",
            "of_meter",
        ),
        // a scalar macro with a subquery, and one nested in subqueries and an expression
        (
            "CREATE MACRO lab_peak() AS (SELECT max(power_kw) FROM meters WHERE meter = 'lab')",
            "lab_peak",
        ),
        (
            "CREATE MACRO deep(x) AS x + (SELECT max(a) FROM \
             (SELECT 1 + (SELECT count(*) FROM meters) AS a) s)",
            "deep",
        ),
        (
            "CREATE MACRO seen(m) AS m IN (SELECT meter FROM rooms)",
            "seen",
        ),
        (
            "CREATE MACRO any_at(m) AS EXISTS (SELECT 1 FROM rooms r WHERE r.meter = m)",
            "any_at",
        ),
        // a CTE in the body over a table; and a CTE of the body's own, which is read as a table
        // too, so no body names one (a subquery in FROM does the same)
        (
            "CREATE MACRO counted() AS TABLE WITH m AS (SELECT * FROM meters) \
             SELECT count(*) AS n FROM m",
            "counted",
        ),
        (
            "CREATE MACRO steps(n) AS TABLE WITH s AS (SELECT * FROM range(n)) SELECT * FROM s",
            "steps",
        ),
        (
            "CREATE MACRO up_to(n) AS TABLE WITH RECURSIVE r(i) AS \
             (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < n) SELECT * FROM r",
            "up_to",
        ),
        // a table function that is not table-free, by name or by string, and DESCRIBE
        (
            "CREATE MACRO listed() AS TABLE SELECT * FROM duckdb_tables()",
            "listed",
        ),
        (
            "CREATE MACRO by_name(t) AS TABLE SELECT * FROM query_table(t)",
            "by_name",
        ),
        (
            "CREATE MACRO shape() AS TABLE SELECT * FROM (DESCRIBE meters)",
            "shape",
        ),
        // a FROM item that is none of the kinds known to read no table: SHOW reads the catalog
        (
            "CREATE MACRO listing() AS TABLE SELECT * FROM (SHOW TABLES)",
            "listing",
        ),
        // in a join, a set operation, one overload of several, a lateral subquery
        (
            "CREATE MACRO joined() AS TABLE SELECT * FROM range(2) r JOIN rooms ON true",
            "joined",
        ),
        (
            "CREATE MACRO both_() AS TABLE SELECT 1 AS x UNION ALL SELECT count(*) FROM meters",
            "both_",
        ),
        (
            "CREATE MACRO ov(a) AS a, (a, b) AS a + b + (SELECT count(*) FROM meters)",
            "ov",
        ),
        (
            "CREATE MACRO lat() AS TABLE SELECT * FROM range(2) r, \
             LATERAL (SELECT count(*) FROM meters WHERE r.range = 0) l",
            "lat",
        ),
        // a helper view, which reads a table, and a table no source provides
        (
            "CREATE VIEW per_meter AS SELECT meter FROM meters;\n\
             CREATE MACRO viewed() AS (SELECT count(*) FROM per_meter)",
            "viewed",
        ),
        ("CREATE MACRO gone() AS TABLE SELECT * FROM meterz", "gone"),
    ];
    for (sql, name) in reads {
        let e = refused(sql);
        assert!(
            e.contains(&format!("helper `{name}` reads a table"))
                && e.contains("CTE")
                && e.contains("view"),
            "{sql}: {e}"
        );
    }
    // What it reads is named, and so is a table function it calls.
    let e = refused("CREATE MACRO lab_peak() AS (SELECT max(power_kw) FROM meters)");
    assert!(e.contains("its body reads `meters`"), "{e}");
    let e = refused("CREATE MACRO listed() AS TABLE SELECT * FROM duckdb_tables()");
    assert!(e.contains("table function `duckdb_tables`"), "{e}");
    // A macro that calls a helper macro that reads a table, at any depth, scalar or table: it
    // reads that table where it is called. Each is named.
    for (sql, names) in [
        (
            "CREATE MACRO total() AS (SELECT count(*) FROM meters);\n\
             CREATE MACRO plus(x) AS x + total();\n\
             CREATE MACRO plus_more(x) AS plus(x) * 2",
            &["total", "plus", "plus_more"][..],
        ),
        (
            "CREATE MACRO lab_rows() AS TABLE SELECT * FROM meters WHERE meter = 'lab';\n\
             CREATE MACRO lab_count() AS TABLE SELECT count(*) AS n FROM lab_rows()",
            &["lab_rows", "lab_count"][..],
        ),
        (
            "CREATE MACRO lab_rows() AS TABLE SELECT * FROM meters WHERE meter = 'lab';\n\
             CREATE MACRO lab_n() AS (SELECT count(*) FROM lab_rows())",
            &["lab_rows", "lab_n"][..],
        ),
    ] {
        let e = refused(sql);
        for name in names {
            assert!(
                e.contains(&format!("helper `{name}` reads a table")),
                "{name} in {sql}: {e}"
            );
        }
    }
}

#[test]
fn a_table_free_helper_macro_and_a_view_serve() {
    // What stays: views, scalar macros over their arguments, and table macros over their
    // arguments alone, through `range`, `generate_series`, `unnest` or VALUES, calling one
    // another. None of them reads a table a CTE of the caller's could stand in for.
    let helpers = "\
        CREATE VIEW lab AS SELECT * FROM meters WHERE meter = 'lab';\n\
        CREATE MACRO kw_to_w(kw) AS kw * 1000;\n\
        CREATE MACRO steps(n) AS TABLE SELECT range AS i FROM range(n);\n\
        CREATE MACRO series(a, b) AS TABLE SELECT * FROM generate_series(a, b) g(i);\n\
        CREATE MACRO each_of(xs) AS TABLE SELECT * FROM unnest(xs) u(x);\n\
        CREATE MACRO pairs() AS TABLE SELECT * FROM (VALUES (1, 'a'), (2, 'b')) v(n, s);\n\
        CREATE MACRO stepped(n) AS TABLE SELECT kw_to_w(i) AS w FROM steps(n);\n\
        CREATE MACRO n_steps(n) AS (SELECT count(*) FROM steps(n));\n";
    let (mut s, dir) = start(helpers, &[]);
    for (sql, want) in [
        ("SELECT count(*) || '!' AS n FROM lab", "4!"),
        ("SELECT kw_to_w(2) || '!' AS w", "2000!"),
        ("SELECT count(*) || '!' AS n FROM steps(3)", "3!"),
        ("SELECT sum(i) || '!' AS n FROM series(1, 4)", "10!"),
        ("SELECT sum(x) || '!' AS n FROM each_of([5, 6])", "11!"),
        ("SELECT max(s) || '!' AS s FROM pairs()", "b!"),
        ("SELECT max(w) || '!' AS w FROM stepped(3)", "2000!"),
        ("SELECT n_steps(5) || '!' AS n", "5!"),
    ] {
        let out = ok(&mut s, sql);
        assert!(out.contains(want), "{sql}: {out}");
    }
    // And a CTE of the agent's named like what they read changes none of them: the view is
    // bound on its own, and a table function is no table a CTE can take, measured on DuckDB 1.5.5.
    let forged = "SELECT 'lab' AS meter, 999.0 AS power_kw FROM range(99)";
    for (sql, want) in [
        (
            format!("WITH meters AS ({forged}) SELECT count(*) || '!' AS n FROM lab"),
            "4!",
        ),
        (
            "WITH range AS (SELECT 1 AS range) SELECT count(*) || '!' AS n FROM steps(3)"
                .to_string(),
            "3!",
        ),
        (
            "WITH steps AS (SELECT 1 AS i) SELECT n_steps(5) || '!' AS n".to_string(),
            "5!",
        ),
    ] {
        let out = ok(&mut s, &sql);
        assert!(out.contains(want), "{sql}: {out}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_string_that_looks_like_sql_runs_beside_helpers() {
    // Beside helper macros, a batch whose text might call `query(` was refused before anything
    // ran, a string literal's text included: `SELECT 'run query (later)' AS note` was refused.
    // No helper macro reads a table now, so such SQL has no table of a helper's to stand a CTE
    // in for, and it runs as it does without helpers. A string literal that names a table
    // function REQ-2 refuses is still only a string (`a_refused_name_in_a_string_literal_is_\
    // only_a_string`). `query()` itself is refused, here as everywhere: the table functions in
    // its string cannot be read before it runs (REQ-2 `a_refused_call_runs_nothing_of_its_batch`).
    let (mut s, dir) = start(EXAMPLE, &[]);
    assert!(ok(&mut s, "SELECT 'run query (later)' AS note").contains("run query (later)"));
    assert!(ok(&mut s, "SELECT 'checkpoint() then query(1)' AS note").contains("checkpoint()"));
    let e = err(&mut s, "SELECT * FROM query('SELECT 42 AS x')");
    assert!(
        e.contains("runs SQL from a string") && e.contains("Nothing ran"),
        "{e}"
    );
    let n = ok(
        &mut s,
        "WITH meters AS (SELECT 1 AS x) SELECT count(*) || '!' AS n FROM hourly",
    );
    assert!(n.contains("2!"), "the view reads the table: {n}");
    answers(&mut s);
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
        "CREATE MACRO hours_of(a, b) AS TABLE SELECT 0 AS hour",
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
    // A macro whose file is its argument would be made, since nothing is read until it is
    // called. It calls a table function that is not table-free, so it is refused at startup
    // (`a_helper_macro_that_reads_a_table_is_refused_at_startup`); the agent's own such macro is
    // refused by the engine's lock when it is called.
    let e = refused("CREATE MACRO read_it(p) AS TABLE SELECT * FROM read_text(p)");
    assert!(
        e.contains("helper `read_it`") && e.contains("`read_text`"),
        "{e}"
    );
    let mut s = Server::start(Path::new(DATA), &[]);
    ok(
        &mut s,
        "CREATE MACRO read_it(p) AS TABLE SELECT * FROM read_text(p)",
    );
    let e = err(&mut s, "SELECT * FROM read_it('/etc/hostname')");
    assert!(
        e.contains("file system operations are disabled") || e.contains("Permission"),
        "{e}"
    );
}

#[test]
fn a_lateral_macro_call_equals_the_per_id_call() {
    // On DuckDB 1.5.5 a macro's argument expressions are substituted into its body, so a body that
    // names an argument next to a table alias of its own binds the caller's `m.meter` to that
    // alias. The naive macro below gives every meter the same answer through LATERAL; the one
    // docs/DIALECT.md shows, which reads its arguments once in a FROM-less CTE, gives each its
    // own. Both are the agent's: a helper macro reads no table
    // (`a_helper_macro_that_reads_a_table_is_refused_at_startup`), so this is advice for the
    // agent's own macros.
    let mut s = Server::start(Path::new(DATA), &[]);
    ok(
        &mut s,
        "CREATE MACRO naive_above(series_id, threshold) AS TABLE \
         SELECT count(*) AS n FROM meters m \
         WHERE m.meter = series_id AND m.power_kw > threshold",
    );
    ok(
        &mut s,
        "CREATE MACRO above(series_id, threshold) AS TABLE \
         WITH arg AS (SELECT series_id AS series_id, threshold AS threshold) \
         SELECT count(*) AS n FROM arg, meters m \
         WHERE m.meter = arg.series_id AND m.power_kw > arg.threshold",
    );
    let per_id: Vec<String> = ["lab", "office", "plant"]
        .iter()
        .map(|id| {
            let out = ok(
                &mut s,
                &format!("SELECT '{id}' AS meter, n FROM above('{id}', 3)"),
            );
            rows(&out).pop().expect("a row")
        })
        .collect();
    let lateral = rows(&ok(
        &mut s,
        "SELECT m.meter, a.n FROM rooms m, LATERAL above(m.meter, 3) a ORDER BY m.meter",
    ));
    assert_eq!(lateral, per_id);
    assert_eq!(per_id, ["lab 3", "office 0", "plant 0"]);

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
