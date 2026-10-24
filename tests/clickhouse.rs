//! REQ-16: ClickHouse as a built-in source, against a real ClickHouse server
//! (`common::clickhouse`, started from `BENCHY_CLICKHOUSE_BIN`; without it each test that needs
//! the server says it was skipped). The config test at the end needs none. Linux only: the server
//! is tied to the test binary's life by `PR_SET_PDEATHSIG`, which only Linux has.
#![cfg(target_os = "linux")]

mod common;
use benchy_mcp::admission::Budget;
use benchy_mcp::exec::{Fault, Fetched};
use benchy_mcp::workspace::LoadedTable;
use benchy_mcp::{config, Bench};
use common::clickhouse::{free_port, PASSWORD};
use common::{unique, Server, BIN};
use std::path::Path;
use std::time::{Duration, Instant};

/// A config's text, parsed as the server would.
fn sources(text: &str) -> config::Sources {
    config::parse(text, Path::new("/nonexistent/benchy.toml")).unwrap_or_else(|e| panic!("{e}"))
}

/// Load the config's one built-in source onto a bench of its own, as a loader does.
fn load(text: &str) -> (Result<(LoadedTable, Fetched), Fault>, Bench) {
    load_within(text, &Budget::default())
}

fn load_within(text: &str, budget: &Budget) -> (Result<(LoadedTable, Fetched), Fault>, Bench) {
    let s = sources(text);
    assert_eq!(s.builtin.len(), 1, "one source");
    let mut bench = common::bench();
    let r = s.builtin[0].load(&mut bench, budget);
    (r, bench)
}

/// Every value as DuckDB writes it as text, `NULL` for a null.
fn rows(bench: &Bench, sql: &str) -> Vec<String> {
    let conn = bench.connection();
    let mut stmt = conn
        .prepare(&format!("SELECT COLUMNS(*)::VARCHAR FROM ({sql})"))
        .unwrap();
    let mut out = Vec::new();
    let mut r = stmt.query([]).unwrap();
    while let Some(row) = r.next().unwrap() {
        let n = row.as_ref().column_count();
        let cells: Vec<String> = (0..n)
            .map(|i| match row.get_ref(i).unwrap() {
                duckdb::types::ValueRef::Null => "NULL".to_string(),
                _ => row.get::<_, String>(i).unwrap_or_else(|_| "?".into()),
            })
            .collect();
        out.push(cells.join("|"));
    }
    out
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Against the server: a Tokyo `DateTime` loads as the same instant in UTC.
#[test]
fn a_clickhouse_table_loads_with_its_schema_read_and_mapped() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_mapped");
    ch.sql(&format!(
        "CREATE TABLE {db}.typed (i8 Int8, u32 UInt32, u64 UInt64, f32 Float32, d Date, \
         dt DateTime('Asia/Tokyo'), dt3 DateTime64(3), dec Decimal(10, 3), b Bool, \
         e Enum8('on' = 1, 'off' = 2), u UUID, ip IPv4, s Nullable(String), fs FixedString(4), \
         lc LowCardinality(String), arr Array(Int32), m Map(String, UInt8), \
         agg AggregateFunction(sum, UInt64)) ENGINE = MergeTree ORDER BY tuple()"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.typed SELECT -5, 4000000000, 18446744073709551615, 1.5, \
         '2026-02-28', toDateTime('2026-03-01 09:00:00', 'Asia/Tokyo'), \
         toDateTime64('2026-03-01 00:00:00.123', 3, 'UTC'), 12.345, true, 'off', \
         '01234567-89ab-cdef-0123-456789abcdef', '10.1.2.3', NULL, 'ab', 'lab', [1, 2], \
         map('k', 7), sumState(toUInt64(3))"
    ));
    let cfg = format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\ntables = [\"typed\"]\n",
        ch.url
    );
    let (r, bench) = load(&cfg);
    let (table, fetched) = r.unwrap_or_else(|f| panic!("{}", f.detail));
    assert_eq!(fetched.rows, 1);
    let types: Vec<String> = table
        .columns
        .iter()
        .map(|(n, t)| format!("{n} {t}"))
        .collect();
    assert_eq!(
        types,
        [
            "i8 BIGINT",
            "u32 BIGINT",
            "u64 HUGEINT",
            "f32 DOUBLE",
            "d DATE",
            "dt TIMESTAMP WITH TIME ZONE",
            "dt3 TIMESTAMP WITH TIME ZONE",
            "dec DECIMAL(10,3)",
            "b BOOLEAN",
            "e VARCHAR",
            "u VARCHAR",
            "ip VARCHAR",
            "s VARCHAR",
            "fs VARCHAR",
            "lc VARCHAR",
            "arr VARCHAR",
            "m VARCHAR",
        ],
        "the aggregate state is not loaded"
    );
    // The values, exactly: the Tokyo DateTime is the same instant in UTC, the FixedString loses
    // its padding, the containers are JSON.
    let got = rows(
        &bench,
        "SELECT i8, u32, u64::VARCHAR, f32, d::VARCHAR, epoch_us(dt), epoch_us(dt3), dec::VARCHAR, \
         b, e, u, ip, s, fs, length(fs), lc, arr, m FROM typed",
    );
    assert_eq!(
        got,
        [
            "-5|4000000000|18446744073709551615|1.5|2026-02-28|1772323200000000|1772323200123000|\
          12.345|true|off|01234567-89ab-cdef-0123-456789abcdef|10.1.2.3|NULL|ab|2|lab|[1,2]|\
          {\"k\":7}"
        ]
    );
}

