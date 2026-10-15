//! REQ-2 `nothing_in_the_corpus_reaches_outside_the_workspace` and the tests around it, driven by
//! `corpus/escape.toml`.
//!
//! The requirement is that **nothing leaks**. The per-layer expectations recorded in the corpus
//! are a second, weaker assertion: they turn a DuckDB upgrade that re-types a statement into a
//! failing test rather than a silent change of posture.

mod common;

use benchy_mcp::{guard::Verdict, result::Cell, Caps, Workspace};

fn sealed() -> Workspace {
    common::bench().seal("1GB").expect("seals")
}
use serde::Deserialize;

const CANARY: &str = "CANARY-8f31d0a7c4e2";

#[derive(Deserialize)]
struct Corpus {
    case: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    sql: String,
    category: String,
    /// The statement type the classifier reads under lockdown, or `UNBOUND`.
    kind: String,
    allowlist: String,
    engine: String,
    #[serde(default)]
    alone: Option<String>,
    /// Under lockdown this statement cannot be prepared, so the allowlist never classifies it.
    #[serde(default)]
    locked: Option<String>,
    /// REQ-9: refused and undone by the no-shadowing check, which neither layer above is.
    #[serde(default)]
    shadow: Option<String>,
}

fn load_corpus() -> Corpus {
    let text = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/escape.toml"),
    )
    .expect("corpus/escape.toml");
    toml::from_str(&text).expect("corpus parses")
}

fn canary_path() -> std::path::PathBuf {
    let p = std::env::temp_dir().join("benchy-test-canary.txt");
    std::fs::write(&p, format!("secret,{CANARY}\n")).unwrap();
    p
}

