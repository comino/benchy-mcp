//! REQ-5: the process that runs agent SQL holds data, never credentials — and nothing else the
//! server was handed.
#![cfg(target_os = "linux")]

mod common;
use common::{engine_of, holds, memory_holds, unique, Server};
use std::path::{Path, PathBuf};

/// Where each of a process's descriptors points.
fn descriptors_of(pid: u32) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .collect()
}

#[test]
fn the_engine_inherits_no_environment_and_no_descriptor() {
    // Whatever a client passed the server — a credential in its environment, an open file — must
    // not reach the process that runs agent SQL. Its environment is built, and empty; every
    // descriptor past stdio is closed before it starts.
    let dir = common::data_dir();
    let held = unique("held");
    std::fs::write(&held, "held open").unwrap();
    // `/proc/<pid>/fd` gives the resolved path. Under a TMPDIR reached through a symlink the path
    // as spelt never matched it: the check that the server holds the file failed, and the one
    // that the engine does not would have passed without looking.
    let held = std::fs::canonicalize(&held).unwrap();
    let c = std::ffi::CString::new(held.to_str().unwrap()).unwrap();
    // Without O_CLOEXEC: inherited by whatever this process starts next.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
    assert!(fd > 2);
    let s = Server::launch(
        &[dir.to_str().unwrap()],
        &[("BENCHY_CANARY", Path::new("canary-in-the-environment"))],
    );
    unsafe { libc::close(fd) };

    // The server was handed both, so the test tests something.
    let server_env = std::fs::read(format!("/proc/{}/environ", s.pid())).unwrap();
    assert!(holds(&server_env, b"canary-in-the-environment"));
    assert!(descriptors_of(s.pid()).contains(&held));

    let engine = engine_of(s.pid());
    let env = std::fs::read(format!("/proc/{engine}/environ")).unwrap();
    assert!(
        env.is_empty(),
        "the engine's environment: {:?}",
        String::from_utf8_lossy(&env)
    );
    let fds = descriptors_of(engine);
    assert!(!fds.contains(&held), "the engine holds {held:?}: {fds:?}");
    drop(s);
    let _ = std::fs::remove_file(&held);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn credentials_never_cross() {
    // The server resolves `${VAR}` and hands the result to the loader that runs the connector,
    // and to no other process. The engine never holds it: not in its environment, not in its
    // arguments, nowhere in its memory.
    let dir = unique("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/connectors");
    std::fs::copy(examples.join("hello.arrows"), dir.join("hello.arrows")).unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let token = format!("secret-{}-{nanos}", std::process::id());
    let table = format!("readings_{}_{nanos}", std::process::id());
    let seen = dir.join("seen");
    let connector = dir.join("connector.sh");
    std::fs::write(
        &connector,
        format!(
            "#!/bin/sh\n[ \"$1\" = fetch ] || exit 1\nprintf %s \"$TOKEN\" > {}\n\
             cat \"$(dirname \"$0\")/hello.arrows\"\n",
            seen.display()
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
            "[[source]]\nname = \"{table}\"\ntype = \"exec\"\ncommand = \"{}\"\n\
             [source.env]\nTOKEN = \"${{BENCHY_TEST_TOKEN}}\"\n\
             [[source.schema]]\nname = \"id\"\ntype = \"int64\"\n\
             [[source.schema]]\nname = \"meter\"\ntype = \"string\"\n\
             [[source.schema]]\nname = \"power_kw\"\ntype = \"float64\"\n",
            connector.display()
        ),
    )
    .unwrap();
    let mut s = Server::launch(
        &["--config", cfg.to_str().unwrap()],
        &[("BENCHY_TEST_TOKEN", Path::new(&token))],
    );
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        token,
        "the connector was given its credential"
    );
    let (err, body, _) = s.query(&format!("SELECT count(*) + 900 AS n FROM {table}"));
    assert!(!err && body.contains("904"), "{body}");

    let engine = engine_of(s.pid());
    for what in ["environ", "cmdline"] {
        let bytes = std::fs::read(format!("/proc/{engine}/{what}")).unwrap();
        assert!(!holds(&bytes, token.as_bytes()), "in the engine's {what}");
    }
    // The scan finds what is there — the table's name, in the engine's catalogue — so not
    // finding the credential means something.
    assert!(memory_holds(engine, table.as_bytes()), "the scan works");
    assert!(
        !memory_holds(engine, token.as_bytes()),
        "the credential is in the engine's memory"
    );
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}
