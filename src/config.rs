//! The operator's config file. REQ-4: what may be loaded, declared where the operator can see it.
//!
//! TOML, read once at startup, so the operator's intent cannot drift mid-session — a config
//! change means a restart, and a bench rebuilt after a hard kill is rebuilt from the text read
//! then. **Unknown fields are rejected everywhere, the top level included**, and so are known
//! fields that cannot apply to the source they are on.
//!
//! Three rules, each against a mistake config loaders commonly make:
//!
//! - **Never warn-and-drop.** If you can compute *did you mean*, you can fail. An unknown key is
//!   an error, not a log line nobody reads.
//! - **Never silently no-op.** An unresolvable `${VAR}` aborts startup rather than splicing in an
//!   empty string, which is how a credential becomes the empty password at 3am.
//! - **Secrets travel in `env` or not at all.** `${VAR}` interpolation works *only* there.
//!   `config` sits in this file on disk and is copied verbatim into every input document a
//!   connector reads — where a connector may well log it — so a secret in it is not a secret.

use crate::exec::{ExecSource, FieldDecl, Limits};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, rename = "source")]
    sources: Vec<Source>,
    budget: Option<BudgetTable>,
    attach: Option<BudgetTable>,
    engine: Option<EngineTable>,
    loader: Option<LoaderTable>,
    helpers: Option<HelpersTable>,
}

/// The engine's own bounds: `memory_ceiling`, as `--memory-ceiling` sets it, the most resident
/// memory the engine may hold (`supervisor::Memory`). For a server and a host alike.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineTable {
    memory_ceiling: Option<String>,
}

/// Every loader's own bound: `memory_ceiling`, as `--loader-memory-ceiling` sets it, the most
/// resident memory one loader may hold with the connector it runs
/// (`supervisor::LOADER_MEMORY_CEILING`). For a server and a host alike.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LoaderTable {
    memory_ceiling: Option<String>,
}

/// REQ-18: the operator's SQL helpers, views and macros the agent calls like built-ins: in a
/// file of their own, or inline, never both.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpersTable {
    file: Option<String>,
    sql: Option<String>,
}

/// REQ-12: what the whole session may fetch from its sources, across every load. A ceiling left
/// out keeps its default ([`crate::admission::Budget`]). The same two keys in `[attach]` are what
/// one attachment of a host's may hold (`benchy-mcp host --config`, docs/HOST-PROTOCOL.md).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetTable {
    rows: Option<u64>,
    bytes: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    /// Every source but a ClickHouse one that lists `tables`, whose tables name themselves.
    name: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    // file
    path: Option<String>,
    // exec
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    config: toml::value::Table,
    #[serde(default)]
    schema: Vec<FieldDecl>,
    max_rows: Option<u64>,
    max_bytes: Option<u64>,
    deadline_secs: Option<u64>,
    narrow: Option<Narrow>,
    // either kind: REQ-14, what the operator states about the data's shape
    time_column: Option<String>,
    #[serde(default)]
    series: Vec<String>,
    // file or exec: the zone a zone-less timestamp is in
    timezone: Option<String>,
    // clickhouse: REQ-16
    url: Option<String>,
    database: Option<String>,
    #[serde(default)]
    tables: Vec<String>,
    query: Option<String>,
    // clickhouse or http
    compression: Option<String>,
    // http: REQ-17, docs/HTTP-SOURCE.md
    method: Option<String>,
    body: Option<String>,
    format: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    params: BTreeMap<String, String>,
    auth: Option<crate::sources::http::AuthTable>,
    pages: Option<crate::sources::http::PagesTable>,
    pushdown: Option<crate::sources::http::Pushdown>,
    retries: Option<u32>,
    rate_limit: Option<crate::sources::http::RateLimit>,
    #[serde(default)]
    redirect_hosts: Vec<String>,
    // http or file: a CSV's delimiter
    delimiter: Option<String>,
}

/// What the operator will let this source return. Narrowing is the cheapest admission control
/// there is: a load bounded to the last 30 days costs the upstream system a fraction of a full
/// scan, and the bound is declared where the operator can see it rather than hoped for.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Narrow {
    /// The timestamp column the window applies to.
    column: Option<String>,
    /// A window ending now, as `30d`, `24h` or `90m`.
    last: Option<String>,
    /// Or an explicit half-open range, RFC 3339 with an offset.
    start: Option<String>,
    end: Option<String>,
    entity_column: Option<String>,
    #[serde(default)]
    entity_ids: Vec<String>,
    #[serde(default)]
    columns: Vec<String>,
}

