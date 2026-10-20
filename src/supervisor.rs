//! REQ-8: the half of containment nothing in-process can provide — and the one process that
//! puts data on the bench.
//!
//! A runaway query loses the query, not the bench. The in-process interrupt handles execution;
//! this handles **planning**, which the interrupt provably cannot reach — a nested-`MAP` blowup
//! was measured at 7.8 s against a 2 s interrupt and 402 MB against a 512 MB limit, because
//! planning never gets to the point where the interrupt flag is read. So the engine is a separate
//! process, and past its hard deadline it is killed and a new one takes its place. The agent loses
//! that query and its scratch tables, and is told so. It does not lose the bench.
//!
//! Memory is bounded the same way, for the same reason. DuckDB's `memory_limit` bounds its buffer
//! manager, and nothing it does while planning: nested `MAP`s took an engine with a 64 MB budget
//! to 229 MB, `repeat('x', 600000000)` one with 256 MB to 1,185 MB, and an ENUM type of
//! 3,000,000 values one with 1 GB to 1,065 MB, still holding 698 MB after it while
//! `duckdb_memory()` said 0. In a container one engine's growth is every session's OOM kill. So
//! while the engine answers a request, and once more as it answers, the supervisor reads its
//! resident memory, and past the operator's ceiling it is killed and rebuilt, as past the hard
//! deadline (`Memory`, `Supervisor::receive`).
//!
//! The engine starts sealed and empty and never loads anything. Each source is turned into a
//! DuckDB file by a loader (`crate::loader`), kept in the session's `store/`, and attached from
//! there; a rebuilt engine attaches the same files again, so a rebuild neither re-reads a file
//! nor asks a source twice (docs/HOST-PROTOCOL.md).

use crate::loader::{Done, Job};
use crate::proto::{read_frame, write_frame, Reply, Request};
use std::io::{BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The engine running now, and the loader running now, for `kill_engine_now`.
static ENGINE: AtomicI32 = AtomicI32::new(0);
static LOADER: AtomicI32 = AtomicI32::new(0);
/// Whether the loader running now may be running a connector.
static LOADER_RUNS_CONNECTORS: AtomicBool = AtomicBool::new(false);

/// Kill the running engine and loader without waiting for anything — for a server that is being
/// stopped by a signal and will not run its destructors.
pub fn kill_engine_now() {
    let connectors = LOADER_RUNS_CONNECTORS.load(Ordering::SeqCst);
    for (slot, connectors) in [(&LOADER, connectors), (&ENGINE, false)] {
        let pid = slot.swap(0, Ordering::SeqCst);
        if pid > 0 {
            kill_with_connectors(pid, connectors);
        }
    }
}

/// Kill `parent` — and first, when it is a process that runs connectors, every connector it is
/// running. A connector leads a process group of its own, so killing the group reaches what it
/// started too; once its parent is gone nothing else would, and a connector's parent-death signal
/// reaches only the connector. Connectors are found through `/proc`; elsewhere their parent-death
/// signal is all there is.
///
/// Finding them reads the `stat` of every process on the machine, so it costs what the machine
/// runs, not what the session does: about 400 processes here (docs/history/PERFORMANCE-rounds.md,
/// round 1 item 8). Only
/// an exec source's loader starts connectors — the engine runs none, and neither does a file,
/// Arrow or built-in loader — so only that one is looked through. `/proc/<pid>/task/*/children`
/// lists a loader's children directly where the kernel is built with CONFIG_PROC_CHILDREN, as
/// 6.8.0-138-generic is ([`children`], which the loader memory watch uses); it is not used here
/// yet, so this kill also works on a kernel without it.
fn kill_with_connectors(parent: i32, runs_connectors: bool) {
    #[cfg(target_os = "linux")]
    for entry in runs_connectors
        .then(|| std::fs::read_dir("/proc"))
        .into_iter()
        .flatten()
        .flatten()
        .flatten()
    {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // `pid (comm) state ppid pgrp …` — the command name may contain anything.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let f: Vec<&str> = rest.split_whitespace().collect();
        let field = |i: usize| f.get(i).and_then(|v| v.parse::<i32>().ok());
        if field(1) == Some(parent) && field(2) == Some(pid) {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = runs_connectors;
    unsafe { libc::kill(parent, libc::SIGKILL) };
}

/// What the engine reports once it is open.
#[derive(Debug, Clone)]
pub struct Opened {
    /// The engine's own version, so nothing upstream has to write one down. REQ-11
    /// `version_not_hardcoded`.
    pub engine_version: String,
    /// How Landlock confines the engine.
    pub posture: crate::sandbox::Posture,
}

/// A size as DuckDB writes one, `1GB` or `512 MiB`, in bytes: KB, MB, GB and TB are powers of
/// 1000, as DuckDB reads them (`1GB` is the 953.6 MiB its errors report), and KiB, MiB, GiB and
/// TiB powers of 1024. `None` for anything else, which is refused rather than guessed at.
pub fn bytes_of(size: &str) -> Option<u64> {
    let s = size.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: f64 = n.parse().ok()?;
    let unit = match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "byte" | "bytes" => 1u64,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return None,
    };
    let bytes = n * unit as f64;
    (bytes.is_finite() && bytes >= 1.0 && bytes < u64::MAX as f64).then_some(bytes as u64)
}

/// DuckDB's memory budget in the engine and the ceiling on the engine's resident memory, both in
/// bytes: what the operator set, checked.
///
/// The ceiling sits above the budget, because an engine working inside its budget is resident
/// past it: on DuckDB 1.5.5 at a 1 GB budget, a hash aggregate of 20,000,000 groups held 1,248 MB
/// and a `string_agg` 1,260 MB, each with 953.6 MiB of the budget in use. Unset, the ceiling is
/// the budget and half again, 1.5 GB at the default 1 GB, and at least 256 MiB over it for a
/// small budget, whose engine is mostly DuckDB itself (about 40 MB idle). Below the budget it is
/// refused: every query DuckDB would turn away cleanly as out of memory would cost the agent its
/// scratch instead.
#[derive(Debug, Clone, Copy)]
pub struct Memory {
    pub budget: u64,
    pub ceiling: u64,
}

impl Memory {
    pub fn new(budget: &str, ceiling: Option<&str>) -> Result<Memory, String> {
        let budget = bytes_of(budget).ok_or_else(|| {
            format!("--memory `{budget}` is not a size: give one like 1GB or 512MiB")
        })?;
        let ceiling = match ceiling {
            None => budget.saturating_add((budget / 2).max(256 << 20)),
            Some(c) => {
                let bytes = bytes_of(c).ok_or_else(|| {
                    format!("--memory-ceiling `{c}` is not a size: give one like 2GB or 1536MiB")
                })?;
                if bytes < budget {
                    return Err(format!(
                        "--memory-ceiling {c} is below --memory, DuckDB's own budget: every query \
                         DuckDB would refuse as out of memory would kill the engine instead. Give \
                         at least the budget, and better half again over it."
                    ));
                }
                bytes
            }
        };
        Ok(Memory { budget, ceiling })
    }
}

/// How often the supervisor reads the engine's resident memory while the engine answers a
/// request.
///
/// Only then, and once more as it answers (`Supervisor::receive`): between requests the engine
/// runs nothing and nothing is read, so an idle session costs nothing: 0 ms of the server's CPU over 10 s idle. A read of `statm` costs 5 µs (`status`,
/// 13 µs), and with the wake-ups the watch cost the server 20 ms of CPU over a 10 s query, where
/// it had cost 0: 0.2% of a core while a query runs. The interval is set by how far past the
/// ceiling the engine may get before it is killed: the fastest growth measured was 160 MB in
/// 50 ms, in a hash aggregate, so about that much.
pub const MEMORY_EVERY: Duration = Duration::from_millis(50);

/// The resident memory of process `pid`, in bytes: VmRSS, read as the second field of `statm`,
/// which is the same count in pages and cheaper to read and parse than `status`.
///
/// Resident, not the address space an `RLIMIT_AS` would bound: DuckDB reserves far more than it
/// touches. An idle engine had 386 MB of address space and 44 MB resident; after a hash
/// aggregate, 1,186 MB and 412 MB.
#[cfg(target_os = "linux")]
fn resident(pid: u32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some(pages * u64::try_from(page).ok()?)
}

/// Elsewhere there is no `/proc`, and only the deadlines bound the engine.
#[cfg(not(target_os = "linux"))]
fn resident(_pid: u32) -> Option<u64> {
    None
}

/// Why the memory ceiling is not enforced on this platform, or `None` where it is. The ceiling is
/// enforced by reading the engine's resident memory ([`resident`]), and only Linux has the `/proc`
/// it is read from; elsewhere the ceiling would be reported and never applied — measured on macOS
/// with a 128 MB ceiling, an engine reached 243 MB and was not killed. So `hello` and `doctor` say
/// so instead of reporting it (REQ-7 `a_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
pub const MEMORY_CEILING_UNENFORCED: Option<&str> = if cfg!(target_os = "linux") {
    None
} else {
    Some(
        "the engine's resident memory is read from /proc, which this platform does not have; \
         only the deadlines bound the engine here",
    )
};

/// The most resident memory one loader may hold, in bytes, unless the operator says otherwise
/// (`--loader-memory-ceiling`, `[loader] memory_ceiling`): 1 GB.
///
/// Well above what a load is measured to need (docs/history/PERFORMANCE-rounds.md, "R6-L1. A
/// memory bound for loaders"): the harness's 10,000,000-row loads peak at 94–112 MB for ClickHouse,
/// Parquet, Arrow and a connector, and at 351–410 MB for CSV and NDJSON, whose stage is held to 256
/// MiB; a 10,000,000-row Parquet file in one row group at 118 MB; a CSV of 300 `DOUBLE` columns,
/// read on one thread under that cap, at 343 MB, and an NDJSON of 2,000 keys an object at 549–614
/// MB, bounded by a row group of its table
/// (`a_wide_json_file_is_read_on_one_thread_within_its_vector`). What passes it is a connector,
/// which nothing else bounds: one that held 800 MB before it wrote a row.
pub const LOADER_MEMORY_CEILING: u64 = 1_000_000_000;

/// The loader memory ceiling the operator gave, `--loader-memory-ceiling` or `[loader]
/// memory_ceiling`, in bytes; [`LOADER_MEMORY_CEILING`] when neither.
pub fn loader_memory_ceiling(given: Option<&str>) -> Result<u64, String> {
    match given {
        None => Ok(LOADER_MEMORY_CEILING),
        Some(c) => bytes_of(c).ok_or_else(|| {
            format!("--loader-memory-ceiling `{c}` is not a size: give one like 1GB or 768MiB")
        }),
    }
}

/// Why the loader memory ceiling is not enforced here, or `None` where it is: as for the engine's
/// ([`MEMORY_CEILING_UNENFORCED`]), a loader's resident memory is read from `/proc`.
pub const LOADER_MEMORY_CEILING_UNENFORCED: Option<&str> = if cfg!(target_os = "linux") {
    None
} else {
    Some(
        "a loader's resident memory is read from /proc, which this platform does not have; \
         only the load timeout and the row and byte limits bound a loader here",
    )
};

/// The processes `pid` started that are still running: every thread's `children`, as the kernel
/// keeps them. `None` where the kernel keeps no such list (built without `CONFIG_PROC_CHILDREN`;
/// 6.8.0-138-generic has it).
#[cfg(target_os = "linux")]
fn children(pid: u32) -> Option<Vec<u32>> {
    static LISTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*LISTED.get_or_init(|| Path::new("/proc/thread-self/children").exists()) {
        return None;
    }
    let mut kids = Vec::new();
    // A process that has just ended has no tasks, and a thread that has just ended no file:
    // either has no children to count.
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
            kids.extend(
                text.split_whitespace()
                    .filter_map(|p| p.parse::<u32>().ok()),
            );
        }
    }
    Some(kids)
}

