//! The ingest benchmark's stage timings (src/trace.rs, bench/ingest.py): they reach the
//! operator's log from every process when asked for, and otherwise not at all, and never the
//! agent.

mod common;
use common::Server;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// Run a session over the small test folder with `envs`, describe and query it, and return what
/// the agent got and what the operator's log got.
fn session(envs: &[(&str, &Path)]) -> (String, String) {
    let dir = common::data_dir();
    let log = common::unique("trace-log");
    let file = std::fs::File::create(&log).unwrap();
    let mut s = Server::launch_logging(&[dir.to_str().unwrap()], envs, file);
    let mut agent = String::new();
    let d = s.call("tools/call", json!({"name": "describe", "arguments": {}}));
    agent += &d.to_string();
    let (err, body, _) = s.query("SELECT count(*) FROM readings");
    assert!(!err, "{body}");
    agent += &body;
    let tools: Value = s.call("tools/list", json!({}));
    agent += &tools.to_string();
    s.close(Duration::from_secs(10));
    let said = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&dir);
    (agent, said)
}

fn traced(log: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| l.strip_prefix("benchy-trace "))
        .map(|l| serde_json::from_str(l).expect("each trace line is one JSON object"))
        .collect()
}

#[test]
fn timings_reach_the_operator_log_from_every_process_and_never_the_agent() {
    let (agent, log) = session(&[("BENCHY_TRACE_TIMINGS", Path::new("1"))]);
    let events = traced(&log);
    for role in ["server", "engine", "loader"] {
        assert!(
            events
                .iter()
                .any(|e| e["role"] == role && e["what"] == "main"),
            "no `main` line from the {role}:\n{log}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["role"] == role && e.get("maxrss_kb").is_some()),
            "no usage line from the {role}:\n{log}"
        );
    }
    for stage in ["file.read", "write_table", "profile", "bench.finish"] {
        assert!(
            events
                .iter()
                .any(|e| e["role"] == "loader" && e["what"] == stage && e.get("us").is_some()),
            "no `{stage}` span from the loader:\n{log}"
        );
    }
    assert!(events.iter().any(|e| e["what"] == "engine.attach"), "{log}");
    assert!(!agent.contains("benchy-trace"), "{agent}");
    assert!(!agent.contains("maxrss"), "{agent}");
}

/// docs/history/PERFORMANCE-rounds.md, round 1 item 6: the engine's `Open` (a process start,
/// DuckDB's open, the seal) and the first loader need nothing from each other until the attach, so
/// the loader is started before the engine has answered. Waited for in turn, every cold start paid
/// for both.
#[test]
fn the_first_loader_starts_while_the_engine_opens() {
    let (_, log) = session(&[("BENCHY_TRACE_TIMINGS", Path::new("1"))]);
    let events = traced(&log);
    let at = |role: &str, what: &str, key: &str| {
        events
            .iter()
            .find(|e| e["role"] == role && e["what"] == what)
            .and_then(|e| e[key].as_u64())
            .unwrap_or_else(|| panic!("no `{what}` from the {role}:\n{log}"))
    };
    let loader = at("server", "loader.spawn", "t");
    let opened = at("engine", "engine.open", "t1");
    assert!(
        loader < opened,
        "the loader started {} µs after the engine had opened:\n{log}",
        loader.saturating_sub(opened)
    );
}

/// docs/history/PERFORMANCE-rounds.md, round 1 item 5: the engine's check of an attached file lists
/// every function DuckDB has, about 0.19 G instructions, so the tables a session starts with are
/// attached in one request and the listing is paid once, not once per table.
#[test]
fn the_tables_a_session_starts_with_are_attached_in_one_request() {
    let dir = common::data_dir();
    std::fs::copy(dir.join("readings.csv"), dir.join("meters.csv")).unwrap();
    let log = common::unique("trace-log");
    let file = std::fs::File::create(&log).unwrap();
    let mut s = Server::launch_logging(
        &[dir.to_str().unwrap()],
        &[("BENCHY_TRACE_TIMINGS", Path::new("1"))],
        file,
    );
    for table in ["readings", "meters"] {
        let (err, body, _) = s.query(&format!("SELECT count(*) FROM {table}"));
        assert!(!err, "{body}");
        assert!(body.contains("500"), "{body}");
    }
    s.close(Duration::from_secs(10));
    let log_text = std::fs::read_to_string(&log).unwrap();
    let _ = std::fs::remove_file(&log);
    let _ = std::fs::remove_dir_all(&dir);
    let events = traced(&log_text);
    let count = |what: &str| {
        events
            .iter()
            .filter(|e| e["role"] == "engine" && e["what"] == what)
            .count()
    };
    assert_eq!(count("engine.attach"), 1, "{log_text}");
    assert_eq!(count("attach.functions"), 1, "{log_text}");
    assert_eq!(count("attach.inspect"), 2, "{log_text}");
}

#[test]
fn without_the_variable_nothing_is_traced() {
    let (_, log) = session(&[]);
    assert!(!log.contains("benchy-trace"), "{log}");
    let (_, log) = session(&[("BENCHY_TRACE_TIMINGS", Path::new("0"))]);
    assert!(!log.contains("benchy-trace"), "{log}");
}
