//! REQ-14: the shape of a table, found once, when it is loaded.
//!
//! The eval's step savings all came from discovery (eval/REPORT.md): `describe` was one call
//! where a shell took two or three of looking. What an agent then still had to find out by
//! querying — which column is time, how often a row arrives, which column tells the series apart,
//! which series is dead, starts late or stops, which hour every series shares a fault in — is
//! what this module works out, so that `describe` can say it.
//!
//! **Once, at load.** A loader profiles the table it has just written, with the threads the loader
//! has, and leaves the result in the table's own comment (`COMMENT ON TABLE`). The engine reads
//! that comment when it attaches the file, so `describe` renders stored text and never scans a
//! table (`describe_does_not_scan_the_table`), and a rebuilt engine re-attaches the stored file
//! without profiling again. The comment is the one place that travels with the file and adds no
//! object to it, which the engine's attach check requires (`attached_table`).
//!
//! **Cheap on purpose.** A full window over the table is what a naive profile costs, and on five
//! million rows it took as long as loading them (measured 2.2–5.3 s against a 2.5 s load on the
//! development box). So the profile is built from aggregates: one scan for the columns, then one
//! pass over the rows ([`pass`]) for per-series counts, extents, zero counts, two checksums of
//! the timestamps and whether they came in order, which together prove a series sits on its step
//! with nothing missing or repeated without ordering it — and, in the same pass, each time's
//! totals and the rows that are 0 or null. A series whose checksums fit but whose times came in
//! no order is settled by an exact distinct count of its times; only irregular series are read
//! again, by a bounded window. A `GROUP BY` series and a `GROUP BY`
//! time per measure, with a window over the zero rows, had cost a third as much CPU as the load
//! itself on the meter data; the pass costs a fifth.
//!
//! **Data only.** Nothing here depends on where the table came from: it reads types as "temporal",
//! "numeric" or "text", never a source's own spelling, and the same rows give the same profile
//! (`the_same_data_gives_the_same_shape_from_csv_and_parquet`). What is a guess says so: a column
//! picked by heuristic is called what it "looks like". Units are never guessed.

use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// What the operator states about a source's shape, in its config. Either may be left out, and a
/// heuristic then decides — and says it is a heuristic.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_column: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<String>,
}

/// A table's profile, as stored in its comment and rendered by `describe`. Text, already worded:
/// rendering it is a matter of fitting lines into a budget, never of computing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// The format of this record. A comment written by another version is ignored, not misread.
    pub v: u32,
    /// The table as a whole: time column, cadence, series, columns. Most useful first.
    pub lines: Vec<String>,
    /// Moments every series shares.
    pub events: Vec<String>,
    /// What the per-series notes are about.
    pub notes_header: String,
    /// One line per series with something to note, most notable first.
    pub notes: Vec<String>,
    /// Series with notes beyond those kept.
    pub more_notes: u64,
    /// Series with nothing to note.
    pub quiet: u64,
}

pub const FORMAT: u32 = 1;
/// A comment longer than this is not a profile benchy wrote; it is ignored.
pub const COMMENT_MAX: usize = 64 * 1024;
/// One rendered line at most. Values in a line come from the data, so they are cut too.
const LINE_MAX: usize = 400;
/// Per-series notes kept in the profile; `describe` shows as many as its budget allows.
const NOTES_KEPT: usize = 40;
/// Moments every series shares, kept.
const EVENTS_KEPT: usize = 6;
/// Numeric columns examined for runs of 0 or null and shared moments. Every numeric column gets
/// its range; these few get the per-series work, whose cost grows with each.
const MEASURES_EXAMINED: usize = 4;
/// Columns whose ranges or distinct counts are reported.
const COLUMNS_REPORTED: usize = 24;
/// Rows a bounded window may read: the irregular series, or the rows that are 0 or null.
const WINDOW_ROWS: u64 = 1_000_000;
/// About how many rows the step is measured on.
const STEP_SAMPLE_ROWS: u64 = 50_000;
/// More series than this are summarised rather than examined one by one.
const SERIES_MAX: usize = 200_000;
/// Values longer than this are cut where they appear in a line.
const VALUE_MAX: usize = 64;
const DAY_MS: i64 = 86_400_000;
/// Between the columns of a series key held as one text, and a null column's value in it.
const KEY_SEP: char = '\u{1f}';
const NULL_PART: &str = "\u{1e}";
/// Runs of 0 or null listed per series before a line is elided to fit.
const RUNS_LISTED: usize = 1000;

impl Profile {
    /// For the table's comment.
    pub fn to_comment(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// From a table's comment, or `None` if it is not a profile this version wrote. Bounded: the
    /// file was written by a loader, and the engine checks what it attaches rather than trusting
    /// it (`attached_table`).
    pub fn from_comment(c: &str) -> Option<Profile> {
        if c.is_empty() || c.len() > COMMENT_MAX {
            return None;
        }
        let mut p: Profile = serde_json::from_str(c).ok()?;
        if p.v != FORMAT {
            return None;
        }
        let tidy = |v: &mut Vec<String>, n: usize| {
            v.truncate(n);
            for s in v.iter_mut() {
                *s = line(std::mem::take(s));
            }
        };
        tidy(&mut p.lines, 40);
        tidy(&mut p.events, EVENTS_KEPT + 1);
        tidy(&mut p.notes, NOTES_KEPT);
        p.notes_header = line(std::mem::take(&mut p.notes_header));
        Some(p)
    }

    /// The profile as indented lines, in at most `room` bytes as they go on the wire. Never cut
    /// silently: what does not fit is counted in a last line that says so. `None` when not even
    /// that line fits — the caller then says the shape is not shown.
    pub fn render(&self, room: usize) -> Option<String> {
        let mut items: Vec<String> = self.lines.iter().map(|l| format!("  {l}\n")).collect();
        if !self.events.is_empty() {
            items.push("  at the same moment in every series:\n".into());
            items.extend(self.events.iter().map(|e| format!("    {e}\n")));
        }
        if !self.notes.is_empty() {
            items.push(format!("  {}\n", self.notes_header));
            items.extend(self.notes.iter().map(|n| format!("    {n}\n")));
            if self.more_notes > 0 {
                items.push(format!(
                    "    … and {} more series with notes like these\n",
                    self.more_notes
                ));
            }
            if self.quiet == 1 {
                items.push("    the one other series: none of these\n".into());
            } else if self.quiet > 0 {
                items.push(format!(
                    "    the other {} series: none of these\n",
                    self.quiet
                ));
            }
        }
        let size = |s: &str| crate::result::wire_len(s);
        let total: usize = items.iter().map(|i| size(i)).sum();
        if total <= room {
            return Some(items.concat());
        }
        // The longest the closing line can be, with a host's longer name for `describe`.
        let mark = 90 + crate::tools::longer("describe");
        let mut out = String::new();
        let mut used = 0;
        let mut shown = 0;
        for it in &items {
            if used + size(it) + mark > room {
                break;
            }
            used += size(it);
            out.push_str(it);
            shown += 1;
        }
        if used + mark > room {
            return None;
        }
        out.push_str(&format!(
            "  … {} more line(s) about this table not shown: over the {} budget\n",
            items.len() - shown,
            crate::tools::name("describe")
        ));
        Some(out)
    }
}

/// A stored line, made safe to render: nothing in it can break the line or hide text, and it is
/// at most [`LINE_MAX`] bytes. The values in it were quoted when the loader wrote it; this is the
/// backstop for a comment the engine reads back.
fn line(s: String) -> String {
    crate::result::cut(visible(&s.replace(['\n', '\r'], " ")), LINE_MAX)
}

/// Whether a reader may not see `c` as what it is: a control character (Unicode Cc, NEL among
/// them), a format character (Cf: zero-width ones, bidi overrides and isolates, the BOM, tag
/// characters), or a line or paragraph separator (Zl, Zp). Text after a U+2028 starts a new line
/// for many readers, and an override reorders what follows it.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{ad}'
                | '\u{600}'..='\u{605}'
                | '\u{61c}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{890}'..='\u{891}'
                | '\u{8e2}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0001}'
                | '\u{e0020}'..='\u{e007f}'
        )
}

