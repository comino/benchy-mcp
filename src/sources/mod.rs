//! REQ-4 and REQ-16: built-in sources — systems benchy reads itself, without a connector.
//!
//! A built-in source is the exec tier with the subprocess taken out. It runs in a loader of its
//! own, holding only its own source's `env` (REQ-5) as an exec connector's loader does, but
//! confined, since it runs only benchy's code: to its staging directory, the resolver's files and
//! TCP to [`BuiltIn::ports`] (REQ-3 `a_network_loader_reaches_only_its_source_and_its_staging`).
//! Its rows go through the same gate an exec connector's do: the
//! Arrow reader in [`crate::exec::read_arrow`], against a declared schema, under the source's
//! caps and the session's budget, with the narrowing re-applied after ingest
//! ([`crate::source::Ingest`]). What a built-in source adds is that the declaration need
//! not be written by hand: it is read from the source and mapped to benchy's types.
//!
//! Each source is a module behind [`Fetch`]. ClickHouse is the first; the next is HTTP, a module
//! that implements the four steps, and a variant of [`BuiltIn`].

pub mod clickhouse;
pub mod http;

use crate::exec::{Fault, Fetched, FieldDecl, Limits, Narrowing};
use crate::workspace::{Bench, LoadedTable};

/// What every built-in source does, in this order, for one table.
pub trait Fetch {
    /// What [`Fetch::plan`] decided: the schema the rows must arrive in, and how to ask for them.
    type Plan;
    /// The table it becomes.
    fn name(&self) -> &str;
    fn limits(&self) -> &Limits;
    /// What the operator lets this source return. Pushed to the source *and* re-applied after
    /// ingest, as an exec connector's is.
    fn narrowing(&self) -> &Narrowing;
    /// Reach the source, read its schema, and map it to benchy's types. Every failure that needs
    /// no data — the address, the credentials, a table that is not there — is found here.
    fn plan(&self) -> Result<Self::Plan, Fault>;
    fn schema<'a>(&self, plan: &'a Self::Plan) -> &'a [FieldDecl];
    /// What the fetch will cost, rows and bytes, asked of the source before anything is fetched
    /// (REQ-12).
    fn estimate(&self, plan: &Self::Plan) -> Result<(u64, u64), Fault>;
    /// The rows, at most `max_rows` of them in at most `max_bytes`, validated as they arrive and
    /// handed to `sink` one batch at a time. `limit` asks for fewer, for `benchy-mcp check`.
    /// What it reads is spent on `budget`, whether or not the rows are accepted.
    fn fetch(
        &self,
        plan: &Self::Plan,
        max_rows: u64,
        max_bytes: u64,
        limit: Option<u64>,
        budget: &crate::admission::Budget,
        sink: &mut crate::exec::Sink,
    ) -> Result<Fetched, Fault>;
}

/// A built-in source, as the config declares it and a loader's job carries it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// One per configured source, made once at startup and handed to one loader: the bytes a smaller
// variant wastes are nothing, and boxing both would only add an allocation each.
#[allow(clippy::large_enum_variant)]
pub enum BuiltIn {
    Clickhouse(clickhouse::Source),
    /// REQ-17: a URL, or a paginated JSON API (docs/HTTP-SOURCE.md).
    Http(http::Spec),
}

impl BuiltIn {
    pub fn name(&self) -> &str {
        match self {
            BuiltIn::Clickhouse(s) => s.name(),
            BuiltIn::Http(s) => &s.name,
        }
    }

    /// Put this source on the bench, within what the session has left.
    pub fn load(
        &self,
        bench: &mut Bench,
        budget: &crate::admission::Budget,
    ) -> Result<(LoadedTable, Fetched), Fault> {
        match self {
            BuiltIn::Clickhouse(s) => load_within(bench, s, budget),
            // HTTP has no count to ask before fetching, and three of its formats are read by
            // DuckDB itself rather than streamed: its own load, sharing the gate and the table
            // writer every stream and file goes through.
            BuiltIn::Http(s) => s.load(bench, budget),
        }
    }

