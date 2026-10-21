//! Loaders: one process per source, the only processes that read a source (docs/HOST-PROTOCOL.md).

use benchy_mcp::loader::{output, Done, Job};
use std::path::PathBuf;

fn unique(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "benchy-loader-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn a_loaders_output_is_checked_before_it_is_kept() {
    // The supervisor moves a loader's output into the store, and the supervisor is not confined.
    // So nothing is taken on a loader's word: the file is the one in the staging directory the
    // job named, and it has to be a plain file of its own — not a way to reach one elsewhere.
    let dir = unique("output");
    let stage = dir.join("stage");
    std::fs::create_dir(&stage).unwrap();
    let job = Job::File {
        name: "readings".into(),
        path: dir.join("in.csv"),
        stage: stage.clone(),
        allow_unsandboxed: false,
        shape: Default::default(),
        timezone: None,
        declared: Default::default(),
    };
    let file = stage.join("bench.duckdb");
    let elsewhere = dir.join("elsewhere.duckdb");
    std::fs::write(&elsewhere, "someone else's").unwrap();
    assert!(output(&job).is_err(), "nothing was written");

    std::os::unix::fs::symlink(&elsewhere, &file).unwrap();
    assert!(output(&job).is_err(), "a symlink to a file elsewhere");
    std::fs::remove_file(&file).unwrap();

    std::fs::hard_link(&elsewhere, &file).unwrap();
    assert!(output(&job).is_err(), "a second name for a file elsewhere");
    std::fs::remove_file(&file).unwrap();

    let c = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(output(&job).is_err(), "a FIFO");
    std::fs::remove_file(&file).unwrap();

    std::fs::create_dir(&file).unwrap();
    assert!(output(&job).is_err(), "a directory");
    std::fs::remove_dir(&file).unwrap();

    std::fs::write(&file, "a table").unwrap();
    std::fs::write(stage.join("bench.duckdb.wal"), "unmerged").unwrap();
    assert!(
        output(&job).is_err(),
        "a write-ahead log not merged into it"
    );
    std::fs::remove_file(stage.join("bench.duckdb.wal")).unwrap();

    assert_eq!(output(&job).unwrap(), file, "a plain file of its own is");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run one loader on `stdin`, as the supervisor does, and read what it reports.
fn run_loader(stdin: &[u8]) -> Done {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_benchy-mcp"))
        .arg("--loader")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let done = benchy_mcp::proto::read_frame(child.stdout.as_mut().unwrap()).unwrap();
    let _ = child.wait();
    done
}

#[cfg(target_os = "linux")]
fn job_frame(job: &Job) -> Vec<u8> {
    let mut buf = Vec::new();
    benchy_mcp::proto::write_frame(&mut buf, job).unwrap();
    buf
}

// Linux only: the loader runs confined, which takes Landlock.
#[test]
#[cfg(target_os = "linux")]
fn a_host_attach_past_what_the_session_has_left_is_refused_as_a_limit() {
    // REQ-12 for a host's attach: the loader is told what the session's budget has left, and an
    // attachment over it is refused before anything is written — as `E_LIMIT`, which a host
    // reads as "the session allows no more", not `E_DATA`, "what you sent is wrong".
    let examples = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/connectors");
    let input = examples.join("hello.arrows");
    let (rows, bytes) = (4u64, std::fs::metadata(&input).unwrap().len());
    let dir = unique("budget");
    let attach = |n: u32, budget_rows: u64, budget_bytes: u64| {
        // The loader makes its staging directory itself, as it does for the supervisor.
        let stage = dir.join(format!("stage-{n}"));
        run_loader(&job_frame(&Job::Arrow {
            name: "readings".into(),
            path: Some(input.clone()),
            stage,
            max_rows: u64::MAX,
            max_bytes: u64::MAX,
            budget_rows,
            budget_bytes,
            allow_unsandboxed: false,
            channel: false,
            shape: Default::default(),
        }))
    };

    let done = attach(0, rows, bytes);
    assert!(done.ok, "exactly what is left fits: {:?}", done.error);
    for (n, budget_rows, budget_bytes, what) in [
        (1, rows - 1, bytes, "one row more than is left"),
        (2, rows, bytes - 1, "one byte more than is left"),
        (
            3,
            rows - 1,
            u64::MAX,
            "rows over, however many bytes are left",
        ),
        (
            4,
            u64::MAX,
            bytes - 1,
            "bytes over, however many rows are left",
        ),
    ] {
        let done = attach(n, budget_rows, budget_bytes);
        assert!(!done.ok, "{what} was attached");
        assert_eq!(done.code.as_deref(), Some("E_LIMIT"), "{what}: {done:?}");
        let error = done.error.unwrap_or_default();
        assert!(error.contains("budget left"), "{what}: {error}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// Linux only: the loader runs confined, which takes Landlock.
#[test]
#[cfg(target_os = "linux")]
fn a_host_attach_stops_at_the_budget_not_after_reading_it_all() {
    // REQ-12: the rows are counted as each batch arrives, and the attach is refused at the batch
    // that passes what the session has left — before it is written, and before the rest is read.
    // Checked only once the whole stream was in its table, a refused attach paid for all of it
    // first. The stream here goes bad after its second batch: read to the end, it is refused as
    // `E_DATA`; stopped at the budget, as `E_LIMIT`.
    use arrow::array::{ArrayRef, Int64Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;
    let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int64, true)]));
    let batch = |from: i64| {
        RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from_iter_values(from..from + 10)) as ArrayRef],
        )
        .unwrap()
    };
    let mut buf = Vec::new();
    {
        let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &schema).unwrap();
        w.write(&batch(0)).unwrap();
        w.write(&batch(10)).unwrap();
        w.finish().unwrap();
    }
    let eos = buf.split_off(buf.len() - 8);
    // A message that says it is 16 bytes of metadata, and is not.
    buf.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 16, 0, 0, 0]);
    buf.extend_from_slice(&[0x5a; 16]);
    buf.extend_from_slice(&eos);
    let dir = unique("budget-early");
    let input = dir.join("bad-after-two.arrows");
    std::fs::write(&input, &buf).unwrap();
    let attach = |n: u32, budget_rows: u64| {
        run_loader(&job_frame(&Job::Arrow {
            name: "readings".into(),
            path: Some(input.clone()),
            stage: dir.join(format!("stage-{n}")),
            max_rows: u64::MAX,
            max_bytes: u64::MAX,
            budget_rows,
            budget_bytes: u64::MAX,
            allow_unsandboxed: false,
            channel: false,
            shape: Default::default(),
        }))
    };
    let whole = attach(0, u64::MAX);
    assert_eq!(
        whole.code.as_deref(),
        Some("E_DATA"),
        "read whole: {whole:?}"
    );
    let stopped = attach(1, 15);
    assert_eq!(
        stopped.code.as_deref(),
        Some("E_LIMIT"),
        "the second batch passes the budget, and nothing past it is read: {stopped:?}"
    );
    assert!(
        stopped.error.unwrap_or_default().contains("budget left"),
        "says it was the budget"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Linux only: the helpers job runs confined, which takes Landlock.
#[test]
#[cfg(target_os = "linux")]
fn a_helpers_job_reports_ok_only_when_every_helper_was_made() {
    // REQ-18: the job's report is ok when every helper is made, and not when one is broken, with
    // what was made either way.
    let dir = unique("helpers-job");
    let col = |n: &str, t: &str| (n.to_string(), t.to_string());
    let tables = vec![(
        "meters".to_string(),
        vec![col("meter", "VARCHAR"), col("power_kw", "DOUBLE")],
    )];
    let run = |n: u32, sql: &str| {
        run_loader(&job_frame(&Job::Helpers {
            helpers: benchy_mcp::helpers::parse(sql).unwrap(),
            tables: tables.clone(),
            stage: dir.join(format!("stage-{n}")),
            memory_limit: "1GB".into(),
            allow_unsandboxed: false,
        }))
    };
    let made = run(
        0,
        "CREATE MACRO twice(x) AS x * 2;\nCREATE VIEW peak AS SELECT max(power_kw) AS kw FROM meters;",
    );
    let built = made.helpers.as_ref().expect("what it made");
    assert!(built.broken.is_empty(), "{:?}", built.broken);
    assert_eq!(built.shown.len(), 2, "{built:?}");
    assert!(made.ok, "every helper made: {made:?}");

    let broken = run(
        1,
        "CREATE VIEW peak AS SELECT max(no_such_column) AS kw FROM meters;",
    );
    let built = broken.helpers.as_ref().expect("what it made");
    assert_eq!(built.broken.len(), 1, "{built:?}");
    assert!(!broken.ok, "a helper broken: {broken:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_loader_that_cannot_read_its_job_says_so() {
    // The supervisor passes a loader's error on to the operator or the host. A loader handed
    // something that is not a job answers with why, not with a bare failure.
    let mut garbage = 5u32.to_be_bytes().to_vec();
    garbage.extend_from_slice(b"{not}");
    let done = run_loader(&garbage);
    assert!(!done.ok);
    let error = done.error.expect("it says why");
    assert!(error.contains("could not read its job"), "{error}");
}

/// Run one loader as [`run_loader`] does, and also return the most memory it held: its own peak
/// resident set, read from the kernel as it is reaped, so nothing has to sample it.
// Reaped with `wait4`, which `Child::wait` cannot do: it does not return the child's usage.
#[allow(clippy::zombie_processes)]
#[cfg(target_os = "linux")]
fn run_loader_measured(stdin: &[u8]) -> (Done, u64) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_benchy-mcp"))
        .arg("--loader")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let done = benchy_mcp::proto::read_frame(child.stdout.as_mut().unwrap()).unwrap();
    let mut status = 0;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let pid = unsafe { libc::wait4(child.id() as i32, &mut status, 0, &mut usage) };
    assert_eq!(pid, child.id() as i32, "the loader is reaped here");
    // Linux reports ru_maxrss in KiB.
    (done, usage.ru_maxrss as u64 * 1024)
}

/// An Arrow IPC stream of `rows` rows in batches of 65,536, as ClickHouse sends them: an instant,
/// an id, a reading and a short label.
#[cfg(target_os = "linux")]
fn arrow_file(path: &std::path::Path, rows: usize) {
    use arrow::array::{Float64Array, Int64Array, StringArray, TimestampMicrosecondArray};
    use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
    let schema = std::sync::Arc::new(Schema::new(vec![
        Field::new(
            "observed_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
        Field::new("id", DataType::Int64, false),
        Field::new("value", DataType::Float64, false),
        Field::new("label", DataType::Utf8, false),
    ]));
    let out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut w = arrow::ipc::writer::StreamWriter::try_new(out, &schema).unwrap();
    let mut start = 0;
    while start < rows {
        let n = (rows - start).min(65_536);
        let ids = (start..start + n).map(|i| i as i64);
        let batch = arrow::array::RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(
                    TimestampMicrosecondArray::from_iter_values(
                        ids.clone().map(|i| 1_767_225_600_000_000 + i * 60_000_000),
                    )
                    .with_timezone("UTC"),
                ),
                std::sync::Arc::new(Int64Array::from_iter_values(ids.clone())),
                std::sync::Arc::new(Float64Array::from_iter_values(
                    ids.clone().map(|i| (i as f64 * 0.37).sin() * 1000.0),
                )),
                std::sync::Arc::new(StringArray::from_iter_values(
                    ids.map(|i| format!("label-{}", (i * 7919) % 100_003)),
                )),
            ],
        )
        .unwrap();
        w.write(&batch).unwrap();
        start += n;
    }
    w.finish().unwrap();
}

