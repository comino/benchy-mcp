//! REQ-15: origin-blind. The same data gives the same table, whatever it came from.
//!
//! One fixture, `tests/fixtures/origin/readings.csv`, is put on the bench seven ways — as CSV, as
//! Parquet, as NDJSON, attached by a host as an Arrow stream from a file, through a pipe and through
//! a stream channel, and fetched by an exec connector —
//! each encoded the way that kind of source commonly encodes it: nanosecond timestamps and
//! unsigned integers in the Parquet, a dictionary-encoded string in the host's stream, zoned
//! millisecond timestamps in the connector's. The agent must not be able to tell them apart.
//!
//! The fixture is three meters for one day of *ElectricityLoadDiagrams20112014* (Artur
//! Trindade, UCI Machine Learning Repository, 2015, https://doi.org/10.24432/C58C86, CC BY 4.0),
//! cut from the eval's data as `eval/fetch_uci.py` makes it, with the quarter-hour of the day
//! added as an integer column.

mod common;

use arrow::array::{
    Array, ArrayRef, DictionaryArray, Float64Array, Int16Array, LargeStringArray, RecordBatch,
    TimestampMillisecondArray, TimestampSecondArray, UInt8Array,
};
use arrow::datatypes::{DataType, Field, Int16Type, Schema, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use benchy_mcp::proto::{read_frame, write_frame};
use common::Server;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/origin/readings.csv"
);

/// What the agent asks, on every bench. Each answer must be byte-identical across sources.
const QUERIES: &[&str] = &[
    "SELECT typeof(ts), typeof(meter), typeof(kw), typeof(quarter) FROM readings LIMIT 1",
    "SELECT * FROM readings ORDER BY ts, meter LIMIT 4",
    "SELECT meter, count(*), sum(kw), min(ts), max(ts), sum(quarter) FROM readings \
     GROUP BY ALL ORDER BY ALL",
    "SELECT time_bucket(INTERVAL 6 HOUR, ts) AS t, round(avg(kw), 6) FROM readings \
     GROUP BY ALL ORDER BY ALL",
    "SELECT ts - INTERVAL 1 DAY, date_trunc('month', ts), epoch(ts) FROM readings \
     WHERE quarter = 95 ORDER BY meter LIMIT 1",
    "SELECT column_name, data_type, is_nullable, column_default, ordinal_position \
     FROM information_schema.columns WHERE table_name = 'readings' ORDER BY ordinal_position",
];

fn unique(tag: &str) -> PathBuf {
    let d = common::unique(&format!("origin-{tag}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The fixture's rows: timestamp text, meter, kW, quarter-hour of the day.
fn rows(fixture: &Path) -> Vec<(String, String, f64, i64)> {
    std::fs::read_to_string(fixture)
        .unwrap()
        .lines()
        .skip(1)
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            (
                f[0].to_string(),
                f[1].to_string(),
                f[2].parse().unwrap(),
                f[3].parse().unwrap(),
            )
        })
        .collect()
}

/// Seconds since the epoch of a zone-less `YYYY-MM-DD HH:MM:SS`, read as UTC.
fn epoch_seconds(ts: &str) -> i64 {
    let n = |r: std::ops::Range<usize>| ts[r].parse::<i64>().unwrap();
    let (y, m, d) = (n(0..4), n(5..7), n(8..10));
    // Days from civil, after Howard Hinnant.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + n(11..13) * 3600 + n(14..16) * 60 + n(17..19)
}

fn write_stream(path: &Path, batch: &RecordBatch) {
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
        w.write(batch).unwrap();
        w.finish().unwrap();
    }
    std::fs::write(path, buf).unwrap();
}

/// The fixture as DuckDB writes Parquet and NDJSON: through `COPY`, with the Parquet's types
/// changed to ones a producer commonly picks — nanosecond timestamps, a small unsigned integer.
fn duckdb_copies(dir: &Path, fixture: &Path) -> (PathBuf, PathBuf) {
    let c = duckdb::Connection::open_in_memory().unwrap();
    let parquet = dir.join("readings.parquet");
    let ndjson = dir.join("readings.ndjson");
    c.execute_batch(&format!(
        "COPY (SELECT ts::TIMESTAMP_NS AS ts, meter, kw, quarter::USMALLINT AS quarter \
              FROM read_csv('{f}')) TO '{}' (FORMAT parquet); \
         COPY (SELECT * FROM read_csv('{f}')) TO '{}' (FORMAT json);",
        parquet.display(),
        ndjson.display(),
        f = fixture.display()
    ))
    .unwrap();
    let types: Vec<String> = {
        let mut st = c
            .prepare(&format!(
                "SELECT column_type FROM (DESCRIBE SELECT * FROM read_parquet('{}'))",
                parquet.display()
            ))
            .unwrap();
        let r = st.query_map([], |r| r.get(0)).unwrap();
        r.collect::<Result<_, _>>().unwrap()
    };
    assert_eq!(
        types,
        ["TIMESTAMP_NS", "VARCHAR", "DOUBLE", "USMALLINT"],
        "the Parquet carries the types this test is about"
    );
    (parquet, ndjson)
}