/// Origin-blind (REQ-15): the ClickHouse table holds `DateTime`, `LowCardinality(String)`,
/// `Nullable(Float32)` and `UInt16`, and `describe` matches, profile included.
#[test]
fn the_same_rows_from_csv_and_clickhouse_describe_and_answer_identically() {
    // Origin-blind: the agent cannot tell which of two tables came from a CSV file and which from
    // ClickHouse. The same rows give the same column types, the same `describe` — profile and all
    // — and the same answers.
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_blind");
    let mut csv = String::from("ts,meter,kw,n\n");
    for m in 1..=3 {
        for i in 0..200 {
            if m == 2 && (50..=60).contains(&i) {
                continue; // a gap
            }
            let ts = 1_767_225_600 + i * 900; // 2026-01-01T00:00:00Z, every 15 minutes
            let kw = if m == 3 && (10..=20).contains(&i) {
                String::new() // nulls
            } else {
                format!("{}", (i % 13) as f64 * 0.25)
            };
            let t = benchy_mcp::result::timestamp_utc(ts * 1_000_000)
                .replace(' ', "T")
                .replace("+00", "Z");
            csv.push_str(&format!("{t},m{m},{kw},{}\n", i % 7));
        }
    }
    let dir = unique("blind");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("readings.csv"), &csv).unwrap();
    // Types a ClickHouse table of meter readings would have, none of them the CSV's.
    ch.sql(&format!(
        "CREATE TABLE {db}.readings (ts DateTime, meter LowCardinality(String), \
         kw Nullable(Float32), n UInt16) ENGINE = MergeTree ORDER BY (meter, ts)"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.readings FORMAT CSVWithNames\n{csv}"
    ));
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"from_csv\"\ntype = \"file\"\npath = \"readings.csv\"\n\
             [[source]]\nname = \"from_ch\"\ntype = \"clickhouse\"\nurl = \"{}\"\n\
             database = \"{db}\"\nquery = \"SELECT * FROM readings\"\n",
            ch.url
        ),
    )
    .unwrap();
    let mut s = Server::launch(&["--config", cfg.to_str().unwrap()], &[]);
    let v = s.call(
        "tools/call",
        serde_json::json!({"name": "describe", "arguments": {}}),
    );
    let text = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    // A table's section: its line, and the indented lines under it.
    let section = |name: &str| -> String {
        let mut lines = text
            .lines()
            .skip_while(|l| !l.starts_with(&format!("{name} (")));
        let head = lines.next().unwrap_or_else(|| panic!("{name} in {text}"));
        let body: Vec<&str> = lines.take_while(|l| l.starts_with(' ')).collect();
        format!("{}\n{}", head.replacen(name, "TABLE", 1), body.join("\n"))
    };
    let (from_csv, from_ch) = (section("from_csv"), section("from_ch"));
    assert!(from_csv.contains("series key meter"), "{from_csv}");
    assert!(
        from_csv.contains("null"),
        "the profile saw the nulls: {from_csv}"
    );
    assert_eq!(from_csv, from_ch, "describe tells them apart:\n{text}");

    for sql in [
        "SELECT typeof(ts), typeof(meter), typeof(kw), typeof(n) FROM {t} LIMIT 1",
        "SELECT count(*), count(kw), sum(kw), sum(n), min(ts), max(ts), sum(epoch_us(ts)) FROM {t}",
        "SELECT meter, count(*), avg(kw) FROM {t} GROUP BY ALL ORDER BY ALL",
        "SELECT * FROM {t} ORDER BY meter, ts LIMIT 40",
    ] {
        let (e1, a, _) = s.query(&sql.replace("{t}", "from_csv"));
        let (e2, b, _) = s.query(&sql.replace("{t}", "from_ch"));
        assert!(!e1 && !e2, "{a}\n{b}");
        // Result ids differ by call; the rest must not.
        let strip = |t: &str| {
            t.lines()
                .filter(|l| !l.contains("result_id"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(strip(&a), strip(&b), "{sql}");
    }
    // And as multisets, every row.
    for (x, y) in [("from_csv", "from_ch"), ("from_ch", "from_csv")] {
        let (err, body, _) = s.query(&format!(
            "SELECT count(*) AS differ FROM (SELECT * FROM {x} EXCEPT ALL SELECT * FROM {y})"
        ));
        assert!(!err && body.contains("differ\n0"), "{x} minus {y}: {body}");
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The ids are ones a careless connector would splice into SQL.
#[test]
fn the_window_and_the_allowlist_are_pushed_down_as_parameters() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_pushdown");
    // Ids a careless connector would splice into SQL and break on — or worse.
    let hostile = [
        "it's",
        "back\\slash",
        "{benchy_ids:String}",
        "') OR 1=1 --",
        "tab\there",
        "Zähler ü",
        "`tick`",
        "plain",
    ];
    ch.sql(&format!(
        "CREATE TABLE {db}.readings (ts DateTime64(3), meter String, kw Float64) \
         ENGINE = MergeTree ORDER BY (meter, ts)"
    ));
    let mut values = Vec::new();
    for (i, m) in hostile.iter().chain(["other", "x"].iter()).enumerate() {
        for h in 0..48 {
            values.push(format!(
                "(toDateTime64('2026-01-01 00:00:00', 3, 'UTC') + INTERVAL {h} HOUR, '{}', {i}.5)",
                m.replace('\\', "\\\\").replace('\'', "\\'")
            ));
        }
    }
    ch.sql(&format!(
        "INSERT INTO {db}.readings VALUES {}",
        values.join(", ")
    ));
    let ids: Vec<String> = hostile[..7].iter().map(|s| toml_str(s)).collect();
    let cfg = format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\n\
         tables = [\"readings\"]\n[source.narrow]\ncolumn = \"ts\"\n\
         start = \"2026-01-01T06:00:00Z\"\nend = \"2026-01-01T10:00:00+01:00\"\n\
         entity_column = \"meter\"\nentity_ids = [{}]\n",
        ch.url,
        ids.join(", ")
    );
    let (r, bench) = load(&cfg);
    let (_, fetched) = r.unwrap_or_else(|f| panic!("{}", f.detail));
    // 06:00 to 09:00 UTC, half-open: three hours, for each of the seven hostile ids.
    assert_eq!(
        fetched.rows,
        7 * 3,
        "only what the narrowing admits was sent"
    );
    let got = rows(
        &bench,
        "SELECT meter, count(*), min(ts)::VARCHAR, max(ts)::VARCHAR FROM readings GROUP BY 1 \
         ORDER BY 1",
    );
    let mut want: Vec<String> = hostile[..7]
        .iter()
        .map(|m| format!("{m}|3|2026-01-01 06:00:00+00|2026-01-01 08:00:00+00"))
        .collect();
    want.sort();
    assert_eq!(got, want);
    // ClickHouse filtered: it returned only the narrowed rows. Its query log writes a query with
    // its parameters bound, each as a typed `_CAST` literal of ClickHouse's own making — which
    // shows they arrived as parameters; that the text benchy sends holds no value is
    // `values_travel_as_query_parameters_never_in_the_sql`.
    let logged = ch.logged(&format!("{db}.readings"));
    let fetch: Vec<_> = logged
        .iter()
        .filter(|(q, _, _)| q.contains("ArrowStream"))
        .collect();
    assert_eq!(fetch.len(), 1, "{logged:#?}");
    assert_eq!(fetch[0].1, 21, "ClickHouse returned only the narrowed rows");
    assert!(
        fetch[0].0.contains("_CAST(1767247200000000, 'Int64')")
            && fetch[0].0.contains("'Array(String)')"),
        "{}",
        fetch[0].0
    );
}

#[test]
fn only_the_pinned_or_named_columns_are_fetched() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_columns");
    ch.sql(&format!(
        "CREATE TABLE {db}.wide (ts DateTime, meter String, kw Float64, note String, \
         secret String) ENGINE = MergeTree ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.wide SELECT toDateTime('2026-01-01 00:00:00', 'UTC') + number, \
         'm', number, 'n', 'do-not-fetch' FROM numbers(10)"
    ));
    for (extra, want) in [
        (
            "[[source.schema]]\nname = \"ts\"\ntype = \"timestamptz\"\nnullable = false\n\
             [[source.schema]]\nname = \"kw\"\ntype = \"float64\"\n",
            ["ts", "kw"],
        ),
        (
            "[source.narrow]\ncolumns = [\"meter\", \"kw\"]\n",
            ["meter", "kw"],
        ),
    ] {
        let cfg = format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\n\
             tables = [\"wide\"]\n{extra}",
            ch.url
        );
        let (r, _) = load(&cfg);
        let (table, _) = r.unwrap_or_else(|f| panic!("{}", f.detail));
        let names: Vec<&str> = table.columns.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, want);
    }
    // ClickHouse read only those columns: the one it was not asked for was never read.
    for (q, _, columns) in ch.logged(&format!("`{db}`.`wide`")) {
        assert!(
            !columns
                .iter()
                .any(|c| c.ends_with(".secret") || c.ends_with(".note")),
            "{q}: {columns:?}"
        );
    }
}

#[test]
fn a_pinned_schema_that_does_not_match_refuses_the_load() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_pinned");
    ch.sql(&format!(
        "CREATE TABLE {db}.r (ts DateTime, kw Float32, flag Nullable(UInt8)) \
         ENGINE = MergeTree ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.r VALUES ('2026-01-01 00:00:00', 1.5, NULL)"
    ));
    let base = format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\ntables = [\"r\"]\n",
        ch.url
    );
    for (pin, says) in [
        // A type that is not what the column loads as.
        ("name = \"kw\"\ntype = \"int64\"", "loads as float64"),
        // A column that is not there.
        ("name = \"missing\"\ntype = \"float64\"", "no such column"),
        // Not null, where the data holds a null: checked in the rows, not the declaration.
        (
            "name = \"flag\"\ntype = \"int64\"\nnullable = false",
            "null",
        ),
    ] {
        let (r, _) = load(&format!("{base}[[source.schema]]\n{pin}\n"));
        let Err(f) = r else { panic!("{pin} loaded") };
        assert!(
            matches!(f.code, "E_SCHEMA" | "E_HOST_SCHEMA_MISMATCH"),
            "{pin}: {}",
            f.code
        );
        assert!(f.detail.contains(says), "{pin}: {}", f.detail);
    }
    // Pinned as it loads, it loads.
    let (r, _) = load(&format!(
        "{base}[[source.schema]]\nname = \"kw\"\ntype = \"float64\"\n"
    ));
    assert!(r.is_ok());
}