/// A CSV of `rows` rows that DuckDB holds in many times its size: sixteen columns, fifteen of
/// them empty, so a 17-byte line is fifteen null strings and a number in memory.
#[cfg(target_os = "linux")]
fn sparse_csv(path: &std::path::Path, rows: usize) {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let header: Vec<String> = (0..16).map(|i| format!("c{i}")).collect();
    writeln!(out, "{}", header.join(",")).unwrap();
    for i in 0..rows {
        writeln!(out, ",,,,,,,,,,,,,,,{}", i % 10).unwrap();
    }
    out.flush().unwrap();
}

// Linux only: the loader runs confined, which takes Landlock, and its peak is `ru_maxrss` in KiB.
#[test]
#[cfg(target_os = "linux")]
fn a_large_csv_is_not_held_in_memory_while_it_is_staged() {
    // A CSV file is read into a stage on every thread before its table is written. The stage
    // was held in memory whole: a 10,000,000-row CSV took its loader to 715 MB. It is now held
    // to `STAGE_MEMORY` and spills past it to the staging directory, so the loader's peak stays
    // where it is as the file grows. Here 6,000,000 and 12,000,000 rows, 97 and 194 MB of CSV
    // that DuckDB holds in about 650 MB and 1.3 GB: the larger may not take the loader past the
    // smaller by as much as the CSV grew. Measured on 4 threads: 373–400 MB and 390–413 MB;
    // held whole, 740 and 1,372 MB. Both files are well past the cap: at 3,000,000 rows the
    // stage barely passes it, and its peak is lower. Nothing of the spill is left in the
    // staging directory.
    let dir = unique("csv-memory");
    let (small, large) = (dir.join("small.csv"), dir.join("large.csv"));
    sparse_csv(&small, 6_000_000);
    sparse_csv(&large, 12_000_000);
    let grown = std::fs::metadata(&large).unwrap().len() - std::fs::metadata(&small).unwrap().len();
    let load = |n: u32, input: &std::path::Path| {
        let stage = dir.join(format!("stage-{n}"));
        let (done, peak) = run_loader_measured(&job_frame(&Job::File {
            name: "readings".into(),
            path: input.to_path_buf(),
            stage: stage.clone(),
            allow_unsandboxed: false,
            shape: Default::default(),
            timezone: None,
            declared: Default::default(),
        }));
        assert!(done.ok, "{:?}", done.error);
        let left: Vec<String> = std::fs::read_dir(&stage)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["bench.duckdb"], "the staging directory holds more");
        peak
    };
    let base = load(0, &small);
    let peak = load(1, &large);
    let _ = std::fs::remove_dir_all(&dir);
    let grew = peak.saturating_sub(base);
    eprintln!(
        "{} MB more CSV: the loader peaked at {} MB, {} MB over the smaller file's",
        grown >> 20,
        peak >> 20,
        grew >> 20
    );
    assert!(
        grew < grown,
        "{} MB more CSV grew the loader by {} MB",
        grown >> 20,
        grew >> 20
    );
}