/// Every process on the machine by its parent, read from each one's `stat`: where the kernel
/// keeps no list of a process's children, the only way to find them. About 3.8 ms for 270
/// processes, so it is done once a second at most ([`Watch`]).
#[cfg(target_os = "linux")]
fn parents() -> std::collections::HashMap<u32, Vec<u32>> {
    let mut by_parent: std::collections::HashMap<u32, Vec<u32>> = Default::default();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // `pid (comm) state ppid …` — the command name may contain anything.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|p| p.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            by_parent.entry(ppid).or_default().push(pid);
        }
    }
    by_parent
}

/// What one loader holds, read every [`MEMORY_EVERY`] while it runs: its own resident memory,
/// and, for a loader that runs a connector, every process under it too: the connector and
/// whatever the connector started (`a_connectors_memory_counts_against_its_loaders_ceiling`).
///
/// A connector is found as the loader's child, through the kernel's list of each thread's
/// children, and the processes it started the same way: the watch cost the server 15 ms of CPU
/// over exec-10m's 3.3 s load, a wake-up and a walk every 50 ms, where finding them through every
/// process's `stat` costs 3.8 ms a time (docs/PERFORMANCE.md R6-L1). Where the kernel keeps no such list, they are found that way, once a second, and the
/// loader alone is read in between. Each process's resident memory is counted whole, so pages two
/// of them share (a connector's forked workers) count once in each: the ceiling errs towards
/// stopping a load, never towards missing one.
struct Watch {
    pid: u32,
    tree: bool,
    reads: u32,
}

impl Watch {
    fn new(pid: u32, tree: bool) -> Watch {
        Watch {
            pid,
            tree,
            reads: 0,
        }
    }

    /// What the loader, and what runs under it, holds now; `None` where it cannot be read.
    #[cfg(target_os = "linux")]
    fn held(&mut self) -> Option<u64> {
        self.reads += 1;
        let mut held = resident(self.pid)?;
        if !self.tree {
            return Some(held);
        }
        let mut scanned = None;
        let mut todo = match children(self.pid) {
            Some(kids) => kids,
            // Once a second: the loader alone in between.
            None if self.reads % 20 == 1 => {
                let all = parents();
                let kids = all.get(&self.pid).cloned().unwrap_or_default();
                scanned = Some(all);
                kids
            }
            None => return Some(held),
        };
        // Bounded, so a process table that changes under the walk cannot keep it going.
        let mut left = 4096;
        while let Some(pid) = todo.pop() {
            left -= 1;
            if left == 0 {
                break;
            }
            held += resident(pid).unwrap_or(0);
            match &scanned {
                Some(all) => todo.extend(all.get(&pid).into_iter().flatten()),
                None => todo.extend(children(pid).unwrap_or_default()),
            }
        }
        Some(held)
    }

    #[cfg(not(target_os = "linux"))]
    fn held(&mut self) -> Option<u64> {
        let _ = (self.pid, self.tree, self.reads);
        None
    }
}

/// A load stopped at the loader memory ceiling, for the operator's log or a host: `E_LIMIT`, a
/// limit of the session's, with what to do about it. Nothing of the load is attached.
fn loader_swollen(name: &str, held: u64, ceiling: u64, connector: bool) -> LoadError {
    let who = if connector {
        "the loader and the connector it ran"
    } else {
        "the loader"
    };
    load_error(
        "E_LIMIT",
        format!(
            "loading `{name}` passed the loader memory ceiling: {who} held {} against {}, and \
             it was stopped; nothing of it was attached. A source with very wide rows or deeply \
             nested values, or a connector that holds its data before it writes it, can grow a \
             loader without bound. Narrow the source (fewer columns, a shorter time window, \
             smaller documents or row groups), or raise --loader-memory-ceiling ([loader] \
             memory_ceiling in the config).",
            size(held),
            size(ceiling)
        ),
    )
}

/// Bytes as a person reads them: `1.5 GB`, `229 MB`.
fn size(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{} MB", bytes / 1_000_000)
    }
}

/// How a session's engine is run.
#[derive(Debug, Clone)]
pub struct EngineSpec {
    /// The session's own directory. The store, each engine's workspace and every loader's staging
    /// directory are made inside it, so ending the session is one removal.
    pub session: PathBuf,
    pub memory_limit: String,
    /// How long one source may take to load before it counts as failed. Loading is host-driven,
    /// and a slow source is not an attack, so this is generous — but it is finite.
    pub load_timeout: Duration,
    /// Serve even where Landlock cannot confine the engine or a file loader.
    pub allow_unsandboxed: bool,
    /// REQ-12: the most the session may fetch from its sources, rows and bytes, across every load
    /// (`session_rows`, `session_bytes`); and, for a host's attach, the most one attachment may
    /// hold and the tables at once.
    pub limits: crate::admission::HostLimits,
}

/// Something to put on the bench.
pub enum Source {
    /// A file benchy reads natively, as table `name`, its zone-less timestamps in `timezone`
    /// (UTC when `None`).
    File {
        name: String,
        path: PathBuf,
        timezone: Option<String>,
        /// R3-5: what the operator declared of it; sniffed when nothing.
        declared: crate::source::Declared,
    },
    /// An Arrow stream a host attaches, as table `name`.
    Arrow { name: String, path: PathBuf },
    /// An exec connector's output.
    Exec(Box<crate::exec::ExecSource>),
    /// REQ-16: a source benchy reads itself, such as a ClickHouse table.
    BuiltIn(Box<crate::sources::BuiltIn>),
}

/// Where a host's attachment comes from: a file it names, or the read end of a pipe it writes the
/// stream into as it fetches (protocol 3), which no file holds on either side; or the next stream
/// on the session's stream channel (protocol 3, `--stream-channel`).
#[derive(Debug)]
pub enum Feed {
    Path(PathBuf),
    Stream(OwnedFd),
    Channel,
    /// A table from the channel refused before its stream is read — its name, say. The stream
    /// is read to its end and dropped all the same, so the next table takes the next stream, and
    /// the table is answered with this.
    ChannelRefused(LoadError),
}

