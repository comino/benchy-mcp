//! The loader memory ceiling (REQ-8): every loader is read from outside while it runs, with the
//! connector it runs and whatever that starts, and past `--loader-memory-ceiling` its load fails
//! and nothing of it is attached. Linux only: resident memory is read from /proc, and elsewhere
//! the ceiling is not enforced (`a_loader_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
#![cfg(target_os = "linux")]

mod common;

use common::Server;
use std::io::Write;
use std::path::Path;

fn describe(s: &mut Server) -> String {
    let v = s.call(
        "tools/call",
        serde_json::json!({"name": "describe", "arguments": {}}),
    );
    v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A CSV of `rows` meter readings. At 2,000,000 rows (70 MB) its loader peaks at 171 MB; at 10
/// rows at 52 MB, a loader's DuckDB and little else (DuckDB 1.5.5, four threads).
fn readings_csv(path: &Path, rows: u64) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    writeln!(f, "ts,meter,kw").unwrap();
    for i in 0..rows {
        writeln!(f, "2026-01-01 00:00:00,m{},{}.5", i % 500, i).unwrap();
    }
}

#[test]
fn a_loader_past_its_memory_ceiling_is_killed_and_attaches_nothing() {
    // REQ-8. A loader is a process of its own, and nothing bounded what one held: a 676 MB CSV
    // of 300 columns, whose stage does not fit its 256 MiB cap and is held whole, took its
    // loader to 2,090 MB. Under the ceiling the load is stopped, the operator is told what
    // happened and what to do, and the other sources load as before.
    let dir = common::unique("loader-ceiling");
    std::fs::create_dir_all(&dir).unwrap();
    readings_csv(&dir.join("big.csv"), 2_000_000);
    readings_csv(&dir.join("small.csv"), 10);
    let log = dir.join("operator.log");
    let mut s = Server::launch_logging(
        &[dir.to_str().unwrap(), "--loader-memory-ceiling", "100MB"],
        &[],
        std::fs::File::create(&log).unwrap(),
    );
    let seen = describe(&mut s);
    assert!(
        seen.contains("small (10 rows)"),
        "the small one loads: {seen}"
    );
    assert!(
        !seen.contains("big (2000000 rows)") && seen.contains("big"),
        "the big one is listed as unavailable, with nothing of it attached: {seen}"
    );
    let (err, body, _) = s.query("SELECT count(*) FROM big");
    assert!(err, "nothing of it is on the bench: {body}");
    assert!(
        !seen.contains("memory") && !body.contains("memory"),
        "the agent hears of tables, not loaders: {seen} {body}"
    );
    s.close(std::time::Duration::from_secs(10));
    let said = std::fs::read_to_string(&log).unwrap();
    assert!(
        said.contains("`big` was not loaded")
            && said.contains("passed the loader memory ceiling")
            && said.contains("against 100 MB"),
        "the operator is told what happened: {said}"
    );
    assert!(
        said.contains("Narrow the source") && said.contains("--loader-memory-ceiling"),
        "and what to do about it: {said}"
    );

    // Under the default the same file loads.
    let mut s = Server::start(&dir, &[]);
    assert!(
        describe(&mut s).contains("big (2000000 rows)"),
        "under 1 GB it loads"
    );
}

#[test]
fn a_loader_memory_ceiling_is_set_in_one_place_and_must_be_a_size() {
    // As the engine's ceiling: on the command line or in the config, never both, and a size that
    // does not parse stops startup rather than leaving the default in its place.
    let dir = common::unique("loader-ceiling-config");
    std::fs::create_dir_all(&dir).unwrap();
    readings_csv(&dir.join("small.csv"), 10);
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        "[[source]]\nname = \"small\"\ntype = \"file\"\npath = \"small.csv\"\n\
         [loader]\nmemory_ceiling = \"100MB\"\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(common::BIN)
            .args(args)
            .args(common::PLATFORM)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let cfg = cfg.to_str().unwrap();
    let both = run(&["--config", cfg, "--loader-memory-ceiling", "1GB"]);
    assert!(!both.status.success());
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("set it in one place"),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );
    let typo = run(&[dir.to_str().unwrap(), "--loader-memory-ceiling", "lots"]);
    assert!(!typo.status.success());
    assert!(
        String::from_utf8_lossy(&typo.stderr).contains("is not a size"),
        "{}",
        String::from_utf8_lossy(&typo.stderr)
    );
    let mut s = Server::launch(&["--config", cfg], &[]);
    assert!(describe(&mut s).contains("small (10 rows)"));
}

