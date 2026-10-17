//! REQ-4: putting data on the bench.
//!
//! Loading happens **before** the engine is sealed — that is the whole reason [`Bench`] and
//! [`Workspace`] are different types. Once sealed the engine reads nothing from disk, so there
//! is no order in which a load could follow a seal.
//!
//! Two tiers: files (CSV, TSV, Parquet, JSON and NDJSON, each named after its file stem) and exec
//! connectors ([`crate::exec`]).

use crate::workspace::{Bench, LoadedTable};
use std::path::{Path, PathBuf};

/// A table name the agent will type. Derived from a file stem, and constrained so it is a plain
/// SQL identifier: an operator should never have to quote what they read in a listing.
pub fn table_name(stem: &str) -> Option<String> {
    let mut out = String::new();
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase())
        } else if matches!(ch, '_' | '-' | ' ' | '.') {
            out.push('_')
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        return None;
    }
    Some(if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
        format!("t_{trimmed}")
    } else {
        trimmed
    })
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[derive(Debug)]
pub enum LoadError {
    Unreadable {
        path: PathBuf,
        why: String,
    },
    Unsupported {
        path: PathBuf,
    },
    BadName {
        path: PathBuf,
    },
    Duplicate {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    Empty {
        dir: PathBuf,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Unreadable { path, why } => {
                write!(f, "could not read {}: {why}", path.display())
            }
            LoadError::Unsupported { path } => write!(
                f,
                "{}: benchy reads {FORMATS}, told apart by the extension",
                path.display()
            ),
            LoadError::BadName { path } => write!(
                f,
                "{}: the file name yields no usable table name",
                path.display()
            ),
            LoadError::Duplicate {
                name,
                first,
                second,
            } => write!(
                f,
                "two files would both be called `{name}`: {} and {}. Rename one, or name them \
                    explicitly with --source",
                first.display(),
                second.display()
            ),
            LoadError::Empty { dir } => write!(
                f,
                "no readable data files in {}. benchy reads {FORMATS} — point it at a \
                    directory that has some, or name one file with --source name=path",
                dir.display()
            ),
        }
    }
}

/// Every extension `reader_for` knows, for the messages that list them.
const FORMATS: &str = ".csv, .tsv, .parquet, .json, .ndjson and .jsonl";

/// The reader a file of `path`'s kind is read with. An HTTP body DuckDB reads itself goes through
/// the same one, under a name with the same extension, so it is sniffed as the file would be
/// (REQ-17 `every_http_format_loads_as_the_same_table_as_its_file`).
pub(crate) fn reader_for(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let p = literal(&path.to_string_lossy());
    match ext.as_str() {
        "csv" => Some(format!("read_csv_auto({p})")),
        "tsv" => Some(format!("read_csv_auto({p}, delim = '\\t')")),
        "parquet" => Some(format!("read_parquet({p})")),
        // DuckDB infers the shape either way; `format` only disambiguates a file that could be
        // read as both a single array and one object per line.
        "json" => Some(format!("read_json_auto({p})")),
        "ndjson" | "jsonl" => Some(format!("read_json_auto({p}, format = 'newline_delimited')")),
        _ => None,
    }
}