/// A file source: table `name` from `path`, its zone-less timestamps in `timezone`; declared as
/// `schema`, a CSV or TSV in `delimiter`, when the operator chose to (R3-5), and sniffed when not.
#[derive(Debug)]
pub struct FileSource {
    pub name: String,
    pub path: PathBuf,
    pub timezone: Option<String>,
    pub declared: crate::source::Declared,
}

/// R3-5: a file source's declaration, checked now, against its file's kind, so a mistake stops
/// startup: each column once, in the vocabulary; a `delimiter` only on a CSV or TSV, which is
/// stated to the reader whether the file is declared or sniffed.
fn file_declaration(
    file: &Path,
    schema: Vec<FieldDecl>,
    delimiter: Option<&str>,
) -> Result<crate::source::Declared, String> {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let delimiter = match delimiter {
        None => None,
        Some(d) if matches!(ext.as_str(), "csv" | "tsv") => {
            Some(crate::sources::http::delimiter(d)?)
        }
        Some(_) => return Err("`delimiter` applies only to a .csv or .tsv file.".into()),
    };
    let mut seen = std::collections::HashSet::new();
    for f in &schema {
        // ASCII only, as DuckDB folds: `Ä` and `ä` are two columns to it.
        if !seen.insert(f.name.to_ascii_lowercase()) {
            return Err(format!(
                "column `{}` is declared twice: DuckDB does not tell names apart by case.",
                f.name
            ));
        }
        if crate::exec::arrow_type(&f.ty).is_none() {
            return Err(format!(
                "column `{}` is declared as `{}`, which is not in the vocabulary. See the \
                 \"Types\" table in docs/EXEC-CONNECTOR-PROTOCOL.md.",
                f.name, f.ty
            ));
        }
    }
    Ok(crate::source::Declared { schema, delimiter })
}

/// What a config file resolves to.
#[derive(Debug, Default)]
pub struct Sources {
    pub files: Vec<FileSource>,
    pub exec: Vec<ExecSource>,
    /// REQ-16: sources benchy reads itself, one table each.
    pub builtin: Vec<crate::sources::BuiltIn>,
    /// REQ-14: what the operator stated about each source's shape, by name. A source that states
    /// nothing is not here, and its shape is found by heuristic.
    pub shapes: BTreeMap<String, crate::profile::Shape>,
    /// The session's budget, nothing spent yet.
    pub budget: crate::admission::Budget,
    /// What the config states of the limits a host's session has, each where it is stated:
    /// `[budget]` and `[attach]`. The command line may state them too, but not both.
    pub limits: Stated,
    /// `[engine] memory_ceiling`, where stated; checked against `--memory` once that is known.
    pub memory_ceiling: Option<String>,
    /// `[loader] memory_ceiling`, where stated.
    pub loader_memory_ceiling: Option<String>,
    /// REQ-18: the operator's helpers, in the order given; empty when there are none.
    pub helpers: Vec<crate::helpers::Def>,
}

/// Limits as stated, each `None` where it is not: `attach_rows`, `attach_bytes`, `session_rows`,
/// `session_bytes`, in that order.
pub type Stated = [Option<u64>; 4];

pub fn load(path: &Path) -> Result<Sources, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text, path)
}

