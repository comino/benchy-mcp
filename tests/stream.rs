//! A streamed attach (docs/HOST-PROTOCOL.md, protocol 3): the host passes the read end of a pipe
//! with its `attach`, over the Unix socket that is the session's stdin, and writes the Arrow
//! stream into the other end. Nothing is spooled to a file on either side, and every rule the
//! file form has holds.

mod common;

use arrow::array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use common::streamed::{pipe, Streamer};
use serde_json::{json, Value};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const V3: &[u64] = &[1, 2, 3];

fn readings(rows: i64, tag: &str) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("meter", DataType::Utf8, false),
        Field::new("power_kw", DataType::Float64, true),
    ]);
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())) as ArrayRef,
            Arc::new(StringArray::from(
                (0..rows).map(|i| format!("{tag}-{i}")).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(Float64Array::from(
                (0..rows).map(|i| Some(i as f64)).collect::<Vec<_>>(),
            )) as ArrayRef,
        ],
    )
    .unwrap()
}

/// `batch` as an Arrow IPC stream; without its end-of-stream marker when `finish` is false.
fn stream(batch: &RecordBatch, finish: bool) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
        w.write(batch).unwrap();
        if finish {
            w.finish().unwrap();
        }
    }
    buf
}

fn code(reply: &Value) -> &str {
    reply["error"]["code"].as_str().unwrap_or("")
}

fn text(reply: &Value) -> &str {
    reply["text"].as_str().unwrap_or_default()
}

