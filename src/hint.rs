//! REQ-11: a concrete fix in every SQL error.
//!
//! An agent that gets `Catalog Error: Scalar Function with name to_char does not exist!` has
//! learned that its guess was wrong and nothing about what to guess next, so it usually spends
//! the following step on a second spelling of the same mistake. Naming the DuckDB form turns two
//! or three wasted steps into none.
//!
//! **Every pattern here was produced by DuckDB v1.5.5 and pasted in, not invented.** The probe
//! that produced them is `docs/DIALECT.md`; `tests/hints.rs` asserts the fixes still fire. The
//! failure mode this guards against is a hint keyed to a message DuckDB no longer emits — worse
//! than no hint, because it silently stops appearing.
//!
//! The rule for adding one: it fires on a mistake a competent model actually makes, and it names
//! a fix that works on this bench. A hint that says "check your syntax" is noise in a payload
//! that repeats every call.

/// Names the agent can use right now: what was loaded, plus whatever it created itself, and how
/// to ask for their columns.
pub struct Catalogue<'a> {
    pub names: &'a [String],
    /// Asked only by a hint that lists columns (#27). `None`, and a hint names tables alone.
    pub columns: Option<&'a Columns<'a>>,
}

/// The columns of those of the tables asked for that the catalog has, by table, each table's in
/// its own order.
pub type Columns<'a> = dyn Fn(&[&str]) -> Vec<(String, Vec<String>)> + 'a;

/// The most the tables and columns a hint lists may take, in bytes. The hint is said with every
/// error and inside the reply's byte cap (8 KB by default), so a table of a thousand columns or a
/// bench of many tables is cut here, saying what it left out, rather than by the cap, which would
/// take the end of the sentence with it (`the_columns_a_hint_lists_are_bounded`). The bundled
/// bench's two tables take 72 bytes; a table of 2,000 columns of 23-byte names is cut to its
/// first 39, the whole hint 1,094 bytes.
const SHAPES: usize = 1024;

/// At most this many tables are named, as a bench of 200 tables must not put 200 names into
/// every error.
const SHOWN: usize = 12;

/// What the agent was doing when DuckDB refused: the statement as written, when it can be told
/// apart in the batch, and the tables the session was to have and could not load.
#[derive(Default)]
pub struct Context<'a> {
    pub statement: Option<&'a str>,
    pub unavailable: &'a [String],
}

impl<'a> Catalogue<'a> {
    /// The names alone, with no columns to ask for.
    pub fn of(names: &'a [String]) -> Self {
        Catalogue {
            names,
            columns: None,
        }
    }

    /// Bounded at [`SHOWN`] names.
    fn list(&self) -> String {
        if self.names.is_empty() {
            return "nothing yet".into();
        }
        let head = self.names.iter().take(SHOWN).cloned().collect::<Vec<_>>();
        match self.names.len().checked_sub(SHOWN) {
            Some(n) if n > 0 => format!("{} (+{n} more)", head.join(", ")),
            _ => head.join(", "),
        }
    }

