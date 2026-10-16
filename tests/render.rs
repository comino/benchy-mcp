//! Values must arrive intact.
//!
//! A cell that renders empty while the data is present is the worst failure this product can
//! have. A refused query is visible; a wrong number is arguable; a **silently blank column** is
//! neither — the agent reasons over it, reports a confident answer, and nothing anywhere says
//! something went missing.
//!
//! This file exists because exactly that shipped: `TIMESTAMP WITH TIME ZONE` came back as an
//! empty string for every row, in the bundled example data, on the one column type a time-series
//! bench exists to hold.

mod common;

use benchy_mcp::{result::Cell, source, Caps, Workspace};

fn bench() -> Workspace {
    let d = common::unique("render");
    std::fs::create_dir(&d).expect("create this test's input directory");
    std::fs::write(
        d.join("t.csv"),
        "observed_at,meter,power_kw\n\
         2026-01-01T00:00:00Z,lab,2.0\n\
         2026-01-01T00:15:30Z,lab,4.0\n",
    )
    .unwrap();
    let mut b = common::bench();
    source::load_dir(&mut b, &d).expect("loads");
    let _ = std::fs::remove_dir_all(&d);
    b.seal("1GB").expect("seals")
}

fn cells(ws: &mut Workspace, sql: &str) -> Vec<String> {
    let out = ws.work(sql, &Caps::default()).expect("runs");
    out[0]
        .rows
        .iter()
        .map(|r| match &r[0] {
            Cell::Text(t) => t.clone(),
            Cell::Null => "<NULL>".into(),
        })
        .collect()
}

#[test]
fn a_timestamptz_arrives_intact() {
    let mut ws = bench();
    // The type the CSV loader actually produces for an ISO timestamp with an offset.
    let ty: String = match &ws
        .work(
            "SELECT typeof(observed_at) FROM t LIMIT 1",
            &Caps::default(),
        )
        .unwrap()[0]
        .rows[0][0]
    {
        Cell::Text(t) => t.clone(),
        Cell::Null => String::new(),
    };
    assert_eq!(ty, "TIMESTAMP WITH TIME ZONE", "the case under test");

    let direct = cells(&mut ws, "SELECT observed_at FROM t ORDER BY 1");
    let cast = cells(&mut ws, "SELECT observed_at::VARCHAR FROM t ORDER BY 1");
    assert_eq!(
        direct, cast,
        "selecting the column must give what casting it gives"
    );
    assert!(
        direct[0].starts_with("2026-01-01 00:00:00"),
        "and the actual instant: {direct:?}"
    );
    assert!(
        direct[1].starts_with("2026-01-01 00:15:30"),
        "including seconds: {direct:?}"
    );
}

#[test]
fn a_timestamptz_expression_arrives_intact_too() {
    let mut ws = bench();
    let v = cells(&mut ws, "SELECT TIMESTAMPTZ '1969-07-20 20:17:40+00'");
    assert!(
        v[0].starts_with("1969-07-20 20:17:40"),
        "dates before the epoch too: {v:?}"
    );
    let v = cells(
        &mut ws,
        "SELECT TIMESTAMPTZ '2026-03-01 12:00:00.123456+00'",
    );
    assert!(
        v[0].contains("12:00:00.123456"),
        "fractional seconds survive: {v:?}"
    );
}

#[test]
fn nested_types_render_rather_than_vanishing() {
    // These were blank under the legacy reader too. They are not a fallback case any more: the
    // Arrow path renders them, so the assertion is that the content is actually there.
    let mut ws = bench();
    for (sql, want) in [
        ("SELECT [1, 2] AS l", vec!["1", "2"]),
        ("SELECT {'a': 1} AS s", vec!["a", "1"]),
        ("SELECT MAP {'a': 1} AS m", vec!["a", "1"]),
        ("SELECT [{'k': 'v'}] AS nested", vec!["k", "v"]),
    ] {
        let v = cells(&mut ws, sql);
        assert!(
            !v[0].is_empty(),
            "{sql} rendered empty, which is indistinguishable from missing data"
        );
        for w in want {
            assert!(v[0].contains(w), "{sql} lost {w:?}: {v:?}");
        }
    }
}

#[test]
fn every_common_type_renders_as_something_a_reader_can_use() {
    // One sweep, because the failure mode being guarded is silence: a type that renders empty
    // does not announce itself anywhere.
    let mut ws = bench();
    for (sql, want) in [
        ("SELECT 1.5::DOUBLE", "1.5"),
        ("SELECT 42::INTEGER", "42"),
        ("SELECT 42::HUGEINT", "42"),
        ("SELECT 1.25::DECIMAL(9,2)", "1.25"),
        ("SELECT true", "true"),
        ("SELECT DATE '2026-01-01'", "2026-01-01"),
        ("SELECT TIME '10:00:00'", "10:00:00"),
        (
            "SELECT TIMESTAMP '2026-01-01 10:00:00'",
            "2026-01-01 10:00:00",
        ),
        ("SELECT INTERVAL 1 DAY", "1 day"),
        ("SELECT 'text'", "text"),
        ("SELECT '\x41'::BLOB", "41"),
        ("SELECT uuid()", "-"),
        ("SELECT NULL::INTEGER", "<NULL>"),
    ] {
        let v = cells(&mut ws, sql);
        assert!(
            v[0].contains(want),
            "{sql} should contain {want:?}, rendered {v:?}"
        );
    }
}

#[test]
fn a_value_arrow_cannot_carry_faithfully_says_so() {
    // BIT, BIGNUM and TIME WITH TIME ZONE come through Arrow as bytes or without their offset.
    // Rendering those bytes produces a confident wrong value — `04f5` for the bits 0101 — which
    // is worse than no value. Each must be either right or a marker that names the fix.
    let mut ws = bench();
    for (sql, right, wrong) in [
        ("SELECT '0101'::BIT", "0101", "04f5"),
        (
            "SELECT 123456789012345678901234567890::BIGNUM",
            "123456789012345678901234567890",
            "80000d",
        ),
        ("SELECT TIMETZ '10:00:00+02'", "+02", "__never__"),
    ] {
        let v = cells(&mut ws, sql);
        assert!(!v[0].contains(wrong), "{sql} rendered a wrong value: {v:?}");
        assert!(
            v[0].contains(right) || v[0].contains("::VARCHAR"),
            "{sql} should be right or name the fix, got {v:?}"
        );
    }
}
