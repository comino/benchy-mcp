//! Regression tests from the HTTP-source bughunt, verified red before the fixes.
//! These preserve the documented redirect and pagination behavior.

mod common;

use benchy_mcp::admission::Budget;
use benchy_mcp::config;
use benchy_mcp::exec::{Fault, Fetched};
use benchy_mcp::sources::http::paginate::link_next;
use benchy_mcp::workspace::LoadedTable;
use benchy_mcp::Bench;
use common::http::{FakeHttp, Reply};
use std::path::Path;

fn source(url: &str, rest: &str) -> String {
    format!(
        "[[source]]\nname = \"readings\"\ntype = \"http\"\nurl = \"{url}\"\nretries = 0\n{rest}"
    )
}

fn load(text: &str) -> (Result<(LoadedTable, Fetched), Fault>, Bench) {
    let sources = config::parse(text, Path::new("/x/benchy.toml")).unwrap();
    let mut bench = common::bench();
    let result = sources.builtin[0].load(&mut bench, &Budget::default());
    (result, bench)
}

/// A quoted Link parameter may contain '<'. It is not a new link target: REQ-17
/// promises RFC 8288 Link pagination, and silently losing `next` truncates the table.
#[test]
fn a_quoted_link_title_does_not_hide_the_next_relation() {
    let header = r#"</readings?page=2>; title="readings < 10"; rel="next""#;
    assert_eq!(link_next(header).as_deref(), Some("/readings?page=2"));
}

/// Exercise the same parser through actual HTTP requests, JSON decoding and table
/// commit: a successful load must include both pages, not just the first one.
#[test]
fn link_pagination_loads_every_page_when_a_title_contains_an_angle_bracket() {
    let api = FakeHttp::start(|request| match request.param("page").as_deref() {
        None => Reply::ok(r#"{"items":[{"n":1}]}"#).header(
            "Link",
            r#"</readings?page=2>; title="readings < 10"; rel="next""#,
        ),
        Some("2") => Reply::ok(r#"{"items":[{"n":2}]}"#),
        _ => Reply::status(400),
    });
    let (result, bench) = load(&source(
        &format!("{}/readings", api.url),
        "format = \"json\"\n[source.pages]\nrows = \"items\"\nnext = \"link\"\n",
    ));
    result.unwrap();
    let count: i64 = bench
        .connection()
        .query_row("SELECT count(*) FROM readings", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "a valid next link must not silently truncate the table"
    );
    assert_eq!(api.requests().len(), 2);
}

/// The redirect allowlist applies to every hop (docs/HTTP-SOURCE.md, Redirects).
/// Leaving the source's origin strips credentials, but must not disable validation
/// of the next redirect. The unlisted hostname uses the same permitted TCP port,
/// so the loader's Landlock port rule cannot substitute for the HTTP allowlist.
#[test]
fn every_redirect_hop_is_checked_after_a_listed_cross_origin_hop() {
    let cdn = FakeHttp::start(|request| match request.path() {
        "/first" => Reply::status(302).header(
            "Location",
            &format!(
                "http://localhost:{}/unlisted",
                request.header("Host").unwrap().rsplit(':').next().unwrap()
            ),
        ),
        "/unlisted" => Reply::ok("n\n7\n"),
        _ => Reply::status(404),
    });
    let first = format!("{}/first", cdn.url);
    let api = FakeHttp::start(move |_| Reply::status(302).header("Location", &first));
    let allowed = cdn.url.strip_prefix("http://").unwrap();
    let (result, _) = load(&source(
        &format!("{}/readings", api.url),
        &format!("format = \"csv\"\nredirect_hosts = [\"{allowed}\"]\n"),
    ));
    assert!(
        cdn.requests()
            .iter()
            .all(|request| request.path() != "/unlisted"),
        "the client contacted an unlisted host after the first cross-origin redirect"
    );
    let fault = match result {
        Ok(_) => panic!("the source loaded data from an unlisted redirect host"),
        Err(fault) => fault,
    };
    assert_eq!(fault.code, "E_CONFIG", "{}", fault.detail);
}

/// A redirect may echo a secret under an unrelated parameter name. The documented
/// stripping rule is about its decoded value, not the case of percent-escape hex
/// digits: `%2f` and `%2F` both carry the same credential to the next host.
#[test]
fn an_echoed_secret_is_stripped_when_percent_escapes_use_lowercase_hex() {
    std::env::set_var("BENCHY_BUGHUNT_HTTP_ESCAPED_SECRET", "bughunt/secret");
    let cdn = FakeHttp::start(|_| Reply::ok("n\n7\n"));
    let next = format!("{}/object?echo=bughunt%2fsecret&part=1", cdn.url);
    let api = FakeHttp::start(move |_| Reply::status(302).header("Location", &next));
    let allowed = cdn.url.strip_prefix("http://").unwrap();
    let (result, _) = load(&source(
        &format!("{}/readings", api.url),
        &format!(
            "format = \"csv\"\nredirect_hosts = [\"{allowed}\"]\n\
             env = {{ KEY = \"${{BENCHY_BUGHUNT_HTTP_ESCAPED_SECRET}}\" }}\n\
             auth = {{ header = \"X-Api-Key\", env = \"KEY\" }}\n"
        ),
    ));
    result.unwrap();
    let requests = cdn.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header("X-Api-Key"), None);
    assert_eq!(requests[0].param("part").as_deref(), Some("1"));
    assert_eq!(
        requests[0].param("echo"),
        None,
        "the cross-origin request must not expose the percent-decoded secret"
    );
}

/// A Link target is resolved against the page that supplied it (REQ-17), including
/// a same-origin redirect's final URL. The alias has a different path directory.
#[test]
fn a_relative_next_link_uses_the_final_response_url_after_a_redirect() {
    let api = FakeHttp::start(|request| match (request.path(), request.param("page")) {
        ("/alias", _) => Reply::status(302).header("Location", "/v1/readings"),
        ("/v1/readings", None) => {
            Reply::ok(r#"{"items":[{"n":1}]}"#).header("Link", r#"<readings?page=2>; rel="next""#)
        }
        ("/v1/readings", Some(page)) if page == "2" => Reply::ok(r#"{"items":[{"n":2}]}"#),
        _ => Reply::status(404),
    });
    let (result, bench) = load(&source(
        &format!("{}/alias", api.url),
        "format = \"json\"\n[source.pages]\nrows = \"items\"\nnext = \"link\"\n",
    ));
    result.unwrap_or_else(|fault| {
        panic!(
            "a relative link must use /v1/readings as its base, not /alias: [{}] {}; requests: {:?}",
            fault.code,
            fault.detail,
            api.requests()
        )
    });
    let count: i64 = bench
        .connection()
        .query_row("SELECT count(*) FROM readings", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}