    /// `tables` with their columns, `readings(series_id, t, n)`, in at most [`SHAPES`] bytes, and
    /// the sentence that says where what was cut is, if anything was. Each table has an even
    /// share of what the ones before it left, so a wide first table cannot crowd out the names
    /// after it; one cut short ends `… [+N columns])`, and tables past [`SHOWN`] are counted,
    /// `(+N more)`, inside the same bytes. A table the catalog gives no columns for is named
    /// alone.
    ///
    /// #27: told only the tables' names, an agent spent its next step on `describe`, about 4 KB
    /// on a host's bench, to learn the one column it got wrong; and a host whose agent has no
    /// `describe` had nowhere to send it. The columns are the catalog's, never the data's, and
    /// where the hint sends the agent for the rest is SQL, which every agent can run.
    fn shapes(&self, tables: &[&str]) -> (String, String) {
        if tables.is_empty() {
            return ("nothing yet".into(), String::new());
        }
        let named = &tables[..tables.len().min(SHOWN)];
        let found = self.columns.map(|f| f(named)).unwrap_or_default();
        // ` (+N more)` is inside the bound. Past `SHOWN` tables it is said whatever fits, so its
        // bytes for the fewest it can count are kept from the start, and the shares are of what
        // is left; the entries took the whole bound before it was put after them, and thirteen
        // tables were listed in 1,034 bytes (`the_list_a_hint_gives_is_held_to_1024_bytes_and_12_tables`).
        let counted = |n: usize| match n {
            0 => String::new(),
            n => format!(" (+{n} more)"),
        };
        let bound = SHAPES - counted(tables.len() - named.len()).len();
        let mut parts: Vec<(String, bool)> = Vec::new();
        let mut used = 0;
        for (i, t) in named.iter().enumerate() {
            let cols = found
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(t))
                .map_or(&[][..], |(_, c)| c.as_slice());
            let sep = if parts.is_empty() { 0 } else { 2 };
            let share = (bound.saturating_sub(used) / (named.len() - i)).saturating_sub(sep);
            let (entry, cut) = entry(t, cols, share)
                .unwrap_or_else(|| (crate::profile::ident(t), !cols.is_empty()));
            // A name alone can be longer than its share; past the bound it is counted instead.
            if used + sep + entry.len() > bound {
                break;
            }
            used += sep + entry.len();
            parts.push((entry, cut));
        }
        // One counted that was not kept for — a name past the bound, or a count of more digits —
        // takes the room of the entries before it, last first.
        while used + counted(tables.len() - parts.len()).len() > SHAPES {
            let Some((entry, _)) = parts.pop() else {
                break;
            };
            used -= entry.len() + if parts.is_empty() { 0 } else { 2 };
        }
        let short: Vec<&str> = named
            .iter()
            .zip(&parts)
            .filter_map(|(t, (_, cut))| cut.then_some(*t))
            .collect();
        let more = tables.len() - parts.len();
        let mut s = parts
            .iter()
            .map(|(entry, _)| entry.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if more > 0 {
            s.push_str(&format!(" (+{more} more)"));
        }
        let mut rest = match short[..] {
            [] => String::new(),
            [t] => format!(
                " `DESCRIBE {}` lists all its columns.",
                crate::profile::ident(t)
            ),
            _ => " `DESCRIBE <table>` lists all of a table's columns.".into(),
        };
        if more > 0 {
            rest.push_str(" `SELECT table_name FROM information_schema.tables` lists every table.");
        }
        (s, rest)
    }
}

/// `name(a, b, c)` in at most `room` bytes, and whether columns were left out to fit: as many as
/// fit, then `… [+N columns])`. `None` where not even the name and one column fit, and the table
/// is named alone.
fn entry(name: &str, cols: &[String], room: usize) -> Option<(String, bool)> {
    let name = crate::profile::ident(name);
    if cols.is_empty() {
        return (name.len() <= room).then_some((name, false));
    }
    let cols: Vec<String> = cols.iter().map(|c| crate::profile::ident(c)).collect();
    let all = format!("{name}({})", cols.join(", "));
    if all.len() <= room {
        return Some((all, false));
    }
    let mut s = format!("{name}(");
    for (i, c) in cols.iter().enumerate() {
        let item = if i == 0 { c.clone() } else { format!(", {c}") };
        // the mark that would follow this column, which counts the ones after it
        let mark = format!(", … [+{} columns])", cols.len() - i - 1);
        if s.len() + item.len() + mark.len() > room {
            if i == 0 {
                return None;
            }
            s.push_str(&format!(", … [+{} columns])", cols.len() - i));
            return Some((s, true));
        }
        s.push_str(&item);
    }
    // Every column fits with a mark after it, so all of them fit without one; not reached.
    Some((all, false))
}

/// What DuckDB calls the thing another engine spells differently. Each key is a name measured to
/// be absent from DuckDB v1.5.5 — `ifnull`, `iif`'s cousin `if`, `coalesce`, `try_cast`,
/// `percentile_cont` and `extract` all exist and are deliberately **not** here.
const DIALECT: &[(&str, &str)] = &[
    (
        "to_char",
        "strftime for timestamps (`strftime(ts, '%Y-%m')`) or printf for numbers",
    ),
    ("to_date", "strptime: `strptime(s, '%Y-%m-%d')::DATE`"),
    ("date_format", "strftime: `strftime(ts, '%Y-%m-%d')`"),
    ("str_to_date", "strptime: `strptime(s, '%Y-%m-%d')`"),
    ("nvl", "coalesce"),
    (
        "nvl2",
        "a CASE: `CASE WHEN x IS NOT NULL THEN a ELSE b END`",
    ),
    (
        "isnull",
        "`x IS NULL`, or coalesce for the two-argument form",
    ),
    ("getdate", "now()"),
    ("sysdate", "now()"),
    ("convert", "a cast: `CAST(x AS INTEGER)` or `x::INTEGER`"),
    ("dateadd", "interval arithmetic: `ts + INTERVAL 1 DAY`"),
    ("charindex", "position: `position(needle IN haystack)`"),
    ("iif", "a CASE, or `if(cond, a, b)` which DuckDB does have"),
    ("rand", "random()"),
    ("timestampdiff", "date_diff: `date_diff('day', a, b)`"),
];