fn unique(tag: &str) -> PathBuf {
    let d = common::unique(tag);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn budget_left(s: &mut Streamer) -> (u64, u64) {
    let r = s.call("describe", json!({}), &[]);
    (
        r["budget"]["rows_left"].as_u64().unwrap(),
        r["budget"]["bytes_left"].as_u64().unwrap(),
    )
}

#[test]
fn a_host_streams_a_table_without_a_file() {
    // Protocol 3: the host hands benchy the read end of a pipe, and writes the stream into it as
    // it fetches. The table is the one the file form makes, and costs the budget what its file
    // would: the stream's bytes, as read.
    let dir = unique("stream-attach");
    let mut s = Streamer::start(&[], V3);
    let (rows0, bytes0) = budget_left(&mut s);
    let bytes = stream(&readings(1000, "s"), true);
    let size = bytes.len() as u64;
    let (r, finished) = s.attach_stream("readings", bytes.clone());
    assert_eq!(r["ok"], true, "{r}");
    assert!(finished);
    assert_eq!(r["rows"], 1000, "{r}");
    assert_eq!(r["columns"][0], json!(["id", "BIGINT"]), "{r}");
    let q = s.call(
        "query",
        json!({"sql": "SELECT sum(id) + 1 AS n, min(meter) || '!' AS m FROM readings"}),
        &[],
    );
    assert!(
        text(&q).contains("499501") && text(&q).contains("s-0!"),
        "{q}"
    );
    assert_eq!(budget_left(&mut s), (rows0 - 1000, bytes0 - size));

    // The file form beside it, in one request: a stream and a file, one descriptor.
    let file = dir.join("f.arrows");
    std::fs::write(&file, stream(&readings(3, "f"), true)).unwrap();
    let (r, finished) = s.attach_stream_with(
        json!({"tables": [{"name": "streamed", "stream": true},
                          {"name": "filed", "path": file}]}),
        stream(&readings(2, "t"), true),
    );
    assert!(finished);
    let each = r["tables"].as_array().expect("one answer per table");
    assert_eq!(each[0]["ok"], true, "{r}");
    assert_eq!(each[0]["rows"], 2, "{r}");
    assert_eq!(each[1]["ok"], true, "{r}");
    assert_eq!(each[1]["rows"], 3, "{r}");

    // Replacing by stream, as by file.
    let (r, _) = s.attach_stream("readings", stream(&readings(5, "again"), true));
    assert_eq!(r["rows"], 5, "{r}");
    let q = s.call(
        "query",
        json!({"sql": "SELECT min(meter) || '!' AS m FROM readings"}),
        &[],
    );
    assert!(text(&q).contains("again-0!"), "{q}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ping_is_answered_while_a_stream_is_attached() {
    // The attach waits for the host to write; a ping does not wait for the attach.
    use std::io::Write;
    let mut s = Streamer::start(&[], V3);
    let (read, mut write) = pipe();
    // Non-blocking, as an event loop's pipes are: the loader still waits for the host to write
    // rather than finding nothing there and giving up.
    unsafe {
        let flags = libc::fcntl(read.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let attach = s.send(
        "attach",
        json!({"name": "readings", "stream": true}),
        &[read.as_raw_fd()],
    );
    drop(read);
    std::thread::sleep(Duration::from_millis(300));
    let asked = Instant::now();
    let ping = s.send("ping", json!({}), &[]);
    let pong = s.recv();
    assert_eq!(pong, json!({"id": ping, "ok": true}));
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    write.write_all(&stream(&readings(4, "p"), true)).unwrap();
    drop(write);
    let r = s.recv();
    assert_eq!(r["id"], attach, "{r}");
    assert_eq!(r["rows"], 4, "{r}");
}

#[test]
fn a_streamed_attach_keeps_every_rule_of_the_file_form() {
    // What the file form refuses, a stream is refused for too, with the same code, and nothing
    // changes: the table already there keeps serving, and the host's writer is told the reader
    // has gone rather than left blocked on a pipe nobody reads.
    let mut s = Streamer::start(&[], V3);
    let (r, _) = s.attach_stream("readings", stream(&readings(3, "good"), true));
    assert_eq!(r["ok"], true, "{r}");

    let one = |schema: Schema, column: ArrayRef| {
        stream(
            &RecordBatch::try_new(Arc::new(schema), vec![column]).unwrap(),
            true,
        )
    };
    let nanos = |rows: i64| {
        one(
            Schema::new(vec![Field::new(
                "at",
                DataType::Timestamp(TimeUnit::Nanosecond, None),
                true,
            )]),
            // Finer than a microsecond, which is refused rather than rounded.
            Arc::new(arrow::array::TimestampNanosecondArray::from(
                (0..rows).map(|i| i * 1000 + 1).collect::<Vec<_>>(),
            )),
        )
    };
    let cased = {
        let schema = Schema::new(vec![
            Field::new("Value", DataType::Int64, true),
            Field::new("value", DataType::Int64, true),
        ]);
        stream(
            &RecordBatch::try_new(
                Arc::new(schema),
                vec![
                    Arc::new(Int64Array::from(vec![1])) as ArrayRef,
                    Arc::new(Int64Array::from(vec![2])) as ArrayRef,
                ],
            )
            .unwrap(),
            true,
        )
    };
    let mut two = stream(&readings(3, "first"), true);
    two.extend(stream(&readings(5, "second"), true));
    let zstd = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/zstd.arrows"),
    )
    .unwrap();
    // 70,000 keys into one 4 KiB value: 300 KB on the wire, 287 MB unpacked.
    let unpacks = {
        use arrow::array::{Array, DictionaryArray, Int32Array};
        let dict = DictionaryArray::try_new(
            Int32Array::from(vec![0; 70_000]),
            Arc::new(StringArray::from(vec!["x".repeat(4096)])),
        )
        .unwrap();
        one(
            Schema::new(vec![Field::new("meter", dict.data_type().clone(), false)]),
            Arc::new(dict),
        )
    };
    // A large stream cut short: the writer is still writing when the reader stops.
    let big_cut = stream(&readings(200_000, "big"), false);
    for (what, bytes, want, says) in [
        (
            "a stream cut short",
            stream(&readings(5, "cut"), false),
            "E_DATA",
            "end-of-stream",
        ),
        (
            "a large stream cut short",
            big_cut,
            "E_DATA",
            "end-of-stream",
        ),
        (
            "a stream with another after it",
            two,
            "E_DATA",
            "after its end-of-stream marker",
        ),
        (
            "not Arrow at all",
            b"id,meter\n1,lab\n".to_vec(),
            "E_DATA",
            "Arrow",
        ),
        ("nothing at all", Vec::new(), "E_DATA", "Arrow"),
        ("a type outside the vocabulary", nanos(1), "E_DATA", "`at`"),
        // Refused at its schema, with megabytes still to write: more than a pipe holds.
        (
            "a large stream of a type outside the vocabulary",
            nanos(500_000),
            "E_DATA",
            "`at`",
        ),
        (
            "two columns DuckDB cannot tell apart",
            cased,
            "E_DATA",
            "twice",
        ),
        ("zstd bodies", zstd, "E_DATA", "LZ4"),
        ("unpacked past the bound", unpacks, "E_LIMIT", "unpack"),
    ] {
        for name in ["readings", "fresh"] {
            let (r, finished) = s.attach_stream(name, bytes.clone());
            assert_eq!(code(&r), want, "{what}: {r}");
            let message = r["error"]["message"].as_str().unwrap();
            assert!(message.contains(says), "{what}: {message}");
            assert!(finished, "{what}: the host's writer was left blocked");
        }
    }
    let q = s.call(
        "query",
        json!({"sql": "SELECT min(meter) || '!' AS m, count(*) AS n FROM readings"}),
        &[],
    );
    assert!(text(&q).contains("good-0!"), "still the good data: {q}");
    let r = s.call("describe", json!({}), &[]);
    assert_eq!(r["tables"].as_array().unwrap().len(), 1, "{r}");
}

#[test]
fn a_descriptor_is_taken_only_by_a_streamed_attach() {
    // One descriptor per streamed table, with the request that names it, and only there. What
    // does not add up is refused whole, and every descriptor that came with it is closed.
    use std::io::Write;
    // Bounded: a descriptor taken by mistake would wait for a writer that never comes.
    let mut s = Streamer::start(&["--load-timeout", "5"], V3);
    // From before the pipe is made until its writer is told: no other test of this binary starts
    // a process meanwhile, which would hold a copy of the read end until it execs (#48,
    // `common::no_forks`). Every process start in this binary goes through `common::spawn`.
    // Without that, a process not yet exec'd held a copy of the read end, and the write that
    // proves the descriptor closed went through now and then.
    let quiet = common::no_forks();
    let (read, mut write) = pipe();
    let r = s.call("ping", json!({}), &[read.as_raw_fd()]);
    assert_eq!(code(&r), "E_REQUEST", "a ping takes no descriptor: {r}");
    drop(read);
    assert!(
        write.write_all(b"x").is_err(),
        "the descriptor it brought was closed"
    );
    drop(quiet);
    let r = s.call("attach", json!({"name": "a", "stream": true}), &[]);
    assert_eq!(code(&r), "E_REQUEST", "a stream needs its descriptor: {r}");
    let (read, _write) = pipe();
    let (read2, _write2) = pipe();
    let r = s.call(
        "attach",
        json!({"name": "a", "stream": true}),
        &[read.as_raw_fd(), read2.as_raw_fd()],
    );
    assert_eq!(code(&r), "E_REQUEST", "one descriptor a stream: {r}");
    let r = s.call(
        "attach",
        json!({"name": "a", "stream": true, "path": "/x.arrows"}),
        &[read.as_raw_fd()],
    );
    assert_eq!(code(&r), "E_REQUEST", "a stream or a path: {r}");
    let r = s.call("attach", json!({"name": "a", "stream": false}), &[]);
    assert_eq!(code(&r), "E_REQUEST", "`stream` is true or absent: {r}");
    // A descriptor that is not a stream: a directory.
    let d = std::fs::File::open(std::env::temp_dir()).unwrap();
    let r = s.call(
        "attach",
        json!({"name": "a", "stream": true}),
        &[d.as_raw_fd()],
    );
    assert_eq!(code(&r), "E_DATA", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a pipe"),
        "says what it takes: {r}"
    );
    let r = s.call("describe", json!({}), &[]);
    assert!(r["tables"].as_array().unwrap().is_empty(), "{r}");

    // Protocol 2 has no `stream`.
    let mut v2 = Streamer::start(&[], &[1, 2]);
    let (read, _write) = pipe();
    let r = v2.call(
        "attach",
        json!({"name": "a", "stream": true}),
        &[read.as_raw_fd()],
    );
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("stream"),
        "{r}"
    );
}

#[test]
fn a_stream_needs_a_socket_to_come_over() {
    // On pipes there is no way to pass a descriptor: a streamed attach is refused, saying so.
    use benchy_mcp::proto::{read_frame, write_frame};
    use std::process::{Command, Stdio};
    let mut child = common::spawn(
        Command::new(common::BIN)
            .arg("host")
            .args(common::PLATFORM)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    write_frame(
        &mut stdin,
        &json!({"id": 1, "call": "hello", "protocol": V3}),
    )
    .unwrap();
    let hello: Value = read_frame(&mut stdout).unwrap();
    assert_eq!(hello["protocol"], 3, "{hello}");
    write_frame(
        &mut stdin,
        &json!({"id": 2, "call": "attach", "name": "a", "stream": true}),
    )
    .unwrap();
    let r: Value = read_frame(&mut stdout).unwrap();
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("socket"),
        "{r}"
    );
    let _ = child.kill();
    let _ = child.wait();
}
