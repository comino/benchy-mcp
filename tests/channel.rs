//! A stream channel (docs/HOST-PROTOCOL.md, protocol 5): the host starts `benchy-mcp host` with one
//! more inherited pipe, names it with `--stream-channel`, and writes Arrow streams into it back to
//! back; each `attach` with `"stream": "channel"` takes the next whole one. No descriptor travels
//! with a request, so the session's stdin can be a plain pipe, as a Node.js host's is.

mod common;

use arrow::array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use common::streamed::{Channeled, Streamer};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

const V5: &[u64] = &[1, 2, 3, 4, 5];
const CHANNEL: &[&str] = &["--stream-channel", "3"];

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

/// `batch` as an Arrow IPC stream of batches of `rows` rows each.
fn batched(batch: &RecordBatch, rows: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
        let mut at = 0;
        while at < batch.num_rows() {
            let n = rows.min(batch.num_rows() - at);
            w.write(&batch.slice(at, n)).unwrap();
            at += n;
        }
        w.finish().unwrap();
    }
    buf
}

fn code(reply: &Value) -> &str {
    reply["error"]["code"].as_str().unwrap_or("")
}

fn message(reply: &Value) -> &str {
    reply["error"]["message"].as_str().unwrap_or("")
}

fn text(reply: &Value) -> &str {
    reply["text"].as_str().unwrap_or_default()
}

fn channel(name: &str) -> Value {
    json!({"name": name, "stream": "channel"})
}

/// The first value of `meter` in `table`, which says which stream it came from.
fn first_meter(h: &mut Channeled, table: &str) -> String {
    let q = h.call(
        "query",
        json!({"sql": format!("SELECT min(meter) || '!' AS m FROM {table}")}),
    );
    assert_eq!(q["ok"], true, "{q}");
    let t = text(&q);
    t.split_whitespace()
        .find(|w| w.ends_with('!'))
        .unwrap_or("")
        .trim_end_matches('!')
        .to_string()
}

fn budget_left(h: &mut Channeled) -> (u64, u64) {
    let r = h.call("describe", json!({}));
    (
        r["budget"]["rows_left"].as_u64().unwrap(),
        r["budget"]["bytes_left"].as_u64().unwrap(),
    )
}

/// Every write the host queued so far went through whole: benchy read each stream to its end.
fn all_written(h: &Channeled, n: usize) {
    for i in 0..n {
        match h.channel.written(Duration::from_secs(20)) {
            Some(Ok(())) => {}
            other => panic!("write {i} of {n}: {other:?}"),
        }
    }
}

