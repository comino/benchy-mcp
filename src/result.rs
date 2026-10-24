//! REQ-10: what comes back is small, says when it is not everything, and can be read further.
//!
//! The byte cap is a promise about the **response**, so it is measured on the response as it goes
//! on the wire — JSON-escaped, across every statement in the batch, including the notices that
//! say a result was cut. A value longer than the cell cap is cut and says by how much; a result
//! longer than a page says how to read on.

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Text(String),
}

impl Cell {
    fn text(&self) -> &str {
        match self {
            Cell::Null => "",
            Cell::Text(t) => t,
        }
    }
    /// What retaining this value costs, for the per-result retention budget.
    pub fn byte_len(&self) -> usize {
        self.text().len() + 3
    }
}

#[derive(Debug, Clone)]
pub struct Caps {
    /// Rows per page.
    pub max_rows: usize,
    /// Bytes per *response*, JSON-escaped, all statements and notices included.
    pub max_bytes: usize,
    /// Bytes per value. A longer value is cut to this and says how much was left out.
    pub max_cell_bytes: usize,
    /// REQ-7. Covers the whole batch, not each statement — a thousand cheap statements is the
    /// same denial of service as one expensive one.
    pub deadline: std::time::Duration,
    /// DuckDB's own budget. It does **not** bound query *planning*; the out-of-process kill
    /// (REQ-8) does that.
    pub memory_limit: String,
    /// REQ-8. How long past the soft deadline the engine gets before the server takes the process
    /// away from it.
    pub hard_grace: std::time::Duration,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_rows: 200,
            max_bytes: 8 * 1024,
            max_cell_bytes: 256,
            deadline: std::time::Duration::from_secs(20),
            memory_limit: "1GB".into(),
            hard_grace: std::time::Duration::from_secs(10),
        }
    }
}

/// The smallest response cap that can still say what it cut. Below this the notices alone would
/// not fit, and a cap that cannot explain itself is worse than a larger one.
pub const MIN_MAX_BYTES: usize = 512;

/// Rows kept per result for paging. Beyond this a result says so and asks for a narrower query:
/// paging through tens of thousands of rows is the job SQL does better.
pub const RETAIN_ROWS: usize = 2000;
/// Bytes kept per result, so one result of wide rows cannot hold the whole retention budget.
pub const RETAIN_BYTES: usize = 1 << 20;
/// How many results are kept for paging.
pub const RETAINED_RESULTS: usize = 32;

/// The retention rule, in one place, so the Arrow and legacy readers cannot drift apart on it:
/// keep rows while there is room, and remember whether more existed. Here beside the limits it
/// applies, and with [`retain`], so that what REQ-10 says is kept is all in the one file the
/// mutation tests examine for it.
pub(crate) struct Keep {
    pub(crate) rows: Vec<Vec<Cell>>,
    bytes: usize,
    pub(crate) more: bool,
    pub(crate) cut: bool,
}

impl Keep {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            bytes: 0,
            more: false,
            cut: false,
        }
    }

    /// A value, cut to the cell cap if it is longer — remembered, so the result can say so.
    pub(crate) fn value(&mut self, s: String, max: usize) -> Cell {
        if s.len() > max {
            self.cut = true;
        }
        Cell::Text(cut(s, max))
    }

    /// Offer a row. `false` once retention is full — and then more rows existed than were kept.
    /// Full is [`RETAIN_ROWS`] rows, or [`RETAIN_BYTES`] reached: the row that reaches it is kept,
    /// so a result can hold up to a row more than 1 MiB
    /// (`a_result_keeps_rows_until_they_reach_1_mib`).
    pub(crate) fn offer(&mut self, row: Vec<Cell>) -> bool {
        if self.rows.len() >= RETAIN_ROWS || self.bytes >= RETAIN_BYTES {
            self.more = true;
            return false;
        }
        self.bytes += row.iter().map(Cell::byte_len).sum::<usize>();
        self.rows.push(row);
        true
    }
}

