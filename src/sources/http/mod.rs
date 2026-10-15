//! REQ-17: HTTP as a built-in source — a URL that answers with Arrow, Parquet, CSV or NDJSON, or
//! a JSON API that answers in pages. The design, and the measurements behind it, are
//! [docs/HTTP-SOURCE.md](../../../docs/HTTP-SOURCE.md).
//!
//! The operator's config is read and checked here ([`spec`]); [`client`] makes the requests —
//! the deadline, retries, redirects, decoding, redaction; [`paginate`] says where a paged API's
//! next page is; and `load` puts each format on the bench by its fastest path, ending where a
//! file or a stream ends. It runs in a loader of its own, which holds this source's `env` and no
//! other's (REQ-5), as ClickHouse's does.

pub mod client;
mod load;
pub mod paginate;
mod skim;

use std::collections::BTreeMap;

/// What the body holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// An Arrow IPC stream, read by the reader an exec connector's stdout goes through.
    Arrow,
    Parquet,
    Csv,
    Ndjson,
    /// JSON pages, the rows at a path in each.
    Json,
}

impl Format {
    pub const NAMES: &'static [&'static str] = &["arrow", "parquet", "csv", "ndjson", "json"];

    pub fn parse(s: &str) -> Option<Format> {
        Some(match s {
            "arrow" => Format::Arrow,
            "parquet" => Format::Parquet,
            "csv" => Format::Csv,
            "ndjson" => Format::Ndjson,
            "json" => Format::Json,
            _ => return None,
        })
    }
}

/// How the body is compressed, beyond what `Content-Encoding` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    /// As `Content-Encoding` says: identity, gzip or zstd.
    Auto,
    /// The body is a gzip or zstd file whatever the header says, such as `readings.csv.gz`
    /// served as `application/gzip`.
    Gzip,
    Zstd,
    None,
}

/// `auth = { … }`: how the source's secret, from its `env`, goes on each request. One form.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthTable {
    /// `Authorization: Bearer <env[bearer]>`.
    bearer: Option<String>,
    /// A header of the operator's naming, such as `X-Api-Key`, from `env[env]`.
    header: Option<String>,
    /// A query parameter of the operator's naming, from `env[env]`. Redacted wherever a URL is
    /// logged.
    param: Option<String>,
    env: Option<String>,
    /// Basic: `env[user]` and `env[password]`.
    user: Option<String>,
    password: Option<String>,
}

/// The validated form: each names the `env` keys it reads, never a value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Auth {
    None,
    Bearer { env: String },
    Header { name: String, env: String },
    Param { name: String, env: String },
    Basic { user: String, password: String },
}

impl Auth {
    fn env_keys(&self) -> Vec<&str> {
        match self {
            Auth::None => vec![],
            Auth::Bearer { env } | Auth::Header { env, .. } | Auth::Param { env, .. } => vec![env],
            Auth::Basic { user, password } => vec![user, password],
        }
    }
}

/// `[source.pages]`: where a JSON page keeps its rows, and how to ask for the next one.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PagesTable {
    rows: Option<String>,
    next: Option<String>,
    cursor: Option<String>,
    cursor_param: Option<String>,
    url: Option<String>,
    offset_param: Option<String>,
    limit_param: Option<String>,
    page_param: Option<String>,
    first_page: Option<u64>,
    size_param: Option<String>,
    page_size: Option<u64>,
    max_pages: Option<u64>,
}

/// A dotted path into a JSON page, `meta.next_cursor`; empty for the page itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JsonPath(pub Vec<String>);

impl JsonPath {
    pub fn parse(s: &str) -> Result<JsonPath, String> {
        if s.is_empty() {
            return Ok(JsonPath(Vec::new()));
        }
        let parts: Vec<String> = s.split('.').map(str::to_string).collect();
        if parts
            .iter()
            .any(|p| p.is_empty() || p.chars().any(char::is_control))
        {
            return Err(format!(
                "`{s}` is not a path: keys separated by dots, such as \"meta.next_cursor\"."
            ));
        }
        Ok(JsonPath(parts))
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Pages {
    pub rows: JsonPath,
    pub style: paginate::Style,
    /// Where the cursor or the next URL is, for those styles.
    pub at: Option<JsonPath>,
    pub max_pages: u64,
}

/// `[source.pushdown]`: the query parameters the operator's narrowing is sent as. Which values
/// they carry is the operator's or the host's narrowing, never the model's.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pushdown {
    pub start: Option<String>,
    pub end: Option<String>,
    pub time_format: Option<String>,
    pub columns: Option<String>,
    pub entities: Option<String>,
}

pub const TIME_FORMATS: &[&str] = &["rfc3339", "unix", "unix_ms", "date"];

/// `rate_limit = { requests = 10, per_secs = 1 }`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub requests: u32,
    pub per_secs: u64,
}

