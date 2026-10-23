//! A loader: one short-lived process that turns one source into a DuckDB file holding one table,
//! and exits. The engine loads nothing itself — it attaches what a loader wrote — so it never
//! parses a source file and never runs a connector (docs/HOST-PROTOCOL.md).
//!
//! A **file loader** is confined. DuckDB is opened first, while it can still read what it probes
//! at startup — measured, it will not start without reading both `/proc` and `/sys`, and a
//! loader must not read `/proc`, where other processes' environments are. Then it is held to one
//! thread, restricted to reading its input and writing its staging directory, and only then
//! given its threads back: started by the confined thread, every one of them is confined.
//!
//! A **network loader** — a built-in source's, such as ClickHouse — is confined the same way, to
//! its staging directory, the resolver's files and TCP to its source's port: it runs only
//! benchy's code, but parses what a remote server sends (REQ-3
//! `a_network_loader_reaches_only_its_source_and_its_staging`).
//!
//! A **connector loader** is not confined: a connector reaches whatever it fetches from. Each of
//! these two holds its source's credentials for as long as the fetch takes, and no other process
//! does (`credentials_never_cross`).

use crate::exec::ExecSource;
use crate::proto::{read_frame, write_frame};
use crate::{source, Bench};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Job {
    /// A file benchy reads natively: CSV, TSV, Parquet, JSON, NDJSON.
    File {
        name: String,
        path: PathBuf,
        stage: PathBuf,
        allow_unsandboxed: bool,
        /// REQ-14: what the operator stated about the table's shape.
        #[serde(default)]
        shape: crate::profile::Shape,
        /// The zone a zone-less timestamp in the file is in; UTC when `None`.
        #[serde(default)]
        timezone: Option<String>,
        /// R3-5: what the operator declared of the file; sniffed when nothing.
        #[serde(default)]
        declared: crate::source::Declared,
    },
    /// An Arrow stream a host attaches, validated as a connector's is, against its own schema.
    Arrow {
        name: String,
        /// The file it is in; `None` when it comes through a pipe, on descriptor [`STREAM_FD`].
        path: Option<PathBuf>,
        stage: PathBuf,
        /// The most one attachment may hold.
        max_rows: u64,
        max_bytes: u64,
        /// What the session's budget has left.
        budget_rows: u64,
        budget_bytes: u64,
        allow_unsandboxed: bool,
        /// REQ-14: what the host stated about the table's shape (protocol 3).
        #[serde(default)]
        shape: crate::profile::Shape,
        /// The pipe on [`STREAM_FD`] is the session's stream channel, which carries the host's
        /// streams back to back: this one is read to its end-of-stream marker and no further,
        /// and, refused, read on to it and dropped (`crate::ipc::Framed`).
        #[serde(default)]
        channel: bool,
    },
    /// A stream on the session's stream channel whose table was refused before it was read — a
    /// name, the count of tables — read to its end-of-stream marker and dropped, so the channel's
    /// next stream is the next attach's. Confined as an Arrow loader is.
    Drain {
        stage: PathBuf,
        /// The most it reads: what an attachment may be.
        max_bytes: u64,
        allow_unsandboxed: bool,
    },
    /// An exec connector's output, validated against the operator's declaration and narrowed
    /// before anything is written.
    Exec {
        source: Box<ExecSource>,
        run_id: String,
        budget_rows: u64,
        budget_bytes: u64,
        stage: PathBuf,
        /// REQ-14: what the operator stated about the table's shape.
        #[serde(default)]
        shape: crate::profile::Shape,
    },
    /// REQ-16: a source benchy reads itself, such as a ClickHouse table. Its loader holds that
    /// source's credentials, as a connector loader does, and validates and narrows its rows the
    /// same way; unlike one, it is confined.
    BuiltIn {
        source: Box<crate::sources::BuiltIn>,
        budget_rows: u64,
        budget_bytes: u64,
        stage: PathBuf,
        #[serde(default)]
        shape: crate::profile::Shape,
        allow_unsandboxed: bool,
    },
    /// REQ-18: the operator's helpers, made in a file of their own against empty stand-ins of
    /// the bench's tables (`crate::helpers`), which the engine attaches read-only. Confined to its
    /// staging directory, and locked down as the engine is.
    Helpers {
        helpers: Vec<crate::helpers::Def>,
        /// Each table on the bench: its name, and its columns' names and DuckDB types.
        tables: Vec<(String, Vec<(String, String)>)>,
        stage: PathBuf,
        memory_limit: String,
        allow_unsandboxed: bool,
    },
}

