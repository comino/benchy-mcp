//! REQ-16: ClickHouse as a built-in source (`type = "clickhouse"`, docs/SOURCES.md).
//!
//! benchy asks ClickHouse's HTTP interface for `FORMAT ArrowStream` and reads the response into
//! the same validated Arrow reader an exec connector's stdout goes through. Three decisions shape
//! the code here, each measured against ClickHouse 26.8:
//!
//! - **The SELECT converts, so what arrives is already canonical.** Each column is cast on the
//!   ClickHouse side to the one type benchy gives it — every instant to `DateTime64(6, 'UTC')`,
//!   every integer to `Int64`, every float to `Float64`, every text-like type to `String` — and
//!   the Arrow settings ask for no dictionaries. Unconverted, a `DateTime` arrived in seconds
//!   and a `Date` as a bare `UInt16`, which the reader refuses (a `DateTime` read as seconds
//!   would also be the wrong instant). The loader's canonical mapping (REQ-15) then holds them
//!   as it holds every source's.
//! - **Values never enter the SQL.** The time window and the entity allowlist travel as
//!   ClickHouse query parameters (`{benchy_start:Int64}`, `param_benchy_start=` on the URL), and
//!   every identifier is backquoted with its escapes. The operator's own `query` is the one text
//!   spliced in, and it is the operator's.
//! - **zstd on the wire to a remote server, decoded under the byte cap.** Asked with
//!   `Accept-Encoding: zstd`, ClickHouse sent the 840,960-row meter table as 1.5 MB instead of
//!   21.9 MB, in the same 0.09 s on one machine; gzip took 0.21 s for 5.4 MB, and Arrow's own LZ4
//!   10.8 MB. Decoding costs the loader 0.3 s of CPU per 240 MB of Arrow (C libzstd,
//!   src/decompress.rs; ruzstd took 0.75 s). To a server on loopback, compression costs CPU at
//!   both ends and saves nothing, so unset it is `none` there ([`Compression::default_for`]).
//!   The decoded bytes are what the cap and the budget count, so compression changes what
//!   crosses the network and nothing about what may land on the bench.
//! - **A read-only user works too.** `readonly = 1`, the usual analytics account, refuses every
//!   setting a request names. Since the SELECT already converts every column, those settings are
//!   a preference, not a need: such a user is sent none, and ClickHouse's own defaults (LZ4
//!   bodies, text as strings) are what the reader takes anyway ([`ReadOnly`]).
//!
//! Credentials: `CLICKHOUSE_USER` and `CLICKHOUSE_PASSWORD` from the source's `env` go in the
//! `X-ClickHouse-User` and `X-ClickHouse-Key` headers — never in the URL, which reaches logs, and
//! never in an error: a URL that carries a user or password is refused at startup. This code runs
//! in a network loader, the one process that holds this source's `env` (REQ-5), confined to its
//! staging directory and TCP to the URL's port (REQ-3
//! `a_network_loader_reaches_only_its_source_and_its_staging`).

use crate::exec::{Check, Fault, Fetched, FieldDecl, Limits, Narrowing};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One table, from one ClickHouse server.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Source {
    /// The table it becomes on the bench.
    pub name: String,
    /// `http://host:8123` or `https://host:8443`, maybe with a path for a proxy in front. Never a
    /// user, a password or a query string: [`check_url`].
    pub url: String,
    pub database: String,
    pub from: From,
    /// Only `CLICKHOUSE_USER` and `CLICKHOUSE_PASSWORD`, resolved by the server from the config.
    pub env: BTreeMap<String, String>,
    /// Columns the operator pinned. Empty: read from ClickHouse and mapped.
    pub schema: Vec<FieldDecl>,
    pub narrowing: Narrowing,
    pub limits: Limits,
    /// How the rows travel.
    pub compression: Compression,
    /// Found on the first request a `readonly = 1` user is refused, and kept for the rest: from
    /// then on benchy sends no settings, and ClickHouse answers with these of its own.
    #[serde(skip)]
    pub readonly: Arc<std::sync::OnceLock<ReadOnly>>,
}

/// REQ-16 `a_read_only_clickhouse_user_loads_what_any_user_loads`. What ClickHouse answers a
/// `readonly = 1` user with, since that user may change no setting: on 26.8, naming one at the
/// value it already has passes, and `output_format_arrow_compression_method = none` or any
/// `max_execution_time` is refused with code 164, in the URL or in a `SETTINGS` clause alike.
/// Everything that matters is in the SELECT already: each column is converted there, so no
/// LowCardinality column and no dictionary is left in the answer. Two settings remain the
/// server's, and 26.8's defaults for them are ones the reader takes: LZ4 Arrow bodies, and text
/// as Arrow strings. A profile that sets either otherwise refuses the load before a row is sent
/// (`refusal`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadOnly {
    /// `output_format_arrow_compression_method`: `lz4_frame` (the default), `zstd` or `none`.
    pub arrow_compression: String,
    /// `output_format_arrow_string_as_string`; off, text arrives as binary.
    pub string_as_string: bool,
    /// `enable_http_compression`: whether `Accept-Encoding: zstd` is honoured.
    pub http_compression: bool,
}

impl ReadOnly {
    /// Why rows from this user cannot be read as `plan` asks, if they cannot.
    fn refusal(&self, plan: &Plan) -> Option<String> {
        let fix = "This user's profile sets it, and with `readonly = 1` no request may change it. \
                   Set it in the profile, or give the user `readonly = 2`: still read-only, with \
                   settings allowed.";
        if !matches!(self.arrow_compression.as_str(), "lz4_frame" | "none") {
            return Some(format!(
                "the user is read-only (`readonly = 1`), and ClickHouse would compress the Arrow \
                 bodies with {}, which benchy does not decode: \
                 `output_format_arrow_compression_method` must be `lz4_frame` or `none`. {fix}",
                self.arrow_compression
            ));
        }
        if !self.string_as_string && plan.columns.iter().any(|c| c.decl.ty == "string") {
            return Some(format!(
                "the user is read-only (`readonly = 1`), and ClickHouse would send text as binary: \
                 `output_format_arrow_string_as_string` must be 1. {fix}"
            ));
        }
        None
    }

    /// For `check`: what the load will run with.
    fn said(&self) -> String {
        format!(
            "read-only (`readonly = 1`): benchy sends no settings, so ClickHouse's own apply — \
             Arrow bodies {}, text as {}, HTTP compression {} — and ClickHouse is not told the \
             deadline, which benchy still keeps",
            self.arrow_compression,
            if self.string_as_string {
                "strings"
            } else {
                "binary"
            },
            if self.http_compression { "on" } else { "off" }
        )
    }
}

/// ClickHouse's READONLY: a request named a setting the user may not change.
const READONLY: u32 = 164;

/// A request ClickHouse refused: the fault, and ClickHouse's own code, which decides whether to
/// try again without settings.
struct Refused {
    fault: Fault,
    code: Option<u32>,
}

/// How a ClickHouse source's rows travel. Measured on one machine against a local server
/// (docs/CAPACITY.md): zstd moves the fewest bytes and costs the most to decode, so it wins on
/// any link slower than the decoder; `none` is fastest where the server is on the same machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    /// The HTTP response, zstd-compressed (`Accept-Encoding: zstd`), decoded as it arrives.
    Zstd,
    /// Arrow's own LZ4 frame compression of each body, decoded by the reader under its bound on
    /// what the stream decodes to.
    Lz4,
    /// Nothing.
    None,
}