/// Keep `r` for paging, among the last [`RETAINED_RESULTS`]: the oldest goes first. Results
/// arrive one at a time, so one is the most ever over; an `if`, not a `while`, so that no
/// change to the comparison can loop on an empty list (a mutant that did hung the suite).
pub(crate) fn retain(kept: &mut std::collections::VecDeque<QueryResult>, r: QueryResult) {
    kept.push_back(r);
    if kept.len() > RETAINED_RESULTS {
        kept.pop_front();
    }
}

/// A result as the workspace keeps it: up to [`RETAIN_ROWS`] rows, every value already cut to the
/// cell cap. Paging reads from here, so it never runs the query again.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub result_id: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// More rows exist than were kept.
    pub more: bool,
    /// Reading stopped early, and why. Such a result is never presented as complete.
    pub partial: Option<String>,
    /// REQ-11: why an empty result is empty, when that can be told.
    pub note: Option<String>,
    /// Some value was longer than the cell cap and was cut. Each cut value says how much is
    /// missing; the footer says so too, because "TRUNCATED" is what an agent learns to look for.
    pub cut_values: bool,
    /// The statement this result came from, as the agent wrote it, at most [`RETAIN_SQL`] bytes;
    /// `None` where it cannot be told apart in its batch. A host's `page` returns it
    /// (`a_page_says_the_sql_of_its_result`).
    pub sql: Option<String>,
    /// The cell cap its values were kept at: the cap of the call that ran it. A page may ask
    /// for a lower one, and a value kept longer than that is cut again as the page shows it
    /// (`a_page_cuts_values_at_its_own_cell_cap`).
    pub cell_cap: usize,
}

/// The most of a result's statement kept with it. A request is at most 1 MiB, and 32 results are
/// kept: whole statements could hold 32 MiB of the engine's memory to say what a page, at most
/// `max_bytes`, shows a few KiB of.
pub const RETAIN_SQL: usize = 64 * 1024;

/// What a host reads of one result of a `query`, beside the text: how many rows benchy holds, and
/// how much of it the text shows (`a_query_reply_counts_the_rows_of_each_result`). A host acts on
/// these without parsing the footer, whose wording may change between releases.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shown {
    pub result_id: String,
    /// Rows held: every row when `complete`, otherwise those kept for paging or read before
    /// reading stopped.
    pub rows: usize,
    /// `rows` is every row the statement returned.
    pub complete: bool,
    /// Rows the text shows.
    pub shown: usize,
    /// The text is less than the whole result: a row not shown, or a value or a row cut.
    pub truncated: bool,
}

/// Cut a value to `max` bytes on a character boundary, and say how much was left out.
pub fn cut(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // No loop stepping back to the boundary, which a mutation test can make endless.
    let end = s.floor_char_boundary(max);
    format!("{}… [+{} bytes]", &s[..end], s.len() - end)
}

/// Length as it goes on the wire: inside a JSON string, where escaping can grow text.
pub fn wire_len(s: &str) -> usize {
    serde_json::to_string(s)
        .map(|j| j.len() - 2)
        .unwrap_or(s.len() * 6)
}

/// Room kept back from the page itself for the line that says what happened to it. The longest
/// footer, rows past what is kept and cut at the byte cap, is about 226 bytes with a host's
/// 64-byte name for `get_result` (`a_long_tool_name_keeps_every_text_within_its_budget`).
const FOOTER_ROOM: usize = 260;

/// Cut `s` so its escaped length, `mark` included, fits `max_wire`.
fn cut_to_wire(s: &str, max_wire: usize, mark: &str) -> String {
    let room = max_wire.saturating_sub(wire_len(mark));
    let mut used = 0usize;
    let mut end = 0usize;
    for (i, ch) in s.char_indices() {
        let n = wire_len(ch.encode_utf8(&mut [0u8; 4]));
        if used + n > room {
            break;
        }
        used += n;
        end = i + ch.len_utf8();
    }
    format!("{}{mark}", &s[..end])
}

const ROW_CUT: &str = "… [row cut at the byte cap]";

/// Fit any text — an error, say — inside `max_wire` bytes on the wire.
pub fn fit(s: &str, max_wire: usize) -> String {
    if wire_len(s) <= max_wire {
        s.to_string()
    } else {
        cut_to_wire(s, max_wire, "… [cut at the byte cap]")
    }
}