impl Job {
    pub fn name(&self) -> &str {
        match self {
            Job::File { name, .. } | Job::Arrow { name, .. } => name,
            Job::Exec { source, .. } => &source.name,
            Job::BuiltIn { source, .. } => source.name(),
            Job::Helpers { .. } => "helpers",
            Job::Drain { .. } => "a refused stream",
        }
    }

    pub fn stage(&self) -> &Path {
        match self {
            Job::File { stage, .. }
            | Job::Arrow { stage, .. }
            | Job::Exec { stage, .. }
            | Job::BuiltIn { stage, .. }
            | Job::Helpers { stage, .. }
            | Job::Drain { stage, .. } => stage,
        }
    }
}

/// Where a loader finds a host's streamed attachment (protocol 3): the read end of the host's
/// pipe, put there by the supervisor and the only descriptor it inherits but its three.
pub const STREAM_FD: i32 = 3;

/// What a loader reports — deliberately little. The supervisor that reads it is not confined, so
/// it acts on none of it but the fetch it reports for the session's budget: the table's name is
/// the job's, and its file is where the job put it, checked ([`output`]).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Done {
    pub ok: bool,
    /// What a connector's fetch cost its source, for the session's budget. For a host's attach,
    /// the rows it wrote, which hold the tables after it in the same request to what is left
    /// (`Room` in `crate::supervisor`); what is spent is what the supervisor measured, never what
    /// a loader parsing untrusted input says (`Supervisor::attach_all`).
    pub fetched_rows: u64,
    pub fetched_bytes: u64,
    /// Operator-facing: may name paths, hosts and accounts.
    pub error: Option<String>,
    /// Why, for a host: `E_DATA` or `E_LIMIT`. Any other code is taken as `E_DATA`.
    pub code: Option<String>,
    /// What a helpers job made (`Job::Helpers`).
    #[serde(default)]
    pub helpers: Option<crate::helpers::Built>,
    /// For a stream from the session's stream channel: it was read to its end-of-stream marker
    /// and not a byte further, loaded or refused, so the channel is at the next stream. Anything
    /// else — false, or no answer at all — loses the channel's place (`Supervisor::attach_all`).
    #[serde(default)]
    pub in_step: bool,
    /// Why it is not, for the host.
    #[serde(default)]
    pub out_of_step: Option<String>,
    /// For `E_LIMIT`, which limit (`crate::exec::Fault::limit`). Only the name: its value is the
    /// job's, which the supervisor set (`Supervisor::store_output`).
    #[serde(default)]
    pub limit: Option<String>,
}

/// Where a finished loader's table is: the bench file in the staging directory its job named — a
/// plain file with no other name, its write-ahead log merged. Asked of the file system, never of
/// the loader, since the supervisor moves it into the store with rights the loader does not have
/// (`a_loaders_output_is_checked_before_it_is_kept`).
pub fn output(job: &Job) -> Result<PathBuf, String> {
    use std::os::unix::fs::MetadataExt;
    let file = job.stage().join(Bench::FILE);
    let md = std::fs::symlink_metadata(&file).map_err(|e| format!("it wrote no table: {e}"))?;
    if !md.file_type().is_file() {
        return Err("what it wrote is not a plain file".into());
    }
    if md.nlink() != 1 {
        return Err("what it wrote has another name besides".into());
    }
    if std::fs::symlink_metadata(file.with_extension("duckdb.wal")).is_ok() {
        return Err("what it wrote has a write-ahead log not merged into it".into());
    }
    Ok(file)
}

/// Runs one job from stdin, answers on stdout, and exits.
pub fn run() -> ! {
    let done = match read_frame::<_, Job>(&mut std::io::stdin().lock()) {
        Ok(job) => crate::trace::span("loader.load", || load(job)),
        Err(e) => Done {
            error: Some(format!("the loader could not read its job: {e}")),
            ..Done::default()
        },
    };
    crate::trace::usage("loader.usage");
    let _ = write_frame(&mut std::io::stdout().lock(), &done);
    std::process::exit(0)
}