/// Why something did not go on the bench, with the code a host acts on: `E_DATA` for what was
/// sent, `E_LIMIT` for what the session allows, `E_DEADLINE` for a load that took too long,
/// `E_REBUILT` or `E_UNAVAILABLE` when the engine was lost, `E_INTERNAL` for benchy's own side.
#[derive(Debug)]
pub struct LoadError {
    pub code: &'static str,
    /// For the operator, or a host: it can name the session's files.
    pub message: String,
    /// Set where this table's stream lost the stream channel its place, and why: the channel is
    /// closed from here on.
    pub closed_channel: Option<String>,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Why the engine did not attach a table: the code a host acts on, and the reason.
type Refused = (&'static str, String);

fn load_error(code: &'static str, message: impl Into<String>) -> LoadError {
    LoadError {
        code,
        message: message.into(),
        closed_channel: None,
    }
}

/// A table on the bench, and where the store keeps it.
#[derive(Debug, Clone)]
pub struct Attached {
    pub name: String,
    pub rows: u64,
    /// Name and DuckDB type, as the engine reported them.
    pub columns: Vec<(String, String)>,
    stored: PathBuf,
}

/// The most tables a session holds at once. Each is a catalog on the agent's search path and a
/// file in the store; past a few dozen, a bench is a warehouse.
pub const MAX_ATTACHMENTS: usize = 64;

/// Why an exchange produced no reply.
enum Lost {
    Died(String),
    Wedged,
    /// The engine held `held` bytes, past the `ceiling`, and is killed for it.
    Swollen {
        held: u64,
        ceiling: u64,
    },
    /// The engine answered, and said why it must not go on (`Reply::rebuild`).
    Spoiled(String),
}

impl Lost {
    fn why(&self) -> String {
        match self {
            Lost::Died(e) => format!("the engine stopped responding: {e}"),
            Lost::Wedged => "the query passed the hard deadline and could not be interrupted — \
                             it was still being planned, where no in-process timer can reach it"
                .to_string(),
            Lost::Swollen { held, ceiling } => format!(
                "the engine passed its memory ceiling, holding {} against {}, and was stopped — \
                 planning a query, a very long string or a large ENUM uses memory that DuckDB's \
                 own budget does not count",
                size(*held),
                size(*ceiling)
            ),
            Lost::Spoiled(why) => format!(
                "{why}. DuckDB holds an ENUM's values outside its memory budget, so the engine \
                 was stopped rather than hold them for the rest of the session"
            ),
        }
    }

    /// What the agent can do differently. Running out of DuckDB's own budget says much the same
    /// (`running_out_of_memory_says_how_to_use_less`).
    fn advice(&self) -> &'static str {
        match self {
            Lost::Spoiled(_) => {
                "Keep no ENUM: cast to VARCHAR in anything you create, and use an ENUM only in a \
                 query."
            }
            Lost::Swollen { .. } => {
                "Use less memory: aggregate or filter before sorting or joining, add a LIMIT, \
                 build smaller strings and nested values, or ask the operator to start benchy \
                 with a larger --memory-ceiling."
            }
            _ => {
                "Narrow the query: a deeply nested type expression or a very wide computed \
                 projection is the usual cause."
            }
        }
    }
}

/// Past this many rebuilds failing in a row, the supervisor stops trying and says so, rather
/// than paying for a doomed rebuild on every request.
const MAX_FAILED_REBUILDS: u32 = 2;

pub struct Supervisor {
    exe: std::path::PathBuf,
    spec: EngineSpec,
    child: Option<Child>,
    /// Where the current engine attaches from. After a hard kill the engine gets no chance to
    /// clean up, so the supervisor removes it.
    workspace_dir: Option<PathBuf>,
    /// Engines started so far; each gets a workspace of its own.
    generation: u32,
    /// Where loaded files are kept for the life of the session, out of the engine's reach.
    store: PathBuf,
    /// What is on the bench, in the order it arrived.
    attachments: Vec<Attached>,
    /// Loads and links made so far, for names that are never reused.
    counter: u64,
    /// REQ-12: what the session may fetch from its sources, across every load.
    budget: crate::admission::Budget,
    /// The next result id. Carried across rebuilds, so no id is ever handed out twice and an old
    /// one can only fail to resolve, never resolve to a different answer.
    next_result_id: u64,
    /// What every result id this session issues starts with: random, so no two sessions issue
    /// the same id — a host whose conversations outlive sessions could otherwise resolve an old
    /// id to another session's answer. It names nothing: an id resolves only in the engine of
    /// the session that issued it (`two_sessions_never_issue_the_same_result_id`).
    result_prefix: String,
    failed_rebuilds: u32,
    /// REQ-14: what the operator stated about each source's shape, by name.
    shapes: std::collections::BTreeMap<String, crate::profile::Shape>,
    /// Tables the session was to have and could not load, by name: `describe` lists them, so the
    /// agent does not go looking, and says nothing else of them.
    unavailable: Vec<String>,
    /// How many times a runaway has cost us the engine. Surfaced so an operator can see that a
    /// workload is repeatedly hitting the wall rather than wondering why scratch keeps vanishing.
    pub kills: u32,
    /// The first engine's `Open`, sent and not yet answered: the first loader runs meanwhile.
    opening: Option<Sent>,
    /// What the first engine said when it opened, once it has.
    opened: Option<Result<Opened, String>>,
    /// REQ-18: the operator's helpers, as the config gives them.
    helpers: Vec<crate::helpers::Def>,
    /// The last helpers file a job made, in the store, and every helper as it showed them: what
    /// a rebuilt engine is given first, before its tables.
    helpers_built: Option<(PathBuf, Vec<crate::helpers::Shown>)>,
    /// REQ-18 `a_table_a_helper_reads_stays_taken_after_a_detach`: every table a helper has read
    /// this session, by every build, folded. Kept here rather than in the engine, so a rebuilt
    /// engine takes them as the one before did
    /// (`the_tables_helpers_read_stay_taken_across_a_rebuild`), and never shrunk: a detach takes
    /// the table off the bench, not out of the helper's SQL.
    helper_reads: std::collections::BTreeSet<String>,
    /// What the engine's texts call the agent's tools: a host's names, from its `hello`, sent
    /// with every request that makes text (`crate::tools`).
    tool_names: crate::tools::Names,
    /// The session's stream channel, where `--stream-channel` names one: the host's streams, back
    /// to back, each read by the loader of the table it is for. This process only hands each
    /// loader a copy and never reads it.
    channel: Option<OwnedFd>,
    /// Why the channel is closed, once it has lost its place.
    channel_closed: Option<String>,
}

/// What the engine's pipes come back as once a request has been written and its reply read.
type Exchanged = (
    Reply,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
);

/// A request on its way to the engine, its reply still to come.
struct Sent {
    reply: mpsc::Receiver<std::io::Result<Exchanged>>,
    reader: std::thread::JoinHandle<()>,
}

impl Supervisor {
    /// Start the session's first engine, and return without waiting for it to open.
    ///
    /// The engine's `Open` — a process start, DuckDB's open, the seal, the confinement — and the
    /// first loader need nothing from each other until the attach, so the first loader runs while
    /// the engine opens (docs/history/PERFORMANCE-rounds.md, round 1 item 6).
    /// [`Supervisor::opened`] waits for the
    /// answer; the first attach waits for it too, and nothing is attached to an engine that did
    /// not open. The two are separate processes, so the engine still seals and confines with no
    /// thread but its own (`sandbox::alone`).
    /// `the_first_loader_starts_while_the_engine_opens` is the test.
    pub fn start(spec: EngineSpec) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let store = spec.session.join("store");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&store)
            .map_err(|e| format!("could not create {}: {e}", store.display()))?;
        let budget =
            crate::admission::Budget::new(spec.limits.session_rows, spec.limits.session_bytes);
        let mut me = Self {
            exe,
            spec,
            child: None,
            workspace_dir: None,
            generation: 0,
            store,
            attachments: Vec::new(),
            counter: 0,
            budget,
            next_result_id: 1,
            result_prefix: result_prefix(),
            failed_rebuilds: 0,
            shapes: Default::default(),
            unavailable: Vec::new(),
            kills: 0,
            opening: None,
            opened: None,
            helpers: Vec::new(),
            helpers_built: None,
            helper_reads: Default::default(),
            tool_names: crate::tools::Names::new(),
            channel: None,
            channel_closed: None,
        };
        me.opening = Some(me.spawn_engine()?);
        Ok(me)
    }

    /// What the first engine said when it opened: waits for it the first time, and is the same
    /// answer after. An `Err` means the session has no engine, and is not retried.
    pub fn opened(&mut self) -> Result<Opened, String> {
        if let Some(sent) = self.opening.take() {
            let answer = self.finish_open(sent);
            self.opened = Some(answer);
        }
        self.opened
            .clone()
            .unwrap_or_else(|| Err("the engine was never started".into()))
    }

    /// Start an engine, sealed and empty. A failure is reported, never retried from in here.
    fn spawn_and_open(&mut self) -> Result<Opened, String> {
        let sent = self.spawn_engine()?;
        self.finish_open(sent)
    }

    /// Start an engine and send it `Open`, without waiting for the answer.
    fn spawn_engine(&mut self) -> Result<Sent, String> {
        self.generation += 1;
        let workspace = self.spec.session.join(format!("ws-{}", self.generation));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&workspace)
            .map_err(|e| format!("could not create {}: {e}", workspace.display()))?;
        self.workspace_dir = Some(workspace.clone());
        let mut cmd = Command::new(&self.exe);
        // An environment benchy builds: the engine needs nothing from the server's, and whatever
        // a client passed the server — a credential among it — must not reach the process that
        // runs agent SQL (`the_engine_inherits_no_environment_and_no_descriptor`).
        cmd.arg("--engine");
        if crate::trace::on() {
            cmd.arg(crate::trace::FLAG);
        }
        cmd.env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                crate::exec::unblock_signals();
                close_inherited_descriptors();
                Ok(())
            });
        }
        crate::trace::mark("engine.spawn", &[]);
        let child = cmd
            .spawn()
            .map_err(|e| format!("could not start the engine: {e}"))?;
        ENGINE.store(child.id() as i32, Ordering::SeqCst);
        self.child = Some(child);
        let req = Request::Open {
            workspace: workspace.display().to_string(),
            memory_limit: self.spec.memory_limit.clone(),
            threads: self.spec.limits.threads as usize,
            allow_unsandboxed: self.spec.allow_unsandboxed,
            first_result_id: self.next_result_id,
            result_prefix: self.result_prefix.clone(),
        };
        match self.send(&req) {
            Ok(sent) => Ok(sent),
            Err(lost) => {
                self.stop_engine();
                Err(format!("the engine stopped while opening: {}", lost.why()))
            }
        }
    }

    /// Wait for the answer to an engine's `Open`. A failure stops the engine.
    fn finish_open(&mut self, sent: Sent) -> Result<Opened, String> {
        match crate::trace::span("engine.open", || {
            self.receive(sent, Duration::from_secs(60))
        }) {
            Ok(reply) if reply.ok => Ok(Opened {
                engine_version: reply.body,
                posture: reply.posture.unwrap_or_else(|| {
                    crate::sandbox::Posture::Unavailable("the engine did not say".into())
                }),
            }),
            Ok(reply) => {
                self.stop_engine();
                Err(reply.body)
            }
            Err(Lost::Died(e)) => {
                self.stop_engine();
                Err(format!("the engine stopped while opening: {e}"))
            }
            Err(Lost::Wedged) => {
                self.stop_engine();
                Err("the engine did not open within 60s".into())
            }
            Err(lost @ (Lost::Swollen { .. } | Lost::Spoiled(_))) => {
                self.stop_engine();
                Err(format!("the engine stopped while opening: {}", lost.why()))
            }
        }
    }

    /// Put `source` on the bench: a loader turns it into a file, the store keeps the file, and
    /// the engine attaches it. Replaces a table of the same name.
    pub fn load(&mut self, source: Source) -> Result<Attached, LoadError> {
        let (_, loaded) = self
            .load_all(vec![source])
            .pop()
            .expect("one source in, one answer out");
        loaded
    }

    /// Put every source on the bench, each through a loader of its own, one after another; then
    /// attach every table that loaded, in one request to the engine. One answer per source, by
    /// name, in order: one that fails costs the others nothing.
    ///
    /// One request, because the check the engine makes of an attached file lists every function
    /// DuckDB has, and is paid once per request rather than once per table
    /// (`Workspace::attach_all`, docs/history/PERFORMANCE-rounds.md round 1 item 5). And the first
    /// engine opens while every loader runs, not only the first (`Supervisor::start`).
    /// `the_tables_a_session_starts_with_are_attached_in_one_request` is the test.
    pub fn load_all(&mut self, sources: Vec<Source>) -> Vec<(String, Result<Attached, LoadError>)> {
        let fetched: Vec<(String, Result<PathBuf, LoadError>)> = sources
            .into_iter()
            .map(|source| {
                let name = match &source {
                    Source::File { name, .. } => name.clone(),
                    Source::Exec(e) => e.name.clone(),
                    Source::Arrow { name, .. } => name.clone(),
                    Source::BuiltIn(b) => b.name().to_string(),
                };
                let stored = crate::trace::span("load", || self.fetch(source));
                (name, stored)
            })
            .collect();
        let ready: Vec<(String, PathBuf)> = fetched
            .iter()
            .filter_map(|(name, r)| r.as_ref().ok().map(|p| (name.clone(), p.clone())))
            .collect();
        let mut kept = self.keep_all(&ready).into_iter();
        fetched
            .into_iter()
            .map(|(name, r)| {
                let loaded = r.and_then(|_| kept.next().expect("one answer per table kept"));
                (name, loaded)
            })
            .collect()
    }

    /// Run `source`'s loader, and move what it wrote into the store. Returns where it is kept.
    fn fetch(&mut self, source: Source) -> Result<PathBuf, LoadError> {
        self.counter += 1;
        let stage = self.spec.session.join(format!("stage-{}", self.counter));
        let job = match source {
            Source::File {
                name,
                path,
                timezone,
                declared,
            } => Job::File {
                shape: self.shapes.get(&name).cloned().unwrap_or_default(),
                name,
                path,
                stage: stage.clone(),
                allow_unsandboxed: self.spec.allow_unsandboxed,
                timezone,
                declared,
            },
            Source::Arrow { name, path } => {
                let (budget_rows, budget_bytes) = self.budget.remaining();
                Job::Arrow {
                    name,
                    path: Some(path),
                    stage: stage.clone(),
                    max_rows: self.spec.limits.attach_rows,
                    max_bytes: self.spec.limits.attach_bytes,
                    shape: Default::default(),
                    budget_rows,
                    budget_bytes,
                    allow_unsandboxed: self.spec.allow_unsandboxed,
                    channel: false,
                }
            }
            Source::Exec(source) => {
                let (rows, bytes) = self.budget.remaining();
                Job::Exec {
                    shape: self.shapes.get(&source.name).cloned().unwrap_or_default(),
                    source,
                    run_id: format!("{:x}{:x}", std::process::id(), self.counter),
                    budget_rows: rows,
                    budget_bytes: bytes,
                    stage: stage.clone(),
                }
            }
            Source::BuiltIn(source) => {
                let (rows, bytes) = self.budget.remaining();
                Job::BuiltIn {
                    shape: self.shapes.get(source.name()).cloned().unwrap_or_default(),
                    source,
                    budget_rows: rows,
                    budget_bytes: bytes,
                    stage: stage.clone(),
                    allow_unsandboxed: self.spec.allow_unsandboxed,
                }
            }
        };
        let stored = crate::trace::span("loader.run", || self.run_loader(&job))
            .and_then(|done| self.store_output(&job, done));
        let _ = std::fs::remove_dir_all(&stage);
        stored
    }

    /// Move what a loader wrote into the store. The name is the job's and the file is where the
    /// job put it, checked: a loader's report is not trusted with either.
    fn store_output(&mut self, job: &Job, done: Done) -> Result<PathBuf, LoadError> {
        let name = job.name().to_string();
        // A fetch cost its source whatever happens next, failed or not, and only the loader
        // that ran it can say what it cost. A host's attach costs nothing until it
        // succeeds, and then what the supervisor measured (`attach`).
        if let Job::Exec { .. } | Job::BuiltIn { .. } = job {
            self.budget.spend(done.fetched_rows, done.fetched_bytes);
        }
        if !done.ok {
            let code = match done.code.as_deref() {
                Some("E_LIMIT") => "E_LIMIT",
                _ => "E_DATA",
            };
            return Err(load_error(
                code,
                done.error.unwrap_or_else(|| "the loader failed".into()),
            ));
        }
        let file = crate::loader::output(job)
            .map_err(|e| load_error("E_DATA", format!("could not keep `{name}`: {e}")))?;
        self.counter += 1;
        // Numbered, not named: a table's name is an operator's or a file's, never part of a path.
        let stored = self.store.join(format!("{}.duckdb", self.counter));
        crate::trace::span("store.move", || move_file(&file, &stored))
            .map_err(|e| load_error("E_INTERNAL", format!("could not keep `{name}`: {e}")))?;
        Ok(stored)
    }

    /// Attach stored tables, by name, in one request. One answer per table, in order; a table
    /// that is not attached leaves the store.
    fn keep_all(&mut self, tables: &[(String, PathBuf)]) -> Vec<Result<Attached, LoadError>> {
        if tables.is_empty() {
            return Vec::new();
        }
        // Nothing attached: every table leaves the store, each with why.
        let none = |code: &'static str, why: &dyn Fn(&str) -> String| {
            tables
                .iter()
                .map(|(name, stored)| {
                    let _ = std::fs::remove_file(stored);
                    Err(load_error(code, why(name)))
                })
                .collect::<Vec<_>>()
        };
        // The first engine may still be opening: nothing is attached until it has, and if it did
        // not, the session has no engine and none is started in its place.
        if let Err(e) = self.opened() {
            return none("E_UNAVAILABLE", &|_| e.clone());
        }
        // An engine lost earlier is rebuilt now, so there is one to attach to.
        if self.child.is_none() {
            if let Err(e) = self.rebuild() {
                return none("E_UNAVAILABLE", &|_| e.clone());
            }
        }
        let each = match crate::trace::span("engine.attach", || self.attach_on_engine(tables)) {
            Ok(each) => each,
            Err(lost) => {
                let lost = lost.why();
                let rebuilt = self.rebuild();
                let why = |name: &str| {
                    format!("{lost} while `{name}` was being attached, so it was not attached")
                };
                return match rebuilt {
                    Ok(()) => none("E_REBUILT", &|name| {
                        format!(
                            "{}. The bench has been rebuilt with every table it had before; \
                             tables the agent created are gone.",
                            why(name)
                        )
                    }),
                    Err(e) => none("E_UNAVAILABLE", &|name| format!("{}. {e}", why(name))),
                };
            }
        };
        let kept = self.keep_attached(tables, each);
        // REQ-18: a helper that waits for one of these tables can be made now.
        if self.helpers_built.is_some() {
            let names: Vec<&str> = kept
                .iter()
                .filter_map(|k| k.as_ref().ok().map(|a| a.name.as_str()))
                .collect::<Vec<_>>();
            let names: Vec<String> = names.into_iter().map(String::from).collect();
            self.helpers_after_attach(&names);
        }
        kept
    }

    /// Record what the engine attached, by name, replacing what was there; a table it did not
    /// attach leaves the store.
    #[allow(clippy::type_complexity)]
    fn keep_attached(
        &mut self,
        tables: &[(String, PathBuf)],
        each: Vec<Result<(u64, Vec<(String, String)>), Refused>>,
    ) -> Vec<Result<Attached, LoadError>> {
        tables
            .iter()
            .zip(each)
            .map(|((name, stored), r)| {
                let (rows, columns) = r.map_err(|(code, e)| {
                    let _ = std::fs::remove_file(stored);
                    load_error(code, e)
                })?;
                let attached = Attached {
                    name: name.clone(),
                    rows,
                    columns,
                    stored: stored.clone(),
                };
                match self.attachments.iter_mut().find(|a| a.name == *name) {
                    Some(entry) => {
                        let old = std::mem::replace(entry, attached.clone());
                        let _ = std::fs::remove_file(old.stored);
                    }
                    None => self.attachments.push(attached.clone()),
                }
                Ok(attached)
            })
            .collect::<Vec<_>>()
    }

    /// REQ-18: the operator's helpers, as the config gives them. Made by `build_helpers`.
    pub fn set_helpers(&mut self, helpers: Vec<crate::helpers::Def>) {
        self.helpers = helpers;
    }

    /// REQ-18: make the operator's helpers against the tables on the bench now, and put them on
    /// the engine in place of any before. A loader job makes them, in a file of its own
    /// (`crate::helpers`); the store keeps it for rebuilds. `Err` when no job ran to the end or
    /// the engine refused the file; a helper that could not be made is in the answer's `broken`,
    /// and then nothing changes on the bench.
    pub fn build_helpers(&mut self) -> Result<crate::helpers::Built, String> {
        if self.helpers.is_empty() {
            return Ok(Default::default());
        }
        self.counter += 1;
        let job = Job::Helpers {
            helpers: self.helpers.clone(),
            tables: self
                .attachments
                .iter()
                .map(|a| (a.name.clone(), a.columns.clone()))
                .collect(),
            stage: self.spec.session.join(format!("stage-{}", self.counter)),
            memory_limit: self.spec.memory_limit.clone(),
            allow_unsandboxed: self.spec.allow_unsandboxed,
        };
        let done = crate::trace::span("helpers.build", || {
            run_loader(&self.exe, &self.spec, &job, true, None)
        });
        let kept = (|| {
            let done = done.map_err(|e| e.message)?;
            let built = match done.helpers {
                Some(b) => b,
                None => {
                    return Err(done
                        .error
                        .unwrap_or_else(|| "the helpers job failed".into()))
                }
            };
            if !built.broken.is_empty() {
                return Ok((built, None));
            }
            let file = crate::loader::output(&job).map_err(|e| format!("the helpers job: {e}"))?;
            self.counter += 1;
            let stored = self.store.join(format!("{}.duckdb", self.counter));
            move_file(&file, &stored).map_err(|e| format!("could not keep the helpers: {e}"))?;
            Ok((built, Some(stored)))
        })();
        let _ = std::fs::remove_dir_all(job.stage());
        let (built, stored) = kept?;
        let Some(stored) = stored else {
            return Ok(built);
        };
        self.helper_reads.extend(built.reads.iter().cloned());
        if let Err(e) = self.install_helpers(&stored, &built.shown) {
            let _ = std::fs::remove_file(&stored);
            return Err(e);
        }
        if let Some((old, _)) = self.helpers_built.replace((stored, built.shown.clone())) {
            let _ = std::fs::remove_file(old);
        }
        Ok(built)
    }

    /// Put the helpers file the store keeps at `stored` on the engine, through a link in its
    /// workspace, as a table's is. Never recovered from here: a rebuild installs helpers too.
    fn install_helpers(
        &mut self,
        stored: &Path,
        shown: &[crate::helpers::Shown],
    ) -> Result<(), String> {
        let Some(workspace) = self.workspace_dir.clone() else {
            return Err("the engine is not running".into());
        };
        self.counter += 1;
        let link = workspace.join(format!("{}.duckdb", self.counter));
        link_or_copy(stored, &link).map_err(|e| format!("could not link the helpers: {e}"))?;
        let req = Request::Helpers {
            file: link.display().to_string(),
            shown: shown.to_vec(),
            reads: self.helper_reads.iter().cloned().collect(),
        };
        let reply = self.exchange(&req, Duration::from_secs(60));
        let _ = std::fs::remove_file(&link);
        match reply {
            Ok(r) if r.ok => Ok(()),
            Ok(r) => Err(r.body),
            Err(lost) => {
                self.kills += 1;
                self.stop_engine();
                Err(format!(
                    "{} while the helpers were being put on it",
                    lost.why()
                ))
            }
        }
    }

    /// The helpers that wait, and the table each waits for, as the last build left them.
    pub fn helpers_waiting(&self) -> Vec<(String, String)> {
        self.helpers_built
            .iter()
            .flat_map(|(_, shown)| shown.iter())
            .filter_map(|s| s.waits.clone().map(|w| (s.name.clone(), w)))
            .collect()
    }

    /// After tables went on the bench: a helper that waits for one of them can be made now, so
    /// the helpers are made again. What happens goes to the operator's log; the tables are on
    /// the bench either way.
    fn helpers_after_attach(&mut self, names: &[String]) {
        let due = self
            .helpers_waiting()
            .iter()
            .any(|(_, table)| names.iter().any(|n| n.eq_ignore_ascii_case(table)));
        if !due {
            return;
        }
        match self.build_helpers() {
            Ok(built) => {
                for b in &built.broken {
                    eprintln!("benchy: {b}");
                }
                for (name, table) in built.waiting() {
                    eprintln!("benchy: helper `{name}` waits for table `{table}`");
                }
            }
            Err(e) => eprintln!("benchy: the helpers could not be made again: {e}"),
        }
    }

    /// REQ-14: what the operator states about the shape of the sources it will load, by name.
    pub fn state_shapes(
        &mut self,
        shapes: std::collections::BTreeMap<String, crate::profile::Shape>,
    ) {
        self.shapes = shapes;
    }

    /// A host's `hello` lowered the session's limits. Only before anything is attached, so the
    /// budget is still whole and is set anew.
    ///
    /// The engine's threads are set as it opens and locked there (`lockdown::lock`), and it has
    /// opened by now, so an engine opened on more threads than the host asked for is stopped,
    /// and the next call rebuilds it on the host's, with the helpers it had and no table: only
    /// where the host lowers them, and only once, before its first call
    /// (`a_host_lowers_the_engines_threads_and_cannot_raise_them`).
    pub fn lower_limits(&mut self, limits: crate::admission::HostLimits) {
        debug_assert!(self.attachments.is_empty());
        let threads_changed = limits.threads != self.spec.limits.threads;
        self.spec.limits = limits;
        self.budget = crate::admission::Budget::new(limits.session_rows, limits.session_bytes);
        if threads_changed {
            self.stop_engine();
        }
    }

    /// A host's `hello` named the agent's tools: every text from now on uses its names.
    pub fn name_tools(&mut self, names: crate::tools::Names) {
        self.tool_names = names;
    }

    /// What is on the bench, in the order it arrived.
    pub fn attached(&self) -> &[Attached] {
        &self.attachments
    }

    /// A host's attach: an Arrow stream at `path`, as table `name`. The name is the host front
    /// end's to check; the count of tables and everything about the file are checked here.
    pub fn attach(&mut self, name: &str, path: &Path) -> Result<Attached, LoadError> {
        self.attach_all(vec![(
            name.to_string(),
            Feed::Path(path.to_path_buf()),
            Default::default(),
        )])
        .pop()
        .expect("one table in, one answer out")
    }

    /// A host's attach of several Arrow streams, each as its table, answered one by one in the
    /// order asked, as if each had been attached on its own after the ones before it: one that is
    /// refused costs the others nothing, and the count of tables and the session's budget are
    /// taken in that order (`Room`).
    ///
    /// Two things are done once for all of them rather than once each. Their loaders run side
    /// by side (`run_loaders`): a loader is a process start and a DuckDB open, about 90 M
    /// instructions and a fifth of a second of wall time for a 1,000-row stream, and they need
    /// nothing from each other. And every table that loaded is attached in one request to the
    /// engine, whose check of an attached file lists every function DuckDB has: 115 M
    /// instructions a request (`Workspace::attach_all`, docs/PERFORMANCE.md R2-5).
    /// `several_tables_attach_in_one_request` and `the_loaders_of_one_attach_run_at_once` are the
    /// tests.
    ///
    /// A table may come through a pipe (`Feed::Stream`): its loader reads the stream as the host
    /// writes it, and no byte of it passes through this process.
    ///
    /// Each table may come with the shape its host states (`profile::Shape`), held against the
    /// stream's schema by its loader before a row is written.
    pub fn attach_all(
        &mut self,
        tables: Vec<(String, Feed, crate::profile::Shape)>,
    ) -> Vec<Result<Attached, LoadError>> {
        let (rows_left, bytes_left) = self.budget.remaining();
        let most = self.spec.limits;
        let mut answers: Vec<Option<Result<Attached, LoadError>>> =
            tables.iter().map(|_| None).collect();
        // What can be refused before anything is read: a table there is no room for even alone.
        // Each loader is given the whole of what the budget has left, and stops past it; what
        // the tables before it took is checked in order once they have all loaded.
        let mut jobs: Vec<(usize, Job, Option<u64>)> = Vec::new();
        let mut inputs: Vec<Input> = Vec::new();
        // For each job, the answer of a table from the channel refused before its loader would
        // have read it: its job reads its stream past, and it is answered with this.
        let mut refused_first: Vec<Option<LoadError>> = Vec::new();
        for (i, (name, feed, shape)) in tables.into_iter().enumerate() {
            let mut refusal = None;
            let feed = match feed {
                Feed::ChannelRefused(e) => {
                    refusal = Some(e);
                    Feed::Channel
                }
                f => f,
            };
            let from_channel = matches!(feed, Feed::Channel);
            if from_channel && self.channel.is_none() {
                answers[i] = Some(Err(channel_closed(self.channel_closed.as_deref())));
                continue;
            }
            if refusal.is_none() {
                if let Err(e) = Room::new(self, rows_left, bytes_left).take(&name, 0, 0) {
                    if !from_channel {
                        answers[i] = Some(Err(e));
                        continue;
                    }
                    refusal = Some(e);
                }
            }
            self.counter += 1;
            let stage = self.spec.session.join(format!("stage-{}", self.counter));
            if let Some(e) = refusal {
                let job = Job::Drain {
                    stage,
                    max_bytes: most.attach_bytes,
                    allow_unsandboxed: self.spec.allow_unsandboxed,
                };
                jobs.push((i, job, None));
                inputs.push(Input::Channel);
                refused_first.push(Some(e));
                continue;
            }
            // A file is measured here, before a loader reads it: the budget is spent by what
            // benchy saw, not by what a loader parsing untrusted input reports. A pipe cannot be:
            // its bytes pass through its loader alone, which counts them as it reads, against the
            // same bounds (`exec::open_host_pipe`). Copying them through this process to count
            // them would cost a copy of every byte, which is what streaming is for not paying.
            let (path, measured, input) = match feed {
                Feed::Path(path) => {
                    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    (Some(path), Some(bytes), Input::Nothing)
                }
                Feed::Stream(fd) => (None, None, Input::Pipe(fd)),
                Feed::Channel | Feed::ChannelRefused(_) => (None, None, Input::Channel),
            };
            let job = Job::Arrow {
                name,
                path,
                stage,
                max_rows: most.attach_rows,
                max_bytes: most.attach_bytes,
                shape,
                budget_rows: rows_left,
                budget_bytes: bytes_left,
                allow_unsandboxed: self.spec.allow_unsandboxed,
                channel: from_channel,
            };
            jobs.push((i, job, measured));
            inputs.push(input);
            refused_first.push(None);
        }
        let (done, lost) = {
            let each: Vec<(&Job, Input)> = jobs.iter().map(|(_, job, _)| job).zip(inputs).collect();
            run_loaders(&self.exe, &self.spec, each, self.channel.as_ref())
        };
        // The channel's place is lost: it is closed, so the host's next write fails rather than
        // waiting on a reader that will never come, and every channel table after says why.
        if let Some((_, why)) = &lost {
            eprintln!("benchy: host: the stream channel is closed: {why}");
            self.channel = None;
            self.channel_closed = Some(why.clone());
        }
        let mut room = Room::new(self, rows_left, bytes_left);
        let mut ready: Vec<(usize, String, PathBuf, u64)> = Vec::new();
        for (k, (((i, job, measured), done), first)) in
            jobs.into_iter().zip(done).zip(refused_first).enumerate()
        {
            let closed_here = lost
                .as_ref()
                .filter(|(at, _)| *at == k)
                .map(|(_, why)| why.clone());
            if let Some(mut e) = first {
                let _ = std::fs::remove_dir_all(job.stage());
                e.closed_channel = closed_here;
                answers[i] = Some(Err(e));
                continue;
            }
            let kept = done.and_then(|done| {
                let rows = done.fetched_rows;
                let bytes = measured.unwrap_or(done.fetched_bytes);
                self.store_output(&job, done)
                    .map(|stored| (stored, rows, bytes))
            });
            let _ = std::fs::remove_dir_all(job.stage());
            let admitted =
                kept.and_then(
                    |(stored, rows, bytes)| match room.take(job.name(), rows, bytes) {
                        Ok(()) => Ok((stored, bytes)),
                        Err(e) => {
                            let _ = std::fs::remove_file(&stored);
                            Err(e)
                        }
                    },
                );
            match admitted {
                Ok((stored, bytes)) => ready.push((i, job.name().to_string(), stored, bytes)),
                Err(mut e) => {
                    e.closed_channel = closed_here;
                    answers[i] = Some(Err(e))
                }
            }
        }
        let names: Vec<(String, PathBuf)> = ready
            .iter()
            .map(|(_, name, stored, _)| (name.clone(), stored.clone()))
            .collect();
        let kept = self.keep_all(&names);
        for ((i, _, _, bytes), attached) in ready.into_iter().zip(kept) {
            // Spent by the rows the engine counted, once the table is on the bench: a refused
            // attach costs nothing.
            if let Ok(a) = &attached {
                self.budget.spend(a.rows, bytes);
            }
            answers[i] = Some(attached);
        }
        answers
            .into_iter()
            .map(|a| a.expect("every table answered"))
            .collect()
    }

    /// Record that table `name` could not be loaded: `describe` lists it as unavailable.
    pub fn mark_unavailable(&mut self, name: &str) {
        if !self.unavailable.iter().any(|n| n == name) {
            self.unavailable.push(name.to_string());
        }
    }

    /// Take `fd` as the session's stream channel (`--stream-channel`): from now on a `Feed::Channel`
    /// table is the next stream on it.
    pub fn set_channel(&mut self, fd: OwnedFd) {
        self.channel = Some(fd);
    }

    /// Close the stream channel for `why`, as when a stream loses it its place: the host's next
    /// write fails, and every channel table from now on is `E_CHANNEL`. A channel already closed
    /// keeps the reason it was closed for.
    pub fn close_channel(&mut self, why: &str) {
        if self.channel.take().is_some() {
            eprintln!("benchy: host: the stream channel is closed: {why}");
            self.channel_closed = Some(why.to_string());
        }
    }

    /// What the session's budget has left: rows, bytes.
    pub fn budget_left(&self) -> (u64, u64) {
        self.budget.remaining()
    }

    /// Take table `name` off the bench. It takes effect even if the engine is lost doing it: the
    /// table is out of the store first, so no rebuild brings it back.
    pub fn detach(&mut self, name: &str) -> Reply {
        let Some(i) = self.attachments.iter().position(|a| a.name == name) else {
            return Reply::fail("E_NAME", format!("no table `{name}` is attached"));
        };
        let gone = self.attachments.remove(i);
        let _ = std::fs::remove_file(&gone.stored);
        if self.child.is_none() {
            // Nothing is attached in an engine that is not running; the next rebuild leaves it
            // out. That changes what a rebuild attaches, so it earns a fresh try: detaching the
            // table that stopped the last ones is how a host brings a session back.
            self.failed_rebuilds = 0;
            return Reply::ok("");
        }
        let req = Request::Detach {
            name: name.to_string(),
        };
        match self.exchange(&req, Duration::from_secs(60)) {
            Ok(r) => r,
            Err(lost) => self.recover(lost),
        }
    }

    /// Link stored files into the engine's workspace and attach them, in one request. The engine
    /// unlinks each as soon as it is attached; the store keeps its own link for the next rebuild.
    /// Returns, per table in order, its rows and columns as the engine counted them, or why it
    /// was refused; or that the engine was lost, in which case it has been stopped, and is
    /// rebuilt — without these files — when next needed. Never recovered from here: a rebuild
    /// attaches too, and recovering inside it would rebuild inside the rebuild.
    #[allow(clippy::type_complexity)]
    fn attach_on_engine(
        &mut self,
        tables: &[(String, PathBuf)],
    ) -> Result<Vec<Result<(u64, Vec<(String, String)>), Refused>>, Lost> {
        let Some(workspace) = self.workspace_dir.clone() else {
            return Ok(tables
                .iter()
                .map(|_| Err(("E_DATA", "the engine is not running".to_string())))
                .collect());
        };
        let mut answers: Vec<Option<Result<(u64, Vec<(String, String)>), Refused>>> =
            Vec::with_capacity(tables.len());
        let mut sent = Vec::new();
        let mut links = Vec::new();
        for (name, stored) in tables {
            self.counter += 1;
            let link = workspace.join(format!("{}.duckdb", self.counter));
            match link_or_copy(stored, &link) {
                Ok(()) => {
                    sent.push((name.clone(), link.display().to_string()));
                    links.push(link);
                    answers.push(None);
                }
                Err(e) => {
                    answers.push(Some(Err(("E_DATA", format!("could not link {name}: {e}")))))
                }
            }
        }
        if !sent.is_empty() {
            let asked = sent.len();
            let reply = self.exchange(&Request::Attach { tables: sent }, Duration::from_secs(60));
            for link in &links {
                let _ = std::fs::remove_file(link);
            }
            let mut each: Box<dyn Iterator<Item = Result<(u64, Vec<(String, String)>), Refused>>> =
                match reply {
                    Ok(r) if r.ok && r.each.len() == asked => {
                        Box::new(r.each.into_iter().map(|t| {
                            if t.ok {
                                Ok((t.tables.first().map_or(0, |(_, rows)| *rows), t.columns))
                            } else if t.code.as_deref() == Some("E_NAME") {
                                // REQ-9 `an_attach_under_the_agents_own_name_is_refused`
                                Err(("E_NAME", t.body))
                            } else {
                                Err(("E_DATA", t.body))
                            }
                        }))
                    }
                    Ok(r) if r.ok => {
                        let why = format!(
                            "the engine answered for {} of the {asked} tables it was asked to attach",
                            r.each.len()
                        );
                        Box::new(std::iter::repeat_n(Err(("E_DATA", why)), asked))
                    }
                    Ok(r) => Box::new(std::iter::repeat_n(Err(("E_DATA", r.body)), asked)),
                    Err(lost) => {
                        self.kills += 1;
                        self.stop_engine();
                        return Err(lost);
                    }
                };
            for a in answers.iter_mut().filter(|a| a.is_none()) {
                *a = each.next();
            }
        }
        Ok(answers
            .into_iter()
            .map(|a| a.expect("every table answered"))
            .collect())
    }

    /// Run one loader to completion, within the load timeout.
    fn run_loader(&mut self, job: &Job) -> Result<Done, LoadError> {
        run_loader(&self.exe, &self.spec, job, true, None)
    }

    /// Kill the engine, if any, and remove what it left on disk.
    fn stop_engine(&mut self) {
        // An `Open` still unanswered is answered by nobody now: its reader sees the pipe close.
        self.opening = None;
        if let Some(mut c) = self.child.take() {
            ENGINE.store(0, Ordering::SeqCst);
            kill_with_connectors(c.id() as i32, false);
            let _ = c.wait();
        }
        if let Some(d) = self.workspace_dir.take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// One request, one reply, with a hard deadline of our own.
    fn exchange(&mut self, req: &Request, hard: Duration) -> Result<Reply, Lost> {
        // Whatever the first engine is asked waits for its `Open` first, which is answered once.
        if self.opening.is_some() {
            self.opened().map_err(Lost::Died)?;
        }
        let sent = self.send(req)?;
        self.receive(sent, hard)
    }

    /// Write `req` to the engine and start reading its reply, without waiting for it.
    fn send(&mut self, req: &Request) -> Result<Sent, Lost> {
        let Some(child) = self.child.as_mut() else {
            return Err(Lost::Died("the engine is not running".into()));
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(Lost::Died("the engine's pipes are gone".into()));
        };
        let mut stdin = stdin;
        let mut stdout = BufReader::new(stdout);
        let frame = crate::proto::frame(req)
            .map_err(|e| Lost::Died(format!("engine write failed: {e}")))?;

        // Written and read on a worker thread, so the deadline is ours, not the engine's. A
        // blocked read on a process stuck in the planner would otherwise hang the server for as
        // long as the planner felt like taking; and so would a blocked write. A request larger
        // than the pipe holds (64 KiB on Linux) to an engine that does not read it, stopped or
        // wedged, left the server without a reply for as long as the engine stayed so: over 90 s
        // past a 1 s deadline. The kill at the hard deadline ends the write with EPIPE, or the
        // read with EOF, so the thread ends with the engine. One thread does both, as one did
        // the read before: the engine reads the whole request before it answers.
        // REQ-8 `a_request_the_engine_does_not_read_is_held_to_the_hard_deadline`.
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let written = stdin
                .write_all(&frame)
                .and_then(|()| stdin.flush())
                .map_err(|e| std::io::Error::new(e.kind(), format!("engine write failed: {e}")));
            let r: std::io::Result<Reply> = written.and_then(|()| read_frame(&mut stdout));
            let _ = tx.send(r.map(|reply| (reply, stdin, stdout)));
        });
        Ok(Sent { reply: rx, reader })
    }

    /// The reply to what `sent` wrote, within `hard`, from an engine that stays under the memory
    /// ceiling meanwhile.
    ///
    /// The ceiling is checked every [`MEMORY_EVERY`] while the reply is awaited, by reading the
    /// engine's resident memory from outside it, so it counts whatever the engine is doing:
    /// planning, which nothing in DuckDB bounds, as much as executing
    /// (`an_engine_past_its_memory_ceiling_is_killed_even_while_planning`).
    fn receive(&mut self, sent: Sent, hard: Duration) -> Result<Reply, Lost> {
        let Sent { reply: rx, reader } = sent;
        let until = Instant::now() + hard;
        let engine = self.child.as_ref().map(|c| c.id());
        let ceiling = self.spec.limits.memory_ceiling;
        let got = loop {
            let left = until.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(MEMORY_EVERY)) {
                Err(mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {
                    if let Some(held) = engine.and_then(resident).filter(|h| *h > ceiling) {
                        return Err(Lost::Swollen { held, ceiling });
                    }
                }
                got => break got,
            }
        };
        match got {
            Ok(Ok((reply, stdin, stdout))) => {
                let _ = reader.join();
                if let Some(why) = reply.rebuild {
                    return Err(Lost::Spoiled(why));
                }
                // And once more as the engine answers, for what it keeps (REQ-8
                // `an_engine_left_past_its_memory_ceiling_is_rebuilt_at_once`). Read only while a
                // request ran, every `MEMORY_EVERY`, a request quicker than that, or growing
                // after its last read, could leave the engine past its ceiling, idle, for the
                // next request to be killed for. Between requests the engine runs nothing, so
                // what it holds as it answers is the most it holds until the next: this read,
                // 5 µs a request, keeps an idle engine under its ceiling with no watch while
                // idle.
                if let Some(held) = engine.and_then(resident).filter(|h| *h > ceiling) {
                    return Err(Lost::Swollen { held, ceiling });
                }
                let child = self.child.as_mut().expect("still running");
                child.stdin = Some(stdin);
                child.stdout = Some(stdout.into_inner());
                Ok(reply)
            }
            Ok(Err(e)) => {
                let _ = reader.join();
                Err(Lost::Died(e.to_string()))
            }
            // The engine is wedged somewhere no interrupt reaches; the kill that follows is what
            // unblocks the detached reader.
            Err(_) => Err(Lost::Wedged),
        }
    }

    /// The engine was lost mid-request: kill it, rebuild it once, and tell the agent what went.
    fn recover(&mut self, lost: Lost) -> Reply {
        let why = lost.why();
        let advice = lost.advice();
        self.kills += 1;
        self.stop_engine();
        match self.rebuild() {
            Ok(()) => Reply::fail(
                "E_REBUILT",
                format!(
                    "{why}. The bench has been rebuilt and is ready, with every table it had. \
                     Tables you created yourself are gone. {advice}"
                ),
            ),
            Err(e) => Reply::fail("E_UNAVAILABLE", format!("{why}. {e}")),
        }
    }

    /// Rebuild the engine from the store, within the limit on consecutive failures.
    fn rebuild(&mut self) -> Result<(), String> {
        if self.failed_rebuilds >= MAX_FAILED_REBUILDS {
            return Err(format!(
                "The bench could not be rebuilt {MAX_FAILED_REBUILDS} times in a row and is not \
                 tried again until what it holds changes: a host can detach the table that stops \
                 it; otherwise, restart benchy."
            ));
        }
        let rebuilt = self.spawn_and_open().and_then(|_| {
            // REQ-18 `helpers_survive_a_rebuild`: the helpers first, so their names are taken
            // before any table arrives and the agent's first query finds them.
            if let Some((stored, shown)) = self.helpers_built.clone() {
                self.install_helpers(&stored, &shown)?;
            }
            let tables: Vec<(String, PathBuf)> = self
                .attachments
                .iter()
                .map(|a| (a.name.clone(), a.stored.clone()))
                .collect();
            if tables.is_empty() {
                return Ok(());
            }
            for answer in self.attach_on_engine(&tables).map_err(|lost| {
                format!(
                    "{} while the bench's tables were being re-attached",
                    lost.why()
                )
            })? {
                answer.map_err(|(_, why)| why)?;
            }
            Ok(())
        });
        match rebuilt {
            Ok(()) => {
                self.failed_rebuilds = 0;
                Ok(())
            }
            Err(e) => {
                self.stop_engine();
                self.failed_rebuilds += 1;
                // The reason can name the session's files, so it goes to the operator's log. The
                // agent is told that, not where (REQ-1).
                eprintln!("benchy: rebuilding the bench failed: {e}");
                Err(
                    "Rebuilding the bench failed; the reason is in the operator's log. Tables \
                     you created yourself are gone."
                        .into(),
                )
            }
        }
    }

    fn request(&mut self, req: &Request, hard: Duration) -> Reply {
        if self.child.is_none() {
            if let Err(e) = self.rebuild() {
                return Reply::fail("E_UNAVAILABLE", format!("the bench is not available. {e}"));
            }
        }
        match self.exchange(req, hard) {
            Ok(r) => r,
            Err(lost) => self.recover(lost),
        }
    }

    pub fn describe(&mut self, budget: usize) -> Reply {
        let req = Request::Describe {
            budget,
            unavailable: self.unavailable.clone(),
            tools: self.tool_names.clone(),
        };
        self.request(&req, Duration::from_secs(30))
    }

    pub fn query(&mut self, sql: String, caps: &crate::Caps) -> Reply {
        // The hard deadline sits above the soft one: the engine gets a fair chance to interrupt
        // itself and report cleanly before we take the process away from it.
        let hard = caps.deadline + caps.hard_grace;
        let req = Request::Query {
            sql,
            max_rows: caps.max_rows,
            max_bytes: caps.max_bytes,
            max_cell_bytes: caps.max_cell_bytes,
            deadline_ms: caps.deadline.as_millis() as u64,
            unavailable: self.unavailable.clone(),
            tools: self.tool_names.clone(),
        };
        let reply = self.request(&req, hard);
        for id in &reply.result_ids {
            if let Some(n) = id
                .strip_prefix(&self.result_prefix)
                .and_then(|n| n.parse::<u64>().ok())
            {
                self.next_result_id = self.next_result_id.max(n + 1);
            }
        }
        reply
    }

    pub fn page(&mut self, result_id: String, offset: usize, caps: &crate::Caps) -> Reply {
        let req = Request::Page {
            result_id,
            offset,
            max_rows: caps.max_rows,
            max_bytes: caps.max_bytes,
            max_cell_bytes: caps.max_cell_bytes,
            tools: self.tool_names.clone(),
        };
        self.request(&req, Duration::from_secs(30))
    }
}

