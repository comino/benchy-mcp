//! REQ-17: the HTTP source (docs/HTTP-SOURCE.md) — its config, read and checked; its fetch,
//! against the scripted server in `common::http`, in process as a loader runs it and served; and
//! the scripted server itself. The origin-blind tests, which compare an HTTP table with the same
//! data as a file, are in `tests/origin.rs` with the other sources'.

mod common;

use benchy_mcp::admission::Budget;
use benchy_mcp::exec::{Fault, Fetched};
use benchy_mcp::workspace::LoadedTable;
use benchy_mcp::{config, Bench};
use common::http::{FakeHttp, Reply, Request};
use common::{unique, Server, BIN};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn parse(text: &str) -> Result<config::Sources, String> {
    config::parse(text, Path::new("/x/benchy.toml"))
}

/// Load a config's one source onto a bench of its own, as its loader does.
fn load(text: &str) -> (Result<(LoadedTable, Fetched), Fault>, Bench) {
    load_within(text, &Budget::default())
}

fn load_within(text: &str, budget: &Budget) -> (Result<(LoadedTable, Fetched), Fault>, Bench) {
    let s = parse(text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(s.builtin.len(), 1, "one source");
    let mut bench = common::bench();
    let r = s.builtin[0].load(&mut bench, budget);
    (r, bench)
}

/// The failure of a load that must fail.
fn refused(r: Result<(LoadedTable, Fetched), Fault>) -> Fault {
    match r {
        Ok((t, _)) => panic!("loaded {} row(s) of `{}`", t.rows, t.name),
        Err(f) => f,
    }
}

/// Every value as DuckDB writes it as text, `NULL` for a null, a row a line.
fn rows(bench: &Bench, sql: &str) -> Vec<String> {
    let conn = bench.connection();
    let mut stmt = conn
        .prepare(&format!("SELECT COLUMNS(*)::VARCHAR FROM ({sql})"))
        .unwrap();
    let mut out = Vec::new();
    let mut r = stmt.query([]).unwrap();
    while let Some(row) = r.next().unwrap() {
        let n = row.as_ref().column_count();
        let cells: Vec<String> = (0..n)
            .map(|i| match row.get_ref(i).unwrap() {
                duckdb::types::ValueRef::Null => "NULL".to_string(),
                _ => row.get::<_, String>(i).unwrap_or_else(|_| "?".into()),
            })
            .collect();
        out.push(cells.join("|"));
    }
    out
}

/// Whether the bench has a table of that name, committed or not.
fn has_table(bench: &Bench, name: &str) -> bool {
    bench
        .connection()
        .query_row(
            "SELECT count(*) FROM duckdb_tables() WHERE table_name = ?",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

/// An http source named `readings`.
fn source(url: &str, rest: &str) -> String {
    format!("[[source]]\nname = \"readings\"\ntype = \"http\"\nurl = \"{url}\"\n{rest}")
}

/// `n` NDJSON rows of meter readings, 15 minutes apart from 2014-01-01, over three meters.
fn ndjson(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "{{\"ts\":\"2014-01-{:02} {:02}:{:02}:00\",\"meter\":\"MT_00{}\",\"kw\":{}.5}}\n",
                1 + i / 96,
                i % 96 / 4,
                i % 4 * 15,
                1 + i % 3,
                i
            )
        })
        .collect()
}

#[test]
fn every_http_config_the_design_shows_is_accepted() {
    // Every format and every way of paging, written as docs/HTTP-SOURCE.md shows them: each is
    // a source a session loads, and none of the secrets it names is in what the config says.
    std::env::set_var("BENCHY_TEST_HTTP_TOKEN", "s3cret-token-value");
    let head = "[[source]]\nname = \"readings\"\ntype = \"http\"\n";
    for body in [
        "url = \"https://data.example.com/export/readings.arrows\"\nformat = \"arrow\"\n",
        "url = \"https://data.example.com/readings.parquet\"\nformat = \"parquet\"\n",
        "url = \"https://data.example.com/readings.csv.gz\"\nformat = \"csv\"\n\
         compression = \"gzip\"\ntimezone = \"Europe/Copenhagen\"\n",
        "url = \"https://data.example.com/readings\"\nformat = \"ndjson\"\nmethod = \"POST\"\n\
         body = \"{\\\"site\\\": \\\"north\\\"}\"\nheaders = { Accept = \"application/x-ndjson\" }\n\
         env = { TOKEN = \"${BENCHY_TEST_HTTP_TOKEN}\" }\nauth = { bearer = \"TOKEN\" }\n",
        "url = \"https://api.example.com/v1/readings\"\nformat = \"json\"\n\
         env = { KEY = \"${BENCHY_TEST_HTTP_TOKEN}\" }\n\
         auth = { header = \"X-Api-Key\", env = \"KEY\" }\n\
         [source.pages]\nrows = \"data\"\nnext = \"cursor\"\ncursor = \"meta.next_cursor\"\n\
         cursor_param = \"cursor\"\n",
        "url = \"https://api.example.com/v1/readings\"\nformat = \"json\"\n\
         [source.pages]\nrows = \"\"\nnext = \"offset\"\noffset_param = \"offset\"\n\
         limit_param = \"limit\"\npage_size = 1000\n",
        "url = \"https://api.example.com/v1/readings\"\nformat = \"json\"\n\
         [source.pages]\nrows = \"items\"\nnext = \"page\"\npage_param = \"page\"\n\
         size_param = \"per_page\"\npage_size = 100\n",
        "url = \"https://api.example.com/v1/readings\"\nformat = \"json\"\n\
         [source.pages]\nrows = \"items\"\nnext = \"link\"\nmax_pages = 500\n",
        "url = \"https://api.example.com/v1/readings\"\nformat = \"json\"\n\
         [source.pages]\nrows = \"results\"\nnext = \"url\"\nurl = \"links.next\"\n",
        "url = \"https://api.example.com/v1/readings\"\nformat = \"json\"\n\
         params = { site = \"north\" }\nretries = 5\n\
         rate_limit = { requests = 10, per_secs = 1 }\nredirect_hosts = [\"cdn.example.net\"]\n\
         [source.pages]\nrows = \"data\"\n\
         [source.narrow]\ncolumn = \"ts\"\nstart = \"2026-01-01T00:00:00Z\"\n\
         end = \"2026-02-01T00:00:00Z\"\nentity_column = \"meter\"\nentity_ids = [\"m1\"]\n\
         [source.pushdown]\nstart = \"from\"\nend = \"to\"\ntime_format = \"unix\"\n\
         entities = \"meter\"\n",
    ] {
        let s = parse(&format!("{head}{body}")).unwrap_or_else(|e| panic!("{body}: {e}"));
        assert_eq!(s.builtin.len(), 1, "{body}");
        assert_eq!(s.builtin[0].name(), "readings");
        assert!(!s.builtin[0].origin().contains("s3cret"));
    }
}