fn load(job: Job) -> Done {
    match job {
        Job::File {
            name,
            path,
            stage,
            allow_unsandboxed,
            shape,
            timezone,
            declared,
        } => file(
            &name,
            &path,
            &stage,
            allow_unsandboxed,
            shape,
            timezone.as_deref().unwrap_or(crate::canonical::UTC),
            &declared,
        ),
        Job::Arrow {
            name,
            path,
            stage,
            max_rows,
            max_bytes,
            budget_rows,
            budget_bytes,
            allow_unsandboxed,
            shape,
            channel,
        } => arrow(
            &name,
            match (path.as_deref(), channel) {
                (Some(path), _) => Input::File(path),
                (None, false) => Input::Pipe,
                (None, true) => Input::Channel,
            },
            &stage,
            (max_rows, max_bytes),
            (budget_rows, budget_bytes),
            allow_unsandboxed,
            shape,
        ),
        Job::Drain {
            stage,
            max_bytes,
            allow_unsandboxed,
        } => drain(&stage, max_bytes, allow_unsandboxed),
        Job::Exec {
            source,
            run_id,
            budget_rows,
            budget_bytes,
            stage,
            shape,
        } => {
            watch_parent();
            exec(&source, &run_id, budget_rows, budget_bytes, &stage, shape)
        }
        Job::BuiltIn {
            source,
            budget_rows,
            budget_bytes,
            stage,
            shape,
            allow_unsandboxed,
        } => builtin(
            &source,
            (budget_rows, budget_bytes),
            &stage,
            shape,
            allow_unsandboxed,
        ),
        Job::Helpers {
            helpers,
            tables,
            stage,
            memory_limit,
            allow_unsandboxed,
        } => helpers_job(&helpers, &tables, &stage, &memory_limit, allow_unsandboxed),
    }
}

/// REQ-18: make the operator's helpers in a file of their own. It reads nothing: confined to its
/// staging directory, it could not read a file if a helper asked, and the lock refuses it first.
fn helpers_job(
    helpers: &[crate::helpers::Def],
    tables: &[(String, Vec<(String, String)>)],
    stage: &Path,
    memory_limit: &str,
    allow_unsandboxed: bool,
) -> Done {
    let bench = match open_confined("helpers", stage, stage, allow_unsandboxed) {
        Ok(b) => b,
        Err(done) => return *done,
    };
    let built = crate::helpers::build(&bench, helpers, tables, memory_limit);
    match bench.finish() {
        Ok(_) => Done {
            ok: built.broken.is_empty(),
            helpers: Some(built),
            ..Done::default()
        },
        Err(e) => failed("helpers", e),
    }
}

fn failed(name: &str, why: impl std::fmt::Display) -> Done {
    Done {
        error: Some(format!("{name}: {why}")),
        ..Done::default()
    }
}

/// Open a bench in `stage`, then confine this process to reading `input` and writing beneath
/// `stage`, before anything of `input` is parsed.
fn open_confined(
    name: &str,
    input: &Path,
    stage: &Path,
    allow_unsandboxed: bool,
) -> Result<Bench, Box<Done>> {
    open_then(name, stage, allow_unsandboxed, || {
        crate::sandbox::restrict_loader(input, stage)
    })
}

/// Open a bench in `stage`, then confine this process with `restrict` while it is the only
/// thread, and give DuckDB its threads back.
fn open_then(
    name: &str,
    stage: &Path,
    allow_unsandboxed: bool,
    restrict: impl FnOnce() -> crate::sandbox::Posture,
) -> Result<Bench, Box<Done>> {
    // Asked before the restriction: it reads what the restriction then refuses.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let bench =
        crate::trace::span("bench.open", || Bench::open_in(stage)).map_err(|e| failed(name, e))?;
    // One thread while the restriction goes on: Landlock confines the thread that asks and the
    // threads it starts afterwards, never one already running.
    bench
        .connection()
        .execute_batch("SET threads = 1")
        .map_err(|e| failed(name, e))?;
    crate::trace::span("loader.alone", || {
        crate::sandbox::alone(std::time::Duration::from_secs(2))
    })
    .map_err(|e| failed(name, format!("the loader cannot be confined: {e}")))?;
    let posture = crate::trace::span("loader.restrict", restrict);
    crate::sandbox::admit(&posture, allow_unsandboxed, "loader").map_err(|e| failed(name, e))?;
    bench
        .connection()
        .execute_batch(&format!("SET threads = {threads}"))
        .map_err(|e| failed(name, e))?;
    Ok(bench)
}

