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
//! A helper that reads a table the bench does not have yet **waits**: it is listed, its name is
//! taken, and calling it says what it waits for. The next attach of that table makes it.
//!
//! **Unqualified names.** A helper names the bench's tables unqualified, and that is safe: the
//! agent's search path resolves them at query time, a table's catalog is renamed by every attach
//! (`benchy_<n>`), so a qualified name could not stay right, and no object of the agent's may take
//! a table's name (REQ-9 `an_agent_object_cannot_shadow_a_host_object_or_builtin`), so the
//! unqualified name can only mean the table.

use duckdb::ffi;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
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
    /// The `--` lines directly above it, with no blank line between, joined into one line and cut
    /// at [`COMMENT_BYTES`].
    pub comment: Option<String>,
}

/// A helper as `describe` shows it: made, or waiting for a table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shown {
    pub name: String,
    /// `dewpoint(t, rh)`, `hours_above(series_id, …)`, or a view's name and columns.
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
    /// Of what the helpers read, what a helper **macro** reads, ASCII case folded, sorted: the
    /// bench's tables and the helper views its body names, and the table a waiting macro waits
    /// for. DuckDB expands a macro inside the caller's query, so a CTE of the caller's under one
    /// of these names is what the macro would read; the agent may not name a CTE so (REQ-18
    /// `a_cte_cannot_stand_in_for_a_table_a_helper_macro_reads`). A view is bound in its own
    /// context, so what only views read is not here.
    #[serde(default)]
    pub macro_reads: Vec<String>,
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
    (!joined.is_empty()).then(|| crate::result::cut(joined, COMMENT_BYTES))
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

    /// Whether a call of table macro `name` binds, its arguments all NULL: `Err` only for a name
    /// it reads that is not there, a table or another helper. DuckDB 1.5.5 binds a table
    /// macro's body when it is made, except where the body reads an argument, as the documented
    /// pattern does in a FROM-less CTE (`WITH arg AS (SELECT id AS id) … FROM arg, readings`):
    /// such a macro over a missing table was made, and failed when called with "Table with name
    /// readings does not exist!", measured. A call binds the whole body, and is prepared, never
    /// run. The arity is found by trying: DuckDB answers a call with the wrong number of
    /// arguments "Macro … does not support the supplied arguments". A scalar macro, which DuckDB
    /// binds when it is made, is no table function, which is no missing table either.
    fn call_binds(&self, name: &str) -> Result<(), Failed> {
        for n in 0..=64 {
            let args = vec!["NULL"; n].join(", ");
            let sql = format!("SELECT * FROM bench.main.{}({args})", quoted(name));
            let c = CString::new(sql).map_err(|_| Failed::Other("NUL byte".into()))?;
            let msg = unsafe {
                let mut prep: ffi::duckdb_prepared_statement = std::ptr::null_mut();
                let ok = ffi::duckdb_prepare(self.0, c.as_ptr(), &mut prep)
                    == ffi::duckdb_state_DuckDBSuccess;
                let msg = if ok {
                    None
                } else {
                    let e = ffi::duckdb_prepare_error(prep);
                    Some(if e.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    })
                };
                ffi::duckdb_destroy_prepare(&mut prep);
                msg
            };
            let Some(msg) = msg else { return Ok(()) };
            if msg.contains("does not support the supplied arguments") {
                continue;
            }
            return match Failed::classify(msg) {
                Failed::Missing { name: missing, .. } if missing.eq_ignore_ascii_case(name) => {
                    Ok(())
                }
                m @ Failed::Missing { .. } => Err(m),
                Failed::Other(_) => Ok(()),
            };
        }
        Ok(())
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
        let mut outcome = raw.create(&d.sql);
        if outcome.is_ok() && d.kind == Kind::Macro {
            // REQ-18 `a_table_macro_over_a_missing_table_waits_like_a_view`
            if let Err(missing) = raw.call_binds(&d.name) {
                let dropped = raw.exec(&format!("DROP MACRO TABLE bench.main.{}", quoted(&d.name)));
                outcome = match dropped {
                    Ok(()) => Err(missing),
                    Err(e) => Err(Failed::Other(format!("it could not be put aside: {e}"))),
                };
            }
        }
        let wait = match outcome {
            Ok(()) => {
                made.push(d);
                None
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
            comment: d.comment.clone(),
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
    // What the helper macros' bodies name, apart: a CTE of the caller's can stand in for it.
    let mut macro_named: HashSet<String> = HashSet::new();
    // Whether a plan or a body could not be serialized, so its tables are not known.
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
    let mut builtin: HashSet<String> = raw
        .rows("SELECT view_name FROM system.main.duckdb_views() WHERE database_name = 'system'")?
        .into_iter()
        .map(|r| r[0].to_ascii_lowercase())
        .collect();
    // With each helper macro's body as DuckDB's parser reads it, for the tables it names. A
    // macro is bound only where it is called, with what it is called with, and a call with NULL
    // arguments is no stand-in for that: DuckDB 1.5.5's binder folds `NULL + (SELECT max(t) FROM
    // weather)` to a NULL constant, scan and all, measured. So a macro's tables are the names its
    // body reads in FROM that are tables on the bench: a body was bound when it was made (or the
    // helper waits), so a table it reads is one of them, and a CTE of the body named otherwise
    // is not. One more column of the one listing, computed for the helpers' rows alone.
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
            if let Some(by) = named_tables(&f[5], &mut macro_named, &mut unread) {
                let why = by_name(&f[1], &by);
                if !built.broken.contains(&why) {
                    built.broken.push(why);
                }
            }
        }
        if f[0] == "system" {
            builtin.insert(f[1].to_ascii_lowercase());
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
    // What the helpers read, of the tables on the bench, and what the waiting ones wait for.
    // Where DuckDB could not serialize a plan or a body, every table on the bench counts, which
    // takes too many names from the agent rather than too few.
    named.extend(macro_named.iter().cloned());
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
    // What the macros read: the bench's tables and the helper views their bodies name, and
    // what a waiting macro waits for. Where a body could not be serialized, every table on the
    // bench and every helper view counts, as above.
    let views: Vec<String> = defs
        .iter()
        .filter(|d| d.kind == Kind::View)
        .map(|d| d.name.to_ascii_lowercase())
        .collect();
    let mut macro_reads: HashSet<String> = tables
        .iter()
        .map(|(t, _)| t.to_ascii_lowercase())
        .chain(views)
        .filter(|t| unread || macro_named.contains(t))
        .collect();
    macro_reads.extend(
        defs.iter()
            .filter(|d| d.kind == Kind::Macro)
            .filter_map(|d| {
                built
                    .shown
                    .iter()
                    .find(|s| s.name == d.name)
                    .and_then(|s| s.waits.as_ref().map(|w| w.to_ascii_lowercase()))
            }),
    );
    built.macro_reads = macro_reads.into_iter().collect();
    built.macro_reads.sort();
    Ok(built)
}

/// DuckDB 1.5.5's table functions that read a table of the bench named in a string, which no
/// parse names as a table: `query_table('readings')`, `query('SELECT … FROM readings')`,
/// `json_execute_serialized_sql(…)`, `duckdb_table_sample('readings')` and the `pragma_*` that
/// take a table. Found by listing `duckdb_functions()`'s table functions with a VARCHAR argument
/// and keeping those that read the bench; the rest read a file, which the engine's lock refuses
/// (`a_helper_cannot_reach_outside_the_bench`), or name a database or a setting. A helper that
/// calls one is refused (`a_helper_that_names_a_table_in_a_string_is_refused_at_startup`): the
/// table it reads could not be taken from the agent.
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

/// Collect, from JSON DuckDB serialized, the names of the tables in it: `table` of a scan in a
/// bound plan (`json_serialize_plan`), `table_name` of a `BASE_TABLE` in a parsed statement
/// (`json_serialize_sql`), ASCII case folded. A name that is no table on the bench is dropped by
/// the caller. Sets `unread` when DuckDB answered with an error instead.
///
/// Returns the first [`BY_NAME`] function it calls, if any: a `TABLE_FUNCTION` of a parse, or a
/// `LOGICAL_GET` of a plan. In a view's plan `query_table` and `query` are already bound to the
/// scans they make, measured on 1.5.5, so a view's tables are found through them and only the
/// others are left to refuse.
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
                    Some("TABLE_FUNCTION") => o["function"]["function_name"].as_str(),
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
        let mut line = format!("  {} — {}", s.signature, s.what);
        if let Some(c) = &s.comment {
            line.push_str(": ");
            line.push_str(c);
        }
        if let Some(w) = &s.waits {
            line.push_str(&format!(" (works once `{w}` is on the bench)"));
        }
        let line = crate::result::cut(line, 320) + "\n";
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
