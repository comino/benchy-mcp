//! Profile regressions reproduced against bff46d2 before their fixes.

mod common;

use benchy_mcp::profile::{self, Shape};
use duckdb::Connection;
use serde_json::json;

fn profile_of(sql: &str, series: &[&str]) -> String {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("SET TimeZone = 'UTC'").unwrap();
    conn.execute_batch(sql).unwrap();
    let columns = conn
        .prepare("DESCRIBE readings")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<(String, String)>, _>>()
        .unwrap();
    profile::compute(
        &conn,
        "readings",
        &columns,
        &Shape {
            time_column: Some("ts".into()),
            series: series.iter().map(|s| (*s).into()).collect(),
        },
        None,
    )
    .unwrap()
    .render(8192)
    .unwrap()
}

/// REQ-14: distinct tuples must remain distinct series, including when a text value contains
/// a control character. Joining key parts with chr(31) aliases these two different tuples.
#[test]
fn composite_series_keys_do_not_alias_on_the_internal_separator() {
    let description = profile_of(
        "CREATE TABLE readings AS
         SELECT TIMESTAMPTZ '2026-01-01 00:00:00+00' + i * INTERVAL 1 HOUR AS ts,
                left_key, right_key, kw
         FROM range(3) r(i), (VALUES
           ('a', 'b' || chr(31) || 'c', 0.0),
           ('a' || chr(31) || 'b', 'c', 1.0)) k(left_key, right_key, kw);",
        &["left_key", "right_key"],
    );
    assert!(
        description.contains("series key left_key, right_key: 2 series"),
        "two different composite keys were merged:\n{description}"
    );
}

/// Infinite timestamps are retained on load (REQ-15). REQ-14's extent must not invent a
/// finite timestamp absent from the data when epoch_us returns NULL for both infinities.
#[test]
fn an_infinite_only_time_column_does_not_invent_an_epoch_extent() {
    let description = profile_of(
        "CREATE TABLE readings AS SELECT * FROM (VALUES
         (TIMESTAMPTZ '-infinity', 1.0), (TIMESTAMPTZ 'infinity', 2.0)) v(ts, kw);",
        &[],
    );
    assert!(description.contains("time column ts"), "{description}");
    assert!(
        !description.contains("1970-01-01"),
        "there is no epoch timestamp in this table:\n{description}"
    );
}

/// DuckDB DATE supports years with more than four digits. Its profile extent must keep the
/// complete date rather than taking a fixed ten-byte prefix of a timestamp (REQ-14).
#[test]
fn dates_with_five_digit_years_keep_the_whole_date_in_the_profile() {
    let description = profile_of(
        "CREATE TABLE readings AS SELECT * FROM (VALUES
         (DATE '12000-01-01', 1.0), (DATE '12000-01-02', 2.0)) v(ts, kw);",
        &[],
    );
    assert!(
        description.contains("12000-01-01 to 12000-01-02"),
        "the date extent was cut:\n{description}"
    );
}

/// Exercise the shipped binary: SQL NULL and a real string containing chr(30) are two
/// different keys. Encoding both as the same sentinel must not merge their series.
#[test]
fn a_loaded_null_series_key_is_distinct_from_a_literal_record_separator() {
    let dir = common::unique("profile-null-key");
    std::fs::create_dir_all(&dir).unwrap();
    let parquet = dir.join("readings.parquet");
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT TIMESTAMPTZ '2026-01-01 00:00:00+00' + i * INTERVAL 1 HOUR AS ts,
                      meter, kw
               FROM range(24) r(i), (VALUES (NULL::VARCHAR, 0.0), (chr(30), 1.0)) k(meter, kw))
         TO '{}' (FORMAT PARQUET)",
        parquet.display().to_string().replace('\'', "''")
    ))
    .unwrap();
    drop(conn);
    let config = dir.join("benchy.toml");
    std::fs::write(
        &config,
        "[[source]]\nname = \"readings\"\ntype = \"file\"\npath = \"./readings.parquet\"\n\
         time_column = \"ts\"\nseries = [\"meter\"]\n",
    )
    .unwrap();
    let mut server = common::Server::launch(&["--config", config.to_str().unwrap()], &[]);
    let query = server.call(
        "tools/call",
        json!({"name": "query", "arguments": {"sql":
            "SELECT count(*) AS distinct_series FROM (SELECT DISTINCT meter FROM readings)"}}),
    );
    let query_text = query["result"]["content"][0]["text"].as_str().unwrap();
    assert!(query_text.contains("\n2\n"), "{query}");
    let reply = server.call("tools/call", json!({"name": "describe", "arguments": {}}));
    let description = reply["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        description.contains("series key meter: 2 series"),
        "NULL and a literal record separator were merged:\n{description}"
    );
}
