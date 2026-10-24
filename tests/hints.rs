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
    // what is left of the deadline, stops. The killed engine's error blamed planning. Measured
    // on these four million rows: the six columns took 8.7 s against a one-second deadline
    // before the head check and the budget, and take milliseconds after; `late` is stopped at a
    // 300 ms deadline.
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

/// The hint of an error, after the dash that sets it off from DuckDB's message.
fn hint_of(e: &str) -> &str {
    e.split_once(" — ").map_or("", |(_, h)| h)
}

/// Whether `text` sends the agent to the `describe` tool, which a host's agent may not have
/// (#27). DuckDB's own `DESCRIBE t` is SQL, which every agent can run through its query tool.
fn names_the_describe_tool(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w == "describe")
}

#[test]
fn an_unknown_column_names_the_columns_of_the_tables_it_reads() {
    // #27. DuckDB 1.5.5: `Binder Error: Referenced column "temp" not found in FROM clause!`. The
    // hint named the bench's tables and sent the agent to `describe` for their columns: a step,
    // about 4 KB on a host's bench, to learn the value column is `power_kw`. It now names the
    // columns of the tables the statement reads, and those alone.
    let mut ws = bench_with_tables();
    ws.work(
        "CREATE VIEW mine AS SELECT meter, room AS place FROM rooms",
        &Caps::default(),
    )
    .expect("a view of the agent's");
    for (sql, want, not) in [
        (
            "SELECT temp FROM meters",
            &["meters(observed_at, meter, power_kw)"][..],
            "rooms",
        ),
        (
            "SELECT * FROM meters WHERE kw > 1",
            &["meters(observed_at, meter, power_kw)"],
            "rooms",
        ),
        (
            "SELECT temp FROM meters JOIN rooms USING (meter)",
            &["meters(observed_at, meter, power_kw)", "rooms(meter, room)"],
            "mine",
        ),
        ("SELECT room FROM mine", &["mine(meter, place)"], "meters"),
    ] {
        let e = err_for(&mut ws, sql);
        let h = hint_of(&e);
        for w in want {
            assert!(h.contains(w), "{sql}\n  wanted {w:?} in: {e}");
        }
        assert!(
            !h.contains(not),
            "{sql}: names a table it does not read: {e}"
        );
        assert!(!names_the_describe_tool(h), "{sql}: {e}");
    }
}

#[test]
fn a_qualified_unknown_column_resolves_its_alias() {
    // #27. DuckDB 1.5.5 words a qualified reference otherwise, `Binder Error: Table "m" does not
    // have a column named "temp"`, and one to a CTE or subquery `Binder Error: Values list "c"
    // does not have a column named "temp"`, which it says of a view too; neither had a hint at
    // all. The alias is said as the table it stands for, that table first.
    let mut ws = bench_with_tables();
    ws.work(
        "CREATE VIEW mine AS SELECT meter, room AS place FROM rooms",
        &Caps::default(),
    )
    .expect("a view of the agent's");
    for (sql, want) in [
        (
            "SELECT v.room FROM mine v",
            "`v` is `mine`. The tables the statement reads: mine(meter, place)",
        ),
        (
            "SELECT m.temp FROM meters m",
            "`m` is `meters`. The tables the statement reads: meters(observed_at, meter, power_kw)",
        ),
        (
            "SELECT m.temp FROM meters AS m",
            "`m` is `meters`. The tables the statement reads: meters(observed_at, meter, power_kw)",
        ),
        (
            "SELECT meters.temp FROM meters",
            "That column is not in `meters`. The tables the statement reads: \
             meters(observed_at, meter, power_kw)",
        ),
        (
            "SELECT r.temp FROM meters m JOIN rooms r USING (meter)",
            "`r` is `rooms`. The tables the statement reads: rooms(meter, room); \
             meters(observed_at, meter, power_kw)",
        ),
        (
            "SELECT \"R\".temp FROM meters m JOIN rooms \"R\" USING (meter)",
            "`R` is `rooms`.",
        ),
        (
            "WITH c AS (SELECT * FROM meters) SELECT c.temp FROM c",
            "`c` is a subquery or CTE of the statement. The tables the statement reads: \
             meters(observed_at, meter, power_kw)",
        ),
    ] {
        let e = err_for(&mut ws, sql);
        assert!(
            hint_of(&e).contains(want),
            "{sql}\n  wanted {want:?} in: {e}"
        );
        assert!(!names_the_describe_tool(hint_of(&e)), "{sql}: {e}");
    }
}