/// What the config said, before it is checked: every key an `http` source may have.
#[derive(Debug, Default)]
pub struct Raw {
    pub name: String,
    pub url: Option<String>,
    pub method: Option<String>,
    pub body: Option<String>,
    pub format: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub params: BTreeMap<String, String>,
    pub auth: Option<AuthTable>,
    /// Already interpolated.
    pub env: BTreeMap<String, String>,
    pub compression: Option<String>,
    pub pages: Option<PagesTable>,
    pub pushdown: Option<Pushdown>,
    pub retries: Option<u32>,
    pub rate_limit: Option<RateLimit>,
    pub redirect_hosts: Vec<String>,
    pub delimiter: Option<String>,
    pub schema: Vec<crate::exec::FieldDecl>,
    pub narrowing: crate::exec::Narrowing,
    pub limits: crate::exec::Limits,
    pub timezone: Option<String>,
}

/// A checked `http` source, as the config declares it and a loader's job carries it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Spec {
    pub name: String,
    pub url: String,
    pub post: Option<String>,
    pub format: Format,
    pub headers: Vec<(String, String)>,
    pub params: Vec<(String, String)>,
    pub auth: Auth,
    pub env: BTreeMap<String, String>,
    pub compression: Compression,
    pub pages: Option<Pages>,
    pub pushdown: Pushdown,
    pub retries: u32,
    pub rate_limit: Option<RateLimit>,
    pub redirect_hosts: Vec<String>,
    /// A CSV's delimiter, as the operator stated it: stated to DuckDB's reader for every CSV, and
    /// `,` for a declared one when unset (`load::Loader::reader_of`).
    pub delimiter: Option<char>,
    pub schema: Vec<crate::exec::FieldDecl>,
    pub narrowing: crate::exec::Narrowing,
    pub limits: crate::exec::Limits,
    pub timezone: Option<String>,
}

/// The most retries one request may have. Each waits longer than the one before, inside the
/// source's one deadline, so more would only wait out the deadline.
pub const MAX_RETRIES: u32 = 10;
/// The default: enough to ride out a deploy or a 429, few enough not to hammer a server that is
/// down.
pub const RETRIES: u32 = 3;
pub const MAX_PAGES: u64 = 10_000;

/// Headers benchy sets itself, or that carry credentials and so belong in `auth`.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "proxy-authorization", "cookie"];
const BENCHYS_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "accept-encoding",
    "te",
    "upgrade",
];

/// A CSV's `delimiter`, as the operator wrote it: one character, not the quote or a line break.
/// An HTTP source's and a file source's alike.
pub fn delimiter(d: &str) -> Result<char, String> {
    let mut cs = d.chars();
    match (cs.next(), cs.next()) {
        (Some('"'), None) => Err("`delimiter` cannot be the quote, `\"`.".into()),
        (Some('\n' | '\r'), None) => Err("`delimiter` cannot be a line break.".into()),
        (Some(c), None) => Ok(c),
        _ => Err(format!(
            "`delimiter = {d:?}`: it is one character, such as \";\" or \"\\t\"."
        )),
    }
}

