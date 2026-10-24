//! Credential regression tests with synthetic secrets and local HTTP servers only.
//! The CLI tests exercise the shipped binary's confined network loaders on Linux.

mod common;

use common::http::{FakeHttp, Reply};
use std::process::{Command, Stdio};

const SECRET: &str = "Q7pVz9kR2mN4sT6wX8yA0bC3dE5fG1hJ";

fn failed_startup(config: &str) -> String {
    let dir = common::unique("security-http");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("benchy.toml");
    std::fs::write(&path, config).unwrap();
    let output = common::spawn(
        Command::new(common::BIN)
            .args(["--config", path.to_str().unwrap()])
            .args(common::PLATFORM)
            .env("BENCHY_SECURITY_HTTP_SECRET", SECRET)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap()
    .wait_with_output()
    .unwrap();
    assert!(
        !output.status.success(),
        "the failed source must prevent startup"
    );
    assert!(
        output.stdout.is_empty(),
        "no source loaded, so MCP must not start"
    );
    let log = String::from_utf8(output.stderr).unwrap();
    assert!(
        log.contains("E_AUTH"),
        "the fixture must reach the source: {log}"
    );
    log
}

/// REQ-5 and SECURITY.md forbid a source credential in a log. ClickHouse's error body is
/// untrusted and may echo the header it received; the loader must redact it before logging.
#[test]
fn a_clickhouse_error_cannot_echo_its_password_into_the_operator_log() {
    let api = FakeHttp::start(|request| {
        let received = request
            .header("X-ClickHouse-Key")
            .unwrap_or("missing-header");
        Reply::status(401).body(format!(
            "authentication failed for supplied key: {received}"
        ))
    });
    let log = failed_startup(&format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\n\
         tables = [\"readings\"]\n\
         env = {{ CLICKHOUSE_PASSWORD = \"${{BENCHY_SECURITY_HTTP_SECRET}}\" }}\n",
        api.url
    ));
    assert_eq!(api.requests()[0].header("X-ClickHouse-Key"), Some(SECRET));
    assert!(
        !log.contains(SECRET),
        "the ClickHouse loader logged its own password: {log}"
    );
}

/// Redaction must precede the 2,000-character diagnostic excerpt: cutting first leaves an
/// unmatched credential prefix that reaches the log.
#[test]
fn an_http_error_is_redacted_before_its_text_is_truncated() {
    let api = FakeHttp::start(|request| {
        let received = request.header("X-Api-Key").unwrap_or("missing-header");
        Reply::status(401).body(format!("{}{received}", "x".repeat(1980)))
    });
    let log = failed_startup(&format!(
        "[[source]]\nname = \"readings\"\ntype = \"http\"\nurl = \"{}\"\n\
         format = \"csv\"\nretries = 0\n\
         env = {{ KEY = \"${{BENCHY_SECURITY_HTTP_SECRET}}\" }}\n\
         auth = {{ header = \"X-Api-Key\", env = \"KEY\" }}\n",
        api.url
    ));
    assert_eq!(api.requests()[0].header("X-Api-Key"), Some(SECRET));
    assert!(
        !log.contains(&SECRET[..20]),
        "the HTTP error leaked the first 20 characters of its credential: {log}"
    );
}

/// The bounded body reader can itself cut a secret. Leading whitespace makes that partial
/// credential visible when the error text is trimmed for the operator log.
#[test]
fn an_http_error_cannot_leak_a_secret_cut_by_the_body_read_limit() {
    let api = FakeHttp::start(|request| {
        let received = request.header("X-Api-Key").unwrap_or("missing-header");
        Reply::status(401).body(format!("{}{received}", " ".repeat(64 * 1024 - 20)))
    });
    let log = failed_startup(&format!(
        "[[source]]\nname = \"readings\"\ntype = \"http\"\nurl = \"{}\"\n\
         format = \"csv\"\nretries = 0\n\
         env = {{ KEY = \"${{BENCHY_SECURITY_HTTP_SECRET}}\" }}\n\
         auth = {{ header = \"X-Api-Key\", env = \"KEY\" }}\n",
        api.url
    ));
    assert_eq!(api.requests()[0].header("X-Api-Key"), Some(SECRET));
    assert!(
        !log.contains(&SECRET[..20]),
        "the bounded HTTP error reader leaked part of its credential: {log}"
    );
}

#[test]
fn a_clickhouse_error_cannot_leak_a_secret_cut_by_the_body_read_limit() {
    let api = FakeHttp::start(|request| {
        let received = request
            .header("X-ClickHouse-Key")
            .unwrap_or("missing-header");
        Reply::status(401).body(format!("{}{received}", " ".repeat(64 * 1024 - 20)))
    });
    let log = failed_startup(&format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\n\
         tables = [\"readings\"]\n\
         env = {{ CLICKHOUSE_PASSWORD = \"${{BENCHY_SECURITY_HTTP_SECRET}}\" }}\n",
        api.url
    ));
    assert_eq!(api.requests()[0].header("X-ClickHouse-Key"), Some(SECRET));
    assert!(
        !log.contains(&SECRET[..20]),
        "the bounded ClickHouse error reader leaked part of its credential: {log}"
    );
}

#[test]
fn clickhouse_check_redacts_an_echoed_password() {
    let api = FakeHttp::start(|request| {
        if request.path() == "/ping" {
            return Reply::ok("Ok.\n");
        }
        let received = request
            .header("X-ClickHouse-Key")
            .unwrap_or("missing-header");
        Reply::status(401).body(format!("authentication failed for key: {received}"))
    });
    let dir = common::unique("security-clickhouse-check");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("benchy.toml");
    std::fs::write(
        &path,
        format!(
            "[[source]]\ntype = \"clickhouse\"\nurl = \"{}\"\n\
             tables = [\"readings\"]\n\
             env = {{ CLICKHOUSE_PASSWORD = \"${{BENCHY_SECURITY_HTTP_SECRET}}\" }}\n",
            api.url
        ),
    )
    .unwrap();
    let output = common::spawn(
        Command::new(common::BIN)
            .args(["check", path.to_str().unwrap(), "readings"])
            .env("BENCHY_SECURITY_HTTP_SECRET", SECRET)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap()
    .wait_with_output()
    .unwrap();
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("E_AUTH"),
        "the check must reach the source: {text}"
    );
    assert!(api
        .requests()
        .iter()
        .any(|r| r.header("X-ClickHouse-Key") == Some(SECRET)));
    assert!(!text.contains(SECRET), "check echoed the password: {text}");
}