/// Every cell of every result, so a canary anywhere is detectable.
fn flatten(results: &[benchy_mcp::QueryResult]) -> String {
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

#[test]
fn nothing_in_the_corpus_reaches_outside_the_workspace() {
    let canary = canary_path();
    let path = canary.display().to_string();
    let corpus = load_corpus();
    let mut ws = sealed();
    ws.work("CREATE TABLE loaded(a INTEGER)", &Caps::default())
        .expect("setup");

    let mut leaked = Vec::new();
    let mut ran = 0usize;
    for case in &corpus.case {
        let sql = case.sql.replace("{canary}", &path);
        // A refusal and a failure are equally fine here; neither can leak. Only a statement
        // that *ran* can put a canary in a result.
        if let Ok(results) = ws.work(&sql, &Caps::default()) {
            ran += 1;
            if flatten(&results).contains(CANARY) {
                leaked.push(format!("{} [{}]", case.sql, case.category));
            }
        }
    }
    let _ = std::fs::remove_file(&canary);

    assert!(
        leaked.is_empty(),
        "{} of {} corpus cases reached a canary outside the workspace:\n  {}",
        leaked.len(),
        corpus.case.len(),
        leaked.join("\n  ")
    );
    assert!(
        ran > 0,
        "no case ran at all — the corpus is not exercising anything"
    );
}

#[test]
fn the_recorded_division_of_labour_still_holds() {
    let canary = canary_path();
    let path = canary.display().to_string();
    let corpus = load_corpus();
    let ws = sealed();

    let mut drift = Vec::new();
    let mut unprepared = 0usize;
    for case in &corpus.case {
        let sql = case.sql.replace("{canary}", &path);
        let verdict = ws.classify(&sql);
        let cannot_bind = matches!(&verdict, Verdict::Allow(k) if k.contains(&"UNBOUND"));
        let kind = match &verdict {
            Verdict::Allow(k) => k.join("+"),
            Verdict::Refuse { kind, .. } => kind.to_string(),
            Verdict::Unparseable(_) => "UNPARSEABLE".into(),
        };
        if kind != case.kind {
            drift.push(format!(
                "{}  corpus says kind={}, measured {kind}",
                case.sql, case.kind
            ));
        }

        if case.locked.as_deref() == Some("unprepared") {
            // The locked engine refuses these at prepare, earlier than the allowlist could
            // classify them. Assert that is still what happens: if one ever becomes preparable,
            // the allowlist must be what stops it, and this test is where that surfaces.
            if cannot_bind {
                unprepared += 1;
            } else {
                drift.push(format!(
                    "{}  corpus says unpreparable under lockdown, but it now binds",
                    case.sql
                ));
            }
            continue;
        }
        let refused = matches!(verdict, Verdict::Refuse { .. });
        let expected = case.allowlist == "refuse";
        if refused != expected {
            drift.push(format!(
                "{}  corpus says allowlist={}, measured {}",
                case.sql,
                case.allowlist,
                if refused { "refuse" } else { "pass" }
            ));
        }
    }
    assert_eq!(
        unprepared, 4,
        "the engine should still refuse four statements before the allowlist sees them"
    );
    let _ = std::fs::remove_file(&canary);
    assert!(
        drift.is_empty(),
        "DuckDB's statement typing has changed under {} case(s):\n  {}\n\
         Re-measure each case alone, locked and unlocked, before updating the corpus.",
        drift.len(),
        drift.join("\n  ")
    );
}

#[test]
fn file_readers_are_select_typed_so_layer_one_cannot_stop_them() {
    // The category error REQ-2 is written around. If DuckDB ever re-types these, REQ-2's
    // division of labour can be revisited — but not before.
    let ws = sealed();
    for sql in [
        "SELECT * FROM read_text('/etc/hostname')",
        "SELECT * FROM read_blob('/etc/hostname')",
        "SELECT * FROM (SELECT * FROM read_text('/etc/hostname'))",
    ] {
        assert!(
            !matches!(ws.classify(sql), Verdict::Refuse { .. }),
            "{sql} is now refused by the allowlist — REQ-2's reasoning has changed"
        );
    }
}

#[test]
fn path_disclosure_names_only_our_own_private_workspace() {
    // Neither SQL-level mechanism stops these, and none ever will — so assert the mitigation.
    // Loaded data is attached READ_ONLY from a private file so DuckDB can enforce REQ-9, which
    // means a path *is* disclosed. What matters is that it is a 0700 temp directory we created,
    // removed when the session ends, and that nothing else leaks: not the operator's home, not
    // the directory the data actually came from.
    let mut ws = sealed();
    for sql in [
        "PRAGMA database_list",
        "SELECT * FROM duckdb_databases()",
        "SELECT current_setting('home_directory')",
    ] {
        let out = ws.work(sql, &Caps::default()).expect("these run by design");
        let text = flatten(&out);
        assert!(
            !text.contains("/home/"),
            "{sql} disclosed a home path: {text}"
        );
        for seg in text.split_whitespace().filter(|s| s.contains(".duckdb")) {
            assert!(
                seg.contains("benchy-"),
                "{sql} disclosed a file outside our own private workspace: {seg}"
            );
        }
    }
}

#[test]
fn a_denied_statement_says_what_is_allowed() {
    // REQ-11: the error has to teach, and the highest-value line is benchy's delta from
    // stock DuckDB — an agent that knows DuckDB reaches for read_parquet immediately.
    let mut ws = sealed();
    let err = ws
        .work("ATTACH '/etc/hostname' AS e", &Caps::default())
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("ATTACH"), "names the offending kind: {msg}");
    assert!(msg.contains("SELECT"), "names what is allowed: {msg}");
    assert!(
        msg.contains("already loaded"),
        "points at the loaded tables: {msg}"
    );
}

#[test]
fn a_batch_returns_every_result_not_only_the_last() {
    let mut ws = sealed();
    let out = ws
        .work(
            "SELECT 1 AS a; SELECT 2 AS b; SELECT 3 AS c",
            &Caps::default(),
        )
        .expect("batch runs");
    assert_eq!(out.len(), 3, "each statement's result comes back");
    let ids: Vec<_> = out.iter().map(|r| r.result_id.as_str()).collect();
    assert_eq!(
        ids.len(),
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        "result ids are unique: {ids:?}"
    );
}

