//! REQ-4: putting files on the bench, and what the agent is shown about them.

mod common;

use benchy_mcp::{source, Caps};

fn fixture_dir() -> std::path::PathBuf {
    let d = common::unique("src");
    std::fs::create_dir(&d).expect("create this test's input directory");
    std::fs::write(
        d.join("meters.csv"),
        "observed_at,meter,power_kw\n2026-01-01T00:00:00Z,lab,2.0\n2026-01-01T00:15:00Z,lab,4.0\n\
         2026-01-01T00:30:00Z,office,1.5\n",
    )
    .unwrap();
    std::fs::write(d.join("notes.txt"), "ignored\n").unwrap();
    d
}

#[test]
fn json_and_ndjson_load_too() {
    let d = std::env::temp_dir().join(format!("benchy-json-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    std::fs::write(
        d.join("events.ndjson"),
        "{\"id\":1,\"kind\":\"open\"}\n{\"id\":2,\"kind\":\"close\"}\n",
    )
    .unwrap();
    std::fs::write(
        d.join("config.json"),
        "[{\"key\":\"retries\",\"value\":3},{\"key\":\"timeout\",\"value\":30}]",
    )
    .unwrap();

    let mut bench = common::bench();
    let loaded = source::load_dir(&mut bench, &d).expect("json loads");
    let names: Vec<&str> = loaded.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"events"),
        "ndjson became a table: {names:?}"
    );
    assert!(names.contains(&"config"), "json became a table: {names:?}");

    let mut ws = bench.seal("1GB").expect("seals");
    let out = ws
        .work("SELECT kind FROM events ORDER BY id", &Caps::default())
        .expect("queries");
    assert_eq!(out[0].rows.len(), 2);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn an_unsupported_file_says_what_is_supported() {
    let d = std::env::temp_dir().join(format!("benchy-unsup-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    std::fs::write(d.join("data.xlsx"), "not really a spreadsheet").unwrap();
    // Both ways a file arrives: a directory with nothing readable in it, and one named file.
    let mut bench = common::bench();
    let from_dir = source::load_dir(&mut bench, &d).unwrap_err().to_string();
    let named = source::load_file(&mut bench, "sheet", &d.join("data.xlsx"))
        .unwrap_err()
        .to_string();
    for err in [from_dir, named] {
        for ext in [".csv", ".tsv", ".parquet", ".json", ".ndjson", ".jsonl"] {
            assert!(err.contains(ext), "lists {ext}: {err}");
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_directory_becomes_a_bench() {
    let dir = fixture_dir();
    let mut bench = common::bench();
    let loaded = source::load_dir(&mut bench, &dir).expect("loads");
    assert_eq!(
        loaded.len(),
        1,
        "only supported files are loaded; notes.txt is skipped"
    );
    assert_eq!(loaded[0].name, "meters");
    assert_eq!(loaded[0].rows, 3);

    let mut ws = bench.seal("1GB").expect("seals");
    let out = ws
        .work(
            "SELECT meter, sum(power_kw) AS kw FROM meters GROUP BY 1 ORDER BY 1",
            &Caps::default(),
        )
        .expect("queries");
    assert_eq!(out[0].rows.len(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_catalogue_carries_no_path() {
    let dir = fixture_dir();
    let mut bench = common::bench();
    source::load_dir(&mut bench, &dir).expect("loads");
    let ws = bench.seal("1GB").expect("seals");

    let described = ws.describe(4096);
    assert!(described.contains("meters"), "names the table: {described}");
    assert!(
        described.contains("power_kw"),
        "names the columns: {described}"
    );
    assert!(
        !described.contains(&dir.display().to_string()),
        "the catalogue must never carry a path: {described}"
    );
    assert!(!described.contains(".csv"), "nor a file name: {described}");
    // REQ-11: the highest-value line is benchy's delta from stock DuckDB
    assert!(
        described.contains("already loaded"),
        "tells the agent not to reach for readers"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_catalogue_is_bounded() {
    let dir = std::env::temp_dir().join(format!("benchy-many-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    for i in 0..60 {
        std::fs::write(dir.join(format!("table_{i:03}.csv")), "a,b,c\n1,2,3\n").unwrap();
    }
    let mut bench = common::bench();
    source::load_dir(&mut bench, &dir).expect("loads");
    let ws = bench.seal("1GB").expect("seals");

    let budget = 1024;
    let described = ws.describe(budget);
    assert!(
        described.len() < budget * 2,
        "stays near its budget: {} bytes",
        described.len()
    );
    assert!(
        described.contains("more table(s) not shown"),
        "says it was cut"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_name_collision_is_refused_with_both_paths() {
    let dir = std::env::temp_dir().join(format!("benchy-dup-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // Same table name, different files on every filesystem: `Sales.csv` would be the same file
    // as `sales.csv` on macOS, whose default filesystem ignores case.
    std::fs::write(dir.join("sales.csv"), "a\n1\n").unwrap();
    std::fs::write(dir.join("sales.tsv"), "a\n1\n").unwrap();
    let mut bench = common::bench();
    let err = source::load_dir(&mut bench, &dir).unwrap_err().to_string();
    assert!(err.contains("`sales`"), "names the collision: {err}");
    assert!(
        err.contains("/sales.csv") && err.contains("/sales.tsv"),
        "and both files: {err}"
    );
    assert!(err.contains("--source"), "says how to fix it: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hidden_file_in_a_directory_is_not_a_table() {
    // A dotfile is one the operator may not know is there: `.secret.csv` became `secret`, and the
    // AppleDouble `._meters.csv` that macOS writes beside `meters.csv` on a non-APFS volume
    // stopped startup as a second `meters`.
    let dir = fixture_dir();
    std::fs::write(dir.join(".secret.csv"), "a\n1\n").unwrap();
    std::fs::write(dir.join("._meters.csv"), "\0\x05\x16\x07").unwrap();
    let planned = source::plan_dir(&dir).expect("the hidden files do not stop startup");
    let names: Vec<&str> = planned.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["meters"], "{planned:?}");

    // A folder of nothing but dotfiles has nothing to load, and says so as an empty one does.
    let hidden = common::unique("hidden");
    std::fs::create_dir(&hidden).unwrap();
    std::fs::write(hidden.join(".only.csv"), "a\n1\n").unwrap();
    let err = source::plan_dir(&hidden).unwrap_err().to_string();
    assert!(err.contains("no readable data files"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&hidden);
}

#[test]
fn a_hidden_file_named_explicitly_still_loads() {
    // Named with --source or in a config, the operator chose it: only the folder scan skips it.
    let dir = common::unique("named-hidden");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join(".secret.csv"), "a\n1\n2\n").unwrap();
    let mut bench = common::bench();
    let t = source::load_file(&mut bench, "secret", &dir.join(".secret.csv")).expect("loads");
    assert_eq!((t.name.as_str(), t.rows), ("secret", 2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_directory_says_what_to_do() {
    let dir = std::env::temp_dir().join(format!("benchy-empty-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut bench = common::bench();
    let err = source::load_dir(&mut bench, &dir).unwrap_err().to_string();
    assert!(err.contains(".csv"), "says what it looks for: {err}");
    assert!(err.contains("--source"), "and the alternative: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn table_names_are_plain_identifiers() {
    assert_eq!(source::table_name("meters").as_deref(), Some("meters"));
    assert_eq!(source::table_name("Sales Q1").as_deref(), Some("sales_q1"));
    assert_eq!(
        source::table_name("2026-data").as_deref(),
        Some("t_2026_data")
    );
    assert_eq!(source::table_name("__x__").as_deref(), Some("x"));
    // As the README says: a letter outside ASCII and any other character is dropped, `_ - .` and
    // a space each become `_`, and nothing is collapsed.
    assert_eq!(
        source::table_name("Zähler (alt)").as_deref(),
        Some("zhler_alt")
    );
    assert_eq!(source::table_name("a.-b").as_deref(), Some("a__b"));
    assert_eq!(source::table_name("!!!"), None);
}

#[test]
fn loading_never_downloads_an_extension() {
    // A reader the build does not include must fail loudly at load time, never be fetched from
    // the internet. DuckDB's default is to install and load a known extension on first use, which
    // would turn a missing reader into a silent download of code during loading — before the
    // seal switches it off — and would hide the missing reader from any test with network access.
    let b = common::bench();
    let setting = |name: &str| -> String {
        b.connection()
            .query_row(
                &format!("SELECT current_setting('{name}')::VARCHAR"),
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(setting("autoinstall_known_extensions"), "false");
    assert_eq!(setting("autoload_known_extensions"), "false");
}

/// Holding one row group of a bulk insert before writing it, not DuckDB's five, a stream's or a
/// Parquet file's loader peaks 11–16 MB lower; a CSV's, staged in memory first, does not move.
#[test]
fn a_loader_writes_each_row_group_as_it_fills() {
    // DuckDB holds five row groups of a bulk insert before it writes them out; a loader writes
    // one table, once, from one thread, and has no use for them. Held to one, a stream's or a
    // Parquet file's loader peaked 11 to 16 MB lower with the same instructions, and every
    // source is still written the same way (docs/history/PERFORMANCE-rounds.md R2-8;
    // `the_physical_layout_says_nothing_of_where_data_came_from`).
    let b = common::bench();
    let held: String = b
        .connection()
        .query_row(
            "SELECT current_setting('write_buffer_row_group_count')::VARCHAR",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(held, "1");
}
