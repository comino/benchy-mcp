//! REQ-3: the process that runs agent SQL is confined by Landlock — all of it.
//!
//! Landlock confines the thread that asks and the threads it starts afterwards, never a thread
//! that is already running. So one thread left running at the seal would be an unconfined thread
//! inside the process that serves SQL, and code that reached it would not be sandboxed at all.
//! The engine loads nothing and runs no connector, so a connector's threads live and die in its
//! loader; and the engine, like every file loader, checks that it is alone before it confines
//! itself.
#![cfg(target_os = "linux")]

mod common;
use common::{unique, BIN};
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn a_thread_already_running_is_never_confined_as_if_it_were_not() {
    // With another thread running, the check refuses — and says why, since an operator will see it.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let other = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    let err = benchy_mcp::sandbox::alone(Duration::from_millis(100))
        .expect_err("another thread is running");
    assert!(
        err.contains("threads were running") && err.contains("started after it"),
        "{err}"
    );
    drop(tx);
    other.join().unwrap();
}

#[test]
fn a_connector_that_never_lets_go_cannot_stall_or_unconfine_the_engine() {
    // A connector that leaves a process holding its stdin, and a document larger than a pipe
    // holds: the thread writing the document can never finish. When connectors ran inside the
    // engine, that thread was still running at the seal. Now it lives and dies in its loader.
    let dir = unique("confine");
    std::fs::create_dir_all(&dir).unwrap();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/connectors");
    std::fs::copy(examples.join("hello.arrows"), dir.join("hello.arrows")).unwrap();
    let holder = dir.join("holder.pid");
    let connector = dir.join("holds-stdin.sh");
    std::fs::write(
        &connector,
        format!(
            "#!/bin/sh\n[ \"$1\" = fetch ] || exit 1\nexec 3<&0\n\
             setsid sleep 30 <&3 >/dev/null 2>&1 &\necho $! > {}\n\
             cat \"$(dirname \"$0\")/hello.arrows\"\n",
            holder.display()
        ),
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
            "[[source]]\nname = \"readings\"\ntype = \"exec\"\ncommand = \"holds-stdin.sh\"\n\
             [source.config]\npad = \"{}\"\n\
             [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n\
             [[source.schema]]\nname = \"meter\"\ntype = \"string\"\n\
             [[source.schema]]\nname = \"power_kw\"\ntype = \"float64\"\n",
            "x".repeat(200_000)
        ),
    )
    .unwrap();

    let started = Instant::now();
    let out = std::process::Command::new(BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    if let Ok(pid) = std::fs::read_to_string(&holder) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("readings (4 rows)"), "it loads: {err}");
    assert!(
        err.contains("Landlock fully enforced"),
        "the engine is confined: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "and does not wait for the stuck writer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run `f` on a thread whose Landlock stack is full. Landlock allows sixteen stacked layers, so
/// a seventeenth cannot be added — and whatever the thread starts inherits the full stack. The
/// layers restrict only making block devices, which nothing here does.
fn with_landlock_exhausted<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    use landlock::{AccessFs, Ruleset, RulesetAttr};
    std::thread::scope(|s| {
        s.spawn(|| {
            for _ in 0..16 {
                Ruleset::default()
                    .handle_access(AccessFs::MakeBlock)
                    .unwrap()
                    .create()
                    .unwrap()
                    .restrict_self()
                    .unwrap();
            }
            f()
        })
        .join()
        .unwrap()
    })
}