/// A session's result-id prefix: eight hex digits from `/dev/urandom` and a dash, `3fa9c1b2-`.
/// Thirty-two bits, so a stale id handed to another session resolves there once in four billion
/// tries; and it says nothing — no path, no pid, no name. Where there is no randomness to be had,
/// the clock's nanoseconds stand in.
fn result_prefix() -> String {
    use std::io::Read;
    let mut bits = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bits))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bits = nanos.to_ne_bytes();
    }
    format!("{:08x}-", u32::from_ne_bytes(bits))
}

/// Run one loader to completion, within the load timeout. Its process group is killed when it
/// is done, whatever it left running.
///
/// `alone` when no other loader runs beside it: only then is it the loader `kill_engine_now`
/// kills. Loaders that run side by side (`run_loaders`) are Arrow loaders, which die with the
/// thread that started them (`PR_SET_PDEATHSIG`) and start no connector, so a server stopped by a
/// signal takes them with it without being told which they are.
///
/// `pipe` is a host's stream for an Arrow job (protocol 3): the loader inherits it as
/// [`crate::loader::STREAM_FD`], and this process lets go of it once the loader has started.
fn run_loader(
    exe: &Path,
    spec: &EngineSpec,
    job: &Job,
    alone: bool,
    pipe: Option<OwnedFd>,
) -> Result<Done, LoadError> {
    let mut cmd = Command::new(exe);
    cmd.arg("--loader");
    if crate::trace::on() {
        cmd.arg(crate::trace::FLAG);
    }
    cmd
        // Built, not inherited: a connector loader passes its source's own environment to the
        // connector, which it gets in the job, and needs only PATH to find it.
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        // A connector's scratch is made under TMPDIR: inside the session, it goes with it.
        .env("TMPDIR", &spec.session)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // A file loader has no children and dies with us. A connector loader watches for that
    // itself, so it can take its connector's whole group with it. The signal is Linux's.
    #[cfg(target_os = "linux")]
    let dies_with_us = matches!(
        job,
        Job::File { .. }
            | Job::Arrow { .. }
            | Job::BuiltIn { .. }
            | Job::Helpers { .. }
            | Job::Drain { .. }
    );
    let piped = pipe.as_ref().map(|p| p.as_raw_fd());
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            crate::exec::unblock_signals();
            match piped {
                // The pipe at its place, without close-on-exec, and nothing else inherited.
                Some(fd) => {
                    let to = crate::loader::STREAM_FD;
                    let placed = if fd == to {
                        libc::fcntl(fd, libc::F_SETFD, 0)
                    } else {
                        libc::dup2(fd, to)
                    };
                    if placed < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    close_descriptors_from(to as u32 + 1);
                }
                None => close_inherited_descriptors(),
            }
            #[cfg(target_os = "linux")]
            if dies_with_us {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            }
            Ok(())
        });
    }
    crate::trace::mark("loader.spawn", &[]);
    let mut child = cmd
        .spawn()
        .map_err(|e| load_error("E_INTERNAL", format!("could not start a loader: {e}")))?;
    // The loader holds the pipe now. Let go of here, the host's writer is told that nobody reads
    // the moment a loader stops early, not once this has finished waiting for it.
    drop(pipe);
    let runs_connectors = matches!(job, Job::Exec { .. });
    if alone {
        LOADER_RUNS_CONNECTORS.store(runs_connectors, Ordering::SeqCst);
        LOADER.store(child.id() as i32, Ordering::SeqCst);
    }
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let sent = write_frame(&mut stdin, job);
    drop(stdin);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let r: std::io::Result<Done> = read_frame(&mut BufReader::new(stdout));
        let _ = tx.send(r);
    });
    let timeout = spec.load_timeout;
    // REQ-8 `a_loader_past_its_memory_ceiling_is_killed_and_attaches_nothing`: the loader is
    // read every `MEMORY_EVERY` while it runs, from outside it, so whatever it is doing counts —
    // DuckDB reading a file, a decoder, a connector — and past the ceiling it is killed below,
    // with every connector it runs. A read is 5 µs; a load of 3 s pays 60 of them.
    let until = Instant::now() + timeout;
    let ceiling = spec.limits.loader_memory_ceiling;
    let mut watch = Watch::new(child.id(), runs_connectors);
    let mut swollen = None;
    let answer = loop {
        let left = until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(MEMORY_EVERY)) {
            Err(mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {
                if let Some(held) = watch.held().filter(|h| *h > ceiling) {
                    swollen = Some(held);
                    break Err(mpsc::RecvTimeoutError::Timeout);
                }
            }
            got => break got,
        }
    };
    let out = match (sent, answer) {
        _ if swollen.is_some() => Err(loader_swollen(
            job.name(),
            swollen.unwrap_or_default(),
            ceiling,
            runs_connectors,
        )),
        (Err(e), _) => Err(load_error(
            "E_INTERNAL",
            format!("could not hand `{}` to a loader: {e}", job.name()),
        )),
        (_, Ok(Ok(done))) => {
            crate::trace::mark(
                "loader.done",
                &[
                    ("fetched_rows", done.fetched_rows.to_string()),
                    ("fetched_bytes", done.fetched_bytes.to_string()),
                ],
            );
            Ok(done)
        }
        (_, Ok(Err(e))) => Err(load_error(
            "E_DATA",
            format!("the loader for `{}` stopped: {e}", job.name()),
        )),
        (_, Err(_)) => Err(load_error(
            "E_DEADLINE",
            format!(
                "loading `{}` did not finish within {}s; raise --load-timeout if the source \
                 is genuinely that slow",
                job.name(),
                timeout.as_secs()
            ),
        )),
    };
    if alone {
        LOADER.store(0, Ordering::SeqCst);
    }
    kill_with_connectors(child.id() as i32, runs_connectors);
    let _ = child.wait();
    out
}

