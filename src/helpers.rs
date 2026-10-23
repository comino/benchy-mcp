//! SQL helpers an operator supplies (REQ-18): views and macros the agent calls like built-ins,
//! read from the config at startup and fixed for the session. No protocol call adds one.
//!
//! **Where they are made.** DuckDB 1.5.5 binds a view's or a macro's body when it is created:
//! `CREATE VIEW daily AS … FROM readings` fails with "Table with name readings does not exist!"
//! until `readings` is there, and so does a table macro, measured. A host attaches its tables
//! after the engine is sealed and confined, when the engine can no longer write a file, and a
//! helper must sit in a catalog attached `READ_ONLY` so that DuckDB, not a check of ours, refuses
//! every `DROP`, `ALTER` or `CREATE OR REPLACE` of it. So a helper is made by a loader job of its
//! own (`Job::Helpers`), confined to its staging directory and locked down as the engine is, in a
//! file of its own, against **empty stand-ins** of the bench's tables: the same names and column
//! types, no rows. The stand-ins stay behind in the loader. What the file keeps is each helper's
//! SQL, which DuckDB binds again on every query (measured: a view made over a stand-in reads the
//! attached table, and still reads it after the table is replaced by one with other types).
//!
//! A view that reads a table the bench does not have yet **waits**: it is listed, its name is
//! taken, and calling it says what it waits for. The next attach of that table makes it.
//!
//! **A macro reads no table.** DuckDB 1.5.5 expands a macro, scalar or table, inside the caller's
//! query, so a table its body names binds to a CTE of the caller's of that name: `WITH meters AS
//! (…) SELECT lab_peak()` returned the CTE's maximum, with no error, where a view over `meters`
//! read the table, measured. So a helper macro whose body reads a table is refused
//! ([`macro_body`]), and what reads a table is a view. No query of the agent's is looked at for
//! a CTE then.
//!
//! **A macro calls only DuckDB and the helpers.** DuckDB 1.5.5 checks only part of a macro's body
//! for the functions it calls when it is made; the rest it binds where the macro is called, where
//! a macro the agent made later under a name nothing had would answer for it, measured. So a
//! helper macro that calls a function neither DuckDB nor a helper has, or one named by a schema,
//! is refused ([`unresolved`]). A view is bound when it is made, here, and again in its own
//! catalog, where nothing of the agent's stood in for a function, measured.
//!
//! **Unqualified names.** A helper names the bench's tables unqualified, and that is safe: the
//! agent's search path resolves them at query time, a table's catalog is renamed by every attach
//! (`benchy_<n>`), so a qualified name could not stay right, and no object of the agent's may take
//! a table's name (REQ-9 `an_agent_object_cannot_shadow_a_host_object_or_builtin`), so the
//! unqualified name can only mean the table.

use duckdb::ffi;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, CString};

/// What kind of helper a statement makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    View,
    Macro,
}

/// One helper as the operator wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Def {
    pub name: String,
    pub kind: Kind,
    /// The statement, without its `;`.
    pub sql: String,
    /// The `--` lines directly above it, with no blank line between, joined into one line, whole:
    /// [`Shown`] cuts it at [`COMMENT_BYTES`], and [`cuts`] tells the operator what that cut.
    pub comment: Option<String>,
}

/// A helper as `describe` shows it: made, or waiting for a table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub name: String,
    /// `dewpoint(t, rh)`, `hours_of(start_at, end_at)`, or a view's name and columns.
    pub signature: String,
    /// `view`, `macro` or `table macro`.
    pub what: String,
    pub comment: Option<String>,
    /// The table it reads that the bench does not have yet; `None` once it is made.
    pub waits: Option<String>,
}

/// What a build made of the operator's helpers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Built {
    /// Every helper, in the config's order, made or waiting.
    pub shown: Vec<Shown>,
    /// Why a helper could not be made, for the operator. Any of these stops startup.
    pub broken: Vec<String>,
    /// The bench's tables the helpers read, ASCII case folded, sorted: each table a made helper
    /// reads, as DuckDB bound it, and the table each waiting helper waits for. The agent may give
    /// nothing of its own one of these names, on the bench or not (REQ-18
    /// `a_table_a_helper_reads_stays_taken_after_a_detach`).
    #[serde(default)]
    pub reads: Vec<String>,
}

impl Built {
    /// The helpers that wait, and the table each waits for.
    pub fn waiting(&self) -> impl Iterator<Item = (&str, &str)> {
        self.shown
            .iter()
            .filter_map(|s| s.waits.as_deref().map(|w| (s.name.as_str(), w)))
    }
}

/// The longest comment `describe` carries for one helper.
const COMMENT_BYTES: usize = 200;

/// The longest line `describe` gives one helper, the cut mark aside: `  `, the signature, ` — `,
/// the kind, `: ` and the comment, and for a waiting view what it waits for ([`line`]). Without
/// the signature, a line is at most 2 + 5 + 11 (`table macro`) + 2 + 218 (a comment cut at
/// [`COMMENT_BYTES`], and its mark of up to 18 bytes) = 238 bytes, so a signature of up to 82
/// bytes keeps a whole comment: `dewpoint(t, rh)` is 15, `hourly (meter, hour, mean_kw, peak_kw,
/// readings)` 51. Past that, a view of many columns or a macro of many parameters gives up the
/// end of its comment, which keeps one helper from taking the room of several in `describe`'s
/// byte budget; the operator is told which ([`cuts`]).
const LINE_BYTES: usize = 320;

/// The operator's helpers SQL, split into its statements, each checked to be `CREATE VIEW
/// <name>` or `CREATE MACRO <name>` (or `CREATE FUNCTION`, DuckDB's other name for a macro), with
/// a plain name no other helper has. The statement's kind and name are read here only to say so
/// early and to name it in `describe`; what DuckDB made of it is checked against them in the
/// build (`build`), which is what counts.
pub fn parse(sql: &str) -> Result<Vec<Def>, String> {
    let mut out: Vec<Def> = Vec::new();
    for (i, piece) in split(sql)?.into_iter().enumerate() {
        let n = i + 1;
        let (kind, name) = header(&piece.text).map_err(|why| {
            format!(
                "helper statement {n} ({}) {why}. A helper is `CREATE VIEW <name> AS …` or \
                 `CREATE MACRO <name>(…) AS …`, nothing else.",
                preview(&piece.text)
            )
        })?;
        if name.len() > 63 {
            return Err(format!(
                "helper `{name}`: a name has at most 63 characters."
            ));
        }
        if name.to_ascii_lowercase().starts_with("benchy_") {
            return Err(format!(
                "helper `{name}`: names starting `benchy_` are benchy's own. Pick another."
            ));
        }
        if let Some(d) = out.iter().find(|d| d.name.eq_ignore_ascii_case(&name)) {
            return Err(format!(
                "two helpers are named `{}`: DuckDB does not tell names apart by case, so each \
                 needs a name of its own.",
                d.name
            ));
        }
        out.push(Def {
            name,
            kind,
            sql: piece.text,
            comment: piece.comment,
        });
    }
    if out.is_empty() {
        return Err("the helpers hold no statement.".into());
    }
    Ok(out)
}