#[test]
fn an_over_budget_clickhouse_load_is_refused_before_transfer() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_budget");
    ch.sql(&format!(
        "CREATE TABLE {db}.big (ts DateTime, meter String, kw Float64) ENGINE = MergeTree \
         ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.big SELECT toDateTime('2026-01-01 00:00:00', 'UTC') + number, \
         concat('meter-', toString(number % 50)), number FROM numbers(100000)"
    ));
    let base = format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\ntables = [\"big\"]\n",
        ch.url
    );
    // Over the source's own cap, and over what the session has left.
    let (r, _) = load(&format!("{base}max_rows = 1000\n"));
    let f = r.err().expect("refused");
    assert_eq!(f.code, "E_TOO_LARGE", "{}", f.detail);
    assert!(f.detail.contains("before fetching"), "{}", f.detail);
    let (r, _) = load_within(&base, &Budget::new(10_000_000, 1_000_000));
    let f = r.err().expect("refused");
    assert_eq!(f.code, "E_TOO_LARGE", "{}", f.detail);
    // ClickHouse counted; it never sent a row.
    let logged = ch.logged(&format!("`{db}`.`big`"));
    assert!(
        logged.iter().any(|(q, _, _)| q.contains("count()")),
        "{logged:#?}"
    );
    assert!(
        !logged.iter().any(|(q, _, _)| q.contains("ArrowStream")),
        "{logged:#?}"
    );
    // Under the budget, the estimate was close to what arrived.
    let (r, _) = load(&base);
    let (_, fetched) = r.unwrap_or_else(|f| panic!("{}", f.detail));
    assert_eq!(fetched.rows, 100_000);
}