#[test]
fn a_name_the_statement_gives_hides_the_table_of_that_name() {
    // #27, REQ-11's "and of no others": an alias or a CTE named like a table on the bench is not
    // that table. `FROM rooms AS meters` reads `rooms` alone, and its `meters.power_kw` is
    // `rooms`'s; the hint listed `meters` first, as the table qualified, which the statement does
    // not read.
    let mut ws = bench_with_tables();
    for (sql, want) in [
        (
            "SELECT meters.power_kw FROM rooms AS meters",
            "`meters` is `rooms`. The tables the statement reads: rooms(meter, room).",
        ),
        (
            "SELECT meters.power_kw FROM rooms meters",
            "`meters` is `rooms`. The tables the statement reads: rooms(meter, room).",
        ),
        (
            "SELECT power_kw FROM rooms AS meters",
            "That column is not in the tables the statement reads: rooms(meter, room).",
        ),
        (
            "WITH meters AS (SELECT * FROM rooms) SELECT meters.power_kw FROM meters",
            "`meters` is a subquery or CTE of the statement. The tables the statement reads: \
             rooms(meter, room).",
        ),
        (
            "WITH meters AS (SELECT * FROM rooms) SELECT power_kw FROM meters",
            "That column is not in the tables the statement reads: rooms(meter, room).",
        ),
        // In its own body a CTE's name is the table's.
        (
            "WITH meters AS (SELECT meter FROM meters) SELECT power_kw FROM meters",
            "That column is not in the tables the statement reads: \
             meters(observed_at, meter, power_kw).",
        ),
        // A recursive CTE's name is the CTE in its body too.
        (
            "WITH RECURSIVE meters AS (SELECT meter FROM rooms UNION ALL \
             SELECT meter FROM meters WHERE false) SELECT power_kw FROM meters",
            "That column is not in the tables the statement reads: rooms(meter, room).",
        ),
        // A subquery's alias, and a column's.
        (
            "SELECT meters.power_kw FROM (SELECT * FROM rooms) meters",
            "The tables the statement reads: rooms(meter, room).",
        ),
        (
            "SELECT meter AS rooms, temp FROM meters",
            "That column is not in the tables the statement reads: \
             meters(observed_at, meter, power_kw).",
        ),
        // Where the statement reads the table as well, under a name of its own, it is listed,
        // and an alias called like it is still the table it stands for.
        (
            "SELECT m.temp FROM meters m JOIN rooms AS meters USING (meter)",
            "`m` is `meters`. The tables the statement reads: \
             meters(observed_at, meter, power_kw); rooms(meter, room).",
        ),
        (
            "SELECT meters.temp FROM meters m JOIN rooms meters USING (meter)",
            "`meters` is `rooms`. The tables the statement reads: \
             rooms(meter, room); meters(observed_at, meter, power_kw).",
        ),
    ] {
        let e = err_for(&mut ws, sql);
        let h = hint_of(&e);
        assert!(h.contains(want), "{sql}\n  wanted {want:?} in: {e}");
    }
}

#[test]
fn an_unknown_table_lists_the_columns_of_what_is_here() {
    // #27: the tables it could have meant, with their columns, so the next statement can be
    // right; and no `describe`, which a host's agent may not have.
    let mut ws = bench_with_tables();
    let e = err_for(&mut ws, "SELECT temp FROM sales");
    let h = hint_of(&e);
    assert!(
        h.contains("What is here: meters(observed_at, meter, power_kw); rooms(meter, room)."),
        "{e}"
    );
    assert!(!names_the_describe_tool(h), "{e}");
}