/// Single-quoted string literals, with `''` handled as an escaped quote. Used to decide whether
/// an empty result is plausibly a vocabulary mistake rather than genuinely absent data.
pub fn literals(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] != '\'' {
            i += 1;
            continue;
        }
        let mut lit = String::new();
        i += 1;
        while i < b.len() {
            if b[i] == '\'' {
                if i + 1 < b.len() && b[i + 1] == '\'' {
                    lit.push('\'');
                    i += 2;
                    continue;
                }
                i += 1;
                break;
            }
            lit.push(b[i]);
            i += 1;
        }
        if !lit.is_empty() {
            out.push(lit);
        }
    }
    out
}

/// Whether `word` appears in `sql` as a whole identifier, case-insensitively. A column called
/// `meter` must not match because the query mentions `meter_id`.
pub fn mentions(sql: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let (hay, needle) = (sql.to_ascii_lowercase(), word.to_ascii_lowercase());
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(off) = hay[from..].find(&needle) {
        let s = from + off;
        let e = s + needle.len();
        let before = s == 0 || !hay[..s].chars().next_back().is_some_and(ident);
        let after = e == hay.len() || !hay[e..].chars().next().is_some_and(ident);
        if before && after {
            return true;
        }
        // One character on, not one byte: a name can start with `Ü`.
        from = s + hay[s..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let rest = &s[s.find(open)? + open.len()..];
    Some(&rest[..rest.find(close)?])
}

/// A fix for a statement DuckDB parsed but refused, or `None` when we have nothing useful to add.
///
/// `None` is the common case and is correct: an error we cannot improve on should be passed
/// through unchanged rather than padded.
pub fn for_sql_error(message: &str, catalogue: &Catalogue, cx: &Context) -> Option<String> {
    // Layer 2 of REQ-2 caught a file read. Say so as a fact about the bench, not as a missing
    // file — the agent's next move should be to use what is already here.
    if message.contains("Permission Error")
        && message.contains("file system operations are disabled")
    {
        // DuckDB 1.5.5 refuses a write in the same words as a read, so the statement tells them
        // apart: an agent that wanted a file out is not helped by hearing the data is in.
        if cx.statement.is_some_and(is_export) {
            return Some(format!(
                "There is no file output here: results come back through {}, and {} pages a \
                 long one by offset. Aggregate in SQL and read the answer, or CREATE TABLE to \
                 keep an intermediate result for the next query.",
                crate::tools::name("query"),
                crate::tools::name("get_result")
            ));
        }
        return Some(format!(
            "File and network functions are unavailable here by design. The data is already on \
             the bench as: {}. Query those directly — no read_csv, read_parquet or ATTACH needed.",
            catalogue.list()
        ));
    }

    if message.contains("Out of Memory Error") {
        return Some(
            "The engine hit its memory budget. Aggregate or filter before sorting or joining, \
             add a LIMIT, or ask the operator to start benchy with a larger --memory. Your \
             tables are intact."
                .into(),
        );
    }

    // DuckDB 1.5.5: `Cannot execute statement of type "UPDATE" on database "…" which is
    // attached in read-only mode!` — the message itself is made origin-blind by `scrub`.
    if message.contains("Cannot execute statement of type")
        && message.contains("attached in read-only mode")
    {
        // The table the statement wrote to, when it can be read off it: the scratch tables are
        // in the catalogue too, and naming one of those as the thing to copy would mislead.
        let target = cx.statement.and_then(written_table);
        let example = target.as_deref().unwrap_or("the_table");
        return Some(format!(
            "loaded tables are read-only; `CREATE TABLE my_{example} AS SELECT * FROM {example}` \
             makes your own copy, which you can change."
        ));
    }

    // #27: what is here, with the columns, so the next statement can be right without a call to
    // `describe`, which a host's agent may not have.
    if let Some(name) = between(message, "Table with name ", " does not exist") {
        let all: Vec<&str> = catalogue.names.iter().map(String::as_str).collect();
        let (here, rest) = catalogue.shapes(&all);
        if cx.unavailable.iter().any(|u| u.eq_ignore_ascii_case(name)) {
            return Some(format!(
                "`{name}` is not available this session: it could not be loaded. What is here: \
                 {here}.{rest}"
            ));
        }
        return Some(format!(
            "There is no `{name}` on the bench. What is here: {here}.{rest}"
        ));
    }

    if let Some(name) = between(message, "Scalar Function with name ", " does not exist") {
        // A backticked identifier reaches the binder as a function named "`__postfix".
        if name.starts_with('`') {
            return Some(
                "DuckDB quotes identifiers with double quotes, not backticks: write \
                 \"my column\" rather than `my column`. Single quotes are for string literals."
                    .into(),
            );
        }
        let lower = name.to_ascii_lowercase();
        if let Some((_, fix)) = DIALECT.iter().find(|(k, _)| *k == lower) {
            return Some(format!("DuckDB spells that {fix}."));
        }
        return Some(format!(
            "DuckDB has no `{name}`. `SELECT DISTINCT function_name FROM duckdb_functions() \
             WHERE function_name ILIKE '%{}%'` lists what it does have.",
            lower.split('_').next_back().unwrap_or(&lower)
        ));
    }

    // `SELECT "hello"` — a double-quoted string in another dialect, an identifier here.
    if message.contains("was not found because the FROM clause is missing") {
        return Some(
            "Double quotes mean an identifier in DuckDB. For a string literal use single \
             quotes: 'hello'."
                .into(),
        );
    }

    // #27: an unknown column is told the columns of the tables the statement reads. DuckDB 1.5.5
    // words it three ways: unqualified, `Binder Error: Referenced column "temp" not found in FROM
    // clause!`; qualified by a table or its alias, `Binder Error: Table "r" does not have a
    // column named "temp"`; and qualified by a view, a CTE or a subquery, `Binder Error: Values
    // list "c" does not have a column named "temp"`. Only the first had a hint, of the tables'
    // names, and it sent the agent to `describe` for their columns.
    if message.contains("Binder Error") && message.contains("not found in FROM clause") {
        let read = tables_read(cx.statement.unwrap_or(""), catalogue.names);
        if read.is_empty() {
            let all: Vec<&str> = catalogue.names.iter().map(String::as_str).collect();
            let (here, rest) = catalogue.shapes(&all);
            return Some(format!(
                "That column is not in what the statement reads. On the bench: {here}.{rest}"
            ));
        }
        let (read, rest) = catalogue.shapes(&read);
        return Some(format!(
            "That column is not in the tables the statement reads: {read}.{rest}"
        ));
    }
    // DuckDB 1.5.5 says `Values list` of a view, a helper's included, as of a CTE or subquery.
    let qualified = |of: &str| {
        between(
            message,
            &format!("Binder Error: {of} \""),
            "\" does not have a column named ",
        )
    };
    let (q, view) = match (qualified("Table"), qualified("Values list")) {
        (Some(q), _) => (q, false),
        (None, Some(q)) => (q, true),
        (None, None) => return None,
    };
    let statement = cx.statement.unwrap_or("");
    let mut read = tables_read(statement, catalogue.names);
    // The alias first: `FROM meters m JOIN rooms meters` qualifies `rooms`'s columns `meters.`.
    let table = aliased(statement, q, &read)
        .and_then(|t| read.iter().position(|r| *r == t))
        .or_else(|| read.iter().position(|t| t.eq_ignore_ascii_case(q)));
    let said = match table {
        Some(i) => {
            // what the agent qualified the column with first: it is what it was looking at
            let t = read.remove(i);
            read.insert(0, t);
            if t.eq_ignore_ascii_case(q) {
                format!("That column is not in `{t}`.")
            } else {
                format!("`{q}` is `{t}`.")
            }
        }
        None if view => format!("`{q}` is a subquery or CTE of the statement."),
        None => format!("`{q}` has no such column."),
    };
    if read.is_empty() {
        return None;
    }
    let (read, rest) = catalogue.shapes(&read);
    Some(format!(
        "{said} The tables the statement reads: {read}.{rest}"
    ))
}

/// The words of `statement` as DuckDB 1.5.5's lexer reads it (`crate::guard::lex`): a name,
/// quoted or not, as the name, and any other mark as itself, so that two names either side of a
/// `,` are not read as a table and its alias. A literal is a word of its own, `'`, which no name
/// is; comments are left out.
fn words(statement: &str) -> Vec<String> {
    use crate::guard::Part;
    let mut out = Vec::new();
    // A scan that stops (`guard::Stuck`) reads no word, and the hint names no table.
    for (part, r) in crate::guard::lex(statement).unwrap_or_default() {
        let text = &statement[r];
        match part {
            Part::Code => {
                let mut word = String::new();
                for c in text.chars() {
                    // What continues a word in DuckDB's lexer.
                    if c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii() {
                        word.push(c);
                        continue;
                    }
                    if !word.is_empty() {
                        out.push(std::mem::take(&mut word));
                    }
                    if !c.is_whitespace() {
                        out.push(c.to_string());
                    }
                }
                if !word.is_empty() {
                    out.push(word);
                }
            }
            Part::Name => out.push(
                text.strip_prefix('"')
                    .and_then(|t| t.strip_suffix('"'))
                    .unwrap_or(text)
                    .replace("\"\"", "\""),
            ),
            Part::Literal => out.push("'".into()),
            Part::Comment => {}
        }
    }
    out
}

/// The tables of `names` that `statement` names, in the order it first names them, as `names`
/// spells them. A name is matched as DuckDB matches one, ASCII case folded. A column of the same
/// name as a table counts too: the hint then lists a table more, which costs bytes and misleads
/// no one.
///
/// A name the statement gives is not the table of that name. An alias, `rooms AS meters` or
/// `rooms meters`, a subquery's or a column's, is not read where it is given, nor where it
/// qualifies a column, `meters.power_kw`: that is `rooms`'s, and listing `meters` first, as the
/// table qualified, sent the agent to a table the statement does not read. A CTE's name is not
/// read anywhere but in its own body, where DuckDB 1.5.5 binds it to the table, unless the CTE
/// is `RECURSIVE` (`a_name_the_statement_gives_hides_the_table_of_that_name`).
fn tables_read<'n>(statement: &str, names: &'n [String]) -> Vec<&'n str> {
    let w = words(statement);
    let is = |i: usize, s: &str| w.get(i).is_some_and(|x| x.eq_ignore_ascii_case(s));
    let table = |x: &str| names.iter().find(|n| n.eq_ignore_ascii_case(x));
    let given =
        |i: usize| i > 0 && (is(i - 1, "as") || w[i - 1] == ")" || table(&w[i - 1]).is_some());
    let ctes: Vec<(&str, std::ops::Range<usize>)> = (0..w.len())
        .filter_map(|i| Some((w[i].as_str(), cte(&w, i)?)))
        .collect();
    let recursive = w.iter().any(|x| x.eq_ignore_ascii_case("recursive"));
    let mut read: Vec<&str> = Vec::new();
    for (i, x) in w.iter().enumerate() {
        let Some(n) = table(x) else {
            continue;
        };
        let a_cte = ctes
            .iter()
            .any(|(c, body)| c.eq_ignore_ascii_case(x) && (recursive || !body.contains(&i)));
        let an_alias = is(i + 1, ".")
            && (0..w.len()).any(|j| j != i && given(j) && w[j].eq_ignore_ascii_case(x));
        if given(i) || a_cte || an_alias {
            continue;
        }
        if !read.contains(&n.as_str()) {
            read.push(n);
        }
    }
    read
}

