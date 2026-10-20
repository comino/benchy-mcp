//! REQ-14: the `describe` tool tells the shape of the data — found once, at load.
//!
//! The fixture plants, in a week of 15-minute readings from seven meters, each quirk the public
//! meter data has (eval/REPORT.md, "the trap" column), plus the ones it happens not to have:
//! a dead series, a late starter, one that stops, one that pauses, a gap, repeated times, a gap
//! hidden by as many repeats, an hour every series reads 0 on a day clocks change, and an hour every series reads double.

mod common;

use benchy_mcp::profile::Shape;
use benchy_mcp::{source, Workspace};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Instant;

const MIN15: i64 = 15 * 60;
/// 2014-03-27 00:00:00 UTC.
const START: i64 = 1_395_878_400;
const ROWS: i64 = 7 * 96;

fn stamp(secs: i64) -> String {
    // the fixture's own formatter, independent of the one under test
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn at(s: &str) -> i64 {
    // "2014-03-28 11:45" → seconds, within the fixture's week
    let day: i64 = s[8..10].parse().unwrap();
    let month: i64 = s[5..7].parse().unwrap();
    let (h, mi): (i64, i64) = (s[11..13].parse().unwrap(), s[14..16].parse().unwrap());
    let day_index = if month == 3 { day - 27 } else { day + 4 };
    START + day_index * 86_400 + h * 3600 + mi * 60
}

/// The planted week, as CSV rows `ts,meter,kw`.
fn fixture_csv() -> String {
    let mut out = String::from("ts,meter,kw\n");
    let (late_until, stop_from) = (at("2014-03-28 11:45"), at("2014-04-01 18:00"));
    let (pause_from, pause_to) = (at("2014-03-31 00:00"), at("2014-04-01 05:45"));
    let (gap_from, gap_to) = (at("2014-03-28 03:00"), at("2014-03-28 05:45"));
    let (rep_from, rep_to) = (at("2014-04-02 10:00"), at("2014-04-02 10:45"));
    // m7 loses an hour and repeats the next: its row count is right, only its times are not
    let (lost_from, lost_to) = (at("2014-04-02 03:00"), at("2014-04-02 03:45"));
    let (twice_from, twice_to) = (at("2014-04-02 04:00"), at("2014-04-02 04:45"));
    let (zero_from, zero_to) = (at("2014-03-30 01:00"), at("2014-03-30 01:45"));
    let (dbl_from, dbl_to) = (at("2014-03-29 20:00"), at("2014-03-29 20:45"));
    for i in 0..ROWS {
        let t = START + i * MIN15;
        let hour = (t / 3600) % 24;
        for s in 1..=7 {
            let mut kw = (s * 10) as f64 + hour as f64 * 0.5 + 1.0;
            match s {
                1 => kw = 0.0,
                2 if t <= late_until => kw = 0.0,
                3 if t >= stop_from => kw = 0.0,
                4 if (pause_from..=pause_to).contains(&t) => kw = 0.0,
                5 if (gap_from..=gap_to).contains(&t) => continue,
                7 if (lost_from..=lost_to).contains(&t) => continue,
                _ => {}
            }
            if (zero_from..=zero_to).contains(&t) {
                kw = 0.0;
            }
            if (dbl_from..=dbl_to).contains(&t) {
                kw *= 2.0;
            }
            let row = format!("{},m{s},{kw}\n", stamp(t));
            out.push_str(&row);
            if (s == 5 && (rep_from..=rep_to).contains(&t))
                || (s == 7 && (twice_from..=twice_to).contains(&t))
            {
                out.push_str(&row);
            }
        }
    }
    out
}

fn dir_with(name: &str, csv: &str) -> PathBuf {
    let d = common::unique("shape");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(name), csv).unwrap();
    d
}

fn sealed(dir: &Path, shapes: &[(&str, Shape)]) -> Workspace {
    let mut bench = common::bench();
    for (n, s) in shapes {
        bench.declare(n, s.clone());
    }
    source::load_dir(&mut bench, dir).expect("loads");
    bench.seal("1GB").expect("seals")
}

fn described() -> String {
    let dir = dir_with("readings.csv", &fixture_csv());
    let ws = sealed(&dir, &[]);
    let d = ws.describe(4096);
    let _ = std::fs::remove_dir_all(&dir);
    d
}

fn has(text: &str, want: &str) {
    assert!(text.contains(want), "expected {want:?} in:\n{text}");
}

/// Series `key`'s line of notes says `want`.
fn noted(text: &str, key: &str, want: &str) {
    let head = format!("    {key}: ");
    assert!(
        text.lines()
            .any(|l| l.starts_with(&head) && l.contains(want)),
        "expected {want:?} in the notes of {key} in:\n{text}"
    );
}

#[test]
fn describe_tells_the_time_column_its_range_and_cadence() {
    let d = described();
    has(
        &d,
        "time column ts (looks like it): 2014-03-27 00:00:00+00 to 2014-04-02 23:45:00+00",
    );
    has(&d, "most series have a row every 15 minutes");
}

/// The guessed key is the text column with the most values that still leaves each series rows
/// to spare, named as a guess; then its values, how many series are on their step with nothing
/// missing or repeated, and each numeric column's range, nulls and zeros.
#[test]
fn describe_tells_the_series_key() {
    let d = described();
    has(
        &d,
        "series key meter (looks like it): 7 series ('m1', 'm2', 'm3', 'm4', 'm5', 'm6', 'm7')",
    );
    has(
        &d,
        "5 of 7 series have a row every 15 minutes from their first to their last, none missing \
         or repeated",
    );
    has(&d, "kw: 0 to");
}

#[test]
fn describe_names_a_dead_series_a_late_start_a_stop_and_a_pause() {
    let d = described();
    has(&d, "'m1': kw is 0 in every row");
    has(
        &d,
        "'m2': kw is 0 from the start (2014-03-27 00:00:00+00) to 2014-03-28 11:45:00+00 (1 day 12 hours)",
    );
    has(
        &d,
        "'m3': kw is 0 from 2014-04-01 18:00:00+00 to the end (2014-04-02 23:45:00+00) (1 day 6 hours)",
    );
    has(
        &d,
        "'m4': kw is 0 from 2014-03-31 00:00:00+00 to 2014-04-01 05:45:00+00 (1 day 6 hours)",
    );
    has(&d, "the one other series: none of these");
}

