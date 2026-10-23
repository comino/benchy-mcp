//! The bench itself: put data on it, work on it, take the result.
//!
//! **One connection serves the agent.** Classification (REQ-2) and execution share it, so the
//! guard always sees exactly what will run — including objects the agent created itself. A
//! second connection would classify against a different catalog and let a statement referencing
//! the agent's own temp object slip past unclassified.

use crate::guard::{Classifier, Verdict};
use crate::{
    lockdown,
    result::{Cell, Keep, QueryResult},
};
use duckdb::{ffi, Connection};
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

/// The storage version a loader's file is written in: v1.3.0, the first in which text is
/// compressed as DICT_FSST, chosen without sampling (REQ-15
/// `a_files_layout_is_the_same_on_every_load`). DuckDB 1.5.5's default, v1.0.0, chooses between
/// dictionary and FSST by estimating FSST's size from a random quarter of a segment's vectors
/// (src/storage/compression/fsst.cpp: `ANALYSIS_SAMPLE_SIZE`, a `RandomEngine` seeded from
/// `getrandom`, which no setting reaches), so at a near tie the same rows were compressed one way
/// or the other from one load to the next: 1 load in 30 of a 300,000-row column of seven short
/// values. FSST stays on: DICT_FSST is dictionary and FSST together. The engine, the same
/// DuckDB, reads it.
const STORAGE_VERSION: &std::ffi::CStr = c"v1.3.0";

/// A bench being set up. The engine is **not** sealed yet, so the host can still read files.
/// It has no `work` method: there is no way to run agent SQL against an unsealed engine.
///
/// Loading writes into a private file. [`Bench::seal`] then attaches that file `READ_ONLY` into
/// a fresh in-memory database, which is how REQ-9's `loaded_is_readonly` is *enforced by DuckDB*
/// rather than by inspecting statement text. Name matching was rejected: a computed name reaches
/// an object that a text check cannot see.
pub struct Bench {
    db: ffi::duckdb_database,
    ingest: Connection,
    loaded: Vec<LoadedTable>,
    dir: PathBuf,
    /// What the operator stated about each table's shape, by name (REQ-14).
    shapes: std::collections::BTreeMap<String, crate::profile::Shape>,
}

/// A sealed bench. The only thing that can run agent SQL, and it can only be reached through
/// [`Bench::seal`] — so "load, then drop the filesystem" is the only expressible order.
pub struct Workspace {
    db: ffi::duckdb_database,
    /// Raw, agent-facing: classify and execute.
    agent: ffi::duckdb_connection,
    /// Safe, host-facing. Retained so the database outlives us; agent SQL never reaches it.
    #[allow(dead_code)]
    ingest: Connection,
    classifier: Classifier,
    /// REQ-2: the table functions the agent may not call (`crate::guard::Calls`).
    calls: crate::guard::Calls,
    next_result_id: u64,
    /// What every result id starts with, before its number: `r`, or a session's own prefix
    /// (`set_result_prefix`).
    result_prefix: String,
    /// REQ-10: the most recent results, kept so `get_result` can page through one without running
    /// the query again. They die with this workspace, which is what makes a stale id fail rather
    /// than resolve to something else.
    retained: std::collections::VecDeque<QueryResult>,
    loaded: Vec<LoadedTable>,
    dir: PathBuf,
    /// Tables attached after the seal: the name the agent types, and the read-only catalog it
    /// lives in. A replaced table gets a new catalog, so an old one is never reused.
    attached: Vec<(String, String)>,
    next_alias: u64,
    /// Tables the session was to have and could not load, for the hint that says so.
    unavailable: Vec<String>,
    /// What the agent may not name an object of its own (REQ-9
    /// `an_agent_object_cannot_shadow_a_host_object_or_builtin`), besides the tables on the bench.
    taken: Taken,
    /// The oids of the agent's tables, views and types as last listed. A CREATE that leaves an
    /// oid not in here made a table, view or type, so it made no function and the costly listing
    /// of functions is not needed (`Workspace::inspect`). A DROP is not listed after, so this may
    /// hold oids that are gone: that only ever sends a CREATE to the costly listing, never past
    /// it. DuckDB never hands out an oid twice.
    seen: std::collections::HashSet<i64>,
    /// REQ-18: the operator's helpers, made or waiting, as `describe` lists them.
    helpers: Vec<crate::helpers::Shown>,
    /// The read-only catalog the helpers that are made live in, on the agent's search path
    /// between its own schema and the tables.
    helper_alias: Option<String>,
    /// Why this engine must not go on, once a batch has left it holding what it cannot let go:
    /// a dynamic PIVOT's type that could not be dropped (`Workspace::drop_pivot_types`).
    spoiled: Option<String>,
}

/// Names that are taken in the function namespace, compared as DuckDB compares identifiers:
/// ASCII case folded, nothing else (`columns_duckdb_tells_apart_attach`).
///
/// Tables and views are taken by the tables on the bench, which the workspace already keeps, and
/// by the sets below. Types need no set: DuckDB refuses `CREATE TYPE` under a built-in type's
/// name itself ("Type with name "integer" already exists!", measured on 1.5.5, temporary or not,
/// aliases like `int4` and `string` included).
#[derive(Default)]
struct Taken {
    /// DuckDB's built-in functions, macros and table functions, every schema of the `system`
    /// catalog, and apart from them the names its parser reads itself ([`PARSER_NAMES`]). Read
    /// on the first CREATE that may have made a function, and kept: no extension loads after the
    /// lock, so neither can grow.
    builtin: Option<Builtin>,
    /// The views of DuckDB's `system` catalog, in every schema of it ([`SYSTEM_VIEWS`]), which an
    /// unqualified name reaches after the agent's own: `sqlite_master`, `duckdb_tables`,
    /// `pg_class`. Read on the first CREATE or ALTER, and kept, for the same reason.
    system_views: Option<std::collections::HashSet<String>>,
    /// The operator's helpers (REQ-18), made or waiting: taken in every namespace, so no object of
    /// the agent's and no table attached later is named like one.
    helpers: std::collections::HashSet<String>,
    /// The tables the operator's helpers read (REQ-18
    /// `a_table_a_helper_reads_stays_taken_after_a_detach`), on the bench or not: a helper names
    /// them unqualified, so an object of the agent's under one of these names would be what the
    /// helper reads once the host's table is away. Not a host's to avoid: attaching one is how
    /// the helper gets its table.
    reads: std::collections::HashSet<String>,
}

/// What DuckDB itself names in the function namespace (`Taken::builtin`).
struct Builtin {
    functions: std::collections::HashSet<String>,
    parser: std::collections::HashSet<String>,
}

/// Whether a type of the temporary catalog named `name`, made in the batch `sql`, is a dynamic
/// PIVOT's. DuckDB 1.5.5 names one `__pivot_enum_` and a random UUID, lower-case, in a CREATE
/// TYPE it adds to the batch; the C API gives no statement's text or origin, so the name is what
/// tells. A type the agent makes is named in its SQL, and DuckDB has no other spelling of an
/// identifier for it to hide behind (`U&"…"` is "NOT IMPLEMENTED"); a pivot's name cannot be in
/// the SQL, which was written before the UUID was drawn
/// (`a_type_the_agent_names_is_never_a_pivots`).
fn pivots_type(name: &str, sql: &str) -> bool {
    let Some(id) = name.strip_prefix("__pivot_enum_") else {
        return false;
    };
    let uuid = id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        });
    uuid && !sql.to_ascii_lowercase().contains(name)
}

/// Why a statement that keeps an ENUM, `what`, is undone (`Workspace::inspect`).
fn enum_refused(what: &str) -> String {
    format!(
        "ENUM types are not kept here: this statement would keep {what}. Nothing it did stays. \
         DuckDB holds an ENUM's values outside its memory budget, so the bench keeps no type of \
         yours and no table or view with an ENUM column: use VARCHAR, casting with ::VARCHAR. An \
         ENUM inside a query that keeps nothing is fine."
    )
}

/// Where a batch is in the transaction DuckDB 1.5.5 wraps around a statement that keeps a dynamic
/// PIVOT: a TRANSACTION, the pivot's CREATE TYPE for each pivot, the statement, a TRANSACTION.
/// Measured, it wraps every statement that is not a SELECT: `CREATE TABLE … AS PIVOT`, and an
/// INSERT, UPDATE, DELETE or MERGE that reads one. Each holds the agent's statement number the
/// first TRANSACTION stands at (`a_pivot_into_a_table_runs`).
///
/// The C API gives a statement's type and not its text, so a TRANSACTION of DuckDB's is told
/// from the agent's by what follows it, which the agent cannot forge: a type only a dynamic
/// PIVOT makes (`pivots_type`), then a statement that is not a SELECT. Neither TRANSACTION runs.
/// A batch that only looks like one is refused as the agent's TRANSACTION where the shape breaks;
/// what ran inside it before then was a pivot's CREATE TYPE or an allowed statement, each as it
/// would run alone, and a CREATE of the agent's own after its TRANSACTION is undone. A
/// TRANSACTION that ends the batch, or is never closed, is refused up front (`crate::guard`).
#[derive(Debug, Clone, Copy)]
enum Wrap {
    None,
    /// A TRANSACTION, not run: a CREATE must follow, and make a pivot's type.
    Opened(u64),
    /// A pivot's type made; another may follow, then the statement that is not a SELECT.
    Typed(u64),
    /// The statement ran; a TRANSACTION must follow, which closes it.
    Ran(u64),
}

impl Wrap {
    /// The agent's own TRANSACTION, at statement `at` counted from 0, refused.
    fn refused(at: u64) -> WorkError {
        WorkError::Denied {
            index: at as usize + 1,
            kind: "TRANSACTION",
            allowed: ALLOWED,
        }
    }
}

/// What a CREATE or ALTER left in the agent's catalogs (`Workspace::inspect`).
#[derive(Debug, PartialEq, Eq)]
enum Made {
    /// The agent's own objects, every name free.
    Own,
    /// DuckDB's CREATE TYPE for a dynamic PIVOT: a type in the temporary catalog named
    /// `__pivot_enum_` and a UUID the batch does not hold. A type the agent names so is its own
    /// (`a_type_the_agent_names_is_never_a_pivots`).
    PivotType,
    /// An object under a taken name, and why it is taken: undone, and the batch stops.
    Taken(String),
}

/// The agent's tables and views where an unqualified name finds them: the `main` schema of its
/// temporary catalog and of `memory`, each with its kind. A table in a schema of its own hides
/// nothing.
///
/// Every catalog read benchy makes names `system.main`. An unqualified `duckdb_tables()` resolved
/// to an agent's table macro of that name, measured on DuckDB 1.5.5, on the host-facing
/// connection too (`benchys_catalog_reads_cannot_be_shadowed`), and so would an unqualified
/// scalar or aggregate: a macro `count()` broke `count(DISTINCT …)`. So these use none.
const AGENT_RELATIONS: &CStr = c"SELECT table_oid, table_name, 'table' \
      FROM system.main.duckdb_tables() \
      WHERE database_name IN ('temp', 'memory') AND schema_name = 'main' \
      UNION ALL SELECT view_oid, view_name, 'view' FROM system.main.duckdb_views() \
      WHERE database_name IN ('temp', 'memory') AND schema_name = 'main' AND NOT internal";
/// The agent's types, where DuckDB also puts a dynamic PIVOT's. Not internal: DuckDB lists every
/// built-in type again under each catalog, with no oid.
///
/// Every schema, not only `main`: a type is refused wherever the agent makes it
/// (`the_agent_keeps_no_enum`).
const AGENT_TYPES: &CStr = c"SELECT type_oid, type_name, database_name \
      FROM system.main.duckdb_types() \
      WHERE database_name IN ('temp', 'memory') AND NOT internal";
/// The SQL literal of a pattern that finds an ENUM spelled in the SQL DuckDB keeps of an object,
/// for `regexp_matches`: an ENUM written out, which DuckDB 1.5.5 keeps as `"ENUM"('a', 'b')`
/// however it was written (`ENUM('a', 'b')` in a column's type), or a type named as a dynamic
/// PIVOT's, which it keeps quoted, `"__pivot_enum_0"`, however it was written: so the name in a
/// string, `'__pivot_enum_x'`, is not one. A type of any other name is the agent's own, refused where it is made, so a dynamic
/// PIVOT's is the only type an object of the agent's could name (`Workspace::inspect`). The
/// helpers are held to the same pattern (`crate::helpers`).
///
/// Read from that SQL because DuckDB records no more: on 1.5.5 `duckdb_dependencies()` lists
/// nothing of the temporary catalog, and a view's or macro's body is kept unbound, depending on
/// nothing; only a table whose DEFAULT, generated column or CHECK casts to a type depends on it,
/// which is what stops a DROP TYPE, and its SQL in `duckdb_tables()` spells all three.
macro_rules! enum_spelled {
    () => {
        r#"'ENUM"?\(|"(?i:__pivot_enum_)'"#
    };
}
pub(crate) use enum_spelled;

/// A C string from a `&str` that ends in its NUL, at compile time: for the listings built with
/// `concat!`, which `c"…"` cannot be.
const fn c_str(s: &'static str) -> &'static CStr {
    match CStr::from_bytes_with_nul(s.as_bytes()) {
        Ok(c) => c,
        Err(_) => panic!("not a C string"),
    }
}

/// A column of the agent's, in any of its tables or views and any schema, whose type holds an
/// ENUM, said as `enum_refused` says it: DuckDB spells a nested one inside the outer type,
/// `STRUCT(z ENUM('c'))[]`. Asked after every CREATE and ALTER.
const AGENT_ENUM_COLUMNS: &CStr = c"SELECT 0, 'the column ' || table_name || '.' || column_name \
      || ' of type ' || data_type, '' FROM system.main.duckdb_columns() \
      WHERE database_name IN ('temp', 'memory') AND data_type LIKE '%ENUM(%' LIMIT 1";
/// A table of the agent's, in any schema, whose SQL spells an ENUM ([`enum_spelled`]): in a
/// column's DEFAULT, a generated column or a CHECK, which DuckDB keeps bound
/// (`an_object_that_keeps_an_enum_in_its_sql_is_refused`).
const ENUM_IN_TABLES: &CStr = c_str(concat!(
    "SELECT 0, 'the table ' || table_name, '' FROM system.main.duckdb_tables() \
     WHERE database_name IN ('temp', 'memory') AND regexp_matches(sql, ",
    enum_spelled!(),
    ") LIMIT 1\0"
));
/// The same of a view's body. `duckdb_views()` writes out the SQL of each of DuckDB's own 48
/// views before any filter applies, about 2 ms here against 0.1 ms without it (DuckDB 1.5.5),
/// so it is asked only when a view may have been made.
const ENUM_IN_VIEWS: &CStr = c_str(concat!(
    "SELECT 0, 'the view ' || view_name, '' FROM system.main.duckdb_views() \
     WHERE database_name IN ('temp', 'memory') AND NOT internal AND regexp_matches(sql, ",
    enum_spelled!(),
    ") LIMIT 1\0"
));
/// The same of an index's expressions.
const ENUM_IN_INDEXES: &CStr = c_str(concat!(
    "SELECT 0, 'the index ' || index_name, '' FROM system.main.duckdb_indexes() \
     WHERE database_name IN ('temp', 'memory') AND regexp_matches(coalesce(sql, ''), ",
    enum_spelled!(),
    ") LIMIT 1\0"
));
/// The types DuckDB made for the dynamic PIVOTs of a batch, and any the agent named so, dropped
/// when the batch is over (`a_pivot_keeps_no_enum_after_its_batch`).
const PIVOT_TYPES: &CStr = c"SELECT type_oid, type_name, database_name \
      FROM system.main.duckdb_types() \
      WHERE database_name = 'temp' AND NOT internal AND starts_with(type_name, '__pivot_enum_')";
/// The agent's functions, in any schema: whether each is where an unqualified name finds it
/// (`1` in its `main` schema), its name, and whether its body spells an ENUM (`true`,
/// [`enum_spelled`]). `duckdb_functions()` builds a row for each of DuckDB's built-ins before
/// any filter applies, which is why it is asked only when a CREATE made no table, view or type.
const AGENT_FUNCTIONS: &CStr = c_str(concat!(
    "SELECT (schema_name = 'main')::INTEGER, function_name, \
       bool_or(regexp_matches(coalesce(macro_definition, ''), ",
    enum_spelled!(),
    "))::VARCHAR FROM system.main.duckdb_functions() \
     WHERE database_name IN ('temp', 'memory') GROUP BY ALL\0"
));
const BUILTIN_FUNCTIONS: &CStr = c"SELECT DISTINCT function_name \
      FROM system.main.duckdb_functions() WHERE database_name = 'system'";
/// DuckDB's system views, of every schema of the `system` catalog (`main`, `information_schema`,
/// `pg_catalog`): 47 on DuckDB 1.5.5. An agent's `CREATE VIEW sqlite_master` answered for the
/// system view of that name, measured (REQ-9 `a_system_view_name_is_taken`). Schemas need no
/// check: DuckDB refuses a schema of the agent's named `information_schema` or `pg_catalog`, and
/// a view in the system catalog, itself. Table functions need none either: `FROM range` names a
/// table, never the function, so a table named `range` hides nothing.
const SYSTEM_VIEWS: &CStr =
    c"SELECT DISTINCT view_name FROM system.main.duckdb_views() WHERE database_name = 'system'";
/// The names DuckDB's parser reads itself, which no catalog lists (REQ-9
/// `a_name_the_parser_reads_itself_is_taken`), found by DuckDB's own parser rather than kept by
/// hand, as `spike/probe-dialect.py` found them for docs/DIALECT.md:
///
/// - every keyword but the unreserved ones: the grammar does not take them as a function's name,
///   or makes something else of a call (`coalesce`, `try_cast`, `extract`, `columns`);
/// - every unreserved keyword, and the names DuckDB's `transform_function.cpp` rewrites that are
///   no keyword (`try`, `percentile_cont`, `percentile_disc`, `ifnull`, `construct_array`, `date`,
///   `__internal_position_operator`, which DuckDB lists nowhere), unless a call of it with one
///   argument parses to a call of that name. Each rewrite there either takes one argument
///   (`try(x)` is a TRY, `date(x)` a cast) or refuses a call with one ("Wrong number of
///   arguments to IF."), so the one call tells them all; a call with one, two and three
///   arguments told the same, at twice the cost;
/// - and those of the rewritten names whose `WITHIN GROUP` call parses to another function's
///   (`percentile_cont` to `quantile_cont`). Only they are asked: any other name's is an error.
///
/// A call the parser leaves alone serializes with `"function_name":"<name>"`, and one it rewrites
/// does not. Every function is named through `system.main`, since this runs inside the agent's
/// transaction, after its CREATE (`benchys_catalog_reads_cannot_be_shadowed`). Measured on DuckDB
/// 1.5.5: 168 names, 158 of them not in `duckdb_functions()`; one parse for each of 337 names,
/// once in an engine's life (docs/history/PERFORMANCE-rounds.md, "The query path: no-shadowing").
pub(crate) const PARSER_NAMES: &CStr =
    c"WITH rewritten(k) AS (VALUES ('try'), ('percentile_cont'), \
        ('percentile_disc'), ('ifnull'), ('construct_array'), ('date'), \
        ('__internal_position_operator')), \
      free(k) AS (SELECT keyword_name FROM system.main.duckdb_keywords() \
        WHERE keyword_category = 'unreserved' UNION ALL SELECT k FROM rewritten) \
      SELECT keyword_name FROM system.main.duckdb_keywords() \
        WHERE keyword_category <> 'unreserved' \
      UNION SELECT k FROM (SELECT k, system.main.json_serialize_sql(system.main.concat( \
          'SELECT ', k, '(1)')) AS j FROM free) \
        WHERE NOT system.main.starts_with(j, '{\"error\":false') \
          OR NOT system.main.contains(j, system.main.concat('\"function_name\":\"', k, '\"')) \
      UNION SELECT k FROM (SELECT k, system.main.json_serialize_sql(system.main.concat( \
          'SELECT ', k, '(0.5) WITHIN GROUP (ORDER BY 1)')) AS j FROM rewritten) \
        WHERE system.main.starts_with(j, '{\"error\":false') \
          AND NOT system.main.contains(j, system.main.concat('\"function_name\":\"', k, '\"'))";

/// What ended up on the bench, and where it came from. The `source` is operator-facing only —
/// REQ-4: the catalogue the agent sees never carries a path.
#[derive(Debug, Clone)]
pub struct LoadedTable {
    pub name: String,
    pub rows: u64,
    pub columns: Vec<(String, String)>,
    pub source: String,
    /// REQ-14: the table's shape, found when it was loaded and kept in its comment. `None` for a
    /// table loaded without one.
    pub profile: Option<crate::profile::Profile>,
    /// What a stream showed of itself as it went into the table, for the profile, so the table
    /// is not read back for it (REQ-14). Taken when the table is recorded.
    pub seen: Option<std::sync::Arc<crate::profile::Seen>>,
}

#[derive(Debug)]
pub enum WorkError {
    /// REQ-2: refused before execution.
    Denied {
        index: usize,
        kind: &'static str,
        allowed: &'static str,
    },
    /// The batch did not parse.
    Parse(String, Option<String>),
    /// DuckDB refused or failed the statement.
    Sql {
        index: usize,
        message: String,
        /// REQ-11: what to do instead. `None` when we have nothing better to say than DuckDB
        /// already did — padding an error we cannot improve is worse than passing it through.
        hint: Option<String>,
    },
    Open(String),
    /// REQ-9 `an_attached_name_is_taken_too`: a table may not be attached under a name the agent
    /// already gave an object of its own.
    Taken(String),
    /// REQ-2 `a_table_function_that_changes_the_engine_is_refused`: the batch calls a table
    /// function, or names a system view, the agent may not use. Nothing ran.
    Refused(String),
    /// REQ-7: the batch outlived its deadline and was interrupted. The workspace survives.
    Deadline {
        seconds: f32,
    },
    /// REQ-10 `a_failed_batch_returns_what_ran_before_it`: statements ran, and kept what they
    /// did, before `error` stopped the batch. Their results go back with the error.
    Partial {
        ran: Vec<QueryResult>,
        error: Box<WorkError>,
    },
}

impl WorkError {
    /// What stopped the batch, whether or not statements ran before it.
    pub fn cause(&self) -> &WorkError {
        match self {
            WorkError::Partial { error, .. } => error.cause(),
            other => other,
        }
    }
}

impl std::fmt::Display for WorkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkError::Denied { index, kind, .. } if *kind == TIME_ZONE => write!(
                f,
                "statement {index} sets TimeZone, which is fixed; nothing after it ran. {}",
                crate::hint::UTC
            ),
            WorkError::Denied {
                index,
                kind,
                allowed,
            } => {
                write!(
                f, "statement {index} is {kind}, which is not allowed here; nothing after it ran. \
                    Allowed: {allowed}. The data is already loaded as tables — file and network \
                    functions are unavailable.")?;
                // DuckDB's own transaction around a PIVOT into a table, over a table the same
                // batch makes, is refused up front (`crate::guard`, `a_pivot_into_a_table_runs`).
                if *kind == "TRANSACTION" {
                    write!(
                        f,
                        " Transactions are not available. If statement {index} keeps a \
                        PIVOT (CREATE TABLE … AS PIVOT, or an INSERT, UPDATE, DELETE or MERGE \
                        that reads one) over a table made earlier in the same call, make that \
                        table in a call of its own first."
                    )?;
                }
                Ok(())
            }
            WorkError::Parse(m, hint) => {
                write!(f, "could not parse: {m}")?;
                if let Some(h) = hint {
                    write!(f, " — {h}")?;
                }
                Ok(())
            }
            WorkError::Sql {
                index,
                message,
                hint,
            } => {
                write!(f, "statement {index}: {message}")?;
                if let Some(h) = hint {
                    write!(f, " — {h}")?;
                }
                Ok(())
            }
            WorkError::Open(m) => write!(f, "could not open the workspace: {m}"),
            WorkError::Taken(m) | WorkError::Refused(m) => write!(f, "{m}"),
            WorkError::Partial { ran, error } => {
                let n = ran.len();
                let (which, they, results) = if n == 1 {
                    ("Statement 1 ran".to_string(), "it", "Its result follows")
                } else {
                    (
                        format!("Statements 1–{n} ran"),
                        "they",
                        "Their results follow",
                    )
                };
                write!(
                    f,
                    "{error}\n{which} before it, and what {they} did is kept. {results}."
                )
            }
            WorkError::Deadline { seconds } => {
                let shown = if *seconds >= 1.0 {
                    format!("{seconds:.0}s")
                } else {
                    format!("{:.0}ms", seconds * 1000.0)
                };
                write!(
                    f,
                    "the batch passed its {shown} deadline and was stopped. Your tables are \
                     intact. Narrow it: filter on a time range, aggregate before joining, or \
                     select fewer columns."
                )
            }
        }
    }
}

