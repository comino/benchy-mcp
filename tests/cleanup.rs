//! "Nothing outlives the session." The bench holds loaded data, and so does the spool that saves
//! a reload from fetching twice. Both are on disk, so the promise is about what is left on disk
//! however the session ends: a client closing the pipe, a client stopping the server, or the
//! server being killed outright.
//!
//! Each test runs the server with its own TMPDIR, so "left behind" is simply "not empty".
//!
//! Linux only: the tests find the engine through `/proc`, and elsewhere the engine is not
//! confined, which is what makes it able to delete at all.
#![cfg(target_os = "linux")]

mod common;
use common::{unique, Server};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// A file source and a connector source, so there is loaded data, a spool, and a connector that
/// once ran.
fn session() -> (PathBuf, PathBuf, PathBuf) {
    let root = unique("cleanup");
    let tmp = root.join("tmp");
    let data = root.join("data");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("rooms.csv"), "room,floor\nB12,1\nC3,2\n").unwrap();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/connectors");
    let connector = data.join("hello.sh");
    std::fs::copy(examples.join("hello.sh"), &connector).unwrap();
    std::fs::copy(examples.join("hello.arrows"), data.join("hello.arrows")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&connector, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = data.join("benchy.toml");
    std::fs::write(
        &cfg,
        "[[source]]\nname = \"rooms\"\ntype = \"file\"\npath = \"rooms.csv\"\n\n\
         [[source]]\nname = \"readings\"\ntype = \"exec\"\ncommand = \"hello.sh\"\n\
         [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n\
         [[source.schema]]\nname = \"meter\"\ntype = \"string\"\n\
         [[source.schema]]\nname = \"power_kw\"\ntype = \"float64\"\n",
    )
    .unwrap();
    (root, tmp, cfg)
}

fn start(tmp: &Path, cfg: &Path) -> Server {
    let mut s = Server::launch(&["--config", cfg.to_str().unwrap()], &[("TMPDIR", tmp)]);
    let (err, body, _) =
        s.query("CREATE TABLE mine AS SELECT * FROM readings; SELECT count(*) FROM mine");
    assert!(!err, "the session works: {body}");
    assert!(
        !left_behind(tmp).is_empty(),
        "the session keeps its data on disk while it runs"
    );
    s
}

/// Everything under `dir`, files and directories alike.
fn left_behind(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    left_behind(dir)
        .into_iter()
        .filter(|p| !p.is_dir())
        .collect()
}

fn eventually(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

/// Every process below `pid`: the engine, a loader, a connector and whatever it started. Taken
/// before the server is stopped, because afterwards they are re-parented and nothing links them.
fn descendants(pid: u32) -> Vec<u32> {
    let parent_of = |p: u32| -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{p}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let all: Vec<u32> = std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .collect();
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let p = found[i];
        found.extend(all.iter().filter(|c| parent_of(**c) == Some(p)));
        i += 1;
    }
    found.remove(0);
    found
}

fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "))
}