/// zstd moves a quarter of the bytes or fewer, and the cap counts the decoded Arrow: a cap between
/// the two sizes refuses the load.
#[test]
fn a_compressed_response_is_decoded_and_bounded() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_zstd");
    ch.sql(&format!(
        "CREATE TABLE {db}.z (ts DateTime, meter String, kw Float64) ENGINE = MergeTree \
         ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.z SELECT toDateTime('2026-01-01 00:00:00', 'UTC') + number * 60, \
         concat('meter-', toString(number % 20)), number % 100 FROM numbers(200000)"
    ));
    let cfg = |compression: &str, extra: &str| {
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\n\
             tables = [\"z\"]\ncompression = \"{compression}\"\n{extra}",
            ch.url
        )
    };
    let fetch = |text: &str| {
        let s = sources(text);
        let benchy_mcp::sources::BuiltIn::Clickhouse(src) = &s.builtin[0] else {
            panic!("a clickhouse source");
        };
        let plan = src.plan().unwrap();
        src.fetch_counted(
            &plan,
            src.limits.max_rows,
            src.limits.max_bytes,
            None,
            &benchy_mcp::admission::Budget::default(),
            &mut |_| Ok(()),
        )
    };
    let (plain, plain_wire) = fetch(&cfg("none", "")).unwrap();
    let (zstd, zstd_wire) = fetch(&cfg("zstd", "")).unwrap();
    let (lz4, lz4_wire) = fetch(&cfg("lz4", "")).unwrap();
    assert_eq!(plain.rows, 200_000);
    assert_eq!(zstd.rows, plain.rows);
    // The same Arrow, either way; what crossed the network is what differs.
    assert_eq!(zstd.bytes, plain.bytes);
    assert_eq!(plain_wire, plain.bytes);
    assert!(zstd_wire * 4 < plain_wire, "{zstd_wire} of {plain_wire}");
    // Arrow's LZ4 compresses each body: fewer bytes, the same rows.
    assert_eq!(lz4.rows, plain.rows);
    assert!(lz4_wire < plain_wire, "{lz4_wire} of {plain_wire}");
    // The byte cap counts what was decoded, not what crossed: a cap between the two refuses.
    let cap = (zstd_wire + plain.bytes) / 2;
    let f = fetch(&cfg("zstd", &format!("max_bytes = {cap}\n")))
        .err()
        .expect("refused");
    assert_eq!(f.code, "E_TOO_LARGE", "{}", f.detail);
}

/// The user is sent no setting once it has been refused them, and loads with each `compression`.
/// A profile that makes zstd Arrow bodies or binary text, which a read-only user cannot change,
/// refuses the load before ClickHouse sends a row, naming the setting.
#[test]
fn a_read_only_clickhouse_user_loads_what_any_user_loads() {
    // `readonly = 1` is how analytics accounts are usually set up, and it refuses every setting a
    // request names. benchy then sends none: the SELECT already converts every column, and
    // ClickHouse's own Arrow output — LZ4 bodies by default — is what the reader takes anyway.
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_readonly");
    ch.sql(&format!(
        "CREATE TABLE {db}.r (ts DateTime, meter LowCardinality(String), kw Float32, \
         note Nullable(String), d Date, id UInt64) ENGINE = MergeTree ORDER BY (meter, ts)"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.r SELECT toDateTime('2026-01-01 00:00:00', 'UTC') + number * 900, \
         concat('m', toString(number % 3)), number / 4, if(number % 5 = 0, NULL, 'n'), \
         toDate('2026-01-01') + number % 9, 18446744073709551615 - number FROM numbers(5000)"
    ));
    let cfg = |user: &str, compression: &str| {
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\n\
             tables = [\"r\"]\ncompression = \"{compression}\"\n\
             env = {{ CLICKHOUSE_USER = \"{user}\" }}\n\
             [source.narrow]\ncolumn = \"ts\"\nstart = \"2026-01-01T06:00:00Z\"\n\
             entity_column = \"meter\"\nentity_ids = [\"m0\", \"m2\"]\n",
            ch.url
        )
    };
    let all = "SELECT * FROM r ORDER BY meter, ts";
    let (r, bench) = load(&cfg("default", "none"));
    let (want_table, want) = r.unwrap_or_else(|f| panic!("{}", f.detail));
    let want_rows = rows(&bench, all);
    assert!(want.rows > 3000, "{}", want.rows);
    for compression in ["zstd", "lz4", "none"] {
        let (r, bench) = load(&cfg("ro", compression));
        let (table, fetched) = r.unwrap_or_else(|f| panic!("{compression}: {}", f.detail));
        assert_eq!(table.columns, want_table.columns, "{compression}");
        assert_eq!(fetched.rows, want.rows, "{compression}");
        assert_eq!(rows(&bench, all), want_rows, "{compression}");
    }
    // A profile that makes ClickHouse compress Arrow bodies with zstd, which the reader does not
    // decode, or write text as binary, cannot be changed by a read-only user: refused before a
    // row is sent, naming the setting.
    for (user, setting) in [
        ("ro_zstd", "output_format_arrow_compression_method"),
        ("ro_binary", "output_format_arrow_string_as_string"),
    ] {
        let (r, _) = load(&cfg(user, "zstd"));
        let f = r.err().unwrap_or_else(|| panic!("{user}: loaded"));
        assert_eq!(f.code, "E_CONFIG", "{user}: {}", f.detail);
        assert!(f.detail.contains(setting), "{user}: {}", f.detail);
        assert!(f.detail.contains("readonly = 2"), "{user}: {}", f.detail);
    }
    let fetched_by = |user: &str| {
        ch.sql("SYSTEM FLUSH LOGS");
        ch.sql(&format!(
            "SELECT count() FROM system.query_log WHERE type = 'QueryFinish' AND user = '{user}' \
             AND query LIKE '%ArrowStream%' AND query LIKE '%{db}%' FORMAT TSV"
        ))
        .trim()
        .to_string()
    };
    assert_eq!(fetched_by("ro"), "3", "one fetch per compression, as `ro`");
    assert_eq!(fetched_by("ro_zstd"), "0");
    assert_eq!(fetched_by("ro_binary"), "0");
}