/// Check an `http` source's config: every key it has must apply, every secret must come from
/// `env` through `auth`, and every parameter benchy sets is benchy's. Errors name the key and
/// the fix, and never a secret's value.
pub fn spec(r: Raw) -> Result<Spec, String> {
    let url = r.url.ok_or(
        "an http source needs `url`, such as \"https://data.example.com/readings.parquet\".",
    )?;
    check_url(&url)?;
    let format = match r.format.as_deref() {
        None => {
            return Err(format!(
                "an http source needs `format`: one of {}.",
                quoted(Format::NAMES)
            ))
        }
        Some(f) => Format::parse(f).ok_or_else(|| {
            format!(
                "`format = \"{f}\"` is not one of {}.",
                quoted(Format::NAMES)
            )
        })?,
    };
    let post = match (r.method.as_deref().unwrap_or("GET"), r.body) {
        ("GET", None) => None,
        ("GET", Some(_)) => {
            return Err("`body` is sent only with `method = \"POST\"`.".into());
        }
        ("POST", body) => Some(body.unwrap_or_default()),
        (m, _) => return Err(format!("`method = \"{m}\"`: it is \"GET\" or \"POST\".")),
    };
    if post.as_deref().is_some_and(|b| b.contains("${")) {
        return Err(no_interpolation("body"));
    }

    let mut headers = Vec::new();
    for (k, v) in r.headers {
        let lower = k.to_ascii_lowercase();
        if k.is_empty()
            || !k
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(format!("header `{k}` is not a header name."));
        }
        if CREDENTIAL_HEADERS.contains(&lower.as_str()) {
            return Err(format!(
                "header `{k}` carries a credential, and `headers` are written in this file in \
                 the clear. Put the secret in `env` and name it in `auth`, such as \
                 auth = {{ bearer = \"TOKEN\" }}."
            ));
        }
        if BENCHYS_HEADERS.contains(&lower.as_str()) {
            return Err(format!("header `{k}` is benchy's to set."));
        }
        if v.contains("${") {
            return Err(no_interpolation(&format!("header `{k}`")));
        }
        if v.chars().any(|c| c.is_control()) {
            return Err(format!("header `{k}` holds a control character."));
        }
        headers.push((k, v));
    }

    let auth = auth(r.auth.unwrap_or_default())?;
    for k in r.env.keys() {
        if !auth.env_keys().contains(&k.as_str()) {
            return Err(format!(
                "env `{k}` would be handed to nothing: an http source reads its `env` only \
                 through `auth`, which names {}.",
                match auth.env_keys().as_slice() {
                    [] => "no key".to_string(),
                    keys => quoted(keys),
                }
            ));
        }
    }
    for k in auth.env_keys() {
        if !r.env.contains_key(k) {
            return Err(format!(
                "`auth` reads env `{k}`, which this source's `env` does not set."
            ));
        }
    }
    if let Auth::Header { name, .. } = &auth {
        let lower = name.to_ascii_lowercase();
        if BENCHYS_HEADERS.contains(&lower.as_str())
            || headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(name))
        {
            return Err(format!("`auth` header `{name}` is also set elsewhere."));
        }
    }

    let compression = match r.compression.as_deref() {
        None | Some("auto") => Compression::Auto,
        Some("gzip") => Compression::Gzip,
        Some("zstd") => Compression::Zstd,
        Some("none") => Compression::None,
        Some(c) => {
            return Err(format!(
                "`compression = \"{c}\"`: it is \"auto\" (the default, as Content-Encoding \
                 says), \"gzip\", \"zstd\" or \"none\"."
            ))
        }
    };

    let delimiter = match (format, r.delimiter) {
        (_, None) => None,
        (Format::Csv, Some(d)) => Some(delimiter(&d)?),
        (_, Some(_)) => {
            return Err("`delimiter` applies only to `format = \"csv\"`.".into());
        }
    };

    let pages =
        match (format, r.pages) {
            (Format::Json, None) => return Err(
                "a `json` source needs [source.pages] with `rows`, the path to the rows in each \
                 page (\"\" when the page is the array)."
                    .into(),
            ),
            (Format::Json, Some(p)) => Some(pages(p)?),
            (_, Some(_)) => {
                return Err("[source.pages] applies only to `format = \"json\"`.".into());
            }
            (_, None) => None,
        };

    // Every parameter benchy sets: none may be set twice, or set by the operator as well.
    let mut ours: Vec<(&str, String)> = Vec::new();
    if let Auth::Param { name, .. } = &auth {
        ours.push(("auth param", name.clone()));
    }
    if let Some(p) = &pages {
        use paginate::Style;
        match &p.style {
            Style::Cursor { param } => ours.push(("cursor_param", param.clone())),
            Style::Offset {
                offset_param,
                limit_param,
                ..
            } => {
                ours.push(("offset_param", offset_param.clone()));
                ours.push(("limit_param", limit_param.clone()));
            }
            Style::Page {
                page_param, size, ..
            } => {
                ours.push(("page_param", page_param.clone()));
                if let Some((s, _)) = size {
                    ours.push(("size_param", s.clone()));
                }
            }
            _ => {}
        }
    }
    let pushdown = r.pushdown.unwrap_or_default();
    pushdown_fits(&pushdown, &r.narrowing, &r.schema)?;
    for (key, v) in [
        ("pushdown.start", &pushdown.start),
        ("pushdown.end", &pushdown.end),
        ("pushdown.columns", &pushdown.columns),
        ("pushdown.entities", &pushdown.entities),
    ] {
        if let Some(v) = v {
            ours.push((key, v.clone()));
        }
    }
    for (i, (key, p)) in ours.iter().enumerate() {
        if p.is_empty() {
            return Err(format!("`{key}` names an empty parameter."));
        }
        if let Some((other, _)) = ours[..i].iter().find(|(_, q)| q == p) {
            return Err(format!(
                "`{other}` and `{key}` both set the parameter `{p}`."
            ));
        }
        if r.params.contains_key(p) {
            return Err(format!(
                "`params` sets `{p}`, which `{key}` sets on every request. Leave it out of \
                 `params`."
            ));
        }
    }
    let mut params = Vec::new();
    for (k, v) in r.params {
        if k.is_empty() {
            return Err("`params` has an empty name.".into());
        }
        if v.contains("${") {
            return Err(no_interpolation(&format!("param `{k}`")));
        }
        params.push((k, v));
    }

    let retries = r.retries.unwrap_or(RETRIES);
    if retries > MAX_RETRIES {
        return Err(format!(
            "`retries = {retries}`: at most {MAX_RETRIES}. Every retry waits inside the source's \
             one `deadline_secs`, so more would only wait it out."
        ));
    }
    if let Some(rl) = r.rate_limit {
        if rl.requests == 0 || rl.per_secs == 0 {
            return Err("`rate_limit` of zero would never send a request.".into());
        }
    }
    for h in &r.redirect_hosts {
        if redirect_host(h).is_none() {
            return Err(format!(
                "`redirect_hosts` entry `{h}` is not a host name: a name, and `:port` when it is \
                 not the scheme's own; no scheme or path."
            ));
        }
    }

    let mut seen = std::collections::HashSet::new();
    for f in &r.schema {
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
    if let Some((_, start, end)) = &r.narrowing.time {
        for t in [start, end].into_iter().flatten() {
            if crate::sources::clickhouse::rfc3339_micros(t).is_none() {
                return Err(format!(
                    "`{t}` is not an RFC 3339 time with an offset, such as 2026-01-01T00:00:00Z."
                ));
            }
        }
    }
    if let Some((_, ids)) = &r.narrowing.entities {
        crate::exec::vet_entities(ids).map_err(|f| f.detail)?;
    }
    if !r.schema.is_empty() && r.narrowing.columns.is_some() {
        return Err(
            "it pins columns in `schema` and names them in `narrow.columns`. Pinned columns are \
             the ones loaded; keep one of the two."
                .into(),
        );
    }

    Ok(Spec {
        name: r.name,
        url,
        post,
        format,
        headers,
        params,
        auth,
        env: r.env,
        compression,
        pages,
        pushdown,
        retries,
        rate_limit: r.rate_limit,
        redirect_hosts: r.redirect_hosts,
        delimiter,
        schema: r.schema,
        narrowing: r.narrowing,
        limits: r.limits,
        timezone: r.timezone,
    })
}