/// A config's text, as read from `path`. Relative paths resolve against the directory of
/// `path`, and errors name it; the file itself is not read again.
pub fn parse(text: &str, path: &Path) -> Result<Sources, String> {
    let parsed: File = toml::from_str(text).map_err(|e| {
        // toml's message already names the line and the offending key, including the
        // "unknown field, expected one of ..." that deny_unknown_fields produces.
        format!("{}: {e}", path.display())
    })?;
    // Relative paths resolve against the config file's directory, never cwd and never PATH.
    // An operator edits this file; the paths in it should mean what they look like they mean.
    let base = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let mut out = Sources::default();
    if let Some(e) = parsed.engine {
        out.memory_ceiling = e.memory_ceiling;
    }
    if let Some(l) = parsed.loader {
        out.loader_memory_ceiling = l.memory_ceiling;
    }
    if let Some(b) = parsed.budget {
        out.limits[2] = b.rows;
        out.limits[3] = b.bytes;
        out.budget = budget(b, path)?;
    }
    if let Some(a) = parsed.attach {
        for (key, v) in [("rows", a.rows), ("bytes", a.bytes)] {
            if v == Some(0) {
                return Err(format!(
                    "{}: [attach] {key} = 0 would let a host attach nothing. Leave it out for \
                     the default, or give the most one attachment may hold.",
                    path.display()
                ));
            }
        }
        if !parsed.sources.is_empty() {
            return Err(format!(
                "{}: [attach] is what a host may attach, and a config with sources is a server's, \
                 which no host attaches to. Give it to `benchy-mcp host --config` alone.",
                path.display()
            ));
        }
        out.limits[0] = a.rows;
        out.limits[1] = a.bytes;
    }
    if let Some(h) = parsed.helpers {
        let sql = match (h.file, h.sql) {
            (Some(f), None) => {
                let file = resolve(&base, &f);
                std::fs::read_to_string(&file).map_err(|e| {
                    format!("{}: [helpers] file {}: {e}", path.display(), file.display())
                })?
            }
            (None, Some(sql)) => sql,
            _ => {
                return Err(format!(
                    "{}: [helpers] takes `file`, a SQL file of helpers, or `sql`, the helpers \
                     inline: one of the two.",
                    path.display()
                ))
            }
        };
        out.helpers = crate::helpers::parse(&sql)
            .map_err(|e| format!("{}: [helpers]: {e}", path.display()))?;
    }
    let mut seen = std::collections::BTreeSet::new();
    for s in parsed.sources {
        inert_keys(&s, path)?;
        let names = table_names(&s, path)?;
        for name in &names {
            if !seen.insert(name.clone()) {
                return Err(format!(
                    "{}: two sources are both named `{name}`. A name is what the agent and the \
                     engine see, so it has to be unique.",
                    path.display(),
                ));
            }
            check_name(name, path)?;
            // REQ-18: a helper and a table may not share a name; one would hide the other.
            if let Some(h) = out
                .helpers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(name))
            {
                return Err(format!(
                    "{}: helper `{}` and source `{name}` have the same name. Rename one of them.",
                    path.display(),
                    h.name
                ));
            }
        }
        if let Some(zone) = &s.timezone {
            // Checked now, against the engine's own zone database, so a typo stops startup
            // rather than failing the load — or, worse, being read as UTC.
            if !crate::canonical::known_zone(zone) {
                return Err(format!(
                    "{}: source `{}` has timezone `{zone}`, which is not a time zone this engine \
                     knows. Use an IANA name, such as `Europe/Copenhagen` or `UTC`.",
                    path.display(),
                    names[0]
                ));
            }
        }
        let shape = crate::profile::Shape {
            time_column: s.time_column.clone(),
            series: s.series.clone(),
        };
        if shape != crate::profile::Shape::default() {
            if s.kind == "exec" || !s.schema.is_empty() {
                check_shape(&shape, &s.schema, &names[0], path)?;
            }
            for name in &names {
                out.shapes.insert(name.clone(), shape.clone());
            }
        }
        // Every kind but clickhouse has exactly one name, checked above.
        let name = names[0].clone();
        match s.kind.as_str() {
            "file" => {
                let p = s.path.ok_or_else(|| {
                    format!(
                        "{}: source `{}` is a file source and needs `path`.",
                        path.display(),
                        name
                    )
                })?;
                let path_of = resolve(&base, &p);
                let declared = file_declaration(&path_of, s.schema, s.delimiter.as_deref())
                    .map_err(|m| format!("{}: source `{name}`: {m}", path.display()))?;
                out.files.push(FileSource {
                    name,
                    path: path_of,
                    timezone: s.timezone,
                    declared,
                });
            }
            "exec" => out.exec.push(exec_source(s, name, &base, path)?),
            "clickhouse" => out.builtin.extend(clickhouse_sources(s, names, path)?),
            "http" => out
                .builtin
                .push(crate::sources::BuiltIn::Http(http_source(s, name, path)?)),
            other => {
                return Err(format!(
                    "{}: source `{name}` has type `{other}`. This benchy understands `file`, \
                     `exec`, `clickhouse` and `http`. Anything else can be an `exec` connector: a \
                     program that writes an Arrow stream to stdout \
                     (docs/EXEC-CONNECTOR-PROTOCOL.md).",
                    path.display(),
                ))
            }
        }
    }
    Ok(out)
}