#[test]
fn a_refused_statement_stops_the_batch_before_anything_runs() {
    let mut ws = sealed();
    ws.work("CREATE TABLE marker(n INTEGER)", &Caps::default())
        .unwrap();
    let err = ws
        .work(
            "INSERT INTO marker VALUES (1); ATTACH '/etc/hostname' AS e",
            &Caps::default(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("ATTACH"));
    let out = ws
        .work("SELECT count(*) FROM marker", &Caps::default())
        .unwrap();
    assert_eq!(
        out[0].rows[0][0],
        Cell::Text("0".into()),
        "the permitted statement must not have run either"
    );
}

#[test]
fn results_are_capped_and_say_so() {
    let mut ws = sealed();
    let caps = Caps {
        max_rows: 10,
        ..Caps::default()
    };
    let out = ws
        .work("SELECT i FROM range(1000) t(i)", &caps)
        .expect("runs");
    // The cap is applied where the agent sees it: in the rendered response.
    let text = benchy_mcp::result::render_batch(&out, &caps);
    let shown = text
        .lines()
        .filter(|l| l.trim().parse::<u64>().is_ok())
        .count();
    assert_eq!(shown, 10, "one page of rows: {text}");
    assert!(
        text.contains("TRUNCATED"),
        "truncation is explicit, never silent: {text}"
    );
}

#[test]
fn a_runaway_batch_loses_the_batch_not_the_bench() {
    // REQ-7. Probe 2 proved the mechanism against DuckDB directly; this proves the product
    // wires it up, and that the workspace is genuinely still usable afterwards.
    use std::time::{Duration, Instant};
    let mut ws = sealed();
    ws.work("CREATE TABLE marker AS SELECT 42 AS n", &Caps::default())
        .unwrap();

    let caps = Caps {
        deadline: Duration::from_millis(500),
        ..Caps::default()
    };
    let start = Instant::now();
    let err = ws
        .work(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t) SELECT sum(n) FROM t",
            &caps,
        )
        .unwrap_err();
    let elapsed = start.elapsed();

    let msg = err.to_string();
    assert!(msg.contains("deadline"), "says what happened: {msg}");
    assert!(
        msg.contains("intact"),
        "reassures that the data survived: {msg}"
    );
    assert!(msg.contains("Narrow"), "says how to fix it (REQ-11): {msg}");
    assert!(
        elapsed < Duration::from_secs(10),
        "stopped promptly, took {elapsed:?}"
    );

    // the whole point: the bench survives
    let out = ws
        .work("SELECT n FROM marker", &Caps::default())
        .expect("still usable");
    assert_eq!(out[0].rows[0][0], Cell::Text("42".into()));
}

#[test]
fn a_deadline_holds_between_statements() {
    // The watchdog interrupted once, at the deadline. DuckDB clears a pending interrupt when the
    // next statement starts, so one that landed between two statements of a batch was lost and
    // the rest of the batch ran unbounded — measured: a 20 ms deadline, a thousand statements,
    // finished after half a second. Only the supervisor's hard kill stood behind it.
    //
    // Counted rather than timed: each statement records that it ran, so a deadline that was
    // lost shows as every statement having run, however fast or slow the machine is.
    use std::time::Duration;
    let mut ws = sealed();
    let caps = Caps {
        deadline: Duration::from_millis(20),
        ..Caps::default()
    };
    ws.work("CREATE TABLE ran (n BIGINT)", &Caps::default())
        .expect("scratch");
    // Short statements, so most of the time is spent between them — where the interrupt was
    // lost — with one in ten recording that the batch got that far.
    let statements = 100;
    let ten = ["SELECT count(*) FROM range(2000)"; 9].join("; ") + "; INSERT INTO ran VALUES (1)";
    let sql = vec![ten.as_str(); statements].join("; ");
    for _ in 0..5 {
        ws.work("DELETE FROM ran", &Caps::default()).expect("reset");
        let e = ws.work(&sql, &caps).err().map(|e| e.to_string());
        assert!(
            e.as_deref().is_some_and(|e| e.contains("deadline")),
            "stopped by the deadline: {e:?}"
        );
        let out = ws
            .work("SELECT count(*) AS n FROM ran", &Caps::default())
            .expect("counts");
        let ran: usize = flatten(&out)
            .split_whitespace()
            .filter_map(|w| w.parse().ok())
            .next()
            .expect("a count");
        assert!(
            ran < statements,
            "the whole batch ran past a 20 ms deadline ({ran} of {statements} markers)"
        );
    }
}

