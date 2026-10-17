//! REQ-15: origin-blind. One mapping, applied to every source as it is loaded, so the same data
//! makes the same table whatever it came from — a CSV, a Parquet file, a host's Arrow stream or a
//! connector's.
//!
//! The mapping is from the column type a reader produced to the type the bench holds:
//!
//! | produced | held |
//! |---|---|
//! | every signed integer, and unsigned up to 32 bits | `BIGINT` |
//! | `UBIGINT` | `HUGEINT` — lossless, and signed, so `a - b` on two counters does not fail |
//! | `FLOAT`, `DOUBLE` | `DOUBLE` |
//! | `TIMESTAMP` at any precision, zone-less | `TIMESTAMP WITH TIME ZONE`, read in the source's zone |
//! | `TIMESTAMP WITH TIME ZONE` | itself: an instant, shown in UTC |
//! | anything else | itself |
//!
//! Only top-level columns are mapped; a list or a struct keeps what is inside it.
//!
//! The conversions are written in SQL and run inside the loader, by DuckDB, for every source:
//! one implementation, so a file and a stream cannot drift apart. Each was chosen by measurement
//! on the 840,960-row eval CSV: `timezone('UTC', ts)` cost about 0.2 s of CPU, a third of the
//! whole load, where `make_timestamptz(epoch_us(ts))` — the same instant, without ICU's calendar
//! — cost 0.01 s. That one gives NULL for `infinity` and `-infinity`, so ICU's cast is kept as
//! the fallback for the rows it returns nothing for: 47 M instructions where it cost 29 M, where
//! `timezone` costs 1,269 M (docs/PERFORMANCE.md, "Infinite timestamps kept on load";
//! REQ-15 `an_infinite_timestamp_is_kept_from_every_source`).

/// The zone a zone-less timestamp is read in when its source names none.
pub const UTC: &str = "UTC";

/// The type the bench holds for a column a reader produced as `ty` (DuckDB's spelling).
pub fn held(ty: &str) -> String {
    match ty {
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "UTINYINT" | "USMALLINT" | "UINTEGER" => {
            "BIGINT".into()
        }
        "UBIGINT" => "HUGEINT".into(),
        "FLOAT" => "DOUBLE".into(),
        "TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS" => {
            "TIMESTAMP WITH TIME ZONE".into()
        }
        other => other.into(),
    }
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The SQL that turns column `ident` (already quoted), produced as `from`, into `to`, reading a
/// zone-less timestamp in `zone`; `None` when it already is `to`.
///
/// Sub-microsecond digits are refused rather than dropped: `TIMESTAMP WITH TIME ZONE` holds
/// microseconds, and a nanosecond timestamp that carries more would be silently rounded. The
/// refusal names the column; a source whose timestamps are whole microseconds, as nearly all
/// are, loads.
pub fn convert(ident: &str, name: &str, from: &str, to: &str, zone: &str) -> Option<String> {
    if from == to {
        return None;
    }
    let instant = |naive: &str| {
        if zone == UTC {
            // The same instant as `timezone('UTC', …)`, without the calendar: see the module.
            // `epoch_us` of `infinity` or `-infinity` is NULL, which made either a null; ICU's
            // cast keeps them, and is paid only for the rows `epoch_us` gives nothing for.
            format!("coalesce(make_timestamptz(epoch_us({naive})), CAST({naive} AS TIMESTAMPTZ))")
        } else {
            format!("timezone({}, {naive})", literal(zone))
        }
    };
    Some(match (from, to) {
        // `epoch_ns` of an infinity is the sentinel itself, `i64::MAX` nanoseconds, whose last
        // digits are not zero: it was refused as finer than a microsecond.
        ("TIMESTAMP_NS", "TIMESTAMP WITH TIME ZONE") => format!(
            "CASE WHEN isfinite({ident}) AND epoch_ns({ident}) % 1000 <> 0 THEN error({}) \
             ELSE {} END",
            literal(&sub_microsecond(name)),
            instant(&format!("CAST({ident} AS TIMESTAMP)"))
        ),
        ("TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS", "TIMESTAMP WITH TIME ZONE") => {
            instant(&format!("CAST({ident} AS TIMESTAMP)"))
        }
        _ => format!("CAST({ident} AS {to})"),
    })
}

/// Why a nanosecond timestamp was refused.
pub fn sub_microsecond(column: &str) -> String {
    format!(
        "column `{column}` has timestamps finer than a microsecond, which the bench cannot hold \
         without rounding them; truncate them to microseconds at the source"
    )
}

/// Whether `zone` is a time zone DuckDB knows. Asked of DuckDB, in memory, so a typo stops
/// startup instead of failing every load.
pub fn known_zone(zone: &str) -> bool {
    let Ok(c) = duckdb::Connection::open_in_memory() else {
        return false;
    };
    c.query_row(
        "SELECT timezone(?, TIMESTAMP '2000-01-01') IS NOT NULL",
        [zone],
        |r| r.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zone_less_timestamp_is_read_in_its_sources_zone() {
        let c = duckdb::Connection::open_in_memory().unwrap();
        c.execute_batch("SET TimeZone = 'UTC'").unwrap();
        let at = |zone: &str| -> String {
            let e = convert("ts", "ts", "TIMESTAMP", "TIMESTAMP WITH TIME ZONE", zone).unwrap();
            c.query_row(
                &format!(
                    "SELECT ({e})::VARCHAR FROM \
                     (SELECT TIMESTAMP '2026-07-01 12:00:00' AS ts)"
                ),
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(at(UTC), "2026-07-01 12:00:00+00");
        assert_eq!(at("Europe/Copenhagen"), "2026-07-01 10:00:00+00");
        assert!(known_zone("Europe/Copenhagen"));
        assert!(!known_zone("Europe/Atlantis"));
    }
}