/// A connector that holds `$DIRECT` bytes in itself, then `$SORTED` bytes in a process it
/// starts, for a few seconds each, before it writes its stream. `sort` holds a line with no
/// newline whole (200 MB of zeros: 198 MB resident), and `dash` a variable twice over (200 MB:
/// 392 MB).
fn holding_connector(dir: &Path) -> std::path::PathBuf {
    let mut data = Vec::new();
    {
        use arrow::array::{ArrayRef, Int64Array, RecordBatch};
        use arrow::datatypes::{DataType, Field, Schema};
        let schema =
            std::sync::Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![std::sync::Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef],
        )
        .unwrap();
        let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut data, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }
    std::fs::write(dir.join("data.arrows"), data).unwrap();
    let p = dir.join("connector");
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  fetch)\n    \
             if [ \"$DIRECT\" -gt 0 ]; then\n      \
             x=$(head -c \"$DIRECT\" /dev/zero | tr '\\0' a); sleep 3\n    fi\n    \
             if [ \"$SORTED\" -gt 0 ]; then\n      \
             (head -c \"$SORTED\" /dev/zero; sleep 3) | sort > /dev/null\n    fi\n    \
             cat '{}' ;;\n  *) exit 2 ;;\nesac\n",
            dir.join("data.arrows").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

/// Measured: a connector that held 100 MB itself, and one whose child held 200 MB, each passed a
/// 150 MB ceiling and was killed with its whole process group.
#[test]
fn a_connectors_memory_counts_against_its_loaders_ceiling() {
    // REQ-8. A connector is a process of the loader's, and is not confined: a connector that
    // held 800 MB before it wrote a row was bounded by nothing, and neither was a process it
    // started. Each counts with its loader: the connector itself, and what it starts.
    let dir = common::unique("connector-ceiling");
    std::fs::create_dir_all(&dir).unwrap();
    let connector = holding_connector(&dir);
    let source = |name: &str, direct: u64, sorted: u64| {
        format!(
            "[[source]]\nname = \"{name}\"\ntype = \"exec\"\ncommand = \"{}\"\n\
             env = {{ DIRECT = \"{direct}\", SORTED = \"{sorted}\" }}\n\
             [[source.schema]]\nname = \"id\"\ntype = \"int64\"\nnullable = false\n",
            connector.display()
        )
    };
    let cfg = dir.join("benchy.toml");
    std::fs::write(
        &cfg,
        format!(
            "{}{}{}",
            source("itself", 100_000_000, 0),
            source("started", 0, 200_000_000),
            source("light", 0, 0)
        ),
    )
    .unwrap();
    let log = dir.join("operator.log");
    let mut s = Server::launch_logging(
        &[
            "--config",
            cfg.to_str().unwrap(),
            "--loader-memory-ceiling",
            "150MB",
        ],
        &[],
        std::fs::File::create(&log).unwrap(),
    );
    let seen = describe(&mut s);
    assert!(seen.contains("light (3 rows)"), "under the ceiling: {seen}");
    for name in ["itself", "started"] {
        assert!(
            !seen.contains(&format!("{name} (3 rows)")),
            "{name}: not attached: {seen}"
        );
    }
    s.close(std::time::Duration::from_secs(10));
    let said = std::fs::read_to_string(&log).unwrap();
    for name in ["itself", "started"] {
        assert!(
            said.contains(&format!(
                "loading `{name}` passed the loader memory ceiling: the loader and the \
                 connector it ran held"
            )),
            "{name}: {said}"
        );
    }
    // And nothing it started is left running: the kill takes the connector's whole group.
    let left = std::process::Command::new("pgrep")
        .args(["-f", &connector.display().to_string()])
        .output()
        .unwrap();
    assert!(left.stdout.is_empty(), "left running: {left:?}");

    // Under the default, both load.
    let mut s = Server::launch(&["--config", cfg.to_str().unwrap()], &[]);
    let seen = describe(&mut s);
    assert!(
        seen.contains("itself (3 rows)") && seen.contains("started (3 rows)"),
        "{seen}"
    );
}