/// The reader for a file of `path`'s kind, declared as `schema` with `delimiter`, or as
/// [`reader_for`] when nothing is declared: a file source's (R3-5) and an HTTP body's, which
/// is spooled under a name with the file's extension (R2-3). `None` for a kind benchy does not
/// read.
///
/// Declared, a CSV or TSV states its dialect — `delimiter` (`,`, or a tab for a TSV), and RFC
/// 4180's double quote as quote and escape — so DuckDB's sniffer reads only the header, by which
/// the declaration is matched, and the formats of dates and times. Measured with DuckDB 1.5.5 on
/// the harness's 1,000-row CSV (R2-3): 66 M instructions to read it, against 462 M with the
/// sniffer finding the dialect, `types` or not; skipping the sniffer whole (`auto_detect =
/// false`) took 13 M, but reads columns by position, so a file whose columns come in another
/// order loaded one column's values into another without a word, and loses the sniffer's date
/// formats. A file in another dialect fails loudly
/// (`a_declared_file_that_does_not_match_fails_naming_why`). An NDJSON or JSON file names its
/// `columns`, which turns the sniffer off; a Parquet file has its own types, which the
/// declaration pins ([`Hold`]).
pub(crate) fn reader_declared(
    path: &Path,
    schema: &[crate::exec::FieldDecl],
    delimiter: Option<char>,
) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let lit = literal(&path.to_string_lossy());
    let delim = |d: char| literal(&d.to_string());
    if schema.is_empty() {
        return match (ext.as_str(), delimiter) {
            ("csv" | "tsv", Some(d)) => Some(format!("read_csv_auto({lit}, delim = {})", delim(d))),
            _ => reader_for(path),
        };
    }
    let cols = schema
        .iter()
        .map(|d| {
            format!(
                "{}: {}",
                literal(&d.name),
                literal(&duckdb_type(&d.ty).expect("checked at startup"))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    match ext.as_str() {
        "csv" | "tsv" => Some(format!(
            "read_csv_auto({lit}, types = {{{cols}}}, delim = {}, quote = '\"', escape = '\"')",
            delim(delimiter.unwrap_or(if ext == "tsv" { '\t' } else { ',' }))
        )),
        "ndjson" | "jsonl" => Some(format!(
            "read_json_auto({lit}, format = 'newline_delimited', columns = {{{cols}}})"
        )),
        "json" => Some(format!("read_json_auto({lit}, columns = {{{cols}}})")),
        _ => reader_for(path),
    }
}

/// What the operator declared of a file source (R3-5): its columns, and a CSV's or TSV's
/// delimiter. Nothing declared, the file is sniffed.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Declared {
    pub schema: Vec<crate::exec::FieldDecl>,
    pub delimiter: Option<char>,
}

/// Put one file on the bench under `name`, reading a zone-less timestamp as UTC.
pub fn load_file(bench: &mut Bench, name: &str, path: &Path) -> Result<LoadedTable, LoadError> {
    load_file_in(bench, name, path, crate::canonical::UTC)
}

/// Put one file on the bench under `name`, in the bench's own types (REQ-15), reading a zone-less
/// timestamp in `zone`.
///
/// The table is written once, converted, by [`write_table`], on one thread, laid out as a
/// stream's [`Ingest`] lays it out. A Parquet file is read straight into that write
/// ([`read_into`]); a CSV or JSON file is read with all of DuckDB's threads into a staging table
/// first, in memory up to [`STAGE_MEMORY`], which is sooner than parsing it on one
/// ([`reads_twice`] has the numbers). Converting
/// in place with `ALTER` was cheaper still, and left the replaced column's blocks free in the
/// file, so the file's size and blocks told a CSV from a Parquet file
/// (`the_physical_layout_says_nothing_of_where_data_came_from`).
pub fn load_file_in(
    bench: &mut Bench,
    name: &str,
    path: &Path,
    zone: &str,
) -> Result<LoadedTable, LoadError> {
    load_file_with(bench, name, path, zone, &[], None)
}

/// R3-5: [`load_file_in`], the file declared by its operator as `schema` with `delimiter`
/// ([`reader_declared`]): held to the declaration as an HTTP body is — each declared column there
/// and of its type, only they loaded, a `nullable = false` one without a null — and otherwise
/// the same table.
pub fn load_file_declared(
    bench: &mut Bench,
    name: &str,
    path: &Path,
    zone: &str,
    schema: &[crate::exec::FieldDecl],
    delimiter: Option<char>,
) -> Result<LoadedTable, LoadError> {
    load_file_with(bench, name, path, zone, schema, delimiter)
}

/// [`load_file_in`] and [`load_file_declared`]: the file, declared as `schema` where that is not
/// empty.
fn load_file_with(
    bench: &mut Bench,
    name: &str,
    path: &Path,
    zone: &str,
    schema: &[crate::exec::FieldDecl],
    delimiter: Option<char>,
) -> Result<LoadedTable, LoadError> {
    let reader = reader_declared(path, schema, delimiter)
        .ok_or_else(|| LoadError::Unsupported { path: path.into() })?;
    let narrowing = crate::exec::Narrowing::default();
    let hold = (!schema.is_empty()).then_some(Hold {
        declared: schema,
        narrowing: &narrowing,
        // A file has no row cap; read straight in, one past it is still a `LIMIT` DuckDB takes.
        max_rows: i64::MAX as u64 - 1,
    });
    // The likeliest reason a declared CSV's header does not match: it was split on the wrong
    // character.
    let dialect = |why: String| {
        let csv = reader.starts_with("read_csv") && !schema.is_empty();
        if csv && (why.contains("COLUMN_TYPES") || why.contains("Conversion Error")) {
            let d = delimiter.unwrap_or(if reader.contains("delim = '\t'") {
                '\t'
            } else {
                ','
            });
            format!("{why} (its header is read with `delimiter = {:?}`: set it if the file uses another)", d.to_string())
        } else {
            why
        }
    };
    let unreadable = |e: duckdb::Error| LoadError::Unreadable {
        path: path.into(),
        why: e.to_string().lines().next().unwrap_or("").to_string(),
    };
    let conn = bench.connection();
    let bytes = std::fs::metadata(path).map_or(u64::MAX, |m| m.len());
    // A declared file names its `columns` already, so it is never read as R3-1's text.
    let written = write_stamps_as_text(conn, name, &reader, zone, hold.as_ref(), bytes, Some(path));
    let (_, _, beside) = written.map_err(|e| match e {
        Staged::Reader(e) => match unreadable(e) {
            LoadError::Unreadable { path, why } => LoadError::Unreadable {
                path,
                why: dialect(why),
            },
            other => other,
        },
        Staged::Refused(f) => LoadError::Unreadable {
            path: path.into(),
            why: f.detail,
        },
    })?;

    let rows: u64 = conn
        .query_row(&format!("SELECT count(*) FROM {}", quote(name)), [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as u64)
        .unwrap_or(0);
    let columns = columns_of(conn, name).map_err(unreadable)?;
    let seen = kept_or_watched(conn, name, &columns, beside);
    let t = LoadedTable {
        name: name.to_string(),
        rows,
        columns,
        source: path.display().to_string(),
        profile: None,
        seen: None,
    };
    let mut kept = t.clone();
    kept.seen = seen;
    bench.record(kept).map_err(|why| LoadError::Unreadable {
        path: path.into(),
        why,
    })?;
    Ok(t)
}

/// [`stage_and_write`], an NDJSON file's or body's timestamps read as text where
/// [`text_timestamps`] can (R3-1), and the sniffed way where it cannot or where the text read
/// fails: that read's table, or its refusal, in its words. Neither way leaves a table behind.
/// A file of fewer than [`TEXT_STAMPS_FROM`] `bytes` is read the sniffed way.
fn write_stamps_as_text(
    conn: &duckdb::Connection,
    name: &str,
    reader: &str,
    zone: &str,
    hold: Option<&Hold>,
    bytes: u64,
    head: Option<&Path>,
) -> Result<Written, Staged> {
    let text = (bytes >= TEXT_STAMPS_FROM)
        .then(|| text_timestamps(conn, reader))
        .flatten();
    let Some(text) = text else {
        return stage_and_write(conn, name, reader, zone, hold, head);
    };
    #[cfg(test)]
    TEXT_STAMPS.with(|u| u.set((u.get().0 + 1, u.get().1)));
    stage_and_write(conn, name, &text, zone, hold, head).or_else(|_| {
        #[cfg(test)]
        TEXT_STAMPS.with(|u| u.set((u.get().0, u.get().1 + 1)));
        crate::trace::span("file.reread", || {
            stage_and_write(conn, name, reader, zone, hold, head)
        })
    })
}

/// The smallest NDJSON file or body whose timestamps are read as text. The `DESCRIBE` that
/// reading them so needs first costs about what the read's own sniff did, plus a second plan and
/// open of the file: on the harness's 1,000-row NDJSON (63 KB) the session took 26 M more
/// instructions (+5%), where at 20,000 rows (1.3 MB) DuckDB alone read it in 15 M fewer.
const TEXT_STAMPS_FROM: u64 = 1 << 20;

/// R3-1: an NDJSON file's reader with the columns its sniffer finds, each top-level `TIMESTAMP`
/// among them read as text and cast; `None` where that would not read the file as the sniffed
/// read does, which then reads it.
///
/// DuckDB's JSON reader parses a timestamp with the `strptime` format its sniffer chose, tried
/// per vector among twelve: 28% of an NDJSON file's read. Measured with DuckDB 1.5.5 alone, one
/// thread, on the harness's 1,000,000 rows (`bench/gen_data.py`, `ts` as
/// `2026-01-01T00:00:00.000000Z`), less an empty connection: 3.94 G instructions read as now;
/// 3.06 G sniffed once (`DESCRIBE`, 0.16 G) and read with those `columns`, `ts` as text, and
/// cast as here; at 20,000 rows 237 M against 222 M. A per-row check of the text's shape cost
/// what the cast saved (a regular expression +1.0 G, `LIKE` +0.8 G), so the cast is held to what
/// is cheap to check, and a file it does not take is read the sniffed way:
///
/// - **The cast is to `TIMESTAMP_NS`**, which applies an offset as the sniffer's `%z` does (a
///   cast to `TIMESTAMP` drops it: `+02:00` read two hours late), keeps the digits past a
///   microsecond so they can be refused rather than cut (REQ-15; the sniffer refuses them),
///   and fails outside 1677–2262, where a two-digit year the sniffer reads as 2026 (`%y`) would
///   have been the year 26.
/// - **19 bytes or more**, the shortest date and time: `epoch`, which the sniffer's formats
///   read as 1900-01-01 and the cast as 1970, a date alone, and a number all fail it.
/// - **Whole microseconds**: `epoch_ns % 1000 = 0`, which an infinity fails too.
///
/// Differentially checked against the sniffed read on 951 files of generated shapes, each odd
/// value both at a vector's first row and inside one: no value read differently. What the cast
/// reads that the sniffer's formats do not — `24:00:00`, a field without its zero, an offset
/// where the vector's first value had none — loads at the time its text says, where the sniffed
/// read refused the file.
///
/// `columns` turns the sniffer off for the read, and with it three things besides the
/// timestamps' formats, so a file whose columns would meet one is left to the sniffed read:
/// a `DATE` is cast rather than read by format (`01-02-2026`); a key a struct did not have in
/// the sample is dropped rather than refused; and of two keys that differ only in case, the
/// second, which the sniffer renames `Id_1`, is looked for by that name.
fn text_timestamps(conn: &duckdb::Connection, reader: &str) -> Option<String> {
    const NDJSON: &str = ", format = 'newline_delimited')";
    let file = reader
        .strip_prefix("read_json_auto(")?
        .strip_suffix(NDJSON)?;
    let sniffed: Vec<(String, String)> = crate::trace::span("file.describe", || {
        let mut st = conn.prepare(&format!("DESCRIBE SELECT * FROM {reader}"))?;
        let cols = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        cols.collect::<Result<_, duckdb::Error>>()
    })
    .ok()?;
    let renamed = |c: &str| {
        c.rsplit_once('_').is_some_and(|(base, n)| {
            !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
                && sniffed.iter().any(|(o, _)| o.eq_ignore_ascii_case(base))
        })
    };
    if !sniffed.iter().any(|(_, t)| t == "TIMESTAMP")
        || sniffed.iter().any(|(c, t)| {
            (t != "TIMESTAMP" && t.contains("TIMESTAMP"))
                || t.contains("DATE")
                || t.contains("STRUCT")
                || renamed(c)
        })
    {
        return None;
    }
    let columns = sniffed
        .iter()
        .map(|(c, t)| {
            let t = if t == "TIMESTAMP" { "VARCHAR" } else { t };
            format!("{}: {}", literal(c), literal(t))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let fallback = literal("benchy: read the sniffed way");
    let each = |when: &dyn Fn(&str) -> String| {
        sniffed
            .iter()
            .filter(|(_, t)| t == "TIMESTAMP")
            .map(|(c, _)| {
                let c = quote(c);
                let when = when(&c);
                format!("CASE WHEN {c} IS NULL THEN NULL {when} ELSE error({fallback}) END AS {c}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let cast = each(&|c| format!("WHEN strlen({c}) >= 19 THEN CAST({c} AS TIMESTAMP_NS)"));
    let held = each(&|c| format!("WHEN epoch_ns({c}) % 1000 = 0 THEN CAST({c} AS TIMESTAMP)"));
    let text = format!("read_json({file}, format = 'newline_delimited', columns = {{{columns}}})");
    Some(format!(
        "(SELECT * REPLACE ({held}) FROM (SELECT * REPLACE ({cast}) FROM {text}))"
    ))
}

#[cfg(test)]
thread_local! {
    /// How many files on this thread had their timestamps read as text, and how many of those
    /// were then read the sniffed way.
    pub(crate) static TEXT_STAMPS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// The in-memory database a file is staged in before its table is written; held to
/// [`STAGE_MEMORY`], past which it spills to the staging directory.
const STAGE: &str = "benchy_stage";

/// What the rows of a body DuckDB reads itself are held to on the way to the bench, as a stream's
/// are (REQ-17): the operator's declaration, the narrowing, and the row cap.
pub struct Hold<'a> {
    /// Pinned columns: each must be there and load as the type pinned, and only they are
    /// loaded. Empty: every column, as the reader typed it.
    pub declared: &'a [crate::exec::FieldDecl],
    pub narrowing: &'a crate::exec::Narrowing,
    pub max_rows: u64,
}

/// Why staging failed: the reader could not read it, or what it read was refused.
enum Staged {
    Reader(duckdb::Error),
    Refused(crate::exec::Fault),
}

impl From<duckdb::Error> for Staged {
    fn from(e: duckdb::Error) -> Self {
        Staged::Reader(e)
    }
}

/// Write table `name` from what `reader` reads, converted to the bench's types with zone-less
/// timestamps read in `zone`, straight from the reader or through a stage ([`reads_twice`]): the
/// one way a file's rows reach the bench, and the rows of an HTTP body DuckDB reads itself, so
/// the two make the same table (REQ-15). Held to
/// `hold` when there is one. Returns the rows the reader produced, the columns kept with the
/// types the reader gave them, and, from a stage, what the watch beside the write saw ([`Beside`]).
///
/// `head` is the file the reader reads. Where its first bytes show rows too wide for a stage on
/// every thread ([`Width::too_wide_for`]), or a stage runs out of memory, it is read on one
/// thread instead ([`on_one_thread`]); `None`, it is staged.
fn stage_and_write(
    conn: &duckdb::Connection,
    name: &str,
    reader: &str,
    zone: &str,
    hold: Option<&Hold>,
    head: Option<&Path>,
) -> Result<Written, Staged> {
    if reads_twice(reader, hold) {
        return read_into(conn, name, reader, zone, hold);
    }
    let threads: i64 = conn.query_row("SELECT current_setting('threads')", [], |r| r.get(0))?;
    // A Parquet file's first bytes are no lines; a narrowed one, staged, is judged by DuckDB.
    let width = head
        .filter(|_| !reader.starts_with("read_parquet("))
        .and_then(Width::of);
    let wide = width.is_some_and(|w| w.too_wide_for(threads));
    // On one thread the JSON reader's own vector counts against the cap too; an NDJSON file
    // whose timestamps are read as text is read by `read_json` inside a `SELECT`.
    let one = match width {
        Some(w) if reader.contains("read_json") => {
            format!("{}MiB", (STAGE_BYTES + w.json_vector()).div_ceil(1 << 20))
        }
        _ => STAGE_MEMORY.to_string(),
    };
    staged_within(conn, name, reader, zone, hold, wide, STAGE_MEMORY, &one)
}

/// [`stage_and_write`] past [`reads_twice`]: staged on every thread, DuckDB held to `cap`, or,
/// where the rows are `wide` or the stage runs DuckDB out of memory, read on one, held to `one`
/// ([`on_one_thread`]).
#[allow(clippy::too_many_arguments)]
fn staged_within(
    conn: &duckdb::Connection,
    name: &str,
    reader: &str,
    zone: &str,
    hold: Option<&Hold>,
    wide: bool,
    cap: &str,
    one: &str,
) -> Result<Written, Staged> {
    if !wide {
        #[cfg(test)]
        TRIED.with(|t| t.borrow_mut().push("staged".into()));
        let written = within(conn, Some(cap), || {
            staged(conn, name, reader, zone, hold, None)
        });
        let Some(why) = out_of_memory(&written) else {
            return written;
        };
        eprintln!(
            "benchy: table `{name}` could not be staged on every thread in {cap} of memory, and \
             was read on one: {why}"
        );
    }
    on_one_thread(conn, name, reader, zone, hold, one, cap)
}

/// The first line of DuckDB's error, where `written` ran it out of memory.
fn out_of_memory(written: &Result<Written, Staged>) -> Option<String> {
    match written {
        Err(Staged::Reader(e)) if e.to_string().contains("Out of Memory Error") => {
            e.to_string().lines().next().map(str::to_string)
        }
        _ => None,
    }
}

/// Run `fill` with DuckDB held to `cap`, if there is one, and its memory limit put back as it
/// was after.
fn within<T>(
    conn: &duckdb::Connection,
    cap: Option<&str>,
    fill: impl FnOnce() -> Result<T, Staged>,
) -> Result<T, Staged> {
    let Some(cap) = cap else {
        return fill();
    };
    let was: String = conn.query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))?;
    conn.execute_batch(&format!("SET memory_limit = '{cap}'"))?;
    let filled = fill();
    put_back_memory_limit(conn, &was)?;
    filled
}

/// [`stage_and_write`] for rows too wide to stage on every thread: on one thread, under `cap`
/// ([`STAGE_MEMORY`], and a JSON file's vector of lines, [`Width::json_vector`]), a CSV straight
/// into the table ([`read_into`]), a JSON file, or a narrowed body, whose rows only a stage counts
/// before the narrowing drops any, through a stage filled on one thread. Where that runs out of
/// memory too, rows the first line did not show, the table is read on one thread without the
/// cap, as every stage that ran out was before, bounded by the loader's memory ceiling alone;
/// the operator is told. The table is the same file every way
/// (`a_table_read_on_one_thread_or_staged_is_the_same_file`).
fn on_one_thread(
    conn: &duckdb::Connection,
    name: &str,
    reader: &str,
    zone: &str,
    hold: Option<&Hold>,
    cap: &str,
    write: &str,
) -> Result<Written, Staged> {
    let straight = reader.starts_with("read_csv")
        && hold.is_none_or(|h| keep_predicate(h.narrowing) == "true");
    let read = |cap: Option<&str>| {
        #[cfg(test)]
        TRIED.with(|t| {
            let how = if straight {
                "straight in"
            } else {
                "staged on one"
            };
            let cap = if cap.is_some() { "" } else { ", uncapped" };
            t.borrow_mut().push(format!("{how}{cap}"))
        });
        within(conn, cap, || {
            let filled = if straight {
                // Watched here, under the cap, rather than after it is put back: scanned
                // without it, the table's blocks stayed in memory as they were read, and the
                // loader grew with the file.
                let (rows, kept, _) = read_into(conn, name, reader, zone, hold)?;
                let columns = columns_of(conn, name)?;
                let seen = crate::trace::span("file.watch", || {
                    watch_scan(conn, &quote(name), &columns, None)
                });
                (rows, kept, seen.map(|seen| (columns, seen)))
            } else {
                let threads: i64 =
                    conn.query_row("SELECT current_setting('threads')", [], |r| r.get(0))?;
                conn.execute_batch("SET threads = 1")?;
                // Written under `write`, not `cap`: the JSON reader's vector is gone by then,
                // and the write of a wide `MAP` given its room grew with the rows outside
                // DuckDB's own count. Measured on DuckDB 1.5.5 alone, 10,000 objects of 2,000
                // keys staged within 424 MiB: written under it, the process peaked at 746 MB;
                // under 256 MiB, at 558 MB, and at 568 MB for 20,000.
                let write = cap.is_some().then_some(write);
                let staged = staged(conn, name, reader, zone, hold, write);
                conn.execute_batch(&format!("SET threads = {threads}"))?;
                staged?
            };
            // The table checkpointed here, under the cap, rather than as the bench closes,
            // under none: there, the 20,000 objects' checkpoint took the process from 573 to
            // 787 MB; under 256 MiB it stayed at 603 MB. The same file either way
            // (`a_table_read_on_one_thread_or_staged_is_the_same_file`).
            if cap.is_some() {
                conn.execute_batch("CHECKPOINT")?;
            }
            Ok(filled)
        })
    };
    let written = read(Some(cap));
    let Some(why) = out_of_memory(&written) else {
        return written;
    };
    eprintln!(
        "benchy: table `{name}` could not be read in {cap} of memory even on one thread, its \
         rows too wide, and was read without that cap: {why}"
    );
    read(None)
}

#[cfg(test)]
thread_local! {
    /// How each read [`staged_within`] made on this thread went.
    pub(crate) static TRIED: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The most memory DuckDB may hold while a table is staged and written. Past it the stage's
/// blocks spill to the bench's temporary directory, `bench.duckdb.tmp` in the staging directory,
/// are read back for the write, and are gone once the stage is detached. Held whole, the stage
/// grew the loader with the file: the harness's 10,000,000-row CSV (475 MB) peaked at 513 MB.
/// Measured on DuckDB 1.5.5 with four threads, interleaved with the uncapped stage
/// (docs/PERFORMANCE.md, "Memory and disk that grow with the input"): at 256 MiB that load
/// peaks at 406 MB, spilling about 95 MB, for 4.5% more instructions and 5% more CPU; at
/// 128 MiB at 300 MB, for 24% more instructions and 33% more CPU, the buffer manager evicting
/// under four readers. A stage that fits, csv-1m's or ndjson-1m's, never spills, and costs what
/// it did. Past the cap, the loader's peak stays where it is as the
/// file grows (`a_large_csv_is_not_held_in_memory_while_it_is_staged`).
const STAGE_MEMORY: &str = "256MiB";

/// [`STAGE_MEMORY`] in bytes.
const STAGE_BYTES: u64 = 256 << 20;

/// What the threads filling a stage may hold between them that DuckDB cannot spill, at most,
/// judged from a file's first line ([`Width::too_wide_for`]): half of [`STAGE_MEMORY`], the rest
/// left to spill through. Past it the file is read on one thread ([`on_one_thread`]).
///
/// Measured on DuckDB 1.5.5 alone, the stage under the cap on four threads, a CSV of 300 `DOUBLE`
/// columns (827 MB) ran out of memory with 211 MB of table and 30 MB of CSV buffers held, and was
/// staged again without the cap, held whole (2,117–2,281 MB in benchy). Each thread appending to
/// a stage pins a block of every column it appends to (256 KiB, DuckDB's block size), whatever
/// the rows in it, and keeping the file's order merges the threads' collections, which pins a
/// block of each column of two at once: 200 columns ran out on two threads, 100 did not on four,
/// and the first 52 MB of the 300-column file ran out on four. The JSON reader parses a vector of
/// 2,048 rows at once: an NDJSON of 2,000 keys an object (163 MB, read as one `MAP`) took 448 MB
/// of allocations on four threads, 192 MB on one, three times a vector of its lines.
///
/// On one thread a CSV goes straight into its table, under the cap: the 300-column file in 20 s
/// within 373 MB, where staged on one thread it took 17 s to stage and its write from the stage
/// needed 512–640 MiB, the table's row group (122,880 rows, 295 MB) and the stage's blocks in one
/// pool, and 19 s more. A JSON file is staged on one thread: read straight in, its reader is
/// asked for its types first, and the 2,000-key file's sniff took 8 s each time. Spilling, one
/// thread is no slower than four, which merge what they spilled: a 413 MB CSV of 100 columns
/// staged in 7.4 s on one, 9.9 s on four, 30 columns in 5.3 s against 7.3 s. Narrower rows are
/// left on every thread: the harness's 10,000,000 rows of 3 columns stage in 1.4 s on four, 4.5 s
/// on one (`a_wide_csv_is_not_held_in_memory_while_it_is_staged`, docs/PERFORMANCE.md R6-W).
const WIDE_PER_THREAD: u64 = STAGE_BYTES / 2;

/// How much of a file [`Width::of`] reads to judge its rows: two lines of the NDJSON above.
const HEAD_BYTES: u64 = 64 << 10;

/// The rows of a file as its first 64 KiB show them, judged from the file's own bytes rather
/// than by DuckDB's sniffer, which reads it again: a `DESCRIBE` of a 1,000-row CSV costs 462 M
/// instructions (R2-3).
#[derive(Debug, Clone, Copy)]
struct Width {
    /// Columns in the first line: a CSV's, or a JSON object's keys, counted by their separators.
    columns: u64,
    /// The mean length of the lines in the first 64 KiB.
    line: u64,
}

impl Width {
    /// The width of the file at `path`; `None` for one it cannot read, left to DuckDB to say
    /// why.
    fn of(path: &Path) -> Option<Width> {
        use std::io::Read;
        let mut head = Vec::new();
        std::fs::File::open(path)
            .and_then(|f| f.take(HEAD_BYTES).read_to_end(&mut head))
            .ok()?;
        if head.is_empty() {
            return None;
        }
        let first = head.split(|b| *b == b'\n').next().unwrap_or(&head);
        let columns = b",\t;|"
            .iter()
            .map(|d| first.iter().filter(|b| *b == d).count())
            .max()
            .unwrap_or(0) as u64
            + 1;
        let lines = head.iter().filter(|b| **b == b'\n').count().max(1) as u64;
        Some(Width {
            columns,
            line: head.len() as u64 / lines,
        })
    }

    /// Whether `threads` filling a stage of these rows would hold more than [`WIDE_PER_THREAD`]
    /// between them: each two blocks of 256 KiB for each column, and three times a vector of
    /// 2,048 lines. Where it judges wrong, the stage runs out of memory and the file is read on
    /// one thread then ([`staged_within`]).
    fn too_wide_for(self, threads: i64) -> bool {
        let each = self.columns * 2 * (256 << 10) + self.line * 2048 * 3;
        each.saturating_mul(threads.max(1) as u64) > WIDE_PER_THREAD
    }

    /// What DuckDB's JSON reader holds of a vector of 2,048 of these lines on one thread, sniffing
    /// or reading, which counts against its memory limit: the text, and a parsed value of 16
    /// bytes for each key and each value. Measured on DuckDB 1.5.5 alone, the sniff of an NDJSON
    /// of 2,000 one-digit keys an object (18 KB a line) peaked at 140, 216 and 231 MB at 1,000,
    /// 2,500 and 5,000 lines, levelling off as a vector fills; this charges it 168 MB. Under
    /// [`STAGE_MEMORY`] alone, its stage ran out of memory on one thread.
    fn json_vector(self) -> u64 {
        2048 * (self.columns * 2 * 16 + self.line)
    }
}

/// Put DuckDB's memory limit back to `was`, as `current_setting('memory_limit')` read it before.
///
/// That text is cut to one decimal, and set again does not always read back the same: on DuckDB
/// 1.5.5, `SET memory_limit = '12.7 GiB'` reads back as `12.6 GiB` (the text parses to a hair
/// under 12.7 GiB, which is shown cut, not rounded). Put back from its text, a limit shrank by
/// 0.1 GiB with each staged table, on a machine whose default was `12.7 GiB` (a host's
/// re-review); here, under a 4000 MB cgroup, `3.1 GiB` read back `3.0 GiB`. So where the
/// text does not read back as itself, the middle of what it stands for is set (`12.75 GiB`),
/// which does, and stays within half its last digit of the limit it was read from. `RESET
/// memory_limit` is no way back: on 1.5.5 it reports DuckDB's default and leaves the buffer pool
/// at the cap, measured (a sort then ran out of memory at 1 MiB).
fn put_back_memory_limit(conn: &duckdb::Connection, was: &str) -> duckdb::Result<()> {
    let now = |conn: &duckdb::Connection| -> duckdb::Result<String> {
        conn.query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))
    };
    conn.execute_batch(&format!("SET memory_limit = '{was}'"))?;
    if now(conn)? == was {
        return Ok(());
    }
    if let Some((number, unit)) = was.split_once(' ') {
        let middle = if number.contains('.') {
            format!("{number}5 {unit}")
        } else {
            format!("{number}.5 {unit}")
        };
        conn.execute_batch(&format!("SET memory_limit = '{middle}'"))?;
    }
    Ok(())
}

/// Fill the stage from `reader`, then write table `name` from it.
fn staged(
    conn: &duckdb::Connection,
    name: &str,
    reader: &str,
    zone: &str,
    hold: Option<&Hold>,
    write: Option<&str>,
) -> Result<Written, Staged> {
    conn.execute_batch(&format!("ATTACH ':memory:' AS {STAGE}"))?;
    let written = (|| {
        let staged = format!("{STAGE}.main.{}", quote(name));
        crate::trace::span("file.read", || {
            conn.execute_batch(&format!("CREATE TABLE {staged} AS SELECT * FROM {reader}"))
        })?;
        let mut st = conn.prepare(
            "SELECT column_name, data_type FROM duckdb_columns() \
             WHERE database_name = ? AND table_name = ? ORDER BY column_index",
        )?;
        let produced: Vec<(String, String)> = st
            .query_map([STAGE, name], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut rows = 0;
        let (kept, keep) = match hold {
            None => (produced, "true".to_string()),
            Some(h) => {
                rows = conn.query_row(&format!("SELECT count(*) FROM {staged}"), [], |r| {
                    r.get::<_, i64>(0)
                })? as u64;
                if rows > h.max_rows {
                    return Err(Staged::Refused(crate::exec::Fault::host(
                        "E_TOO_LARGE",
                        format!(
                            "{rows} row(s), more than the {} this source and the session's \
                             budget allow. Refused, not truncated: nothing of it was loaded.",
                            h.max_rows
                        ),
                    )));
                }
                let kept = held_columns(produced, h)?;
                nulls_refused(conn, &staged, h)?;
                (kept, keep_predicate(h.narrowing))
            }
        };
        let select = converted(&kept, zone);
        // No narrowing, no predicate: the statement a file's table is written with.
        let select = if keep == "true" {
            format!("SELECT {select} FROM {staged}")
        } else {
            format!("SELECT * FROM (SELECT {select} FROM {staged}) WHERE {keep}")
        };
        // Watched beside the write, and taken before the stage is detached under it.
        if let Some(write) = write {
            conn.execute_batch(&format!("SET memory_limit = '{write}'"))?;
        }
        let beside = Beside::start(conn, &select);
        write_table(conn, name, &select)?;
        Ok((rows, kept, beside.and_then(Beside::finish)))
    })();
    let _ = conn.execute_batch(&format!("DETACH {STAGE}"));
    written
}

/// Each of `kept`, as the reader produced it, converted to the bench's own type (REQ-15), as a
/// select list.
fn converted(kept: &[(String, String)], zone: &str) -> String {
    kept.iter()
        .map(|(column, produced)| {
            let held = crate::canonical::held(produced);
            let value = crate::canonical::convert(&quote(column), column, produced, &held, zone)
                .unwrap_or_else(|| quote(column));
            format!("{value} AS {}", quote(column))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether what `reader` reads goes straight into its table ([`read_into`]) rather than through
/// the stage. Measured with bench/ingest.py, five runs, minima (docs/PERFORMANCE.md, 2026-10-11):
///
/// - **Parquet**, a file or an HTTP body staged as one: 14.4% and 12.2% fewer instructions, the
///   loader's peak memory 102 MB to 72 MB, the first `describe` 10–26% sooner. Read straight in.
/// - **NDJSON and JSON files**: 3.2% fewer instructions, 13% less CPU, and 143 MB to 99 MB; but
///   the first `describe` later, measured again against the stage interleaved, ten runs each
///   (R2-1): 1,000,000 rows, 63 MB, 876 ms staged against 1,083 ms straight in (minimum),
///   898 against 1,116 ms (median), for 3% more instructions and 9% more loader CPU. DuckDB's
///   JSON reader parses on every thread into the stage, on the write's one straight in. Staged
///   whatever its size (R4-2): staged only up to 64 MiB while the stage held the whole table in
///   memory, a 10,000,000-row file (633 MB) was read straight in, 6.63 s to its first
///   `describe`. Held to [`STAGE_MEMORY`] since, its stage holds no memory that grows with the
///   file (`a_json_file_is_staged_whatever_its_size`; docs/PERFORMANCE.md R4-2 has the numbers).
///   Read on a thread of its own beside the write instead, its chunks referenced into it, the
///   first `describe` was no sooner and the loader spent 15% more CPU, handing every vector
///   across cores; with the reader on every thread too, 1.2 G more instructions and 60 MB more.
/// - **Parquet** staged again, measured with the same runs: no sooner, 17% more instructions and
///   101 MB instead of 70 MB. Read straight in.
/// - **CSV and TSV files**: 2.6% *more* instructions and the first `describe` 21% later. DuckDB
///   parses a CSV file on every thread, which a write on one thread gives up, and its types are
///   sniffed twice. Staged.
/// - **A CSV or NDJSON body over HTTP** is spooled to a file and read as that file is (R3-3),
///   by the same rule.
/// - **A narrowed body** is charged the rows the reader produced, before the narrowing drops
///   any, which only the stage counts. Staged.
fn reads_twice(reader: &str, hold: Option<&Hold>) -> bool {
    let as_a_file = reader.starts_with("read_parquet(");
    match hold {
        None => as_a_file,
        Some(h) => as_a_file && keep_predicate(h.narrowing) == "true",
    }
}

/// Read what `reader` reads straight into table `name`, converted, in one statement on one
/// thread, in a transaction that a refusal rolls back.
///
/// The stage is a table in memory that the reader fills with every thread, then read back on
/// one to write the table: its own appends and statistics, then a scan of them, and the whole
/// table held between, in memory up to [`STAGE_MEMORY`]. Read straight into the write, the
/// reader runs on the write's one thread, asked for its types first (`DESCRIBE`), and the table
/// is laid out as before (`the_physical_layout_says_nothing_of_where_data_came_from`). A held body is read to one row
/// past its cap, so one too large is refused having written at most that, and the rollback
/// leaves the file as it was (`a_body_read_into_its_table_is_held_and_refused_whole`).
fn read_into(
    conn: &duckdb::Connection,
    name: &str,
    reader: &str,
    zone: &str,
    hold: Option<&Hold>,
) -> Result<Written, Staged> {
    let threads: i64 = conn.query_row("SELECT current_setting('threads')", [], |r| r.get(0))?;
    let produced: Vec<(String, String)> = crate::trace::span("file.describe", || {
        let mut st = conn.prepare(&format!("DESCRIBE SELECT * FROM {reader}"))?;
        let cols = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        cols.collect::<Result<_, duckdb::Error>>()
    })?;
    let kept = match hold {
        None => produced,
        Some(h) => held_columns(produced, h)?,
    };
    let limit = hold.map_or(String::new(), |h| {
        format!(" LIMIT {}", h.max_rows.saturating_add(1))
    });
    conn.execute_batch("BEGIN TRANSACTION")?;
    let written = (|| {
        let select = format!("SELECT {} FROM {reader}{limit}", converted(&kept, zone));
        // A Parquet file is read a second time, by the watch beside the write: on parquet-1m,
        // ten runs interleaved, the first `describe` 568 / 623 → 509 / 546 ms (minimum /
        // median) for 4.2% more instructions (docs/PERFORMANCE.md, R3-2). A JSON reader is not
        // (none comes here since R4-2 stages every JSON file): parsed twice, it would cost about
        // 3.9 G more per 1,000,000 rows to take some 45 ms off the path.
        let beside = reader
            .starts_with("read_parquet(")
            .then(|| Beside::start(conn, &select))
            .flatten();
        write_table(conn, name, &select)?;
        let beside = beside.and_then(Beside::finish);
        let mut rows = 0;
        if let Some(h) = hold {
            rows = conn.query_row(&format!("SELECT count(*) FROM {}", quote(name)), [], |r| {
                r.get::<_, i64>(0)
            })? as u64;
            if rows > h.max_rows {
                return Err(too_many(h));
            }
            nulls_refused(conn, &quote(name), h)?;
        }
        Ok((rows, beside))
    })();
    match written {
        Ok((rows, beside)) => {
            conn.execute_batch("COMMIT")?;
            Ok((rows, kept, beside))
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            // Not given back by the write in the aborted transaction.
            let _ = conn.execute_batch(&format!("SET threads = {threads}"));
            Err(e)
        }
    }
}

/// A held body with more rows than its cap: refused, never cut.
fn too_many(h: &Hold) -> Staged {
    Staged::Refused(crate::exec::Fault::host(
        "E_TOO_LARGE",
        format!(
            "more than the {} row(s) this source and the session's budget allow. Refused, not \
             truncated: nothing of it was loaded.",
            h.max_rows
        ),
    ))
}

/// A column pinned `nullable = false` that holds a null in `table`, refused.
fn nulls_refused(conn: &duckdb::Connection, table: &str, h: &Hold) -> Result<(), Staged> {
    for d in h.declared.iter().filter(|d| !d.nullable) {
        let column = &d.name;
        let nulls: i64 = conn.query_row(
            &format!(
                "SELECT count(*) FROM {table} WHERE {} IS NULL",
                quote(column)
            ),
            [],
            |r| r.get(0),
        )?;
        if nulls > 0 {
            return Err(Staged::Refused(crate::exec::Fault::host(
                "E_SCHEMA",
                format!(
                    "column `{column}` holds {nulls} null(s), and is pinned `nullable = false`"
                ),
            )));
        }
    }
    Ok(())
}

/// The columns a held body loads, with the types the reader gave them: the pinned ones, in their
/// order, each checked; or the narrowing's; or every one. Their nulls are checked once they are
/// read ([`nulls_refused`]).
fn held_columns(
    produced: Vec<(String, String)>,
    h: &Hold,
) -> Result<Vec<(String, String)>, Staged> {
    let schema = |why: String| Staged::Refused(crate::exec::Fault::host("E_SCHEMA", why));
    let find = |c: &str| produced.iter().find(|(n, _)| n == c).cloned();
    if h.declared.is_empty() {
        let Some(wanted) = &h.narrowing.columns else {
            return Ok(produced);
        };
        return wanted
            .iter()
            .map(|c| find(c).ok_or_else(|| schema(format!("column `{c}` is not in the body"))))
            .collect();
    }
    let mut out = Vec::with_capacity(h.declared.len());
    for d in h.declared {
        let (column, ty) = find(&d.name)
            .ok_or_else(|| schema(format!("column `{}` is not in the body", d.name)))?;
        let pinned = crate::canonical::held(&duckdb_type(&d.ty).expect("validated at config time"));
        let loads = crate::canonical::held(&ty);
        if pinned != loads {
            return Err(schema(format!(
                "column `{column}` loads as {loads}, and is pinned as `{}` ({pinned})",
                d.ty
            )));
        }
        out.push((column, ty));
    }
    Ok(out)
}

/// What [`load_read`] made: the table, unrecorded; the rows the reader produced; and each column
/// kept, as the reader typed it.
pub type ReadTable = (LoadedTable, u64, Vec<(String, String)>);

/// REQ-17: put the rows `reader` reads — an HTTP body DuckDB reads itself, staged or spooled
/// to a file of `bytes` bytes, `head` — on the bench as table `name`, exactly as the same file is
/// put there ([`stage_and_write`]), held to `hold`. Returns the table, unrecorded, the rows the reader produced, and each
/// column kept as the reader typed it. A reader's failure says what DuckDB said, which names the
/// column when one is to blame.
#[allow(clippy::too_many_arguments)]
pub fn load_read(
    bench: &Bench,
    name: &str,
    reader: &str,
    zone: &str,
    hold: &Hold,
    what: &str,
    bytes: u64,
    head: Option<&Path>,
) -> Result<ReadTable, crate::exec::Fault> {
    let conn = bench.connection();
    // An undeclared NDJSON body is a spooled file (R3-3), read as that file is, its timestamps
    // as text (R3-1); a declared one names its own `columns` already.
    let written = if hold.declared.is_empty() {
        write_stamps_as_text(conn, name, reader, zone, Some(hold), bytes, head)
    } else {
        stage_and_write(conn, name, reader, zone, Some(hold), head)
    };
    let (read, produced, beside) = written.map_err(|e| match e {
        Staged::Refused(f) => f,
        Staged::Reader(e) => {
            let said = e.to_string();
            let first = said.lines().next().unwrap_or("").to_string();
            // A value that does not cast to its column's type is the schema's fault, not the
            // body's shape; so is a declared CSV column the header does not have.
            let code = if said.contains("Conversion Error")
                || said.contains("Could not convert")
                || said.contains("COLUMN_TYPES error")
            {
                "E_SCHEMA"
            } else {
                "E_CONFIG"
            };
            crate::exec::Fault::host(code, format!("DuckDB could not read it: {first}"))
        }
    })?;
    let internal = |e: duckdb::Error| crate::exec::Fault::host("E_INTERNAL", e.to_string());
    let rows: u64 = conn
        .query_row(&format!("SELECT count(*) FROM {}", quote(name)), [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(internal)? as u64;
    if read > rows {
        eprintln!(
            "benchy: source `{name}` returned {} row(s) outside the narrowing it was given; they \
             were not loaded",
            read - rows
        );
    }
    let columns = columns_of(conn, name).map_err(internal)?;
    let seen = kept_or_watched(conn, name, &columns, beside);
    Ok((
        LoadedTable {
            name: name.to_string(),
            rows,
            columns,
            source: what.to_string(),
            profile: None,
            seen,
        },
        read,
        produced,
    ))
}

/// Write table `name` from `select`, on one thread. The file a table is stored in is the agent's
/// to inspect — `pragma_database_size()`, `pragma_storage_info` — and a parallel write lays it
/// out by how the input was split among threads: on the eval's 840,960 meter rows, 9 row groups
/// from CSV and 8 with 16 free blocks from Parquet, measured. Written on one thread, the same rows
/// make the same file whatever they came from (REQ-15
/// `the_physical_layout_says_nothing_of_where_data_came_from`), and a smaller one: 7.0 MiB in 28
/// blocks, against 8.0 MiB from CSV and 11.5 MiB with 15 free blocks from Parquet, as a server
/// stored them. A server's start on that data, loading and profiling, went from 0.7–0.8 s to
/// 0.9 s for the CSV and 0.6 s to 0.7–0.8 s for the Parquet, on 4 busy ARM cores, with the
/// same CPU time: the reading, most of the work, still has every thread.
///
/// R4-1, measured again on DuckDB 1.5.5 (docs/PERFORMANCE.md): split into fixed groups of row
/// groups, each a transaction on a thread of its own, committed in order, 10,000,000 rows were
/// written in 1.3 s instead of 2.9 s, and exec-10m answered its first `describe` 20–24% sooner;
/// the row groups, segments and compression were the same from every source. But DuckDB numbers
/// the blocks in the order its threads write them, so a segment's `block_id`, which the agent
/// reads from `pragma_storage_info`, changed from one load to the next (and through DuckDB's own
/// parallel insert, the block count too). So the write stays on one thread
/// (`the_physical_layout_says_nothing_of_where_data_came_from`).
fn write_table(conn: &duckdb::Connection, name: &str, select: &str) -> Result<(), duckdb::Error> {
    let threads: i64 = conn.query_row("SELECT current_setting('threads')", [], |r| r.get(0))?;
    conn.execute_batch("SET threads = 1")?;
    // Its own thread's CPU: DuckDB has no other here, and a file's watch runs beside it
    // ([`Beside`]), whose CPU the process's would count too.
    let written = crate::trace::span_thread("write_table", || {
        conn.execute_batch(&format!("CREATE TABLE {} AS {select}", quote(name)))
    });
    // The write's own failure is the one to report. Inside a transaction it aborts it, and
    // then this fails too, "Current transaction is aborted": reported first, that hid why a
    // file could not be read (`a_file_that_fails_midway_leaves_no_table_and_says_why`).
    let restored = conn.execute_batch(&format!("SET threads = {threads}"));
    written.and(restored)
}

/// R2-4: the profile a stream keeps as its rows go in ([`crate::profile::Watch`]), kept for a
/// table already written, from one scan of it as Arrow in its own order, each column read with
/// [`crate::profile::Held::of`] as a stream's is. `None`, and the profile reads the table as
/// before, when the scan fails.
///
/// A file's profile read the table back twice, the scan's aggregates and the pass, each
/// decompressing it: 0.78–0.83 G instructions per 1,000,000 rows, 26% of parquet-1m
/// (docs/PERFORMANCE.md). The bounded queries still read the table, as for a stream. Only the
/// columns the watch reads are scanned, a DuckDB vector at a time.
///
/// A staged file is watched from its stage instead, beside the write ([`Beside`]); this is for a
/// file read straight into its table, which has no stage, and for a watch that could not start.
fn watched(
    conn: &duckdb::Connection,
    table: &str,
    columns: &[(String, String)],
) -> Option<std::sync::Arc<crate::profile::Seen>> {
    #[cfg(test)]
    WATCHED_FROM.with(|w| w.set((w.get().0, w.get().1 + 1)));
    crate::trace::span("file.watch", || {
        watch_scan(conn, &quote(table), columns, None).map(std::sync::Arc::new)
    })
}

/// What the rows of `from`, a table or a parenthesised `SELECT` whose columns are `columns`,
/// show a [`crate::profile::Watch`], read in their order. `None` if the scan fails, or `stop`
/// is set before it ends.
fn watch_scan(
    conn: &duckdb::Connection,
    from: &str,
    columns: &[(String, String)],
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> Option<crate::profile::Seen> {
    let mut watch = crate::profile::Watch::new(columns);
    let needed: Vec<usize> = (0..columns.len()).filter(|i| watch.needs(*i)).collect();
    if !needed.is_empty() {
        let list = needed
            .iter()
            .map(|i| quote(&columns[*i].0))
            .collect::<Vec<_>>()
            .join(", ");
        let mut st = conn.prepare(&format!("SELECT {list} FROM {from}")).ok()?;
        let _ = st.stream_arrow([]).ok()?;
        while let Some(s) = st.step().ok()? {
            if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed)) {
                return None;
            }
            let b = arrow::array::RecordBatch::from(&s);
            let mut held: Vec<Option<crate::profile::Held>> =
                (0..columns.len()).map(|_| None).collect();
            // The table holds the bench's types already: a time is an instant, in UTC.
            for (j, i) in needed.iter().enumerate() {
                held[*i] = crate::profile::Held::of(b.column(j), &columns[*i].1, true);
            }
            watch.add(b.num_rows(), &held);
        }
    }
    watch.finish()
}

/// What writing a table from a reader made: the rows the reader produced, each column kept as the
/// reader typed it, and what the watch beside the write saw, if one ran ([`Beside`]).
type Written = (u64, Vec<(String, String)>, Option<WatchedStage>);

/// What a watch beside a staged file's write saw: the columns of the `SELECT` it scanned, as
/// DuckDB described them, and the [`crate::profile::Seen`].
type WatchedStage = (Vec<(String, String)>, crate::profile::Seen);

/// R3-2: a staged file's [`crate::profile::Watch`], kept on a thread of its own from the
/// stage's converted `SELECT` — the statement the table is written from — while the load's one
/// thread writes the table from it.
///
/// Kept after the write instead ([`watched`]), the watch was on the path to the first
/// `describe`: 333 ms on one thread for `examples/capacity.py`'s 5,000,000-row, four-column CSV,
/// where the scan and pass R2-4 replaced took 184 ms on every thread, which made the first
/// `describe` 4.4% later (docs/PERFORMANCE.md, R3-2). The write holds DuckDB to one thread
/// ([`write_table`]), so the two are a core each. The stage is uncompressed and, up to
/// [`STAGE_MEMORY`], in memory: read twice, it is read where it lies, and the thread holds no more
/// than a chunk of it. Past the cap, where both read back what spilled, the 5,000,000-row CSV's
/// loader peaked 47 MB higher (317 → 364 MB), and still not more as the file grows
/// (`a_large_csv_is_not_held_in_memory_while_it_is_staged`).
///
/// The `SELECT` is the write's, rows and order alike, on a second connection to the same
/// database, in the loader's time zone, which a narrowing's window is read in. What it saw is
/// used only if its columns are the table's ([`kept_or_watched`]); else the table is read.
///
/// The thread is started by the load, after the loader has confined itself
/// (`loader::open_then`): Landlock confines only threads started after it (`sandbox::alone`).
/// It is joined before the stage is detached, and before the load returns, so no thread outlives
/// the write; a failed write stops it at its next batch.
struct Beside {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Option<WatchedStage>>>,
}

impl Beside {
    /// Start watching what `select` reads. `None`, and the table is read after the write, if no
    /// second connection or thread could be had.
    fn start(conn: &duckdb::Connection, select: &str) -> Option<Self> {
        let zone: String = conn
            .query_row("SELECT current_setting('TimeZone')", [], |r| r.get(0))
            .ok()?;
        let second = conn.try_clone().ok()?;
        let select = select.to_string();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name("benchy-file-watch".into())
            .spawn(move || {
                crate::trace::span_thread("file.watch", || {
                    second
                        .execute_batch(&format!("SET TimeZone = {}", literal(&zone)))
                        .ok()?;
                    let mut st = second.prepare(&format!("DESCRIBE {select}")).ok()?;
                    let columns: Vec<(String, String)> = st
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                        .ok()?
                        .collect::<Result<_, _>>()
                        .ok()?;
                    let seen =
                        watch_scan(&second, &format!("({select})"), &columns, Some(&stopped))?;
                    Some((columns, seen))
                })
            })
            .ok()?;
        Some(Beside {
            stop,
            thread: Some(thread),
        })
    }

    /// Wait for the watch to see every row, and take what it saw.
    fn finish(mut self) -> Option<WatchedStage> {
        self.thread.take()?.join().ok()?
    }
}

impl Drop for Beside {
    /// A write that failed: stop the watch, and wait for it.
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = t.join();
        }
    }
}

/// What the watch beside the write saw, if it saw a table whose columns are `columns`; else the
/// table `name`, scanned now ([`watched`]).
fn kept_or_watched(
    conn: &duckdb::Connection,
    name: &str,
    columns: &[(String, String)],
    beside: Option<WatchedStage>,
) -> Option<std::sync::Arc<crate::profile::Seen>> {
    match beside {
        Some((seen_columns, seen)) if seen_columns == columns => {
            #[cfg(test)]
            WATCHED_FROM.with(|w| w.set((w.get().0 + 1, w.get().1)));
            Some(std::sync::Arc::new(seen))
        }
        _ => watched(conn, name, columns),
    }
}

#[cfg(test)]
thread_local! {
    /// How many files on this thread had their profile kept from their stage, beside the write,
    /// and how many from a scan of their table after it.
    static WATCHED_FROM: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

/// A table's columns and their DuckDB types, in order.
fn columns_of(
    conn: &duckdb::Connection,
    table: &str,
) -> Result<Vec<(String, String)>, duckdb::Error> {
    let mut st = conn.prepare(
        "SELECT column_name, data_type FROM duckdb_columns() \
         WHERE database_name = current_database() AND table_name = ? ORDER BY column_index",
    )?;
    let rows = st.query_map([table], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

/// Put every supported file at the top of a directory, but no dotfile, on the bench. The zero-config path: this is what
/// `benchy-mcp ./data` does.
pub fn load_dir(bench: &mut Bench, dir: &Path) -> Result<Vec<LoadedTable>, LoadError> {
    let mut out = Vec::new();
    for (name, path) in plan_dir(dir)? {
        out.push(load_file(bench, &name, &path)?);
    }
    Ok(out)
}

/// Which table each supported file in `dir` becomes, in a stable order — refused as a whole if
/// two files would make the same table, or if there is nothing to load.
pub fn plan_dir(dir: &Path) -> Result<Vec<(String, PathBuf)>, LoadError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| LoadError::Unreadable {
            path: dir.into(),
            why: e.to_string(),
        })?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    // Skipped, never silently: the operator learns each file that did not become a table and
    // why (`a_skipped_file_is_named_in_the_operator_log`). The agent hears nothing of it.
    entries.retain(|p| {
        // A dotfile is one the operator may not know is there: `.secret.csv` became a table, and
        // the AppleDouble `._meters.csv` macOS writes beside `meters.csv` on a non-APFS volume
        // stopped startup as a second `meters` (`a_hidden_file_in_a_directory_is_not_a_table`).
        // Named with --source or in a config it loads: the operator chose it.
        let hidden = p
            .file_name()
            .is_some_and(|n| n.as_encoded_bytes().starts_with(b"."));
        let why = if hidden {
            "a hidden file; a folder's dotfiles are not read"
        } else if p.is_dir() {
            "a directory; a folder is read at its top level only"
        } else if !p.is_file() {
            "not a regular file"
        } else if reader_for(p).is_none() {
            "not a format benchy reads"
        } else {
            return true;
        };
        eprintln!("benchy: skipped {}: {why}", p.display());
        false
    });
    if entries.is_empty() {
        return Err(LoadError::Empty { dir: dir.into() });
    }

    let mut seen: Vec<(String, PathBuf)> = Vec::new();
    for path in entries {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let name = table_name(stem).ok_or_else(|| LoadError::BadName { path: path.clone() })?;
        if let Some((_, first)) = seen.iter().find(|(n, _)| *n == name) {
            return Err(LoadError::Duplicate {
                name,
                first: first.clone(),
                second: path,
            });
        }
        seen.push((name, path));
    }
    Ok(seen)
}

/// The DuckDB spelling of a protocol type token. Paired with [`crate::exec::arrow_type`]: one
/// token, one Arrow type, one DuckDB type, decided at config time rather than at load time.
pub fn duckdb_type(token: &str) -> Option<String> {
    Some(
        match token {
            "bool" => "BOOLEAN",
            "int8" => "TINYINT",
            "int16" => "SMALLINT",
            "int32" => "INTEGER",
            "int64" => "BIGINT",
            "uint8" => "UTINYINT",
            "uint16" => "USMALLINT",
            "uint32" => "UINTEGER",
            "uint64" => "UBIGINT",
            "float32" => "FLOAT",
            "float64" => "DOUBLE",
            "string" => "VARCHAR",
            "binary" => "BLOB",
            "date" => "DATE",
            "time" => "TIME",
            "timestamp" => "TIMESTAMP",
            "timestamptz" => "TIMESTAMP WITH TIME ZONE",
            other if other.starts_with("decimal(") => {
                // validated by exec::arrow_type before we get here
                return Some(other.to_uppercase());
            }
            _ => return None,
        }
        .to_string(),
    )
}

/// The vocabulary token of a DuckDB type: [`duckdb_type`] read backwards. `None` for a type the
/// vocabulary has no token for, such as a struct.
pub fn vocabulary_of(ty: &str) -> Option<String> {
    Some(
        match ty {
            "BOOLEAN" => "bool",
            "TINYINT" => "int8",
            "SMALLINT" => "int16",
            "INTEGER" => "int32",
            "BIGINT" => "int64",
            "UTINYINT" => "uint8",
            "USMALLINT" => "uint16",
            "UINTEGER" => "uint32",
            "UBIGINT" => "uint64",
            "FLOAT" => "float32",
            "DOUBLE" => "float64",
            "VARCHAR" => "string",
            "BLOB" => "binary",
            "DATE" => "date",
            "TIME" => "time",
            "TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS" => "timestamp",
            "TIMESTAMP WITH TIME ZONE" => "timestamptz",
            d if d.starts_with("DECIMAL(") => return Some(d.to_lowercase()),
            _ => return None,
        }
        .to_string(),
    )
}

/// REQ-4: put an exec connector's output on the bench.
///
/// The table is created from the **operator's** declaration, not from the stream's schema, so the
/// column names and types an agent sees are the ones an operator wrote down. The stream having
/// already been validated against that same declaration is what makes the two agree.
pub fn load_exec(
    bench: &mut Bench,
    src: &crate::exec::ExecSource,
    n: &crate::exec::Narrowing,
    run_id: &str,
) -> Result<(LoadedTable, crate::exec::Fetched), crate::exec::Fault> {
    load_exec_within(bench, src, n, run_id, &crate::admission::Budget::default())
}

/// REQ-4 with REQ-12 attached: ask what it costs, and refuse over budget *before* fetching. A
/// rebuilt bench never comes back here: it re-attaches what the session stored, so the source is
/// asked once per session (`reload_does_not_amplify`).
pub fn load_exec_within(
    bench: &mut Bench,
    src: &crate::exec::ExecSource,
    n: &crate::exec::Narrowing,
    run_id: &str,
    budget: &crate::admission::Budget,
) -> Result<(LoadedTable, crate::exec::Fetched), crate::exec::Fault> {
    if let Some((rows, bytes)) =
        crate::trace::span("exec.estimate", || crate::exec::estimate(src, n, run_id))
    {
        budget.admit(&src.name, rows, bytes)?;
    }
    let origin = Origin {
        what: format!("exec:{}", src.command.display()),
        zone: src.timezone.as_deref().unwrap_or(crate::canonical::UTC),
    };
    let mut ingest = Ingest::open(bench, &src.name, &src.schema, &origin, n)?;
    // The fetch spends what it reads on `budget`, failed or not.
    let fetched = crate::trace::span("exec.stream", || {
        crate::exec::fetch_within(src, n, run_id, budget, &mut |b| ingest.push(b))
    })?;
    let table = ingest.finish(&fetched.schema)?;
    Ok((record(bench, table)?, fetched))
}

/// Where a table came from, for the operator, and the zone its zone-less timestamps are in.
pub struct Origin<'a> {
    /// Operator-facing only: never shown to the agent.
    pub what: String,
    pub zone: &'a str,
}

/// Put a table an [`Ingest`] wrote in the bench's catalogue. Without this the table exists in
/// DuckDB but not in the catalogue, so `describe` never mentions it and the agent can only reach
/// it by guessing the name.
pub fn record(
    bench: &mut Bench,
    mut table: LoadedTable,
) -> Result<LoadedTable, crate::exec::Fault> {
    // What the stream showed of itself goes to the profile alone, which takes it as it is.
    let seen = table.seen.take();
    let mut kept = table.clone();
    kept.seen = seen;
    bench
        .record(kept)
        .map_err(|e| crate::exec::Fault::host("E_CONFIG", e))?;
    Ok(table)
}

/// A validated stream going onto the bench as table `name`, batch by batch as it arrives
/// ([`Ingest::push`]), in one transaction that commits only once the whole stream has been
/// accepted ([`Ingest::finish`]). Dropped unfinished — the stream was refused, or failed after
/// its last batch — it rolls back, and leaves no table.
///
/// `declared` is what the table is (the operator's declaration, or a host stream's own); the
/// schema the batches are sent in is one the declaration accepted (`crate::exec::type_ok`). Each
/// batch is read by DuckDB as sent and converted to the bench's own types by the same SQL that
/// converts a file's (`crate::canonical`), so a stream and a file holding the same data make the
/// same table (REQ-15).
///
/// Nothing is held but the batch at hand. A stream was collected into a list until it ended, then
/// copied into an in-memory staging database, then written: 10,000,000 rows from ClickHouse
/// peaked at 940 MB in the loader, against 710 MB for the same rows from a CSV file
/// (`a_large_stream_is_not_held_in_memory_while_it_loads`). Each batch now goes from the decoder
/// through the conversion into the table, and DuckDB writes each row group to the file as it
/// fills. The inserts are serial — one batch each, read by a scan that does not split — so the
/// rows land in the order they were sent, in row groups filled one after another, and the file is
/// laid out as one [`write_table`] writes on one thread
/// (`the_physical_layout_says_nothing_of_where_data_came_from`). Measured, the same with the
/// loader's threads as with one.
pub struct Ingest<'a> {
    conn: &'a duckdb::Connection,
    name: String,
    declared: Vec<crate::exec::FieldDecl>,
    what: String,
    zone: String,
    narrowing: crate::exec::Narrowing,
    /// Each column as it arrives and as the bench holds it, once the stream's types are known.
    columns: Option<Vec<Column>>,
    /// The `INSERT` each batch goes through: the conversion and the narrowing.
    statement: String,
    /// It, prepared on the first batch and run for each. Planning it for every batch cost
    /// little: preparing once took 0.3% off a 10,000,000-row load's instructions, inside the
    /// spread, so it is kept for the batches a small-batch connector sends, not for the numbers.
    prepared: Option<duckdb::Statement<'a>>,
    /// Small batches wait here until there are enough rows to be worth a statement.
    pending: Vec<arrow::array::RecordBatch>,
    pending_rows: usize,
    received: u64,
    /// The loader's thread count, given back when the stream is done.
    threads: i64,
    /// The transaction is open: committed by [`Ingest::finish`], rolled back on drop.
    open: bool,
    /// The profile, kept as the rows go in ([`crate::profile::Watch`]), on a thread of its own.
    watch: Option<Watcher>,
}

/// Batches waiting for the watch, at most: each is already in memory for its insert, so the
/// queue holds them a little longer rather than copying them. With one, the decoder can run a
/// batch ahead of the watch; the insert, the slower of the two, sets the pace.
const WATCH_QUEUE: usize = 1;

/// The profile's [`crate::profile::Watch`], kept on a thread of its own beside the insert.
///
/// The watch reads each batch as it is sent, the insert the same batch through DuckDB, and
/// neither needs the other: run one after the other on one thread, the watch's time was on the
/// path to the first `describe` (R2-2: exec-10m's cold start did not fall with its CPU when the
/// profile moved from reading the table back to the insert's thread). A batch is an `Arc` of
/// its columns, so handing it over copies nothing.
///
/// The thread starts with the stream's first batch, after the loader has confined itself:
/// Landlock confines only the threads started after it (`sandbox::alone`), and a confined
/// loader opens no [`Ingest`] before it is confined (`loader::open_then`), so this thread is
/// confined with the rest. It ends when the stream does: [`Ingest::finish`] joins it for what it
/// saw, and a refused stream's drop joins it too, so no thread outlives the table.
struct Watcher {
    batches: Option<std::sync::mpsc::SyncSender<arrow::array::RecordBatch>>,
    thread: Option<std::thread::JoinHandle<crate::profile::Watch>>,
}

impl Watcher {
    /// Start watching, with each column's held type and whether a zone-less timestamp in it is
    /// read in UTC ([`crate::profile::Held::of`]). `None` if no thread could be started: the
    /// profile then reads the table, as for anything the watch cannot see.
    fn start(mut watch: crate::profile::Watch, held: Vec<(String, bool)>) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<arrow::array::RecordBatch>(WATCH_QUEUE);
        let thread = std::thread::Builder::new()
            .name("benchy-watch".into())
            .spawn(move || {
                for batch in rx {
                    crate::trace::span_thread("stream.watch", || {
                        let cols: Vec<Option<crate::profile::Held>> = held
                            .iter()
                            .enumerate()
                            .map(|(i, (ty, utc))| {
                                watch
                                    .needs(i)
                                    .then(|| crate::profile::Held::of(batch.column(i), ty, *utc))
                                    .flatten()
                            })
                            .collect();
                        watch.add(batch.num_rows(), &cols);
                    });
                }
                watch
            })
            .ok()?;
        Some(Watcher {
            batches: Some(tx),
            thread: Some(thread),
        })
    }

    /// Hand a batch over. A watch that is gone has given up, and the profile reads the table.
    fn send(&self, batch: &arrow::array::RecordBatch) {
        if let Some(tx) = &self.batches {
            let _ = tx.send(batch.clone());
        }
    }

    /// Wait for the watch to see every batch sent, and take it.
    fn finish(mut self) -> Option<crate::profile::Watch> {
        drop(self.batches.take());
        self.thread.take()?.join().ok()
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        drop(self.batches.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A column as it arrives, and as the bench holds it.
struct Column {
    ident: String,
    name: String,
    sent: String,
    held: String,
    zone: String,
}

/// Batches smaller than this are gathered into one insert: a connector may send one row per batch,
/// and a statement per row is a statement's planning per row. A batch this large or larger, as
/// ClickHouse's 65,409-row batches are, goes in as it is, so it is never copied to be gathered.
/// Eight of DuckDB's vectors.
///
/// Not a row group's 122,880 (docs/PERFORMANCE.md, 2026-10-11): gathering ClickHouse's and the
/// bench's 64 Ki-row batches in pairs to reach it changed 10,000,000-row loads by -0.1% (exec)
/// and -0.8% (ClickHouse) of instructions, inside the spread, and raised the loader's peak memory
/// by 5 MB, the copy the pairs are gathered into.
const GATHER_ROWS: usize = 16_384;

/// The table function a batch is read through, registered on the loader's connection only. It
/// lives in the loader's database, which is never the engine's: sealing opens a fresh one.
const BATCH_FN: &str = "benchy_batch";

thread_local! {
    /// The batch [`BATCH_FN`] reads. Set on the thread that runs the statement, and taken by the
    /// scan's init, which DuckDB runs on that thread as it starts the statement.
    static BATCH: std::cell::RefCell<Option<arrow::array::RecordBatch>> =
        const { std::cell::RefCell::new(None) };
}

/// [`BATCH_FN`]: the batch in [`BATCH`], a vector at a time.
///
/// The batch is taken when the statement starts (`init`), not when it is planned (`bind`): the
/// statement is prepared once per stream and run once per batch, and only its column types are
/// fixed at planning.
struct BatchScan;

struct BatchInit {
    batch: arrow::array::RecordBatch,
    at: std::sync::atomic::AtomicUsize,
}

impl duckdb::vtab::VTab for BatchScan {
    type BindData = ();
    type InitData = BatchInit;

    fn bind(bind: &duckdb::vtab::BindInfo) -> Result<(), Box<dyn std::error::Error>> {
        let schema = BATCH
            .with(|b| b.borrow().as_ref().map(|b| b.schema()))
            .ok_or("no batch is waiting")?;
        for f in schema.fields() {
            bind.add_result_column(f.name(), duckdb::vtab::to_duckdb_logical_type_for_field(f)?);
        }
        Ok(())
    }

    fn init(_: &duckdb::vtab::InitInfo) -> Result<BatchInit, Box<dyn std::error::Error>> {
        let batch = BATCH
            .with(|b| b.borrow_mut().take())
            .ok_or("no batch is waiting")?;
        Ok(BatchInit {
            batch,
            at: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn func(
        func: &duckdb::vtab::TableFunctionInfo<Self>,
        output: &mut duckdb::core::DataChunkHandle,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::atomic::Ordering::Relaxed;
        let init = func.get_init_data();
        let vector = unsafe { duckdb::ffi::duckdb_vector_size() } as usize;
        let at = init.at.fetch_add(vector, Relaxed);
        if at >= init.batch.num_rows() {
            output.set_len(0);
            return Ok(());
        }
        let n = vector.min(init.batch.num_rows() - at);
        fill(&init.batch, at, n, output)
    }
}

/// Rows `at..at + n` of `batch` into `output`, one DuckDB vector.
///
/// Fixed-width values are copied as they lie, and validity a word at a time. A string goes in
/// without DuckDB's UTF-8 check: arrow checked every string of the stream as it decoded it
/// (`ArrayData::validate_values`), and DuckDB checks each again as it writes the column's
/// statistics, so a check here was the second of three. A string of up to 12 bytes is written into
/// its 16-byte slot directly, as DuckDB inlines it; a longer one is copied into the vector's own
/// heap. Every other layout goes through duckdb-rs's conversion, which is what all of them used:
/// one C call, its UTF-8 check and a validity call per null, per value. Measured with
/// bench/ingest.py on 10,000,000 rows (docs/PERFORMANCE.md, 2026-10-11): -4.6% of the load's
/// instructions from an exec connector, -3.7% from ClickHouse
/// (`a_batch_lands_as_duckdbs_own_arrow_conversion_lands_it`).
fn fill(
    batch: &arrow::array::RecordBatch,
    at: usize,
    n: usize,
    output: &mut duckdb::core::DataChunkHandle,
) -> Result<(), Box<dyn std::error::Error>> {
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::DataType as D;
    use duckdb::ffi;
    let chunk = output.get_ptr();
    for (i, col) in batch.columns().iter().enumerate() {
        let col = col.slice(at, n);
        let mut vector = unsafe { ffi::duckdb_data_chunk_get_vector(chunk, i as u64) };
        match col.data_type() {
            D::Int8
            | D::Int16
            | D::Int32
            | D::Int64
            | D::UInt8
            | D::UInt16
            | D::UInt32
            | D::UInt64
            | D::Float32
            | D::Float64
            | D::Date32
            | D::Timestamp(_, None) => {
                // DuckDB holds each of these in the same bytes: the integer, the float, days
                // since the epoch, and a count of the timestamp's own unit.
                let width = col.data_type().primitive_width().expect("fixed width");
                let data = col.to_data();
                let bytes = &data.buffers()[0].as_slice()
                    [data.offset() * width..(data.offset() + n) * width];
                unsafe {
                    let to = ffi::duckdb_vector_get_data(vector) as *mut u8;
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), to, bytes.len());
                }
                validity(&col, vector, n);
            }
            D::Utf8 => {
                let a = col.as_string::<i32>();
                strings(vector, n, |i| {
                    (!a.is_null(i)).then(|| a.value(i).as_bytes())
                });
                validity(&col, vector, n);
            }
            D::LargeUtf8 => {
                let a = col.as_string::<i64>();
                strings(vector, n, |i| {
                    (!a.is_null(i)).then(|| a.value(i).as_bytes())
                });
                validity(&col, vector, n);
            }
            D::Utf8View => {
                let a = col.as_string_view();
                strings(vector, n, |i| {
                    (!a.is_null(i)).then(|| a.value(i).as_bytes())
                });
                validity(&col, vector, n);
            }
            _ => duckdb::vtab::arrow::write_arrow_array_to_vector(&col, &mut vector)?,
        }
    }
    output.set_len(n);
    Ok(())
}

/// DuckDB's `string_t` holds up to this many bytes inline, after its 4-byte length; a longer
/// string is a length, a 4-byte prefix and a pointer.
const INLINE: usize = 12;

/// `n` strings into `vector`, each the bytes of a valid UTF-8 string, or `None` for a null.
fn strings<'s>(
    vector: duckdb::ffi::duckdb_vector,
    n: usize,
    value: impl Fn(usize) -> Option<&'s [u8]>,
) {
    let slots = unsafe {
        std::slice::from_raw_parts_mut(
            duckdb::ffi::duckdb_vector_get_data(vector) as *mut [u8; 16],
            n,
        )
    };
    for (i, slot) in slots.iter_mut().enumerate() {
        let s = value(i).unwrap_or_default();
        if s.len() <= INLINE {
            // Zeroed past the string: DuckDB compares an inlined string by all 16 bytes.
            let mut cell = [0u8; 16];
            cell[..4].copy_from_slice(&(s.len() as u32).to_ne_bytes());
            cell[4..4 + s.len()].copy_from_slice(s);
            *slot = cell;
        } else {
            unsafe {
                duckdb::ffi::duckdb_unsafe_vector_assign_string_element_len(
                    vector,
                    i as u64,
                    s.as_ptr() as *const std::os::raw::c_char,
                    s.len() as u64,
                )
            }
        }
    }
}

/// `col`'s nulls into `vector`'s validity. Arrow's bitmap and DuckDB's mask are both a bit per
/// row, set when the row has a value, least significant first, so they are copied 64 rows at a
/// time.
fn validity(col: &arrow::array::ArrayRef, vector: duckdb::ffi::duckdb_vector, n: usize) {
    let Some(nulls) = col.nulls().filter(|b| b.null_count() > 0) else {
        return;
    };
    let words = unsafe {
        duckdb::ffi::duckdb_vector_ensure_validity_writable(vector);
        std::slice::from_raw_parts_mut(
            duckdb::ffi::duckdb_vector_get_validity(vector),
            n.div_ceil(64),
        )
    };
    let bits = nulls.inner();
    let chunks = bits.inner().bit_chunks(bits.offset(), n);
    let mut k = 0;
    for w in chunks.iter() {
        words[k] = w;
        k += 1;
    }
    if chunks.remainder_len() > 0 {
        words[k] = chunks.remainder_bits() | (u64::MAX << chunks.remainder_len());
    }
}

fn ingest_fault(what: &str, e: impl std::fmt::Display) -> crate::exec::Fault {
    // No name here: every caller says which source it was.
    crate::exec::Fault::host("E_HOST_SCHEMA_MISMATCH", format!("{what}: {e}"))
}

/// A conversion's failure, as DuckDB's `error()` says it: it names the column.
fn conversion_fault(e: duckdb::Error) -> crate::exec::Fault {
    let said = e.to_string();
    let why = said
        .split_once("Invalid Input Error: ")
        .map_or(said.as_str(), |(_, w)| w);
    crate::exec::Fault::host(
        "E_HOST_SCHEMA_MISMATCH",
        format!(
            "could not create the table: {}",
            why.lines().next().unwrap_or("")
        ),
    )
}

impl<'a> Ingest<'a> {
    /// Start putting table `name`, declared as `declared`, from `origin`, on the bench, keeping
    /// only the rows inside `n`.
    pub fn open(
        bench: &'a Bench,
        name: &str,
        declared: &[crate::exec::FieldDecl],
        origin: &Origin,
        n: &crate::exec::Narrowing,
    ) -> Result<Self, crate::exec::Fault> {
        let conn = bench.connection();
        // Registered once per bench; a second registration is refused, and is harmless.
        let _ = conn.register_table_function::<BatchScan>(BATCH_FN);
        // DuckDB's compression is left as it is. Turning FSST off was the one candidate that
        // paid: -4.9% of a 10,000,000-row load's instructions, and the same file for these
        // tables; but 1,000,000 rows of unique text took 96 MB instead of 39 MB, so a session
        // with text-heavy tables would pay in disk and memory. Chimp, Patas, ZSTD, Roaring and
        // DICT_FSST off saved nothing. Decided 2026-10-11: FSST kept on, small free-text tables
        // matter more than about 5% of a load (docs/PERFORMANCE.md).
        // One thread while the stream goes in. Every insert is serial whatever the setting — one
        // batch, read by a scan that does not split — but the loader's other threads were not
        // idle: a 3,000,000-row attach took 17.0 to 19.5 G instructions and 4.0 to 4.8 s of CPU
        // with them, and 11.3 G and 2.5 s with one, measured.
        let threads: i64 = conn
            .query_row("SELECT current_setting('threads')", [], |r| r.get(0))
            .map_err(|e| ingest_fault("could not start the table", e))?;
        conn.execute_batch("SET threads = 1; BEGIN TRANSACTION")
            .map_err(|e| ingest_fault("could not start the table", e))?;
        Ok(Ingest {
            conn,
            name: name.to_string(),
            declared: declared.to_vec(),
            what: origin.what.clone(),
            zone: origin.zone.to_string(),
            narrowing: n.clone(),
            columns: None,
            statement: String::new(),
            prepared: None,
            pending: Vec::new(),
            pending_rows: 0,
            received: 0,
            threads,
            open: true,
            watch: None,
        })
    }

    /// Create the table for a stream sent as `stream`. Opened without a declaration, the stream
    /// declares itself (`crate::exec::decls_from_stream`), cut to the narrowing's columns when it
    /// names some: an undeclared HTTP Arrow body (REQ-17).
    fn create(&mut self, stream: &arrow::datatypes::Schema) -> Result<(), crate::exec::Fault> {
        if self.declared.is_empty() {
            let own = crate::exec::decls_from_stream(stream)
                .map_err(|why| crate::exec::Fault::host("E_HOST_SCHEMA_MISMATCH", why))?;
            self.declared = match &self.narrowing.columns {
                None => own,
                Some(wanted) => wanted
                    .iter()
                    .map(|c| {
                        own.iter().find(|d| &d.name == c).cloned().ok_or_else(|| {
                            crate::exec::Fault::host(
                                "E_SCHEMA",
                                format!("column `{c}` is not in the stream"),
                            )
                        })
                    })
                    .collect::<Result<_, _>>()?,
            };
        }
        let mut columns = Vec::with_capacity(self.declared.len());
        for f in &self.declared {
            let sent = stream
                .field_with_name(&f.name)
                .map_err(|_| {
                    crate::exec::Fault::host(
                        "E_HOST_SCHEMA_MISMATCH",
                        format!("column `{}` is not in the stream", f.name),
                    )
                })?
                .data_type();
            let staged = stage_type(sent).ok_or_else(|| {
                crate::exec::Fault::host(
                    "E_HOST_SCHEMA_MISMATCH",
                    format!("column `{}` is {sent:?}, which cannot be read", f.name),
                )
            })?;
            let held =
                crate::canonical::held(&duckdb_type(&f.ty).expect("validated at config time"));
            // A zoned timestamp arrives as the UTC instant it is, so its zone is UTC whatever
            // the source's: the source's zone is for what was sent without one.
            let zone = if matches!(sent, arrow::datatypes::DataType::Timestamp(_, Some(_))) {
                crate::canonical::UTC
            } else {
                &self.zone
            };
            columns.push(Column {
                ident: quote(&f.name),
                name: f.name.clone(),
                sent: staged,
                held,
                zone: zone.to_string(),
            });
        }
        let held_cols = columns
            .iter()
            .map(|c| format!("{} {}", c.ident, c.held))
            .collect::<Vec<_>>()
            .join(", ");
        let converted = columns
            .iter()
            .map(|c| {
                let value = crate::canonical::convert(&c.ident, &c.name, &c.sent, &c.held, &c.zone)
                    .unwrap_or_else(|| c.ident.clone());
                format!("{value} AS {}", c.ident)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let table = quote(&self.name);
        // REQ-6: **the narrowing is applied by the host, at every declared level, always** — a
        // connector may declare `enforced` and be wrong. And it is applied before anything
        // reaches the workspace file: a row outside it is never inserted. Deleting rows
        // afterwards was not enough — DuckDB keeps deleted rows in the file, where their bytes
        // and pragma_storage_info's statistics gave them back
        // (`narrowed_rows_never_reach_the_file`). It is applied to the values as the bench holds
        // them, so a window means the same instants whatever the stream sent.
        self.statement = format!(
            "INSERT INTO {table} SELECT * FROM (SELECT {converted} FROM {BATCH_FN}()) WHERE {}",
            keep_predicate(&self.narrowing)
        );
        self.conn
            .execute_batch(&format!("CREATE TABLE {table} ({held_cols})"))
            .map_err(|e| ingest_fault("could not create the table", e))?;
        let held: Vec<(String, String)> = columns
            .iter()
            .map(|c| (c.name.clone(), c.held.clone()))
            .collect();
        // Rows the narrowing drops never reach the table, and are dropped in SQL: what the
        // stream shows is not the table, which the profile reads instead.
        if keep_predicate(&self.narrowing) == "true" {
            let read = columns
                .iter()
                .map(|c| (c.held.clone(), c.zone == crate::canonical::UTC))
                .collect();
            self.watch = Watcher::start(crate::profile::Watch::new(&held), read);
        }
        self.columns = Some(columns);
        Ok(())
    }

    /// Put one validated batch on the bench.
    pub fn push(&mut self, b: arrow::array::RecordBatch) -> Result<(), crate::exec::Fault> {
        if self.columns.is_none() {
            self.create(&b.schema())?;
        }
        // The declaration decides column order; the stream is permuted into it, because
        // set-equality is what the host checks and a producer may order differently.
        let ordered = permute(&b, &self.declared)
            .map_err(|e| crate::exec::Fault::host("E_HOST_SCHEMA_MISMATCH", e))?;
        drop(b);
        self.received += ordered.num_rows() as u64;
        if ordered.num_rows() >= GATHER_ROWS {
            // Large enough on its own: inserted as it is, never copied to be gathered.
            self.insert_pending()?;
            return self.insert(ordered);
        }
        self.pending_rows += ordered.num_rows();
        self.pending.push(ordered);
        if self.pending_rows >= GATHER_ROWS {
            self.insert_pending()?;
        }
        Ok(())
    }

    /// Insert what is waiting, as one batch.
    fn insert_pending(&mut self) -> Result<(), crate::exec::Fault> {
        let batch = match self.pending.len() {
            0 => return Ok(()),
            1 => self.pending.pop().expect("one"),
            _ => {
                let schema = self.pending[0].schema();
                let all = arrow::compute::concat_batches(&schema, &self.pending)
                    .map_err(|e| ingest_fault("could not gather the batches", e))?;
                self.pending.clear();
                all
            }
        };
        self.pending_rows = 0;
        self.insert(batch)
    }

    /// Insert one batch, through the conversion and the narrowing.
    fn insert(&mut self, batch: arrow::array::RecordBatch) -> Result<(), crate::exec::Fault> {
        if let Some(w) = &self.watch {
            w.send(&batch);
        }
        BATCH.with(|b| *b.borrow_mut() = Some(batch));
        let inserted = crate::trace::span_thread("stream.insert", || {
            if self.prepared.is_none() {
                self.prepared = Some(self.conn.prepare(&self.statement)?);
            }
            self.prepared
                .as_mut()
                .expect("prepared")
                .execute([])
                .map(drop)
        });
        BATCH.with(|b| *b.borrow_mut() = None);
        inserted.map_err(conversion_fault)
    }

    /// The stream, sent as `stream`, was accepted whole: commit the table. It is returned to be
    /// [`record`]ed.
    pub fn finish(
        mut self,
        stream: &arrow::datatypes::Schema,
    ) -> Result<LoadedTable, crate::exec::Fault> {
        if self.columns.is_none() {
            // No batches: a complete stream with no rows is an empty table.
            self.create(stream)?;
        }
        self.insert_pending()?;
        crate::trace::span("stream.commit", || self.conn.execute_batch("COMMIT"))
            .map_err(|e| ingest_fault("could not write the table", e))?;
        self.open = false;
        let kept: i64 = self
            .conn
            .query_row(
                &format!("SELECT count(*) FROM {}", quote(&self.name)),
                [],
                |r| r.get(0),
            )
            .map_err(|e| ingest_fault("could not count the rows", e))?;
        let dropped = self.received - kept as u64;
        if dropped > 0 {
            eprintln!(
                "benchy: source `{}` returned {dropped} row(s) outside the narrowing it was \
                 given; they were not loaded",
                self.name
            );
        }
        let columns = self.columns.take().expect("created above");
        let seen = self
            .watch
            .take()
            .and_then(Watcher::finish)
            .filter(|_| dropped == 0)
            .and_then(crate::profile::Watch::finish);
        Ok(LoadedTable {
            name: self.name.clone(),
            rows: kept as u64,
            columns: columns.into_iter().map(|c| (c.name, c.held)).collect(),
            source: self.what.clone(),
            profile: None,
            seen: seen.map(std::sync::Arc::new),
        })
    }
}

impl Drop for Ingest<'_> {
    fn drop(&mut self) {
        if self.open {
            let _ = self.conn.execute_batch("ROLLBACK");
        }
        let _ = self
            .conn
            .execute_batch(&format!("SET threads = {}", self.threads));
    }
}

/// The DuckDB type a column sent as `t` arrives in: the type DuckDB reads it as, a dictionary as
/// its values and a zoned timestamp as the UTC instant it is, zone-less.
fn stage_type(t: &arrow::datatypes::DataType) -> Option<String> {
    use arrow::datatypes::{DataType::*, TimeUnit};
    Some(
        match t {
            Boolean => "BOOLEAN",
            Int8 => "TINYINT",
            Int16 => "SMALLINT",
            Int32 => "INTEGER",
            Int64 => "BIGINT",
            UInt8 => "UTINYINT",
            UInt16 => "USMALLINT",
            UInt32 => "UINTEGER",
            UInt64 => "UBIGINT",
            Float32 => "FLOAT",
            Float64 => "DOUBLE",
            Utf8 | LargeUtf8 | Utf8View => "VARCHAR",
            Dictionary(_, v) => return stage_type(v),
            Binary | LargeBinary | BinaryView => "BLOB",
            Date32 => "DATE",
            Time64(TimeUnit::Microsecond) => "TIME",
            Timestamp(TimeUnit::Second, _) => "TIMESTAMP_S",
            Timestamp(TimeUnit::Millisecond, _) => "TIMESTAMP_MS",
            Timestamp(TimeUnit::Microsecond, _) => "TIMESTAMP",
            Timestamp(TimeUnit::Nanosecond, _) => "TIMESTAMP_NS",
            Decimal128(p, s) => return Some(format!("DECIMAL({p},{s})")),
            _ => return None,
        }
        .to_string(),
    )
}

/// One batch in the declaration's column order, as DuckDB reads it: a dictionary unpacked — its
/// size was bounded as the stream was read — and a zoned timestamp as its instant, zone-less.
/// Neither changes a value: the zone a timestamp is shown in is not part of it.
fn permute(
    b: &arrow::array::RecordBatch,
    declared: &[crate::exec::FieldDecl],
) -> Result<arrow::array::RecordBatch, String> {
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    let mut fields = Vec::with_capacity(declared.len());
    let mut cols = Vec::with_capacity(declared.len());
    for f in declared {
        let i = b
            .schema()
            .index_of(&f.name)
            .map_err(|_| format!("column `{}` is not in the batch", f.name))?;
        let mut col = b.column(i).clone();
        if let DataType::Dictionary(_, values) = col.data_type() {
            let values = values.as_ref().clone();
            col = arrow::compute::cast(&col, &values).map_err(|e| e.to_string())?;
        }
        if let DataType::Timestamp(unit, Some(_)) = col.data_type() {
            use arrow::datatypes::{
                TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
                TimestampSecondType,
            };
            col = match unit {
                TimeUnit::Second => std::sync::Arc::new(
                    col.as_primitive::<TimestampSecondType>()
                        .clone()
                        .with_timezone_opt(None::<String>),
                ),
                TimeUnit::Millisecond => std::sync::Arc::new(
                    col.as_primitive::<TimestampMillisecondType>()
                        .clone()
                        .with_timezone_opt(None::<String>),
                ),
                TimeUnit::Microsecond => std::sync::Arc::new(
                    col.as_primitive::<TimestampMicrosecondType>()
                        .clone()
                        .with_timezone_opt(None::<String>),
                ),
                TimeUnit::Nanosecond => std::sync::Arc::new(
                    col.as_primitive::<TimestampNanosecondType>()
                        .clone()
                        .with_timezone_opt(None::<String>),
                ),
            };
        }
        fields.push(Field::new(&f.name, col.data_type().clone(), f.nullable));
        cols.push(col);
    }
    arrow::array::RecordBatch::try_new(std::sync::Arc::new(Schema::new(fields)), cols)
        .map_err(|e| e.to_string())
}

/// The rows a narrowing admits, as a SQL predicate: `true` when there is no narrowing.
///
/// Nulls are outside. A row whose timestamp or entity is unknown cannot be shown to be inside the
/// window, and `col IN (...)` is `NULL` rather than `false` for a null column — so the null case
/// is named explicitly, which is the difference between filtering and appearing to.
fn keep_predicate(n: &crate::exec::Narrowing) -> String {
    let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let ident = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut keep = vec!["true".to_string()];
    if let Some((col, start, end)) = &n.time {
        keep.push(format!("{} IS NOT NULL", ident(col)));
        if let Some(s) = start {
            keep.push(format!("{} >= TIMESTAMPTZ {}", ident(col), lit(s)));
        }
        if let Some(e) = end {
            // Half-open: [start, end).
            keep.push(format!("{} < TIMESTAMPTZ {}", ident(col), lit(e)));
        }
    }
    if let Some((col, ids)) = &n.entities {
        let list = ids.iter().map(|i| lit(i)).collect::<Vec<_>>().join(", ");
        keep.push(format!(
            "{c} IS NOT NULL AND {c} IN ({list})",
            c = ident(col)
        ));
    }
    keep.join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::*;
    use arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    /// A batch of every layout [`fill`] writes itself, and some it hands to duckdb-rs: nulls in
    /// runs and alone, strings empty, inline, exactly 12 bytes, longer, and not ASCII. Built
    /// `skip` rows longer and sliced, so every array starts at an offset that is not a multiple
    /// of 64, which is how a batch arrives when a vector starts inside it.
    fn every_layout(rows: usize, skip: usize) -> RecordBatch {
        let all = rows + skip;
        let null = |i: usize| i % 7 == 3 || (100..300).contains(&i);
        let text = |i: usize| match i % 5 {
            0 => String::new(),
            1 => format!("MT_{i}"),
            2 => "abcdefghijkl".to_string(),
            3 => format!("abcdefghijklm{i}"),
            _ => format!("Ærø – ünïcødé {i}, a longer string than twelve bytes"),
        };
        let strings: Vec<Option<String>> = (0..all).map(|i| (!null(i)).then(|| text(i))).collect();
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("i", Arc::new(Int64Array::from_iter_values(0..all as i64))),
            (
                "i8",
                Arc::new(Int8Array::from_iter(
                    (0..all).map(|i| (!null(i)).then_some((i % 120) as i8)),
                )),
            ),
            (
                "u64",
                Arc::new(UInt64Array::from_iter_values(
                    (0..all).map(|i| u64::MAX - i as u64),
                )),
            ),
            (
                "f32",
                Arc::new(Float32Array::from_iter(
                    (0..all).map(|i| (!null(i)).then_some(i as f32 / 10.0)),
                )),
            ),
            (
                "f64",
                Arc::new(Float64Array::from_iter(
                    (0..all).map(|i| (!null(i)).then_some(i as f64 * 0.25 - 3.0)),
                )),
            ),
            (
                "ts",
                Arc::new(TimestampMillisecondArray::from_iter((0..all).map(|i| {
                    (!null(i)).then_some(1_700_000_000_000 + 60_000 * i as i64)
                }))),
            ),
            (
                "d",
                Arc::new(Date32Array::from_iter(
                    (0..all).map(|i| (!null(i)).then_some(19_000 + i as i32)),
                )),
            ),
            ("s", Arc::new(StringArray::from(strings.clone()))),
            ("l", Arc::new(LargeStringArray::from(strings.clone()))),
            ("v", Arc::new(StringViewArray::from(strings.clone()))),
            (
                "b",
                Arc::new(BooleanArray::from_iter(
                    (0..all).map(|i| (!null(i)).then_some(i % 3 == 0)),
                )),
            ),
            (
                "dec",
                Arc::new(
                    Decimal128Array::from_iter((0..all).map(|i| (!null(i)).then_some(i as i128)))
                        .with_precision_and_scale(10, 2)
                        .unwrap(),
                ),
            ),
            (
                "bin",
                Arc::new(BinaryArray::from_iter(
                    strings.iter().map(|s| s.as_ref().map(|s| s.as_bytes())),
                )),
            ),
        ];
        let schema = Schema::new(
            columns
                .iter()
                .map(|(n, a)| Field::new(*n, a.data_type().clone(), true))
                .collect::<Vec<_>>(),
        );
        RecordBatch::try_new(
            Arc::new(schema),
            columns.into_iter().map(|(_, a)| a).collect(),
        )
        .unwrap()
        .slice(skip, rows)
    }

    #[test]
    fn a_batch_lands_as_duckdbs_own_arrow_conversion_lands_it() {
        // The reference is duckdb-rs's conversion, which every batch went through before `fill`:
        // the same batch appended to a table of the types it arrives in, compared column by
        // column, row by row, with the table `Ingest` wrote.
        let dir = std::env::temp_dir().join(format!(
            "benchy-fill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bench = Bench::open_in(&dir).unwrap();
        let batch = every_layout(5_000, 3);
        let declared: Vec<crate::exec::FieldDecl> = [
            ("i", "int64"),
            ("i8", "int8"),
            ("u64", "uint64"),
            ("f32", "float32"),
            ("f64", "float64"),
            ("ts", "timestamp"),
            ("d", "date"),
            ("s", "string"),
            ("l", "string"),
            ("v", "string"),
            ("b", "bool"),
            ("dec", "decimal(10,2)"),
            ("bin", "binary"),
        ]
        .iter()
        .map(|(n, t)| crate::exec::FieldDecl {
            name: n.to_string(),
            ty: t.to_string(),
            nullable: true,
        })
        .collect();
        let origin = Origin {
            what: "test".into(),
            zone: crate::canonical::UTC,
        };
        let n = crate::exec::Narrowing::default();
        let mut ingest = Ingest::open(&bench, "t", &declared, &origin, &n).unwrap();
        // Two batches, so the prepared statement runs twice and each run reads its own batch.
        ingest.push(batch.slice(0, 2_500)).unwrap();
        ingest.push(batch.slice(2_500, 2_500)).unwrap();
        let table = ingest.finish(&batch.schema()).unwrap();
        assert_eq!(table.rows, 5_000);

        let conn = bench.connection();
        let staged = batch
            .schema()
            .fields()
            .iter()
            .map(|f| format!("{} {}", quote(f.name()), stage_type(f.data_type()).unwrap()))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!("CREATE TABLE expect ({staged})"))
            .unwrap();
        let mut app = conn.appender("expect").unwrap();
        app.append_record_batch(batch.clone()).unwrap();
        drop(app);
        for f in batch.schema().fields() {
            let c = quote(f.name());
            let differ: i64 = conn
                .query_row(
                    &format!(
                        "SELECT count(*) FROM t FULL JOIN expect e USING (i) \
                         WHERE t.{c} IS DISTINCT FROM e.{c} OR t.i IS NULL OR e.i IS NULL"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(differ, 0, "column {c} differs");
        }
        let nulls: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE s IS NULL", [], |r| r.get(0))
            .unwrap();
        assert!(nulls > 200, "the nulls arrived: {nulls}");
        drop(bench);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh bench in a directory of its own, removed with it.
    struct TestBench(Option<Bench>, std::path::PathBuf);

    impl TestBench {
        fn new(what: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "benchy-{what}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            TestBench(Some(Bench::open_in(&dir).unwrap()), dir)
        }
        fn bench(&self) -> &Bench {
            self.0.as_ref().unwrap()
        }
    }

    impl Drop for TestBench {
        fn drop(&mut self) {
            drop(self.0.take());
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }

    /// `columns` (name, declared type, array), sent in batches of `cut` rows.
    fn stream(
        bench: &Bench,
        name: &str,
        columns: Vec<(&str, &str, ArrayRef)>,
        cut: usize,
    ) -> LoadedTable {
        let declared: Vec<crate::exec::FieldDecl> = columns
            .iter()
            .map(|(n, t, _)| crate::exec::FieldDecl {
                name: n.to_string(),
                ty: t.to_string(),
                nullable: true,
            })
            .collect();
        let schema = Arc::new(Schema::new(
            columns
                .iter()
                .map(|(n, _, a)| Field::new(*n, a.data_type().clone(), true))
                .collect::<Vec<_>>(),
        ));
        let batch =
            RecordBatch::try_new(schema.clone(), columns.into_iter().map(|c| c.2).collect())
                .unwrap();
        let origin = Origin {
            what: "test".into(),
            zone: crate::canonical::UTC,
        };
        let n = crate::exec::Narrowing::default();
        let mut ingest = Ingest::open(bench, name, &declared, &origin, &n).unwrap();
        let mut at = 0;
        while at < batch.num_rows() {
            let len = cut.min(batch.num_rows() - at);
            ingest.push(batch.slice(at, len)).unwrap();
            at += len;
        }
        ingest.finish(&schema).unwrap()
    }

    /// The profile of `t` from what its stream showed, and from reading the table, which must
    /// be the same; and whether the scan and the pass were taken from what the stream showed.
    fn both(bench: &Bench, t: &LoadedTable, shape: &crate::profile::Shape) -> (bool, bool) {
        use crate::profile::{compute, SEEN_USED};
        assert!(t.seen.is_some(), "{}: the stream kept nothing", t.name);
        SEEN_USED.with(|u| u.set((0, 0)));
        let kept = compute(
            bench.connection(),
            &t.name,
            &t.columns,
            shape,
            t.seen.clone(),
        );
        let used = SEEN_USED.with(|u| u.get());
        let read = compute(bench.connection(), &t.name, &t.columns, shape, None);
        assert_eq!(kept, read, "{}", t.name);
        assert!(
            !read.as_ref().unwrap().lines.is_empty(),
            "{}: {read:?}",
            t.name
        );
        (used.0 == 1, used.1 == 1)
    }

    fn ts_us(v: Vec<Option<i64>>) -> ArrayRef {
        Arc::new(TimestampMicrosecondArray::from(v).with_timezone("UTC"))
    }

    #[test]
    fn a_body_read_into_its_table_is_held_and_refused_whole() {
        // A Parquet body is read straight into its table (`read_into`); held to its cap and its
        // pins, a refusal rolls the write back, so nothing of it is loaded and the file keeps no
        // trace of it, and one that fits loads every row.
        let tb = TestBench::new("held");
        let bench = tb.bench();
        let conn = bench.connection();
        let body = tb.1.join("body.parquet");
        conn.execute_batch(&format!(
            "COPY (SELECT i AS id, CASE WHEN i = 7 THEN NULL ELSE 'm' || i END AS meter \
             FROM range(10) r(i)) TO {} (FORMAT parquet)",
            literal(&body.to_string_lossy())
        ))
        .unwrap();
        let reader = reader_for(&body).unwrap();
        let size = || -> (i64, i64) {
            conn.execute_batch("CHECKPOINT").unwrap();
            conn.query_row(
                "SELECT used_blocks, free_blocks FROM pragma_database_size()",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let before = size();
        let none = crate::exec::Narrowing::default();
        let read = |max_rows: u64, declared: &[crate::exec::FieldDecl]| {
            let hold = Hold {
                declared,
                narrowing: &none,
                max_rows,
            };
            load_read(
                bench,
                "t",
                &reader,
                crate::canonical::UTC,
                &hold,
                "test",
                0,
                None,
            )
        };
        let refused = read(9, &[]).unwrap_err();
        assert_eq!(refused.code, "E_TOO_LARGE", "{}", refused.detail);
        let pinned = [crate::exec::FieldDecl {
            name: "meter".into(),
            ty: "string".into(),
            nullable: false,
        }];
        let refused = read(10, &pinned).unwrap_err();
        assert_eq!(refused.code, "E_SCHEMA", "{}", refused.detail);
        assert!(refused.detail.contains("1 null"), "{}", refused.detail);
        let gone: i64 = conn
            .query_row(
                "SELECT count(*) FROM duckdb_tables() WHERE table_name = 't'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, 0);
        assert_eq!(size(), before, "a refused body left blocks behind");
        let (t, produced, _) = read(10, &[]).unwrap();
        assert_eq!((t.rows, produced), (10, 10));
    }

    #[test]
    fn the_profile_kept_as_rows_arrive_is_the_profile_of_the_table() {
        let tb = TestBench::new("seen");
        let bench = tb.bench();
        let no = crate::profile::Shape::default();
        const Q: i64 = 900_000_000;
        let t0 = 1_395_878_400_000_000i64; // 2014-03-27, UTC, in microseconds

        // Seven meters with the quirks the profile looks for (`profile::tests::fixture`): a dead
        // meter, a late start, a stop into nulls, a pause, an hour all read 0, an hour all read
        // double, and one time twice, 0 and null.
        let mut ts = Vec::new();
        let mut meter = Vec::new();
        let mut kw = Vec::new();
        for i in 0..672i64 {
            for s in 1..8i64 {
                ts.push(Some(t0 + i * Q));
                meter.push(Some(format!("m{s}")));
                let base = (s * 10) as f64 + (i % 96) as f64 * 0.5 + 1.0;
                kw.push(match () {
                    _ if s == 1 => Some(0.0),
                    _ if s == 2 && i < 140 => Some(0.0),
                    _ if s == 3 && i > 500 => None,
                    _ if s == 4 && (300..=420).contains(&i) => Some(0.0),
                    _ if (200..=203).contains(&i) => Some(0.0),
                    _ if (260..=263).contains(&i) => Some(2.0 * base),
                    _ => Some(base),
                });
            }
        }
        for v in [Some(0.0), None] {
            ts.push(Some(t0 + 2 * 86_400_000_000 + 6 * 3_600_000_000));
            meter.push(Some("m5".into()));
            kw.push(v);
        }
        let meters =
            |m: &Vec<Option<String>>| -> ArrayRef { Arc::new(StringArray::from(m.clone())) };
        let a = stream(
            bench,
            "a",
            vec![
                ("ts", "timestamptz", ts_us(ts.clone())),
                ("meter", "string", meters(&meter)),
                ("kw", "float64", Arc::new(Float64Array::from(kw.clone()))),
            ],
            1_000,
        );
        assert_eq!(both(bench, &a, &no), (true, true));
        // Stated, and the same.
        let stated = crate::profile::Shape {
            time_column: Some("ts".into()),
            series: vec!["meter".into()],
        };
        assert_eq!(both(bench, &a, &stated), (true, true));

        // One series, every time once, in milliseconds without a zone: zeros in runs, nulls,
        // NaN and a negative zero.
        let n = 20_000i64;
        let b = stream(
            bench,
            "b",
            vec![
                (
                    "ts",
                    "timestamp",
                    Arc::new(TimestampMillisecondArray::from_iter_values(
                        (0..n).map(|i| 1_767_225_600_000 + i * 60_000),
                    )),
                ),
                (
                    "kw",
                    "float64",
                    Arc::new(Float64Array::from_iter((0..n).map(|i| match i % 997 {
                        0..=40 => Some(0.0),
                        41 => None,
                        42 => Some(f64::NAN),
                        43 => Some(-0.0),
                        k => Some(k as f64 * 1.5),
                    }))),
                ),
            ],
            4_096,
        );
        assert_eq!(both(bench, &b, &no), (true, true));

        // An integer key, a float, an unsigned counter, before 1970 with odd microseconds, and a
        // date after the time column.
        let n = 6_000i64;
        let c = stream(
            bench,
            "c",
            vec![
                (
                    "ts",
                    "timestamp",
                    Arc::new(TimestampMicrosecondArray::from_iter_values(
                        (0..n).map(|i| -1_000_000_123 + (i / 5) * 60_000_000),
                    )),
                ),
                (
                    "sensor",
                    "int32",
                    Arc::new(Int32Array::from_iter(
                        (0..n).map(|i| (i % 97 != 13).then_some((i % 5) as i32 - 2)),
                    )),
                ),
                (
                    "temp",
                    "float32",
                    Arc::new(Float32Array::from_iter_values(
                        (0..n).map(|i| ((i * 37) % 400) as f32 / 8.0 - 10.0),
                    )),
                ),
                (
                    "count",
                    "uint64",
                    Arc::new(UInt64Array::from_iter_values(
                        (0..n).map(|i| u64::MAX - (i as u64 % 300) * 1_000_003),
                    )),
                ),
                (
                    "day",
                    "date",
                    Arc::new(Date32Array::from_iter_values(
                        (0..n).map(|i| -3 + (i / 1440) as i32),
                    )),
                ),
            ],
            2_500,
        );
        assert_eq!(both(bench, &c, &no), (true, true));

        // Two text columns, either of which may be the key; and both stated together, which no
        // tally kept: the scan is kept, the pass is read.
        let n = 9_000usize;
        let d = stream(
            bench,
            "d",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + (i / 30) as i64 * Q)).collect()),
                ),
                (
                    "site",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("site {}", i % 3)),
                    )),
                ),
                (
                    "meter",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("a meter with a long name {}", i % 30)),
                    )),
                ),
                (
                    "kw",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..n).map(|i| {
                        if i % 30 == 7 && i < 3_000 {
                            0.0
                        } else {
                            (i % 11) as f64
                        }
                    }))),
                ),
            ],
            65_536,
        );
        assert_eq!(both(bench, &d, &no), (true, true));
        let two = crate::profile::Shape {
            time_column: None,
            series: vec!["site".into(), "meter".into()],
        };
        assert_eq!(both(bench, &d, &two), (true, false));

        // Three candidates: too many to tally each, so the pass is read.
        let e = stream(
            bench,
            "e",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + (i / 30) as i64 * Q)).collect()),
                ),
                (
                    "a",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("{}", i % 2)),
                    )),
                ),
                (
                    "b",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("{}", i % 30)),
                    )),
                ),
                (
                    "c",
                    "int64",
                    Arc::new(Int64Array::from_iter_values((0..n).map(|i| (i % 3) as i64))),
                ),
                (
                    "kw",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
                ),
            ],
            1_024,
        );
        assert_eq!(both(bench, &e, &no), (true, false));

        // A date as the time column, ten series a day.
        let f = stream(
            bench,
            "f",
            vec![
                (
                    "day",
                    "date",
                    Arc::new(Date32Array::from_iter_values(
                        (0..3_000)
                            .filter(|i| i % 10 != 4 || *i < 1_000)
                            .map(|i| 19_000 + i / 10),
                    )),
                ),
                (
                    "unit",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..3_000)
                            .filter(|i| i % 10 != 4 || *i < 1_000)
                            .map(|i| format!("u{}", i % 10)),
                    )),
                ),
                (
                    "v",
                    "int64",
                    Arc::new(Int64Array::from_iter_values(
                        (0..3_000)
                            .filter(|i| i % 10 != 4 || *i < 1_000)
                            .map(|i| (i % 7) as i64),
                    )),
                ),
            ],
            700,
        );
        assert_eq!(both(bench, &f, &no), (true, true));

        // More series than a guessed key's tally keeps: it gives up, and the pass is read.
        let n = 120_000usize;
        let g = stream(
            bench,
            "g",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + (i / 60_000) as i64 * Q)).collect()),
                ),
                (
                    "id",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("k{}", i % 60_000)),
                    )),
                ),
                (
                    "v",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
                ),
            ],
            65_536,
        );
        assert_eq!(both(bench, &g, &no), (true, false));

        // Times that repeat, and nothing to tell the rows apart.
        let h = stream(
            bench,
            "h",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..4_000).map(|i| Some(t0 + (i / 2) as i64 * Q)).collect()),
                ),
                (
                    "v",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..4_000).map(|i| i as f64))),
                ),
            ],
            1_000,
        );
        assert_eq!(both(bench, &h, &no), (true, true));

        // A first batch whose times do not repeat, before batches whose times do, cut by a key:
        // the first batch keeps one series and no distinct counts, so the key is chosen from
        // counts read from the table, and the pass is read.
        let n = 220_000usize;
        let at = |i: usize| {
            if i < 20_000 {
                i
            } else {
                20_000 + (i - 20_000) / 10
            }
        };
        let i = stream(
            bench,
            "i",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + at(i) as i64 * Q)).collect()),
                ),
                (
                    "meter",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("m{}", i % 10)),
                    )),
                ),
                (
                    "kw",
                    "float64",
                    Arc::new(Float64Array::from_iter_values(
                        (0..n).map(|i| (i % 7) as f64),
                    )),
                ),
            ],
            // Batches of fewer than 16,384 rows are gathered before they go in.
            20_000,
        );
        assert_eq!(both(bench, &i, &no), (true, false));
        let read = crate::profile::compute(bench.connection(), "i", &i.columns, &no, None);
        assert!(
            read.unwrap().lines.iter().any(|l| l.contains("meter")),
            "the key was found"
        );

        // One series with runs of 0 and of null lasting days: its runs are kept as its rows go
        // in, and are the runs the query over the table finds.
        let n = 8_640i64;
        let j = stream(
            bench,
            "j",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + i * 60_000_000)).collect()),
                ),
                (
                    "kw",
                    "float64",
                    Arc::new(Float64Array::from_iter((0..n).map(|i| match i {
                        1_000..=3_880 => Some(0.0),
                        5_000..=7_160 => None,
                        _ if i % 97 == 0 => Some(0.0),
                        _ => Some(1.0 + (i % 13) as f64),
                    }))),
                ),
            ],
            20_000,
        );
        crate::profile::RUNS_READ.with(|r| r.set(0));
        let kept =
            crate::profile::compute(bench.connection(), "j", &j.columns, &no, j.seen.clone());
        assert_eq!(
            crate::profile::RUNS_READ.with(|r| r.get()),
            0,
            "runs were read"
        );
        assert!(
            format!("{kept:?}").contains("is 0 from"),
            "the run of 0 is said: {kept:?}"
        );
        assert_eq!(both(bench, &j, &no), (true, true));
    }

    #[test]
    fn a_kept_distinct_count_of_a_tallied_key_is_duckdbs() {
        // A key a tally keeps series for is hashed once per series, not once per row: a value
        // counted twice changes nothing. What is counted must still be what DuckDB counts: a
        // null that began a series is not a value, and the text a null stands for in a key is
        // one; an integer key is counted as an integer; and a tally that gives up within a
        // batch leaves the rest of that batch to be counted row by row.
        let tb = TestBench::new("keys");
        let bench = tb.bench();
        const Q: i64 = 60_000_000;
        let t0 = 1_767_225_600_000_000i64;
        let check = |t: &LoadedTable, key: &str| {
            let seen = t.seen.as_ref().expect("kept");
            let kept = seen.candidate_distinct().expect("counted")[0];
            let duck: i64 = bench
                .connection()
                .query_row(
                    &format!("SELECT approx_count_distinct({key}) FROM {}", t.name),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(kept, duck, "{}", t.name);
            kept
        };

        // Null first, then the text that stands for a null in a key, then one more value.
        let n = 3_000usize;
        let a = stream(
            bench,
            "a",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + (i / 3) as i64 * Q)).collect()),
                ),
                (
                    "k",
                    "string",
                    Arc::new(StringArray::from_iter((0..n).map(|i| match i % 3 {
                        0 => None,
                        1 => Some("\u{1e}"),
                        _ => Some("a"),
                    }))),
                ),
                (
                    "v",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
                ),
            ],
            1_000,
        );
        assert_eq!(check(&a, "k"), 2);

        // An integer key with nulls, in a cycle of 40.
        let n = 8_000usize;
        let b = stream(
            bench,
            "b",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + (i / 40) as i64 * Q)).collect()),
                ),
                (
                    "k",
                    "int32",
                    Arc::new(Int32Array::from_iter((0..n).map(|i| {
                        (i % 40 != 7).then_some((i % 40) as i32 * 1_009 - 20_000)
                    }))),
                ),
                (
                    "v",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
                ),
            ],
            3_000,
        );
        assert!(check(&b, "k") > 30);

        // More keys than a tally keeps: it gives up within the first batch, and the keys after
        // that in the batch, each seen once, are counted all the same.
        let n = 120_000usize;
        let c = stream(
            bench,
            "c",
            vec![
                (
                    "ts",
                    "timestamptz",
                    ts_us((0..n).map(|i| Some(t0 + (i / 60_000) as i64 * Q)).collect()),
                ),
                (
                    "k",
                    "string",
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("k{i}")),
                    )),
                ),
                (
                    "v",
                    "float64",
                    Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
                ),
            ],
            65_536,
        );
        assert!(check(&c, "k") > 50_000);
    }

    #[test]
    fn a_files_profile_is_kept_from_one_scan_and_is_the_profile_of_the_table() {
        // R2-4: a file's table is profiled as a stream's is, from what one Arrow scan of it
        // shows the streams' `Watch`, not from the scan and the pass that each read it back. The
        // profile is the one reading the table gives, byte for byte, from every format: seven
        // meters with a dead one, a late start, a stop into nulls, a pause, an hour all 0, NaN,
        // an integer and a date column.
        let tb = TestBench::new("file-seen");
        let bench = tb.bench();
        let conn = bench.connection();
        conn.execute_batch(
            "CREATE TEMP TABLE fixture AS SELECT \
               TIMESTAMPTZ '2014-03-27 00:00:00+00' + to_minutes(15 * (i // 7)) AS ts, \
               DATE '2014-03-27' + (i // 700)::INTEGER AS day, \
               'm' || (i % 7 + 1) AS meter, \
               (i % 5)::BIGINT AS n, \
               CASE WHEN i % 7 = 0 THEN 0.0 \
                    WHEN i % 7 = 1 AND i < 1000 THEN 0.0 \
                    WHEN i % 7 = 2 AND i > 3500 THEN NULL \
                    WHEN i % 7 = 3 AND i BETWEEN 2100 AND 2900 THEN 0.0 \
                    WHEN i // 7 BETWEEN 200 AND 203 THEN 0.0 \
                    WHEN i = 1234 THEN 'NaN'::DOUBLE \
                    ELSE (i % 96) * 0.5 + 1 END AS kw \
             FROM range(4704) r(i)",
        )
        .unwrap();
        let no = crate::profile::Shape::default();
        for ext in ["parquet", "csv", "ndjson"] {
            let path = tb.1.join(format!("f_{ext}.{ext}"));
            let format = if ext == "ndjson" { "json" } else { ext };
            // DuckDB's JSON reader takes `2014-03-27T00:00:00Z` for a time, not the
            // `2014-03-27 00:00:00+00` it writes.
            let rows = if ext == "ndjson" {
                "(SELECT * REPLACE (strftime(ts, '%Y-%m-%dT%H:%M:%SZ') AS ts) FROM fixture)"
            } else {
                "fixture"
            };
            conn.execute_batch(&format!(
                "COPY {rows} TO {} (FORMAT {format})",
                literal(&path.to_string_lossy())
            ))
            .unwrap();
        }
        let mut tb = tb;
        let bench = tb.0.as_mut().unwrap();
        for ext in ["parquet", "csv", "ndjson"] {
            let path = tb.1.join(format!("f_{ext}.{ext}"));
            let name = format!("f_{ext}");
            crate::profile::SEEN_USED.with(|u| u.set((0, 0)));
            let t = load_file(bench, &name, &path).unwrap();
            let used = crate::profile::SEEN_USED.with(|u| u.get());
            assert_eq!(used, (1, 1), "{ext}: the profile read the table back");
            let mut kept = t.clone();
            kept.seen = watched(bench.connection(), &t.name, &t.columns);
            assert_eq!(both(bench, &kept, &no), (true, true), "{ext}");
        }
    }

    #[test]
    fn a_files_profile_is_kept_beside_its_write() {
        // R3-2: a file's profile is kept on a thread of its own from the statement its table is
        // written from, while the load's one thread writes it, so the watch is off the path to
        // the first `describe`: a staged file's stage, and a Parquet file read a second time.
        // Either way the profile the bench records is the one reading the table gives, byte for
        // byte; and a narrowed body is watched as narrowed, its time window read in the
        // loader's zone, whatever zone a second connection would start in.
        let tb = TestBench::new("beside");
        let bench = tb.bench();
        let conn = bench.connection();
        conn.execute_batch(
            "CREATE TEMP TABLE fixture AS SELECT \
               TIMESTAMPTZ '2014-03-27 00:00:00+00' + to_minutes(15 * (i // 7)) AS ts, \
               'm' || (i % 7 + 1) AS meter, \
               (i % 5)::BIGINT AS n, \
               CASE WHEN i % 7 = 0 THEN 0.0 \
                    WHEN i % 7 = 2 AND i > 3500 THEN NULL \
                    ELSE (i % 96) * 0.5 + 1 END::DOUBLE AS kw \
             FROM range(4704) r(i)",
        )
        .unwrap();
        for ext in ["parquet", "csv", "ndjson"] {
            let path = tb.1.join(format!("b_{ext}.{ext}"));
            let format = if ext == "ndjson" { "json" } else { ext };
            let rows = if ext == "ndjson" {
                "(SELECT * REPLACE (strftime(ts, '%Y-%m-%dT%H:%M:%SZ') AS ts) FROM fixture)"
            } else {
                "fixture"
            };
            conn.execute_batch(&format!(
                "COPY {rows} TO {} (FORMAT {format})",
                literal(&path.to_string_lossy())
            ))
            .unwrap();
        }
        let no = crate::profile::Shape::default();
        let mut tb = tb;
        let bench = tb.0.as_mut().unwrap();
        for ext in ["parquet", "csv", "ndjson"] {
            let path = tb.1.join(format!("b_{ext}.{ext}"));
            let name = format!("b_{ext}");
            WATCHED_FROM.with(|w| w.set((0, 0)));
            crate::profile::SEEN_USED.with(|u| u.set((0, 0)));
            let t = load_file(bench, &name, &path).unwrap();
            assert_eq!(
                WATCHED_FROM.with(|w| w.get()),
                (1, 0),
                "{name}: (beside the write, after it)"
            );
            assert_eq!(
                crate::profile::SEEN_USED.with(|u| u.get()),
                (1, 1),
                "{name}"
            );
            // What the bench recorded, in the table's comment, against reading the table.
            let recorded: String = bench
                .connection()
                .query_row(
                    "SELECT comment FROM duckdb_tables() WHERE table_name = ?",
                    [&name],
                    |r| r.get(0),
                )
                .unwrap();
            let read =
                crate::profile::compute(bench.connection(), &name, &t.columns, &no, None).unwrap();
            assert_eq!(recorded, read.to_comment(), "{name}");
        }

        // A body, narrowed to a window stated without an offset, in a loader whose zone is not
        // the one a fresh connection gets.
        let bench = tb.0.as_mut().unwrap();
        bench
            .connection()
            .execute_batch("SET TimeZone = 'Asia/Kolkata'")
            .unwrap();
        let reader = reader_for(&tb.1.join("b_csv.csv")).unwrap();
        let narrowing = crate::exec::Narrowing {
            time: Some((
                "ts".into(),
                Some("2014-03-27 05:00:00".into()),
                Some("2014-03-28 05:00:00".into()),
            )),
            ..Default::default()
        };
        let hold = Hold {
            declared: &[],
            narrowing: &narrowing,
            max_rows: 1_000_000,
        };
        WATCHED_FROM.with(|w| w.set((0, 0)));
        let (t, read, _) = load_read(
            bench,
            "body",
            &reader,
            crate::canonical::UTC,
            &hold,
            "test",
            0,
            None,
        )
        .map_err(|f| f.detail)
        .unwrap();
        assert_eq!(WATCHED_FROM.with(|w| w.get()), (1, 0));
        assert!(t.rows > 0 && t.rows < read, "{} of {read}", t.rows);
        assert_eq!(both(bench, &t, &no), (true, true));
    }

    #[test]
    fn a_file_that_fails_midway_leaves_no_table_and_says_why() {
        // A file whose rows go bad after thousands of good ones fails in DuckDB's own words,
        // whether it is read straight into its table or through the stage, and leaves nothing
        // of it on the bench. Read straight in, the write's failure aborted its transaction, and
        // giving the loader its threads back then failed too, "Current transaction is aborted",
        // which was reported instead of why.
        let mut body = String::new();
        for i in 0..60_000 {
            body.push_str(&format!("{{\"i\":{i},\"m\":\"m{}\"}}\n", i % 7));
        }
        body.push_str("{\"i\": 60000, \"m\": oops}\n");
        for straight in [true, false] {
            let mut tb = TestBench::new("midway");
            let path = tb.1.join("t.ndjson");
            std::fs::write(&path, &body).unwrap();
            let bench = tb.0.as_mut().unwrap();
            // Every JSON file is staged since R4-2; read straight in, as a Parquet file is.
            let failed = if straight {
                let reader = reader_for(&path).unwrap();
                match read_into(
                    bench.connection(),
                    "t",
                    &reader,
                    crate::canonical::UTC,
                    None,
                ) {
                    Err(Staged::Reader(e)) => e.to_string(),
                    _ => panic!("read straight in, the file did not fail"),
                }
            } else {
                load_file(bench, "t", &path).unwrap_err().to_string()
            };
            assert!(failed.contains("Malformed JSON"), "{failed}");
            let tables: i64 = bench
                .connection()
                .query_row(
                    "SELECT count(*) FROM duckdb_tables() WHERE table_name = 't'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(tables, 0);
            // The loader keeps its threads for the next table.
            let threads: i64 = bench
                .connection()
                .query_row("SELECT current_setting('threads')", [], |r| r.get(0))
                .unwrap();
            assert!(threads > 1 || std::thread::available_parallelism().unwrap().get() == 1);
        }
    }

    fn decl(cols: &[(&str, &str)]) -> Vec<crate::exec::FieldDecl> {
        cols.iter()
            .map(|(n, t)| crate::exec::FieldDecl {
                name: n.to_string(),
                ty: t.to_string(),
                nullable: true,
            })
            .collect()
    }

    #[test]
    fn a_declared_file_loads_as_the_sniffed_file_does() {
        // R3-5: a file source may be declared, as the operator's choice: a CSV or TSV is read in
        // its stated dialect with the declared types, its columns matched by the header's names
        // in any order; an NDJSON file with the declared `columns`, not sniffed; a Parquet file
        // pinned. Declared as the sniffer types them, each makes the table the sniffed read
        // makes, in a zone other than UTC (REQ-15). Only declared columns load.
        let zone = "Europe/Copenhagen";
        let mut csv = String::from("kw;ts;meter;note\n");
        let mut json = String::new();
        for i in 0..3000 {
            let ts = format!("2026-03-29 0{}:{:02}:00", (i / 60) % 4, i % 60);
            let kw = if i % 11 == 0 {
                String::new()
            } else {
                format!("{}.5", i % 97)
            };
            csv.push_str(&format!("{kw};{ts};m{};x\n", i % 7));
            let kwj = if kw.is_empty() { "null".into() } else { kw };
            json.push_str(&format!(
                "{{\"ts\":\"{ts}\",\"meter\":\"m{}\",\"kw\":{kwj}}}\n",
                i % 7
            ));
        }
        let declared = decl(&[("ts", "timestamp"), ("meter", "string"), ("kw", "float64")]);
        let mut tb = TestBench::new("declared-file");
        std::fs::write(tb.1.join("t.csv"), &csv).unwrap();
        std::fs::write(tb.1.join("t.tsv"), csv.replace(';', "\t")).unwrap();
        std::fs::write(tb.1.join("t.ndjson"), &json).unwrap();
        let bench = tb.0.as_mut().unwrap();
        bench
            .connection()
            .execute_batch(&format!(
                "COPY (FROM read_json_auto({})) TO {} (FORMAT parquet)",
                literal(&tb.1.join("t.ndjson").to_string_lossy()),
                literal(&tb.1.join("t.parquet").to_string_lossy())
            ))
            .unwrap();
        for (file, delimiter) in [
            ("t.csv", Some(';')),
            ("t.tsv", None),
            ("t.ndjson", None),
            ("t.parquet", None),
        ] {
            let path = tb.1.join(file);
            let what = file.to_string();
            load_file_in(bench, "s", &path, zone).unwrap();
            let t = load_file_declared(bench, "t", &path, zone, &declared, delimiter)
                .unwrap_or_else(|e| panic!("{what}: {e}"));
            let conn = bench.connection();
            let sniffed: Vec<(String, String)> = columns_of(conn, "s")
                .unwrap()
                .into_iter()
                .filter(|(c, _)| c != "note")
                .collect();
            let mut want = sniffed.clone();
            want.sort_by_key(|(c, _)| ["ts", "meter", "kw"].iter().position(|x| x == c));
            assert_eq!(t.columns, want, "{what}");
            let differ: i64 = conn
                .query_row(
                    "SELECT count(*) FROM ((SELECT ts, meter, kw FROM t EXCEPT ALL \
                     SELECT ts, meter, kw FROM s) UNION ALL (SELECT ts, meter, kw FROM s \
                     EXCEPT ALL SELECT ts, meter, kw FROM t))",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(differ, 0, "{what}");
            conn.execute_batch("DROP TABLE s; DROP TABLE t").unwrap();
        }
    }

    #[test]
    fn a_declared_file_that_does_not_match_fails_naming_why() {
        // A declared file in another shape fails loudly, never a column of nulls or of another
        // column's values: a CSV split on another delimiter names `delimiter`; a declared column
        // the file lacks, a value that is not its type, a type the Parquet file does not hold,
        // and a null in a column declared `nullable = false` each name the column.
        let mut tb = TestBench::new("declared-bad");
        std::fs::write(tb.1.join("t.csv"), "ts,meter\n2026-01-01 00:00:00,m1\n").unwrap();
        std::fs::write(
            tb.1.join("t.ndjson"),
            "{\"ts\":\"2026-01-01 00:00:00\",\"kw\":\"high\"}\n",
        )
        .unwrap();
        let bench = tb.0.as_mut().unwrap();
        bench
            .connection()
            .execute_batch(&format!(
                "COPY (SELECT 1::INTEGER AS n, NULL::VARCHAR AS s) TO {} (FORMAT parquet)",
                literal(&tb.1.join("t.parquet").to_string_lossy())
            ))
            .unwrap();
        let ts_meter = decl(&[("ts", "timestamp"), ("meter", "string")]);
        let mut strict = decl(&[("s", "string")]);
        strict[0].nullable = false;
        // (file, declaration, delimiter, what the refusal says)
        type Case<'a> = (
            &'a str,
            Vec<crate::exec::FieldDecl>,
            Option<char>,
            &'a [&'a str],
        );
        let cases: Vec<Case> = vec![
            ("t.csv", ts_meter.clone(), Some(';'), &["delimiter"]),
            (
                "t.csv",
                decl(&[("ts", "timestamp"), ("site", "string")]),
                None,
                &["site"],
            ),
            (
                "t.ndjson",
                decl(&[("ts", "timestamp"), ("kw", "float64")]),
                None,
                &["high"],
            ),
            ("t.parquet", decl(&[("n", "string")]), None, &["`n`"]),
            ("t.parquet", strict, None, &["`s`", "nullable"]),
        ];
        for (file, declared, delimiter, says) in cases {
            let path = tb.1.join(file);
            let e = load_file_declared(bench, "t", &path, "UTC", &declared, delimiter)
                .unwrap_err()
                .to_string();
            for s in says {
                assert!(e.contains(s), "{file}: {e}");
            }
            let left: i64 = bench
                .connection()
                .query_row(
                    "SELECT count(*) FROM duckdb_tables() WHERE table_name = 't'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(left, 0, "{file}");
        }
    }

    #[test]
    fn a_json_files_timestamps_read_as_text_load_as_the_sniffer_reads_them() {
        // R3-1: a JSON file's timestamp columns are read as text and cast, which is a third of
        // the read's instructions cheaper than the format the sniffer found. The table is the
        // one the sniffed read makes, value for value and type for type, or the file is read the
        // sniffed way. Each file has 20,600 rows, past the sniffer's 20,480-object sample, and
        // its last value is the odd one out, at the first row of a vector and inside one, since
        // the sniffed read probes a format per vector. Staged, as every JSON file is (R4-2), in a
        // zone other than UTC.
        use Read::*;
        #[derive(Debug, PartialEq)]
        enum Read {
            /// Read as text: the sniffed read's table.
            Text,
            /// Read as text where the sniffed read refuses the file: this instant.
            Loads(&'static str),
            /// The text refused, and read the sniffed way: its table, or its refusal.
            Sniffed,
        }
        let iso = |i: usize| format!("2026-01-01T{:02}:{:02}:00.250000Z", (i / 60) % 24, i % 60);
        // (what, each row's `ts`, the odd value at the end, how the file is read)
        type Rows = Box<dyn Fn(usize) -> String>;
        let cases: Vec<(&str, Rows, Option<&str>, Read)> = vec![
            ("iso, Z, microseconds", Box::new(iso), None, Text),
            (
                "iso, Z, whole seconds",
                Box::new(|i| format!("2026-03-29T01:{:02}:{:02}Z", (i / 60) % 60, i % 60)),
                None,
                Text,
            ),
            (
                "space, no zone",
                Box::new(|i| format!("2026-01-01 00:{:02}:{:02}", (i / 60) % 60, i % 60)),
                None,
                Text,
            ),
            (
                "space, fractions of 2 digits",
                Box::new(|i| format!("2026-01-01 00:00:{:02}.{}", i % 60, 10 + i % 90)),
                None,
                Text,
            ),
            (
                "T, no zone",
                Box::new(|i| format!("2026-01-01T00:{:02}:{:02}", (i / 60) % 60, i % 60)),
                None,
                Text,
            ),
            (
                "offsets",
                Box::new(|i| format!("2026-01-01T00:00:{:02}+02:00", i % 60)),
                None,
                Text,
            ),
            (
                "offsets with fractions",
                Box::new(|i| {
                    let o = ["+0530", "-0100", "+00:00"][i % 3];
                    format!("2026-01-01T00:00:{:02}.5{o}", i % 60)
                }),
                None,
                Text,
            ),
            ("a null", Box::new(iso), Some("null"), Text),
            (
                "a space and a fraction",
                Box::new(iso),
                Some("\"2026-01-01 00:00:00.5\""),
                Text,
            ),
            // The sniffed read refuses this, its vector's formats ending at `.%fZ`. A zone-less
            // time, and one converted to UTC by its offset, is read in the source's zone, as the
            // sniffed read reads every one.
            (
                "an offset",
                Box::new(iso),
                Some("\"2026-01-01T00:00:00+02:00\""),
                Loads("2025-12-31 21:00:00+00"),
            ),
            // The sniffed read refuses each of these; read as text, each is refused too.
            ("not a time", Box::new(iso), Some("\"not a time\""), Sniffed),
            ("an empty string", Box::new(iso), Some("\"\""), Sniffed),
            ("a number", Box::new(iso), Some("5"), Sniffed),
            (
                "nanoseconds",
                Box::new(iso),
                Some("\"2026-01-01T00:00:00.123456789Z\""),
                Sniffed,
            ),
            (
                "seven digits",
                Box::new(iso),
                Some("\"2026-01-01 00:00:00.1234567\""),
                Sniffed,
            ),
            (
                "a date alone",
                Box::new(iso),
                Some("\"2026-01-01\""),
                Sniffed,
            ),
            // The sniffed read takes each of these, and not as the cast does.
            ("epoch", Box::new(iso), Some("\"epoch\""), Sniffed),
            (
                "infinity, padded",
                Box::new(iso),
                Some("\"   infinity          \""),
                Sniffed,
            ),
            (
                "a two-digit year",
                Box::new(iso),
                Some("\"26-01-02 03:04:05\""),
                Sniffed,
            ),
            (
                "12-hour",
                Box::new(iso),
                Some("\"01-02-2026 03:04:05 PM\""),
                Sniffed,
            ),
            (
                "two-digit years throughout",
                Box::new(|i| format!("26-01-02 03:{:02}:{:02}", (i / 60) % 60, i % 60)),
                None,
                Sniffed,
            ),
            (
                "month first, 12-hour, throughout",
                Box::new(|i| format!("01-02-2026 03:{:02}:{:02} PM", (i / 60) % 60, i % 60)),
                None,
                Sniffed,
            ),
        ];
        let rows = 20_600;
        let zone = "Europe/Copenhagen";
        for (what, ts, odd, read) in &cases {
            for at in [rows - 1, rows - 1 - 7] {
                let mut body = String::new();
                for i in 0..rows {
                    let v = match odd {
                        Some(o) if i == at => o.to_string(),
                        _ => format!("\"{}\"", ts(i)),
                    };
                    body.push_str(&format!(
                        "{{\"i\":{i},\"ts\":{v},\"kw\":{}.5,\"m\":\"m{}\"}}\n",
                        i % 1000,
                        i % 7
                    ));
                }
                {
                    let mut tb = TestBench::new("text-stamps");
                    let path = tb.1.join("t.ndjson");
                    std::fs::write(&path, &body).unwrap();
                    let bench = tb.0.as_mut().unwrap();
                    let reader = reader_for(&path).unwrap();
                    let conn = bench.connection();
                    let sniffed = stage_and_write(conn, "s", &reader, zone, None, None).is_ok();
                    TEXT_STAMPS.with(|u| u.set((0, 0)));
                    let loaded = load_file_in(bench, "t", &path, zone);
                    let used = TEXT_STAMPS.with(|u| u.get());
                    let case = format!("{what}, odd row {at}");
                    assert_eq!(used.0, 1, "{case}: the timestamps were not read as text");
                    assert_eq!(used.1 == 1, *read == Sniffed, "{case}: {used:?}");
                    let conn = bench.connection();
                    if let Loads(instant) = read {
                        assert!(!sniffed, "{case}: the sniffed read took it");
                        assert!(loaded.is_ok(), "{case}: {loaded:?}");
                        let at_instant: bool = conn
                            .query_row(
                                &format!("SELECT ts = ?::TIMESTAMPTZ FROM t WHERE i = {at}"),
                                [instant],
                                |r| r.get(0),
                            )
                            .unwrap();
                        assert!(at_instant, "{case}");
                        continue;
                    }
                    assert_eq!(sniffed, loaded.is_ok(), "{case}: {loaded:?}");
                    if !sniffed {
                        continue;
                    }
                    assert_eq!(
                        columns_of(conn, "t").unwrap(),
                        columns_of(conn, "s").unwrap()
                    );
                    let differ: i64 = conn
                        .query_row(
                            "SELECT count(*) FROM t FULL JOIN s USING (i) \
                             WHERE t.ts IS DISTINCT FROM s.ts OR t.kw IS DISTINCT FROM s.kw \
                             OR t.m IS DISTINCT FROM s.m OR t.i IS NULL OR s.i IS NULL",
                            [],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(differ, 0, "{case}");
                }
            }
        }
    }

    #[test]
    fn an_ndjson_bodys_timestamps_read_as_text_load_as_the_sniffer_reads_them() {
        // R3-1 for an HTTP NDJSON body, spooled to a file and read as that file is (R3-3),
        // held to its narrowing: its timestamps are read as text, and the table is the sniffed
        // read's; an `epoch` past the sample has it read the sniffed way, to the same table.
        let none = crate::exec::Narrowing::default();
        let narrowed = crate::exec::Narrowing {
            entities: Some(("m".into(), vec!["m1".into(), "m3".into()])),
            ..Default::default()
        };
        for (odd, read_as_text) in [(None, true), (Some("\"epoch\""), false)] {
            for narrowing in [&none, &narrowed] {
                let mut body = String::new();
                for i in 0..30_000 {
                    let ts = match odd {
                        Some(o) if i == 29_999 => o.to_string(),
                        _ => format!("\"2026-01-01T00:{:02}:{:02}Z\"", (i / 60) % 60, i % 60),
                    };
                    body.push_str(&format!("{{\"i\":{i},\"ts\":{ts},\"m\":\"m{}\"}}\n", i % 7));
                }
                let tb = TestBench::new("text-stamps-body");
                let path = tb.1.join("http-body.ndjson");
                std::fs::write(&path, &body).unwrap();
                let bench = tb.bench();
                let reader = reader_for(&path).unwrap();
                let hold = Hold {
                    declared: &[],
                    narrowing,
                    max_rows: u64::MAX,
                };
                let conn = bench.connection();
                stage_and_write(conn, "s", &reader, "UTC", Some(&hold), None)
                    .map_err(|_| "the sniffed read failed")
                    .unwrap();
                TEXT_STAMPS.with(|u| u.set((0, 0)));
                let bytes = body.len() as u64;
                load_read(bench, "t", &reader, "UTC", &hold, "test", bytes, None).unwrap();
                let used = TEXT_STAMPS.with(|u| u.get());
                assert_eq!(used, (1, usize::from(!read_as_text)), "{odd:?}");
                assert_eq!(
                    columns_of(conn, "t").unwrap(),
                    columns_of(conn, "s").unwrap()
                );
                let differ: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM ((FROM t EXCEPT ALL FROM s) UNION ALL \
                         (FROM s EXCEPT ALL FROM t))",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(differ, 0, "{odd:?}");
            }
        }
    }

    #[test]
    fn a_json_file_whose_columns_the_text_read_would_change_is_read_as_it_was() {
        // R3-1 reads a file with the columns its sniffer found, and that read differs from the
        // sniffed one in more than the timestamps' formats. Each of these files would load
        // otherwise read that way — or load where the sniffed read refuses it — so it is read as
        // it was, once: the same table, or the same refusal.
        let iso = "\"2026-01-01T00:00:00Z\"";
        let cases: Vec<(&str, &str, String)> = vec![
            (
                "no timestamp",
                "t.ndjson",
                "{\"i\":1,\"m\":\"m1\"}\n".repeat(30_000),
            ),
            // A date is cast, not read by format: `26-01-02` the year 26, not 2026.
            (
                "a date beside",
                "t.ndjson",
                format!("{{\"ts\":{iso},\"d\":\"26-01-02\"}}\n").repeat(30_000),
            ),
            // A key a struct did not have in the sample is dropped, not refused.
            ("a struct beside", "t.ndjson", {
                let mut b = format!("{{\"ts\":{iso},\"o\":{{\"a\":1}}}}\n").repeat(30_000);
                b.push_str(&format!("{{\"ts\":{iso},\"o\":{{\"a\":1,\"b\":2}}}}\n"));
                b
            }),
            // A list's timestamps are cast, and the cast to `TIMESTAMP` drops an offset.
            (
                "timestamps in a list",
                "t.ndjson",
                format!("{{\"ts\":{iso},\"l\":[\"2026-01-01T00:00:00+02:00\"]}}\n").repeat(30_000),
            ),
            // `Id` is renamed `Id_1`, and read by that name it is not there.
            (
                "two keys differing in case",
                "t.ndjson",
                format!("{{\"ts\":{iso},\"id\":1,\"Id\":2}}\n").repeat(30_000),
            ),
            // Not newline-delimited: its format is the sniffer's to find.
            ("a .json file", "t.json", {
                let rows = vec![format!("{{\"ts\":{iso}}}"); 30_000];
                format!("[{}]", rows.join(","))
            }),
            // Too small for the `DESCRIBE` to pay.
            (
                "under a mebibyte",
                "t.ndjson",
                format!("{{\"ts\":{iso}}}\n").repeat(1_000),
            ),
        ];
        for (what, file, body) in cases {
            let mut tb = TestBench::new("as-it-was");
            let path = tb.1.join(file);
            std::fs::write(&path, body).unwrap();
            let bench = tb.0.as_mut().unwrap();
            let conn = bench.connection();
            let reader = reader_for(&path).unwrap();
            let sniffed = stage_and_write(conn, "s", &reader, crate::canonical::UTC, None, None);
            TEXT_STAMPS.with(|u| u.set((0, 0)));
            let loaded = load_file(bench, "t", &path);
            assert_eq!(TEXT_STAMPS.with(|u| u.get()), (0, 0), "{what}");
            assert_eq!(sniffed.is_ok(), loaded.is_ok(), "{what}: {loaded:?}");
            if loaded.is_err() {
                continue;
            }
            let conn = bench.connection();
            assert_eq!(
                columns_of(conn, "t").unwrap(),
                columns_of(conn, "s").unwrap()
            );
            let differ: i64 = conn
                .query_row(
                    "SELECT count(*) FROM ((FROM t EXCEPT ALL FROM s) UNION ALL \
                     (FROM s EXCEPT ALL FROM t))",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(differ, 0, "{what}");
        }
    }

    #[test]
    fn a_json_file_is_staged_whatever_its_size() {
        // R4-2: a JSON or NDJSON file is staged however large it is, parsed on every thread,
        // and watched beside the write. Read straight in, a 10,000,000-row NDJSON file (633 MB)
        // was parsed on the write's one thread: 6.63 s to its first `describe`, against 4.39 s
        // for the same rows as a staged CSV. Its stage is held to `STAGE_MEMORY` as a CSV's is.
        // Here a few rows padded past 64 MiB with blank lines, the bound R2-1 staged up to.
        let mut body = String::new();
        for i in 0..100 {
            body.push_str(&format!(
                "{{\"ts\":\"2026-01-01T00:{:02}:00Z\",\"kw\":{i}.5}}\n",
                i % 60
            ));
        }
        body.push_str(&"\n".repeat(65 << 20));
        for file in ["t.ndjson", "t.json"] {
            let mut tb = TestBench::new("large-json");
            let path = tb.1.join(file);
            std::fs::write(&path, &body).unwrap();
            let bench = tb.0.as_mut().unwrap();
            WATCHED_FROM.with(|w| w.set((0, 0)));
            let t = load_file(bench, "t", &path).unwrap();
            assert_eq!(t.rows, 100, "{file}");
            assert_eq!(
                WATCHED_FROM.with(|w| w.get()),
                (1, 0),
                "{file}: (watched beside a staged write, scanned after a straight one)"
            );
        }
    }

    #[test]
    fn a_spooled_body_is_staged_or_read_straight_in_as_its_file_is() {
        // R3-3: an HTTP CSV or NDJSON body is a file in the staging directory once spooled, and
        // goes the way the same file goes: staged, as every CSV and JSON file is (R4-2), or, a
        // Parquet body, read straight in; narrowed, it is staged, to be charged the rows the
        // reader produced.
        let none = crate::exec::Narrowing::default();
        let narrowed = crate::exec::Narrowing {
            entities: Some(("meter".into(), vec!["MT_001".into()])),
            ..Default::default()
        };
        let hold = |narrowing| Hold {
            declared: &[],
            narrowing,
            max_rows: u64::MAX,
        };
        let ndjson = "read_json_auto('http-body.ndjson', format = 'newline_delimited')";
        assert_eq!(
            reads_twice(ndjson, Some(&hold(&none))),
            reads_twice(ndjson, None)
        );
        assert!(!reads_twice(ndjson, Some(&hold(&narrowed))));
        assert!(!reads_twice(
            "read_csv_auto('http-body.csv')",
            Some(&hold(&none))
        ));
        let parquet = "read_parquet('http-body.parquet')";
        assert!(reads_twice(parquet, Some(&hold(&none))));
        assert!(!reads_twice(parquet, Some(&hold(&narrowed))));
    }

    #[test]
    fn a_files_layout_is_the_same_on_every_load() {
        // REQ-15: the same rows make the same file, segment for segment, every time. DuckDB 1.5.5
        // chooses between dictionary and FSST for a text segment by estimating FSST's size from a
        // random quarter of its vectors (src/storage/compression/fsst.cpp, `ANALYSIS_SAMPLE_SIZE`,
        // a `RandomEngine` seeded from `getrandom`), so at a near tie the choice changed from one
        // load to the next: 1 load in 30 of this fixture's `m`, measured. Written in storage
        // version v1.3.0 or later, text is DICT_FSST, chosen without sampling.
        let mut body = String::new();
        for i in 0..300_000 {
            body.push_str(&format!(
                "{{\"ts\":\"2026-01-01T{:02}:{:02}:00Z\",\"m\":\"m{}\",\"kw\":{}.5}}\n",
                (i / 60) % 24,
                i % 60,
                i % 7,
                i % 1000
            ));
        }
        let layout = || {
            let mut tb = TestBench::new("same-layout");
            let path = tb.1.join("t.ndjson");
            std::fs::write(&path, &body).unwrap();
            let bench = tb.0.as_mut().unwrap();
            load_file(bench, "t", &path).unwrap();
            let conn = bench.connection();
            conn.execute_batch("CHECKPOINT").unwrap();
            // With the blocks each segment lies in, and the file's size: a parallel write
            // makes the same segments every time, in blocks taken in the order its threads
            // finish (docs/PERFORMANCE.md, R4-1).
            conn.prepare(
                "SELECT row_group_id || ' ' || column_name || ' ' || segment_type || ' ' || \
                 count || ' ' || compression || ' ' || block_id || ' ' || block_offset \
                 FROM pragma_storage_info('t') \
                 UNION ALL SELECT 'size ' || total_blocks || ' ' || free_blocks \
                 FROM pragma_database_size() ORDER BY ALL",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        let first = layout();
        let sampled: Vec<&String> = first
            .iter()
            .filter(|s| s.contains(" Dictionary ") || s.contains(" FSST "))
            .collect();
        assert!(sampled.is_empty(), "chosen by a random sample: {sampled:?}");
        for _ in 0..10 {
            assert_eq!(layout(), first);
        }
    }

    #[test]
    fn a_memory_limit_put_back_reads_as_it_did() {
        // The test below compares the limit before and after a stage, and so depends on the
        // machine: DuckDB's default is a share of its memory, and only some limits read back as
        // they were set. These do not, on any machine.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let read = || -> String {
            conn.query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))
                .unwrap()
        };
        for set in ["12.7 GiB", "3.1 GiB", "953.6 MiB", "6.0 GiB"] {
            conn.execute_batch(&format!("SET memory_limit = '{set}'"))
                .unwrap();
            let was = read();
            conn.execute_batch("SET memory_limit = '1MiB'").unwrap();
            put_back_memory_limit(&conn, &was).unwrap();
            assert_eq!(read(), was, "{set}");
            put_back_memory_limit(&conn, &was).unwrap();
            assert_eq!(read(), was, "{set}, again");
        }
        // and the buffer pool has it too, not only the setting
        conn.execute_batch(
            "SELECT count(*) FROM (SELECT i::VARCHAR AS s FROM range(3000000) t(i) ORDER BY s)",
        )
        .unwrap();
    }

    #[test]
    fn a_table_read_on_one_thread_or_staged_is_the_same_file() {
        // A CSV's stage is held to `STAGE_MEMORY` and spills past it. Rows too wide for a stage
        // on every thread are read on one instead, straight into the table, or through a stage
        // filled on one thread where a narrowing has to count them first; a stage that runs out
        // of memory is read on one thread too, and one that runs out there as well, without the
        // cap. Every way the table is the same file (REQ-15
        // `the_physical_layout_says_nothing_of_where_data_came_from`), and the bench's memory
        // limit is what it was.
        let mut body = String::from("ts,m,kw\n");
        for i in 0..300_000 {
            body.push_str(&format!(
                "2026-01-01 {:02}:{:02}:00,m{},{}.5\n",
                (i / 60) % 24,
                i % 60,
                i % 7,
                i % 1000
            ));
        }
        let all = crate::exec::Narrowing {
            entities: Some(("m".into(), (0..7).map(|i| format!("m{i}")).collect())),
            ..Default::default()
        };
        let layout = |cap: &str, wide: bool, narrowed: bool| {
            let tb = TestBench::new("capped");
            let path = tb.1.join("t.csv");
            std::fs::write(&path, &body).unwrap();
            let bench = tb.bench();
            let conn = bench.connection();
            let setting = |s: &str| -> String {
                conn.query_row(
                    &format!("SELECT current_setting('{s}')::VARCHAR"),
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            };
            let before = (setting("memory_limit"), setting("threads"));
            let reader = reader_for(&path).unwrap();
            let hold = Hold {
                declared: &[],
                narrowing: &all,
                max_rows: u64::MAX,
            };
            let hold = narrowed.then_some(&hold);
            TRIED.with(|t| t.borrow_mut().clear());
            let case = format!("{cap}, wide {wide}, narrowed {narrowed}");
            if let Err(Staged::Reader(e)) = staged_within(
                conn,
                "t",
                &reader,
                crate::canonical::UTC,
                hold,
                wide,
                cap,
                cap,
            ) {
                panic!("{case}: {e}");
            }
            assert_eq!(
                (setting("memory_limit"), setting("threads")),
                before,
                "{case}"
            );
            conn.execute_batch("CHECKPOINT").unwrap();
            let size: (i64, i64, i64) = conn
                .query_row(
                    "SELECT total_blocks, used_blocks, free_blocks FROM pragma_database_size()",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            let segments: Vec<String> = conn
                .prepare(
                    "SELECT row_group_id || ' ' || column_name || ' ' || segment_type || ' ' || \
                     count || ' ' || compression || ' ' || stats FROM pragma_storage_info('t') \
                     ORDER BY ALL",
                )
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            ((size, segments), TRIED.with(|t| t.borrow().clone()))
        };
        let (uncapped, tried) = layout("100GiB", false, false);
        assert!(uncapped.1.len() > 6, "{:?}", uncapped.1);
        assert_eq!(tried, ["staged"]);
        assert_eq!(
            layout(STAGE_MEMORY, false, false),
            (uncapped.clone(), vec!["staged".into()])
        );
        // Wide: straight in, under the cap; narrowed, through a stage on one thread.
        assert_eq!(
            layout(STAGE_MEMORY, true, false),
            (uncapped.clone(), vec!["straight in".into()])
        );
        assert_eq!(
            layout(STAGE_MEMORY, true, true),
            (uncapped.clone(), vec!["staged on one".into()])
        );
        assert_eq!(layout("100GiB", false, true).0, uncapped);
        // Too small for anything: out of memory staged, then on one thread, then read on one
        // thread without the cap.
        assert_eq!(
            layout("1MiB", false, false),
            (
                uncapped.clone(),
                ["staged", "straight in", "straight in, uncapped"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert_eq!(
            layout("1MiB", false, true),
            (
                uncapped,
                ["staged", "staged on one", "staged on one, uncapped"]
                    .map(String::from)
                    .to_vec()
            )
        );
    }

    #[test]
    fn a_wide_json_file_is_read_on_one_thread_within_its_vector() {
        // A JSON file of wide objects is read on one thread at once, and its stage held to
        // `STAGE_MEMORY` and what the JSON reader holds of a vector of its lines
        // (`Width::json_vector`): under `STAGE_MEMORY` alone, this file's stage ran out of memory
        // on one thread too, and was read again without the cap. 2,100 objects of 2,000 keys,
        // one vector and a little, which DuckDB reads as one `MAP`.
        let tb = TestBench::new("wide-json");
        let path = tb.1.join("t.ndjson");
        let mut body = String::new();
        for i in 0..2_100 {
            let keys: Vec<String> = (0..2000)
                .map(|j| format!("\"k{j}\":{}", (i + j) % 10))
                .collect();
            body.push_str(&format!("{{{}}}\n", keys.join(",")));
        }
        std::fs::write(&path, &body).unwrap();
        let bench = tb.bench();
        let conn = bench.connection();
        conn.execute_batch("SET threads = 4").unwrap();
        let reader = reader_for(&path).unwrap();
        TRIED.with(|t| t.borrow_mut().clear());
        if let Err(Staged::Reader(e)) =
            stage_and_write(conn, "t", &reader, crate::canonical::UTC, None, Some(&path))
        {
            panic!("{e}");
        }
        assert_eq!(TRIED.with(|t| t.borrow().clone()), ["staged on one"]);
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 2_100);
    }

    #[test]
    fn a_file_is_read_on_one_thread_where_its_first_line_is_wide() {
        // `Width` judges a file by its first 64 KiB, so that rows too wide for a stage on
        // every thread are read on one from the start, rather than after a stage that ran out of
        // memory (`a_wide_file_is_not_held_in_memory_while_it_is_staged`).
        let dir = std::env::temp_dir().join(format!("benchy-wide-rows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, body: String| {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            path
        };
        let csv = |columns: usize| {
            let line = |v: &str| vec![v; columns].join(",") + "\n";
            file(
                &format!("c{columns}.csv"),
                line("c") + &line("123.45").repeat(1000),
            )
        };
        let ndjson = |keys: usize| {
            let line = (0..keys)
                .map(|k| format!("\"k{k}\": 123.45"))
                .collect::<Vec<_>>()
                .join(", ");
            file(
                &format!("k{keys}.ndjson"),
                format!("{{{line}}}\n").repeat(10),
            )
        };
        let harness = file(
            "readings.csv",
            "ts,meter,kw\n".to_string() + &"2026-01-01 00:00:00,m001,1.5\n".repeat(5000),
        );
        let harness_json = file(
            "readings.ndjson",
            "{\"ts\":\"2026-01-01T00:00:00.000000Z\",\"meter\":\"m001\",\"kw\":1.5}\n".repeat(5000),
        );
        for (path, threads, wide) in [
            (&harness, 4, false),
            (&harness_json, 4, false),
            (&harness, 64, false),
            (&csv(30), 4, false),
            (&csv(100), 4, true),
            (&csv(300), 4, true),
            (&csv(300), 2, true),
            (&ndjson(30), 4, false),
            (&ndjson(2000), 4, true),
            (&ndjson(2000), 1, true),
            (&csv(100), 1, false),
            (&dir.join("missing.csv"), 4, false),
        ] {
            let judged = Width::of(path).is_some_and(|w| w.too_wide_for(threads));
            assert_eq!(judged, wide, "{} on {threads}", path.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