/// The words of the body of the CTE that `w[i]` names, `name AS [NOT] [MATERIALIZED] (…)` or
/// `name(a, b) AS (…)`, from its `(` to its `)`.
fn cte(w: &[String], i: usize) -> Option<std::ops::Range<usize>> {
    let mut j = i + 1;
    if w.get(j)? == "(" {
        j = closing(w, j)? + 1;
    }
    if !w.get(j)?.eq_ignore_ascii_case("as") {
        return None;
    }
    j += 1;
    for word in ["not", "materialized"] {
        if w.get(j)?.eq_ignore_ascii_case(word) {
            j += 1;
        }
    }
    (w.get(j)? == "(").then_some(())?;
    Some(j..closing(w, j)?)
}

/// Where the `(` at `w[open]` is closed.
fn closing(w: &[String], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (k, x) in w.iter().enumerate().skip(open) {
        match x.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            _ => {}
        }
    }
    None
}

/// The table of `read` that `alias` stands for in `statement`: `readings r` or `readings AS r`,
/// in a FROM or a JOIN.
fn aliased<'n>(statement: &str, alias: &str, read: &[&'n str]) -> Option<&'n str> {
    let w = words(statement);
    (0..w.len()).find_map(|i| {
        let table = read.iter().find(|t| t.eq_ignore_ascii_case(&w[i]))?;
        let next = w.get(i + 1)?;
        let named = if next.eq_ignore_ascii_case("as") {
            w.get(i + 2)?
        } else {
            next
        };
        named.eq_ignore_ascii_case(alias).then_some(*table)
    })
}

