//! The host protocol, `benchy-host/1` to `/5` (docs/HOST-PROTOCOL.md; the streamed attach of 3 is
//! in tests/stream.rs, the stream channel of 5 in tests/channel.rs, and the tool names of 4 in
//! tests/answers.rs), driven the way a host drives it: frames on the stdio of `benchy-mcp host`.

mod common;

use arrow::array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use benchy_mcp::proto::{read_frame, write_frame};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// A session, not yet greeted.
    fn spawn(args: &[&str]) -> Host {
        Host::spawn_with(args, Stdio::null(), false)
    }

    /// A session, not yet greeted, writing its stage timings to `log` (src/trace.rs).
    fn spawn_traced(args: &[&str], log: std::fs::File) -> Host {
        Host::spawn_with(args, Stdio::from(log), true)
    }

    fn spawn_with(args: &[&str], stderr: Stdio, traced: bool) -> Host {
        let mut cmd = Command::new(common::BIN);
        if traced {
            cmd.env("BENCHY_TRACE_TIMINGS", "1");
        }
        let mut child = cmd
            .arg("host")
            .args(common::PLATFORM)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .unwrap();
        Host {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            next: 100,
        }
    }

    /// A session, greeted, speaking protocol 1.
    fn start(args: &[&str]) -> Host {
        let mut h = Host::spawn(args);
        let hello = h.call("hello", json!({"protocol": [1]}));
        assert_eq!(hello["ok"], true, "{hello}");
        h
    }

    /// A session, greeted, speaking protocol 2.
    fn start_v2(args: &[&str]) -> Host {
        let mut h = Host::spawn(args);
        let hello = h.call("hello", json!({"protocol": [1, 2]}));
        assert_eq!(hello["protocol"], 2, "{hello}");
        h
    }

    fn send(&mut self, v: &Value) {
        write_frame(&mut self.stdin, v).unwrap();
    }

    /// A frame of exactly these bytes, for what a `Value` cannot hold — a key given twice.
    fn send_raw(&mut self, json: &str) {
        use std::io::Write;
        self.stdin
            .write_all(&(json.len() as u32).to_be_bytes())
            .unwrap();
        self.stdin.write_all(json.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        read_frame(&mut self.stdout).expect("a reply")
    }

    /// Whether the session has ended: the process exits within a few seconds. Bounded, so a
    /// session that goes on fails the test instead of hanging it.
    fn ended(&mut self) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if self.child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn call(&mut self, call: &str, fields: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let mut req = fields;
        req["id"] = id.into();
        req["call"] = call.into();
        self.send(&req);
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }
}

fn unique(tag: &str) -> PathBuf {
    let d = common::unique(tag);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn readings(rows: i64, tag: &str) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("meter", DataType::Utf8, false),
        Field::new("power_kw", DataType::Float64, true),
    ]);
    let ids: Vec<i64> = (0..rows).collect();
    let meters: Vec<String> = (0..rows).map(|i| format!("{tag}-{i}")).collect();
    let power: Vec<Option<f64>> = (0..rows).map(|i| Some(i as f64)).collect();
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int64Array::from(ids)) as ArrayRef,
            Arc::new(StringArray::from(meters)) as ArrayRef,
            Arc::new(Float64Array::from(power)) as ArrayRef,
        ],
    )
    .unwrap()
}

/// An Arrow IPC stream holding `batch`; without its end-of-stream marker when `finish` is false.
fn arrows(path: &Path, batch: &RecordBatch, finish: bool) -> PathBuf {
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
        w.write(batch).unwrap();
        if finish {
            w.finish().unwrap();
        }
    }
    std::fs::write(path, buf).unwrap();
    path.to_path_buf()
}

fn attach(h: &mut Host, name: &str, path: &Path) -> Value {
    h.call("attach", json!({"name": name, "path": path}))
}

fn code(reply: &Value) -> &str {
    reply["error"]["code"].as_str().unwrap_or("")
}

