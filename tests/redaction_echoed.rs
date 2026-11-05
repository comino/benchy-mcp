//! Credentials echoed back by a source, in a form benchy did not write, never reach its error
//! or its log (PRJ-110, PRJ-111; from the prj-quality sibling-paths hunt). Synthetic secrets and
//! local servers only.

mod common;

use common::http::{FakeHttp, Reply};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

/// Start the shipped binary, send `initialize`, and return whether it answered (it serves MCP)
/// and everything it logged. Stops it either way.
fn startup(args: &[&str], envs: &[(&str, &str)]) -> (bool, String) {
    let mut child = common::spawn(
        Command::new(common::BIN)
            .args(args)
            .args(common::PLATFORM)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let mut err = child.stderr.take().unwrap();
    let log = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2026-07-28", "capabilities": {},
        "clientInfo": {"name": "bughunt-sibling", "version": "0"}}});
    let mut input = child.stdin.take().unwrap();
    let _ = writeln!(input, "{hello}");
    let mut line = String::new();
    let _ = BufReader::new(child.stdout.take().unwrap()).read_line(&mut line);
    let _ = child.kill();
    let _ = child.wait();
    drop(input);
    (line.contains("\"result\""), log.join().unwrap())
}

fn write_config(tag: &str, text: &str) -> std::path::PathBuf {
    let dir = common::unique(tag);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("benchy.toml");
    std::fs::write(&path, text).unwrap();
    path
}

// ---- Credential redaction: the log path still compares one spelling only ----

/// The bughunt fix made URL stripping compare decoded values, since `%2f` and `%2F` are the
/// same octet (RFC 3986 §2.1). Redaction of the operator log still knows only benchy's own
/// uppercase spelling, so an API that echoes the key as it re-encodes it (.NET's UrlEncode
/// writes lowercase hex) puts the credential in the log. docs/SOURCES.md: every log line has
/// every secret "as written, percent-encoded, and Basic's base64 — replaced by `***`, an API's
/// error body included".
#[test]
fn an_http_error_echoing_the_key_with_lowercase_escapes_is_redacted() {
    const SECRET: &str = "sibling/secret-Q7pVz9kR2mN4sT6w";
    let lower = "sibling%2fsecret-Q7pVz9kR2mN4sT6w";
    let api =
        FakeHttp::start(move |_| Reply::status(401).body(format!("401: unknown api_key={lower}")));
    let cfg = write_config(
        "sibling-lower-redact",
        &format!(
            "[[source]]\nname = \"readings\"\ntype = \"http\"\nurl = \"{}/export\"\n\
             format = \"csv\"\nretries = 0\n\
             env = {{ KEY = \"${{BENCHY_SIBLING_SECRET}}\" }}\n\
             auth = {{ param = \"api_key\", env = \"KEY\" }}\n",
            api.url
        ),
    );
    let (served, log) = startup(
        &["--config", cfg.to_str().unwrap()],
        &[("BENCHY_SIBLING_SECRET", SECRET)],
    );
    assert!(!served, "the only source failed: {log}");
    assert_eq!(api.requests()[0].param("api_key").as_deref(), Some(SECRET));
    assert!(
        log.contains("E_AUTH"),
        "the fixture must reach the source: {log}"
    );
    assert!(
        !log.to_ascii_lowercase()
            .contains(&lower.to_ascii_lowercase()),
        "the operator log carries the credential, percent-encoded in lowercase: {log}"
    );
}

/// The bughunt fix shared redaction with ClickHouse; an exec connector's credentials get none.
/// A connector that relays its API's error (Python's `raise_for_status()` names the URL, key
/// and all) puts its `env` value in the operator log. SECURITY.md: "A source credential or an
/// exec connector's environment reaching ... an error message or a log" is credential exposure.
#[test]
fn an_exec_connectors_relayed_error_does_not_log_its_credential() {
    const TOKEN: &str = "sibling-exec-token-9fK2mQ7x";
    let dir = common::unique("sibling-exec-redact");
    std::fs::create_dir_all(&dir).unwrap();
    let connector = dir.join("connector.sh");
    std::fs::write(
        &connector,
        "#!/bin/sh\n[ \"$1\" = fetch ] || exit 1\n\
         printf '{\"type\":\"error\",\"code\":\"E_AUTH\",\"message\":\"401 Client Error: \
         Unauthorized for url: https://api.invalid/v1?api_key=%s\"}\\n' \"$TOKEN\" >&2\n\
         echo \"retrying with $TOKEN\" >&2\nexit 1\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&connector, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"readings\"\ntype = \"exec\"\ncommand = \"{}\"\n\
             [source.env]\nTOKEN = \"${{BENCHY_SIBLING_TOKEN}}\"\n\
             [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n",
            connector.display()
        ),
    )
    .unwrap();
    let (served, log) = startup(
        &["--config", cfg.to_str().unwrap()],
        &[("BENCHY_SIBLING_TOKEN", TOKEN)],
    );
    assert!(!served, "the only source failed: {log}");
    assert!(log.contains("E_AUTH"), "the connector must have run: {log}");
    assert!(
        !log.contains(TOKEN),
        "the operator log carries the connector's credential: {log}"
    );
}

// ---- BC and five-digit years: other types, other callers ----

/// PRJ-111's review: `describe` relayed a connector's stderr unmasked, in the operator's
/// `exec-test` CLI. Its environment is set as a fetch's is.
#[test]
fn an_exec_connectors_describe_does_not_relay_its_credential() {
    const TOKEN: &str = "sibling-describe-token-3Hq8Lw";
    let dir = common::unique("sibling-describe-redact");
    std::fs::create_dir_all(&dir).unwrap();
    let connector = dir.join("connector.sh");
    std::fs::write(
        &connector,
        "#!/bin/sh\necho \"describe saw $TOKEN\" >&2\nexit 1\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&connector, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"readings\"\ntype = \"exec\"\ncommand = \"{}\"\n\
             [source.env]\nTOKEN = \"${{BENCHY_SIBLING_TOKEN}}\"\n\
             [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n",
            connector.display()
        ),
    )
    .unwrap();
    let out = common::spawn(
        Command::new(common::BIN)
            .args(["exec-test", cfg.to_str().unwrap(), "readings"])
            .env("BENCHY_SIBLING_TOKEN", TOKEN)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap()
    .wait_with_output()
    .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains("describe"),
        "the connector's describe must have run: {said}"
    );
    assert!(
        !said.contains(TOKEN),
        "exec-test relayed the connector's credential: {said}"
    );
}
