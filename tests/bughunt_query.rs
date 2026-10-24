//! Regression tests found by the query/rendering bughunt.

mod common;

use benchy_mcp::{result::Cell, Caps, Workspace};

fn workspace() -> Workspace {
    common::bench().seal("1GB").expect("seal an empty bench")
}

fn cell(ws: &mut Workspace, sql: &str) -> String {
    let results = ws.work(sql, &Caps::default()).expect("query runs");
    assert!(results[0].partial.is_none(), "{sql}: {:?}", results[0]);
    match &results[0].rows[0][0] {
        Cell::Text(value) => value.clone(),
        Cell::Null => panic!("{sql} unexpectedly returned NULL"),
    }
}

/// REQ-10 says an unrenderable value must be marked instead of silently giving a wrong
/// value. The top-level BIT guard must apply to the same value inside a nested type.
#[test]
fn a_nested_bit_is_faithful_or_explicitly_unrenderable() {
    let mut ws = workspace();
    for sql in [
        "SELECT ['0101'::BIT]",
        "SELECT {'bits': '0101'::BIT}",
        "SELECT MAP {'bits': '0101'::BIT}",
    ] {
        let value = cell(&mut ws, sql);
        assert!(
            value.contains("0101") || value.contains("not rendered"),
            "{sql}: BIT must not appear as its internal byte encoding: {value}"
        );
    }
}

/// The top-level TIMETZ safeguard must not disappear when the value is in a list.
#[test]
fn a_nested_timetz_keeps_its_offset_or_says_it_cannot_render() {
    let mut ws = workspace();
    let value = cell(&mut ws, "SELECT [TIMETZ '10:00:00+02']");
    assert!(
        value.contains("+02") || value.contains("not rendered"),
        "the time zone offset was silently lost: {value}"
    );
}

/// BIGNUM's Arrow representation consists of DuckDB's internal bytes, including when
/// nested. Those bytes are not the number the user selected.
#[test]
fn a_nested_bignum_is_faithful_or_explicitly_unrenderable() {
    let mut ws = workspace();
    let value = cell(&mut ws, "SELECT [123456789012345678901234567890::BIGNUM]");
    assert!(
        value.contains("123456789012345678901234567890") || value.contains("not rendered"),
        "BIGNUM must not appear as its internal byte encoding: {value}"
    );
}

/// REQ-10's TRUNCATED notice applies to every page with cut cells, including the final
/// page returned by get_result. Reaching the last row does not restore missing cell bytes.
#[test]
fn the_last_page_still_reports_cut_values() {
    let dir = common::data_dir();
    let mut server = common::Server::start(&dir, &["--max-rows", "1", "--max-cell-bytes", "16"]);
    let (error, first, _) = server.query("SELECT repeat('x', 20) AS v FROM range(2)");
    assert!(!error, "{first}");
    let id = first
        .split("result_id ")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .expect("the initial page gives a result id");
    let reply = server.call(
        "tools/call",
        serde_json::json!({
            "name": "get_result",
            "arguments": {"result_id": id, "offset": 1}
        }),
    );
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let page = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("page text");
    assert!(page.contains("[+4 bytes]"), "the value is cut: {page}");
    assert!(
        page.contains("TRUNCATED"),
        "a final page with missing value bytes must report truncation: {page}"
    );
}

/// UTC instants are promised to use DuckDB's own VARCHAR spelling. Astronomical year 0
/// is 1 BC, and must not be shown as an ordinary AD year.
#[test]
fn a_bc_timestamptz_renders_as_duckdb_writes_it() {
    let mut ws = workspace();
    let direct = cell(&mut ws, "SELECT TIMESTAMPTZ '0001-01-01 (BC) 00:00:00+00'");
    let cast = cell(
        &mut ws,
        "SELECT TIMESTAMPTZ '0001-01-01 (BC) 00:00:00+00'::VARCHAR",
    );
    assert_eq!(direct, cast, "the same instant must have the same spelling");
}

/// HUGEINT can have 39 decimal digits. Passing it through Arrow must not silently drop
/// the final digit while reporting a complete, untruncated result.
#[test]
fn a_39_digit_hugeint_keeps_every_digit_or_says_it_cannot_render() {
    let mut ws = workspace();
    let expected = "170141183460469231731687303715884105727";
    let value = cell(&mut ws, &format!("SELECT '{expected}'::HUGEINT"));
    assert!(
        value == expected || value.contains("not rendered"),
        "HUGEINT must not silently lose decimal digits: expected {expected}, got {value}"
    );
}

/// An unsigned 128-bit value above the signed range must not become a negative number
/// in the agent's answer. The maximum UHUGEINT previously rendered as -1.
#[test]
fn an_uhugeint_above_the_signed_range_keeps_its_value_or_says_it_cannot_render() {
    let mut ws = workspace();
    let expected = "340282366920938463463374607431768211455";
    let value = cell(&mut ws, &format!("SELECT '{expected}'::UHUGEINT"));
    assert!(
        value == expected || value.contains("not rendered"),
        "UHUGEINT must not be rendered as signed: expected {expected}, got {value}"
    );
}

/// The machine-readable truncation flag must cover a header that the renderer has cut,
/// as well as cuts to rows and values.
#[test]
fn a_cut_column_name_marks_the_result_truncated() {
    let mut ws = workspace();
    let caps = Caps {
        max_cell_bytes: 16,
        ..Caps::default()
    };
    let results = ws.work("SELECT 1 AS abcdefghijklmnopq", &caps).unwrap();
    let (text, shown) = benchy_mcp::result::render_batch_shown(&results, &caps);
    assert!(text.contains("[+1 bytes]"), "the header is cut: {text}");
    assert!(
        shown[0].truncated,
        "a cut header must not be reported as a complete rendering: {text}; {shown:?}"
    );
}