/// `s` with every [`hidden`] character spelled out as `\u{…}`.
fn visible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if hidden(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// The first `max` bytes of `s`, on a character boundary, and whether anything was left out.
fn head(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

/// REQ-15 `a_data_value_in_describe_is_a_quoted_literal`. A value from the data, as `describe`
/// shows it: a SQL string literal, which is what the agent would type to filter on it, and which
/// keeps a value written as an instruction reading as data. Quotes are doubled, hidden characters
/// spelled out, and a long value is cut at a character boundary with `…` inside the quotes.
pub fn value(s: &str) -> String {
    let (h, cut) = head(s, VALUE_MAX);
    format!(
        "'{}{}'",
        visible(&h.replace('\'', "''")),
        if cut { "…" } else { "" }
    )
}

/// A column or table name as `describe` shows it: bare when it is a plain identifier, otherwise
/// as a quoted identifier the agent can type, with the same care as a [`value`].
pub fn ident(s: &str) -> String {
    let plain = s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        return s.to_string();
    }
    let (h, cut) = head(s, VALUE_MAX);
    format!(
        "\"{}{}\"",
        visible(&h.replace('"', "\"\"")),
        if cut { "…" } else { "" }
    )
}

/// `1 row`, `2 rows`.
fn count(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// `items` joined by `sep` within `max` bytes. What does not fit is left out between whole items
/// — the first ones, then `…`, then the last — and `None` only when not even the first and last
/// fit, which the caller words itself.
fn elide(items: &[String], sep: &str, gap: &str, max: usize) -> Option<String> {
    let all = items.join(sep);
    if all.len() <= max || items.len() < 2 {
        return Some(all);
    }
    let last = &items[items.len() - 1];
    let mut kept = 0;
    let mut used = last.len() + gap.len();
    for it in &items[..items.len() - 1] {
        if used + it.len() + sep.len() > max {
            break;
        }
        used += it.len() + sep.len();
        kept += 1;
    }
    if kept == 0 {
        return None;
    }
    Some(format!("{}{gap}{last}", items[..kept].join(sep)))
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// What kind of column a DuckDB type is, as far as a profile cares. Read from the type alone, so
/// the same data profiles the same whichever reader produced it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Date,
    Timestamp,
    TimestampTz,
    Integer,
    Real,
    Text,
    Other,
}

fn kind(ty: &str) -> Kind {
    let t = ty.to_ascii_uppercase();
    match t.as_str() {
        "DATE" => Kind::Date,
        "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => Kind::TimestampTz,
        _ if t.starts_with("TIMESTAMP") => Kind::Timestamp,
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT" | "USMALLINT"
        | "UINTEGER" | "UBIGINT" | "UHUGEINT" => Kind::Integer,
        "FLOAT" | "DOUBLE" | "REAL" => Kind::Real,
        _ if t.starts_with("DECIMAL") => Kind::Real,
        "VARCHAR" => Kind::Text,
        _ if t.starts_with("ENUM") => Kind::Text,
        _ => Kind::Other,
    }
}

fn has_kind(kinds: &[(String, Kind)], name: &str) -> Option<Kind> {
    kinds.iter().find(|(n, _)| n == name).map(|(_, k)| *k)
}

fn temporal(k: Kind) -> bool {
    matches!(k, Kind::Date | Kind::Timestamp | Kind::TimestampTz)
}

/// A duration, in at most its two largest units: `15 minutes`, `13 days 15 minutes`.
pub fn span(ms: i64) -> String {
    let units = [
        (DAY_MS, "day"),
        (3_600_000, "hour"),
        (60_000, "minute"),
        (1_000, "second"),
        (1, "millisecond"),
    ];
    let mut left = ms.max(0);
    let mut parts = Vec::new();
    for (size, name) in units {
        if parts.len() == 2 {
            break;
        }
        let n = left / size;
        if n > 0 {
            parts.push(format!("{n} {name}{}", if n == 1 { "" } else { "s" }));
            left -= n * size;
        } else if !parts.is_empty() {
            break; // `1 day 0 hours 15 minutes` is noise: stop at the first gap
        }
    }
    if parts.is_empty() {
        "0 seconds".into()
    } else {
        parts.join(" ")
    }
}

/// A number short enough to read and exact enough to filter near.
fn num(x: f64) -> String {
    if !x.is_finite() {
        return x.to_string();
    }
    if x == x.trunc() && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    let a = x.abs();
    if !(1e-4..1e9).contains(&a) {
        return format!("{x:.4e}");
    }
    // six significant digits, trailing zeros dropped
    let decimals = (5 - a.log10().floor() as i32).clamp(0, 10) as usize;
    let mut s = format!("{x:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// How a time column's values are written back to the agent: as DuckDB writes them, so a value
/// read here can be pasted into a filter.
#[derive(Debug, Clone, Copy)]
struct Clock(Kind);

impl Clock {
    fn at(self, ms: i64) -> String {
        let micros = ms.saturating_mul(1000);
        let full = crate::result::timestamp_utc(micros); // `YYYY-MM-DD HH:MM:SS[.f]+00`
        let naive = full.trim_end_matches("+00");
        match self.0 {
            Kind::Date => naive[..10].to_string(),
            Kind::TimestampTz => full,
            _ => naive.to_string(),
        }
    }

    /// `A to B`, with B's date left out when it is A's.
    fn range(self, a: i64, b: i64) -> String {
        if a == b {
            return format!("at {}", self.at(a));
        }
        let (x, y) = (self.at(a), self.at(b));
        if !matches!(self.0, Kind::Date) && x.get(..10) == y.get(..10) {
            format!("at {x} to {}", &y[11..])
        } else {
            format!("at {x} to {y}")
        }
    }
}

/// A day on which clocks change, by the rules most meter data is recorded under, or `None`.
/// A calendar fact, not a guess about the data: the profile reports it beside a moment every
/// series shares, and the agent decides what it means.
fn clock_change(ms: i64) -> Option<&'static str> {
    let days = ms.div_euclid(DAY_MS);
    let (_, m, d) = crate::result::civil_from_days(days);
    // 1970-01-01 was a Thursday.
    let sunday = (days + 4).rem_euclid(7) == 0;
    if !sunday {
        return None;
    }
    match (m, d) {
        (3, 25..=31) => Some("the last Sunday of March, when clocks in the EU go forward an hour"),
        (10, 25..=31) => Some("the last Sunday of October, when clocks in the EU go back an hour"),
        (3, 8..=14) => Some("the second Sunday of March, when clocks in the US go forward an hour"),
        (11, 1..=7) => Some("the first Sunday of November, when clocks in the US go back an hour"),
        _ => None,
    }
}

/// A numeric column, as the first scan saw it.
struct Col {
    name: String,
    min: Option<f64>,
    max: Option<f64>,
    nulls: i64,
    zeros: i64,
}

/// One series, as the `GROUP BY` saw it.
#[derive(Debug, Clone, Default)]
struct Series {
    key: String,
    rows: i64,
    /// Rows with a time.
    timed: i64,
    first: Option<i64>,
    last: Option<i64>,
    /// Where the series sits between two steps of the grid from the table's first time: the
    /// least and the most. A series on its step has one phase.
    phase: (i64, i64),
    /// The sum of the step indices of its times, and of their squares.
    sum: i128,
    sum_sq: i128,
    /// What [`Tally`] keeps to work the three above out once the step is known: the first time
    /// seen, the greatest common divisor of every time's distance from it, and the sums of
    /// those distances and of their squares.
    t0: Option<i64>,
    gcd: u64,
    sum_d: i128,
    sum_d2: i128,
    /// Whether its times came in order, in the order the rows were read: each later than every
    /// one before it, or each earlier. Then no time repeats, which no checksum proves
    /// ([`Series::regular`]). `rising` and `falling` are each way, and `top` and `bottom` the
    /// latest and earliest so far, for [`Series::note`].
    ordered: bool,
    rising: bool,
    falling: bool,
    top: i64,
    bottom: i64,
    measures: Vec<SeriesMeasure>,
    /// Filled in by the checks below.
    notes: Vec<(u64, String)>,
}

#[derive(Debug, Clone, Default)]
struct SeriesMeasure {
    nulls: i64,
    zeros: i64,
    min: Option<f64>,
    max: Option<f64>,
}

impl Series {
    /// Time `t` of one of the series' rows, into what [`Series::place`] works from.
    #[inline]
    fn note(&mut self, t: i64) {
        match self.t0 {
            None => {
                self.t0 = Some(t);
                (self.rising, self.falling) = (true, true);
                (self.top, self.bottom) = (t, t);
                self.ordered = true;
            }
            Some(t0) => {
                self.rising &= t > self.top;
                self.falling &= t < self.bottom;
                self.top = self.top.max(t);
                self.bottom = self.bottom.min(t);
                self.ordered = self.rising || self.falling;
                let d = t.wrapping_sub(t0);
                let u = d.unsigned_abs();
                // Most distances are a multiple of what is already kept: one division.
                if self.gcd == 0 || u % self.gcd != 0 {
                    self.gcd = gcd(self.gcd, u);
                }
                self.sum_d = self.sum_d.wrapping_add(d as i128);
                self.sum_d2 = self
                    .sum_d2
                    .wrapping_add((d as i128).wrapping_mul(d as i128));
            }
        }
    }

    /// The phase and the checksums of the step indices, from what [`Tally`] kept. Every time is
    /// on one phase exactly when `step` divides every distance from the first; then each index
    /// is `(t - o) / step` for `o` the first time's grid point, and its sums follow from the
    /// sums of the distances. Off one phase, only that is said: [`Series::regular`] asks no more.
    fn place(&mut self, step: i64, origin: i64) {
        let Some(t0) = self.t0 else {
            return;
        };
        let ph = t0.wrapping_sub(origin).rem_euclid(step);
        if !self.gcd.is_multiple_of(step as u64) {
            self.phase = (ph, ph.wrapping_add(1));
            return;
        }
        self.phase = (ph, ph);
        let (n, st) = (self.timed as i128, step as i128);
        let c = (t0.wrapping_sub(origin).wrapping_sub(ph)) as i128;
        self.sum = self.sum_d.wrapping_add(n.wrapping_mul(c)) / st;
        self.sum_sq = self
            .sum_d2
            .wrapping_add(2i128.wrapping_mul(c).wrapping_mul(self.sum_d))
            .wrapping_add(n.wrapping_mul(c).wrapping_mul(c))
            / (st * st);
    }

    /// On its step from first to last, nothing missing and nothing repeated — proven, without
    /// ordering the rows, from the count, the extent and the phase, and from its times having
    /// come in order ([`Series::ordered`]): as many distinct indices as the extent holds, each
    /// on the step, are every index once. Times that came in no order may still be regular,
    /// and only an exact count of their distinct times settles it (`settle`).
    fn regular(&self, step: i64, origin: i64) -> bool {
        self.ordered && self.fits(step, origin)
    }

    /// What a row every step from first to last would give: the count, the extent, the phase
    /// and two checksums of the step indices. Necessary, not sufficient. One missing row and one
    /// repeated change the sum or the sum of squares unless they coincide, but three of each need
    /// not: `{0,1,1,4,5,5,6,6,8,9}` has the count, the sum (45) and the sum of squares (285) of
    /// `0..9` (`three_gaps_hidden_by_three_repeats_are_found`). So a series that fits is
    /// regular only once no time is known to repeat. `origin` is the table's first time; the
    /// indices count steps from it.
    fn fits(&self, step: i64, origin: i64) -> bool {
        let (Some(a), Some(b)) = (self.first, self.last) else {
            return false;
        };
        if self.timed != self.rows || step <= 0 || self.phase.0 != self.phase.1 {
            return false;
        }
        if self.rows == 1 {
            return true;
        }
        let (lo, hi) = (((a - origin) / step) as i128, ((b - origin) / step) as i128);
        let n = self.rows as i128;
        if hi - lo + 1 != n {
            return false;
        }
        let squares = |m: i128| m * (m + 1) * (2 * m + 1) / 6;
        self.sum == n * lo + n * (n - 1) / 2 && self.sum_sq == squares(hi) - squares(lo - 1)
    }
}

struct Ctx<'a> {
    conn: &'a Connection,
    from: String,
    /// The time column as epoch milliseconds.
    t: String,
    /// The series key as text, never null, or a constant when there is none.
    k: String,
}

fn rows<T>(
    conn: &Connection,
    sql: &str,
    f: impl FnMut(&duckdb::Row<'_>) -> duckdb::Result<T>,
) -> Result<Vec<T>, String> {
    let mut st = conn.prepare(sql).map_err(|e| e.to_string())?;
    let r = st.query_map([], f).map_err(|e| e.to_string())?;
    r.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

/// One row of numbers, each read as a double: the shape of every scan here.
fn values(conn: &Connection, sql: &str) -> Option<Vec<Option<f64>>> {
    conn.query_row(sql, [], |r| {
        (0..r.as_ref().column_count())
            .map(|i| r.get::<_, Option<f64>>(i))
            .collect::<duckdb::Result<Vec<_>>>()
    })
    .ok()
}

/// Profile table `table`, whose columns are `columns`, as the loader just wrote it.
///
/// `Err` only when the operator stated something the table contradicts: a stated column that is
/// not there, or a time column that is not a date or timestamp. Anything else that goes wrong
/// gives a smaller profile, never a failed load.
///
/// `seen` is what a [`Watch`] kept as the table's rows went in, when they came from a stream: the
/// table is then not read back for what it holds.
pub fn compute(
    conn: &Connection,
    table: &str,
    columns: &[(String, String)],
    shape: &Shape,
    seen: Option<std::sync::Arc<Seen>>,
) -> Result<Profile, String> {
    let has = |c: &str| columns.iter().find(|(n, _)| n == c).map(|(_, t)| kind(t));
    if let Some(t) = &shape.time_column {
        match has(t) {
            None => return Err(format!("the stated time column `{t}` is not in the table")),
            Some(k) if !temporal(k) => {
                return Err(format!(
                    "the stated time column `{t}` is not a date or timestamp"
                ))
            }
            _ => {}
        }
    }
    for s in &shape.series {
        if has(s).is_none() {
            return Err(format!(
                "the stated series column `{s}` is not in the table"
            ));
        }
        if Some(s) == shape.time_column.as_ref() {
            return Err(format!(
                "`{s}` is stated as both the time column and a series column"
            ));
        }
    }
    let seen = seen.map(|s| std::sync::Arc::try_unwrap(s).unwrap_or_else(|s| (*s).clone()));
    Ok(profile(conn, table, columns, shape, seen).unwrap_or_default())
}

/// The profile proper. `None` when a query fails; the load goes on without one.
fn profile(
    conn: &Connection,
    table: &str,
    columns: &[(String, String)],
    shape: &Shape,
    seen: Option<Seen>,
) -> Option<Profile> {
    let mut p = Profile {
        v: FORMAT,
        ..Profile::default()
    };
    let from = quote(table);
    let kinds: Vec<(String, Kind)> = columns.iter().map(|(n, t)| (n.clone(), kind(t))).collect();

    // The time column: stated, or the first temporal one.
    let (time, time_stated) = match &shape.time_column {
        Some(t) => (Some(t.clone()), true),
        None => (
            kinds
                .iter()
                .find(|(_, k)| temporal(*k))
                .map(|(n, _)| n.clone()),
            false,
        ),
    };
    let time_kind = time
        .as_ref()
        .and_then(|t| kinds.iter().find(|(n, _)| n == t).map(|(_, k)| *k));

    let candidates: Vec<&(String, Kind)> = kinds
        .iter()
        .filter(|(n, k)| matches!(k, Kind::Text | Kind::Integer) && Some(n) != time.as_ref())
        .take(8)
        .collect();
    let numeric: Vec<&(String, Kind)> = kinds
        .iter()
        .filter(|(_, k)| matches!(k, Kind::Integer | Kind::Real))
        .take(COLUMNS_REPORTED)
        .collect();

    // One scan: rows, the time column's nulls, extent and distinct count, and every numeric
    // column's range, nulls and zeros. Approximate distinct counts: they only choose a heuristic,
    // and HyperLogLog is deterministic and blind to row order, so the same data still decides the
    // same way. They are also most of what the scan costs (on five million rows, each was about
    // as much as all the rest together), so each is counted only when its answer is used: the
    // time column's when no series key is stated, the candidates' when the times repeat and
    // none is, or when there is no time column at all.
    let series_stated = !shape.series.is_empty();
    let tk = time_kind.unwrap_or(Kind::Timestamp);
    let t_ms = time.as_ref().map(|t| match tk {
        Kind::Date => format!("epoch_ms(CAST({} AS TIMESTAMP))", quote(t)),
        _ => format!("epoch_ms({})", quote(t)),
    });
    let mut sel = vec!["count(*)::BIGINT".to_string()];
    if let (Some(t), Some(ms)) = (&time, &t_ms) {
        sel.push(format!("count({})::BIGINT", quote(t)));
        sel.push(if series_stated {
            "NULL".to_string()
        } else {
            format!("approx_count_distinct({})::BIGINT", quote(t))
        });
        sel.push(format!("min({ms})::DOUBLE"));
        sel.push(format!("max({ms})::DOUBLE"));
    }
    let hll: Vec<String> = candidates
        .iter()
        .map(|(c, _)| format!("approx_count_distinct({})::BIGINT", quote(c)))
        .collect();
    if time.is_none() {
        sel.extend(hll.iter().cloned());
        // Each candidate's values, which its distinct count cannot exceed
        // (`a_distinct_count_never_exceeds_the_values`): the column is read for its count
        // already, so this costs the scan little.
        sel.extend(
            candidates
                .iter()
                .map(|(c, _)| format!("count({})::BIGINT", quote(c))),
        );
    }
    for (c, _) in &numeric {
        let q = quote(c);
        sel.push(format!("min({q})::DOUBLE"));
        sel.push(format!("max({q})::DOUBLE"));
        // Not `FILTER (… IS NULL)`: a filtered aggregate cost more than the column's range.
        sel.push(format!("(count(*) - count({q}))::BIGINT"));
        sel.push(format!("count(*) FILTER ({q} = 0)::BIGINT"));
    }
    // What the stream showed as it went in stands for the table only if it is this table, cut
    // by the same time column. Without one, the scan asks for the distinct counts, which a
    // stream keeps whenever it has no time column.
    let seen = seen.filter(|s| {
        s.columns == columns && s.time == time && (time.is_some() || s.candidate_distinct.is_some())
    });
    #[cfg(test)]
    if seen.is_some() {
        SEEN_USED.with(|u| u.set((u.get().0 + 1, u.get().1)));
    }
    let scan = match &seen {
        // The scan's answers, as it would give them: each a double, NULL where it is NULL.
        Some(s) => {
            let mut v = vec![Some(s.total as f64)];
            if time.is_some() {
                v.push(Some(s.timed as f64));
                v.push((!series_stated).then_some(s.time_distinct as f64));
                v.push(s.first.map(|t| t as f64));
                v.push(s.last.map(|t| t as f64));
            } else {
                v.extend(
                    s.candidate_distinct
                        .iter()
                        .flatten()
                        .map(|d| Some(*d as f64)),
                );
                // Each candidate's values, as the stream counted them; were they not kept, they
                // are asked of the table (`describe_columns`).
                match &s.candidate_values {
                    Some(n) => v.extend(n.iter().map(|n| Some(*n as f64))),
                    None => v.extend(candidates.iter().map(|_| None)),
                }
            }
            for n in &s.numbers {
                v.extend([n.min, n.max, Some(n.nulls as f64), Some(n.zeros as f64)]);
            }
            v
        }
        None => crate::trace::span("profile.scan", || {
            values(conn, &format!("SELECT {} FROM {from}", sel.join(", ")))
        })?,
    };
    let mut at = scan.into_iter();
    let mut next = || at.next().flatten();
    let total = next().unwrap_or(0.0) as i64;
    if total == 0 {
        return Some(p);
    }
    let (timed, time_distinct, origin, end) = if time.is_some() {
        (
            next().unwrap_or(0.0) as i64,
            next().unwrap_or(0.0) as i64,
            next().unwrap_or(0.0) as i64,
            next().unwrap_or(0.0) as i64,
        )
    } else {
        (0, 0, 0, 0)
    };
    let mut distinct: Vec<i64> = if time.is_none() {
        candidates
            .iter()
            .map(|_| next().unwrap_or(0.0) as i64)
            .collect()
    } else {
        Vec::new()
    };
    let counted: Vec<Option<i64>> = if time.is_none() {
        candidates
            .iter()
            .map(|_| next().map(|v| v as i64))
            .collect()
    } else {
        Vec::new()
    };
    let cols: Vec<Col> = numeric
        .iter()
        .map(|(n, _)| Col {
            name: n.clone(),
            min: next(),
            max: next(),
            nulls: next().unwrap_or(0.0) as i64,
            zeros: next().unwrap_or(0.0) as i64,
        })
        .collect();
    let time_unique = time.is_some() && time_distinct as f64 >= 0.95 * timed as f64;
    if time.is_some() && !series_stated && !time_unique && !hll.is_empty() {
        distinct = match seen.as_ref().and_then(|s| s.candidate_distinct.clone()) {
            Some(d) => d,
            None => crate::trace::span("profile.distinct", || {
                values(conn, &format!("SELECT {} FROM {from}", hll.join(", ")))
            })?
            .into_iter()
            .map(|d| d.unwrap_or(0.0) as i64)
            .collect(),
        };
    }

    // The first time as a literal of its own type, for the samples of the table's first slice
    // of time ([`first_literal`]): asked once, for the key's sample and the step's.
    let first_lit = time
        .as_ref()
        .and_then(|t| first_literal(conn, &from, &quote(t)));
    // The series key: stated, or the text (else integer) column with the most distinct values
    // that still leaves every series ten rows on average, the leftmost on a tie — and only when
    // timestamps repeat, since a table whose every timestamp is distinct reads as one series
    // however it is cut. Where two columns of a kind could be the key, the one whose values cut
    // the first slice of time into series that keep a step wins ([`step_share`]): an integer
    // reading, CO₂ in ppm beside a device id, has more values than the devices, but its
    // "series" do not keep one (`an_integer_reading_is_not_taken_for_the_series_key`).
    let mut series_cols: Vec<String> = shape.series.clone();
    if !series_stated && time.is_some() && !time_unique {
        let pick = |want: Kind| {
            let able: Vec<(&String, i64)> = candidates
                .iter()
                .zip(&distinct)
                .filter(|((_, k), d)| *k == want && **d >= 2 && **d * 10 <= total)
                .map(|((n, _), d)| (n, *d))
                .collect();
            let most = able
                .iter()
                .rev()
                .max_by_key(|(_, d)| *d)
                .map(|(n, _)| (*n).clone())?;
            if able.len() < 2 {
                return Some(most);
            }
            let (Some(t), Some(ms), Some(lit)) = (&time, &t_ms, &first_lit) else {
                return Some(most);
            };
            let width = sample_width(origin, end, timed);
            let shares: Vec<(&String, f64)> = able
                .iter()
                .map(|(n, _)| {
                    let sh = crate::trace::span("profile.key", || {
                        step_share(conn, &from, &quote(n), ms, &quote(t), lit, width)
                    });
                    (*n, sh.unwrap_or(0.0))
                })
                .collect();
            let of = |name: &str| shares.iter().find(|(n, _)| *n == name).map_or(0.0, |s| s.1);
            // The leftmost of the most regular, and only when clearly more regular than the
            // most values: two columns that cut alike keep the rule above.
            let best = shares
                .iter()
                .rev()
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(n, sh)| ((*n).clone(), *sh))?;
            Some(if best.1 > of(&most) + 0.1 {
                best.0
            } else {
                most
            })
        };
        if let Some(k) = pick(Kind::Text).or_else(|| pick(Kind::Integer)) {
            series_cols.push(k);
        } else if total as u64 <= WINDOW_ROWS {
            if let Some(t) = &time {
                let few = |want: Kind| {
                    let names: Vec<String> = candidates
                        .iter()
                        .zip(&distinct)
                        .filter(|((_, k), d)| *k == want && **d >= 2 && **d * 2 <= total)
                        .map(|((n, _), _)| n.clone())
                        .collect();
                    few_rows_key(conn, &from, t, &names, (timed, time_distinct))
                };
                if let Some(k) = few(Kind::Text).or_else(|| few(Kind::Integer)) {
                    series_cols.push(k);
                }
            }
        }
    }
    // Times repeat and nothing tells the rows apart: every repeat would be reported as a fault of
    // one series, which is a guess presented as a finding. Said instead, and not assessed.
    let unkeyed = time.is_some() && !time_unique && series_cols.is_empty();
    let shown = |c: &str| ident(c);
    let guess = |stated: bool| if stated { "" } else { " (looks like it)" };

    // Without a time column, say what is useful anyway: ranges, and the small text columns.
    let Some(time) = time else {
        describe_columns(
            conn,
            &from,
            &cols_lines(&cols, total),
            &candidates,
            (&distinct, &counted),
            total,
            &mut p,
        );
        return Some(p);
    };
    let clock = Clock(tk);
    let t_ms = t_ms.unwrap_or_default();
    // The key as one text: each column's value, a null as [`NULL_PART`], joined by
    // [`KEY_SEP`]. Both are control characters, so no printable value is mistaken for them, and
    // the key is split back into its values to be shown (`key_shown`).
    let k = match series_cols.len() {
        0 => "''".to_string(),
        _ => series_cols
            .iter()
            .map(|c| format!("coalesce(CAST({} AS VARCHAR), chr(30))", quote(c)))
            .collect::<Vec<_>>()
            .join(" || chr(31) || "),
    };
    let integer_key: Vec<bool> = series_cols
        .iter()
        .map(|c| has_kind(&kinds, c) == Some(Kind::Integer))
        .collect();
    let key_shown = |key: &str| -> String {
        key.split(KEY_SEP)
            .enumerate()
            .map(|(i, part)| {
                if part == NULL_PART {
                    "NULL".to_string()
                } else if integer_key.get(i) == Some(&true) && part.parse::<i128>().is_ok() {
                    part.to_string()
                } else {
                    value(part)
                }
            })
            .collect::<Vec<_>>()
            .join(" / ")
    };
    // REQ-14 `an_integer_series_key_is_listed_in_number_order`: the keys are held and kept in
    // text order, which is what decides the series kept past [`SERIES_MAX`]; they are shown in
    // the order of the key's type, an integer column's by number, and a null last.
    let key_order = |a: &str, b: &str| -> std::cmp::Ordering {
        for (i, (x, y)) in a.split(KEY_SEP).zip(b.split(KEY_SEP)).enumerate() {
            let o = match (x == NULL_PART, y == NULL_PART) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ if integer_key.get(i) == Some(&true) => {
                    match (x.parse::<i128>(), y.parse::<i128>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        _ => x.cmp(y),
                    }
                }
                _ => x.cmp(y),
            };
            if o.is_ne() {
                return o;
            }
        }
        std::cmp::Ordering::Equal
    };
    let cx = Ctx {
        conn,
        from: from.clone(),
        t: t_ms,
        k,
    };
    let measures: Vec<&Col> = cols
        .iter()
        .filter(|c| !series_cols.contains(&c.name))
        .take(MEASURES_EXAMINED)
        .collect();

    let step = crate::trace::span("profile.step", || {
        measure_step(
            &cx,
            &quote(&time),
            first_lit.as_deref()?,
            origin,
            end,
            timed,
        )
    });
    // Per series, in one pass: rows, extent, phase, the checksums of the step indices, and each
    // examined measure's nulls, zeros and range — and each time's rows, zeros and sum, from which
    // the moments every series shares are found ([`pass`]).
    let checked = step.filter(|s| *s > 0);
    let keyed = !series_cols.is_empty();
    let per_time = keyed && checked.is_some_and(|st| st <= DAY_MS && DAY_MS % st == 0);
    let Pass {
        mut series,
        times,
        flat,
    } = match seen.and_then(|s| s.pass(&series_cols, &measures, checked, origin, per_time)) {
        Some(p) => {
            #[cfg(test)]
            SEEN_USED.with(|u| u.set((u.get().0, u.get().1 + 1)));
            p
        }
        None => crate::trace::span("profile.pass", || {
            Some(if keyed {
                pass(&cx, &measures, checked, origin, keyed, per_time)?
            } else {
                Pass {
                    series: one_series(&cx, &measures, checked, origin)?,
                    times: None,
                    flat: None,
                }
            })
        })?,
    };
    let too_many = series.len() > SERIES_MAX;
    series.truncate(SERIES_MAX);
    // The last key examined, when series after it were not.
    let examined_to = too_many
        .then(|| series.last().map(|s| s.key.clone()))
        .flatten();

    // The table's extent, from the scan: past `SERIES_MAX`, the series kept are the first by key,
    // and their extent was said as the table's (`many_series_keep_the_tables_own_extent`).
    let (first, last) = if timed > 0 {
        (Some(origin), Some(end))
    } else {
        (None, None)
    };
    // Past three billion steps from the first time, a series is not proven regular: an index
    // squared, summed over as many rows as a table can hold, must stay inside an i128.
    let step = step.filter(|st| {
        last.zip(first)
            .is_none_or(|(b, a)| (b - a) / st < 3_000_000_000)
    });

    // The time column.
    let mut l = format!("time column {}{}", shown(&time), guess(time_stated));
    if let (Some(a), Some(b)) = (first, last) {
        l.push_str(&format!(": {} to {}", clock.at(a), clock.at(b)));
    }
    match step {
        Some(s) if keyed => l.push_str(&format!("; most series have a row every {}", span(s))),
        Some(s) => l.push_str(&format!("; most rows are {} apart", span(s))),
        None => {}
    }
    if timed < total {
        l.push_str(&format!(
            "; {} no time",
            count((total - timed) as u64, "row has", "rows have")
        ));
    }
    p.lines.push(l);
    let others: Vec<String> = kinds
        .iter()
        .filter(|(n, k)| temporal(*k) && *n != time)
        .map(|(n, _)| shown(n))
        .collect();
    if !others.is_empty() {
        p.lines
            .push(format!("other time columns: {}", others.join(", ")));
    }

    // Which series are on the step, with nothing missing or repeated.
    let mut regular: Vec<bool> = series
        .iter()
        .map(|s| step.is_some_and(|st| s.regular(st, origin)))
        .collect();
    if let Some(st) = step {
        crate::trace::span("profile.settle", || {
            settle(&cx, &series, &mut regular, st, origin)
        });
    }
    let n_regular = regular.iter().filter(|r| **r).count();
    let full_range = |s: &Series| s.first == first && s.last == last;
    if keyed {
        let mut l = format!(
            "series key {}{}: {}{} series",
            series_cols
                .iter()
                .map(|c| shown(c))
                .collect::<Vec<_>>()
                .join(", "),
            guess(series_stated),
            if too_many { "more than " } else { "" },
            series.len()
        );
        let mut keys: Vec<&str> = series.iter().map(|s| s.key.as_str()).collect();
        keys.sort_by(|a, b| key_order(a, b));
        // Past `SERIES_MAX`, the series kept are the first in text order, so the greatest of them
        // is not the table's, and by number neither is the least: no range is shown then.
        let ranged = !too_many;
        if series.len() <= 12 {
            // Every value when they fit the line, else the first ones that do and how many more.
            let all: Vec<String> = keys.iter().map(|k| key_shown(k)).collect();
            let room = LINE_MAX.saturating_sub(l.len() + 24);
            let mut shown_keys = String::new();
            let mut n = 0;
            for v in &all {
                let add = if n == 0 { v.len() } else { v.len() + 2 };
                if shown_keys.len() + add > room {
                    break;
                }
                if n > 0 {
                    shown_keys.push_str(", ");
                }
                shown_keys.push_str(v);
                n += 1;
            }
            if n == all.len() {
                l.push_str(&format!(" ({shown_keys})"));
            } else if n > 0 {
                l.push_str(&format!(" ({shown_keys}, … and {} more)", all.len() - n));
            }
        } else if let (Some(a), Some(b), true) = (keys.first(), keys.last(), ranged) {
            l.push_str(&format!(" ({} … {})", key_shown(a), key_shown(b)));
        }
        p.lines.push(l);
        if let Some(st) = step {
            let whole = series
                .iter()
                .zip(&regular)
                .filter(|(s, r)| **r && full_range(s))
                .count();
            let mut l = if let Some(to) = &examined_to {
                let to = key_shown(to);
                format!(
                    "{n_regular} of the first {} series by key, to {to}, {} a row every {} from \
                     {} first to {} last, none missing or repeated; the series after {to} were \
                     not examined one by one and have no notes below",
                    series.len(),
                    if n_regular == 1 { "has" } else { "have" },
                    span(st),
                    if n_regular == 1 { "its" } else { "their" },
                    if n_regular == 1 { "its" } else { "their" },
                )
            } else if n_regular == series.len() {
                format!(
                    "every series has a row every {} from its first to its last, none missing \
                     or repeated",
                    span(st)
                )
            } else {
                let (has, their) = if n_regular == 1 {
                    ("has", "its")
                } else {
                    ("have", "their")
                };
                format!(
                    "{n_regular} of {} series {has} a row every {} from {their} first to {their} \
                     last, none missing or repeated",
                    series.len(),
                    span(st)
                )
            };
            if whole == series.len() && !too_many {
                l.push_str("; all span the whole range");
            }
            p.lines.push(l);
        }
    } else if let Some(st) = step {
        if n_regular == 1 {
            p.lines.push(format!(
                "a row every {} from first to last, none missing or repeated",
                span(st)
            ));
        }
        if time_unique && !series_stated {
            p.lines
                .push("each timestamp appears once, so no series key is assumed".to_string());
        }
    }
    if unkeyed {
        // Counted exactly here: the scan's approximate count only chose the heuristic, and a
        // number said to the agent must be true (it said 44 of 50).
        let times = conn
            .query_row(
                &format!(
                    "SELECT count(DISTINCT {})::BIGINT FROM {from}",
                    quote(&time)
                ),
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(time_distinct);
        p.lines.push(format!(
            "{} distinct times in {}, and no column looks like a series key: gaps and repeats \
             are not assessed",
            times,
            count(timed as u64, "row", "rows")
        ));
    }
    p.lines.extend(cols_lines(&cols, total));

    // Per series: what the GROUP BY alone shows, then what the bounded windows find.
    let threshold = step.map(|s| (8 * s).max(DAY_MS));
    for s in series.iter_mut() {
        note_extent(s, first, last, step, clock, keyed);
        for (j, m) in measures.iter().enumerate() {
            let sm = &s.measures[j];
            let name = shown(&m.name);
            if sm.nulls == s.rows {
                s.notes
                    .push((u64::MAX, format!("{name} is null in every row")));
            } else if sm.zeros == s.rows {
                s.notes
                    .push((u64::MAX, format!("{name} is 0 in every row")));
            } else if sm.nulls == 0 && s.rows > 1 && sm.min == sm.max {
                s.notes.push((
                    u64::MAX - 1,
                    format!("{name} is {} in every row", num(sm.min.unwrap_or(0.0))),
                ));
            }
        }
    }
    if let Some(st) = step {
        if !unkeyed {
            crate::trace::span("profile.gaps", || {
                gaps(&cx, &mut series, &regular, st, clock)
            });
        }
        if let Some(th) = threshold {
            crate::trace::span("profile.runs", || {
                for (j, m) in measures.iter().enumerate() {
                    let rows = flat.as_ref().map(|f| f[j].as_slice());
                    runs(&cx, &mut series, j, &m.name, (st, th), clock, rows);
                }
            });
            p.notes_header =
                format!(
                "{} something to note (a run of 0 or null lasting {} or more, a gap, a repeated \
                 time, a late start or early end):",
                if keyed { "series with" } else { "the series has" },
                span(th)
            );
        }
        if keyed && series.len() >= 3 && st <= DAY_MS && DAY_MS % st == 0 {
            p.events = crate::trace::span("profile.moments", || match &times {
                Some(times) => times.moments(&measures, st, clock),
                None => shared_moments(&cx, &measures, st, clock),
            });
        }
    }

    // Most notable first: a dead series, then by how long its notes last.
    let mut noted: Vec<&Series> = series.iter().filter(|s| !s.notes.is_empty()).collect();
    noted.sort_by(|a, b| {
        let w = |s: &Series| s.notes.iter().map(|n| n.0).max().unwrap_or(0);
        w(b).cmp(&w(a)).then_with(|| a.key.cmp(&b.key))
    });
    p.quiet = (series.len() - noted.len()) as u64;
    if !keyed {
        p.quiet = 0;
    }
    p.more_notes = noted.len().saturating_sub(NOTES_KEPT) as u64;
    for s in noted.into_iter().take(NOTES_KEPT) {
        let mut notes = s.notes.clone();
        notes.sort_by_key(|a| std::cmp::Reverse(a.0));
        let text = notes
            .into_iter()
            .map(|n| n.1)
            .collect::<Vec<_>>()
            .join("; ");
        p.notes.push(line(if keyed {
            format!("{}: {text}", key_shown(&s.key))
        } else {
            text
        }));
    }
    if p.notes.is_empty() {
        p.quiet = 0;
    }
    for l in p.lines.iter_mut().chain(p.events.iter_mut()) {
        *l = line(std::mem::take(l));
    }
    Some(p)
}

/// Each numeric column's range, nulls and zeros.
fn cols_lines(cols: &[Col], total: i64) -> Vec<String> {
    cols.iter()
        .filter_map(|c| {
            let (min, max) = (c.min?, c.max?);
            let mut l = format!("{}: {} to {}", ident(&c.name), num(min), num(max));
            match c.nulls {
                0 => l.push_str(", never null"),
                n if n == total => l.push_str(", always null"),
                n => l.push_str(&format!(", null in {}", count(n as u64, "row", "rows"))),
            }
            if c.zeros > 0 {
                l.push_str(&format!("; 0 in {}", count(c.zeros as u64, "row", "rows")));
            }
            Some(l)
        })
        .collect()
}

/// DuckDB's order on doubles, which its `min`, `max` and comparisons use: NaN equals NaN and is
/// greater than everything else, where Rust's `<` says false.
fn duck_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// The hash the pass looks series and times up with: rustc's FxHash, a multiply and a rotate per
/// word, which halved the pass against the default SipHash on the meter data. It starts from a
/// random state, as SipHash does, so keys cannot be chosen in advance to collide.
#[derive(Clone, Copy)]
struct Fx(u64);

#[derive(Clone, Copy)]
struct FxSeeded(u64);

impl Default for FxSeeded {
    fn default() -> Self {
        use std::hash::BuildHasher;
        FxSeeded(std::collections::hash_map::RandomState::new().hash_one(0u8))
    }
}

impl std::hash::BuildHasher for FxSeeded {
    type Hasher = Fx;
    fn build_hasher(&self) -> Fx {
        Fx(self.0)
    }
}

impl std::hash::Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            self.write_u64(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            self.write_u64(tail_word(rest));
        }
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn write_i64(&mut self, n: i64) {
        self.write_u64(n as u64);
    }
    fn write_u8(&mut self, n: u8) {
        self.write_u64(u64::from(n));
    }
    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

type FxMap<K, V> = std::collections::HashMap<K, V, FxSeeded>;

/// Times kept per table for the shared moments. Past this many, the moments are left to a
/// query of their own (`shared_moments`), which DuckDB can spill; this map cannot. About 40 bytes
/// a time with one measure.
const TIMES_MAX: usize = 1_000_000;

/// Per time: its rows, and each examined measure's zeros and sum — what `shared_moments` asks
/// DuckDB for, gathered in [`pass`] instead.
#[derive(Clone)]
struct Times {
    nm: usize,
    /// The grid from the table's first time, one step apart, where most times are: a time on it
    /// finds its slot by its index, without a lookup.
    grid: Option<(i64, i64)>,
    on_grid: Vec<u32>,
    slot: FxMap<i64, usize>,
    t: Vec<i64>,
    n: Vec<i64>,
    /// `slot * nm + j`, for measure `j`.
    zeros: Vec<i64>,
    sums: Vec<f64>,
    /// Whether the sum has a value: a time whose every value is null has no sum.
    summed: Vec<bool>,
    /// The last time found, and its slot. Rows at one time arrive together, every meter's
    /// reading for a minute, so most rows find their time here without a lookup: without a
    /// step to make a grid of, as a stream's rows are tallied, the lookup was a hash a row.
    recent: Option<(i64, usize)>,
    /// The rows of the current run of one time, added up apart and into the time's slot when
    /// the run ends ([`Times::close`]): its time, its rows, and each measure's zeros, sum and
    /// whether it has one. At 500 meters a minute, finding the slot and adding into it row by
    /// row took 70 instructions a row (R3-4). A time in one run sums exactly as row by row,
    /// from 0.0 in the same order; one in several runs adds up each run first, and the table
    /// read back is added up the same way, in the same order.
    run_t: i64,
    runs: Run,
}

/// What [`Times`] adds up of the current run of one time, per measure, in plain arrays.
#[derive(Clone, Copy, Default)]
struct Run {
    n: i64,
    zeros: [i64; MEASURES_EXAMINED],
    sums: [f64; MEASURES_EXAMINED],
    summed: [bool; MEASURES_EXAMINED],
}

impl Times {
    fn new(nm: usize, grid: Option<(i64, i64)>) -> Self {
        assert!(nm <= MEASURES_EXAMINED, "{nm} measures");
        Times {
            nm,
            grid,
            on_grid: Vec::new(),
            slot: Default::default(),
            t: Vec::new(),
            n: Vec::new(),
            zeros: Vec::new(),
            sums: Vec::new(),
            summed: Vec::new(),
            recent: None,
            run_t: 0,
            runs: Run::default(),
        }
    }

    /// Add the run of one time to its slot. `false` when it would be a time past `max`.
    #[inline(never)]
    fn close(&mut self, max: usize) -> bool {
        let r = std::mem::take(&mut self.runs);
        if r.n == 0 {
            return true;
        }
        let Some(slot) = self.at(self.run_t).filter(|_| self.t.len() <= max) else {
            return false;
        };
        self.n[slot] += r.n;
        let k = slot * self.nm;
        for j in 0..self.nm {
            self.zeros[k + j] += r.zeros[j];
            if r.summed[j] {
                self.sums[k + j] += r.sums[j];
                self.summed[k + j] = true;
            }
        }
        true
    }

    /// The slot for time `t`, or `None` when it would be one too many.
    #[inline(always)]
    fn at(&mut self, t: i64) -> Option<usize> {
        if let Some((r, i)) = self.recent {
            if r == t {
                return Some(i);
            }
            // #54: rows sorted by series, then time, bring each series' times in the order the
            // first series gave them their slots, so a time is most often the one in the slot
            // after the last time's: a compare, where it was a lookup a row.
            if self.t.get(i + 1) == Some(&t) {
                self.recent = Some((t, i + 1));
                return Some(i + 1);
            }
        }
        let i = self.find(t)?;
        self.recent = Some((t, i));
        Some(i)
    }

    fn find(&mut self, t: i64) -> Option<usize> {
        let cell = self.grid.and_then(|(origin, step)| {
            let d = t.checked_sub(origin)?;
            let q = d / step;
            (d >= 0 && d - q * step == 0 && q < 2 * TIMES_MAX as i64).then_some(q as usize)
        });
        if let Some(c) = cell {
            if let Some(i) = self.on_grid.get(c).filter(|i| **i != u32::MAX) {
                return Some(*i as usize);
            }
        } else if let Some(i) = self.slot.get(&t) {
            return Some(*i);
        }
        if self.t.len() >= TIMES_MAX {
            return None;
        }
        let i = self.t.len();
        if let Some(c) = cell {
            if self.on_grid.len() <= c {
                self.on_grid.resize(c + 1, u32::MAX);
            }
            self.on_grid[c] = i as u32;
        }
        self.slot.insert(t, i);
        self.t.push(t);
        self.n.push(0);
        self.zeros.extend(std::iter::repeat_n(0, self.nm));
        self.sums.extend(std::iter::repeat_n(0.0, self.nm));
        self.summed.extend(std::iter::repeat_n(false, self.nm));
        Some(i)
    }

    fn sum(&self, slot: usize, j: usize) -> Option<f64> {
        let k = slot * self.nm + j;
        self.summed[k].then(|| self.sums[k])
    }

    /// `shared_moments`, from these totals: the same conditions, in the same order, with
    /// DuckDB's NULLs and NaN, so that the same data says the same moments either way.
    fn moments(&self, measures: &[&Col], step: i64, clock: Clock) -> Vec<String> {
        use std::cmp::Ordering::{Greater, Less};
        let mut out = Vec::new();
        let c = self.t.len() as i64;
        let most = self.n.iter().copied().max().unwrap_or(0);
        let gt0 = |x: f64| duck_cmp(x, 0.0) == Greater;
        let near = |t: Option<i64>, j: usize| {
            t.and_then(|t| self.slot.get(&t))
                .and_then(|i| self.sum(*i, j))
        };
        for (j, m) in measures.iter().enumerate() {
            let mut found: Vec<(i64, i64, Option<f64>, i64)> = Vec::new();
            for (i, &t) in self.t.iter().enumerate() {
                let (n, z) = (self.n[i], self.zeros[i * self.nm + j]);
                let s = self.sum(i, j);
                let (a, b) = (
                    near(t.checked_sub(DAY_MS), j),
                    near(t.checked_add(DAY_MS), j),
                );
                let dead = z == n && n >= 3 && n * 2 >= most;
                let odd = z < n
                    && match (s, a, b) {
                        (Some(s), Some(a), Some(b)) if gt0(a) && gt0(b) && gt0(s) => {
                            let hi = if duck_cmp(a, b) == Less { b } else { a };
                            let lo = if duck_cmp(a, b) == Greater { b } else { a };
                            duck_cmp(s, 1.8 * hi) != Less || duck_cmp(s * 1.8, lo) != Greater
                        }
                        _ => false,
                    };
                if dead || odd {
                    let ratio = match (s, a, b) {
                        (Some(s), Some(a), Some(b)) => Some(s / ((a + b) / 2.0)),
                        _ => None,
                    };
                    found.push((t, if z == n { 0 } else { 1 }, ratio, c));
                }
            }
            found.sort_by_key(|f| (f.1, f.0));
            found.truncate(20_001);
            word_moments(&found, &m.name, step, clock, &mut out);
        }
        out
    }
}

/// A text column of a batch DuckDB handed over, in whichever layout it chose.
enum Keys<'a> {
    Utf8(&'a duckdb::arrow::array::StringArray),
    Large(&'a duckdb::arrow::array::LargeStringArray),
    View(&'a duckdb::arrow::array::StringViewArray),
}

impl<'a> Keys<'a> {
    fn of(a: &'a duckdb::arrow::array::ArrayRef) -> Option<Self> {
        use duckdb::arrow::array::AsArray;
        a.as_string_opt::<i32>()
            .map(Keys::Utf8)
            .or_else(|| a.as_string_opt::<i64>().map(Keys::Large))
            .or_else(|| a.as_string_view_opt().map(Keys::View))
    }

    #[inline(always)]
    fn words(&self, i: usize) -> Words {
        match self {
            Keys::Utf8(a) => a.words(i),
            Keys::Large(a) => a.words(i),
            Keys::View(a) => a.words(i),
        }
    }

    fn value(&self, i: usize) -> &'a str {
        match self {
            Keys::Utf8(a) => a.value(i),
            Keys::Large(a) => a.value(i),
            Keys::View(a) => a.value(i),
        }
    }
}

/// Per series: rows, extent, phase, the checksums of the step indices, and each examined
/// measure's nulls, zeros and range; and, when `per_time`, each time's totals ([`Times`]).
///
/// One pass, which DuckDB streams here as Arrow and this code adds up ([`Tally`]), instead of a
/// `GROUP BY` series, a `GROUP BY` time per measure joined to itself a day either way, and a
/// window over the rows that are 0 or null. Those were most of the profile: on the meter data
/// (840,960 rows, 24 series) the profile took 1.49 billion instructions with them and 1.06
/// billion with the pass, against 5.0 billion for the load. A table a stream wrote has had the
/// same tally kept as its rows went in ([`Seen`]), and is not read again.
fn pass(
    cx: &Ctx,
    measures: &[&Col],
    step: Option<i64>,
    origin: i64,
    keyed: bool,
    per_time: bool,
) -> Option<Pass> {
    use duckdb::arrow::array::{Array, AsArray};
    use duckdb::arrow::datatypes::{Float64Type, Int64Type};
    let nm = measures.len();
    let mut sel = Vec::new();
    if keyed {
        sel.push(cx.k.clone());
    }
    sel.push(cx.t.clone());
    for m in measures {
        // Monotone, so the range of the doubles is the double of the range; and 0 is 0.
        sel.push(format!("CAST({} AS DOUBLE)", quote(&m.name)));
    }
    let mut st = cx
        .conn
        .prepare(&format!("SELECT {} FROM {}", sel.join(", "), cx.from))
        .ok()?;
    // Streamed, never the whole result at once. The iterator panics on an error, so the batches
    // are taken with `step`, which returns it.
    drop(st.stream_arrow([]).ok()?);
    let grid = step.map(|st| (origin, st));
    let mut tally = Tally::new(nm, per_time, grid, TIMES_MAX, FLAT_MAX, usize::MAX);
    while let Some(batch) = st.step().ok()? {
        let cols = batch.columns();
        let keys = if keyed {
            Some(Keys::of(&cols[0])?)
        } else {
            None
        };
        let base = usize::from(keyed);
        let ts = cols.get(base)?.as_primitive_opt::<Int64Type>()?;
        let ms = (0..nm)
            .map(|j| cols.get(base + 1 + j)?.as_primitive_opt::<Float64Type>())
            .collect::<Option<Vec<_>>>()?;
        let values: Vec<Doubles> = ms
            .iter()
            .map(|m| Doubles {
                v: m.values(),
                nulls: nulls_of(*m),
            })
            .collect();
        tally.add(
            batch.len(),
            |i| keys.as_ref().map_or("", |k| k.value(i)),
            |i| keys.as_ref().map_or(key_words(b""), |k| k.words(i)),
            Some(Stamps {
                v: ts.values(),
                nulls: nulls_of(ts),
            }),
            &values,
            |_| {},
        );
    }
    Some(tally.finish(step, origin))
}

/// What [`pass`] adds up, row by row, in the order the rows are in the table. It needs neither
/// the step nor the first time, which are known only once every row has been seen: each series
/// keeps its first time, the greatest common divisor of every other time's distance from it, and
/// the sums of those distances and of their squares, from which [`Tally::finish`] works out the
/// phase and the checksums of the step indices exactly. So a stream's rows can be tallied as
/// they go into the table ([`Seen`]).
///
/// Series are ordered by key, as `ORDER BY` orders text (by its bytes), and only the first
/// [`SERIES_MAX`] + 1 are kept. More than twice that is never held: past it the keys are cut to
/// the first ones, and a key after the last one kept cannot be among them, so its rows are
/// skipped from then on, which leaves every kept series complete. A series key is matched
/// against the last row's series, or the one after it, before any lookup; a time on the step's
/// grid finds its slot by index.
///
/// R3-4: what a row touches is kept apart from the rest of its series, in plain fields ([`Hot`],
/// [`HotMeasure`]), the key's first sixteen bytes among them, so a row's series is found without
/// reaching the key's text, and its counts are added without an `Option` each. At 500 series
/// sharing each minute that was 386 instructions a row, a third of it finding the series
/// (docs/PERFORMANCE.md, R3-4). The [`Series`] are filled in from them once, by
/// [`Tally::finish`].
#[derive(Clone)]
struct Tally {
    nm: usize,
    /// Each series' key; the rest of each is filled in by [`Tally::finish`].
    series: Vec<Series>,
    hot: Vec<Hot>,
    /// Series `x`'s measure `j` at `x * nm + j`.
    measures: Vec<HotMeasure>,
    index: FxMap<String, usize>,
    /// The series of integer keys, by value ([`int_words`]).
    ints: FxMap<i128, usize>,
    cutoff: Option<String>,
    times: Option<Times>,
    times_max: usize,
    flat: Option<Vec<Vec<(u32, i64, bool)>>>,
    flat_rows: usize,
    flat_max: usize,
    /// More series than this, and the tally gives up ([`Tally::alive`]): what a stream is
    /// tallied for is only a guess at the key, and a guess at a column of ids must not hold a
    /// series per row.
    series_max: usize,
    alive: bool,
    last: Option<usize>,
    /// Whether the last row's series came after the one before it: meters in a fixed cycle, as
    /// every meter reading at each minute is. Then the next series is tried first, and a row
    /// costs one key compare, not two (`a_cycle_of_series_costs_one_key_compare_a_row`).
    cycling: bool,
    /// Each row's series in the batch being added ([`NO_SERIES`]: none), kept from batch to batch.
    xs: Vec<u32>,
}

/// A row whose key is past the cut, in [`Tally`]'s `xs`.
const NO_SERIES: u32 = u32::MAX;

/// What looking a key up found ([`Tally::find`]).
enum Found {
    At(usize),
    /// Past the cut: the row belongs to no series kept.
    Past,
    /// A series too many ([`Tally::alive`]).
    Full,
    /// A series past twice those kept, which waits for [`Tally::cut`].
    Cut,
}

/// What a row of series `x` changes, kept apart from the rest of [`Series`]: the counts, the
/// extent and what [`Series::note`] keeps, each a plain value; and the key's [`Words`].
#[derive(Clone, Copy)]
struct Hot {
    key: Words,
    rows: i64,
    timed: i64,
    /// `i64::MAX` and `i64::MIN` while `timed` is 0.
    first: i64,
    last: i64,
    /// The first time, once `timed` is not 0.
    t0: i64,
    gcd: u64,
    sum_d: i128,
    sum_d2: i128,
    /// [`Series::ordered`], each way; `last` and `first` are the latest and earliest so far.
    rising: bool,
    falling: bool,
}

/// What a row changes of one of its series' measures. `min` starts as NaN, which DuckDB orders
/// above every value, and `max` as -∞, below every value, so the first value replaces either
/// unless it is equal, and then it is the same value; with no value at all they are not read.
#[derive(Clone, Copy)]
struct HotMeasure {
    nulls: i64,
    zeros: i64,
    min: f64,
    max: f64,
}

impl HotMeasure {
    const NONE: HotMeasure = HotMeasure {
        nulls: 0,
        zeros: 0,
        min: f64::NAN,
        max: f64::NEG_INFINITY,
    };
}

/// A key as a tally finds a row's series by it: its length and its first sixteen bytes as two
/// little-endian words, zero past its end. Two keys of sixteen bytes or fewer are the same exactly
/// when their words are; a longer one is compared in full too ([`Tally::is`]).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Words {
    len: usize,
    w0: u64,
    w1: u64,
}

/// The `len` of an integer key's [`Words`], which no text has: its words are its value.
const INT_KEY: usize = usize::MAX;

/// R3-10: an integer key's [`Words`], its value in two words. Written as text for every row so
/// that it could be found as text, an integer key cost the watch thread 940 ms of CPU where the
/// same rows with text keys cost 619 ms, at exec_series-10m's shape: the watch close to the path
/// to the first `describe` (docs/PERFORMANCE.md, R3-10). Its text is written once, for its
/// series ([`Tally::find`]); its distinct count was always counted as an integer.
#[inline(always)]
fn int_words(x: i128) -> Words {
    Words {
        len: INT_KEY,
        w0: x as u64,
        w1: (x >> 64) as u64,
    }
}

impl Words {
    /// The integer these are the words of ([`int_words`]), if they are.
    fn int(&self) -> Option<i128> {
        (self.len == INT_KEY).then_some(((self.w1 as u128) << 64 | self.w0 as u128) as i128)
    }
}

/// [`Words`] of the key `k`.
#[inline(always)]
fn key_words(k: &[u8]) -> Words {
    let word = |b: &[u8]| u64::from_le_bytes(b[..8].try_into().expect("eight bytes"));
    let len = k.len();
    match len {
        0..8 => Words {
            len,
            w0: tail_word(k),
            w1: 0,
        },
        8..16 => Words {
            len,
            w0: word(k),
            w1: tail_word(&k[8..]),
        },
        _ => Words {
            len,
            w0: word(k),
            w1: word(&k[8..]),
        },
    }
}

/// The low `n` bytes of a word, as a mask: all of it from eight up.
#[inline(always)]
fn low_bytes(n: usize) -> u64 {
    if n >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * n)) - 1
    }
}