/// The URL reaches logs and errors, so it carries no user, password, query string or fragment:
/// literal parameters go in `params`, a secret one in `auth = { param = … }`.
pub fn check_url(url: &str) -> Result<(), String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| format!("`url = \"{url}\"` is not a URL: {e}"))?;
    if !matches!(uri.scheme_str(), Some("http") | Some("https")) {
        return Err(format!(
            "`url = \"{url}\"` must start with http:// or https://."
        ));
    }
    let Some(authority) = uri.authority() else {
        return Err(format!("`url = \"{url}\"` names no host."));
    };
    if authority.as_str().contains('@') {
        return Err(
            "`url` carries a user or password. A URL reaches logs and error messages; put the \
             secret in `env` and name it in `auth`, such as auth = { user = \"USER\", password = \
             \"PASSWORD\" }."
                .into(),
        );
    }
    if uri.query().is_some() || url.contains('#') {
        return Err(
            "`url` has a query string. A URL reaches logs, and a query string is where tokens \
             hide: put literal parameters in `params`, and a secret one in `env`, named by \
             auth = { param = \"…\", env = \"…\" }."
                .into(),
        );
    }
    Ok(())
}

fn auth(a: AuthTable) -> Result<Auth, String> {
    let named = |k: &Option<String>, what: &str| -> Result<(), String> {
        match k {
            Some(v) if v.is_empty() => Err(format!("`auth` `{what}` is empty.")),
            _ => Ok(()),
        }
    };
    for (k, w) in [
        (&a.bearer, "bearer"),
        (&a.header, "header"),
        (&a.param, "param"),
        (&a.env, "env"),
        (&a.user, "user"),
        (&a.password, "password"),
    ] {
        named(k, w)?;
    }
    let forms = [
        a.bearer.is_some(),
        a.header.is_some(),
        a.param.is_some(),
        a.user.is_some() || a.password.is_some(),
    ];
    let one_of = "`auth` is one of { bearer = \"ENV_KEY\" }, { header = \"X-Api-Key\", env = \
                  \"ENV_KEY\" }, { param = \"api_key\", env = \"ENV_KEY\" } or { user = \
                  \"ENV_KEY\", password = \"ENV_KEY\" }: each names keys of this source's `env`.";
    if forms.iter().filter(|f| **f).count() > 1 {
        return Err(format!("`auth` has more than one form. {one_of}"));
    }
    Ok(match a {
        AuthTable {
            bearer: Some(env),
            env: None,
            ..
        } => Auth::Bearer { env },
        AuthTable {
            header: Some(name),
            env: Some(env),
            ..
        } => {
            if !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                return Err(format!("`auth` header `{name}` is not a header name."));
            }
            Auth::Header { name, env }
        }
        AuthTable {
            param: Some(name),
            env: Some(env),
            ..
        } => Auth::Param { name, env },
        AuthTable {
            user: Some(user),
            password: Some(password),
            env: None,
            ..
        } => Auth::Basic { user, password },
        AuthTable {
            bearer: None,
            header: None,
            param: None,
            user: None,
            password: None,
            env: None,
        } => Auth::None,
        _ => return Err(one_of.into()),
    })
}