/// Never silently no-op: a ceiling of zero admits nothing, so the first source would fail far
/// from the line that caused it.
fn budget(b: BudgetTable, path: &Path) -> Result<crate::admission::Budget, String> {
    let d = crate::admission::Budget::default();
    for (key, v) in [("rows", b.rows), ("bytes", b.bytes)] {
        if v == Some(0) {
            return Err(format!(
                "{}: [budget] {key} = 0 would let the session fetch nothing. Leave it out for \
                 the default, or give the most the session may fetch.",
                path.display()
            ));
        }
    }
    Ok(crate::admission::Budget::new(
        b.rows.unwrap_or(d.max_rows),
        b.bytes.unwrap_or(d.max_bytes),
    ))
}

/// Never silently no-op, applied to keys: one that cannot apply to this kind of source is an
/// error. Otherwise `max_rows = 1000` on a file source reads as a cap and caps nothing.
fn inert_keys(s: &Source, path: &Path) -> Result<(), String> {
    const FILE: &[&str] = &["file"];
    const EXEC: &[&str] = &["exec"];
    const CLICKHOUSE: &[&str] = &["clickhouse"];
    const HTTP: &[&str] = &["http"];
    const FETCHED: &[&str] = &["exec", "clickhouse", "http"];
    // Each key, whether it is set, and the kinds of source it applies to.
    let keys: &[(&str, bool, &[&str])] = &[
        ("path", s.path.is_some(), FILE),
        ("command", s.command.is_some(), EXEC),
        ("args", !s.args.is_empty(), EXEC),
        ("config", !s.config.is_empty(), EXEC),
        ("env", !s.env.is_empty(), FETCHED),
        // A file's is the operator's choice (R3-5): read as declared, not sniffed.
        (
            "schema",
            !s.schema.is_empty(),
            &["exec", "clickhouse", "http", "file"],
        ),
        ("max_rows", s.max_rows.is_some(), FETCHED),
        ("max_bytes", s.max_bytes.is_some(), FETCHED),
        ("deadline_secs", s.deadline_secs.is_some(), FETCHED),
        ("narrow", s.narrow.is_some(), FETCHED),
        ("url", s.url.is_some(), &["clickhouse", "http"]),
        ("database", s.database.is_some(), CLICKHOUSE),
        ("tables", !s.tables.is_empty(), CLICKHOUSE),
        ("query", s.query.is_some(), CLICKHOUSE),
        (
            "compression",
            s.compression.is_some(),
            &["clickhouse", "http"],
        ),
        ("method", s.method.is_some(), HTTP),
        ("body", s.body.is_some(), HTTP),
        ("format", s.format.is_some(), HTTP),
        ("headers", !s.headers.is_empty(), HTTP),
        ("params", !s.params.is_empty(), HTTP),
        ("auth", s.auth.is_some(), HTTP),
        ("pages", s.pages.is_some(), HTTP),
        ("delimiter", s.delimiter.is_some(), &["http", "file"]),
        ("pushdown", s.pushdown.is_some(), HTTP),
        ("retries", s.retries.is_some(), HTTP),
        ("rate_limit", s.rate_limit.is_some(), HTTP),
        ("redirect_hosts", !s.redirect_hosts.is_empty(), HTTP),
        // ClickHouse sends every instant with its zone, so there is nothing for one to apply to.
        // A CSV or JSON body may hold zone-less ones, as a file may.
        ("timezone", s.timezone.is_some(), &["file", "exec", "http"]),
    ];
    if !matches!(s.kind.as_str(), "file" | "exec" | "clickhouse" | "http") {
        // The kind itself is refused, naming the ones there are, once the name is known.
        return Ok(());
    }
    match keys
        .iter()
        .find(|(_, set, kinds)| *set && !kinds.contains(&s.kind.as_str()))
    {
        Some((key, _, kinds)) => Err(format!(
            "{}: source `{}` is a {} source, and `{key}` applies only to {} sources. It would do \
             nothing here, so it is refused rather than ignored.",
            path.display(),
            s.name
                .as_deref()
                .or(s.tables.first().map(String::as_str))
                .unwrap_or("?"),
            s.kind,
            match kinds.split_last() {
                Some((last, [])) => last.to_string(),
                Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
                None => String::new(),
            }
        )),
        None => Ok(()),
    }
}

