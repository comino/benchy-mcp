//! Properties of the shipped server, driven over MCP: REQ-1 isolation, REQ-8 survivable failure,
//! and REQ-10's addressable results.

mod common;
use common::{data_dir, Server};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn cross_session_canary() {
    // REQ-1. Two live sessions over the same data. A puts a canary in its own scratch; B looks
    // everywhere SQL can look and must not find it.
    let dir = data_dir();
    let mut a = Server::start(&dir, &[]);
    let mut b = Server::start(&dir, &[]);
    let (err, body, _) = a.query("CREATE TABLE secret AS SELECT 'CANARY-7c1e' AS v");
    assert!(!err, "{body}");
    let (err, body, _) = a.query("SELECT v FROM secret");
    assert!(
        !err && body.contains("CANARY-7c1e"),
        "A sees its own: {body}"
    );

    let (err, _, _) = b.query("SELECT v FROM secret");
    assert!(err, "B cannot name A's table");
    for sql in [
        "SELECT table_name FROM duckdb_tables()",
        "SELECT database_name, path FROM duckdb_databases()",
        "SELECT view_name FROM duckdb_views() WHERE NOT internal",
    ] {
        let (_, body, _) = b.query(sql);
        assert!(
            !body.contains("CANARY") && !body.contains("secret"),
            "{sql}: {body}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn schema_has_no_addressing() {
    // REQ-1. Nothing the agent can pass names a workspace, a path or a session: access is a
    // capability the connection holds, never a name the model can say.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let tools = s.call("tools/list", json!({}));
    fn keys(v: &Value, out: &mut Vec<String>) {
        if let Some(props) = v.get("properties").and_then(Value::as_object) {
            for (k, sub) in props {
                out.push(k.to_lowercase());
                keys(sub, out);
            }
        }
        if let Some(items) = v.get("items") {
            keys(items, out);
        }
    }
    let mut all = Vec::new();
    for t in tools["result"]["tools"].as_array().expect("tools") {
        keys(&t["inputSchema"], &mut all);
    }
    for k in &all {
        for bad in [
            "path",
            "file",
            "dir",
            "workspace",
            "session",
            "database",
            "db",
            "url",
            "uri",
            "host",
            "endpoint",
            "location",
            "source",
        ] {
            assert!(!k.contains(bad), "tool parameter `{k}` can name a {bad}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn planner_blowup_killed() {
    // REQ-8. The case nothing in-process can stop: a query that takes seconds just to *plan* and
    // never reaches the interrupt check. The server must kill the engine, rebuild the bench, and
    // say what was lost.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--deadline", "1", "--hard-grace", "1"]);
    let (err, body, _) = s.query("CREATE TABLE scratch AS SELECT 1 AS n");
    assert!(!err, "{body}");

    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let blowup = format!("WITH {} SELECT m FROM l18", ctes.join(", "));
    let (err, body, took) = s.query(&blowup);
    assert!(err, "the blowup must not succeed: {body}");
    assert!(
        body.contains("rebuilt"),
        "says the bench was rebuilt: {body}"
    );
    assert!(
        took < Duration::from_secs(30),
        "stopped in bounded time: {took:?}"
    );

    let (err, body, _) = s.query("SELECT count(*) FROM readings");
    assert!(
        !err && body.contains("500"),
        "the server survived and the data is back: {body}"
    );
    let (err, _, _) = s.query("SELECT n FROM scratch");
    assert!(err, "and the scratch really is gone, as the message said");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A query whose *planning* grows the engine far past DuckDB's own memory budget: nested `MAP`s,
/// eighteen deep. With `--memory 64MB` it took the engine to 229 MB over 9.6 s before DuckDB
/// refused it as out of memory (DuckDB 1.5.5, aarch64).
#[cfg(target_os = "linux")]
fn nested_maps(depth: usize) -> String {
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..depth {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    format!("WITH {} SELECT m FROM l{}", ctes.join(", "), depth - 1)
}

// Linux only: the memory ceiling is read from /proc, and elsewhere it is not enforced
// (`a_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
#[test]
#[cfg(target_os = "linux")]
fn an_engine_past_its_memory_ceiling_is_killed_even_while_planning() {
    // REQ-8. DuckDB's memory_limit bounds its buffer manager, not planning: in a container one
    // engine's growth is every session's OOM. The server watches the engine's resident memory
    // and kills it past the ceiling, whatever phase the query is in, long before the deadline.
    let dir = data_dir();
    let mut s = Server::start(
        &dir,
        &[
            "--deadline",
            "60",
            "--hard-grace",
            "60",
            "--memory",
            "64MB",
            "--memory-ceiling",
            "128MB",
        ],
    );
    let (err, body, _) = s.query("CREATE TABLE scratch AS SELECT 1 AS n");
    assert!(!err, "{body}");
    let (err, body, took) = s.query(&nested_maps(18));
    assert!(err, "the blowup must not succeed: {body}");
    assert!(
        body.contains("memory ceiling"),
        "says it was the memory ceiling, not DuckDB's budget or the deadline: {body}"
    );
    assert!(
        body.contains("rebuilt"),
        "says the bench was rebuilt: {body}"
    );
    assert!(
        body.contains("aggregate") && body.contains("--memory-ceiling"),
        "says how to use less, as running out of DuckDB's budget does: {body}"
    );
    assert!(
        took < Duration::from_secs(30),
        "stopped by the ceiling, not a deadline: {took:?}"
    );
    let (err, body, _) = s.query("SELECT count(*) FROM readings");
    assert!(!err && body.contains("500"), "the data is back: {body}");
    let (err, _, _) = s.query("SELECT n FROM scratch");
    assert!(err, "and the scratch really is gone, as the message said");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A process's resident memory, in bytes: VmRSS, which the server reads as pages from `statm`.
#[cfg(target_os = "linux")]
fn resident(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .expect("VmRSS");
    kb * 1024
}

/// There is no lower "idle" ceiling, because what an engine keeps between requests is its
/// tables, inside DuckDB's budget, and memory its allocator gives back by itself: a hash
/// aggregate at a 512 MB budget left the engine at 274 MB, and at 120 MB a second later.
#[test]
#[cfg(target_os = "linux")]
fn an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once() {
    // REQ-8. The ceiling was read only while a request ran, every 50 ms, so a request quicker
    // than that which left the engine past it was answered and the engine sat there idle, for
    // the next request to be killed for it. Each request here keeps 500,000 BIGINTs in a table
    // of its own, about 5 MB, in about 20 ms and inside DuckDB's budget; measured without the
    // check, the 27th left the engine idle at 194.3 MB against 192 MB, three runs of three.
    // After every answer the engine is under its ceiling, and the request that left it past
    // says so.
    let dir = data_dir();
    let ceiling = 192_000_000;
    let mut s = Server::start(&dir, &["--memory", "192MB", "--memory-ceiling", "192MB"]);
    let mut rebuilt = false;
    for i in 0..80 {
        let (err, body, _) = s.query(&format!(
            "CREATE TEMP TABLE big{i} AS SELECT i FROM range(500000) t(i)"
        ));
        if err {
            assert!(
                body.contains("memory ceiling") && body.contains("rebuilt"),
                "{i}: {body}"
            );
            rebuilt = true;
        }
        let held = resident(common::engine_of(s.pid()));
        assert!(
            held <= ceiling,
            "{i}: the engine sits at {held} after an answer, past its ceiling of {ceiling}"
        );
    }
    assert!(rebuilt, "the tables never reached the ceiling");
    assert!(s.alive());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_spoofed_pivot_enum_cannot_be_pinned() {
    // REQ-9. A host's review: a type the agent named like a dynamic PIVOT's passed as one, a
    // table whose INTEGER column's DEFAULT cast to it passed the ENUM-column check, and at the
    // end of the batch the type could not be dropped for the table, and that was swallowed. Three
    // batches of 1,500,000 values left an idle engine at 799 MB, under its ceiling, for good.
    // Here, smaller: whatever stops it, nothing of either stays, batch after batch.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    for i in 0..3 {
        let (err, body, _) = s.query(&format!(
            "CREATE TEMP TYPE \"__pivot_enum_{i}\" AS ENUM \
               (SELECT 'v' || i FROM range(100000) t(i)); \
             CREATE TEMP TABLE pin{i} \
               (c INTEGER DEFAULT length(('v1'::\"__pivot_enum_{i}\")::VARCHAR))"
        ));
        assert!(err, "{i}: the pin was kept: {body}");
        assert!(body.contains("ENUM"), "{i}: says why: {body}");
        let (err, body, _) = s.query(
            "SELECT 'kept:' || (SELECT count(*) FROM system.main.duckdb_types() \
               WHERE NOT internal AND database_name IN ('temp', 'memory')) \
             || ',' || (SELECT count(*) FROM system.main.duckdb_tables() \
               WHERE table_name LIKE 'pin%') AS k",
        );
        assert!(!err && body.contains("kept:0,0"), "{i}: {body}");
    }
    let (err, body, _) = s.query("PIVOT readings ON meter USING sum(power_kw)");
    assert!(
        !err && body.contains("m6"),
        "a dynamic PIVOT still works: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_memory_ceiling_the_engine_cannot_live_under_is_refused_at_startup() {
    // Below DuckDB's own budget, every query DuckDB would refuse cleanly as out of memory would
    // cost the agent its scratch instead; and a size that does not parse is not guessed at.
    let dir = data_dir();
    for (args, names) in [
        (
            vec!["--memory", "1GB", "--memory-ceiling", "512MB"],
            "--memory-ceiling",
        ),
        (vec!["--memory-ceiling", "lots"], "--memory-ceiling"),
        (vec!["--memory", "lots"], "--memory"),
    ] {
        for host in [false, true] {
            let mut cmd = std::process::Command::new(common::BIN);
            if host {
                cmd.arg("host");
            } else {
                cmd.arg(&dir);
            }
            let out = cmd
                .args(&args)
                .args(common::PLATFORM)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success(), "{args:?} started");
            assert!(err.contains(names), "{args:?}: {err}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_result_can_be_fetched_again_by_id() {
    // REQ-10. A result is addressable after the fact, so an agent can go back to it without
    // re-running the query.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let (err, body, _) = s.query("SELECT count(*) AS c FROM readings");
    assert!(!err, "{body}");
    let id = body
        .split("result_id ")
        .nth(1)
        .and_then(|r| r.split_whitespace().next())
        .map(|x| x.trim_end_matches(['.', ',']).to_string())
        .expect("a result id");
    let again = s.call(
        "tools/call",
        json!({"name": "get_result", "arguments": {"result_id": id}}),
    );
    let text: String = again["result"]["content"]
        .as_array()
        .expect("content")
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect();
    assert!(text.contains("500"), "the same answer comes back: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every result id in a reply's text, in order.
fn result_ids(body: &str) -> Vec<String> {
    body.split("result_id ")
        .skip(1)
        .filter_map(|r| r.split_whitespace().next())
        .map(|x| x.trim_end_matches(['.', ',']).to_string())
        .collect()
}

/// The prefix is 32 bits (`3fa9c1b2-7`), so a stale id resolves in another session once in
/// four billion tries.
#[test]
fn two_sessions_never_issue_the_same_result_id() {
    // REQ-10. Ids never repeat within a session, and every session numbered from `r1`: a host
    // whose conversations outlive sessions could hand a new session an old id and get a
    // different answer. Each session's ids carry a random prefix of its own, so one session's
    // never resolve in another, and still name nothing another session could use (REQ-1).
    let dir = data_dir();
    let mut sessions = [Server::start(&dir, &[]), Server::start(&dir, &[])];
    let mut issued: Vec<Vec<String>> = Vec::new();
    for s in &mut sessions {
        let mut ids = Vec::new();
        for sql in ["SELECT 1 AS a", "SELECT 2 AS b; SELECT 3 AS c"] {
            let (err, body, _) = s.query(sql);
            assert!(!err, "{body}");
            ids.extend(result_ids(&body));
        }
        assert_eq!(ids.len(), 3, "{ids:?}");
        let prefix = |id: &str| id.rsplit_once('-').map(|(p, _)| p.to_string());
        assert!(
            ids.iter()
                .all(|id| prefix(id).is_some() && prefix(id) == prefix(&ids[0])),
            "one session, one prefix: {ids:?}"
        );
        issued.push(ids);
    }
    for id in &issued[0] {
        assert!(!issued[1].contains(id), "{id} was issued twice: {issued:?}");
    }
    // An id from the other session does not resolve here, where `r1` would have.
    let v = sessions[1].call(
        "tools/call",
        json!({"name": "get_result", "arguments": {"result_id": issued[0][0]}}),
    );
    assert_eq!(v["result"]["isError"], true, "{v}");
    let v = sessions[1].call(
        "tools/call",
        json!({"name": "get_result", "arguments": {"result_id": issued[1][0]}}),
    );
    assert_ne!(v["result"]["isError"], true, "its own does: {v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_load_that_keeps_failing_is_not_retried_forever() {
    // REQ-8. A load that never finishes used to count as a lost engine, and losing an engine
    // meant rebuilding it — straight into the same load, without end. It must fail once, say
    // why, and let the process exit.
    let dir = common::unique("stuck");
    std::fs::create_dir_all(&dir).unwrap();
    let connector = dir.join("connector");
    std::fs::write(&connector, "#!/bin/sh\nsleep 60\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&connector, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "[[source]]\nname = \"stuck\"\ntype = \"exec\"\ncommand = \"{}\"\ndeadline_secs = 120\n\
             [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n",
            connector.display()
        ),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let mut child = std::process::Command::new(common::BIN)
        .args(common::PLATFORM)
        .args(["--config", cfg.to_str().unwrap(), "--load-timeout", "2"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(40) {
            let _ = child.kill();
            panic!("benchy was still retrying a load that cannot succeed after 40 s");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut err = String::new();
    use std::io::Read;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(!status.success(), "a failed load is a failed start");
    assert!(err.contains("did not finish within 2s"), "says why: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_server_answers_while_a_query_runs() {
    // REQ-8. A query can take its whole deadline. The server must keep answering meanwhile — a
    // host's ping, another client's request — rather than stall behind it.
    let dir = data_dir();
    let mut s = Server::start(&dir, &["--deadline", "4", "--hard-grace", "4"]);
    s.send(&json!({"jsonrpc": "2.0", "id": 900, "method": "tools/call", "params": {
        "name": "query",
        "arguments": {"sql": "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t) SELECT sum(n) FROM t"}
    }}));
    std::thread::sleep(Duration::from_millis(300));
    let asked = std::time::Instant::now();
    s.send(&json!({"jsonrpc": "2.0", "id": 901, "method": "ping"}));
    let first = s.next();
    assert_eq!(first["id"], 901, "the ping was answered first: {first}");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "and promptly: {:?}",
        asked.elapsed()
    );
    let second = s.next();
    assert_eq!(
        second["id"], 900,
        "then the query, at its deadline: {second}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_rebuild_reads_nothing_again() {
    // A rebuilt bench must be the bench the session started with. Re-reading the config or a
    // source for a rebuild would let an edit made mid-session silently change what the agent is
    // looking at — so a rebuild attaches what the session stored, and reads neither.
    let dir = common::unique("read-once");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.csv"), "n\n1\n2\n3\n").unwrap();
    std::fs::write(dir.join("b.csv"), "n\n1\n2\n3\n4\n5\n").unwrap();
    let cfg = dir.join("benchy.toml");
    let source =
        |file: &str| format!("[[source]]\nname = \"t\"\ntype = \"file\"\npath = \"{file}\"\n");
    std::fs::write(&cfg, source("a.csv")).unwrap();

    let mut s = Server::launch(
        &[
            "--config",
            cfg.to_str().unwrap(),
            "--deadline",
            "1",
            "--hard-grace",
            "1",
        ],
        &[],
    );
    let (_, body, _) = s.query("SELECT count(*) AS n FROM t");
    assert!(body.lines().any(|l| l.trim() == "3"), "{body}");

    std::fs::write(&cfg, source("b.csv")).unwrap();
    std::fs::write(dir.join("a.csv"), "n\n1\n2\n3\n4\n5\n6\n7\n").unwrap();
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let (err, body, _) = s.query(&format!("WITH {} SELECT m FROM l18", ctes.join(", ")));
    assert!(
        err && body.contains("rebuilt"),
        "the engine was rebuilt: {body}"
    );

    let (_, body, _) = s.query("SELECT count(*) AS n FROM t");
    assert!(
        body.lines().any(|l| l.trim() == "3"),
        "the rebuilt bench is the one the session started with: {body}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Linux only: it finds and kills engines through /proc.
#[test]
#[cfg(target_os = "linux")]
fn an_engine_that_dies_at_every_attach_is_not_rebuilt_forever() {
    // A rebuild re-attaches the stored tables. An engine lost during one of those attaches was
    // recovered by rebuilding again, from inside the rebuild — and the count of failed rebuilds
    // only rose when a rebuild returned, which that one never did. A stored file DuckDB blocks on
    // opening, and a watcher that kills each engine caught there, make every attach lose its
    // engine.
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = common::unique("rebuild-loop");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(dir.join("t.csv"), "n\n1\n2\n").unwrap();
    let mut s = Server::launch(
        &[
            dir.to_str().unwrap(),
            "--deadline",
            "1",
            "--hard-grace",
            "1",
        ],
        &[("TMPDIR", &tmp)],
    );
    let sessions: Vec<std::path::PathBuf> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    for session in &sessions {
        for stored in std::fs::read_dir(session.join("store")).unwrap().flatten() {
            std::fs::remove_file(stored.path()).unwrap();
            let c = std::ffi::CString::new(stored.path().to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        }
    }
    let server = s.pid();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut killed = std::collections::BTreeSet::new();
            let started = std::time::Instant::now();
            while !stop.load(Ordering::SeqCst) {
                // Bounded: past this, the server is taken down and the query below fails.
                if started.elapsed() > Duration::from_secs(30) {
                    unsafe { libc::kill(server as i32, libc::SIGKILL) };
                    break;
                }
                let blocked = sessions.iter().any(|session| {
                    std::fs::read_dir(session)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .any(|ws| {
                            ws.file_name().to_string_lossy().starts_with("ws-")
                                && std::fs::read_dir(ws.path())
                                    .into_iter()
                                    .flatten()
                                    .flatten()
                                    .any(|f| {
                                        use std::os::unix::fs::FileTypeExt;
                                        f.file_type().is_ok_and(|t| t.is_fifo())
                                    })
                        })
                });
                if blocked {
                    for pid in engines_of(server) {
                        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        killed.insert(pid);
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            killed.len()
        })
    };
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let started = std::time::Instant::now();
    let (err, body, _) = s.query(&format!("WITH {} SELECT m FROM l18", ctes.join(", ")));
    assert!(err, "{body}");
    assert!(body.contains("Rebuilding the bench failed"), "{body}");
    // The next request tries once more, and the one after that is told it is over.
    let (err, body, _) = s.query("SELECT 1");
    assert!(err && body.contains("not available"), "{body}");
    let (err, body, _) = s.query("SELECT 1");
    assert!(err && body.contains("is not tried again"), "{body}");
    stop.store(true, Ordering::SeqCst);
    let killed = watcher.join().unwrap();
    assert_eq!(killed, 2, "one engine lost at each of the two rebuilds");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a lost engine is noticed at once, not at the attach's timeout"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The server's engines: its children started with `--engine`.
#[cfg(target_os = "linux")]
fn engines_of(server: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let parent = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')?
                        .1
                        .split_whitespace()
                        .nth(1)?
                        .parse()
                        .ok()
                });
            parent == Some(server)
                && std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|c| c.split(|b| *b == 0).any(|a| a == b"--engine"))
        })
        .collect()
}

#[test]
fn a_failed_rebuild_tells_the_agent_no_path() {
    // REQ-1: the model never sees a path. A rebuild that fails passed its error through, and with
    // it the operator's layout. A rebuild re-attaches what the session stored, so it fails when a
    // stored file is no database — and DuckDB's error for that names the file.
    let dir = common::unique("rebuild-path");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(dir.join("t.csv"), "n\n1\n2\n").unwrap();
    let mut s = Server::launch(
        &[
            dir.to_str().unwrap(),
            "--deadline",
            "1",
            "--hard-grace",
            "1",
        ],
        &[("TMPDIR", &tmp)],
    );
    for session in std::fs::read_dir(&tmp).unwrap().flatten() {
        for stored in std::fs::read_dir(session.path().join("store"))
            .unwrap()
            .flatten()
        {
            // A new file, not the old one overwritten: the running engine reads that one.
            std::fs::remove_file(stored.path()).unwrap();
            std::fs::write(stored.path(), "not a database").unwrap();
        }
    }
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..19 {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    let (err, body, _) = s.query(&format!("WITH {} SELECT m FROM l18", ctes.join(", ")));
    assert!(err, "{body}");
    assert!(
        body.contains("could not be rebuilt") || body.contains("failed"),
        "says the rebuild failed: {body}"
    );
    let name = dir.file_name().unwrap().to_str().unwrap();
    assert!(!body.contains(name), "a path reached the agent: {body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_deadline_of_zero_is_refused_at_startup() {
    // Every query would fail at once, and the caps a host reads from `hello` could not be handed
    // back: a deadline is at least one second.
    let dir = data_dir();
    for args in [
        vec![dir.to_str().unwrap(), "--deadline", "0"],
        vec!["host", "--deadline", "0"],
    ] {
        let out = std::process::Command::new(common::BIN)
            .args(&args)
            .args(common::PLATFORM)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} started");
        assert!(err.contains("--deadline"), "{args:?}: {err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_batch_returns_what_ran_before_it() {
    // The CREATE and the SELECT ran and were kept, and the reply said only that statement 3
    // failed: the agent learned nothing of the answer it had paid for, and a get_result for it
    // found nothing.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let (err, body, _) =
        s.query("CREATE TABLE t AS SELECT 1 AS a; SELECT 42 AS answer; SELECT nope FROM t");
    assert!(err, "the batch failed: {body}");
    assert!(body.starts_with("statement 3: "), "{body}");
    assert!(
        body.contains("Statements 1–2 ran before it, and what they did is kept"),
        "{body}"
    );
    assert!(body.contains("answer\n42\n"), "{body}");
    let id = body
        .split("answer\n42\n")
        .nth(1)
        .and_then(|rest| rest.split("result_id ").nth(1))
        .map(|t| t.split_whitespace().next().unwrap().to_string())
        .expect("the result has an id");
    let page = s.call(
        "tools/call",
        serde_json::json!({"name": "get_result", "arguments": {"result_id": id}}),
    );
    let text = page["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("answer\n42"), "{text}");
    let (err, body, _) = s.query("SELECT a FROM t");
    assert!(!err && body.contains('1'), "{body}");

    // Within the byte cap, the error first: a large result before it cannot push it out.
    let mut s = Server::start(&dir, &["--max-bytes", "1024"]);
    let (err, body, _) =
        s.query("SELECT repeat('x', 100) AS v FROM range(40); SELECT nope FROM readings");
    assert!(err);
    assert!(body.starts_with("statement 2: "), "{body}");
    assert!(body.contains("Statement 1 ran before it"), "{body}");
    assert!(body.contains("result_id "), "{body}");
    assert!(
        benchy_mcp::result::wire_len(&body) <= 1024,
        "{} bytes",
        benchy_mcp::result::wire_len(&body)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_client_without_initialize_can_list_the_tools() {
    // MCP 2026-07-28 has no `initialize`: a client probes with `server/discover` and stamps each
    // request with its version in `_meta`. There a list result must say how long it stays fresh
    // and who may cache it. Without `ttlMs` and `cacheScope`, Claude Code 2.1, which talks this
    // way, connected and then refused the tools: "Invalid result for tools/list".
    use std::io::{BufRead, BufReader, Write};
    let meta = json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                      "io.modelcontextprotocol/clientInfo": {"name": "stateless", "version": "0"},
                      "io.modelcontextprotocol/clientCapabilities": {}});
    let mut child = std::process::Command::new(common::BIN)
        .arg(data_dir())
        .args(common::PLATFORM)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut ask = |id: u64, method: &str| -> Value {
        let m = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {"_meta": meta}});
        writeln!(stdin, "{m}").unwrap();
        stdin.flush().unwrap();
        loop {
            let mut line = String::new();
            assert!(
                out.read_line(&mut line).unwrap() > 0,
                "closed during {method}"
            );
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v["id"] == id {
                    return v;
                }
            }
        }
    };
    let found = ask(1, "server/discover");
    assert!(
        found["result"]["supportedVersions"]
            .as_array()
            .is_some_and(|v| v.contains(&json!("2026-07-28"))),
        "{found}"
    );
    let listed = ask(2, "tools/list");
    let r = &listed["result"];
    assert_eq!(r["tools"].as_array().map(Vec::len), Some(4), "{listed}");
    assert!(r["ttlMs"].is_u64(), "ttlMs is a whole number: {listed}");
    assert!(
        r["cacheScope"] == "public" || r["cacheScope"] == "private",
        "cacheScope: {listed}"
    );
    let _ = child.kill();
    let _ = child.wait();
}
