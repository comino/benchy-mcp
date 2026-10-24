//! Regression tests from the host/configuration bughunt.

mod common;

use benchy_mcp::config;
use serde_json::json;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn config_with_last(last: &str) -> String {
    format!(
        r#"[[source]]
name = "readings"
type = "exec"
command = "/unused/connector"
schema = [{{name = "ts", type = "timestamptz"}}]
[source.narrow]
column = "ts"
last = "{last}"
"#
    )
}

/// A typo in a duration is a configuration error. Splitting the last byte of a UTF-8 suffix
/// instead of checking its character boundary previously panicked before returning an error.
#[test]
fn a_non_ascii_duration_suffix_is_a_config_error_not_a_panic() {
    let parsed = std::panic::catch_unwind(|| {
        config::parse(&config_with_last("30日"), Path::new("benchy.toml"))
    });
    assert!(parsed.is_ok(), "a malformed duration must not panic");
    let error = parsed.unwrap().expect_err("the duration must be refused");
    assert!(
        error.contains("last") && error.contains("duration"),
        "{error}"
    );
}

const CASE_COLLISION: &str = r#"[[source]]
name = "readings"
type = "file"
path = "first.csv"

[[source]]
name = "READINGS"
type = "file"
path = "second.csv"
"#;

/// Configured table names use DuckDB's case-insensitive identity, just as column names do.
/// The duplicate-source guard previously put the original spelling into its set, allowing
/// both declarations even though an unqualified query cannot distinguish their tables.
#[test]
fn source_names_that_differ_only_by_ascii_case_are_duplicates() {
    let parsed = config::parse(CASE_COLLISION, Path::new("benchy.toml"));
    assert!(
        parsed.is_err(),
        "readings and READINGS are the same SQL name and must be refused: {parsed:?}"
    );
}

/// The parser's case-sensitive duplicate check reached the shipped binary: it loaded both
/// inputs and completed the MCP handshake, presenting an ambiguous bench instead of refusing
/// the configuration. These files deliberately hold different values under the same SQL name.
#[test]
fn a_case_colliding_configuration_is_refused_before_serving_mcp() {
    let dir = common::unique("bughunt-case-collision");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("first.csv"), "value\n11\n").unwrap();
    std::fs::write(dir.join("second.csv"), "value\n22\n").unwrap();
    let config = dir.join("benchy.toml");
    std::fs::write(&config, CASE_COLLISION).unwrap();
    let mut child = Command::new(common::BIN)
        .arg("--config")
        .arg(&config)
        .args(common::PLATFORM)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let hello = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2026-07-28", "capabilities": {},
            "clientInfo": {"name": "bughunt", "version": "0"}
        }
    });
    {
        let mut input = child.stdin.take().unwrap();
        // A correctly refusing process may already have closed stdin.
        let _ = writeln!(input, "{hello}");
    }
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("configuration checking and the MCP handshake must finish within 20s");
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        !output.status.success() && output.stdout.is_empty(),
        "a duplicate source must stop startup before MCP starts; status={}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let error = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    assert!(
        error.contains("readings")
            && (error.contains("two sources") || error.contains("duplicate")),
        "startup must fail for the name collision, not an unrelated environment error: {error}"
    );
}
