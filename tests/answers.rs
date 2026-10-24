//! What a host can act on without parsing benchy's prose (docs/HOST-PROTOCOL.md): the rows of each
//! result of a `query`, the SQL behind a retained result, and the names benchy's texts give the
//! agent's tools.

mod common;

use arrow::array::{ArrayRef, Int64Array, RecordBatch};
use arrow::datatypes::{DataType, Field, Schema};
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
    fn spawn(args: &[&str]) -> Host {
        let mut child = Command::new(common::BIN)
            .arg("host")
            .args(common::PLATFORM)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Host {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            next: 100,
        }
    }

    /// A session greeted with `hello`'s `fields`, and the reply.
    fn hello(args: &[&str], fields: Value) -> (Host, Value) {
        let mut h = Host::spawn(args);
        let r = h.call("hello", fields);
        (h, r)
    }

    /// A session greeted, speaking the newest protocol this build does.
    fn start(args: &[&str]) -> Host {
        let (h, r) = Host::hello(args, json!({"protocol": [1, 2, 3, 4]}));
        assert_eq!(r["ok"], true, "{r}");
        h
    }

    fn call(&mut self, call: &str, fields: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let mut req = fields;
        req["id"] = id.into();
        req["call"] = call.into();
        write_frame(&mut self.stdin, &req).unwrap();
        let reply: Value = read_frame(&mut self.stdout).expect("a reply");
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    fn query(&mut self, sql: &str, caps: Value) -> Value {
        self.call("query", json!({"sql": sql, "caps": caps}))
    }

    /// Whether the session has ended within a few seconds.
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
}

fn code(reply: &Value) -> &str {
    reply["error"]["code"].as_str().unwrap_or("")
}

fn text(reply: &Value) -> String {
    reply["text"]
        .as_str()
        .or(reply["error"]["message"].as_str())
        .unwrap_or_else(|| panic!("no text: {reply}"))
        .to_string()
}

/// An Arrow stream of `rows` rows in `dir`, as a host writes one.
fn arrows(dir: &Path, name: &str, rows: i64) -> PathBuf {
    let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())) as ArrayRef],
    )
    .unwrap();
    let mut buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    let path = dir.join(format!("{name}.arrows"));
    std::fs::write(&path, buf).unwrap();
    path
}