#[test]
fn a_host_streams_tables_back_to_back_over_one_channel() {
    // Plain pipes for the protocol, as a Node.js host has them, and the channel on descriptor 3.
    // The host writes its streams ahead of the requests, back to back; each attach takes the
    // next whole one, in the order asked, and costs the budget the stream's bytes.
    let dir = common::unique("channel");
    std::fs::create_dir_all(&dir).unwrap();
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    let (rows0, bytes0) = budget_left(&mut h);
    let a = stream(&readings(1000, "a"), true);
    let b = stream(&readings(7, "b"), true);
    let size = (a.len() + b.len()) as u64;
    h.channel.feed(a);
    h.channel.feed(b);
    let r = h.call("attach", channel("a"));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["rows"], 1000, "{r}");
    assert_eq!(r["columns"][0], json!(["id", "BIGINT"]), "{r}");
    let r = h.call("attach", channel("b"));
    assert_eq!(r["rows"], 7, "{r}");
    assert_eq!(first_meter(&mut h, "a"), "a-0");
    assert_eq!(first_meter(&mut h, "b"), "b-0");
    assert_eq!(budget_left(&mut h), (rows0 - 1007, bytes0 - size));
    all_written(&h, 2);

    // Several tables in one request: the channel's in the order of the list, a file between.
    let file = dir.join("f.arrows");
    std::fs::write(&file, stream(&readings(2, "f"), true)).unwrap();
    // The first larger than a pipe holds: read beside the second, the two would interleave.
    h.channel.feed(batched(&readings(100_000, "c"), 10_000));
    h.channel.feed(stream(&readings(4, "d"), true));
    let r = h.call(
        "attach",
        json!({"tables": [{"name": "c", "stream": "channel"},
                          {"name": "f", "path": file},
                          {"name": "d", "stream": "channel", "series": ["meter"]}]}),
    );
    let each = r["tables"].as_array().expect("one answer per table");
    assert_eq!(
        each.iter().map(|t| t["rows"].clone()).collect::<Vec<_>>(),
        [json!(100_000), json!(2), json!(4)],
        "{r}"
    );
    assert_eq!(first_meter(&mut h, "c"), "c-0");
    assert_eq!(first_meter(&mut h, "d"), "d-0");
    all_written(&h, 2);

    // A stream more than a pipe holds, read as it is written, and the one after it.
    h.channel.feed(stream(&readings(200_000, "big"), true));
    h.channel.feed(stream(&readings(3, "after"), true));
    let r = h.call("attach", channel("big"));
    assert_eq!(r["rows"], 200_000, "{r}");
    let r = h.call("attach", channel("after"));
    assert_eq!(r["rows"], 3, "{r}");
    assert_eq!(first_meter(&mut h, "after"), "after-0");

    // Replacing by the channel, as by file.
    h.channel.feed(stream(&readings(5, "again"), true));
    let r = h.call("attach", channel("a"));
    assert_eq!(r["rows"], 5, "{r}");
    assert_eq!(first_meter(&mut h, "a"), "again-0");
    all_written(&h, 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ping_is_answered_while_a_channel_attach_waits() {
    // The attach waits for the host to write; a ping does not wait for the attach. The channel's
    // read end is non-blocking, as an event loop's pipes often are: the loader still waits for
    // the host to write rather than finding nothing there and giving up.
    let mut h = Channeled::spawn_with(CHANNEL, true);
    assert_eq!(h.call("hello", json!({"protocol": V5}))["protocol"], 5);
    let attach = h.send("attach", channel("readings"));
    std::thread::sleep(Duration::from_millis(300));
    let asked = Instant::now();
    let ping = h.send("ping", json!({}));
    assert_eq!(h.recv(), json!({"id": ping, "ok": true}));
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    h.channel.feed(stream(&readings(4, "p"), true));
    let r = h.recv();
    assert_eq!(r["id"], attach, "{r}");
    assert_eq!(r["rows"], 4, "{r}");
}

#[test]
fn a_refused_stream_leaves_the_channel_at_the_next_one() {
    // A stream refused part-way is read on to its end-of-stream marker and dropped, so the next
    // attach takes the next stream, whatever refused this one: what the file form refuses, a
    // limit, a name, the count of tables. Nothing changes, and the host's writes all go through.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    h.channel.feed(stream(&readings(3, "good"), true));
    let r = h.call("attach", channel("readings"));
    assert_eq!(r["ok"], true, "{r}");

    let one = |schema: Schema, column: ArrayRef| {
        stream(
            &RecordBatch::try_new(Arc::new(schema), vec![column]).unwrap(),
            true,
        )
    };
    // Finer than a microsecond, which is refused rather than rounded; megabytes of it, more
    // than a pipe holds, refused at its schema.
    let nanos = one(
        Schema::new(vec![Field::new(
            "at",
            DataType::Timestamp(TimeUnit::Nanosecond, None),
            true,
        )]),
        Arc::new(arrow::array::TimestampNanosecondArray::from(
            (0..500_000).map(|i| i * 1000 + 1).collect::<Vec<_>>(),
        )),
    );
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
    let big = stream(&readings(100_000, "big"), true);
    let bad_shape = json!({"name": "readings", "stream": "channel", "time_column": "nowhere"});
    for (what, req, bytes, want, says) in [
        (
            "a type outside the vocabulary",
            channel("readings"),
            nanos,
            "E_DATA",
            "`at`",
        ),
        (
            "two columns DuckDB cannot tell apart",
            channel("fresh"),
            cased,
            "E_DATA",
            "twice",
        ),
        ("zstd bodies", channel("readings"), zstd, "E_DATA", "LZ4"),
        (
            "unpacked past the bound",
            channel("fresh"),
            unpacks,
            "E_LIMIT",
            "unpack",
        ),
        (
            "a stated column it does not have",
            bad_shape,
            big.clone(),
            "E_DATA",
            "nowhere",
        ),
        (
            "a name the agent could not type",
            channel("Readings"),
            big.clone(),
            "E_NAME",
            "Readings",
        ),
    ] {
        h.channel.feed(bytes);
        h.channel.feed(stream(&readings(2, "next"), true));
        let r = h.call("attach", req);
        assert_eq!(code(&r), want, "{what}: {r}");
        assert!(message(&r).contains(says), "{what}: {r}");
        assert!(r["error"].get("channel").is_none(), "{what}: {r}");
        let r = h.call("attach", channel("next"));
        assert_eq!(
            r["rows"], 2,
            "{what}: the next stream is the next attach's: {r}"
        );
        assert_eq!(first_meter(&mut h, "next"), "next-0", "{what}");
        all_written(&h, 2);
    }
    assert_eq!(
        first_meter(&mut h, "readings"),
        "good-0",
        "still the good data"
    );

    // What the session allows, lowered in `hello`: rows in one attachment, the budget, the count
    // of tables. A table refused before its loader would read it is read all the same.
    let mut h = Channeled::start(
        CHANNEL,
        V5,
        json!({"limits": {"attach_rows": 1000, "session_rows": 1500, "attachments": 2}}),
    );
    let refused = |h: &mut Channeled, what: &str, req: Value, bytes: Vec<u8>, says: &str| {
        h.channel.feed(bytes);
        h.channel.feed(stream(&readings(2, what), true));
        let r = h.call("attach", req);
        assert_eq!(code(&r), "E_LIMIT", "{what}: {r}");
        assert!(message(&r).contains(says), "{what}: {r}");
        assert!(r["error"].get("channel").is_none(), "{what}: {r}");
        let r = h.call("attach", channel("next"));
        assert_eq!(r["rows"], 2, "{what}: {r}");
        assert_eq!(first_meter(h, "next"), format!("{what}-0"));
        all_written(h, 2);
    };
    refused(
        &mut h,
        "rows",
        channel("one"),
        stream(&readings(1001, "x"), true),
        "1000 rows",
    );
    h.channel.feed(stream(&readings(1000, "one"), true));
    assert_eq!(h.call("attach", channel("one"))["rows"], 1000);
    all_written(&h, 1);
    // 1,002 of 1,500 rows spent: 600 more pass what is left, replacement or not.
    refused(
        &mut h,
        "budget",
        channel("one"),
        stream(&readings(600, "x"), true),
        "budget",
    );
    // Two tables on the bench: a third name is refused before a loader starts.
    refused(
        &mut h,
        "count",
        channel("third"),
        stream(&readings(5, "x"), true),
        "tables",
    );

    // Bytes past an attachment's: refused at the first byte past it, and the rest of the stream,
    // less than an attachment's bytes again, read and dropped.
    let mut h = Channeled::start(CHANNEL, V5, json!({"limits": {"attach_bytes": 1_000_000}}));
    // In batches, as a fetch arrives: the cap is passed part-way, with less than its bytes left.
    let wide = batched(&readings(20_000, &"w".repeat(50)), 1000);
    assert!(
        wide.len() > 1_000_000 && wide.len() < 2_000_000,
        "{}",
        wide.len()
    );
    refused(&mut h, "bytes", channel("one"), wide, "1000000 bytes");
}

#[test]
fn a_channel_that_loses_its_place_is_closed_and_says_so() {
    // Where benchy cannot find the end of a stream — the channel ended before its marker, its
    // messages cannot be followed, more is left of a refused one than an attachment may be, or
    // its load ran out of time — the channel's place is lost. It is closed, so the host's next
    // write fails; the answer that closed it says so, every channel attach after it is
    // `E_CHANNEL`, and the session goes on for everything else.
    let dir = common::unique("channel-closed");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f.arrows");
    std::fs::write(&file, stream(&readings(2, "f"), true)).unwrap();
    let still_serves = |h: &mut Channeled, what: &str| {
        let r = h.call("attach", channel("later"));
        assert_eq!(code(&r), "E_CHANNEL", "{what}: {r}");
        assert!(message(&r).contains("closed"), "{what}: {r}");
        let r = h.call(
            "attach",
            json!({"tables": [{"name": "later", "stream": "channel"},
                              {"name": "filed", "path": file}]}),
        );
        assert_eq!(r["tables"][0]["error"]["code"], "E_CHANNEL", "{what}: {r}");
        assert_eq!(r["tables"][1]["rows"], 2, "{what}: {r}");
        let q = h.call("query", json!({"sql": "SELECT count(*) AS n FROM filed"}));
        assert_eq!(q["ok"], true, "{what}: {q}");
        assert_eq!(h.call("ping", json!({}))["ok"], true, "{what}");
    };
    let closed = |r: &Value, what: &str, want: &str| {
        assert_eq!(code(r), want, "{what}: {r}");
        assert_eq!(r["error"]["channel"], "closed", "{what}: {r}");
        assert!(message(r).contains("channel is closed"), "{what}: {r}");
    };

    // The channel ends before the marker: the host closed its end mid-stream.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    h.channel.feed(stream(&readings(5, "cut"), false));
    h.channel.close();
    let r = h.call("attach", channel("cut"));
    closed(&r, "cut short", "E_DATA");
    assert!(message(&r).contains("end-of-stream"), "{r}");
    still_serves(&mut h, "cut short");

    // Not Arrow: nothing to follow. The first table of a list closes it; the second is told so.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    h.channel.feed(b"id,meter\n1,lab\n".to_vec());
    let r = h.call(
        "attach",
        json!({"tables": [{"name": "csv", "stream": "channel"},
                          {"name": "after", "stream": "channel"}]}),
    );
    closed(&r["tables"][0], "not Arrow", "E_DATA");
    assert_eq!(r["tables"][1]["error"]["code"], "E_CHANNEL", "{r}");
    all_written(&h, 1);
    // benchy has let go of its end, and nothing it started holds one: the host's write fails.
    h.channel.feed(stream(&readings(2, "x"), true));
    let w = h.channel.written(Duration::from_secs(20));
    assert!(
        matches!(&w, Some(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe),
        "{w:?}"
    );
    still_serves(&mut h, "not Arrow");

    // Refused with more of it left than an attachment may be: not read to its end.
    let mut h = Channeled::start(CHANNEL, V5, json!({"limits": {"attach_bytes": 100_000}}));
    let long = stream(&readings(20_000, "long"), true);
    assert!(long.len() > 300_000, "{}", long.len());
    h.channel.feed(long);
    let r = h.call("attach", channel("long"));
    closed(&r, "too long to drain", "E_LIMIT");
    let w = h.channel.written(Duration::from_secs(20));
    assert!(
        matches!(&w, Some(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe),
        "the host's writer is told: {w:?}"
    );
    still_serves(&mut h, "too long to drain");

    // A load that runs out of time stops part-way.
    let mut h = Channeled::start(
        &["--stream-channel", "3", "--load-timeout", "2"],
        V5,
        json!({}),
    );
    let half = stream(&readings(1000, "half"), true);
    h.channel.feed(half[..half.len() / 2].to_vec());
    let r = h.call("attach", channel("slow"));
    closed(&r, "out of time", "E_DEADLINE");
    still_serves(&mut h, "out of time");

    // A loader killed at its memory ceiling stops part-way too (Linux only, where the ceiling is
    // enforced: `a_loader_memory_ceiling_that_cannot_be_enforced_is_not_reported`). 300,000 rows
    // of 400-byte strings, 126 MB, take a loader to 145 MB.
    if cfg!(target_os = "linux") {
        let mut h = Channeled::start(
            &["--stream-channel", "3", "--loader-memory-ceiling", "90MB"],
            V5,
            json!({}),
        );
        h.channel
            .feed(batched(&readings(300_000, &"w".repeat(400)), 65_536));
        let r = h.call("attach", channel("wide"));
        closed(&r, "past the loader memory ceiling", "E_LIMIT");
        assert!(message(&r).contains("loader memory ceiling"), "{r}");
        still_serves(&mut h, "past the loader memory ceiling");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refused_request_that_names_the_channel_closes_it() {
    // A host may write ahead of its requests. A request refused whole reads nothing, so its
    // streams would stay in the channel and the next attach would take one of them under its own
    // name, `ok: true`: one table's rows named as another's. Whatever refuses the request, if it
    // names a table from the channel, the channel is closed and the refusal says so; the next
    // channel attach is `E_CHANNEL`, never the wrong stream.
    let dir = common::unique("channel-refused");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f.arrows");
    std::fs::write(&file, stream(&readings(2, "f"), true)).unwrap();
    let cases: Vec<(&str, &[u64], Value)> = vec![
        (
            "a name twice",
            V5,
            json!({"tables": [channel("meters"), channel("meters")]}),
        ),
        (
            "a field a table does not take",
            V5,
            json!({"tables": [channel("meters"), {"name": "x", "stream": "channel", "colour": 1}]}),
        ),
        (
            "a table that is not an object",
            V5,
            json!({"tables": [channel("meters"), "rooms"]}),
        ),
        (
            "a `stream` that is neither form",
            V5,
            json!({"tables": [channel("meters"), {"name": "x", "stream": "pipe"}]}),
        ),
        (
            "a field the call does not take",
            V5,
            json!({"name": "meters", "stream": "channel", "colour": 1}),
        ),
        (
            "one table's fields beside `tables`",
            V5,
            json!({"name": "meters", "stream": "channel", "tables": [channel("x")]}),
        ),
        (
            "more tables than a session holds",
            V5,
            json!({"tables": [channel("a"), channel("b"), channel("c")]}),
        ),
        (
            "a stated shape that is not one",
            V5,
            json!({"name": "meters", "stream": "channel", "series": []}),
        ),
        (
            "\"channel\" before protocol 5",
            &[1, 2, 3, 4],
            channel("meters"),
        ),
    ];
    for (what, protocol, req) in cases {
        let mut h = Channeled::start(CHANNEL, protocol, json!({"limits": {"attachments": 2}}));
        // Written ahead, as a host may.
        h.channel.feed(stream(&readings(50, "meters"), true));
        let r = h.call("attach", req);
        assert_eq!(code(&r), "E_REQUEST", "{what}: {r}");
        assert_eq!(r["error"]["channel"], "closed", "{what}: {r}");
        assert!(message(&r).contains("channel is closed"), "{what}: {r}");
        h.channel.feed(stream(&readings(2, "rooms"), true));
        let r = h.call("attach", channel("rooms"));
        assert_ne!(
            r["ok"], true,
            "{what}: another table's stream under this name: {r}"
        );
        if protocol == V5 {
            assert_eq!(code(&r), "E_CHANNEL", "{what}: {r}");
            // Everything but the channel still serves.
            let r = h.call("attach", json!({"name": "filed", "path": file}));
            assert_eq!(r["rows"], 2, "{what}: {r}");
        }
    }

    // As it reaches the frame: a key given twice, read leniently only to answer it.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    h.channel.feed(stream(&readings(50, "meters"), true));
    h.send_body(
        br#"{"id": 7, "call": "attach", "name": "meters", "name": "m", "stream": "channel"}"#,
    );
    let r = h.recv();
    assert_eq!(r["id"], 7, "{r}");
    assert_eq!(r["error"]["channel"], "closed", "a key twice: {r}");
    h.channel.feed(stream(&readings(2, "rooms"), true));
    assert_eq!(code(&h.call("attach", channel("rooms"))), "E_CHANNEL");

    // An id still in use: refused behind the request that holds it, which takes its own stream
    // first, so the close comes in order.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    h.send_body(br#"{"id": 9, "call": "attach", "name": "first", "stream": "channel"}"#);
    h.send_body(br#"{"id": 9, "call": "attach", "name": "meters", "stream": "channel"}"#);
    h.channel.feed(stream(&readings(3, "first"), true));
    h.channel.feed(stream(&readings(50, "meters"), true));
    let r = h.recv();
    assert_eq!(r["id"], 9, "{r}");
    assert_eq!(
        r["rows"], 3,
        "the request before the refusal reads its own stream: {r}"
    );
    let r = h.recv();
    assert_eq!(r["id"], Value::Null, "{r}");
    assert_eq!(r["error"]["channel"], "closed", "an id in use: {r}");
    assert_eq!(code(&h.call("attach", channel("rooms"))), "E_CHANNEL");
    assert_eq!(first_meter(&mut h, "first"), "first-0");

    // A refused request that names no table from the channel leaves it be.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    h.channel.feed(stream(&readings(2, "kept"), true));
    for req in [
        json!({"name": "filed", "path": file, "colour": 1}),
        json!({"tables": [{"name": "a", "path": file}, {"name": "a", "path": file}]}),
    ] {
        let r = h.call("attach", req);
        assert_eq!(code(&r), "E_REQUEST", "{r}");
        assert!(r["error"].get("channel").is_none(), "{r}");
    }
    let r = h.call("attach", channel("kept"));
    assert_eq!(r["rows"], 2, "{r}");
    assert_eq!(first_meter(&mut h, "kept"), "kept-0");

    // Nor is a channel claimed closed in a session that has none.
    let mut h = Channeled::start(&[], V5, json!({}));
    let r = h.call("attach", json!({"tables": [channel("a"), channel("a")]}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(r["error"].get("channel").is_none(), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stream_channel_is_one_the_operator_names() {
    // A descriptor the session happens to inherit is not a channel: only the one named with
    // `--stream-channel` is, checked at startup.
    let mut h = Channeled::start(&[], V5, json!({}));
    h.channel.feed(stream(&readings(2, "stray"), true));
    let r = h.call("attach", channel("stray"));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(message(&r).contains("--stream-channel"), "{r}");
    let r = h.call("describe", json!({}));
    assert!(r["tables"].as_array().unwrap().is_empty(), "{r}");

    // `stream` is `true` or "channel"; "channel" came in protocol 5.
    let mut h = Channeled::start(CHANNEL, V5, json!({}));
    let r = h.call("attach", json!({"name": "a", "stream": "pipe"}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    let r = h.call(
        "attach",
        json!({"name": "a", "stream": "channel", "path": "/x"}),
    );
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    let mut v4 = Channeled::start(CHANNEL, &[1, 2, 3, 4], json!({}));
    let r = v4.call("attach", channel("a"));
    assert_eq!(code(&r), "E_REQUEST", "{r}");

    // What `--stream-channel` names must be there, a pipe, a socket or a file, and not stdio.
    let start = |args: &[&str], fd3: Option<std::os::fd::OwnedFd>| {
        let mut cmd = std::process::Command::new(common::BIN);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if let Some(fd) = &fd3 {
            common::streamed::inherit_as(&mut cmd, fd, 3);
        } else {
            // "No descriptor 3" has to be made, not assumed: under cargo-mutants the test
            // process inherits cargo's jobserver pipe, and the binary took that as a channel and
            // started, which failed the weekly run's baseline before any mutant was tested.
            use std::os::unix::process::CommandExt;
            // SAFETY: close is async-signal-safe; EBADF, when there is no descriptor 3, is fine.
            unsafe {
                cmd.pre_exec(|| {
                    libc::close(3);
                    Ok(())
                });
            }
        }
        cmd.output().unwrap()
    };
    let dir = std::fs::File::open(std::env::temp_dir()).unwrap();
    for (what, args, fd3) in [
        (
            "no descriptor 3",
            vec!["host", "--stream-channel", "3"],
            None,
        ),
        ("stdin", vec!["host", "--stream-channel", "0"], None),
        ("not a number", vec!["host", "--stream-channel", "x"], None),
        (
            "a directory",
            vec!["host", "--stream-channel", "3"],
            Some(dir.try_clone().unwrap().into()),
        ),
        (
            "not a host",
            vec!["--stream-channel", "3", "/nowhere"],
            None,
        ),
    ] {
        let mut args = args;
        args.extend(common::PLATFORM);
        let out = start(&args, fd3);
        assert!(!out.status.success(), "{what}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("--stream-channel"), "{what}: {err}");
    }
}

#[test]
fn a_channel_and_a_descriptor_serve_one_session() {
    // Both streamed forms in one session, and in one request: a descriptor with the request for
    // one table, the channel for the next.
    use std::io::Write;
    let (mut s, mut chan) = Streamer::spawn_with_channel(CHANNEL);
    assert_eq!(s.call("hello", json!({"protocol": V5}), &[])["protocol"], 5);
    let (r, _) = s.attach_stream("piped", stream(&readings(3, "p"), true));
    assert_eq!(r["rows"], 3, "{r}");
    chan.write_all(&stream(&readings(4, "c"), true)).unwrap();
    let r = s.call("attach", channel("chan"), &[]);
    assert_eq!(r["rows"], 4, "{r}");
    chan.write_all(&stream(&readings(5, "c2"), true)).unwrap();
    let (r, _) = s.attach_stream_with(
        json!({"tables": [{"name": "both_p", "stream": true},
                          {"name": "both_c", "stream": "channel"}]}),
        stream(&readings(6, "p2"), true),
    );
    assert_eq!(r["tables"][0]["rows"], 6, "{r}");
    assert_eq!(r["tables"][1]["rows"], 5, "{r}");
}
