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

#[test]
fn describe_tells_the_time_column_its_range_and_cadence() {
    let d = described();
    has(
        &d,
        "time column ts (looks like it): 2014-03-27 00:00:00+00 to 2014-04-02 23:45:00+00",
    );
    has(&d, "most series have a row every 15 minutes");
}

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
    has(
        &heuristic,
        "'north': 200 rows repeat a time already present",
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
    // and no count is more than the column's values.
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
    // The keys are held as text, and were listed in text order: `50 series (1 … 9)`.
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
    // 300 series, each with a two-day pause: far more notes than any budget holds.
    let mut csv = String::from("ts,meter,kw\n");
    for i in 0..(10 * 24) {
        let t = START + i * 3600;
        for s in 0..300 {
            let paused = (s * 7 % 200..s * 7 % 200 + 48).contains(&i);
            csv.push_str(&format!(
                "{},s{s:03},{}\n",
                stamp(t),
                if paused { 0 } else { 1 + s }
            ));
        }
    }
    let dir = dir_with("many.csv", &csv);
    let ws = sealed(&dir, &[]);
    // 4,600 bytes: every time is shown as an instant, `+00` and all (REQ-15), so forty notes
    // take that much more room than they did as zone-less times at 4,096.
    let d = ws.describe(4600);
    assert!(
        benchy_mcp::result::wire_len(&d) <= 4600,
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