/// One place decides whether a failure was the deadline or the statement's own fault. The
/// watchdog's verdict is authoritative: DuckDB reports an interrupt as an ordinary error, so
/// without it every timeout would surface as `INTERRUPT Error: Interrupted!` — true, and useless
/// to an agent that needs to know its tables survived and what to do differently.
///
/// `index` counts the agent's statements from 0, and `sql` is the batch, so the hint can read the
/// statement that failed.
fn fault(
    ws: &Workspace,
    timed_out: bool,
    cap: &crate::result::Caps,
    index: u64,
    msg: &str,
    sql: &str,
) -> WorkError {
    if timed_out {
        WorkError::Deadline {
            seconds: cap.deadline.as_secs_f32(),
        }
    } else {
        let raw: String = msg.lines().next().unwrap_or("").into();
        let names = ws.nameable();
        let statement = crate::hint::statement(sql, index as usize);
        let cx = crate::hint::Context {
            statement: statement.as_deref(),
            unavailable: &ws.unavailable,
        };
        if let Some((message, hint)) = ws.helper_written(&raw, statement.as_deref()) {
            return WorkError::Sql {
                index: index as usize + 1,
                message,
                hint: Some(hint),
            };
        }
        let columns = |tables: &[&str]| ws.columns_of(tables);
        let catalogue = crate::hint::Catalogue {
            names: &names,
            columns: Some(&columns),
        };
        WorkError::Sql {
            index: index as usize + 1,
            hint: ws
                .waiting_helper(&raw)
                .or_else(|| crate::hint::for_sql_error(&raw, &catalogue, &cx)),
            message: crate::hint::scrub(&raw),
        }
    }
}

/// A handle that can interrupt a running query from another thread, and do nothing else.
///
/// SAFETY: `duckdb_interrupt` is the one C API call explicitly intended to be made from a
/// different thread *while* a query runs on that connection. This handle exposes only that call:
/// it cannot read a result, execute, or free anything, so the exclusive-ownership invariant that
/// makes [`Workspace`] `Send` is not weakened by handing one to a watchdog.
#[derive(Clone, Copy)]
struct Interrupt(ffi::duckdb_connection);
unsafe impl Send for Interrupt {}
impl Interrupt {
    fn fire(self) {
        unsafe { ffi::duckdb_interrupt(self.0) }
    }
}

/// `search_path` is per-connection, unlike `lock_configuration` which is database-wide. The
/// agent connection must be told separately, and before lockdown, or it cannot see the attached
/// tables at all.
unsafe fn raw_exec(con: ffi::duckdb_connection, sql: &str) -> Result<(), String> {
    let c = CString::new(sql).map_err(|_| "NUL byte".to_string())?;
    let mut r: ffi::duckdb_result = std::mem::zeroed();
    let ok = ffi::duckdb_query(con, c.as_ptr(), &mut r) == ffi::duckdb_state_DuckDBSuccess;
    let out = if ok {
        Ok(())
    } else {
        let e = ffi::duckdb_result_error(&mut r);
        Err(if e.is_null() {
            "error".into()
        } else {
            CStr::from_ptr(e).to_string_lossy().into_owned()
        })
    };
    ffi::duckdb_destroy_result(&mut r);
    out
}

/// The names of the columns `sql` would return, from preparing it on `con` without running it;
/// `None` if it does not prepare.
unsafe fn bound_columns(con: ffi::duckdb_connection, sql: &str) -> Option<Vec<String>> {
    let c = CString::new(sql).ok()?;
    let mut prep: ffi::duckdb_prepared_statement = std::ptr::null_mut();
    let prepared =
        ffi::duckdb_prepare(con, c.as_ptr(), &mut prep) == ffi::duckdb_state_DuckDBSuccess;
    let out = prepared.then(|| {
        (0..ffi::duckdb_prepared_statement_column_count(prep))
            .filter_map(|i| {
                let p = ffi::duckdb_prepared_statement_column_name(prep, i);
                if p.is_null() {
                    return None;
                }
                let name = CStr::from_ptr(p).to_string_lossy().into_owned();
                ffi::duckdb_free(p as *mut std::ffi::c_void);
                Some(name)
            })
            .collect()
    });
    // A statement that did not prepare is destroyed too: it holds its error.
    ffi::duckdb_destroy_prepare(&mut prep);
    out
}

const ALLOWED: &str = "SELECT, CREATE, INSERT, UPDATE, DELETE, MERGE, DROP, ALTER";

/// Statement `index` (from 1) of the batch `sql`, a `kind` the agent may not run. A `SET` of
/// `TimeZone` is told the session is UTC and how to reach local periods, not the statements
/// allowed: the zone is the one setting an agent has a reason of its own to want (#52).
fn denied(index: usize, kind: &'static str, sql: &str) -> WorkError {
    let zone = kind == "SET"
        && crate::hint::statement(sql, index - 1).is_some_and(|s| crate::hint::sets_time_zone(&s));
    WorkError::Denied {
        index,
        kind: if zone { TIME_ZONE } else { kind },
        allowed: ALLOWED,
    }
}

/// The kind a refused `SET TimeZone` is given, so it is told what it needs ([`denied`]).
const TIME_ZONE: &str = "SET TimeZone";

/// A directory name no other bench or session will pick: random bits and a counter. A clock was
/// not enough — macOS reads it to the microsecond, so two benches opened together in one process
/// got the same name.
///
/// The process id is mixed in only where there is no randomness to be had, never written out:
/// the path is in the agent's catalog (`duckdb_databases()`, `allowed_directories`), and a pid
/// there is a fact about the machine the agent has no use for (REQ-15).
pub fn fresh_name(prefix: &str) -> String {
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut bits = [0u8; 8];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bits))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        bits = (nanos ^ (u64::from(std::process::id()) << 40)).to_ne_bytes();
    }
    format!(
        "{prefix}-{:016x}-{}",
        u64::from_ne_bytes(bits),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// The most an empty result's note may take (REQ-11), and never more than the batch has left of
/// its deadline (`Workspace::why_empty`). Past it the note's query is interrupted and the result
/// goes back without a note: a note is a convenience, and the batch's deadline is not.
const NOTE_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// The most text values an empty result's note lists for one column (REQ-11).
const NOTE_VALUES: usize = 20;

/// Rows read from the head of a column before its full distinct values are: more than
/// [`NOTE_VALUES`] distinct among them proves the column cannot give a note, after one vector
/// instead of the whole table (`Workspace::distinct`).
const NOTE_HEAD_ROWS: usize = 2048;

/// REQ-14: the profile a loader left in table `name`'s comment in catalog `db`, if it left one
/// this version can read. Read once, at attach: `describe` only renders it.
/// What the catalogs of a batch hold, from one listing of each kind (`catalog_listings`).
#[derive(Default)]
struct Listings {
    /// Each catalog's tables: schema, name, comment.
    tables: std::collections::HashMap<String, Vec<(String, String, Option<String>)>>,
    /// How many views, sequences, indexes, types and schemas of its own each catalog holds.
    others: std::collections::HashMap<String, u64>,
    /// Each table's columns, by catalog and table, in order: name and DuckDB type.
    columns: std::collections::HashMap<(String, String), Vec<(String, String)>>,
}

/// One catalog's part of the [`Listings`].
struct Held<'a> {
    tables: &'a [(String, String, Option<String>)],
    others: u64,
    columns: Vec<(String, String)>,
}

impl Listings {
    fn of(&self, alias: &str) -> Held<'_> {
        let tables = self.tables.get(alias).map_or(&[][..], |t| t.as_slice());
        let columns = tables
            .first()
            .and_then(|(_, t, _)| self.columns.get(&(alias.to_string(), t.clone())))
            .cloned()
            .unwrap_or_default();
        Held {
            tables,
            others: self.others.get(alias).copied().unwrap_or(0),
            columns,
        }
    }
}

fn comment_of(conn: &Connection, db: &str, name: &str) -> Option<crate::profile::Profile> {
    let c: Option<String> = conn
        .query_row(
            "SELECT comment FROM system.main.duckdb_tables() \
             WHERE database_name = ? AND table_name = ?",
            [db, name],
            |r| r.get(0),
        )
        .ok()?;
    crate::profile::Profile::from_comment(&c?)
}

/// Whether the agent's connection is in UTC, given the zone it reports. A library without ICU
/// has no `TimeZone` setting to read and UTC is its only zone; with ICU loaded, a zone that
/// cannot be read is as bad as a wrong one (`a_zone_other_than_utc_stops_the_engine`).
fn zone_held(zone: Option<&str>, icu_loaded: impl FnOnce() -> i64) -> Result<(), String> {
    match zone {
        Some("UTC") => Ok(()),
        Some(z) => Err(format!("the agent connection is in time zone {z}, not UTC")),
        None if icu_loaded() == 0 => Ok(()),
        None => Err("ICU is loaded but the agent connection's time zone cannot be read".into()),
    }
}

/// An in-memory database with its two connections: the agent's, raw, and the host's.
fn fresh_engine() -> Result<(ffi::duckdb_database, ffi::duckdb_connection, Connection), WorkError> {
    unsafe {
        let (mut db, mut agent) = (ptr::null_mut(), ptr::null_mut());
        let mem = CString::new(":memory:").unwrap();
        if ffi::duckdb_open(mem.as_ptr(), &mut db) != ffi::duckdb_state_DuckDBSuccess {
            return Err(WorkError::Open("duckdb_open failed".into()));
        }
        if ffi::duckdb_connect(db, &mut agent) != ffi::duckdb_state_DuckDBSuccess {
            return Err(WorkError::Open("duckdb_connect failed".into()));
        }
        let ingest = Connection::open_from_raw(db).map_err(|e| WorkError::Open(e.to_string()))?;
        Ok((db, agent, ingest))
    }
}

impl Bench {
    /// The file a bench is built in, inside the directory it was opened in.
    pub const FILE: &'static str = "bench.duckdb";

    /// Close the bench and hand back its file, complete: what a loader delivers.
    pub fn finish(self) -> Result<PathBuf, WorkError> {
        let file = self.workspace_file();
        drop(self.ingest);
        unsafe {
            let mut db = self.db;
            ffi::duckdb_close(&mut db);
        }
        Ok(file)
    }

    /// Where this bench keeps its files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Open an unsealed bench. The host may load; the agent cannot reach it, because only a
    /// sealed [`Workspace`] can run agent SQL.
    pub fn open() -> Result<Self, WorkError> {
        Self::open_in(&std::env::temp_dir().join(fresh_name("benchy")))
    }