/// The unknown table's `E_CONFIG` carries ClickHouse's own code for the operator; the slow query
/// times out at its deadline.
#[test]
fn clickhouse_failures_map_to_fault_codes() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_faults");
    ch.sql(&format!(
        "CREATE TABLE {db}.r (ts DateTime, kw Float64) ENGINE = MergeTree ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.r VALUES ('2026-01-01 00:00:00', 1)"
    ));
    let at = |url: &str, rest: &str| {
        format!("[[source]]\ntype = \"clickhouse\"\nurl = \"{url}\"\ndatabase = \"{db}\"\n{rest}\n")
    };
    let fails = |text: &str| load(text).0.err().expect("fails");

    // Wrong password: E_AUTH, and the password is nowhere in what is reported.
    std::env::set_var("BENCHY_TEST_CH_WRONG", "not-the-password");
    let f = fails(&at(
        &ch.url,
        "tables = [\"r\"]\nenv = { CLICKHOUSE_USER = \"reader\", CLICKHOUSE_PASSWORD = \
         \"${BENCHY_TEST_CH_WRONG}\" }",
    ));
    assert_eq!(f.code, "E_AUTH", "{}", f.detail);
    assert!(!f.detail.contains("not-the-password"), "{}", f.detail);

    // Nothing listening: E_UNAVAILABLE.
    let closed = format!("http://127.0.0.1:{}", free_port());
    let f = fails(&at(&closed, "tables = [\"r\"]"));
    assert_eq!(f.code, "E_UNAVAILABLE", "{}", f.detail);

    // A table that is not there: E_CONFIG, with ClickHouse's own words for the operator.
    let f = fails(&at(&ch.url, "tables = [\"nope\"]"));
    assert_eq!(f.code, "E_CONFIG", "{}", f.detail);
    assert!(f.detail.contains("code 60"), "{}", f.detail);

    // Slower than its deadline: a timeout, at the deadline rather than when ClickHouse is done.
    let t = Instant::now();
    let f = fails(&at(
        &ch.url,
        "name = \"slow\"\nquery = \"SELECT number, sleepEachRow(0.5) AS s FROM numbers(40) \
         SETTINGS max_block_size = 1\"\ndeadline_secs = 2",
    ));
    assert!(
        matches!(f.code, "E_TIMEOUT" | "E_HOST_TIMEOUT"),
        "{}: {}",
        f.code,
        f.detail
    );
    assert!(t.elapsed() < Duration::from_secs(12), "{:?}", t.elapsed());
}