/// The most loaders `run_loaders` runs at once. Each is a process with a DuckDB of its own: a
/// 10,000,000-row stream peaks at about 116 MB in its loader (docs/history/PERFORMANCE-rounds.md),
/// so four at
/// once stay under half a gigabyte.
const MAX_LOADERS_AT_ONCE: usize = 4;

/// What a loader is given besides its job: nothing; the read end of the host's own pipe for this
/// stream, which this process lets go of once the loader has it; or a copy of the session's
/// stream channel.
pub(crate) enum Input {
    Nothing,
    Pipe(OwnedFd),
    Channel,
}

/// A table from the stream channel once the channel is closed, or in a session without one.
fn channel_closed(why: Option<&str>) -> LoadError {
    load_error(
        "E_CHANNEL",
        match why {
            Some(why) => format!(
                "the stream channel is closed: {why}. Nothing more comes through it in this \
                 session; attach by `path` or with a descriptor, or start a new session"
            ),
            None => "this session has no stream channel".to_string(),
        },
    )
}

/// Run every job's loader, side by side, as many at once as the machine has cores and at most
/// `MAX_LOADERS_AT_ONCE`, and answer for each in order. Only for jobs that need nothing from each
/// other: a host's Arrow streams, each given what the budget has left before any of them.
///
/// A loader is a process start, a confinement and a DuckDB open before it reads a row: about
/// 90 M instructions and, on a 4-core machine, 125 ms of wall time each for a 1,000-row stream,
/// which one after another was most of what a host waited for (docs/PERFORMANCE.md R2-5).
///
/// A job with a pipe is started in its turn like any other: a host that writes its streams one
/// after another, in the order of the list, is read in that order.
///
/// The jobs that read the stream channel (`Input::Channel`) run one at a time, in the order of
/// the list, beside the others: each takes the next whole stream, so the next may start only once
/// one is done. Where one loses the channel its place, the rest are not run, and that job's index
/// and why come back with the answers.
#[allow(clippy::type_complexity)]
fn run_loaders(
    exe: &Path,
    spec: &EngineSpec,
    jobs: Vec<(&Job, Input)>,
    channel: Option<&OwnedFd>,
) -> (Vec<Result<Done, LoadError>>, Option<(usize, String)>) {
    // What a thread takes at a time: one job, or every channel job, in order.
    let mut work: Vec<Vec<usize>> = Vec::new();
    let mut channel_work: Option<usize> = None;
    for (k, (_, input)) in jobs.iter().enumerate() {
        match (input, channel_work) {
            (Input::Channel, Some(w)) => work[w].push(k),
            (Input::Channel, None) => {
                channel_work = Some(work.len());
                work.push(vec![k]);
            }
            _ => work.push(vec![k]),
        }
    }
    let width = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_LOADERS_AT_ONCE)
        .min(work.len());
    let answers: Vec<std::sync::Mutex<Option<Result<Done, LoadError>>>> =
        jobs.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let lost: std::sync::Mutex<Option<(usize, String)>> = std::sync::Mutex::new(None);
    let jobs: Vec<(&Job, std::sync::Mutex<Input>)> = jobs
        .into_iter()
        .map(|(job, input)| (job, std::sync::Mutex::new(input)))
        .collect();
    let run_one = |k: usize, alone: bool| -> Result<Done, LoadError> {
        let (job, input) = &jobs[k];
        let input = std::mem::replace(
            &mut *input.lock().unwrap_or_else(|p| p.into_inner()),
            Input::Nothing,
        );
        let pipe = match input {
            Input::Nothing => None,
            Input::Pipe(fd) => Some(fd),
            Input::Channel => {
                if let Some((_, why)) = &*lost.lock().unwrap_or_else(|p| p.into_inner()) {
                    return Err(channel_closed(Some(why)));
                }
                // A copy for the loader, close-on-exec until it is put on its descriptor.
                match channel.map(|c| c.try_clone()) {
                    Some(Ok(copy)) => Some(copy),
                    _ => {
                        let why = "the channel could not be handed to a loader".to_string();
                        *lost.lock().unwrap_or_else(|p| p.into_inner()) = Some((k, why.clone()));
                        return Err(load_error("E_INTERNAL", why));
                    }
                }
            }
        };
        let from_channel = matches!(job, Job::Arrow { channel: true, .. } | Job::Drain { .. });
        let done = crate::trace::span("loader.run", || run_loader(exe, spec, job, alone, pipe));
        if from_channel {
            // In step only on the loader's word that it read to the marker and no further; a
            // loader that gave no answer — killed past the load timeout, crashed — left the
            // channel somewhere in the middle of a stream.
            let out = match &done {
                Ok(d) if d.in_step => None,
                Ok(d) => Some(d.out_of_step.clone().unwrap_or_else(|| {
                    "its loader stopped before the stream's end-of-stream marker".to_string()
                })),
                Err(e) => Some(format!(
                    "its load stopped part-way through the stream ({})",
                    e.code
                )),
            };
            if let Some(why) = out {
                *lost.lock().unwrap_or_else(|p| p.into_inner()) = Some((k, why));
            }
        }
        done
    };
    if width <= 1 {
        // One at a time, in the order of the list, which is the order a host writes its streams
        // in. Not by `work`: that runs every channel table before the descriptor tables after
        // the first, and a host writing a channel table, a descriptor's, then a channel table
        // waited on the descriptor's full pipe while the second channel table's loader waited
        // for a stream not yet written, until the load timeout failed it and closed the channel
        // (`a_channel_and_a_descriptor_load_in_request_order_on_one_cpu`). One CPU is what a
        // pod with `cpu: 1` has: `available_parallelism` reads the cgroup quota.
        for (k, answer) in answers.iter().enumerate() {
            *answer.lock().unwrap_or_else(|p| p.into_inner()) = Some(run_one(k, true));
        }
    } else {
        let next = std::sync::atomic::AtomicUsize::new(0);
        // Each thread waits for every loader it starts, so a loader's parent-death signal, which
        // follows the thread that started it, fires only when the server goes.
        std::thread::scope(|s| {
            for _ in 0..width {
                s.spawn(|| loop {
                    let w = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = work.get(w) else {
                        break;
                    };
                    for &k in item {
                        let done = run_one(k, false);
                        *answers[k].lock().unwrap_or_else(|p| p.into_inner()) = Some(done);
                    }
                });
            }
        });
    }
    let answers = answers
        .into_iter()
        .map(|a| {
            a.into_inner()
                .unwrap_or_else(|p| p.into_inner())
                .unwrap_or_else(|| Err(load_error("E_INTERNAL", "a loader was never run")))
        })
        .collect();
    (
        answers,
        lost.into_inner().unwrap_or_else(|p| p.into_inner()),
    )
}