/// [`Words`] of the `len` bytes at `start` of `data`, a string array's values: two loads and two
/// masks when sixteen bytes are there to read, where [`key_words`] reads a short key in up to
/// three pieces, a branch each. Past the end of the key are the next keys' bytes, masked off.
#[inline(always)]
fn words_at(data: &[u8], start: usize, len: usize) -> Words {
    match data.get(start..start + 16) {
        Some(b) => {
            let (a, b) = b.split_at(8);
            Words {
                len,
                w0: u64::from_le_bytes(a.try_into().expect("eight bytes")) & low_bytes(len),
                w1: u64::from_le_bytes(b.try_into().expect("eight bytes"))
                    & low_bytes(len.saturating_sub(8)),
            }
        }
        None => key_words(&data[start..start + len]),
    }
}

/// A string array a key is read from, by the key's [`Words`] first.
trait KeyArray {
    fn words(&self, i: usize) -> Words;
}

impl<O: duckdb::arrow::array::OffsetSizeTrait> KeyArray
    for duckdb::arrow::array::GenericStringArray<O>
{
    #[inline(always)]
    fn words(&self, i: usize) -> Words {
        let o = self.value_offsets();
        let (start, end) = (o[i].as_usize(), o[i + 1].as_usize());
        words_at(self.value_data(), start, end - start)
    }
}

impl KeyArray for duckdb::arrow::array::StringViewArray {
    /// A string of up to twelve bytes is held in its view, after its length: its words are the
    /// view's, shifted.
    #[inline(always)]
    fn words(&self, i: usize) -> Words {
        let v = self.views()[i];
        let len = v as u32 as usize;
        if len > 12 {
            return key_words(self.value(i).as_bytes());
        }
        Words {
            len,
            w0: (v >> 32) as u64 & low_bytes(len),
            w1: (v >> 96) as u64 & low_bytes(len.saturating_sub(8)),
        }
    }
}