#[test]
fn an_http_source_refuses_what_it_cannot_apply() {
    // Config only. Each would otherwise do nothing, or something other than it says.
    let base = "[[source]]\nname = \"r\"\ntype = \"http\"\nurl = \"https://api.example.com/r\"\n";
    let json = "format = \"json\"\n";
    for (extra, says) in [
        ("", "needs `format`"),
        ("format = \"xml\"\n", "not one of"),
        (
            "format = \"csv\"\ntables = [\"a\"]\n",
            "`tables` applies only to clickhouse",
        ),
        ("format = \"csv\"\nquery = \"SELECT 1\"\n", "`query`"),
        ("format = \"csv\"\ndatabase = \"d\"\n", "`database`"),
        ("format = \"csv\"\ncommand = \"./c\"\n", "`command`"),
        ("format = \"csv\"\npath = \"r.csv\"\n", "`path`"),
        (
            "format = \"csv\"\nmethod = \"PUT\"\n",
            "\"GET\" or \"POST\"",
        ),
        (
            "format = \"csv\"\nbody = \"{}\"\n",
            "only with `method = \"POST\"`",
        ),
        ("format = \"csv\"\ncompression = \"lz4\"\n", "\"auto\""),
        ("format = \"csv\"\nunknown_key = 1\n", "unknown field"),
        (
            "format = \"csv\"\nheaders = { Host = \"x\" }\n",
            "benchy's to set",
        ),
        (
            "format = \"csv\"\nheaders = { \"Bad Name\" = \"x\" }\n",
            "not a header name",
        ),
        ("format = \"csv\"\nretries = 11\n", "at most 10"),
        (
            "format = \"csv\"\nrate_limit = { requests = 0, per_secs = 1 }\n",
            "never send",
        ),
        (
            "format = \"csv\"\nredirect_hosts = [\"https://cdn.example.net\"]\n",
            "not a host name",
        ),
        (
            "format = \"csv\"\n[source.pages]\nrows = \"data\"\n",
            "only to `format = \"json\"`",
        ),
        (json, "needs [source.pages]"),
        (
            &format!("{json}[source.pages]\nnext = \"link\"\n"),
            "needs `rows`",
        ),
        (
            &format!("{json}[source.pages]\nrows = \"a..b\"\n"),
            "is not a path",
        ),
        (
            &format!("{json}[source.pages]\nrows = \"d\"\nnext = \"scroll\"\n"),
            "is not one of",
        ),
        (
            &format!("{json}[source.pages]\nrows = \"d\"\nnext = \"cursor\"\ncursor = \"c\"\n"),
            "needs `cursor_param`",
        ),
        (
            &format!("{json}[source.pages]\nrows = \"d\"\nnext = \"link\"\ncursor = \"c\"\n"),
            "refused rather than ignored",
        ),
        (
            &format!("{json}[source.pages]\nrows = \"d\"\nmax_pages = 3\n"),
            "refused rather than ignored",
        ),
        (
            &format!(
                "{json}[source.pages]\nrows = \"d\"\nnext = \"offset\"\noffset_param = \"o\"\n\
                 limit_param = \"l\"\n"
            ),
            "needs `page_size`",
        ),
        (
            &format!(
                "{json}[source.pages]\nrows = \"d\"\nnext = \"offset\"\noffset_param = \"o\"\n\
                 limit_param = \"l\"\npage_size = 0\n"
            ),
            "would fetch nothing",
        ),
        (
            &format!(
                "{json}[source.pages]\nrows = \"d\"\nnext = \"page\"\npage_param = \"p\"\n\
                 size_param = \"n\"\n"
            ),
            "go together",
        ),
        (
            &format!("{json}[source.pages]\nrows = \"d\"\nnext = \"link\"\nmax_pages = 0\n"),
            "would fetch nothing",
        ),
        (
            &format!(
                "{json}params = {{ offset = \"5\" }}\n[source.pages]\nrows = \"d\"\n\
                 next = \"offset\"\noffset_param = \"offset\"\nlimit_param = \"l\"\n\
                 page_size = 10\n"
            ),
            "Leave it out of `params`",
        ),
        (
            &format!(
                "{json}[source.pages]\nrows = \"d\"\nnext = \"offset\"\noffset_param = \"o\"\n\
                 limit_param = \"o\"\npage_size = 10\n"
            ),
            "both set the parameter `o`",
        ),
        // Pushdown carries the operator's narrowing; without one it would carry nothing.
        (
            "format = \"csv\"\n[source.pushdown]\nstart = \"from\"\n",
            "sets no window",
        ),
        (
            "format = \"csv\"\n[source.narrow]\ncolumn = \"ts\"\nlast = \"30d\"\n\
             [source.pushdown]\nstart = \"from\"\nend = \"to\"\n",
            "has none",
        ),
        (
            "format = \"csv\"\n[source.narrow]\ncolumn = \"ts\"\nlast = \"30d\"\n\
             [source.pushdown]\nstart = \"from\"\ntime_format = \"iso\"\n",
            "is not one of",
        ),
        (
            "format = \"csv\"\n[source.pushdown]\ntime_format = \"unix\"\n",
            "neither is set",
        ),
        (
            "format = \"csv\"\n[source.pushdown]\ncolumns = \"fields\"\n",
            "names any",
        ),
        (
            "format = \"csv\"\n[source.pushdown]\nentities = \"ids\"\n",
            "sets none",
        ),
        (
            "format = \"csv\"\n[source.narrow]\ncolumn = \"ts\"\nstart = \"2026-01-01\"\n",
            "offset",
        ),
        (
            "format = \"csv\"\n[[source.schema]]\nname = \"a\"\ntype = \"int64\"\n\
             [[source.schema]]\nname = \"A\"\ntype = \"int64\"\n",
            "declared twice",
        ),
        (
            "format = \"csv\"\n[[source.schema]]\nname = \"a\"\ntype = \"int128\"\n",
            "not in the vocabulary",
        ),
        (
            "format = \"csv\"\ntime_column = \"nope\"\n[[source.schema]]\nname = \"a\"\n\
             type = \"int64\"\n",
            "not in its schema",
        ),
        // Pinned columns are the ones loaded, as for ClickHouse: two lists would disagree.
        (
            "format = \"csv\"\n[source.narrow]\ncolumns = [\"a\"]\n[[source.schema]]\n\
             name = \"a\"\ntype = \"int64\"\n",
            "keep one of the two",
        ),
        // A cursor needs a key to be kept in, and one path inside the other is no shape a
        // page can have.
        (
            &format!(
                "{json}[source.pages]\nrows = \"\"\nnext = \"cursor\"\ncursor = \"c\"\n\
                 cursor_param = \"c\"\n"
            ),
            "leaves no key",
        ),
        (
            &format!(
                "{json}[source.pages]\nrows = \"data\"\nnext = \"url\"\nurl = \"data.next\"\n"
            ),
            "overlap",
        ),
        // A CSV's dialect is a CSV's, one character, and not the quote or a line break.
        (
            "format = \"ndjson\"\ndelimiter = \";\"\n",
            "`delimiter` applies only to `format = \"csv\"`",
        ),
        ("format = \"csv\"\ndelimiter = \";;\"\n", "one character"),
        ("format = \"csv\"\ndelimiter = \"\"\n", "one character"),
        ("format = \"csv\"\ndelimiter = \"\\\"\"\n", "the quote"),
        ("format = \"csv\"\ndelimiter = \"\\n\"\n", "line"),
    ] {
        let e = parse(&format!("{base}{extra}"))
            .err()
            .unwrap_or_else(|| panic!("accepted: {extra}"));
        assert!(e.contains(says), "{extra}\nwanted `{says}`, got: {e}");
    }
    // And a key only an http source has is refused on another.
    for (other, key) in [
        ("type = \"file\"\npath = \"a.csv\"\n", "format"),
        (
            "type = \"clickhouse\"\nurl = \"http://h:8123\"\nquery = \"SELECT 1\"\n",
            "pages",
        ),
    ] {
        let extra = if key == "format" {
            "format = \"csv\"\n".to_string()
        } else {
            "[source.pages]\nrows = \"d\"\n".to_string()
        };
        let e = parse(&format!("[[source]]\nname = \"a\"\n{other}{extra}"))
            .err()
            .unwrap();
        assert!(e.contains(&format!("`{key}`")) && e.contains("http"), "{e}");
    }
}

