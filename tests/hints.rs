//! REQ-11 `hint_names_the_fix`: a SQL error that does not say what to do instead costs the agent
//! a step, and the agent's next guess is usually the same mistake in a different spelling.
//!
//! Every message asserted here was produced by the real engine first (see `docs/DIALECT.md`).
//! These are not invented error strings: if DuckDB rewords one, this test goes red, which is the
//! point — a hint keyed to a message that no longer occurs is worse than no hint.

mod common;

use benchy_mcp::{source, Caps, Workspace};

/// Tests in one binary run in parallel threads, so the fixture must be unique per test or they
/// delete each other's data.
fn unique(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "benchy-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

fn bench_with_tables() -> Workspace {
    let d = unique("hint");
    let _ = std::fs::create_dir_all(&d);
    std::fs::write(
        d.join("meters.csv"),
        "observed_at,meter,power_kw\n2026-01-01T00:00:00Z,lab,2.0\n2026-01-01T00:15:00Z,lab,4.0\n",
    )
    .unwrap();
    std::fs::write(d.join("rooms.csv"), "meter,room\nlab,B12\n").unwrap();
    let mut bench = common::bench();
    source::load_dir(&mut bench, &d).expect("loads");
    let _ = std::fs::remove_dir_all(&d);
    bench.seal("1GB").expect("seals")
}

fn err_for(ws: &mut Workspace, sql: &str) -> String {
    ws.work(sql, &Caps::default())
        .expect_err("should fail")
        .to_string()
}

#[test]
fn hint_names_the_fix() {
    let mut ws = bench_with_tables();

    // REQUIREMENTS: "a query using `read_parquet` returns an error whose hint names the
    // already-loaded tables". Reaching for a file reader is the single most likely first move
    // for a model that has used DuckDB anywhere else.
    let e = err_for(&mut ws, "SELECT * FROM read_parquet('/tmp/x.parquet')");
    assert!(
        e.contains("meters") && e.contains("rooms"),
        "names what is on the bench: {e}"
    );
    assert!(
        e.contains("already on the bench"),
        "says the data is here, not that the file is missing: {e}"
    );

    // REQUIREMENTS: "a Postgres-ism returns the DuckDB equivalent".
    let e = err_for(
        &mut ws,
        "SELECT to_char(observed_at, 'YYYY-MM') FROM meters",
    );
    assert!(
        e.contains("strftime"),
        "names the DuckDB equivalent of to_char: {e}"
    );
}

#[test]
fn an_unknown_table_names_the_ones_that_exist() {
    let mut ws = bench_with_tables();
    let e = err_for(&mut ws, "SELECT * FROM sales");
    assert!(
        e.contains("meters") && e.contains("rooms"),
        "names the bench: {e}"
    );
}

#[test]
fn a_scratch_table_is_not_forgotten_by_the_hint() {
    // The catalogue the hint names must include what the agent made itself, or the hint sends it
    // looking for a table it just created.
    let mut ws = bench_with_tables();
    ws.work("CREATE TABLE hourly AS SELECT 1 AS n", &Caps::default())
        .expect("scratch");
    let e = err_for(&mut ws, "SELECT * FROM hourley");
    assert!(e.contains("hourly"), "names the scratch table too: {e}");
}

#[test]
fn dialect_slips_from_other_engines_each_name_their_duckdb_form() {
    let mut ws = bench_with_tables();
    for (sql, want) in [
        ("SELECT nvl(power_kw, 0) FROM meters", "coalesce"),
        (
            "SELECT date_format(observed_at, '%Y') FROM meters",
            "strftime",
        ),
        ("SELECT getdate()", "now()"),
        ("SELECT TOP 5 * FROM meters", "LIMIT"),
        ("SELECT `meter` FROM meters", "double quotes"),
        ("SELECT * FROM meters LIMIT 1 FOR UPDATE", "nothing to lock"),
    ] {
        let e = err_for(&mut ws, sql);
        assert!(e.contains(want), "{sql}\n  wanted {want:?} in: {e}");
    }
}

#[test]
fn running_out_of_memory_says_how_to_use_less() {
    let mut ws = bench_with_tables();
    // A sort that cannot fit the budget. The ceiling is what makes this reachable at all.
    let mut ws2 = {
        // memory_limit is applied at seal time, so build a second bench at the small budget
        let d = unique("hint-oom");
        let _ = std::fs::create_dir_all(&d);
        std::fs::write(d.join("t.csv"), "a\n1\n").unwrap();
        let mut b = common::bench();
        source::load_dir(&mut b, &d).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        b.seal("128MB").unwrap()
    };
    let e = err_for(
        &mut ws2,
        "SELECT count(*) FROM (SELECT i, repeat('x',400)||i::VARCHAR AS s \
         FROM range(3000000) t(i) ORDER BY s) q",
    );
    assert!(
        e.contains("Out of Memory") || e.contains("memory"),
        "still says what happened: {e}"
    );
    assert!(
        e.contains("aggregate") || e.contains("--memory"),
        "says how to fit: {e}"
    );
    // and the bench that did not run out is untouched
    assert!(ws.work("SELECT 1", &Caps::default()).is_ok());
}

#[test]
fn a_correct_query_carries_no_hint() {
    // A hint on a success would be noise in every payload, which is the cost REQ-11 exists to
    // avoid. 'Lab' matches no value exactly — the empty-result note would name 'lab' — yet this
    // query answers, so a note that fired on anything but an empty result would show here.
    let mut ws = bench_with_tables();
    let out = ws
        .work(
            "SELECT count(*) FROM meters WHERE meter <> 'Lab'",
            &Caps::default(),
        )
        .expect("runs");
    assert_eq!(out[0].rows.len(), 1);
    assert!(
        out[0].note.is_none(),
        "a result that answered carries no note: {:?}",
        out[0].note
    );
}

#[test]
fn empty_results_say_what_exists() {
    // REQ-11. Zero rows is ambiguous: a wrong filter value and genuinely absent data look
    // identical, so the agent spends a step guessing which it was.
    let mut ws = bench_with_tables();

    let out = ws
        .work("SELECT * FROM meters WHERE meter = 'Lab'", &Caps::default())
        .expect("runs");
    assert!(out[0].rows.is_empty(), "the filter really matches nothing");
    let note = out[0].note.as_deref().unwrap_or("");
    assert!(note.contains("meter"), "names the column: {note}");
    assert!(
        note.contains("lab"),
        "and the value that would match: {note}"
    );

    // Absent data with a value that *is* in the column stays quiet — the agent asked a fair
    // question and got a fair answer, and a note here would be noise.
    let out = ws
        .work(
            "SELECT * FROM meters WHERE meter = 'lab' AND power_kw > 999",
            &Caps::default(),
        )
        .expect("runs");
    assert!(out[0].rows.is_empty());
    assert!(
        out[0].note.is_none(),
        "no note when the vocabulary was right: {:?}",
        out[0].note
    );
}

#[test]
fn the_empty_result_note_stays_inside_the_deadline() {
    // The note ran a full `SELECT DISTINCT … ORDER BY` per text column the query named, after
    // the watchdog had stopped, on a connection it does not interrupt. Columns of unique values
    // can never give a note and were scanned whole anyway: on ten million rows and twelve such
    // columns that took 12 s, so a batch that had done honest work for most of its deadline went
    // past the server's hard one, and the engine was killed with the agent's tables. Here, four
    // million rows: six columns of unique values, which the head of each column rules out, and
    // one, `late`, whose head is one value and whose whole is not, which only the note's budget,
    // what is left of the deadline, stops.
    let mut bench = common::bench();
    let unique: Vec<String> = (1..=6).map(|n| format!("'{n}-' || i AS c{n}")).collect();
    bench
        .connection()
        .execute_batch(&format!(
            "CREATE TABLE w AS SELECT i AS id, {}, 'k' || (i % 3) AS kind, \
             CASE WHEN i % 122880 < 30000 THEN 'a' ELSE 'late-' || i END AS late \
             FROM range(4000000) r(i)",
            unique.join(", "),
        ))
        .unwrap();
    let mut columns = vec![("id".to_string(), "BIGINT".to_string())];
    columns.extend((1..=6).map(|n| (format!("c{n}"), "VARCHAR".to_string())));
    columns.push(("kind".into(), "VARCHAR".into()));
    columns.push(("late".into(), "VARCHAR".into()));
    bench
        .record(benchy_mcp::LoadedTable {
            name: "w".into(),
            rows: 4_000_000,
            columns,
            source: "test".into(),
            profile: None,
            seen: None,
        })
        .unwrap();
    let mut ws = bench.seal("1GB").unwrap();
    // What the note may take past the deadline is what DuckDB needs to stop its interrupted
    // query and the batch to finish, which is the machine's: about 1 ms on the development box,
    // and on GitHub's shared runners 65–157 ms in four runs (macOS and Linux), more than either
    // engine's measured stop latency explained. 250 ms covers that and still fails a note held
    // to its own 500 ms budget instead of the deadline (about 500 ms against 450 here). The
    // server's hard deadline is 10 s past the batch's, so this is the margin that matters.
    const ALLOWANCE: std::time::Duration = std::time::Duration::from_millis(250);
    let mut timed = |sql: &str, deadline: std::time::Duration| {
        let caps = Caps {
            deadline,
            ..Caps::default()
        };
        let t = std::time::Instant::now();
        let out = ws.work(sql, &caps).expect(sql);
        let took = t.elapsed();
        assert!(out[0].rows.is_empty(), "{sql}");
        assert!(
            took < deadline + ALLOWANCE,
            "{sql}: the batch and its note took {took:?} against a deadline of {deadline:?}"
        );
        out[0].note.clone().unwrap_or_default()
    };
    // Read whole, the six took 8.7 s here.
    let named = (1..=6)
        .map(|n| format!("c{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let note = timed(
        &format!("SELECT {named}, kind FROM w WHERE id = -1 AND kind = 'K0'"),
        std::time::Duration::from_secs(1),
    );
    // and the column that can explain the empty result still does, after six that cannot
    assert!(note.contains("w.kind holds: k0, k1, k2"), "{note}");
    // `late` read whole takes about a second. A deadline shorter than the note's own budget
    // is what bounds it. 200 ms keeps the note's own 500 ms budget, were it the bound, well past
    // the allowance.
    let note = timed(
        "SELECT late FROM w WHERE id = -1 AND late = 'b'",
        std::time::Duration::from_millis(200),
    );
    assert_eq!(note, "", "the note ran out of time");
}

#[test]
fn a_non_empty_result_never_carries_a_note() {
    // The mutation test for the feature: make the note unconditional and this goes red.
    let mut ws = bench_with_tables();
    let out = ws
        .work("SELECT * FROM meters WHERE meter = 'lab'", &Caps::default())
        .expect("runs");
    assert!(!out[0].rows.is_empty());
    assert!(out[0].note.is_none());
}

#[test]
fn a_write_to_a_loaded_table_says_how_to_make_your_own() {
    // DuckDB 1.5.5: `Invalid Input Error: Cannot execute statement of type "UPDATE" on database
    // "benchy_1" which is attached in read-only mode!` — true, no help, and it names the
    // bench's internal catalog, which says nothing the agent can use (REQ-15).
    let mut ws = bench_with_tables();
    // a scratch table sorts first in the catalogue, and is not the one to copy
    ws.work("CREATE TABLE aaa AS SELECT 1 AS n", &Caps::default())
        .unwrap();
    for sql in [
        "UPDATE meters SET power_kw = 1",
        "INSERT INTO meters SELECT * FROM meters",
        "DELETE FROM meters",
        "DROP TABLE meters",
        "ALTER TABLE meters ADD COLUMN z INTEGER",
    ] {
        let e = err_for(&mut ws, sql);
        assert!(
            e.contains("loaded tables are read-only")
                && e.contains("CREATE TABLE my_meters AS SELECT * FROM meters"),
            "{sql}: {e}"
        );
        assert!(!e.contains("benchy_"), "{sql}: {e}");
    }
}

#[test]
fn an_export_is_told_results_come_back_through_query() {
    // DuckDB 1.5.5 refuses a write as it refuses a read: `Permission Error: Cannot access file
    // "out.csv" - file system operations are disabled by configuration`. Told to stop using
    // read_csv, an agent that wanted a file out learns nothing.
    let mut ws = bench_with_tables();
    for sql in [
        "COPY meters TO 'out.csv'",
        "COPY (SELECT 1) TO 'out.parquet' (FORMAT parquet)",
        "EXPORT DATABASE 'd'",
        "SELECT 1; COPY meters TO 'out.csv'",
    ] {
        let e = err_for(&mut ws, sql);
        assert!(
            e.contains("no file output") && e.contains("get_result"),
            "{sql}: {e}"
        );
        assert!(!e.contains("read_csv"), "{sql}: {e}");
    }
    // a read keeps its own hint
    let e = err_for(&mut ws, "COPY meters FROM 'in.csv'");
    assert!(e.contains("already on the bench"), "{e}");
}

#[test]
fn a_dynamic_pivot_answers_once() {
    // DuckDB runs a dynamic PIVOT as two statements, a CREATE TYPE of the pivot's values and the
    // query. The first came back as an empty `Count` result with an id of its own, and every
    // statement after the PIVOT was numbered one too high.
    let mut ws = bench_with_tables();
    let out = ws
        .work(
            "PIVOT meters ON meter USING sum(power_kw)",
            &Caps::default(),
        )
        .expect("runs");
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].columns.contains(&"lab".to_string()), "{out:?}");
    let e = err_for(
        &mut ws,
        "PIVOT meters ON meter USING sum(power_kw); SELECT nope FROM meters",
    );
    assert!(e.starts_with("statement 2:"), "{e}");
    // and a statement that returns no rows by nature is not an empty answer to a filter
    let out = ws
        .work(
            "CREATE VIEW v AS SELECT * FROM meters WHERE meter = 'nope'",
            &Caps::default(),
        )
        .expect("runs");
    assert!(out.iter().all(|r| r.note.is_none()), "{out:?}");
}