/// The tables a source becomes: its `name`, or, for a ClickHouse source that lists `tables`, one
/// per table, each named after it.
fn table_names(s: &Source, path: &Path) -> Result<Vec<String>, String> {
    let at = path.display();
    match (&s.name, s.tables.is_empty()) {
        (Some(n), true) => Ok(vec![n.clone()]),
        (None, false) => Ok(s.tables.clone()),
        (Some(n), false) => Err(format!(
            "{at}: source `{n}` has both `name` and `tables`. Each listed table becomes a table of \
             the same name; `name` goes with `query`, which makes one."
        )),
        (None, true) if s.kind == "clickhouse" => Err(format!(
            "{at}: a clickhouse source needs `tables`, or `query` with a `name`."
        )),
        (None, true) => Err(format!("{at}: a `{}` source needs a `name`.", s.kind)),
    }
}

/// REQ-16: one ClickHouse source per table it names, or one for its query. Its credentials are
/// in `env` and nowhere else, under the two names it reads: a third would be handed to nothing.
fn clickhouse_sources(
    s: Source,
    names: Vec<String>,
    path: &Path,
) -> Result<Vec<crate::sources::BuiltIn>, String> {
    use crate::sources::clickhouse;
    let first = names[0].clone();
    let bad = |m: &str| format!("{}: source `{first}`: {m}", path.display());
    let url = s.url.clone().ok_or_else(|| {
        bad("a clickhouse source needs `url`, such as \"http://localhost:8123\".")
    })?;
    clickhouse::check_url(&url).map_err(|e| bad(&e))?;
    let database = s.database.clone().unwrap_or_else(|| "default".into());
    if database.is_empty() || database.chars().any(char::is_control) {
        return Err(bad("`database` is empty or holds a control character."));
    }
    match (&s.query, s.tables.is_empty()) {
        (Some(_), false) => {
            return Err(bad(
                "it has both `tables` and `query`. List tables to load each as it is, or write \
                 one `query` for one table, with a `name`.",
            ))
        }
        (Some(q), true) if q.trim().is_empty() => return Err(bad("`query` is empty.")),
        (None, true) => {
            return Err(bad(
                "a clickhouse source with a `name` needs a `query`; to load tables by their own \
                 names, list them in `tables` instead.",
            ))
        }
        _ => {}
    }
    for k in s.env.keys() {
        if !clickhouse::ENV_KEYS.contains(&k.as_str()) {
            return Err(bad(&format!(
                "env `{k}` would be handed to nothing: a clickhouse source reads {} from its \
                 `env`, and nothing else.",
                clickhouse::ENV_KEYS.join(" and ")
            )));
        }
    }
    let mut env = BTreeMap::new();
    for (k, v) in &s.env {
        env.insert(k.clone(), interpolate(v, k, &first, path)?);
    }
    let mut seen = std::collections::HashSet::new();
    for f in &s.schema {
        if !seen.insert(f.name.to_ascii_lowercase()) {
            return Err(bad(&format!(
                "column `{}` is pinned twice: DuckDB does not tell names apart by case.",
                f.name
            )));
        }
        if crate::exec::arrow_type(&f.ty).is_none() {
            return Err(bad(&format!(
                "column `{}` is pinned as `{}`, which is not in the vocabulary. See the type \
                 table in docs/SOURCES.md.",
                f.name, f.ty
            )));
        }
    }
    let mut limits = crate::exec::Limits::default();
    if let Some(v) = s.max_rows {
        limits.max_rows = v;
    }
    if let Some(v) = s.max_bytes {
        limits.max_bytes = v;
    }
    if let Some(v) = s.deadline_secs {
        limits.deadline = std::time::Duration::from_secs(v);
    }
    let narrowing = match s.narrow {
        Some(n) => build_narrowing(n, &first, path)?,
        None => crate::exec::Narrowing::default(),
    };
    if !s.schema.is_empty() && narrowing.columns.is_some() {
        return Err(bad(
            "it pins columns in `schema` and names them in `narrow.columns`. Pinned columns are \
             the ones loaded; keep one of the two.",
        ));
    }
    if let Some((_, start, end)) = &narrowing.time {
        for t in [start, end].into_iter().flatten() {
            if clickhouse::rfc3339_micros(t).is_none() {
                return Err(bad(&format!(
                    "`{t}` is not an RFC 3339 time with an offset, such as 2026-01-01T00:00:00Z."
                )));
            }
        }
    }
    if let Some((_, ids)) = &narrowing.entities {
        crate::exec::vet_entities(ids).map_err(|f| bad(&f.detail))?;
    }
    let compression = match s.compression.as_deref() {
        None => clickhouse::Compression::default_for(&url),
        Some(c) => clickhouse::Compression::parse(c).ok_or_else(|| {
            bad(&format!(
                "`compression = \"{c}\"`: it is \"zstd\", \"lz4\" or \"none\" (unset: \"none\" \
                 for a server on loopback, else \"zstd\")."
            ))
        })?,
    };
    let from = |name: &str| match &s.query {
        Some(q) => clickhouse::From::Query(q.clone()),
        None => clickhouse::From::Table(name.to_string()),
    };
    Ok(names
        .iter()
        .map(|name| {
            crate::sources::BuiltIn::Clickhouse(clickhouse::Source {
                name: name.clone(),
                url: url.clone(),
                database: database.clone(),
                from: from(name),
                env: env.clone(),
                schema: s.schema.clone(),
                narrowing: narrowing.clone(),
                limits: limits.clone(),
                compression,
                readonly: Default::default(),
            })
        })
        .collect())
}

