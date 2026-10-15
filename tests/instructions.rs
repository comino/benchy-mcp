//! REQ-11's remaining named tests: `instructions_carry_deltas`, `version_not_hardcoded` and
//! `reference_is_on_demand`.
//!
//! A dialect primer in the per-call payload is expensive, because a large prompt repeats at every
//! step. So the split has to be enforced, not merely intended: the short deltas that must always
//! be known ride in the instructions, and the long reference sits behind a call.

mod common;
use common::{data_dir, Server};

fn instructions(s: &mut serde_json::Value) -> String {
    s["result"]["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn instructions_carry_deltas() {
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let mut init = s.call(
        "initialize",
        serde_json::json!({"protocolVersion": "2026-07-28", "capabilities": {},
                           "clientInfo": {"name": "t", "version": "0"}}),
    );
    let text = instructions(&mut init);

    assert!(text.contains("DuckDB"), "names the engine: {text}");
    for refused in ["read_csv", "read_parquet", "ATTACH", "INSTALL"] {
        assert!(
            text.contains(refused),
            "names {refused} as unavailable: {text}"
        );
    }
    assert!(
        text.contains("capped") || text.contains("truncated"),
        "says results are bounded: {text}"
    );
    assert!(
        text.contains("describe"),
        "says where to start, which is the step that pays for itself: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn version_not_hardcoded() {
    // Dialect drifts between releases. A version baked into a string literal goes stale in
    // silence, and an agent told the wrong one reasons from the wrong reference.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let mut init = s.call(
        "initialize",
        serde_json::json!({"protocolVersion": "2026-07-28", "capabilities": {},
                           "clientInfo": {"name": "t", "version": "0"}}),
    );
    let text = instructions(&mut init);
    s.notify("notifications/initialized");

    let (err, body, _) = s.query("SELECT version() AS v");
    assert!(!err, "{body}");
    let running = body
        .lines()
        .find(|l| l.starts_with('v') && l.len() > 1)
        .unwrap_or("")
        .trim()
        .to_string();
    assert!(running.starts_with('v'), "got a version back: {body}");
    assert!(
        text.contains(&running),
        "the instructions must name the engine that is actually running ({running}): {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn reference_is_on_demand() {
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let mut init = s.call(
        "initialize",
        serde_json::json!({"protocolVersion": "2026-07-28", "capabilities": {},
                           "clientInfo": {"name": "t", "version": "0"}}),
    );
    let text = instructions(&mut init);
    s.notify("notifications/initialized");

    let tools = s.call("tools/list", serde_json::json!({}));
    let listed: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        listed.contains(&"dialect".to_string()),
        "the reference is reachable: {listed:?}"
    );
    assert!(
        !listed.contains(&"load".to_string()),
        "and there is still no load tool: {listed:?}"
    );

    let full = s.call(
        "tools/call",
        serde_json::json!({"name": "dialect", "arguments": {}}),
    );
    let body: String = full["result"]["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect();
    assert!(
        body.contains("to_char") && body.contains("strftime"),
        "the reference actually contains the mapping table"
    );
    assert!(
        body.contains("ASOF") || body.contains("QUALIFY"),
        "and what DuckDB is worth reaching for"
    );

    // The load-bearing half: none of that bulk may ride along in what is sent every call.
    assert!(
        body.len() > 8 * text.len(),
        "the reference ({} bytes) should dwarf the always-sent instructions ({} bytes) — if it \
         does not, either the instructions have grown a primer or the reference is a stub",
        body.len(),
        text.len()
    );
    let payload: String = tools["result"]["tools"].to_string();
    assert!(
        !payload.contains("to_char"),
        "the tool list must not carry the mapping table either"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_dialect_reference_is_written_for_the_agent() {
    // The file the tool serves also tells its maintainers how it was measured and which tests
    // keep it true. None of that helps an agent, and all of it costs the agent bytes.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let full = s.call(
        "tools/call",
        serde_json::json!({"name": "dialect", "arguments": {}}),
    );
    let body: String = full["result"]["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect();
    for maintainer in [
        "REQ-",
        "src/",
        "tests/",
        "version_not_hardcoded",
        "Two audiences",
        "Regenerating",
        "<!--",
    ] {
        assert!(!body.contains(maintainer), "{maintainer:?} served:\n{body}");
    }
    assert!(body.contains("to_char") && body.contains("ASOF"), "{body}");
    // and the repository's copy still carries what the maintainers need
    let file = include_str!("../docs/DIALECT.md");
    assert!(file.contains("src/hint.rs") && file.contains("Regenerating"));
    let _ = std::fs::remove_dir_all(&dir);
}