fn pages(p: PagesTable) -> Result<Pages, String> {
    use paginate::Style;
    let rows = JsonPath::parse(p.rows.as_deref().ok_or(
        "[source.pages] needs `rows`, the path to the rows in each page (\"\" when the page is \
         the array).",
    )?)?;
    let param = |v: &Option<String>, key: &str, style: &str| -> Result<String, String> {
        v.clone()
            .ok_or_else(|| format!("[source.pages] next = \"{style}\" needs `{key}`."))
    };
    let next = p.next.as_deref().unwrap_or("none");
    // Each key, whether it is set, and the styles it applies to.
    let keys: &[(&str, bool, &[&str])] = &[
        ("cursor", p.cursor.is_some(), &["cursor"]),
        ("cursor_param", p.cursor_param.is_some(), &["cursor"]),
        ("url", p.url.is_some(), &["url"]),
        ("offset_param", p.offset_param.is_some(), &["offset"]),
        ("limit_param", p.limit_param.is_some(), &["offset"]),
        ("page_param", p.page_param.is_some(), &["page"]),
        ("first_page", p.first_page.is_some(), &["page"]),
        ("size_param", p.size_param.is_some(), &["page"]),
        ("page_size", p.page_size.is_some(), &["offset", "page"]),
        (
            "max_pages",
            p.max_pages.is_some(),
            &["cursor", "offset", "page", "link", "url"],
        ),
    ];
    if let Some((key, _, styles)) = keys.iter().find(|(_, set, s)| *set && !s.contains(&next)) {
        return Err(format!(
            "[source.pages] `{key}` applies only to next = {}; it would do nothing with next = \
             \"{next}\", so it is refused rather than ignored.",
            quoted(styles)
        ));
    }
    let mut at = None;
    let style = match next {
        "none" => Style::Single,
        "cursor" => {
            at = Some(JsonPath::parse(&param(&p.cursor, "cursor", next)?)?);
            Style::Cursor {
                param: param(&p.cursor_param, "cursor_param", next)?,
            }
        }
        "url" => {
            at = Some(JsonPath::parse(&param(&p.url, "url", next)?)?);
            Style::NextUrl
        }
        "link" => Style::Link,
        "offset" => {
            let page_size = p
                .page_size
                .ok_or("[source.pages] next = \"offset\" needs `page_size`.")?;
            if page_size == 0 {
                return Err("[source.pages] `page_size = 0` would fetch nothing.".into());
            }
            Style::Offset {
                offset_param: param(&p.offset_param, "offset_param", next)?,
                limit_param: param(&p.limit_param, "limit_param", next)?,
                page_size,
            }
        }
        "page" => {
            let size = match (&p.size_param, p.page_size) {
                (Some(s), Some(n)) if n > 0 => Some((s.clone(), n)),
                (None, None) => None,
                _ => {
                    return Err(
                        "[source.pages] `size_param` and `page_size` go together, and a page \
                         size is more than 0."
                            .into(),
                    )
                }
            };
            Style::Page {
                page_param: param(&p.page_param, "page_param", next)?,
                first: p.first_page.unwrap_or(1),
                size,
            }
        }
        other => {
            return Err(format!(
                "[source.pages] next = \"{other}\" is not one of \"cursor\", \"offset\", \
                 \"page\", \"link\" or \"url\"; leave it out for a single response."
            ))
        }
    };
    if let Some(at) = &at {
        let (a, b) = (&rows.0, &at.0);
        if a.is_empty() {
            return Err(format!(
                "[source.pages] rows = \"\" says the page is the array of rows, which leaves no \
                 key for next = \"{next}\" to keep its {next} in."
            ));
        }
        if a.starts_with(b) || b.starts_with(a) {
            return Err(format!(
                "[source.pages] `rows` and `{next}` overlap: one is inside the other."
            ));
        }
    }
    let max_pages = match p.max_pages {
        Some(0) => return Err("[source.pages] `max_pages = 0` would fetch nothing.".into()),
        Some(n) => n,
        None if style == Style::Single => 1,
        None => MAX_PAGES,
    };
    Ok(Pages {
        rows,
        style,
        at,
        max_pages,
    })
}