/// Served, the password from benchy's environment signs in; a wrong one stops startup with
/// `E_AUTH`, and neither password reaches the operator log.
#[test]
fn clickhouse_credentials_stay_in_the_loader() {
    // The password goes from benchy's environment, through the config, to the loader that
    // fetches — in a header — and to no other process. The engine never holds it, and a failed
    // sign-in does not write it to the operator log.
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_creds");
    let table = format!("creds_{}", std::process::id());
    ch.sql(&format!(
        "CREATE TABLE {db}.{table} (ts DateTime, kw Float64) ENGINE = MergeTree ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.{table} VALUES ('2026-01-01 00:00:00', 904)"
    ));
    let dir = unique("ch-creds");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\ndatabase = \"{db}\"\n\
             tables = [\"{table}\"]\n\
             env = {{ CLICKHOUSE_USER = \"reader\", CLICKHOUSE_PASSWORD = \"${{CH_PASSWORD}}\" }}\n",
            ch.url
        ),
    )
    .unwrap();
    let mut s = Server::launch(
        &["--config", cfg.to_str().unwrap()],
        &[("CH_PASSWORD", Path::new(PASSWORD))],
    );
    let (err, body, _) = s.query(&format!("SELECT kw FROM {table}"));
    assert!(!err && body.contains("904"), "{body}");
    let engine = common::engine_of(s.pid());
    for what in ["environ", "cmdline"] {
        let bytes = std::fs::read(format!("/proc/{engine}/{what}")).unwrap();
        assert!(!common::holds(&bytes, PASSWORD.as_bytes()), "{what}");
    }
    assert!(
        common::memory_holds(engine, table.as_bytes()),
        "the scan works"
    );
    assert!(
        !common::memory_holds(engine, PASSWORD.as_bytes()),
        "the password is in the engine's memory"
    );
    drop(s);

    // The wrong one: startup stops, the log says E_AUTH, and neither password is in it.
    let out = std::process::Command::new(BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .env("CH_PASSWORD", "wrong-password-0b1d")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(log.contains("E_AUTH"), "{log}");
    assert!(
        !log.contains("wrong-password-0b1d") && !log.contains(PASSWORD),
        "{log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `check` says when a user is read-only and which of ClickHouse's settings its load will run
/// with, and names the fix for an unknown table, a wrong password, a read-only user's profile that
/// makes Arrow benchy does not read, and an address with nothing listening.
#[test]
fn check_names_the_fix_for_each_clickhouse_failure() {
    let Some(ch) = common::clickhouse::server() else {
        return;
    };
    let db = ch.database("t_check");
    ch.sql(&format!(
        "CREATE TABLE {db}.r (ts DateTime, meter LowCardinality(String), kw Float32, \
         agg AggregateFunction(sum, UInt64)) ENGINE = MergeTree ORDER BY ts"
    ));
    ch.sql(&format!(
        "INSERT INTO {db}.r SELECT '2026-01-01 00:00:00', 'm', 1, sumState(toUInt64(1))"
    ));
    let dir = unique("ch-check");
    std::fs::create_dir_all(&dir).unwrap();
    let closed = format!("http://127.0.0.1:{}", free_port());
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{u}\"\ndatabase = \"{db}\"\n\
             tables = [\"r\"]\n\
             [[source]]\ntype = \"clickhouse\"\nurl = \"{u}\"\ndatabase = \"{db}\"\n\
             tables = [\"nope\"]\n\
             [[source]]\nname = \"locked\"\ntype = \"clickhouse\"\nurl = \"{u}\"\n\
             database = \"{db}\"\nquery = \"SELECT * FROM r\"\n\
             env = {{ CLICKHOUSE_USER = \"reader\", CLICKHOUSE_PASSWORD = \"wrong\" }}\n\
             [[source]]\nname = \"readonly\"\ntype = \"clickhouse\"\nurl = \"{u}\"\n\
             database = \"{db}\"\nquery = \"SELECT * FROM r\"\n\
             env = {{ CLICKHOUSE_USER = \"ro\" }}\n\
             [[source]]\nname = \"ro_zstd\"\ntype = \"clickhouse\"\nurl = \"{u}\"\n\
             database = \"{db}\"\nquery = \"SELECT * FROM r\"\n\
             env = {{ CLICKHOUSE_USER = \"ro_zstd\" }}\n\
             [[source]]\nname = \"away\"\ntype = \"clickhouse\"\nurl = \"{closed}\"\n\
             query = \"SELECT 1\"\n",
            u = ch.url
        ),
    )
    .unwrap();
    let check = |name: &str| {
        let out = std::process::Command::new(BIN)
            .args(["check", cfg.to_str().unwrap(), name])
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let (ok, text) = check("r");
    assert!(ok, "{text}");
    // The mapping, for the operator: ClickHouse's type and what benchy loads.
    assert!(
        text.contains("DateTime") && text.contains("-> timestamptz"),
        "{text}"
    );
    assert!(
        text.contains("LowCardinality(String)") && text.contains("-> string"),
        "{text}"
    );
    assert!(
        text.contains("`agg`") && text.contains("finalizeAggregation"),
        "{text}"
    );
    assert!(text.contains("estimate: 1 row(s)"), "{text}");
    assert!(!text.contains("readonly = 1"), "{text}");

    // A `readonly = 1` user loads, and `check` says how: with ClickHouse's own settings, which
    // it names, and no time limit ClickHouse knows of.
    let (ok, text) = check("readonly");
    assert!(ok, "{text}");
    assert!(
        text.contains("readonly = 1") && text.contains("lz4_frame"),
        "{text}"
    );
    assert!(text.contains("fetch: 1 row(s) arrived"), "{text}");
    // One whose profile makes Arrow output benchy cannot read fails where it would: the fetch,
    // naming the setting.
    let (ok, text) = check("ro_zstd");
    assert!(!ok, "{text}");
    assert_eq!(text.matches("  FAIL  ").count(), 1, "{text}");
    assert!(
        text.contains("FAIL  fetch") && text.contains("output_format_arrow_compression_method"),
        "{text}"
    );

    for (name, fix) in [
        ("nope", "Check `database`, `tables` or `query`"),
        ("locked", "CLICKHOUSE_PASSWORD"),
        ("away", "HTTP interface"),
    ] {
        let (ok, text) = check(name);
        assert!(!ok, "{name}: {text}");
        assert!(
            text.contains("FAIL") && text.contains(fix),
            "{name}: {text}"
        );
        // One fault is one failure: the steps that need the failed one say they were skipped.
        assert_eq!(text.matches("  FAIL  ").count(), 1, "{name}: {text}");
        assert!(text.contains("skip  fetch"), "{name}: {text}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each stops startup: a key of another kind of source, an `env` name it would hand to nothing,
/// `tables` with `query`, a URL with a password, a window without an offset, columns both pinned
/// and named.
#[test]
fn a_clickhouse_source_refuses_what_it_cannot_apply() {
    // Config only: no server needed. Each of these would otherwise do nothing, or do something
    // other than it says.
    let base = "[[source]]\ntype = \"clickhouse\"\nurl = \"http://127.0.0.1:8123\"\n";
    let parse = |text: &str| config::parse(text, Path::new("/x/benchy.toml"));
    for (extra, says) in [
        ("tables = [\"r\"]\ncommand = \"./c\"\n", "`command`"),
        ("tables = [\"r\"]\npath = \"r.csv\"\n", "`path`"),
        ("tables = [\"r\"]\nconfig = { k = \"v\" }\n", "`config`"),
        (
            "tables = [\"r\"]\nenv = { TOKEN = \"x\" }\n",
            "handed to nothing",
        ),
        (
            "tables = [\"r\"]\nquery = \"SELECT 1\"\n",
            "both `tables` and `query`",
        ),
        (
            "name = \"r\"\ntables = [\"r\"]\n",
            "both `name` and `tables`",
        ),
        ("name = \"r\"\n", "needs a `query`"),
        (
            "query = \"SELECT 1\"\n",
            "needs `tables`, or `query` with a `name`",
        ),
        ("tables = [\"bad name\"]\n", "not usable as a table name"),
        ("tables = [\"r\", \"r\"]\n", "both named `r`"),
        (
            "tables = [\"r\"]\n[source.narrow]\ncolumn = \"ts\"\nstart = \"2026-01-01\"\n",
            "offset",
        ),
        (
            "tables = [\"r\"]\n[source.narrow]\ncolumns = [\"a\"]\n\
             [[source.schema]]\nname = \"a\"\ntype = \"int64\"\n",
            "keep one of the two",
        ),
        ("tables = [\"r\"]\nunknown_key = 1\n", "unknown field"),
    ] {
        let e = parse(&format!("{base}{extra}"))
            .err()
            .unwrap_or_else(|| panic!("accepted: {extra}"));
        assert!(e.contains(says), "{extra}: {e}");
    }
    let e = parse(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"http://u:hunter2@h:8123\"\ntables = [\"r\"]\n",
    )
    .err()
    .unwrap();
    assert!(
        !e.contains("hunter2") && e.contains("CLICKHOUSE_PASSWORD"),
        "{e}"
    );
    // And a key only a clickhouse source has is refused on another.
    let e =
        parse("[[source]]\nname = \"a\"\ntype = \"file\"\npath = \"a.csv\"\nurl = \"http://h\"\n")
            .err()
            .unwrap();
    assert!(e.contains("`url`") && e.contains("clickhouse"), "{e}");
    // One source that lists two tables is two tables, with the settings they share.
    let s = parse(&format!(
        "{base}tables = [\"a\", \"b\"]\ndatabase = \"x\"\nmax_rows = 5\n"
    ))
    .unwrap();
    let names: Vec<&str> = s.builtin.iter().map(|b| b.name()).collect();
    assert_eq!(names, ["a", "b"]);
}

/// Loopback is `localhost`, 127.0.0.0/8 and ::1.
#[test]
fn an_unset_compression_follows_where_the_server_is() {
    // Config only. On this machine, compressing costs ClickHouse CPU and decoding costs benchy
    // CPU, and loopback has bandwidth to spare, so nothing is compressed; anywhere else, zstd.
    // What the operator sets is what is used, wherever the server is.
    use benchy_mcp::sources::{clickhouse::Compression, BuiltIn};
    let compression = |url: &str, extra: &str| {
        let s = sources(&format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{url}\"\ntables = [\"r\"]\n{extra}"
        ));
        let BuiltIn::Clickhouse(c) = &s.builtin[0] else {
            panic!("a clickhouse source");
        };
        c.compression
    };
    for url in [
        "http://127.0.0.1:8123",
        "http://127.8.9.10:8123/",
        "http://localhost:8123",
        "https://LOCALHOST:8443",
        "http://[::1]:8123",
    ] {
        assert_eq!(compression(url, ""), Compression::None, "{url}");
        assert_eq!(
            compression(url, "compression = \"zstd\"\n"),
            Compression::Zstd,
            "{url}"
        );
    }
    for url in [
        "http://10.0.0.5:8123",
        "http://192.168.1.2:8123",
        "https://clickhouse.example.com:8443",
        "http://localhost.example.com:8123",
        "http://[::2]:8123",
        "http://128.0.0.1:8123",
    ] {
        assert_eq!(compression(url, ""), Compression::Zstd, "{url}");
        assert_eq!(
            compression(url, "compression = \"none\"\n"),
            Compression::None,
            "{url}"
        );
    }
}

/// A stand-in for ClickHouse's HTTP interface that answers `DESCRIBE` with `columns`, the count
/// with one row, and the fetch with `arrow`, zstd-encoded when `zstd`: what a hostile or broken
/// server could send, which a real one will not. Returns its URL; it serves until the test ends.
fn fake_clickhouse(columns: &[(&str, &str)], arrow: Vec<u8>, zstd: bool) -> String {
    let describe: String = columns
        .iter()
        .map(|(n, t)| format!("{{\"name\":\"{n}\",\"type\":\"{t}\",\"default_type\":\"\"}}\n"))
        .collect();
    let arrow = if zstd {
        zstd::bulk::compress(&arrow, 1).unwrap()
    } else {
        arrow
    };
    fake_http(move |sql| {
        if sql.starts_with("DESCRIBE") {
            reply(200, "", describe.as_bytes())
        } else if sql.contains("count()") {
            reply(200, "", b"1\t1\n")
        } else if zstd {
            reply(200, "Content-Encoding: zstd\r\n", &arrow)
        } else {
            reply(200, "", &arrow)
        }
    })
}

/// An HTTP response: a status, headers each ending in CRLF, and a body.
fn reply(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// An HTTP server on a free local port that answers each POST with `answer(its body)`.
fn fake_http(answer: impl Fn(&str) -> Vec<u8> + Send + 'static) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut conn) = conn else { return };
            let mut r = BufReader::new(conn.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut sql = vec![0; len];
            let _ = r.read_exact(&mut sql);
            let _ = conn.write_all(&answer(&String::from_utf8_lossy(&sql)));
        }
    });
    url
}

/// One batch as an Arrow stream, finished.
fn arrow_stream(columns: Vec<(&str, arrow::array::ArrayRef)>) -> Vec<u8> {
    let batch = arrow::array::RecordBatch::try_from_iter(columns).unwrap();
    let mut buf = Vec::new();
    let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &batch.schema()).unwrap();
    w.write(&batch).unwrap();
    w.finish().unwrap();
    drop(w);
    buf
}

/// A server is trusted no more than a connector, and needs none to test: views that unpack past
/// the cap, a zstd body that decodes past it (decoded as it is read, so the read stops at the
/// cap), and an exception written after a whole stream.
#[test]
fn a_clickhouse_response_is_held_to_what_a_connectors_stream_is() {
    // No server needed: what is tested is what benchy does with a response no real ClickHouse
    // sends. The response goes through the reader a connector's stdout does, so it is held to
    // the same bounds — and a server is no more trusted with them than a connector is.
    use arrow::array::{ArrayRef, Int64Array, StringViewBuilder};
    use std::sync::Arc;
    let cfg = |url: &str, extra: &str| {
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{url}\"\ntables = [\"r\"]\n\
             max_bytes = 1048576\n{extra}"
        )
    };

    // Views: 2,000 views of one 64 KiB block, 114 KB on the wire and 131 MB unpacked.
    let mut views = StringViewBuilder::new();
    let block = views.append_block(arrow::buffer::Buffer::from_vec(vec![b'x'; 64 * 1024]));
    for _ in 0..2000 {
        views.try_append_view(block, 0, 64 * 1024).unwrap();
    }
    let body = arrow_stream(vec![("s", Arc::new(views.finish()) as ArrayRef)]);
    let url = fake_clickhouse(&[("s", "String")], body, false);
    let (r, _) = load(&cfg(&url, "compression = \"none\"\n"));
    let f = r.err().expect("views past the bound are refused");
    assert_eq!(f.code, "E_TOO_LARGE", "{}", f.detail);
    assert!(f.detail.contains("unpack"), "{}", f.detail);

    // zstd: 64 MiB of Arrow that compresses to almost nothing. Decoded as it is read, and the
    // read stops at the cap, so what is decoded is the cap, not the body.
    let zeros = arrow_stream(vec![(
        "n",
        Arc::new(Int64Array::from(vec![0i64; 8 * 1024 * 1024])) as ArrayRef,
    )]);
    let url = fake_clickhouse(&[("n", "Int64")], zeros, true);
    let started = Instant::now();
    let (r, _) = load(&cfg(&url, "compression = \"zstd\"\n"));
    let f = r
        .err()
        .expect("a body that decodes past the cap is refused");
    assert_eq!(f.code, "E_TOO_LARGE", "{}", f.detail);
    assert!(started.elapsed() < Duration::from_secs(20));

    // An exception after a whole stream: ClickHouse writes one into the body when a query fails
    // after the response has started. Read up to the first end-of-stream marker, the rows before
    // it loaded as if the query had succeeded.
    let mut body = arrow_stream(vec![(
        "n",
        Arc::new(Int64Array::from(vec![1i64, 2, 3])) as ArrayRef,
    )]);
    body.extend_from_slice(
        b"Code: 241. DB::Exception: Memory limit exceeded. (MEMORY_LIMIT_EXCEEDED)\n",
    );
    let url = fake_clickhouse(&[("n", "Int64")], body.clone(), false);
    let budget = Budget::new(1_000, 10 * 1024 * 1024);
    let (r, _) = load_within(&cfg(&url, "compression = \"none\"\n"), &budget);
    let f = r
        .err()
        .expect("an exception after the stream fails the load");
    assert_eq!(f.code, "E_UNAVAILABLE", "{}", f.detail);
    assert!(f.detail.contains("Code: 241"), "{}", f.detail);
    // And what was read was spent, though nothing loaded.
    let (rows, bytes) = budget.spent();
    assert_eq!(rows, 3);
    assert!(bytes > 0 && bytes <= body.len() as u64, "{bytes}");
}