impl Tally {
    fn new(
        nm: usize,
        per_time: bool,
        grid: Option<(i64, i64)>,
        times_max: usize,
        flat_max: usize,
        series_max: usize,
    ) -> Self {
        Tally {
            nm,
            series: Vec::new(),
            hot: Vec::new(),
            measures: Vec::new(),
            index: Default::default(),
            ints: Default::default(),
            cutoff: None,
            times: per_time.then(|| Times::new(nm, grid)),
            times_max,
            flat: Some(vec![Vec::new(); nm]),
            flat_rows: 0,
            flat_max,
            series_max,
            alive: true,
            last: None,
            cycling: false,
            xs: Vec::new(),
        }
    }

    /// Whether series `x` is the one keyed `key`, whose words are `w`.
    #[inline(always)]
    fn is<'k>(&self, x: usize, w: Words, key: impl FnOnce() -> &'k str) -> bool {
        #[cfg(test)]
        KEY_COMPARES.with(|c| c.set(c.get() + 1));
        self.hot[x].key == w
            && (w.len <= 16
                || w.len == INT_KEY
                || self.series[x].key.as_bytes()[16..] == key().as_bytes()[16..])
    }

    /// `n` rows: each one's key, as text and as its [`Words`], its time in epoch milliseconds,
    /// and each measure's value. The text is read only to begin a series, or to compare a key
    /// longer than its words.
    ///
    /// `fresh(i)` is called for each row whose key is not one of a series already kept — a new
    /// series, a key past the cut, or [`NULL_PART`] — so that a caller counting the keys' distinct
    /// values counts only those ([`Watch::add`]). Returns the rows it took: `n`, or fewer when it
    /// gave up on the way.
    ///
    /// A column at a time: each row's series is found first, then the rows are added to their
    /// series, then each measure's values, then the times', each over the batch's own slices.
    /// Row by row, through a closure and an `Option` for each value, the watch took 356
    /// instructions a row of exec_series' 500 meters; this way, with the times added up a run at
    /// a time, 285, where the watch took 398 before R3-4 (docs/PERFORMANCE.md).
    fn add<'k>(
        &mut self,
        n: usize,
        key: impl Fn(usize) -> &'k str,
        words: impl Fn(usize) -> Words,
        time: Option<Stamps>,
        values: &[Doubles],
        mut fresh: impl FnMut(usize),
    ) -> usize {
        debug_assert_eq!(values.len(), self.nm);
        let mut xs = std::mem::take(&mut self.xs);
        let mut from = 0;
        while from < n && self.alive {
            xs.clear();
            let to = self.find_each(from, n, &key, &words, &mut fresh, &mut xs);
            self.take(from, &xs, time, values);
            from = to;
            if from < n && self.alive {
                // Past twice the series kept: cut to the first, and find that row's again.
                self.cut();
            }
        }
        self.xs = xs;
        from
    }

    /// The series of each row from `from` on, into `xs` ([`NO_SERIES`] for a key past the cut),
    /// until the rows end, the tally gives up, or a row would begin a series past twice what is
    /// kept, which must wait for [`Tally::cut`]. Returns where it stopped.
    #[inline(always)]
    fn find_each<'k>(
        &mut self,
        from: usize,
        n: usize,
        key: &impl Fn(usize) -> &'k str,
        words: &impl Fn(usize) -> Words,
        fresh: &mut impl FnMut(usize),
        xs: &mut Vec<u32>,
    ) -> usize {
        let null = key_words(NULL_PART.as_bytes());
        // Kept in locals for the loop, and written back once: as fields, the compiler loaded and
        // stored them every row.
        let (mut last, mut cycling) = (self.last, self.cycling);
        xs.reserve(n - from);
        let mut stopped = n;
        for i in from..n {
            let w = words(i);
            // The last row's series, then the one after it, or the other way round when the
            // last row's series came after the one before: a meter's rows arrive in runs, or in
            // a fixed cycle, and either way the key is found without hashing it.
            let tried = last.map(|x| {
                let next = if x + 1 == self.hot.len() { 0 } else { x + 1 };
                if cycling {
                    (next, x)
                } else {
                    (x, next)
                }
            });
            let at = match tried {
                Some((a, _)) if self.is(a, w, || key(i)) => Some(a),
                Some((_, b)) if self.is(b, w, || key(i)) => Some(b),
                _ => match self.find(key(i), w) {
                    Found::At(x) => Some(x),
                    Found::Past => None,
                    Found::Full => {
                        self.alive = false;
                        stopped = i;
                        break;
                    }
                    Found::Cut => {
                        stopped = i;
                        break;
                    }
                },
            };
            let len = self.hot.len();
            cycling = matches!((last, at), (Some(l), Some(x))
                if x != l && x == if l + 1 == len { 0 } else { l + 1 });
            let h = at.map(|x| &mut self.hot[x]);
            if h.as_ref().is_none_or(|h| h.rows == 0) || w == null {
                fresh(i);
            }
            last = at;
            // A series is new before it has a row: its count must say so to the next row.
            if let Some(h) = h {
                h.rows += 1;
            }
            xs.push(at.map_or(NO_SERIES, |x| x as u32));
        }
        self.last = last;
        self.cycling = cycling;
        stopped
    }

    /// Rows `from..from + xs.len()`, whose series are `xs`: their times and values.
    #[inline(always)]
    fn take(&mut self, from: usize, xs: &[u32], time: Option<Stamps>, values: &[Doubles]) {
        let nm = self.nm;
        let rows = from..from + xs.len();
        // Each column's slice of these rows, zipped with the series, so no row is bounds-checked
        // twice; and a column without nulls is not checked at all ([`nulls_of`]).
        if let Some(ts) = time {
            let hot = &mut self.hot[..];
            for (i, (x, t)) in rows.clone().zip(xs.iter().zip(&ts.v[rows.clone()])) {
                if *x != NO_SERIES && ts.valid(i) {
                    hot[*x as usize].note(*t);
                }
            }
        }
        for (j, d) in values.iter().enumerate() {
            let measures = &mut self.measures[..];
            let mut flat = self.flat.as_mut().map(|f| &mut f[j]);
            let mut kept = 0;
            for (i, (x, v)) in rows.clone().zip(xs.iter().zip(&d.v[rows.clone()])) {
                if *x == NO_SERIES {
                    continue;
                }
                let m = &mut measures[*x as usize * nm + j];
                let v = *v;
                let null = !valid(d.nulls, i);
                if null || v == 0.0 {
                    if null {
                        m.nulls += 1;
                    } else {
                        m.zeros += 1;
                    }
                    if let (Some(f), Some(ts)) = (flat.as_mut(), time.filter(|ts| ts.valid(i))) {
                        f.push((*x, ts.v[i], null));
                        kept += 1;
                    }
                    if null {
                        continue;
                    }
                }
                // `duck_cmp(v, m.min).is_lt()` and `duck_cmp(v, m.max).is_gt()`: NaN is above
                // every value and equal to itself.
                if v < m.min || (m.min.is_nan() && !v.is_nan()) {
                    m.min = v;
                }
                if v > m.max || (v.is_nan() && !m.max.is_nan()) {
                    m.max = v;
                }
            }
            if self.flat.is_some() {
                self.flat_rows += kept;
            }
        }
        if self.flat_rows > self.flat_max {
            self.flat = None;
        }
        if let Some(ts) = time {
            self.add_times(rows, ts, values);
        }
    }

    /// Rows `rows`' times and values, into the totals per time.
    #[inline(always)]
    fn add_times(&mut self, rows: std::ops::Range<usize>, ts: Stamps, values: &[Doubles]) {
        let Some(tm) = self.times.as_mut() else {
            return;
        };
        for (i, t) in rows.clone().zip(&ts.v[rows]) {
            if !ts.valid(i) {
                continue;
            }
            if *t != tm.run_t {
                if !tm.close(self.times_max) {
                    self.times = None;
                    return;
                }
                tm.run_t = *t;
            }
            let r = &mut tm.runs;
            r.n += 1;
            for (j, d) in values.iter().enumerate() {
                if !valid(d.nulls, i) {
                    continue;
                }
                let v = d.v[i];
                r.zeros[j] += i64::from(v == 0.0);
                r.sums[j] += v;
                r.summed[j] = true;
            }
        }
    }

    /// #54: `n` rows of a tally that waits to begin ([`Watch::begin`]): their totals per time
    /// alone, which nothing else keeps.
    fn wait(&mut self, n: usize, time: Option<Stamps>, values: &[Doubles]) {
        if let Some(ts) = time {
            self.add_times(0..n, ts, values);
        }
    }

    /// #54: begin a tally that waited, with its one series keyed `key` (words `w`) as `one`
    /// kept it of every row so far ([`Series::note`]): `rows` rows, `timed` of them with a time
    /// from `first` to `last`, and its measures and rows of 0 or null. A row adds the same to
    /// either, so the series is the one the tally would have kept
    /// (`a_stream_sorted_by_series_is_tallied_by_its_key`).
    fn begin(
        &mut self,
        (key, w): (&str, Words),
        rows: i64,
        (timed, first, last): (i64, Option<i64>, Option<i64>),
        one: &Series,
        measures: &[HotMeasure],
        flat: Option<Vec<Vec<(u32, i64, bool)>>>,
    ) {
        let Found::At(x) = self.find(key, w) else {
            self.alive = false;
            return;
        };
        let h = &mut self.hot[x];
        h.rows = rows;
        if timed > 0 {
            h.timed = timed;
            h.first = first.unwrap_or(i64::MAX);
            h.last = last.unwrap_or(i64::MIN);
            h.t0 = one.t0.unwrap_or(0);
            h.gcd = one.gcd;
            h.sum_d = one.sum_d;
            h.sum_d2 = one.sum_d2;
            h.rising = one.rising;
            h.falling = one.falling;
        }
        let nm = self.nm;
        self.measures[x * nm..(x + 1) * nm].copy_from_slice(measures);
        self.flat_rows = flat.as_ref().map_or(0, |f| f.iter().map(Vec::len).sum());
        self.flat = flat.filter(|_| self.flat_rows <= self.flat_max);
        self.last = Some(x);
    }

    /// The series keyed `key`, by lookup, or begun.
    #[cold]
    ///
    /// An integer key ([`int_words`]) is looked up by its value, and written as text only to
    /// begin its series, as `CAST(… AS VARCHAR)` writes it: `key` is not read for it.
    fn find(&mut self, key: &str, w: Words) -> Found {
        let int = w.int();
        let found = match int {
            Some(x) => self.ints.get(&x),
            None => self.index.get(key),
        };
        if let Some(x) = found {
            return Found::At(*x);
        }
        let text = match int {
            Some(x) => {
                #[cfg(test)]
                INT_TEXTS.with(|c| c.set(c.get() + 1));
                std::borrow::Cow::Owned(x.to_string())
            }
            None => std::borrow::Cow::Borrowed(key),
        };
        if self.cutoff.as_deref().is_some_and(|c| *text >= *c) {
            return Found::Past;
        }
        if self.series.len() >= self.series_max {
            return Found::Full;
        }
        if self.series.len() >= 2 * (SERIES_MAX + 1) {
            return Found::Cut;
        }
        match int {
            Some(x) => self.ints.insert(x, self.series.len()),
            None => self.index.insert(text.to_string(), self.series.len()),
        };
        self.series.push(Series {
            key: text.into_owned(),
            ..Series::default()
        });
        self.hot.push(Hot {
            key: w,
            rows: 0,
            timed: 0,
            first: i64::MAX,
            last: i64::MIN,
            t0: 0,
            gcd: 0,
            sum_d: 0,
            sum_d2: 0,
            rising: true,
            falling: true,
        });
        self.measures
            .extend(std::iter::repeat_n(HotMeasure::NONE, self.nm));
        Found::At(self.series.len() - 1)
    }

    /// Keep the first [`SERIES_MAX`] + 1 series by key, and refuse every key after them from now
    /// on, so that a series kept is complete.
    #[cold]
    fn cut(&mut self) {
        let keep = SERIES_MAX + 1;
        let nm = self.nm;
        let mut order: Vec<usize> = (0..self.series.len()).collect();
        order.sort_by(|a, b| self.series[*a].key.cmp(&self.series[*b].key));
        self.cutoff = Some(self.series[order[keep]].key.clone());
        order.truncate(keep);
        let mut series = std::mem::take(&mut self.series);
        self.series = order
            .iter()
            .map(|x| std::mem::take(&mut series[*x]))
            .collect();
        self.hot = order.iter().map(|x| self.hot[*x]).collect();
        self.measures = order
            .iter()
            .flat_map(|x| self.measures[x * nm..(x + 1) * nm].iter().copied())
            .collect();
        // Indices change; the runs are left to their query.
        self.flat = None;
        self.last = None;
        self.index.clear();
        self.ints.clear();
        for (i, (s, h)) in self.series.iter().zip(&self.hot).enumerate() {
            match h.key.int() {
                Some(x) => self.ints.insert(x, i),
                None => self.index.insert(s.key.clone(), i),
            };
        }
    }
    /// The series in key order, each with its phase and checksums for `step` from `origin`, the
    /// table's first time; the kept rows follow their series.
    fn finish(self, step: Option<i64>, origin: i64) -> Pass {
        let keep = SERIES_MAX + 1;
        let Tally {
            nm,
            mut series,
            hot,
            measures,
            mut times,
            flat,
            times_max,
            ..
        } = self;
        if times.as_mut().is_some_and(|tm| !tm.close(times_max)) {
            times = None;
        }
        for (x, (s, h)) in series.iter_mut().zip(&hot).enumerate() {
            let timed = h.timed > 0;
            s.rows = h.rows;
            s.timed = h.timed;
            s.first = timed.then_some(h.first);
            s.last = timed.then_some(h.last);
            s.t0 = timed.then_some(h.t0);
            s.gcd = h.gcd;
            s.sum_d = h.sum_d;
            s.sum_d2 = h.sum_d2;
            s.ordered = timed && (h.rising || h.falling);
            s.measures = measures[x * nm..(x + 1) * nm]
                .iter()
                .map(|m| {
                    let some = m.nulls < h.rows;
                    SeriesMeasure {
                        nulls: m.nulls,
                        zeros: m.zeros,
                        min: some.then_some(m.min),
                        max: some.then_some(m.max),
                    }
                })
                .collect();
        }
        if let Some(st) = step.filter(|s| *s > 0) {
            for s in series.iter_mut() {
                s.place(st, origin);
            }
        }
        let mut order: Vec<usize> = (0..series.len()).collect();
        order.sort_by(|a, b| series[*a].key.cmp(&series[*b].key));
        let mut moved = vec![u32::MAX; series.len()];
        for (to, from) in order.iter().enumerate() {
            moved[*from] = to as u32;
        }
        let mut slots: Vec<Option<Series>> = series.into_iter().map(Some).collect();
        let mut series: Vec<Series> = order.iter().filter_map(|i| slots[*i].take()).collect();
        series.truncate(keep);
        let flat = flat.map(|f| {
            f.into_iter()
                .map(|rows| {
                    rows.into_iter()
                        .map(|(x, t, z)| (moved[x as usize], t, z))
                        .filter(|r| (r.0 as usize) < keep)
                        .collect()
                })
                .collect()
        });
        Pass {
            series,
            times,
            flat,
        }
    }
}