#[test]
fn a_session_leaves_nothing_behind() {
    let (root, tmp, cfg) = session();
    let mut s = start(&tmp, &cfg);
    assert!(
        s.close(Duration::from_secs(10)).is_some(),
        "exits when the client closes the pipe"
    );
    let left = left_behind(&tmp);
    assert!(left.is_empty(), "left behind: {left:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_stopped_server_leaves_nothing_behind() {
    // How MCP clients commonly stop a server. SIGTERM runs no destructors.
    let (root, tmp, cfg) = session();
    let mut s = start(&tmp, &cfg);
    let below = descendants(s.pid());
    assert!(!below.is_empty(), "the server has an engine");
    s.signal("TERM");
    let clean = eventually(Duration::from_secs(5), || left_behind(&tmp).is_empty());
    assert!(clean, "left behind: {:?}", left_behind(&tmp));
    assert!(
        eventually(Duration::from_secs(5), || !below
            .iter()
            .any(|p| running(*p))),
        "still running: {below:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_killed_server_leaves_no_data_behind() {
    // SIGKILL leaves the server no chance to act. The engine notices on its own that its server
    // is gone, and exits; the session's warden, which only waits for the server to go, removes
    // the session's directory — all of it, which the sealed engine could not: removing the
    // directory itself needs a right in TMPDIR.
    let (root, tmp, cfg) = session();
    let mut s = start(&tmp, &cfg);
    let below = descendants(s.pid());
    assert!(!below.is_empty(), "the server has an engine");
    s.signal("KILL");
    assert!(
        eventually(Duration::from_secs(5), || !below
            .iter()
            .any(|p| running(*p))),
        "the engine outlived its server: {below:?}"
    );
    assert!(
        eventually(Duration::from_secs(5), || left_behind(&tmp).is_empty()),
        "left behind: {:?}",
        left_behind(&tmp)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_killed_server_takes_a_running_connector_with_it() {
    // Killed mid-load: the connector, and anything it started, must not run on against a source
    // with nobody left to receive the data.
    let root = unique("cleanup-load");
    let tmp = root.join("tmp");
    let data = root.join("data");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let started = data.join("started");
    let connector = data.join("slow.sh");
    std::fs::write(
        &connector,
        format!(
            // Writing into its scratch until it is stopped, so the session reappears for as long
            // as its loader takes to notice the server has gone.
            "#!/bin/sh\n[ \"$1\" = fetch ] || exit 1\necho cache > upstream-cache\n\
             ( while :; do mkdir -p \"$TMPDIR/cache\" && echo x > \"$TMPDIR/cache/f\"; \
             sleep 0.01; done ) &\necho $$ $! > {}\nwait\n",
            started.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&connector, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let cfg = data.join("benchy.toml");
    std::fs::write(
        &cfg,
        "[[source]]\nname = \"slow\"\ntype = \"exec\"\ncommand = \"slow.sh\"\ndeadline_secs = 300\n\
         [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n",
    )
    .unwrap();

    let mut child = std::process::Command::new(common::BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .env("TMPDIR", &tmp)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        eventually(Duration::from_secs(20), || {
            std::fs::read_to_string(&started).is_ok_and(|s| s.split_whitespace().count() == 2)
        }),
        "the connector started"
    );
    let pids: Vec<u32> = std::fs::read_to_string(&started)
        .unwrap()
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    child.kill().unwrap();
    let _ = child.wait();
    let alive = |pid: &u32| {
        Path::new(&format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.contains(") Z "))
    };
    assert!(
        eventually(Duration::from_secs(5), || !pids.iter().any(alive)),
        "the connector or its child outlived the server: {pids:?}"
    );
    // The connector is stopped by its loader and the files are removed by the engine: two
    // processes, each noticing the server is gone on its own schedule.
    assert!(
        eventually(Duration::from_secs(5), || files_under(&tmp).is_empty()),
        "the connector's scratch was left: {:?}",
        files_under(&tmp)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_store_is_private_to_the_session() {
    // What was loaded is kept for rebuilds, read before any source is asked again — so a file
    // planted there would be served as if loaded. It is inside the session's own directory: one
    // entry in TMPDIR, made by this user, mode 0700.
    use std::os::unix::fs::MetadataExt;
    let (root, tmp, cfg) = session();
    let s = start(&tmp, &cfg);
    let entries: Vec<PathBuf> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one session directory: {entries:?}");
    let md = std::fs::metadata(&entries[0]).unwrap();
    assert!(md.is_dir());
    assert_eq!(md.mode() & 0o777, 0o700, "private");
    assert_eq!(md.uid(), unsafe { libc::geteuid() }, "ours");
    let stored: Vec<String> = std::fs::read_dir(entries[0].join("store"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !stored.is_empty() && stored.iter().all(|n| n.ends_with(".duckdb")),
        "the store is inside it: {stored:?}"
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}

/// A Python connector — Python keeps whatever signal mask it starts with, unlike `sh` — that
/// records its blocked signals and its pid, starts a child of its own, and waits.
fn python_connector(dir: &Path, seen: &Path) -> PathBuf {
    let c = dir.join("conn.py");
    std::fs::write(
        &c,
        format!(
            "#!/usr/bin/env python3\n\
             import subprocess, sys, time\n\
             if sys.argv[1] != 'fetch':\n    sys.exit(1)\n\
             blk = [l for l in open('/proc/self/status') if l.startswith('SigBlk')][0].split()[1]\n\
             child = subprocess.Popen(['sleep', '600'])\n\
             open({seen:?}, 'w').write(f'{{blk}} {{__import__(\"os\").getpid()}} {{child.pid}}\\n')\n\
             time.sleep(600)\n",
            seen = seen.display().to_string()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    c
}

fn python_session() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let root = unique("cleanup-py");
    let tmp = root.join("tmp");
    let data = root.join("data");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let seen = data.join("seen");
    python_connector(&data, &seen);
    let cfg = data.join("benchy.toml");
    std::fs::write(
        &cfg,
        "[[source]]\nname = \"slow\"\ntype = \"exec\"\ncommand = \"conn.py\"\n\
         deadline_secs = 300\n[[source.schema]]\nname = \"id\"\ntype = \"int64\"\n",
    )
    .unwrap();
    (root, tmp, cfg, seen)
}

fn seen_of(seen: &Path) -> Option<(u64, u32, u32)> {
    let text = std::fs::read_to_string(seen).ok()?;
    let mut it = text.split_whitespace();
    let blk = u64::from_str_radix(it.next()?, 16).ok()?;
    Some((blk, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

#[test]
fn a_connector_starts_with_no_signal_blocked() {
    // The server blocks SIGTERM, SIGINT and SIGHUP on every thread so one of them can wait for
    // those signals, and a child inherits its parent's mask. A connector that kept it never
    // saw the SIGTERM that asks it to stop, and outlived its engine.
    let (root, tmp, cfg, seen) = python_session();
    let mut child = std::process::Command::new(common::BIN)
        .args(["--config", cfg.to_str().unwrap()])
        .env("TMPDIR", &tmp)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        eventually(Duration::from_secs(20), || seen_of(&seen).is_some()),
        "the connector started"
    );
    let (blk, pid, grandchild) = seen_of(&seen).unwrap();
    let _ = child.kill();
    let _ = child.wait();
    for sig in [1u32, 2, 15] {
        assert!(
            blk >> (sig - 1) & 1 == 0,
            "signal {sig} blocked in the connector: {blk:x}"
        );
    }
    assert!(
        eventually(Duration::from_secs(5), || !running(pid)
            && !running(grandchild)),
        "the connector outlived the server"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_load_that_times_out_leaves_no_connector_and_no_scratch() {
    // A load past --load-timeout kills the engine. The connector, its own children, and the
    // scratch directory it was given go with it — nothing is left fetching for nobody.
    let (root, tmp, cfg, seen) = python_session();
    let out = std::process::Command::new(common::BIN)
        .args(["--config", cfg.to_str().unwrap(), "--load-timeout", "3"])
        .env("TMPDIR", &tmp)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let (_, pid, grandchild) = seen_of(&seen).expect("the connector started");
    assert!(
        eventually(Duration::from_secs(5), || !running(pid)
            && !running(grandchild)),
        "the connector or its child outlived the load"
    );
    let left = left_behind(&tmp);
    assert!(left.is_empty(), "left behind: {left:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Run `--warden` on `dir` with stdin already closed, as a server that has gone would leave it.
fn run_warden(dir: &Path, tmp: &Path) -> std::process::ExitStatus {
    std::process::Command::new(common::BIN)
        .arg("--warden")
        .arg(dir)
        .env("TMPDIR", tmp)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
}

fn dir_with_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn the_warden_removes_only_a_session_directory() {
    // Anyone can run a binary on PATH by hand. The warden is forked from its server, so there
    // is no `--warden` to run: given any directory — elsewhere, named otherwise, open to others,
    // or one that looks like a session — the binary refuses the argument and leaves it. The
    // warden itself still checks it was handed a session (`a_session`, in src/supervisor.rs).
    let root = unique("warden");
    let tmp = root.join("tmp");
    let elsewhere = root.join("benchy-1-ab-0");
    let named = tmp.join("victim");
    let open = tmp.join("benchy-2-ab-0");
    let session = tmp.join("benchy-3-ab-0");
    dir_with_mode(&elsewhere, 0o700);
    dir_with_mode(&named, 0o700);
    dir_with_mode(&open, 0o755);
    dir_with_mode(&session, 0o700);
    for d in [&elsewhere, &named, &open, &session] {
        assert!(!run_warden(d, &tmp).success(), "{d:?}");
        assert!(d.exists(), "removed {d:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The session's warden: the one process below `pid` named `benchy-warden`.
fn warden_of(pid: u32) -> u32 {
    let warden: Vec<u32> = descendants(pid)
        .into_iter()
        .filter(|p| {
            std::fs::read_to_string(format!("/proc/{p}/comm"))
                .is_ok_and(|c| c.trim_end() == "benchy-warden")
        })
        .collect();
    assert_eq!(warden.len(), 1, "one warden: {warden:?}");
    warden[0]
}

#[test]
fn the_warden_holds_nothing_of_the_servers() {
    // R3-11: the warden is forked from its server rather than started from the binary again,
    // which spent about 21 M instructions linking DuckDB for a process that only waits. It holds
    // what that one held: forked before the config is read, it forgets the environment it was
    // forked with, so a credential the server was given is nowhere in it — not in its
    // environment, not in any page of its memory — and it has one thread and no descriptor but
    // its end of the server's pipe and /dev/null.
    let (root, tmp, cfg) = session();
    let secret = format!(
        "benchy-secret-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let text = std::fs::read_to_string(&cfg).unwrap().replace(
        "command = \"hello.sh\"\n",
        "command = \"hello.sh\"\nenv = { TOKEN = \"${BENCHY_TEST_SECRET}\" }\n",
    );
    std::fs::write(&cfg, text).unwrap();
    let mut s = Server::launch(
        &["--config", cfg.to_str().unwrap()],
        &[("TMPDIR", &tmp), ("BENCHY_TEST_SECRET", Path::new(&secret))],
    );
    let (err, body, _) = s.query("SELECT count(*) FROM readings");
    assert!(!err, "the session works: {body}");
    let w = warden_of(s.pid());
    let has = |hay: &[u8]| hay.windows(secret.len()).any(|x| x == secret.as_bytes());
    let environ = std::fs::read(format!("/proc/{w}/environ")).unwrap();
    assert!(!has(&environ), "the secret is in the warden's environment");
    let status = std::fs::read_to_string(format!("/proc/{w}/status")).unwrap();
    assert!(status.contains("\nThreads:\t1\n"), "{status}");
    let fds: Vec<(String, PathBuf)> = std::fs::read_dir(format!("/proc/{w}/fd"))
        .unwrap()
        .flatten()
        .map(|e| {
            let to = std::fs::read_link(e.path()).unwrap_or_default();
            (e.file_name().to_string_lossy().into_owned(), to)
        })
        .collect();
    let mut names: Vec<&str> = fds.iter().map(|(n, _)| n.as_str()).collect();
    names.sort();
    assert_eq!(names, ["0", "1", "2"], "{fds:?}");
    for (n, to) in &fds {
        let to = to.to_string_lossy();
        let want = if n == "0" { "pipe:" } else { "/dev/null" };
        assert!(to.starts_with(want), "descriptor {n} is {to}");
    }
    // Every page it can read. The test is the server's parent and so the warden's ancestor,
    // which Yama's default lets read it; where it may not, its environment above still counts.
    match std::fs::File::open(format!("/proc/{w}/mem")) {
        Err(e) => eprintln!("the warden's memory is not readable here ({e}); not scanned"),
        Ok(mut mem) => {
            use std::io::{Read, Seek, SeekFrom};
            let maps = std::fs::read_to_string(format!("/proc/{w}/maps")).unwrap();
            let mut scanned = 0usize;
            for line in maps.lines() {
                let mut f = line.split_whitespace();
                let (range, perms) = (f.next().unwrap(), f.next().unwrap());
                if !perms.starts_with('r') {
                    continue;
                }
                let (a, b) = range.split_once('-').unwrap();
                let a = u64::from_str_radix(a, 16).unwrap();
                let b = u64::from_str_radix(b, 16).unwrap();
                let mut page = vec![0u8; (b - a) as usize];
                if mem.seek(SeekFrom::Start(a)).is_err() || mem.read_exact(&mut page).is_err() {
                    continue;
                }
                assert!(!has(&page), "the secret is in the warden's memory: {line}");
                scanned += page.len();
            }
            assert!(scanned > 1 << 20, "scanned {scanned} bytes");
        }
    }
    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_signal_to_the_warden_is_not_the_server_going() {
    // The warden waits on a pipe. A signal interrupts the wait, and an interrupted read is not an
    // end of file: the session stays for as long as its server does.
    let (root, tmp, cfg) = session();
    let s = start(&tmp, &cfg);
    let warden = warden_of(s.pid());
    // Rust handles SIGBUS itself, and its handler interrupts the read.
    unsafe { libc::kill(warden as i32, libc::SIGBUS) };
    std::thread::sleep(Duration::from_millis(1500));
    let sessions: Vec<PathBuf> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert!(
        sessions[0].join("store").is_dir(),
        "the session was removed under a live server"
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&root);
}