fn file(
    name: &str,
    path: &Path,
    stage: &Path,
    allow_unsandboxed: bool,
    shape: crate::profile::Shape,
    zone: &str,
    declared: &source::Declared,
) -> Done {
    let mut bench = match open_confined(name, path, stage, allow_unsandboxed) {
        Ok(b) => b,
        Err(done) => return *done,
    };
    bench.declare(name, shape);
    let loaded = source::load_file_declared(
        &mut bench,
        name,
        path,
        zone,
        &declared.schema,
        declared.delimiter,
    );
    if let Err(e) = loaded {
        return failed(name, e);
    }
    match crate::trace::span("bench.finish", || bench.finish()) {
        Ok(_) => Done {
            ok: true,
            ..Done::default()
        },
        Err(e) => failed(name, e),
    }
}

/// Where a host's Arrow stream comes from: a file; the pipe on [`STREAM_FD`], the host's own for
/// this stream (protocol 3); or the session's stream channel on [`STREAM_FD`], which carries the
/// host's streams back to back.
#[derive(Clone, Copy)]
enum Input<'a> {
    File(&'a Path),
    Pipe,
    Channel,
}

/// The pipe the supervisor put on [`STREAM_FD`].
fn stream_fd() -> std::fs::File {
    use std::os::fd::FromRawFd;
    // SAFETY: the supervisor put the pipe here, and nothing else in this process owns it.
    unsafe { std::fs::File::from_raw_fd(STREAM_FD) }
}

/// A host's Arrow stream into a table, from its [`Input`]. They differ only in how the stream is
/// opened; from its first batch on it goes into the table by the same path, so the table is the
/// same (REQ-15 `the_physical_layout_says_nothing_of_where_data_came_from`).
///
/// A channel's stream is read to its end-of-stream marker and no further, and, if it is refused,
/// read on to its marker and dropped, at most an attachment's bytes more, so the next attach
/// takes the next stream (`a_refused_stream_leaves_the_channel_at_the_next_one`). Only once the
/// loader is confined: a loader that cannot be reads none of it.
fn arrow(
    name: &str,
    input: Input,
    stage: &Path,
    (max_rows, max_bytes): (u64, u64),
    budget: (u64, u64),
    allow_unsandboxed: bool,
    shape: crate::profile::Shape,
) -> Done {
    // A pipe needs no rule to be read: it is already open. Confined to its staging directory and
    // nothing else, the loader can read no file at all.
    let at = match input {
        Input::File(path) => path,
        Input::Pipe | Input::Channel => stage,
    };
    let bench = match open_confined(name, at, stage, allow_unsandboxed) {
        Ok(b) => b,
        Err(done) => return *done,
    };
    let (pipe, channel): (Option<Box<dyn std::io::Read>>, _) = match input {
        Input::File(_) => (None, None),
        Input::Pipe => (Some(Box::new(stream_fd())), None),
        Input::Channel => {
            let framed = crate::ipc::Framed::new(stream_fd());
            (Some(Box::new(framed.clone())), Some(framed))
        }
    };
    let opened = match (input, pipe) {
        (Input::File(path), _) => crate::exec::open_host_stream(path, max_bytes),
        (_, pipe) => crate::exec::open_host_pipe(pipe.expect("a pipe"), max_bytes, budget.1),
    };
    let mut done = into_table(bench, name, opened, max_rows, budget, shape);
    if let Some(channel) = channel {
        let rest = if done.ok {
            // Loaded: read to its marker, which the framing stopped at.
            if channel.ended() {
                Ok(0)
            } else {
                Err("its end-of-stream marker was not where the stream ended".to_string())
            }
        } else {
            crate::trace::span("arrow.drain", || channel.drain(max_bytes))
        };
        match rest {
            Ok(_) => done.in_step = true,
            Err(why) => done.out_of_step = Some(why),
        }
    }
    done
}