impl Hot {
    /// Time `t` of one of the series' rows: its extent, and what [`Series::place`] works from,
    /// as [`Series::note`] keeps it.
    #[inline(always)]
    fn note(&mut self, t: i64) {
        if self.timed == 0 {
            self.t0 = t;
        } else {
            let d = t.wrapping_sub(self.t0);
            let u = d.unsigned_abs();
            // Most distances are a multiple of what is already kept: one division.
            if !u.is_multiple_of(self.gcd) {
                self.gcd = gcd(self.gcd, u);
            }
            self.sum_d = self.sum_d.wrapping_add(d as i128);
            self.sum_d2 = self
                .sum_d2
                .wrapping_add((d as i128).wrapping_mul(d as i128));
        }
        // A compare each way a row, beside the extent's: before the first row `last` is still
        // `i64::MIN` and `first` `i64::MAX`. A table written newest first is in order too: with
        // only `rising`, the meter data written backwards cost 1.5–1.9 billion instructions
        // more, in `settle`'s exact count, than written forwards.
        self.rising &= t > self.last;
        self.falling &= t < self.first;
        self.timed += 1;
        self.first = self.first.min(t);
        self.last = self.last.max(t);
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
thread_local! {
    /// How many profiles on this thread took their scan, and their pass, from a [`Seen`].
    pub(crate) static SEEN_USED: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
    /// How many keys a tally compared with a series' on this thread, to find a row's series.
    pub(crate) static KEY_COMPARES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many integer keys a watch on this thread wrote as text.
    pub(crate) static INT_TEXTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many times runs were read from the table on this thread ([`runs_sql`]).
    pub(crate) static RUNS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One column of a batch, for [`Watch::add`]: read where it lies — its values and its validity
/// bitmap — with what makes each value the one the bench holds applied as it is read.
///
/// The watch first took each column as a `Vec<Option<_>>` the batch was copied into. Those
/// copies were 15–20% of `stream.watch` and 1–4 MB of a loader's peak (docs/PERFORMANCE.md,
/// R2-2); read in place, they cost neither.
pub struct Held<'a> {
    values: Values<'a>,
    nulls: Option<&'a duckdb::arrow::buffer::NullBuffer>,
}

enum Values<'a> {
    /// A timestamp: each value times `up`, over `down`, is microseconds since the epoch.
    Time {
        raw: &'a [i64],
        up: i64,
        down: i64,
    },
    /// `DATE`, in days since the epoch.
    Days(&'a [i32]),
    /// `BIGINT` or `HUGEINT`, in the width it was sent.
    Int(Ints<'a>),
    Real(&'a [f64]),
    Real32(&'a [f32]),
    Text(Keys<'a>),
}

enum Ints<'a> {
    I8(&'a [i8]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    U8(&'a [u8]),
    U16(&'a [u16]),
    U32(&'a [u32]),
    U64(&'a [u64]),
}

impl Ints<'_> {
    fn get(&self, i: usize) -> i128 {
        match self {
            Ints::I8(v) => v[i] as i128,
            Ints::I16(v) => v[i] as i128,
            Ints::I32(v) => v[i] as i128,
            Ints::I64(v) => v[i] as i128,
            Ints::U8(v) => v[i] as i128,
            Ints::U16(v) => v[i] as i128,
            Ints::U32(v) => v[i] as i128,
            Ints::U64(v) => v[i] as i128,
        }
    }

    /// `f(i, value)` for the first `n` values, the width matched once, not per value.
    fn each(&self, n: usize, mut f: impl FnMut(usize, i128)) {
        macro_rules! each {
            ($v:expr) => {
                for (i, x) in $v[..n].iter().enumerate() {
                    f(i, *x as i128)
                }
            };
        }
        match self {
            Ints::I8(v) => each!(v),
            Ints::I16(v) => each!(v),
            Ints::I32(v) => each!(v),
            Ints::I64(v) => each!(v),
            Ints::U8(v) => each!(v),
            Ints::U16(v) => each!(v),
            Ints::U32(v) => each!(v),
            Ints::U64(v) => each!(v),
        }
    }
}

/// Column `a`'s validity, if it has a null: a column without one is read as having none, so its
/// rows are not checked one by one. DuckDB, and pyarrow's IPC writer, send a validity bitmap
/// whatever the nulls; read row by row, the watch's checks of it were about a tenth of its
/// instructions at 500 series (R3-4).
#[inline]
fn nulls_of(a: &dyn duckdb::arrow::array::Array) -> Option<&duckdb::arrow::buffer::NullBuffer> {
    a.nulls().filter(|n| n.null_count() > 0)
}

/// Whether row `i` of a column with validity `nulls` has a value.
#[inline]
fn valid(nulls: Option<&duckdb::arrow::buffer::NullBuffer>, i: usize) -> bool {
    nulls.is_none_or(|b| b.is_valid(i))
}

impl<'a> Held<'a> {
    /// Column `a`, sent as it was, which the conversion's SQL makes `held`, a DuckDB type as the
    /// bench holds it (`crate::canonical::held`), reading a zone-less timestamp in UTC when
    /// `utc`. `None` for a column whose held values cannot be worked out here exactly as that SQL
    /// works them out, which leaves the profile to read the table: a zone-less timestamp read in
    /// a zone other than UTC (ICU's calendar), a decimal, and any type the profile does not
    /// read. DuckDB's infinite timestamps and dates, which `epoch_ms` makes null, are found as
    /// [`Watch::add`] reads them, and give up the same way.
    pub fn of(a: &'a duckdb::arrow::array::ArrayRef, held: &str, utc: bool) -> Option<Self> {
        use duckdb::arrow::array::{Array, ArrayRef, AsArray};
        use duckdb::arrow::datatypes::{DataType as D, TimeUnit as U, *};
        fn p<T: ArrowPrimitiveType>(a: &ArrayRef) -> &[T::Native] {
            a.as_primitive::<T>().values()
        }
        let values = match (held, a.data_type()) {
            // A zoned timestamp is the instant it names, whatever the zone.
            ("TIMESTAMP WITH TIME ZONE", D::Timestamp(unit, zone)) if zone.is_some() || utc => {
                let (raw, up, down): (&[i64], _, _) = match unit {
                    U::Second => (p::<TimestampSecondType>(a), 1_000_000, 1),
                    U::Millisecond => (p::<TimestampMillisecondType>(a), 1_000, 1),
                    U::Microsecond => (p::<TimestampMicrosecondType>(a), 1, 1),
                    U::Nanosecond => (p::<TimestampNanosecondType>(a), 1, 1_000),
                };
                Values::Time { raw, up, down }
            }
            ("DATE", D::Date32) => Values::Days(p::<Date32Type>(a)),
            ("BIGINT", D::Int8) => Values::Int(Ints::I8(p::<Int8Type>(a))),
            ("BIGINT", D::Int16) => Values::Int(Ints::I16(p::<Int16Type>(a))),
            ("BIGINT", D::Int32) => Values::Int(Ints::I32(p::<Int32Type>(a))),
            ("BIGINT", D::Int64) => Values::Int(Ints::I64(p::<Int64Type>(a))),
            ("BIGINT", D::UInt8) => Values::Int(Ints::U8(p::<UInt8Type>(a))),
            ("BIGINT", D::UInt16) => Values::Int(Ints::U16(p::<UInt16Type>(a))),
            ("BIGINT", D::UInt32) => Values::Int(Ints::U32(p::<UInt32Type>(a))),
            ("HUGEINT", D::UInt64) => Values::Int(Ints::U64(p::<UInt64Type>(a))),
            ("DOUBLE", D::Float64) => Values::Real(p::<Float64Type>(a)),
            ("DOUBLE", D::Float32) => Values::Real32(p::<Float32Type>(a)),
            ("VARCHAR", D::Utf8 | D::LargeUtf8 | D::Utf8View) => Values::Text(Keys::of(a)?),
            _ => return None,
        };
        Some(Held {
            values,
            nulls: nulls_of(a),
        })
    }
}

/// A batch's times in epoch milliseconds, and which rows have one, as [`Tally::add`] reads them.
#[derive(Clone, Copy)]
struct Stamps<'a> {
    v: &'a [i64],
    nulls: Option<&'a duckdb::arrow::buffer::NullBuffer>,
}

impl Stamps<'_> {
    #[inline(always)]
    fn valid(&self, i: usize) -> bool {
        valid(self.nulls, i)
    }
}

/// A numeric column of a batch as doubles, as the profile reads it: in place when it was sent
/// as doubles, else converted into a buffer the watch keeps from batch to batch.
#[derive(Clone, Copy)]
struct Doubles<'a> {
    v: &'a [f64],
    nulls: Option<&'a duckdb::arrow::buffer::NullBuffer>,
}

/// Series a guessed key may reach before its tally gives up; past it, if that column is the key
/// after all, the table is read again. Each series costs a few hundred bytes.
const WATCH_SERIES_MAX: usize = 50_000;
/// Times a guessed key's tally keeps per time, and rows of 0 or null for its runs, before it
/// leaves them to their queries ([`shared_moments`], [`runs_sql`]), which say the same.
const WATCH_TIMES_MAX: usize = 250_000;
const WATCH_FLAT_MAX: usize = 500_000;
/// Keys guessed at most: a tally is kept for each candidate column only when there are this
/// few, since each costs a lookup a row.
const WATCH_KEYS_MAX: usize = 2;

/// #54: how a candidate's series and the times have followed one another, row after row, while
/// the watch has not chosen between the one series and the keyed tallies ([`Watch::add`]). Rows
/// sorted by series, then time, as most per-device exports are, show their first batches as one
/// series whose times do not repeat, which once chose the one series: the table's pass was then
/// read again on one thread, 250–307 ms of a 0.90–1.07 s load of 10,000,000 rows of 150 series
/// from Parquet, where the same rows sorted by time took 6–16 ms of 0.59–0.64 s.
///
/// Only rows with a time are counted, each against the one with a time before it.
#[derive(Clone, Copy, Default)]
struct Order {
    /// The row before's series ([`Tally`]'s index) and time.
    before: Option<(u32, i64)>,
    rows: i64,
    /// Rows whose series is not the row before's.
    changes: i64,
    /// Rows of the row before's series earlier than it, and later.
    earlier: i64,
    later: i64,
    /// Rows beginning another series no later than the row before, and no earlier.
    back: i64,
    ahead: i64,
}

/// The one value a candidate has held while its tally waits ([`Watch::begin`]).
#[derive(Clone)]
enum Wait {
    /// No row yet.
    Unset,
    Null,
    Text(String),
    Int(i128),
}

/// What an [`Order`] says of its candidate so far.
#[derive(PartialEq)]
enum Sorted {
    /// One value yet, in times that kept one direction: one series, or the first of several.
    Waiting,
    /// Runs of its values, each in time order, and a value that began back in time: rows sorted
    /// by series, then time, whose times will repeat.
    Series,
    /// Neither.
    No,
}

impl Order {
    /// The rows of a batch: each one's series, `xs`, and its time.
    #[inline(always)]
    fn see(&mut self, xs: impl Iterator<Item = u32>, time: Stamps) {
        let mut o = *self;
        for (i, (x, t)) in xs.zip(time.v).enumerate() {
            if !time.valid(i) {
                continue;
            }
            let t = *t;
            o.rows += 1;
            if let Some((bx, bt)) = o.before {
                if x == bx {
                    o.earlier += i64::from(t < bt);
                    o.later += i64::from(t > bt);
                } else {
                    o.changes += 1;
                    o.back += i64::from(t <= bt);
                    o.ahead += i64::from(t >= bt);
                }
            }
            o.before = Some((x, t));
        }
        *self = o;
    }

    fn sorted(&self) -> Sorted {
        // Within a series, times that go both ways are not a series' in order.
        if self.earlier > 0 && self.later > 0 {
            return Sorted::No;
        }
        if self.changes == 0 {
            return Sorted::Waiting;
        }
        // Runs of ten rows on average at least, as the profile asks of a key's series: a
        // reading that changes every row, beside times in no order, also goes back at changes.
        if self.changes * 10 > self.rows {
            return Sorted::No;
        }
        if (self.earlier == 0 && self.back > 0) || (self.later == 0 && self.ahead > 0) {
            Sorted::Series
        } else {
            Sorted::No
        }
    }
}

/// The profile's first scan and its pass, worked out from a stream's batches as they go into the
/// table ([`crate::source::Ingest`]), so the table is not read back for them.
///
/// The first scan's aggregates are kept for every column it would ask about. Its distinct counts
/// are DuckDB's own `approx_count_distinct`, the same HyperLogLog over the same hashes ([`Hll`]),
/// so the same rows choose the same way whichever way they were counted
/// (`a_kept_distinct_count_is_duckdbs`). The pass is kept one way: by each candidate column, when
/// there are at most [`WATCH_KEYS_MAX`] and the first batch's times repeat, or once a candidate's
/// values and the times follow one another as rows sorted by series, then time ([`Order`]); else
/// as one series. Until one of those is known both are kept, so a stream sorted by series is
/// tallied by its key (`a_stream_sorted_by_series_is_tallied_by_its_key`). A cut the profile
/// then makes that was not kept, or whose tally gave up, is read from the table as before, and
/// says the same.
///
/// Measured with bench/ingest.py (docs/PERFORMANCE.md, 2026-10-11): a 10,000,000-row stream of
/// one series, 12.8% fewer instructions in all (exec) and 12.5% (ClickHouse); of 500 series
/// sharing each minute, 15.0%. Keeping every tally, rather than choosing one, saved nothing: the
/// unused one cost what the scan it replaced did.
///
/// One series needs no tally: its rows, extent and measures are the table's, which the scan's
/// aggregates already keep, so only the checksums of its times are added up ([`Series::note`]).
///
/// `profile` uses what is here only for the table it was kept for: the same columns, and the same
/// time column (`the_profile_kept_as_rows_arrive_is_the_profile_of_the_table`).
pub struct Watch {
    columns: Vec<(String, String)>,
    time: Option<usize>,
    candidates: Vec<usize>,
    numeric: Vec<usize>,
    need: Vec<bool>,
    ok: bool,
    total: i64,
    timed: i64,
    first: Option<i64>,
    last: Option<i64>,
    time_hll: Hll,
    /// `None` once the first batch has shown they will not be asked for.
    candidate_hll: Option<Vec<Hll>>,
    /// Each candidate's values, not null: what its distinct count cannot exceed
    /// (`a_distinct_count_never_exceeds_the_values`). Kept beside `candidate_hll`.
    candidate_values: Vec<i64>,
    numbers: Vec<SeriesMeasure>,
    /// The table as one series, while that may be the cut: what [`Series::note`] keeps of its
    /// times. The rest of it is the scan's.
    one: Option<Series>,
    /// The measures the one series examines: the first numeric columns.
    one_nm: usize,
    /// The one series' rows of 0 or null in each of its measures, with a time, for its runs
    /// ([`runs_of`]); `None` past [`WATCH_FLAT_MAX`], when the runs are left to their query.
    one_flat: Option<Flat>,
    one_flat_rows: usize,
    /// Per candidate column guessed as the key: the numeric columns it examines, and its tally.
    keyed: Vec<(usize, Vec<usize>, Tally)>,
    /// Whether the first batch has been seen: it chooses between `one` and `keyed`, or leaves
    /// the choice to `order`.
    chosen: bool,
    /// #54: per tally in `keyed`, how its series and the times have followed one another, while
    /// both `one` and `keyed` are kept; `None` once one of them is chosen.
    order: Option<Vec<Order>>,
    /// #54: per tally in `keyed`, the one value its candidate has held, while the tallies wait
    /// to begin ([`Watch::begin`]); `None` once they have.
    waits: Option<Vec<Wait>>,
    /// Whether the first batch's times repeat: then a time is hashed only when it is not the
    /// row before's ([`Watch::times`]).
    repeating: bool,
    /// Buffers kept from batch to batch, so a batch allocates nothing: each row's time in epoch
    /// milliseconds, and the numeric columns not sent as doubles.
    ms: Vec<i64>,
    converted: Vec<Vec<f64>>,
}

/// Per measure, the rows of 0 or null kept for the runs: series index, time, and whether null.
type Flat = Vec<Vec<(u32, i64, bool)>>;

/// What a [`Watch`] saw, handed with the table to [`compute`].
#[derive(Clone)]
pub struct Seen {
    columns: Vec<(String, String)>,
    time: Option<String>,
    total: i64,
    timed: i64,
    first: Option<i64>,
    last: Option<i64>,
    time_distinct: i64,
    /// Kept unless the first batch's times did not repeat ([`Watch::add`]).
    candidate_distinct: Option<Vec<i64>>,
    /// Each candidate's values, not null, kept with `candidate_distinct`.
    candidate_values: Option<Vec<i64>>,
    numbers: Vec<SeriesMeasure>,
    /// The pass for the table as one series — none, or the one — if it was kept.
    one: Option<(Vec<Series>, Option<Flat>)>,
    keyed: Vec<(String, Vec<String>, Tally)>,
}

impl Seen {
    /// The candidates' distinct counts, as kept.
    #[cfg(test)]
    pub(crate) fn candidate_distinct(&self) -> Option<&[i64]> {
        self.candidate_distinct.as_deref()
    }

    /// The pass, for the table cut into series by `series_cols` and examining `measures`, if it
    /// was kept for that cut.
    fn pass(
        self,
        series_cols: &[String],
        measures: &[&Col],
        step: Option<i64>,
        origin: i64,
        per_time: bool,
    ) -> Option<Pass> {
        if series_cols.is_empty() {
            let (mut series, flat) = self.one?;
            if let Some(st) = step.filter(|s| *s > 0) {
                for s in series.iter_mut() {
                    s.place(st, origin);
                }
            }
            return Some(Pass {
                series,
                times: None,
                flat,
            });
        }
        let [key] = series_cols else {
            return None;
        };
        let (_, _, tally) = self.keyed.into_iter().find(|(c, m, _)| {
            c == key
                && m.len() == measures.len()
                && m.iter().zip(measures).all(|(a, b)| *a == b.name)
        })?;
        let mut p = tally.finish(step, origin);
        if !per_time {
            p.times = None;
        }
        Some(p)
    }
}

impl std::fmt::Debug for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Seen({} rows)", self.total)
    }
}

/// The first `n` columns of `kinds` that `pick` accepts, by index.
fn first_of(kinds: &[Kind], n: usize, pick: impl Fn(Kind) -> bool) -> Vec<usize> {
    (0..kinds.len())
        .filter(|i| pick(kinds[*i]))
        .take(n)
        .collect()
}

/// `$body` with `$a` bound to the string array `$k` holds, the layout matched once a batch rather
/// than once a value.
macro_rules! with_keys {
    ($k:expr, |$a:ident| $body:expr) => {
        match $k {
            Keys::Utf8($a) => $body,
            Keys::Large($a) => $body,
            Keys::View($a) => $body,
        }
    };
}

/// Whether each of `n` rows of candidate column `col` holds `w`, which the first row sets when it
/// is not set yet. A value this cannot compare is a change.
fn holds(col: &Held, n: usize, w: &mut Wait) -> bool {
    if n == 0 {
        return true;
    }
    let nulls = col.nulls;
    match &col.values {
        Values::Text(keys) => with_keys!(keys, |a| {
            if matches!(w, Wait::Unset) {
                *w = if valid(nulls, 0) {
                    Wait::Text(a.value(0).to_string())
                } else {
                    Wait::Null
                };
            }
            match w {
                Wait::Text(s) => (0..n).all(|i| valid(nulls, i) && a.value(i) == s.as_str()),
                Wait::Null => (0..n).all(|i| !valid(nulls, i)),
                _ => false,
            }
        }),
        Values::Int(v) => {
            if matches!(w, Wait::Unset) {
                *w = if valid(nulls, 0) {
                    Wait::Int(v.get(0))
                } else {
                    Wait::Null
                };
            }
            let mut same = true;
            match w {
                Wait::Int(x) => {
                    let x = *x;
                    v.each(n, |i, y| same &= valid(nulls, i) && y == x);
                }
                Wait::Null => same = (0..n).all(|i| !valid(nulls, i)),
                _ => same = false,
            }
            same
        }
        _ => false,
    }
}

impl Watch {
    /// Keep the profile of a table whose columns are `columns`, name and DuckDB type, as the
    /// bench holds them.
    pub fn new(columns: &[(String, String)]) -> Self {
        let kinds: Vec<Kind> = columns.iter().map(|(_, t)| kind(t)).collect();
        // As `profile` chooses them, from the types alone: the time column is the first temporal
        // one unless the operator stated another, and then nothing kept here is used.
        let time = (0..kinds.len()).find(|i| temporal(kinds[*i]));
        let candidates = first_of(&kinds, 8, |k| matches!(k, Kind::Text | Kind::Integer));
        let numeric = first_of(&kinds, COLUMNS_REPORTED, |k| {
            matches!(k, Kind::Integer | Kind::Real)
        });
        let examined = |without: Option<usize>| -> Vec<usize> {
            numeric
                .iter()
                .copied()
                .filter(|c| Some(*c) != without)
                .take(MEASURES_EXAMINED)
                .collect()
        };
        let keyed = if candidates.len() <= WATCH_KEYS_MAX {
            candidates
                .iter()
                .map(|c| {
                    let m = examined(Some(*c));
                    let t = Tally::new(
                        m.len(),
                        true,
                        None,
                        WATCH_TIMES_MAX,
                        WATCH_FLAT_MAX,
                        WATCH_SERIES_MAX,
                    );
                    (*c, m, t)
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut need = vec![false; columns.len()];
        for c in time.iter().chain(&candidates).chain(&numeric) {
            need[*c] = true;
        }
        Watch {
            columns: columns.to_vec(),
            time,
            ok: true,
            total: 0,
            timed: 0,
            first: None,
            last: None,
            time_hll: Hll::default(),
            candidate_hll: Some(vec![Hll::default(); candidates.len()]),
            candidate_values: vec![0; candidates.len()],
            numbers: vec![SeriesMeasure::default(); numeric.len()],
            one: Some(Series::default()),
            one_nm: examined(None).len(),
            one_flat: Some(vec![Vec::new(); examined(None).len()]),
            one_flat_rows: 0,
            keyed,
            chosen: false,
            order: None,
            waits: None,
            repeating: false,
            converted: vec![Vec::new(); numeric.len()],
            candidates,
            numeric,
            need,
            ms: Vec::new(),
        }
    }

    /// Whether [`Watch::add`] needs column `i`.
    pub fn needs(&self, i: usize) -> bool {
        self.need[i]
    }

    /// Stop: what was seen cannot stand for the table (its rows were narrowed, or a column came
    /// in a form this cannot read exactly), and the table will be read instead.
    pub fn give_up(&mut self) {
        self.ok = false;
    }

    /// #54: begin the tallies that waited, each with the one series `one` and the scan's
    /// aggregates kept of the rows so far, `rows` (rows, timed, first, last), before any later
    /// row is added to those: the measures from the scan's, each column's own, and the rows of 0
    /// or null from `one`'s where it examines the same columns. With none so far, each begins
    /// empty.
    fn begin(&mut self, (rows, timed, first, last): (i64, i64, Option<i64>, Option<i64>)) {
        let Some(waits) = self.waits.take() else {
            return;
        };
        if rows == 0 {
            return;
        }
        let Some(one) = self.one.as_ref() else {
            self.keyed.clear();
            return;
        };
        let at = |c: usize| self.numeric.iter().position(|n| *n == c).expect("numeric");
        let one_cols = &self.numeric[..self.one_nm];
        let null = key_words(NULL_PART.as_bytes());
        for ((_, examined, tally), w) in self.keyed.iter_mut().zip(&waits) {
            let key = match w {
                Wait::Text(s) => (s.as_str(), key_words(s.as_bytes())),
                Wait::Int(x) => ("", int_words(*x)),
                Wait::Null | Wait::Unset => (NULL_PART, null),
            };
            let measures: Vec<HotMeasure> = examined
                .iter()
                .map(|e| {
                    let m = &self.numbers[at(*e)];
                    HotMeasure {
                        nulls: m.nulls,
                        zeros: m.zeros,
                        min: m.min.unwrap_or(f64::NAN),
                        max: m.max.unwrap_or(f64::NEG_INFINITY),
                    }
                })
                .collect();
            let flat = self.one_flat.as_ref().and_then(|f| {
                examined
                    .iter()
                    .map(|e| Some(f[one_cols.iter().position(|c| c == e)?].clone()))
                    .collect::<Option<Vec<_>>>()
            });
            tally.begin(key, rows, (timed, first, last), one, &measures, flat);
        }
    }

    /// `n` rows, in the order they went into the table: `cols[i]` is column `i`
    /// ([`Held::of`]), for every column [`Watch::needs`].
    pub fn add(&mut self, n: usize, cols: &[Option<Held>]) {
        if !self.ok {
            return;
        }
        if self
            .need
            .iter()
            .zip(cols)
            .any(|(need, c)| *need && c.is_none())
        {
            self.ok = false;
            return;
        }
        // What the batches before this one came to, should a waiting tally be begun from them.
        let before = (self.total, self.timed, self.first, self.last);
        self.total += n as i64;
        // The time, in epoch milliseconds as `epoch_ms` says it: truncated toward zero. A row
        // without one keeps a 0 in `ms`, never read: `tnulls` says it has none.
        let mut ms = std::mem::take(&mut self.ms);
        ms.clear();
        let tnulls = match self.time.and_then(|t| cols[t].as_ref()) {
            Some(Held {
                values: Values::Time { raw, up, down },
                nulls,
            }) => {
                let (up, down) = (*up, *down);
                // Microseconds, as ClickHouse and most producers send them, need no scaling: the
                // multiplication's check and the division are left out of that loop.
                let scaled = |x: i64| Some(x.checked_mul(up)? / down);
                let kept = match ((up, down) == (1, 1), self.repeating) {
                    (true, false) => self.times::<false>(&raw[..n], *nulls, &mut ms, Some),
                    (true, true) => self.times::<true>(&raw[..n], *nulls, &mut ms, Some),
                    (false, false) => self.times::<false>(&raw[..n], *nulls, &mut ms, scaled),
                    (false, true) => self.times::<true>(&raw[..n], *nulls, &mut ms, scaled),
                };
                if !kept {
                    self.ok = false;
                    return;
                }
                *nulls
            }
            Some(Held {
                values: Values::Days(raw),
                nulls,
            }) => {
                for (i, d) in raw[..n].iter().enumerate() {
                    if !valid(*nulls, i) {
                        ms.push(0);
                        continue;
                    }
                    if *d == i32::MAX || *d == -i32::MAX {
                        self.ok = false;
                        return;
                    }
                    self.time_hll.insert(murmur(*d as u32 as u64));
                    let t = *d as i64 * DAY_MS;
                    ms.push(t);
                    self.timed += 1;
                    self.first = Some(self.first.map_or(t, |f| f.min(t)));
                    self.last = Some(self.last.map_or(t, |l| l.max(t)));
                }
                *nulls
            }
            Some(_) => {
                self.ok = false;
                return;
            }
            None => None,
        };
        let has_time = self.time.is_some();
        let time = |i: usize| (has_time && valid(tnulls, i)).then(|| ms[i]);
        if !self.chosen {
            // One tally, not all: on the first batch, times that repeat say the table will be cut
            // into series, if a candidate was tallied. Times that do not are left to `order`,
            // with both kept until it says which (#54): rows sorted by series show their first
            // batches as one series, and a reading that changes every row beside times that
            // never go back says one series at once. The tally not kept is the pass the profile
            // reads from the table, should the rest of the stream say otherwise.
            self.chosen = true;
            // Counted exactly, once: 64 registers are too few to tell one batch's times apart.
            let mut first: Vec<i64> = (0..n).filter_map(time).collect();
            first.sort_unstable();
            first.dedup();
            let repeats = (first.len() as f64) < 0.95 * self.timed as f64;
            self.repeating = repeats;
            if repeats && !self.keyed.is_empty() {
                self.one = None;
            } else if has_time && !self.keyed.is_empty() {
                self.order = Some(vec![Order::default(); self.keyed.len()]);
                self.waits = Some(vec![Wait::Unset; self.keyed.len()]);
            } else {
                self.keyed.clear();
            }
            // The candidates' distinct counts choose the key only when times repeat, and with
            // a time column the profile asks for them only then. Times that do not repeat here
            // say they will not be asked for: they are not kept, and should the rest of the
            // stream repeat its times after all, the profile counts them from the table
            // (`profile.distinct`), as it would have read the pass. A one-series stream's
            // text column cost 66 of its 164 instructions a row (R2-2). While `order` waits they
            // are kept, as the keyed tallies are.
            if has_time && !repeats && self.order.is_none() {
                self.candidate_hll = None;
            }
        }
        // #54: while every candidate holds the one value it began with, its tally would hold
        // one series, the one `one` keeps, and is not run: only its totals per time are kept,
        // which `one` does not keep. At the first batch where one changes, every tally begins
        // from what `one` and the scan's aggregates kept of the batches before
        // ([`Watch::begin`]), before this batch is added to them. Run all along, the waiting
        // tally cost a 10,000,000-row Parquet table of one series beside a constant text column
        // 2.2 G instructions (+9.9%) and 300 ms of the watch's CPU.
        let mut waiting = false;
        if self.waits.is_some() {
            let waits = self.waits.as_mut().expect("waiting");
            waiting = self
                .keyed
                .iter()
                .zip(waits.iter_mut())
                .all(|((c, _, _), w)| holds(cols[*c].as_ref().expect("needed"), n, w));
            if !waiting {
                self.begin(before);
            }
        }
        // Each numeric column as a double: the cast is monotone, so the range of the doubles is
        // the double of the range, and only 0 casts to 0.
        let mut converted = std::mem::take(&mut self.converted);
        for (c, buf) in self.numeric.iter().zip(converted.iter_mut()) {
            let col = cols[*c].as_ref().expect("needed");
            buf.clear();
            match &col.values {
                Values::Real(_) => {}
                Values::Real32(v) => buf.extend(v[..n].iter().map(|x| *x as f64)),
                Values::Int(v) => {
                    buf.reserve(n);
                    v.each(n, |_, x| buf.push(x as f64));
                }
                _ => {
                    self.ok = false;
                    self.converted = converted;
                    return;
                }
            }
        }
        let doubles: Vec<Doubles> = self
            .numeric
            .iter()
            .zip(&converted)
            .map(|(c, buf)| {
                let col = cols[*c].as_ref().expect("needed");
                Doubles {
                    v: match &col.values {
                        Values::Real(v) => &v[..n],
                        _ => buf.as_slice(),
                    },
                    nulls: col.nulls,
                }
            })
            .collect();
        // The one series' rows of 0 or null are kept as they are counted, for its runs: read from
        // the table instead (`profile.runs`), they were 0.7 G instructions of a 10,000,000-row
        // stream of one series (R2-7). They are few: a row kept is one already counted as 0 or
        // null.
        let keep_flat = has_time && self.one.is_some();
        let (mut flat_rows, mut overflow) = (self.one_flat_rows, false);
        for (j, (acc, d)) in self.numbers.iter_mut().zip(&doubles).enumerate() {
            let mut flat = self
                .one_flat
                .as_mut()
                .filter(|_| keep_flat)
                .and_then(|f| f.get_mut(j));
            let mut keep = |i: usize, null: bool| {
                if let (Some(f), Some(t)) = (flat.as_mut(), time(i)) {
                    f.push((0, t, null));
                    flat_rows += 1;
                    if flat_rows > WATCH_FLAT_MAX {
                        overflow = true;
                        flat = None;
                    }
                }
            };
            // The batch's range in locals, from the sentinels a series' measures start from
            // ([`HotMeasure::NONE`]), folded in once: an `Option` and `duck_cmp` a row before.
            let (mut lo, mut hi, mut values) = (f64::NAN, f64::NEG_INFINITY, 0usize);
            for (i, x) in d.v.iter().enumerate() {
                if !valid(d.nulls, i) {
                    acc.nulls += 1;
                    keep(i, true);
                    continue;
                }
                let x = *x;
                values += 1;
                if x == 0.0 {
                    acc.zeros += 1;
                    keep(i, false);
                }
                if x < lo || (lo.is_nan() && !x.is_nan()) {
                    lo = x;
                }
                if x > hi || (x.is_nan() && !hi.is_nan()) {
                    hi = x;
                }
            }
            if values > 0 {
                if acc.min.is_none_or(|m| duck_cmp(lo, m).is_lt()) {
                    acc.min = Some(lo);
                }
                if acc.max.is_none_or(|m| duck_cmp(hi, m).is_gt()) {
                    acc.max = Some(hi);
                }
            }
        }
        self.one_flat_rows = flat_rows;
        if overflow {
            self.one_flat = None;
        }
        if let Some(one) = self.one.as_mut().filter(|_| has_time) {
            for i in 0..n {
                if let Some(t) = time(i) {
                    one.note(t);
                }
            }
        }
        let at = |c: usize| self.numeric.iter().position(|n| *n == c).expect("numeric");
        // Each candidate's distinct count, kept as DuckDB keeps it. A key that a tally already
        // keeps a series for was counted when that series began, and counting a value twice
        // changes nothing, so a tallied key is hashed once per series rather than once per row:
        // 60 of the 460 instructions a row of 500 series cost (R2-2). `from` is where each
        // column's rows are still to be counted, should its tally give up within the batch.
        let mut from = vec![0; self.candidates.len()];
        let null = key_words(NULL_PART.as_bytes());
        let stamps = has_time.then(|| Stamps {
            v: &ms[..n],
            nulls: tnulls,
        });
        for (ki, (c, examined, tally)) in self.keyed.iter_mut().enumerate() {
            if !tally.alive {
                continue;
            }
            let m: Vec<Doubles> = examined.iter().map(|e| doubles[at(*e)]).collect();
            let col = cols[*c].as_ref().expect("needed");
            let nulls = col.nulls;
            let k = self
                .candidates
                .iter()
                .position(|x| x == c)
                .expect("a candidate");
            let mut h = self.candidate_hll.as_mut().map(|v| &mut v[k]);
            if waiting {
                // The one value, counted once: counting it again changes nothing.
                let w = &self.waits.as_ref().expect("waiting")[ki];
                if let Some(h) = h.as_mut() {
                    match w {
                        Wait::Text(s) => h.insert(hash_bytes(s.as_bytes())),
                        Wait::Int(x) => h.insert(int_hash(*x, self.columns[*c].1 == "HUGEINT")),
                        Wait::Null | Wait::Unset => {}
                    }
                }
                tally.wait(n, stamps, &m);
                from[k] = n;
                continue;
            }
            let took = match &col.values {
                Values::Text(keys) => with_keys!(keys, |a| tally.add(
                    n,
                    |i| if valid(nulls, i) {
                        a.value(i)
                    } else {
                        NULL_PART
                    },
                    |i| if valid(nulls, i) { a.words(i) } else { null },
                    stamps,
                    &m,
                    |i| {
                        if let Some(h) = h.as_mut().filter(|_| valid(nulls, i)) {
                            h.insert(hash_bytes(a.value(i).as_bytes()));
                        }
                    }
                )),
                Values::Int(v) => {
                    // Found by value; only a null's key is text ([`Tally::find`]).
                    let huge = self.columns[*c].1 == "HUGEINT";
                    tally.add(
                        n,
                        |i| if valid(nulls, i) { "" } else { NULL_PART },
                        |i| {
                            if valid(nulls, i) {
                                int_words(v.get(i))
                            } else {
                                null
                            }
                        },
                        stamps,
                        &m,
                        |i| {
                            if let Some(h) = h.as_mut().filter(|_| valid(nulls, i)) {
                                h.insert(int_hash(v.get(i), huge));
                            }
                        },
                    )
                }
                _ => {
                    tally.alive = false;
                    0
                }
            };
            from[k] = took;
        }
        if let Some(hll) = self.candidate_hll.as_mut() {
            for (((h, c), from), values) in hll
                .iter_mut()
                .zip(&self.candidates)
                .zip(from)
                .zip(self.candidate_values.iter_mut())
            {
                let col = cols[*c].as_ref().expect("needed");
                let nulls = col.nulls;
                *values += (n - nulls.map_or(0, |b| b.null_count())) as i64;
                match &col.values {
                    Values::Text(k) => with_keys!(k, |a| {
                        for i in from..n {
                            if valid(nulls, i) {
                                h.insert(hash_bytes(a.value(i).as_bytes()));
                            }
                        }
                    }),
                    Values::Int(v) => {
                        let huge = self.columns[*c].1 == "HUGEINT";
                        v.each(n, |i, x| {
                            if i >= from && valid(nulls, i) {
                                h.insert(int_hash(x, huge));
                            }
                        });
                    }
                    _ => {
                        self.ok = false;
                        return;
                    }
                }
            }
        }
        // #54: while both are kept, the batch's order chooses, once a candidate says it can:
        // the keyed tallies at the first that reads as rows sorted by series, the one series
        // once none can. A tally that gave up says nothing more; one that held every row of the
        // batch has each row's series in `xs`, since a watch's tally is never cut.
        if let (Some(order), Some(st)) = (self.order.as_mut(), stamps) {
            let (mut series, mut more) = (false, false);
            for ((_, _, tally), o) in self.keyed.iter().zip(order.iter_mut()) {
                if waiting {
                    // Every row is the one series'.
                    o.see(std::iter::repeat(0), st);
                } else if tally.alive && tally.xs.len() == n {
                    o.see(tally.xs.iter().copied(), st);
                } else {
                    continue;
                }
                match o.sorted() {
                    Sorted::Series => series = true,
                    Sorted::Waiting => more = true,
                    Sorted::No => {}
                }
            }
            if series {
                self.one = None;
                self.one_flat = None;
                self.order = None;
            } else if !more {
                self.keyed.clear();
                self.candidate_hll = None;
                self.order = None;
                self.waits = None;
            }
        }
        drop(doubles);
        self.converted = converted;
        self.ms = ms;
    }

    /// A batch's timestamps, `raw` with validity `nulls`, each made microseconds by `us`: their
    /// distinct count, extent and number, and each one's epoch milliseconds into `ms` (0 where
    /// there is none). `false` when one cannot be read exactly: it overflows, or it is one of
    /// DuckDB's infinities, which `epoch_ms` makes null.
    #[inline]
    fn times<const REPEATING: bool>(
        &mut self,
        raw: &[i64],
        nulls: Option<&duckdb::arrow::buffer::NullBuffer>,
        ms: &mut Vec<i64>,
        us: impl Fn(i64) -> Option<i64>,
    ) -> bool {
        // The extent kept in locals, folded in once: an `Option` updated per row was most of
        // this loop's instructions after the hash.
        let (mut lo, mut hi, mut timed) = (i64::MAX, i64::MIN, 0i64);
        // When times repeat, a time is hashed only when it is not the row before's: counting a
        // value twice changes nothing, and rows at one time arrive together, 500 a minute in
        // exec_series, where the hash cost 15 instructions a row (R3-4). Where times do not
        // repeat, the check alone cost 7 a row, so it is not made. `i64::MAX` is never a time
        // here.
        let mut before = i64::MAX;
        ms.reserve(raw.len());
        for (i, x) in raw.iter().enumerate() {
            if !valid(nulls, i) {
                ms.push(0);
                continue;
            }
            if *x == i64::MAX || *x == -i64::MAX {
                return false;
            }
            let Some(us) = us(*x) else {
                return false;
            };
            if !REPEATING || us != before {
                self.time_hll.insert(murmur(us as u64));
                before = us;
            }
            // As `epoch_ms` says it: truncated toward zero.
            let t = us / 1000;
            ms.push(t);
            timed += 1;
            lo = lo.min(t);
            hi = hi.max(t);
        }
        if timed > 0 {
            self.timed += timed;
            self.first = Some(self.first.map_or(lo, |f| f.min(lo)));
            self.last = Some(self.last.map_or(hi, |l| l.max(hi)));
        }
        true
    }

    /// What was seen, or `None` if it cannot stand for the table.
    pub fn finish(mut self) -> Option<Seen> {
        if !self.ok {
            return None;
        }
        // Tallies that waited to the end hold one series each, begun here so that each says
        // what it would have, should the profile ask for its cut.
        self.begin((self.total, self.timed, self.first, self.last));
        let name = |c: usize| self.columns[c].0.clone();
        let flat = self.one_flat;
        let one = self.one.map(|mut s| {
            if self.total == 0 {
                return (Vec::new(), flat);
            }
            s.rows = self.total;
            s.timed = self.timed;
            s.first = self.first;
            s.last = self.last;
            s.measures = self.numbers[..self.one_nm].to_vec();
            (vec![s], flat)
        });
        Some(Seen {
            time: self.time.map(name),
            total: self.total,
            timed: self.timed,
            first: self.first,
            last: self.last,
            time_distinct: self.time_hll.count(),
            candidate_values: self
                .candidate_hll
                .is_some()
                .then_some(self.candidate_values),
            candidate_distinct: self
                .candidate_hll
                .map(|h| h.iter().map(Hll::count).collect()),
            numbers: self.numbers,
            one,
            keyed: self
                .keyed
                .into_iter()
                .filter(|(_, _, t)| t.alive)
                .map(|(c, m, t)| (name(c), m.into_iter().map(name).collect(), t))
                .collect(),
            columns: self.columns,
        })
    }
}

/// DuckDB's `approx_count_distinct` (1.5.5, `HyperLogLog`): 64 registers, each the most trailing
/// zeros, plus one, of the hashes it was given past their low 6 bits, and Ertl's estimator
/// ("New cardinality estimation algorithms for HyperLogLog sketches", arXiv:1702.01284) over
/// them. Ported so a count kept as rows arrive is the count DuckDB would give
/// (`a_kept_distinct_count_is_duckdbs`).
#[derive(Clone, Copy)]
struct Hll([u8; 64]);

impl Default for Hll {
    fn default() -> Self {
        Hll([0; 64])
    }
}

impl Hll {
    const P: u32 = 6;
    const Q: usize = 64 - Self::P as usize;
    const M: f64 = 64.0;

    fn insert(&mut self, h: u64) {
        let i = (h & ((1 << Self::P) - 1)) as usize;
        let z = (((h >> Self::P) | (1u64 << Self::Q)).trailing_zeros() + 1) as u8;
        self.0[i] = self.0[i].max(z);
    }

    fn count(&self) -> i64 {
        let mut c = [0u32; Self::Q + 2];
        for k in self.0 {
            c[k as usize] += 1;
        }
        let mut z = Self::M * hll_tau((Self::M - c[Self::Q] as f64) / Self::M);
        for k in (1..=Self::Q).rev() {
            z += c[k] as f64;
            z *= 0.5;
        }
        z += Self::M * hll_sigma(c[0] as f64 / Self::M);
        (0.721_347_520_444_481_7 * Self::M * Self::M / z).round() as i64
    }
}

extern "C" {
    /// The C library's, which DuckDB's own estimator calls: the same last bit.
    fn pow(x: f64, y: f64) -> f64;
}

fn hll_sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let (mut y, mut z) = (1.0, x);
    loop {
        x *= x;
        let before = z;
        z += x * y;
        y += y;
        if before == z {
            return z;
        }
    }
}

fn hll_tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let (mut y, mut z) = (1.0, 1.0 - x);
    loop {
        x = x.sqrt();
        let before = z;
        y *= 0.5;
        z -= unsafe { pow(1.0 - x, 2.0) } * y;
        if before == z {
            return z / 3.0;
        }
    }
}

/// DuckDB's hash of a `BIGINT`, or of a `HUGEINT` (each half hashed, and the two combined).
fn int_hash(x: i128, huge: bool) -> u64 {
    if huge {
        murmur(x as u64) ^ murmur((x >> 64) as u64)
    } else {
        murmur(x as i64 as u64)
    }
}

/// DuckDB's hash of an integer (`MurmurHash64`).
fn murmur(mut x: u64) -> u64 {
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8feb86659fd93);
    x ^= x >> 32;
    x = x.wrapping_mul(0xd6e8feb86659fd93);
    x ^= x >> 32;
    x
}

/// DuckDB's hash of a string's bytes (`HashBytes`).
fn hash_bytes(b: &[u8]) -> u64 {
    let mut h: u64 = 0xe17a1465 ^ (b.len() as u64).wrapping_mul(0xc6a4a7935bd1e995);
    let (words, rest) = b.as_chunks::<8>();
    for w in words {
        h ^= u64::from_le_bytes(*w);
        h = h.wrapping_mul(0xd6e8feb86659fd93);
    }
    if !rest.is_empty() {
        h ^= tail_word(rest);
        h = h.wrapping_mul(0xd6e8feb86659fd93);
    }
    murmur(h)
}

/// The last 1 to 7 bytes of a string as a little-endian word, zero above them: what copying them
/// into a zeroed `[u8; 8]` gives, without the call to `memcpy` a copy of variable length compiles
/// to — one a row, for every key looked up and every text value counted (R2-2).
#[inline]
fn tail_word(r: &[u8]) -> u64 {
    let (mut w, mut at) = (0u64, 0);
    if r.len() & 4 != 0 {
        w = u32::from_le_bytes([r[0], r[1], r[2], r[3]]) as u64;
        at = 4;
    }
    if r.len() & 2 != 0 {
        w |= (u16::from_le_bytes([r[at], r[at + 1]]) as u64) << (8 * at);
        at += 2;
    }
    if r.len() & 1 != 0 {
        w |= (r[at] as u64) << (8 * at);
    }
    w
}

/// What [`pass`] finds for a table without a series key, as one aggregate DuckDB runs on every
/// thread. With one series there is no key to look up and no shared moment to find, and on five
/// million rows the pass's single thread took longer than this aggregate across four (0.3–0.7 s
/// against about 0.4 s, on the development box under load). The runs are left to their query.
fn one_series(cx: &Ctx, measures: &[&Col], step: Option<i64>, origin: i64) -> Option<Vec<Series>> {
    let t = &cx.t;
    let (idx, ph) = match step {
        Some(st) => (
            format!("(({t}) - {origin}) // {st}"),
            format!("(({t}) - {origin}) % {st}"),
        ),
        None => ("0".to_string(), "0".to_string()),
    };
    // The sums are HUGEINT, read as text; squared as BIGINT, an index must stay under 3e9, which
    // the profile checks before it trusts them.
    let mut sel = vec![
        "count(*)::BIGINT".to_string(),
        format!("count({t})::BIGINT"),
        format!("min({t})"),
        format!("max({t})"),
        format!("min({ph})::BIGINT"),
        format!("max({ph})::BIGINT"),
        format!("sum({idx})::VARCHAR"),
        format!("sum(({idx}) * ({idx}))::VARCHAR"),
    ];
    for m in measures {
        let q = quote(&m.name);
        sel.push(format!("(count(*) - count({q}))::BIGINT"));
        sel.push(format!("count(*) FILTER ({q} = 0)::BIGINT"));
        sel.push(format!("min({q})::DOUBLE"));
        sel.push(format!("max({q})::DOUBLE"));
    }
    let text = |s: Option<String>| -> i128 { s.and_then(|s| s.parse().ok()).unwrap_or(0) };
    let s = cx
        .conn
        .query_row(
            &format!("SELECT {} FROM {}", sel.join(", "), cx.from),
            [],
            |r| {
                let mut s = Series {
                    key: String::new(),
                    rows: r.get(0)?,
                    timed: r.get(1)?,
                    first: r.get(2)?,
                    last: r.get(3)?,
                    phase: (
                        r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    ),
                    sum: text(r.get(6)?),
                    sum_sq: text(r.get(7)?),
                    ..Series::default()
                };
                for j in 0..measures.len() {
                    let b = 8 + 4 * j;
                    s.measures.push(SeriesMeasure {
                        nulls: r.get(b)?,
                        zeros: r.get(b + 1)?,
                        min: r.get(b + 2)?,
                        max: r.get(b + 3)?,
                    });
                }
                Ok(s)
            },
        )
        .ok()?;
    Some(vec![s])
}

/// What [`pass`] found.
struct Pass {
    /// Ordered by key, at most [`SERIES_MAX`] + 1.
    series: Vec<Series>,
    times: Option<Times>,
    /// Per examined measure, each row that is 0 or null and has a time: its series' index, the
    /// time, and whether it is null. `None` past [`FLAT_MAX`] rows in all.
    flat: Option<Vec<Vec<(u32, i64, bool)>>>,
}

/// Rows of 0 or null the pass keeps for the runs, across the measures. The query it saves reads
/// at most [`WINDOW_ROWS`] per measure; this holds 16 bytes a row.
const FLAT_MAX: usize = 2 * WINDOW_ROWS as usize;

/// The step most rows follow, measured on the table's first slice of time — sized to hold about
/// [`STEP_SAMPLE_ROWS`] rows if rows are spread evenly, and widened if it holds too few to tell.
/// Every series is in it. A filter on the time column skips most of the table unread; ordering
/// to find a sample did not (measured on five million rows: a top-N took 0.15–0.5 s and the
/// filter 0.05 s). The mode, ties to the shorter step, so the same data always gives the same
/// answer.
fn measure_step(
    cx: &Ctx,
    raw: &str,
    first: &str,
    origin: i64,
    end: i64,
    total: i64,
) -> Option<i64> {
    let mut width = sample_width(origin, end, total);
    for _ in 0..4 {
        let sql = format!(
            "SELECT d, count(*) FROM (SELECT t - lag(t) OVER (PARTITION BY k ORDER BY t) AS d \
               FROM (SELECT {k} AS k, {t} AS t FROM {f} \
                     WHERE {raw} < {first} + to_milliseconds({width}))) \
             WHERE d > 0 GROUP BY d ORDER BY count(*) DESC, d LIMIT 1",
            k = cx.k,
            t = cx.t,
            f = cx.from,
        );
        if let Ok((d, n)) = cx
            .conn
            .query_row(&sql, [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
        {
            if n >= 3 || width >= end - origin {
                return Some(d).filter(|d| *d > 0);
            }
        }
        if width >= end - origin {
            return None;
        }
        width = width.saturating_mul(16);
    }
    None
}

/// How much of the table's first slice of time holds about [`STEP_SAMPLE_ROWS`] rows, in
/// milliseconds, if rows are spread evenly.
fn sample_width(origin: i64, end: i64, total: i64) -> i64 {
    ((end - origin) as i128 * STEP_SAMPLE_ROWS as i128 / total.max(1) as i128)
        .clamp(1, i64::MAX as i128) as i64
}

/// The first time of column `raw` as a literal of its own type, exactly: compared with a
/// constant, the filter of a sample goes into the scan, where each row group's range can skip
/// it unread. Compared with a subquery's answer it cannot.
fn first_literal(conn: &Connection, from: &str, raw: &str) -> Option<String> {
    let (first, ty): (String, String) = conn
        .query_row(
            &format!("SELECT min({raw})::VARCHAR, typeof(min({raw})) FROM {from}"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()?;
    Some(format!("CAST({} AS {ty})", literal(&first)))
}

/// How well column `col` cuts the table's first slice of time, `width` wide, into series that
/// keep a step: the share of its rows that follow the row before in their series by the most
/// common distance. A device id's series are nearly all on their step; a reading's values, taken
/// as series, are a scatter of distances, repeats among them. One slice, as [`measure_step`]
/// reads, so the filter skips most of the table unread; asked only of the columns that could be
/// the key, and only when there are two of a kind.
fn step_share(
    conn: &Connection,
    from: &str,
    col: &str,
    t: &str,
    raw: &str,
    first: &str,
    width: i64,
) -> Option<f64> {
    let sql = format!(
        "SELECT max(n)::DOUBLE, sum(n)::DOUBLE FROM (SELECT d, count(*) AS n FROM \
           (SELECT t - lag(t) OVER (PARTITION BY k ORDER BY t) AS d \
            FROM (SELECT {col} AS k, {t} AS t FROM {from} \
                  WHERE {raw} < {first} + to_milliseconds({width}))) \
         WHERE d IS NOT NULL GROUP BY d)"
    );
    let v = values(conn, &sql)?;
    match (v.first().copied().flatten(), v.get(1).copied().flatten()) {
        (Some(most), Some(all)) if all > 0.0 => Some(most / all),
        _ => Some(0.0),
    }
}

/// The series key of a table too small for the ten-rows rule, or `None`. The bundled example
/// has eight rows in two series, so no column left each series ten rows, and its repeated times
/// read as repeats of one series. A column is the key when every one of its values has rows at
/// two times or more, and it tells apart nearly every row that shares a time with another: with
/// the time it is unique in at least 80% of the timed rows, and more so than the time alone.
/// Exact counts, over at most [`WINDOW_ROWS`] rows, and asked only when the ten-rows rule found
/// nothing; the most rows told apart wins, the leftmost on a tie.
fn few_rows_key(
    conn: &Connection,
    from: &str,
    time: &str,
    names: &[String],
    (timed, time_distinct): (i64, i64),
) -> Option<String> {
    let t = quote(time);
    let mut best: Option<(i64, &String)> = None;
    for n in names {
        let c = quote(n);
        let sql = format!(
            "SELECT count(DISTINCT ({c}, {t})) FILTER ({t} IS NOT NULL)::BIGINT, \
               (SELECT min(r) FROM (SELECT count(DISTINCT {t})::BIGINT AS r FROM {from} \
                GROUP BY {c}))::BIGINT FROM {from}"
        );
        let Ok((pairs, least)) = conn.query_row(&sql, [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))
        }) else {
            continue;
        };
        let told_apart = pairs * 5 >= timed * 4 && pairs > time_distinct;
        if least.unwrap_or(0) >= 2 && told_apart && best.is_none_or(|(b, _)| pairs > b) {
            best = Some((pairs, n));
        }
    }
    best.map(|(_, n)| n.clone())
}

/// A series that starts after the rest or ends before them.
fn note_extent(
    s: &mut Series,
    first: Option<i64>,
    last: Option<i64>,
    step: Option<i64>,
    clock: Clock,
    keyed: bool,
) {
    let slack = step.unwrap_or(0);
    if !keyed {
        return;
    }
    if let (Some(a), Some(f)) = (s.first, first) {
        if a > f + slack {
            s.notes.push((
                (a - f) as u64,
                format!(
                    "first row {}, {} after the earliest",
                    clock.at(a),
                    span(a - f)
                ),
            ));
        }
    }
    if let (Some(b), Some(l)) = (s.last, last) {
        if b + slack < l {
            s.notes.push((
                (l - b) as u64,
                format!(
                    "last row {}, {} before the latest",
                    clock.at(b),
                    span(l - b)
                ),
            ));
        }
    }
}

/// Series that fit their step ([`Series::fits`]) but whose times came in no order, so a
/// repeat could hide a gap: regular exactly when they have as many distinct times as rows. One
/// `GROUP BY` over those series alone, by an exact distinct count; a table whose series' times
/// come in order as it is read, oldest or newest first, asks nothing here. The meter data
/// shuffled row by row paid 334 ms of CPU for it, against 257 ms for the rest of its profile;
/// `SELECT DISTINCT k, t` counted per key took 382 ms.
fn settle(cx: &Ctx, series: &[Series], regular: &mut [bool], step: i64, origin: i64) {
    let unsure: std::collections::HashMap<&str, usize> = (0..series.len())
        .filter(|i| !regular[*i] && series[*i].fits(step, origin))
        .map(|i| (series[i].key.as_str(), i))
        .collect();
    if unsure.is_empty() {
        return;
    }
    // Past as many keys as the gaps' window lists, the whole table is counted rather than a
    // list that long parsed.
    let only = if unsure.len() <= 2000 {
        let mut keys: Vec<String> = unsure.keys().map(|k| literal(k)).collect();
        keys.sort();
        format!(" AND {} IN ({})", cx.k, keys.join(", "))
    } else {
        String::new()
    };
    let sql = format!(
        "SELECT {k} AS k, count(DISTINCT {t})::BIGINT FROM {f} WHERE {t} IS NOT NULL{only} \
         GROUP BY ALL",
        k = cx.k,
        t = cx.t,
        f = cx.from,
    );
    let found = rows(cx.conn, &sql, |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })
    .unwrap_or_default();
    for (k, distinct) in found {
        if let Some(i) = unsure.get(k.as_str()) {
            regular[*i] = distinct == series[*i].timed;
        }
    }
}

/// Where irregular series break their step: gaps, repeated timestamps, rows off the step. A
/// bounded window over those series alone; the rest were proven regular without one.
fn gaps(cx: &Ctx, series: &mut [Series], regular: &[bool], step: i64, clock: Clock) {
    let mut pick: Vec<usize> = (0..series.len())
        .filter(|i| !regular[*i] && series[*i].timed >= 2)
        .collect();
    if pick.is_empty() {
        return;
    }
    // The most rows off from what their extent holds first.
    pick.sort_by_key(|i| {
        let s = &series[*i];
        let expected = match (s.first, s.last) {
            (Some(a), Some(b)) => (b - a) / step + 1,
            _ => 0,
        };
        std::cmp::Reverse((expected - s.timed).abs())
    });
    let mut keys = Vec::new();
    let mut n = 0u64;
    let mut skipped = Vec::new();
    for i in &pick {
        let rows = series[*i].timed as u64;
        if n + rows > WINDOW_ROWS || keys.len() >= 2000 {
            skipped.push(*i);
            continue;
        }
        n += rows;
        keys.push(literal(&series[*i].key));
    }
    if !keys.is_empty() {
        // Counted per series by DuckDB, not listed row by row: a list cut at a limit counted
        // only the anomalies before the cut, as if exact, and left the series after it
        // without notes (`anomalies_past_any_limit_are_counted_exactly`). The longest gap is
        // the earliest of the longest, as the list in time order found it.
        let sql = format!(
            "SELECT k, count(*) FILTER (d = 0)::BIGINT, \
               count(*) FILTER (g)::BIGINT, coalesce(sum(d // {step} - 1) FILTER (g), 0)::BIGINT, \
               count(*) FILTER (d <> 0 AND NOT g)::BIGINT, \
               arg_max(p, (d, -p)) FILTER (g), max(d) FILTER (g) \
             FROM (SELECT k, p, t - p AS d, t - p > {step} AND (t - p) % {step} = 0 AS g \
               FROM (SELECT {k} AS k, {t} AS t, lag({t}) OVER (PARTITION BY {k} ORDER BY {t}) \
                 AS p FROM {f} WHERE {k} IN ({keys}) AND {t} IS NOT NULL) \
               WHERE p IS NOT NULL AND t - p <> {step}) \
             GROUP BY k",
            k = cx.k,
            t = cx.t,
            f = cx.from,
            keys = keys.join(", ")
        );
        let found = rows(cx.conn, &sql, |r| {
            Ok((
                r.get::<_, String>(0)?,
                [
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ],
                (r.get::<_, Option<i64>>(5)?, r.get::<_, Option<i64>>(6)?),
            ))
        })
        .unwrap_or_default();
        for (key, [dups, gaps, missing, off], longest) in found {
            let (dups, gaps, off) = (dups as u64, gaps as u64, off as u64);
            let longest = match longest {
                (Some(p), Some(d)) => (p, p + d),
                _ => (0, 0),
            };
            let Some(s) = series.iter_mut().find(|s| s.key == key) else {
                continue;
            };
            if off * 2 > s.timed as u64 {
                s.notes
                    .push((u64::MAX - 2, format!("rows are not {} apart", span(step))));
                continue;
            }
            if gaps > 0 {
                let (p, t) = longest;
                s.notes.push((
                    (missing * step) as u64,
                    format!(
                        "{}, {} missing in all; the longest {} (no rows after {} until {})",
                        count(gaps, "gap", "gaps"),
                        count(missing as u64, "row", "rows"),
                        span(t - p - step),
                        clock.at(p),
                        clock.at(t)
                    ),
                ));
            }
            if dups > 0 {
                s.notes.push((
                    dups * step as u64,
                    format!(
                        "{} a time already present",
                        count(dups, "row repeats", "rows repeat")
                    ),
                ));
            }
            if off > 0 {
                s.notes.push((
                    off * step as u64,
                    format!("{} off the step", count(off, "row is", "rows are")),
                ));
            }
        }
    }
    // Said, not dropped: these series are irregular, and only where is unknown.
    for i in skipped {
        series[i].notes.push((
            1,
            format!(
                "not a row every {} throughout (where, not examined: too many rows)",
                span(step)
            ),
        ));
    }
}

/// Runs of 0 or null in measure `j` lasting at least `threshold`: a bounded window over only the
/// rows that are 0 or null, in the series that have enough of them for such a run.
fn runs(
    cx: &Ctx,
    series: &mut [Series],
    j: usize,
    name: &str,
    (step, threshold): (i64, i64),
    clock: Clock,
    flat: Option<&[(u32, i64, bool)]>,
) {
    let need = (threshold / step).max(1);
    let mut keys = Vec::new();
    let mut picked = vec![false; series.len()];
    let mut n = 0u64;
    for (i, s) in series.iter().enumerate() {
        let m = &s.measures[j];
        let flat = m.zeros + m.nulls;
        if flat < need || m.zeros == s.rows || m.nulls == s.rows {
            continue; // too few for a run, or the whole series, already said
        }
        if n + flat as u64 > WINDOW_ROWS {
            break;
        }
        n += flat as u64;
        keys.push(literal(&s.key));
        picked[i] = true;
    }
    if keys.is_empty() {
        return;
    }
    let found = match flat {
        Some(rows) => runs_of(rows, &picked, series, step, threshold),
        None => runs_sql(cx, &keys, name, step, threshold),
    };
    word_runs(series, &found, name, step, clock);
}

/// [`runs`] from the rows the pass kept — each 0 or null of this measure with a time, by series
/// index — without a query: the window the query runs, on what is already in memory. Ordered by
/// time and then null after 0, where the query leaves rows at the same time in no set order.
/// Like the query's running sum, rows at the same time share the count of breaks up to the last
/// of them.
fn runs_of(
    rows: &[(u32, i64, bool)],
    picked: &[bool],
    series: &[Series],
    step: i64,
    threshold: i64,
) -> Vec<(String, bool, i64, i64)> {
    let mut these: Vec<(u32, i64, bool)> = rows
        .iter()
        .filter(|r| picked.get(r.0 as usize) == Some(&true))
        .copied()
        .collect();
    these.sort_unstable();
    let mut found = Vec::new();
    for one in these.chunk_by(|a, b| a.0 == b.0) {
        // Each row's break, then each row's group: the breaks so far, peers included.
        let mut groups: Vec<(bool, i64, i64)> = Vec::with_capacity(one.len());
        let mut breaks = 0i64;
        let mut i = 0;
        while i < one.len() {
            let mut end = i;
            while end < one.len() && one[end].1 == one[i].1 {
                let brk =
                    end == 0 || one[end].1 - one[end - 1].1 > step || one[end].2 != one[end - 1].2;
                breaks += i64::from(brk);
                end += 1;
            }
            for r in &one[i..end] {
                groups.push((r.2, breaks, r.1));
            }
            i = end;
        }
        // GROUP BY z, group: the first and last time of each.
        let mut spans: std::collections::BTreeMap<(bool, i64), (i64, i64)> = Default::default();
        for (z, g, t) in groups {
            let e = spans.entry((z, g)).or_insert((t, t));
            e.0 = e.0.min(t);
            e.1 = e.1.max(t);
        }
        let key = &series[one[0].0 as usize].key;
        let mut kept: Vec<(String, bool, i64, i64)> = spans
            .into_iter()
            .filter(|(_, (a, e))| e - a + step >= threshold)
            .map(|((z, _), (a, e))| (key.clone(), z, a, e))
            .collect();
        kept.sort_by_key(|r| r.2);
        found.extend(kept);
        if found.len() > 20_001 {
            break;
        }
    }
    found.truncate(20_001);
    found
}

/// [`runs`] by query, when the pass kept no rows for it.
fn runs_sql(
    cx: &Ctx,
    keys: &[String],
    name: &str,
    step: i64,
    threshold: i64,
) -> Vec<(String, bool, i64, i64)> {
    #[cfg(test)]
    RUNS_READ.with(|r| r.set(r.get() + 1));
    let q = quote(name);
    let sql = format!(
        "WITH f AS (SELECT {k} AS k, {t} AS t, ({q} IS NULL) AS z FROM {from} \
           WHERE {k} IN ({keys}) AND {t} IS NOT NULL AND ({q} IS NULL OR {q} = 0)), \
         b AS (SELECT k, t, z, CASE WHEN lag(t) OVER w IS NULL OR t - lag(t) OVER w > {step} \
                OR z <> lag(z) OVER w THEN 1 ELSE 0 END AS brk FROM f \
                WINDOW w AS (PARTITION BY k ORDER BY t)), \
         g AS (SELECT k, t, z, sum(brk) OVER (PARTITION BY k ORDER BY t) AS g FROM b) \
         SELECT k, z, min(t) AS a, max(t) AS e FROM g GROUP BY k, z, g \
         HAVING max(t) - min(t) + {step} >= {threshold} ORDER BY k, a LIMIT 20001",
        k = cx.k,
        t = cx.t,
        from = cx.from,
        keys = keys.join(", ")
    );
    rows(cx.conn, &sql, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })
    .unwrap_or_default()
}

/// What [`runs`] found, ordered by series and start, as each series' note.
fn word_runs(
    series: &mut [Series],
    found: &[(String, bool, i64, i64)],
    name: &str,
    step: i64,
    clock: Clock,
) {
    let shown = ident(name);
    let mut i = 0;
    while i < found.len() {
        let key = found[i].0.clone();
        let Some(s) = series.iter_mut().find(|s| s.key == key) else {
            i += 1;
            continue;
        };
        let mut parts = Vec::new();
        let mut weight = 0u64;
        let (mut runs_null, mut runs_zero) = (0, 0);
        while i < found.len() && found[i].0 == key {
            let (_, z, a, e) = found[i];
            let len = e - a + step;
            weight += len as u64;
            if z {
                runs_null += 1;
            } else {
                runs_zero += 1;
            }
            if parts.len() < RUNS_LISTED {
                let from = if Some(a) == s.first {
                    format!("from the start ({})", clock.at(a))
                } else {
                    format!("from {}", clock.at(a))
                };
                let to = if Some(e) == s.last {
                    format!("to the end ({})", clock.at(e))
                } else {
                    format!("to {}", clock.at(e))
                };
                parts.push(format!(
                    "{} {from} {to} ({})",
                    if z { "null" } else { "0" },
                    span(len)
                ));
            }
            i += 1;
        }
        // As many runs as fit a line, the first ones and the last, with what is left out in
        // between said rather than cut through (`a_long_line_is_elided_between_items_never_\
        // inside_one`). The key goes in front of this note, so it is given its room.
        let total = runs_null + runs_zero;
        let room = LINE_MAX.saturating_sub(VALUE_MAX + 40 + shown.len());
        let listed =
            elide(&parts, ", then ", ", then … then ", room).unwrap_or_else(|| parts[0].clone());
        let shown_all = parts.len() == total && listed == parts.join(", then ");
        let more = if shown_all {
            String::new()
        } else {
            format!(", {total} runs in all")
        };
        s.notes.push((weight, format!("{shown} is {listed}{more}")));
    }
}

/// Moments every series shares: a time when every series that has a row reads 0, or when the
/// total across series is at least 1.8 times — or at most 1/1.8 of — the total at the same time a
/// day before and a day after. Reported only when rare (at most 1% of times); a pattern that
/// recurs, such as night on solar panels, is not a moment.
fn shared_moments(cx: &Ctx, measures: &[&Col], step: i64, clock: Clock) -> Vec<String> {
    let mut out = Vec::new();
    for m in measures {
        let q = quote(&m.name);
        let sql = format!(
            "WITH g AS MATERIALIZED (SELECT {t} AS t, count(*) AS n, count(*) FILTER ({q} = 0) AS z, \
               sum({q})::DOUBLE AS s FROM {from} WHERE {t} IS NOT NULL GROUP BY 1), \
             total AS (SELECT count(*) AS c, max(n) AS most FROM g) \
             SELECT g.t, CASE WHEN g.z = g.n THEN 0 ELSE 1 END AS kind, \
               g.s / ((a.s + b.s) / 2) AS ratio, total.c \
             FROM g CROSS JOIN total \
               LEFT JOIN g a ON a.t = g.t - {DAY_MS} LEFT JOIN g b ON b.t = g.t + {DAY_MS} \
             WHERE (g.z = g.n AND g.n >= 3 AND g.n * 2 >= total.most) \
                OR (g.z < g.n AND a.s > 0 AND b.s > 0 AND g.s > 0 \
                    AND (g.s >= 1.8 * greatest(a.s, b.s) OR g.s * 1.8 <= least(a.s, b.s))) \
             ORDER BY kind, g.t LIMIT 20001",
            t = cx.t,
            from = cx.from
        );
        let Ok(found) = rows(cx.conn, &sql, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }) else {
            continue;
        };
        word_moments(&found, &m.name, step, clock, &mut out);
    }
    out
}

/// What the moments query found for measure `name` — each time, whether every series read 0
/// there (0) or the total was out of line (1), the ratio, and how many times there are, ordered
/// by kind and time — as lines.
fn word_moments(
    found: &[(i64, i64, Option<f64>, i64)],
    name: &str,
    step: i64,
    clock: Clock,
    out: &mut Vec<String>,
) {
    let name = ident(name);
    for kind in [0i64, 1] {
        let these: Vec<(i64, f64)> = found
            .iter()
            .filter(|f| f.1 == kind)
            .map(|f| (f.0, f.2.unwrap_or(0.0)))
            .collect();
        let Some(times) = found.first().map(|f| f.3) else {
            continue;
        };
        if these.is_empty() || these.len() as i64 * 100 > times {
            continue;
        }
        // consecutive times, one step apart, are one moment
        let mut ranges: Vec<(i64, i64, f64, usize)> = Vec::new();
        for (t, r) in these {
            match ranges.last_mut() {
                Some(last) if t - last.1 == step => {
                    last.1 = t;
                    last.2 += r;
                    last.3 += 1;
                }
                _ => ranges.push((t, t, r, 1)),
            }
        }
        let moments = ranges.len();
        for (a, b, r, n) in ranges.into_iter().take(EVENTS_KEPT) {
            let what = if kind == 0 {
                format!("{name} is 0 in every series")
            } else {
                format!(
                    "{name} summed over the series is {}× the same time a day before and \
                         after",
                    num((r / n as f64 * 10.0).round() / 10.0)
                )
            };
            let mut l = format!(
                "{what} {} ({})",
                clock.range(a, b),
                count(n as u64, "time", "times")
            );
            if b - a < 2 * 3_600_000 {
                if let Some(c) = clock_change(a) {
                    l.push_str(&format!(" — {}, {c}", clock.at(a).get(..10).unwrap_or("")));
                }
            }
            out.push(l);
        }
        if moments > EVENTS_KEPT {
            out.push(format!("… and {} more such moments", moments - EVENTS_KEPT));
        }
    }
}

/// Without a time column: numeric ranges, and the text columns' distinct counts, with their values
/// when there are few.
fn describe_columns(
    conn: &Connection,
    from: &str,
    numeric: &[String],
    candidates: &[&(String, Kind)],
    // Each candidate's estimated distinct count, and its count of values where known.
    (distinct, counted): (&[i64], &[Option<i64>]),
    total: i64,
    p: &mut Profile,
) {
    p.lines.extend(numeric.iter().cloned());
    // REQ-14 `a_distinct_count_never_exceeds_the_values`: the estimate is HyperLogLog, which
    // can say more values than the column has: 24 of 20, measured on DuckDB 1.5.5. Each is held
    // to the column's count of values, which the first scan reads beside it, or a stream counted
    // as it went in (asked here only if neither did: a separate count of a 1M-row table's two
    // text columns cost 200 M instructions, 3% of its load). Where the estimate reaches that
    // count, a table of up to `EXACT_ROWS` rows has its count made exact; making every count
    // exact cost a 50,000-row table 14% more instructions to load, this one query only where an
    // estimate was too high. Measured: loads of 50,000 and 1,000,000 rows without a time column
    // within the noise of before. An estimate too low stays "about": 50 values on 50 rows read
    // "about 47 distinct". Counting one within a quarter below exactly was measured (a whole
    // session to its `describe`, five runs, medians, DuckDB 1.5.5) and not taken: 50,000 unique
    // names on 50,000 rows went from 976 M to 1,047 M instructions, 7% of the load for a
    // cosmetic gain.
    const EXACT_ROWS: i64 = 100_000;
    let shown: Vec<usize> = (0..candidates.len())
        .filter(|i| candidates[*i].1 == Kind::Text && distinct[*i] >= 1)
        .collect();
    let mut values_of: Vec<Option<i64>> = (0..candidates.len())
        .map(|i| counted.get(i).copied().flatten())
        .collect();
    let missing: Vec<usize> = shown
        .iter()
        .copied()
        .filter(|i| values_of[*i].is_none())
        .collect();
    let ask = |agg: &str, which: &[usize]| -> Vec<Option<i64>> {
        if which.is_empty() {
            return Vec::new();
        }
        let sql = format!(
            "SELECT {} FROM {from}",
            which
                .iter()
                .map(|i| format!("{}::DOUBLE", agg.replace("{}", &quote(&candidates[*i].0))))
                .collect::<Vec<_>>()
                .join(", ")
        );
        values(conn, &sql)
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.map(|v| v as i64))
            .collect()
    };
    for (i, n) in missing.iter().zip(ask("count({})", &missing)) {
        values_of[*i] = n;
    }
    let over: Vec<usize> = shown
        .iter()
        .copied()
        .filter(|i| values_of[*i].is_some_and(|n| distinct[*i] >= n))
        .collect();
    let mut exactly: Vec<Option<i64>> = vec![None; candidates.len()];
    if total <= EXACT_ROWS {
        for (i, n) in over.iter().zip(ask("count(DISTINCT {})", &over)) {
            exactly[*i] = n;
        }
    }
    let mut listed = 0;
    for (i, (name, k)) in candidates.iter().enumerate() {
        let d = distinct[i];
        if *k != Kind::Text || d < 1 {
            continue;
        }
        let (d, is_exact) = match (exactly[i], values_of[i]) {
            (Some(n), _) => (n, true),
            (None, Some(n)) => (d.min(n), false),
            (None, None) => (d, false),
        };
        if d < 1 {
            continue;
        }
        let shown = ident(name);
        if d <= 12 && total >= 2 * d && listed < 3 {
            listed += 1;
            let sql = format!(
                "SELECT DISTINCT CAST({q} AS VARCHAR) FROM {from} WHERE {q} IS NOT NULL \
                 ORDER BY 1 LIMIT 13",
                q = quote(name)
            );
            if let Ok(values) = rows(conn, &sql, |r| r.get::<_, String>(0)) {
                if values.len() <= 12 {
                    p.lines.push(format!(
                        "{shown}: {} distinct ({})",
                        values.len(),
                        values
                            .iter()
                            .map(|v| value(v))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    continue;
                }
            }
        }
        p.lines.push(if is_exact {
            format!("{shown}: {d} distinct")
        } else {
            format!("{shown}: about {d} distinct")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A week of 15-minute readings from seven meters with the quirks the profile looks for: a
    /// dead meter, a late start, a stop into nulls, a pause, an hour every meter reads 0, an hour
    /// every meter reads double, and two rows at one time, one 0 and one null.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "SET TimeZone = 'UTC';
             CREATE TABLE r AS
             SELECT TIMESTAMPTZ '2014-03-27 00:00:00+00' + to_minutes(i * 15) AS ts,
                    'm' || s AS meter,
                    CASE WHEN s = 1 THEN 0
                         WHEN s = 2 AND i < 140 THEN 0
                         WHEN s = 3 AND i > 500 THEN NULL
                         WHEN s = 4 AND i BETWEEN 300 AND 420 THEN 0
                         WHEN i BETWEEN 200 AND 203 THEN 0
                         WHEN i BETWEEN 260 AND 263 THEN 2 * (s * 10 + (i % 96) * 0.5 + 1)
                         ELSE s * 10 + (i % 96) * 0.5 + 1 END::DOUBLE AS kw
             FROM range(672) a(i), range(1, 8) b(s);
             INSERT INTO r VALUES (TIMESTAMPTZ '2014-03-29 06:00:00+00', 'm5', 0),
                                  (TIMESTAMPTZ '2014-03-29 06:00:00+00', 'm5', NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn the_pass_finds_what_the_queries_find() {
        // The moments and the runs are worked out from the one pass; their queries are what a
        // table too large for the pass falls back on. Both must say the same of the same data.
        let conn = fixture();
        let cx = Ctx {
            conn: &conn,
            from: "\"r\"".into(),
            t: "epoch_ms(\"ts\")".into(),
            k: "coalesce(CAST(\"meter\" AS VARCHAR), chr(30))".into(),
        };
        let col = Col {
            name: "kw".into(),
            min: None,
            max: None,
            nulls: 0,
            zeros: 0,
        };
        let origin: i64 = conn
            .query_row("SELECT min(epoch_ms(ts)) FROM r", [], |r| r.get(0))
            .unwrap();
        let step = 900_000;
        let clock = Clock(Kind::TimestampTz);
        let p = pass(&cx, &[&col], Some(step), origin, true, true).unwrap();
        assert_eq!(p.series.len(), 7);
        let times = p.times.expect("kept");
        let from_pass = times.moments(&[&col], step, clock);
        assert!(from_pass.len() >= 2, "{from_pass:?}");
        assert_eq!(from_pass, shared_moments(&cx, &[&col], step, clock));

        let flat = p.flat.expect("kept");
        let keys: Vec<String> = p.series.iter().map(|s| literal(&s.key)).collect();
        let picked = vec![true; p.series.len()];
        let from_pass = runs_of(&flat[0], &picked, &p.series, step, DAY_MS);
        assert!(from_pass.len() >= 4, "{from_pass:?}");
        assert_eq!(from_pass, runs_sql(&cx, &keys, "kw", step, DAY_MS));
    }

    #[test]
    fn a_kept_distinct_count_is_duckdbs() {
        // The counts kept as a stream goes in choose the time column's uniqueness and the series
        // key; they must be the counts DuckDB's `approx_count_distinct` gives the table, for
        // every type the profile counts and from none to hundreds of thousands of values.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("SET TimeZone = 'UTC'").unwrap();
        let mut checked = 0;
        for n in [0i64, 1, 2, 7, 64, 100, 1_000, 4_096, 30_000, 250_000] {
            for (what, expr) in [
                ("bigint", format!("(i * 7919 - {n} * 31)::BIGINT")),
                (
                    "hugeint",
                    "(18446744073709551615 - i * 104729)::UBIGINT::HUGEINT".into(),
                ),
                (
                    "timestamptz",
                    "make_timestamptz((i * 60000037 - 900000000000)::BIGINT)".into(),
                ),
                ("date", "(DATE '1969-12-25' + (i * 3)::INTEGER)".into()),
                ("short text", "'m' || (i * 13)".into()),
                (
                    "text",
                    "'Ærø meter number ' || i || repeat('x', (i % 23)::INTEGER)".into(),
                ),
                (
                    "empty text",
                    "CASE WHEN i % 2 = 0 THEN '' ELSE 'abcdefgh' || i END".into(),
                ),
            ] {
                let src = format!("(SELECT {expr} AS x FROM range({n}) r(i))");
                let want: i64 = conn
                    .query_row(
                        &format!("SELECT approx_count_distinct(x) FROM {src}"),
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                let mut h = Hll::default();
                let cols = match what {
                    "timestamptz" => "NULL, epoch_us(x), NULL",
                    "date" => "NULL, NULL, x - DATE '1970-01-01'",
                    _ => "x::VARCHAR, NULL, NULL",
                };
                let mut st = conn.prepare(&format!("SELECT {cols} FROM {src}")).unwrap();
                let mut rows = st.query([]).unwrap();
                while let Some(r) = rows.next().unwrap() {
                    h.insert(match what {
                        "bigint" | "hugeint" => {
                            let x: i128 = r.get::<_, String>(0).unwrap().parse().unwrap();
                            int_hash(x, what == "hugeint")
                        }
                        "timestamptz" => murmur(r.get::<_, i64>(1).unwrap() as u64),
                        "date" => murmur(r.get::<_, i32>(2).unwrap() as u32 as u64),
                        _ => hash_bytes(r.get::<_, String>(0).unwrap().as_bytes()),
                    });
                }
                assert_eq!(h.count(), want, "{what}, {n} values");
                checked += 1;
            }
        }
        assert_eq!(checked, 70);
    }

    #[test]
    fn a_cycle_of_series_costs_one_key_compare_a_row() {
        // R3-4: meters read in a fixed cycle, every meter at each minute, as exec_series-10m's
        // 500 are. The row after one of series x is one of series x + 1, so that is tried
        // first, and a row costs one key compare, not a miss on the last row's series and then
        // a hit on the next. Batches end anywhere in the cycle.
        let keys: Vec<String> = (0..500).map(|k| format!("m{k:03}")).collect();
        let (rounds, batch) = (40, 4_096);
        let n = 500 * rounds;
        let mut tally = Tally::new(1, true, None, TIMES_MAX, FLAT_MAX, usize::MAX);
        KEY_COMPARES.with(|c| c.set(0));
        for from in (0..n).step_by(batch) {
            let len = batch.min(n - from);
            let ts: Vec<i64> = (from..from + len)
                .map(|i| (i / 500) as i64 * 60_000)
                .collect();
            let kw: Vec<f64> = (from..from + len).map(|i| (i % 7) as f64).collect();
            tally.add(
                len,
                |i| keys[(from + i) % 500].as_str(),
                |i| key_words(keys[(from + i) % 500].as_bytes()),
                Some(Stamps {
                    v: &ts,
                    nulls: None,
                }),
                &[Doubles {
                    v: &kw,
                    nulls: None,
                }],
                |_| {},
            );
        }
        let compares = KEY_COMPARES.with(|c| c.get());
        // The first round finds each series new, and a cycle's wrap from the last series to
        // the first is one more.
        assert!(
            compares <= n + 2 * 500,
            "{compares} key compares for {n} rows"
        );
        let p = tally.finish(Some(60_000), 0);
        assert_eq!(p.series.len(), 500);
        for s in &p.series {
            assert_eq!(
                (s.rows, s.timed),
                (rounds as i64, rounds as i64),
                "{}",
                s.key
            );
            assert!(s.regular(60_000, 0), "{}", s.key);
        }
    }

    #[test]
    fn a_tally_tells_apart_keys_alike_in_their_first_words() {
        // R3-4: a row's series is found by its key's length and first sixteen bytes, and only a
        // longer key is compared in full; keys that differ only past their sixteenth byte, or
        // only by a trailing NUL, which a zero-padded word does not show, are still told apart,
        // in runs and in a cycle alike.
        let keys = [
            "",
            "\0",
            "a",
            "a\0",
            "abcdefg",
            "abcdefgh",
            "abcdefgh\0",
            "abcdefghijklmno",
            "abcdefghijklmnop",
            "abcdefghijklmnop\0",
            "abcdefghijklmnopq",
            "abcdefghijklmnopr",
            "abcdefghijklmnopqrstuvwxyz-1",
            "abcdefghijklmnopqrstuvwxyz-2",
            NULL_PART,
        ];
        let k = keys.len();
        // Each key i has 3 + i rows: first a cycle over every key, then each key's rest in a run.
        let mut rows: Vec<usize> = (0..k).collect();
        for (i, _) in keys.iter().enumerate() {
            rows.extend(std::iter::repeat_n(i, 2 + i));
        }
        let ts: Vec<i64> = (0..rows.len()).map(|r| r as i64 * 1_000).collect();
        let kw: Vec<f64> = rows.iter().map(|k| *k as f64).collect();
        let mut tally = Tally::new(1, false, None, TIMES_MAX, FLAT_MAX, usize::MAX);
        tally.add(
            rows.len(),
            |r| keys[rows[r]],
            |r| key_words(keys[rows[r]].as_bytes()),
            Some(Stamps {
                v: &ts,
                nulls: None,
            }),
            &[Doubles {
                v: &kw,
                nulls: None,
            }],
            |_| {},
        );
        let p = tally.finish(None, 0);
        let mut want: Vec<(&str, i64)> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| (*key, 3 + i as i64))
            .collect();
        want.sort();
        let got: Vec<(&str, i64)> = p.series.iter().map(|s| (s.key.as_str(), s.rows)).collect();
        assert_eq!(got, want);
        for s in &p.series {
            // Every row of a series carries the key's own index as its value.
            let m = &s.measures[0];
            assert_eq!(m.min, m.max, "{:?}", s.key);
        }
    }

    #[test]
    fn a_keys_words_are_read_alike_from_every_string_layout() {
        // R3-4: a row's key words are read from the batch where they lie: two loads of a
        // string array's values, masked to the key's length, where sixteen bytes are there to
        // read; the view itself, for a string of up to twelve bytes in a view. Each must be the
        // words of the key's own bytes: keys of every length to 20 bytes, the last ones at the
        // end of the values, where fewer than sixteen bytes are left, and a sliced array.
        use duckdb::arrow::array::{Array, LargeStringArray, StringArray, StringViewArray};
        let keys: Vec<String> = (0..=20)
            .map(|n| {
                (0..n)
                    .map(|k| (b'a' + (k as u8 + n as u8) % 26) as char)
                    .collect()
            })
            .chain(["\u{1e}".to_string(), "Ærø".to_string(), "m000".to_string()])
            .collect();
        let utf8 = StringArray::from_iter_values(&keys);
        let large = LargeStringArray::from_iter_values(&keys);
        let view = StringViewArray::from_iter_values(&keys);
        for (i, key) in keys.iter().enumerate() {
            let want = key_words(key.as_bytes());
            assert!(utf8.words(i) == want, "utf8 {i}");
            assert!(large.words(i) == want, "large {i}");
            assert!(view.words(i) == want, "view {i}");
        }
        let cut = utf8.slice(3, 10);
        let cut = cut.as_any().downcast_ref::<StringArray>().unwrap();
        for i in 0..cut.len() {
            assert!(
                cut.words(i) == key_words(keys[3 + i].as_bytes()),
                "sliced {i}"
            );
        }
    }

    #[test]
    fn a_tallys_totals_per_time_count_every_row_of_every_time() {
        // R3-4: a run of rows at one time is added up apart and into its time once it ends.
        // Every time's rows, zeros and sums must still be its rows', the last run's included:
        // runs cut by batches, a time that comes back after others, nulls and a time whose
        // every value is null. Values are halves, so any order of adding gives the same sum.
        let n = 5_000usize;
        let t = |i: usize| {
            ((i / 7) % 300) as i64 * 60_000 + if i.is_multiple_of(97) { 30_000 } else { 0 }
        };
        let v = |i: usize, j: usize| match (i + j) % 11 {
            0 => None,
            1 | 2 => Some(0.0),
            k => Some(k as f64 * 0.5),
        };
        let ts: Vec<i64> = (0..n).map(t).collect();
        let cols: Vec<Vec<f64>> = (0..2)
            .map(|j| (0..n).map(|i| v(i, j).unwrap_or(0.0)).collect())
            .collect();
        let nulls: Vec<duckdb::arrow::buffer::NullBuffer> = (0..2)
            .map(|j| (0..n).map(|i| v(i, j).is_some()).collect())
            .collect();
        let mut tally = Tally::new(2, true, None, TIMES_MAX, FLAT_MAX, usize::MAX);
        let none = key_words(b"");
        let mut from = 0;
        for cut in [1, 333, 1_000, 2_048, 5_000] {
            let len = cut.min(n) - from;
            let sliced: Vec<duckdb::arrow::buffer::NullBuffer> =
                nulls.iter().map(|b| b.slice(from, len)).collect();
            let values: Vec<Doubles> = (0..2)
                .map(|j| Doubles {
                    v: &cols[j][from..from + len],
                    nulls: Some(&sliced[j]),
                })
                .collect();
            tally.add(
                len,
                |_| "",
                |_| none,
                Some(Stamps {
                    v: &ts[from..from + len],
                    nulls: None,
                }),
                &values,
                |_| {},
            );
            from += len;
        }
        let tm = tally.finish(None, 0).times.expect("kept");
        // Per time: rows, and each measure's zeros, sum and whether it has one.
        type Totals = (i64, [i64; 2], [f64; 2], [bool; 2]);
        let mut want: std::collections::BTreeMap<i64, Totals> = Default::default();
        for (i, time) in ts.iter().enumerate() {
            let e = want.entry(*time).or_default();
            e.0 += 1;
            for j in 0..2 {
                if let Some(x) = v(i, j) {
                    e.1[j] += i64::from(x == 0.0);
                    e.2[j] += x;
                    e.3[j] = true;
                }
            }
        }
        assert_eq!(tm.t.len(), want.len());
        for (slot, time) in tm.t.iter().enumerate() {
            let (rows, zeros, sums, summed) = want[time];
            assert_eq!(tm.n[slot], rows, "rows at {time}");
            for j in 0..2 {
                assert_eq!(tm.zeros[slot * 2 + j], zeros[j], "zeros at {time}");
                assert_eq!(tm.summed[slot * 2 + j], summed[j], "summed at {time}");
                assert_eq!(tm.sums[slot * 2 + j], sums[j], "sum at {time}");
            }
        }
    }

    #[test]
    fn an_integer_key_is_written_as_text_once_a_series() {
        // R3-10: a tally keyed by an integer column found each row's series by the integer
        // written as text, every row: the watch thread's CPU 940 ms against 619 ms with text
        // keys at exec_series-10m's shape. It is found by the integer itself, and written as
        // text once, when its series begins, as `CAST(… AS VARCHAR)` writes it: negative, wider
        // than 32 bits, an unsigned counter past `i64::MAX`, and a null, which is a series too.
        use duckdb::arrow::array::{
            ArrayRef, Float64Array, Int64Array, TimestampMicrosecondArray, UInt64Array,
        };
        use std::sync::Arc;
        let t0 = 1_767_225_600_000_000i64;
        let cycle: [Option<i128>; 6] = [
            Some(-20_000),
            Some(0),
            Some(7),
            Some(4_294_967_296),
            Some(i64::MAX as i128),
            None,
        ];
        let (rounds, n) = (400usize, 6 * 400usize);
        let ts: ArrayRef = Arc::new(
            TimestampMicrosecondArray::from_iter_values(
                (0..n).map(|i| t0 + (i / 6) as i64 * 60_000_000),
            )
            .with_timezone("UTC"),
        );
        let kw: ArrayRef = Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64)));
        let signed: ArrayRef = Arc::new(Int64Array::from_iter(
            (0..n).map(|i| cycle[i % 6].map(|x| x as i64)),
        ));
        let unsigned: ArrayRef =
            Arc::new(UInt64Array::from_iter((0..n).map(|i| {
                cycle[i % 6].map(|x| if x < 0 { u64::MAX } else { x as u64 })
            })));
        for (key, ty, want) in [
            (
                &signed,
                "BIGINT",
                vec![
                    "-20000",
                    "0",
                    "4294967296",
                    "7",
                    "9223372036854775807",
                    NULL_PART,
                ],
            ),
            (
                &unsigned,
                "HUGEINT",
                vec![
                    "0",
                    "18446744073709551615",
                    "4294967296",
                    "7",
                    "9223372036854775807",
                    NULL_PART,
                ],
            ),
        ] {
            let cols = vec![
                ("ts".to_string(), "TIMESTAMP WITH TIME ZONE".to_string()),
                ("meter".to_string(), ty.to_string()),
                ("kw".to_string(), "DOUBLE".to_string()),
            ];
            let mut w = Watch::new(&cols);
            INT_TEXTS.with(|c| c.set(0));
            for from in (0..n).step_by(1_000) {
                let len = 1_000.min(n - from);
                let a: Vec<ArrayRef> = [&ts, key, &kw].iter().map(|x| x.slice(from, len)).collect();
                let held: Vec<Option<Held>> =
                    (0..3).map(|i| Held::of(&a[i], &cols[i].1, true)).collect();
                w.add(len, &held);
            }
            let written = INT_TEXTS.with(|c| c.get());
            assert!(
                written <= 6,
                "{ty}: {written} keys written as text for {n} rows"
            );
            let seen = w.finish().expect("kept");
            let (_, _, tally) = seen.keyed.into_iter().next().expect("tallied");
            let p = tally.finish(Some(60_000), t0 / 1000);
            let keys: Vec<&str> = p.series.iter().map(|s| s.key.as_str()).collect();
            // In the order `ORDER BY` puts text: by its bytes.
            let mut want = want;
            want.sort();
            assert_eq!(keys, want, "{ty}");
            assert!(p.series.iter().all(|s| s.rows == rounds as i64), "{ty}");
        }
    }

    #[test]
    fn placing_a_series_matches_counting_its_steps() {
        // `Tally` keeps sums of distances from a series' first time; the phase and the checksums
        // of the step indices worked out from them must be what counting each index gives.
        let origin = 1_000_000_007i64;
        for (step, times) in [
            (900_000i64, vec![0i64, 900_000, 1_800_000, 2_700_000]),
            (900_000, vec![2_700_000, 0, 1_800_000, 900_000, 900_000]),
            (900_000, vec![60_000, 960_000, 2_760_000]),
            (900_000, vec![0, 450_000, 900_000]),
            (60_000, vec![7 * 60_000]),
            (
                86_400_000,
                (0..400).map(|i| i * 86_400_000 + 3_600_000).collect(),
            ),
            (1, vec![5, 9, 12, 3]),
        ] {
            let mut tally = Tally::new(0, false, None, 0, 0, usize::MAX);
            let ts: Vec<i64> = times.iter().map(|t| origin + t).collect();
            let none = key_words(b"");
            tally.add(
                ts.len(),
                |_| "",
                |_| none,
                Some(Stamps {
                    v: &ts,
                    nulls: None,
                }),
                &[],
                |_| {},
            );
            let s = &tally.finish(Some(step), origin).series[0];
            let phases: Vec<i64> = times.iter().map(|t| t % step).collect();
            let one = phases.iter().all(|p| *p == phases[0]);
            assert_eq!(s.phase.0 == s.phase.1, one, "{times:?}");
            if one {
                assert_eq!(s.phase.0, phases[0]);
                let ix: Vec<i128> = times.iter().map(|t| (t / step) as i128).collect();
                assert_eq!(s.sum, ix.iter().sum::<i128>(), "{times:?}");
                assert_eq!(
                    s.sum_sq,
                    ix.iter().map(|x| x * x).sum::<i128>(),
                    "{times:?}"
                );
            }
        }
    }

    #[test]
    fn duckdb_orders_nan_above_everything() {
        // `max` of a column holding a NaN is NaN in DuckDB, and the pass must say the same.
        use std::cmp::Ordering::*;
        assert_eq!(duck_cmp(f64::NAN, f64::INFINITY), Greater);
        assert_eq!(duck_cmp(1.0, f64::NAN), Less);
        assert_eq!(duck_cmp(f64::NAN, f64::NAN), Equal);
        assert_eq!(duck_cmp(-0.0, 0.0), Equal);
    }
}