#[test]
fn an_http_secret_comes_only_from_env_through_auth() {
    // A secret in this file is in the clear, and a URL reaches logs: a credential has one way
    // in — benchy's environment, through `env`, named by `auth` — and no refusal repeats it.
    std::env::set_var("BENCHY_TEST_HTTP_SECRET", "s3cret-value");
    let base = "[[source]]\nname = \"r\"\ntype = \"http\"\nformat = \"csv\"\n";
    let api = "url = \"https://api.example.com/r\"\n";
    for (extra, says) in [
        (
            "url = \"https://user:hunter2@api.example.com/r\"\n".to_string(),
            "user or password",
        ),
        (
            "url = \"https://api.example.com/r?api_key=hunter2\"\n".to_string(),
            "query string",
        ),
        (
            "url = \"ftp://api.example.com/r\"\n".to_string(),
            "http:// or https://",
        ),
        (
            format!("{api}headers = {{ Authorization = \"Bearer hunter2\" }}\n"),
            "carries a credential",
        ),
        (
            format!("{api}headers = {{ cookie = \"session=hunter2\" }}\n"),
            "carries a credential",
        ),
        (
            format!("{api}headers = {{ X-Key = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n"),
            "only available in `env`",
        ),
        (
            format!("{api}params = {{ key = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n"),
            "only available in `env`",
        ),
        (
            format!("{api}method = \"POST\"\nbody = \"${{BENCHY_TEST_HTTP_SECRET}}\"\n"),
            "only available in `env`",
        ),
        (
            format!("{api}env = {{ TOKEN = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n"),
            "handed to nothing",
        ),
        (
            format!(
                "{api}env = {{ TOKEN = \"${{BENCHY_TEST_HTTP_SECRET}}\", OTHER = \"x\" }}\n\
                 auth = {{ bearer = \"TOKEN\" }}\n"
            ),
            "handed to nothing",
        ),
        (
            format!("{api}auth = {{ bearer = \"TOKEN\" }}\n"),
            "does not set",
        ),
        (
            format!(
                "{api}env = {{ TOKEN = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n\
                 auth = {{ bearer = \"TOKEN\", header = \"X-Key\", env = \"TOKEN\" }}\n"
            ),
            "more than one form",
        ),
        (
            format!(
                "{api}env = {{ TOKEN = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n\
                 auth = {{ header = \"X-Key\" }}\n"
            ),
            "`auth` is one of",
        ),
        (
            format!(
                "{api}env = {{ U = \"u\", P = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n\
                 auth = {{ user = \"U\" }}\n"
            ),
            "`auth` is one of",
        ),
        (
            format!(
                "{api}env = {{ K = \"${{BENCHY_TEST_HTTP_SECRET}}\" }}\n\
                 headers = {{ X-Key = \"literal\" }}\nauth = {{ header = \"x-key\", env = \"K\" }}\n"
            ),
            "set elsewhere",
        ),
        (
            format!("{api}auth = {{ bearer = \"T\" }}\nenv = {{ T = \"${{BENCHY_TEST_HTTP_UNSET}}\" }}\n"),
            "is not set in benchy's environment",
        ),
    ] {
        let e = parse(&format!("{base}{extra}"))
            .err()
            .unwrap_or_else(|| panic!("accepted: {extra}"));
        assert!(e.contains(says), "{extra}\nwanted `{says}`, got: {e}");
        assert!(
            !e.contains("hunter2") && !e.contains("s3cret"),
            "a refusal repeats a secret: {e}"
        );
    }
}

#[test]
fn the_fake_http_server_answers_and_records_each_request() {
    // The stand-in the HTTP source's integration tests will run against: it has to send what
    // they need — a status, headers, a chunked body, a body cut short — and record what was
    // asked of it.
    let server = FakeHttp::start(|req| match req.path() {
        "/page" => Reply::ok(format!("page {}", req.param("cursor").unwrap_or_default()))
            .header("Link", "</page?cursor=2>; rel=\"next\""),
        "/chunked" => Reply::ok(vec![b'x'; 10_000]).chunked(777),
        "/cut" => Reply::ok(vec![b'y'; 10_000]).cut_after(100),
        "/busy" => Reply::status(429).header("Retry-After", "1"),
        _ => Reply::status(404),
    });
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut r = agent
        .get(&format!("{}/page?cursor=a%2Fb", server.url))
        .header("X-Api-Key", "k")
        .call()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers().get("Link").unwrap().to_str().unwrap(),
        "</page?cursor=2>; rel=\"next\""
    );
    assert_eq!(r.body_mut().read_to_string().unwrap(), "page a/b");

    let mut r = agent
        .get(&format!("{}/chunked", server.url))
        .call()
        .unwrap();
    assert_eq!(r.body_mut().read_to_vec().unwrap().len(), 10_000);

    let mut r = agent.get(&format!("{}/cut", server.url)).call().unwrap();
    assert!(
        r.body_mut().read_to_vec().is_err(),
        "a body cut short is an error to the client"
    );

    let r = agent.get(&format!("{}/busy", server.url)).call().unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers().get("Retry-After").unwrap(), "1");

    agent
        .post(&format!("{}/q", server.url))
        .send("{\"a\":1}")
        .unwrap();

    let seen = server.requests();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].param("cursor").as_deref(), Some("a/b"));
    assert_eq!(seen[0].header("x-api-key"), Some("k"));
    assert_eq!(seen[4].method, "POST");
    assert_eq!(seen[4].body, b"{\"a\":1}");
}

/// A server that answers each request in turn from `replies`, the last one again once they run
/// out, and keeps when each request arrived.
fn scripted(replies: Vec<Reply>) -> (FakeHttp, Arc<Mutex<Vec<Instant>>>) {
    let at = Arc::new(Mutex::new(Vec::new()));
    let log = at.clone();
    let server = FakeHttp::start(move |_| {
        let mut at = log.lock().unwrap();
        at.push(Instant::now());
        replies[(at.len() - 1).min(replies.len() - 1)].clone()
    });
    (server, at)
}