/// Each pushdown parameter needs the narrowing whose values it carries: a parameter with no value
/// would be sent empty, or not at all, and the operator would think the API narrows.
fn pushdown_fits(
    p: &Pushdown,
    n: &crate::exec::Narrowing,
    schema: &[crate::exec::FieldDecl],
) -> Result<(), String> {
    let window = n.time.as_ref();
    if p.start.is_some() && window.is_none() {
        return Err(
            "`pushdown.start` sends the window's start, and [source.narrow] sets no window.".into(),
        );
    }
    if p.end.is_some() && window.is_none() {
        return Err(
            "`pushdown.end` sends the window's end, and [source.narrow] sets no window.".into(),
        );
    }
    if p.end.is_some() && window.is_some_and(|(_, _, end)| end.is_none()) {
        return Err(
            "`pushdown.end` sends the window's end, and the window has none (it is `last`, or \
             `start` alone)."
                .into(),
        );
    }
    if p.start.is_some() && window.is_some_and(|(_, start, _)| start.is_none()) {
        return Err("`pushdown.start` sends the window's start, and the window has none.".into());
    }
    match &p.time_format {
        Some(_) if p.start.is_none() && p.end.is_none() => {
            return Err(
                "`pushdown.time_format` formats `pushdown.start` and `end`, and neither is set."
                    .into(),
            )
        }
        Some(f) if !TIME_FORMATS.contains(&f.as_str()) => {
            return Err(format!(
                "`pushdown.time_format = \"{f}\"` is not one of {}.",
                quoted(TIME_FORMATS)
            ))
        }
        _ => {}
    }
    if p.columns.is_some() && n.columns.is_none() && schema.is_empty() {
        return Err(
            "`pushdown.columns` sends the columns to fetch, and neither [source.narrow] \
             `columns` nor a schema names any."
                .into(),
        );
    }
    if p.entities.is_some() && n.entities.is_none() {
        return Err(
            "`pushdown.entities` sends the entity allowlist, and [source.narrow] sets none.".into(),
        );
    }
    Ok(())
}

fn no_interpolation(what: &str) -> String {
    format!(
        "{what} has `${{…}}`. Interpolation is only available in `env`, and a secret travels \
         from there through `auth`."
    )
}