    /// Open a bench in `dir`, which must not exist yet: a directory someone else created first
    /// could hold files planted for the bench to read.
    pub fn open_in(dir: &Path) -> Result<Self, WorkError> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = dir.to_path_buf();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| WorkError::Open(format!("{}: {e}", dir.display())))?;
        let file = dir.join(Bench::FILE);
        unsafe {
            let mut db = ptr::null_mut();
            let path = CString::new(file.to_string_lossy().as_ref())
                .map_err(|_| WorkError::Open("workspace path contains a NUL".into()))?;
            // The file is written in storage version STORAGE_VERSION, which must be set as it is
            // created: a `SET` after the open applies only to databases attached later.
            let mut config = ptr::null_mut();
            if ffi::duckdb_create_config(&mut config) != ffi::duckdb_state_DuckDBSuccess {
                return Err(WorkError::Open("duckdb_create_config failed".into()));
            }
            let set = ffi::duckdb_set_config(
                config,
                c"storage_compatibility_version".as_ptr(),
                STORAGE_VERSION.as_ptr(),
            );
            let mut error = ptr::null_mut();
            let opened = set == ffi::duckdb_state_DuckDBSuccess
                && ffi::duckdb_open_ext(path.as_ptr(), &mut db, config, &mut error)
                    == ffi::duckdb_state_DuckDBSuccess;
            ffi::duckdb_destroy_config(&mut config);
            if !opened {
                let why = if error.is_null() {
                    "could not set its storage version".to_string()
                } else {
                    let why = std::ffi::CStr::from_ptr(error)
                        .to_string_lossy()
                        .into_owned();
                    ffi::duckdb_free(error.cast());
                    why
                };
                return Err(WorkError::Open(format!("duckdb_open failed: {why}")));
            }
            let ingest =
                Connection::open_from_raw(db).map_err(|e| WorkError::Open(e.to_string()))?;
            // Before anything is loaded: a reader this build does not include must fail loudly,
            // never be fetched. DuckDB's default installs and loads a known extension on first
            // use, so without this a missing Parquet or JSON reader becomes a silent download of
            // code from the internet during loading — before the seal turns it off.
            // And in UTC, whatever the machine's zone: a loader's reader parses offsets and a
            // conversion casts, and neither may depend on where benchy happens to run (REQ-15).
            // GLOBAL, so every connection to the bench is in it, not this one alone: `TimeZone`
            // is per connection, and the helpers' build opens one of its own
            // (`every_bench_connection_is_utc_on_a_host_that_is_not`).
            // And one row group of a bulk insert held before it is written, not DuckDB's five: a
            // loader writes one table once, on one thread. Measured on DuckDB 1.5.5, the loader's
            // peak fell 11–16 MB (parquet-1m 72 → 60 MB, exec-1m 70 → 57 MB, exec-10m 116 →
            // 101 MB, ch-10m 104 → 94 MB; a staged CSV's did not move) with instructions within
            // ±0.8%, and the same row groups, segments and compression from every source. Each
            // row group's segments now start a block of their own: the eval's 840,960 meter rows
            // take 29 blocks where they took 28, ch-10m's file is 2% larger, and queries over
            // them run as fast (docs/history/PERFORMANCE-rounds.md R2-8;
            // `a_loader_writes_each_row_group_as_it_fills`).
            ingest
                .execute_batch(
                    "SET autoinstall_known_extensions = false; \
                     SET autoload_known_extensions = false; \
                     SET GLOBAL TimeZone = 'UTC'; \
                     SET write_buffer_row_group_count = 1;",
                )
                .map_err(|e| {
                    WorkError::Open(format!("could not disable extension autoload: {e}"))
                })?;
            Ok(Self {
                db,
                ingest,
                loaded: Vec::new(),
                dir,
                shapes: Default::default(),
            })
        }
    }

    pub fn workspace_file(&self) -> PathBuf {
        self.dir.join("bench.duckdb")
    }

    /// Host-side only. Never reachable from agent SQL.
    pub fn connection(&self) -> &Connection {
        &self.ingest
    }

    /// A raw connection of the bench's own, for what the C API gives and the safe one does not:
    /// a prepared statement's type, which the helpers' build holds to the guard's
    /// (`crate::helpers::build`). The caller disconnects it.
    pub(crate) fn connect_raw(&self) -> Result<ffi::duckdb_connection, WorkError> {
        let mut con = ptr::null_mut();
        if unsafe { ffi::duckdb_connect(self.db, &mut con) } != ffi::duckdb_state_DuckDBSuccess {
            return Err(WorkError::Open("duckdb_connect failed".into()));
        }
        Ok(con)
    }

    /// What the operator stated about table `name`'s shape, before it is loaded.
    pub fn declare(&mut self, name: &str, shape: crate::profile::Shape) {
        self.shapes.insert(name.to_string(), shape);
    }

    /// A table is on the bench. REQ-14: it is profiled now, once, with the loader's threads, and
    /// the profile goes into the table's comment, which travels with the file to the engine.
    /// `Err` only when the operator's stated shape does not match the table.
    pub fn record(&mut self, mut t: LoadedTable) -> Result<(), String> {
        let shape = self.shapes.get(&t.name).cloned().unwrap_or_default();
        let p = crate::trace::span("profile", || {
            crate::profile::compute(&self.ingest, &t.name, &t.columns, &shape, t.seen.take())
        })?;
        // A comment benchy cannot write loses the shape, never the table.
        let _ = self.ingest.execute_batch(&format!(
            "COMMENT ON TABLE \"{}\" IS '{}'",
            t.name.replace('"', "\"\""),
            p.to_comment().replace('\'', "''")
        ));
        t.profile = Some(p);
        self.loaded.push(t);
        Ok(())
    }

    /// Attach what was loaded `READ_ONLY`, lock the engine, prove the boundary holds, and hand
    /// back the only type that can run agent SQL.
    ///
    /// REQ-9 is enforced here and by DuckDB: loaded tables live in a read-only catalog, and the
    /// agent's own objects land in `memory.main` because that is first on the search path. An
    /// `UPDATE` against a loaded table fails in the engine, not in a string check.
    pub fn seal(self, memory_limit: &str) -> Result<Workspace, WorkError> {
        let file = self.workspace_file();
        // flush and release the writable handle before re-attaching it read-only
        drop(self.ingest);
        unsafe {
            let mut db = self.db;
            ffi::duckdb_close(&mut db);
        }

        let (db, agent, ingest) = fresh_engine()?;
        let quoted = file.to_string_lossy().replace('\'', "''");
        ingest
            .execute_batch(&format!("ATTACH '{quoted}' AS bench (READ_ONLY)"))
            .map_err(|e| {
                WorkError::Open(format!("could not attach the workspace read-only: {e}"))
            })?;
        // Unlinked once attached. Turning external access off puts every attached database's
        // path on DuckDB's own allowed list, so `read_blob` could read the file's raw bytes by
        // the path the catalog discloses (`the_workspace_file_cannot_be_read_as_a_file`). DuckDB
        // reads through the handle it holds; nothing is left at the path, and the space goes
        // back when the engine exits, however it exits.
        for f in [file.clone(), file.with_extension("duckdb.wal")] {
            let _ = std::fs::remove_file(f);
        }
        // `memory.main` first, so an unqualified CREATE is the agent's own and an unqualified
        // SELECT still finds the loaded tables.
        const SEARCH_PATH: &str = "SET search_path = 'memory.main,bench.main'";
        ingest
            .execute_batch(SEARCH_PATH)
            .map_err(|e| WorkError::Open(e.to_string()))?;
        unsafe { raw_exec(agent, SEARCH_PATH) }.map_err(WorkError::Open)?;

        let mut loaded = self.loaded;
        for t in &mut loaded {
            t.profile = comment_of(&ingest, "bench", &t.name);
        }
        lockdown::apply(&ingest, memory_limit).map_err(|e| WorkError::Open(e.to_string()))?;
        let ws = Workspace {
            db,
            agent,
            ingest,
            classifier: unsafe { Classifier::from_raw(agent) },
            calls: unsafe { crate::guard::Calls::for_engine(agent) },
            next_result_id: 1,
            result_prefix: "r".into(),
            unavailable: Vec::new(),
            taken: Taken::default(),
            seen: Default::default(),
            helpers: Vec::new(),
            helper_alias: None,
            spoiled: None,
            retained: std::collections::VecDeque::new(),
            loaded,
            dir: self.dir,
            attached: Vec::new(),
            next_alias: 1,
        };
        // `lock_configuration` is database-wide, not per-connection: applying it on the ingest
        // connection locks the agent connection too. Stronger than per-connection would be — so
        // prove it on the agent connection rather than assume it, and refuse to serve if it
        // does not hold.
        // `Bench` deliberately has no `Drop`: ownership of the database moves into `Workspace`
        // here, and the only path that does not reach this point is a startup failure, after
        // which the process exits. REQ-1 makes that true by construction — one bench, one
        // process.
        ws.verify_closed()?;
        ws.verify_locked(1)?;
        ws.verify_utc()?;
        ws.verify_readonly()?;
        Ok(ws)
    }
}

impl Workspace {
    pub fn loaded(&self) -> &[LoadedTable] {
        &self.loaded
    }

    /// The agent's connection, raw, past the statement allowlist: for `tests/threads.rs`, which
    /// puts a function of its own on it to see which threads run agent SQL and what they can
    /// reach. Nothing in the engine calls it; agent SQL reaches the bench only through
    /// [`Workspace::work`].
    #[doc(hidden)]
    pub fn agent_connection(&self) -> ffi::duckdb_connection {
        self.agent
    }

    /// A bench that starts sealed and empty, on one thread, in a process that confines itself
    /// some other way or not at all: the tests. The engine's is [`Workspace::open_confined`].
    pub fn open_sealed(dir: &Path, memory_limit: &str) -> Result<Workspace, WorkError> {
        Self::open_confined(dir, memory_limit, 1, || Ok(())).map(|(ws, ())| ws)
    }

    /// A bench that starts sealed and empty, its agent SQL on `threads` threads. Tables arrive
    /// later, each by [`Workspace::attach`], from `dir` — the one directory DuckDB may open after
    /// the lock, emptied again by every attach, because an allowed directory is open to every
    /// file reader.
    ///
    /// `confine` is called between the two steps of the lock (`lockdown::prepare` and
    /// `lockdown::lock`), on this thread, with DuckDB's workers sent away: the engine confines
    /// itself there, so the workers `lock` starts for agent SQL are started by a confined thread
    /// and are confined too (REQ-7 `the_engines_threads_are_started_after_it_is_confined`). What
    /// it returns is handed back beside the bench.
    pub fn open_confined<T>(
        dir: &Path,
        memory_limit: &str,
        threads: usize,
        confine: impl FnOnce() -> Result<T, String>,
    ) -> Result<(Workspace, T), WorkError> {
        let (db, agent, ingest) = fresh_engine()?;
        unsafe { raw_exec(agent, "SET search_path = 'memory.main'") }.map_err(WorkError::Open)?;
        lockdown::prepare(&ingest, memory_limit, Some(dir))
            .map_err(|e| WorkError::Open(e.to_string()))?;
        let ws = Workspace {
            db,
            agent,
            ingest,
            classifier: unsafe { Classifier::from_raw(agent) },
            calls: unsafe { crate::guard::Calls::for_engine(agent) },
            next_result_id: 1,
            result_prefix: "r".into(),
            unavailable: Vec::new(),
            taken: Taken::default(),
            seen: Default::default(),
            helpers: Vec::new(),
            helper_alias: None,
            spoiled: None,
            retained: std::collections::VecDeque::new(),
            loaded: Vec::new(),
            dir: dir.to_path_buf(),
            attached: Vec::new(),
            next_alias: 1,
        };
        // Before the confinement, so the refusal proven is DuckDB's own and not Landlock's.
        ws.verify_closed()?;
        let confined = confine().map_err(WorkError::Open)?;
        lockdown::lock(&ws.ingest, threads).map_err(|e| WorkError::Open(e.to_string()))?;
        ws.verify_locked(threads)?;
        ws.verify_utc()?;
        Ok((ws, confined))
    }

    /// Put table `name` from the DuckDB file `file` — which holds it under that name — on the
    /// bench, read-only, now. A name already there is replaced: the agent's next query sees the
    /// new data, its own tables stay, and retained results go, since some may hold the old rows.
    /// If anything fails, the bench is as it was.
    pub fn attach(&mut self, name: &str, file: &Path) -> Result<LoadedTable, WorkError> {
        self.attach_all(&[(name.to_string(), file.to_path_buf())])
            .pop()
            .expect("one table in, one answer out")
    }

    /// [`Workspace::attach`] for several tables at once, in order, with one answer for each: a
    /// table that is refused costs the others nothing.
    ///
    /// Each file is attached and checked on its own, as `attach` does, except for the one check
    /// that has to list every function DuckDB has: whether the file brings a function of its own.
    /// `duckdb_functions()` builds a row for each of DuckDB's ~3,000 built-in functions and
    /// overloads before any filter applies (DuckDB 1.5 gives that table function neither filter
    /// nor projection pushdown, and no other listing reaches a catalog's macros), which was 70% of
    /// the engine's instructions and about 0.19 G per table (docs/history/PERFORMANCE-rounds.md,
    /// round 1 item 5). Asked
    /// once for every catalog in the batch, it is paid once. Every catalog is still checked
    /// before any of them goes on the agent's search path, so the check is as strict as before
    /// (`an_attached_file_brings_its_table_and_nothing_else`,
    /// `a_function_in_one_file_of_a_batch_refuses_that_file_alone`).
    pub fn attach_all(
        &mut self,
        tables: &[(String, std::path::PathBuf)],
    ) -> Vec<Result<LoadedTable, WorkError>> {
        let mut checked: Vec<Result<(String, LoadedTable), WorkError>> = Vec::new();
        // Attached and waiting for the check: where its answer goes, its name, its alias.
        let mut pending: Vec<(usize, &str, String)> = Vec::new();
        // REQ-9 `an_attached_name_is_taken_too`: the agent's own tables and views, which sit
        // ahead of every attached catalog on its search path. Read on its connection, the only
        // one that sees its temporary catalog; if they cannot be read, nothing is attached.
        let own = self.agent_rows(AGENT_RELATIONS);
        for (name, file) in tables {
            let mine = match &own {
                // REQ-18 `a_host_cannot_attach_under_a_helpers_name`
                _ if self.taken.helpers.contains(&name.to_ascii_lowercase()) => {
                    Some(WorkError::Taken(format!(
                        "`{name}` is the name of a helper the operator provides; attach it under \
                         another name"
                    )))
                }
                Ok(own) => own.iter().any(|r| r.1.eq_ignore_ascii_case(name)).then(|| {
                    WorkError::Taken(format!(
                        "the agent has a table or view named `{name}` of its own, which \
                             would hide this one from it; attach it under another name"
                    ))
                }),
                Err(e) => Some(WorkError::Open(format!("could not inspect `{name}`: {e}"))),
            };
            if let Some(refused) = mine {
                for f in [file.to_path_buf(), file.with_extension("duckdb.wal")] {
                    let _ = std::fs::remove_file(f);
                }
                checked.push(Err(refused));
                continue;
            }
            // Numbered, and the same whatever the table came from: the catalog's name is in
            // `duckdb_databases()` and in the agent's search path, so it says no more than that
            // this is benchy's (REQ-15). `benchy_` is a prefix no host may name a table with.
            let alias = format!("benchy_{}", self.next_alias);
            self.next_alias += 1;
            let quoted = file.to_string_lossy().replace('\'', "''");
            let attached = crate::trace::span("attach.attach", || {
                self.ingest
                    .execute_batch(&format!("ATTACH '{quoted}' AS {alias} (READ_ONLY)"))
            });
            // Unlinked whatever happened: an allowed directory is open to every file reader, so
            // nothing may stay in it. DuckDB reads an attached file through the handle it holds.
            for f in [file.to_path_buf(), file.with_extension("duckdb.wal")] {
                let _ = std::fs::remove_file(f);
            }
            if let Err(e) = attached {
                checked.push(Err(WorkError::Open(format!(
                    "could not attach `{name}`: {e}"
                ))));
                continue;
            }
            pending.push((checked.len(), name, alias));
            checked.push(Err(WorkError::Open(format!("`{name}` was not checked"))));
        }
        let listed = {
            let each: Vec<(&str, &str)> = pending
                .iter()
                .map(|(_, name, alias)| (*name, alias.as_str()))
                .collect();
            crate::trace::span("attach.listings", || self.catalog_listings(&each))
        };
        for (i, name, alias) in pending {
            let found = listed.as_ref().map(|l| l.of(&alias));
            match crate::trace::span("attach.inspect", || {
                self.attached_table(name, &alias, found)
            }) {
                Ok(t) => checked[i] = Ok((alias, t)),
                Err(e) => {
                    let _ = self.ingest.execute_batch(&format!("DETACH {alias}"));
                    checked[i] = Err(e);
                }
            }
        }
        let aliases: Vec<&str> = checked
            .iter()
            .filter_map(|c| c.as_ref().ok().map(|(a, _)| a.as_str()))
            .collect();
        if !aliases.is_empty() {
            let found = crate::trace::span("attach.functions", || {
                self.catalogs_with_functions(&aliases)
            });
            for c in checked.iter_mut() {
                let Ok((alias, t)) = c else { continue };
                let why = match &found {
                    Ok(with) if !with.contains(alias.as_str()) => continue,
                    Ok(_) => format!(
                        "what was attached as `{}` holds more than that one table",
                        t.name
                    ),
                    Err(e) => format!("could not inspect `{}`: {e}", t.name),
                };
                let _ = self.ingest.execute_batch(&format!("DETACH {alias}"));
                *c = Err(WorkError::Open(why));
            }
        }
        checked
            .into_iter()
            .map(|c| {
                let (alias, table) = c?;
                self.put_on_bench(alias, table)
            })
            .collect()
    }