#[test]
fn a_missing_sandbox_is_refused_without_the_flag() {
    let dir = common::data_dir();
    let run = |extra: &[&str]| {
        with_landlock_exhausted(|| {
            std::process::Command::new(BIN)
                .arg(&dir)
                .args(extra)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
        })
    };

    let out = run(&[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must not serve unconfined: {err}");
    assert!(
        err.contains("Landlock could not confine") && err.contains("--allow-unsandboxed"),
        "says what is missing and how to go on anyway: {err}"
    );

    // With the flag it serves, and says what it is doing.
    let out = run(&["--allow-unsandboxed"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("without an OS sandbox"), "and is told: {err}");
    assert!(!err.contains("Landlock could not confine"), "{err}");
    let mut s = with_landlock_exhausted(|| {
        common::Server::launch(&[dir.to_str().unwrap(), "--allow-unsandboxed"], &[])
    });
    assert!(
        s.alive(),
        "the operator chose to serve unconfined, and it does"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run one loader job to completion and return its report.
fn run_loader(job: &benchy_mcp::loader::Job) -> benchy_mcp::loader::Done {
    use std::process::{Command, Stdio};
    let mut child = Command::new(BIN)
        .arg("--loader")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    benchy_mcp::proto::write_frame(child.stdin.as_mut().unwrap(), job).unwrap();
    let done = benchy_mcp::proto::read_frame(child.stdout.as_mut().unwrap()).unwrap();
    let _ = child.wait();
    done
}

#[test]
fn a_loader_that_cannot_be_confined_refuses_without_the_flag() {
    // A file loader parses whatever the file holds, and a parser subverted by its input is
    // contained only by the restriction. Where that cannot be applied, the loader refuses — the
    // engine's own refusal comes first in a server, so this asks a loader directly.
    let dir = unique("loader-confine");
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("t.csv");
    std::fs::write(&input, "n\n1\n2\n").unwrap();
    let job = |stage: &str, allow_unsandboxed: bool| benchy_mcp::loader::Job::File {
        name: "t".into(),
        path: input.clone(),
        stage: dir.join(stage),
        allow_unsandboxed,
        shape: Default::default(),
        timezone: None,
        declared: Default::default(),
    };

    let done = with_landlock_exhausted(|| run_loader(&job("refused", false)));
    let err = done.error.unwrap_or_default();
    assert!(!done.ok, "loaded unconfined");
    assert!(
        err.contains("Landlock could not confine the loader")
            && err.contains("--allow-unsandboxed"),
        "says what is missing and how to go on anyway: {err}"
    );

    let done = with_landlock_exhausted(|| run_loader(&job("allowed", true)));
    assert!(
        done.ok,
        "the operator chose to load unconfined: {:?}",
        done.error
    );
    // And where it can be applied, it is, and the load goes ahead.
    let done = run_loader(&job("confined", false));
    assert!(done.ok, "{:?}", done.error);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`: the ABI this kernel
/// speaks, or 0 where it has none.
fn kernel_landlock_abi() -> u32 {
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    u32::try_from(v).unwrap_or(0)
}

#[test]
fn doctor_probes_the_restriction_the_server_applies() {
    // The probe used to apply a stricter deny-everything ruleset than the engine does, so it
    // could pass on a machine where the engine's own restriction did something else.
    let out = std::process::Command::new(BIN)
        .args(["doctor", "--probe"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    // Landlock refuses TCP from ABI 4 (Linux 6.7); before that it is not Landlock's to refuse,
    // the filesystem is confined all the same, and the doctor says which.
    // The ABI is asked of the kernel here, not through `sandbox::landlock_abi`: the doctor's
    // answer depends on that function, so an oracle sharing it would agree with whatever it said.
    // Where it can refuse, the refusal must count: "no" alone also matches a probe the doctor
    // set aside as not Landlock's to refuse, which is what it did when it misread the ABI.
    let tcp = if kernel_landlock_abi() >= 4 {
        "open a TCP connection          : no  (must be no)"
    } else {
        "not Landlock's to refuse here"
    };
    // truncate(2) is Landlock's to refuse from ABI 3 (Linux 6.2).
    let truncate = |probe: &str, must: &str| {
        if kernel_landlock_abi() >= 3 {
            format!("{probe:<31}: {must:<3} (must be {must})")
        } else {
            format!("{probe:<31}: ")
        }
    };
    let (engine, loader) = text
        .split_once("a file loader's own restriction")
        .unwrap_or_else(|| panic!("no loader probe:\n{text}"));
    for line in [
        "the engine's own restriction",
        "read in its workspace          : yes",
        "read outside it                : no",
        "create a file in its workspace : no",
        "read the store's contents      : no",
        "delete in its session          : yes",
        &truncate("truncate in its workspace", "no"),
        &truncate("truncate a file outside it", "no"),
        tcp,
    ] {
        assert!(engine.contains(line), "engine: missing `{line}`:\n{text}");
    }
    for line in [
        "read its input                 : yes",
        "read anything else             : no",
        "write in its staging directory : yes",
        "write anywhere else            : no",
        "read the store's contents      : no",
        &truncate("truncate its staged files", "yes"),
        &truncate("truncate its input", "no"),
        &truncate("truncate anything else", "no"),
        tcp,
        "VERIFIED",
    ] {
        assert!(loader.contains(line), "loader: missing `{line}`:\n{text}");
    }

    // And it measures: where the restriction cannot be applied, it says so and fails.
    let out = with_landlock_exhausted(|| {
        std::process::Command::new(BIN)
            .args(["doctor", "--probe"])
            .output()
            .unwrap()
    });
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("NOT ENFORCED"), "{text}");
}

#[test]
fn doctor_says_in_json_what_each_mechanism_enforces() {
    // A host checks a container at start by reading this, not the prose: an overall verdict,
    // the exit status agreeing with it, and each mechanism with every check it made.
    let run = || {
        std::process::Command::new(BIN)
            .args(["doctor", "--probe", "--json"])
            .output()
            .unwrap()
    };
    let out = run();
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON alone ({e}):\n{text}"));
    assert!(out.status.success(), "{text}");
    assert_eq!(v["verdict"], "enforced", "{v}");
    assert!(v["reason"].is_null(), "{v}");
    assert!(
        v["engine"]
            .as_str()
            .is_some_and(|e| e.starts_with("DuckDB v")),
        "{v}"
    );
    assert_eq!(v["landlock_abi"], kernel_landlock_abi(), "{v}");
    let mechanisms = v["mechanisms"].as_array().expect("mechanisms");
    let names: Vec<&str> = mechanisms
        .iter()
        .filter_map(|m| m["name"].as_str())
        .collect();
    assert_eq!(names, ["engine", "file_loader", "network_loader"], "{v}");
    for m in mechanisms {
        assert_eq!(m["enforced"], true, "{m}");
        assert!(m["posture"].is_string(), "{m}");
        let checks = m["checks"].as_array().expect("checks");
        assert!(checks.len() >= 8, "{m}");
        for c in checks {
            assert!(c["check"].is_string() && c["allowed"].is_boolean(), "{c}");
            // `must_be` is null where the right is not Landlock's to withhold on this kernel.
            if let Some(must) = c["must_be"].as_bool() {
                assert_eq!(c["allowed"], must, "{c}");
            }
        }
    }
    let outside = mechanisms[0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "read outside it")
        .expect("the engine's read outside its workspace");
    assert_eq!(outside["allowed"], false, "{outside}");

    // Where the restriction cannot be applied: still JSON, saying so, and a failing exit.
    let out = with_landlock_exhausted(run);
    let text = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON alone ({e}):\n{text}"));
    assert!(!out.status.success(), "{text}");
    assert_eq!(v["verdict"], "not_enforced", "{v}");
    assert!(v["reason"].is_string(), "{v}");
    assert!(
        v["mechanisms"]
            .as_array()
            .is_some_and(|m| m.iter().any(|m| m["enforced"] == false)),
        "{v}"
    );
}

#[test]
fn a_network_loader_reaches_only_its_source_and_its_staging() {
    // A built-in source's loader runs only benchy's code, but it parses what a remote server
    // sends, so it is confined as a file loader is: before the first byte of an answer is read,
    // and refusing where it cannot be, unless the operator said so. Its server here is reached by
    // name, `localhost`, so the load goes through the resolver's files too. What the restriction
    // grants and withholds, right by right, is `doctor --probe`'s network-loader section, checked
    // at the end.
    use arrow::array::{ArrayRef, Int64Array};
    use common::http::{FakeHttp, Reply};
    use std::sync::Arc;
    let batch = arrow::array::RecordBatch::try_from_iter([(
        "n",
        Arc::new(Int64Array::from(vec![1i64, 2, 3])) as ArrayRef,
    )])
    .unwrap();
    let mut rows = Vec::new();
    {
        let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut rows, &batch.schema()).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    let ch = FakeHttp::start(move |req| {
        let sql = String::from_utf8_lossy(&req.body);
        if sql.starts_with("DESCRIBE") {
            // Long enough for the test to look at the loader while it waits for its first answer.
            Reply::ok("{\"name\":\"n\",\"type\":\"Int64\",\"default_type\":\"\"}\n")
                .delay(Duration::from_millis(500))
        } else if sql.contains("count()") {
            Reply::ok("3\n")
        } else {
            Reply::ok(rows.clone())
        }
    });
    let port = ch.url.rsplit(':').next().unwrap().to_string();
    let cfg = format!(
        "[[source]]\ntype = \"clickhouse\"\nurl = \"http://localhost:{port}\"\ntables = [\"t\"]\n\
         compression = \"none\"\n"
    );
    let sources = benchy_mcp::config::parse(&cfg, Path::new("/nonexistent/benchy.toml")).unwrap();
    let source = serde_json::to_value(&sources.builtin[0]).unwrap();
    let dir = unique("net-loader");
    std::fs::create_dir_all(&dir).unwrap();
    // Written as the server writes it, field by field, so this test speaks the job's wire form.
    let job = |stage: &str, allow_unsandboxed: bool| {
        serde_json::json!({
            "kind": "built_in",
            "source": source,
            "budget_rows": 1_000_000,
            "budget_bytes": 1u64 << 30,
            "stage": dir.join(stage),
            "allow_unsandboxed": allow_unsandboxed,
        })
    };
    let run = |job: &serde_json::Value| -> (benchy_mcp::loader::Done, bool) {
        use std::process::{Command, Stdio};
        let asked = ch.requests().len();
        let mut child = Command::new(BIN)
            .arg("--loader")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        benchy_mcp::proto::write_frame(child.stdin.as_mut().unwrap(), job).unwrap();
        // While its first question is unanswered, the loader has read nothing from the server:
        // it must already be confined. Landlock leaves no mark in /proc but the
        // no-new-privileges bit it requires, which nothing else in a loader sets.
        let t = Instant::now();
        while ch.requests().len() == asked && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap_or_default();
        let sealed = status.contains("NoNewPrivs:\t1");
        let done = benchy_mcp::proto::read_frame(child.stdout.as_mut().unwrap()).unwrap();
        let _ = child.wait();
        (done, sealed)
    };

    // Confined, it loads: its source's port, its staging directory and the resolver are enough.
    let (done, sealed) = run(&job("confined", false));
    assert!(done.ok, "{:?}", done.error);
    assert_eq!(done.fetched_rows, 3);
    assert!(
        dir.join("confined").join(benchy_mcp::Bench::FILE).is_file(),
        "it wrote its table"
    );
    assert!(sealed, "it was confined before its first answer arrived");

    // Where it cannot be confined, it refuses, and says how to go on anyway.
    let (done, _) = with_landlock_exhausted(|| run(&job("refused", false)));
    let err = done.error.unwrap_or_default();
    assert!(!done.ok, "loaded unconfined");
    assert!(
        err.contains("Landlock could not confine the loader")
            && err.contains("--allow-unsandboxed"),
        "says what is missing and how to go on anyway: {err}"
    );
    let (done, _) = with_landlock_exhausted(|| run(&job("allowed", true)));
    assert!(
        done.ok,
        "the operator chose to load unconfined: {:?}",
        done.error
    );

    // Right by right, measured by the doctor on this kernel. The TCP rights are Landlock's to
    // withhold from ABI 4 (Linux 6.7); before that the doctor says so instead.
    let out = std::process::Command::new(BIN)
        .args(["doctor", "--probe"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    let (_, net) = text
        .split_once("a network loader's own restriction")
        .unwrap_or_else(|| panic!("no network-loader probe:\n{text}"));
    // Each right counts from the ABI that can withhold it: truncate from 3, TCP from 4.
    let tcp_or_truncate = |abi: u32, probe: &str, must: &str| {
        if kernel_landlock_abi() >= abi {
            format!("{probe:<31}: {must:<3} (must be {must})")
        } else {
            format!("{probe:<31}: ")
        }
    };
    let tcp = |probe: &str, must: &str| tcp_or_truncate(4, probe, must);
    for line in [
        "read the resolver's files      : yes (must be yes)".to_string(),
        "resolve a name                 : yes (must be yes)".to_string(),
        "read anything else             : no  (must be no)".to_string(),
        "write in its staging directory : yes (must be yes)".to_string(),
        "write anywhere else            : no  (must be no)".to_string(),
        "read the store's contents      : no  (must be no)".to_string(),
        tcp_or_truncate(3, "truncate its staged files", "yes"),
        tcp_or_truncate(3, "truncate anything else", "no"),
        tcp("connect to its source's port", "yes"),
        tcp("connect to another port", "no"),
        tcp("listen on a port", "no"),
        "VERIFIED".to_string(),
    ] {
        assert!(
            net.contains(&line),
            "network loader: missing `{line}`:\n{text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_confined_process_cannot_truncate_what_it_was_not_granted() {
    // Truncating by path, truncate(2), is Landlock's to refuse only from ABI 3 (Linux 6.2), and
    // only where the ruleset handles it. Unhandled, a compromised engine or loader could empty
    // any file this user can write, granted or not. Each restriction is applied here to a thread
    // of its own, which Landlock confines alone, as `with_landlock_exhausted` does.
    if kernel_landlock_abi() < 3 {
        eprintln!("skipped: this kernel's Landlock cannot refuse truncate(2)");
        return;
    }
    use benchy_mcp::sandbox;
    let dir = unique("truncate");
    let (ws, stage) = (dir.join("ws"), dir.join("stage"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&stage).unwrap();
    let (outside, input) = (dir.join("outside"), dir.join("input.csv"));
    let (held, built) = (ws.join("held"), stage.join("bench"));
    let truncates = |p: &Path| -> bool {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        unsafe { libc::truncate(c.as_ptr(), 0) == 0 }
    };
    // What a thread confined by `restrict` could truncate: a file outside every grant, the
    // engine's workspace, a loader's input, and the staging directory.
    let confined = |restrict: &(dyn Fn() -> sandbox::Posture + Sync)| {
        for f in [&outside, &input, &held, &built] {
            std::fs::write(f, "kept").unwrap();
        }
        std::thread::scope(|s| {
            s.spawn(|| {
                assert!(restrict().is_enforced());
                [&outside, &held, &input, &built].map(|f| truncates(f))
            })
            .join()
            .unwrap()
        })
    };

    let engine = confined(&|| sandbox::restrict_to(&ws, &dir));
    let loader = confined(&|| sandbox::restrict_loader(&input, &stage));
    let network = confined(&|| sandbox::restrict_network_loader(&stage, &[]));
    assert_eq!(
        engine,
        [false, false, false, false],
        "the engine truncates nothing: outside, its workspace, an input, a staging directory"
    );
    for (who, got) in [("file loader", loader), ("network loader", network)] {
        assert_eq!(
            got,
            [false, false, false, true],
            "a {who} truncates only in its staging directory: outside, the workspace, an \
             input, its staging directory"
        );
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "kept");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_http_loader_is_confined_before_its_first_request() {
    // REQ-17 under REQ-3: an HTTP source's loader is the network loader too. Confined before its
    // first answer, it still loads: a CSV body through its spool in its staging directory, and
    // a JSON page through the first page DuckDB sniffs there. Its server is reached by name.
    use common::http::{FakeHttp, Reply};
    let api = FakeHttp::start(|req| {
        let r = match req.path() {
            "/readings.csv" => Reply::ok("n,kw\n1,1.5\n2,2.5\n3,3.5\n"),
            _ => Reply::ok("{\"data\": [{\"n\": 1}, {\"n\": 2}, {\"n\": 3}]}"),
        };
        // Long enough for the test to look at the loader while it waits for its first answer.
        r.delay(Duration::from_millis(500))
    });
    let port = api.url.rsplit(':').next().unwrap().to_string();
    let dir = unique("http-loader");
    std::fs::create_dir_all(&dir).unwrap();
    for (stage, rest) in [
        (
            "csv",
            "url = \"http://localhost:PORT/readings.csv\"\nformat = \"csv\"\n",
        ),
        (
            "json",
            "url = \"http://localhost:PORT/api\"\nformat = \"json\"\n[source.pages]\n\
             rows = \"data\"\n",
        ),
    ] {
        let cfg = format!(
            "[[source]]\nname = \"readings\"\ntype = \"http\"\n{}",
            rest.replace("PORT", &port)
        );
        let sources =
            benchy_mcp::config::parse(&cfg, Path::new("/nonexistent/benchy.toml")).unwrap();
        let job = serde_json::json!({
            "kind": "built_in",
            "source": serde_json::to_value(&sources.builtin[0]).unwrap(),
            "budget_rows": 1_000_000,
            "budget_bytes": 1u64 << 30,
            "stage": dir.join(stage),
            "allow_unsandboxed": false,
        });
        use std::process::{Command, Stdio};
        let asked = api.requests().len();
        let mut child = Command::new(BIN)
            .arg("--loader")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        benchy_mcp::proto::write_frame(child.stdin.as_mut().unwrap(), &job).unwrap();
        let t = Instant::now();
        while api.requests().len() == asked && t.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Landlock's one mark in /proc: the no-new-privileges bit it requires.
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap_or_default();
        let done: benchy_mcp::loader::Done =
            benchy_mcp::proto::read_frame(child.stdout.as_mut().unwrap()).unwrap();
        let _ = child.wait();
        assert!(done.ok, "{stage}: {:?}", done.error);
        assert_eq!(done.fetched_rows, 3, "{stage}");
        assert!(
            status.contains("NoNewPrivs:\t1"),
            "{stage}: it was not confined before its first answer arrived"
        );
        assert!(dir.join(stage).join(benchy_mcp::Bench::FILE).is_file());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
