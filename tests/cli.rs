//! What someone sees the first time they run the binary by hand, before any client is involved.
//!
//! A release bundle carries DuckDB's library beside the binary, so "which library did this load?"
//! is the first question when an install misbehaves. And a server that serves MCP on stdio looks
//! hung to a person at a terminal unless it says what it is waiting for.

mod common;
use common::{data_dir, BIN, PLATFORM};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn version_names_the_duckdb_library_it_runs() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(format!("benchy-mcp {}", env!("CARGO_PKG_VERSION")).as_str()),
        "the first line is what Cargo.toml says, which is what a release reports: {text}"
    );
    let duckdb = lines.next().unwrap_or_default();
    assert!(
        duckdb.starts_with("DuckDB v") && duckdb[8..].starts_with(|c: char| c.is_ascii_digit()),
        "the second line is the library's own version: {text}"
    );
}

#[test]
fn doctor_names_the_library_file_it_loaded() {
    let out = Command::new(BIN).arg("doctor").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    // The loader picks the library from the binary's rpath. When a bundle was moved without its
    // library, or a stray copy elsewhere wins, this line is where that shows.
    let loaded = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("loaded from "))
        .unwrap_or_else(|| panic!("doctor names the library file: {text}"));
    let path = std::path::Path::new(loaded);
    assert!(path.is_file(), "{loaded} exists: {text}");
    assert!(
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("libduckdb")),
        "{loaded} is DuckDB's library: {text}"
    );
}

#[test]
fn a_terminal_is_told_the_server_waits_for_a_client() {
    let d = data_dir();
    let (mut master, mut slave) = (0, 0);
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            // `*const` on Linux, `*mut` on macOS: a null `*mut` is both.
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "a pseudo-terminal for stdin");
    let tty = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(slave) };
    let mut child = Command::new(BIN)
        .arg(&d)
        .args(PLATFORM)
        .stdin(tty)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let err = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen = Vec::new();
    let told = loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) if line.contains("waiting for an MCP client") => break true,
            Ok(line) => seen.push(line),
            Err(_) => break false,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    unsafe { libc::close(master) };
    let _ = std::fs::remove_dir_all(&d);
    assert!(
        told,
        "a terminal on stdin is told what the server waits for: {seen:?}"
    );
}

#[test]
fn the_help_example_names_the_binary_by_absolute_path() {
    // Desktop apps do not read the shell's PATH, so a bare `benchy-mcp` as the command fails
    // there with nothing on screen to say why.
    let out = Command::new(BIN).arg("--help").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("\"command\":\"benchy-mcp\""), "{text}");
    assert!(
        text.contains("\"command\":\"/abs/path/to/benchy-mcp\""),
        "{text}"
    );
    assert!(text.contains("PATH"), "says why: {text}");
    // Built from source, the binary is on no PATH: the clone's `target/release` is where it is.
    assert!(!text.contains("-- benchy-mcp "), "{text}");
}

#[test]
fn the_readme_shows_the_options_as_help_prints_them() {
    // The README's Options block says it is what `--help` prints. Copied by hand, it fell behind
    // twice: `check`'s HTTP half and `--stream-channel` were missing from it.
    let out = Command::new(BIN).arg("--help").output().unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    let from = help.find("USAGE\n").expect("help has USAGE");
    let to = help
        .find("\nServes MCP")
        .expect("help ends with how it is served");
    let help = help[from..to].trim_end();
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    let start = readme.find("```\nUSAGE\n").expect("README has the block") + 4;
    let end = start + readme[start..].find("```").unwrap();
    assert_eq!(readme[start..end].trim_end(), help);
}

#[test]
fn a_skipped_file_is_named_in_the_operator_log() {
    // A folder is read at its top level, and only the formats benchy reads. What it passes over
    // is said to the operator, file by file; nothing about it reaches the agent.
    let dir = data_dir();
    std::fs::write(dir.join("notes.txt"), "not data\n").unwrap();
    std::fs::write(dir.join(".hidden.csv"), "a\n1\n").unwrap();
    std::fs::create_dir_all(dir.join("archive")).unwrap();
    std::fs::write(dir.join("archive").join("old.csv"), "a\n1\n").unwrap();
    let log = dir.with_extension("log");
    let mut s = common::Server::launch_logging(
        &[dir.to_str().unwrap()],
        &[],
        std::fs::File::create(&log).unwrap(),
    );
    let r = s.call(
        "tools/call",
        serde_json::json!({"name": "describe", "arguments": {}}),
    );
    let described = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    drop(s);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("skipped") && text.contains("notes.txt") && text.contains("not a format"),
        "{text}"
    );
    assert!(
        text.contains("archive") && text.contains("a directory"),
        "{text}"
    );
    assert!(
        text.contains(".hidden.csv") && text.contains("a hidden file"),
        "{text}"
    );
    assert!(
        !described.contains("notes")
            && !described.contains("archive")
            && !described.contains("hidden"),
        "{described}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&log);
}

/// What `--version --json` prints: each field docs/HOST-PROTOCOL.md documents, and what a
/// release's `compat.json` carries beside `min_glibc`, which is measured from the built binaries
/// rather than known to them (packaging/compat.py).
const VERSION_FIELDS: [&str; 5] = [
    "benchy",
    "duckdb",
    "host_protocols",
    "platforms",
    "full_sandbox_min_landlock_abi",
];

