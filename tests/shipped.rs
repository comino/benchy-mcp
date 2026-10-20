//! What a stranger's `cargo build --release` must be able to do.
//!
//! The build links DuckDB's official library, which carries Parquet, JSON and ICU. A library
//! linked through DUCKDB_LIB_DIR need not — one compiled from the crates.io sources has no ICU —
//! and a reader or a time zone the README promises would then be missing while every other test
//! stays green. This file is where that shows.

mod common;
use common::{data_dir, unique, Server};

#[test]
fn every_advertised_format_loads_in_the_shipped_build() {
    let d = unique("shipped");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/readings.parquet"),
        d.join("pq.parquet"),
    )
    .unwrap();
    std::fs::write(d.join("cs.csv"), "a,b\n1,x\n2,y\n3,z\n").unwrap();
    std::fs::write(d.join("ts.tsv"), "a\tb\n1\tx\n2\ty\n").unwrap();
    std::fs::write(
        d.join("js.json"),
        r#"[{"a":1},{"a":2},{"a":3},{"a":4},{"a":5}]"#,
    )
    .unwrap();
    std::fs::write(d.join("nd.ndjson"), "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n").unwrap();
    std::fs::write(d.join("jl.jsonl"), "{\"a\":1}\n{\"a\":2}\n").unwrap();

    // If any reader is missing from the build, loading fails and the server never answers.
    let mut s = Server::start(&d, &[]);
    for (table, rows) in [
        ("pq", 4),
        ("cs", 3),
        ("ts", 2),
        ("js", 5),
        ("nd", 3),
        ("jl", 2),
    ] {
        let (err, body, _) = s.query(&format!("SELECT count(*) AS n FROM {table}"));
        assert!(
            !err && body.lines().any(|l| l.trim() == rows.to_string()),
            "{table} should have {rows} rows: {body}"
        );
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn time_zone_conversion_works_in_the_shipped_build() {
    // Local-day views depend on converting to a named zone, and without ICU a TIMESTAMPTZ has
    // no date_trunc or interval arithmetic either. The engine runs sealed here, so this also shows
    // the time-zone data needs no file once the process is locked down.
    let d = data_dir();
    let mut s = Server::start(&d, &[]);
    let (err, body, _) = s.query(
        "SELECT timezone('Europe/Berlin', TIMESTAMPTZ '2026-01-15 12:00:00+00')::VARCHAR AS local",
    );
    assert!(
        !err && body.contains("2026-01-15 13:00:00"),
        "noon UTC is 13:00 in Berlin in January: {body}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_agent_is_told_times_are_utc_and_how_to_bucket_local_days() {
    // #52: the session is pinned to UTC, and nothing said so: `SET TimeZone` was refused with the
    // list of statements allowed, and an agent asked for "per day" bucketed in UTC without a
    // word. The instructions say it, and so does the refusal, with the way to local periods; and
    // that way is held here to what it promises, on a loaded TIMESTAMPTZ column of a sealed bench.
    let advice = "Times are UTC. For local calendar periods, convert first, e.g. \
                  `date_trunc('day', timezone('Europe/Berlin', ts))`.";
    let d = unique("utc-days");
    std::fs::create_dir_all(&d).unwrap();
    // 23:30 on the 14th, 00:30 and 13:00 on the 15th in Berlin; in UTC two on the 14th.
    std::fs::write(
        d.join("readings.csv"),
        "ts,kw\n2026-01-14T22:30:00Z,1\n2026-01-14T23:30:00Z,2\n2026-01-15T12:00:00Z,3\n",
    )
    .unwrap();
    let mut s = Server::start(&d, &[]);
    let init = s.call(
        "initialize",
        serde_json::json!({"protocolVersion": "2026-07-28", "capabilities": {},
                           "clientInfo": {"name": "t", "version": "0"}}),
    );
    let instructions = init["result"]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains(advice), "{instructions}");

    for sql in [
        "SET TimeZone = 'Europe/Berlin'",
        "set timezone to 'Europe/Berlin'",
        "SET GLOBAL TimeZone = 'Europe/Berlin'",
        "SET SESSION \"TimeZone\" = 'Europe/Berlin'",
        "RESET TimeZone",
        "SELECT 1; /* local */ SET TimeZone = 'Europe/Berlin'",
    ] {
        let (err, body, _) = s.query(sql);
        assert!(err, "{sql}: {body}");
        assert!(body.contains(advice), "{sql}: {body}");
        assert!(
            !body.contains("Allowed:"),
            "{sql}: the generic list: {body}"
        );
    }
    // Any other setting is refused as before.
    let (err, body, _) = s.query("SET threads = 4");
    assert!(err && body.contains("Allowed: SELECT"), "{body}");
    assert!(!body.contains("Times are UTC"), "{body}");

    let (err, body, _) = s.query("SELECT typeof(ts) AS t FROM readings LIMIT 1");
    assert!(
        !err && body.contains("TIMESTAMP WITH TIME ZONE"),
        "the column the advice is for: {body}"
    );
    let (err, body, _) = s.query(
        "SELECT date_trunc('day', timezone('Europe/Berlin', ts))::VARCHAR AS day, count(*) AS n \
         FROM readings GROUP BY ALL ORDER BY day",
    );
    assert!(!err, "{body}");
    let rows: Vec<&str> = body.lines().skip(1).take(2).collect();
    assert_eq!(
        rows,
        ["2026-01-14 00:00:00 | 1", "2026-01-15 00:00:00 | 2"],
        "Berlin's days: {body}"
    );
    let (err, body, _) = s.query(
        "SELECT date_trunc('day', ts)::VARCHAR AS day, count(*) AS n \
         FROM readings GROUP BY ALL ORDER BY day",
    );
    assert!(!err, "{body}");
    assert!(
        body.contains("2026-01-14 00:00:00+00 | 2"),
        "and UTC's differ: {body}"
    );
    let _ = std::fs::remove_dir_all(&d);
}