/// A CSV of `rows` very wide rows: 200 one-digit columns, which DuckDB holds as `BIGINT`s, four
/// times the line in memory.
#[cfg(target_os = "linux")]
fn wide_csv(path: &std::path::Path, rows: usize) {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let header: Vec<String> = (0..200).map(|i| format!("c{i}")).collect();
    writeln!(out, "{}", header.join(",")).unwrap();
    for i in 0..rows {
        let line: Vec<String> = (0..200).map(|j| ((i + j) % 10).to_string()).collect();
        writeln!(out, "{}", line.join(",")).unwrap();
    }
    out.flush().unwrap();
}

// Linux only: the loader runs confined, which takes Landlock, and its peak is `ru_maxrss` in KiB.
#[test]
#[cfg(target_os = "linux")]
fn a_wide_csv_is_not_held_in_memory_while_it_is_staged() {
    // A stage is held to `STAGE_MEMORY`, but each thread that fills it pins a block of every
    // column it appends to: a stage of wide rows on every thread ran DuckDB out of memory under
    // the cap, and was staged again without it, held whole. A 676 MB CSV of 300 columns took its
    // loader to 2,090 MB. A file whose first line is that wide is now read on one thread,
    // straight into its table under the cap, which spills past it as a narrow stage does. Here
    // 200,000 and 600,000 rows of 200 columns, 76 and 229 MB of CSV that DuckDB holds in about
    // 320 and 960 MB, both past the cap: the larger may not take the loader past the smaller by
    // as much as the CSV grew. Measured on 4 threads, three runs each: 328–345 MB and
    // 361–395 MB, and 350–375 MB at 800,000 rows. Staged on every thread and then without the
    // cap, 200,000 and 400,000 rows took it to 1,007 and 1,371 MB.
    let dir = unique("wide-memory");
    let (small, large) = (dir.join("small.csv"), dir.join("large.csv"));
    wide_csv(&small, 200_000);
    wide_csv(&large, 600_000);
    let grown = std::fs::metadata(&large).unwrap().len() - std::fs::metadata(&small).unwrap().len();
    let load = |n: u32, input: &std::path::Path| {
        let (done, peak) = run_loader_measured(&job_frame(&Job::File {
            name: "wide".into(),
            path: input.to_path_buf(),
            stage: dir.join(format!("stage-{n}")),
            allow_unsandboxed: false,
            shape: Default::default(),
            timezone: None,
            declared: Default::default(),
        }));
        assert!(done.ok, "{:?}", done.error);
        peak
    };
    let base = load(0, &small);
    let peak = load(1, &large);
    let _ = std::fs::remove_dir_all(&dir);
    let grew = peak.saturating_sub(base);
    eprintln!(
        "{} MB more CSV: the loader peaked at {} MB, {} MB over the smaller file's",
        grown >> 20,
        peak >> 20,
        grew >> 20
    );
    assert!(
        grew < grown,
        "{} MB more CSV grew the loader by {} MB",
        grown >> 20,
        grew >> 20
    );
}