#[test]
fn a_query_reply_counts_the_rows_of_each_result() {
    // A host that adds its own hint to an empty result, or says how much its agent did not see,
    // reads it here rather than in the footer, whose wording may change between releases.
    let mut h = Host::start(&[]);
    let r = h.query(
        "SELECT 1 AS x; SELECT 1 AS x WHERE false; SELECT * FROM range(500); \
         SELECT * FROM range(5000); SELECT repeat('x', 400) AS long",
        json!({"max_rows": 50}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let results = r["results"].as_array().unwrap_or_else(|| panic!("{r}"));
    let ids: Vec<&Value> = results.iter().map(|x| &x["result_id"]).collect();
    let expected: Vec<&Value> = r["result_ids"].as_array().unwrap().iter().collect();
    assert_eq!(ids, expected, "one entry a result, in order: {r}");
    let facts: Vec<Value> = results
        .iter()
        .map(|x| json!([x["rows"], x["complete"], x["shown"], x["truncated"]]))
        .collect();
    assert_eq!(
        facts,
        vec![
            json!([1, true, 1, false]),
            // empty, and all of it: what a host's own empty-result hint looks for
            json!([0, true, 0, false]),
            // one page of fifty shown
            json!([500, true, 50, true]),
            // more rows than are kept for paging: `rows` is what is kept, and not all
            json!([2000, false, 50, true]),
            // every row shown, a value cut at the cell cap
            json!([1, true, 1, true]),
        ],
        "{r}"
    );
    // A result the byte cap left out of the text was shown nowhere.
    let r = h.query(
        &"SELECT * FROM range(40); ".repeat(8),
        json!({"max_bytes": 512}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let results = r["results"].as_array().unwrap();
    assert_eq!(results.len(), 8, "{r}");
    let last = results.last().unwrap();
    assert_eq!(last["rows"], 40, "{r}");
    assert_eq!(last["shown"], 0, "{r}");
    assert_eq!(last["truncated"], true, "{r}");
    // A failed batch is refused as before: its results are in the message, as the agent sees them.
    let r = h.query("SELECT 1; SELECT nope", json!({}));
    assert_eq!(code(&r), "E_SQL", "{r}");
    assert!(r.get("results").is_none(), "{r}");
}

#[test]
fn a_page_says_the_sql_of_its_result() {
    // A host whose tool returns a result with its SQL keeps no mapping of its own. The SQL is the
    // agent's statement as it wrote it, one statement of a batch, and nothing an operator's helper
    // holds: the body of a helper the statement calls is not in it.
    let dir = common::unique("answers-sql");
    std::fs::create_dir_all(&dir).unwrap();
    let toml = dir.join("host.toml");
    std::fs::write(
        &toml,
        "[helpers]\nsql = \"\"\"\n\
         CREATE MACRO doubled(x) AS x * 2 + length('helper-body-secret') - 18;\n\
         CREATE VIEW numbers AS SELECT 'helper-view-secret' AS tag, range AS n FROM range(3);\n\
         \"\"\"\n",
    )
    .unwrap();
    let mut h = Host::start(&["--config", toml.to_str().unwrap()]);
    let r = h.query(
        "SELECT doubled(21) AS answer;\n  -- the agent's own comment\n  SELECT n FROM numbers ORDER BY n",
        json!({}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let ids = r["result_ids"].as_array().unwrap().clone();
    assert_eq!(ids.len(), 2, "{r}");
    let first = h.call("page", json!({"result_id": ids[0]}));
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(first["sql"], "SELECT doubled(21) AS answer", "{first}");
    let second = h.call("page", json!({"result_id": ids[1], "offset": 1}));
    assert_eq!(
        second["sql"], "SELECT n FROM numbers ORDER BY n",
        "{second}"
    );
    for p in [&first, &second] {
        let s = p.to_string();
        assert!(!s.contains("secret"), "no helper's body: {s}");
    }
    // A `;` in a dollar-quoted string is the string's. The split did not know the form and found
    // two statements where DuckDB found one, so the SQL was left out; the split is now DuckDB's
    // (`page_sql_keeps_every_literal_whole`), and the statement is given whole.
    let r = h.query("SELECT $$a;b$$ AS x", json!({}));
    assert_eq!(r["ok"], true, "{r}");
    let p = h.call("page", json!({"result_id": r["result_ids"][0]}));
    assert_eq!(p["ok"], true, "{p}");
    assert_eq!(p["sql"], "SELECT $$a;b$$ AS x", "{p}");
    // Bounded like the text: a statement longer than the reply may be is cut, and says so.
    let long = format!("SELECT '{}' AS padding", "y".repeat(3000));
    let r = h.query(&long, json!({}));
    let id = r["result_ids"][0].clone();
    let p = h.call("page", json!({"result_id": id, "caps": {"max_bytes": 512}}));
    let sql = p["sql"].as_str().unwrap_or_else(|| panic!("{p}"));
    assert!(
        benchy_mcp::result::wire_len(sql) <= 512 && sql.contains("cut"),
        "{sql}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_result_keeps_64_kib_of_its_sql() {
    // A request may be 1 MiB and 32 results are kept, so whole statements could hold 32 MiB of
    // the engine to say what a page shows a few KiB of. 64 KiB of each is kept (`RETAIN_SQL`),
    // the number written here so that a change to it is caught: a statement of 65,536 bytes
    // comes back whole, one a byte longer cut, through a session whose pages may be larger.
    let mut h = Host::start(&["--max-bytes", "200000"]);
    let statement = |len: usize| {
        let pad = len - "SELECT '' AS v".len();
        format!("SELECT '{}' AS v", "y".repeat(pad))
    };
    for (len, whole) in [(65_536, true), (65_537, false)] {
        let sql = statement(len);
        let r = h.query(&sql, json!({}));
        assert_eq!(r["ok"], true, "{len}");
        let p = h.call("page", json!({"result_id": r["result_ids"][0]}));
        let kept = p["sql"].as_str().unwrap_or_else(|| panic!("{len}"));
        if whole {
            assert_eq!(kept, sql, "{len}");
        } else {
            assert!(
                kept.len() <= 65_536 && kept.ends_with("… [cut at the byte cap]"),
                "{len}: {} bytes, ending {:?}",
                kept.len(),
                &kept[kept.len().saturating_sub(40)..]
            );
        }
    }
}

#[test]
fn a_page_cuts_values_at_its_own_cell_cap() {
    // #18: caps may lower the session's limits for one call, `page` among them. A page with
    // `max_cell_bytes` 16 showed all 200 bytes of a value kept at the query's 256, and did not
    // say TRUNCATED. Now it cuts each value as a query at 16 would have, and a value cut once
    // already when it was kept says what is missing of the value, not of the cut.
    let mut h = Host::start(&[]);
    let r = h.query(
        "SELECT repeat('x', 200) AS short, repeat('y', 300) AS long, 'ok' AS small",
        json!({}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let id = r["result_ids"][0].clone();
    let whole = text(&r);
    assert!(whole.contains(&"x".repeat(200)), "{whole}");
    assert!(whole.contains("[+44 bytes]"), "{whole}");

    let p = h.call(
        "page",
        json!({"result_id": id, "caps": {"max_cell_bytes": 16}}),
    );
    assert_eq!(p["ok"], true, "{p}");
    let page = text(&p);
    let row = page.lines().nth(1).unwrap_or_else(|| panic!("{page}"));
    assert_eq!(
        row,
        format!(
            "{}… [+184 bytes] | {}… [+284 bytes] | ok",
            "x".repeat(16),
            "y".repeat(16)
        ),
        "{page}"
    );
    assert!(page.contains("TRUNCATED"), "{page}");
    // The session's cap again: the result as it was kept.
    let p = h.call("page", json!({"result_id": id}));
    assert!(text(&p).contains(&"x".repeat(200)), "{p}");
    // A value exactly as long as the cap it was kept at was not cut then, and is cut now; and a
    // cut inside a character goes back to its start, as the query's does: 16 bytes is five '€'.
    let r = h.query(
        "SELECT repeat('z', 256) AS edge, repeat('€', 50) AS euro",
        json!({}),
    );
    let p = h.call(
        "page",
        json!({"result_id": r["result_ids"][0], "caps": {"max_cell_bytes": 16}}),
    );
    assert!(
        text(&p).contains(&format!(
            "{}… [+240 bytes] | €€€€€… [+135 bytes]",
            "z".repeat(16)
        )),
        "{p}"
    );
}

#[test]
fn a_host_lowers_max_bytes_to_512_and_no_further() {
    // docs/HOST-PROTOCOL.md: `max_bytes` stays at least 512, the floor `--max-bytes` has
    // (`the_response_cap_is_at_least_512_bytes`). Written as the number the docs give, so that a
    // change of the floor is caught here and not followed.
    let mut h = Host::start(&[]);
    let sql = "SELECT repeat('x', 5000) AS v FROM range(10)";
    let r = h.query(sql, json!({"max_bytes": 511}));
    assert_eq!(code(&r), "E_REQUEST", "{r}");
    assert!(text(&r).contains("must be at least 512"), "{r}");
    let r = h.query(sql, json!({"max_bytes": 512}));
    assert_eq!(r["ok"], true, "{r}");
    assert!(benchy_mcp::result::wire_len(&text(&r)) <= 512, "{r}");
    let id = r["result_ids"][0].clone();
    let p = h.call("page", json!({"result_id": id, "caps": {"max_bytes": 511}}));
    assert_eq!(code(&p), "E_REQUEST", "a page too: {p}");
    let p = h.call("page", json!({"result_id": id, "caps": {"max_bytes": 512}}));
    assert_eq!(p["ok"], true, "{p}");
}

#[test]
fn page_sql_keeps_every_literal_whole() {
    // #16: a `--` or `/*` inside a string is the string's. Comments were found by a scan that
    // knew `'…'` and `"…"` alone, so `SELECT $$a -- b$$ AS y, 'q' AS z` came back as
    // `SELECT $$a`, and `$$a/*b*/c$$` as `$$a c$$`: SQL for a replay or an audit log that is not
    // what ran. Each literal form DuckDB 1.5.5 lexes, and a quoted name, keeps its text; only the
    // comments around them go. The split now lexes as the table-function check does
    // (`the_literals_the_check_skips_are_duckdbs`), and a `;` in a dollar-quoted string, which
    // used to leave the SQL out, no longer does.
    let mut h = Host::start(&[]);
    for (batch, written) in [
        ("SELECT $$a/*b*/c$$ AS x", vec!["SELECT $$a/*b*/c$$ AS x"]),
        (
            "SELECT $$a -- b$$ AS y, 'q' AS z",
            vec!["SELECT $$a -- b$$ AS y, 'q' AS z"],
        ),
        (
            "SELECT $t$a;/*b*/ -- c$t$ AS x",
            vec!["SELECT $t$a;/*b*/ -- c$t$ AS x"],
        ),
        (
            "SELECT E'it\\'s /*b*/ -- c' AS x",
            vec!["SELECT E'it\\'s /*b*/ -- c' AS x"],
        ),
        (
            "SELECT 'a--b', 'it''s /*c*/' AS x",
            vec!["SELECT 'a--b', 'it''s /*c*/' AS x"],
        ),
        (
            "SELECT 1 AS \"a--b/*c*/;d\"",
            vec!["SELECT 1 AS \"a--b/*c*/;d\""],
        ),
        // A comment is dropped, a nested one whole, and the batch splits where DuckDB splits it,
        // a `;` in a dollar-quoted string or after a comment's quote included.
        (
            "SELECT 1 /* a /* b */ ' */ AS x; -- it's\nSELECT $$a;b$$ AS y",
            vec!["SELECT 1   AS x", "SELECT $$a;b$$ AS y"],
        ),
    ] {
        let r = h.query(batch, json!({}));
        assert_eq!(r["ok"], true, "{batch}: {r}");
        let ids = r["result_ids"].as_array().unwrap().clone();
        assert_eq!(ids.len(), written.len(), "{batch}: {r}");
        for (id, sql) in ids.iter().zip(&written) {
            let p = h.call("page", json!({"result_id": id}));
            assert_eq!(p["sql"], *sql, "{batch}: {p}");
        }
    }
}

/// The names a host gives the agent's tools, none of them benchy's own.
fn renamed() -> Value {
    json!({"get_result": "bench_get_result", "describe": "bench_describe",
           "query": "bench_query", "dialect": "bench_dialect"})
}

/// Whether `text` names one of benchy's own tools: each as a word on its own. `query` is an
/// ordinary word in these texts, so it is looked for where it names the tool.
fn names_a_benchy_tool(text: &str) -> Option<&'static str> {
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    for tool in ["get_result", "describe", "dialect"] {
        if words.contains(&tool) {
            return Some(tool);
        }
    }
    ["through query", "`query`", "call query"]
        .into_iter()
        .find(|t| text.contains(t))
}

#[test]
fn texts_name_the_tools_as_the_host_names_them() {
    // A host whose agent's tools have other names would otherwise send its agent to tools that do
    // not exist: every text that names a tool — a result's footer, a hint, `describe` — names it
    // as the host's `hello` does, in protocol 4.
    let dir = common::unique("answers-tools");
    std::fs::create_dir_all(&dir).unwrap();
    let (mut h, r) = Host::hello(
        &["--max-bytes", "600"],
        json!({"protocol": [1, 2, 3, 4], "tool_names": renamed()}),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["protocol"], 4, "{r}");
    for i in 0..6 {
        let path = arrows(&dir, &format!("t{i}"), 20 + i);
        let a = h.call(
            "attach",
            json!({"name": format!("table_{i}"), "path": path}),
        );
        assert_eq!(a["ok"], true, "{a}");
    }
    let mut seen: Vec<String> = Vec::new();
    // A page that is not all the rows, and results the byte cap left out.
    seen.push(text(
        &h.query("SELECT * FROM range(100)", json!({"max_rows": 5})),
    ));
    seen.push(text(&h.query(
        &"SELECT * FROM range(40); ".repeat(8),
        json!({"max_bytes": 512}),
    )));
    // Hints: a table that is not there, a column that is not, a file written.
    for sql in [
        "SELECT * FROM nowhere",
        "SELECT nope FROM table_0",
        "COPY (SELECT 1) TO 'out.csv'",
    ] {
        let r = h.query(sql, json!({}));
        assert_eq!(code(&r), "E_SQL", "{r}");
        seen.push(text(&r));
    }
    // A catalogue over its budget.
    seen.push(text(&h.call("describe", json!({}))));
    let must = [
        "bench_get_result",
        "bench_describe",
        "bench_query",
        "describe budget",
    ];
    let all = seen.join("\n---\n");
    for word in must {
        let word = word.replace("describe budget", "bench_describe budget");
        assert!(all.contains(&word), "`{word}` named somewhere: {all}");
    }
    for t in &seen {
        assert_eq!(names_a_benchy_tool(t), None, "{t}");
    }

    // Without the map, the texts name benchy's own tools, as they always have.
    let mut plain = Host::start(&[]);
    let t = text(&plain.query("SELECT * FROM range(100)", json!({"max_rows": 5})));
    assert_eq!(names_a_benchy_tool(&t), Some("get_result"), "{t}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tool_names_are_known_tools_plain_names_and_protocol_4() {
    // `tool_names` labels the agent's tools in benchy's texts and names nothing benchy resolves:
    // a key benchy has no tool for, a name that is not a plain identifier, two tools under one
    // name, or the map in a session that agreed an older protocol ends the session, as anything
    // wrong with `hello` does.
    let refused = [
        json!({"protocol": [4], "tool_names": {"get_results": "x"}}),
        json!({"protocol": [4], "tool_names": {"describe": "bench/describe"}}),
        json!({"protocol": [4], "tool_names": {"describe": "bench.describe"}}),
        json!({"protocol": [4], "tool_names": {"describe": "https://x"}}),
        json!({"protocol": [4], "tool_names": {"describe": ""}}),
        json!({"protocol": [4], "tool_names": {"describe": "1st"}}),
        json!({"protocol": [4], "tool_names": {"describe": "a b"}}),
        json!({"protocol": [4], "tool_names": {"describe": "x".repeat(65)}}),
        json!({"protocol": [4], "tool_names": {"describe": 7}}),
        json!({"protocol": [4], "tool_names": ["describe"]}),
        json!({"protocol": [4], "tool_names": {"describe": "same", "query": "same"}}),
        json!({"protocol": [4], "tool_names": {"describe": "query"}}),
        json!({"protocol": [1, 2, 3], "tool_names": {"describe": "bench_describe"}}),
    ];
    for hello in refused {
        let (mut h, r) = Host::hello(&[], hello.clone());
        assert_eq!(code(&r), "E_REQUEST", "{hello}: {r}");
        assert!(h.ended(), "{hello}: the session ended");
    }
    // Some of the tools may be renamed, and a name may have a dash; the rest keep benchy's.
    let (mut h, r) = Host::hello(
        &[],
        json!({"protocol": [4], "tool_names": {"get_result": "Bench-Page_2"}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    // One text that names both: the renamed tool, and one that keeps benchy's name. (This read
    // the unknown-table hint for `describe` until #27, which names no tool there.)
    let t = text(&h.query("COPY (SELECT 1) TO 'out.csv'", json!({})));
    assert!(
        t.contains("through query") && t.contains("Bench-Page_2"),
        "{t}"
    );
    let t = text(&h.query("SELECT * FROM range(100)", json!({"max_rows": 5})));
    assert!(t.contains("Bench-Page_2"), "{t}");
}

#[test]
fn a_long_tool_name_keeps_every_text_within_its_budget() {
    // A host's name may be 64 bytes where benchy's is 10: the room a text keeps back for the line
    // that names a tool grows with it, so the pointer is never what the byte cap cuts.
    let long = format!("t{}", "x".repeat(63));
    let (mut h, r) = Host::hello(
        &[],
        json!({"protocol": [4], "tool_names": {"get_result": long}}),
    );
    assert_eq!(r["ok"], true, "{r}");
    let t = text(&h.query(
        "SELECT repeat('v', 30) AS v FROM range(1000)",
        json!({"max_rows": 100, "max_bytes": 512}),
    ));
    assert!(benchy_mcp::result::wire_len(&t) <= 512, "{t}");
    assert!(t.contains(&format!("Read on with {long} ")), "{t}");
    assert!(t.contains(", or aggregate."), "the footer whole: {t}");

    // A table's profile, as `describe` renders it: what is over its room is counted in a closing
    // line naming the host's tool, and the room holds that line too, at every size.
    let name = format!("d{}", "x".repeat(63));
    let profile = benchy_mcp::profile::Profile {
        v: benchy_mcp::profile::FORMAT,
        lines: (0..30)
            .map(|i| format!("line {i}: {}", "w".repeat(40)))
            .collect(),
        ..Default::default()
    };
    let names: benchy_mcp::tools::Names = [("describe".to_string(), name.clone())]
        .into_iter()
        .collect();
    benchy_mcp::tools::set(&names);
    let mut closed = 0;
    for room in 0..2000 {
        if let Some(r) = profile.render(room) {
            assert!(benchy_mcp::result::wire_len(&r) <= room, "{room}: {r}");
            closed += r.contains(&format!("not shown: over the {name} budget")) as usize;
        }
    }
    benchy_mcp::tools::set(&benchy_mcp::tools::Names::new());
    assert!(closed > 0, "the closing line names the host's tool");
}