/// ClickHouse's message — an error status with its exception code, or an exception after the
/// stream — names hosts, databases and users, so it is the operator's.
#[test]
fn a_clickhouse_error_reaches_the_operator_not_the_agent() {
    // ClickHouse's errors carry its own words: hosts, databases, users, query text. The code
    // decides what an operator is told the fault is; none of it, nor which source failed, may
    // reach the agent, which hears that a table is not available and nothing more. Two ways a
    // server fails: an error status with `X-ClickHouse-Exception-Code`, and an exception written
    // into the body after a whole stream.
    use arrow::array::{ArrayRef, Int64Array};
    use std::sync::Arc;
    let dir = unique("ch-err");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rooms.csv"), "room,floor\nlab,1\n").unwrap();
    let refused = fake_http(|_| {
        reply(
            404,
            "X-ClickHouse-Exception-Code: 60\r\n",
            b"Code: 60. DB::Exception: Table secret_db.payroll_2026 does not exist, \
              asked by user acct_7731. (UNKNOWN_TABLE)",
        )
    });
    let mut after = arrow_stream(vec![(
        "n",
        Arc::new(Int64Array::from(vec![1i64])) as ArrayRef,
    )]);
    after.extend_from_slice(
        b"Code: 241. DB::Exception: on replica ch-prod-eu-3.internal: memory limit. \
          (MEMORY_LIMIT_EXCEEDED)",
    );
    let late = fake_clickhouse(&[("n", "Int64")], after, false);
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"rooms\"\ntype = \"file\"\npath = \"rooms.csv\"\n\
             [[source]]\ntype = \"clickhouse\"\nurl = \"{refused}\"\ntables = [\"payroll\"]\n\
             [[source]]\ntype = \"clickhouse\"\nurl = \"{late}\"\ntables = [\"usage\"]\n\
             compression = \"none\"\n"
        ),
    )
    .unwrap();
    let log = dir.join("operator.log");
    let mut s = Server::launch_logging(
        &["--config", cfg.to_str().unwrap()],
        &[],
        std::fs::File::create(&log).unwrap(),
    );
    let v = s.call(
        "tools/call",
        serde_json::json!({"name": "describe", "arguments": {}}),
    );
    let mut seen = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(seen.contains("payroll") && seen.contains("usage"), "{seen}");
    for sql in ["SELECT * FROM payroll", "SELECT * FROM usage"] {
        seen.push_str(&s.query(sql).1);
    }
    for secret in [
        "secret_db",
        "acct_7731",
        "ch-prod-eu-3",
        "DB::Exception",
        "Code:",
        "127.0.0.1",
        "ClickHouse",
        "clickhouse",
        "E_CONFIG",
        "E_UNAVAILABLE",
    ] {
        assert!(
            !seen.contains(secret),
            "the agent was told `{secret}`: {seen}"
        );
    }
    drop(s);
    let logged = std::fs::read_to_string(&log).unwrap();
    for told in ["secret_db", "E_CONFIG", "ch-prod-eu-3", "E_UNAVAILABLE"] {
        assert!(
            logged.contains(told),
            "the operator was not told `{told}`: {logged}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_clickhouse_fetch_is_charged_when_served() {
    // `a_failed_fetch_still_spends_what_it_read`, for a built-in source in a server: the loader
    // reports what the fetch read whether or not the load succeeded, and the supervisor charges
    // the session before it reports the failure. `late` sends 2,000 rows and then an exception,
    // which fails it; `good` sends 2,000 rows. Under a budget of one stream and a half, `good`
    // is refused once `late` has been charged; with room for both, it loads.
    use arrow::array::{ArrayRef, Int64Array};
    use std::sync::Arc;
    let dir = unique("ch-charged");
    std::fs::create_dir_all(&dir).unwrap();
    let rows = arrow_stream(vec![(
        "n",
        Arc::new(Int64Array::from_iter_values(0..2000)) as ArrayRef,
    )]);
    let size = rows.len() as u64;
    let mut late = rows.clone();
    late.extend_from_slice(b"Code: 241. DB::Exception: memory limit. (MEMORY_LIMIT_EXCEEDED)");
    let late = fake_clickhouse(&[("n", "Int64")], late, false);
    let good = fake_clickhouse(&[("n", "Int64")], rows, false);
    let serve = |bytes: u64| {
        let cfg = dir.join("benchy.toml");
        std::fs::write(
            &cfg,
            format!(
                "[budget]\nbytes = {bytes}\n\
                 [[source]]\ntype = \"clickhouse\"\nurl = \"{late}\"\ntables = [\"late\"]\n\
                 compression = \"none\"\n\
                 [[source]]\ntype = \"clickhouse\"\nurl = \"{good}\"\ntables = [\"good\"]\n\
                 compression = \"none\"\n"
            ),
        )
        .unwrap();
        let out = std::process::Command::new(BIN)
            .args(common::PLATFORM)
            .args(["--config", cfg.to_str().unwrap()])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = serve(size + size / 2);
    assert!(
        err.contains("`late` was not loaded") && err.contains("`good` was not loaded"),
        "the second fetch went past what the failed first left: {err}"
    );
    let err = serve(3 * size);
    assert!(err.contains("good (2000 rows)"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