fn preview(s: &str) -> String {
    let flat: String = strip_trivia(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut = crate::result::cut(flat, 48);
    format!("`{cut}`")
}

/// One statement of the helpers SQL, and the comment above it.
#[derive(Debug, PartialEq, Eq)]
struct Piece {
    text: String,
    comment: Option<String>,
}

/// Split SQL at the `;` that end its statements, outside quotes, comments and dollar quotes.
/// Statements that are only whitespace and comments are left out. DuckDB's own parser is the
/// judge of where a statement ends: the build refuses a piece it does not read as exactly one
/// statement, so a split that disagrees with it stops startup rather than making something else.
fn split(sql: &str) -> Result<Vec<Piece>, String> {
    let b = sql.as_bytes();
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' => {
                let q = b[i];
                // An E'…' string takes backslash escapes.
                let escapes = q == b'\''
                    && i > 0
                    && matches!(b[i - 1], b'e' | b'E')
                    && (i < 2 || !ident(b[i - 2]));
                i += 1;
                loop {
                    match b.get(i) {
                        None => return Err("a quote is never closed in the helpers.".into()),
                        Some(b'\\') if escapes => i += 2,
                        Some(&c) if c == q => {
                            if b.get(i + 1) == Some(&q) {
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(_) => i += 1,
                    }
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while depth > 0 {
                    match (b.get(i), b.get(i + 1)) {
                        (None, _) => {
                            return Err("a /* comment is never closed in the helpers.".into())
                        }
                        (Some(b'/'), Some(b'*')) => {
                            depth += 1;
                            i += 2;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            depth -= 1;
                            i += 2;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'$' if i == 0 || !ident(b[i - 1]) => {
                // $tag$ … $tag$, the tag empty or an identifier that does not start with a digit
                let mut j = i + 1;
                while j < b.len() && ident(b[j]) {
                    j += 1;
                }
                let tag_ok =
                    b.get(j) == Some(&b'$') && !b.get(i + 1).is_some_and(u8::is_ascii_digit);
                if tag_ok {
                    let tag = &sql[i..=j];
                    match sql[j + 1..].find(tag) {
                        Some(at) => i = j + 1 + at + tag.len(),
                        None => return Err("a $-quote is never closed in the helpers.".into()),
                    }
                } else {
                    i += 1;
                }
            }
            b';' => {
                push_piece(&mut pieces, &sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_piece(&mut pieces, &sql[start..]);
    Ok(pieces)
}

fn push_piece(pieces: &mut Vec<Piece>, text: &str) {
    if strip_trivia(text).is_empty() {
        return;
    }
    pieces.push(Piece {
        text: text.trim().to_string(),
        comment: comment_above(text),
    });
}

/// The `--` lines directly above a statement, joined into one: the block that ends on the line
/// before its first word, with no blank line between.
fn comment_above(text: &str) -> Option<String> {
    let lead_len = text.len() - strip_trivia(text).len();
    let lead = &text[..lead_len];
    let mut lines: Vec<&str> = Vec::new();
    // The newline that ends the line before the statement's, and nothing more: a blank line
    // left after it separates a comment from the statement.
    let lead = lead.trim_end_matches([' ', '\t']);
    let lead = lead.strip_suffix('\n').unwrap_or(lead);
    let lead = lead.strip_suffix('\r').unwrap_or(lead);
    for line in lead.split('\n').rev() {
        match line.trim().strip_prefix("--") {
            Some(c) => lines.push(c.trim()),
            None => break,
        }
    }
    lines.reverse();
    let joined = lines
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// `text` from its first word on: leading whitespace and comments gone.
fn strip_trivia(text: &str) -> &str {
    let mut s = text;
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.find('\n').map_or("", |n| &rest[n + 1..]);
        } else if let Some(rest) = t.strip_prefix("/*") {
            s = rest.find("*/").map_or("", |n| &rest[n + 2..]);
        } else {
            return t;
        }
    }
}

/// The kind and name a helper statement declares, read from its first three words.
fn header(text: &str) -> Result<(Kind, String), String> {
    let s = strip_trivia(text);
    let word = |s: &str| -> (String, usize) {
        let t = s.trim_start();
        let lead = s.len() - t.len();
        let end = t.find(char::is_whitespace).unwrap_or(t.len());
        (t[..end].to_string(), lead + end)
    };
    let (create, at) = word(s);
    if !create.eq_ignore_ascii_case("create") {
        return Err("is not a CREATE".into());
    }
    let (rest, at2) = word(&s[at..]);
    let kind = if rest.eq_ignore_ascii_case("view") {
        Kind::View
    } else if rest.eq_ignore_ascii_case("macro") || rest.eq_ignore_ascii_case("function") {
        Kind::Macro
    } else {
        return Err(format!(
            "creates `{rest}`: OR REPLACE, TEMP and every other kind of object are not helpers"
        ));
    };
    let tail = s[at + at2..].trim_start();
    let end = tail
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(tail.len());
    let name = &tail[..end];
    let next = tail[end..].chars().next();
    let plain = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && matches!(next, None | Some('(') | Some(' ' | '\t' | '\n' | '\r'));
    if !plain || name.eq_ignore_ascii_case("if") {
        return Err(
            "has no plain name: give it one of letters, digits and underscores, unquoted and \
             unqualified, with no IF NOT EXISTS"
                .into(),
        );
    }
    Ok((kind, name.to_string()))
}

/// Make `defs` in the database `bench` holds open, in order, against empty stand-ins of
/// `tables` (name, and each column's name and DuckDB type), locked down as the engine is. Run by
/// a loader job, confined to its staging directory; the file it leaves is the helpers' catalog.
pub fn build(
    bench: &crate::Bench,
    defs: &[Def],
    tables: &[(String, Vec<(String, String)>)],
    memory_limit: &str,
) -> Built {
    match build_in(bench, defs, tables, memory_limit) {
        Ok(b) => b,
        Err(e) => Built {
            broken: vec![format!("the helpers could not be built: {e}")],
            ..Built::default()
        },
    }
}

/// A raw connection, closed when dropped.
struct Raw(ffi::duckdb_connection);

impl Drop for Raw {
    fn drop(&mut self) {
        unsafe { ffi::duckdb_disconnect(&mut self.0) }
    }
}

impl Raw {
    fn exec(&self, sql: &str) -> Result<(), String> {
        self.rows(sql).map(|_| ())
    }

    /// Every value of the result, as text.
    fn rows(&self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        let c = CString::new(sql).map_err(|_| "NUL byte".to_string())?;
        unsafe {
            let mut r: ffi::duckdb_result = std::mem::zeroed();
            let ok =
                ffi::duckdb_query(self.0, c.as_ptr(), &mut r) == ffi::duckdb_state_DuckDBSuccess;
            let out = if ok {
                let cols = ffi::duckdb_column_count(&mut r);
                Ok((0..ffi::duckdb_row_count(&mut r))
                    .map(|row| {
                        (0..cols)
                            .map(|col| {
                                if ffi::duckdb_value_is_null(&mut r, col, row) {
                                    return String::new();
                                }
                                let v = ffi::duckdb_value_varchar(&mut r, col, row);
                                let s = CStr::from_ptr(v).to_string_lossy().into_owned();
                                ffi::duckdb_free(v as *mut std::ffi::c_void);
                                s
                            })
                            .collect()
                    })
                    .collect())
            } else {
                Err(result_error(&mut r))
            };
            ffi::duckdb_destroy_result(&mut r);
            out
        }
    }

    /// Prepare and run one helper statement, which must be exactly one CREATE.
    fn create(&self, sql: &str) -> Result<(), Failed> {
        let c = CString::new(sql).map_err(|_| Failed::Other("it holds a NUL byte".into()))?;
        unsafe {
            let mut ex: ffi::duckdb_extracted_statements = std::ptr::null_mut();
            let n = ffi::duckdb_extract_statements(self.0, c.as_ptr(), &mut ex);
            let out = (|| {
                if n != 1 {
                    let e = ffi::duckdb_extract_statements_error(ex);
                    return Err(Failed::Other(if n == 0 && !e.is_null() {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    } else {
                        format!("DuckDB reads it as {n} statements, not one")
                    }));
                }
                let mut prep: ffi::duckdb_prepared_statement = std::ptr::null_mut();
                if ffi::duckdb_prepare_extracted_statement(self.0, ex, 0, &mut prep)
                    != ffi::duckdb_state_DuckDBSuccess
                {
                    let e = ffi::duckdb_prepare_error(prep);
                    let msg = if e.is_null() {
                        "it could not be prepared".to_string()
                    } else {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    };
                    ffi::duckdb_destroy_prepare(&mut prep);
                    return Err(Failed::classify(msg));
                }
                // The guard's own test, then the one kind a helper may be.
                let kind = ffi::duckdb_prepared_statement_type(prep);
                if !crate::guard::is_allowed(kind)
                    || kind != ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE
                {
                    ffi::duckdb_destroy_prepare(&mut prep);
                    return Err(Failed::Other(format!(
                        "it is {}, and a helper is a CREATE",
                        crate::guard::type_name(kind)
                    )));
                }
                let mut r: ffi::duckdb_result = std::mem::zeroed();
                let ok =
                    ffi::duckdb_execute_prepared(prep, &mut r) == ffi::duckdb_state_DuckDBSuccess;
                let out = if ok {
                    Ok(())
                } else {
                    Err(Failed::classify(result_error(&mut r)))
                };
                ffi::duckdb_destroy_result(&mut r);
                ffi::duckdb_destroy_prepare(&mut prep);
                out
            })();
            ffi::duckdb_destroy_extracted(&mut ex);
            out
        }
    }
}

unsafe fn result_error(r: &mut ffi::duckdb_result) -> String {
    let e = ffi::duckdb_result_error(r);
    if e.is_null() {
        "it failed".into()
    } else {
        CStr::from_ptr(e).to_string_lossy().into_owned()
    }
}

/// Why a helper statement was not made.
enum Failed {
    /// It reads something that is not there: its name, whether it is a table or view (rather
    /// than a function), and DuckDB's words.
    Missing {
        name: String,
        table: bool,
        said: String,
    },
    Other(String),
}

impl Failed {
    /// DuckDB 1.5.5's words for a name it cannot find: "Catalog Error: Table with name readings
    /// does not exist!", and the same with "Table Function", "Scalar Function" or "Macro
    /// Function" for a function.
    fn classify(msg: String) -> Failed {
        let first = msg.lines().next().unwrap_or("").to_string();
        if let Some(rest) = first.strip_prefix("Catalog Error: ") {
            if let Some((kind, after)) = rest.split_once(" with name ") {
                if let Some(name) = after.strip_suffix(" does not exist!") {
                    return Failed::Missing {
                        name: name.to_string(),
                        table: kind == "Table",
                        said: first.clone(),
                    };
                }
            }
        }
        Failed::Other(first)
    }
}

/// Why a helper that keeps an ENUM is refused. DuckDB holds an ENUM's values outside its memory
/// budget, so the bench keeps none (REQ-9 `the_agent_keeps_no_enum`), and a helper is held to the
/// agent's rule: no view with an ENUM column, nested or not, and no view or macro whose body
/// spells one (`an_object_that_keeps_an_enum_in_its_sql_is_refused`).
fn enum_kept(name: &str) -> String {
    format!(
        "helper `{name}` keeps an ENUM, which DuckDB holds outside its memory budget, so the \
         bench keeps none. Use VARCHAR."
    )
}

/// The SQL for an identifier, quoted.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn build_in(
    bench: &crate::Bench,
    defs: &[Def],
    tables: &[(String, Vec<(String, String)>)],
    memory_limit: &str,
) -> Result<Built, String> {
    let conn = bench.connection();
    // The stand-ins, in a catalog of the loader's memory that the file never holds.
    conn.execute_batch("ATTACH ':memory:' AS benchy_stubs")
        .map_err(|e| e.to_string())?;
    for (name, columns) in tables {
        let cols = columns
            .iter()
            .map(|(c, ty)| format!("{} {ty}", quoted(c)))
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute_batch(&format!(
            "CREATE TABLE benchy_stubs.main.{} ({cols})",
            quoted(name)
        ))
        .map_err(|e| format!("could not stand in for table `{name}`: {e}"))?;
    }
    // The engine's lock, so a helper can reach no more while it is made than it can once the
    // agent calls it (REQ-18 `a_helper_cannot_reach_outside_the_bench`).
    crate::lockdown::apply(conn, memory_limit).map_err(|e| e.to_string())?;
    let raw = Raw(bench.connect_raw().map_err(|e| e.to_string())?);
    // What an unqualified CREATE makes lands in the first catalog of the search path: the file.
    raw.exec("SET search_path = 'bench.main,benchy_stubs.main'")?;

    let mut built = Built::default();
    // A waiting helper, and what it waits for, by folded name.
    let mut waits: HashMap<String, String> = HashMap::new();
    let mut made: Vec<&Def> = Vec::new();
    for (i, d) in defs.iter().enumerate() {
        let folded = d.name.to_ascii_lowercase();
        if tables.iter().any(|(t, _)| t.eq_ignore_ascii_case(&d.name)) {
            built.broken.push(format!(
                "helper `{}` has the name of a table on the bench. Give it another name.",
                d.name
            ));
            continue;
        }
        let wait = match raw.create(&d.sql) {
            Ok(()) => {
                made.push(d);
                None
            }
            // A macro that reads a table, or a helper view, which is not there yet: it reads one,
            // which no helper macro may (`a_helper_macro_that_reads_a_table_is_refused_at_startup`),
            // so it does not wait as a view does.
            Err(Failed::Missing { name, table, .. }) if table && d.kind == Kind::Macro => {
                built
                    .broken
                    .push(reads_a_table(&d.name, &format!("its body reads `{name}`")));
                continue;
            }
            Err(Failed::Missing { name, table, said }) => {
                let folded_missing = name.to_ascii_lowercase();
                if let Some(w) = waits.get(&folded_missing) {
                    // It reads a helper that waits: it waits for the same table.
                    Some(w.clone())
                } else if let Some(later) = defs[i..]
                    .iter()
                    .find(|o| o.name.eq_ignore_ascii_case(&name))
                {
                    built.broken.push(format!(
                        "helper `{}` reads helper `{}`, which comes after it. Put `{}` first.",
                        d.name, later.name, later.name
                    ));
                    continue;
                } else if defs.iter().any(|o| o.name.eq_ignore_ascii_case(&name)) {
                    built.broken.push(format!(
                        "helper `{}` reads helper `{name}`, which could not be made.",
                        d.name
                    ));
                    continue;
                } else if table {
                    // A table the bench does not have yet: the helper waits for it.
                    Some(name)
                } else {
                    built.broken.push(format!("helper `{}`: {said}", d.name));
                    continue;
                }
            }
            Err(Failed::Other(why)) => {
                built.broken.push(format!("helper `{}`: {why}", d.name));
                continue;
            }
        };
        if let Some(w) = &wait {
            waits.insert(folded, w.clone());
        }
        built.shown.push(Shown {
            name: d.name.clone(),
            signature: d.name.clone(),
            what: match d.kind {
                Kind::View => "view".into(),
                Kind::Macro => "macro".into(),
            },
            comment: d
                .comment
                .clone()
                .map(|c| crate::result::cut(c, COMMENT_BYTES)),
            waits: wait,
        });
    }

    // What DuckDB made, read from its catalog: each helper made exactly what it said, and
    // nothing else was made anywhere.
    // With each view, the plan DuckDB binds for reading it, which names every table it scans:
    // the tables the helpers read (`Built::reads`). DuckDB 1.5.5 records no dependency of a view
    // or macro on a table (`duckdb_dependencies()` is empty for both, measured), and the plan is
    // the binder's own answer, other helpers' views expanded in it. It is the plan before the
    // optimizer, so a scan the optimizer would drop (`WHERE false`) is still there. And whether
    // its body spells an ENUM, as an agent's view is read (`crate::workspace::enum_spelled`).
    let views = raw.rows(concat!(
        "SELECT view_name, system.main.json_serialize_plan(system.main.concat( \
           'SELECT * FROM bench.main.\"', system.main.replace(view_name, '\"', '\"\"'), '\"')), \
         regexp_matches(sql, ",
        crate::workspace::enum_spelled!(),
        ")::VARCHAR FROM system.main.duckdb_views() WHERE database_name = 'bench' AND NOT internal",
    ))?;
    let mut named: HashSet<String> = HashSet::new();
    // Whether a plan could not be serialized, so its tables are not known.
    let mut unread = false;
    for v in &views {
        if let Some(f) = named_tables(&v[1], &mut named, &mut unread) {
            built.broken.push(by_name(&v[0], &f));
        }
        if v[2] == "true" {
            built.broken.push(enum_kept(&v[0]));
        }
    }
    // One listing of functions, DuckDB's built-ins with the helpers': `duckdb_functions()` builds
    // a row for each built-in before any filter applies, so each listing of it costs the same,
    // about 115 M instructions. So the built-ins are read after the helpers are made, not before:
    // a helper named like one has stood in for it in the helpers after it, but it stops startup
    // all the same.
    let mut functions = Vec::new();
    let mut strays = Vec::new();
    // Each helper macro's overloads, by folded name, with what DuckDB's parser made of each body.
    let mut bodies: Vec<(String, Result<Calls, String>)> = Vec::new();
    let mut builtin: HashSet<String> = raw
        .rows("SELECT view_name FROM system.main.duckdb_views() WHERE database_name = 'system'")?
        .into_iter()
        .map(|r| r[0].to_ascii_lowercase())
        .collect();
    // DuckDB's own functions alone, of the same listing, for what a helper macro calls
    // ([`unresolved`]): a system view's name is no function, and the agent may make a macro of it.
    let mut system_functions: HashSet<String> = HashSet::new();
    // With each helper macro's body as DuckDB's parser reads it, for whether it reads a table
    // ([`macro_body`]). A macro is bound only where it is called, with what it is called with,
    // and a call with NULL arguments is no stand-in for that: DuckDB 1.5.5's binder folds `NULL +
    // (SELECT max(t) FROM weather)` to a NULL constant, scan and all, measured. So the body is
    // read as parsed, which is what DuckDB expands into the caller's query. One more column of
    // the one listing, computed for the helpers' rows alone.
    for f in raw.rows(concat!(
        "SELECT database_name, function_name, function_type, \
         array_to_string(parameters, ', '), \
         regexp_matches(coalesce(macro_definition, ''), ",
        crate::workspace::enum_spelled!(),
        "), CASE WHEN database_name = 'bench' THEN system.main.json_serialize_sql( \
           CASE WHEN function_type = 'table_macro' THEN macro_definition \
           ELSE system.main.concat('SELECT ', macro_definition) END) END \
         FROM system.main.duckdb_functions()",
    ))? {
        if f[0] == "bench" {
            bodies.push((f[1].to_ascii_lowercase(), macro_body(&f[5])));
        }
        if f[0] == "system" {
            builtin.insert(f[1].to_ascii_lowercase());
            system_functions.insert(f[1].to_ascii_lowercase());
            continue;
        }
        if f[0] == "bench" && f[4] == "true" {
            built.broken.push(enum_kept(&f[1]));
        }
        if f[0] == "bench" {
            functions.push(vec![f[1].clone(), f[2].clone(), f[3].clone()]);
        } else if !strays.contains(&f[1]) {
            strays.push(f[1].clone());
            built.broken.push(format!(
                "the helpers made function `{}` in catalog `{}`: a helper is a view or a macro, \
                 made in the helpers' own catalog, and nothing else.",
                f[1], f[0]
            ));
        }
    }
    let reading = macros_that_read(&bodies);
    let calling = unresolved(&bodies, &system_functions);
    for d in defs {
        let folded = d.name.to_ascii_lowercase();
        if let Some(how) = reading.get(&folded) {
            built.broken.push(reads_a_table(&d.name, how));
        }
        if let Some(why) = calling.get(&folded) {
            built.broken.push(format!("helper `{}` {why}.", d.name));
        }
    }
    for d in defs {
        if builtin.contains(&d.name.to_ascii_lowercase()) {
            built.broken.push(format!(
                "helper `{}` is named like DuckDB's built-in `{}`, which it would stand in for \
                 in every query. Give it another name.",
                d.name, d.name
            ));
        }
    }
    // The names DuckDB's parser reads itself, which no catalog lists: a macro under one is made,
    // but a call by its plain name is DuckDB's own (`coalesce`, `try`, `percentile_cont`). The
    // set the agent is held to (REQ-9 `a_name_the_parser_reads_itself_is_taken`), read the same
    // way, and only when a helper is a macro: one parse of each of 337 names, about 50 M
    // instructions on DuckDB 1.5.5. A view is read in FROM, where the parser takes these words
    // as names (REQ-18 `a_helper_named_like_a_parser_keyword_is_refused_at_startup`).
    if defs.iter().any(|d| d.kind == Kind::Macro) {
        let sql = crate::workspace::PARSER_NAMES
            .to_str()
            .map_err(|e| e.to_string())?;
        let parser: HashSet<String> = raw
            .rows(sql)?
            .into_iter()
            .map(|r| r[0].to_ascii_lowercase())
            .collect();
        for d in defs.iter().filter(|d| d.kind == Kind::Macro) {
            let folded = d.name.to_ascii_lowercase();
            if parser.contains(&folded) && !builtin.contains(&folded) {
                built.broken.push(format!(
                    "helper `{}` is named like a word DuckDB's SQL parser reads itself, so a call \
                     of `{}(…)` would be DuckDB's own and never reach the helper. Give it another \
                     name.",
                    d.name, d.name
                ));
            }
        }
    }
    let others = raw.rows(
        "SELECT 'table ' || table_name, database_name FROM system.main.duckdb_tables() \
           WHERE database_name IN ('bench', 'temp') \
         UNION ALL SELECT 'view ' || view_name, database_name FROM system.main.duckdb_views() \
           WHERE database_name IN ('temp', 'benchy_stubs') AND NOT internal \
         UNION ALL SELECT 'sequence ' || sequence_name, database_name \
           FROM system.main.duckdb_sequences() WHERE database_name <> 'system' \
         UNION ALL SELECT 'index ' || index_name, database_name \
           FROM system.main.duckdb_indexes() WHERE database_name <> 'system' \
         UNION ALL SELECT 'type ' || type_name, database_name FROM system.main.duckdb_types() \
           WHERE database_name <> 'system' AND NOT internal \
         UNION ALL SELECT 'schema ' || schema_name, database_name \
           FROM system.main.duckdb_schemas() WHERE database_name <> 'system' AND NOT internal",
    )?;
    for o in &others {
        built.broken.push(format!(
            "the helpers made {} in catalog `{}`: a helper is a view or a macro, made in the \
             helpers' own catalog, and nothing else.",
            o[0], o[1]
        ));
    }
    let mut columns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in raw.rows(
        "SELECT table_name, column_name, data_type FROM system.main.duckdb_columns() \
         WHERE database_name = 'bench' ORDER BY table_name, column_index",
    )? {
        if c[2].contains("ENUM(") && !built.broken.iter().any(|b| b == &enum_kept(&c[0])) {
            built.broken.push(enum_kept(&c[0]));
        }
        columns
            .entry(c[0].to_ascii_lowercase())
            .or_default()
            .push(crate::profile::ident(&c[1]));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for v in &views {
        let folded = v[0].to_ascii_lowercase();
        seen.insert(folded.clone());
        match made.iter().find(|d| d.name.to_ascii_lowercase() == folded) {
            Some(d) if d.kind == Kind::View => {
                if let Some(s) = built.shown.iter_mut().find(|s| s.name == d.name) {
                    let cols = columns.get(&folded).cloned().unwrap_or_default();
                    s.signature = format!("{} ({})", d.name, cols.join(", "));
                }
            }
            _ => built.broken.push(format!(
                "the helpers made a view `{}` that no helper says it makes.",
                v[0]
            )),
        }
    }
    let mut overloads: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for f in &functions {
        overloads
            .entry(f[0].to_ascii_lowercase())
            .or_default()
            .push((f[1].clone(), f[2].clone()));
    }
    for (folded, each) in &overloads {
        seen.insert(folded.clone());
        match made.iter().find(|d| d.name.to_ascii_lowercase() == *folded) {
            Some(d) if d.kind == Kind::Macro => {
                if let Some(s) = built.shown.iter_mut().find(|s| s.name == d.name) {
                    s.signature = each
                        .iter()
                        .map(|(_, params)| format!("{}({params})", d.name))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    if each.iter().all(|(t, _)| t == "table_macro") {
                        s.what = "table macro".into();
                    }
                }
            }
            _ => built.broken.push(format!(
                "the helpers made a macro `{folded}` that no helper says it makes."
            )),
        }
    }
    for d in &made {
        if !seen.contains(&d.name.to_ascii_lowercase()) {
            built.broken.push(format!(
                "helper `{}` made no {} of that name.",
                d.name,
                match d.kind {
                    Kind::View => "view",
                    Kind::Macro => "macro",
                }
            ));
        }
    }
    // What the helpers read, of the tables on the bench, and what the waiting ones wait for: what
    // the views read, since a helper macro reads no table. Where DuckDB could not serialize a
    // plan, every table on the bench counts, which takes too many names from the agent rather
    // than too few.
    let mut reads: HashSet<String> = tables
        .iter()
        .map(|(t, _)| t.to_ascii_lowercase())
        .filter(|t| unread || named.contains(t))
        .collect();
    reads.extend(
        built
            .shown
            .iter()
            .filter_map(|s| s.waits.as_ref().map(|w| w.to_ascii_lowercase())),
    );
    built.reads = reads.into_iter().collect();
    built.reads.sort();
    Ok(built)
}

/// DuckDB 1.5.5's table functions that read a table of the bench named in a string, which no
/// parse names as a table: `query_table('readings')`, `query('SELECT … FROM readings')`,
/// `json_execute_serialized_sql(…)`, `duckdb_table_sample('readings')` and the `pragma_*` that
/// take a table. Found by listing `duckdb_functions()`'s table functions with a VARCHAR argument
/// and keeping those that read the bench; the rest read a file, which the engine's lock refuses
/// (`a_helper_cannot_reach_outside_the_bench`), or name a database or a setting. A view that
/// calls one is refused (`a_helper_that_names_a_table_in_a_string_is_refused_at_startup`): the
/// table it reads could not be taken from the agent. A helper macro calls no table function but
/// [`TABLE_FREE`] ones, so these are looked for in views alone.
const BY_NAME: &[&str] = &[
    "query",
    "query_table",
    "json_execute_serialized_sql",
    "duckdb_table_sample",
    "pragma_table_info",
    "pragma_storage_info",
    "pragma_show",
];

/// Why helper `name`, which calls `function`, is refused.
fn by_name(name: &str, function: &str) -> String {
    format!(
        "helper `{name}` calls `{function}`, which reads a table named in a string: DuckDB cannot \
         say which table that is, so it could not be kept from the agent. Name the table in FROM."
    )
}

/// Collect, from a view's bound plan as DuckDB serialized it (`json_serialize_plan`), the names
/// of the tables in it, ASCII case folded: `table` of a scan (and `table_name`, should a plan
/// carry one). A name that is no table on the bench is dropped by the caller. Sets `unread` when
/// DuckDB answered with an error instead.
///
/// Returns the first [`BY_NAME`] function it calls, if any: a `LOGICAL_GET` of the plan. In a
/// view's plan `query_table` and `query` are already bound to the scans they make, measured on
/// 1.5.5, so a view's tables are found through them and only the others are left to refuse.
fn named_tables(json: &str, out: &mut HashSet<String>, unread: &mut bool) -> Option<String> {
    fn walk(v: &serde_json::Value, out: &mut HashSet<String>, by: &mut Option<String>) {
        match v {
            serde_json::Value::Object(o) => {
                for key in ["table", "table_name"] {
                    if let Some(t) = o.get(key).and_then(|t| t.as_str()) {
                        out.insert(t.to_ascii_lowercase());
                    }
                }
                let called = match o.get("type").and_then(|t| t.as_str()) {
                    Some("LOGICAL_GET") => o["name"].as_str(),
                    _ => None,
                };
                if let Some(f) = called.map(str::to_ascii_lowercase) {
                    if by.is_none() && BY_NAME.contains(&f.as_str()) {
                        *by = Some(f);
                    }
                }
                o.values().for_each(|x| walk(x, out, by));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out, by)),
            _ => {}
        }
    }
    let mut by = None;
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(v) if v["error"] == serde_json::Value::Bool(false) => walk(&v, out, &mut by),
        _ => *unread = true,
    }
    by
}

/// The table functions a helper macro may call: they make rows from their arguments alone and
/// read no table. Every other table function is refused in a helper macro, the ones that read
/// the bench (`query_table`, `duckdb_tables`), a file (`read_text`) or something not yet known
/// alike, so a function DuckDB adds is refused until it is listed here. A table macro of the
/// operator's that reads no table may be called too ([`macros_that_read`]). A CTE of the caller's
/// cannot stand in for a table function: `WITH range AS (…)` left `range(3)` in a macro's body
/// as it was, measured on DuckDB 1.5.5 (`a_table_free_helper_macro_and_a_view_serve`).
const TABLE_FREE: &[&str] = &["range", "generate_series", "unnest"];

/// The kinds of FROM item, as DuckDB 1.5.5's parser serializes them, that read no table of
/// their own: no FROM, a subquery, a join, VALUES, a PIVOT or UNPIVOT, and a table function,
/// which [`TABLE_FREE`] judges. What each holds is read in turn. A `BASE_TABLE` names a table, a
/// view or a CTE, and a `SHOW_REF` is a DESCRIBE, SUMMARIZE or SHOW: those read a table, and so
/// does any kind not listed here, as far as benchy knows.
const READS_NONE: &[&str] = &[
    "EMPTY",
    "SUBQUERY",
    "JOIN",
    "EXPRESSION_LIST",
    "PIVOT",
    "TABLE_FUNCTION",
];

/// What a helper macro's body calls: each function, DuckDB's or a helper's, and each table
/// function, by folded name, a table function's name among the functions too.
#[derive(Debug, Default, PartialEq, Eq)]
struct Calls {
    functions: HashSet<String>,
    table_functions: HashSet<String>,
    /// Each call named by a schema or a catalog other than DuckDB's own, as written
    /// (`s.lower`), which [`unresolved`] refuses.
    elsewhere: BTreeSet<String>,
}

/// What a helper macro's body, as DuckDB's parser serialized it (`json_serialize_sql` of the body
/// DuckDB keeps, a scalar one after `SELECT `), calls, or `Err` with how it reads a table, REQ-18
/// `a_helper_macro_that_reads_a_table_is_refused_at_startup`.
///
/// This fails closed: a body DuckDB's parser could not serialize, or did as anything but one
/// SELECT, is taken to read a table, as is a FROM item of a kind not in [`READS_NONE`]. A FROM
/// item is found by its shape, whatever its kind: every table reference DuckDB 1.5.5 serializes
/// has a `type` and a `sample`, and neither an expression (which has a `class`) nor a query node
/// (which has `modifiers`) is one, measured on DuckDB 1.5.5 over every kind of FROM item, query
/// node and modifier, and some forty kinds of expression. A `BASE_TABLE` anywhere is refused even
/// so, and named, whatever its shape. A body's own CTE is a `BASE_TABLE`
/// where it is read, and refused like a table: telling the two apart takes DuckDB's scoping
/// rules (a CTE's own body, `WITH RECURSIVE`, the order of a WITH), which a check of benchy's
/// could get wrong in the direction that lets a table through, and a subquery in FROM does what
/// such a CTE does.
fn macro_body(json: &str) -> Result<Calls, String> {
    fn walk(v: &serde_json::Value, calls: &mut Calls) -> Result<(), String> {
        match v {
            serde_json::Value::Object(o) => {
                if let Some(ty) = o.get("type").and_then(|t| t.as_str()) {
                    let from_item = o.contains_key("sample")
                        && !o.contains_key("class")
                        && !o.contains_key("modifiers");
                    match ty {
                        "BASE_TABLE" => {
                            let t = o.get("table_name").and_then(|t| t.as_str()).unwrap_or("");
                            return Err(format!(
                                "its body reads `{t}`, a table, a view or a CTE of its own, which \
                                 benchy does not tell apart"
                            ));
                        }
                        _ if from_item && !READS_NONE.contains(&ty) => {
                            return Err(format!(
                                "its body reads from a `{ty}`, which benchy does not know to read \
                                 no table"
                            ))
                        }
                        _ => {}
                    }
                    let name = |f: &serde_json::Value| {
                        f["function_name"].as_str().map(str::to_ascii_lowercase)
                    };
                    if from_item && ty == "TABLE_FUNCTION" {
                        let f = name(&o["function"])
                            .ok_or("its body calls a table function benchy cannot name")?;
                        calls.table_functions.insert(f);
                    } else if o.contains_key("class") {
                        // Any expression that names a function: a call, a window or an
                        // aggregate, a table function's own among them.
                        if let Some(f) = name(v) {
                            let part = |k: &str| o.get(k).and_then(|p| p.as_str()).unwrap_or("");
                            let (catalog, schema) = (part("catalog"), part("schema"));
                            // DuckDB's catalog, or no catalog and at most `main`, which DuckDB's
                            // own rewrite writes (`[x]` is `main.list_value(x)`).
                            if !(catalog.eq_ignore_ascii_case("system")
                                || catalog.is_empty()
                                    && (schema.is_empty() || schema.eq_ignore_ascii_case("main")))
                            {
                                let at = [catalog, schema, f.as_str()]
                                    .iter()
                                    .filter(|p| !p.is_empty())
                                    .copied()
                                    .collect::<Vec<_>>()
                                    .join(".");
                                calls.elsewhere.insert(at);
                            }
                            calls.functions.insert(f);
                        }
                    }
                }
                o.values().try_for_each(|x| walk(x, calls))
            }
            serde_json::Value::Array(a) => a.iter().try_for_each(|x| walk(x, calls)),
            _ => Ok(()),
        }
    }
    let unread = || {
        "benchy could not read its body as DuckDB parses it, so cannot tell that it reads none"
            .to_string()
    };
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| unread())?;
    if v["error"] != serde_json::Value::Bool(false)
        || v["statements"].as_array().map(Vec::len) != Some(1)
    {
        return Err(unread());
    }
    let mut calls = Calls::default();
    walk(&v["statements"], &mut calls)?;
    Ok(calls)
}

/// Of the helper macros, by folded name with each overload's body ([`macro_body`]), the ones
/// that read a table, and how: a body that reads one, a table function that is neither
/// [`TABLE_FREE`] nor a helper macro, or a call of a helper macro that reads one, at any depth.
/// DuckDB expands a macro in the body of one that calls it, so the outer one reads what the
/// inner one does, where it is called.
fn macros_that_read(bodies: &[(String, Result<Calls, String>)]) -> HashMap<String, String> {
    let helpers: HashSet<&str> = bodies.iter().map(|(n, _)| n.as_str()).collect();
    let mut reading: HashMap<String, String> = HashMap::new();
    for (name, body) in bodies {
        let how = match body {
            Err(how) => Some(how.clone()),
            Ok(c) => c
                .table_functions
                .iter()
                .filter(|f| !TABLE_FREE.contains(&f.as_str()) && !helpers.contains(f.as_str()))
                .min()
                .map(|f| format!("it calls table function `{f}`")),
        };
        if let Some(how) = how {
            reading.entry(name.clone()).or_insert(how);
        }
    }
    loop {
        let mut more = Vec::new();
        for (name, body) in bodies {
            let Ok(c) = body else { continue };
            if reading.contains_key(name) {
                continue;
            }
            if let Some(h) = c
                .functions
                .iter()
                .chain(&c.table_functions)
                .filter(|f| reading.contains_key(*f))
                .min()
            {
                more.push((
                    name.clone(),
                    format!("it calls helper `{h}`, which reads one"),
                ));
            }
        }
        if more.is_empty() {
            return reading;
        }
        reading.extend(more);
    }
}

/// Of the helper macros, by folded name with each overload's body ([`macro_body`]), the ones that
/// call what may resolve to an object of the agent's, and why it is refused, REQ-18
/// `a_helper_macro_that_calls_an_unknown_function_is_refused_at_startup`: a function that is
/// neither one of DuckDB's `system` catalog (`functions`) nor a helper macro, or any call named
/// by a schema or catalog other than DuckDB's.
///
/// DuckDB 1.5.5 checks a macro's body for the functions it calls when it is made, but only in
/// part, and binds the rest where it is called, against the caller's search path, measured: a
/// call in the select list or WHERE of a SELECT with a FROM, a table macro's included, and one in
/// a lambda's body, were made with `nosuch` nowhere; `CASE`, an aggregate's argument, a FROM-less
/// subquery, a table function's arguments, VALUES and a parameter's default were refused. Made,
/// the helper called a macro the agent made afterwards under that name. So every name its body
/// calls must be DuckDB's or a helper's, which the agent may not give a macro of its own in
/// `main`, temporary or not (REQ-9 `an_agent_object_cannot_shadow_a_host_object_or_builtin`,
/// REQ-18 `an_agent_cannot_replace_or_shadow_a_helper`). A macro of the agent's in a schema of its
/// own may have any name, and `s.lower(x)` called one the agent made after the helper, as did
/// `x.lower()`, which DuckDB parses the same way and binds to a schema `x` when there is one. So
/// a call names no schema but `main`, which DuckDB's own rewrite writes (`[x]` is
/// `main.list_value(x)`), and no catalog but `system`.
///
/// The other way round, taking from the agent each name a helper calls that nothing defines,
/// would keep a helper that cannot work and a set of names `describe` never explains; refused
/// here, no such name is left. A parameter's default is not in the body DuckDB lists, and needs
/// no check: DuckDB binds it when the macro is made (`nosuch(1)`, and a lambda calling it, were
/// refused) and refuses a subquery in one.
///
/// By name, which the parse gives: an operator (`+`, `~~`) and a rewrite (`[1]`) are each a
/// function DuckDB lists. Swept over some two hundred expressions on 1.5.5, the one name a parse
/// gives that the listing lacks is `overlay`, for `OVERLAY(… PLACING …)`, which DuckDB does not
/// bind either. A body benchy could not read is refused already as reading a table
/// ([`macros_that_read`]).
fn unresolved(
    bodies: &[(String, Result<Calls, String>)],
    functions: &HashSet<String>,
) -> HashMap<String, String> {
    let helpers: HashSet<&str> = bodies.iter().map(|(n, _)| n.as_str()).collect();
    let mut out: HashMap<String, String> = HashMap::new();
    for (name, body) in bodies {
        let Ok(c) = body else { continue };
        let why = if let Some(at) = c.elsewhere.first() {
            format!(
                "calls `{at}`, named by a schema or a catalog, which DuckDB resolves where the \
                 helper is called: a schema the agent made of that name would hold what the \
                 helper calls, and `x.f()` is read so too. Call a function by its name alone, \
                 `f(x)`"
            )
        } else if let Some(f) = c
            .functions
            .iter()
            .filter(|f| !functions.contains(*f) && !helpers.contains(f.as_str()))
            .min()
        {
            format!(
                "calls `{f}`, which is neither DuckDB's nor a helper's. DuckDB binds a macro's \
                 body where it is called, so a macro the agent made named `{f}` would be what \
                 the helper calls. Call a function DuckDB has, or make `{f}` a helper"
            )
        } else {
            continue;
        };
        out.entry(name.clone()).or_insert(why);
    }
    out
}

/// Why helper macro `name`, which reads a table as `how` says, is refused.
fn reads_a_table(name: &str, how: &str) -> String {
    format!(
        "helper `{name}` reads a table: {how}. DuckDB expands a macro inside the query that calls \
         it, where a CTE of that query named like the table would stand in for it, so a helper \
         macro may read none. Write it as a view (`CREATE VIEW {name} AS SELECT …`), which DuckDB \
         binds on its own, for the agent to filter; a macro may compute from its arguments, with \
         `range`, `generate_series`, `unnest` or VALUES."
    )
}

/// Helper `s`'s line in `describe`, before it is cut at [`LINE_BYTES`].
fn line(s: &Shown) -> String {
    let mut line = format!("  {} — {}", s.signature, s.what);
    if let Some(c) = &s.comment {
        line.push_str(": ");
        line.push_str(c);
    }
    if let Some(w) = &s.waits {
        line.push_str(&format!(" (works once `{w}` is on the bench)"));
    }
    line
}

/// #60, REQ-18 `a_cut_helper_comment_or_line_is_told_to_the_operator`: for the operator's log,
/// each helper of `shown` whose comment (as `defs` holds it, whole) or whole line `describe` cuts,
/// and by how many bytes. `describe` marks a cut for the agent (`… [+51 bytes]`), but the agent
/// never sees what was cut, and the operator who wrote it would otherwise learn of it only from
/// the agent's answers. Told, not refused: a long comment reaches nothing it should not, and a
/// deployment that started before would stop on the upgrade.
pub fn cuts(defs: &[Def], shown: &[Shown]) -> Vec<String> {
    let mut out = Vec::new();
    for s in shown {
        let whole = defs
            .iter()
            .find(|d| d.name == s.name)
            .and_then(|d| d.comment.as_deref());
        if let Some(c) = whole.filter(|c| c.len() > COMMENT_BYTES) {
            let k = kept(c, COMMENT_BYTES);
            out.push(format!(
                "helper `{}`'s comment is {} bytes; `describe` shows the first {k} and marks the \
                 other {} cut. A comment is cut at {COMMENT_BYTES} bytes: shorten it to keep all \
                 of it.",
                s.name,
                c.len(),
                c.len() - k
            ));
        }
        out.extend(line_cut(s));
    }
    out
}

/// What [`cuts`] says of helper `s`'s whole line, if `describe` cuts it. Apart, for a waiting
/// view: its columns are in its line only once its table is attached and it is made.
pub fn line_cut(s: &Shown) -> Option<String> {
    let l = line(s);
    if l.len() <= LINE_BYTES {
        return None;
    }
    let k = kept(&l, LINE_BYTES);
    let parts = if s.waits.is_some() {
        "signature, kind, comment and what it waits for"
    } else {
        "signature, kind and comment"
    };
    Some(format!(
        "helper `{}`'s line in `describe`, its {parts}, is {} bytes; \
         `describe` shows the first {k} and marks the other {} cut. A line is cut at \
         {LINE_BYTES} bytes: shorten the comment, or the parameters' or columns' names.",
        s.name,
        l.len(),
        l.len() - k
    ))
}

/// How many of `s`'s bytes `crate::result::cut` keeps at `max`: up to the start of a character.
fn kept(s: &str, max: usize) -> usize {
    (0..=max.min(s.len()))
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

/// The helpers' part of `describe`: one line each, as many as fit in `room` bytes as the client
/// gets them, and a line saying how many did not.
pub fn render(shown: &[Shown], room: usize) -> String {
    if shown.is_empty() {
        return String::new();
    }
    let head = "\nHelpers the operator provides — use them like built-ins:\n";
    let mut out = head.to_string();
    let mut used = crate::result::wire_len(head);
    for (i, s) in shown.iter().enumerate() {
        let line = crate::result::cut(line(s), LINE_BYTES) + "\n";
        let left = shown.len() - i;
        let tail = format!("  … {left} more helper(s) not shown.\n");
        if used + crate::result::wire_len(&line) + crate::result::wire_len(&tail) > room {
            if i == 0 {
                return String::new();
            }
            out.push_str(&tail);
            return out;
        }
        used += crate::result::wire_len(&line);
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_sql_splits_where_duckdb_would() {
        let sql = "-- first one\n-- on two lines\nCREATE MACRO a(x) AS x || ';';\n\n\
                   /* not this */ CREATE VIEW b AS SELECT 'it''s; here' AS s, \"x;\" FROM t;\n\
                   -- far\n\nCREATE MACRO c() AS $$;$$;\n-- trailing comment only\n";
        let pieces = split(sql).unwrap();
        assert_eq!(pieces.len(), 3, "{pieces:?}");
        assert_eq!(pieces[0].comment.as_deref(), Some("first one on two lines"));
        assert_eq!(pieces[1].comment, None);
        assert_eq!(
            pieces[2].comment, None,
            "a blank line ends the comment block"
        );
        assert!(pieces[1].text.ends_with("FROM t"), "{}", pieces[1].text);
        assert!(split("CREATE MACRO a() AS 'open").is_err());
    }

    #[test]
    fn a_cut_is_told_from_the_first_byte_past_a_cap() {
        let shown = |name: &str, signature: &str, comment: &str| Shown {
            name: name.into(),
            signature: signature.into(),
            what: "macro".into(),
            comment: Some(crate::result::cut(comment.into(), COMMENT_BYTES)),
            waits: None,
        };
        let def = |name: &str, comment: &str| Def {
            name: name.into(),
            kind: Kind::Macro,
            sql: String::new(),
            comment: Some(comment.into()),
        };
        // A comment at the cap is whole; one byte more is cut, and so said.
        let at = "c".repeat(COMMENT_BYTES);
        let past = "c".repeat(COMMENT_BYTES + 1);
        assert!(cuts(&[def("f", &at)], &[shown("f", "f()", &at)]).is_empty());
        let said = cuts(&[def("f", &past)], &[shown("f", "f()", &past)]);
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("is 201 bytes; `describe` shows the first 200 and marks the other 1"),
            "{said:?}"
        );
        // A character across the cap is kept whole or not at all, as `describe` cuts it.
        let wide = format!("{}é", "c".repeat(COMMENT_BYTES - 1));
        let said = cuts(&[def("f", &wide)], &[shown("f", "f()", &wide)]);
        assert!(
            said[0].contains("shows the first 199 and marks the other 2"),
            "{said:?}"
        );
        // A line at the cap is whole; one byte more is cut, and so said.
        let mut s = shown("f", "", "");
        s.comment = None;
        s.signature = "c".repeat(LINE_BYTES - "  ".len() - " — macro".len());
        assert_eq!(line(&s).len(), LINE_BYTES);
        assert!(line_cut(&s).is_none());
        s.signature.push('c');
        assert!(line_cut(&s)
            .unwrap()
            .contains("is 321 bytes; `describe` shows the first 320 and marks the other 1"));
    }

    #[test]
    fn a_macro_body_benchy_cannot_read_reads_a_table() {
        // Fails closed (REQ-18 `a_helper_macro_that_reads_a_table_is_refused_at_startup`): what
        // DuckDB's parser did not serialize as exactly one statement is taken to read a table.
        let one = r#"{"error":false,"statements":[{"node":{"type":"SELECT_NODE","modifiers":[],
            "from_table":{"type":"EMPTY","alias":"","sample":null}}}]}"#;
        assert_eq!(macro_body(one), Ok(Calls::default()));
        for unread in [
            "",
            "not json",
            r#"{"error":true,"error_message":"Parser Error"}"#,
            r#"{"error":false,"statements":[]}"#,
            r#"{"statements":[{"node":{}}]}"#,
            one.replace("}]}", "},{\"node\":{}}]}").as_str(),
        ] {
            assert!(macro_body(unread).is_err(), "{unread}");
        }
        let unknown = one.replace("\"EMPTY\"", "\"SOME_NEW_REF\"");
        assert!(macro_body(&unknown).unwrap_err().contains("SOME_NEW_REF"));
    }

    #[test]
    fn a_helper_is_a_create_view_or_macro_with_a_plain_name() {
        let ok = parse(
            "CREATE VIEW daily AS SELECT 1; create function f(a) as a; CREATE MACRO g(x) AS x",
        )
        .unwrap();
        assert_eq!(
            ok.iter()
                .map(|d| (d.kind, d.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (Kind::View, "daily"),
                (Kind::Macro, "f"),
                (Kind::Macro, "g")
            ]
        );
        for bad in [
            "CREATE TABLE t AS SELECT 1",
            "CREATE OR REPLACE MACRO f() AS 1",
            "CREATE TEMP VIEW v AS SELECT 1",
            "CREATE VIEW main.v AS SELECT 1",
            "CREATE VIEW \"v\" AS SELECT 1",
            "CREATE MACRO IF NOT EXISTS f() AS 1",
            "SELECT 1",
            "ATTACH 'x' AS y",
            "CREATE MACRO benchy_x() AS 1",
            "CREATE MACRO f() AS 1; CREATE VIEW F AS SELECT 1",
            "",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
