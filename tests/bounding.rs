//! REQ-10: what comes back is bounded as it goes on the wire, says when it is not everything,
//! and can be read further without running the query again.
//!
//! The cap is a promise about the *response*, so it is measured on the response, JSON-escaped:
//! escaping can grow text — a control character becomes six bytes — and an agent pays for the
//! escaped form.

mod common;
use common::{data_dir, Server};
use serde_json::json;

fn wire_len(s: &str) -> usize {
    serde_json::to_string(s).unwrap().len() - 2
}

fn get_result(s: &mut Server, id: &str, offset: Option<u64>) -> (bool, String) {
    let mut args = json!({ "result_id": id });
    if let Some(o) = offset {
        args["offset"] = json!(o);
    }
    let v = s.call(
        "tools/call",
        json!({"name": "get_result", "arguments": args}),
    );
    let r = &v["result"];
    let text: String = r["content"]
        .as_array()
        .map(|cs| cs.iter().filter_map(|c| c["text"].as_str()).collect())
        .unwrap_or_default();
    (r["isError"].as_bool().unwrap_or(false), text)
}

fn result_id(body: &str) -> String {
    body.split("result_id ")
        .nth(1)
        .and_then(|r| r.split(|c: char| !(c.is_alphanumeric() || c == '-')).next())
        .expect("a result id")
        .to_string()
}