impl Compression {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "zstd" => Compression::Zstd,
            "lz4" => Compression::Lz4,
            "none" => Compression::None,
            _ => return None,
        })
    }

    /// When the operator has not said: `none` for a server on loopback, `zstd` anywhere else
    /// (`an_unset_compression_follows_where_the_server_is`). On loopback both ends of the
    /// compression are this machine's CPU and the bytes cost next to nothing to move. Measured
    /// with bench/ingest.py, 10,000,000 rows from a ClickHouse 26.8 on the same 4-core aarch64
    /// machine, CPU of benchy's processes and ClickHouse's together: zstd 8.52 s (ClickHouse
    /// 2.67 s of it, the decode 0.75 s with ruzstd), none 6.10 s (ClickHouse 0.87 s), Arrow's
    /// LZ4 6.71 s. A private network is not loopback: its bandwidth is not known, and on 1 Gbit/s
    /// `none`'s 240 MB take 2 s against zstd's 42 MB in 0.3 s. Only literal loopback counts —
    /// `localhost`, 127.0.0.0/8, ::1 — since a name is not resolved here.
    pub fn default_for(url: &str) -> Self {
        let host = url
            .parse::<ureq::http::Uri>()
            .ok()
            .and_then(|u| u.host().map(str::to_string))
            .unwrap_or_default();
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if loopback {
            Compression::None
        } else {
            Compression::Zstd
        }
    }
}

/// What the table is read from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum From {
    /// A table in `database`, by name.
    Table(String),
    /// The operator's own SELECT.
    Query(String),
}

/// The `env` keys a ClickHouse source reads. Any other would be handed to nothing, so the config
/// refuses it rather than ignore it.
pub const ENV_KEYS: [&str; 2] = ["CLICKHOUSE_USER", "CLICKHOUSE_PASSWORD"];

/// One column on its way to the bench.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// What ClickHouse calls its type, for the operator: never shown to the agent.
    pub ch_type: String,
    pub decl: FieldDecl,
    /// The ClickHouse expression that turns the column into `decl`'s type.
    pub expr: String,
}

/// What [`Source::plan`] decided.
#[derive(Debug, Clone)]
pub struct Plan {
    pub columns: Vec<Column>,
    pub schema: Vec<FieldDecl>,
    /// Columns not loaded, with their ClickHouse type and why: for the operator log.
    pub dropped: Vec<(String, String, String)>,
    /// `SELECT <raw columns> FROM <table> WHERE <narrowing>`: what ClickHouse reads.
    inner: String,
    /// The narrowing's values, as query parameters.
    params: Vec<(String, String)>,
}

impl Plan {
    /// The query that fetches the rows, each converted.
    pub fn select(&self, limit: Option<u64>) -> String {
        let exprs: Vec<String> = self
            .columns
            .iter()
            .map(|c| format!("{} AS {}", c.expr, ident(&c.name)))
            .collect();
        let limit = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
        format!(
            "SELECT {} FROM ({}{limit}) FORMAT ArrowStream",
            exprs.join(", "),
            self.inner
        )
    }

    pub fn params(&self) -> &[(String, String)] {
        &self.params
    }
}

// ---------------------------------------------------------------------------------------------
// Types

/// How one ClickHouse type is loaded: its vocabulary token, whether it may be null, and the
/// conversion as the text around the column (`pre` + column + `post`), cast at the end to
/// `target` so that the Arrow type ClickHouse writes is the one the token names.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapping {
    pub token: String,
    pub nullable: bool,
    pre: &'static str,
    post: &'static str,
    target: String,
}

impl Mapping {
    pub fn expr(&self, column: &str) -> String {
        let target = if self.nullable {
            format!("Nullable({})", self.target)
        } else {
            self.target.clone()
        };
        format!(
            "CAST({}{}{} AS {target})",
            self.pre,
            ident(column),
            self.post
        )
    }
}

/// `Name(args)` → (`Name`, `args`); `Name` → (`Name`, "").
fn split_type(t: &str) -> (&str, &str) {
    let t = t.trim();
    match t.find('(') {
        Some(i) if t.ends_with(')') => (t[..i].trim(), &t[i + 1..t.len() - 1]),
        _ => (t, ""),
    }
}

/// REQ-16 `clickhouse_types_map_to_the_canonical_vocabulary`. A ClickHouse type, as `DESCRIBE`
/// writes it, to what benchy loads — or why it is not loaded.
///
/// One canonical type per kind, whatever the source: integers are BIGINT, floats DOUBLE, every
/// instant TIMESTAMPTZ in microseconds and UTC, text VARCHAR. `UInt64` is sent as itself, not
/// cast: half its range does not fit Int64, and a wrapped id is a silent wrong answer; the
/// loader's canonical mapping holds it as HUGEINT, as it does a Parquet file's (REQ-15). The
/// containers — Array, Map, Tuple, Nested, JSON, Variant, Dynamic, the geo types — arrive as
/// their JSON text, which DuckDB's `json` functions read; integers past 64 bits and decimals past
/// 38 digits as their exact decimal text. Aggregate states and intervals have no value a reader
/// could use, and are dropped (`unsupported_clickhouse_types_become_json_text_or_are_dropped`).
pub fn map_type(ch: &str) -> Result<Mapping, String> {
    let (mut name, mut args) = split_type(ch);
    let mut nullable = false;
    // Wrappers that change nothing about the values benchy loads.
    loop {
        match name {
            "LowCardinality" => (name, args) = split_type(args),
            "Nullable" => {
                nullable = true;
                (name, args) = split_type(args);
            }
            // `SimpleAggregateFunction(f, T)` stores plain values of T.
            "SimpleAggregateFunction" => match args.split_once(',') {
                Some((_, inner)) => (name, args) = split_type(inner),
                None => return Err(format!("{ch} is not a type benchy can read")),
            },
            _ => break,
        }
    }
    let m = |token: &str, pre: &'static str, post: &'static str, target: &str| Mapping {
        token: token.into(),
        nullable,
        pre,
        post,
        target: target.into(),
    };
    let text = |pre, post| m("string", pre, post, "String");
    Ok(match name {
        "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" => {
            m("int64", "", "", "Int64")
        }
        "UInt64" => m("uint64", "", "", "UInt64"),
        "Int128" | "Int256" | "UInt128" | "UInt256" => text("toString(", ")"),
        "Float32" | "Float64" | "BFloat16" => m("float64", "", "", "Float64"),
        "Bool" | "Boolean" => m("bool", "", "", "Bool"),
        "Decimal" | "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => {
            let parts: Vec<&str> = args.split(',').map(str::trim).collect();
            let (p, s): (u32, u32) = match (name, parts.as_slice()) {
                ("Decimal", [p, s]) => (
                    p.parse().map_err(|_| format!("{ch}: no precision"))?,
                    s.parse().map_err(|_| format!("{ch}: no scale"))?,
                ),
                ("Decimal", [p]) => (p.parse().map_err(|_| format!("{ch}: no precision"))?, 0),
                ("Decimal32", [s]) => (9, s.parse().map_err(|_| format!("{ch}: no scale"))?),
                ("Decimal64", [s]) => (18, s.parse().map_err(|_| format!("{ch}: no scale"))?),
                ("Decimal128", [s]) => (38, s.parse().map_err(|_| format!("{ch}: no scale"))?),
                ("Decimal256", [s]) => (76, s.parse().map_err(|_| format!("{ch}: no scale"))?),
                _ => return Err(format!("{ch} is not a decimal benchy can read")),
            };
            if p <= 38 {
                Mapping {
                    token: format!("decimal({p},{s})"),
                    nullable,
                    pre: "",
                    post: "",
                    target: format!("Decimal({p}, {s})"),
                }
            } else {
                text("toString(", ")")
            }
        }
        "String" => text("toValidUTF8(", ")"),
        // Padded with NUL bytes to its width; the padding is not part of the value.
        "FixedString" => text("toValidUTF8(trimRight(toString(", "), char(0)))"),
        "Enum" | "Enum8" | "Enum16" | "UUID" | "IPv4" | "IPv6" | "Time" | "Time64" => {
            text("toString(", ")")
        }
        "Date" | "Date32" => m("date", "", "", "Date32"),
        "DateTime" | "DateTime64" => m(
            "timestamptz",
            "toDateTime64(",
            ", 6, 'UTC')",
            "DateTime64(6, 'UTC')",
        ),
        "Array" | "Map" | "Tuple" | "Nested" | "JSON" | "Object" | "Variant" | "Dynamic"
        | "Point" | "Ring" | "LineString" | "MultiLineString" | "Polygon" | "MultiPolygon" => {
            text("toJSONString(", ")")
        }
        "AggregateFunction" => {
            return Err(
                "an aggregate state is ClickHouse's own binary; finalize it in a `query` \
                        with finalizeAggregation(), or a -Merge combinator"
                    .into(),
            )
        }
        n if n.starts_with("Interval") => {
            return Err("an interval has no column type here; convert it in a `query`".into())
        }
        _ => return Err(format!("{ch} is not a type benchy can read")),
    })
}