/// The release platform this binary was built for, as a bundle's name says it.
fn this_platform() -> String {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else {
        "macos"
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" if os == "macos" => "arm64",
        a => a,
    };
    format!("{os}-{arch}")
}

/// `hello`'s reply to a host offering `offered`.
fn hello(offered: &[u64]) -> serde_json::Value {
    use benchy_mcp::proto::{read_frame, write_frame};
    let mut child = Command::new(BIN)
        .arg("host")
        .args(PLATFORM)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    write_frame(
        &mut stdin,
        &serde_json::json!({"id": 1, "call": "hello", "protocol": offered}),
    )
    .unwrap();
    let reply = read_frame(&mut stdout).expect("a reply to hello");
    let _ = child.kill();
    let _ = child.wait();
    reply
}

#[test]
fn version_json_says_what_a_pin_bump_checks() {
    // A host that pins a release checks it in its CI before deploying, against the release's
    // compat.json or the binary it was handed: what it reads must be what `hello` and `doctor`
    // find at run time, or the check passes a pin that fails in production.
    let out = Command::new(BIN)
        .args(["--version", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON alone ({e}):\n{text}"));
    let mut got: Vec<&str> = v
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    got.sort_unstable();
    let mut want = VERSION_FIELDS.to_vec();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "every documented field, and none that is not: {v}"
    );
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/HOST-PROTOCOL.md"
    ))
    .unwrap();
    for f in VERSION_FIELDS.iter().chain(&["min_glibc"]) {
        assert!(
            doc.contains(&format!("`{f}`")),
            "HOST-PROTOCOL.md documents `{f}`"
        );
    }

    // The version Cargo.toml says, and the library's own, as plain `--version` says them.
    assert_eq!(v["benchy"], env!("CARGO_PKG_VERSION"), "{v}");
    let plain = Command::new(BIN).arg("--version").output().unwrap();
    let plain = String::from_utf8(plain.stdout).unwrap();
    let duckdb = v["duckdb"].as_str().expect("duckdb is a string");
    assert!(duckdb.starts_with(|c: char| c.is_ascii_digit()), "{v}");
    assert_eq!(
        plain.lines().nth(1),
        Some(format!("DuckDB v{duckdb}").as_str())
    );
    let platforms: Vec<&str> = v["platforms"]
        .as_array()
        .expect("platforms is a list")
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    assert!(platforms.contains(&this_platform().as_str()), "{v}");

    // The protocol range is the one `hello` agrees to: its newest is the newest offered of
    // everything, its oldest is answered alone, and one past either end is refused.
    let protocols: Vec<u64> = v["host_protocols"]
        .as_array()
        .expect("host_protocols is a list")
        .iter()
        .map(|p| p.as_u64().expect("a version number"))
        .collect();
    let (oldest, newest) = (protocols[0], *protocols.last().unwrap());
    assert_eq!(
        protocols,
        (oldest..=newest).collect::<Vec<_>>(),
        "no gaps: {v}"
    );
    let all: Vec<u64> = (0..=newest + 1).collect();
    let greeted = hello(&all);
    assert_eq!(greeted["protocol"], newest, "{greeted}");
    assert_eq!(hello(&[oldest])["protocol"], oldest);
    for outside in [oldest - 1, newest + 1] {
        let r = hello(&[outside]);
        assert_eq!(r["error"]["code"], "E_VERSION", "{outside}: {r}");
    }
    assert_eq!(greeted["benchy"], v["benchy"], "{greeted}");
    assert_eq!(greeted["engine"], format!("DuckDB v{duckdb}"), "{greeted}");

    // The ABI is the one below which the sandbox's full posture falls short, on any kernel, not
    // only on the one these tests run on.
    let full = v["full_sandbox_min_landlock_abi"]
        .as_u64()
        .expect("a Landlock ABI") as u32;
    let posture = benchy_mcp::sandbox::Posture::Full;
    assert!(posture.shortfall(full).is_none(), "{v}");
    assert!(posture.shortfall(full - 1).is_some(), "{v}");

    // `doctor` names the same version and library; and, where Landlock is, it calls the sandbox
    // full exactly from the ABI this says the full posture needs, as `hello` does.
    let doctor = Command::new(BIN).arg("doctor").output().unwrap();
    let doctor = String::from_utf8(doctor.stdout).unwrap();
    assert_eq!(
        doctor.lines().next(),
        Some(format!("benchy-mcp {}", env!("CARGO_PKG_VERSION")).as_str())
    );
    assert!(doctor.contains(&format!(": DuckDB v{duckdb} ")), "{doctor}");
    #[cfg(target_os = "linux")]
    {
        let out = Command::new(BIN)
            .args(["doctor", "--probe", "--json"])
            .output()
            .unwrap();
        let d: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(d["benchy"], v["benchy"], "{d}");
        assert_eq!(d["engine"], format!("DuckDB v{duckdb}"), "{d}");
        let full = u64::from(full);
        let abi = d["landlock_abi"].as_u64().unwrap();
        assert_eq!(d["verdict"] == "enforced", abi >= full, "{d}");
        assert_eq!(
            greeted["sandbox_posture"]["network"],
            abi >= full,
            "{greeted}"
        );
    }
}