fn quoted(names: &[&str]) -> String {
    let q: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    match q.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

impl Spec {
    /// What the operator log calls it: the method, the URL as configured (which carries no
    /// secret and no query) and the format. Never the agent.
    pub fn origin(&self) -> String {
        format!(
            "http {} {} ({})",
            if self.post.is_some() { "POST" } else { "GET" },
            self.url,
            Format::NAMES[self.format as usize]
        )
    }

    /// The URL every request starts from: the operator's, its literal `params`, and the
    /// narrowing pushed down in the parameters `[source.pushdown]` names. Paging parameters go on
    /// it per page; the `auth` parameter per request, never here, so it is in nothing logged.
    pub fn base_url(&self) -> String {
        let mut u = self.url.clone();
        for (k, v) in self.params.iter().cloned().chain(self.pushed()) {
            u = paginate::set_param(&u, &k, &v);
        }
        u
    }

    /// The pushdown parameters and their values: the window in `time_format`, the entities and
    /// the columns comma-joined. Values only ever go on the URL, percent-encoded.
    pub fn pushed(&self) -> Vec<(String, String)> {
        let p = &self.pushdown;
        let mut out = Vec::new();
        if let Some((_, start, end)) = &self.narrowing.time {
            let fmt = p.time_format.as_deref().unwrap_or("rfc3339");
            for (param, value) in [(&p.start, start), (&p.end, end)] {
                if let (Some(k), Some(v)) = (param, value) {
                    out.push((k.clone(), time_param(v, fmt)));
                }
            }
        }
        if let (Some(k), Some((_, ids))) = (&p.entities, &self.narrowing.entities) {
            out.push((k.clone(), ids.join(",")));
        }
        if let Some(k) = &p.columns {
            let cols: Vec<&str> = match &self.narrowing.columns {
                Some(c) => c.iter().map(String::as_str).collect(),
                None => self.schema.iter().map(|f| f.name.as_str()).collect(),
            };
            out.push((k.clone(), cols.join(",")));
        }
        out
    }

    /// Put this source on the bench, within what the session has left. Every fault is redacted
    /// of this source's secrets before it leaves.
    pub fn load(
        &self,
        bench: &mut crate::workspace::Bench,
        budget: &crate::admission::Budget,
    ) -> Result<(crate::workspace::LoadedTable, crate::exec::Fetched), crate::exec::Fault> {
        let started = std::time::Instant::now();
        let client = client::Client::new(self);
        let loaded = crate::trace::span("http.fetch", || {
            load::load(self, &client, bench, budget, false)
        })
        .map_err(|f| client.redact.fault(f))?;
        eprintln!(
            "benchy: source `{}`: {} row(s), {} byte(s) decoded, {} byte(s) on the wire, {} \
             page(s), {} request(s) ({} retried), in {:.2}s",
            self.name,
            loaded.fetched.rows,
            loaded.fetched.bytes,
            client.wire.load(std::sync::atomic::Ordering::Relaxed),
            loaded.pages,
            client.requests.load(std::sync::atomic::Ordering::Relaxed),
            client.retried.load(std::sync::atomic::Ordering::Relaxed),
            started.elapsed().as_secs_f64()
        );
        let table = crate::source::record(bench, loaded.table)?;
        Ok((table, loaded.fetched))
    }

    /// `benchy-mcp check`: the request as it is sent, the secret masked; one load of the body,
    /// or of a paged API's first page alone, into a bench of its own; and the schema that load
    /// read, as `[[source.schema]]` entries to pin.
    pub fn check(&self) -> Vec<crate::exec::Check> {
        use crate::exec::Check;
        let client = client::Client::new(self);
        let mut out = Vec::new();
        let first = match &self.pages {
            Some(p) => paginate::Pager::new(p.style.clone(), &self.base_url(), p.max_pages)
                .map(|mut pg| pg.first())
                .unwrap_or_else(|_| self.base_url()),
            None => self.base_url(),
        };
        let shown = match &self.auth {
            Auth::Param { name, .. } => format!(
                "{first}{}{}=***",
                if first.contains('?') { '&' } else { '?' },
                paginate::encode(name)
            ),
            _ => first,
        };
        let mut said: Vec<String> = self
            .headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect();
        said.push(format!(
            "Accept-Encoding: {}",
            if self.compression == Compression::Auto {
                "zstd, gzip"
            } else {
                "identity"
            }
        ));
        match &self.auth {
            Auth::Bearer { .. } => said.push("Authorization: Bearer ***".into()),
            Auth::Basic { .. } => said.push("Authorization: Basic ***".into()),
            Auth::Header { name, .. } => said.push(format!("{name}: ***")),
            Auth::Param { .. } | Auth::None => {}
        }
        if let Some(body) = &self.post {
            said.push(format!("body: {body}"));
        }
        out.push(Check {
            ok: true,
            warn: false,
            skipped: false,
            label: client.redact.apply(&format!(
                "request: {} {shown}",
                if self.post.is_some() { "POST" } else { "GET" }
            )),
            detail: client.redact.apply(&said.join("\n")),
        });
        let mut bench = match crate::workspace::Bench::open() {
            Ok(b) => b,
            Err(e) => {
                out.push(Check {
                    ok: false,
                    warn: false,
                    skipped: false,
                    label: "fetch".into(),
                    detail: format!("could not open a bench to load into: {e}"),
                });
                return out;
            }
        };
        let dir = bench.dir().to_path_buf();
        let budget = crate::admission::Budget::default();
        match load::load(self, &client, &mut bench, &budget, true) {
            Ok(l) => {
                out.push(Check {
                    ok: true,
                    warn: false,
                    skipped: false,
                    label: format!(
                        "fetch: {} row(s) {}, {} byte(s) decoded",
                        l.fetched.rows,
                        if self.pages.is_some() {
                            "on the first page"
                        } else {
                            "in the body"
                        },
                        l.fetched.bytes
                    ),
                    detail: String::new(),
                });
                let pins: Vec<String> = l
                    .read_as
                    .iter()
                    .map(|d| {
                        format!(
                            "[[source.schema]]\nname = {}\ntype = \"{}\"",
                            toml::Value::String(d.name.clone()),
                            d.ty
                        )
                    })
                    .collect();
                out.push(Check {
                    ok: true,
                    warn: l.read_as.len() < l.table.columns.len(),
                    skipped: false,
                    label: format!(
                        "schema: {} column(s) of `{}`, as read; to pin them:",
                        l.read_as.len(),
                        self.name
                    ),
                    detail: pins.join("\n"),
                });
            }
            Err(f) => {
                let f = client.redact.fault(f);
                out.push(Check {
                    ok: false,
                    warn: false,
                    skipped: false,
                    label: "fetch".into(),
                    detail: crate::exec::advice_for(&f),
                });
                out.push(crate::exec::skipped("schema", "fetch"));
            }
        }
        drop(bench);
        let _ = std::fs::remove_dir_all(dir);
        out
    }
}

/// A `redirect_hosts` entry: the host, and the port it names, if it names one. A listed host is
/// reached on that port, or on its scheme's own when it names none, and on no other: the loader
/// is confined to the ports it is told of (REQ-3), so an entry says each one.
pub fn redirect_host(entry: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = match entry.split_once(':') {
        Some((h, p)) => (h, Some(p.parse::<u16>().ok().filter(|p| *p > 0)?)),
        None => (entry, None),
    };
    let ok = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b));
    ok.then_some((host, port))
}