#[test]
fn a_host_attaches_a_table_during_a_session() {
    // REQ-4. The bench starts empty and sealed; data arrives whenever the host has it.
    let dir = unique("host-attach");
    let mut h = Host::start(&[]);
    let r = h.call("query", json!({"sql": "SELECT 41 + 1 AS answer"}));
    assert!(
        r["ok"] == true && r["text"].as_str().unwrap().contains("42"),
        "{r}"
    );
    h.call(
        "query",
        json!({"sql": "CREATE TABLE mine AS SELECT 7 AS n"}),
    );

    let r = attach(
        &mut h,
        "readings",
        &arrows(&dir.join("a.arrows"), &readings(3, "a"), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["rows"], 3);
    assert_eq!(r["columns"][0], json!(["id", "BIGINT"]), "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 500 AS n FROM readings"}),
    );
    assert!(r["text"].as_str().unwrap().contains("503"), "{r}");

    let r = attach(
        &mut h,
        "later",
        &arrows(&dir.join("b.arrows"), &readings(2, "b"), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT (SELECT count(*) FROM later) * 100 + (SELECT n FROM mine) AS n"}),
    );
    assert!(
        r["text"].as_str().unwrap().contains("207"),
        "both, and the agent's own: {r}"
    );

    let r = h.call("describe", json!({}));
    let names: Vec<&str> = r["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["readings", "later"], "{r}");
    let text = r["text"].as_str().unwrap();
    assert!(
        text.contains("readings") && text.contains("later"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_host_protocol_refuses_what_it_does_not_know() {
    // Requests are strict: a field benchy does not know is an error, never ignored, so a host
    // that writes `max_row` is told rather than served with the full caps.
    let mut h = Host::start(&["--max-rows", "50"]);
    let r = h.call("frobnicate", json!({}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    let r = h.call("query", json!({"sql": "SELECT 1", "max_row": 5}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("max_row"),
        "{r}"
    );
    let r = h.call(
        "query",
        json!({"sql": "SELECT 1", "caps": {"max_rows": 51}}),
    );
    assert_eq!(code(&r), "E_REQUEST", "a cap is never raised: {r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT 1", "caps": {"max_bytes": 100}}),
    );
    assert_eq!(
        code(&r),
        "E_REQUEST",
        "nor lowered past what can say it cut: {r}"
    );
    let r = h.call("query", json!({"sql": "SELECT 1", "caps": {"max_rows": 5}}));
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["caps"]["max_rows"], 5, "the caps that applied: {r}");
    let r = h.call("query", json!({"sql": 7}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    let r = h.call("hello", json!({"protocol": [1]}));
    assert_eq!(code(&r), "E_REQUEST", "hello comes once: {r}");
    h.send(&json!({"call": "ping"}));
    let r = h.recv();
    assert!(r["id"].is_null() && code(&r) == "E_REQUEST", "{r}");

    // A session that does not open with hello ends there.
    let mut h = Host::spawn(&[]);
    let r = h.call("query", json!({"sql": "SELECT 1"}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(h.ended(), "the session ended");
    // Nor does one whose hello offers no version in common.
    let mut h = Host::spawn(&[]);
    let r = h.call("hello", json!({"protocol": [9]}));
    assert_eq!(code(&r), "E_VERSION", "{r}");
    assert!(h.ended(), "the session ended");
}

#[test]
fn hello_says_what_the_session_is() {
    let mut h = Host::spawn(&["--max-rows", "50", "--deadline", "7"]);
    let r = h.call("hello", json!({"protocol": [7, 1]}));
    assert_eq!(r["protocol"], 1, "{r}");
    let mut v3 = Host::spawn(&[]);
    let r3 = v3.call("hello", json!({"protocol": [1, 2, 3, 4, 5, 6, 7]}));
    assert_eq!(r3["protocol"], 6, "the newest version both speak: {r3}");
    assert!(r["engine"].as_str().unwrap().starts_with("DuckDB v"), "{r}");
    assert_eq!(r["caps"]["max_rows"], 50, "{r}");
    assert_eq!(r["caps"]["deadline_secs"], 7, "{r}");
    assert_eq!(r["limits"]["attachments"], 64, "{r}");
    assert!(r["sandbox"].is_string(), "{r}");
}

// Linux only: the memory ceiling is read from /proc, and elsewhere it is not enforced
// (`a_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
#[test]
#[cfg(target_os = "linux")]
fn a_host_may_lower_the_memory_ceiling_but_never_raise_it() {
    // The operator sets the ceiling on the engine's resident memory, on the command line or in
    // the config, never both. In protocol 3 `hello`'s `limits` may lower it, as any limit, never
    // raise it, and never below DuckDB's own budget. Anything else ends the session, as any
    // `hello` the session cannot open with does.
    const MB: u64 = 1_000_000;
    let args = [
        "--memory",
        "64MB",
        "--memory-ceiling",
        "1GB",
        "--deadline",
        "60",
    ];
    for (offer, ceiling, why) in [
        (json!([1, 2, 3]), 2_000 * MB, "raised"),
        (json!([1, 2, 3]), 32 * MB, "below --memory"),
        (json!([1, 2]), 128 * MB, "not in protocol 2"),
    ] {
        let mut h = Host::spawn(&args);
        let r = h.call(
            "hello",
            json!({"protocol": offer, "limits": {"memory_ceiling": ceiling}}),
        );
        assert_eq!(code(&r), "E_REQUEST", "{why}: {r}");
        assert!(h.ended(), "{why}: the session ended");
    }

    let mut h = Host::spawn(&args);
    let r = h.call("hello", json!({"protocol": [1, 2, 3]}));
    assert_eq!(r["limits"]["memory"], 64 * MB, "{r}");
    assert_eq!(
        r["limits"]["memory_ceiling"],
        1_000 * MB,
        "the operator's: {r}"
    );
    let dir = unique("host-ceiling");
    let cfg = dir.join("host.toml");
    std::fs::write(&cfg, "[engine]\nmemory_ceiling = \"900MB\"\n").unwrap();
    let mut c = Host::spawn(&["--memory", "64MB", "--config", cfg.to_str().unwrap()]);
    let r = c.call("hello", json!({"protocol": [1]}));
    assert_eq!(
        r["limits"]["memory_ceiling"],
        900 * MB,
        "from the config: {r}"
    );
    let both = Command::new(common::BIN)
        .args([
            "host",
            "--memory-ceiling",
            "1GB",
            "--config",
            cfg.to_str().unwrap(),
        ])
        .args(common::PLATFORM)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!both.status.success(), "set in two places, and started");
    let mut d = Host::spawn(&["--memory", "1GB"]);
    let r = d.call("hello", json!({"protocol": [1]}));
    assert_eq!(
        r["limits"]["memory_ceiling"],
        1_500 * MB,
        "unset, half again DuckDB's budget: {r}"
    );

    // Lowered, it is the one enforced: a planning blowup that peaks near 230 MB passes under the
    // operator's 1 GB and is killed under the host's 128 MB.
    let mut h = Host::spawn(&args);
    let r = h.call(
        "hello",
        json!({"protocol": [1, 2, 3], "limits": {"memory_ceiling": 128 * MB}}),
    );
    assert_eq!(r["protocol"], 3, "{r}");
    assert_eq!(r["limits"]["memory_ceiling"], 128 * MB, "{r}");
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..18 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let sql = format!("WITH {} SELECT m FROM l17", ctes.join(", "));
    let r = h.call("query", json!({ "sql": sql }));
    assert_eq!(code(&r), "E_REBUILT", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("memory ceiling")),
        "{r}"
    );
    let r = h.call("query", json!({"sql": "SELECT 42 AS n"}));
    assert_eq!(r["ok"], true, "the rebuilt bench answers: {r}");
}

/// An Arrow stream of `rows` readings whose `meter` is `width` bytes: at 300,000 rows of 400
/// bytes (126 MB) its loader peaks at 145 MB; a loader with next to nothing to load, at about 50.
#[cfg(target_os = "linux")]
fn wide_readings(path: &Path, rows: i64, width: usize) -> PathBuf {
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("meter", DataType::Utf8, false),
    ]);
    let mut w = StreamWriter::try_new(std::fs::File::create(path).unwrap(), &schema).unwrap();
    let pad = "x".repeat(width);
    for start in (0..rows).step_by(65_536) {
        let ids: Vec<i64> = (start..rows.min(start + 65_536)).collect();
        let meters: Vec<String> = ids.iter().map(|i| format!("m{i}-{pad}")).collect();
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(Int64Array::from(ids)) as ArrayRef,
                Arc::new(StringArray::from(meters)) as ArrayRef,
            ],
        )
        .unwrap();
        w.write(&batch).unwrap();
    }
    w.finish().unwrap();
    path.to_path_buf()
}

// Linux only: a loader's memory is read from /proc, and elsewhere the ceiling is not enforced
// (`a_loader_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
#[test]
#[cfg(target_os = "linux")]
fn a_host_lowers_the_loader_ceiling_and_cannot_raise_it() {
    // REQ-8. The operator sets the loader memory ceiling, on the command line or in the config;
    // from protocol 6 `hello` reports it, and a host may lower it for its session, never raise
    // it. Lowered, it is the one enforced: past it the attach is `E_LIMIT`, saying what
    // happened, and nothing is attached.
    const MB: u64 = 1_000_000;
    let dir = unique("host-loader-ceiling");
    let wide = wide_readings(&dir.join("wide.arrows"), 300_000, 400);
    let small = arrows(&dir.join("small.arrows"), &readings(10, "s"), true);
    let v6 = json!([1, 2, 3, 4, 5, 6]);

    let mut h = Host::spawn(&[]);
    let r = h.call("hello", json!({"protocol": v6}));
    assert_eq!(r["protocol"], 6, "{r}");
    assert_eq!(
        r["limits"]["loader_memory_ceiling"],
        1_000 * MB,
        "the default: {r}"
    );
    assert_eq!(attach(&mut h, "wide", &wide)["ok"], true, "under 1 GB");
    // Before protocol 6 it is not reported: a host may send `limits` back as it got them.
    let mut h = Host::spawn(&[]);
    let r = h.call("hello", json!({"protocol": [1, 2, 3, 4, 5]}));
    assert!(r["limits"].get("loader_memory_ceiling").is_none(), "{r}");

    let cfg = dir.join("host.toml");
    std::fs::write(&cfg, "[loader]\nmemory_ceiling = \"600MB\"\n").unwrap();
    let mut c = Host::spawn(&["--config", cfg.to_str().unwrap()]);
    let r = c.call("hello", json!({"protocol": v6}));
    assert_eq!(
        r["limits"]["loader_memory_ceiling"],
        600 * MB,
        "from the config: {r}"
    );

    // Lowered by the host, and held to.
    let mut h = Host::spawn(&["--loader-memory-ceiling", "800MB"]);
    let r = h.call(
        "hello",
        json!({"protocol": v6, "limits": {"loader_memory_ceiling": 90 * MB}}),
    );
    assert_eq!(r["limits"]["loader_memory_ceiling"], 90 * MB, "{r}");
    let r = attach(&mut h, "wide", &wide);
    assert_eq!(code(&r), "E_LIMIT", "{r}");
    let said = r["error"]["message"].as_str().unwrap_or("");
    assert!(
        said.contains("passed the loader memory ceiling") && said.contains("against 90 MB"),
        "{r}"
    );
    assert!(
        said.contains("nothing of it was attached") && said.contains("Narrow the source"),
        "says what happened and what to do: {r}"
    );
    let r = h.call("describe", json!({}));
    assert_eq!(r["tables"], json!([]), "nothing attached: {r}");
    assert_eq!(
        attach(&mut h, "small", &small)["ok"],
        true,
        "a small one fits"
    );

    // Never raised, nor to nothing, nor given before protocol 6: the session ends, as for any
    // limit.
    for (offer, ceiling, why) in [
        (v6.clone(), 900 * MB, "raised"),
        (json!([1, 2, 3, 4, 5]), 90 * MB, "not in protocol 5"),
        (v6.clone(), 0, "to nothing"),
    ] {
        let mut h = Host::spawn(&["--loader-memory-ceiling", "800MB"]);
        let r = h.call(
            "hello",
            json!({"protocol": offer, "limits": {"loader_memory_ceiling": ceiling}}),
        );
        assert_eq!(code(&r), "E_REQUEST", "{why}: {r}");
        assert!(h.ended(), "{why}: the session ended");
    }
}

#[test]
fn a_bad_attachment_attaches_nothing() {
    // Validated as a connector's stream is. A refusal changes nothing: the table already there
    // keeps serving, and nothing new appears.
    let dir = unique("host-bad");
    let mut h = Host::start(&[]);
    let good = arrows(&dir.join("good.arrows"), &readings(3, "good"), true);
    assert_eq!(attach(&mut h, "readings", &good)["ok"], true);

    let cut = arrows(&dir.join("cut.arrows"), &readings(5, "cut"), false);
    let junk = dir.join("junk.arrows");
    std::fs::write(&junk, "id,meter\n1,lab\n").unwrap();
    // Not Arrow either, though it ends as a stream does: read past the marker check, refused by
    // the reader of the stream's first message, whose words must be about a host's input.
    let marked = dir.join("marked.arrows");
    std::fs::write(&marked, b"id,meter\n1,lab\n\xff\xff\xff\xff\0\0\0\0").unwrap();
    let link = dir.join("link.arrows");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    let nanos = {
        let schema = Schema::new(vec![Field::new(
            "at",
            DataType::Timestamp(TimeUnit::Nanosecond, None),
            true,
        )]);
        let b = RecordBatch::try_new(
            Arc::new(schema),
            vec![Arc::new(arrow::array::TimestampNanosecondArray::from(vec![1i64])) as ArrayRef],
        )
        .unwrap();
        arrows(&dir.join("nanos.arrows"), &b, true)
    };
    let twice = {
        let schema = Schema::new(vec![
            Field::new("x", DataType::Int64, true),
            Field::new("x", DataType::Int64, true),
        ]);
        let b = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
                Arc::new(Int64Array::from(vec![2])) as ArrayRef,
            ],
        )
        .unwrap();
        arrows(&dir.join("twice.arrows"), &b, true)
    };
    let cased = {
        let schema = Schema::new(vec![
            Field::new("Value", DataType::Int64, true),
            Field::new("value", DataType::Int64, true),
        ]);
        let b = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
                Arc::new(Int64Array::from(vec![2])) as ArrayRef,
            ],
        )
        .unwrap();
        arrows(&dir.join("cased.arrows"), &b, true)
    };
    // A whole stream, then another: read up to the first marker, it was 3 rows of 8.
    let two = {
        let p = dir.join("two.arrows");
        let mut bytes = std::fs::read(arrows(&p, &readings(3, "first"), true)).unwrap();
        bytes.extend(std::fs::read(arrows(&p, &readings(5, "second"), true)).unwrap());
        std::fs::write(&p, bytes).unwrap();
        p
    };
    for (what, path, says) in [
        ("a stream cut short", &cut, "end-of-stream"),
        (
            "a stream with another after it",
            &two,
            "after its end-of-stream marker",
        ),
        ("not Arrow at all", &junk, "Arrow"),
        ("not Arrow, ending as a stream does", &marked, "Arrow"),
        ("a symlink", &link, "regular file"),
        ("a type outside the vocabulary", &nanos, "`at`"),
        ("two columns with one name", &twice, "`x`"),
        ("two columns DuckDB cannot tell apart", &cased, "twice"),
        ("nothing there", &dir.join("missing.arrows"), "regular file"),
    ] {
        for name in ["readings", "fresh"] {
            let r = attach(&mut h, name, path);
            assert_eq!(code(&r), "E_DATA", "{what}: {r}");
            let message = r["error"]["message"].as_str().unwrap();
            assert!(message.contains(says), "{what}: {message}");
            // A host's stream comes through no stdout: that is a connector's word.
            assert!(!message.contains("stdout"), "{what}: {message}");
            let dir_name = dir.file_name().unwrap().to_str().unwrap();
            assert!(
                !message.contains(dir_name),
                "{what} named a path: {message}"
            );
        }
    }
    let r = h.call(
        "query",
        json!({"sql": "SELECT min(meter) || '!' AS m FROM readings"}),
    );
    assert!(
        r["text"].as_str().unwrap().contains("good-0!"),
        "still the good data: {r}"
    );
    let r = h.call("describe", json!({}));
    assert_eq!(r["tables"].as_array().unwrap().len(), 1, "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attach_name_the_agent_could_not_type_is_refused() {
    // Refused, not rewritten: the host knows exactly what the agent types.
    let dir = unique("host-names");
    let mut h = Host::start(&[]);
    let file = arrows(&dir.join("t.arrows"), &readings(1, "t"), true);
    // `left`, `join` and `asof` are not reserved words, yet no query can name them unquoted.
    for name in [
        "Readings",
        "1st",
        "a-b",
        "select",
        "from",
        "left",
        "join",
        "asof",
        "glob",
        "similar",
        "benchy_x",
        "main",
        "",
        &"x".repeat(64),
    ] {
        let r = attach(&mut h, name, &file);
        assert_eq!(code(&r), "E_NAME", "{name:?}: {r}");
    }
    // An unreserved keyword is a fine table name, and so is one DuckDB parses as a name.
    let r = attach(&mut h, "values", &file);
    assert_eq!(r["ok"], true, "{r}");
    let r = attach(&mut h, "data", &file);
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 10 AS n FROM data"}),
    );
    assert!(r["text"].as_str().unwrap().contains("11"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_detach_drops_the_results_that_could_hold_its_rows() {
    let dir = unique("host-detach");
    let mut h = Host::start(&[]);
    attach(
        &mut h,
        "readings",
        &arrows(&dir.join("a.arrows"), &readings(3, "a"), true),
    );
    let r = h.call("query", json!({"sql": "SELECT * FROM readings"}));
    let id = r["result_ids"][0].as_str().unwrap().to_string();
    let r = h.call("page", json!({"result_id": id}));
    assert_eq!(r["ok"], true, "retained: {r}");

    let r = h.call("detach", json!({"name": "readings"}));
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call("page", json!({"result_id": id}));
    assert_eq!(code(&r), "E_NO_RESULT", "its rows are gone with it: {r}");
    let r = h.call("query", json!({"sql": "SELECT * FROM readings"}));
    assert_eq!(code(&r), "E_SQL", "{r}");
    let r = h.call("detach", json!({"name": "readings"}));
    assert_eq!(code(&r), "E_NAME", "{r}");
    let r = h.call("describe", json!({}));
    assert!(r["tables"].as_array().unwrap().is_empty(), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ping_is_answered_while_a_query_runs() {
    // One call runs at a time, but a ping never waits behind it; and an id still in use is not
    // accepted twice.
    let mut h = Host::start(&["--deadline", "3"]);
    h.send(&json!({"id": 1, "call": "query",
                   "sql": "SELECT count(*) FROM range(100000000000) a"}));
    std::thread::sleep(Duration::from_millis(300));
    let asked = Instant::now();
    h.send(&json!({"id": 1, "call": "query", "sql": "SELECT 1"}));
    h.send(&json!({"id": 2, "call": "ping"}));
    let first = h.recv();
    assert!(first["id"].is_null(), "{first}");
    assert_eq!(code(&first), "E_REQUEST", "an id in use: {first}");
    let pong = h.recv();
    assert_eq!(pong, json!({"id": 2, "ok": true}));
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "{:?}",
        asked.elapsed()
    );
    let done = h.recv();
    assert_eq!(done["id"], 1, "{done}");
    assert_eq!(code(&done), "E_DEADLINE", "{done}");
}

#[test]
fn a_rebuild_restores_the_attachments() {
    // REQ-8 for a host: a query that wedges the planner costs the engine. The host is told
    // E_REBUILT, and every table it attached is back — the agent's own are not.
    let dir = unique("host-rebuild");
    let mut h = Host::start(&["--deadline", "1", "--hard-grace", "1"]);
    attach(
        &mut h,
        "readings",
        &arrows(&dir.join("a.arrows"), &readings(4, "a"), true),
    );
    h.call(
        "query",
        json!({"sql": "CREATE TABLE mine AS SELECT 1 AS n"}),
    );
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let r = h.call(
        "query",
        json!({"sql": format!("WITH {} SELECT m FROM l18", ctes.join(", "))}),
    );
    assert_eq!(code(&r), "E_REBUILT", "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 800 AS n FROM readings"}),
    );
    assert!(r["text"].as_str().unwrap().contains("804"), "{r}");
    let r = h.call("query", json!({"sql": "SELECT * FROM mine"}));
    assert_eq!(code(&r), "E_SQL", "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_holds_at_most_its_limit_of_tables() {
    let dir = unique("host-limit");
    let mut h = Host::start(&[]);
    let file = arrows(&dir.join("t.arrows"), &readings(1, "t"), true);
    for i in 0..64 {
        let r = attach(&mut h, &format!("t{i}"), &file);
        assert_eq!(r["ok"], true, "{i}: {r}");
    }
    let r = attach(&mut h, "one_more", &file);
    assert_eq!(code(&r), "E_LIMIT", "{r}");
    let r = attach(&mut h, "t3", &file);
    assert_eq!(r["ok"], true, "replacing is not adding: {r}");
    let r = h.call("detach", json!({"name": "t0"}));
    assert_eq!(r["ok"], true, "{r}");
    let r = attach(&mut h, "one_more", &file);
    assert_eq!(r["ok"], true, "room again: {r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_session_leaves_nothing_behind() {
    // REQ-1 for a host: when it closes the pipe, the engine stops and the session's directory —
    // the store, the engine's workspace — goes with it.
    let dir = unique("host-end");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut child = Command::new(common::BIN)
        .arg("host")
        .args(common::PLATFORM)
        .env("TMPDIR", &tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    write_frame(
        &mut stdin,
        &json!({"id": 1, "call": "hello", "protocol": [1]}),
    )
    .unwrap();
    let _: Value = read_frame(&mut stdout).unwrap();
    let file = arrows(&dir.join("a.arrows"), &readings(3, "a"), true);
    write_frame(
        &mut stdin,
        &json!({"id": 2, "call": "attach", "name": "readings", "path": file}),
    )
    .unwrap();
    let r: Value = read_frame(&mut stdout).unwrap();
    assert_eq!(r["ok"], true, "{r}");
    assert!(
        std::fs::read_dir(&tmp).unwrap().next().is_some(),
        "the session is on disk while it runs"
    );
    drop(stdin);
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(started.elapsed() < Duration::from_secs(10), "it ended");
        std::thread::sleep(Duration::from_millis(20));
    }
    let left: Vec<PathBuf> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_dictionary_encoded_attachment_is_unpacked_within_the_bound() {
    // pandas writes a categorical as a dictionary, and ClickHouse a LowCardinality column when
    // asked to. It is unpacked into its values — the same table a plain string column makes —
    // and its unpacked size is counted against what an attachment may hold, because a few bytes
    // of keys can each name a long value: 70,000 keys into one 4 KiB value is a stream of 300 KB
    // that unpacks to 287 MB, past the 256 MiB an attachment may be. Refused as `E_LIMIT`, before
    // anything is unpacked, rather than as an appender's error naming duckdb-rs.
    use arrow::array::{Array, DictionaryArray, Int32Array};
    let dir = unique("host-dict");
    let mut h = Host::start(&[]);
    let keys = Int32Array::from(vec![0, 1, 0]);
    let values = Arc::new(StringArray::from(vec!["lab", "office"]));
    let dict = DictionaryArray::try_new(keys, values).unwrap();
    let schema = Schema::new(vec![Field::new("meter", dict.data_type().clone(), false)]);
    let b = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(dict) as ArrayRef]).unwrap();
    let r = attach(&mut h, "readings", &arrows(&dir.join("d.arrows"), &b, true));
    assert_eq!(r["ok"], true, "{r}");
    let q = h.call(
        "query",
        json!({"sql": "SELECT string_agg(meter, ',' ORDER BY meter), any_value(typeof(meter)) \
                       FROM readings"}),
    );
    let text = q["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("lab,lab,office") && text.contains("VARCHAR"),
        "{q}"
    );

    let keys = Int32Array::from(vec![0; 70_000]);
    let values = Arc::new(StringArray::from(vec!["x".repeat(4096)]));
    let dict = DictionaryArray::try_new(keys, values).unwrap();
    let schema = Schema::new(vec![Field::new("meter", dict.data_type().clone(), false)]);
    let b = RecordBatch::try_new(Arc::new(schema), vec![Arc::new(dict) as ArrayRef]).unwrap();
    let file = arrows(&dir.join("big.arrows"), &b, true);
    assert!(std::fs::metadata(&file).unwrap().len() < 1024 * 1024);
    let r = attach(&mut h, "readings", &file);
    assert_eq!(code(&r), "E_LIMIT", "{r}");
    let message = r["error"]["message"].as_str().unwrap();
    assert!(message.contains("unpack"), "{message}");
    assert!(
        !message.contains("duckdb-rs"),
        "an internal message leaked: {message}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_view_attachment_is_unpacked_within_the_bound() {
    // A string or binary view is 16 bytes on the wire whatever it points at, and every view may
    // point at the same block: 4,200 views of one 64 KiB block are a file of 130 KB that unpacks
    // to 275 MB, past the 256 MiB an attachment may be. Measured before this was counted, such a
    // file of 162 KB attached as 262 MB and took the loader to 1.19 GB of RSS. Refused as
    // `E_LIMIT`, before anything is unpacked.
    use arrow::array::{Array, BinaryViewBuilder};
    let dir = unique("host-view");
    let mut h = Host::start(&[]);
    let views = |n: usize, len: usize| {
        let mut b = BinaryViewBuilder::new();
        let block = b.append_block(arrow::buffer::Buffer::from_vec(vec![7u8; len]));
        for _ in 0..n {
            b.try_append_view(block, 0, len as u32).unwrap();
        }
        let a = b.finish();
        let schema = Schema::new(vec![Field::new("blob", a.data_type().clone(), false)]);
        RecordBatch::try_new(Arc::new(schema), vec![Arc::new(a) as ArrayRef]).unwrap()
    };
    let r = attach(
        &mut h,
        "blobs",
        &arrows(&dir.join("v.arrows"), &views(3, 20), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let q = h.call(
        "query",
        json!({"sql": "SELECT sum(octet_length(blob)), any_value(typeof(blob)) FROM blobs"}),
    );
    let text = q["text"].as_str().unwrap_or_default();
    assert!(text.contains("60") && text.contains("BLOB"), "{q}");

    let file = arrows(&dir.join("big.arrows"), &views(4200, 64 * 1024), true);
    assert!(std::fs::metadata(&file).unwrap().len() < 1024 * 1024);
    let r = attach(&mut h, "blobs", &file);
    assert_eq!(code(&r), "E_LIMIT", "{r}");
    let message = r["error"]["message"].as_str().unwrap();
    assert!(message.contains("unpack"), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_that_does_not_open_properly_ends() {
    // Whatever is wrong with the first request — not a hello, no id, a field hello does not
    // take, a version list that is not a list — the session ends with a refusal: a host that
    // cannot open a session properly has nothing else it can do in one.
    for (first, why) in [
        (json!({"id": 1, "call": "frobnicate"}), "an unknown call"),
        (json!({"call": "hello", "protocol": [1]}), "no id"),
        (
            json!({"id": 1, "call": "hello", "protocol": [1], "caps": {}}),
            "an unknown field",
        ),
        (
            json!({"id": 1, "call": "hello", "protocol": 1}),
            "a version that is not a list",
        ),
    ] {
        let mut h = Host::spawn(&[]);
        h.send(&first);
        let r = h.recv();
        assert_eq!(code(&r), "E_REQUEST", "{why}: {r}");
        assert!(h.ended(), "{why}: the session went on");
    }
}

#[test]
fn a_request_is_read_exactly_as_sent() {
    let mut h = Host::start(&["--deadline", "5"]);
    // A key given twice has two values, and taking either one is a guess.
    h.send_raw(r#"{"id": 7, "call": "query", "sql": "SELECT 1", "sql": "SELECT 2"}"#);
    let r = h.recv();
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    // The caps hello reports can be handed back, each lowered but never raised — the deadline
    // among them.
    let caps = h.call(
        "query",
        json!({"sql": "SELECT 1",
                                       "caps": {"max_rows": 200, "max_bytes": 8192,
                                                "max_cell_bytes": 256, "deadline_secs": 1}}),
    );
    assert_eq!(caps["ok"], true, "{caps}");
    assert_eq!(caps["caps"]["deadline_secs"], 1, "{caps}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT 1", "caps": {"deadline_secs": 6}}),
    );
    assert_eq!(code(&r), "E_REQUEST", "a deadline is never raised: {r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) FROM range(100000000000) a",
                                   "caps": {"deadline_secs": 1}}),
    );
    assert_eq!(code(&r), "E_DEADLINE", "the lowered deadline applied: {r}");
}

#[test]
fn an_id_in_use_is_answered_once() {
    // A request that reuses an id still being answered is refused — and the refusal carries no
    // id, or the host would see two replies with one id. So for every kind of request: a ping, a
    // call that does not exist, a field that does not either, a key given twice.
    let mut h = Host::start(&["--deadline", "3"]);
    h.send(&json!({"id": 9, "call": "query",
                   "sql": "SELECT count(*) FROM range(100000000000) a"}));
    std::thread::sleep(Duration::from_millis(300));
    h.send(&json!({"id": 9, "call": "ping"}));
    h.send(&json!({"id": 9, "call": "nosuch"}));
    h.send(&json!({"id": 9, "call": "query", "sql": "SELECT 1", "max_row": 1}));
    h.send_raw(r#"{"id": 9, "call": "query", "sql": "SELECT 1", "sql": "SELECT 2"}"#);
    for _ in 0..4 {
        let r = h.recv();
        assert!(r["id"].is_null(), "a second reply with id 9: {r}");
        assert_eq!(code(&r), "E_REQUEST", "{r}");
        assert!(r["error"]["message"].as_str().unwrap().contains("9"), "{r}");
    }
    let done = h.recv();
    assert_eq!(done["id"], 9, "{done}");
    assert_eq!(code(&done), "E_DEADLINE", "{done}");
}

#[test]
fn a_host_that_closes_its_input_still_gets_its_answers() {
    // Closing stdin says "no more requests", not "I have gone": a host that writes everything,
    // closes its end and then reads — `communicate()`, a shell pipeline — is answered in full.
    let mut h = Host::spawn(&[]);
    h.send(&json!({"id": 1, "call": "hello", "protocol": [1]}));
    h.send(&json!({"id": 2, "call": "query", "sql": "SELECT 40 + 2 AS answer"}));
    drop(std::mem::replace(&mut h.stdin, spare_stdin()));
    let hello = h.recv();
    assert_eq!(hello["id"], 1, "{hello}");
    let answer = h.recv();
    assert_eq!(answer["id"], 2, "{answer}");
    assert!(
        answer["text"].as_str().unwrap_or("").contains("42"),
        "{answer}"
    );
    assert!(h.ended(), "and then the session ends");
}

#[test]
fn a_host_that_has_gone_is_not_worked_for() {
    // A host that has closed both pipes will read no reply, so what it queued is not run: only
    // the call already running finishes, bounded by its deadline.
    let mut h = Host::start(&["--deadline", "3"]);
    for id in 1..=3 {
        h.send(&json!({"id": id, "call": "query",
                       "sql": "SELECT count(*) FROM range(100000000000) a"}));
    }
    std::thread::sleep(Duration::from_millis(300));
    let closed = Instant::now();
    drop(std::mem::replace(&mut h.stdin, spare_stdin()));
    drop(std::mem::replace(&mut h.stdout, spare_stdout()));
    while h.child.try_wait().unwrap().is_none() {
        assert!(
            closed.elapsed() < Duration::from_secs(20),
            "the session did not end"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        closed.elapsed() < Duration::from_secs(6),
        "the queued calls ran for a host that had gone: {:?}",
        closed.elapsed()
    );
}

/// Stand-ins for a Host's pipes, so the real ones can be closed while the Host lives on.
fn spare_stdin() -> ChildStdin {
    let mut other = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
    let stdin = other.stdin.take().unwrap();
    let _ = other.wait();
    stdin
}

fn spare_stdout() -> ChildStdout {
    let mut other = Command::new("true").stdout(Stdio::piped()).spawn().unwrap();
    let stdout = other.stdout.take().unwrap();
    let _ = other.wait();
    stdout
}

/// A host session in `tmp`, with `good` and `broken` attached and `broken`'s stored file gone —
/// what a compromised engine can do, since it may delete within its session. A wedged query then
/// kills the engine, and no rebuild can succeed while `broken` is on the bench.
fn a_bench_that_cannot_rebuild(dir: &Path, tmp: &Path) -> (Child, Host) {
    let mut child = Command::new(common::BIN)
        .arg("host")
        .args(common::PLATFORM)
        .args(["--deadline", "1", "--hard-grace", "1"])
        .env("TMPDIR", tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut h = Host {
        stdin: child.stdin.take().unwrap(),
        stdout: child.stdout.take().unwrap(),
        child: Command::new("true").spawn().unwrap(),
        next: 100,
    };
    assert_eq!(h.call("hello", json!({"protocol": [1]}))["ok"], true);
    let file = arrows(&dir.join("t.arrows"), &readings(2, "t"), true);
    assert_eq!(attach(&mut h, "good", &file)["ok"], true);
    assert_eq!(attach(&mut h, "broken", &file)["ok"], true);
    let session = std::fs::read_dir(tmp)
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let mut stored: Vec<PathBuf> = std::fs::read_dir(session.join("store"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    stored.sort_by_key(|p| {
        p.file_stem()
            .and_then(|s| s.to_str()?.parse::<u64>().ok())
            .unwrap_or(0)
    });
    std::fs::remove_file(stored.last().unwrap()).unwrap();
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let r = h.call(
        "query",
        json!({"sql": format!("WITH {} SELECT m FROM l18", ctes.join(", "))}),
    );
    assert_eq!(code(&r), "E_UNAVAILABLE", "{r}");
    (child, h)
}

#[test]
fn a_detach_brings_a_bench_that_could_not_rebuild_back() {
    // A rebuild that fails is not retried forever. But detaching the table that stops it changes
    // what a rebuild attaches, and earns a fresh try.
    let dir = unique("host-unstick");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let (mut child, mut h) = a_bench_that_cannot_rebuild(&dir, &tmp);
    for _ in 0..3 {
        let r = h.call("query", json!({"sql": "SELECT 1"}));
        assert_eq!(code(&r), "E_UNAVAILABLE", "{r}");
    }
    let r = h.call("query", json!({"sql": "SELECT 1"}));
    assert!(
        r["error"]["message"].as_str().unwrap().contains("detach"),
        "says what brings it back: {r}"
    );
    assert_eq!(h.call("detach", json!({"name": "broken"}))["ok"], true);
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 300 AS n FROM good"}),
    );
    assert!(
        r["ok"] == true && r["text"].as_str().unwrap().contains("302"),
        "back, with the table that could be restored: {r}"
    );
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_killed_host_leaves_nothing_behind_even_without_an_engine() {
    // SIGKILL gives the server no chance to act, and a session whose engine could not be rebuilt
    // has no engine to notice either. What the session holds — the store among it — still goes.
    let dir = unique("host-killed");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let (mut child, _h) = a_bench_that_cannot_rebuild(&dir, &tmp);
    child.kill().unwrap();
    let _ = child.wait();
    let started = Instant::now();
    let left = || -> Vec<PathBuf> {
        std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect()
    };
    while !left().is_empty() && started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(left().is_empty(), "left behind: {:?}", left());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_budget_moves_by_what_was_attached_and_nothing_else() {
    // REQ-12 for a host: every attach counts against the session's budget, replacements too —
    // by the rows the engine counted and the bytes benchy measured, not by what a loader says.
    // A refused attach costs nothing.
    let dir = unique("host-budget");
    let mut h = Host::start(&[]);
    let left = |h: &mut Host| -> (u64, u64) {
        let r = h.call("describe", json!({}));
        (
            r["budget"]["rows_left"]
                .as_u64()
                .expect("describe says what is left"),
            r["budget"]["bytes_left"].as_u64().unwrap(),
        )
    };
    let (rows0, bytes0) = left(&mut h);
    let file = arrows(&dir.join("a.arrows"), &readings(3, "a"), true);
    let size = std::fs::metadata(&file).unwrap().len();
    assert_eq!(attach(&mut h, "readings", &file)["ok"], true);
    assert_eq!(left(&mut h), (rows0 - 3, bytes0 - size));
    let cut = arrows(&dir.join("cut.arrows"), &readings(5, "cut"), false);
    assert_eq!(code(&attach(&mut h, "readings", &cut)), "E_DATA");
    assert_eq!(code(&attach(&mut h, "Nope", &file)), "E_NAME");
    assert_eq!(
        left(&mut h),
        (rows0 - 3, bytes0 - size),
        "a refusal costs nothing"
    );
    assert_eq!(attach(&mut h, "readings", &file)["ok"], true);
    assert_eq!(
        left(&mut h),
        (rows0 - 6, bytes0 - 2 * size),
        "a replacement counts"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_request_larger_than_a_request_can_be_ends_the_session() {
    // A request is SQL or a path: a megabyte is more than any needs, and every frame is parsed,
    // so a larger one is not read at all. Like any frame that cannot be read, it ends the session.
    let mut h = Host::start(&[]);
    let sql = format!("SELECT '{}' AS x", "x".repeat(2 << 20));
    // The session may end on the length alone, before the rest is written: a broken pipe here is
    // the refusal, not a failure of the test.
    let _ = write_frame(&mut h.stdin, &json!({"id": 5, "call": "query", "sql": sql}));
    assert!(h.ended(), "a 2 MiB request was read");
}

#[test]
fn the_agent_cannot_forge_what_the_host_reads_from_the_catalog() {
    // Measured on DuckDB 1.5.5 before the fix: a macro the agent named `duckdb_columns` stood in
    // for DuckDB's own on the host-facing connection too, which sees the agent's schema, and the
    // host's next attach was answered with the agent's columns; one named `duckdb_tables` made
    // every later attach fail and put a table that does not exist in `describe`. benchy reads
    // the catalog through `system.main` (`benchys_catalog_reads_cannot_be_shadowed`), and the
    // agent may not take a built-in's name (REQ-9).
    let dir = unique("host-forge");
    let mut h = Host::start(&[]);
    for sql in [
        "CREATE MACRO duckdb_columns() AS TABLE SELECT database_name, table_name, \
         'forged' AS column_name, 'VARCHAR' AS data_type, 0 AS column_index \
         FROM system.main.duckdb_columns()",
        "CREATE MACRO duckdb_tables() AS TABLE SELECT 'memory' AS database_name, \
         'main' AS schema_name, 'ghost' AS table_name, NULL AS comment, false AS internal",
    ] {
        let r = h.call("query", json!({ "sql": sql }));
        assert_eq!(code(&r), "E_SQL", "refused: {r}");
    }
    let r = attach(
        &mut h,
        "readings",
        &arrows(&dir.join("a.arrows"), &readings(3, "a"), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(
        r["columns"],
        json!([
            ["id", "BIGINT"],
            ["meter", "VARCHAR"],
            ["power_kw", "DOUBLE"]
        ]),
        "{r}"
    );
    let d = h.call("describe", json!({}));
    let text = d["text"].as_str().unwrap_or("");
    assert!(
        !text.contains("ghost") && !text.contains("forged"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attach_under_the_agents_own_name_is_refused() {
    // REQ-9: the agent's schema is ahead of every attached catalog on its search path, so a host
    // table attached under a name the agent already gave one of its own would be hidden from it.
    // The attach is refused, with the code for a name, and the agent's table stays.
    let dir = unique("host-own-name");
    let mut h = Host::start(&[]);
    let r = h.call(
        "query",
        json!({"sql": "CREATE TEMP TABLE later AS SELECT 42 AS mine"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = attach(
        &mut h,
        "later",
        &arrows(&dir.join("l.arrows"), &readings(2, "l"), true),
    );
    assert_eq!(code(&r), "E_NAME", "{r}");
    let msg = r["error"]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("`later`") && msg.contains("another name"),
        "{r}"
    );
    let r = h.call("query", json!({"sql": "SELECT mine + 500 AS n FROM later"}));
    assert!(r["text"].as_str().unwrap_or("").contains("542"), "{r}");
    let r = attach(
        &mut h,
        "later_too",
        &arrows(&dir.join("m.arrows"), &readings(2, "m"), true),
    );
    assert_eq!(r["ok"], true, "another name is free: {r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn columns_duckdb_tells_apart_attach() {
    // DuckDB folds ASCII case only, so `Ä` and `ä` are two columns to it, as they are here.
    let dir = unique("host-unicode");
    let mut h = Host::start(&[]);
    let schema = Schema::new(vec![
        Field::new("Ä", DataType::Int64, true),
        Field::new("ä", DataType::Int64, true),
    ]);
    let b = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            Arc::new(Int64Array::from(vec![2])) as ArrayRef,
        ],
    )
    .unwrap();
    let r = attach(&mut h, "umlauts", &arrows(&dir.join("u.arrows"), &b, true));
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT \"Ä\" * 10 + \"ä\" + 500 AS n FROM umlauts"}),
    );
    assert!(r["text"].as_str().unwrap_or("").contains("512"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hosts_describe_carries_the_shape_of_what_it_attached() {
    // REQ-14 through the host protocol: the Arrow loader profiles what it writes, as a file
    // loader does, and the host's `describe` text carries it. Three meters, hourly, one dead.
    let mut h = Host::start(&[]);
    let dir = unique("host-shape");
    let schema = Schema::new(vec![
        Field::new(
            "at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("meter", DataType::Utf8, false),
        Field::new("kw", DataType::Float64, true),
    ]);
    let (mut at, mut meter, mut kw) = (vec![], vec![], vec![]);
    for i in 0..(3 * 24) {
        for m in ["a", "b", "c"] {
            at.push(1_767_225_600_000_000i64 + i * 3_600_000_000);
            meter.push(m);
            kw.push(if m == "c" { 0.0 } else { 1.0 + i as f64 });
        }
    }
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(arrow::array::TimestampMicrosecondArray::from(at).with_timezone("UTC"))
                as ArrayRef,
            Arc::new(StringArray::from(meter)) as ArrayRef,
            Arc::new(Float64Array::from(kw)) as ArrayRef,
        ],
    )
    .unwrap();
    let r = attach(
        &mut h,
        "load",
        &arrows(&dir.join("load.arrows"), &batch, true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call("describe", json!({}));
    let text = r["text"].as_str().unwrap();
    for want in [
        "time column at (looks like it): 2026-01-01 00:00:00+00 to 2026-01-03 23:00:00+00",
        "series key meter (looks like it): 3 series ('a', 'b', 'c')",
        "'c': kw is 0 in every row",
    ] {
        assert!(text.contains(want), "expected {want:?} in:\n{text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hosts_describe_quotes_data_values_and_names_duckdbs_types() {
    // The same text as MCP's describe: values as quoted literals, types as DuckDB names them
    // (REQ-15 `a_data_value_in_describe_is_a_quoted_literal`).
    let mut h = Host::start(&[]);
    let dir = unique("host-hostile");
    let schema = Schema::new(vec![
        Field::new(
            "at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("meter", DataType::Utf8, false),
        Field::new("kw", DataType::Float64, true),
    ]);
    let (mut at, mut meter, mut kw) = (vec![], vec![], vec![]);
    for i in 0..24 {
        for m in ["lab", common::HOSTILE] {
            at.push(1_767_225_600_000_000i64 + i * 3_600_000_000);
            meter.push(m);
            kw.push(if m == "lab" { 1.0 + i as f64 } else { 0.0 });
        }
    }
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(arrow::array::TimestampMicrosecondArray::from(at).with_timezone("UTC"))
                as ArrayRef,
            Arc::new(StringArray::from(meter)) as ArrayRef,
            Arc::new(Float64Array::from(kw)) as ArrayRef,
        ],
    )
    .unwrap();
    let r = attach(
        &mut h,
        "load",
        &arrows(&dir.join("load.arrows"), &batch, true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = attach(
        &mut h,
        "plain",
        &arrows(&dir.join("plain.arrows"), &readings(3, "p"), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call("describe", json!({}));
    let text = r["text"].as_str().unwrap();
    common::assert_values_quoted(text, 2);
    assert!(
        text.contains("at timestamptz, meter varchar, kw double")
            && text.contains("id bigint, meter varchar, power_kw double"),
        "{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn attach_many(h: &mut Host, tables: &[(&str, &Path)]) -> Value {
    let tables: Vec<Value> = tables
        .iter()
        .map(|(name, path)| json!({"name": name, "path": path}))
        .collect();
    h.call("attach", json!({"tables": tables}))
}

/// The answer for each table of a protocol-2 `attach`, in the order asked.
fn each(reply: &Value) -> &Vec<Value> {
    assert_eq!(reply["ok"], true, "{reply}");
    reply["tables"].as_array().expect("one answer per table")
}

/// The trace lines of `role` doing `what`, from a session's log.
fn traced(log: &str, role: &str, what: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| l.strip_prefix("benchy-trace "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["role"] == role && e["what"] == what)
        .collect()
}

#[test]
fn several_tables_attach_in_one_request() {
    // Protocol 2: one `attach` puts several tables on the bench, each answered as an attach of
    // its own would be, and the engine is asked once — the listing of DuckDB's functions that
    // checks an attached file is paid once a request, not once a table (docs/PERFORMANCE.md R2-5).
    let dir = unique("host-many");
    let log = dir.join("trace.log");
    let mut h = Host::spawn_traced(&[], std::fs::File::create(&log).unwrap());
    let hello = h.call("hello", json!({"protocol": [1, 2]}));
    assert_eq!(hello["protocol"], 2, "{hello}");
    let files: Vec<PathBuf> = (1..=3)
        .map(|n| arrows(&dir.join(format!("{n}.arrows")), &readings(n, "m"), true))
        .collect();
    let r = attach_many(
        &mut h,
        &[("one", &files[0]), ("two", &files[1]), ("three", &files[2])],
    );
    let answers = each(&r);
    assert_eq!(answers.len(), 3, "{r}");
    for (answer, (name, rows)) in answers.iter().zip([("one", 1), ("two", 2), ("three", 3)]) {
        assert_eq!(answer["ok"], true, "{r}");
        assert_eq!(answer["name"], name, "{r}");
        assert_eq!(answer["rows"], rows, "{r}");
        assert_eq!(answer["columns"][0], json!(["id", "BIGINT"]), "{r}");
    }
    let r = h.call(
        "query",
        json!({"sql": "SELECT (SELECT count(*) FROM one) * 100 + (SELECT count(*) FROM two) * 10 \
                       + (SELECT count(*) FROM three) AS n"}),
    );
    assert!(r["text"].as_str().unwrap().contains("123"), "{r}");
    let r = h.call("describe", json!({}));
    let names: Vec<&str> = r["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["one", "two", "three"], "in the order asked: {r}");
    drop(h);
    let text = std::fs::read_to_string(&log).unwrap();
    let engine = |what: &str| traced(&text, "engine", what).len();
    assert_eq!(
        engine("engine.attach"),
        1,
        "one request to the engine: {text}"
    );
    assert_eq!(engine("attach.functions"), 1, "{text}");
    assert_eq!(engine("attach.inspect"), 3, "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_loaders_of_one_attach_run_at_once() {
    // Each table is turned into a DuckDB file by a loader of its own, a process, and they need
    // nothing from each other: the loaders of one request run side by side, as many at once as
    // the machine has cores, up to four.
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    if cores < 2 {
        eprintln!("one core: the loaders take turns, and there is nothing to see");
        return;
    }
    let dir = unique("host-side-by-side");
    let log = dir.join("trace.log");
    let mut h = Host::spawn_traced(&[], std::fs::File::create(&log).unwrap());
    h.call("hello", json!({"protocol": [2]}));
    let file = arrows(&dir.join("t.arrows"), &readings(2000, "t"), true);
    let r = attach_many(&mut h, &[("a", &file), ("b", &file), ("c", &file)]);
    assert!(each(&r).iter().all(|t| t["ok"] == true), "{r}");
    drop(h);
    let text = std::fs::read_to_string(&log).unwrap();
    // Each loader's run, as the server saw it: from when it started to when it answered.
    let mut runs: Vec<(u64, u64)> = traced(&text, "server", "loader.run")
        .iter()
        .map(|e| {
            let t0 = e["t0"].as_u64().unwrap();
            (t0, t0 + e["us"].as_u64().unwrap())
        })
        .collect();
    runs.sort();
    assert_eq!(runs.len(), 3, "{text}");
    assert!(
        runs[1].0 < runs[0].1,
        "the second loader started only once the first had finished: {runs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_bad_table_of_an_attach_is_refused_alone() {
    // Each table of a protocol-2 `attach` is answered on its own: a name the agent could not
    // type, a stream that is not one, a file that is not there — each refused with the code an
    // attach of its own would get, and the tables around it attached. A refusal still changes
    // nothing: a table being replaced by a bad one keeps serving.
    let dir = unique("host-many-bad");
    let mut h = Host::start_v2(&[]);
    let good = arrows(&dir.join("good.arrows"), &readings(3, "good"), true);
    let other = arrows(&dir.join("other.arrows"), &readings(5, "other"), true);
    let cut = arrows(&dir.join("cut.arrows"), &readings(5, "cut"), false);
    assert_eq!(attach(&mut h, "kept", &good)["ok"], true);
    let missing = dir.join("missing.arrows");
    let r = attach_many(
        &mut h,
        &[
            ("first", &good),
            ("Bad", &good),
            ("kept", &cut),
            ("gone", &missing),
            ("last", &other),
        ],
    );
    let answers = each(&r);
    let codes: Vec<&str> = answers.iter().map(code).collect();
    assert_eq!(codes, ["", "E_NAME", "E_DATA", "E_DATA", ""], "{r}");
    assert_eq!(answers[0]["rows"], 3, "{r}");
    assert_eq!(answers[4]["rows"], 5, "{r}");
    let names: Vec<&str> = answers
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["first", "Bad", "kept", "gone", "last"], "{r}");
    for a in &answers[1..4] {
        assert_eq!(a["ok"], false, "{r}");
        let message = a["error"]["message"].as_str().unwrap();
        let dir_name = dir.file_name().unwrap().to_str().unwrap();
        assert!(!message.contains(dir_name), "named a path: {message}");
    }
    let r = h.call(
        "query",
        json!({"sql": "SELECT (SELECT count(*) FROM first) * 100 + (SELECT count(*) FROM kept) \
                       * 10 + (SELECT count(*) FROM last) AS n"}),
    );
    assert!(r["text"].as_str().unwrap().contains("335"), "{r}");
    let r = h.call("describe", json!({}));
    assert_eq!(r["tables"].as_array().unwrap().len(), 3, "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attach_of_several_tables_is_held_to_the_limit_in_order() {
    // The tables of one request are counted as if attached one after another: with room for one
    // more, the first new name fits and the next is refused as `E_LIMIT`; replacing a table is
    // not adding one, however late in the request it comes. And each table of a request costs
    // the budget what it would have cost alone, a refused one nothing.
    let dir = unique("host-many-limit");
    let mut h = Host::start_v2(&[]);
    let file = arrows(&dir.join("t.arrows"), &readings(1, "t"), true);
    let size = std::fs::metadata(&file).unwrap().len();
    let names: Vec<String> = (0..63).map(|i| format!("t{i}")).collect();
    let first: Vec<(&str, &Path)> = names.iter().map(|n| (n.as_str(), file.as_path())).collect();
    let r = attach_many(&mut h, &first);
    assert!(each(&r).iter().all(|t| t["ok"] == true), "{r}");
    let left = |h: &mut Host| -> (u64, u64) {
        let r = h.call("describe", json!({}));
        (
            r["budget"]["rows_left"].as_u64().unwrap(),
            r["budget"]["bytes_left"].as_u64().unwrap(),
        )
    };
    let (rows0, bytes0) = left(&mut h);
    let r = attach_many(
        &mut h,
        &[("fits", &file), ("one_more", &file), ("t5", &file)],
    );
    let codes: Vec<&str> = each(&r).iter().map(code).collect();
    assert_eq!(codes, ["", "E_LIMIT", ""], "{r}");
    assert_eq!(
        left(&mut h),
        (rows0 - 2, bytes0 - 2 * size),
        "two attached, and the refusal cost nothing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_attach_request_that_is_not_well_formed_attaches_nothing() {
    // A request either names one table (`name`, `path`) or lists several (`tables`), and what it
    // lists is as strict as any request: a list that is empty or longer than a session can hold,
    // a name twice, an entry with a field it does not take — refused as a whole, as
    // `E_REQUEST`, with nothing attached. Protocol 1 has no `tables` at all.
    let dir = unique("host-many-strict");
    let file = arrows(&dir.join("t.arrows"), &readings(1, "t"), true);
    let mut h = Host::start(&[]);
    let r = attach_many(&mut h, &[("a", &file)]);
    assert_eq!(
        code(&r),
        "E_REQUEST",
        "protocol 1 takes one table a request: {r}"
    );

    let mut h = Host::start_v2(&[]);
    let too_many: Vec<String> = (0..65).map(|i| format!("t{i}")).collect();
    let too_many: Vec<(&str, &Path)> = too_many
        .iter()
        .map(|n| (n.as_str(), file.as_path()))
        .collect();
    for (what, req) in [
        ("an empty list", json!({"tables": []})),
        ("not a list", json!({"tables": {"name": "a", "path": file}})),
        (
            "a name and a list",
            json!({"name": "a", "path": file, "tables": [{"name": "b", "path": file}]}),
        ),
        ("neither", json!({})),
        (
            "a name twice",
            json!({"tables": [{"name": "a", "path": file}, {"name": "a", "path": file}]}),
        ),
        (
            "a field an entry does not take",
            json!({"tables": [{"name": "a", "path": file, "rows": 1}]}),
        ),
        (
            "an entry without a path",
            json!({"tables": [{"name": "a"}]}),
        ),
        ("an entry that is not an object", json!({"tables": ["a"]})),
    ] {
        let r = h.call("attach", req);
        assert_eq!(code(&r), "E_REQUEST", "{what}: {r}");
    }
    let r = attach_many(&mut h, &too_many);
    assert_eq!(code(&r), "E_REQUEST", "more than a session holds: {r}");
    let r = h.call("describe", json!({}));
    assert!(r["tables"].as_array().unwrap().is_empty(), "{r}");
    // A single table is still attached the way protocol 1 attaches it.
    let r = attach(&mut h, "single", &file);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["rows"], 1, "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session greeted in protocol 3 with `hello`'s extra `fields`: the reply.
fn hello_v3(args: &[&str], fields: Value) -> (Host, Value) {
    let mut h = Host::spawn(args);
    let mut req = fields;
    req["protocol"] = json!([1, 2, 3]);
    let r = h.call("hello", req);
    (h, r)
}

/// Whether `benchy-mcp` with `args` refuses to start, saying something on stderr.
fn refuses_to_start(args: &[&str]) -> bool {
    let out = Command::new(common::BIN)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    !out.status.success() && !out.stderr.is_empty()
}

#[test]
fn limits_are_lowered_by_the_host_and_raised_only_by_the_operator() {
    // What one attachment and the session may hold. The operator sets them, raised or lowered,
    // on the command line or in a config; a host may lower them for its session in `hello`,
    // never raise them, as it lowers the caps in a query.
    let dir = unique("host-limits");
    let (_, r) = hello_v3(&[], json!({}));
    // Off Linux the memory ceiling is not enforced, and reported as `null`
    // (`a_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
    let ceiling = if cfg!(target_os = "linux") {
        json!(1_500_000_000u64)
    } else {
        Value::Null
    };
    assert_eq!(
        r["limits"],
        json!({"attach_rows": 10_000_000u64, "attach_bytes": 268_435_456u64, "attachments": 64,
               "session_rows": 50_000_000u64, "session_bytes": 2_147_483_648u64,
               "memory": 1_000_000_000u64, "memory_ceiling": ceiling}),
        "{r}"
    );

    // Lowered by the host, and held to.
    let (mut h, r) = hello_v3(
        &[],
        json!({"limits": {"attach_rows": 5, "session_rows": 8, "attachments": 2}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["limits"]["attach_rows"], 5, "{r}");
    assert_eq!(r["limits"]["session_rows"], 8, "{r}");
    assert_eq!(r["limits"]["attachments"], 2, "{r}");
    assert_eq!(
        r["limits"]["attach_bytes"], 268_435_456u64,
        "the rest as they were: {r}"
    );
    let rows =
        |n: i64, tag: &str| arrows(&dir.join(format!("{tag}.arrows")), &readings(n, tag), true);
    let r = attach(&mut h, "a", &rows(6, "six"));
    assert_eq!(code(&r), "E_LIMIT", "over attach_rows: {r}");
    assert_eq!(attach(&mut h, "a", &rows(5, "five"))["ok"], true);
    let r = attach(&mut h, "b", &rows(4, "four"));
    assert_eq!(code(&r), "E_LIMIT", "over what session_rows has left: {r}");
    assert_eq!(attach(&mut h, "b", &rows(3, "three"))["ok"], true);
    let r = attach(&mut h, "c", &rows(0, "none"));
    assert_eq!(code(&r), "E_LIMIT", "over attachments: {r}");
    let r = h.call("describe", json!({}));
    assert_eq!(r["budget"]["rows_left"], 0, "{r}");

    // attach_bytes, for a file and for a stream.
    let big = rows(100, "hundred");
    let size = std::fs::metadata(&big).unwrap().len();
    let lowered = json!({"limits": {"attach_bytes": size - 1}});
    let (mut h, r) = hello_v3(&[], lowered.clone());
    assert_eq!(r["limits"]["attach_bytes"], size - 1, "{r}");
    let r = attach(&mut h, "a", &big);
    assert_eq!(code(&r), "E_LIMIT", "{r}");
    let mut s = common::streamed::Streamer::spawn(&[]);
    let mut req = lowered;
    req["protocol"] = json!([3]);
    assert_eq!(s.call("hello", req, &[])["ok"], true);
    let (r, _) = s.attach_stream("a", std::fs::read(&big).unwrap());
    assert_eq!(code(&r), "E_LIMIT", "streamed: {r}");
    assert!(
        r["error"]["message"].as_str().unwrap().contains("at most"),
        "{r}"
    );
    let (r, _) = s.attach_stream("a", std::fs::read(rows(3, "small")).unwrap());
    assert_eq!(r["ok"], true, "{r}");

    // Never raised by the host, nor lowered to nothing; and only in protocol 3. Each ends the
    // session, as anything wrong with `hello` does.
    for (what, args, offered, limits) in [
        (
            "raised",
            &[][..],
            json!([1, 2, 3]),
            json!({"attach_rows": 10_000_001u64}),
        ),
        (
            "raised",
            &[][..],
            json!([3]),
            json!({"session_bytes": 2_147_483_649u64}),
        ),
        ("zero", &[][..], json!([3]), json!({"attachments": 0})),
        ("unknown", &[][..], json!([3]), json!({"attach_row": 5})),
        (
            "not a number",
            &[][..],
            json!([3]),
            json!({"attach_rows": "5"}),
        ),
        (
            "protocol 2",
            &[][..],
            json!([1, 2]),
            json!({"attach_rows": 5}),
        ),
        (
            "past the operator's",
            &["--attach-rows", "100"][..],
            json!([3]),
            json!({"attach_rows": 101}),
        ),
    ] {
        let mut h = Host::spawn(args);
        let r = h.call("hello", json!({"protocol": offered, "limits": limits}));
        assert_eq!(code(&r), "E_REQUEST", "{what}: {r}");
        assert!(h.ended(), "{what}: the session ended");
    }

    // Raised, or lowered, by the operator: on the command line.
    let (_, r) = hello_v3(
        &[
            "--attach-rows",
            "20000000",
            "--attach-bytes",
            "600000000",
            "--session-rows",
            "90000000",
            "--session-bytes",
            "5000000000",
        ],
        json!({"limits": {"attach_rows": 15_000_000u64}}),
    );
    assert_eq!(r["ok"], true, "past the default, under the operator's: {r}");
    assert_eq!(r["limits"]["attach_rows"], 15_000_000u64, "{r}");
    assert_eq!(r["limits"]["attach_bytes"], 600_000_000u64, "{r}");
    assert_eq!(r["limits"]["session_rows"], 90_000_000u64, "{r}");
    assert_eq!(r["limits"]["session_bytes"], 5_000_000_000u64, "{r}");
    let (mut h, _) = hello_v3(&["--attach-rows", "5"], json!({}));
    assert_eq!(code(&attach(&mut h, "a", &rows(6, "six"))), "E_LIMIT");
    // A protocol-1 host is held to the operator's limits too.
    let mut h = Host::start(&["--session-rows", "4"]);
    assert_eq!(code(&attach(&mut h, "a", &rows(5, "five"))), "E_LIMIT");

    // And in a config, which for a host holds limits and no sources.
    let cfg = dir.join("host.toml");
    std::fs::write(
        &cfg,
        "[budget]\nrows = 6\n\n[attach]\nrows = 4\nbytes = 100000\n",
    )
    .unwrap();
    let cfg = cfg.to_str().unwrap();
    let (mut h, r) = hello_v3(&["--config", cfg], json!({}));
    assert_eq!(r["limits"]["attach_rows"], 4, "{r}");
    assert_eq!(r["limits"]["attach_bytes"], 100_000, "{r}");
    assert_eq!(r["limits"]["session_rows"], 6, "{r}");
    assert_eq!(code(&attach(&mut h, "a", &rows(5, "five"))), "E_LIMIT");
    // Set in two places, a limit is refused rather than one place quietly ignored; and `[attach]`
    // or `--attach-rows` mean nothing to a server no host attaches to.
    assert!(refuses_to_start(&[
        "host",
        "--config",
        cfg,
        "--attach-rows",
        "3"
    ]));
    let sources = dir.join("sources.toml");
    std::fs::write(
        &sources,
        format!(
            "[[source]]\nname = \"r\"\ntype = \"file\"\npath = \"{}\"\n",
            big.display()
        ),
    )
    .unwrap();
    assert!(refuses_to_start(&[
        "host",
        "--config",
        sources.to_str().unwrap()
    ]));
    assert!(refuses_to_start(&["host", "--attach-rows", "0"]));
    let data = common::data_dir();
    assert!(refuses_to_start(&[
        data.to_str().unwrap(),
        "--attach-rows",
        "5"
    ]));
    let attach_in_server = dir.join("server.toml");
    std::fs::write(
        &attach_in_server,
        format!(
            "[attach]\nrows = 4\n\n[[source]]\nname = \"r\"\ntype = \"file\"\npath = \"{}\"\n",
            big.display()
        ),
    )
    .unwrap();
    assert!(refuses_to_start(&[
        "--config",
        attach_in_server.to_str().unwrap()
    ]));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_session_that_must_have_memory_ceilings_refuses_to_start_without_them() {
    // A host running many sessions in one container relies on each engine and loader being
    // killed past its ceiling, and checked `limits.memory_ceiling` for a number in every `hello`.
    // `--require-memory-ceilings` says so once, at startup: where the ceilings are enforced
    // (Linux, which has the `/proc` they are read from) it changes nothing, and elsewhere the
    // session does not start (`ceilings_that_cannot_be_enforced_are_refused_when_required`).
    if cfg!(target_os = "linux") {
        let mut h = Host::spawn(&["--require-memory-ceilings"]);
        let r = h.call("hello", json!({"protocol": [1, 2, 3, 4, 5, 6]}));
        assert_eq!(r["ok"], true, "{r}");
        assert!(r["limits"]["memory_ceiling"].is_u64(), "{r}");
        assert!(r["limits"]["loader_memory_ceiling"].is_u64(), "{r}");
        let data = common::data_dir();
        let mut s = common::Server::start(&data, &["--require-memory-ceilings"]);
        assert!(s.alive(), "the server serves too");
        let _ = std::fs::remove_dir_all(&data);
    } else {
        let out = Command::new(common::BIN)
            .args(["host", "--allow-unsandboxed", "--require-memory-ceilings"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{err}");
        assert!(err.contains("--require-memory-ceilings"), "{err}");
    }
}

/// Three meters, hourly, for three days: `at`, `meter`, `kw`.
fn meters() -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new(
            "at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("meter", DataType::Utf8, false),
        Field::new("kw", DataType::Float64, true),
    ]);
    let (mut at, mut meter, mut kw) = (vec![], vec![], vec![]);
    for i in 0..(3 * 24) {
        for m in ["a", "b", "c"] {
            at.push(1_767_225_600_000_000i64 + i * 3_600_000_000);
            meter.push(m);
            kw.push(1.0 + i as f64);
        }
    }
    RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(arrow::array::TimestampMicrosecondArray::from(at).with_timezone("UTC"))
                as ArrayRef,
            Arc::new(StringArray::from(meter)) as ArrayRef,
            Arc::new(Float64Array::from(kw)) as ArrayRef,
        ],
    )
    .unwrap()
}

#[test]
fn a_host_states_the_shape_of_what_it_attaches() {
    // Protocol 3: a host names the time column and the series key, as an operator's config can,
    // and `describe` says what they are rather than what they look like. A name the stream does
    // not have, or a time column that is not a time, is refused before a row is written, and
    // changes nothing.
    let dir = unique("host-stated");
    let file = arrows(&dir.join("m.arrows"), &meters(), true);
    let (mut h, _) = hello_v3(&[], json!({}));
    let r = h.call(
        "attach",
        json!({"name": "load", "path": file, "time_column": "at", "series": ["meter"]}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let text = h.call("describe", json!({}))["text"]
        .as_str()
        .unwrap()
        .to_string();
    for want in [
        "time column at: 2026-01-01 00:00:00+00 to 2026-01-03 23:00:00+00",
        "series key meter: 3 series ('a', 'b', 'c')",
    ] {
        assert!(text.contains(want), "expected {want:?} in:\n{text}");
    }
    assert!(!text.contains("looks like"), "{text}");

    for (what, fields, want, says) in [
        (
            "no such time column",
            json!({"time_column": "when"}),
            "E_DATA",
            "`when`, which the stream does not have",
        ),
        (
            "a time column that is not a time",
            json!({"time_column": "kw"}),
            "E_DATA",
            "`kw`, which is `float64`",
        ),
        (
            "no such series column",
            json!({"series": ["site"]}),
            "E_DATA",
            "`site`, which the stream does not have",
        ),
        (
            "a column both",
            json!({"time_column": "at", "series": ["at"]}),
            "E_REQUEST",
            "`at`",
        ),
        (
            "a series named twice",
            json!({"series": ["meter", "meter"]}),
            "E_REQUEST",
            "twice",
        ),
        (
            "an empty series",
            json!({"series": []}),
            "E_REQUEST",
            "series",
        ),
        (
            "a series not a list",
            json!({"series": "meter"}),
            "E_REQUEST",
            "series",
        ),
        (
            "a time column not a string",
            json!({"time_column": 1}),
            "E_REQUEST",
            "time_column",
        ),
    ] {
        let mut req = fields;
        req["name"] = "load".into();
        req["path"] = file.to_str().unwrap().into();
        let r = h.call("attach", req);
        assert_eq!(code(&r), want, "{what}: {r}");
        let message = r["error"]["message"].as_str().unwrap();
        assert!(message.contains(says), "{what}: {message}");
    }
    let r = h.call("describe", json!({}));
    assert_eq!(r["tables"].as_array().unwrap().len(), 1, "{r}");
    assert!(
        r["text"].as_str().unwrap().contains("series key meter:"),
        "{r}"
    );

    // On an entry of `tables`, and on a stream.
    let r = h.call(
        "attach",
        json!({"tables": [{"name": "one", "path": file, "time_column": "at"},
                          {"name": "two", "path": file, "series": ["nope"]}]}),
    );
    let each = each(&r);
    assert_eq!(each[0]["ok"], true, "{r}");
    assert_eq!(each[1]["error"]["code"], "E_DATA", "{r}");
    let mut s = common::streamed::Streamer::start(&[], &[3]);
    let (r, _) = s.attach_stream_with(
        json!({"name": "load", "stream": true, "time_column": "at", "series": ["meter"]}),
        std::fs::read(&file).unwrap(),
    );
    assert_eq!(r["ok"], true, "{r}");
    let streamed = s.call("describe", json!({}), &[]);
    assert_eq!(
        streamed["text"], text,
        "the same data with the same stated shape, from a file and a pipe"
    );

    // Protocol 2 has no stated shape.
    let mut v2 = Host::start_v2(&[]);
    let r = v2.call(
        "attach",
        json!({"name": "load", "path": file, "time_column": "at"}),
    );
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A host's config holding `helpers` (REQ-18), in `dir`.
fn helpers_config(dir: &Path, helpers: &str) -> PathBuf {
    std::fs::write(dir.join("helpers.sql"), helpers).unwrap();
    let cfg = dir.join("host.toml");
    std::fs::write(&cfg, "[helpers]\nfile = \"helpers.sql\"\n").unwrap();
    cfg
}

const HOST_HELPERS: &str = "-- Rows per meter.\n\
    CREATE VIEW per_meter AS SELECT meter, count(*) AS n FROM readings GROUP BY ALL;\n\
    -- Twice a number.\n\
    CREATE MACRO twice(x) AS 2 * x;\n";

#[test]
fn a_view_over_a_missing_table_errors_until_attached() {
    // REQ-18: a host's helpers are made before its first attach. One that reads a table the host
    // has not attached yet waits: calling it says what for, and the attach of that table makes it.
    let dir = unique("host-helpers");
    let cfg = helpers_config(&dir, HOST_HELPERS);
    let mut h = Host::start(&["--config", cfg.to_str().unwrap()]);
    let r = h.call("query", json!({"sql": "SELECT twice(21) AS answer"}));
    assert!(r["text"].as_str().unwrap_or("").contains("42"), "{r}");
    let r = h.call("query", json!({"sql": "SELECT * FROM per_meter"}));
    assert_eq!(code(&r), "E_SQL", "{r}");
    let said = r["error"]["message"].as_str().unwrap();
    assert!(
        said.contains("`per_meter` is a helper")
            && said.contains("`readings`, which is not on the bench yet"),
        "{said}"
    );
    let d = h.call("describe", json!({}));
    assert!(
        d["text"]
            .as_str()
            .unwrap()
            .contains("per_meter — view: Rows per meter. (works once `readings` is on the bench)"),
        "{d}"
    );

    let r = attach(
        &mut h,
        "readings",
        &arrows(&dir.join("a.arrows"), &readings(4, "a"), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 700 AS k FROM per_meter"}),
    );
    assert!(r["text"].as_str().unwrap_or("").contains("704"), "{r}");
    let d = h.call("describe", json!({}));
    assert!(
        d["text"]
            .as_str()
            .unwrap()
            .contains("per_meter (meter, n) — view: Rows per meter.\n"),
        "{d}"
    );

    // Taken away, the table is missing again, and the view says which table.
    let r = h.call("detach", json!({"name": "readings"}));
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call("query", json!({"sql": "SELECT * FROM per_meter"}));
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("readings"),
        "{r}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_host_cannot_attach_under_a_helpers_name() {
    // REQ-18: a helper's name is taken from the start, a waiting one's too, so no table the host
    // attaches later hides it or is hidden by it.
    let dir = unique("host-helper-name");
    let cfg = helpers_config(&dir, HOST_HELPERS);
    let mut h = Host::start(&["--config", cfg.to_str().unwrap()]);
    let file = arrows(&dir.join("a.arrows"), &readings(2, "a"), true);
    for name in ["twice", "per_meter"] {
        let r = attach(&mut h, name, &file);
        assert_eq!(code(&r), "E_NAME", "{name}: {r}");
        assert!(
            r["error"]["message"].as_str().unwrap().contains("helper"),
            "{r}"
        );
    }
    let r = attach(&mut h, "readings", &file);
    assert_eq!(r["ok"], true, "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Helpers that read three tables. Each waits for `readings` at first, the first table DuckDB
/// finds missing in each; attached all at once, the others are known only from what DuckDB made
/// of each helper: `sites` from a join in one view's bound plan, `meters` from a scalar subquery
/// in another's. A helper macro reads no table
/// (`a_helper_macro_that_reads_a_table_is_refused_at_startup`).
const READING_HELPERS: &str = "-- Rows per meter.\n\
    CREATE VIEW per_meter AS SELECT meter, count(*) AS n FROM readings GROUP BY ALL;\n\
    -- Readings at a site.\n\
    CREATE VIEW at_sites AS SELECT r.meter FROM readings r, sites s WHERE r.id = s.id;\n\
    -- Readings and meters.\n\
    CREATE VIEW counted AS \
    SELECT (SELECT count(*) FROM readings) + (SELECT count(*) FROM meters) AS c;\n";

/// Attach `names` from `file` in one request, each answered ok.
fn attach_all(h: &mut Host, names: &[&str], file: &Path) {
    let tables: Vec<Value> = names
        .iter()
        .map(|n| json!({"name": n, "path": file}))
        .collect();
    let r = h.call("attach", json!({ "tables": tables }));
    for (name, a) in names.iter().zip(each(&r)) {
        assert_eq!(a["ok"], true, "{name}: {r}");
    }
}

/// A query that wedges DuckDB's planner past any deadline, so the engine is killed and rebuilt.
fn wedge() -> String {
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    format!("WITH {} SELECT m FROM l18", ctes.join(", "))
}

#[test]
fn a_table_a_helper_reads_stays_taken_after_a_detach() {
    // REQ-18, from a host's report on Linux: once `readings` was detached, the agent's `CREATE
    // TABLE readings AS SELECT 'FORGED' …` was kept, `per_meter` then served the agent's rows,
    // and the host's attach of `readings` again was refused with E_NAME. A table a helper reads
    // is the helper's, on the bench or not.
    let dir = unique("host-helper-reads");
    let cfg = helpers_config(&dir, READING_HELPERS);
    let mut h = Host::start_v2(&["--config", cfg.to_str().unwrap()]);
    let file = arrows(&dir.join("a.arrows"), &readings(4, "a"), true);
    let all = ["readings", "sites", "meters"];
    let ask = "SELECT (SELECT count(*) FROM per_meter) + 700 AS k, \
               (SELECT count(*) FROM at_sites) + 800 AS s, (SELECT c FROM counted) + 900 AS c";
    attach_all(&mut h, &all, &file);
    let r = h.call("query", json!({ "sql": ask }));
    let text = r["text"].as_str().unwrap_or("");
    assert!(
        text.contains("704") && text.contains("804") && text.contains("908"),
        "{r}"
    );
    for name in all {
        let r = h.call("detach", json!({ "name": name }));
        assert_eq!(r["ok"], true, "{name}: {r}");
    }

    for (sql, name) in [
        (
            "CREATE TABLE readings AS SELECT 'FORGED' AS meter FROM range(999)",
            "readings",
        ),
        (
            "CREATE TEMP TABLE READINGS AS SELECT 'FORGED' AS meter",
            "READINGS",
        ),
        (
            "CREATE VIEW readings AS SELECT 'FORGED' AS meter",
            "readings",
        ),
        ("CREATE TABLE sites AS SELECT 0 AS id", "sites"),
        ("CREATE TEMP VIEW meters AS SELECT 0 AS id", "meters"),
    ] {
        let r = h.call("query", json!({ "sql": sql }));
        assert_eq!(code(&r), "E_SQL", "{sql}: {r}");
        let said = r["error"]["message"].as_str().unwrap_or("");
        assert!(
            said.contains(&format!("`{name}` is taken")) && said.contains("helper"),
            "{sql}: {said}"
        );
    }
    // Made under another name and renamed into it, the same.
    let r = h.call(
        "query",
        json!({"sql": "CREATE TABLE mine AS SELECT 'FORGED' AS meter"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "ALTER TABLE mine RENAME TO readings"}),
    );
    assert_eq!(code(&r), "E_SQL", "{r}");
    let r = h.call("query", json!({"sql": "SELECT * FROM per_meter"}));
    assert!(
        !r.to_string().contains("FORGED"),
        "the helper serves no row of the agent's: {r}"
    );

    // The host's tables come back under their own names, and the helpers read them.
    for name in all {
        let r = attach(&mut h, name, &file);
        assert_eq!(r["ok"], true, "{name}: {r}");
    }
    let r = h.call("query", json!({ "sql": ask }));
    let text = r["text"].as_str().unwrap_or("");
    assert!(
        text.contains("704") && text.contains("804") && text.contains("908"),
        "{r}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_table_a_waiting_helper_needs_is_taken() {
    // REQ-18, from the same report: `describe` told the agent `per_meter` "works once `readings`
    // is on the bench", the agent made a `readings` of its own, and the host's attach of it was
    // refused with E_NAME, so the helper was never made.
    let dir = unique("host-helper-waits");
    let cfg = helpers_config(&dir, HOST_HELPERS);
    let mut h = Host::start(&["--config", cfg.to_str().unwrap()]);
    let r = h.call(
        "query",
        json!({"sql": "CREATE TABLE readings AS SELECT 1 AS meter"}),
    );
    assert_eq!(code(&r), "E_SQL", "{r}");
    let said = r["error"]["message"].as_str().unwrap_or("");
    assert!(
        said.contains("`readings` is taken") && said.contains("helper"),
        "{said}"
    );
    let r = attach(
        &mut h,
        "readings",
        &arrows(&dir.join("a.arrows"), &readings(4, "a"), true),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 700 AS k FROM per_meter"}),
    );
    assert!(r["text"].as_str().unwrap_or("").contains("704"), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_tables_helpers_read_stay_taken_across_a_rebuild() {
    // The tables the helpers read are the server's to keep, not the engine's: a rebuilt engine
    // holds them taken as the one before did, a detached one too.
    let dir = unique("host-helper-reads-rebuild");
    let cfg = helpers_config(&dir, READING_HELPERS);
    let mut h = Host::start_v2(&[
        "--config",
        cfg.to_str().unwrap(),
        "--deadline",
        "1",
        "--hard-grace",
        "1",
    ]);
    let file = arrows(&dir.join("a.arrows"), &readings(4, "a"), true);
    attach_all(&mut h, &["readings", "sites", "meters"], &file);
    let r = h.call("detach", json!({"name": "sites"}));
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call("query", json!({ "sql": wedge() }));
    assert_eq!(code(&r), "E_REBUILT", "{r}");
    let r = h.call(
        "query",
        json!({"sql": "CREATE TABLE sites AS SELECT 'FORGED' AS s"}),
    );
    assert_eq!(code(&r), "E_SQL", "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_table_the_agent_had_first_stays_its_own() {
    // DuckDB names a waiting helper's missing tables one at a time: `joined` waits for
    // `readings`, and only once `readings` is there does it say it reads `weather` too. The
    // agent made a `weather` before that, while the name was free, and a name is never taken
    // after the agent has it: its later statements are not refused for it. The helper, made only
    // from the bench's tables, waits on and reads nothing of the agent's; the host's attach of
    // `weather` is refused until the agent's is gone.
    let dir = unique("host-helper-reads-first");
    let cfg = helpers_config(
        &dir,
        "CREATE VIEW joined AS SELECT r.meter FROM readings r, weather w WHERE r.id = w.id;\n",
    );
    let mut h = Host::start(&["--config", cfg.to_str().unwrap()]);
    let file = arrows(&dir.join("a.arrows"), &readings(4, "a"), true);
    let r = h.call(
        "query",
        json!({"sql": "CREATE TABLE weather AS SELECT 'FORGED' AS meter, 0 AS id"}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let r = attach(&mut h, "readings", &file);
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "CREATE TABLE later AS SELECT 1 AS n"}),
    );
    assert_eq!(r["ok"], true, "the agent's weather does not stop it: {r}");
    let r = h.call("query", json!({"sql": "SELECT * FROM joined"}));
    assert_eq!(code(&r), "E_SQL", "{r}");
    assert!(!r.to_string().contains("FORGED"), "{r}");
    let r = attach(&mut h, "weather", &file);
    assert_eq!(code(&r), "E_NAME", "{r}");

    let r = h.call("query", json!({"sql": "DROP TABLE weather"}));
    assert_eq!(r["ok"], true, "{r}");
    let r = attach(&mut h, "weather", &file);
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 700 AS k FROM joined"}),
    );
    assert!(r["text"].as_str().unwrap_or("").contains("704"), "{r}");
    let r = h.call("detach", json!({"name": "weather"}));
    assert_eq!(r["ok"], true, "{r}");
    let r = h.call(
        "query",
        json!({"sql": "CREATE TABLE weather AS SELECT 'FORGED' AS meter, 0 AS id"}),
    );
    assert_eq!(code(&r), "E_SQL", "taken once the agent's is gone: {r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_system_view_name_is_taken() {
    // From the same report: an agent view named `sqlite_master` or `duckdb_tables` was kept, and
    // answered for the system view of that name wherever the agent named it unqualified. Every
    // view of DuckDB's `system` catalog, in each of its schemas, is taken.
    let mut h = Host::start(&[]);
    for (sql, name) in [
        (
            "CREATE VIEW sqlite_master AS SELECT 'FORGED' AS name",
            "sqlite_master",
        ),
        (
            "CREATE TEMP TABLE duckdb_tables AS SELECT 'FORGED' AS table_name",
            "duckdb_tables",
        ),
        (
            "CREATE VIEW pragma_database_list AS SELECT 'FORGED' AS name",
            "pragma_database_list",
        ),
        (
            "CREATE VIEW Pg_Class AS SELECT 'FORGED' AS relname",
            "Pg_Class",
        ),
        (
            "CREATE TEMP VIEW information_schema_tables AS SELECT 1 AS n",
            "",
        ),
    ] {
        let r = h.call("query", json!({ "sql": sql }));
        if name.is_empty() {
            // A name no system view has stays free.
            assert_eq!(r["ok"], true, "{sql}: {r}");
            continue;
        }
        assert_eq!(code(&r), "E_SQL", "{sql}: {r}");
        let said = r["error"]["message"].as_str().unwrap_or("");
        assert!(
            said.contains(&format!("`{name}` is taken")) && said.contains("system view"),
            "{sql}: {said}"
        );
    }
    let r = h.call(
        "query",
        json!({"sql": "SELECT count(*) + 900 AS n FROM sqlite_master WHERE name = 'FORGED'"}),
    );
    assert!(r["text"].as_str().unwrap_or("").contains("900"), "{r}");
}

#[test]
fn result_ids_continue_across_a_rebuild() {
    // A result id names one answer for the whole session: the engine a rebuild starts counts on
    // from the last id the session issued, so an id from before can only fail to resolve, never
    // resolve to a later answer.
    let mut h = Host::start(&["--deadline", "1", "--hard-grace", "1"]);
    let mut before = Vec::new();
    for i in 0..3 {
        let r = h.call("query", json!({"sql": format!("SELECT {i} + 900 AS n")}));
        before.push(r["result_ids"][0].as_str().unwrap().to_string());
    }
    let r = h.call("query", json!({ "sql": wedge() }));
    assert_eq!(code(&r), "E_REBUILT", "{r}");
    for i in 0..3 {
        let r = h.call("query", json!({"sql": format!("SELECT 'after {i}' AS n")}));
        let after = r["result_ids"][0].as_str().unwrap().to_string();
        assert!(
            !before.contains(&after),
            "{after} was issued before the rebuild: {before:?}"
        );
    }
    for id in &before {
        let r = h.call("page", json!({ "result_id": id }));
        assert_eq!(code(&r), "E_NO_RESULT", "{id}: {r}");
    }
}