/// A stream on the channel whose table was refused before it was read (`Job::Drain`): read to
/// its end-of-stream marker, at most `max_bytes`, and dropped. Confined first, as the loader
/// that would have read it is; the bench it opens to be is left empty.
fn drain(stage: &Path, max_bytes: u64, allow_unsandboxed: bool) -> Done {
    let name = "a refused stream";
    if let Err(done) = open_confined(name, stage, stage, allow_unsandboxed) {
        return *done;
    }
    match crate::ipc::Framed::new(stream_fd()).drain(max_bytes) {
        Ok(_) => Done {
            in_step: true,
            ..Done::default()
        },
        Err(why) => Done {
            out_of_step: Some(why),
            ..Done::default()
        },
    }
}

/// [`arrow`] from the stream's schema on, in a bench already confined.
fn into_table(
    mut bench: Bench,
    name: &str,
    opened: Result<crate::exec::HostStream, crate::exec::Fault>,
    max_rows: u64,
    (budget_rows, budget_bytes): (u64, u64),
    shape: crate::profile::Shape,
) -> Done {
    let refused = |f: crate::exec::Fault| Done {
        code: Some(f.code.to_string()),
        limit: f.limit.map(str::to_string),
        ..failed(name, f.detail)
    };
    let host = match opened {
        Ok(host) => host,
        Err(f) => return refused(f),
    };
    let decls = host.decls.clone();
    // What the host stated, held to the stream's own schema before a row is written: the profile
    // would refuse it too, once the whole stream was in its table.
    if let Err(why) = stated(&shape, &decls) {
        return refused(crate::exec::Fault::host("E_DATA", why));
    }
    bench.declare(name, shape);
    let origin = source::Origin {
        what: "attach".into(),
        zone: crate::canonical::UTC,
    };
    // REQ-12: the session's budget is checked before rows are written, not after. Checked once
    // the whole stream was in its table, a refused attach paid for every insert first. A file's
    // bytes are known before a batch is read, and a pipe's are held to it as they arrive
    // (`open_host_pipe`); the rows are counted as each batch arrives, and the stream stops at the
    // batch that passes what is left (`a_host_attach_stops_at_the_budget_not_after_reading_it_all`).
    if let Some(bytes) = host.bytes.filter(|b| *b > budget_bytes) {
        return refused(crate::exec::Fault::limit(
            "session_bytes",
            format!(
                "it is {bytes} bytes, and this session has {budget_bytes} bytes of its budget left"
            ),
        ));
    }
    let over = || {
        crate::exec::Fault::limit(
            "session_rows",
            format!(
                "it holds more than {budget_rows} rows, and this session has {budget_rows} rows \
                 of its budget left"
            ),
        )
    };
    let mut rows = 0u64;
    let mut bytes = 0u64;
    let stored = (|| {
        let everything = crate::exec::Narrowing::default();
        let mut ingest = source::Ingest::open(&bench, name, &decls, &origin, &everything)?;
        let mut past_budget = false;
        let stream = crate::trace::span("arrow.stream", || {
            host.read(max_rows, &mut |b| {
                rows += b.num_rows() as u64;
                if rows > budget_rows {
                    past_budget = true;
                    return Err(over());
                }
                ingest.push(b)
            })
        });
        // The read reports what the sink refused as the stream's fault; this one is the budget's.
        if past_budget {
            return Err(over());
        }
        let (stream, read) = stream?;
        bytes = read;
        ingest.finish(&stream.schema)
    })();
    if let Err(f) = stored.and_then(|t| source::record(&mut bench, t)) {
        return refused(f);
    }
    match crate::trace::span("bench.finish", || bench.finish()) {
        Ok(_) => Done {
            ok: true,
            // The rows written, which `Supervisor::attach_all` holds the tables after this
            // one in the same request to (`Room`); never what is spent.
            fetched_rows: rows,
            // The stream's bytes: for a file, its size, which the supervisor measured itself;
            // for a pipe, what came through it, which only this process saw.
            fetched_bytes: bytes,
            ..Done::default()
        },
        Err(e) => failed(name, e),
    }
}