impl Spec {
    /// The ports this source's loader may connect to: its URL's, and each redirect host's — the
    /// one it names, or 443, and 80 too when the source itself is plain http, since a redirect
    /// never goes from https down to http.
    pub fn ports(&self) -> Vec<u16> {
        let mut out: Vec<u16> = crate::sources::port_of(&self.url).into_iter().collect();
        for h in &self.redirect_hosts {
            match redirect_host(h) {
                Some((_, Some(p))) => out.push(p),
                Some((_, None)) => {
                    out.push(443);
                    if self.url.starts_with("http://") {
                        out.push(80);
                    }
                }
                None => {}
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether `next`, a URL off this source's origin, is on a listed redirect host and port.
    pub fn redirect_listed(&self, next: &str) -> bool {
        let Some((scheme, host)) = paginate::host_of(next) else {
            return false;
        };
        let Some(port) = crate::sources::port_of(next) else {
            return false;
        };
        if scheme != "https" && !self.url.starts_with("http://") {
            return false;
        }
        let default = if scheme == "https" { 443 } else { 80 };
        self.redirect_hosts.iter().any(|e| match redirect_host(e) {
            Some((h, p)) => h.eq_ignore_ascii_case(&host) && p.unwrap_or(default) == port,
            None => false,
        })
    }
}

/// A window's bound as `[source.pushdown] time_format` says: as written (RFC 3339), seconds or
/// milliseconds since the epoch, or the UTC day it falls on.
fn time_param(v: &str, fmt: &str) -> String {
    let Some(us) = crate::sources::clickhouse::rfc3339_micros(v) else {
        return v.to_string();
    };
    match fmt {
        "unix" => us.div_euclid(1_000_000).to_string(),
        "unix_ms" => us.div_euclid(1_000).to_string(),
        "date" => crate::result::timestamp_utc(us)[..10].to_string(),
        _ => v.to_string(),
    }
}