/// What a host's attach has room for, taken table by table in the order asked: the count of
/// tables a session holds (`MAX_ATTACHMENTS`; replacing one is not adding one), and the rows
/// and bytes the session's budget has left. A table is held to what the ones before it in the
/// same request left, as if each had been attached on its own, one after another.
///
/// The rows are the ones a table's loader reports writing. That is the loader's word, which
/// benchy otherwise takes for nothing; it is taken here because the loader already holds its own
/// table to the budget as it reads it, and so is trusted with that much already. What is spent
/// is still what the engine counted, once the table is on the bench (`Supervisor::attach_all`).
/// A table the engine then refuses has taken its room from the ones after it in the request:
/// they are refused rather than let past what is left.
struct Room {
    names: std::collections::HashSet<String>,
    rows_left: u64,
    bytes_left: u64,
    /// The most tables the session holds: `MAX_ATTACHMENTS`, or fewer where its host said so.
    most: usize,
}

impl Room {
    fn new(s: &Supervisor, rows_left: u64, bytes_left: u64) -> Room {
        Room {
            names: s.attachments.iter().map(|a| a.name.clone()).collect(),
            rows_left,
            bytes_left,
            most: (s.spec.limits.attachments as usize).min(MAX_ATTACHMENTS),
        }
    }