#[test]
fn describe_names_gaps_and_repeated_times() {
    let d = described();
    has(
        &d,
        "'m5': 1 gap, 12 rows missing in all; the longest 3 hours (no rows after 2014-03-28 \
         02:45:00+00 until 2014-03-28 06:00:00+00)",
    );
    has(&d, "4 rows repeat a time already present");
    // as many rows as its extent holds, so only the checksums of its times give it away
    has(
        &d,
        "'m7': 1 gap, 4 rows missing in all; the longest 1 hour (no rows after 2014-04-02 \
         02:45:00+00 until 2014-04-02 04:00:00+00); 4 rows repeat a time already present",
    );
}

#[test]
fn describe_names_a_moment_every_series_shares() {
    let d = described();
    has(&d, "at the same moment in every series:");
    has(
        &d,
        "kw is 0 in every series at 2014-03-30 01:00:00+00 to 01:45:00+00 (4 times) — 2014-03-30, \
         the last Sunday of March, when clocks in the EU go forward an hour",
    );
    has(
        &d,
        "kw summed over the series is 2× the same time a day before and after at 2014-03-29 \
         20:00:00+00 to 20:45:00+00 (4 times)\n",
    );
}

#[test]
fn a_stated_shape_wins_over_the_heuristic() {
    // Two text columns: the heuristic picks the one with more values; the operator says otherwise.
    let mut csv = String::from("at,site,meter,kw\n");
    for i in 0..200 {
        for (site, meter) in [
            ("north", "a"),
            ("north", "b"),
            ("south", "a"),
            ("south", "b"),
        ] {
            csv.push_str(&format!(
                "{},{site},{meter},{}\n",
                stamp(START + i * 3600),
                i % 7
            ));
        }
    }
    let dir = dir_with("plant.csv", &csv);
    let heuristic = sealed(&dir, &[]).describe(4096);
    // a tie goes to the leftmost; that it is not the whole key shows as repeated times
    has(
        &heuristic,
        "series key site (looks like it): 2 series ('north', 'south')",
    );
    // both sites' notes are alike, and are said once
    has(
        &heuristic,
        "'north', 'south' (2 series): 200 rows repeat a time already present",
    );
    let stated = sealed(
        &dir,
        &[(
            "plant",
            Shape {
                time_column: Some("at".into()),
                series: vec!["site".into(), "meter".into()],
            },
        )],
    )
    .describe(4096);
    has(&stated, "time column at: ");
    has(
        &stated,
        "series key site, meter: 4 series ('north' / 'a', 'north' / 'b', 'south' / 'a', 'south' / 'b')",
    );
    has(&stated, "every series has a row every 1 hour");
    assert!(
        !stated.contains("looks like"),
        "nothing is guessed: {stated}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stated_column_that_is_not_there_refuses_the_load() {
    let dir = dir_with("readings.csv", &fixture_csv());
    for shape in [
        Shape {
            time_column: Some("when".into()),
            series: vec![],
        },
        Shape {
            time_column: Some("meter".into()),
            series: vec![],
        },
        Shape {
            time_column: None,
            series: vec!["sensor".into()],
        },
    ] {
        let mut bench = common::bench();
        bench.declare("readings", shape.clone());
        let e = source::load_dir(&mut bench, &dir).expect_err("refused, not ignored");
        let e = e.to_string();
        assert!(e.contains("stated"), "says what was stated: {e}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_stated_shape_must_name_declared_columns() {
    let cfg = r#"
[[source]]
name = "readings"
type = "exec"
command = "/bin/true"
time_column = "observed"
  [[source.schema]]
  name = "observed"
  type = "string"
"#;
    let e = benchy_mcp::config::parse(cfg, Path::new("/x/benchy.toml")).unwrap_err();
    assert!(e.contains("a time column is a date"), "{e}");
    let cfg = cfg.replace("time_column = \"observed\"", "series = [\"meter\"]");
    let e = benchy_mcp::config::parse(&cfg, Path::new("/x/benchy.toml")).unwrap_err();
    assert!(e.contains("not in its schema"), "{e}");
    // and a file source's stated shape travels to its loader
    let ok = r#"
[[source]]
name = "readings"
type = "file"
path = "./readings.csv"
time_column = "ts"
series = ["meter"]
"#;
    let s = benchy_mcp::config::parse(ok, Path::new("/x/benchy.toml")).unwrap();
    assert_eq!(
        s.shapes.get("readings"),
        Some(&Shape {
            time_column: Some("ts".into()),
            series: vec!["meter".into()],
        })
    );
}

#[test]
fn a_table_without_time_gets_its_columns_summarised() {
    let mut csv = String::from("meter,room,area_m2\n");
    for i in 0..30 {
        csv.push_str(&format!(
            "{},{},{}\n",
            ["lab", "office", "store"][i % 3],
            i,
            i * 2
        ));
    }
    let dir = dir_with("rooms.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(&d, "meter: 3 distinct ('lab', 'office', 'store')");
    has(&d, "area_m2: 0 to 58, never null");
    assert!(!d.contains("time column"), "{d}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_distinct_count_never_exceeds_the_values() {
    // DuckDB's HyperLogLog, `approx_count_distinct`, said 24 of `name_0` … `name_19`, measured on
    // 1.5.5; a host saw "about 51 distinct" on a 50-row table. A small table's count is exact,
    // and no count is more than the column's values. Each estimate is held to the values counted
    // in the profile's one scan (for a stream, as it went in), so loads of 50,000 and 1,000,000
    // rows without a time column cost the same as before the cap, measured.
    let mut csv = String::from("room_name,tag,area_m2\n");
    for i in 0..20 {
        let tag = if i % 2 == 0 {
            String::new()
        } else {
            format!("name_{i}")
        };
        csv.push_str(&format!("name_{i},{tag},{}\n", i * 2));
    }
    let dir = dir_with("rooms.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(&d, "room_name: 20 distinct\n");
    has(&d, "tag: 10 distinct ('name_1', ");
    assert!(!d.contains("about"), "{d}");
    let _ = std::fs::remove_dir_all(&dir);

    // A large table's count stays an estimate, held to the column's values: 20 values and
    // 150,000 nulls.
    let mut csv = String::from("room_name,area_m2\n");
    for i in 0..150_000 {
        if i < 20 {
            csv.push_str(&format!("name_{i},{i}\n"));
        } else {
            csv.push_str(&format!(",{i}\n"));
        }
    }
    let dir = dir_with("rooms.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(&d, "room_name: about 20 distinct\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_integer_series_key_is_listed_in_number_order() {
    // The keys are held as text, and were listed in text order: `50 series (1 … 9)`. Past
    // 200,000 series the ones kept are the first in text order, so the key gets no range.
    let mut csv = String::from("ts,series_id,kw\n");
    for t in 0..10 {
        for s in 1..=50 {
            csv.push_str(&format!("2026-01-01 00:{t:02}:00,{s},1.5\n"));
        }
    }
    let dir = dir_with("readings.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(
        &d,
        "series key series_id (looks like it): 50 series (1 … 50)",
    );
    let _ = std::fs::remove_dir_all(&dir);

    let mut csv = String::from("ts,series_id,kw\n");
    for t in 0..10 {
        for s in [-3, 2, 10, 9, 100] {
            csv.push_str(&format!("2026-01-01 00:{t:02}:00,{s},1.5\n"));
        }
    }
    let dir = dir_with("readings.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(&d, "5 series (-3, 2, 9, 10, 100)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_shape_is_summarised_never_cut_silently() {
    // 300 series, each with a two-day pause: far more notes than any budget holds. Past `s199`
    // the pause is of nulls, so no two series' notes are alike and said once.
    let mut csv = String::from("ts,meter,kw\n");
    for i in 0..(10 * 24) {
        let t = START + i * 3600;
        for s in 0..300 {
            let paused = (s * 7 % 200..s * 7 % 200 + 48).contains(&i);
            csv.push_str(&format!(
                "{},s{s:03},{}\n",
                stamp(t),
                match (paused, s < 200) {
                    (true, true) => "0".to_string(),
                    (true, false) => String::new(),
                    _ => (1 + s).to_string(),
                }
            ));
        }
    }
    let dir = dir_with("many.csv", &csv);
    let ws = sealed(&dir, &[]);
    // 4,700 bytes: every time is shown as an instant, `+00` and all (REQ-15), so forty notes
    // take that much more room than they did as zone-less times at 4,096; and the line saying
    // what is noted says a value held too long is (#51): 58 bytes more than at 4,600.
    let d = ws.describe(4700);
    assert!(
        benchy_mcp::result::wire_len(&d) <= 4700,
        "within its budget: {} bytes",
        d.len()
    );
    has(&d, "… and 260 more series with notes like these");
    has(
        &d,
        "series key meter (looks like it): 300 series ('s000' … 's299')",
    );
    // less room: the notes stop where it runs out, and say how many lines are not shown
    let less = ws.describe(1500);
    assert!(
        benchy_mcp::result::wire_len(&less) <= 1500,
        "{}",
        less.len()
    );
    has(
        &less,
        "more line(s) about this table not shown: over the describe budget",
    );
    // and with almost no room, the table still shows and its shape is said to be missing
    let small = ws.describe(300);
    has(&small, "many (");
    has(&small, "is not shown: over the describe budget");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn describe_does_not_scan_the_table() {
    // Two million rows: profiling them takes most of a second, rendering a stored profile takes
    // microseconds. A describe that profiled — or counted — on every call would be seen here.
    let mut bench = common::bench();
    bench
        .connection()
        .execute_batch(
            "CREATE TABLE big AS SELECT TIMESTAMP '2026-01-01' + to_minutes(i // 50) AS ts, \
             'e' || (i % 50) AS entity, (i % 997) * 1.5 AS value FROM range(2000000) r(i)",
        )
        .unwrap();
    bench
        .record(benchy_mcp::LoadedTable {
            name: "big".into(),
            rows: 2_000_000,
            columns: vec![
                ("ts".into(), "TIMESTAMP".into()),
                ("entity".into(), "VARCHAR".into()),
                ("value".into(), "DOUBLE".into()),
            ],
            source: "test".into(),
            profile: None,
            seen: None,
        })
        .unwrap();
    let ws = bench.seal("1GB").unwrap();
    let first = ws.describe(4096);
    has(&first, "series key entity (looks like it): 50 series");
    let mut best = std::time::Duration::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        assert_eq!(ws.describe(4096), first, "the same every time");
        best = best.min(t.elapsed());
    }
    assert!(best.as_millis() < 50, "describe took {best:?}");
}

#[test]
fn the_same_data_gives_the_same_shape_from_csv_and_parquet() {
    let dir = dir_with("readings.csv", &fixture_csv());
    let other = common::unique("shape-parquet");
    std::fs::create_dir_all(&other).unwrap();
    // Written in another row order too: the profile depends on the rows, not on their order.
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "COPY (SELECT * FROM read_csv_auto('{}') ORDER BY meter DESC, ts DESC) TO '{}' \
         (FORMAT parquet)",
        dir.join("readings.csv").display(),
        other.join("readings.parquet").display()
    ))
    .unwrap();
    let a = sealed(&dir, &[]).describe(4096);
    let b = sealed(&other, &[]).describe(4096);
    assert_eq!(a, b);
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&other);
}

#[test]
fn the_shape_reaches_the_agent_through_the_engine() {
    // Through the shipped binary: a loader profiles, the comment travels in the file, the engine
    // reads it at attach, and the MCP `describe` renders it.
    let dir = dir_with("readings.csv", &fixture_csv());
    let mut s = common::Server::start(&dir, &[]);
    let r = s.call("tools/call", json!({"name": "describe", "arguments": {}}));
    let text = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    has(&text, "series key meter (looks like it): 7 series");
    has(&text, "'m1': kw is 0 in every row");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Optional: the public meter data the eval uses, when `eval/fetch_uci.py` has fetched it.
#[test]
fn the_public_meter_data_shows_its_quirks() {
    let csv = Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/uci/readings.csv");
    if !csv.exists() {
        eprintln!("skipped: {} is not fetched", csv.display());
        return;
    }
    let dir = common::unique("shape-uci");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&csv, dir.join("readings.csv")).unwrap();
    let d = sealed(&dir, &[]).describe(4096);
    eprintln!("{d}");
    has(&d, "series key meter (looks like it): 24 series");
    has(&d, "every series has a row every 15 minutes");
    has(&d, "'MT_223': kw is 0 in every row");
    has(&d, "'MT_106': kw is 0 from the start");
    has(&d, "'MT_109': kw is 0 from the start");
    has(&d, "to the end (2014-12-31 23:45:00+00) (13 days");
    has(
        &d,
        "kw is 0 in every series at 2014-03-30 01:00:00+00 to 01:45:00+00",
    );
    has(&d, "2014-10-26 01:00:00+00 to 01:45:00+00");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The bundled example, `examples/data/meters.csv`: eight rows, two meters. `office` repeats one
/// time and misses the next, which is the answer the README's first question should not trust.
fn example_meters() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/data/meters.csv"))
        .unwrap()
}

#[test]
fn a_small_table_still_finds_its_series_key() {
    // Eight rows held two series, so the key's values could not leave each series ten rows, and
    // with no key the four rows that share a time with the other meter read as four repeats.
    // Only when the first rule finds nothing, the key is the column whose every value has rows
    // at two times or more and which, with the time, tells apart at least 80% of the rows, more
    // than the time alone does: counted exactly, over at most a million rows. It finds what
    // stating the key finds: one gap and one repeat, not four repeats.
    let dir = dir_with("meters.csv", &example_meters());
    let d = sealed(&dir, &[]).describe(4096);
    has(
        &d,
        "series key meter (looks like it): 2 series ('lab', 'office')",
    );
    has(
        &d,
        "1 of 2 series has a row every 15 minutes from its first to its last, none missing or \
         repeated",
    );
    has(
        &d,
        "'office': power_kw is 1 in every row; 1 gap, 1 row missing in all; the longest 15 minutes (no rows after \
         2026-01-01 00:15:00+00 until 2026-01-01 00:45:00+00); 1 row repeats a time already \
         present",
    );
    assert!(!d.contains("4 rows repeat"), "{d}");
    // the guess is what the operator would have stated
    let stated = sealed(
        &dir,
        &[(
            "meters",
            Shape {
                time_column: None,
                series: vec!["meter".into()],
            },
        )],
    )
    .describe(4096);
    assert_eq!(
        d.replace("series key meter (looks like it)", "series key meter"),
        stated
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeated_times_with_no_key_are_said_to_be_unassessed() {
    // Two readings at every time and nothing that tells them apart — an id is unique per row, so
    // it is no key. Calling each pair a repeat would be a guess presented as a finding.
    let mut csv = String::from("ts,id,kw\n");
    for i in 0..50 {
        for j in 0..2 {
            csv.push_str(&format!(
                "{},{},{}.5\n",
                stamp(START + i * MIN15),
                i * 2 + j,
                i * 2 + j
            ));
        }
    }
    let dir = dir_with("pairs.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(
        &d,
        "50 distinct times in 100 rows, and no column looks like a series key: gaps and repeats \
         are not assessed",
    );
    assert!(!d.contains("repeat a time"), "{d}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn describe_names_duckdbs_own_types() {
    // `float` and `int` are 32-bit in DuckDB; the columns are DOUBLE and BIGINT.
    let dir = dir_with("meters.csv", &example_meters());
    std::fs::write(dir.join("rooms.csv"), "room,floor,open\nlab,1,true\n").unwrap();
    let d = sealed(&dir, &[]).describe(4096);
    has(
        &d,
        "observed_at timestamptz, meter varchar, power_kw double",
    );
    has(&d, "room varchar, floor bigint, open boolean");
    // and the same through the shipped binary
    let mut s = common::Server::start(&dir, &[]);
    let r = s.call("tools/call", json!({"name": "describe", "arguments": {}}));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    has(
        text,
        "observed_at timestamptz, meter varchar, power_kw double",
    );
    has(text, "room varchar, floor bigint, open boolean");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn counts_are_said_in_good_grammar() {
    let dir = dir_with("meters.csv", &example_meters());
    std::fs::write(dir.join("rooms.csv"), "room,floor\nlab,0\nhall,1\nyard,2\n").unwrap();
    let d = sealed(&dir, &[]).describe(4096);
    has(&d, "floor: 0 to 2, never null; 0 in 1 row\n");
    let f = described();
    for text in [&d, &f] {
        for bad in [
            "(s)",
            " 1 rows",
            " 1 times",
            " 1 gaps",
            "1 of 2 series have",
            "other 1 series",
        ] {
            assert!(!text.contains(bad), "{bad:?} in:\n{text}");
        }
    }
    has(&f, "(4 times)");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_long_line_is_elided_between_items_never_inside_one() {
    // A series paused ten times, and twelve series with long names: each line has more than its
    // room, and is shortened where one item ends, saying how many are left out.
    let mut csv = String::from("ts,meter,kw\n");
    let long = |s: usize| format!("{}{s:02}", "meter-with-a-long-name-".repeat(2));
    for i in 0..(30 * 24) {
        let t = START + i * 3600;
        for s in 0..12 {
            let paused = s == 0 && (i / 24) % 3 == 1;
            csv.push_str(&format!(
                "{},{},{}\n",
                stamp(t),
                long(s),
                if paused { 0 } else { 1 }
            ));
        }
    }
    let dir = dir_with("long.csv", &csv);
    let d = sealed(&dir, &[]).describe(8192);
    assert!(!d.contains("bytes]"), "a line was cut inside a value:\n{d}");
    has(&d, ", then … then 0 from ");
    has(&d, "10 runs in all");
    let key = d
        .lines()
        .find(|l| l.contains("series key"))
        .expect("a key line");
    assert!(
        key.ends_with(" more)")
            && key.contains("'meter-with-a-long-name-meter-with-a-long-name-00'"),
        "{key}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A table whose series key and text column carry [`common::HOSTILE`], one series of it dead so
/// it is also named in a note.
fn hostile_dir() -> PathBuf {
    let field = format!("\"{}\"", common::HOSTILE.replace('"', "\"\""));
    let mut csv = String::from("ts,meter,kw\n");
    for i in 0..20 {
        let t = stamp(START + i * MIN15);
        csv.push_str(&format!("{t},lab,{}\n", i + 1));
        csv.push_str(&format!("{t},{field},0\n"));
    }
    let dir = dir_with("readings.csv", &csv);
    std::fs::write(
        dir.join("labels.csv"),
        format!("s,n\n{field},1\n{field},2\n"),
    )
    .unwrap();
    dir
}

#[test]
fn a_data_value_in_describe_is_a_quoted_literal() {
    // A value is data, and the agent reads describe as prose: unquoted, a value written as an
    // instruction reads as one. As a literal it is what the agent would type, and a line
    // separator, NEL, an escape or a bidi override in it is spelled out rather than obeyed.
    let dir = hostile_dir();
    let d = sealed(&dir, &[]).describe(8192);
    // the key line, the note naming the dead series, and the text column's values
    common::assert_values_quoted(&d, 3);
    has(
        &d,
        &format!("{}: kw is 0 in every row", common::HOSTILE_SHOWN),
    );
    has(&d, &format!("s: 1 distinct ({})", common::HOSTILE_SHOWN));
    // the same through the shipped binary
    let mut s = common::Server::start(&dir, &[]);
    let r = s.call("tools/call", json!({"name": "describe", "arguments": {}}));
    common::assert_values_quoted(r["result"]["content"][0]["text"].as_str().unwrap(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_integer_reading_is_not_taken_for_the_series_key() {
    // Integer device ids beside integer readings: CO₂ in ppm and RSSI in dBm both have more
    // values than there are devices, and each still leaves ten rows a value. The most values
    // picked `co2`: 1600 series, none on a step, thousands of invented gaps. Where two columns
    // of a kind could be the key, the one that cuts the table's first slice of time into series
    // that keep a step wins over the one with the most values, when clearly more of its rows
    // keep it. That costs one query per such column over that slice, as the step's sample
    // reads, and none when only one column could be the key.
    let mut csv = String::from("ts,device_id,co2,rssi\n");
    let mut x: u64 = 1;
    let mut rand = |n: u64| {
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (x >> 33) % n
    };
    for i in 0..2000 {
        let t = stamp(START + i * MIN15);
        for d in 1..=50 {
            let (co2, rssi) = (400 + rand(1600), -30 - rand(91) as i64);
            csv.push_str(&format!("{t},{d},{co2},{rssi}\n"));
        }
    }
    let dir = dir_with("air.csv", &csv);
    let d = sealed(&dir, &[]).describe(4096);
    has(
        &d,
        "series key device_id (looks like it): 50 series (1 … 50)",
    );
    has(
        &d,
        "every series has a row every 15 minutes from its first to its last, none missing or \
         repeated; all span the whole range",
    );
    assert!(!d.contains("missing in all"), "{d}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn anomalies_past_any_limit_are_counted_exactly() {
    // 300,001 rows of `a` at one time, then a row a second: the anomalies were listed row by
    // row up to 200,001 and counted from that list, so `a` showed 200,001 repeats as if exact,
    // and `b`, after `a` by key, lost its gap.
    let mut csv = String::from("t,s,v\n");
    let sec = |n: i64| stamp(START + n);
    for _ in 0..300_001 {
        csv.push_str(&format!("{},a,1\n", sec(0)));
    }
    for n in 1..=100 {
        csv.push_str(&format!("{},a,1\n", sec(n)));
    }
    for n in (0..=100).filter(|n| *n != 50) {
        csv.push_str(&format!("{},b,1\n", sec(n)));
    }
    let dir = dir_with("burst.csv", &csv);
    let shape = Shape {
        time_column: Some("t".into()),
        series: vec!["s".into()],
    };
    let d = sealed(&dir, &[("burst", shape)]).describe(4096);
    noted(&d, "'a'", "300000 rows repeat a time already present");
    noted(
        &d,
        "'b'",
        "1 gap, 1 row missing in all; the longest 1 second (no rows after 2014-03-27 \
         00:00:49+00 until 2014-03-27 00:00:51+00)",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn many_series_keep_the_tables_own_extent() {
    // 200,000 regular series, then `zzz`, which ends later and misses three rows. Past 200,000
    // series the profile examines the first by key; the extent, the key's range and "every
    // series" were said of those as if of the table.
    let mut bench = common::bench();
    bench
        .connection()
        .execute_batch(
            "CREATE TABLE many AS SELECT TIMESTAMP '2026-01-01' + to_minutes(15 * (i % 10)) \
             AS ts, 's' || lpad((i // 10)::VARCHAR, 6, '0') AS sensor, 1.5 AS v \
             FROM range(2000000) r(i) \
             UNION ALL SELECT TIMESTAMP '2026-01-01' + to_minutes(15 * x), 'zzz', 1.5 \
             FROM unnest([0, 1, 2, 3, 5, 6, 8, 9, 11]) u(x)",
        )
        .unwrap();
    bench.declare(
        "many",
        Shape {
            time_column: Some("ts".into()),
            series: vec!["sensor".into()],
        },
    );
    bench
        .record(benchy_mcp::LoadedTable {
            name: "many".into(),
            rows: 2_000_009,
            columns: vec![
                ("ts".into(), "TIMESTAMP".into()),
                ("sensor".into(), "VARCHAR".into()),
                ("v".into(), "DECIMAL(2,1)".into()),
            ],
            source: "test".into(),
            profile: None,
            seen: None,
        })
        .unwrap();
    let d = bench.seal("1GB").unwrap().describe(4096);
    has(
        &d,
        "time column ts: 2026-01-01 00:00:00 to 2026-01-01 02:45:00",
    );
    has(&d, "series key sensor: more than 200000 series\n");
    has(
        &d,
        "200000 of the first 200000 series by key, to 's199999', have a row every 15 minutes",
    );
    has(
        &d,
        "the series after 's199999' were not examined one by one",
    );
    for wrong in ["every series has", "all span the whole range", "'s199999')"] {
        assert!(!d.contains(wrong), "{wrong:?} in:\n{d}");
    }
}

#[test]
fn three_gaps_hidden_by_three_repeats_are_found() {
    // The count, the sum and the sum of squares of the step indices are those of a row every
    // step for each of `a`, `c` and `d`: three (or four) rows missing and as many repeated,
    // placed so that both checksums still match. Only `b` is regular.
    let a: Vec<i64> = vec![0, 1, 1, 4, 5, 5, 6, 6, 8, 9];
    let b: Vec<i64> = (0..10).collect();
    let mut c: Vec<i64> = (0..2000)
        .filter(|i| ![1002, 1003, 1007].contains(i))
        .collect();
    c.extend([1001, 1005, 1006]);
    let mut d: Vec<i64> = (0..=20).filter(|i| ![1, 2, 4, 7].contains(i)).collect();
    d.extend([0, 3, 5, 6]);
    d.sort();
    let mut rows: Vec<(i64, &str)> = Vec::new();
    for (s, ix) in [("a", &a), ("b", &b), ("c", &c), ("d", &d)] {
        rows.extend(ix.iter().map(|i| (*i, s)));
    }
    rows.sort();
    let csv = |rows: &mut dyn Iterator<Item = &(i64, &str)>| {
        let mut out = String::from("ts,meter,kw\n");
        for (i, s) in rows {
            out.push_str(&format!("{},{s},1.5\n", stamp(START + i * MIN15)));
        }
        out
    };
    let shape = Shape {
        time_column: None,
        series: vec!["meter".into()],
    };
    let mut seen = Vec::new();
    // In time order, backwards, and shuffled, where no series' times come in order and only an
    // exact count tells `b` regular.
    let mut shuffled = rows.clone();
    shuffled.sort_by_key(|(i, s)| (i * 7919 + s.len() as i64 * 31 + s.as_bytes()[0] as i64) % 101);
    for text in [
        csv(&mut rows.iter()),
        csv(&mut rows.iter().rev()),
        csv(&mut shuffled.iter()),
    ] {
        let dir = dir_with("hidden.csv", &text);
        let got = sealed(&dir, &[("hidden", shape.clone())]).describe(8192);
        has(&got, "1 of 4 series has a row every 15 minutes");
        for (key, gaps, missing) in [("'a'", 2, 3), ("'c'", 2, 3), ("'d'", 3, 4)] {
            noted(
                &got,
                key,
                &format!("{gaps} gaps, {missing} rows missing in all"),
            );
            noted(
                &got,
                key,
                &format!("{missing} rows repeat a time already present"),
            );
        }
        seen.push(got);
        let _ = std::fs::remove_dir_all(&dir);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], seen[2]);

    // And one series with no key: it was said to be "a row every 15 minutes from first to last,
    // none missing or repeated" beside "no column looks like a series key".
    let mut out = String::from("ts,kw\n");
    for i in &d {
        out.push_str(&format!("{},1.5\n", stamp(START + i * MIN15)));
    }
    let dir = dir_with("one.csv", &out);
    let got = sealed(&dir, &[]).describe(4096);
    assert!(!got.contains("none missing or repeated"), "{got}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A bench whose tables `sql` makes, each recorded as loaded: its columns and rows read back.
fn recorded(sql: &str, tables: &[&str]) -> Workspace {
    let mut bench = common::bench();
    bench.connection().execute_batch(sql).unwrap();
    for t in tables {
        let columns: Vec<(String, String)> = {
            let mut st = bench
                .connection()
                .prepare(&format!(
                    "SELECT column_name, data_type FROM information_schema.columns \
                     WHERE table_name = '{t}' ORDER BY ordinal_position"
                ))
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let rows: i64 = bench
            .connection()
            .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))
            .unwrap();
        bench
            .record(benchy_mcp::LoadedTable {
                name: t.to_string(),
                rows: rows as u64,
                columns,
                source: "test".into(),
                profile: None,
                seen: None,
            })
            .unwrap();
    }
    bench.seal("1GB").unwrap()
}

/// #50's shape, smaller: 149 sensors over ten days, 52 reading every 5 minutes, 49 every 10 and
/// 48 every 15, each complete but for what is planted — a late start, an early stop, a 4-day
/// gap, two days of 0 in a 15-minute series, a 15-minute series one of whose rows is a second
/// late, and ten dead sensors — beside a two-row table of sites. Read as one cadence, every complete 10- and 15-minute series was a broken 5-minute
/// one ("1014: 8735 gaps, 17470 rows missing"), and those notes crowded out the rest.
const MIXED: &str = "CREATE TABLE readings AS
    WITH s AS (SELECT 1001 + i AS sensor,
                 CASE WHEN i < 52 THEN 5 WHEN i < 101 THEN 10 ELSE 15 END AS every
               FROM range(149) r(i)),
    m AS (SELECT sensor, every, unnest(range(0, 14400, every)) AS m FROM s)
    SELECT TIMESTAMP '2026-01-01' + to_minutes(m)
        + to_seconds(CASE WHEN sensor = 1140 AND m = 7200 THEN 1 ELSE 0 END) AS ts, sensor,
      CASE WHEN sensor BETWEEN 1090 AND 1099 THEN 0
           WHEN sensor = 1130 AND m >= 1440 AND m < 4320 THEN 0
           ELSE round(20 + 5 * sin(m / 97.0 + sensor), 3) END AS temp
    FROM m
    WHERE NOT (sensor = 1010 AND m < 2880)
      AND NOT (sensor = 1060 AND m >= 11520)
      AND NOT (sensor = 1120 AND m >= 4320 AND m < 10080)
    ORDER BY m, sensor;
    CREATE TABLE sites AS SELECT * FROM (VALUES (1001, 'north'), (1002, 'south')) v(sensor, site);";

/// #50: read at the table's one step (5 minutes), every complete 10- and 15-minute series of
/// this fixture was reported broken: "8735 gaps, 17470 rows missing".
///
/// 149 series, each complete but for what is planted. A series' step is the greatest common
/// divisor of its times' distances when that is longer than the table's, or shorter with the
/// series complete on it; a series that breaks its step is judged at the distance at least half
/// its rows keep, so a 15-minute series one of whose rows is a second late has two rows off the
/// step, not a gap every row. The time column's line says how many series keep each step, the
/// line on regular series how many of them do, and a series off the most common step says its
/// own. Its runs of 0 and its late start or early end are judged at its step too.
#[test]
fn each_series_is_judged_at_its_own_step() {
    let d = recorded(MIXED, &["readings", "sites"]).describe(4096);
    eprintln!("{d}");
    has(
        &d,
        "; series have a row every 5 minutes (52 series), 10 minutes (49) or 15 minutes (48)",
    );
    has(
        &d,
        "147 of 149 series have a row at their own step from their first to their last, none \
         missing or repeated (52 every 5 minutes, 49 every 10 minutes, 46 every 15 minutes)",
    );
    // the one gap, at its series' own step: four days of 15 minutes
    noted(
        &d,
        "1120",
        "1 gap, 384 rows missing in all; the longest 4 days (no rows after 2026-01-03 23:45:00 \
         until 2026-01-08 00:00:00)",
    );
    assert_eq!(d.matches("missing in all").count(), 1, "{d}");
    // a second late: judged at the distance most of its rows keep, not at the table's step
    noted(
        &d,
        "1140",
        "a row every 15 minutes, where the most common step is 5 minutes; 2 rows are off the \
         step",
    );
    // a run of 0 in a 15-minute series is one run, not 192 rows each broken from the last
    noted(
        &d,
        "1130",
        "temp is 0 from 2026-01-02 00:00:00 to 2026-01-03 23:45:00 (2 days)",
    );
    noted(
        &d,
        "1010",
        "first row 2026-01-03 00:00:00, 2 days after the earliest",
    );
    noted(&d, "1060", "last row 2026-01-08 23:50:00");
    // a slower series is still said to be slower, and alike notes are said once
    has(
        &d,
        "1053–1059, 1061–1089, 1100–1101 (38 series): a row every 10 minutes, where the most \
         common step is 5 minutes\n",
    );
    has(
        &d,
        "1102–1119, 1121–1129, 1131–1139, 1141–1149 (45 series): a row every 15 minutes, \
         where the most common step is 5 minutes\n",
    );
    has(
        &d,
        "1090–1099 (10 series): a row every 10 minutes, where the most common step is 5 \
         minutes; temp is 0 in every row\n",
    );
    // and the other table's shape is there
    has(
        &d,
        "sites (2 rows)\n  sensor integer, site varchar\n  sensor: 1001 to 1002",
    );
    has(&d, "  site: 2 distinct\n");
    assert!(!d.contains("not shown"), "{d}");
}

#[test]
fn every_tables_shape_comes_before_any_tables_notes() {
    // 300 series, each with its own two-day pause: more notes than the budget holds, in the
    // first table. The second table's shape still shows, and the first's notes stop short.
    let sql = "CREATE TABLE many AS SELECT TIMESTAMP '2026-01-01' + to_hours(i) AS ts,
        's' || lpad(s::VARCHAR, 3, '0') AS meter,
        CASE WHEN i >= s * 7 % 200 AND i < s * 7 % 200 + 48 THEN 0 ELSE 1 + s END AS kw
      FROM range(240) r(i), range(300) q(s);
      CREATE TABLE sites AS SELECT * FROM (VALUES
          ('s001', 'north', 2.5, 1, 12, 1998, 52.52, 13.40, 230, 3),
          ('s002', 'south', 4.0, 3, 40, 2011, 48.14, 11.58, 400, 1))
        v(meter, site, area, floor, rooms, built, lat, lon, volts, phases);";
    let d = recorded(sql, &["many", "sites"]).describe(4096);
    eprintln!("{d}");
    assert!(benchy_mcp::result::wire_len(&d) <= 4096, "{}", d.len());
    has(
        &d,
        "series key meter (looks like it): 300 series ('s000' … 's299')",
    );
    has(
        &d,
        "more line(s) about this table not shown: over the describe budget",
    );
    has(&d, "  site: 2 distinct\n");
    has(&d, "  area: 2.5 to 4, never null\n");
    has(&d, "  phases: 1 to 3, never null\n");
    assert!(!d.contains("is not shown"), "{d}");
}

/// A month and a half of 15-minute readings in five rooms, with what a stuck sensor and a
/// step-like column look like: `r1` reads exactly 21.5 for 21 days, `r2` reads 21.5 for 30 hours
/// with a 6-hour gap in them, and `status` holds each value for about five days, as a mode or a
/// fault code does. `order` sorts the rows.
fn stuck_sql(order: &str) -> String {
    format!(
        "CREATE TABLE air AS SELECT * FROM (
           SELECT TIMESTAMP '2026-01-01' + to_minutes(15 * i) AS ts, 'r' || s AS room,
             CASE WHEN s = 1 AND i >= 960 AND i < 960 + 21 * 96 THEN 21.5
                  WHEN s = 2 AND i >= 3800 AND i < 3800 + 120 THEN 21.5
                  ELSE round(20 + 3 * sin(i / 13.0 + s), 3) END AS temp,
             (i // 500 + s) % 4 + 1 AS status
           FROM range(45 * 96) r(i), range(1, 6) q(s)
           WHERE NOT (s = 2 AND i >= 3848 AND i < 3872))
         ORDER BY {order}"
    )
}

/// #51. Why ten times the series' average run: measured on the public meter data's 370 meters
/// over four years, six meters hold one non-zero value for a day or more, 74 runs, every one of
/// them ten times its meter's average. Rounded to 10 kW, a low-resolution reading, 48 meters
/// have 1,084 such runs and the rule keeps 789 in 44; rounded to 50 kW, 173 meters have 5,888
/// and it keeps 2,461 in 147.
#[test]
fn a_stuck_value_inside_a_series_is_noted() {
    let mut seen = Vec::new();
    for order in ["ts, room", "room, ts DESC", "hash(ts, room)"] {
        let d = recorded(&stuck_sql(order), &["air"]).describe(4096);
        eprintln!("{d}");
        noted(
            &d,
            "'r1'",
            "temp is 21.5 from 2026-01-11 00:00:00 to 2026-01-31 23:45:00 (21 days)",
        );
        // a gap breaks a run, as it breaks a run of 0: two 12-hour runs, neither noted
        noted(&d, "'r2'", "1 gap, 24 rows missing in all");
        assert!(!d.contains("'r2': temp is 21.5"), "{d}");
        // a column that holds each value for days, everywhere, is not stuck
        assert!(!d.contains("status is"), "{d}");
        has(&d, "the other 3 series: none of these");
        seen.push(d);
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], seen[2]);
}

/// #15's tables as CSV text: `fast`, one series of 20 rows 500 µs apart from 2026-01-01; `pair`,
/// meters `a` and `b` at those times, `b` without its eleventh.
fn sub_ms_csv() -> (String, String) {
    let at = |i: i64| format!("2026-01-01 00:00:00.{:06}", i * 500);
    let mut fast = String::from("ts,kw\n");
    let mut pair = String::from("ts,meter,kw\n");
    for i in 0..20 {
        fast.push_str(&format!("{},{i}.5\n", at(i)));
        for m in ["a", "b"] {
            if !(m == "b" && i == 10) {
                pair.push_str(&format!("{},{m},{i}.5\n", at(i)));
            }
        }
    }
    (fast, pair)
}

/// `rows` as an Arrow stream file `name.arrows` in `dir`: each row's step from 2026-01-01 in
/// 500 µs, and its meter where it has one.
fn sub_ms_stream(dir: &Path, name: &str, rows: &[(i64, Option<&str>)]) {
    use arrow::array::{
        ArrayRef, Float64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
    };
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;
    let base = 1_767_225_600_000_000i64; // 2026-01-01T00:00:00Z
    let ts = TimestampMicrosecondArray::from_iter_values(rows.iter().map(|r| base + r.0 * 500))
        .with_timezone("UTC");
    let mut fields = vec![Field::new(
        "ts",
        arrow::array::Array::data_type(&ts).clone(),
        false,
    )];
    let mut cols: Vec<ArrayRef> = vec![Arc::new(ts)];
    if rows[0].1.is_some() {
        fields.push(Field::new("meter", DataType::Utf8, false));
        cols.push(Arc::new(StringArray::from_iter_values(
            rows.iter().map(|r| r.1.unwrap()),
        )));
    }
    fields.push(Field::new("kw", DataType::Float64, false));
    cols.push(Arc::new(Float64Array::from_iter_values(
        rows.iter().map(|r| r.0 as f64 + 0.5),
    )));
    let b = RecordBatch::try_new(Arc::new(Schema::new(fields)), cols).unwrap();
    let mut buf = Vec::new();
    {
        let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &b.schema()).unwrap();
        w.write(&b).unwrap();
        w.finish().unwrap();
    }
    std::fs::write(dir.join(format!("{name}.arrows")), buf).unwrap();
}

#[test]
fn times_finer_than_a_millisecond_are_profiled_as_they_are() {
    // #15: the profile read every time as epoch milliseconds. Rows 500 µs apart were "a row
    // every 1 millisecond", half of them "a time already present", and the extent ended at
    // `.009` where the last row is at `.0095`: three false facts. The profile reads
    // microseconds, as the bench holds them, from a table read back, from files watched as
    // they are written, and from a connector's stream.
    let (fast, pair) = sub_ms_csv();
    let dir = common::unique("sub-ms");
    std::fs::create_dir_all(&dir).unwrap();
    let data = dir.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("fast.csv"), &fast).unwrap();
    std::fs::write(data.join("pair.csv"), &pair).unwrap();
    let check = |what: &str, d: &str| {
        for want in [
            "time column ts (looks like it): 2026-01-01 00:00:00+00 to \
             2026-01-01 00:00:00.0095+00; most rows are 500 microseconds apart",
            "a row every 500 microseconds from first to last, none missing or repeated",
            "most series have a row every 500 microseconds",
            "1 gap, 1 row missing in all; the longest 500 microseconds (no rows after \
             2026-01-01 00:00:00.0045+00 until 2026-01-01 00:00:00.0055+00)",
        ] {
            assert!(d.contains(want), "{what}: expected {want:?} in:\n{d}");
        }
        assert!(
            !d.contains("a time already present") && !d.contains("millisecond"),
            "{what}:\n{d}"
        );
    };

    // Read back: tables written by SQL, which no watch saw.
    let sql = format!(
        "CREATE TABLE fast AS SELECT CAST(ts AS TIMESTAMPTZ) AS ts, kw FROM read_csv('{}');
         CREATE TABLE pair AS SELECT CAST(ts AS TIMESTAMPTZ) AS ts, meter, kw
           FROM read_csv('{}');",
        data.join("fast.csv").display(),
        data.join("pair.csv").display()
    );
    check(
        "read back",
        &recorded(&sql, &["fast", "pair"]).describe(4096),
    );
    // A time far from 1970 is said to the microsecond too: as a double, 9999-12-31 is held to
    // 32 µs, and the extent was read as one.
    let far = recorded(
        "CREATE TABLE far AS SELECT * FROM (VALUES
           (TIMESTAMPTZ '2026-01-01 00:00:00+00', 1.0),
           (TIMESTAMPTZ '9999-12-31 23:59:59.999999+00', 2.0)) v(ts, kw);",
        &["far"],
    )
    .describe(4096);
    assert!(
        far.contains("2026-01-01 00:00:00+00 to 9999-12-31 23:59:59.999999+00"),
        "{far}"
    );

    let described = |s: &mut common::Server| {
        let v = s.call("tools/call", json!({"name": "describe", "arguments": {}}));
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // A folder of CSV files, watched as each is written.
    let mut files = common::Server::start(&data, &[]);
    check("files", &described(&mut files));

    // A connector's stream for each table.
    let one: Vec<(i64, Option<&str>)> = (0..20).map(|i| (i, None)).collect();
    let two: Vec<(i64, Option<&str>)> = (0..20)
        .flat_map(|i| [(i, Some("a")), (i, Some("b"))])
        .filter(|r| !(r.0 == 10 && r.1 == Some("b")))
        .collect();
    sub_ms_stream(&dir, "fast", &one);
    sub_ms_stream(&dir, "pair", &two);
    let mut cfg = String::new();
    for (name, keyed) in [("fast", false), ("pair", true)] {
        let c = dir.join(format!("{name}.sh"));
        let body = format!(
            "#!/bin/sh\ncat '{}'\n",
            dir.join(format!("{name}.arrows")).display()
        );
        std::fs::write(&c, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
        let meter = if keyed {
            "[[source.schema]]\nname = \"meter\"\ntype = \"string\"\nnullable = false\n"
        } else {
            ""
        };
        cfg.push_str(&format!(
            "[[source]]\nname = \"{name}\"\ntype = \"exec\"\ncommand = \"./{name}.sh\"\n\
             [[source.schema]]\nname = \"ts\"\ntype = \"timestamptz\"\nnullable = false\n\
             {meter}[[source.schema]]\nname = \"kw\"\ntype = \"float64\"\nnullable = false\n"
        ));
    }
    let toml = dir.join("benchy.toml");
    std::fs::write(&toml, cfg).unwrap();
    let mut connector = common::Server::launch(&["--config", toml.to_str().unwrap()], &[]);
    check("connector", &described(&mut connector));
    let _ = std::fs::remove_dir_all(&dir);
}