#[test]
fn the_columns_a_hint_lists_are_bounded() {
    // #27. The hint is said with every error, inside the reply's byte cap: a table of 2,000
    // columns, or a bench of 30 tables, is cut by the hint itself, saying what it left out and
    // where the rest is, rather than by the cap, which would cut the sentence off.
    let mut bench = common::bench();
    let cols: Vec<String> = (0..2000)
        .map(|i| format!("a_long_column_name_{i:04}"))
        .collect();
    let mut sql = format!(
        "CREATE TABLE wide AS SELECT {}",
        cols.iter()
            .map(|c| format!("1 AS {c}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for t in 0..30 {
        sql.push_str(&format!(
            "; CREATE TABLE t{t:02} AS SELECT 1 AS id, 2 AS value_of_t{t:02}"
        ));
    }
    bench.connection().execute_batch(&sql).unwrap();
    let int = |c: &str| (c.to_string(), "INTEGER".to_string());
    let mut tables = vec![("wide".to_string(), cols.iter().map(|c| int(c)).collect())];
    for t in 0..30 {
        tables.push((
            format!("t{t:02}"),
            vec![int("id"), int(&format!("value_of_t{t:02}"))],
        ));
    }
    for (name, columns) in tables {
        bench
            .record(benchy_mcp::LoadedTable {
                name,
                rows: 1,
                columns,
                source: "test".into(),
                profile: None,
                seen: None,
            })
            .unwrap();
    }
    let mut ws = bench.seal("1GB").unwrap();

    let e = err_for(&mut ws, "SELECT temp FROM wide");
    let h = hint_of(&e);
    assert!(
        h.starts_with(
            "That column is not in the tables the statement reads: \
             wide(a_long_column_name_0000, "
        ),
        "{e}"
    );
    assert!(h.contains(" columns])"), "the cut is marked: {e}");
    assert!(
        h.contains("`DESCRIBE wide`"),
        "and says where the rest are: {e}"
    );
    assert!(h.len() < 1300, "{} bytes: {e}", h.len());
    // and where it sends the agent is a statement that runs here
    let out = ws
        .work("DESCRIBE wide", &Caps::default())
        .expect("DESCRIBE runs");
    assert!(!out[0].rows.is_empty());

    // Many tables: as many as the bound holds, with their columns, then how many more.
    let e = err_for(&mut ws, "SELECT * FROM nowhere");
    let h = hint_of(&e);
    assert!(
        h.contains("t00(id, value_of_t00); t01(id, value_of_t01)"),
        "{e}"
    );
    assert!(h.contains(" more)"), "says there are more: {e}");
    assert!(h.len() < 1300, "{} bytes: {e}", h.len());
    assert!(!names_the_describe_tool(h), "{e}");
}

#[test]
fn an_unknown_column_of_a_helper_view_lists_its_columns() {
    // #27, REQ-18: a helper view is a table to the agent, and its columns are the catalog's
    // like any other's.
    let dir = common::unique("hint-helpers");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("helpers.sql"),
        include_str!("../examples/helpers.sql"),
    )
    .unwrap();
    let toml = dir.join("benchy.toml");
    std::fs::write(&toml, "[helpers]\nfile = \"helpers.sql\"\n").unwrap();
    let data = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/data"));
    let mut s = common::Server::start(data, &["--config", toml.to_str().unwrap()]);
    for (sql, want) in [
        (
            "SELECT temp FROM hourly",
            "hourly(meter, hour, mean_kw, peak_kw, readings)",
        ),
        (
            "SELECT h.temp FROM reading_spans h",
            "`h` is `reading_spans`. The tables the statement reads: \
             reading_spans(meter, observed_at, power_kw, held)",
        ),
    ] {
        let (err, body, _) = s.query(sql);
        assert!(err, "{sql} ran: {body}");
        assert!(body.contains(want), "{sql}\n  wanted {want:?} in: {body}");
        assert!(!names_the_describe_tool(hint_of(&body)), "{sql}: {body}");
    }
    let _ = std::fs::remove_dir_all(dir);
}