    fn take(&mut self, name: &str, rows: u64, bytes: u64) -> Result<(), LoadError> {
        let most = self.most;
        if !self.names.contains(name) && self.names.len() >= most {
            return Err(load_error(
                "E_LIMIT",
                format!(
                    "a session holds at most {most} tables; detach one first, or \
                     replace one by attaching its name again"
                ),
            ));
        }
        if rows > self.rows_left {
            return Err(load_error(
                "E_LIMIT",
                format!(
                    "it holds {rows} rows, and this session has {} rows of its budget left \
                     after the tables before it",
                    self.rows_left
                ),
            ));
        }
        if bytes > self.bytes_left {
            return Err(load_error(
                "E_LIMIT",
                format!(
                    "it is {bytes} bytes, and this session has {} bytes of its budget left \
                     after the tables before it",
                    self.bytes_left
                ),
            ));
        }
        self.rows_left -= rows;
        self.bytes_left -= bytes;
        self.names.insert(name.to_string());
        Ok(())
    }
}

/// Move a file between two of the session's directories. A Landlock domain that benchy itself
/// runs inside — a host may sandbox it — can forbid moving a file to another directory and say so
/// with EXDEV; then the file is copied instead.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            std::fs::copy(from, to)?;
            std::fs::remove_file(from)
        }
        r => r,
    }
}