/// REQ-17: an http source, checked as it will be loaded ([`crate::sources::http::spec`]). Its
/// `env` is interpolated here, as every source's is, and handed only to `auth`.
fn http_source(s: Source, name: String, path: &Path) -> Result<crate::sources::http::Spec, String> {
    let bad = |m: String| format!("{}: source `{name}`: {m}", path.display());
    let mut env = BTreeMap::new();
    for (k, v) in &s.env {
        env.insert(k.clone(), interpolate(v, k, &name, path)?);
    }
    let mut limits = Limits::default();
    if let Some(v) = s.max_rows {
        limits.max_rows = v;
    }
    if let Some(v) = s.max_bytes {
        limits.max_bytes = v;
    }
    if let Some(v) = s.deadline_secs {
        limits.deadline = std::time::Duration::from_secs(v);
    }
    let narrowing = match s.narrow {
        Some(n) => build_narrowing(n, &name, path)?,
        None => crate::exec::Narrowing::default(),
    };
    crate::sources::http::spec(crate::sources::http::Raw {
        name: name.clone(),
        url: s.url,
        method: s.method,
        body: s.body,
        format: s.format,
        headers: s.headers,
        params: s.params,
        auth: s.auth,
        env,
        compression: s.compression,
        pages: s.pages,
        pushdown: s.pushdown,
        retries: s.retries,
        rate_limit: s.rate_limit,
        redirect_hosts: s.redirect_hosts,
        delimiter: s.delimiter,
        schema: s.schema,
        narrowing,
        limits,
        timezone: s.timezone,
    })
    .map_err(bad)
}

/// An exec source's stated shape must name declared columns, and its time column must be a date
/// or timestamp. A file's columns are known only once it is read, so a file source's shape is
/// checked by its loader, which refuses the load the same way.
fn check_shape(
    shape: &crate::profile::Shape,
    schema: &[FieldDecl],
    name: &str,
    path: &Path,
) -> Result<(), String> {
    let declared = |c: &str| schema.iter().find(|f| f.name == c);
    if let Some(t) = &shape.time_column {
        match declared(t) {
            None => {
                return Err(format!(
                    "{}: source `{name}` states `time_column = \"{t}\"`, which is not in its \
                     schema.",
                    path.display()
                ))
            }
            Some(f) if !matches!(f.ty.as_str(), "date" | "timestamp" | "timestamptz") => {
                return Err(format!(
                    "{}: source `{name}` states `time_column = \"{t}\"`, which is declared \
                     `{}`: a time column is a date, timestamp or timestamptz.",
                    path.display(),
                    f.ty
                ))
            }
            _ => {}
        }
    }
    for c in &shape.series {
        if declared(c).is_none() {
            return Err(format!(
                "{}: source `{name}` states `series` column `{c}`, which is not in its schema.",
                path.display()
            ));
        }
        if Some(c) == shape.time_column.as_ref() {
            return Err(format!(
                "{}: source `{name}` states `{c}` as both its time column and a series column.",
                path.display()
            ));
        }
    }
    Ok(())
}