// ---------------------------------------------------------------------------------------------
// SQL text

/// REQ-16 `identifiers_are_quoted_whatever_they_hold`. A backquoted identifier, with the escapes
/// ClickHouse reads inside backquotes.
pub fn ident(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('`');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('`');
    out
}

/// A string inside a parameter's text, single-quoted with ClickHouse's escapes.
fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `application/x-www-form-urlencoded`'s rules for a query string: everything but the unreserved
/// characters percent-encoded.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An RFC 3339 instant with its offset, to microseconds since the epoch. Strict on purpose: a
/// window without an offset means a different instant to every reader of it.
pub fn rfc3339_micros(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    let mut micros = 0i64;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        let mut end = start;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
        if end == start {
            return None;
        }
        let digits = &s[start..end];
        let six: String = digits
            .chars()
            .chain(std::iter::repeat('0'))
            .take(6)
            .collect();
        micros = six.parse().ok()?;
        i = end;
    }
    let offset_secs = match s.get(i..)? {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(&o[..1], "+" | "-") && &o[3..4] == ":" => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let v = oh * 3600 + om * 60;
            if &o[..1] == "-" {
                -v
            } else {
                v
            }
        }
        _ => return None,
    };
    let leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in = [
        31,
        if leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=12).contains(&mo) || d < 1 || d > days_in[(mo - 1) as usize] {
        return None;
    }
    if h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let (yy, mm) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_secs;
    Some(secs * 1_000_000 + micros)
}