// Linux only: the loader runs confined, which takes Landlock, and its peak is `ru_maxrss` in KiB.
#[test]
#[cfg(target_os = "linux")]
fn a_large_stream_is_not_held_in_memory_while_it_loads() {
    // A stream was collected into a list of batches until it ended, then copied into an
    // in-memory staging database, then written: this 112 MB stream grew the loader by 321 MB.
    // Each batch now goes into the table as it arrives, and the loader never holds the stream
    // whole: 68 MB, which is DuckDB's own buffers, measured. Measured as the loader's own peak
    // against a one-row load's, so the libraries and DuckDB's own start-up are not counted.
    let dir = unique("memory");
    let (small, large) = (dir.join("small.arrows"), dir.join("large.arrows"));
    arrow_file(&small, 1);
    arrow_file(&large, 3_000_000);
    let size = std::fs::metadata(&large).unwrap().len();
    let attach = |n: u32, input: &std::path::Path| {
        let (done, peak) = run_loader_measured(&job_frame(&Job::Arrow {
            name: "readings".into(),
            path: Some(input.to_path_buf()),
            stage: dir.join(format!("stage-{n}")),
            max_rows: u64::MAX,
            max_bytes: u64::MAX,
            budget_rows: u64::MAX,
            budget_bytes: u64::MAX,
            allow_unsandboxed: false,
            channel: false,
            shape: Default::default(),
        }));
        assert!(done.ok, "{:?}", done.error);
        peak
    };
    let base = attach(0, &small);
    let peak = attach(1, &large);
    let _ = std::fs::remove_dir_all(&dir);
    let grew = peak.saturating_sub(base);
    eprintln!(
        "a {} MB stream: the loader peaked at {} MB, {} MB over a one-row load",
        size >> 20,
        peak >> 20,
        grew >> 20
    );
    assert!(
        grew < size,
        "a {} MB stream grew the loader by {} MB",
        size >> 20,
        grew >> 20
    );
}