    /// The TCP ports its loader may connect to, and no other (REQ-3
    /// `a_network_loader_reaches_only_its_source_and_its_staging`).
    pub fn ports(&self) -> Vec<u16> {
        match self {
            BuiltIn::Clickhouse(s) => port_of(&s.url).into_iter().collect(),
            BuiltIn::Http(s) => s.ports(),
        }
    }

    /// `benchy-mcp check`: each step of a load, without a server or a model.
    pub fn check(&self) -> Vec<crate::exec::Check> {
        match self {
            BuiltIn::Clickhouse(s) => s.check(),
            BuiltIn::Http(s) => s.check(),
        }
    }

    /// What the operator log calls it. Never the agent: it names a host.
    pub fn origin(&self) -> String {
        match self {
            BuiltIn::Clickhouse(s) => s.origin(),
            BuiltIn::Http(s) => s.origin(),
        }
    }
}

/// The port a URL connects to: its own, or its scheme's. `None` for a URL that is not one, which
/// the config refuses before any loader runs.
pub fn port_of(url: &str) -> Option<u16> {
    let uri: ureq::http::Uri = url.parse().ok()?;
    uri.port_u16().or(match uri.scheme_str()? {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    })
}

/// Plan, estimate, admit, fetch, store — the load every built-in source shares.
///
/// The estimate is checked against the source's own caps and against the session's budget
/// before a byte is fetched, so an over-budget load costs the source one count, not a scan
/// (REQ-12 `estimate_before_fetch`). The fetch is bounded by the same caps either way: an
/// estimate is an estimate.
pub fn load_within<S: Fetch>(
    bench: &mut Bench,
    src: &S,
    budget: &crate::admission::Budget,
) -> Result<(LoadedTable, Fetched), Fault> {
    let plan = crate::trace::span("builtin.plan", || src.plan())?;
    let (rows, bytes) = crate::trace::span("builtin.estimate", || src.estimate(&plan))?;
    let limits = src.limits();
    if rows > limits.max_rows || bytes > limits.max_bytes {
        return Err(Fault::host(
            "E_TOO_LARGE",
            format!(
                "source `{}` estimates {rows} row(s) / {bytes} byte(s), over its own cap of {} \
                 row(s) / {} byte(s). Refused before fetching: narrow it, or raise `max_rows` or \
                 `max_bytes` for this source.",
                src.name(),
                limits.max_rows,
                limits.max_bytes
            ),
        ));
    }
    budget.admit(src.name(), rows, bytes)?;
    let (rows_left, bytes_left) = budget.remaining();
    let max_rows = limits.max_rows.min(rows_left);
    let max_bytes = limits.max_bytes.min(bytes_left);
    let origin = crate::source::Origin {
        what: "built-in".into(),
        // Every instant a built-in source sends carries its zone.
        zone: crate::canonical::UTC,
    };
    let mut ingest = crate::source::Ingest::open(
        bench,
        src.name(),
        src.schema(&plan),
        &origin,
        src.narrowing(),
    )?;
    // The fetch spends what it reads on `budget`, failed or not.
    let fetched = crate::trace::span("builtin.stream", || {
        src.fetch(&plan, max_rows, max_bytes, None, budget, &mut |b| {
            ingest.push(b)
        })
    })?;
    let table = ingest.finish(&fetched.schema)?;
    Ok((crate::source::record(bench, table)?, fetched))
}

#[cfg(test)]
mod tests {
    use super::port_of;

    #[test]
    fn a_sources_port_is_its_urls_or_its_schemes() {
        // The one TCP port its loader may connect to: a wrong answer here is a load its own
        // sandbox refuses.
        assert_eq!(port_of("http://ch.example:8123"), Some(8123));
        assert_eq!(port_of("https://ch.example:8443/proxy"), Some(8443));
        assert_eq!(port_of("http://ch.example"), Some(80));
        assert_eq!(port_of("https://ch.example/"), Some(443));
        assert_eq!(port_of("https://[::1]:9000"), Some(9000));
        assert_eq!(port_of("ftp://ch.example"), None);
        assert_eq!(port_of("not a url"), None);
    }
}