/// Hard-link a stored file into the engine's workspace, or copy it where linking is refused.
fn link_or_copy(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::hard_link(from, to) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => std::fs::copy(from, to).map(|_| ()),
        r => r,
    }
}

/// Start the session's warden: a process that removes the session's directory once the server
/// is gone, however it went. SIGKILL runs no handler and no destructor, and the engine, which
/// watches too, may not be running — after a rebuild that failed, there is none. The warden holds
/// nothing but the read end of a pipe whose write end only this process has; the kernel closes it
/// when this process dies, whatever kills it.
///
/// **Forked, not started from the binary again** (R3-11, docs/PERFORMANCE.md): `exec` of this
/// binary spent about 21 M instructions, nearly all of them the dynamic linker resolving
/// `libduckdb.so`, for a process that only waits; a fork shares this one's pages and links
/// nothing. A fork is a copy of this process, so it is made **before anything it should not
/// hold exists here** — before the config is read and its `${VAR}` credentials resolved, before
/// DuckDB is opened or any thread started — and the copy then forgets the environment it was
/// forked with, which is where a credential is before the config names it
/// (`the_warden_holds_nothing_of_the_servers`). It must be called while this process has one
/// thread: a fork copies only the thread that calls it, and a lock another one held stays held
/// in the copy. That is checked, and starting is refused otherwise.
pub fn start_warden(session: &Path) -> Result<(), String> {
    let refused = |what: &str| {
        format!(
            "could not start the session's warden: {what}: {}",
            std::io::Error::last_os_error()
        )
    };
    if !one_thread() {
        return Err(
            "could not start the session's warden: this process has started a thread".into(),
        );
    }
    let tmp = std::env::temp_dir();
    let null = c"/dev/null";
    let mut pipe = [0; 2];
    // Close-on-exec: the engine and the loaders this process starts must not hold the write end,
    // or the pipe would outlive this process.
    if !cloexec_pipe(&mut pipe) {
        return Err(refused("pipe"));
    }
    let [read, write] = pipe;
    match unsafe { libc::fork() } {
        -1 => {
            unsafe {
                libc::close(read);
                libc::close(write);
            }
            Err(refused("fork"))
        }
        0 => unsafe {
            // Its own session, so a signal meant for the terminal's foreground group does not
            // stop it before it has cleaned up.
            libc::setsid();
            crate::exec::unblock_signals();
            libc::dup2(read, 0);
            let devnull = libc::open(null.as_ptr(), libc::O_RDWR);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            close_inherited_descriptors();
            // A name in `ps`; macOS has no prctl, and its warden goes by the server's name.
            #[cfg(target_os = "linux")]
            libc::prctl(libc::PR_SET_NAME, c"benchy-warden".as_ptr());
            forget_environment();
            libc::_exit(run_warden(session, &tmp))
        },
        _ => {
            unsafe { libc::close(read) };
            // `write` is held open for the life of this process and never written: its closing
            // is the signal.
            Ok(())
        }
    }
}

/// A pipe whose two ends close on exec, so the engine and the loaders this process starts do
/// not hold the write end. macOS has no `pipe2`: there the flag is set just after, which nothing
/// can race, since the warden is started from one thread (`one_thread`).
fn cloexec_pipe(pipe: &mut [libc::c_int; 2]) -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        unsafe {
            libc::pipe(pipe.as_mut_ptr()) == 0
                && pipe
                    .iter()
                    .all(|fd| libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0)
        }
    }
}

/// Whether this process has one thread, from `/proc/self/stat`'s `num_threads`. Elsewhere than
/// Linux it cannot tell, and says yes: the server starts the warden before any thread there too.
fn one_thread() -> bool {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        stat.rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(17))
            .is_some_and(|n| n == "1")
    }
    #[cfg(not(target_os = "linux"))]
    true
}

/// Overwrite every environment string this process has with zeros, and drop them: the forked
/// warden's copy of the server's environment, which may hold a credential the config was about
/// to name. Zeroed, not only unset: `clearenv` drops the pointers and leaves the strings in the
/// stack pages they came in, which `/proc/<pid>/environ` reads.
unsafe fn forget_environment() {
    extern "C" {
        static mut environ: *mut *mut libc::c_char;
    }
    let mut at = unsafe { environ };
    while !at.is_null() && !unsafe { *at }.is_null() {
        let s = unsafe { *at };
        for i in 0..unsafe { libc::strlen(s) } {
            unsafe { std::ptr::write_volatile(s.add(i), 0) };
        }
        at = unsafe { at.add(1) };
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::clearenv()
    };
    // macOS has no clearenv: the forked warden is alone, so its environment is pointed at an
    // empty list, which is what clearenv does.
    #[cfg(not(target_os = "linux"))]
    unsafe {
        static mut EMPTY: [*mut libc::c_char; 1] = [std::ptr::null_mut()];
        environ = std::ptr::addr_of_mut!(EMPTY).cast();
    }
}

/// The warden itself: wait for the server's end of the pipe, its standard input, to close, then
/// remove the session; the exit status.
///
/// Only a directory benchy could have made as a session — `benchy-…` in the temporary directory
/// `tmp`, this user's, mode 0700 — whatever it is handed. A signal interrupts the wait without
/// ending it. And where the warden, not the server, had to remove the session, it looks again a
/// few times: a connector still running when its server went can write into its scratch for the
/// moment its loader takes to stop it.
fn run_warden(session: &Path, tmp: &Path) -> i32 {
    if a_session(session, tmp).is_err() {
        return 2;
    }
    let mut buf = [0u8; 64];
    loop {
        match unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) } {
            0 => break,
            n if n > 0 => continue,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue
            }
            _ => break,
        }
    }
    if session.exists() {
        for pause in [0, 300, 1000, 2000] {
            std::thread::sleep(Duration::from_millis(pause));
            let _ = std::fs::remove_dir_all(session);
        }
    }
    0
}

/// Whether `dir` is a session directory benchy could have made in `tmp`.
fn a_session(dir: &Path, tmp: &Path) -> Result<(), &'static str> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(dir).map_err(|_| "it is not there")?;
    if !md.is_dir() {
        return Err("it is not a directory");
    }
    if !dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("benchy-"))
    {
        return Err("it is not named as a session is");
    }
    let parent = dir.parent().and_then(|p| p.canonicalize().ok());
    if parent.is_none() || parent != tmp.canonicalize().ok() {
        return Err("it is not in the temporary directory");
    }
    if md.mode() & 0o777 != 0o700 || md.uid() != unsafe { libc::geteuid() } {
        return Err("it is not private to this user, as a session is");
    }
    Ok(())
}

/// Close every descriptor but the three standard ones before a child runs: a host's open files
/// and sockets are not the engine's or a loader's to hold. Async-signal-safe.
fn close_inherited_descriptors() {
    close_descriptors_from(3)
}

/// Close every descriptor from `first` on. Async-signal-safe: for between fork and exec.
fn close_descriptors_from(first: u32) {
    #[cfg(target_os = "linux")]
    unsafe {
        // close_range(first, ~0, 0), since Linux 5.9; where it is missing, one by one.
        if libc::syscall(libc::SYS_close_range, first, u32::MAX, 0u32) != 0 {
            for fd in first as i32..1024 {
                libc::close(fd);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    unsafe {
        for fd in first as i32..1024 {
            libc::close(fd);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop_engine();
    }
}

#[cfg(test)]
mod tests {
    use super::{a_session, Room, MAX_ATTACHMENTS};

    #[test]
    #[cfg(target_os = "linux")]
    fn the_warden_is_forked_only_from_one_thread() {
        // A fork copies the thread that calls it alone, and a lock another thread held stays
        // held in the copy. The test harness runs this on a thread of its own, so the warden is
        // refused here, and nothing is forked.
        let refused = super::start_warden(std::path::Path::new("/nonexistent/benchy-0-ab-0"));
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("started a thread")),
            "{refused:?}"
        );
    }

    #[test]
    fn only_a_session_directory_is_the_wardens_to_remove() {
        // The warden is handed its server's session, and checks it is one benchy could have
        // made — `benchy-…` in the temporary directory, this user's, mode 0700 — before it
        // removes anything: not one elsewhere, named otherwise, or open to others.
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("benchy-ward-{}", std::process::id()));
        let tmp = root.join("tmp");
        let dir = |p: &std::path::Path, mode: u32| {
            std::fs::create_dir_all(p).unwrap();
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let elsewhere = root.join("benchy-1-ab-0");
        let named = tmp.join("victim");
        let open = tmp.join("benchy-2-ab-0");
        let session = tmp.join("benchy-3-ab-0");
        dir(&elsewhere, 0o700);
        dir(&named, 0o700);
        dir(&open, 0o755);
        dir(&session, 0o700);
        assert!(a_session(&elsewhere, &tmp).is_err());
        assert!(a_session(&named, &tmp).is_err());
        assert!(a_session(&open, &tmp).is_err());
        assert!(a_session(&tmp.join("benchy-4-ab-0"), &tmp).is_err());
        assert_eq!(a_session(&session, &tmp), Ok(()));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn room(names: &[&str], rows_left: u64, bytes_left: u64) -> Room {
        Room {
            names: names.iter().map(|n| n.to_string()).collect(),
            rows_left,
            bytes_left,
            most: MAX_ATTACHMENTS,
        }
    }

    #[test]
    fn the_tables_of_one_attach_are_held_to_the_budget_in_order() {
        // REQ-12 for a host's attach of several tables: each loader is given all the budget has
        // left, so tables that each fit alone are checked together, in the order asked — the
        // first fits, and one that passes what the ones before it left is refused as `E_LIMIT`,
        // in rows or in bytes. Exactly what is left fits; a refused table takes nothing, so one
        // after it can still fit.
        let mut r = room(&[], 10, 100);
        assert!(r.take("a", 6, 40).is_ok());
        let e = r.take("b", 5, 40).unwrap_err();
        assert_eq!(e.code, "E_LIMIT", "{e}");
        assert!(e.message.contains("4 rows of its budget left"), "{e}");
        let e = r.take("c", 1, 61).unwrap_err();
        assert_eq!(e.code, "E_LIMIT", "{e}");
        assert!(e.message.contains("60 bytes of its budget left"), "{e}");
        assert!(r.take("d", 4, 60).is_ok(), "exactly what is left fits");
        assert!(r.take("e", 1, 0).is_err(), "one row more does not");
        assert!(r.take("f", 0, 1).is_err(), "nor one byte more");
    }

    #[test]
    fn the_tables_of_one_attach_are_counted_in_order() {
        // With room for one more table, the first new name takes it; replacing a table already
        // there, or one earlier in the same request, is not adding one.
        let full: Vec<String> = (1..MAX_ATTACHMENTS).map(|i| format!("t{i}")).collect();
        let full: Vec<&str> = full.iter().map(|s| s.as_str()).collect();
        let mut r = room(&full, u64::MAX, u64::MAX);
        assert!(r.take("new", 1, 1).is_ok());
        assert_eq!(r.take("newer", 1, 1).unwrap_err().code, "E_LIMIT");
        assert!(r.take("t1", 1, 1).is_ok(), "replacing is not adding");
        assert!(
            r.take("new", 1, 1).is_ok(),
            "nor is replacing one just added"
        );
    }
}