/// REQ-15: an error says nothing of how the bench is built. DuckDB names the catalog a loaded
/// table is attached as (`on database "benchy_1" which is attached in read-only mode!`), and the
/// name is the bench's, not the data's: it is said as what it means to the agent instead.
pub fn scrub(message: &str) -> String {
    let Some(at) = message.find(" on database \"") else {
        return message.to_string();
    };
    let rest = &message[at + " on database \"".len()..];
    let Some(end) = rest.find('"') else {
        return message.to_string();
    };
    let tail = &rest[end + 1..];
    match tail.strip_prefix(" which is attached in read-only mode!") {
        Some(after) => format!(
            "{} on a loaded table, which is read-only{after}",
            &message[..at]
        ),
        None => message.to_string(),
    }
}

/// The plain name a write names as its target — `UPDATE t`, `INSERT INTO t`, `DELETE FROM t`,
/// `DROP TABLE t`, `ALTER TABLE t` — or `None` when it is not a plain identifier.
fn written_table(statement: &str) -> Option<String> {
    let words: Vec<&str> = statement.split_whitespace().collect();
    let first = words.first()?.to_ascii_uppercase();
    let after = match first.as_str() {
        "UPDATE" => 1,
        "INSERT" | "DELETE" | "DROP" | "ALTER" => 2,
        _ => return None,
    };
    let name = words.get(after)?.trim_end_matches(['(', ';', ',']);
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    plain.then(|| name.to_string())
}