/// A host's stated shape against the stream's schema: every column it names is there, and the
/// time column is a date or a timestamp. That no column is named twice the host front end has
/// checked already.
fn stated(shape: &crate::profile::Shape, decls: &[crate::exec::FieldDecl]) -> Result<(), String> {
    let column = |c: &str| decls.iter().find(|d| d.name == c);
    if let Some(t) = &shape.time_column {
        match column(t) {
            None => {
                return Err(format!(
                    "it states the time column `{t}`, which the stream does not have"
                ))
            }
            Some(d) if !matches!(d.ty.as_str(), "date" | "timestamp" | "timestamptz") => {
                return Err(format!(
                    "it states the time column `{t}`, which is `{}`: a time column is a date or a \
                     timestamp",
                    d.ty
                ))
            }
            _ => {}
        }
    }
    match shape.series.iter().find(|c| column(c).is_none()) {
        Some(c) => Err(format!(
            "it states the series column `{c}`, which the stream does not have"
        )),
        None => Ok(()),
    }
}

fn exec(
    source: &ExecSource,
    run_id: &str,
    budget_rows: u64,
    budget_bytes: u64,
    stage: &Path,
    shape: crate::profile::Shape,
) -> Done {
    let mut bench = match crate::trace::span("bench.open", || Bench::open_in(stage)) {
        Ok(b) => b,
        Err(e) => return failed(&source.name, e),
    };
    bench.declare(&source.name, shape);
    let budget = crate::admission::Budget::new(budget_rows, budget_bytes);
    let done =
        match source::load_exec_within(&mut bench, source, &source.narrowing, run_id, &budget) {
            Ok(_) => match crate::trace::span("bench.finish", || bench.finish()) {
                Ok(_) => Done {
                    ok: true,
                    ..Done::default()
                },
                Err(e) => failed(&source.name, e),
            },
            Err(f) => failed(&source.name, format!("[{}] {}", f.code, f.detail)),
        };
    // What the fetch read, reported whether or not the load succeeded: the source did the work
    // either way (`a_failed_fetch_still_spends_what_it_read`).
    let (fetched_rows, fetched_bytes) = budget.spent();
    Done {
        fetched_rows,
        fetched_bytes,
        ..done
    }
}

/// A built-in source's loader, holding that source's credentials and nobody else's. It is
/// confined before its first request, so before any byte the server sends is parsed: to its
/// staging directory, the resolver's files and TCP to its source's ports (REQ-3
/// `a_network_loader_reaches_only_its_source_and_its_staging`). TLS needs nothing more: its
/// roots are in the binary. It starts no process, so the parent-death signal the supervisor sets
/// is all it needs to go with the server.
fn builtin(
    source: &crate::sources::BuiltIn,
    (budget_rows, budget_bytes): (u64, u64),
    stage: &Path,
    shape: crate::profile::Shape,
    allow_unsandboxed: bool,
) -> Done {
    let ports = source.ports();
    let mut bench = match open_then(source.name(), stage, allow_unsandboxed, || {
        crate::sandbox::restrict_network_loader(stage, &ports)
    }) {
        Ok(b) => b,
        Err(done) => return *done,
    };
    bench.declare(source.name(), shape);
    let budget = crate::admission::Budget::new(budget_rows, budget_bytes);
    let done = match source.load(&mut bench, &budget) {
        Ok(_) => match crate::trace::span("bench.finish", || bench.finish()) {
            Ok(_) => Done {
                ok: true,
                ..Done::default()
            },
            Err(e) => failed(source.name(), e),
        },
        Err(f) => failed(source.name(), format!("[{}] {}", f.code, f.detail)),
    };
    // What the fetch read, failed or not, as a connector loader reports it.
    let (fetched_rows, fetched_bytes) = budget.spent();
    Done {
        fetched_rows,
        fetched_bytes,
        ..done
    }
}

/// A connector loader outlives nothing. When its supervisor is gone — re-parented, `getppid`
/// changes — it stops the connector with its whole process group and exits. A file loader needs
/// no such thread: it has no children, and its parent-death signal is enough.
fn watch_parent() {
    let parent = unsafe { libc::getppid() };
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if unsafe { libc::getppid() } != parent {
            crate::exec::kill_running_connector();
            unsafe { libc::_exit(0) };
        }
    });
}