#[test]
fn a_batch_that_finishes_is_not_killed_by_its_own_deadline() {
    // A deadline stops a batch that has not finished; it must never be reported against one
    // that has. The watchdog fires only under the lock the batch takes to finish, and a finished
    // batch is judged by its statements, not by whether the watchdog fired. So: batches that end
    // right around their deadline, each leaving a table behind only if it really finished — a
    // batch reported stopped must not have made its table, and one reported done must have.
    use std::time::{Duration, Instant};
    let mut ws = sealed();
    let generous = Caps::default();
    // Calibrate a query to take about as long as the deadline on this machine.
    let deadline = Duration::from_millis(30);
    let mut n: u64 = 200_000;
    let sql = |n: u64| format!("SELECT count(*) AS c FROM range({n}) t(i) WHERE hash(i) % 7 = 1");
    loop {
        let t = Instant::now();
        ws.work(&sql(n), &generous).expect("calibration runs");
        if t.elapsed() >= deadline || n > 1 << 34 {
            break;
        }
        n *= 2;
    }
    let tight = Caps {
        deadline,
        ..Caps::default()
    };
    for i in 0..60 {
        let made = format!("m{i}");
        let r = ws.work(&format!("CREATE TABLE {made} AS {}", sql(n)), &tight);
        let exists = ws.work(&format!("SELECT c FROM {made}"), &generous).is_ok();
        match r {
            Ok(_) => assert!(exists, "{made}: reported done, but it made nothing"),
            Err(e) => {
                assert!(e.to_string().contains("deadline"), "{e}");
                assert!(
                    !exists,
                    "{made} was made, yet its batch was reported stopped"
                );
            }
        }
    }
}

#[test]
fn the_corpus_records_why_the_engine_is_the_boundary() {
    // REQ-2's withdrawn claim, measured rather than recorded. `the_recorded_division_of_labour_\
    // still_holds` measures what the allowlist does alone; this measures the other mechanism.
    // Every case runs straight on a freshly locked engine with no allowlist in front, and must
    // fail or succeed as the corpus says. And every case recorded as a leak must really reach
    // the canary on an unlocked engine — so the lock, and only the lock, is what stops it.
    //
    // Its own canary: the shared one is removed by other tests running in parallel.
    let canary = std::env::temp_dir().join(format!("benchy-canary-{}.txt", std::process::id()));
    std::fs::write(&canary, format!("secret,{CANARY}\n")).unwrap();
    let path = canary.display().to_string();
    let engine = |locked: bool| {
        let c = duckdb::Connection::open_in_memory().unwrap();
        c.execute_batch(
            "SET autoinstall_known_extensions = false; SET autoload_known_extensions = false; \
             CREATE TABLE loaded AS SELECT 1 AS x",
        )
        .unwrap();
        if locked {
            benchy_mcp::lockdown::apply(&c, "1GB").unwrap();
        }
        c
    };
    let text = |c: &duckdb::Connection, sql: &str| -> Result<String, duckdb::Error> {
        let mut st = c.prepare(&format!("SELECT t::VARCHAR FROM ({sql}) t"))?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?.join("\n"))
    };

    let corpus = load_corpus();
    let mut drift = Vec::new();
    for case in &corpus.case {
        let sql = case.sql.replace("{canary}", &path);
        let refused = engine(true).execute_batch(&sql).is_err();
        if refused != (case.engine == "error") {
            drift.push(format!(
                "{}  corpus says engine={}, the locked engine {}",
                case.sql,
                case.engine,
                if refused { "refused it" } else { "ran it" }
            ));
        }
    }
    assert!(drift.is_empty(), "{}", drift.join("\n"));

    let leaks: Vec<&Case> = corpus
        .case
        .iter()
        .filter(|c| c.alone.as_deref() == Some("leaks"))
        .collect();
    assert_eq!(
        leaks.len(),
        5,
        "the measured count of allowlist-alone leaks"
    );
    for c in leaks {
        let sql = c.sql.replace("{canary}", &path);
        assert_eq!(
            c.allowlist, "pass",
            "{}: a leak the allowlist passed",
            c.sql
        );
        let open = text(&engine(false), &sql).unwrap_or_else(|e| panic!("{}: {e}", c.sql));
        assert!(
            open.contains(CANARY),
            "{}: without the lock it must reach the canary, or it is no leak",
            c.sql
        );
        assert!(
            text(&engine(true), &sql).is_err(),
            "{}: the locked engine alone must refuse it",
            c.sql
        );
    }
    let _ = std::fs::remove_file(&canary);
}