/// Whether a statement writes a file: `COPY … TO` or `EXPORT DATABASE`.
fn is_export(statement: &str) -> bool {
    let words: Vec<String> = statement
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_uppercase())
        .collect();
    match words.first().map(String::as_str) {
        Some("EXPORT") => true,
        Some("COPY") => words.iter().any(|w| w == "TO"),
        _ => false,
    }
}

/// Said wherever the agent may assume a zone of its own: the instructions, and a refused
/// `SET TimeZone`. The lockdown pins every connection to UTC (REQ-10
/// `the_agent_connection_is_utc_on_a_host_that_is_not`), and nothing said so, so an agent asked
/// for "per day" bucketed UTC days without a word. The example is run on a loaded TIMESTAMPTZ
/// column and gives Berlin's days (`the_agent_is_told_times_are_utc_and_how_to_bucket_local_days`).
pub const UTC: &str = "Times are UTC. For local calendar periods, convert first, e.g. \
                       `date_trunc('day', timezone('Europe/Berlin', ts))`.";

/// Whether `statement` sets or resets `TimeZone`: `SET [GLOBAL|SESSION|LOCAL] TimeZone …` or
/// `RESET …`, in any case, the name quoted or not, comments anywhere.
pub fn sets_time_zone(statement: &str) -> bool {
    use crate::guard::Part;
    let mut words = Vec::new();
    // A scan that stops (`guard::Stuck`) reads no word, and so no SET.
    for (part, r) in crate::guard::lex(statement).unwrap_or_default() {
        let text = &statement[r];
        match part {
            Part::Code => words.extend(
                text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .filter(|w| !w.is_empty())
                    .map(str::to_ascii_lowercase),
            ),
            Part::Name => words.push(text.trim_matches('"').to_ascii_lowercase()),
            Part::Literal => words.push(String::new()),
            Part::Comment => {}
        }
        if words.len() >= 3 {
            break;
        }
    }
    let mut w = words.iter().map(String::as_str);
    if !matches!(w.next(), Some("set" | "reset")) {
        return false;
    }
    match w.next() {
        Some("global" | "session" | "local") => w.next() == Some("timezone"),
        name => name == Some("timezone"),
    }
}

