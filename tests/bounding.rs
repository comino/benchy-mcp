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

/// The whole response, JSON-escaped as the client receives it, is what the cap bounds. The
/// corpus's nine shapes include one 1 MB value, 200 columns, 60 statements in a batch, and text
/// that escaping inflates sixfold.
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

/// Not an error: the result is there, and a host's `page` still gets its `sql`.
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

/// The data includes a 1,500-column table and a 20,000-byte column name.
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

/// Built whole, a `SELECT` with no `LIMIT` over 10,000,000 rows cost 0.63 G instructions and
/// 150 ms a call, for 2,000 rows kept.
#[test]
fn a_query_without_a_limit_reads_no_further_than_it_keeps() {
    // REQ-10. A query's result was built whole before its first 2,000 rows were kept, so a
    // `SELECT` with no `LIMIT` cost the whole result in time and memory: on 1,000,000 rows 80 ms
    // against 1 ms with a `LIMIT`, and on 10,000,000 an engine past its memory ceiling, killed
    // and rebuilt with the agent's tables. A trillion rows can never be built inside the
    // deadline, so before the result was streamed this was the deadline, or on Linux the memory
    // ceiling first; read only as far as it is kept, it answers at once, and the agent's table
    // made before it is still there.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--deadline", "5", "--memory", "128MB"]);
    let (err, body, _) = s.query("CREATE TEMP TABLE kept AS SELECT 7 AS v");
    assert!(!err, "{body}");
    let (err, body, took) =
        s.query("SELECT i, repeat('x', 100) || i AS s FROM range(1000000000000) t(i)");
    assert!(!err, "the rows kept are the answer: {took:?} {body}");
    assert!(
        body.contains("of 2000+"),
        "and it says more rows exist than are kept: {body}"
    );
    let (err, body, _) = s.query("SELECT v FROM kept");
    assert!(
        !err && body.contains('7'),
        "the engine was not rebuilt: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn a_query_without_a_limit_holds_no_more_than_it_keeps() {
    // REQ-10. The shape a host reported, through the shipped binary at `--memory 384MB`: every
    // row of ten million, four columns, no `LIMIT`. Built whole, the result took the engine's
    // peak resident memory up by 488 MB, most of the way to its ceiling (622 MiB at that budget);
    // a wider row went past it, and the engine was killed and rebuilt. Read only as far as it is
    // kept, the peak grows by what DuckDB's streaming buffer and a chunk or two hold.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--memory", "384MB"]);
    assert!(s.alive());
    let engine = common::engine_of(s.pid());
    let peak = |pid: u32| -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let kb: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap();
        kb * 1024
    };
    let before = peak(engine);
    let (err, body, _) = s.query(
        "SELECT TIMESTAMPTZ '2026-01-01 00:00:00+00' + to_minutes(i) AS ts, \
         'm' || lpad((i % 500)::VARCHAR, 3, '0') AS meter, (i % 997) * 1.5 AS kw, i AS id \
         FROM range(10000000) t(i)",
    );
    assert!(!err, "{body}");
    assert!(body.contains("of 2000+"), "{body}");
    assert_eq!(common::engine_of(s.pid()), engine, "the engine was rebuilt");
    let grew = peak(engine).saturating_sub(before);
    assert!(
        grew < LIMITLESS_PEAK,
        "the engine's peak resident memory grew by {grew} bytes for 2,000 rows kept"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the ten-million-row query may add to the engine's peak resident memory. Measured on the
/// development box (4 cores, DuckDB 1.5.5): built whole, 488 MB; streamed, 2.3 MB in three runs
/// (and 6–15 MiB over one to a hundred calls of a `SELECT *` over a loaded table of as many rows).
/// 128 MiB leaves a machine with more threads, and so more chunks in flight, room many times over,
/// and still fails a result built whole by a factor of almost four.
#[cfg(target_os = "linux")]
const LIMITLESS_PEAK: u64 = 128 << 20;

/// Reading on past the rows kept, to find such an error, was measured and not taken: it cost
/// more than building the result whole (0.88 G instructions and 178 ms a call over 10,000,000
/// rows), for rows the agent never sees, and a query over more rows than the deadline can read
/// would have become a deadline error instead of an answer. What counts as read is DuckDB's:
/// before the stream is handed over it runs until about 1 MB of the result is buffered, counted
/// by the width of the values' types, and an error there is raised.
#[test]
fn an_error_past_the_rows_kept_is_not_raised() {
    // REQ-10, and the price of reading no further than is kept: DuckDB computes the rows it is
    // asked for, and an error it would raise only in a row far past the ones kept is never
    // raised. The answer is the rows kept, and says more rows exist. Built whole, as before,
    // this query failed with `late-10000000`.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let (err, body, _) = s.query(
        "SELECT CASE WHEN i < 10000000 THEN i ELSE error('late-' || i) END AS v \
         FROM range(20000000) t(i)",
    );
    assert!(!err, "{body}");
    assert!(body.contains("of 2000+"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// One row in four of `range`, each 1,600 bytes as DuckDB holds it (`BIGINT[200]`), so that a
/// stream's first rows fill DuckDB's buffer and the rest of the 2,000 kept need a fetch.
///
/// Execution runs until about 1 MB of the result is buffered (`streaming_buffer_size`), counted
/// by the width of the values' types, not the length of their text: probed on DuckDB 1.5.5, a
/// `BIGINT` result of one row in 2,048, and 10 KB strings for 1,000 rows, never filled it, and an
/// error or an interrupt in rows that close came from the execution, before any fetch. Here 1,024
/// rows fill it, and the fetches after them are what runs on. Each cell renders cut to 256
/// bytes, so the 2,000 kept stay inside the 1 MiB retained.
const WIDE: &str = "list_resize([i], 200, i)::BIGINT[200] AS a";

#[test]
fn an_error_in_the_rows_kept_still_fails_the_query() {
    // REQ-10. Read as a stream, an error past the rows DuckDB buffered comes from the fetch of
    // a chunk rather than from the execution, and DuckDB's fetch answers it with no chunk, as it
    // answers the end of the rows. Taken for the end, this was 1,024 rows called complete.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let (err, body, _) = s.query(&format!(
        "SELECT CASE WHEN i < 7200 THEN i ELSE error('early-' || i) END AS v, {WIDE} \
         FROM range(100000000) t(i) WHERE i % 4 = 0"
    ));
    assert!(err, "{}", &body[body.len().saturating_sub(300)..]);
    assert!(body.contains("early-7200"), "{body}");
    assert!(s.alive());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_deadline_while_rows_are_read_is_the_deadline() {
    // REQ-7 and REQ-10. A query whose first rows come at once and the rest slowly is still
    // running as they are read, and the watchdog's interrupt lands in a fetch, which answers it
    // with no chunk, as it answers the end of the rows. Taken for the end, this was the first
    // 1,024 rows called complete.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--deadline", "2"]);
    let (err, body, _) = s.query(&format!(
        "SELECT {WIDE} FROM range(1000000000000) t(i) \
         WHERE (i < 7200 AND i % 4 = 0) OR i = 999999999999"
    ));
    assert!(err, "{}", &body[body.len().saturating_sub(300)..]);
    assert!(body.contains("deadline"), "{body}");
    assert!(s.alive());
    let _ = std::fs::remove_dir_all(&dir);
}