#[test]
fn the_corpus_shadows_are_refused_and_undone() {
    // REQ-9 `an_agent_object_cannot_shadow_a_host_object_or_builtin`, over the corpus's cases: on
    // a bench holding a table named `loaded`, each is refused, and every name still means what
    // it meant.
    let d = std::env::temp_dir().join(format!("benchy-corpus-shadow-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("loaded.csv"), "a\n1\n2\n").unwrap();
    let mut b = common::bench();
    benchy_mcp::source::load_dir(&mut b, &d).expect("loads");
    let _ = std::fs::remove_dir_all(&d);
    let mut ws = b.seal("1GB").expect("seals");
    let corpus = load_corpus();
    let mut shadows = 0;
    for case in corpus.case.iter().filter(|c| c.shadow.is_some()) {
        assert_eq!(case.shadow.as_deref(), Some("refused"), "{}", case.sql);
        shadows += 1;
        let e = ws
            .work(&case.sql, &Caps::default())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(e.contains("another name"), "{}: {e:?}", case.sql);
    }
    assert_eq!(shadows, 6, "the corpus's shadowing cases");
    let out = ws
        .work(
            "SELECT lower('A') || sum(a)::VARCHAR AS v FROM loaded",
            &Caps::default(),
        )
        .expect("runs");
    assert_eq!(flatten(&out).trim(), "a3");
    let out = ws
        .work(
            "SELECT count(*)::VARCHAR AS n FROM duckdb_columns() WHERE column_name = 'forged'",
            &Caps::default(),
        )
        .expect("runs");
    assert_eq!(flatten(&out).trim(), "0");
}

#[test]
fn checked_per_statement_before_run() {
    // REQ-2. A statement that depends on an object an earlier statement in the same batch creates
    // cannot be bound up front, so an up-front pass cannot classify it. It has to be checked at
    // the moment it is prepared to run, when that object exists.
    let mut ws = sealed();
    for (sql, kind) in [
        (
            "CREATE TABLE t1 AS SELECT 42 AS x; PREPARE p1 AS SELECT x FROM t1; EXECUTE p1",
            "PREPARE",
        ),
        (
            "CREATE TABLE t2 AS SELECT 1 AS x; CALL pragma_table_info('t2')",
            "CALL",
        ),
        // Not `PRAGMA table_info(...)`: DuckDB rewrites it to a SELECT over pragma_table_info when
        // preparing it, so it is SELECT-typed and allowed standalone too. It reads the catalog,
        // nothing more.
    ] {
        // The CREATE before it ran, so the refusal comes back with its result.
        match ws
            .work(sql, &Caps::default())
            .as_ref()
            .map_err(|e| e.cause())
        {
            Err(benchy_mcp::WorkError::Denied { kind: k, .. }) => {
                assert_eq!(*k, kind, "{sql}: refused as the wrong kind")
            }
            other => panic!(
                "{sql}: the dependent {kind} must be refused by the allowlist, got {other:?}"
            ),
        }
    }

    // A dependent COPY never reaches the allowlist at all: under the lock DuckDB will not even
    // prepare it, the same as a standalone COPY (`locked = "unprepared"` in the corpus). Either
    // way it must not run.
    let err = ws
        .work(
            "CREATE TABLE t4 AS SELECT 1 AS x; COPY t4 TO 'out.csv'",
            &Caps::default(),
        )
        .expect_err("a dependent COPY must not run");
    assert!(
        err.to_string().contains("disabled by configuration"),
        "stopped by the engine lock: {err}"
    );
}

#[test]
fn the_workspace_file_cannot_be_read_as_a_file() {
    // Turning external access off puts every attached database's path on DuckDB's own allowed
    // list, so `read_blob` could read the workspace file's raw bytes by the path the catalog
    // discloses. Once attached, the file is unlinked: DuckDB reads through its handle, and there
    // is nothing at the path.
    let mut ws = sealed();
    let out = ws
        .work(
            "SELECT path FROM duckdb_databases() WHERE database_name = 'bench'",
            &Caps::default(),
        )
        .expect("the catalog answers");
    let path = flatten(&out).trim().to_string();
    assert!(
        path.ends_with(".duckdb"),
        "the path is disclosed, as recorded: {path:?}"
    );
    let read = ws.work(
        &format!("SELECT octet_length(content) AS n FROM read_blob('{path}')"),
        &Caps::default(),
    );
    let got = read.as_ref().map(|r| flatten(r)).unwrap_or_default();
    assert!(
        got.trim().is_empty(),
        "the workspace file was read as a file: {got} bytes"
    );
}

#[test]
fn explain_cannot_smuggle_a_refused_statement() {
    // `EXPLAIN ANALYZE` runs the statement it explains, and DuckDB types the whole thing as
    // EXPLAIN — so allowing EXPLAIN let refused kinds run: SET VARIABLE, PREPARE, CALL,
    // CHECKPOINT. The lock held against every one that matters, but the allowlist said
    // otherwise, and a plan is not worth a hole in it.
    let mut ws = sealed();
    let e = ws
        .work("EXPLAIN ANALYZE SET VARIABLE v = 7", &Caps::default())
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(
        e.contains("EXPLAIN"),
        "refused, and says what it was: {e:?}"
    );
    let out = ws
        .work("SELECT getvariable('v') AS v", &Caps::default())
        .expect("runs");
    assert!(
        !flatten(&out).contains('7'),
        "the smuggled statement ran: {}",
        flatten(&out)
    );
}

#[test]
fn a_dynamic_pivot_still_runs() {
    // The reason checking cannot simply refuse whatever fails to bind up front: DuckDB splits a
    // dynamic PIVOT into a CREATE plus a statement that depends on it.
    let mut ws = sealed();
    let out = ws
        .work(
            "CREATE TABLE s AS SELECT * FROM (VALUES ('a', 1), ('b', 2), ('a', 3)) t(k, v); \
             PIVOT s ON k USING sum(v)",
            &Caps::default(),
        )
        .expect("a dynamic PIVOT is legitimate SQL");
    let last = out.last().expect("a result");
    assert_eq!(last.columns, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn each_statement_kind_is_named_as_duckdb_types_it() {
    // The kind is what a refusal names and what the corpus records. Named wrong, a refusal tells
    // the agent it did something it did not do — `MERGE` and `UPDATE EXTENSIONS` were both
    // refused as "UNKNOWN", and `MERGE` is ordinary DML on the agent's own tables. Every kind the
    // classifier can meet under the lock, measured on DuckDB 1.5.5.
    let mut ws = sealed();
    ws.work("CREATE TABLE t (x INTEGER)", &Caps::default())
        .unwrap();
    for (sql, kind) in [
        ("SELECT 1", "SELECT"),
        ("PRAGMA version", "SELECT"),
        ("CREATE TABLE u (x INTEGER)", "CREATE"),
        ("CREATE MACRO m(a) AS a", "CREATE"),
        ("INSERT INTO t VALUES (1)", "INSERT"),
        ("UPDATE t SET x = 2", "UPDATE"),
        ("DELETE FROM t", "DELETE"),
        (
            "MERGE INTO t USING (SELECT 5 AS x) s ON t.x = s.x \
             WHEN NOT MATCHED THEN INSERT VALUES (s.x)",
            "MERGE",
        ),
        ("DROP TABLE t", "DROP"),
        ("ALTER TABLE t ADD COLUMN y INTEGER", "ALTER"),
    ] {
        let v = ws.classify(sql);
        assert_eq!(v, Verdict::Allow(vec![kind]), "{sql}");
    }
    for (sql, kind) in [
        ("SET threads = 4", "SET"),
        ("USE memory", "SET"),
        ("LOAD json", "LOAD"),
        ("INSTALL json", "LOAD"),
        ("ATTACH ':memory:' AS x", "ATTACH"),
        ("DETACH memory", "DETACH"),
        ("PRAGMA enable_profiling", "PRAGMA"),
        ("CALL pragma_version()", "CALL"),
        ("BEGIN TRANSACTION", "TRANSACTION"),
        ("EXPLAIN SELECT 1", "EXPLAIN"),
        ("VACUUM", "VACUUM"),
        ("PREPARE p AS SELECT 1", "PREPARE"),
        ("UPDATE EXTENSIONS", "UPDATE EXTENSIONS"),
        // Typed as something other than their own kind, which leaves the guard's names for
        // ANALYZE, VARIABLE_SET and COPY_DATABASE unreachable (.cargo/mutants.toml). Should
        // DuckDB start typing one as its own kind, this is where it shows.
        ("ANALYZE", "VACUUM"),
        ("SET VARIABLE v = 1", "SET"),
        ("COPY FROM DATABASE memory TO x", "TRANSACTION"),
    ] {
        assert_eq!(
            ws.classify(sql),
            Verdict::Refuse { index: 1, kind },
            "{sql}"
        );
        let err = ws.work(sql, &Caps::default()).unwrap_err().to_string();
        assert!(err.contains(kind), "the refusal names {kind}: {err}");
    }
    // Under the lock these never bind, so the classifier never reads their kind and the guard's
    // names for COPY, EXPORT and EXECUTE go unused; the engine refuses them first. The corpus
    // records COPY and EXPORT as escapes; EXECUTE has nothing to run, PREPARE being refused.
    for sql in ["COPY t TO 'x.csv'", "EXPORT DATABASE 'x'", "EXECUTE p"] {
        assert_eq!(ws.classify(sql), Verdict::Allow(vec!["UNBOUND"]), "{sql}");
    }
}

#[test]
fn merge_is_ordinary_dml() {
    // On the agent's own tables it works like INSERT and UPDATE do; its source reads files no
    // more than theirs can, since the engine lock stops the reader.
    let mut ws = sealed();
    ws.work(
        "CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1)",
        &Caps::default(),
    )
    .unwrap();
    ws.work(
        "MERGE INTO t USING (SELECT 2 AS x) s ON t.x = s.x \
         WHEN NOT MATCHED THEN INSERT VALUES (s.x)",
        &Caps::default(),
    )
    .expect("merges");
    let out = ws
        .work("SELECT sum(x) + 700 AS s FROM t", &Caps::default())
        .unwrap();
    assert!(
        format!("{:?}", out[0].rows).contains("703"),
        "{:?}",
        out[0].rows
    );
    let canary = canary_path();
    let read = ws.work(
        &format!(
            "MERGE INTO t USING read_text('{}') s ON false \
             WHEN NOT MATCHED THEN INSERT VALUES (length(s.content))",
            canary.display()
        ),
        &Caps::default(),
    );
    assert!(read.is_err(), "the engine lock stops the reader");
    let _ = std::fs::remove_file(&canary);
}