/// The `index`th statement (from 0) of a batch, as written ([`statements`]).
pub fn statement(sql: &str, index: usize) -> Option<String> {
    statements(sql).into_iter().nth(index)
}

/// The statements of a batch, as written: split at the `;`s that are not in a string literal, a
/// quoted name or a comment, as DuckDB 1.5.5's lexer finds those (`crate::guard::lex`), with
/// empty statements skipped as DuckDB skips them. Comments are dropped from what is returned,
/// and nothing else: every literal and quoted name is kept whole, so a `--` or `/*` in a
/// `$$…$$` or `E'…'` string is the string's (`page_sql_keeps_every_literal_whole`). Before,
/// this read `'…'` and `"…"` alone, and `SELECT $$a -- b$$ AS y, 'q' AS z` became
/// `SELECT $$a`. It is still not DuckDB's parser, so a caller that must be exact checks the
/// count against DuckDB's.
pub fn statements(sql: &str) -> Vec<String> {
    use crate::guard::Part;
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    // A scan that stops (`guard::Stuck`) finds no statement, so no result is given SQL.
    for (part, r) in crate::guard::lex(sql).unwrap_or_default() {
        let text = &sql[r];
        match part {
            Part::Code => {
                let mut pieces = text.split(';');
                if let Some(first) = pieces.next() {
                    cur.push_str(first);
                }
                for piece in pieces {
                    out.push(std::mem::take(&mut cur));
                    cur.push_str(piece);
                }
            }
            Part::Name | Part::Literal => cur.push_str(text),
            // `1/*c*/FROM` is `1 FROM`; a `--` comment ends at a line break, which is kept.
            Part::Comment if text.starts_with("/*") => cur.push(' '),
            Part::Comment => {}
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A fix for a batch that never parsed. Takes the SQL because a parser error names a position,
/// not a construct: `syntax error at or near "5"` is only recognisable as `TOP 5` if you look.
pub fn for_parse_error(message: &str, sql: &str) -> Option<String> {
    if message.contains("locking clause is not supported") {
        return Some(
            "There is nothing to lock on the bench — the workspace is yours alone and nobody \
             else can write to it. Drop the FOR UPDATE."
                .into(),
        );
    }
    if message.contains("syntax error") && has_top_n(sql) {
        return Some("DuckDB has no `TOP n`. Use `SELECT ... LIMIT n`.".into());
    }
    None
}

/// `TOP` followed by a number, as a whole word — so a column called `topic` or `stop` does not
/// trigger it.
fn has_top_n(sql: &str) -> bool {
    let b = sql.as_bytes();
    let mut i = 0;
    while let Some(off) = sql[i..].to_ascii_uppercase().find("TOP") {
        let s = i + off;
        let e = s + 3;
        let before_ok = s == 0 || !(b[s - 1] as char).is_alphanumeric() && b[s - 1] != b'_';
        let after = sql[e..].trim_start();
        if before_ok
            && e < sql.len()
            && (b[e] as char).is_whitespace()
            && after.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            return true;
        }
        i = e;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_n_is_recognised_without_catching_ordinary_words() {
        assert!(has_top_n("SELECT TOP 5 * FROM t"));
        assert!(has_top_n("select top 10 a from t"));
        assert!(!has_top_n("SELECT topic FROM t"));
        assert!(!has_top_n("SELECT stop FROM t"));
        assert!(!has_top_n("SELECT top FROM t"));
        assert!(!has_top_n("SELECT a_top 5 FROM t"));
    }

    #[test]
    fn a_statement_is_found_in_its_batch() {
        let sql =
            "SELECT ';' AS a; -- a; comment\n COPY t TO 'x.csv'; /* ; */ ;EXPORT DATABASE 'd'";
        assert_eq!(statement(sql, 0).as_deref(), Some("SELECT ';' AS a"));
        assert_eq!(statement(sql, 1).as_deref(), Some("COPY t TO 'x.csv'"));
        assert_eq!(statement(sql, 2).as_deref(), Some("EXPORT DATABASE 'd'"));
        assert_eq!(statement(sql, 3), None);
        assert!(is_export("COPY t TO 'x.csv'") && is_export("copy (select 1) to 'x'"));
        assert!(!is_export("COPY t FROM 'x.csv'") && !is_export("SELECT * FROM read_csv('x')"));
        assert_eq!(
            scrub("Invalid Input Error: Cannot execute statement of type \"UPDATE\" on database \"benchy_1\" which is attached in read-only mode!"),
            "Invalid Input Error: Cannot execute statement of type \"UPDATE\" on a loaded table, which is read-only"
        );
    }

    #[test]
    fn literals_are_extracted_including_escaped_quotes() {
        assert_eq!(literals("SELECT * FROM t WHERE a = 'lab'"), vec!["lab"]);
        assert_eq!(
            literals("WHERE a = 'x' AND b = 'y z'"),
            vec!["x".to_string(), "y z".to_string()]
        );
        assert_eq!(literals("WHERE a = 'it''s'"), vec!["it's"]);
        assert!(literals("SELECT count(*) FROM t").is_empty());
        assert!(literals("WHERE a = ''").is_empty());
    }

    #[test]
    fn mentions_matches_whole_identifiers_only() {
        assert!(mentions("SELECT meter FROM t", "meter"));
        assert!(mentions("SELECT \"Meter\" FROM t", "meter"));
        assert!(!mentions("SELECT meter_id FROM t", "meter"));
        assert!(!mentions("SELECT submeter FROM t", "meter"));
        assert!(mentions("SELECT a FROM meters WHERE meter='x'", "meter"));
        assert!(!mentions("SELECT 1", ""));
    }

    #[test]
    fn mentions_survives_a_name_that_starts_with_a_multibyte_letter() {
        // The first match is only a prefix of `Überstunden_2024`, and stepping one byte past it
        // landed inside `Ü`: a panic in the engine, which cost the agent its bench.
        let sql = "SELECT \"Überstunden_2024\" FROM emp WHERE \"Überstunden\" = 'nope'";
        assert!(mentions(sql, "Überstunden"));
        assert!(!mentions(
            "SELECT \"Überstunden_2024\" FROM emp",
            "Überstunden"
        ));
        assert!(mentions("SELECT größe FROM t", "größe"));
    }

    #[test]
    fn a_message_we_cannot_improve_gets_no_hint() {
        let names = ["t".to_string()];
        assert!(for_sql_error(
            "Conversion Error: something odd",
            &Catalogue::of(&names),
            &Context::default()
        )
        .is_none());
        assert!(for_parse_error("Parser Error: unexpected token", "SELECT 1").is_none());
    }

    #[test]
    fn statements_keep_every_literal_whole() {
        // #16: only comments are dropped. Each literal form as DuckDB 1.5.5 lexes it
        // (`crate::guard::lex`), and a quoted name, keeps a `--`, a `/*` and a `;` inside it.
        for sql in [
            "SELECT $$a/*b*/c$$ AS x",
            "SELECT $$a -- b$$ AS y, 'q' AS z",
            "SELECT $t$a;/*b*/ -- c$t$ AS x",
            "SELECT $é$a--b$é$ AS x",
            "SELECT E'it\\'s /*b*/ -- c' AS x",
            "SELECT e'a\\\\' AS w, '--' AS x",
            "SELECT 'a--b', 'it''s /*c*/;' AS x",
            "SELECT 'a\\' AS w, '/*' AS x",
            "SELECT 1 AS \"a--b/*c*/;d\"",
        ] {
            assert_eq!(statements(sql), vec![sql.to_string()], "{sql}");
        }
        assert_eq!(
            statements("SELECT 1 /* a /* b */ ' */ AS x; -- it's\nSELECT $$a;b$$ AS y;;"),
            vec!["SELECT 1   AS x", "SELECT $$a;b$$ AS y"]
        );
        assert_eq!(
            statements("SELECT 1 -- c\nFROM t -- d"),
            vec!["SELECT 1 \nFROM t"]
        );
        // `x$a$` is a name, not a dollar quote, and `xE'` no prefix.
        assert_eq!(
            statements("SELECT 1 AS x$a$; SELECT 2 AS z$a$"),
            vec!["SELECT 1 AS x$a$", "SELECT 2 AS z$a$"]
        );
        assert_eq!(
            statements("SELECT xE'a\\' AS v; SELECT 2"),
            vec!["SELECT xE'a\\' AS v", "SELECT 2"]
        );
    }
}