/// One row as a page shows it, each value at most `max` bytes, as the call that kept it would
/// have cut it at that cap; whether a value was cut here is added to `cut`.
fn row_line(row: &[Cell], kept_at: usize, max: usize, cut: &mut bool) -> String {
    row.iter()
        .map(|c| match recut(c.text(), kept_at, max) {
            Some(v) => {
                *cut = true;
                std::borrow::Cow::Owned(v)
            }
            None => std::borrow::Cow::Borrowed(c.text()),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// A value kept at the cell cap `kept_at`, cut to the lower cap `max`, or `None` where it fits.
/// Paging with a lower cap than the query's showed every kept value whole: a host's
/// `"max_cell_bytes": 16` got all 200 bytes of `repeat('x', 200)`, and no word of a cut (#18).
///
/// A value longer than `kept_at` was cut when it was kept, so it ends in the mark [`cut`]
/// wrote, and what it says is missing counts against the value it was cut from, not against
/// the mark. Without that mark it is not a value but benchy's own word about one, such as a
/// type that is not rendered, which the query did not cut either.
fn recut(v: &str, kept_at: usize, max: usize) -> Option<String> {
    if v.len() <= max {
        return None;
    }
    let (head, whole) = if v.len() > kept_at {
        let rest = v.strip_suffix(" bytes]")?;
        let at = rest.rfind("… [+")?;
        let missing: usize = rest[at + "… [+".len()..].parse().ok()?;
        (&v[..at], at + missing)
    } else {
        (v, v.len())
    };
    if head.len() <= max {
        return None;
    }
    let end = head.floor_char_boundary(max);
    Some(format!("{}… [+{} bytes]", &head[..end], whole - end))
}

/// One page as rendered: its text, the wire bytes it takes, the rows it shows, and whether it is
/// less than the whole result.
struct Page {
    text: String,
    bytes: usize,
    shown: usize,
    truncated: bool,
}

/// Render one page of `r`, starting at `offset`, inside `budget` wire bytes. The result is always
/// explicit about whether it is everything.
fn render_page(
    r: &QueryResult,
    offset: usize,
    max_rows: usize,
    max_cell: usize,
    budget: usize,
) -> Page {
    /// Less room than this and a page would be all cut marks: point to the result instead.
    const LEAST: usize = 64;
    let room = budget.saturating_sub(FOOTER_ROOM);
    let get_result = crate::tools::name("get_result");
    // Column names are cut like values: an alias is as much the agent's to make long.
    let names: Vec<String> = r.columns.iter().map(|c| cut(c.clone(), max_cell)).collect();
    let mut header = format!("{}\n", names.join(" | "));
    let mut body = String::new();
    let mut shown = 0usize;
    let mut out_of_bytes = false;
    let mut cut_here = false;
    if wire_len(&header) > room {
        header = format!(
            "{}\n",
            cut_to_wire(&names.join(" | "), room / 2, "… [columns cut]")
        );
        out_of_bytes = true;
    }
    let mut used = wire_len(&header);
    if room < LEAST {
        out_of_bytes = true;
    } else {
        body.push_str(&header);
        for row in r.rows.iter().skip(offset).take(max_rows) {
            let line = format!("{}\n", row_line(row, r.cell_cap, max_cell, &mut cut_here));
            let n = wire_len(&line);
            if used + n > room && shown == 0 {
                // A single row wider than the whole response: show what fits of it rather than
                // nothing, or paging would stall on it forever.
                let line = cut_to_wire(
                    &row_line(row, r.cell_cap, max_cell, &mut cut_here),
                    room.saturating_sub(used + 2),
                    ROW_CUT,
                );
                // `used` is not counted on: a page out of bytes has no note, the one thing that
                // reads it after the rows.
                body.push_str(&line);
                body.push('\n');
                out_of_bytes = true;
                shown += 1;
                break;
            }
            if used + n > room {
                out_of_bytes = true;
                break;
            }
            body.push_str(&line);
            used += n;
            shown += 1;
        }
    }

    let cut_values = r.cut_values || cut_here;
    let kept = r.rows.len();
    let first = offset + 1;
    let last = offset + shown;
    let complete = r.partial.is_none() && !r.more && offset == 0 && shown == kept && !out_of_bytes;
    // `a_page_past_the_end_says_so`: an offset at or past the last row kept is an empty page that
    // says where the rows end, not a range that runs backwards ("rows 10001–10000 of 500").
    let footer = if offset > 0 && offset >= kept {
        let held = match (kept, r.more || r.partial.is_some()) {
            (0, _) => "has no rows".to_string(),
            (k, false) => format!("has {k} row(s), at offsets 0 to {}", k - 1),
            (k, true) => format!(
                "keeps its first {k} row(s), at offsets 0 to {}; the rest are not kept for paging",
                k - 1
            ),
        };
        format!(
            "\nno rows at offset {offset}: result_id {} {held}.",
            r.result_id
        )
    } else if complete && cut_values {
        format!(
            "\n{} row(s), result_id {} — TRUNCATED: some values were cut, and each says how much \
             is missing. Select a substring to read the rest.",
            kept, r.result_id
        )
    } else if complete {
        let mut f = format!("\n{} row(s), result_id {}", kept, r.result_id);
        if let Some(n) = &r.note {
            // The note gets what the page left, and no more: it can quote values of any length.
            // A complete page used at most `room`, so what is left is never less than
            // `FOOTER_ROOM` less this line, some 200 bytes: there is always room for a note.
            // The separator is counted with the line it joins: subtracted on its own, an error in
            // it was one the backstop below made up for, and no test could see.
            f.push_str(" — ");
            let left = budget.saturating_sub(used + wire_len(&f));
            f.push_str(&fit(n, left));
        }
        f
    } else if let Some(why) = &r.partial {
        format!(
            "\nrows {first}–{last}, result_id {} — TRUNCATED: reading stopped after row {kept} \
             ({why}). This is not all the rows.",
            r.result_id
        )
    } else if last < kept {
        let why = if out_of_bytes {
            "at the byte cap"
        } else {
            "one page"
        };
        let beyond = if r.more { "+" } else { "" };
        format!(
            "\nrows {first}–{last} of {kept}{beyond} ({why}), result_id {} — TRUNCATED: this is \
             not all the rows. Read on with {get_result} {} offset {last}, or aggregate.",
            r.result_id, r.result_id
        )
    } else if out_of_bytes {
        format!(
            "\nrows {first}–{last} of {kept}, result_id {} — TRUNCATED: a row was cut at the byte \
             cap. Select fewer or narrower columns.",
            r.result_id
        )
    } else if r.more {
        format!(
            "\nrows {first}–{last}, result_id {} — TRUNCATED: more rows exist than are kept for \
             paging. Narrow the query, or aggregate.",
            r.result_id
        )
    } else {
        format!(
            "\nrows {first}–{last} of {kept}, result_id {} — the end.",
            r.result_id
        )
    };
    let not_shown = shown == 0 && room < LEAST;
    let text = if not_shown {
        format!(
            "result_id {} — TRUNCATED: not shown, the response is at its byte cap. Read it with \
             {get_result} {}.",
            r.result_id, r.result_id
        )
    } else {
        // The backstop: everything above keeps inside the budget, and this makes sure of it.
        fit(&format!("{body}{footer}"), budget)
    };
    let truncated = not_shown
        || cut_values
        || r.partial.is_some()
        || r.more
        || out_of_bytes
        || offset + shown < kept;
    Page {
        bytes: wire_len(&text),
        text,
        shown,
        truncated,
    }
}

/// One page of a retained result, as `get_result` returns it.
pub fn render_one(r: &QueryResult, offset: usize, caps: &Caps) -> String {
    render_page(
        r,
        offset,
        caps.max_rows,
        caps.max_cell_bytes,
        caps.max_bytes,
    )
    .text
}

/// Render every result of a batch inside one byte budget. What does not fit says where to find
/// it, and the whole response — notices included — stays within `caps.max_bytes`.
pub fn render_batch(results: &[QueryResult], caps: &Caps) -> String {
    render_batch_shown(results, caps).0
}

/// [`render_batch`], and what each result's part of the text shows of it, in order: a result the
/// byte cap left out shows nothing.
pub fn render_batch_shown(results: &[QueryResult], caps: &Caps) -> (String, Vec<Shown>) {
    let mut shown: Vec<Shown> = results
        .iter()
        .map(|r| Shown {
            result_id: r.result_id.clone(),
            rows: r.rows.len(),
            complete: r.partial.is_none() && !r.more,
            shown: 0,
            truncated: true,
        })
        .collect();
    let mut out = String::new();
    let mut used = 0usize;
    for (i, r) in results.iter().enumerate() {
        let sep = if out.is_empty() { 0 } else { wire_len("\n\n") };
        let left = caps.max_bytes.saturating_sub(used + sep);
        let rest = results.len() - i;
        // Leave room for a one-line pointer to each result still to come.
        let reserve = (rest - 1) * 120;
        let page = render_page(
            r,
            0,
            caps.max_rows,
            caps.max_cell_bytes,
            left.saturating_sub(reserve),
        );
        let n = page.bytes;
        if n > left {
            let tail = format!(
                "\n\n… {rest} more result(s) not shown: the response is at its byte cap. Fetch \
                 them with {}.",
                crate::tools::name("get_result")
            );
            if used + wire_len(&tail) <= caps.max_bytes {
                out.push_str(&tail);
            }
            break;
        }
        if sep > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&page.text);
        used += sep + n;
        shown[i].shown = page.shown;
        shown[i].truncated = page.truncated;
    }
    (out, shown)
}

/// Microseconds since 1970-01-01 UTC, rendered the way DuckDB renders a `TIMESTAMPTZ`.
///
/// We format this ourselves because the C result API we read values through returns nothing at
/// all for `TIMESTAMP WITH TIME ZONE` — not an error, an empty cell. See `tests/render.rs`.
/// The instant is stored in UTC and [`crate::lockdown`] pins the session to UTC, so `+00` is a
/// statement about the data rather than about the machine's clock.
pub fn timestamp_utc(micros: i64) -> String {
    const US_PER_DAY: i64 = 86_400_000_000;
    let days = micros.div_euclid(US_PER_DAY);
    let rest = micros.rem_euclid(US_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let (secs, us) = (rest / 1_000_000, rest % 1_000_000);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    let mut frac = String::new();
    if us != 0 {
        frac = format!(".{us:06}");
        while frac.ends_with('0') {
            frac.pop();
        }
    }
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}{frac}+00")
}

/// Days since 1970-01-01 to a proleptic Gregorian date. Howard Hinnant's `civil_from_days`,
/// which is exact for the whole range and needs no tables.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(rows: Vec<&str>, partial: Option<&str>) -> QueryResult {
        QueryResult {
            result_id: "r1".into(),
            columns: vec!["a".into()],
            rows: rows
                .into_iter()
                .map(|r| vec![Cell::Text(r.into())])
                .collect(),
            more: false,
            partial: partial.map(str::to_string),
            note: None,
            cut_values: false,
            sql: None,
            cell_cap: Caps::default().max_cell_bytes,
        }
    }

    #[test]
    fn a_partial_result_is_never_presented_as_complete() {
        // No SQL has been found that makes Arrow conversion fail partway through a result, so this
        // covers the other half: once a reader marks a result partial, it can never render as
        // complete — not even when every row it kept is shown.
        let text = render_batch(
            &[result(
                vec!["1", "2"],
                Some("a chunk could not be converted"),
            )],
            &Caps::default(),
        );
        assert!(text.contains("TRUNCATED"), "{text}");
        assert!(text.contains("not all the rows"), "{text}");
    }

    #[test]
    fn the_response_budget_holds_for_awkward_shapes() {
        let caps = Caps {
            max_bytes: 600,
            ..Caps::default()
        };
        let wide = "é\u{1}\"".repeat(400);
        let many: Vec<QueryResult> = (0..40).map(|_| result(vec!["x"; 30], None)).collect();
        for batch in [
            vec![result(vec![wide.as_str()], None)],
            vec![result(vec![wide.as_str(); 50], None)],
            many,
        ] {
            let text = render_batch(&batch, &caps);
            assert!(
                wire_len(&text) <= caps.max_bytes,
                "{} bytes",
                wire_len(&text)
            );
            assert!(text.contains("TRUNCATED"), "{text}");
        }
    }

    #[test]
    fn a_value_is_cut_back_to_a_character_boundary() {
        // 16 bytes falls inside the sixth '€' (three bytes each): the cut keeps five, never
        // part of the sixth, and never all of it, which would be more than the cap.
        assert_eq!(cut("€".repeat(10), 16), "€€€€€… [+15 bytes]");
        assert_eq!(cut("€".repeat(10), 15), "€€€€€… [+15 bytes]");
        assert_eq!(cut("€".repeat(5), 15), "€€€€€");
    }

    #[test]
    fn the_wire_length_is_the_text_as_json_escapes_it() {
        // What every cap is measured in: inside a JSON string, quotes not counted.
        assert_eq!(wire_len(""), 0);
        assert_eq!(wire_len("abc"), 3);
        assert_eq!(wire_len("a\"b\\"), 6);
        assert_eq!(wire_len("\u{1}"), 6);
        assert_eq!(wire_len("€"), 3);
    }

    /// A page of `rows` inside `budget`, values kept and shown at up to 4096 bytes.
    fn page(rows: Vec<&str>, offset: usize, max_rows: usize, budget: usize) -> Page {
        let r = QueryResult {
            cell_cap: 4096,
            ..result(rows, None)
        };
        render_page(&r, offset, max_rows, 4096, budget)
    }

    // The edges of a page, each where one byte decides. At a budget of 1,024 the rows have
    // 764 bytes (`FOOTER_ROOM` is kept for the footer), and the header `a\n` takes 3 of them,
    // a newline counting 2 as it is escaped: a row of 759 bytes is a line of 761, which fills
    // the page exactly. The caps' own tests show only that a page fits, which a page that
    // kept less than fits does too.

    #[test]
    fn rows_that_fill_the_page_exactly_are_shown_whole() {
        let one = "x".repeat(759);
        let p = page(vec![&one, "y"], 0, 10, 1024);
        assert_eq!(p.shown, 1);
        assert!(p
            .text
            .starts_with(&format!("a\n{one}\n\nrows 1–1 of 2 (at the byte cap)")));
        assert!(!p.text.contains(ROW_CUT), "{}", p.text);
        // two rows that come to the page between them, 3 + 402 + 359 bytes
        let (a, b) = ("x".repeat(400), "y".repeat(357));
        let p = page(vec![&a, &b], 0, 10, 1024);
        assert_eq!(p.shown, 2);
        assert!(p
            .text
            .starts_with(&format!("a\n{a}\n{b}\n\n2 row(s), result_id r1")));
    }

    #[test]
    fn a_row_wider_than_the_page_is_shown_cut_and_read_past() {
        // What fits of it, to the byte, and the next page starts after it: shown nothing, paging
        // would stall on it.
        let wide = "x".repeat(2000);
        let p = page(vec![&wide, "y"], 0, 10, 1024);
        assert_eq!(p.shown, 1);
        let kept = "x".repeat(764 - 3 - 2 - wire_len(ROW_CUT));
        assert_eq!(
            p.text,
            format!(
                "a\n{kept}{ROW_CUT}\n\nrows 1–1 of 2 (at the byte cap), result_id r1 — TRUNCATED: \
                 this is not all the rows. Read on with {} r1 offset 1, or aggregate.",
                crate::tools::name("get_result")
            )
        );
    }

    #[test]
    fn a_header_that_fills_the_page_exactly_is_not_cut() {
        let fits = QueryResult {
            columns: vec!["c".repeat(762)],
            ..result(vec![], None)
        };
        let p = render_page(&fits, 0, 10, 4096, 1024);
        assert!(!p.text.contains("[columns cut]"), "{}", p.text);
        let over = QueryResult {
            columns: vec!["c".repeat(763)],
            ..result(vec![], None)
        };
        let p = render_page(&over, 0, 10, 4096, 1024);
        assert!(p.text.contains("[columns cut]"), "{}", p.text);
    }

    #[test]
    fn a_page_with_the_least_room_is_still_shown() {
        // 64 bytes for the rows, at a budget of 324, is the least a page is shown with.
        let p = page(vec!["1"], 0, 10, 324);
        assert_eq!(p.shown, 1);
        assert!(p.text.starts_with("a\n1\n\n1 row(s)"), "{}", p.text);
        let p = page(vec!["1"], 0, 10, 323);
        assert_eq!(p.shown, 0);
        assert!(p.text.contains("not shown"), "{}", p.text);
        // and with no rows to show, the page says there are none, not that it is not shown
        let p = page(vec![], 0, 10, 324);
        assert_eq!(p.text, "a\n\n0 row(s), result_id r1");
    }

    #[test]
    fn a_page_is_called_truncated_for_each_reason_alone() {
        // `truncated` is what a host reads; each reason on its own must set it. Every row
        // shown, but more existed than were kept:
        let more = QueryResult {
            more: true,
            ..result(vec!["1", "2"], None)
        };
        let p = render_page(&more, 0, 10, 4096, 1024);
        assert_eq!(p.shown, 2);
        assert!(p.truncated, "{}", p.text);
        // the one row shown, and cut at the byte cap:
        let wide = "x".repeat(2000);
        let p = page(vec![&wide], 0, 10, 1024);
        assert_eq!(p.shown, 1);
        assert!(p.truncated, "{}", p.text);
        // and none of them:
        let p = page(vec!["1", "2"], 0, 10, 1024);
        assert!(!p.truncated, "{}", p.text);
    }

    #[test]
    fn a_page_counts_its_rows_from_1() {
        let p = page(vec!["1", "2", "3"], 0, 1, 1024);
        assert!(p.text.contains("\nrows 1–1 of 3 (one page)"), "{}", p.text);
        let p = page(vec!["1", "2", "3"], 1, 1, 1024);
        assert!(p.text.contains("\nrows 2–2 of 3 (one page)"), "{}", p.text);
        // an empty result read from its start is complete, not a page past its end
        let p = page(vec![], 0, 10, 1024);
        assert_eq!(p.text, "a\n\n0 row(s), result_id r1");
    }

    #[test]
    fn a_note_gets_all_the_page_left() {
        // 1,024 less the rows (6), the footer's line (24) and " — " (5): 989 bytes of note, and
        // the response comes to the budget exactly.
        let r = QueryResult {
            note: Some("n".repeat(1000)),
            ..result(vec!["1"], None)
        };
        let p = render_page(&r, 0, 10, 4096, 1024);
        let mark = "… [cut at the byte cap]";
        assert_eq!(
            p.text,
            format!(
                "a\n1\n\n1 row(s), result_id r1 — {}{mark}",
                "n".repeat(989 - wire_len(mark))
            )
        );
        assert_eq!(p.bytes, 1024);
    }

    /// A result of `rows` one-letter rows with id `id`: each row a line of 3 bytes.
    fn letters(id: &str, rows: usize) -> QueryResult {
        QueryResult {
            result_id: id.into(),
            ..result(vec!["x"; rows], None)
        }
    }

    #[test]
    fn a_batch_shares_its_budget_to_the_byte() {
        // Three results in 1,023 bytes. The first is given what is left less 120 for each result
        // after it, and takes 30. The second is given 1,023 - 30 - 4 (the separator) - 120 =
        // 869, so 609 for its rows: its header and 202 rows of 3 bytes, exactly. The third has no
        // room for rows and says where it is.
        let caps = Caps {
            max_bytes: 1023,
            max_rows: 1000,
            ..Caps::default()
        };
        let (text, shown) = render_batch_shown(
            &[letters("r1", 1), letters("r2", 400), letters("r3", 1)],
            &caps,
        );
        let counts: Vec<(usize, bool)> = shown.iter().map(|s| (s.shown, s.truncated)).collect();
        assert_eq!(counts, [(1, false), (202, true), (0, true)], "{text}");
        assert!(
            text.starts_with("a\nx\n\n1 row(s), result_id r1\n\na\nx\n"),
            "{text}"
        );
        assert!(
            text.contains("rows 1–202 of 400 (at the byte cap), result_id r2")
                && text.contains("\n\nresult_id r3 — TRUNCATED: not shown"),
            "{text}"
        );
        assert!(wire_len(&text) <= 1023);
    }

    #[test]
    fn a_page_that_fills_the_response_exactly_is_shown() {
        // The note takes all the page leaves, so the page is the response's 1,024 bytes.
        let r = QueryResult {
            note: Some("n".repeat(1000)),
            ..result(vec!["1"], None)
        };
        let caps = Caps {
            max_bytes: 1024,
            ..Caps::default()
        };
        let (text, shown) = render_batch_shown(std::slice::from_ref(&r), &caps);
        assert_eq!(wire_len(&text), 1024);
        assert_eq!(shown[0].shown, 1, "{text}");
    }

    #[test]
    fn a_result_with_no_room_left_is_pointed_to() {
        // The first page fills its 1,024 - 120 bytes; the second result's pointer, its id twice,
        // does not fit the 116 left, and the line that says results are missing does.
        let first = QueryResult {
            note: Some("n".repeat(1000)),
            ..result(vec!["1"], None)
        };
        let caps = Caps {
            max_bytes: 1024,
            ..Caps::default()
        };
        let long_id = "r-0123456789abcdef-12";
        let (text, shown) = render_batch_shown(&[first, letters(long_id, 1)], &caps);
        assert_eq!(shown[1].shown, 0);
        assert!(!text.contains(long_id), "{text}");
        assert!(
            text.ends_with(&format!(
                "\n\n… 1 more result(s) not shown: the response is at its byte cap. Fetch them \
                 with {}.",
                crate::tools::name("get_result")
            )),
            "{text}"
        );
        assert!(wire_len(&text) <= 1024);
    }

    #[test]
    fn a_fitted_text_keeps_all_that_fits() {
        // A cut that kept less than fits would still be inside the cap, so the cap's tests
        // alone cannot tell it from this: the whole room, counted as it is escaped.
        let mark = "… [cut at the byte cap]";
        let room = 40 - wire_len(mark);
        assert_eq!(fit("abc", 3), "abc");
        assert_eq!(
            fit(&"x".repeat(100), 40),
            format!("{}{mark}", "x".repeat(room))
        );
        assert_eq!(
            fit(&"\"".repeat(100), 40),
            format!("{}{mark}", "\"".repeat(room / 2))
        );
    }

    #[test]
    fn the_epoch_and_the_ordinary_case() {
        assert_eq!(timestamp_utc(0), "1970-01-01 00:00:00+00");
        assert_eq!(
            timestamp_utc(1_767_225_600_000_000),
            "2026-01-01 00:00:00+00"
        );
    }

    #[test]
    fn before_the_epoch_does_not_wrap() {
        // Apollo 11 touchdown, 1969-07-20 20:17:40 UTC. A naive truncating division renders this
        // as the day after, with a negative time of day.
        assert_eq!(timestamp_utc(-14_182_940_000_000), "1969-07-20 20:17:40+00");
        assert_eq!(timestamp_utc(-1), "1969-12-31 23:59:59.999999+00");
    }

    #[test]
    fn leap_days_and_century_rules() {
        assert_eq!(
            timestamp_utc(1_709_164_800_000_000),
            "2024-02-29 00:00:00+00"
        ); // leap year
        assert_eq!(timestamp_utc(951_782_400_000_000), "2000-02-29 00:00:00+00"); // divisible by 400: is a leap year
        assert_eq!(
            timestamp_utc(4_107_542_400_000_000),
            "2100-03-01 00:00:00+00"
        ); // divisible by 100, not 400: is not
    }

    #[test]
    fn fractional_seconds_are_trimmed_but_never_rounded() {
        assert_eq!(timestamp_utc(1_500_000), "1970-01-01 00:00:01.5+00");
        assert_eq!(timestamp_utc(1_000_001), "1970-01-01 00:00:01.000001+00");
        assert_eq!(timestamp_utc(1_000_000), "1970-01-01 00:00:01+00");
    }
}