/// The bounding corpus: shapes that each break a naive cap in a different way.
fn corpus() -> Vec<(&'static str, String)> {
    let many_columns = (1..=200)
        .map(|i| format!("repeat('x', 100) AS c{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let many_statements = (1..=60)
        .map(|i| format!("SELECT {i} AS n, repeat('y', 500) AS pad"))
        .collect::<Vec<_>>()
        .join("; ");
    vec![
        (
            "one wide value",
            "SELECT repeat('x', 1000000) AS wide".into(),
        ),
        (
            "many wide rows",
            "SELECT repeat('x', 5000) AS w FROM range(100)".into(),
        ),
        ("two hundred columns", format!("SELECT {many_columns}")),
        ("sixty statements", many_statements),
        (
            "escape-heavy text",
            "SELECT repeat('\"\\', 5000) AS q".into(),
        ),
        (
            "control characters",
            "SELECT repeat(chr(1), 5000) AS c".into(),
        ),
        ("multibyte text", "SELECT repeat('€', 100000) AS e".into()),
        ("a long list", "SELECT range(0, 20000) AS l".into()),
        (
            "a large blob",
            "SELECT repeat('z', 100000)::BLOB AS b".into(),
        ),
    ]
}

#[test]
fn cap_measured_on_the_wire() {
    let dir = data_dir();
    for cap in [8192usize, 1024] {
        let mut s = Server::start(&dir, &["--max-bytes", &cap.to_string()]);
        for (label, sql) in corpus() {
            let (err, body, _) = s.query(&sql);
            assert!(!err, "{label}: {body}");
            let n = wire_len(&body);
            assert!(
                n <= cap,
                "{label}: {n} bytes on the wire against a {cap} cap"
            );
            assert!(
                body.contains("TRUNCATED"),
                "{label}: a cut response must say so"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_full_page_is_not_called_truncated() {
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--max-rows", "200"]);
    let (_, body, _) = s.query("SELECT i FROM range(200) t(i)");
    assert!(
        !body.contains("TRUNCATED"),
        "exactly one page is complete: {}",
        &body[body.len().saturating_sub(200)..]
    );
    let (_, body, _) = s.query("SELECT i FROM range(201) t(i)");
    assert!(body.contains("TRUNCATED"), "one row more is not");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_truncated_result_can_be_paged() {
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--max-rows", "200"]);
    let (_, first, _) = s.query("SELECT i FROM range(500) t(i)");
    assert!(
        first.contains("TRUNCATED") && first.contains("offset 200"),
        "says how to read on: {first}"
    );
    let id = result_id(&first);

    let (err, second) = get_result(&mut s, &id, Some(200));
    assert!(!err, "{second}");
    let rows: Vec<&str> = second
        .lines()
        .filter(|l| l.trim().parse::<u64>().is_ok())
        .collect();
    assert_eq!(
        rows.first().map(|r| r.trim()),
        Some("200"),
        "page two starts where page one stopped"
    );
    assert_eq!(rows.len(), 200);

    let (err, last) = get_result(&mut s, &id, Some(400));
    assert!(!err, "{last}");
    let rows: Vec<&str> = last
        .lines()
        .filter(|l| l.trim().parse::<u64>().is_ok())
        .collect();
    assert_eq!(rows.last().map(|r| r.trim()), Some("499"));
    assert!(
        !last.contains("TRUNCATED"),
        "the last page is the end of the result: {last}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_page_past_the_end_says_so() {
    // An offset past the last row answered "rows 10001–10000 of 500": a range that runs
    // backwards and calls itself a page. It is an empty page that says where the rows end.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--max-rows", "200"]);
    let (_, first, _) = s.query("SELECT i FROM range(500) t(i)");
    let id = result_id(&first);
    for offset in [500, 10_000] {
        let (err, page) = get_result(&mut s, &id, Some(offset));
        assert!(!err, "{page}");
        assert!(
            page.contains(&format!(
                "no rows at offset {offset}: result_id {id} has 500 row(s), at offsets 0 to 499"
            )),
            "{page}"
        );
        assert!(
            !page.lines().any(|l| l.trim().parse::<u64>().is_ok()),
            "no row: {page}"
        );
        assert!(!page.contains(&format!("{}–", offset + 1)), "{page}");
    }
    let (_, empty, _) = s.query("SELECT i FROM range(0) t(i)");
    let id = result_id(&empty);
    let (err, page) = get_result(&mut s, &id, Some(5));
    assert!(!err, "{page}");
    assert!(
        page.contains(&format!("no rows at offset 5: result_id {id} has no rows")),
        "{page}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn result_ids_never_outlive_their_workspace() {
    // After a rebuild the old workspace is gone, so its ids must stop resolving — and a new
    // result must never reuse one, or an old id would silently name a different answer.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--deadline", "1", "--hard-grace", "1"]);
    let (_, body, _) = s.query("SELECT 'before' AS v");
    let before = result_id(&body);

    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let (err, body, _) = s.query(&format!("WITH {} SELECT m FROM l18", ctes.join(", ")));
    assert!(
        err && body.contains("rebuilt"),
        "the engine was rebuilt: {body}"
    );

    let (err, text) = get_result(&mut s, &before, None);
    assert!(
        err,
        "an id from the lost workspace must not resolve: {text}"
    );
    let (_, body, _) = s.query("SELECT 'after' AS v");
    assert_ne!(
        result_id(&body),
        before,
        "a new result must not reuse an old id"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn tool(s: &mut Server, name: &str, args: serde_json::Value) -> String {
    let v = s.call("tools/call", json!({"name": name, "arguments": args}));
    v["result"]["content"]
        .as_array()
        .map(|cs| cs.iter().filter_map(|c| c["text"].as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn every_tool_response_obeys_the_byte_cap() {
    // The cap held for `query` and for nothing else: `get_result` returned 24 times it, because
    // the empty-result note carried whole 40 KB values, and `describe` and `dialect` were never
    // measured at all. Every tool, at a small cap, against data made to break each one.
    let cap = 1024usize;
    let dir = common::unique("every-tool");
    std::fs::create_dir_all(&dir).unwrap();
    let wide: Vec<String> = (1..=1500).map(|i| format!("column_{i}")).collect();
    std::fs::write(
        dir.join("a_wide.csv"),
        format!("{}\n{}\n", wide.join(","), vec!["1"; 1500].join(",")),
    )
    .unwrap();
    let mut labels = String::from("label\n");
    for i in 0..5 {
        labels.push_str(&format!("{}{i}\n", "v".repeat(40_000)));
    }
    std::fs::write(dir.join("labels.csv"), labels).unwrap();

    let mut s = Server::start(&dir, &["--max-bytes", &cap.to_string()]);
    let check = |what: &str, text: &str| {
        let n = wire_len(text);
        assert!(
            n <= cap,
            "{what}: {n} bytes on the wire against a {cap} cap"
        );
    };
    check("describe", &tool(&mut s, "describe", json!({})));
    check("dialect", &tool(&mut s, "dialect", json!({})));

    let long_name = "n".repeat(20_000);
    for sql in [
        "SELECT * FROM labels WHERE label = 'absent'".to_string(),
        format!("SELECT 1 AS \"{long_name}\""),
        "SELECT * FROM a_wide".to_string(),
    ] {
        let (_, body, _) = s.query(&sql);
        check(&format!("query {}", &sql[..40.min(sql.len())]), &body);
        let id = result_id(&body);
        let (_, page) = get_result(&mut s, &id, None);
        check(
            &format!("get_result for {}", &sql[..40.min(sql.len())]),
            &page,
        );
        assert!(
            !page.contains("Read it with get_result"),
            "get_result must not send the agent back to get_result: {page}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