    /// The catalogs among `aliases` that hold a function of any kind: a macro or a table macro,
    /// the only kinds a DuckDB file can carry. The aliases are benchy's own `benchy_<n>`.
    fn catalogs_with_functions(
        &self,
        aliases: &[&str],
    ) -> Result<std::collections::HashSet<String>, duckdb::Error> {
        let list = aliases
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let mut st = self.ingest.prepare(&format!(
            "SELECT DISTINCT database_name FROM system.main.duckdb_functions() \
             WHERE database_name IN ({list})"
        ))?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// A checked catalog goes on the agent's search path as table `table.name`, replacing one of
    /// the same name. If that fails, the bench is as it was.
    fn put_on_bench(
        &mut self,
        alias: String,
        table: LoadedTable,
    ) -> Result<LoadedTable, WorkError> {
        let name = table.name.clone();
        let previous = self.attached.iter().position(|(n, _)| *n == name);
        let old = match previous {
            Some(i) => Some(std::mem::replace(&mut self.attached[i].1, alias.clone())),
            None => {
                self.attached.push((name.clone(), alias.clone()));
                None
            }
        };
        if let Err(e) = crate::trace::span("attach.search_path", || self.point_search_path()) {
            // As it was: the new catalog out, the old one back.
            match (previous, &old) {
                (Some(i), Some(o)) => self.attached[i].1 = o.clone(),
                _ => {
                    self.attached.pop();
                }
            }
            let _ = self.point_search_path();
            let _ = self.ingest.execute_batch(&format!("DETACH {alias}"));
            return Err(e);
        }
        if let Some(o) = old {
            let _ = self.ingest.execute_batch(&format!("DETACH {o}"));
            // Replacing is how a host takes rows away, so what may hold the old rows goes too.
            self.retained.clear();
        }
        self.loaded.retain(|t| t.name != name);
        self.loaded.push(table.clone());
        Ok(table)
    }

    /// Take table `name` away. Retained results go with it, since some may hold its rows.
    pub fn detach(&mut self, name: &str) -> Result<(), WorkError> {
        let Some(i) = self.attached.iter().position(|(n, _)| n == name) else {
            return Err(WorkError::Open(format!("no table `{name}` was attached")));
        };
        let (_, alias) = self.attached.remove(i);
        self.point_search_path()?;
        let _ = self.ingest.execute_batch(&format!("DETACH {alias}"));
        self.loaded.retain(|t| t.name != name);
        self.retained.clear();
        Ok(())
    }

    /// What an attach brought: the table is there under its name, nothing else is, and DuckDB
    /// will not write to it. Functions are the one thing not checked here: `attach_all` asks
    /// for every catalog of a batch at once, before any goes on the search path.
    ///
    /// Nothing else, because the catalog goes on the agent's search path, and DuckDB resolves a
    /// function there before its own — a macro named `lower` in the file stands in for `lower`,
    /// measured. A loader writes one table, so a file holding anything more is not what a loader
    /// wrote (`an_attached_file_brings_its_table_and_nothing_else`).
    ///
    /// What the catalog holds is read from `found`, its part of the listings `attach_all` made
    /// once for the whole batch (`catalog_listings`).
    fn attached_table(
        &self,
        name: &str,
        alias: &str,
        found: Result<Held<'_>, &duckdb::Error>,
    ) -> Result<LoadedTable, WorkError> {
        let held =
            found.map_err(|e| WorkError::Open(format!("could not inspect `{name}`: {e}")))?;
        let only_the_table = held.tables.len() == 1 && held.others == 0;
        if !only_the_table {
            return Err(WorkError::Open(format!(
                "what was attached as `{name}` holds more than that one table"
            )));
        }
        let (schema, table, comment) = &held.tables[0];
        if schema != "main" || table != name {
            return Err(WorkError::Open(format!(
                "`{name}` is not in what was attached"
            )));
        }
        let ident = name.replace('"', "\"\"");
        let rows: i64 = self
            .ingest
            .query_row(
                &format!("SELECT count(*) FROM {alias}.main.\"{ident}\""),
                [],
                |r| r.get(0),
            )
            .map_err(|e| WorkError::Open(format!("`{name}` is not in what was attached: {e}")))?;
        let columns = held.columns.clone();
        if self
            .ingest
            .execute_batch(&format!(
                "CREATE TABLE {alias}.main.benchy_probe (x INTEGER)"
            ))
            .is_ok()
        {
            return Err(WorkError::Open(format!("`{name}` was attached writable")));
        }
        Ok(LoadedTable {
            name: name.to_string(),
            rows: rows as u64,
            columns,
            source: "attach".into(),
            profile: comment
                .as_deref()
                .and_then(crate::profile::Profile::from_comment),
            seen: None,
        })
    }

    /// What each catalog of a batch holds, but functions (`catalogs_with_functions`): one
    /// listing of each kind for all of them, where each catalog was asked on its own before.
    ///
    /// Asked one catalog at a time, the check was 35 M instructions a table, measured on a host's
    /// batch of eight (docs/PERFORMANCE.md R2-5): a query over six listings, then one over the
    /// columns, then one over the tables for the comment. Measured with DuckDB 1.5, the six
    /// listings filtered to one catalog took 6.3 ms, filtered to eight 5.7 ms; the columns
    /// 2.9 ms for one, 3.2 ms for eight; the comment 1.1 ms for one, 1.5 ms for eight.
    /// `anything_else_in_one_file_of_a_batch_refuses_that_file_alone` holds each catalog to the
    /// same check as before.
    fn catalog_listings(&self, each: &[(&str, &str)]) -> Result<Listings, duckdb::Error> {
        let mut out = Listings::default();
        if each.is_empty() {
            return Ok(out);
        }
        // benchy's own `benchy_<n>`, quoted all the same.
        let list = each
            .iter()
            .map(|(_, a)| format!("'{}'", a.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");
        let mut st = self.ingest.prepare(&format!(
            "SELECT database_name, schema_name, table_name, comment FROM system.main.duckdb_tables() \
             WHERE database_name IN ({list})"
        ))?;
        for t in st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
            let (db, schema, table, comment): (String, String, String, Option<String>) = t?;
            out.tables
                .entry(db)
                .or_default()
                .push((schema, table, comment));
        }
        let others = [
            "system.main.duckdb_views()",
            "system.main.duckdb_sequences()",
            "system.main.duckdb_indexes()",
        ]
        .iter()
        .map(|f| format!("SELECT database_name FROM {f} WHERE database_name IN ({list})"))
        .chain(
            ["system.main.duckdb_types()", "system.main.duckdb_schemas()"]
                .iter()
                .map(|f| {
                    format!(
                        "SELECT database_name FROM {f} WHERE database_name IN ({list}) \
                     AND NOT internal"
                    )
                }),
        )
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
        let mut st = self.ingest.prepare(&format!(
            "SELECT database_name, count(*) FROM ({others}) GROUP BY ALL"
        ))?;
        for o in st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
            let (db, n): (String, i64) = o?;
            out.others.insert(db, n as u64);
        }
        let mut st = self.ingest.prepare(&format!(
            "SELECT database_name, table_name, column_name, data_type FROM system.main.duckdb_columns() \
             WHERE database_name IN ({list}) ORDER BY database_name, table_name, column_index"
        ))?;
        for c in st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))? {
            let (db, table, column, ty): (String, String, String, String) = c?;
            out.columns
                .entry((db, table))
                .or_default()
                .push((column, ty));
        }
        Ok(out)
    }

    /// The agent's own schema first, where its CREATE lands, then every attached catalog. Set on
    /// the agent's connection alone, and the agent cannot change it: SET is not allowed.
    ///
    /// The helpers' catalog comes after the agent's schema, so an unqualified CREATE stays the
    /// agent's own, and before the tables, though no helper and table share a name.
    fn point_search_path(&self) -> Result<(), WorkError> {
        let mut path = vec!["memory.main".to_string()];
        path.extend(self.helper_alias.iter().map(|a| format!("{a}.main")));
        path.extend(self.attached.iter().map(|(_, a)| format!("{a}.main")));
        let sql = format!("SET search_path = '{}'", path.join(","));
        unsafe { raw_exec(self.agent, &sql) }.map_err(WorkError::Open)
    }

    /// The catalog a loaded table lives in.
    fn catalog_of(&self, table: &str) -> &str {
        self.attached
            .iter()
            .find(|(n, _)| n == table)
            .map_or("bench", |(_, a)| a.as_str())
    }

    /// What the engine actually is. REQ-11's `version_not_hardcoded`: the dialect drifts between
    /// releases, so an agent told "DuckDB" and an agent told "DuckDB v1.5.5" are not equally
    /// well informed, and a version baked into a string literal goes stale silently.
    pub fn engine_version(&self) -> String {
        self.ingest
            .query_row("SELECT system.main.version()", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap_or_else(|_| "unknown".into())
    }

    /// The private directory holding the read-only workspace file. REQ-3 restricts the process
    /// to this and nothing else.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// REQ-2 layer 1 in isolation. Exposed so the escape corpus can assert the division of
    /// labour between the two mechanisms still holds after a DuckDB upgrade.
    pub fn classify(&self, sql: &str) -> Verdict {
        self.classifier.check(sql)
    }

    /// Every name the agent can use right now — what was loaded, plus whatever it created
    /// itself. REQ-11's hints name this rather than only the loaded set, because a hint that
    /// omits the table the agent made one call ago sends it looking for something it has.
    ///
    /// Read on the host-facing connection: it sees the same catalogs, and the agent connection
    /// has just failed a statement.
    fn nameable(&self) -> Vec<String> {
        let sql = "SELECT table_name FROM system.main.duckdb_tables() WHERE NOT internal \
                   UNION SELECT view_name FROM system.main.duckdb_views() WHERE NOT internal \
                   ORDER BY 1";
        let queried = (|| -> Result<Vec<String>, duckdb::Error> {
            let mut st = self.ingest.prepare(sql)?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect()
        })();
        match queried {
            Ok(names) if !names.is_empty() => names,
            // The catalogue query is a convenience, not a guarantee. If it fails we still know
            // what we put on the bench ourselves.
            _ => self.loaded.iter().map(|t| t.name.clone()).collect(),
        }
    }

    /// The columns of each of `tables` the agent can name — loaded, its own, a helper view —
    /// in their order, for REQ-11's hints (#27). Asked only by the hints that list columns, and
    /// no table is scanned. A loaded table's are the ones kept from its attach.
    ///
    /// The rest are what `SELECT * FROM <name>` would give, prepared and never run on the
    /// agent's connection, which is the one whose search path resolves a helper view's tables.
    /// Measured on the bundled bench with its helpers, DuckDB 1.5.5, per failing query that
    /// names a helper view, over the whole session: 5.1 M instructions before #27, 6.4 M so.
    /// `duckdb_columns()` on the host-facing connection took it to 12.3 M, as it builds a row
    /// for every column of every catalog, DuckDB's own 527 included, before any filter applies;
    /// `pragma_table_info` there cannot bind a helper view, whose tables are named unqualified.
    /// A loaded table's columns cost nothing measurable: 3.7–3.9 M before and after.
    fn columns_of(&self, tables: &[&str]) -> Vec<(String, Vec<String>)> {
        tables
            .iter()
            .filter_map(|t| {
                if let Some(l) = self.loaded.iter().find(|l| l.name.eq_ignore_ascii_case(t)) {
                    return Some((
                        l.name.clone(),
                        l.columns.iter().map(|(c, _)| c.clone()).collect(),
                    ));
                }
                let sql = format!("SELECT * FROM \"{}\"", t.replace('"', "\"\""));
                // SAFETY: the agent connection is this workspace's, and no query runs on it now:
                // the statement that failed has returned. Prepared only, so nothing executes.
                let cols = unsafe { bound_columns(self.agent, &sql) }?;
                Some((t.to_string(), cols))
            })
            .collect()
    }

    /// REQ-9: prove the loaded data really is read-only, rather than trusting the attach.
    fn verify_readonly(&self) -> Result<(), WorkError> {
        let Some(t) = self.loaded.first() else {
            return Ok(());
        };
        let probe = format!("DELETE FROM bench.main.\"{}\"", t.name.replace('"', "\"\""));
        if self.raw_exec_unchecked(&probe).is_ok() {
            return Err(WorkError::Open(format!(
                "loaded table `{}` is writable after sealing",
                t.name
            )));
        }
        Ok(())
    }

    /// The boundary must be in force on the connection the agent actually uses. A silently
    /// unlocked engine would be a leak discovered later; this makes it a startup failure.
    ///
    /// That it does its job, not merely that it is locked: proven before the process confines
    /// itself, so that the refusal is DuckDB's. Under Landlock the read would be refused all the
    /// same, and an engine whose own boundary had failed would pass.
    fn verify_closed(&self) -> Result<(), WorkError> {
        if self
            .raw_exec_unchecked("SELECT * FROM read_text('/etc/hostname')")
            .is_ok()
        {
            return Err(WorkError::Open(
                "the agent connection can still read files after lockdown".into(),
            ));
        }
        Ok(())
    }

    /// That the lock holds on the agent's connection, and that agent SQL runs on the `threads`
    /// asked for. External access cannot be turned back on even before the lock (DuckDB 1.5.5),
    /// so that alone would not show a lock missing: `threads` can be set until the lock and not
    /// after it, which shows it, and is the setting a runaway would raise first.
    fn verify_locked(&self, threads: usize) -> Result<(), WorkError> {
        if self
            .raw_exec_unchecked("SET enable_external_access = true")
            .is_ok()
        {
            return Err(WorkError::Open(
                "the agent connection accepted re-enabling external access after lockdown".into(),
            ));
        }
        for undo in [
            format!("SET threads = {}", threads + 1),
            format!("SET GLOBAL threads = {}", threads + 1),
            "RESET threads".to_string(),
        ] {
            if self.raw_exec_unchecked(&undo).is_ok() {
                return Err(WorkError::Open(format!(
                    "the agent connection accepted `{undo}` after lockdown: the configuration is \
                     not locked"
                )));
            }
        }
        let on = self
            .agent_rows(c"SELECT current_setting('threads')::VARCHAR")
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|row| row.1);
        if on.as_deref() != Some(threads.to_string().as_str()) {
            return Err(WorkError::Open(format!(
                "agent SQL runs on {} thread(s), not the {threads} asked for",
                on.as_deref().unwrap_or("an unknown number of")
            )));
        }
        Ok(())
    }

    /// Results must not depend on the host's zone, and `TimeZone` is per connection: proven on
    /// the connection the agent actually uses, as the lock is, not assumed from the one the
    /// lockdown ran on (REQ-10 `the_agent_connection_is_utc_on_a_host_that_is_not`).
    fn verify_utc(&self) -> Result<(), WorkError> {
        let zone = self
            .agent_rows(c"SELECT current_setting('TimeZone')")
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .map(|row| row.1);
        let icu = || -> i64 {
            self.ingest
                .query_row(
                    "SELECT count(*) FROM system.main.duckdb_extensions() \
                     WHERE extension_name = 'icu' AND loaded",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(1)
        };
        zone_held(zone.as_deref(), icu).map_err(WorkError::Open)
    }

    fn raw_exec_unchecked(&self, sql: &str) -> Result<(), String> {
        unsafe {
            let c = CString::new(sql).map_err(|_| "NUL byte".to_string())?;
            let mut r: ffi::duckdb_result = std::mem::zeroed();
            let ok = ffi::duckdb_query(self.agent, c.as_ptr(), &mut r)
                == ffi::duckdb_state_DuckDBSuccess;
            let out = if ok {
                Ok(())
            } else {
                let e = ffi::duckdb_result_error(&mut r);
                Err(if e.is_null() {
                    "error".into()
                } else {
                    CStr::from_ptr(e).to_string_lossy().into_owned()
                })
            };
            ffi::duckdb_destroy_result(&mut r);
            out
        }
    }

    /// REQ-2: classify, then run. Every statement is checked immediately before it runs.
    /// REQ-10 `a_batch_returns_every_result_not_only_the_last`: each statement's result comes back.
    pub fn work(
        &mut self,
        sql: &str,
        cap: &crate::result::Caps,
    ) -> Result<Vec<QueryResult>, WorkError> {
        // On the text, before the classifier prepares anything, so a refused function is never
        // even bound.
        if let Some(why) = self.calls.check(sql) {
            return Err(WorkError::Refused(why));
        }
        match self.classifier.check(sql) {
            Verdict::Unparseable(m) => {
                let hint = crate::hint::for_parse_error(&m, sql);
                return Err(WorkError::Parse(m, hint));
            }
            Verdict::Refuse { index, kind } => return Err(denied(index, kind, sql)),
            Verdict::Allow(_) => {}
        }
        // REQ-7: one watchdog for the whole batch. Proven in probe 2 — an execution-phase
        // runaway is cancelled and the workspace survives with its tables intact.
        // A condvar rather than a polling sleep, for two reasons.
        //
        // Latency: `finish` joins this thread at the end of *every* batch. A watchdog sleeping in
        // 20ms ticks made that join wait out the rest of the current tick, so `SELECT 1` cost
        // 22ms. `a_trivial_query_does_not_pay_for_the_watchdog` is the regression test.
        //
        // Correctness: the flag is now checked and the interrupt fired under the same lock that
        // `finish` must take to set it. The poll version could observe "not finished", be
        // descheduled, and fire into a connection whose batch had already returned.
        //
        // And past the deadline it fires again every few milliseconds until the batch ends.
        // DuckDB clears a pending interrupt when the next statement starts, so a single one that
        // landed between two statements was lost, and the rest of the batch ran unbounded.
        const REFIRE: std::time::Duration = std::time::Duration::from_millis(5);
        let started = std::time::Instant::now();
        let done = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let watchdog = {
            let flag = done.clone();
            let handle = Interrupt(self.agent);
            let deadline = cap.deadline;
            std::thread::spawn(move || {
                let (lock, cv) = &*flag;
                let mut fired = false;
                let mut finished = lock.lock().unwrap_or_else(|e| e.into_inner());
                while !*finished {
                    let left = deadline.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        // still holding the lock: `finish` cannot complete the batch underneath us
                        handle.fire();
                        fired = true;
                    }
                    let wait = if left.is_zero() { REFIRE } else { left };
                    finished = cv
                        .wait_timeout(finished, wait)
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                fired
            })
        };
        type Done = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;
        let finish = |done: &Done, w: std::thread::JoinHandle<bool>| -> bool {
            let (lock, cv) = &**done;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cv.notify_all();
            w.join().unwrap_or(false)
        };

        let mut out = Vec::new();
        // Whether each result answers a query; only a query's empty result is explained.
        let mut queries = Vec::new();
        // Which of the agent's statements each result answers, counted from 0, and how many
        // statements DuckDB found in the batch: what tells each result's SQL apart.
        let mut answers = Vec::new();
        let extracted: u64;
        // Statements DuckDB added to the agent's: a dynamic PIVOT's CREATE TYPE. They answer
        // nothing and do not count when statements are numbered.
        let mut added = 0u64;
        // Whether a dynamic PIVOT made a type, which goes when the batch does.
        let mut pivoted = false;
        // Where the batch is in a transaction DuckDB wrapped around a statement that keeps a
        // dynamic PIVOT (`Wrap`), and the agent's statement it wraps.
        let mut wrap = Wrap::None;
        let mut watchdog = Some(watchdog);
        let mut failure = None;
        unsafe {
            let c = CString::new(sql).map_err(|_| WorkError::Parse("NUL byte".into(), None))?;
            let mut ex: ffi::duckdb_extracted_statements = ptr::null_mut();
            let n = ffi::duckdb_extract_statements(self.agent, c.as_ptr(), &mut ex);
            extracted = n as u64;
            for i in 0..n {
                let mine = i - added;
                // No statement starts past the deadline, whatever became of the interrupt.
                if started.elapsed() >= cap.deadline {
                    if let Some(w) = watchdog.take() {
                        finish(&done, w);
                    }
                    failure = Some(fault(self, true, cap, mine, "", sql));
                    break;
                }
                let mut prep: ffi::duckdb_prepared_statement = ptr::null_mut();
                if ffi::duckdb_prepare_extracted_statement(self.agent, ex, i, &mut prep)
                    != ffi::duckdb_state_DuckDBSuccess
                {
                    let e = ffi::duckdb_prepare_error(prep);
                    let msg = if e.is_null() {
                        "prepare failed".into()
                    } else {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    };
                    ffi::duckdb_destroy_prepare(&mut prep);
                    let timed_out = watchdog.take().is_some_and(|w| finish(&done, w));
                    failure = Some(fault(self, timed_out, cap, mine, &msg, sql));
                    break;
                }
                // REQ-2 `checked_per_statement_before_run`. The up-front pass could not bind a
                // statement that depends on an earlier one in this batch; now it is bound, so it
                // can be classified — and a refused kind is refused here, before it executes.
                let kind = ffi::duckdb_prepared_statement_type(prep);
                // DuckDB's own transaction around a statement that keeps a dynamic PIVOT
                // (`Wrap`): neither of its TRANSACTIONs runs, and what it wraps runs as every
                // statement does, each CREATE in a transaction of benchy's. Any other TRANSACTION
                // is the agent's, and refused where it stands.
                let transaction =
                    kind == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_TRANSACTION;
                let refused = match wrap {
                    Wrap::None | Wrap::Ran(_) if transaction => {
                        wrap = match wrap {
                            Wrap::None => Wrap::Opened(mine),
                            _ => Wrap::None,
                        };
                        ffi::duckdb_destroy_prepare(&mut prep);
                        added += 1;
                        continue;
                    }
                    Wrap::Opened(at)
                        if kind != ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE =>
                    {
                        Some(at)
                    }
                    Wrap::Typed(at)
                        if transaction
                            || kind == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT =>
                    {
                        Some(at)
                    }
                    Wrap::Ran(at) => Some(at),
                    _ => None,
                };
                if let Some(at) = refused {
                    ffi::duckdb_destroy_prepare(&mut prep);
                    if let Some(w) = watchdog.take() {
                        finish(&done, w);
                    }
                    failure = Some(Wrap::refused(at));
                    break;
                }
                if !crate::guard::is_allowed(kind) {
                    ffi::duckdb_destroy_prepare(&mut prep);
                    if let Some(w) = watchdog.take() {
                        finish(&done, w);
                    }
                    failure = Some(denied(
                        mine as usize + 1,
                        crate::guard::type_name(kind),
                        sql,
                    ));
                    break;
                }
                // REQ-9 `an_agent_object_cannot_shadow_a_host_object_or_builtin`. Only a CREATE
                // or an ALTER can put a name in a catalog, so only they run in a transaction of
                // their own, are inspected, and are undone if the name is taken; every other
                // statement pays one comparison. Checked after the statement rather than parsed
                // before it: the C API gives a prepared statement's type and not the name it
                // creates (`json_serialize_sql` serializes SELECT alone), so before execution the
                // name could only come from a second SQL parser of ours, the kind of check the
                // escaped-name spoofs are written against, and blind to the CREATE TYPE DuckDB
                // adds for a dynamic PIVOT. After it, the catalog names the object as DuckDB will
                // resolve it. Measured on DuckDB 1.5.5 through `benchy-mcp host`, instructions per
                // call (docs/history/PERFORMANCE-rounds.md, "The query path: no-shadowing"): a
                // small SELECT
                // 1.22 M before and 1.20 M after, the same within the noise; a CREATE TEMP TABLE
                // AS and its DROP 1.75 M → 4.1 M, the transaction and one listing of tables and
                // views; a CREATE VIEW and its DROP 5.55 M → 3.9 M, since a CREATE that makes a
                // table or view no longer lists types; a CREATE MACRO and its DROP 6.9 M →
                // 119 M, the listing of every function, ~30 ms here, paid only by a CREATE that
                // made no table, view or type.
                let create = kind == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_CREATE;
                let alter = kind == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_ALTER;
                if create || alter {
                    if let Err(e) = raw_exec(self.agent, "BEGIN TRANSACTION") {
                        ffi::duckdb_destroy_prepare(&mut prep);
                        let timed_out = watchdog.take().is_some_and(|w| finish(&done, w));
                        failure = Some(fault(self, timed_out, cap, mine, &e, sql));
                        break;
                    }
                }
                // REQ-10 `a_query_without_a_limit_reads_no_further_than_it_keeps`. A SELECT is
                // streamed and read only as far as its rows are kept (`Keep`); its result is
                // destroyed before the next statement, which ends its execution. Built whole,
                // a `SELECT *` over 10,000,000 rows cost the result before 2,000 rows of it were
                // kept: through MCP at `--memory 384MB`, 0.63 G instructions and 150 ms a call,
                // and the engine's peak resident memory 45 → 402–424 MiB, at a ceiling of
                // 622 MiB; streamed, 23 M and 7–18 ms, and 45 → 51–60 MiB
                // (`a_query_without_a_limit_holds_no_more_than_it_keeps`).
                //
                // The price is that an error DuckDB would raise only in rows past the ones kept
                // is never raised (`an_error_past_the_rows_kept_is_not_raised`): the answer is
                // the rows kept, a prefix of what the query makes, and says more exist. Reading
                // the rest within the deadline to find such an error was measured and not taken:
                // fetched and dropped, the same query cost 0.88 G and 178 ms a call, more than
                // building the result whole, for a query whose rows past the 2,000th the agent
                // never sees; and a query over more rows than the deadline can read would have
                // become a deadline error instead of an answer. An error in the rows that are
                // read, or the watchdog's interrupt while they are, still fails the statement
                // (`an_error_in_the_rows_kept_still_fails_the_query`,
                // `a_deadline_while_rows_are_read_is_the_deadline`). Every other kind runs to
                // completion as before: DML, a CREATE … AS, and the CREATE TYPE of a dynamic
                // PIVOT have their effects in full.
                let streamed = kind == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT
                    && arrow_reads(self.agent, prep);
                let mut r: ffi::duckdb_result = std::mem::zeroed();
                let ok = if streamed {
                    ffi::duckdb_execute_prepared_streaming(prep, &mut r)
                } else {
                    ffi::duckdb_execute_prepared(prep, &mut r)
                } == ffi::duckdb_state_DuckDBSuccess;
                if !ok {
                    let e = ffi::duckdb_result_error(&mut r);
                    let msg = if e.is_null() {
                        "execution failed".into()
                    } else {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    };
                    ffi::duckdb_destroy_result(&mut r);
                    ffi::duckdb_destroy_prepare(&mut prep);
                    // The watchdog stops first: an interrupt it fired into the ROLLBACK would
                    // leave the transaction open under every statement after it.
                    let timed_out = watchdog.take().is_some_and(|w| finish(&done, w));
                    if create || alter {
                        let _ = self.settle(false);
                    }
                    failure = Some(fault(self, timed_out, cap, mine, &msg, sql));
                    break;
                }
                let id = format!("{}{}", self.result_prefix, self.next_result_id);
                let result = collect(&mut r, &id, cap);
                // A streamed result stopped at the rows kept is destroyed here, before the next
                // statement: that is what ends its execution.
                ffi::duckdb_destroy_result(&mut r);
                ffi::duckdb_destroy_prepare(&mut prep);
                let result = match result {
                    Ok(result) => result,
                    // Only a streamed result fails here: its error, the watchdog's interrupt
                    // among them, comes with the fetch of a chunk rather than the execution.
                    // A SELECT is never in a transaction of benchy's, so there is none to settle.
                    Err(msg) => {
                        let timed_out = watchdog.take().is_some_and(|w| finish(&done, w));
                        failure = Some(fault(self, timed_out, cap, mine, &msg, sql));
                        break;
                    }
                };
                if create || alter {
                    // Under the watchdog's lock, so it cannot fire into the check or the COMMIT:
                    // an interrupted COMMIT that had committed was reported as stopped by the
                    // deadline while its table stayed (`a_batch_that_finishes_is_not_killed_\
                    // by_its_own_deadline`). The statement itself has finished; what is held up
                    // is benchy's own bounded work, a listing or two. A deadline that passes
                    // meanwhile stops the next statement, before it starts.
                    let opened = matches!(wrap, Wrap::Opened(_));
                    let made = {
                        let _hold = done.0.lock().unwrap_or_else(|e| e.into_inner());
                        self.inspect(alter, sql).and_then(|made| match made {
                            Made::Taken(why) => Ok(Err(why)),
                            // Not DuckDB's: undone below.
                            made if opened && made != Made::PivotType => Ok(Ok(made)),
                            made => self.settle(true).map(|()| Ok(made)),
                        })
                    };
                    match made {
                        Ok(Ok(made)) => {
                            if let (Wrap::Opened(at), false) = (wrap, made == Made::PivotType) {
                                // A TRANSACTION of the agent's own, before a CREATE of its own.
                                if let Some(w) = watchdog.take() {
                                    finish(&done, w);
                                }
                                let _ = self.settle(false);
                                failure = Some(Wrap::refused(at));
                                break;
                            }
                            if made == Made::PivotType {
                                pivoted = true;
                                if let Wrap::Opened(at) = wrap {
                                    wrap = Wrap::Typed(at);
                                }
                                if result.rows.is_empty() {
                                    added += 1;
                                    continue;
                                }
                            }
                        }
                        Ok(Err(why)) => {
                            if let Some(w) = watchdog.take() {
                                finish(&done, w);
                            }
                            let _ = self.settle(false);
                            failure = Some(WorkError::Sql {
                                index: mine as usize + 1,
                                message: why,
                                hint: None,
                            });
                            break;
                        }
                        Err(e) => {
                            let timed_out = watchdog.take().is_some_and(|w| finish(&done, w));
                            let _ = self.settle(false);
                            failure = Some(fault(self, timed_out, cap, mine, &e, sql));
                            break;
                        }
                    }
                }
                if let Wrap::Typed(at) = wrap {
                    wrap = Wrap::Ran(at);
                }
                self.next_result_id += 1;
                queries.push(kind == ffi::duckdb_statement_type_DUCKDB_STATEMENT_TYPE_SELECT);
                answers.push(mine as usize);
                out.push(result);
            }
            ffi::duckdb_destroy_extracted(&mut ex);
        }
        if let Some(w) = watchdog.take() {
            finish(&done, w);
        }
        if pivoted {
            self.drop_pivot_types();
        }

        // REQ-11, and deliberately after the deadline is settled: this costs extra queries, so
        // it must never be what pushes a batch past its budget. It gets what is left of the
        // deadline and no more than `NOTE_BUDGET` of it: the server's hard deadline counts from
        // the request, so time spent here past the batch's own deadline is time the engine is
        // killed in (`the_empty_result_note_stays_inside_the_deadline`).
        let unexplained =
            |r: &QueryResult, query: bool| query && r.rows.is_empty() && !r.columns.is_empty();
        if out.iter().zip(&queries).any(|(r, q)| unexplained(r, *q)) {
            let left = cap.deadline.saturating_sub(started.elapsed());
            if let Some(note) = self.why_empty(sql, left.min(NOTE_BUDGET)) {
                for (r, q) in out.iter_mut().zip(&queries) {
                    if unexplained(r, *q) {
                        r.note = Some(note.clone());
                    }
                }
            }
        }
        // Each result's statement, for a host's `page` (`a_page_says_the_sql_of_its_result`): the
        // agent's own text, never what DuckDB made of it, so a helper it calls is named and not
        // expanded. Only where the split at the semicolons finds the statements DuckDB found:
        // the split lexes as DuckDB does (`crate::guard::lex`), and no batch DuckDB parses has
        // been found where it differs, but one would otherwise give a result another's SQL.
        if !out.is_empty() {
            let written = crate::hint::statements(sql);
            if written.len() as u64 + added == extracted {
                for (r, i) in out.iter_mut().zip(&answers) {
                    r.sql = written
                        .get(*i)
                        .map(|w| crate::result::fit(w, crate::result::RETAIN_SQL));
                }
            }
        }
        for r in &out {
            crate::result::retain(&mut self.retained, r.clone());
        }
        match failure {
            None => Ok(out),
            Some(error) if out.is_empty() => Err(error),
            Some(error) => Err(WorkError::Partial {
                ran: out,
                error: Box::new(error),
            }),
        }
    }

    /// The tables the session was to have and could not load. The engine is told with each
    /// query, so a hint can say one is not available rather than that it never existed.
    pub fn set_unavailable(&mut self, names: Vec<String>) {
        self.unavailable = names;
    }

    /// REQ-18: put the operator's helpers on the bench from `file`, which a helpers job wrote,
    /// in place of any before them; `shown` is every helper, made or waiting.
    ///
    /// The file is attached `READ_ONLY`, so DuckDB refuses every DROP, ALTER, COMMENT or CREATE
    /// OR REPLACE of a helper, qualified or not (`an_agent_cannot_replace_or_shadow_a_helper`),
    /// as it refuses a write to a table. It is held to what a helpers job makes: the views and
    /// macros `shown` says were made, and nothing else, since its catalog goes on the agent's
    /// search path. Their names are taken from now on, the waiting ones' too, so nothing the agent
    /// makes and no table attached later is named like one. If anything fails, the bench is as
    /// it was.
    ///
    /// `reads` are the tables the helpers read, which are taken for the agent from now on too
    /// (`a_table_a_helper_reads_stays_taken_after_a_detach`), except one the agent already has an
    /// object of: a name is never taken after the agent has it (`Workspace::inspect`). That can
    /// only be a table a waiting helper reads besides the one it waits for, which DuckDB names
    /// only once that one is there; the host's attach of it is refused (`E_NAME`), and the
    /// helper, made only from the bench's own tables, waits on and reads nothing of the agent's.
    pub fn put_helpers(
        &mut self,
        file: &Path,
        shown: Vec<crate::helpers::Shown>,
        reads: &[String],
    ) -> Result<(), WorkError> {
        let own = self.agent_rows(AGENT_RELATIONS).map_err(WorkError::Open)?;
        let alias = format!("benchy_{}", self.next_alias);
        self.next_alias += 1;
        let quoted = file.to_string_lossy().replace('\'', "''");
        let attached = self
            .ingest
            .execute_batch(&format!("ATTACH '{quoted}' AS {alias} (READ_ONLY)"));
        // Unlinked whatever happened, as a table's file is: the directory is open to readers.
        for f in [file.to_path_buf(), file.with_extension("duckdb.wal")] {
            let _ = std::fs::remove_file(f);
        }
        attached.map_err(|e| WorkError::Open(format!("could not attach the helpers: {e}")))?;
        let undo = |ws: &Self, e: WorkError| {
            let _ = ws.ingest.execute_batch(&format!("DETACH {alias}"));
            Err(e)
        };
        if let Err(e) = self.helpers_hold(&alias, &shown) {
            return undo(self, e);
        }
        let old = self.helper_alias.replace(alias.clone());
        if let Err(e) = self.point_search_path() {
            self.helper_alias = old;
            let _ = self.point_search_path();
            return undo(self, e);
        }
        if let Some(o) = old {
            let _ = self.ingest.execute_batch(&format!("DETACH {o}"));
        }
        self.taken
            .helpers
            .extend(shown.iter().map(|s| s.name.to_ascii_lowercase()));
        self.taken.reads.extend(
            reads
                .iter()
                .filter(|r| !own.iter().any(|o| o.1.eq_ignore_ascii_case(r)))
                .map(|r| r.to_ascii_lowercase()),
        );
        self.helpers = shown;
        Ok(())
    }

    /// The helpers' catalog holds the views and macros `shown` says were made, each under its
    /// name and kind, and nothing else, and DuckDB will not write to it.
    fn helpers_hold(&self, alias: &str, shown: &[crate::helpers::Shown]) -> Result<(), WorkError> {
        let held = (|| -> Result<Vec<(String, String)>, duckdb::Error> {
            // benchy's own `benchy_<n>`
            let mut st = self.ingest.prepare(&format!(
                "SELECT 'view', lower(view_name) FROM system.main.duckdb_views() \
                   WHERE database_name = '{alias}' AND NOT internal \
                 UNION SELECT 'macro', lower(function_name) FROM system.main.duckdb_functions() \
                   WHERE database_name = '{alias}' \
                 UNION ALL SELECT 'table', table_name FROM system.main.duckdb_tables() \
                   WHERE database_name = '{alias}' \
                 UNION ALL SELECT 'sequence', sequence_name FROM system.main.duckdb_sequences() \
                   WHERE database_name = '{alias}' \
                 UNION ALL SELECT 'index', index_name FROM system.main.duckdb_indexes() \
                   WHERE database_name = '{alias}' \
                 UNION ALL SELECT 'type', type_name FROM system.main.duckdb_types() \
                   WHERE database_name = '{alias}' AND NOT internal \
                 UNION ALL SELECT 'schema', schema_name FROM system.main.duckdb_schemas() \
                   WHERE database_name = '{alias}' AND NOT internal"
            ))?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })()
        .map_err(|e| WorkError::Open(format!("could not inspect the helpers: {e}")))?;
        let mut want: Vec<(String, String)> = shown
            .iter()
            .filter(|s| s.waits.is_none())
            .map(|s| {
                let kind = if s.what == "view" { "view" } else { "macro" };
                (kind.to_string(), s.name.to_ascii_lowercase())
            })
            .collect();
        let mut held = held;
        want.sort();
        held.sort();
        if held != want {
            return Err(WorkError::Open(format!(
                "the helpers' file holds {held:?}, where the helpers made {want:?}"
            )));
        }
        if self
            .ingest
            .execute_batch(&format!(
                "CREATE TABLE {alias}.main.benchy_probe (x INTEGER)"
            ))
            .is_ok()
        {
            return Err(WorkError::Open("the helpers were attached writable".into()));
        }
        Ok(())
    }

    /// REQ-18 `a_write_to_a_helper_is_told_it_is_a_helper`: DuckDB's refusal of a write to the
    /// helpers' read-only catalog, said of a helper rather than of a loaded table, and the hint to
    /// make one's own; `None` for any other error. DuckDB names the catalog ("Cannot execute
    /// statement of type "DROP" on database "benchy_3" which is attached in read-only mode!",
    /// 1.5.5), which tells the helpers' from a table's; the helper is the one the statement
    /// names. The catalog's name is benchy's own and goes no further than here (REQ-15).
    fn helper_written(&self, message: &str, statement: Option<&str>) -> Option<(String, String)> {
        let alias = self.helper_alias.as_ref()?;
        let at = message.find(&format!(
            " on database \"{alias}\" which is attached in read-only mode!"
        ))?;
        let words: Vec<String> = statement
            .unwrap_or("")
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map(str::to_ascii_lowercase)
            .collect();
        let name = self
            .helpers
            .iter()
            .find(|s| words.contains(&s.name.to_ascii_lowercase()))
            .map(|s| s.name.clone());
        let message = format!("{} on a helper, which is read-only", &message[..at]);
        let hint = match name {
            Some(n) => format!(
                "`{n}` is a helper the operator provides, fixed for the session: it cannot be \
                 dropped, changed or replaced. Make your own under another name, such as `my_{n}`."
            ),
            None => "the operator's helpers are fixed for the session: they cannot be dropped, \
                     changed or replaced. Make your own under another name."
                .into(),
        };
        Some((message, hint))
    }

    /// REQ-18: what to tell an agent whose statement named a helper that waits for a table: what
    /// it waits for. `None` for any other error.
    fn waiting_helper(&self, message: &str) -> Option<String> {
        let (_, after) = message.split_once(" with name ")?;
        let name = after.split(" does not exist").next()?;
        let s = self
            .helpers
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name))?;
        let table = s.waits.as_ref()?;
        Some(format!(
            "`{}` is a helper the operator provides. It reads table `{table}`, which is not on \
             the bench yet, and works once it is.",
            s.name
        ))
    }

    /// What the CREATE or ALTER that just ran left in the agent's catalogs, asked inside its
    /// transaction so a taken name can still be undone (REQ-9
    /// `an_agent_object_cannot_shadow_a_host_object_or_builtin`).
    ///
    /// Every name of the namespace is checked, not only the new one: a name is never taken after
    /// the agent has it, because an attach under it is refused (`an_attached_name_is_taken_too`),
    /// a table a helper reads is not taken from an agent that has one of its name already
    /// (`a_table_the_agent_had_first_stays_its_own`), and the system views are fixed for the
    /// engine's life and checked from the agent's first CREATE, so any clash is this statement's. An ALTER can rename a table or view and make nothing
    /// else, so only those are listed after one.
    ///
    /// A dynamic PIVOT runs as a CREATE TYPE of the pivoted values and then the query; the type
    /// in the temporary catalog is the trace the first leaves (`a_dynamic_pivot_answers_once`).
    ///
    /// And no ENUM is kept (`the_agent_keeps_no_enum`): DuckDB holds an ENUM's values outside
    /// its memory budget, where neither `memory_limit` nor `duckdb_memory()` sees them. An ENUM
    /// type of 3,000,000 values left an engine with a 1 GB budget holding 698 MB that
    /// `duckdb_memory()` called 0; an outside red team measured 10,000,000 values at about 5 GB,
    /// and ENUM columns piling up batch after batch to 3.5–4.4 GB. So a type of the agent's own,
    /// of any kind, and a column of any table or view whose type holds an ENUM, nested or not,
    /// are undone as a taken name is, and so is an object whose SQL spells one: a DEFAULT, a
    /// generated column, a CHECK, an index, a view's or macro's body
    /// (`an_object_that_keeps_an_enum_in_its_sql_is_refused`). An ENUM in a query that keeps
    /// nothing is the agent's to use.
    fn inspect(&mut self, alter: bool, sql: &str) -> Result<Made, String> {
        if let Some((_, what, _)) = self.agent_rows(AGENT_ENUM_COLUMNS)?.pop() {
            return Ok(Made::Taken(enum_refused(&what)));
        }
        let relations = self.agent_rows(AGENT_RELATIONS)?;
        if let Some(name) = relations
            .iter()
            .map(|r| &r.1)
            .find(|n| self.loaded.iter().any(|t| t.name.eq_ignore_ascii_case(n)))
        {
            return Ok(Made::Taken(format!(
                "`{name}` is taken: a table on the bench has that name, and an object of yours \
                 named so would hide it from every query that names it. Nothing was {done}. \
                 Give yours another name, such as `my_{name}`.",
                done = if alter { "changed" } else { "created" }
            )));
        }
        // REQ-18 `an_agent_cannot_replace_or_shadow_a_helper`
        if let Some(name) = relations
            .iter()
            .map(|r| &r.1)
            .find(|n| self.taken.helpers.contains(&n.to_ascii_lowercase()))
        {
            return Ok(Made::Taken(format!(
                "`{name}` is taken: the operator provides a helper of that name, and an object of \
                 yours named so would stand in for it. Nothing was {done}. Give yours another \
                 name, such as `my_{name}`.",
                done = if alter { "changed" } else { "created" }
            )));
        }
        // REQ-18 `a_table_a_helper_reads_stays_taken_after_a_detach`
        if let Some(name) = relations
            .iter()
            .map(|r| &r.1)
            .find(|n| self.taken.reads.contains(&n.to_ascii_lowercase()))
        {
            return Ok(Made::Taken(format!(
                "`{name}` is taken: a helper the operator provides reads a table of that name, \
                 and an object of yours named so would be what the helper reads. Nothing was \
                 {done}. Give yours another name, such as `my_{name}`.",
                done = if alter { "changed" } else { "created" }
            )));
        }
        // REQ-9 `a_system_view_name_is_taken`
        if self.taken.system_views.is_none() {
            let views = self.agent_rows(SYSTEM_VIEWS)?;
            self.taken.system_views = Some(
                views
                    .into_iter()
                    .map(|r| r.1.to_ascii_lowercase())
                    .collect(),
            );
        }
        let system = self.taken.system_views.as_ref().expect("read above");
        if let Some(name) = relations
            .iter()
            .map(|r| &r.1)
            .find(|n| system.contains(&n.to_ascii_lowercase()))
        {
            return Ok(Made::Taken(format!(
                "`{name}` is taken: DuckDB has a system view of that name, and an object of yours \
                 named so would answer for it wherever it is named unqualified. Nothing was \
                 {done}. Give yours another name, such as `my_{name}`.",
                done = if alter { "changed" } else { "created" }
            )));
        }
        let fresh = |kind: &str| {
            relations
                .iter()
                .any(|r| r.2 == kind && !self.seen.contains(&r.0))
        };
        // Only where an ENUM may have been spelled: a new table, or a table an ALTER changed, and
        // a new view. A CREATE that made neither, in `main`, is read in full below.
        let (table, view) = (fresh("table"), fresh("view"));
        let mut spelled = Vec::new();
        if table || alter {
            spelled.push(ENUM_IN_TABLES);
        }
        if view {
            spelled.push(ENUM_IN_VIEWS);
        }
        if let Some(what) = self.enum_spelled(&spelled)? {
            return Ok(Made::Taken(enum_refused(&what)));
        }
        self.seen.extend(relations.iter().map(|r| r.0));
        if table || view || alter {
            return Ok(Made::Own);
        }
        let types = self.agent_rows(AGENT_TYPES)?;
        let made = types.iter().find(|t| !self.seen.contains(&t.0)).map(|t| {
            if t.2 == "temp" && pivots_type(&t.1, sql) {
                Made::PivotType
            } else {
                Made::Taken(enum_refused(&format!("the type {}", t.1)))
            }
        });
        self.seen.extend(types.iter().map(|t| t.0));
        if let Some(made) = made {
            return Ok(made);
        }
        // A CREATE that made no table or view in `main` and no type: one in another schema, an
        // index, a macro, or a table or view replaced under the oid it had.
        if let Some(what) = self.enum_spelled(&[ENUM_IN_TABLES, ENUM_IN_VIEWS, ENUM_IN_INDEXES])? {
            return Ok(Made::Taken(enum_refused(&what)));
        }
        if self.taken.builtin.is_none() {
            let names = |sql| -> Result<std::collections::HashSet<String>, String> {
                Ok(self
                    .agent_rows(sql)?
                    .into_iter()
                    .map(|r| r.1.to_ascii_lowercase())
                    .collect())
            };
            self.taken.builtin = Some(Builtin {
                functions: names(BUILTIN_FUNCTIONS)?,
                parser: names(PARSER_NAMES)?,
            });
        }
        let builtin = self.taken.builtin.as_ref().expect("read above");
        let functions = self.agent_rows(AGENT_FUNCTIONS)?;
        for (_, name, _) in functions.iter().filter(|f| f.0 == 1) {
            let folded = name.to_ascii_lowercase();
            if builtin.parser.contains(&folded) && !builtin.functions.contains(&folded) {
                return Ok(Made::Taken(format!(
                    "`{name}` is taken: DuckDB's SQL parser reads `{name}` itself, so a macro of \
                     yours named so would answer only where the name is quoted, and differently \
                     from DuckDB's. Nothing was created. Give yours another name, such as \
                     `my_{name}`."
                )));
            }
            let whose = if builtin.functions.contains(&folded) {
                "DuckDB has a built-in function"
            } else if self.taken.helpers.contains(&folded) {
                "the operator provides a helper"
            } else {
                continue;
            };
            return Ok(Made::Taken(format!(
                "`{name}` is taken: {whose} of that name, and a macro of yours named so would \
                 stand in for it in every query. Nothing was created. Give yours another name, \
                 such as `my_{name}`."
            )));
        }
        // After the names, so a macro under a taken name is told that first.
        if let Some((_, name, _)) = functions.iter().find(|f| f.2 == "true") {
            return Ok(Made::Taken(enum_refused(&format!("the macro {name}"))));
        }
        Ok(Made::Own)
    }

    /// The first of the agent's objects the `listings` find keeping an ENUM in its SQL, said as
    /// `enum_refused` says it (`an_object_that_keeps_an_enum_in_its_sql_is_refused`).
    fn enum_spelled(&self, listings: &[&CStr]) -> Result<Option<String>, String> {
        for listing in listings {
            if let Some((_, what, _)) = self.agent_rows(listing)?.pop() {
                return Ok(Some(what));
            }
        }
        Ok(None)
    }

    /// Rows of a catalog listing on the agent's connection, which alone sees its temporary
    /// catalog. A listing of one column comes back as the text of the second field; otherwise
    /// the first column is an oid and the second and third are text. Read with the legacy
    /// accessors, which are right for the BIGINT and VARCHAR these listings return.
    fn agent_rows(&self, sql: &CStr) -> Result<Vec<(i64, String, String)>, String> {
        unsafe {
            let mut r: ffi::duckdb_result = std::mem::zeroed();
            if ffi::duckdb_query(self.agent, sql.as_ptr(), &mut r)
                != ffi::duckdb_state_DuckDBSuccess
            {
                let e = ffi::duckdb_result_error(&mut r);
                let msg = if e.is_null() {
                    "the catalog could not be read".to_string()
                } else {
                    CStr::from_ptr(e).to_string_lossy().into_owned()
                };
                ffi::duckdb_destroy_result(&mut r);
                return Err(msg);
            }
            let cols = ffi::duckdb_column_count(&mut r);
            let text = |r: &mut ffi::duckdb_result, col: u64, row: u64| -> String {
                if col >= cols || ffi::duckdb_value_is_null(r, col, row) {
                    return String::new();
                }
                let v = ffi::duckdb_value_varchar(r, col, row);
                let s = CStr::from_ptr(v).to_string_lossy().into_owned();
                ffi::duckdb_free(v as *mut std::ffi::c_void);
                s
            };
            let rows = (0..ffi::duckdb_row_count(&mut r))
                .map(|row| {
                    if cols == 1 {
                        (0, text(&mut r, 0, row), String::new())
                    } else {
                        (
                            ffi::duckdb_value_int64(&mut r, 0, row),
                            text(&mut r, 1, row),
                            text(&mut r, 2, row),
                        )
                    }
                })
                .collect();
            ffi::duckdb_destroy_result(&mut r);
            Ok(rows)
        }
    }

    /// Drop the types named as a dynamic PIVOT's, once the batch that made them is over: DuckDB
    /// keeps them, and a pivot over a column of millions of values, again and again, would pile
    /// them up as the agent's own ENUMs would (`a_pivot_keeps_no_enum_after_its_batch`).
    ///
    /// One that cannot be dropped is held by something of the agent's, and stays, outside
    /// DuckDB's memory budget, for as long as the engine does: a host's review pinned
    /// 1,500,000 values a batch with a table whose DEFAULT cast to one, 799 MB idle after three,
    /// under the ceiling, while this swallowed the failed DROP. Nothing of the agent's may name
    /// one any more (`an_object_that_keeps_an_enum_in_its_sql_is_refused`), so this is the guard
    /// behind that guard: what cannot be dropped is said, and the engine is rebuilt
    /// (`a_pivot_type_that_cannot_be_dropped_spoils_the_engine`).
    fn drop_pivot_types(&mut self) {
        // The DROP's own error says what failed, so the types are listed once: listing them
        // again after cost a dynamic PIVOT about 6 M instructions more
        // (docs/history/PERFORMANCE-rounds.md, "The query path: no-shadowing").
        let left = match self.agent_rows(PIVOT_TYPES) {
            Ok(types) => types
                .into_iter()
                .filter(|(_, name, _)| {
                    let sql = format!(
                        "DROP TYPE IF EXISTS temp.main.\"{}\"",
                        name.replace('"', "\"\"")
                    );
                    unsafe { raw_exec(self.agent, &sql) }.is_err()
                })
                .map(|t| t.1)
                .collect::<Vec<_>>(),
            Err(e) => vec![format!("(the types could not be listed: {e})")],
        };
        if !left.is_empty() {
            self.spoiled = Some(format!(
                "a dynamic PIVOT's type could not be dropped after the batch, held by an object \
                 of yours: {}",
                left.join(", ")
            ));
        }
    }

    /// Why this engine must not go on, if a batch left it holding what it cannot let go: the
    /// server rebuilds it (`a_pivot_type_that_cannot_be_dropped_spoils_the_engine`).
    pub fn spoiled(&self) -> Option<&str> {
        self.spoiled.as_deref()
    }

    /// End the transaction a CREATE or ALTER ran in: keep what it made, or undo it.
    fn settle(&self, keep: bool) -> Result<(), String> {
        unsafe { raw_exec(self.agent, if keep { "COMMIT" } else { "ROLLBACK" }) }
    }

    /// Continue numbering where a previous workspace stopped. An id names one result for the life
    /// of the server; a rebuilt workspace must never hand out an id its predecessor used, or an
    /// old id would silently resolve to a different answer.
    pub fn set_first_result_id(&mut self, n: u64) {
        self.next_result_id = n.max(1);
    }

    /// Start every result id with `prefix`, a session's own, so no two sessions issue the same id
    /// (`two_sessions_never_issue_the_same_result_id`).
    pub fn set_result_prefix(&mut self, prefix: &str) {
        self.result_prefix = prefix.to_string();
    }

    /// The statement a retained result came from, as the agent wrote it, when it could be told
    /// apart in its batch.
    pub fn sql_of(&self, result_id: &str) -> Option<&str> {
        self.retained
            .iter()
            .find(|r| r.result_id == result_id)
            .and_then(|r| r.sql.as_deref())
    }

    /// One page of a retained result, or `None` if this workspace never produced that id.
    pub fn page(
        &self,
        result_id: &str,
        offset: usize,
        caps: &crate::result::Caps,
    ) -> Option<String> {
        self.retained
            .iter()
            .find(|r| r.result_id == result_id)
            .map(|r| crate::result::render_one(r, offset, caps))
    }

    /// REQ-11: why an empty result is empty, when we can tell.
    ///
    /// Zero rows is ambiguous — a filter value the data has never heard of looks exactly like
    /// data that genuinely is not there — and an agent that cannot tell them apart spends a step
    /// guessing. Saying what the column holds lets it retry with the right spelling at once.
    ///
    /// Deliberately narrow, because a note on every empty result would be noise in a payload
    /// that repeats:
    ///
    /// - only when the query carries a **string literal**, which is the shape of the mistake;
    /// - only for a loaded table and a text column the query actually **names**;
    /// - only when the column is small enough to enumerate honestly;
    /// - and only when one of those literals is **not** among the values. A correct filter over
    ///   genuinely absent data stays quiet.
    ///
    /// And only within `budget`: past it the query reading a column is interrupted and no other
    /// column is read (`the_empty_result_note_stays_inside_the_deadline`).
    fn why_empty(&self, sql: &str, budget: std::time::Duration) -> Option<String> {
        let wanted = crate::hint::literals(sql);
        if wanted.is_empty() || budget.is_zero() {
            return None;
        }
        let until = std::time::Instant::now() + budget;
        // Set at the first column read, so a note that reads nothing starts no thread.
        let mut alarm = None;
        let mut notes = Vec::new();
        'tables: for t in &self.loaded {
            if !crate::hint::mentions(sql, &t.name) {
                continue;
            }
            for (col, ty) in &t.columns {
                if notes.len() >= 2 {
                    break;
                }
                if !ty.contains("VARCHAR") || !crate::hint::mentions(sql, col) {
                    continue;
                }
                if std::time::Instant::now() >= until {
                    break 'tables;
                }
                alarm.get_or_insert_with(|| Alarm::set(self.ingest.interrupt_handle(), until));
                let Some(values) = self.distinct(&t.name, col, NOTE_VALUES + 1) else {
                    continue;
                };
                if values.len() > NOTE_VALUES {
                    continue; // too many to enumerate without becoming the noise we avoid
                }
                // Fire only when *nothing* the query quoted is in this column. Comparison is
                // exact, because SQL string equality is: `'Lab'` against a column holding `lab`
                // is precisely the mistake this exists to reveal, and a case-insensitive check
                // would call it a match and stay silent.
                //
                // "no literal matches" rather than "some literal is missing", because a query
                // that filters correctly on one column and also carries a date literal would
                // otherwise get a note about a column that was never the problem.
                let unknown = !wanted.iter().any(|w| values.contains(w));
                if unknown && !values.is_empty() {
                    notes.push(format!("{}.{} holds: {}", t.name, col, values.join(", ")));
                }
            }
        }
        drop(alarm);
        if notes.is_empty() {
            return None;
        }
        Some(format!(
            "No rows matched. {}. If you filtered on a value that is not in that list, that is \
             why — the column uses the spelling above.",
            notes.join("; ")
        ))
    }

    /// Distinct values of a loaded column, capped at `limit`. `None` if it cannot be read at
    /// all, or was interrupted.
    ///
    /// The head of the column first: a column with `limit` distinct values among its first
    /// [`NOTE_HEAD_ROWS`] has too many for a note, and reading the rest only to sort them cost a
    /// hash aggregate of every value in the table. A column that can give a note is still read
    /// whole, within the note's budget.
    fn distinct(&self, table: &str, column: &str, limit: usize) -> Option<Vec<String>> {
        let column = format!("\"{}\"", column.replace('"', "\"\""));
        let from = format!(
            "{}.main.\"{}\"",
            self.catalog_of(table),
            table.replace('"', "\"\"")
        );
        let read = |q: &str| -> Option<Vec<String>> {
            let mut st = self.ingest.prepare(q).ok()?;
            let rows = st.query_map([], |r| r.get::<_, String>(0)).ok()?;
            rows.collect::<Result<Vec<_>, _>>().ok()
        };
        let head = read(&format!(
            "SELECT DISTINCT {column} FROM (SELECT {column} FROM {from} \
             WHERE {column} IS NOT NULL LIMIT {NOTE_HEAD_ROWS}) LIMIT {limit}"
        ))?;
        if head.len() >= limit {
            return Some(head);
        }
        let values = read(&format!(
            "SELECT DISTINCT {column} FROM {from} WHERE {column} IS NOT NULL \
             ORDER BY 1 LIMIT {limit}"
        ))?;
        // Quoted back to the agent in the note, so each is cut: a value can be any length.
        Some(
            values
                .into_iter()
                .map(|v| crate::result::cut(v, 64))
                .collect(),
        )
    }
}