#[test]
fn retries_back_off_and_honour_retry_after_within_the_deadline() {
    // A 503 that says when to come back is retried then, not sooner; a 429 that does not is
    // retried after a short, jittered wait; and then the rows load.
    let (server, at) = scripted(vec![
        Reply::status(503).header("Retry-After", "1"),
        Reply::status(429),
        Reply::ok("a\n1\n"),
    ]);
    let (r, bench) = load(&source(&format!("{}/r", server.url), "format = \"csv\"\n"));
    r.unwrap();
    assert_eq!(rows(&bench, "SELECT a FROM readings"), ["1"]);
    let at = at.lock().unwrap().clone();
    assert_eq!(at.len(), 3);
    assert!(
        at[1] - at[0] >= Duration::from_millis(950),
        "Retry-After: 1 was not waited for: {:?}",
        at[1] - at[0]
    );
    assert!(
        at[2] - at[1] < Duration::from_millis(900),
        "the first backoff is at most half a second: {:?}",
        at[2] - at[1]
    );

    // A wait that would pass the deadline fails now, rather than sleeping into it.
    let (server, at) = scripted(vec![Reply::status(429).header("Retry-After", "120")]);
    let started = Instant::now();
    let (r, _) = load(&source(
        &format!("{}/r", server.url),
        "format = \"csv\"\ndeadline_secs = 5\n",
    ));
    let f = refused(r);
    assert_eq!(f.code, "E_TIMEOUT", "{}", f.detail);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(at.lock().unwrap().len(), 1);

    // Past its retries, a server that stays down is unavailable; each try was one request.
    let (server, at) = scripted(vec![Reply::status(503)]);
    let (r, _) = load(&source(
        &format!("{}/r", server.url),
        "format = \"csv\"\nretries = 2\n",
    ));
    let f = refused(r);
    assert_eq!(f.code, "E_UNAVAILABLE", "{}", f.detail);
    assert!(f.detail.contains("503"), "{}", f.detail);
    assert_eq!(at.lock().unwrap().len(), 3);

    // Any other 4xx is an answer, not a hiccup: never retried.
    for (status, code) in [(404, "E_CONFIG"), (401, "E_AUTH"), (403, "E_AUTH")] {
        let (server, at) = scripted(vec![Reply::status(status).body("no")]);
        let (r, _) = load(&source(&format!("{}/r", server.url), "format = \"csv\"\n"));
        let f = refused(r);
        assert_eq!(f.code, code, "{status}: {}", f.detail);
        assert_eq!(at.lock().unwrap().len(), 1, "{status} was retried");
    }

    // A server that never answers is held to the source's one deadline.
    let (server, _) = scripted(vec![Reply::ok("a\n1\n").delay(Duration::from_secs(30))]);
    let started = Instant::now();
    let (r, _) = load(&source(
        &format!("{}/r", server.url),
        "format = \"csv\"\ndeadline_secs = 2\nretries = 0\n",
    ));
    let f = refused(r);
    assert_eq!(f.code, "E_TIMEOUT", "{}", f.detail);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_redirect_off_the_origin_carries_no_credential() {
    // The shape of a presigned object-store URL behind an API: the API answers with a redirect
    // to another host. Followed only to a host the operator listed, and then without the key.
    std::env::set_var("BENCHY_TEST_HTTP_KEY", "k-0b1d-secret");
    let cdn = FakeHttp::start(|_| Reply::ok("a\n7\n"));
    let cdn_port = cdn.url.rsplit(':').next().unwrap().to_string();
    let port = cdn_port.clone();
    let api = FakeHttp::start(move |req| match req.path() {
        "/same" => Reply::status(302).header("Location", "/data?part=1"),
        "/data" => Reply::ok("a\n1\n"),
        // The key echoed back on the URL it points at, as some APIs do.
        "/cdn" => Reply::status(307).header(
            "Location",
            &format!("http://127.0.0.1:{port}/obj?sig=abc&api_key=k-0b1d-secret"),
        ),
        "/away" => Reply::status(302).header("Location", &format!("http://localhost:{port}/obj")),
        _ => Reply::status(404),
    });
    let header = "format = \"csv\"\nenv = { KEY = \"${BENCHY_TEST_HTTP_KEY}\" }\n\
                  auth = { header = \"X-Api-Key\", env = \"KEY\" }\n";
    let param = "format = \"csv\"\nenv = { KEY = \"${BENCHY_TEST_HTTP_KEY}\" }\n\
                 auth = { param = \"api_key\", env = \"KEY\" }\n";
    // A listed host is reached on the port it is listed with, its scheme's when none is: the
    // loader may connect to those ports and no other (REQ-3).
    let listed = format!("redirect_hosts = [\"127.0.0.1:{cdn_port}\"]\n");
    let listed = listed.as_str();

    // The same origin: followed, with the credential.
    let (r, bench) = load(&source(&format!("{}/same", api.url), header));
    r.unwrap();
    assert_eq!(rows(&bench, "SELECT a FROM readings"), ["1"]);
    let seen = api.requests();
    assert_eq!(seen.len(), 2);
    assert!(seen
        .iter()
        .all(|q| q.header("X-Api-Key") == Some("k-0b1d-secret")));
    assert_eq!(seen[1].param("part").as_deref(), Some("1"));

    // A listed host: followed, without the header and without the parameter.
    for auth in [header, param] {
        let before = cdn.requests().len();
        let (r, bench) = load(&source(
            &format!("{}/cdn", api.url),
            &format!("{auth}{listed}"),
        ));
        r.unwrap();
        assert_eq!(rows(&bench, "SELECT a FROM readings"), ["7"]);
        let to_cdn: Vec<Request> = cdn.requests()[before..].to_vec();
        assert_eq!(to_cdn.len(), 1);
        assert_eq!(to_cdn[0].header("X-Api-Key"), None, "{auth}");
        assert_eq!(to_cdn[0].param("api_key"), None, "{auth}");
        assert_eq!(to_cdn[0].param("sig").as_deref(), Some("abc"));
    }

    // Not listed: refused, naming where it pointed and nothing of its path; nothing is sent.
    let before = cdn.requests().len();
    let (r, _) = load(&source(
        &format!("{}/away", api.url),
        &format!("{header}{listed}"),
    ));
    let f = refused(r);
    assert_eq!(f.code, "E_CONFIG", "{}", f.detail);
    assert!(
        f.detail.contains(&format!("http://localhost:{cdn_port}")),
        "{}",
        f.detail
    );
    assert!(!f.detail.contains("/obj"), "{}", f.detail);
    assert_eq!(cdn.requests().len(), before);
    // Nor is the listed host reached at all without `redirect_hosts`, or on a port it is not
    // listed with.
    for hosts in [
        "",
        "redirect_hosts = [\"127.0.0.1\"]\n",
        "redirect_hosts = [\"127.0.0.1:1\"]\n",
    ] {
        let (r, _) = load(&source(
            &format!("{}/cdn", api.url),
            &format!("{header}{hosts}"),
        ));
        assert_eq!(refused(r).code, "E_CONFIG", "{hosts}");
        assert_eq!(cdn.requests().len(), before, "{hosts}");
    }
    // The loader's ports: the source's own, and each listed host's.
    let s = parse(&source(
        "https://api.example.com/r",
        "format = \"csv\"\nredirect_hosts = [\"cdn.example.net\", \"files.example.org:8443\"]\n",
    ))
    .unwrap();
    assert_eq!(s.builtin[0].ports(), [443, 8443]);
}

/// `body` gzip-compressed.
fn gzip(body: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(body).unwrap();
    e.finish().unwrap()
}

#[test]
fn an_http_body_is_held_to_max_bytes_as_it_decompresses() {
    // `max_bytes` counts the body as it decodes, so a small compressed body that decodes to a
    // great deal is stopped at the cap: decoding stops there, rather than after the whole of it.
    let mut csv = String::from("n,pad\n");
    let pad = "x".repeat(90);
    for i in 0..100_000 {
        csv.push_str(&format!("{i},{pad}\n"));
    }
    let gz = gzip(csv.as_bytes());
    let zst = zstd::bulk::compress(csv.as_bytes(), 3).unwrap();
    assert!(csv.len() > 9_000_000 && gz.len() < 500_000 && zst.len() < 1_500_000);
    let (gz, zst) = (Arc::new(gz), Arc::new(zst));
    let server = FakeHttp::start(move |req| match req.path() {
        "/encoded.csv" => Reply::ok(gz.to_vec()).header("Content-Encoding", "gzip"),
        "/encoded-zstd.csv" => Reply::ok(zst.to_vec()).header("Content-Encoding", "zstd"),
        "/readings.csv.gz" => Reply::ok(gz.to_vec())
            .header("Content-Type", "application/gzip")
            .chunked(64 * 1024),
        _ => Reply::status(404),
    });
    for (path, extra) in [
        ("/encoded.csv", ""),
        ("/encoded-zstd.csv", ""),
        ("/readings.csv.gz", "compression = \"gzip\"\n"),
    ] {
        let url = format!("{}{path}", server.url);
        let (r, bench) = load(&source(&url, &format!("format = \"csv\"\n{extra}")));
        r.unwrap_or_else(|f| panic!("{path}: [{}] {}", f.code, f.detail));
        assert_eq!(rows(&bench, "SELECT count(*) FROM readings"), ["100000"]);

        let budget = Budget::default();
        let (r, bench) = load_within(
            &source(
                &url,
                &format!("format = \"csv\"\n{extra}max_bytes = 1000000\n"),
            ),
            &budget,
        );
        let f = refused(r);
        assert_eq!(f.code, "E_TOO_LARGE", "{path}: {}", f.detail);
        assert!(!has_table(&bench, "readings"));
        let decoded = budget.spent().1;
        assert!(
            (1_000_000..1_200_000).contains(&decoded),
            "{path}: decoded {decoded} bytes under a cap of 1,000,000"
        );
    }
}

/// A Parquet file of `n` rows in row groups of 10,000, as DuckDB writes it.
fn parquet(n: u64) -> Vec<u8> {
    let dir = unique("http-parquet");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("t.parquet");
    duckdb::Connection::open_in_memory()
        .unwrap()
        .execute_batch(&format!(
            "COPY (SELECT range AS n, (range * 0.5)::DOUBLE AS kw FROM range({n})) TO '{}' \
             (FORMAT parquet, ROW_GROUP_SIZE 10000)",
            p.display()
        ))
        .unwrap();
    let bytes = std::fs::read(&p).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

#[test]
fn a_parquet_body_over_max_rows_is_refused_from_its_footer() {
    // A Parquet file says how many rows it holds in its footer, which DuckDB reads without a row
    // group. Here every row group is garbage and the footer is whole: refused from the footer,
    // the load never reaches the garbage.
    let good = parquet(100_000);
    let mut broken = good.clone();
    let half = broken.len() / 2;
    for b in &mut broken[4..half] {
        *b = 0xAB;
    }
    let (good, broken) = (Arc::new(good), Arc::new(broken));
    let server = FakeHttp::start(move |req| match req.path() {
        "/good.parquet" => Reply::ok(good.to_vec()),
        "/broken.parquet" => Reply::ok(broken.to_vec()),
        _ => Reply::status(404),
    });
    let (r, bench) = load(&source(
        &format!("{}/good.parquet", server.url),
        "format = \"parquet\"\n",
    ));
    r.unwrap();
    assert_eq!(
        rows(
            &bench,
            "SELECT count(*), sum(n), any_value(typeof(n)), any_value(typeof(kw)) FROM readings"
        ),
        ["100000|4999950000|BIGINT|DOUBLE"]
    );
    let budget = Budget::default();
    let (r, bench) = load_within(
        &source(
            &format!("{}/broken.parquet", server.url),
            "format = \"parquet\"\nmax_rows = 1000\n",
        ),
        &budget,
    );
    let f = refused(r);
    assert_eq!(f.code, "E_TOO_LARGE", "{}", f.detail);
    assert!(f.detail.contains("100000"), "{}", f.detail);
    assert!(!has_table(&bench, "readings"));
    assert_eq!(budget.spent().0, 0, "no row was read");
    // The staged body is gone with the refusal.
    let left: Vec<_> = std::fs::read_dir(bench.dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("bench.duckdb"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A cursor-paged API over `pages`, each a list of row objects: `/api?cursor=<i>` answers page
/// `i`, with the next page's cursor while there is one. `broken` replaces page 1's answer.
fn cursor_api(pages: Vec<String>, broken: Option<Reply>) -> FakeHttp {
    FakeHttp::start(move |req| {
        let i: usize = req.param("cursor").map_or(0, |c| c.parse().unwrap());
        if i == 1 {
            if let Some(b) = &broken {
                return b.clone();
            }
        }
        let next = if i + 1 < pages.len() {
            format!("\"{}\"", i + 1)
        } else {
            "null".into()
        };
        Reply::ok(format!(
            "{{\"data\": [{}], \"meta\": {{\"next_cursor\": {next}}}}}",
            pages[i]
        ))
    })
}

const CURSOR: &str = "format = \"json\"\nretries = 1\n[source.pages]\nrows = \"data\"\n\
                      next = \"cursor\"\ncursor = \"meta.next_cursor\"\ncursor_param = \"cursor\"\n";

/// Three pages of rows `{"n": …}`.
fn three_pages() -> Vec<String> {
    (0..3)
        .map(|p| {
            (0..100)
                .map(|i| format!("{{\"n\": {}}}", p * 100 + i))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

#[test]
fn a_failed_page_fails_the_source_with_no_partial_table() {
    // Page 0 is fine; page 1 breaks. Nothing of page 0 may be left on the bench: a table that
    // stops at page 1 of 3 is a wrong answer with nothing to say so.
    let (api, bench) = {
        let api = cursor_api(three_pages(), None);
        let (r, bench) = load(&source(&format!("{}/api", api.url), CURSOR));
        r.unwrap();
        (api, bench)
    };
    assert_eq!(
        rows(&bench, "SELECT count(*), sum(n) FROM readings"),
        ["300|44850"]
    );
    assert_eq!(api.requests().len(), 3);

    let ok = Reply::ok("{\"data\": [{\"n\": 1}], \"meta\": {\"next_cursor\": null}}".to_string());
    let long = ok.body.len();
    for (what, broken, code) in [
        (
            "an error past its retries",
            Reply::status(500),
            "E_UNAVAILABLE",
        ),
        (
            "a page cut short",
            ok.clone().cut_after(long / 2),
            "E_HOST_TRUNCATED",
        ),
        ("a page that is not JSON", Reply::ok("<html>"), "E_CONFIG"),
        (
            "a page without its rows",
            Reply::ok("{\"meta\": {\"next_cursor\": null}}"),
            "E_CONFIG",
        ),
    ] {
        let api = cursor_api(three_pages(), Some(broken));
        let (r, bench) = load(&source(&format!("{}/api", api.url), CURSOR));
        let f = refused(r);
        assert_eq!(f.code, code, "{what}: {}", f.detail);
        assert!(!has_table(&bench, "readings"), "{what}: a table was left");
    }

    // A bulk body cut short: CSV and NDJSON are read by DuckDB's own reader, which sees the end
    // of what it was given and nothing more. The cut must still fail the load.
    for (format, body) in [
        ("csv", "n\n".to_string() + &"12345\n".repeat(20_000)),
        ("ndjson", ndjson(2000)),
    ] {
        let len = body.len();
        for reply in [
            Reply::ok(body.clone()).cut_after(len / 2),
            Reply::ok(body.clone()).chunked(4096).cut_after(len / 2),
        ] {
            let server = FakeHttp::start(move |_| reply.clone());
            let (r, bench) = load(&source(
                &format!("{}/body", server.url),
                &format!("format = \"{format}\"\n"),
            ));
            let f = refused(r);
            assert_eq!(f.code, "E_HOST_TRUNCATED", "{format}: {}", f.detail);
            assert!(!has_table(&bench, "readings"), "{format}: a table was left");
        }
    }
}

/// Watch `path` from another thread while a load runs: the first time it is there, whether it is
/// a regular file (not a pipe) and how long, once it is `at_least` bytes long or not a regular
/// file; and the most bytes it was seen to hold.
fn watch_spool(
    path: std::path::PathBuf,
    at_least: u64,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<(Option<(bool, u64)>, u64)> {
    std::thread::spawn(move || {
        let (mut first, mut most) = (None, 0);
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(m) = std::fs::symlink_metadata(&path) {
                most = most.max(m.len());
                if first.is_none() && (!m.is_file() || m.len() >= at_least) {
                    first = Some((m.is_file(), m.len()));
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        (first, most)
    })
}

/// What is left in a bench's staging directory besides the bench's own files.
fn left_in(bench: &Bench) -> Vec<String> {
    std::fs::read_dir(bench.dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("bench.duckdb"))
        .collect()
}

#[test]
fn a_csv_or_ndjson_body_is_spooled_to_staging_within_max_bytes_and_removed() {
    // R3-3: a CSV or NDJSON body is written to a file in the loader's staging directory as it
    // arrives, then read as the same file is read, with every thread. Through a FIFO DuckDB
    // read it on one. The spool holds no more than `max_bytes`, and is gone after the load,
    // whether it loaded or failed.
    let csv = "n\n".to_string() + &"12345\n".repeat(20_000);
    for (format, body) in [("csv", csv), ("ndjson", ndjson(2000))] {
        let len = body.len();
        let half = len / 2;
        let stall = Duration::from_millis(1500);
        let cases = [
            ("whole", Reply::ok(body.clone()), String::new(), None),
            (
                "cut short",
                Reply::ok(body.clone()).cut_after(half + 100),
                String::new(),
                Some("E_HOST_TRUNCATED"),
            ),
            (
                "past max_bytes",
                Reply::ok(body.clone()).chunked(4096),
                format!("max_bytes = {}\n", half - 1000),
                Some("E_TOO_LARGE"),
            ),
        ];
        for (what, reply, caps, code) in cases {
            // Past its cap, a body stalls after the cap, not before it: the load must not wait
            // for bytes past the cap, so it ends before the stall does.
            let at = if code == Some("E_TOO_LARGE") {
                half + 5000
            } else {
                half
            };
            let reply = reply.stall_after(at, stall);
            let server = FakeHttp::start(move |_| reply.clone());
            let text = source(
                &format!("{}/body", server.url),
                &format!("format = \"{format}\"\n{caps}"),
            );
            let s = parse(&text).unwrap();
            let mut bench = common::bench();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let seen = watch_spool(
                bench.dir().join(format!("http-body.{format}")),
                half as u64,
                stop.clone(),
            );
            let started = Instant::now();
            let r = s.builtin[0].load(&mut bench, &Budget::default());
            let took = started.elapsed();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let (first, most) = seen.join().unwrap();
            // Refused at its cap, the body is not stalled, and its spool may come and go
            // between two looks.
            if code != Some("E_TOO_LARGE") {
                let (is_file, _) =
                    first.unwrap_or_else(|| panic!("{format}, {what}: no spool seen"));
                assert!(is_file, "{format}, {what}: the body went through a pipe");
            }
            match code {
                None => {
                    r.unwrap_or_else(|f| panic!("{format}: [{}] {}", f.code, f.detail));
                    assert_eq!(most, len as u64, "{format}: the spool is the decoded body");
                    assert_eq!(
                        rows(&bench, "SELECT count(*) FROM readings"),
                        [if format == "csv" { "20000" } else { "2000" }]
                    );
                }
                Some(code) => {
                    let f = refused(r);
                    assert_eq!(f.code, code, "{format}, {what}: {}", f.detail);
                    assert!(!has_table(&bench, "readings"), "{format}, {what}");
                }
            }
            if code == Some("E_TOO_LARGE") {
                assert!(
                    most <= (half - 1000) as u64,
                    "{format}: {most} bytes spooled"
                );
                assert!(
                    took < stall,
                    "{format}: the load waited {took:?} past its cap"
                );
            }
            assert!(
                left_in(&bench).is_empty(),
                "{format}, {what}: {:?}",
                left_in(&bench)
            );
        }
    }
}

#[test]
fn a_later_json_page_with_a_wider_type_fails_the_source() {
    // Decision 6: the first page is sniffed by DuckDB, as a file of its rows would be, and that
    // schema is pinned. A later page that does not fit it fails, naming the column, rather than
    // loading as another type than the pages before it.
    let pages = vec!["{\"n\": 1}".to_string(), "{\"n\": 2.5}".to_string()];
    let api = cursor_api(pages, None);
    let (r, bench) = load(&source(&format!("{}/api", api.url), CURSOR));
    let f = refused(r);
    assert_eq!(f.code, "E_SCHEMA", "{}", f.detail);
    assert!(
        f.detail.contains("`n`") || f.detail.contains("'n'"),
        "{}",
        f.detail
    );
    assert!(!has_table(&bench, "readings"));

    // The same pages, declared as what they are, load.
    let api = cursor_api(
        vec!["{\"n\": 1}".to_string(), "{\"n\": 2.5}".to_string()],
        None,
    );
    let (r, bench) = load(&source(
        &format!("{}/api", api.url),
        &format!("{CURSOR}[[source.schema]]\nname = \"n\"\ntype = \"float64\"\n"),
    ));
    r.unwrap();
    assert_eq!(
        rows(&bench, "SELECT sum(n), any_value(typeof(n)) FROM readings"),
        ["3.5|DOUBLE"]
    );
}

#[test]
fn json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order() {
    // R4-3: a page's next request is found by a scan for its cursor, not by decoding its rows,
    // so the fetch goes on while earlier pages decode on threads of their own, a few pages
    // ahead at most. The rows still enter the table in page order, whichever page decodes
    // first, and a page that fails its decode still fails the source, as that page, with
    // nothing loaded.
    let sizes: Vec<usize> = (0..20)
        .map(|p| if p % 3 == 0 { 2_000 } else { 50 })
        .collect();
    let mut pages = Vec::new();
    let mut n = 0;
    for size in &sizes {
        pages.push(
            (n..n + size)
                .map(|i| format!("{{\"n\": {i}}}"))
                .collect::<Vec<_>>()
                .join(","),
        );
        n += size;
    }
    let api = cursor_api(pages.clone(), None);
    let (r, bench) = load(&source(&format!("{}/api", api.url), CURSOR));
    r.unwrap_or_else(|f| panic!("[{}] {}", f.code, f.detail));
    assert_eq!(
        rows(
            &bench,
            "SELECT count(*), count(*) FILTER (n != rowid) FROM readings"
        ),
        [format!("{n}|0")]
    );
    assert_eq!(api.requests().len(), 20);

    // The second page's last row holds a wider value than the first page's type, past 200,000
    // that take its decode tens of milliseconds, where the scan for its cursor takes a few. Read
    // one page at a time, the source failed after two requests; ahead, after a few more while
    // that page decoded, never all twenty.
    pages[1] = (0..200_000)
        .map(|i| format!("{{\"n\": {i}}}"))
        .chain(["{\"n\": 2.5}".to_string()])
        .collect::<Vec<_>>()
        .join(",");
    let api = cursor_api(pages, None);
    let (r, bench) = load(&source(&format!("{}/api", api.url), CURSOR));
    let f = refused(r);
    assert_eq!(f.code, "E_SCHEMA", "{}", f.detail);
    assert!(f.detail.contains("page 2"), "{}", f.detail);
    assert!(!has_table(&bench, "readings"));
    let asked = api.requests().len();
    assert!(
        (3..=7).contains(&asked),
        "{asked} request(s): not ahead, or more than five pages ahead"
    );
}

#[test]
fn every_paging_style_loads_every_page() {
    // 250 rows, 100 to a page, five ways of saying where the next page is. Offset and page
    // number end only at an empty page (decision 5), so they ask once more than the others.
    let all: Vec<String> = (0..250).map(|i| format!("{{\"n\": {i}}}")).collect();
    let page = move |from: usize| -> String { all[from.min(250)..(from + 100).min(250)].join(",") };
    let api = FakeHttp::start(move |req| {
        let q = |k: &str| req.param(k).map(|v| v.parse::<usize>().unwrap());
        match req.path() {
            "/offset" => Reply::ok(format!("[{}]", page(q("offset").unwrap()))),
            "/page" => Reply::ok(format!(
                "{{\"result\": {{\"items\": [{}]}}}}",
                page((q("page").unwrap() - 1) * q("per_page").unwrap())
            )),
            "/link" => {
                let at = q("at").unwrap_or(0);
                let r = Reply::ok(format!("{{\"items\": [{}]}}", page(at)));
                if at + 100 < 250 {
                    r.header(
                        "Link",
                        &format!(
                            "</link?at={}>; rel=\"next\", </link>; rel=\"first\"",
                            at + 100
                        ),
                    )
                } else {
                    r
                }
            }
            "/url" => {
                let at = q("after").unwrap_or(0);
                let next = if at + 100 < 250 {
                    format!("\"/url?after={}\"", at + 100)
                } else {
                    "null".into()
                };
                Reply::ok(format!(
                    "{{\"results\": [{}], \"links\": {{\"next\": {next}}}}}",
                    page(at)
                ))
            }
            "/cursor" => {
                let at = q("c").unwrap_or(0);
                let next = if at + 100 < 250 {
                    (at + 100).to_string()
                } else {
                    String::new()
                };
                Reply::ok(format!("{{\"rows\": [{}], \"c\": \"{next}\"}}", page(at)))
            }
            _ => Reply::status(404),
        }
    });
    for (path, pages, requests) in [
        (
            "/offset",
            "rows = \"\"\nnext = \"offset\"\noffset_param = \"offset\"\nlimit_param = \"limit\"\n\
             page_size = 100\n",
            4,
        ),
        (
            "/page",
            "rows = \"result.items\"\nnext = \"page\"\npage_param = \"page\"\n\
             size_param = \"per_page\"\npage_size = 100\n",
            4,
        ),
        ("/link", "rows = \"items\"\nnext = \"link\"\n", 3),
        (
            "/url",
            "rows = \"results\"\nnext = \"url\"\nurl = \"links.next\"\n",
            3,
        ),
        (
            "/cursor",
            "rows = \"rows\"\nnext = \"cursor\"\ncursor = \"c\"\ncursor_param = \"c\"\n",
            3,
        ),
    ] {
        let before = api.requests().len();
        let (r, bench) = load(&source(
            &format!("{}{path}", api.url),
            &format!("format = \"json\"\n[source.pages]\n{pages}"),
        ));
        r.unwrap_or_else(|f| panic!("{path}: [{}] {}", f.code, f.detail));
        assert_eq!(
            rows(
                &bench,
                "SELECT count(*), sum(n), any_value(typeof(n)) FROM readings"
            ),
            ["250|31125|BIGINT"],
            "{path}"
        );
        assert_eq!(api.requests().len() - before, requests, "{path}");
    }
}

#[test]
fn the_narrowing_is_pushed_down_as_the_operators_parameters() {
    // The window, the entities and the columns travel in the parameters the operator named, in
    // the format named. This API ignores them all and sends everything: the narrowing is
    // re-applied after ingest, so that costs bytes, never correctness.
    let body = Arc::new(
        (0..192)
            .map(|i| {
                format!(
                    "{{\"ts\":\"2014-01-{:02}T{:02}:{:02}:00Z\",\"meter\":\"MT_00{}\",\"kw\":1.5,\"quarter\":{}}}\n",
                    1 + i / 96,
                    i % 96 / 4,
                    i % 4 * 15,
                    1 + i % 3,
                    i % 96
                )
            })
            .collect::<String>(),
    );
    let api = FakeHttp::start(move |_| Reply::ok(body.to_string()));
    let (r, bench) = load(&source(
        &format!("{}/export", api.url),
        "format = \"ndjson\"\nparams = { site = \"north\" }\n\
         [source.narrow]\ncolumn = \"ts\"\nstart = \"2014-01-01T00:00:00Z\"\n\
         end = \"2014-01-01T12:00:00Z\"\nentity_column = \"meter\"\n\
         entity_ids = [\"MT_001\", \"MT_002\"]\ncolumns = [\"ts\", \"meter\", \"kw\"]\n\
         [source.pushdown]\nstart = \"from\"\nend = \"to\"\ntime_format = \"unix\"\n\
         entities = \"meter\"\ncolumns = \"fields\"\n",
    ));
    r.unwrap();
    let q = &api.requests()[0];
    assert_eq!(q.param("site").as_deref(), Some("north"));
    assert_eq!(q.param("from").as_deref(), Some("1388534400"));
    assert_eq!(q.param("to").as_deref(), Some("1388577600"));
    assert_eq!(q.param("meter").as_deref(), Some("MT_001,MT_002"));
    assert_eq!(q.param("fields").as_deref(), Some("ts,meter,kw"));
    assert_eq!(
        rows(
            &bench,
            "SELECT count(*), min(ts), max(ts), string_agg(DISTINCT meter, ',' ORDER BY meter) \
             FROM readings"
        ),
        ["32|2014-01-01 00:00:00+00|2014-01-01 11:30:00+00|MT_001,MT_002"]
    );
    assert_eq!(
        rows(
            &bench,
            "SELECT string_agg(column_name, ',' ORDER BY column_index) FROM duckdb_columns() \
             WHERE table_name = 'readings'"
        ),
        ["ts,meter,kw"]
    );

    // The same window as RFC 3339, milliseconds and days.
    for (format, from, to) in [
        ("rfc3339", "2014-01-01T00:00:00Z", "2014-01-01T12:00:00Z"),
        ("unix_ms", "1388534400000", "1388577600000"),
        ("date", "2014-01-01", "2014-01-01"),
    ] {
        let before = api.requests().len();
        let (r, _) = load(&source(
            &format!("{}/export", api.url),
            &format!(
                "format = \"ndjson\"\n[source.narrow]\ncolumn = \"ts\"\n\
                 start = \"2014-01-01T00:00:00Z\"\nend = \"2014-01-01T12:00:00Z\"\n\
                 [source.pushdown]\nstart = \"from\"\nend = \"to\"\ntime_format = \"{format}\"\n"
            ),
        ));
        r.unwrap();
        let q = &api.requests()[before];
        assert_eq!(q.param("from").as_deref(), Some(from), "{format}");
        assert_eq!(q.param("to").as_deref(), Some(to), "{format}");
    }
}

#[test]
fn a_refused_pushdown_parameter_fails_naming_the_parameters_sent() {
    // Decision 8: a 400 cannot say which parameter it disliked, and quietly fetching everything
    // can be a thousand-fold more bytes. So it fails, at once, naming what was sent.
    let (api, at) = scripted(vec![Reply::status(400).body("unknown parameter")]);
    let (r, _) = load(&source(
        &format!("{}/export", api.url),
        "format = \"ndjson\"\nretries = 3\n[source.narrow]\nentity_column = \"meter\"\n\
         entity_ids = [\"MT_001\"]\n[source.pushdown]\nentities = \"meter_id\"\n",
    ));
    let f = refused(r);
    assert_eq!(f.code, "E_CONFIG", "{}", f.detail);
    assert!(f.detail.contains("meter_id"), "{}", f.detail);
    assert!(f.detail.contains("unknown parameter"), "{}", f.detail);
    assert_eq!(at.lock().unwrap().len(), 1, "a 400 was retried");
}

#[test]
fn check_shows_the_request_the_schema_to_pin_and_the_first_pages_rows() {
    // `benchy-mcp check` for an http source: the request as sent, the secret masked; the schema
    // a load would pin, as `[[source.schema]]` to paste; and how many rows the first page held.
    let pages = three_pages();
    let api = cursor_api(pages, None);
    let dir = unique("http-check");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        source(
            &format!("{}/api", api.url),
            &format!(
                "params = {{ site = \"north\" }}\nenv = {{ KEY = \"${{HTTP_CHECK_KEY}}\" }}\n\
                 auth = {{ param = \"api_key\", env = \"KEY\" }}\n{CURSOR}"
            ),
        ),
    )
    .unwrap();
    let out = std::process::Command::new(BIN)
        .args(["check", cfg.to_str().unwrap(), "readings"])
        .env("HTTP_CHECK_KEY", "k-check-77aa")
        .output()
        .unwrap();
    let said =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains(&format!("GET {}/api?site=north&api_key=***", api.url)),
        "{said}"
    );
    assert!(
        said.contains("[[source.schema]]")
            && said.contains("name = \"n\"")
            && said.contains("type = \"int64\""),
        "{said}"
    );
    assert!(said.contains("100 row(s)"), "{said}");
    assert!(!said.contains("k-check-77aa"), "{said}");
    // Only the first page was asked for.
    assert_eq!(api.requests().len(), 1);
    assert_eq!(
        api.requests()[0].param("api_key").as_deref(),
        Some("k-check-77aa")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
#[test]
fn http_credentials_stay_in_the_loader() {
    // The token goes from benchy's environment, through the config, to the loader that fetches,
    // and to no other process: not the engine's environment, arguments or memory. Nor the log,
    // a parameter on the URL and an API that echoes it back included.
    const TOKEN: &str = "tok-5c1e-0b1d-a11ce";
    let api = FakeHttp::start(|req| {
        let bearer = req.header("Authorization") == Some(&format!("Bearer {TOKEN}"));
        let param = req.param("api_key").as_deref() == Some(TOKEN);
        if bearer || param {
            Reply::ok("{\"kw\": 904}\n")
        } else {
            let key = req.param("api_key").unwrap_or_default();
            Reply::status(401).body(format!("invalid key {key} for api.internal-7"))
        }
    });
    let dir = unique("http-creds");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        source(
            &format!("{}/readings", api.url),
            "format = \"ndjson\"\nenv = { TOKEN = \"${HTTP_TOKEN}\" }\nauth = { bearer = \"TOKEN\" }\n",
        ),
    )
    .unwrap();
    let mut s = Server::launch(
        &["--config", cfg.to_str().unwrap()],
        &[("HTTP_TOKEN", Path::new(TOKEN))],
    );
    let (err, body, _) = s.query("SELECT kw FROM readings");
    assert!(!err && body.contains("904"), "{body}");
    let engine = common::engine_of(s.pid());
    for what in ["environ", "cmdline"] {
        let bytes = std::fs::read(format!("/proc/{engine}/{what}")).unwrap();
        assert!(!common::holds(&bytes, TOKEN.as_bytes()), "{what}");
    }
    assert!(common::memory_holds(engine, b"readings"), "the scan works");
    assert!(
        !common::memory_holds(engine, TOKEN.as_bytes()),
        "the token is in the engine's memory"
    );
    drop(s);

    // A wrong key as a parameter, which the API echoes: E_AUTH in the log, the key in it masked.
    std::fs::write(
        &cfg,
        source(
            &format!("{}/readings", api.url),
            "format = \"ndjson\"\nenv = { KEY = \"${HTTP_TOKEN}\" }\n\
             auth = { param = \"api_key\", env = \"KEY\" }\n",
        ),
    )
    .unwrap();
    let out = std::process::Command::new(BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .env("HTTP_TOKEN", "wrong-key-77f1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(log.contains("E_AUTH") && log.contains("401"), "{log}");
    assert!(log.contains("invalid key ***"), "{log}");
    assert!(!log.contains("wrong-key-77f1"), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_http_fetch_is_charged_when_served() {
    // What a failed fetch read is spent on the session's budget before the next source is
    // fetched: `late` sends a body and then breaks off, `good` the same body whole. Under a
    // budget of one body and a half, `good` is refused once `late` has been charged; with room
    // for both, it loads.
    let body = Arc::new(ndjson(2000));
    let size = body.len() as u64;
    let b = body.clone();
    let api = FakeHttp::start(move |req| match req.path() {
        "/late" => Reply::ok(format!("{}{{\"ts\": oops", b)),
        _ => Reply::ok(b.to_string()),
    });
    let dir = unique("http-charged");
    std::fs::create_dir_all(&dir).unwrap();
    let serve = |bytes: u64| {
        let cfg = dir.join("benchy.toml");
        std::fs::write(
            &cfg,
            format!(
                "[budget]\nbytes = {bytes}\n\
                 [[source]]\nname = \"late\"\ntype = \"http\"\nurl = \"{u}/late\"\nformat = \"ndjson\"\n\
                 [[source]]\nname = \"good\"\ntype = \"http\"\nurl = \"{u}/good\"\nformat = \"ndjson\"\n",
                u = api.url
            ),
        )
        .unwrap();
        let out = std::process::Command::new(BIN)
            .args(common::PLATFORM)
            .args(["--config", cfg.to_str().unwrap()])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let err = serve(size + size / 2);
    assert!(
        err.contains("`late` was not loaded") && err.contains("`good` was not loaded"),
        "the second fetch went past what the failed first left: {err}"
    );
    let err = serve(3 * size);
    assert!(err.contains("good (2000 rows)"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_http_error_reaches_the_operator_not_the_agent() {
    // An API's errors carry its own words — hosts, accounts, the key it was sent. The operator's
    // log has the status and the message, the key masked; the agent hears that a table is not
    // available, and nothing of where it came from or why.
    std::env::set_var("BENCHY_TEST_HTTP_ERR_KEY", "k-err-9f3a");
    let api = FakeHttp::start(|req| {
        Reply::status(500).body(format!(
            "upstream db-prod-7.internal refused key {} for acct_7731",
            req.param("api_key").unwrap_or_default()
        ))
    });
    let dir = unique("http-err");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rooms.csv"), "room,floor\nlab,1\n").unwrap();
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"rooms\"\ntype = \"file\"\npath = \"rooms.csv\"\n{}",
            source(
                &format!("{}/v1/readings", api.url),
                "format = \"csv\"\nretries = 0\nenv = { KEY = \"${BENCHY_TEST_HTTP_ERR_KEY}\" }\n\
                 auth = { param = \"api_key\", env = \"KEY\" }\n",
            )
        ),
    )
    .unwrap();
    let log = dir.join("operator.log");
    let mut s = Server::launch_logging(
        &["--config", cfg.to_str().unwrap()],
        &[],
        std::fs::File::create(&log).unwrap(),
    );
    let v = s.call(
        "tools/call",
        serde_json::json!({"name": "describe", "arguments": {}}),
    );
    let mut seen = v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        seen.contains("readings") && seen.contains("rooms"),
        "{seen}"
    );
    seen.push_str(&s.query("SELECT * FROM readings").1);
    for secret in [
        "db-prod-7",
        "acct_7731",
        "k-err-9f3a",
        "127.0.0.1",
        "/v1/",
        "500",
        "HTTP",
        "E_UNAVAILABLE",
    ] {
        assert!(
            !seen.contains(secret),
            "the agent was told `{secret}`: {seen}"
        );
    }
    drop(s);
    let logged = std::fs::read_to_string(&log).unwrap();
    for told in [
        "500",
        "db-prod-7",
        "acct_7731",
        "E_UNAVAILABLE",
        "refused key ***",
    ] {
        assert!(
            logged.contains(told),
            "the operator was not told `{told}`: {logged}"
        );
    }
    assert!(!logged.contains("k-err-9f3a"), "{logged}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_declared_csv_is_read_in_its_stated_dialect() {
    // A declared CSV is read with its dialect stated, not sniffed: `delimiter` (`,` unless set),
    // RFC 4180's double quote as quote and escape, and a header. DuckDB's sniffer then only reads
    // the header, by which the declaration is matched, and the formats of dates and times: 66 M
    // instructions of a 1,000-row table instead of 462 M. A body in another dialect fails loudly,
    // never loads as other values.
    let comma = "ts,meter,kw\n2014-01-01 00:00:00,MT_001,1.5\n2014-01-01 00:15:00,\"MT_00,2\",\n";
    let server = FakeHttp::start(move |r| match r.path() {
        "/comma.csv" => Reply::ok(comma),
        "/semi.csv" => Reply::ok(
            comma
                .replace(',', ";")
                .replace("\"MT_00;2\"", "\"MT_00,2\""),
        ),
        "/dmy.csv" => Reply::ok("d,n\n01/02/2026,1\n13/02/2026,2\n"),
        "/backslash.csv" => Reply::ok("v,n\n\"a \\\"b\\\" c\",1\n"),
        _ => Reply::status(404),
    });
    let readings = "[[source.schema]]\nname = \"ts\"\ntype = \"timestamp\"\n\
                    [[source.schema]]\nname = \"meter\"\ntype = \"string\"\n\
                    [[source.schema]]\nname = \"kw\"\ntype = \"float64\"\n";
    let at = |path: &str, rest: &str| {
        source(
            &format!("{}{path}", server.url),
            &format!("format = \"csv\"\n{rest}{readings}"),
        )
    };
    let want = vec![
        "2014-01-01 00:00:00+00|MT_001|1.5".to_string(),
        "2014-01-01 00:15:00+00|MT_00,2|NULL".to_string(),
    ];
    let (r, bench) = load(&at("/comma.csv", ""));
    r.unwrap();
    assert_eq!(rows(&bench, "SELECT * FROM readings ORDER BY ts"), want);

    // Another delimiter is refused until it is stated, naming the setting.
    let f = refused(load(&at("/semi.csv", "")).0);
    assert_eq!(f.code, "E_SCHEMA", "{}", f.detail);
    assert!(f.detail.contains("delimiter"), "{}", f.detail);
    let (r, bench) = load(&at("/semi.csv", "delimiter = \";\"\n"));
    r.unwrap();
    assert_eq!(rows(&bench, "SELECT * FROM readings ORDER BY ts"), want);

    // The sniffer still finds a date's format, as it does for the file.
    let (r, bench) = load(&source(
        &format!("{}/dmy.csv", server.url),
        "format = \"csv\"\n[[source.schema]]\nname = \"d\"\ntype = \"date\"\n\
         [[source.schema]]\nname = \"n\"\ntype = \"int64\"\n",
    ));
    r.unwrap();
    assert_eq!(
        rows(&bench, "SELECT * FROM readings ORDER BY n"),
        vec!["2026-02-01|1", "2026-02-13|2"]
    );

    // Quotes escaped with a backslash are not RFC 4180's: refused, not read as other values.
    let (r, bench) = load(&source(
        &format!("{}/backslash.csv", server.url),
        "format = \"csv\"\n[[source.schema]]\nname = \"v\"\ntype = \"string\"\n\
         [[source.schema]]\nname = \"n\"\ntype = \"int64\"\n",
    ));
    refused(r);
    assert!(!has_table(&bench, "readings"));
}