/// The fixture as a host might hand it over: second-resolution zone-less timestamps, a
/// dictionary-encoded meter, a 16-bit quarter.
fn host_stream(dir: &Path, fixture: &Path) -> PathBuf {
    let rows = rows(fixture);
    let meter: DictionaryArray<Int16Type> = rows.iter().map(|r| r.1.as_str()).collect();
    let schema = Schema::new(vec![
        Field::new("ts", DataType::Timestamp(TimeUnit::Second, None), false),
        Field::new("meter", meter.data_type().clone(), false),
        Field::new("kw", DataType::Float64, false),
        Field::new("quarter", DataType::Int16, false),
    ]);
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(TimestampSecondArray::from_iter_values(
                rows.iter().map(|r| epoch_seconds(&r.0)),
            )) as ArrayRef,
            Arc::new(meter) as ArrayRef,
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.2))) as ArrayRef,
            Arc::new(Int16Array::from_iter_values(
                rows.iter().map(|r| r.3 as i16),
            )) as ArrayRef,
        ],
    )
    .unwrap();
    let p = dir.join("readings.arrows");
    write_stream(&p, &batch);
    p
}

/// The fixture as a connector might send it: zoned millisecond timestamps, as ClickHouse sends a
/// `DateTime64(3, 'UTC')`, a large string, and an 8-bit unsigned quarter.
fn connector_stream(dir: &Path, fixture: &Path) -> PathBuf {
    let rows = rows(fixture);
    let schema = Schema::new(vec![
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new("meter", DataType::LargeUtf8, false),
        Field::new("kw", DataType::Float64, false),
        Field::new("quarter", DataType::UInt8, false),
    ]);
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(
                TimestampMillisecondArray::from_iter_values(
                    rows.iter().map(|r| epoch_seconds(&r.0) * 1000),
                )
                .with_timezone("UTC"),
            ) as ArrayRef,
            Arc::new(LargeStringArray::from_iter_values(
                rows.iter().map(|r| r.1.as_str()),
            )) as ArrayRef,
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.2))) as ArrayRef,
            Arc::new(UInt8Array::from_iter_values(rows.iter().map(|r| r.3 as u8))) as ArrayRef,
        ],
    )
    .unwrap();
    let p = dir.join("stream.arrows");
    write_stream(&p, &batch);
    p
}

/// An exec source named `readings` whose connector runs `body`, declared as an operator would
/// declare the fixture.
fn exec_config(dir: &Path, connector_name: &str, body: &str) -> PathBuf {
    let c = dir.join(connector_name);
    std::fs::write(&c, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"readings\"\ntype = \"exec\"\ncommand = \"./{connector_name}\"\n\
             [[source.schema]]\nname = \"ts\"\ntype = \"timestamptz\"\nnullable = false\n\
             [[source.schema]]\nname = \"meter\"\ntype = \"string\"\nnullable = false\n\
             [[source.schema]]\nname = \"kw\"\ntype = \"float64\"\n\
             [[source.schema]]\nname = \"quarter\"\ntype = \"int64\"\n"
        ),
    )
    .unwrap();
    cfg
}

/// A host session over `benchy-mcp host`, enough of one to attach and ask.
struct Host {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next: u64,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Host {
    fn start(args: &[&str]) -> Host {
        Host::start_speaking(args, &[1])
    }

    /// A session greeted offering the protocol versions `protocol`.
    fn start_speaking(args: &[&str], protocol: &[u64]) -> Host {
        let mut child = Command::new(common::BIN)
            .arg("host")
            .args(common::PLATFORM)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut h = Host {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            next: 0,
        };
        assert_eq!(h.call("hello", json!({ "protocol": protocol }))["ok"], true);
        h
    }