/// Interrupts what runs on a connection once `until` has passed, and again every few
/// milliseconds after, until it is dropped: DuckDB clears a pending interrupt when the next
/// statement starts, as the batch's watchdog found. It fires under the lock the drop takes, so
/// it never fires once the drop has returned.
struct Alarm {
    done: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Alarm {
    fn set(handle: std::sync::Arc<duckdb::InterruptHandle>, until: std::time::Instant) -> Self {
        const REFIRE: std::time::Duration = std::time::Duration::from_millis(5);
        let done = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let flag = done.clone();
        let thread = std::thread::spawn(move || {
            let (lock, cv) = &*flag;
            let mut finished = lock.lock().unwrap_or_else(|e| e.into_inner());
            while !*finished {
                let left = until.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    handle.interrupt();
                }
                let wait = if left.is_zero() { REFIRE } else { left };
                finished = cv
                    .wait_timeout(finished, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        });
        Self {
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for Alarm {
    fn drop(&mut self) {
        let (lock, cv) = &*self.done;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// REQ-10: bounded as measured on what actually goes out, with truncation explicit.
/// Read a result as Arrow, falling back to the legacy per-value reader if the conversion cannot
/// be set up.
///
/// The legacy `duckdb_value_*` family is deprecated **and wrong for this product**: it returns
/// nothing at all — not an error — for `TIMESTAMP WITH TIME ZONE`, which is what a time-series
/// CSV loads as, and for every nested type. The chunk API cannot be mixed with it ("if this
/// function is used, none of the other result functions can be used and vice versa"), so this is
/// one path or the other, decided before the first chunk is fetched.
///
/// Arrow is also the direction REQ-6 already points, and arrow-rs renders every type correctly
/// rather than us hand-decoding two dozen physical layouts.
///
/// `Err` is the error a streamed result raised as its rows were read (`Workspace::work`).
unsafe fn collect(
    r: &mut ffi::duckdb_result,
    id: &str,
    cap: &crate::result::Caps,
) -> Result<QueryResult, String> {
    match collect_arrow(r, id, cap) {
        Some(q) => q,
        // Never given a stream: a statement is streamed only once `arrow_reads` says this
        // reader can read it. On DuckDB 1.5.5 the legacy reader builds a stream whole outside
        // any `try`, and an error in building it is a C++ exception through the C API, which
        // aborts the process: a test that handed it a stream that failed at row 2,500,000 was
        // killed by SIGABRT ("Rust cannot catch foreign exceptions").
        None if ffi::duckdb_result_is_streaming(*r) => {
            Err("the result could not be read as Arrow".into())
        }
        // Nothing has been fetched yet when this returns `None`, so the legacy path is still
        // usable. It renders less, but it renders.
        None => Ok(collect_legacy(r, id, cap)),
    }
}

/// Whether a prepared statement's result can be read as Arrow, decided from its column types
/// before it runs, as `collect_arrow` decides from the result's: only such a statement is
/// streamed, so the legacy reader is never handed a stream (`collect`). No SQL is known for
/// which it cannot, as no SQL is known that sends a result to the legacy reader.
unsafe fn arrow_reads(con: ffi::duckdb_connection, prep: ffi::duckdb_prepared_statement) -> bool {
    use duckdb::arrow::ffi::FFI_ArrowSchema;
    let cols = ffi::duckdb_prepared_statement_column_count(prep) as usize;
    if cols == 0 {
        return false;
    }
    let owned: Vec<CString> = (0..cols)
        .map(|c| CString::new(format!("c{c}")).unwrap())
        .collect();
    let mut names: Vec<*const std::os::raw::c_char> = owned.iter().map(|n| n.as_ptr()).collect();
    let mut types: Vec<ffi::duckdb_logical_type> = (0..cols)
        .map(|c| ffi::duckdb_prepared_statement_column_logical_type(prep, c as u64))
        .collect();
    let mut opts: ffi::duckdb_arrow_options = ptr::null_mut();
    ffi::duckdb_connection_get_arrow_options(con, &mut opts);
    let mut schema = FFI_ArrowSchema::empty();
    let failed = arrow_error(ffi::duckdb_to_arrow_schema(
        opts,
        types.as_mut_ptr(),
        names.as_mut_ptr(),
        cols as u64,
        &mut schema as *mut FFI_ArrowSchema as *mut ffi::ArrowSchema,
    ));
    for t in &mut types {
        ffi::duckdb_destroy_logical_type(t);
    }
    ffi::duckdb_destroy_arrow_options(&mut opts);
    failed.is_none()
}

/// The error a result holds, if it holds one. A streamed result's fetch answers an error, the
/// watchdog's interrupt among them, with no chunk, as it answers the end of the rows; this is
/// what tells them apart (`an_error_in_the_rows_kept_still_fails_the_query`).
unsafe fn result_error(r: &mut ffi::duckdb_result) -> Option<String> {
    let e = ffi::duckdb_result_error(r);
    (!e.is_null()).then(|| CStr::from_ptr(e).to_string_lossy().into_owned())
}

/// A value this reader cannot render faithfully. Saying so, with the fix, is the only honest
/// output: an empty cell is indistinguishable from missing data, and a wrong one is worse.
fn not_rendered(what: &str) -> Cell {
    Cell::Text(format!(
        "⟨{what} not rendered — add ::VARCHAR to this column⟩"
    ))
}

/// DuckDB types whose Arrow form loses their meaning: BIT and BIGNUM arrive as DuckDB's own
/// internal bytes, and TIME WITH TIME ZONE arrives without its offset. Formatting those gives a
/// confident wrong value — `04f5` for the bits `0101` — so they are marked instead.
fn unfaithful_in_arrow(t: ffi::duckdb_type) -> Option<&'static str> {
    match t {
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIT => Some("BIT"),
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGNUM => Some("BIGNUM"),
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIME_TZ => Some("TIME WITH TIME ZONE"),
        _ => None,
    }
}

unsafe fn collect_legacy(
    r: &mut ffi::duckdb_result,
    id: &str,
    cap: &crate::result::Caps,
) -> QueryResult {
    let cols = ffi::duckdb_column_count(r) as usize;
    // Never a stream (`collect`): of a stream, DuckDB 1.5.5 counts 0 rows.
    let rows = ffi::duckdb_row_count(r) as usize;
    let mut names = Vec::with_capacity(cols);
    for c in 0..cols {
        let n = ffi::duckdb_column_name(r, c as u64);
        names.push(if n.is_null() {
            format!("col{c}")
        } else {
            CStr::from_ptr(n).to_string_lossy().into_owned()
        });
    }
    // Read once per column, not per cell: the reader below branches on it for every value.
    let types: Vec<ffi::duckdb_type> = (0..cols)
        .map(|c| ffi::duckdb_column_type(r, c as u64))
        .collect();

    let mut keep = Keep::new();
    for row in 0..rows {
        let mut cells = Vec::with_capacity(cols);
        for (col, &ty) in types.iter().enumerate() {
            let cell = if ffi::duckdb_value_is_null(r, col as u64, row as u64) {
                Cell::Null
            } else {
                let v = ffi::duckdb_value_varchar(r, col as u64, row as u64);
                if v.is_null() {
                    // The value is *not* null — we just checked — so this API cannot render it.
                    // TIMESTAMP WITH TIME ZONE and every nested type land here, and no typed
                    // accessor does better: `duckdb_value_timestamp` and `duckdb_value_int64`
                    // both return 0 for a TIMESTAMPTZ. The Arrow path is what renders these.
                    not_rendered(type_name(ty))
                } else {
                    let s = CStr::from_ptr(v).to_string_lossy().into_owned();
                    ffi::duckdb_free(v as *mut std::ffi::c_void);
                    keep.value(s, cap.max_cell_bytes)
                }
            };
            cells.push(cell);
        }
        if !keep.offer(cells) {
            break;
        }
    }
    QueryResult {
        result_id: id.to_string(),
        columns: names,
        rows: keep.rows,
        more: keep.more,
        partial: None,
        note: None,
        cut_values: keep.cut,
        sql: None,
        cell_cap: cap.max_cell_bytes,
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        unsafe {
            ffi::duckdb_disconnect(&mut self.agent);
            ffi::duckdb_close(&mut self.db);
        }
        // nothing outlives the session: REQ says the bench keeps nothing
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// REQ-4 `the_catalogue_is_bounded`. This is the one thing in the agent's context on *every*
/// turn, so it is capped, deterministically ordered, and carries **no path, host or credential**.
///
/// REQ-14: under each table, the shape its loader found — rendered from the stored profile, never
/// computed here, so describing a table costs nothing however large it is
/// (`describe_does_not_scan_the_table`). Tables and their columns come first; profiles share what
/// is left of the budget, in order, and whatever does not fit says so.
impl Workspace {
    pub fn describe(&self, budget_bytes: usize) -> String {
        self.describe_with(budget_bytes, &[])
    }

    /// `unavailable` are tables the session was to have and could not load: named, and nothing
    /// more — the reason is the operator's (REQ-15). Their line is kept room for before any
    /// profile, so the agent always learns not to go looking for them.
    pub fn describe_with(&self, budget_bytes: usize, unavailable: &[String]) -> String {
        let gone = if unavailable.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = unavailable
                .iter()
                .map(|n| format!("`{}`", crate::result::cut(n.clone(), 64)))
                .collect();
            format!(
                "\nNot loaded, and not available this session: {}. The operator has the \
                 reason.\n",
                names.join(", ")
            )
        };
        let mut s = String::new();
        s.push_str(
            "Tables on the bench. SQL is DuckDB. The data is already loaded — file and \
                    network functions are unavailable and will be refused.\n\n",
        );
        /// Kept back for the line that says what is not shown.
        const POINTER: usize = 200;
        // Each shown table's own lines, and its index for the profile that goes under them.
        let mut parts: Vec<(usize, String)> = Vec::new();
        let mut tail = String::new();
        let mut used = s.len();
        for (shown, t) in self.loaded.iter().enumerate() {
            let head = format!("{} ({} rows)\n  ", t.name, t.rows);
            // A column name comes from a file header and can be any length.
            let cols: Vec<String> = t
                .columns
                .iter()
                .map(|(n, ty)| format!("{} {}", crate::profile::ident(n), short_type(ty)))
                .collect();
            let room = budget_bytes.saturating_sub(used + head.len() + 1 + POINTER);
            let all = cols.join(", ");
            if all.len() <= room {
                let part = format!("{head}{all}\n");
                used += part.len();
                parts.push((shown, part));
                continue;
            }
            if shown > 0 {
                tail = format!(
                    "\n… {} more table(s) not shown. `SELECT table_name FROM \
                     information_schema.tables` lists them all; `DESCRIBE <table>` shows one.\n",
                    self.loaded.len() - shown
                );
                break;
            }
            // The first table always shows — as many of its columns as fit, and where the rest are.
            let mut fitted = String::new();
            for (i, c) in cols.iter().enumerate() {
                let item = if i == 0 { c.clone() } else { format!(", {c}") };
                if fitted.len() + item.len() > room {
                    fitted.push_str(&format!(
                        " … (+{} more columns; `DESCRIBE {}` lists them)",
                        cols.len() - i,
                        t.name
                    ));
                    break;
                }
                fitted.push_str(&item);
            }
            let part = format!("{head}{fitted}\n");
            used += part.len();
            parts.push((shown, part));
        }

        // REQ-18: the operator's helpers come next, before any profile, since an agent that does
        // not know of one writes its SQL out longhand. Each one line, as many as fit.
        let shown_parts: usize = parts.iter().map(|(_, p)| crate::result::wire_len(p)).sum();
        let helpers = crate::helpers::render(
            &self.helpers,
            budget_bytes.saturating_sub(
                crate::result::wire_len(&s)
                    + shown_parts
                    + crate::result::wire_len(&tail)
                    + POINTER
                    + crate::result::wire_len(&gone),
            ),
        );
        // What is left goes to the profiles, first table first. Measured as the client receives
        // it: a profile is many short lines, and each newline is two bytes once escaped.
        let mut left = budget_bytes.saturating_sub(
            crate::result::wire_len(&s)
                + POINTER
                + crate::result::wire_len(&gone)
                + crate::result::wire_len(&helpers),
        );
        left = left.saturating_sub(shown_parts);
        // Every table's own lines before any table's notes about single series
        // (`every_tables_shape_comes_before_any_tables_notes`).
        let profiles: Vec<&crate::profile::Profile> = parts
            .iter()
            .filter_map(|(i, _)| self.loaded[*i].profile.as_ref())
            .collect();
        let mut rendered = crate::profile::render_all(&profiles, left).into_iter();
        let mut unshown = 0;
        for (i, part) in &parts {
            s.push_str(part);
            let Some(p) = &self.loaded[*i].profile else {
                continue;
            };
            match rendered.next().flatten() {
                Some(r) => s.push_str(&r),
                None if p.lines.is_empty() => {}
                None => unshown += 1,
            }
        }
        s.push_str(&tail);
        s.push_str(&helpers);
        s.push_str(&gone);
        if unshown > 0 {
            s.push_str(&format!(
                "\n(The shape of {unshown} table(s) is not shown: over the {} budget.)\n",
                crate::tools::name("describe")
            ));
        }
        // REQ-14: the agent's own tables are named, not profiled — they change with every
        // statement, and the agent made them, so it can ask. Names only, and only if they fit.
        let mine = self.own_tables();
        if !mine.is_empty() {
            let line = format!(
                "\nYour own tables (not profiled; `DESCRIBE <name>` shows one): {}\n",
                mine.join(", ")
            );
            if crate::result::wire_len(&s) + crate::result::wire_len(&line) <= budget_bytes {
                s.push_str(&line);
            } else {
                s.push_str(&format!(
                    "\nYou have {} table(s) of your own; `SELECT table_name FROM \
                     information_schema.tables` lists them.\n",
                    mine.len()
                ));
            }
        }
        s
    }

    /// The tables and views the agent made, by name.
    fn own_tables(&self) -> Vec<String> {
        let sql = "SELECT table_name FROM system.main.duckdb_tables() \
                   WHERE database_name = 'memory' AND NOT internal \
                   UNION SELECT view_name FROM system.main.duckdb_views() \
                   WHERE database_name = 'memory' AND NOT internal ORDER BY 1";
        (|| -> Result<Vec<String>, duckdb::Error> {
            let mut st = self.ingest.prepare(sql)?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|r| r.map(|n| crate::result::cut(n, 64))).collect()
        })()
        .unwrap_or_default()
    }
}

/// Enough of a name to tell the agent what it is looking at. Only the nested types can reach
/// this — everything else renders through the varchar path.
fn type_name(t: ffi::duckdb_type) -> &'static str {
    match t {
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_TIMESTAMP_TZ => "TIMESTAMP WITH TIME ZONE",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_LIST => "LIST",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_STRUCT => "STRUCT",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_MAP => "MAP",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_ARRAY => "ARRAY",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_UNION => "UNION",
        ffi::DUCKDB_TYPE_DUCKDB_TYPE_ENUM => "ENUM",
        _ => "this type",
    }
}

/// A column's type as `describe` names it: DuckDB's own name, lower-cased, and `timestamptz`,
/// DuckDB's alias, for the long `TIMESTAMP WITH TIME ZONE` — the agent pays for every character
/// on every turn. Never a friendlier name: in DuckDB `float` and `int` are 32-bit, and the bench's
/// columns are `DOUBLE` and `BIGINT` (REQ-15), so a friendlier name is a wrong one
/// (`describe_names_duckdbs_own_types`). A nested or parameterised type is left as DuckDB
/// spells it, since its field names are case-sensitive.
fn short_type(t: &str) -> String {
    match t {
        "TIMESTAMP WITH TIME ZONE" => "timestamptz".into(),
        _ if t
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') =>
        {
            t.to_ascii_lowercase()
        }
        _ if t.starts_with("DECIMAL(") => t.to_ascii_lowercase(),
        other => other.into(),
    }
}

/// Errors from the Arrow C interface, consumed and freed.
unsafe fn arrow_error(e: ffi::duckdb_error_data) -> Option<String> {
    if e.is_null() {
        return None;
    }
    let out = if ffi::duckdb_error_data_has_error(e) {
        let m = ffi::duckdb_error_data_message(e);
        Some(if m.is_null() {
            "arrow conversion failed".to_string()
        } else {
            CStr::from_ptr(m).to_string_lossy().into_owned()
        })
    } else {
        None
    };
    let mut e = e;
    ffi::duckdb_destroy_error_data(&mut e);
    out
}

/// `None` means the conversion could not be *set up* — and, critically, that no chunk has been
/// fetched, so the caller may still use the legacy reader. Once a chunk is fetched this commits.
unsafe fn collect_arrow(
    r: &mut ffi::duckdb_result,
    id: &str,
    cap: &crate::result::Caps,
) -> Option<Result<QueryResult, String>> {
    use duckdb::arrow::array::{Array, StructArray};
    use duckdb::arrow::ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema};
    use duckdb::arrow::util::display::{ArrayFormatter, ArrayFormatterFactory, FormatOptions};

    let cols = ffi::duckdb_column_count(r) as usize;
    if cols == 0 {
        return None;
    }

    let mut names = Vec::with_capacity(cols);
    let mut owned_names = Vec::with_capacity(cols);
    let mut types: Vec<ffi::duckdb_logical_type> = Vec::with_capacity(cols);
    let mut unfaithful: Vec<Option<&'static str>> = Vec::with_capacity(cols);
    for c in 0..cols {
        let n = ffi::duckdb_column_name(r, c as u64);
        let owned = if n.is_null() {
            CString::new(format!("col{c}")).unwrap()
        } else {
            CString::new(CStr::from_ptr(n).to_bytes()).unwrap_or_default()
        };
        names.push(owned.as_ptr());
        owned_names.push(owned);
        types.push(ffi::duckdb_column_logical_type(r, c as u64));
        unfaithful.push(unfaithful_in_arrow(ffi::duckdb_column_type(r, c as u64)));
    }

    let opts = ffi::duckdb_result_get_arrow_options(r);
    let mut schema = FFI_ArrowSchema::empty();
    let setup = arrow_error(ffi::duckdb_to_arrow_schema(
        opts,
        types.as_mut_ptr(),
        names.as_mut_ptr(),
        cols as u64,
        &mut schema as *mut FFI_ArrowSchema as *mut ffi::ArrowSchema,
    ));
    for t in &mut types {
        ffi::duckdb_destroy_logical_type(t);
    }
    if setup.is_some() {
        let mut opts = opts;
        ffi::duckdb_destroy_arrow_options(&mut opts);
        return None; // nothing fetched: the caller can still fall back
    }

    // Match DuckDB's own rendering so a value the agent reads is a value it can paste back into
    // a filter.
    let fmt = FormatOptions::default()
        .with_display_error(true)
        .with_timestamp_format(Some("%Y-%m-%d %H:%M:%S%.f"))
        .with_timestamp_tz_format(Some("%Y-%m-%d %H:%M:%S%.f%:z"))
        .with_formatter_factory(Some(&TIMES));

    let mut keep = Keep::new();
    let mut partial = None;
    let mut column_names: Vec<String> = owned_names
        .iter()
        .map(|c| c.to_string_lossy().into())
        .collect();

    'chunks: loop {
        let mut chunk = ffi::duckdb_fetch_chunk(*r);
        if chunk.is_null() {
            if let Some(e) = result_error(r) {
                let mut opts = opts;
                ffi::duckdb_destroy_arrow_options(&mut opts);
                return Some(Err(e));
            }
            break;
        }
        let mut array = FFI_ArrowArray::empty();
        let failed = arrow_error(ffi::duckdb_data_chunk_to_arrow(
            opts,
            chunk,
            &mut array as *mut FFI_ArrowArray as *mut ffi::ArrowArray,
        ));
        ffi::duckdb_destroy_data_chunk(&mut chunk);
        // Committed to this path now, so a failure cannot fall back. What was read so far is
        // kept, and the result is marked partial: it is never presented as complete.
        if let Some(e) = failed {
            partial = Some(format!("a chunk could not be converted: {e}"));
            break;
        }
        let Ok(data) = from_ffi(array, &schema) else {
            partial = Some("a chunk could not be read".into());
            break;
        };
        let batch = StructArray::from(data);
        if batch.num_columns() != column_names.len() {
            column_names = (0..batch.num_columns())
                .map(|i| format!("col{i}"))
                .collect();
        }
        // A date or a timestamp at the top level is [`Times`]'s too: arrow-rs asks the factory
        // only for what is inside a list, a struct or a map.
        let formatters: Vec<_> = batch
            .columns()
            .iter()
            .map(|c| {
                TIMES
                    .create_array_formatter(c.as_ref(), &fmt, None)
                    .transpose()
                    .unwrap_or_else(|| ArrayFormatter::try_new(c.as_ref(), &fmt))
            })
            .collect();

        for row in 0..batch.len() {
            let mut cells = Vec::with_capacity(batch.num_columns());
            for (col, f) in formatters.iter().enumerate() {
                let cell = if batch.column(col).is_null(row) {
                    Cell::Null
                } else if let Some(what) = unfaithful.get(col).copied().flatten() {
                    not_rendered(what)
                } else {
                    match f {
                        Ok(f) => keep.value(f.value(row).to_string(), cap.max_cell_bytes),
                        Err(_) => not_rendered(&batch.column(col).data_type().to_string()),
                    }
                };
                cells.push(cell);
            }
            if !keep.offer(cells) {
                break 'chunks;
            }
        }
    }

    let mut opts = opts;
    ffi::duckdb_destroy_arrow_options(&mut opts);

    Some(Ok(QueryResult {
        result_id: id.to_string(),
        columns: column_names,
        rows: keep.rows,
        more: keep.more,
        partial,
        note: None,
        cut_values: keep.cut,
        sql: None,
        cell_cap: cap.max_cell_bytes,
    }))
}

/// Dates and timestamps, rendered as DuckDB's own `::VARCHAR` renders them, at the top level of
/// a result and inside a list, a struct or a map.
///
/// - **`infinity` and `-infinity`.** DuckDB holds them as the largest value of the type and its
///   negation (`i32::MAX` days, `i64::MAX` ticks at every unit), which Arrow carries as plain
///   numbers. Formatted as numbers, a TIMESTAMPTZ's was the year 294247, and a TIMESTAMP's or a
///   DATE's an `ERROR: Cast error` cell in a result marked complete
///   (`an_infinite_time_renders_as_duckdb_writes_it`).
/// - **A UTC instant.** DuckDB hands us `Timestamp(_, Some("Etc/UTC"))`, and arrow-rs cannot
///   format a named zone without chrono-tz; pulling in a timezone database to print `+00` on
///   data we have already pinned to UTC (verified at seal) would be a strange trade. These are
///   rendered from the raw instant instead, which also keeps the output byte-identical to
///   DuckDB's `::VARCHAR`. A zone that is ever *not* UTC takes arrow-rs's own path, so a wrong
///   offset is never invented.
///
/// Anything else, and a finite zone-less value, is arrow-rs's own formatting.
#[derive(Debug)]
struct Times;

static TIMES: Times = Times;

/// The raw values of a column [`Times`] renders: days, or a timestamp's ticks, with the unit
/// that makes them microseconds when they are a UTC instant.
enum Ticks<'a> {
    Days(&'a [i32]),
    Time(&'a [i64], Option<duckdb::arrow::datatypes::TimeUnit>),
}

struct TimeFormat<'a> {
    array: &'a dyn duckdb::arrow::array::Array,
    ticks: Ticks<'a>,
    /// arrow-rs's formatter, for a finite value that is not a UTC instant.
    finite: Option<duckdb::arrow::util::display::ArrayFormatter<'a>>,
    null: &'a str,
}

impl duckdb::arrow::util::display::ArrayFormatterFactory for Times {
    fn create_array_formatter<'a>(
        &self,
        array: &'a dyn duckdb::arrow::array::Array,
        options: &duckdb::arrow::util::display::FormatOptions<'a>,
        _field: Option<&'a duckdb::arrow::datatypes::Field>,
    ) -> Result<
        Option<duckdb::arrow::util::display::ArrayFormatter<'a>>,
        duckdb::arrow::error::ArrowError,
    > {
        use duckdb::arrow::array::AsArray;
        use duckdb::arrow::datatypes::{
            DataType, Date32Type, TimeUnit, TimestampMicrosecondType, TimestampMillisecondType,
            TimestampNanosecondType, TimestampSecondType,
        };
        use duckdb::arrow::util::display::ArrayFormatter;
        let ticks = match array.data_type() {
            DataType::Date32 => Ticks::Days(array.as_primitive::<Date32Type>().values()),
            DataType::Timestamp(unit, zone) => {
                let raw: &[i64] = match unit {
                    TimeUnit::Second => array.as_primitive::<TimestampSecondType>().values(),
                    TimeUnit::Millisecond => {
                        array.as_primitive::<TimestampMillisecondType>().values()
                    }
                    TimeUnit::Microsecond => {
                        array.as_primitive::<TimestampMicrosecondType>().values()
                    }
                    TimeUnit::Nanosecond => {
                        array.as_primitive::<TimestampNanosecondType>().values()
                    }
                };
                let utc = zone.as_deref().is_some_and(|z| {
                    matches!(z, "UTC" | "Etc/UTC" | "utc" | "Z" | "+00:00" | "00:00")
                });
                Ticks::Time(raw, utc.then_some(*unit))
            }
            _ => return Ok(None),
        };
        let finite = match ticks {
            Ticks::Time(_, Some(_)) => None,
            _ => Some(ArrayFormatter::try_new(array, options)?),
        };
        Ok(Some(ArrayFormatter::new(
            Box::new(TimeFormat {
                array,
                ticks,
                finite,
                null: options.null(),
            }),
            options.safe(),
        )))
    }
}

impl duckdb::arrow::util::display::DisplayIndex for TimeFormat<'_> {
    fn write(
        &self,
        idx: usize,
        f: &mut dyn std::fmt::Write,
    ) -> duckdb::arrow::util::display::FormatResult {
        use duckdb::arrow::datatypes::TimeUnit;
        if self.array.is_null(idx) {
            f.write_str(self.null)?;
            return Ok(());
        }
        let (v, top) = match self.ticks {
            Ticks::Days(d) => (i64::from(d[idx]), i64::from(i32::MAX)),
            Ticks::Time(t, _) => (t[idx], i64::MAX),
        };
        if v == top {
            f.write_str("infinity")?;
        } else if v == -top {
            f.write_str("-infinity")?;
        } else if let Ticks::Time(_, Some(unit)) = self.ticks {
            let micros = match unit {
                TimeUnit::Second => v.saturating_mul(1_000_000),
                TimeUnit::Millisecond => v.saturating_mul(1_000),
                TimeUnit::Microsecond => v,
                TimeUnit::Nanosecond => v.div_euclid(1_000),
            };
            f.write_str(&crate::result::timestamp_utc(micros))?;
        } else if let Some(finite) = &self.finite {
            finite.value(idx).write(f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchys_catalog_reads_cannot_be_shadowed() {
        // Measured on DuckDB 1.5.5 before every catalog read named `system.main`: a table macro
        // the agent called `duckdb_types` turned the CREATE after it into a dynamic PIVOT's and
        // swallowed its answer; one called `duckdb_columns` in its own schema, which the
        // host-facing connection sees too, answered an attach with the agent's columns; one
        // called `duckdb_tables` failed every attach after it and put a table that does not
        // exist in `describe` and the hints. The macros are planted here past the guard, which
        // refuses those names (REQ-9), so this holds the reads to their own account.
        let dir = std::env::temp_dir().join(fresh_name("benchy-shadowed-reads"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
        for plant in [
            "CREATE TEMP MACRO duckdb_types() AS TABLE SELECT 'temp' AS database_name, \
             'main' AS schema_name, '__pivot_enum_' || range AS type_name, range AS type_oid, \
             false AS internal FROM range(1000)",
            "CREATE MACRO duckdb_columns() AS TABLE SELECT database_name, table_name, \
             'forged' AS column_name, 'VARCHAR' AS data_type, 0 AS column_index \
             FROM system.main.duckdb_columns()",
            "CREATE MACRO duckdb_functions() AS TABLE SELECT 'nowhere' AS database_name, \
             'main' AS schema_name, 'none' AS function_name",
            "CREATE MACRO duckdb_tables() AS TABLE SELECT database_name, schema_name, \
             table_name, comment, internal, table_oid FROM system.main.duckdb_tables() \
             UNION ALL SELECT 'memory', 'main', 'ghost', NULL, false, 1 \
             UNION ALL SELECT database_name, 'main', 'extra', NULL, false, 3 \
             FROM system.main.duckdb_databases() WHERE database_name LIKE 'benchy%'",
            "CREATE MACRO duckdb_views() AS TABLE SELECT 'memory' AS database_name, \
             'main' AS schema_name, 'ghost_view' AS view_name, false AS internal, 2 AS view_oid \
             UNION ALL SELECT database_name, 'main', 'extra', false, 4 \
             FROM system.main.duckdb_databases() WHERE database_name LIKE 'benchy%'",
            // what the attach check counts besides tables: one more of each in every catalog
            "CREATE MACRO duckdb_sequences() AS TABLE SELECT database_name \
             FROM system.main.duckdb_databases() WHERE database_name LIKE 'benchy%'",
            "CREATE MACRO duckdb_indexes() AS TABLE SELECT database_name \
             FROM system.main.duckdb_databases() WHERE database_name LIKE 'benchy%'",
            "CREATE MACRO duckdb_types() AS TABLE SELECT database_name, false AS internal \
             FROM system.main.duckdb_databases() WHERE database_name LIKE 'benchy%'",
            "CREATE MACRO duckdb_schemas() AS TABLE SELECT database_name, false AS internal \
             FROM system.main.duckdb_databases() WHERE database_name LIKE 'benchy%'",
            "CREATE MACRO version() AS 'not duckdb'",
            "CREATE MACRO count() AS 99",
        ] {
            ws.raw_exec_unchecked(plant)
                .unwrap_or_else(|e| panic!("{plant}: {e}"));
        }

        let file = |name: &str, extra: &str| {
            let f = dir.join(format!("{name}.duckdb"));
            let c = Connection::open(&f).unwrap();
            c.execute_batch(&format!("CREATE TABLE {name} AS SELECT 1 AS id; {extra}"))
                .unwrap();
            f
        };
        let t = ws
            .attach("readings", &file("readings", ""))
            .expect("attached");
        assert_eq!(t.columns, [("id".to_string(), "INTEGER".to_string())]);
        assert_eq!(t.rows, 1);
        assert!(
            ws.attach(
                "other",
                &file("other", "CREATE MACRO lower(x) AS 'shadowed'")
            )
            .is_err(),
            "a file carrying a macro is refused"
        );

        assert!(!ws.nameable().iter().any(|n| n.starts_with("ghost")));
        assert!(!ws.own_tables().iter().any(|n| n.starts_with("ghost")));
        assert!(
            ws.engine_version().starts_with('v'),
            "{}",
            ws.engine_version()
        );

        let caps = crate::result::Caps::default();
        let out = ws
            .work("CREATE TABLE mine AS SELECT 1 AS x", &caps)
            .expect("runs");
        assert_eq!(out.len(), 1, "not taken for a pivot's CREATE");
        // A type of the agent's own is refused (`the_agent_keeps_no_enum`); taken for a pivot's,
        // it would have been kept, answering nothing.
        let e = ws
            .work("CREATE TYPE mood AS ENUM ('ok')", &caps)
            .expect_err("the agent's own type, not a pivot's")
            .to_string();
        assert!(e.contains("the type mood"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pivot_type_that_cannot_be_dropped_spoils_the_engine() {
        // The end of a batch drops its dynamic PIVOTs' types. One that cannot be dropped is held
        // by something of the agent's, outside DuckDB's memory budget, for as long as the engine
        // lives; it was swallowed, and a host's review pinned 799 MB so. Now the engine says it
        // must be rebuilt. The pin is planted here past the guard, which refuses it
        // (`an_object_that_keeps_an_enum_in_its_sql_is_refused`), so this holds the end of the
        // batch to its own account.
        let dir = std::env::temp_dir().join(fresh_name("benchy-spoiled"));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ws = Workspace::open_sealed(&dir, "1GB").unwrap();
        let free = "__pivot_enum_00000000-0000-4000-8000-000000000001";
        ws.raw_exec_unchecked(&format!("CREATE TEMP TYPE \"{free}\" AS ENUM ('a')"))
            .unwrap();
        ws.drop_pivot_types();
        assert_eq!(ws.spoiled(), None, "a type nothing holds is dropped");
        let pinned = "__pivot_enum_00000000-0000-4000-8000-000000000000";
        ws.raw_exec_unchecked(&format!(
            "CREATE TEMP TYPE \"{pinned}\" AS ENUM ('a'); \
             CREATE TEMP TABLE pin (c INTEGER DEFAULT length(('a'::\"{pinned}\")::VARCHAR))"
        ))
        .unwrap();
        ws.drop_pivot_types();
        let why = ws.spoiled().expect("the engine must go");
        assert!(why.contains(pinned), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zone_other_than_utc_stops_the_engine() {
        assert!(zone_held(Some("UTC"), || 1).is_ok());
        let e = zone_held(Some("Europe/Berlin"), || 1).unwrap_err();
        assert!(e.contains("Europe/Berlin"), "{e}");
        assert!(
            zone_held(None, || 0).is_ok(),
            "no ICU: UTC is DuckDB's only zone"
        );
        assert!(
            zone_held(None, || 1).is_err(),
            "ICU loaded and no zone to read"
        );
    }

    /// Set in the process [`in_a_zone_other_than_utc`] starts, which runs the test's body.
    const IN_ZONE: &str = "BENCHY_TEST_IN_ZONE";

    /// Run test `name` again in a process of its own whose zone is not UTC, and `true` if this
    /// is the process that should only wait for it. The engine's environment is empty, so on a
    /// real host the zone comes from /etc/localtime; ICU reads `TZ` before it, and a process of
    /// its own is the only way to give ICU a zone: it settles its default once per process.
    fn in_a_zone_other_than_utc(name: &str) -> bool {
        if std::env::var_os(IN_ZONE).is_some() {
            // the zone this process is in really is not UTC, or the test proves nothing
            let plain = Connection::open_in_memory().unwrap();
            let zone: String = plain
                .query_row("SELECT current_setting('TimeZone')", [], |r| r.get(0))
                .unwrap();
            assert_eq!(zone, "Asia/Kolkata", "the host's zone did not take");
            return false;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("workspace::tests::{name}"),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(IN_ZONE, "1")
            .env("TZ", "Asia/Kolkata")
            .output()
            .unwrap();
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success() && said.contains("1 passed"),
            "in Asia/Kolkata: {said}"
        );
        true
    }

    /// What `sql` gives on the agent's connection, as text: its one row's one column.
    fn on_the_agents(ws: &Workspace, sql: &CStr) -> String {
        let rows = ws.agent_rows(sql).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        rows[0].1.clone()
    }

    /// The zone is now set globally before the lock, which reaches connections opened before
    /// and after; the seal reads it back on the agent's connection, sealed from a bench and
    /// sealed empty, and a day's bucket of 23:30 UTC is that day. Run in a process of its own
    /// whose zone is Asia/Kolkata, checked to have taken.
    #[test]
    fn the_agent_connection_is_utc_on_a_host_that_is_not() {
        // Measured on DuckDB 1.5.5 before the fix, on a host in Europe/Berlin: `SET TimeZone` is
        // per connection, the lockdown set it on the host's alone, and the agent's kept the
        // host's zone — `current_setting` said Europe/Berlin, a day's bucket of 23:30 UTC was the
        // next day's, and every TIMESTAMPTZ column was "not rendered".
        if in_a_zone_other_than_utc("the_agent_connection_is_utc_on_a_host_that_is_not") {
            return;
        }
        let zone = c"SELECT current_setting('TimeZone')";
        let bucket = c"SELECT date_trunc('day', TIMESTAMPTZ '2026-01-01 23:30:00+00')::VARCHAR";
        // the agent's connection is opened before the lockdown runs: sealed from a bench...
        let dir = std::env::temp_dir().join(fresh_name("benchy-zone-sealed"));
        let bench = Bench::open_in(&dir).unwrap();
        let ws = bench.seal("1GB").unwrap();
        assert_eq!(on_the_agents(&ws, zone), "UTC");
        assert_eq!(on_the_agents(&ws, bucket), "2026-01-01 00:00:00+00");
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
        // ...and sealed empty, for tables attached later
        let dir = std::env::temp_dir().join(fresh_name("benchy-zone-empty"));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::open_sealed(&dir, "1GB").unwrap();
        assert_eq!(on_the_agents(&ws, zone), "UTC");
        assert_eq!(on_the_agents(&ws, bucket), "2026-01-01 00:00:00+00");
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_bench_connection_is_utc_on_a_host_that_is_not() {
        // A loader's bench sets its zone on its own connection; the helpers' build opens another
        // after it (`connect_raw`), which took the host's zone before the fix.
        if in_a_zone_other_than_utc("every_bench_connection_is_utc_on_a_host_that_is_not") {
            return;
        }
        let dir = std::env::temp_dir().join(fresh_name("benchy-zone-bench"));
        let bench = Bench::open_in(&dir).unwrap();
        let zone = |c: &Connection| -> String {
            c.query_row("SELECT current_setting('TimeZone')", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(zone(bench.connection()), "UTC");
        assert_eq!(zone(&bench.connection().try_clone().unwrap()), "UTC");
        let raw = bench.connect_raw().unwrap();
        let seen = unsafe {
            let mut r: ffi::duckdb_result = std::mem::zeroed();
            let sql = c"SELECT current_setting('TimeZone')";
            assert_eq!(
                ffi::duckdb_query(raw, sql.as_ptr(), &mut r),
                ffi::duckdb_state_DuckDBSuccess
            );
            let v = ffi::duckdb_value_varchar(&mut r, 0, 0);
            let s = CStr::from_ptr(v).to_string_lossy().into_owned();
            ffi::duckdb_free(v.cast());
            ffi::duckdb_destroy_result(&mut r);
            let mut raw = raw;
            ffi::duckdb_disconnect(&mut raw);
            s
        };
        assert_eq!(seen, "UTC", "a connection opened after the bench's");
        let _ = bench.finish();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_common_type_is_streamed() {
        // A SELECT is streamed only where `arrow_reads` says its result reads as Arrow; one
        // that does not is built whole, as every result was. So a type it wrongly turns away
        // costs the whole result again (`a_query_without_a_limit_reads_no_further_than_it_keeps`).
        // A sweep of the types `every_common_type_renders_as_something_a_reader_can_use` renders,
        // and the three Arrow carries unfaithfully.
        let dir = std::env::temp_dir().join(fresh_name("benchy-arrow-reads"));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::open_sealed(&dir, "1GB").unwrap();
        for sql in [
            c"SELECT 1::TINYINT, 1::SMALLINT, 1::INTEGER, 1::BIGINT, 1::HUGEINT, 1::UBIGINT, \
              1::UHUGEINT, 1.5::FLOAT, 1.5::DOUBLE, 1.5::DECIMAL(18,3), 1.5::DECIMAL(38,3)",
            c"SELECT 'x', 'x'::BLOB, true, NULL, uuid(), INTERVAL 1 DAY",
            c"SELECT DATE '2026-01-01', TIME '01:02:03', TIMESTAMP '2026-01-01', \
              TIMESTAMPTZ '2026-01-01 00:00:00+00', TIMESTAMP_S '2026-01-01', \
              TIMESTAMP_MS '2026-01-01', TIMESTAMP_NS '2026-01-01'",
            c"SELECT [1, 2], {'a': 1}, MAP {'k': 1}, [1, 2]::INTEGER[2], \
              union_value(n := 1), 'a'::ENUM('a', 'b')",
            c"SELECT '0101'::BIT, 1::BIGNUM, TIMETZ '01:02:03+01'",
        ] {
            let ok = unsafe {
                let mut prep: ffi::duckdb_prepared_statement = ptr::null_mut();
                let prepared = ffi::duckdb_prepare(ws.agent, sql.as_ptr(), &mut prep)
                    == ffi::duckdb_state_DuckDBSuccess;
                assert!(prepared, "{sql:?}");
                let ok = arrow_reads(ws.agent, prep);
                ffi::duckdb_destroy_prepare(&mut prep);
                ok
            };
            assert!(ok, "{sql:?} is built whole");
        }
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