/// REQ-16 `a_clickhouse_url_cannot_carry_a_password`. The URL is logged and shown in errors, so it
/// may hold neither a user nor a password, and no query string: the settings on it are benchy's.
pub fn check_url(url: &str) -> Result<(), String> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|e| format!("`url = \"{url}\"` is not a URL: {e}"))?;
    match uri.scheme_str() {
        Some("http") | Some("https") => {}
        _ => {
            return Err(format!(
                "`url = \"{url}\"` must start with http:// or https://. benchy speaks ClickHouse's \
                 HTTP interface — port 8123, or 8443 with TLS — not its native protocol."
            ))
        }
    }
    let Some(authority) = uri.authority() else {
        return Err(format!("`url = \"{url}\"` names no host."));
    };
    if authority.as_str().contains('@') {
        return Err(
            "`url` carries a user or password. A URL reaches logs and error messages; put the \
             user and password in `env` as CLICKHOUSE_USER and CLICKHOUSE_PASSWORD, which travel \
             in headers."
                .into(),
        );
    }
    if uri.query().is_some() || url.contains('#') {
        return Err(format!(
            "`url = \"{url}\"` has a query string. Settings on the URL are benchy's to set; name \
             the database with `database`."
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// HTTP

/// A ClickHouse error, mapped to the closed set of fault codes: the code decides what the agent
/// could ever be told, and the whole text goes to the operator log only.
fn fault_for(status: u16, code: Option<u32>, text: &str, origin: &str) -> Fault {
    // https://clickhouse.com/docs/en/interfaces/http and src/Common/ErrorCodes.cpp
    let fault_code = match code {
        // UNKNOWN_USER, WRONG_PASSWORD, REQUIRED_PASSWORD, IP_ADDRESS_NOT_ALLOWED,
        // ACCESS_DENIED, AUTHENTICATION_FAILED
        Some(192 | 193 | 194 | 195 | 497 | 516) => "E_AUTH",
        // UNKNOWN_TABLE, UNKNOWN_DATABASE, UNKNOWN_IDENTIFIER, NO_SUCH_COLUMN_IN_TABLE,
        // SYNTAX_ERROR, READONLY, CANNOT_PARSE_DATETIME, ILLEGAL_TYPE_OF_ARGUMENT
        Some(16 | 41 | 43 | 47 | 60 | 62 | 81 | 164) => "E_CONFIG",
        // TIMEOUT_EXCEEDED, SOCKET_TIMEOUT
        Some(159 | 209) => "E_TIMEOUT",
        // TOO_MANY_ROWS_OR_BYTES
        Some(396) => "E_TOO_LARGE",
        // MEMORY_LIMIT_EXCEEDED, TOO_MANY_SIMULTANEOUS_QUERIES, QUOTA_EXCEEDED
        Some(241 | 202 | 201) => "E_UNAVAILABLE",
        _ if status == 401 || status == 403 => "E_AUTH",
        _ if status >= 500 || status == 0 => "E_UNAVAILABLE",
        _ => "E_CONFIG",
    };
    let first = text.trim();
    let first: String = first.chars().take(2000).collect();
    Fault::host(
        fault_code,
        format!(
            "{origin}: ClickHouse answered HTTP {status}{}: {first}",
            code.map(|c| format!(", code {c}")).unwrap_or_default()
        ),
    )
}

/// `Code: 60. DB::Exception: …` → 60.
fn exception_code(text: &str) -> Option<u32> {
    let at = text.find("Code: ")?;
    let digits: String = text[at + 6..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Whether a body read failed on the deadline. ureq hands its own error back inside the
/// `io::Error`, with a kind that does not say so.
pub(crate) fn timed_out(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::TimedOut
        || e.get_ref()
            .and_then(|inner| inner.downcast_ref::<ureq::Error>())
            .is_some_and(|u| matches!(u, ureq::Error::Timeout(_)))
}

/// What a body reader saw: a transport error, and the last bytes, where ClickHouse writes an
/// exception that happens after the response has started.
#[derive(Default)]
struct Seen {
    /// The first transport error: whether it was a timeout, and what it said.
    error: Option<(bool, String)>,
    tail: std::collections::VecDeque<u8>,
    /// Bytes as they crossed the network, before any decoding.
    wire: u64,
}

/// Watches a body: the bytes off the wire (`decoded: false`), or the bytes the reader gets.
struct Watch<R> {
    inner: R,
    seen: Arc<Mutex<Seen>>,
    decoded: bool,
}

impl<R: Read> Read for Watch<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => {
                let mut s = self.seen.lock().unwrap();
                if !self.decoded {
                    s.wire += n as u64;
                } else if n > 0 {
                    s.tail.extend(&buf[..n]);
                    let over = s.tail.len().saturating_sub(4096);
                    s.tail.drain(..over);
                }
                Ok(n)
            }
            Err(e) => {
                let mut s = self.seen.lock().unwrap();
                if s.error.is_none() {
                    s.error = Some((timed_out(&e), e.to_string()));
                }
                Err(e)
            }
        }
    }
}

impl Source {
    pub fn origin(&self) -> String {
        match &self.from {
            From::Table(t) => format!("clickhouse {} {}.{t}", self.url, self.database),
            From::Query(_) => format!("clickhouse {} (query for `{}`)", self.url, self.name),
        }
    }

    fn agent(&self, within: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            // A status is ClickHouse's answer, read and classified here.
            .http_status_as_error(false)
            // Connect where the config says, and nowhere else: no proxy from the environment
            // (the loader's is empty anyway), and no redirect, which could take the headers
            // with the password elsewhere.
            .proxy(None)
            .max_redirects(0)
            .timeout_connect(Some(within.min(Duration::from_secs(10))))
            .timeout_global(Some(within))
            .user_agent(format!("benchy-mcp/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent()
    }

    fn user(&self) -> Option<&str> {
        self.env.get("CLICKHOUSE_USER").map(String::as_str)
    }

    /// POST `sql`, with `params` and the settings every request carries, on the URL. A status
    /// other than 200 is turned into a fault here, with ClickHouse's own message for the log.
    ///
    /// A user that may not change settings is found by being refused them: one failed request,
    /// once per source, and nothing at all for every other user. It is then asked, without
    /// settings, what it is; a `readonly = 1` user gets this request again, and every later one,
    /// without settings (`a_read_only_clickhouse_user_loads_what_any_user_loads`).
    fn post(
        &self,
        sql: &str,
        params: &[(String, String)],
        within: Duration,
        compress: Compression,
    ) -> Result<ureq::http::Response<ureq::Body>, Fault> {
        let plain = self.readonly.get().is_some();
        match self.send(sql, params, within, compress, !plain) {
            Ok(r) => Ok(r),
            Err(Refused {
                fault,
                code: Some(READONLY),
            }) if !plain => match self.read_only(within) {
                Some(ro) => {
                    let _ = self.readonly.set(ro);
                    self.send(sql, params, within, compress, false)
                        .map_err(|r| r.fault)
                }
                None => Err(fault),
            },
            Err(r) => Err(r.fault),
        }
    }

    /// What ClickHouse's own settings are for this user, asked without naming any; `None`
    /// unless it is `readonly = 1`.
    fn read_only(&self, within: Duration) -> Option<ReadOnly> {
        let resp = self
            .send(
                "SELECT getSetting('readonly'), \
                 getSetting('output_format_arrow_compression_method'), \
                 getSetting('output_format_arrow_string_as_string'), \
                 getSetting('enable_http_compression') FORMAT TSV",
                &[],
                within,
                Compression::None,
                false,
            )
            .ok()?;
        let mut text = String::new();
        resp.into_body()
            .into_reader()
            .take(4096)
            .read_to_string(&mut text)
            .ok()?;
        let f: Vec<&str> = text.trim().split('\t').collect();
        let on = |s: &str| matches!(s, "1" | "true");
        match f.as_slice() {
            ["1", method, strings, http] => Some(ReadOnly {
                arrow_compression: method.to_string(),
                string_as_string: on(strings),
                http_compression: on(http),
            }),
            _ => None,
        }
    }

    /// One request: with benchy's settings, or with none.
    fn send(
        &self,
        sql: &str,
        params: &[(String, String)],
        within: Duration,
        compress: Compression,
        settings: bool,
    ) -> Result<ureq::http::Response<ureq::Body>, Refused> {
        let mut query: Vec<(String, String)> = vec![("database".into(), self.database.clone())];
        if settings {
            query.extend(self.settings(within, compress));
        }
        query.extend(params.iter().cloned());
        let qs: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
            .collect();
        let url = format!("{}/?{}", self.url.trim_end_matches('/'), qs.join("&"));
        let mut req = self.agent(within).post(&url);
        if let Some(u) = self.user() {
            req = req.header("X-ClickHouse-User", u);
        }
        if let Some(p) = self.env.get("CLICKHOUSE_PASSWORD") {
            if self.user().is_none() {
                req = req.header("X-ClickHouse-User", "default");
            }
            req = req.header("X-ClickHouse-Key", p);
        }
        // Asked for even without the setting that turns it on: a read-only user's server may
        // have it on (26.8 does by default), and an answer that is not compressed is read too.
        if compress == Compression::Zstd {
            req = req.header("Accept-Encoding", "zstd");
        }
        let resp = req.send(sql.as_bytes()).map_err(|e| Refused {
            fault: self.transport(&e.to_string(), matches!(e, ureq::Error::Timeout(_))),
            code: None,
        })?;
        let status = resp.status().as_u16();
        if status == 200 {
            return Ok(resp);
        }
        let code = resp
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok());
        let mut text = Vec::new();
        let _ = resp
            .into_body()
            .into_reader()
            .take(64 * 1024)
            .read_to_end(&mut text);
        let text = String::from_utf8_lossy(&text);
        let code = code.or_else(|| exception_code(&text));
        Err(Refused {
            fault: fault_for(status, code, &text, &self.origin()),
            code,
        })
    }

    /// The settings every request carries, for a user that may set them.
    fn settings(&self, within: Duration, compress: Compression) -> Vec<(String, String)> {
        let mut query: Vec<(String, String)> = vec![
            // Said every time, since the server's default (LZ4) is not always what was asked for;
            // and a LowCardinality column could come as a dictionary.
            (
                "output_format_arrow_compression_method".into(),
                if compress == Compression::Lz4 {
                    "lz4_frame"
                } else {
                    "none"
                }
                .into(),
            ),
            ("output_format_arrow_string_as_string".into(), "1".into()),
            (
                "output_format_arrow_low_cardinality_as_dictionary".into(),
                "0".into(),
            ),
            // ClickHouse stops the query itself at the deadline, rather than run on for a client
            // that has gone.
            (
                "max_execution_time".into(),
                within.as_secs().max(1).to_string(),
            ),
        ];
        if compress == Compression::Zstd {
            query.push(("enable_http_compression".into(), "1".into()));
        }
        query
    }

    fn transport(&self, why: &str, timed_out: bool) -> Fault {
        if timed_out {
            Fault::host(
                "E_HOST_TIMEOUT",
                format!(
                    "{}: no answer within {:?}: {why}",
                    self.origin(),
                    self.limits.deadline
                ),
            )
        } else {
            Fault::host(
                "E_UNAVAILABLE",
                format!("{}: could not reach ClickHouse: {why}", self.origin()),
            )
        }
    }

    /// A small text answer: `DESCRIBE`, a count.
    fn ask(&self, sql: &str, params: &[(String, String)]) -> Result<String, Fault> {
        let resp = self.post(sql, params, self.limits.deadline, Compression::None)?;
        let mut text = String::new();
        resp.into_body()
            .into_reader()
            .take(16 * 1024 * 1024)
            .read_to_string(&mut text)
            .map_err(|e| self.transport(&e.to_string(), timed_out(&e)))?;
        if text.contains("DB::Exception") {
            return Err(fault_for(200, exception_code(&text), &text, &self.origin()));
        }
        Ok(text)
    }

    /// The columns ClickHouse has, as `DESCRIBE` names and types them.
    fn describe(&self) -> Result<Vec<(String, String, String)>, Fault> {
        let target = match &self.from {
            From::Table(t) => format!("TABLE {}.{}", ident(&self.database), ident(t)),
            From::Query(q) => format!("({})", wrap(q)),
        };
        let text = self.ask(&format!("DESCRIBE {target} FORMAT JSONEachRow"), &[])?;
        let mut out = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                Fault::host(
                    "E_HOST_PROTOCOL",
                    format!("{}: DESCRIBE answered something else: {e}", self.origin()),
                )
            })?;
            let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            out.push((field("name"), field("type"), field("default_type")));
        }
        Ok(out)
    }

    /// Read the schema, map it, and write the query.
    pub fn plan(&self) -> Result<Plan, Fault> {
        let described = self.describe()?;
        self.plan_from(described)
    }

    /// The plan for the columns `DESCRIBE` gave: name, type, and how it is filled.
    fn plan_from(&self, described: Vec<(String, String, String)>) -> Result<Plan, Fault> {
        let config = |why: String| Fault::host("E_CONFIG", format!("{}: {why}", self.origin()));
        if described.is_empty() {
            return Err(config("ClickHouse describes no columns".into()));
        }
        let pinned = !self.schema.is_empty();
        let wanted: Option<Vec<String>> = if pinned {
            Some(self.schema.iter().map(|f| f.name.clone()).collect())
        } else {
            self.narrowing.columns.clone()
        };
        if let Some(w) = &wanted {
            for c in w {
                if !described.iter().any(|(n, _, _)| n == c) {
                    return Err(Fault::host(
                        if pinned { "E_SCHEMA" } else { "E_CONFIG" },
                        format!(
                            "{}: column `{c}` is {}, but ClickHouse has no such column",
                            self.origin(),
                            if pinned {
                                "pinned in `schema`"
                            } else {
                                "in `narrow.columns`"
                            },
                        ),
                    ));
                }
            }
        }

        let mut columns = Vec::new();
        let mut dropped = Vec::new();
        let mut raw = Vec::new();
        for (name, ch_type, default_type) in &described {
            if wanted.as_ref().is_some_and(|w| !w.contains(name)) {
                continue;
            }
            if default_type == "EPHEMERAL" {
                dropped.push((
                    name.clone(),
                    ch_type.clone(),
                    "an EPHEMERAL column holds nothing".into(),
                ));
                continue;
            }
            let mapping = match map_type(ch_type) {
                Ok(m) => m,
                Err(why) if !pinned => {
                    dropped.push((name.clone(), ch_type.clone(), why));
                    continue;
                }
                Err(why) => {
                    return Err(Fault::host(
                        "E_SCHEMA",
                        format!(
                            "{}: pinned column `{name}` is {ch_type}: {why}",
                            self.origin()
                        ),
                    ))
                }
            };
            if name.is_empty() || name.chars().any(char::is_control) || name.len() > 255 {
                return Err(config(format!(
                    "column {name:?} has a name the agent could not type. Load it from a `query` \
                     that gives it another name with AS."
                )));
            }
            if columns
                .iter()
                .any(|c: &Column| c.name.eq_ignore_ascii_case(name))
            {
                return Err(config(format!(
                    "columns `{name}` and another differ only in case, which DuckDB does not tell \
                     apart. Load it from a `query` that renames one with AS."
                )));
            }
            let mut decl = FieldDecl {
                name: name.clone(),
                ty: mapping.token.clone(),
                nullable: mapping.nullable,
            };
            if let Some(p) = self.schema.iter().find(|f| &f.name == name) {
                if p.ty != mapping.token {
                    return Err(Fault::host(
                        "E_SCHEMA",
                        format!(
                            "{}: column `{name}` is pinned as {}, but ClickHouse's {ch_type} \
                             loads as {}. Pin {}, or leave the column's type to be read.",
                            self.origin(),
                            p.ty,
                            mapping.token,
                            mapping.token
                        ),
                    ));
                }
                // A pinned NOT NULL holds the data to it: checked in every batch.
                decl.nullable = p.nullable;
            }
            raw.push(ident(name));
            columns.push(Column {
                name: name.clone(),
                ch_type: ch_type.clone(),
                expr: mapping.expr(name),
                decl,
            });
        }
        if columns.is_empty() {
            return Err(config(format!(
                "none of its columns can be loaded ({})",
                dropped
                    .iter()
                    .map(|(n, t, _)| format!("`{n}` {t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if columns.len() > 512 {
            return Err(config(format!(
                "{} columns; a table may have at most 512. Name the ones to load in \
                 `narrow.columns`.",
                columns.len()
            )));
        }

        // The narrowing: on raw columns, so ClickHouse can use its primary key, with every value
        // a query parameter.
        let mut wheres = Vec::new();
        let mut params = Vec::new();
        let col = |c: &str| columns.iter().find(|x| x.name == c);
        if let Some((c, start, end)) = &self.narrowing.time {
            let Some(column) = col(c) else {
                return Err(config(format!(
                    "the time window's column `{c}` is not among the columns loaded"
                )));
            };
            let lhs = match column.decl.ty.as_str() {
                "timestamptz" => ident(c),
                // A date is the instant its day starts, in UTC, as DuckDB re-applies it.
                "date" => format!("toDateTime64({}, 6, 'UTC')", ident(c)),
                other => {
                    return Err(config(format!(
                        "the time window's column `{c}` loads as {other}, not a date or a \
                         timestamp"
                    )))
                }
            };
            for (bound, op, key) in [(start, ">=", "benchy_start"), (end, "<", "benchy_end")] {
                if let Some(t) = bound {
                    let micros = rfc3339_micros(t).ok_or_else(|| {
                        config(format!(
                            "`{t}` is not an RFC 3339 time with an offset, such as \
                             2026-01-01T00:00:00Z"
                        ))
                    })?;
                    wheres.push(format!(
                        "{lhs} {op} fromUnixTimestamp64Micro({{{key}:Int64}}, 'UTC')"
                    ));
                    params.push((format!("param_{key}"), micros.to_string()));
                }
            }
        }
        if let Some((c, ids)) = &self.narrowing.entities {
            crate::exec::vet_entities(ids)?;
            let Some(column) = col(c) else {
                return Err(config(format!(
                    "the entity column `{c}` is not among the columns loaded"
                )));
            };
            let (base, _) = split_type(&column.ch_type);
            let text_like = column.ch_type.contains("String") && !base.starts_with("Array");
            let lhs = if text_like {
                ident(c)
            } else {
                format!("toString({})", ident(c))
            };
            wheres.push(format!("{lhs} IN {{benchy_ids:Array(String)}}"));
            let list: Vec<String> = ids.iter().map(|i| quoted(i)).collect();
            params.push(("param_benchy_ids".into(), format!("[{}]", list.join(","))));
        }
        let from = match &self.from {
            From::Table(t) => format!("{}.{}", ident(&self.database), ident(t)),
            From::Query(q) => format!("({})", wrap(q)),
        };
        let mut inner = format!("SELECT {} FROM {from}", raw.join(", "));
        if !wheres.is_empty() {
            inner.push_str(&format!(" WHERE {}", wheres.join(" AND ")));
        }
        let schema = columns.iter().map(|c| c.decl.clone()).collect();
        Ok(Plan {
            columns,
            schema,
            dropped,
            inner,
            params,
        })
    }

    /// Rows by `count()` under the narrowing; bytes from each column's Arrow width, with text
    /// columns measured on a sample of at most 10,000 rows. Both are cheap for ClickHouse: the
    /// count reads no column, or only the key, and the sample stops at its limit.
    pub fn estimate(&self, plan: &Plan) -> Result<(u64, u64), Fault> {
        let texts: Vec<&Column> = plan
            .columns
            .iter()
            .filter(|c| c.decl.ty == "string")
            .collect();
        let sql = if texts.is_empty() {
            format!("SELECT count() FROM ({}) FORMAT TSV", plan.inner)
        } else {
            let avgs: Vec<String> = texts
                .iter()
                .map(|c| format!("avg(length({}))", ident(&c.name)))
                .collect();
            let exprs: Vec<String> = texts
                .iter()
                .map(|c| format!("{} AS {}", c.expr, ident(&c.name)))
                .collect();
            format!(
                "SELECT (SELECT count() FROM ({inner})), {} FROM (SELECT {} FROM ({inner} LIMIT \
                 10000)) FORMAT TSV",
                avgs.join(", "),
                exprs.join(", "),
                inner = plan.inner
            )
        };
        let text = self.ask(&sql, &plan.params)?;
        let fields: Vec<&str> = text.trim().split('\t').collect();
        let rows: u64 = fields.first().and_then(|r| r.parse().ok()).ok_or_else(|| {
            Fault::host(
                "E_HOST_PROTOCOL",
                format!("{}: the count answered {text:?}", self.origin()),
            )
        })?;
        let mut per_row = 0.0f64;
        let mut text_avg = fields[1..].iter().map(|f| f.parse::<f64>().unwrap_or(0.0));
        for c in &plan.columns {
            per_row += match c.decl.ty.as_str() {
                "string" => {
                    // An offset, and the bytes.
                    let avg = text_avg.next().unwrap_or(0.0);
                    4.0 + if avg.is_finite() { avg } else { 0.0 }
                }
                "bool" => 0.125,
                "date" => 4.0,
                t if t.starts_with("decimal") => 16.0,
                _ => 8.0,
            };
            if c.decl.nullable {
                per_row += 0.125;
            }
        }
        // The schema message and each batch's header: small, and once per 65,409 rows.
        let bytes = (rows as f64 * per_row).ceil() as u64 + 4096;
        Ok((rows, bytes))
    }

    /// The rows, streamed from the response into the validating reader, and what that cost on
    /// the network, for the operator log.
    pub fn fetch(
        &self,
        plan: &Plan,
        max_rows: u64,
        max_bytes: u64,
        limit: Option<u64>,
        budget: &crate::admission::Budget,
        sink: &mut crate::exec::Sink,
    ) -> Result<Fetched, Fault> {
        let started = std::time::Instant::now();
        let (fetched, wire) = self.fetch_counted(plan, max_rows, max_bytes, limit, budget, sink)?;
        eprintln!(
            "benchy: source `{}`: {} row(s), {} byte(s) as Arrow, {} byte(s) on the wire, in \
             {:.2}s",
            self.name,
            fetched.rows,
            fetched.bytes,
            wire,
            started.elapsed().as_secs_f64()
        );
        Ok(fetched)
    }

    /// [`Source::fetch`], and the bytes that crossed the network.
    pub fn fetch_counted(
        &self,
        plan: &Plan,
        max_rows: u64,
        max_bytes: u64,
        limit: Option<u64>,
        budget: &crate::admission::Budget,
        sink: &mut crate::exec::Sink,
    ) -> Result<(Fetched, u64), Fault> {
        // Known by now: planning asked ClickHouse first. Refused before a row is sent.
        if let Some(why) = self.readonly.get().and_then(|ro| ro.refusal(plan)) {
            return Err(Fault::host("E_CONFIG", format!("{}: {why}", self.origin())));
        }
        let resp = self.post(
            &plan.select(limit),
            &plan.params,
            self.limits.deadline,
            self.compression,
        )?;
        let header = resp
            .headers()
            .get("Content-Encoding")
            .map(|v| v.to_str().unwrap_or("?").to_string());
        let encoding = crate::decompress::Encoding::from_header(header.as_deref());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let raw = Watch {
            inner: resp.into_body().into_reader(),
            seen: seen.clone(),
            decoded: false,
        };
        use crate::decompress::{decoder, Encoding};
        let body: Box<dyn Read> = match encoding {
            Some(Encoding::Identity) => Box::new(raw),
            Some(e @ Encoding::Zstd) => {
                decoder(e, std::io::BufReader::with_capacity(64 * 1024, raw)).map_err(|e| {
                    Fault::host("E_HOST_PROTOCOL", format!("{}: {e}", self.origin()))
                })?
            }
            // Never asked for: gzip is for the HTTP source.
            Some(Encoding::Gzip) | None => {
                return Err(Fault::host(
                    "E_HOST_PROTOCOL",
                    format!(
                        "{}: the response is {}-encoded, which benchy did not ask for",
                        self.origin(),
                        header.unwrap_or_default()
                    ),
                ))
            }
        };
        let mut body = Watch {
            inner: body,
            seen: seen.clone(),
            decoded: true,
        };
        let declared = crate::exec::declared_schema(&crate::exec::ExecSource {
            name: self.name.clone(),
            narrowing: Narrowing::default(),
            command: Default::default(),
            args: Vec::new(),
            env: BTreeMap::new(),
            config: serde_json::Value::Null,
            schema: plan.schema.clone(),
            limits: self.limits.clone(),
            timezone: None,
        })?;
        match crate::exec::read_arrow(
            &mut body,
            Some(&declared),
            max_rows,
            max_bytes,
            budget,
            sink,
        ) {
            Ok(f) => {
                let wire = seen.lock().unwrap().wire;
                Ok((f, wire))
            }
            Err(f) if f.code == "E_HOST_OVERSIZE" || f.code == "E_TOO_LARGE" => Err(Fault::host(
                "E_TOO_LARGE",
                format!(
                    "{}: more than {max_rows} row(s) / {max_bytes} byte(s), what this source and \
                         the session's budget allow. Refused, not truncated: nothing of it was \
                         loaded. ({})",
                    self.origin(),
                    f.detail
                ),
            )),
            Err(f) => {
                // The reader stops at the first byte after the end-of-stream marker, which is
                // where ClickHouse writes an exception that comes after the stream has started.
                // A little more is read so its text, and its code, reach the operator's log;
                // a stream that ended without its marker may have one there too.
                let _ = std::io::copy(&mut (&mut body).take(4096), &mut std::io::sink());
                let s = seen.lock().unwrap();
                // ClickHouse writes an exception that comes after the response has started into
                // the body, after whatever Arrow it had sent.
                let tail = String::from_utf8_lossy(&s.tail.iter().copied().collect::<Vec<_>>())
                    .into_owned();
                if let Some(at) = tail.find("Code: ") {
                    let text = &tail[at..];
                    return Err(fault_for(200, exception_code(text), text, &self.origin()));
                }
                if let Some((timeout, why)) = &s.error {
                    return Err(self.transport(why, *timeout));
                }
                Err(Fault::host(
                    f.code,
                    format!("{}: {}", self.origin(), f.detail),
                ))
            }
        }
    }

    /// `benchy-mcp check`: every step of a load, each failure with its fix.
    pub fn check(&self) -> Vec<Check> {
        let mut out = Vec::new();
        let ok = |label: String| Check {
            ok: true,
            warn: false,
            skipped: false,
            label,
            detail: String::new(),
        };
        let bad = |label: &str, f: &Fault, fix: &str| Check {
            ok: false,
            warn: false,
            skipped: false,
            label: label.into(),
            detail: format!("[{}] {}\n     {fix}", f.code, f.detail),
        };

        // Reach: /ping answers without credentials.
        let ping = format!("{}/ping", self.url.trim_end_matches('/'));
        match self.agent(Duration::from_secs(10)).get(&ping).call() {
            Ok(r) if r.status().as_u16() == 200 => out.push(ok(format!("{} answers", self.url))),
            Ok(r) => {
                out.push(bad(
                    "reach",
                    &fault_for(r.status().as_u16(), None, "", &self.origin()),
                    "Something answers at that address, but not ClickHouse's HTTP interface. \
                     Check the host, the port (8123, or 8443 with TLS) and any path in `url`.",
                ));
                return skip_rest(out, "reach");
            }
            Err(e) => {
                out.push(bad(
                    "reach",
                    &self.transport(&e.to_string(), matches!(e, ureq::Error::Timeout(_))),
                    "Is ClickHouse running there, and reachable from this machine? benchy speaks \
                     its HTTP interface — port 8123, or 8443 with TLS — not the native protocol \
                     on 9000.",
                ));
                return skip_rest(out, "reach");
            }
        }

        // Credentials, and whether the settings every request makes are allowed: said, since a
        // read-only user's loads run on ClickHouse's settings instead.
        match self.ask("SELECT 1 FORMAT TSV", &[]) {
            Ok(_) => out.push(ok(format!(
                "signed in as {}{}",
                self.user().unwrap_or("default"),
                self.readonly
                    .get()
                    .map(|ro| format!(", {}", ro.said()))
                    .unwrap_or_default()
            ))),
            Err(f) => {
                let fix = if f.code == "E_AUTH" {
                    "Set CLICKHOUSE_USER and CLICKHOUSE_PASSWORD in this source's `env`, from \
                     variables in benchy's environment: env = { CLICKHOUSE_PASSWORD = \
                     \"${CH_PASSWORD}\" }. Check the user may connect from this host."
                } else {
                    "ClickHouse refused a plain `SELECT 1`; its message above says why."
                };
                out.push(bad("sign in", &f, fix));
                return skip_rest(out, "sign in");
            }
        }

        // Schema.
        let plan = match self.plan() {
            Ok(p) => p,
            Err(f) => {
                let fix = match f.code {
                    "E_SCHEMA" => {
                        "A pinned `[[source.schema]]` is strict: each pinned column must exist and \
                         load as the type pinned. Remove the pins to have the schema read."
                    }
                    "E_AUTH" => "The user may not read this table: GRANT SELECT on it.",
                    _ => {
                        "Check `database`, `tables` or `query`, and the columns `narrow` names: \
                         ClickHouse's message above says which is wrong."
                    }
                };
                out.push(bad("schema", &f, fix));
                return skip_rest(out, "schema");
            }
        };
        let lines: Vec<String> = plan
            .columns
            .iter()
            .map(|c| {
                format!(
                    "{:<24} {:<32} -> {}{}",
                    c.name,
                    one_line(&c.ch_type),
                    c.decl.ty,
                    if c.decl.nullable { "" } else { " not null" }
                )
            })
            .collect();
        let lines = lines.join("\n     ");
        out.push(Check {
            ok: true,
            warn: false,
            skipped: false,
            label: format!(
                "schema: {} column(s) of `{}`, as benchy loads them",
                plan.columns.len(),
                self.name
            ),
            detail: lines,
        });
        for (n, t, why) in &plan.dropped {
            out.push(Check {
                ok: true,
                warn: true,
                skipped: false,
                label: format!("column `{n}` ({t}) is not loaded"),
                detail: why.clone(),
            });
        }

        // Estimate.
        let budget = crate::admission::Budget::default();
        match self.estimate(&plan) {
            Ok((rows, bytes)) => {
                let over = rows > self.limits.max_rows || bytes > self.limits.max_bytes;
                out.push(Check {
                    ok: true,
                    warn: over,
                    skipped: false,
                    label: format!("estimate: {rows} row(s), about {bytes} byte(s)"),
                    detail: if over {
                        format!(
                            "Over this source's cap of {} row(s) / {} byte(s): the load would be \
                             refused before fetching. Narrow it with [source.narrow], or raise \
                             `max_rows` / `max_bytes`.",
                            self.limits.max_rows, self.limits.max_bytes
                        )
                    } else {
                        String::new()
                    },
                });
            }
            Err(f) => out.push(bad(
                "estimate",
                &f,
                "The count under the narrowing failed; ClickHouse's message above says why.",
            )),
        }

        // One row, through the reader a load uses.
        let (rows_left, bytes_left) = budget.remaining();
        match self.fetch(
            &plan,
            self.limits.max_rows.min(rows_left),
            self.limits.max_bytes.min(bytes_left),
            Some(1),
            &budget,
            &mut |_| Ok(()),
        ) {
            Ok(f) => out.push(ok(format!(
                "fetch: {} row(s) arrived as declared, end-of-stream and all",
                f.rows
            ))),
            Err(f) => out.push(bad(
                "fetch",
                &f,
                "The rows did not arrive as benchy asked for them; the message above says why.",
            )),
        }
        out
    }
}

/// The steps of `check`, in order. Each needs the ones before it, so after a failure the rest
/// are said to be skipped rather than left out or failed for the same reason.
const STEPS: [&str; 5] = ["reach", "sign in", "schema", "estimate", "fetch"];

fn skip_rest(mut out: Vec<Check>, failed: &str) -> Vec<Check> {
    let after = STEPS
        .iter()
        .position(|s| *s == failed)
        .map_or(STEPS.len(), |i| i + 1);
    for step in &STEPS[after..] {
        out.push(crate::exec::skipped(step, failed));
    }
    out
}

/// A type as `DESCRIBE` wrote it, on one line: a Tuple's comes with a line per element.
fn one_line(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The operator's query as a subquery: a trailing `;` dropped, and on lines of its own, so a
/// trailing `-- comment` cannot swallow the parenthesis.
fn wrap(q: &str) -> String {
    format!("\n{}\n", q.trim().trim_end_matches(';').trim_end())
}

impl super::Fetch for Source {
    type Plan = Plan;

    fn name(&self) -> &str {
        &self.name
    }
    fn limits(&self) -> &Limits {
        &self.limits
    }
    fn narrowing(&self) -> &Narrowing {
        &self.narrowing
    }
    fn plan(&self) -> Result<Plan, Fault> {
        let plan = Source::plan(self)?;
        for (n, t, why) in &plan.dropped {
            eprintln!(
                "benchy: source `{}`: column `{n}` ({t}) is not loaded: {why}",
                self.name
            );
        }
        Ok(plan)
    }
    fn schema<'a>(&self, plan: &'a Plan) -> &'a [FieldDecl] {
        &plan.schema
    }
    fn estimate(&self, plan: &Plan) -> Result<(u64, u64), Fault> {
        Source::estimate(self, plan)
    }
    fn fetch(
        &self,
        plan: &Plan,
        max_rows: u64,
        max_bytes: u64,
        limit: Option<u64>,
        budget: &crate::admission::Budget,
        sink: &mut crate::exec::Sink,
    ) -> Result<Fetched, Fault> {
        Source::fetch(self, plan, max_rows, max_bytes, limit, budget, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(t: &str) -> (String, bool) {
        let m = map_type(t).unwrap_or_else(|e| panic!("{t}: {e}"));
        (m.token, m.nullable)
    }

    /// Every integer to BIGINT but `UInt64`, which does not fit and is held as HUGEINT, as from
    /// every source (REQ-15); every float to DOUBLE, every instant to TIMESTAMPTZ in microseconds
    /// and UTC, `Date` to DATE, text-like types to VARCHAR. Each conversion ends in the ClickHouse
    /// type whose Arrow form is the bench's.
    #[test]
    fn clickhouse_types_map_to_the_canonical_vocabulary() {
        for (ch, want, nullable) in [
            ("Int8", "int64", false),
            ("Int16", "int64", false),
            ("Int32", "int64", false),
            ("Int64", "int64", false),
            ("UInt8", "int64", false),
            ("UInt16", "int64", false),
            ("UInt32", "int64", false),
            ("UInt64", "uint64", false),
            ("Float32", "float64", false),
            ("Float64", "float64", false),
            ("Bool", "bool", false),
            ("String", "string", false),
            ("FixedString(3)", "string", false),
            ("LowCardinality(String)", "string", false),
            ("LowCardinality(Nullable(String))", "string", true),
            ("Nullable(Int32)", "int64", true),
            ("Enum8('a' = 1, 'b' = 2)", "string", false),
            ("Enum16('x' = 1)", "string", false),
            ("UUID", "string", false),
            ("IPv4", "string", false),
            ("IPv6", "string", false),
            ("Date", "date", false),
            ("Date32", "date", false),
            ("Nullable(Date)", "date", true),
            ("DateTime", "timestamptz", false),
            ("DateTime('Europe/Berlin')", "timestamptz", false),
            ("DateTime64(3)", "timestamptz", false),
            ("DateTime64(9, 'UTC')", "timestamptz", false),
            ("Nullable(DateTime64(6, 'Asia/Tokyo'))", "timestamptz", true),
            ("Decimal(10, 3)", "decimal(10,3)", false),
            ("Decimal(38, 0)", "decimal(38,0)", false),
            ("Decimal32(2)", "decimal(9,2)", false),
            ("Decimal64(4)", "decimal(18,4)", false),
            ("Decimal128(5)", "decimal(38,5)", false),
            ("SimpleAggregateFunction(sum, UInt32)", "int64", false),
        ] {
            assert_eq!(token(ch), (want.to_string(), nullable), "{ch}");
            // Every token is in the vocabulary the reader and the bench share.
            assert!(crate::exec::arrow_type(want).is_some(), "{want}");
        }
        // The conversion ends in the one ClickHouse type whose Arrow form is that token's, and
        // only a nullable column may carry a null through it.
        assert_eq!(
            map_type("DateTime").unwrap().expr("ts"),
            "CAST(toDateTime64(`ts`, 6, 'UTC') AS DateTime64(6, 'UTC'))"
        );
        assert_eq!(
            map_type("Nullable(UInt16)").unwrap().expr("n"),
            "CAST(`n` AS Nullable(Int64))"
        );
        assert_eq!(map_type("Date").unwrap().expr("d"), "CAST(`d` AS Date32)");
        assert_eq!(
            map_type("LowCardinality(String)").unwrap().expr("m"),
            "CAST(toValidUTF8(`m`) AS String)"
        );
    }

    /// Integers past 64 bits and decimals past 38 digits arrive as exact text.
    #[test]
    fn unsupported_clickhouse_types_become_json_text_or_are_dropped() {
        // Containers arrive as JSON text, exact wide numbers as decimal text.
        for ch in [
            "Array(Int32)",
            "Map(String, Int32)",
            "Tuple(\n    a Int32,\n    b String)",
            "Nested(x Int32)",
            "JSON",
            "Object('json')",
            "Variant(String, UInt64)",
            "Dynamic",
            "Point",
            "MultiPolygon",
        ] {
            let m = map_type(ch).unwrap_or_else(|e| panic!("{ch}: {e}"));
            assert_eq!(m.token, "string", "{ch}");
            assert!(m.expr("c").starts_with("CAST(toJSONString(`c`)"), "{ch}");
        }
        for ch in ["Int128", "UInt256", "Decimal(50, 4)", "Decimal256(3)"] {
            let m = map_type(ch).unwrap();
            assert_eq!(m.token, "string", "{ch}");
            assert!(m.expr("c").starts_with("CAST(toString(`c`)"), "{ch}");
        }
        // What has no value a reader could use is dropped, saying how to load it instead.
        for ch in [
            "AggregateFunction(sum, UInt64)",
            "IntervalDay",
            "Nothing",
            "Nullable(Nothing)",
            "SomethingNew(1)",
        ] {
            assert!(map_type(ch).is_err(), "{ch}");
        }
        assert!(map_type("AggregateFunction(uniq, String)")
            .unwrap_err()
            .contains("finalizeAggregation"));
    }

    /// A backquote, a backslash or a newline in a name cannot end the identifier it is in.
    #[test]
    fn identifiers_are_quoted_whatever_they_hold() {
        assert_eq!(ident("ts"), "`ts`");
        assert_eq!(ident("a`b"), "`a\\`b`");
        assert_eq!(ident("a\\`; DROP"), "`a\\\\\\`; DROP`");
        assert_eq!(ident("x\ny"), "`x\\ny`");
        assert_eq!(ident("{p:String}"), "`{p:String}`");
        assert_eq!(ident("Zählerstand"), "`Zählerstand`");
        // Whatever the text, the quoted form never ends early: every backquote inside is escaped.
        for hostile in ["`", "\\`", "\\\\`", "a`\n`b", "`)--", "\0`"] {
            let q = ident(hostile);
            let inner = &q[1..q.len() - 1];
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    chars.next();
                } else {
                    assert_ne!(c, '`', "{hostile:?} → {q}");
                    assert!(!c.is_control(), "{hostile:?} → {q}");
                }
            }
        }
    }

    fn src() -> Source {
        Source {
            name: "readings".into(),
            url: "http://127.0.0.1:1".into(),
            database: "default".into(),
            from: From::Table("readings".into()),
            env: BTreeMap::new(),
            schema: Vec::new(),
            narrowing: Narrowing::default(),
            limits: Limits::default(),
            compression: Compression::Zstd,
            readonly: Default::default(),
        }
    }

    /// The parameters are percent-encoded on the URL; none is in the SQL text, however hostile.
    #[test]
    fn values_travel_as_query_parameters_never_in_the_sql() {
        // The plan is built from what DESCRIBE said; here it is built by hand around the same
        // narrowing code, by planning against a canned description.
        let hostile = vec![
            "it's".to_string(),
            "back\\slash".into(),
            "{benchy_ids:String}".into(),
            "') OR 1=1 --".into(),
            "tab\there".into(),
            "Zähler ü".into(),
            "`tick`".into(),
        ];
        let mut s = src();
        s.narrowing = Narrowing {
            time: Some((
                "ts".into(),
                Some("2026-01-01T00:00:00Z".into()),
                Some("2026-01-02T00:00:00+01:00".into()),
            )),
            entities: Some(("meter".into(), hostile.clone())),
            ..Narrowing::default()
        };
        let plan = s
            .plan_from(vec![
                ("ts".into(), "DateTime".into(), String::new()),
                (
                    "meter".into(),
                    "LowCardinality(String)".into(),
                    String::new(),
                ),
                ("kw".into(), "Float32".into(), String::new()),
            ])
            .unwrap();
        let sql = plan.select(None);
        for v in &hostile {
            assert!(!sql.contains(v.as_str()), "{v:?} is in the SQL: {sql}");
        }
        assert!(!sql.contains("2026"), "{sql}");
        assert!(
            sql.contains("`meter` IN {benchy_ids:Array(String)}"),
            "{sql}"
        );
        assert!(
            sql.contains("`ts` >= fromUnixTimestamp64Micro({benchy_start:Int64}, 'UTC')"),
            "{sql}"
        );
        assert!(sql.contains("`ts` < fromUnixTimestamp64Micro({benchy_end:Int64}, 'UTC')"));
        let params: BTreeMap<_, _> = plan.params().iter().cloned().collect();
        assert_eq!(params["param_benchy_start"], "1767225600000000");
        assert_eq!(params["param_benchy_end"], "1767308400000000");
        assert_eq!(
            params["param_benchy_ids"],
            "['it\\'s','back\\\\slash','{benchy_ids:String}','\\') OR 1=1 --','tab\\there',\
             'Zähler ü','`tick`']"
        );
        // And on the wire every parameter is percent-encoded: no `&` or `=` of a value can start
        // another one.
        assert_eq!(url_encode("a&b=c d'{}"), "a%26b%3Dc%20d%27%7B%7D");
        assert_eq!(url_encode("ü"), "%C3%BC");
    }

    /// A window without an offset means a different instant to each reader.
    #[test]
    fn a_time_window_must_say_its_offset() {
        assert_eq!(rfc3339_micros("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(rfc3339_micros("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(rfc3339_micros("1969-12-31T23:00:00-01:00"), Some(0));
        assert_eq!(
            rfc3339_micros("2026-10-11T12:34:56.5Z"),
            Some(1_791_722_096_500_000)
        );
        assert_eq!(
            rfc3339_micros("2024-02-29T00:00:00.000001Z"),
            Some(1_709_164_800_000_001)
        );
        for bad in [
            "2026-01-01T00:00:00",
            "2026-01-01",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:00:00+0100",
            "2026-01-01T00:00:00.Z",
            "now",
            "",
        ] {
            assert_eq!(rfc3339_micros(bad), None, "{bad}");
        }
    }

    /// A URL reaches logs, so it may carry no user or password.
    #[test]
    fn a_clickhouse_url_cannot_carry_a_password() {
        assert!(check_url("http://localhost:8123").is_ok());
        assert!(check_url("https://ch.example:8443/proxy/").is_ok());
        let e = check_url("http://user:hunter2@localhost:8123").unwrap_err();
        assert!(
            e.contains("CLICKHOUSE_PASSWORD") && !e.contains("hunter2"),
            "{e}"
        );
        assert!(check_url("http://user@localhost:8123").is_err());
        assert!(check_url("http://localhost:8123/?user=x&password=y").is_err());
        assert!(check_url("localhost:8123").is_err());
        assert!(check_url("tcp://localhost:9000").is_err());
    }
}