    fn call(&mut self, call: &str, fields: Value) -> Value {
        self.next += 1;
        let mut req = fields;
        req["id"] = self.next.into();
        req["call"] = call.into();
        write_frame(&mut self.stdin, &req).unwrap();
        read_frame(&mut self.stdout).expect("a reply")
    }
}

/// What the agent sees of a bench: `describe`, then each query's answer.
trait Bench {
    fn describe(&mut self) -> String;
    fn ask(&mut self, sql: &str) -> String;
}

impl Bench for Server {
    fn describe(&mut self) -> String {
        let v = self.call("tools/call", json!({"name": "describe", "arguments": {}}));
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn ask(&mut self, sql: &str) -> String {
        let (err, body, _) = self.query(sql);
        assert!(!err, "{sql}: {body}");
        without_result_ids(&body)
    }
}

impl Bench for Host {
    fn describe(&mut self) -> String {
        let v = self.call("describe", json!({}));
        v["text"].as_str().unwrap().to_string()
    }
    fn ask(&mut self, sql: &str) -> String {
        let v = self.call("query", json!({"sql": sql}));
        assert_eq!(v["ok"], true, "{sql}: {v}");
        without_result_ids(v["text"].as_str().unwrap())
    }
}

impl Bench for common::streamed::Streamer {
    fn describe(&mut self) -> String {
        let v = self.call("describe", json!({}), &[]);
        v["text"].as_str().unwrap().to_string()
    }
    fn ask(&mut self, sql: &str) -> String {
        let v = self.call("query", json!({"sql": sql}), &[]);
        assert_eq!(v["ok"], true, "{sql}: {v}");
        without_result_ids(v["text"].as_str().unwrap())
    }
}

impl Bench for common::streamed::Channeled {
    fn describe(&mut self) -> String {
        let v = self.call("describe", json!({}));
        v["text"].as_str().unwrap().to_string()
    }
    fn ask(&mut self, sql: &str) -> String {
        let v = self.call("query", json!({"sql": sql}));
        assert_eq!(v["ok"], true, "{sql}: {v}");
        without_result_ids(v["text"].as_str().unwrap())
    }
}

/// A result id starts with its session's own random prefix
/// (`two_sessions_never_issue_the_same_result_id`); replaced, so two sessions can be compared.
fn without_result_ids(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("result_id ") {
        out.push_str(&rest[..at + "result_id ".len()]);
        rest = &rest[at + "result_id ".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
            .unwrap_or(rest.len());
        out.push_str("ID");
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn everything(b: &mut dyn Bench) -> Vec<String> {
    let mut out = vec![b.describe()];
    out.extend(QUERIES.iter().map(|q| b.ask(q)));
    out
}

/// Every source this test can start without anything outside the repository, named.
fn every_source(dir: &Path, fixture: &Path) -> Vec<(&'static str, Box<dyn Bench>)> {
    let (parquet, ndjson) = duckdb_copies(dir, fixture);
    let mut out: Vec<(&'static str, Box<dyn Bench>)> = Vec::new();
    for (what, file) in [
        ("csv", fixture.to_path_buf()),
        ("parquet", parquet),
        ("ndjson", ndjson),
    ] {
        let source = format!("readings={}", file.display());
        out.push((what, Box::new(Server::launch(&["--source", &source], &[]))));
    }
    let mut host = Host::start(&[]);
    let attached = host.call(
        "attach",
        json!({"name": "readings", "path": host_stream(dir, fixture)}),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    out.push(("arrow attach", Box::new(host)));
    // The same stream, streamed through a pipe rather than read from a file (protocol 3).
    let mut streamed = common::streamed::Streamer::start(&[], &[1, 2, 3]);
    let (attached, _) = streamed.attach_stream(
        "readings",
        std::fs::read(host_stream(dir, fixture)).unwrap(),
    );
    assert_eq!(attached["ok"], true, "{attached}");
    out.push(("streamed attach", Box::new(streamed)));
    // And through a stream channel, the session's own inherited pipe (protocol 3).
    let mut channeled =
        common::streamed::Channeled::start(&["--stream-channel", "3"], &[1, 2, 3, 4, 5], json!({}));
    channeled
        .channel
        .feed(std::fs::read(host_stream(dir, fixture)).unwrap());
    let attached = channeled.call("attach", json!({"name": "readings", "stream": "channel"}));
    assert_eq!(attached["ok"], true, "{attached}");
    out.push(("channel attach", Box::new(channeled)));
    let stream = connector_stream(dir, fixture);
    let cfg = exec_config(dir, "connector", &format!("cat {}", stream.display()));
    out.push((
        "exec connector",
        Box::new(Server::launch(&["--config", cfg.to_str().unwrap()], &[])),
    ));
    out
}

/// Each source's answers against the first's, naming the first that differs.
fn assert_all_alike(seen: &[(&str, Vec<String>)]) {
    let (first, want) = &seen[0];
    for (what, got) in &seen[1..] {
        for (i, (w, g)) in want.iter().zip(got).enumerate() {
            let asked = if i == 0 { "describe" } else { QUERIES[i - 1] };
            assert_eq!(
                g, w,
                "{what} differs from {first} on: {asked}\n--- {what}:\n{g}\n--- {first}:\n{w}"
            );
        }
    }
}

#[test]
fn the_same_data_looks_the_same_from_every_source() {
    let dir = unique("same");
    let mut seen = Vec::new();
    for (what, mut bench) in every_source(&dir, Path::new(FIXTURE)) {
        seen.push((what, everything(bench.as_mut())));
    }
    assert_all_alike(&seen);
    // And what they agree on is the canonical form, not a common accident.
    let (_, csv) = &seen[0];
    assert!(
        csv[1].contains("TIMESTAMP WITH TIME ZONE")
            && csv[1].contains("VARCHAR")
            && csv[1].contains("DOUBLE")
            && csv[1].contains("BIGINT"),
        "{}",
        csv[1]
    );
    assert!(
        csv[2].contains("2014-01-01 00:00:00+00"),
        "a zone-less timestamp is read as UTC: {}",
        csv[2]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The catalog, as much of it as names a database, a setting, a table or a column.
const CATALOG: &[&str] = &[
    "SELECT database_name, path, comment, tags, internal, type, readonly FROM duckdb_databases() \
     ORDER BY ALL",
    "SELECT name, value FROM duckdb_settings() ORDER BY name",
    "SELECT database_name, schema_name, table_name, comment, tags, sql FROM duckdb_tables() \
     ORDER BY ALL",
    "SELECT database_name, table_name, column_name, comment, data_type, column_default \
     FROM duckdb_columns() WHERE NOT internal ORDER BY ALL",
    "SELECT catalog_name, schema_name, sql_path FROM information_schema.schemata ORDER BY ALL",
    "SELECT * FROM information_schema.tables ORDER BY ALL",
    "SELECT database_name, schema_name, comment, sql FROM duckdb_schemas() ORDER BY ALL",
    "SELECT database_name, view_name, sql FROM duckdb_views() WHERE NOT internal ORDER BY ALL",
    "SELECT current_database(), current_schema(), current_setting('search_path')",
];

/// A session's directory is random and its own; replaced, so two sessions can be compared.
fn without_session_dir(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("benchy-") {
        out.push_str(&rest[..at]);
        out.push_str("benchy-SESSION");
        rest = &rest[at + "benchy-".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn the_catalog_says_nothing_of_where_data_came_from() {
    let dir = unique("catalog");
    let stream = connector_stream(&dir, Path::new(FIXTURE));
    let file = dir.join("meters-from-the-lab.csv");
    std::fs::copy(FIXTURE, &file).unwrap();
    let cfg = exec_config(&dir, "zq7-feed", &format!("cat {}", stream.display()));
    let wide = [
        "--max-rows",
        "1000",
        "--max-bytes",
        "1000000",
        "--max-cell-bytes",
        "4096",
    ];
    let source = format!("readings={}", file.display());
    let mut dumps = Vec::new();
    for (what, args) in [
        ("file", vec!["--source", source.as_str()]),
        ("exec", vec!["--config", cfg.to_str().unwrap()]),
    ] {
        let mut args = args;
        args.extend(wide);
        let mut s = Server::launch(&args, &[]);
        let pid = s.pid().to_string();
        let dump: String = CATALOG.iter().map(|q| s.ask(q)).collect();
        for leak in [
            "meters-from-the-lab",
            ".csv",
            "zq7",
            "exec:",
            "benchy_attach",
            dir.to_str().unwrap(),
        ] {
            assert!(
                !dump.contains(leak),
                "{what}: the catalog says `{leak}`:\n{dump}"
            );
        }
        // A process id, anywhere, as a number of its own.
        let digits: Vec<&str> = dump
            .split(|c: char| !c.is_ascii_digit())
            .filter(|d| !d.is_empty())
            .collect();
        assert!(
            !digits.contains(&pid.as_str()),
            "{what}: the catalog carries the server's pid {pid}:\n{dump}"
        );
        dumps.push((what, without_session_dir(&dump)));
    }
    assert_eq!(
        dumps[0].1, dumps[1].1,
        "a file's catalog and a connector's differ"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the file a table is stored in shows of how it was written: its blocks, and the row
/// groups and segments of the table. The database is the one the table is in, by whatever name.
const LAYOUT: &[&str] = &[
    "SELECT database_size, block_size, total_blocks, used_blocks, free_blocks \
     FROM pragma_database_size() \
     WHERE database_name = (SELECT database_name FROM duckdb_tables() \
                            WHERE table_name = 'readings')",
    "SELECT count(DISTINCT row_group_id), count(*) FROM pragma_storage_info('readings')",
    "SELECT row_group_id, column_name, segment_type, compression, count \
     FROM pragma_storage_info('readings') ORDER BY ALL",
    // Where each segment lies, which the agent can read too. A parallel write gives the same
    // counts, row groups and segments from every source but puts them in blocks in the order its
    // threads happened to finish, which differs from one load to the next (docs/PERFORMANCE.md,
    // R4-1).
    "SELECT row_group_id, column_name, segment_type, block_id, block_offset \
     FROM pragma_storage_info('readings') ORDER BY ALL",
];

/// Rows enough for seven row groups, in the fixture's shape: 840,000 quarter-hours, as the eval's
/// data has, written as CSV in `dir`.
fn many_rows(dir: &Path) -> PathBuf {
    let fixture = dir.join("many.csv");
    let mut csv = String::from("ts,meter,kw,quarter\n");
    for i in 0..840_000i64 {
        let (day, q) = (i / 96, i % 96);
        csv.push_str(&format!(
            "2014-{:02}-{:02} {:02}:{:02}:00,MT_{:03},{}.25,{q}\n",
            1 + day / 28 % 12,
            1 + day % 28,
            q / 4,
            q % 4 * 15,
            i % 7,
            i % 1000
        ));
    }
    std::fs::write(&fixture, csv).unwrap();
    fixture
}

#[test]
fn the_physical_layout_says_nothing_of_where_data_came_from() {
    // `pragma_database_size()` and `pragma_storage_info` show how a table's file was written,
    // and the agent can call both. A CSV converted in place by `ALTER` left the replaced column's
    // blocks free in the file, and a parallel write left a different number of row groups for
    // each reader: on the eval's 840,960 meter rows, 8.4 MB in 9 row groups from CSV and
    // 12.3 MB with 16 free blocks in 8 from Parquet. Every source is now written the same way.
    let dir = unique("layout");
    let fixture = many_rows(&dir);
    let mut seen = Vec::new();
    for (what, mut bench) in every_source(&dir, &fixture) {
        seen.push((
            what,
            LAYOUT.iter().map(|q| bench.ask(q)).collect::<Vec<_>>(),
        ));
    }
    let (first, want) = &seen[0];
    for (what, got) in &seen[1..] {
        for (q, (w, g)) in LAYOUT.iter().zip(want.iter().zip(got)) {
            assert_eq!(g, w, "{what} differs from {first} on: {q}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A connector, declared `declared`, whose stream holds one column `v` of `array`, served.
fn serve_one_column(
    dir: &Path,
    declared: &str,
    array: ArrayRef,
    extra: &str,
) -> Result<Server, String> {
    let schema = Schema::new(vec![Field::new("v", array.data_type().clone(), true)]);
    let batch = RecordBatch::try_new(Arc::new(schema), vec![array]).unwrap();
    let stream = dir.join(format!("{declared}.arrows"));
    write_stream(&stream, &batch);
    let c = dir.join("one");
    std::fs::write(&c, format!("#!/bin/sh\ncat {}\n", stream.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = dir.join("one.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"t\"\ntype = \"exec\"\ncommand = \"./one\"\n{extra}\n\
             [[source.schema]]\nname = \"v\"\ntype = \"{declared}\"\n"
        ),
    )
    .unwrap();
    // A load that fails leaves nothing to serve, and the server says why on stderr and exits.
    let out = Command::new(common::BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .args(common::PLATFORM)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    if !out.status.success() && !said.contains("sealed") {
        return Err(said);
    }
    Ok(Server::launch(&["--config", cfg.to_str().unwrap()], &[]))
}

#[test]
fn a_stream_may_be_narrower_than_its_declaration_never_wider() {
    // The operator declares what a column holds; the connector may send any type every value of
    // which that holds exactly, and the bench holds the declared column's canonical type. A type
    // that could change a value is refused, never cast.
    use arrow::array::{
        Float32Array, Int32Array, Int64Array, StringViewArray, TimestampNanosecondArray,
        UInt64Array,
    };
    let dir = unique("widen");
    let ns = |v: Vec<i64>| -> ArrayRef {
        Arc::new(TimestampNanosecondArray::from(v).with_timezone("Europe/Copenhagen"))
    };
    for (declared, array, typed, value) in [
        (
            "int64",
            Arc::new(Int32Array::from(vec![-7])) as ArrayRef,
            "BIGINT",
            "-7",
        ),
        (
            "float64",
            Arc::new(Float32Array::from(vec![0.5])) as ArrayRef,
            "DOUBLE",
            "0.5",
        ),
        (
            "string",
            Arc::new(StringViewArray::from(vec!["lab"])) as ArrayRef,
            "VARCHAR",
            "lab",
        ),
        (
            "timestamptz",
            ns(vec![1_388_534_400_000_001_000]),
            "TIMESTAMP WITH TIME ZONE",
            "2014-01-01 00:00:00.000001+00",
        ),
    ] {
        let mut s = serve_one_column(&dir, declared, array, "")
            .unwrap_or_else(|e| panic!("{declared}: refused: {e}"));
        let got = s.ask("SELECT typeof(v), v::VARCHAR FROM t");
        assert!(
            got.contains(typed) && got.contains(value),
            "{declared}: {got}"
        );
    }
    for (declared, array, why) in [
        (
            "int32",
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            "64 bits where 32 are declared",
        ),
        (
            "int64",
            Arc::new(UInt64Array::from(vec![1])) as ArrayRef,
            "an unsigned 64-bit value can pass a signed one",
        ),
        (
            "int64",
            Arc::new(Float32Array::from(vec![1.0])) as ArrayRef,
            "a float for an integer",
        ),
        (
            "timestamptz",
            Arc::new(TimestampNanosecondArray::from(vec![
                1_388_534_400_000_000_000,
            ])) as ArrayRef,
            "a zone-less time for an instant",
        ),
        (
            "timestamptz",
            ns(vec![1_388_534_400_000_000_001]),
            "a nanosecond the bench cannot hold",
        ),
    ] {
        let said = match serve_one_column(&dir, declared, array, "") {
            Ok(_) => panic!("{declared}: loaded {why}"),
            Err(said) => said,
        };
        assert!(
            said.contains("`v`"),
            "{declared}, {why}: names the column: {said}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_zone_less_timestamp_is_utc_unless_its_source_names_a_zone() {
    // A zone-less timestamp is an instant only once a zone is known. UTC unless the operator
    // says otherwise, per source — meter data is commonly written in local time.
    let dir = unique("zone");
    let csv = dir.join("readings.csv");
    std::fs::write(&csv, "ts,kw\n2026-07-01 12:00:00,1.5\n").unwrap();
    let cfg = |zone: &str| {
        let p = dir.join(format!("z{}.toml", zone.len()));
        let line = if zone.is_empty() {
            String::new()
        } else {
            format!("timezone = \"{zone}\"\n")
        };
        std::fs::write(
            &p,
            format!(
                "[[source]]\nname = \"readings\"\ntype = \"file\"\npath = \"{}\"\n{line}",
                csv.display()
            ),
        )
        .unwrap();
        p
    };
    let at = |zone: &str| {
        let c = cfg(zone);
        let mut s = Server::launch(&["--config", c.to_str().unwrap()], &[]);
        s.ask("SELECT ts FROM readings")
    };
    assert!(at("").contains("2026-07-01 12:00:00+00"), "{}", at(""));
    let local = at("Europe/Copenhagen");
    assert!(local.contains("2026-07-01 10:00:00+00"), "{local}");

    // The same through a connector declaring `timestamp`.
    use arrow::array::TimestampMillisecondArray;
    let naive: ArrayRef = Arc::new(TimestampMillisecondArray::from(vec![1_782_907_200_000]));
    let mut s =
        serve_one_column(&dir, "timestamp", naive, "timezone = \"Europe/Copenhagen\"").unwrap();
    let got = s.ask("SELECT typeof(v), v FROM t");
    assert!(
        got.contains("TIMESTAMP WITH TIME ZONE") && got.contains("2026-07-01 10:00:00+00"),
        "{got}"
    );

    // A zone the engine does not know stops startup, rather than being read as UTC.
    let out = Command::new(common::BIN)
        .args(["--config", cfg("Europe/Atlantis").to_str().unwrap()])
        .args(common::PLATFORM)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("Europe/Atlantis"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ClickHouse's own `ArrowStream` encoder, through an exec connector: `clickhouse local` reading
/// the fixture, LZ4-compressed bodies and zoned `DateTime64` included, as a server sends them.
/// Needs the ClickHouse binary, named by `BENCHY_CLICKHOUSE_BIN` as for every ClickHouse test;
/// without it the test says it was skipped. CI's `clickhouse` job runs it.
#[test]
fn clickhouse_looks_the_same_as_a_file() {
    let Some(ch) = std::env::var_os("BENCHY_CLICKHOUSE_BIN") else {
        eprintln!("skipped: set BENCHY_CLICKHOUSE_BIN to a ClickHouse binary");
        return;
    };
    let ch = ch.to_string_lossy();
    let dir = unique("clickhouse");
    let cfg = exec_config(
        &dir,
        "clickhouse.sh",
        &format!(
            "exec {ch} local --query \"SELECT parseDateTime64BestEffort(ts, 3, 'UTC') AS ts, \
             meter, kw, toUInt8(quarter) AS quarter FROM file('{FIXTURE}', 'CSVWithNames', \
             'ts String, meter String, kw Float64, quarter UInt8') FORMAT ArrowStream\""
        ),
    );
    let mut csv = Server::launch(&["--source", &format!("readings={FIXTURE}")], &[]);
    let mut ch = Server::launch(&["--config", cfg.to_str().unwrap()], &[]);
    assert_all_alike(&[
        ("csv", everything(&mut csv)),
        ("clickhouse", everything(&mut ch)),
    ]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sixth way: the built-in ClickHouse source (REQ-16), reading the fixture from a ClickHouse
/// server table typed the way ClickHouse tables of meter data are — `DateTime`,
/// `LowCardinality(String)`, `UInt8` — none of them the CSV's.
#[cfg(target_os = "linux")]
#[test]
fn the_clickhouse_source_looks_the_same_as_a_file() {
    let Some(server) = common::clickhouse::server() else {
        return;
    };
    let db = server.database("t_origin");
    server.sql(&format!(
        "CREATE TABLE {db}.readings (ts DateTime('UTC'), meter LowCardinality(String), \
         kw Float64, quarter UInt8) ENGINE = MergeTree ORDER BY (meter, ts)"
    ));
    server.sql(&format!(
        "INSERT INTO {db}.readings FORMAT CSVWithNames\n{}",
        std::fs::read_to_string(FIXTURE).unwrap()
    ));
    let dir = unique("clickhouse-builtin");
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\n\
             tables = [\"readings\"]\n",
            server.url
        ),
    )
    .unwrap();
    let mut csv = Server::launch(&["--source", &format!("readings={FIXTURE}")], &[]);
    let mut ch = Server::launch(&["--config", cfg.to_str().unwrap()], &[]);
    assert_all_alike(&[
        ("csv", everything(&mut csv)),
        ("clickhouse source", everything(&mut ch)),
    ]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fixture served over HTTP (REQ-17) in each format the HTTP source reads, from a scripted
/// server: CSV, the Parquet and NDJSON `duckdb_copies` writes, the connector's Arrow stream, and
/// the NDJSON's rows as cursor-paged JSON, `page_rows` to a page. Each config's one source is the
/// URL, named `readings` as the fixture's file is. Launched by the caller, one at a time: five
/// servers of 840,000 rows at once are more than a small machine has.
fn http_configs(dir: &Path, fixture: &Path, page_rows: usize) -> Vec<(&'static str, PathBuf)> {
    let (parquet, ndjson) = duckdb_copies(dir, fixture);
    let arrow = connector_stream(dir, fixture);
    let lines: Vec<String> = std::fs::read_to_string(&ndjson)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let pages = lines.chunks(page_rows).count();
    for (i, chunk) in lines.chunks(page_rows).enumerate() {
        let next = if i + 1 < pages {
            format!("\"{}\"", i + 1)
        } else {
            "null".into()
        };
        std::fs::write(
            dir.join(format!("page-{i}.json")),
            format!(
                "{{\"data\": [{}], \"meta\": {{\"next_cursor\": {next}}}}}",
                chunk.join(",")
            ),
        )
        .unwrap();
    }
    drop(lines);
    let files = dir.to_path_buf();
    let csv = fixture.to_path_buf();
    // Read from disk at each request, so the test holds no body longer than it is sent.
    let server = common::http::FakeHttp::start(move |req| {
        use common::http::Reply;
        let file = match req.path() {
            "/readings.csv" => csv.clone(),
            "/readings.parquet" => parquet.clone(),
            "/readings.ndjson" => ndjson.clone(),
            "/readings.arrows" => arrow.clone(),
            "/pages" => files.join(format!(
                "page-{}.json",
                req.param("cursor").unwrap_or_else(|| "0".into())
            )),
            _ => return Reply::status(404),
        };
        Reply::ok(std::fs::read(file).unwrap()).chunked(256 * 1024)
    });
    let mut out = Vec::new();
    for (what, path, rest) in [
        ("http csv", "/readings.csv", "format = \"csv\"\n"),
        (
            "http parquet",
            "/readings.parquet",
            "format = \"parquet\"\n",
        ),
        ("http ndjson", "/readings.ndjson", "format = \"ndjson\"\n"),
        ("http arrow", "/readings.arrows", "format = \"arrow\"\n"),
        (
            "http json pages",
            "/pages",
            "format = \"json\"\n[source.pages]\nrows = \"data\"\nnext = \"cursor\"\n\
             cursor = \"meta.next_cursor\"\ncursor_param = \"cursor\"\n",
        ),
    ] {
        let cfg = dir.join(format!("{}.toml", what.replace(' ', "-")));
        std::fs::write(
            &cfg,
            format!(
                "[[source]]\nname = \"readings\"\ntype = \"http\"\nurl = \"{}{path}\"\n{rest}",
                server.url
            ),
        )
        .unwrap();
        out.push((what, cfg));
    }
    out
}

#[test]
fn every_http_format_loads_as_the_same_table_as_its_file() {
    // REQ-17 is held to REQ-15: the fixture fetched over HTTP, in every format, answers
    // `describe` and the six queries byte for byte as the same file does.
    let dir = unique("http-same");
    let mut file = Server::launch(&["--source", &format!("readings={FIXTURE}")], &[]);
    let mut seen = vec![("csv file", everything(&mut file))];
    drop(file);
    for (what, cfg) in http_configs(&dir, Path::new(FIXTURE), 100) {
        let mut s = Server::launch(&["--config", cfg.to_str().unwrap()], &[]);
        seen.push((what, everything(&mut s)));
    }
    assert_all_alike(&seen);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_http_table_has_the_physical_layout_of_a_file_table() {
    // `the_physical_layout_says_nothing_of_where_data_came_from`, for the HTTP source: 840,000
    // rows, the JSON pages 10,000 to a page, laid out as the same CSV file lays them out, and a
    // catalog that names nothing of the URL.
    let dir = unique("http-layout");
    let fixture = many_rows(&dir);
    let wide = [
        "--max-rows",
        "1000",
        "--max-bytes",
        "1000000",
        "--max-cell-bytes",
        "4096",
    ];
    let shown = |b: &mut dyn Bench| -> Vec<String> {
        let mut out: Vec<String> = LAYOUT.iter().map(|q| b.ask(q)).collect();
        out.extend(CATALOG.iter().map(|q| without_session_dir(&b.ask(q))));
        out
    };
    let source = format!("readings={}", fixture.display());
    let mut args = vec!["--source", source.as_str()];
    args.extend(wide);
    let mut file = Server::launch(&args, &[]);
    let want = shown(&mut file);
    drop(file);
    for (what, cfg) in http_configs(&dir, &fixture, 10_000) {
        let mut args = vec!["--config", cfg.to_str().unwrap()];
        args.extend(wide);
        let mut s = Server::launch(&args, &[]);
        let got = shown(&mut s);
        for (q, (w, g)) in LAYOUT.iter().chain(CATALOG).zip(want.iter().zip(&got)) {
            assert_eq!(g, w, "{what} differs from the csv file on: {q}");
        }
        assert!(
            !got.concat().contains("127.0.0.1"),
            "{what}: the catalog names the server"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stated_shape_looks_the_same_from_every_source() {
    // REQ-14 and REQ-15 together: the same data with the same stated shape — the time column and
    // the series key an operator's config names, or a host's attach (protocol 3) — gives the same
    // `describe`, which says what they are rather than what they look like, from a file, a host's
    // attach from a file or through a pipe, and a connector.
    let dir = unique("stated");
    let fixture = Path::new(FIXTURE);
    let stated = "time_column = \"ts\"\nseries = [\"meter\"]\n";
    let csv = dir.join("csv.toml");
    std::fs::write(
        &csv,
        format!(
            "[[source]]\nname = \"readings\"\ntype = \"file\"\npath = \"{}\"\n{stated}",
            fixture.display()
        ),
    )
    .unwrap();
    let stream = connector_stream(&dir, fixture);
    let exec = exec_config(&dir, "connector", &format!("cat {}", stream.display()));
    let text = std::fs::read_to_string(&exec).unwrap();
    let (head, schema) = text.split_once("[[source.schema]]").unwrap();
    std::fs::write(&exec, format!("{head}{stated}[[source.schema]]{schema}")).unwrap();
    let host_file = host_stream(&dir, fixture);
    let shape = json!({"time_column": "ts", "series": ["meter"]});

    let mut seen: Vec<(&str, Vec<String>)> = Vec::new();
    let mut server = Server::launch(&["--config", csv.to_str().unwrap()], &[]);
    seen.push(("csv", everything(&mut server)));
    let mut host = Host::start_speaking(&[], &[3]);
    let mut req = shape.clone();
    req["name"] = "readings".into();
    req["path"] = host_file.to_str().unwrap().into();
    let r = host.call("attach", req);
    assert_eq!(r["ok"], true, "{r}");
    seen.push(("arrow attach", everything(&mut host)));
    let mut streamed = common::streamed::Streamer::start(&[], &[3]);
    let mut req = shape.clone();
    req["name"] = "readings".into();
    req["stream"] = true.into();
    let (r, _) = streamed.attach_stream_with(req, std::fs::read(&host_file).unwrap());
    assert_eq!(r["ok"], true, "{r}");
    seen.push(("streamed attach", everything(&mut streamed)));
    let mut channeled =
        common::streamed::Channeled::start(&["--stream-channel", "3"], &[5], json!({}));
    channeled.channel.feed(std::fs::read(&host_file).unwrap());
    let mut req = shape.clone();
    req["name"] = "readings".into();
    req["stream"] = "channel".into();
    let r = channeled.call("attach", req);
    assert_eq!(r["ok"], true, "{r}");
    seen.push(("channel attach", everything(&mut channeled)));
    let mut connector = Server::launch(&["--config", exec.to_str().unwrap()], &[]);
    seen.push(("exec connector", everything(&mut connector)));
    assert_all_alike(&seen);
    let describe = &seen[0].1[0];
    assert!(
        describe.contains("time column ts:") && describe.contains("series key meter:"),
        "{describe}"
    );
    assert!(!describe.contains("looks like"), "{describe}");
    let _ = std::fs::remove_dir_all(&dir);
}
