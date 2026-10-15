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