fn check_name(name: &str, path: &Path) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    if !ok {
        return Err(format!(
            "{}: `{name}` is not usable as a table name. Letters, digits and underscores, \
             starting with a letter.",
            path.display()
        ));
    }
    if name.matches('.').count() > 1 {
        return Err(format!(
            "{}: `{name}` has more than one dot. A two-part name gives you a namespace; a \
             three-part one forges a catalog identity.",
            path.display()
        ));
    }
    Ok(())
}

/// Resolved to an **absolute** path before exec. The connector's cwd is a fresh scratch
/// directory, so a path left relative would resolve against that and not be found — and a
/// connector found by luck is a connector someone else can substitute.
fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    // canonicalize also resolves `..` and symlinks, so the permission check below applies to the
    // file that will actually be executed. It fails only if the path does not exist, and that
    // error is better reported by the caller, which knows the source name.
    joined.canonicalize().unwrap_or(joined)
}

fn exec_source(s: Source, name: String, base: &Path, path: &Path) -> Result<ExecSource, String> {
    let command = s.command.ok_or_else(|| {
        format!(
            "{}: source `{}` is an exec source and needs `command`.",
            path.display(),
            name
        )
    })?;
    if s.schema.is_empty() {
        return Err(format!(
            "{}: source `{}` needs a `[[source.schema]]` declaration. The connector does not get \
             to tell us its own schema — its output is a third party's, and column names flow \
             into what the model reads. See docs/EXEC-CONNECTOR-PROTOCOL.md, \"Validation\".",
            path.display(),
            name
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for f in &s.schema {
        // ASCII only, as DuckDB folds: `Ä` and `ä` are two columns to it.
        if !seen.insert(f.name.to_ascii_lowercase()) {
            return Err(format!(
                "{}: source `{}` declares column `{}` twice: DuckDB does not tell names apart by \
                 case.",
                path.display(),
                name,
                f.name
            ));
        }
        if crate::exec::arrow_type(&f.ty).is_none() {
            return Err(format!(
                "{}: source `{}` column `{}` declares type `{}`, which is not in the vocabulary. \
                 See the \"Types\" table in docs/EXEC-CONNECTOR-PROTOCOL.md.",
                path.display(),
                name,
                f.name,
                f.ty
            ));
        }
    }

    let config = toml_to_json(toml::Value::Table(s.config));
    if let Some(bad) = finds_interpolation(&config) {
        return Err(format!(
            "{}: source `{}` has `${{...}}` in `config` ({bad}). Interpolation is only available \
             in `env`: `config` is copied into every input document the connector reads, where \
             it may well be logged. Secrets travel in the environment or not at all.",
            path.display(),
            name
        ));
    }

    let mut env = BTreeMap::new();
    for (k, v) in s.env {
        env.insert(k.clone(), interpolate(&v, &k, &name, path)?);
    }

    let mut limits = Limits::default();
    if let Some(v) = s.max_rows {
        limits.max_rows = v;
    }
    if let Some(v) = s.max_bytes {
        limits.max_bytes = v;
    }
    if let Some(v) = s.deadline_secs {
        limits.deadline = std::time::Duration::from_secs(v);
    }

    let narrowing = match s.narrow {
        Some(n) => build_narrowing(n, &name, path)?,
        None => crate::exec::Narrowing::default(),
    };

    Ok(ExecSource {
        narrowing,
        name,
        command: resolve(base, &command),
        args: s.args,
        env,
        config,
        schema: s.schema,
        limits,
        timezone: s.timezone,
    })
}

/// `${VAR}` from the host environment. An unresolvable one **aborts startup**: splicing in an
/// empty string turns a missing credential into an anonymous connection attempt, and the error
/// you get back is from the wrong layer.
fn interpolate(v: &str, key: &str, source: &str, path: &Path) -> Result<String, String> {
    let mut out = String::with_capacity(v.len());
    let mut rest = v;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find('}') else {
            return Err(format!(
                "{}: source `{source}` env `{key}` has an unclosed `${{`.",
                path.display()
            ));
        };
        let name = after[..j].trim();
        let got = std::env::var(name).map_err(|_| {
            format!(
                "{}: source `{source}` env `{key}` needs ${{{name}}}, and {name} is not set in \
                 benchy's environment. Set it, or remove the entry — benchy will not start with \
                 a credential it cannot resolve.",
                path.display()
            )
        })?;
        out.push_str(&got);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn finds_interpolation(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if s.contains("${") => Some(s.clone()),
        serde_json::Value::Array(a) => a.iter().find_map(finds_interpolation),
        serde_json::Value::Object(o) => o.values().find_map(finds_interpolation),
        _ => None,
    }
}

fn toml_to_json(v: toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s),
        toml::Value::Integer(i) => serde_json::Value::from(i),
        toml::Value::Float(f) => serde_json::Value::from(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => {
            serde_json::Value::Object(t.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect())
        }
    }
}

/// Turn the operator's `[source.narrow]` into the narrowing sent to the connector — and, just as
/// importantly, re-applied after ingest. A connector may declare `enforced` and be wrong; the
/// host filtering afterwards costs almost nothing and means a buggy connector costs bytes rather
/// than correctness.
fn build_narrowing(n: Narrow, source: &str, path: &Path) -> Result<crate::exec::Narrowing, String> {
    let bad = |m: &str| format!("{}: source `{source}`: {m}", path.display());

    let time =
        match (&n.column, &n.last, &n.start, &n.end) {
            (None, None, None, None) => None,
            (None, _, _, _) => return Err(bad(
                "[source.narrow] sets a time window but no `column`. Name the timestamp column \
                 the window applies to.",
            )),
            (Some(c), Some(last), None, None) => {
                let d = parse_duration(last).ok_or_else(|| {
                    bad(&format!(
                    "`last = \"{last}\"` is not a duration. Use a number and one of d, h or m — \
                     for example \"30d\"."
                ))
                })?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| bad("the system clock is before 1970"))?;
                let start = now.as_micros() as i64 - d.as_micros() as i64;
                // RFC 3339, as the protocol promises: `T`, and `Z` rather than `+00`.
                let rendered = crate::result::timestamp_utc(start).replace(' ', "T");
                let rfc3339 = format!("{}Z", rendered.trim_end_matches("+00"));
                Some((c.clone(), Some(rfc3339), None))
            }
            (Some(c), None, start, end) => {
                if start.is_none() && end.is_none() {
                    return Err(bad(
                        "[source.narrow] names a `column` but no window. Add `last`, or `start` \
                     and/or `end`.",
                    ));
                }
                Some((c.clone(), start.clone(), end.clone()))
            }
            (Some(_), Some(_), _, _) => return Err(bad(
                "[source.narrow] sets both `last` and an explicit `start`/`end`. Pick one — two \
                 ways to say the same thing is two ways to disagree.",
            )),
        };

    let entities =
        match (&n.entity_column, n.entity_ids.is_empty()) {
            (Some(c), false) => Some((c.clone(), n.entity_ids.clone())),
            (None, true) => None,
            (Some(_), true) => return Err(bad(
                "`entity_column` is set but `entity_ids` is empty. An empty allowlist would load \
                 nothing, which is more likely a mistake than an intention.",
            )),
            (None, false) => return Err(bad("`entity_ids` is set but `entity_column` is not.")),
        };

    Ok(crate::exec::Narrowing {
        time,
        entities,
        columns: (!n.columns.is_empty()).then_some(n.columns),
        params: serde_json::Map::new(),
    })
}

/// `30d`, `24h`, `90m`. Deliberately three units: a window an operator has to compute in seconds
/// is a window an operator gets wrong.
fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: u64 = n.parse().ok()?;
    let secs = match unit {
        "d" => n.checked_mul(86_400)?,
        "h" => n.checked_mul(3_600)?,
        "m" => n.checked_mul(60)?,
        _ => return None,
    };
    Some(std::time::Duration::from_secs(secs))
}
