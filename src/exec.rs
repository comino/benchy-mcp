//! REQ-4 and REQ-5: the exec connector tier.
//!
//! A source benchy does not know how to read is a subprocess that does. It speaks
//! `docs/EXEC-CONNECTOR-PROTOCOL.md` (contract `benchy-exec/1`): one JSON document in on stdin,
//! one Arrow streaming IPC stream out on stdout, NDJSON control events on stderr.
//!
//! A connector runs in a loader (`crate::loader`), a process of its own that writes the result
//! to a file and exits, with only the environment its source lists. The engine that runs agent
//! SQL only ever attaches that file: it never starts a connector and never holds a credential
//! (`credentials_never_cross`). So agent SQL can never reach a connector, its network or its
//! credentials — adding a source does not widen what a query can touch.
//!
//! Three things here are load-bearing and each exists because of a measured failure:
//!
//! - **The operator declares the schema; the connector does not.** A connector is a third party
//!   and its output is attacker-influenceable. If `describe`'s schema were authoritative, column
//!   names would flow from a network payload into what the model reads.
//! - **A stream without its end-of-stream marker is a failure, not a short read.** Measured on
//!   pyarrow 25.0.1: a two-batch stream cut at the exact message boundary parses clean and
//!   returns half the rows. A paginating connector that dies between pages cuts there every time.
//! - **What a stream decodes to is bounded, not only what it is.** A compressed body's
//!   `bodyLength` is the *compressed* size — 31.6× smaller, measured on Int64 — and a dictionary
//!   or a view column unpacks to more than its keys or views. All are counted against
//!   `max_bytes` before they are decoded (`crate::ipc`, `read_stream`).

use arrow::array::RecordBatch;
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The end-of-stream marker: a continuation prefix followed by a zero metadata length.
const EOS: [u8; 8] = [0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0];

/// Hard caps. Defaults are the protocol's; an operator may lower them per source.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    pub max_rows: u64,
    pub max_bytes: u64,
    pub max_batch_bytes: u64,
    pub deadline: Duration,
    pub cancel_grace: Duration,
    pub describe_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_rows: 10_000_000,
            max_bytes: 256 * 1024 * 1024,
            max_batch_bytes: 16 * 1024 * 1024,
            deadline: Duration::from_secs(300),
            cancel_grace: Duration::from_millis(2000),
            describe_timeout: Duration::from_secs(5),
        }
    }
}

/// One field of the **operator's** declaration. Never the connector's.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDecl {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default = "yes")]
    pub nullable: bool,
}

fn yes() -> bool {
    true
}

/// The closed type vocabulary. Closed on purpose: each token maps to exactly one Arrow
/// type and one DuckDB type, so "what will this column be" has one answer at config time.
pub fn arrow_type(token: &str) -> Option<DataType> {
    Some(match token {
        "bool" => DataType::Boolean,
        "int8" => DataType::Int8,
        "int16" => DataType::Int16,
        "int32" => DataType::Int32,
        "int64" => DataType::Int64,
        "uint8" => DataType::UInt8,
        "uint16" => DataType::UInt16,
        "uint32" => DataType::UInt32,
        "uint64" => DataType::UInt64,
        "float32" => DataType::Float32,
        "float64" => DataType::Float64,
        "string" => DataType::Utf8,
        "binary" => DataType::Binary,
        "date" => DataType::Date32,
        "time" => DataType::Time64(TimeUnit::Microsecond),
        "timestamp" => DataType::Timestamp(TimeUnit::Microsecond, None),
        "timestamptz" => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        other => {
            let rest = other.strip_prefix("decimal(")?.strip_suffix(')')?;
            let (p, s) = rest.split_once(',')?;
            let p: u8 = p.trim().parse().ok()?;
            let s: i8 = s.trim().parse().ok()?;
            if !(1..=38).contains(&p) || s < 0 || s as u8 > p {
                return None;
            }
            DataType::Decimal128(p, s)
        }
    })
}

/// What the operator configured. `config` is copied verbatim into the input document and is
/// trusted; `env` is where credentials travel, and nowhere else.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecSource {
    pub name: String,
    /// What the operator will let this source return. Sent to the connector *and* re-applied
    /// after ingest, because a connector may declare `enforced` and be wrong.
    pub narrowing: Narrowing,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub config: serde_json::Value,
    pub schema: Vec<FieldDecl>,
    pub limits: Limits,
    /// The zone a zone-less `timestamp` is in; UTC when `None` (REQ-15).
    #[serde(default)]
    pub timezone: Option<String>,
}

/// The connector's answer to `describe`. Only `name` and `version` are checked, and `exec-test`
/// prints them; nothing of it reaches the model.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Describe {
    #[serde(default)]
    pub protocol: Vec<u32>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub estimate: Option<String>,
    #[serde(default)]
    pub narrowing: BTreeMap<String, String>,
}

/// Why a load failed. Operator-facing, all of it: the agent is told only that the table is not
/// available (`a_connector_error_reaches_the_operator_not_the_agent`). A connector's error event
/// carries its own words — a message with URLs, account ids and stack traces, a hint — and a
/// language model cannot distrust a string, so none of it heads for one; nor does which source
/// failed, or how, since the agent works with tables, not with where they came from (REQ-15).
#[derive(Debug)]
pub struct Fault {
    /// A closed-set code. The host's, or the connector's when it classified itself.
    pub code: &'static str,
    /// Operator-facing. May contain paths, hosts and stack traces. Never shown to the model.
    pub detail: String,
}

impl Fault {
    pub fn host(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// REQ-5. A connector holds credentials, so a writable connector is a credential handover.
pub fn check_permissions(path: &Path) -> Result<(), Fault> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            // The commonest first-run mistake, so say where we looked and why there.
            Fault::host(
                "E_CONFIG",
                format!(
                    "no connector at {}. A relative `command` resolves against the directory of \
                     the config file — never the working directory and never PATH — so that a \
                     path in the file means what it looks like it means.",
                    path.display()
                ),
            )
        } else {
            Fault::host("E_HOST_CONNECTOR_PERMS", format!("{}: {e}", path.display()))
        }
    })?;
    if !md.is_file() {
        return Err(Fault::host(
            "E_HOST_CONNECTOR_PERMS",
            format!("{}: not a regular file", path.display()),
        ));
    }
    let mode = md.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(Fault::host(
            "E_HOST_CONNECTOR_PERMS",
            format!(
                "{}: not executable (mode {:o})",
                path.display(),
                mode & 0o7777
            ),
        ));
    }
    {
        use std::os::unix::fs::MetadataExt;
        let me = unsafe { libc::geteuid() };
        let personal = || is_personal_group(md.uid(), md.gid());
        if let Some(who) = who_else_can_write(mode, md.uid(), me, personal) {
            let fix = match who {
                Writer::Owner => "chown it to the user that runs benchy",
                Writer::Everyone => "chmod go-w",
                Writer::Group => "chmod g-w",
            };
            return Err(Fault::host(
                "E_HOST_CONNECTOR_PERMS",
                format!(
                    "{}: writable by {} (mode {:o}, owner uid {}). A connector holds \
                     credentials, so anyone who can write it can take them. {fix} and start \
                     again.",
                    path.display(),
                    who.describe(),
                    mode & 0o7777,
                    md.uid()
                ),
            ));
        }
    }
    if let Some(parent) = path.parent() {
        if let Ok(pmd) = std::fs::metadata(parent) {
            let pmode = pmd.permissions().mode();
            // world-writable without the sticky bit: anyone may replace the connector
            if pmode & 0o002 != 0 && pmode & 0o1000 == 0 {
                return Err(Fault::host(
                    "E_HOST_CONNECTOR_PERMS",
                    format!(
                        "{}: its directory is world-writable without the sticky bit (mode {:o}), \
                         so the connector can be replaced.",
                        path.display(),
                        pmode & 0o7777
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Build the connector's environment. **Constructed, never inherited** — notably `HOME` is
/// absent unless the operator listed it, because `~/.aws/credentials`, `~/.pgpass` and
/// `~/.netrc` are ambient credentials an SDK picks up silently, which would defeat the
/// per-source allowlist and make REQ-5's test meaningless.
fn env_for(src: &ExecSource, op: &str, run_id: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "PATH".into(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
    );
    env.insert("TZ".into(), "UTC".into());
    env.insert("LC_ALL".into(), "C.UTF-8".into());
    env.insert("BENCHY_EXEC_PROTOCOL".into(), "1".into());
    env.insert("BENCHY_SOURCE_NAME".into(), src.name.clone());
    env.insert("BENCHY_RUN_ID".into(), run_id.into());
    env.insert("BENCHY_OP".into(), op.into());
    // The operator's allowlist last, so a source may override PATH deliberately.
    for (k, v) in &src.env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// Someone other than the user running benchy who could rewrite a connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    /// It belongs to another user, who can always change it.
    Owner,
    /// It is world-writable.
    Everyone,
    /// It is group-writable, and the group is shared.
    Group,
}

impl Writer {
    fn describe(self) -> &'static str {
        match self {
            Writer::Owner => "its owner, another user",
            Writer::Everyone => "everyone",
            Writer::Group => "a group other users belong to",
        }
    }
}

/// Who, besides the user running benchy (`me`) and root, could rewrite a file with this mode and
/// owner. `personal_group` is asked only when it matters.
///
/// Group-writable is fine when the group is the owner's personal one — the user-private-group
/// scheme most Linux distributions use, where every user has a group of their own name and
/// nobody else in it. There the default umask is 0002, so every file a user creates, a fresh
/// clone of this repository included, is group-writable while nobody else can write it.
pub fn who_else_can_write(
    mode: u32,
    owner: u32,
    me: u32,
    personal_group: impl FnOnce() -> bool,
) -> Option<Writer> {
    if owner != me && owner != 0 {
        return Some(Writer::Owner);
    }
    if mode & 0o002 != 0 {
        return Some(Writer::Everyone);
    }
    if mode & 0o020 != 0 && !personal_group() {
        return Some(Writer::Group);
    }
    None
}

/// Whether `gid` is `uid`'s personal group: its primary group, named after the user, with no
/// other members listed. A lookup that fails counts as shared.
pub fn is_personal_group(uid: u32, gid: u32) -> bool {
    use std::ffi::CStr;
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; 16 * 1024];
        if libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut found) != 0
            || found.is_null()
            || pw.pw_gid != gid
        {
            return false;
        }
        let user = CStr::from_ptr(pw.pw_name).to_bytes().to_vec();
        let mut gr: libc::group = std::mem::zeroed();
        let mut found: *mut libc::group = std::ptr::null_mut();
        let mut buf = vec![0 as libc::c_char; 64 * 1024];
        if libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut found) != 0
            || found.is_null()
            || CStr::from_ptr(gr.gr_name).to_bytes() != user.as_slice()
        {
            return false;
        }
        let mut member = gr.gr_mem;
        while !member.is_null() && !(*member).is_null() {
            if CStr::from_ptr(*member).to_bytes() != user.as_slice() {
                return false;
            }
            member = member.add(1);
        }
        true
    }
}

/// Start a child with no signal blocked. The server blocks SIGTERM, SIGINT and SIGHUP on every
/// thread so that one thread can wait for them, and a signal mask survives fork and exec alike: a
/// child that kept it never sees the SIGTERM that asks it to stop — measured, a Python connector
/// started with all three blocked. Only async-signal-safe calls, so it may run between fork and
/// exec.
pub fn unblock_signals() {
    unsafe {
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
}

/// The connector this process started last. One runs at a time, so one slot is enough.
static RUNNING: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Stop the connector this process started last, and everything it started, if it is still
/// ours. A pid is trusted only while its parent is this process, so a number the kernel has
/// since handed to another process is never signalled.
pub fn kill_running_connector() {
    let pid = RUNNING.load(std::sync::atomic::Ordering::SeqCst);
    if pid <= 0 {
        return;
    }
    // `pid (comm) state ppid …` — the command name may contain anything, so split after it.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let ppid = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(1))
        .and_then(|p| p.parse::<u32>().ok());
    if ppid == Some(std::process::id()) {
        // setsid() made it a group leader, so this reaches its children too.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
}

fn spawn(
    src: &ExecSource,
    op: &str,
    run_id: &str,
    cwd: &Path,
) -> std::io::Result<std::process::Child> {
    let mut cmd = Command::new(&src.command);
    cmd.arg(op)
        .args(&src.args)
        .current_dir(cwd)
        .env_clear()
        .envs(env_for(src, op, run_id))
        .env("TMPDIR", cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            // Own process group, so the whole connector *and its children* can be signalled.
            libc::setsid();
            unblock_signals();
            // Die with the loader — SIGKILL, not a request a connector could ignore: it must not
            // run on mid-fetch with nobody left to take the data. The group is stopped too: by
            // the loader if the server is killed (`kill_running_connector`), by the supervisor
            // if the loader is.
            //
            // Not attempted: hiding the connector's environment from other processes of the
            // same user. `PR_SET_DUMPABLE` here would be undone by the exec that follows, and
            // those processes can read the operator's config anyway.
            #[cfg(target_os = "linux")]
            {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    RUNNING.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    Ok(child)
}

/// A reader that stops the world at `cap` bytes and remembers the last eight it saw.
///
/// The cap bounds memory: arrow's reader allocates per message from the stream it is given, so a
/// reader that refuses to yield more than `cap` bytes bounds every allocation behind it without
/// this code having to parse flatbuffer headers itself.
///
/// The tail is how a truncated stream is caught. Measured on pyarrow 25.0.1: a stream cut at an
/// exact message boundary parses clean and silently returns half the rows, and a connector that
/// dies between pages cuts exactly there. The end-of-stream marker is the only thing that
/// distinguishes "finished" from "stopped".
struct Capped<R> {
    inner: R,
    seen: u64,
    cap: u64,
    tail: [u8; 8],
    over: bool,
    /// The connector closed its end: everything it wrote has been read. When a read fails before
    /// this, benchy stopped reading, and whatever the connector does next is a consequence.
    eof: bool,
    /// Tripped by the control-channel thread when an in-band `estimate` arrives over budget.
    /// Failing the read is how the fetch is cancelled: dropping this closes the pipe, the
    /// connector gets EPIPE, and the upstream system stops doing work it was doing for nothing.
    abort: Arc<Mutex<Option<String>>>,
}

impl<R: Read> Capped<R> {
    fn new(inner: R, cap: u64, abort: Arc<Mutex<Option<String>>>) -> Self {
        Self {
            inner,
            seen: 0,
            cap,
            tail: [0; 8],
            over: false,
            eof: false,
            abort,
        }
    }
    fn ended_cleanly(&self) -> bool {
        self.tail == EOS
    }
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(why) = self.abort.lock().unwrap().clone() {
            return Err(std::io::Error::other(why));
        }
        if self.seen > self.cap {
            self.over = true;
            return Err(std::io::Error::other("byte cap reached"));
        }
        // One byte past the cap may be read, and is refused once it is: a stream of exactly the
        // cap must still be able to find the end of its pipe after its marker
        // (`a_stream_must_end_at_its_end_marker`).
        let room = (self.cap + 1 - self.seen).min(buf.len() as u64) as usize;
        let n = self.inner.read(&mut buf[..room])?;
        self.seen += n as u64;
        if self.seen > self.cap {
            self.over = true;
            return Err(std::io::Error::other("byte cap reached"));
        }
        if n == 0 && room > 0 {
            self.eof = true;
        }
        if n > 0 {
            let got = &buf[..n];
            if got.len() >= 8 {
                self.tail.copy_from_slice(&got[got.len() - 8..]);
            } else {
                self.tail.rotate_left(got.len());
                let at = 8 - got.len();
                self.tail[at..].copy_from_slice(got);
            }
        }
        Ok(n)
    }
}

/// What a fetch produced, with the counts the operator log and REQ-12 both want. The rows
/// themselves went to the fetch's [`Sink`] as they arrived.
pub struct Fetched {
    /// The stream's own schema: the types it was sent in, which the declaration accepted.
    pub schema: arrow::datatypes::SchemaRef,
    /// How many record batches the rows came in.
    pub batches: u64,
    pub rows: u64,
    pub bytes: u64,
    /// `estimate` events seen on stderr before the first batch, for REQ-12.
    pub estimate: Option<(u64, u64)>,
}

/// Events a connector writes on stderr. Anything that is not JSON, or JSON without a `type`, is
/// kept as raw log text and is **not** an error — a Python traceback, a library's logging and a
/// stray `print(file=sys.stderr)` must not break the contract. Non-tolerance here is the largest
/// adoption tax a plugin protocol can levy.
#[derive(Default)]
struct Control {
    /// The **first** error wins, so a late unclassified failure cannot mask the real cause.
    error: Option<Fault>,
    estimate: Option<(u64, u64)>,
    log: String,
}

/// What is kept of a connector's stderr: its first `error` event wherever it comes, and the
/// head and tail of everything else. Keeping only the head lost an error sent after a lot of
/// logging, and showed the operator the start of a log whose traceback is at the end.
#[derive(Default)]
struct StderrKept {
    first_error: Option<String>,
    head: String,
    tail: std::collections::VecDeque<String>,
    tail_bytes: usize,
    dropped: usize,
}

impl StderrKept {
    const HEAD: usize = 128 * 1024;
    const TAIL: usize = 128 * 1024;

    fn push(&mut self, line: &str) {
        if self.first_error.is_none() && is_error_event(line) {
            self.first_error = Some(line.trim_end().to_string());
            return;
        }
        if self.head.len() + line.len() <= Self::HEAD {
            self.head.push_str(line);
            return;
        }
        self.tail.push_back(line.to_string());
        self.tail_bytes += line.len();
        while self.tail_bytes > Self::TAIL {
            let Some(old) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes -= old.len();
            self.dropped += old.len();
        }
    }

    /// The first error first, so `read_control` sees it as the first error, then the log.
    fn into_text(self) -> String {
        let mut out = String::new();
        if let Some(e) = self.first_error {
            out.push_str(&e);
            out.push('\n');
        }
        out.push_str(&self.head);
        if self.dropped > 0 {
            out.push_str(&format!("… {} bytes of stderr not kept …\n", self.dropped));
        }
        for l in self.tail {
            out.push_str(&l);
        }
        out
    }
}

/// The longest stderr line kept whole. An `error` event is a few hundred bytes; a line longer
/// than this is log text, and the rest of it is not kept.
const MAX_STDERR_LINE: usize = 64 * 1024;

/// Each line `r` gives, to `f`, until it ends: at most [`MAX_STDERR_LINE`] bytes of each, the
/// rest read and dropped so the connector is never blocked on a full pipe; bytes that are not
/// UTF-8 replaced, since they are log text too (`a_connectors_stderr_line_is_bounded`).
fn each_line(r: impl Read, mut f: impl FnMut(&str)) {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(r);
    let mut line = Vec::new();
    loop {
        line.clear();
        let Ok(n) = (&mut reader)
            .take(MAX_STDERR_LINE as u64)
            .read_until(b'\n', &mut line)
        else {
            return;
        };
        if n == 0 {
            return;
        }
        if line.last() != Some(&b'\n') && n == MAX_STDERR_LINE {
            // The rest of a line too long to keep, up to and including its newline.
            let mut skip = Vec::new();
            while (&mut reader)
                .take(MAX_STDERR_LINE as u64)
                .read_until(b'\n', &mut skip)
                .is_ok_and(|m| m > 0 && skip.last() != Some(&b'\n'))
            {
                skip.clear();
            }
            line.push(b'\n');
        }
        f(&String::from_utf8_lossy(&line));
    }
}

fn is_error_event(line: &str) -> bool {
    matches!(
        serde_json::from_str::<serde_json::Value>(line.trim()),
        Ok(serde_json::Value::Object(m)) if m.get("type").and_then(|v| v.as_str()) == Some("error")
    )
}

fn read_control(text: &str) -> Control {
    let mut c = Control::default();
    for line in text.lines() {
        let Ok(serde_json::Value::Object(m)) = serde_json::from_str::<serde_json::Value>(line)
        else {
            c.log.push_str(line);
            c.log.push('\n');
            continue;
        };
        match m.get("type").and_then(|v| v.as_str()) {
            Some("error") => {
                if c.error.is_none() {
                    let code = m.get("code").and_then(|v| v.as_str()).unwrap_or("");
                    let mut detail = m
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no message)")
                        .to_string();
                    if let Some(hint) = m.get("hint").and_then(|v| v.as_str()) {
                        detail.push_str(&format!(" (hint: {hint})"));
                    }
                    c.error = Some(Fault {
                        code: known_code(code),
                        detail,
                    });
                } else {
                    c.log.push_str(line);
                    c.log.push('\n');
                }
            }
            Some("estimate") => {
                let rows = m.get("rows").and_then(|v| v.as_u64()).unwrap_or(0);
                let bytes = m.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                c.estimate = Some((rows, bytes));
            }
            // `log`, `progress`, `end`, `state` and anything unknown are operator-facing only.
            _ => {
                c.log.push_str(line);
                c.log.push('\n');
            }
        }
    }
    c
}

/// The closed set of error codes. An unrecognised code becomes `E_INTERNAL` rather than being echoed:
/// the code selects a sentence the host writes, so an unknown one must not reach that lookup.
fn known_code(code: &str) -> &'static str {
    const CODES: &[&str] = &[
        "E_CONFIG",
        "E_AUTH",
        "E_UNAVAILABLE",
        "E_RATE_LIMIT",
        "E_TIMEOUT",
        "E_NARROWING",
        "E_TOO_LARGE",
        "E_SCHEMA",
        "E_INTERNAL",
        "E_CANCELLED",
    ];
    CODES
        .iter()
        .find(|c| **c == code)
        .copied()
        .unwrap_or("E_INTERNAL")
}

/// Run a subcommand that answers with one small JSON document, within a deadline that is real.
///
/// The obvious shape — spawn, read stdout to EOF, then check how long it took — has no timeout
/// at all: the read blocks until the connector closes the pipe, so a connector that hangs hangs
/// the host, and `describe`'s documented 5 s cap is decoration. The pipes are drained on threads
/// and the deadline is enforced by signalling the process group, which is what closes them.
fn run_json(
    src: &ExecSource,
    op: &str,
    run_id: &str,
    input: Option<Vec<u8>>,
    within: Duration,
) -> Result<(Vec<u8>, Vec<u8>, bool), Fault> {
    let scratch = Scratch::new(&src.name)?;
    let mut child = spawn(src, op, run_id, scratch.path()).map_err(|e| {
        Fault::host(
            "E_CONFIG",
            format!("could not start {}: {e}", src.command.display()),
        )
    })?;
    let stdin = child.stdin.take();
    match input {
        Some(doc) => {
            if let Some(mut w) = stdin {
                std::thread::spawn(move || {
                    let _ = w.write_all(&doc);
                });
            }
        }
        // `describe` gets stdin closed immediately: it has nothing to read and must not wait.
        None => drop(stdin),
    }
    let out_pipe = child.stdout.take().unwrap();
    let err_pipe = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || read_capped(out_pipe, 256 * 1024));
    let errs = std::thread::spawn(move || read_capped(err_pipe, 256 * 1024));

    let status = wait_within(&mut child, within, src.limits.cancel_grace);
    // Done or stopped: anything it started and left behind in its group goes too.
    unsafe { libc::killpg(child.id() as i32, libc::SIGKILL) };
    let timed_out = status.is_err();
    // The wait killed the group on expiry, so both pipes are closed and these joins return.
    let (o, e) = (
        out.join().unwrap_or_default(),
        errs.join().unwrap_or_default(),
    );
    match status {
        Ok(st) => Ok((o, e, st.success())),
        Err(f) if timed_out => Err(f),
        Err(f) => Err(f),
    }
}

/// Ask the connector what it is. Pure by contract: no network, no credentials, no side effects.
pub fn describe(src: &ExecSource) -> Result<Describe, Fault> {
    check_permissions(&src.command)?;
    let (out, err, ok) = run_json(
        src,
        "describe",
        "describe",
        None,
        src.limits.describe_timeout,
    )?;
    let control = read_control(&String::from_utf8_lossy(&err));
    if let Some(mut f) = control.error {
        f.detail = format!("{} [describe] {}", src.name, f.detail);
        return Err(f);
    }
    if !ok {
        return Err(Fault::host(
            "E_HOST_PROTOCOL",
            format!(
                "{}: `describe` failed. stderr: {}",
                src.name,
                control.log.trim()
            ),
        ));
    }
    let d: Describe = serde_json::from_slice(&out).map_err(|e| {
        Fault::host(
            "E_HOST_PROTOCOL",
            format!(
                "{}: `describe` did not return a JSON document: {e}",
                src.name
            ),
        )
    })?;
    if !d.protocol.contains(&1) {
        return Err(Fault::host(
            "E_HOST_COMPAT",
            format!(
                "{}: connector speaks protocol {:?}, this benchy speaks 1",
                src.name, d.protocol
            ),
        ));
    }
    if !is_slug(&d.name) || !is_version(&d.version) {
        return Err(Fault::host(
            "E_HOST_PROTOCOL",
            format!(
                "{}: connector reported name {:?} version {:?}; `name` must match \
                 [a-z0-9][a-z0-9-]{{0,62}} and `version` [A-Za-z0-9.+-]{{1,32}}",
                src.name, d.name, d.version
            ),
        ));
    }
    Ok(d)
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

fn read_capped(mut r: impl Read + Send + 'static, cap: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.by_ref().take(cap as u64).read_to_end(&mut buf);
    buf
}

/// Wait, then escalate: SIGTERM to the process *group*, a grace period, then SIGKILL.
fn wait_within(
    child: &mut std::process::Child,
    within: Duration,
    grace: Duration,
) -> Result<std::process::ExitStatus, Fault> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return Ok(s),
            Ok(None) => {}
            Err(e) => return Err(Fault::host("E_HOST_PROTOCOL", e.to_string())),
        }
        if start.elapsed() > within {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Past the deadline. setsid() put the connector in its own group, so this reaches its
    // children too — a connector whose own subprocess is running the expensive query included.
    //
    // Everything from here returns a timeout, including the case where it exits politely during
    // the grace period: it did not finish within the deadline on its own, and reporting the
    // resulting signal status as an ordinary failure would attribute our decision to the
    // connector.
    let pid = child.id() as i32;
    unsafe { libc::killpg(pid, libc::SIGTERM) };
    let grace_start = Instant::now();
    while grace_start.elapsed() < grace {
        if let Ok(Some(_)) = child.try_wait() {
            return Err(Fault::host(
                "E_HOST_TIMEOUT",
                format!("connector did not finish within {within:?} and was stopped"),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
    Err(Fault::host(
        "E_HOST_TIMEOUT",
        format!("connector did not finish within {within:?} and was killed"),
    ))
}

/// A per-fetch scratch directory, mode 0700, removed when this drops. It is the connector's cwd
/// and its `TMPDIR`, so an SDK's temp files land somewhere we delete. The host never reads it.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Result<Self, Fault> {
        let p = std::env::temp_dir().join(format!(
            "benchy-exec-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p)
            .map_err(|e| Fault::host("E_HOST_PROTOCOL", format!("scratch: {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Self(p))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the operator, or a host, narrowed a fetch to — never the model: what a source fetches is
/// decided by whoever runs benchy. The values are still treated as untrusted, since a host may
/// take them from its own users: they travel in the input **document**, never in argv, so a
/// connector cannot be tricked into treating an entity id as an option.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Narrowing {
    pub time: Option<(String, Option<String>, Option<String>)>,
    pub entities: Option<(String, Vec<String>)>,
    pub columns: Option<Vec<String>>,
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// Entity ids are untrusted — whoever wrote them, a connector puts them into a query. Bound them
/// before a connector ever sees one.
pub fn vet_entities(ids: &[String]) -> Result<(), Fault> {
    if ids.len() > 1024 {
        return Err(Fault::host(
            "E_NARROWING",
            format!("{} entity ids; at most 1024", ids.len()),
        ));
    }
    for id in ids {
        if id.len() > 512 {
            return Err(Fault::host("E_NARROWING", "an entity id exceeds 512 bytes"));
        }
        if id
            .chars()
            .any(|c| c == '\0' || (c.is_control() && c != '\t'))
        {
            return Err(Fault::host(
                "E_NARROWING",
                "an entity id contains a NUL or a control character",
            ));
        }
    }
    Ok(())
}

/// The input document for `op`, or why it cannot be sent. Everything that can refuse a request
/// is checked here, before any connector is started: a refusal after the start would leave a
/// connector running for a request nobody is going to read.
fn request(src: &ExecSource, op: &str, run_id: &str, n: &Narrowing) -> Result<Vec<u8>, Fault> {
    if let Some((_, ids)) = &n.entities {
        vet_entities(ids)?;
    }
    let doc = serde_json::to_vec(&input_document(src, op, run_id, n))
        .map_err(|e| Fault::host("E_INTERNAL", e.to_string()))?;
    if doc.len() > 1024 * 1024 {
        return Err(Fault::host(
            "E_NARROWING",
            "the narrowing does not fit the 1 MiB input document",
        ));
    }
    Ok(doc)
}

fn input_document(src: &ExecSource, op: &str, run_id: &str, n: &Narrowing) -> serde_json::Value {
    let mut narrowing = serde_json::Map::new();
    if let Some((col, start, end)) = &n.time {
        narrowing.insert(
            "time".into(),
            serde_json::json!({ "column": col, "start": start, "end": end }),
        );
    }
    if let Some((col, ids)) = &n.entities {
        narrowing.insert(
            "entities".into(),
            serde_json::json!({ "column": col, "ids": ids }),
        );
    }
    if let Some(cols) = &n.columns {
        narrowing.insert("columns".into(), serde_json::json!(cols));
    }
    narrowing.insert("params".into(), serde_json::Value::Object(n.params.clone()));

    serde_json::json!({
        "protocol": 1,
        // GREASE, in the TLS sense. The protocol says a connector MUST ignore unknown keys, and that
        // rule is what makes additive evolution possible — but a rule only tested on the day it
        // is first needed is a rule that fails on that day, in production, for everyone at once.
        // Sending a reserved key on *every* document means a connector that cannot tolerate one
        // breaks immediately and always, while its author is still looking at it.
        "_benchy_reserved": serde_json::Value::Null,
        "op": op,
        "run": { "id": run_id, "attempt": 1 },
        "source": { "name": src.name, "config": src.config },
        "narrowing": narrowing,
        "schema": src.schema.iter().map(|f| serde_json::json!({
            "name": f.name, "type": f.ty, "nullable": f.nullable
        })).collect::<Vec<_>>(),
        "state": serde_json::Value::Null,
        "limits": {
            "max_rows": src.limits.max_rows,
            "max_bytes": src.limits.max_bytes,
            "max_batch_bytes": src.limits.max_batch_bytes,
            "deadline_ms": src.limits.deadline.as_millis() as u64,
            "cancel_grace_ms": src.limits.cancel_grace.as_millis() as u64,
        }
    })
}

/// REQ-12. `estimate` is asked *before* any bytes are fetched when the connector implements the
/// subcommand, so an over-budget load costs the upstream system one cheap call instead of a full
/// scan. A connector that declares `inband` instead sends its estimate on stderr before the
/// first batch, and this cancels mid-stream — later, but still before most of the work.
///
/// Each validated batch goes to `sink` as it arrives. A fetch that then fails — the connector
/// reports an error, exits non-zero, or never sends its end-of-stream marker — has still sent
/// the sink what came before: the caller decides nothing until this returns `Ok`.
pub fn fetch_within(
    src: &ExecSource,
    n: &Narrowing,
    run_id: &str,
    budget: &crate::admission::Budget,
    sink: &mut Sink,
) -> Result<Fetched, Fault> {
    // The pass budget bounds what is read, not only what was estimated: a connector that does
    // not estimate would otherwise be bounded by its own caps alone. The tighter caps go into
    // the input document too, so the connector is told the limit that will be enforced.
    let fetch_started = Instant::now();
    let (budget_rows, budget_bytes) = budget.remaining();
    let rows_by_budget = budget_rows < src.limits.max_rows;
    let bytes_by_budget = budget_bytes < src.limits.max_bytes;
    let mut src = src.clone();
    src.limits.max_rows = src.limits.max_rows.min(budget_rows);
    src.limits.max_bytes = src.limits.max_bytes.min(budget_bytes);
    let src = &src;

    check_permissions(&src.command)?;
    let doc = request(src, "fetch", run_id, n)?;
    let declared = declared_schema(src)?;

    let scratch = Scratch::new(&src.name)?;
    let mut child = spawn(src, "fetch", run_id, scratch.path()).map_err(|e| {
        // The command is the operator's, so a command that will not start is a config error and
        // must read as one — not as a protocol failure the connector could be blamed for.
        Fault::host(
            "E_CONFIG",
            format!("could not start {}: {e}", src.command.display()),
        )
    })?;

    // Write the document on a thread: a connector that never reads stdin must not wedge us.
    let mut stdin = child.stdin.take().unwrap();
    std::thread::spawn(move || {
        let _ = stdin.write_all(&doc);
        // EOF is the connector's signal that the document is complete.
    });

    // stderr on its own thread, or a chatty connector fills the pipe and deadlocks the fetch.
    // Line-oriented rather than read-to-end, so an in-band `estimate` can stop the fetch while
    // it is still happening instead of being read once it is too late to matter.
    let stderr = child.stderr.take().unwrap();
    let abort: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    // The watcher needs to *stop* the connector, not merely record that it should be stopped.
    // Setting a flag is not enough: a connector that estimates and then goes quiet leaves us
    // blocked reading a pipe that never delivers, so the flag is checked only once it no longer
    // matters. Signalling the group is what actually ends it.
    let pid = child.id() as i32;
    let (name, rows_left, bytes_left) = {
        let (r, b) = budget.remaining();
        (src.name.clone(), r, b)
    };
    let watcher = abort.clone();
    let errs = std::thread::spawn(move || {
        let mut kept = StderrKept::default();
        each_line(stderr, |line| {
            kept.push(line);
            if let Ok(serde_json::Value::Object(m)) = serde_json::from_str(line.trim()) {
                if m.get("type").and_then(|v| v.as_str()) == Some("estimate") {
                    let r = m.get("rows").and_then(|v| v.as_u64()).unwrap_or(0);
                    let b = m.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    if r > rows_left || b > bytes_left {
                        *watcher.lock().unwrap() = Some(format!(
                            "source `{name}` estimated {r} row(s) / {b} byte(s) mid-stream, \
                             over the remaining budget of {rows_left} / {bytes_left}; cancelled"
                        ));
                        // setsid() put it in its own group, so this reaches the connector's own
                        // subprocesses too — including whatever is running the expensive query.
                        unsafe { libc::killpg(pid, libc::SIGTERM) };
                    }
                }
            }
        });
        kept.into_text()
    });

    // The deadline has to act *during* the read, not after it. `read_stream` blocks until the
    // connector closes stdout, so a connector that hangs holds the host for as long as it likes
    // and `limits.deadline` never gets a chance to mean anything. Signalling the group is what
    // closes the pipe, which is what ends the read.
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = {
        let done = done.clone();
        let cancel = abort.clone();
        let (deadline, grace) = (src.limits.deadline, src.limits.cancel_grace);
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut cancelled_at: Option<Instant> = None;
            while start.elapsed() < deadline {
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    return false;
                }
                // A cancellation sends SIGTERM, and a connector — or anything it runs — may
                // ignore it. After the cancel grace it is killed, as the protocol says, rather
                // than holding the fetch until the deadline.
                if cancelled_at.is_none() && cancel.lock().is_ok_and(|c| c.is_some()) {
                    cancelled_at = Some(Instant::now());
                }
                if cancelled_at.is_some_and(|t| t.elapsed() >= grace) {
                    unsafe { libc::killpg(pid, libc::SIGKILL) };
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if done.load(std::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            unsafe { libc::killpg(pid, libc::SIGTERM) };
            std::thread::sleep(grace);
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            true
        })
    };

    let stdout = child.stdout.take().unwrap();
    let mut capped = Capped::new(stdout, src.limits.max_bytes, abort.clone());
    let mut rows_read = 0;
    let read = read_stream(
        &mut capped,
        Some(&declared),
        src.limits.max_rows,
        src.limits.max_bytes,
        &mut rows_read,
        sink,
        |r| nothing_after_the_connectors_marker(r, &mut child, pid),
        " on stdout",
    );
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let timed_out = watchdog.join().unwrap_or(false);
    let clean = capped.ended_cleanly();
    let seen = capped.seen;
    let reached_eof = capped.eof;
    drop(capped); // closes our end; a connector still writing gets EPIPE

    // What was read cost the source, whatever is decided below. With a partial start a failed
    // source no longer ends the session, so a failure that spent nothing left the whole budget
    // to the next source: three 1 MB sources under a 1.5 MB budget fetched about 2.5 MB,
    // measured (`a_failed_fetch_still_spends_what_it_read`).
    budget.spend(rows_read, seen);

    // A cancellation is decided *here*, before anything that waits.
    //
    // Everything downstream would otherwise misreport it — a cancelled fetch looks exactly like
    // a truncated stream and a non-zero exit. And more importantly, nothing below may block on
    // the connector: we have already decided not to pay for it, so waiting for it to notice
    // would be paying for it anyway. The stderr thread is left to end when the pipe closes.
    if timed_out {
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
        drop(errs);
        return Err(Fault::host(
            "E_HOST_TIMEOUT",
            format!(
                "source `{}` did not finish within {:?} and was stopped",
                src.name, src.limits.deadline
            ),
        ));
    }

    if let Some(why) = abort.lock().unwrap().clone() {
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
        drop(errs);
        return Err(Fault::host("E_TOO_LARGE", why));
    }

    // Over a cap is our decision too. Like a cancellation it must not wait on the connector,
    // which may be blocked writing to a pipe we stopped reading, and must not be reported as
    // the connector's failure when it exits on the broken pipe.
    let read = match read {
        Err(f) if f.code == "E_TOO_LARGE" || f.code == "E_HOST_OVERSIZE" => {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            let _ = child.kill();
            let _ = child.wait();
            drop(errs);
            // Which cap it passed, and whose it was: the source's own, or what the session had
            // left. Either being the budget's used to blame the budget for both
            // (`a_sources_own_cap_is_not_called_the_sessions_budget`).
            let over_rows = rows_read > src.limits.max_rows;
            if !(if over_rows {
                rows_by_budget
            } else {
                bytes_by_budget
            }) {
                return Err(f);
            }
            return Err(Fault::host(
                "E_TOO_LARGE",
                format!(
                    "source `{}` returned more than the {} row(s) / {} byte(s) left in this \
                     session's budget. Refused, not truncated: nothing of it was loaded.",
                    src.name, src.limits.max_rows, src.limits.max_bytes
                ),
            ));
        }
        // Any other refusal reached before the connector closed its end is ours too: benchy read
        // a schema, a batch or a header it will not accept, and stopped reading. What the
        // connector does next — die of SIGPIPE (a shell reports 141), exit 23 as curl does, or
        // catch the broken pipe and send an `error` event about it — is a consequence of that,
        // and reporting it would hide the reason the load was refused. So, like a cap, it is
        // decided here and the connector is stopped rather than waited for. Its stderr is still
        // read, for the operator: the pipe keeps what was written before the kill.
        Err(f) if !reached_eof => {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            let _ = child.kill();
            let status = child.wait().ok();
            let said = read_control(&join_within(errs, src.limits.cancel_grace));
            let mut f = f;
            f.detail = format!("{}: {}", src.name, f.detail);
            if let Some(e) = said.error {
                f.detail.push_str(&format!(
                    " | after benchy stopped reading, the connector reported [{}] {}",
                    e.code, e.detail
                ));
            }
            if let Some(code) = status.and_then(|s| s.code()).filter(|c| *c != 0) {
                f.detail
                    .push_str(&format!(" | it then exited {code}, which is not its fault"));
            }
            if !said.log.trim().is_empty() {
                f.detail
                    .push_str(&format!(" | stderr: {}", said.log.trim()));
            }
            return Err(f);
        }
        other => other,
    };

    // What the deadline has left, not the whole of it again — with a moment to exit in, since
    // the stream has already ended.
    let left = src
        .limits
        .deadline
        .saturating_sub(fetch_started.elapsed())
        .max(Duration::from_secs(1));
    let status = wait_within(&mut child, left, src.limits.cancel_grace);
    // The connector is done; anything it started and left behind in its group goes too.
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    let control = read_control(&errs.join().unwrap_or_default());

    // The connector's own verdict wins over ours: it knows why it failed, we only know that the
    // bytes stopped. An `error` event with exit 0 still fails the load — the error wins.
    if let Some(mut f) = control.error {
        f.detail = format!(
            "{}: {} | stderr: {}",
            src.name,
            f.detail,
            control.log.trim()
        );
        return Err(f);
    }
    let status = status?;
    if !status.success() {
        return Err(Fault::host(
            "E_HOST_UNCLASSIFIED",
            format!(
                "{}: connector exited {:?} without an error event. stderr: {}",
                src.name,
                status.code(),
                control.log.trim()
            ),
        ));
    }

    let Stream {
        schema,
        batches,
        rows,
    } = read?;
    if !clean {
        // The measured case: a clean-boundary cut parses without complaint and returns half the
        // rows. Only the end-of-stream marker separates "finished" from "stopped".
        return Err(Fault::host(
            "E_HOST_TRUNCATED",
            format!(
                "{}: the stream ended without its end-of-stream marker after {rows} row(s). \
                 The connector exited 0, so it believes it succeeded — it is most likely dying \
                 between pages and not reporting it.",
                src.name
            ),
        ));
    }
    Ok(Fetched {
        schema,
        batches,
        rows,
        bytes: seen,
        estimate: control.estimate,
    })
}

/// What the stderr thread kept, if it ends within `within`. It ends when the pipe closes, which a
/// process outside the connector's group — something it daemonised — can put off indefinitely,
/// and an operator's log is not worth a hung load.
fn join_within(t: std::thread::JoinHandle<String>, within: Duration) -> String {
    let start = Instant::now();
    while !t.is_finished() && start.elapsed() < within {
        std::thread::sleep(Duration::from_millis(5));
    }
    if t.is_finished() {
        t.join().unwrap_or_default()
    } else {
        String::new()
    }
}

pub fn declared_schema(src: &ExecSource) -> Result<Schema, Fault> {
    if src.schema.is_empty() || src.schema.len() > 512 {
        return Err(Fault::host(
            "E_CONFIG",
            format!(
                "{}: the declared schema has {} fields; 1 to 512 are allowed",
                src.name,
                src.schema.len()
            ),
        ));
    }
    let mut fields = Vec::with_capacity(src.schema.len());
    for f in &src.schema {
        let Some(ty) = arrow_type(&f.ty) else {
            return Err(Fault::host(
                "E_CONFIG",
                format!(
                    "{}: column `{}` declares type `{}`, which is not in the vocabulary. See \
                     the \"Types\" table in docs/EXEC-CONNECTOR-PROTOCOL.md.",
                    src.name, f.name, f.ty
                ),
            ));
        };
        fields.push(Field::new(&f.name, ty, f.nullable));
    }
    Ok(Schema::new(fields))
}

/// The vocabulary token for an Arrow type, when it has one: [`arrow_type`] read backwards, with
/// every encoding [`type_ok`] accepts for it — a string however it is laid out, a timestamp at
/// any precision. A host declares nothing, so this is its stream's declaration.
pub fn vocabulary_token(t: &DataType) -> Option<String> {
    let token = match t {
        DataType::Boolean => "bool".to_string(),
        DataType::Int8 => "int8".into(),
        DataType::Int16 => "int16".into(),
        DataType::Int32 => "int32".into(),
        DataType::Int64 => "int64".into(),
        DataType::UInt8 => "uint8".into(),
        DataType::UInt16 => "uint16".into(),
        DataType::UInt32 => "uint32".into(),
        DataType::UInt64 => "uint64".into(),
        DataType::Float32 => "float32".into(),
        DataType::Float64 => "float64".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "string".into(),
        DataType::Dictionary(_, v) if is_string(v) => "string".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "binary".into(),
        DataType::Date32 => "date".into(),
        DataType::Time64(TimeUnit::Microsecond) => "time".into(),
        DataType::Timestamp(_, None) => "timestamp".into(),
        DataType::Timestamp(_, Some(_)) => "timestamptz".into(),
        DataType::Decimal128(p, s) => format!("decimal({p},{s})"),
        _ => return None,
    };
    arrow_type(&token).map(|_| token)
}

fn is_string(t: &DataType) -> bool {
    matches!(t, DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View)
}

/// Where a stream's validated batches go as they arrive, in order. A stream is never collected
/// first: 10,000,000 rows from ClickHouse held as a list of batches until the stream ended peaked
/// at 940 MB in the loader, against 710 MB for the same rows from a CSV file (docs/CAPACITY.md).
pub type Sink<'a> = dyn FnMut(RecordBatch) -> Result<(), Fault> + 'a;

/// What a validated stream was: its schema as sent, how many batches and how many rows. The
/// batches themselves went to a [`Sink`].
pub struct Stream {
    pub schema: arrow::datatypes::SchemaRef,
    pub batches: u64,
    pub rows: u64,
}

/// A host's Arrow stream, held to what a connector's is held to — against its own schema, since a
/// host declares none. Every column's type must be in the vocabulary and every name present and
/// distinct; the end-of-stream marker must be there, and the whole within the caps. Nothing in a
/// refusal names the file: the host knows which one it sent. Opened first, to read the
/// declaration from the stream's schema, and then read with [`HostStream::read`].
///
/// It comes as a file ([`open_host_stream`]) or through a pipe the host writes into as it fetches
/// ([`open_host_pipe`], protocol 3). Both are held to the same rules and go into the table by
/// the same path; they differ only in when a rule can be checked. A file's size and its marker
/// are known before a batch is read; a pipe's bytes are counted as they arrive, and its marker
/// is found where it ends.
pub struct HostStream {
    /// The declaration read from the stream.
    pub decls: Vec<FieldDecl>,
    /// Its size in bytes, when that is known before it is read: a file's.
    pub bytes: Option<u64>,
    declared: Schema,
    input: HostInput,
    max_bytes: u64,
}

enum HostInput {
    File(std::fs::File),
    /// A pipe, its schema already read: it cannot be read again from the start.
    Pipe {
        capped: Capped<Box<dyn Read>>,
        /// Boxed: the decoder is most of a stream's state, and a file needs none of it.
        opened: Box<Opened>,
        /// What the session's budget had left, in bytes, when the attach began.
        budget_bytes: u64,
    },
}

/// What [`open_stream`] read of a stream: its schema, the decoder that read it, and what has been
/// decoded so far.
type Opened = (
    arrow::datatypes::SchemaRef,
    arrow::ipc::reader::StreamDecoder,
    crate::ipc::Decoded,
);

/// Open a host's Arrow stream that comes through a pipe (protocol 3), reading its schema. Its
/// bytes are counted as they are read, against what an attachment may be (`max_bytes`) and what
/// the session's budget has left (`budget_bytes`), and the read stops at the first byte past
/// either: the bounds a file's size is checked against before it is read.
///
/// The pipe is the host's own for this one stream, or its stream channel, read through
/// [`crate::ipc::Framed`]: there the end of the stream reads as the end of the input, so "nothing
/// after the marker" holds of the stream, and the next one waits in the channel.
pub fn open_host_pipe(
    pipe: Box<dyn Read>,
    max_bytes: u64,
    budget_bytes: u64,
) -> Result<HostStream, Fault> {
    let data = |why: String| Fault::host("E_DATA", why);
    let mut capped = Capped::new(
        pipe,
        max_bytes.min(budget_bytes),
        Arc::new(Mutex::new(None)),
    );
    let opened = match open_stream(&mut capped, max_bytes, "") {
        Ok(o) => o,
        Err(_) if capped.over => return Err(pipe_over(max_bytes, budget_bytes)),
        Err(e) => return Err(data(format!("it is not an Arrow IPC stream: {}", e.detail))),
    };
    let decls = decls_from_stream(&opened.0).map_err(data)?;
    let declared = schema_of(&decls);
    Ok(HostStream {
        decls,
        bytes: None,
        declared,
        input: HostInput::Pipe {
            capped,
            opened: Box::new(opened),
            budget_bytes,
        },
        max_bytes,
    })
}

/// A pipe's stream ran past what it may be: an attachment's bytes, or what the session's budget
/// has left, whichever is less — the refusals a file's size gets.
fn pipe_over(max_bytes: u64, budget_bytes: u64) -> Fault {
    let why = if budget_bytes < max_bytes {
        format!(
            "it is more than {budget_bytes} bytes, and this session has {budget_bytes} bytes of \
             its budget left"
        )
    } else {
        format!("it is more than {max_bytes} bytes, and an attachment may be at most {max_bytes}")
    };
    Fault::host("E_LIMIT", why)
}

/// A host's stream that does not end with its marker, file or pipe: the same refusal.
const CUT_SHORT: &str = "it does not end with the Arrow end-of-stream marker, so it may have been \
                         cut short; write it as an IPC stream and finish the writer";

/// Open a host's Arrow stream, checking its size, its end-of-stream marker and its schema.
pub fn open_host_stream(path: &Path, max_bytes: u64) -> Result<HostStream, Fault> {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let data = |why: String| Fault::host("E_DATA", why);
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| data(format!("it could not be opened: {}", e.kind())))?;
    let md = f
        .metadata()
        .map_err(|e| data(format!("it could not be read: {}", e.kind())))?;
    if !md.is_file() {
        return Err(data("it is not a regular file".into()));
    }
    let bytes = md.len();
    if bytes > max_bytes {
        return Err(Fault::host(
            "E_LIMIT",
            format!("it is {bytes} bytes, and an attachment may be at most {max_bytes}"),
        ));
    }
    // A stream cut at a message boundary parses as a valid, shorter one; only the marker tells
    // them apart.
    let mut tail = [0u8; 8];
    let ends = bytes >= 8
        && f.seek(SeekFrom::End(-8)).is_ok()
        && f.read_exact(&mut tail).is_ok()
        && tail == EOS;
    if !ends {
        return Err(data(CUT_SHORT.into()));
    }
    f.seek(SeekFrom::Start(0))
        .map_err(|e| data(format!("it could not be read: {}", e.kind())))?;
    let schema = open_stream(&mut f, max_bytes, "")
        .map_err(|e| data(format!("it is not an Arrow IPC stream: {}", e.detail)))?
        .0;
    let decls = decls_from_stream(&schema).map_err(data)?;
    let declared = schema_of(&decls);
    Ok(HostStream {
        decls,
        bytes: Some(bytes),
        declared,
        input: HostInput::File(f),
        max_bytes,
    })
}

/// The declaration a stream that comes without one makes of its own schema — a host's, or an
/// undeclared HTTP source's: every column named, once, and typed from the vocabulary, all
/// nullable. Nothing in a refusal names where the stream came from.
pub fn decls_from_stream(schema: &Schema) -> Result<Vec<FieldDecl>, String> {
    if schema.fields().is_empty() || schema.fields().len() > 512 {
        return Err(format!(
            "it has {} columns; 1 to 512 are allowed",
            schema.fields().len()
        ));
    }
    let mut decls: Vec<FieldDecl> = Vec::with_capacity(schema.fields().len());
    for field in schema.fields() {
        let name = field.name();
        if name.is_empty() {
            return Err("a column has no name".into());
        }
        if decls
            .iter()
            // ASCII only, as DuckDB folds: `Ä` and `ä` are two columns to it.
            .any(|d| d.name.eq_ignore_ascii_case(name))
        {
            return Err(format!(
                "the column `{name}` is there twice: DuckDB does not tell names apart by case"
            ));
        }
        let Some(ty) = vocabulary_token(field.data_type()) else {
            return Err(format!(
                "column `{name}` is {:?}, which is not in the type vocabulary; see \"Types\" in \
                 docs/EXEC-CONNECTOR-PROTOCOL.md",
                field.data_type()
            ));
        };
        decls.push(FieldDecl {
            name: name.clone(),
            ty,
            nullable: true,
        });
    }
    Ok(decls)
}

/// A declaration as the Arrow schema a stream is checked against. Every token is from the
/// vocabulary: checked when the declaration was made.
pub fn schema_of(decls: &[FieldDecl]) -> Schema {
    Schema::new(
        decls
            .iter()
            .map(|d| {
                Field::new(
                    &d.name,
                    arrow_type(&d.ty).expect("from the vocabulary"),
                    d.nullable,
                )
            })
            .collect::<Vec<_>>(),
    )
}

impl HostStream {
    /// Read the stream, every batch to `sink` as it is validated. Returns what it was and its
    /// size in bytes: a file's, or what came through the pipe up to its end.
    pub fn read(self, max_rows: u64, sink: &mut Sink) -> Result<(Stream, u64), Fault> {
        use std::io::{Seek, SeekFrom};
        let max_bytes = self.max_bytes;
        let refused = |e: Fault| match e.code {
            "E_HOST_OVERSIZE" | "E_TOO_LARGE" => Fault::host(
                "E_LIMIT",
                format!(
                    "it holds more than an attachment may: {max_rows} rows, or {max_bytes} bytes \
                     once decoded. {}",
                    e.detail
                ),
            ),
            _ => Fault::host("E_DATA", e.detail),
        };
        match self.input {
            HostInput::File(mut file) => {
                file.seek(SeekFrom::Start(0)).map_err(|e| {
                    Fault::host("E_DATA", format!("it could not be read: {}", e.kind()))
                })?;
                let stream = read_stream(
                    &mut file,
                    Some(&self.declared),
                    max_rows,
                    max_bytes,
                    &mut 0,
                    sink,
                    nothing_after_the_marker,
                    "",
                )
                .map_err(refused)?;
                Ok((stream, self.bytes.unwrap_or(0)))
            }
            HostInput::Pipe {
                mut capped,
                opened,
                budget_bytes,
            } => {
                let read = read_opened(
                    &mut capped,
                    *opened,
                    Some(&self.declared),
                    max_rows,
                    &mut 0,
                    sink,
                    nothing_after_the_marker,
                );
                // Past the bytes it may be, whatever the read made of being stopped there.
                if capped.over {
                    return Err(pipe_over(max_bytes, budget_bytes));
                }
                let stream = read.map_err(refused)?;
                // A pipe closed at a message boundary reads as a shorter stream: only the marker
                // tells the two apart, as for a file.
                if !capped.ended_cleanly() {
                    return Err(Fault::host("E_DATA", CUT_SHORT));
                }
                Ok((stream, capped.seen))
            }
        }
    }
}

/// The stream's first message, which must be its schema: the schema, the decoder that read it,
/// and what has been decoded so far. A stream that fails here is reported as no stream at all —
/// zero bytes with exit 0 lands here too, and must never be read as "no rows": an empty result is
/// a *complete stream with no batches* — unless it was refused for its size or its compression.
///
/// `on` says where the stream came from, in the refusal's words: a connector's stdout, or nothing
/// for a host's input or a built-in source's body, which come through no stdout.
fn open_stream<R: Read>(r: &mut R, max_decoded: u64, on: &str) -> Result<Opened, Fault> {
    let no_schema = |why: String| {
        Fault::host(
            "E_HOST_NO_SCHEMA",
            format!("no readable Arrow stream{on}: {why}"),
        )
    };
    let mut decoded = crate::ipc::Decoded::new(max_decoded);
    let first = match crate::ipc::next(r, &mut decoded) {
        Ok(Some(m)) => m,
        Ok(None) => return Err(no_schema("it is empty".into())),
        Err(f)
            if matches!(
                f.code,
                "E_HOST_OVERSIZE" | "E_TOO_LARGE" | "E_HOST_COMPRESSED"
            ) =>
        {
            return Err(f)
        }
        Err(f) => return Err(no_schema(f.detail)),
    };
    let schema = first
        .schema
        .ok_or_else(|| no_schema("its first message is not a schema".into()))?;
    let mut decoder = arrow::ipc::reader::StreamDecoder::new();
    let mut buffer = arrow::buffer::Buffer::from_vec(first.bytes);
    while !buffer.is_empty() {
        decoder
            .decode(&mut buffer)
            .map_err(|e| no_schema(e.to_string()))?;
    }
    Ok((std::sync::Arc::new(schema), decoder, decoded))
}

/// An Arrow stream from a built-in source (`crate::sources`), held to exactly what a connector's
/// stdout is held to: the declared schema, the row and byte caps, no body compression, no
/// dictionary, and the end-of-stream marker. Read as it arrives, so nothing buffers the whole
/// body first.
///
/// What was read is spent on `budget` whether or not the stream is accepted, as a connector's
/// fetch is (`a_failed_fetch_still_spends_what_it_read`). Each batch goes to `sink` as it is
/// validated; nothing is accepted until this returns `Ok`. Without a declaration, the stream's
/// own schema is its declaration, held to the vocabulary as a host's is ([`decls_from_stream`]).
pub fn read_arrow<R: Read>(
    r: R,
    declared: Option<&Schema>,
    max_rows: u64,
    max_bytes: u64,
    budget: &crate::admission::Budget,
    sink: &mut Sink,
) -> Result<Fetched, Fault> {
    let mut capped = Capped::new(r, max_bytes, Arc::new(Mutex::new(None)));
    let mut rows_read = 0;
    let read = read_stream(
        &mut capped,
        declared,
        max_rows,
        max_bytes,
        &mut rows_read,
        sink,
        nothing_after_the_marker,
        "",
    );
    budget.spend(rows_read, capped.seen);
    let Stream {
        schema,
        batches,
        rows,
    } = read?;
    if !capped.ended_cleanly() {
        return Err(Fault::host(
            "E_HOST_TRUNCATED",
            format!("the stream ended without its end-of-stream marker after {rows} row(s)"),
        ));
    }
    Ok(Fetched {
        schema,
        batches,
        rows,
        bytes: capped.seen,
        estimate: None,
    })
}

/// Read the stream, validating every batch against the operator's declaration.
///
/// `max_decoded` bounds what the stream decodes to, not only what it is: a dictionary-encoded
/// or view column is unpacked before it is loaded, and a few bytes of keys or views can each name
/// a long value, so its unpacked size is counted as each batch arrives — the sum of the value
/// lengths its keys pick or its views state, and an offset per row — and the stream refused once
/// that passes the bound. 1,000,000 `int32` keys into one 1 KiB value are a 4 MB stream and 1 GB
/// unpacked ([`unpacked_size`]). What they unpack to and what the bodies decode to are one total
/// against the bound, not two ([`crate::ipc::Decoded::add_unpacked`]).
///
/// `rows` counts the rows read, refused or not: a fetch that fails still cost its source them.
/// Each batch goes to `sink` once it has passed every check, before the next is read, and a
/// sink's refusal ends the read. `after_marker` checks that the input ends where the stream does;
/// `on` is where it came from, as [`open_stream`] takes it.
#[allow(clippy::too_many_arguments)]
fn read_stream<R: Read>(
    r: &mut R,
    declared: Option<&Schema>,
    max_rows: u64,
    max_decoded: u64,
    rows: &mut u64,
    sink: &mut Sink,
    after_marker: impl FnOnce(&mut R) -> Result<(), Fault>,
    on: &str,
) -> Result<Stream, Fault> {
    let opened = open_stream(r, max_decoded, on)?;
    read_opened(r, opened, declared, max_rows, rows, sink, after_marker)
}

/// [`read_stream`] from its first batch on, the schema already read by [`open_stream`].
#[allow(clippy::too_many_arguments)]
fn read_opened<R: Read>(
    r: &mut R,
    (schema, mut decoder, mut decoded): Opened,
    declared: Option<&Schema>,
    max_rows: u64,
    rows: &mut u64,
    sink: &mut Sink,
    after_marker: impl FnOnce(&mut R) -> Result<(), Fault>,
) -> Result<Stream, Fault> {
    let own;
    let declared = match declared {
        Some(d) => {
            check_schema(schema.as_ref(), d)?;
            d
        }
        None => {
            own = schema_of(
                &decls_from_stream(&schema)
                    .map_err(|why| Fault::host("E_HOST_SCHEMA_MISMATCH", why))?,
            );
            &own
        }
    };

    let mut batches = 0u64;
    let bad = |e: arrow::error::ArrowError| {
        Fault::host("E_HOST_PROTOCOL", format!("bad Arrow stream: {e}"))
    };
    // A stream that stops without its marker ends the loop as one that has it does: the caller
    // tells them apart by the marker (`Capped::ended_cleanly`).
    while let Some(message) = crate::ipc::next(r, &mut decoded)? {
        let end = message.end;
        let mut buffer = arrow::buffer::Buffer::from_vec(message.bytes);
        let mut got = Vec::new();
        while !buffer.is_empty() {
            if let Some(batch) = decoder.decode(&mut buffer).map_err(bad)? {
                got.push(batch);
            }
        }
        if end {
            decoder.finish().map_err(bad)?;
        }
        for batch in got {
            check_nullability(&batch, declared)?;
            // Into the same total as the bodies (#14).
            decoded.add_unpacked(unpacked_size(&batch))?;
            *rows += batch.num_rows() as u64;
            if *rows > max_rows {
                // Never truncate. A load that silently stops at the cap hands the model a table
                // it believes is complete; over the cap is a refusal, not a smaller answer.
                return Err(Fault::host(
                    "E_HOST_OVERSIZE",
                    format!("more than {max_rows} rows"),
                ));
            }
            batches += 1;
            sink(batch)?;
        }
        if end {
            after_marker(r)?;
            return Ok(Stream {
                schema,
                batches,
                rows: *rows,
            });
        }
    }
    Ok(Stream {
        schema,
        batches,
        rows: *rows,
    })
}

/// The end-of-stream marker ends the input too. Read up to the first marker, a connector that
/// wrote two streams one after the other — a paginating one restarting its writer per page —
/// loaded the first and lost the rest without a word: 100 of 200 rows, measured. So the next read
/// must find the end: of the pipe, which a connector closes by exiting, or of a host's file. One
/// byte past the marker is enough to refuse, so this reads at most one, and a connector still
/// writing gets EPIPE (`a_stream_must_end_at_its_end_marker`).
fn nothing_after_the_marker<R: Read>(r: &mut R) -> Result<(), Fault> {
    let mut byte = [0u8; 1];
    loop {
        return match r.read(&mut byte) {
            Ok(0) => Ok(()),
            Ok(_) => Err(Fault::host(
                "E_HOST_PROTOCOL",
                "bad Arrow stream: there is more after its end-of-stream marker; write one \
                 stream, and finish its writer once, at the end",
            )),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => Err(crate::ipc::io_fault(e)),
        };
    }
}

/// [`nothing_after_the_marker`] for a connector's stdout, which the connector does not always
/// close when it is done: a helper it started in the background holds the pipe open, and waiting
/// for the end of the pipe waited for the helper (`a_finished_connector_leaves_nothing_running`).
/// So the pipe is watched, not read blind: a byte, or the end, is answered as it comes; once the
/// connector itself has exited, its process group is stopped, which closes the pipe for anything
/// in it. A process outside the group that still holds the pipe a moment later has written
/// nothing after the marker before the connector exited, and is not waited for.
fn nothing_after_the_connectors_marker(
    r: &mut Capped<std::process::ChildStdout>,
    child: &mut std::process::Child,
    pid: i32,
) -> Result<(), Fault> {
    use std::os::unix::io::AsRawFd;
    let fd = r.inner.as_raw_fd();
    let mut exited: Option<Instant> = None;
    loop {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut p, 1, 20) } > 0 && p.revents != 0 {
            // Readable: a byte, or the end. The read does not block.
            return nothing_after_the_marker(r);
        }
        if let Some(at) = exited {
            if at.elapsed() >= Duration::from_millis(200) {
                return Ok(());
            }
        } else if matches!(child.try_wait(), Ok(Some(_))) {
            exited = Some(Instant::now());
            unsafe { libc::killpg(pid, libc::SIGKILL) };
        }
        // A cancellation or the deadline stops the connector's group, which closes the pipe.
        if r.abort.lock().unwrap().is_some() {
            return nothing_after_the_marker(r);
        }
    }
}

/// What a batch's dictionary-encoded and view columns take once unpacked: for each row, the
/// length of the value it holds, and a four-byte offset. Other columns count nothing: the
/// stream's own size already bounds them.
///
/// Neither a dictionary nor a view is bounded by its size on the wire. A few bytes of keys can
/// each pick a long value, and a view is 16 bytes whatever it points at, with any number of views
/// pointing at the same bytes: 2,000 views of one 64 KiB block were a stream of 114 KB that loaded
/// as 131 MB under a `max_bytes` of 1 MiB, measured. Both are counted from what the stream says —
/// a key's value length from the dictionary's offsets, a view's from the view itself — without
/// unpacking anything, and in time proportional to the batch's rows, not to the dictionary: a
/// dictionary is sent once and every batch may pick from it, and re-reading all of it for each
/// batch took 15.7 s of CPU for a 5.2 MB stream (`a_large_dictionary_costs_each_batch_only_its_keys`).
fn unpacked_size(batch: &RecordBatch) -> u64 {
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{
        Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
    };
    /// A view's low 32 bits are the length of the value it stands for.
    fn view_len(view: u128) -> u64 {
        view as u32 as u64
    }
    /// The length of value `i` of `values`, read in constant time; 0 for a null or a key that
    /// points past the values, which arrow refuses to decode anyway.
    fn length_of(values: &dyn Array) -> Box<dyn Fn(usize) -> u64 + '_> {
        if let Some(v) = values.as_string_opt::<i32>() {
            Box::new(move |i| {
                if i < v.len() && v.is_valid(i) {
                    v.value_length(i) as u64
                } else {
                    0
                }
            })
        } else if let Some(v) = values.as_string_opt::<i64>() {
            Box::new(move |i| {
                if i < v.len() && v.is_valid(i) {
                    v.value_length(i) as u64
                } else {
                    0
                }
            })
        } else if let Some(v) = values.as_string_view_opt() {
            Box::new(move |i| {
                if i < v.len() && v.is_valid(i) {
                    view_len(v.views()[i])
                } else {
                    0
                }
            })
        } else {
            Box::new(|_| 0)
        }
    }
    /// What `views` hold, each valid one by the length it states, and an offset per row.
    fn viewed(views: &[u128], valid: impl Fn(usize) -> bool) -> u64 {
        views
            .iter()
            .enumerate()
            .map(|(i, v)| 4 + if valid(i) { view_len(*v) } else { 0 })
            .sum()
    }
    macro_rules! picked {
        ($col:expr, $k:ty) => {
            $col.as_dictionary_opt::<$k>().map(|d| {
                let len = length_of(d.values().as_ref());
                d.keys()
                    .iter()
                    .map(|k| 4 + k.map_or(0, |k| len(k as usize)))
                    .sum::<u64>()
            })
        };
    }
    batch
        .columns()
        .iter()
        .filter_map(|c| {
            picked!(c, Int8Type)
                .or_else(|| picked!(c, Int16Type))
                .or_else(|| picked!(c, Int32Type))
                .or_else(|| picked!(c, Int64Type))
                .or_else(|| picked!(c, UInt8Type))
                .or_else(|| picked!(c, UInt16Type))
                .or_else(|| picked!(c, UInt32Type))
                .or_else(|| picked!(c, UInt64Type))
                .or_else(|| {
                    c.as_string_view_opt()
                        .map(|v| viewed(v.views(), |i| v.is_valid(i)))
                })
                .or_else(|| {
                    c.as_binary_view_opt()
                        .map(|v| viewed(v.views(), |i| v.is_valid(i)))
                })
        })
        .sum()
}

/// Set-equal by name, case-sensitive, order-insensitive. Extra fields are never silently
/// dropped: a surprise column carries attacker-influenceable text heading for the model.
fn check_schema(got: &Schema, declared: &Schema) -> Result<(), Fault> {
    let names = |s: &Schema| -> Vec<String> {
        let mut v: Vec<String> = s.fields().iter().map(|f| f.name().clone()).collect();
        v.sort();
        v
    };
    let (g, d) = (names(got), names(declared));
    if g != d {
        let missing: Vec<&String> = d.iter().filter(|n| !g.contains(n)).collect();
        let extra: Vec<&String> = g.iter().filter(|n| !d.contains(n)).collect();
        return Err(Fault::host(
            "E_HOST_SCHEMA_MISMATCH",
            format!("missing {missing:?}, unexpected {extra:?}"),
        ));
    }
    for want in declared.fields() {
        let have = got.field_with_name(want.name()).unwrap();
        if !type_ok(have.data_type(), want.data_type()) {
            let fix = conversion(want.name(), have.data_type(), want.data_type())
                .map(|f| format!(". {f}"))
                .unwrap_or_default();
            return Err(Fault::host(
                "E_HOST_SCHEMA_MISMATCH",
                format!(
                    "column `{}` is {:?}, declared {:?}{fix}",
                    want.name(),
                    have.data_type(),
                    want.data_type()
                ),
            ));
        }
    }
    Ok(())
}

/// The fix for the type mismatches producers commonly make, in the producer's own terms, or
/// `None`. ClickHouse's ArrowStream sends `DateTime` as UInt32 seconds and `UInt64` as UInt64;
/// a text timestamp and a zone-less one are the usual mistakes elsewhere
/// (`a_type_mismatch_names_the_conversion`).
fn conversion(col: &str, got: &DataType, want: &DataType) -> Option<String> {
    use DataType::*;
    let integer = |t: &DataType| {
        matches!(
            t,
            Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64
        )
    };
    Some(match (want, got) {
        (Timestamp(_, Some(_)), t) if integer(t) => format!(
            "An integer where a timestamp is declared is usually epoch seconds, as ClickHouse \
             sends a `DateTime`: select `toDateTime64({col}, 6, 'UTC')` instead, or in pyarrow \
             `pa.array(values, pa.timestamp('us', tz='UTC'))`"
        ),
        (Timestamp(_, Some(_)), Utf8 | LargeUtf8 | Utf8View) => format!(
            "Text where a timestamp is declared: send a timestamp — from ClickHouse \
             `parseDateTime64BestEffort({col}, 6, 'UTC')`, from pyarrow \
             `pc.strptime(values, format, 'us')` with a zone"
        ),
        (Timestamp(_, Some(_)), Timestamp(_, None)) => "A zone-less timestamp for a \
             `timestamptz`: send it with its zone (ClickHouse `toDateTime64(col, 6, 'UTC')`), or \
             declare it `timestamp` and name the source's `timezone` in benchy.toml"
            .to_string(),
        (Int64, UInt64) => format!(
            "An unsigned 64-bit integer can exceed `int64`: select `toInt64({col})` if every \
             value fits, or declare it `uint64`"
        ),
        (Int8 | Int16 | Int32 | Int64, Float16 | Float32 | Float64) => format!(
            "A float where an integer is declared: declare it `float64`, or send whole numbers \
             (ClickHouse `toInt64({col})`)"
        ),
        (Float64, Decimal128(..) | Decimal256(..)) => format!(
            "A decimal where `float64` is declared: declare the decimal, or select \
             `toFloat64({col})`"
        ),
        (want, got) if integer(want) && integer(got) => {
            "An integer wider than declared: declare the wider type".to_string()
        }
        _ => return None,
    })
}

/// Whether a stream may carry `got` where `want` is declared: the same type, or one every value of
/// which `want` holds exactly. A string in any layout — large, view, or dictionary-encoded, which
/// is unpacked within the byte bound (`read_stream`) — an integer of fewer bits, `float32` for
/// `float64`, a timestamp at any precision, a decimal with no more digits on either side of the
/// point. Never a narrowing: a cast that can change a value is where a silent wrong number gets
/// in. A nanosecond timestamp is accepted here and checked in the data as it is loaded: one with
/// sub-microsecond digits is refused then (`crate::canonical::convert`).
///
/// A zoned timestamp is an instant and a zone-less one is not, so neither stands in for the
/// other: `timestamptz` takes any zone, since the instant is what is kept, and `timestamp` none.
fn type_ok(got: &DataType, want: &DataType) -> bool {
    use DataType::*;
    if got == want {
        return true;
    }
    match (want, got) {
        (Utf8, LargeUtf8 | Utf8View) => true,
        (Utf8, Dictionary(k, v)) => k.is_dictionary_key_type() && is_string(v),
        (Binary, LargeBinary | BinaryView) => true,
        (Int16, Int8 | UInt8) => true,
        (Int32, Int8 | Int16 | UInt8 | UInt16) => true,
        (Int64, Int8 | Int16 | Int32 | UInt8 | UInt16 | UInt32) => true,
        (UInt16, UInt8) => true,
        (UInt32, UInt8 | UInt16) => true,
        (UInt64, UInt8 | UInt16 | UInt32) => true,
        (Float64, Float32) => true,
        (Timestamp(_, None), Timestamp(_, None)) => true,
        (Timestamp(_, Some(_)), Timestamp(_, Some(_))) => true,
        (Decimal128(p, s), Decimal128(gp, gs)) => {
            *gs >= 0 && gs <= s && (*gp as i16 - *gs as i16) <= (*p as i16 - *s as i16)
        }
        _ => false,
    }
}

/// Nullability is enforced by **data**, not by the Arrow field flag. Producers set the flag
/// carelessly; the data does not lie.
fn check_nullability(batch: &RecordBatch, declared: &Schema) -> Result<(), Fault> {
    for want in declared.fields() {
        if want.is_nullable() {
            continue;
        }
        let Ok(i) = batch.schema().index_of(want.name()) else {
            continue;
        };
        let col = batch.column(i);
        if col.null_count() > 0 {
            return Err(Fault::host(
                "E_HOST_SCHEMA_MISMATCH",
                format!(
                    "column `{}` is declared NOT NULL but a batch carries {} null(s)",
                    want.name(),
                    col.null_count()
                ),
            ));
        }
    }
    Ok(())
}

/// REQ-12 `estimate_before_fetch`. Ask the connector what a fetch will cost, when it implements
/// the subcommand. Cheap by contract, and the answer arrives before the upstream system has done
/// the expensive thing.
///
/// A connector that cannot answer is not an error: it declared `"estimate": "none"` or answered
/// badly, and the byte and row caps still bound it. It is simply bounded *later*, by a refusal
/// after work was done rather than before — which is the incentive to implement one.
/// `None` when there is no estimate to be had — including a request that cannot be sent, which
/// the fetch then refuses with the reason.
pub fn estimate(src: &ExecSource, n: &Narrowing, run_id: &str) -> Option<(u64, u64)> {
    let doc = request(src, "estimate", run_id, n).ok()?;
    let (out, _err, ok) = run_json(
        src,
        "estimate",
        run_id,
        Some(doc),
        src.limits.describe_timeout,
    )
    .ok()?;
    if !ok {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    Some((
        v.get("rows").and_then(|x| x.as_u64()).unwrap_or(0),
        v.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0),
    ))
}

/// `benchy-mcp exec-test` — the harness a connector author needs before there is a server.
///
/// Runs the checks that can be made without a live upstream: the ones an author actually
/// trips over. Each finding names the fix, because a
/// conformance tool that only says "non-conformant" makes the author read the spec twice.
#[derive(Debug)]
pub struct Check {
    pub ok: bool,
    pub warn: bool,
    /// Not run: what it checks needs a step that already failed.
    pub skipped: bool,
    pub label: String,
    pub detail: String,
}

fn pass(label: &str) -> Check {
    Check {
        ok: true,
        warn: false,
        skipped: false,
        label: label.into(),
        detail: String::new(),
    }
}
fn fail(label: &str, detail: impl Into<String>) -> Check {
    Check {
        ok: false,
        warn: false,
        skipped: false,
        label: label.into(),
        detail: detail.into(),
    }
}
/// A check not run, because a step it needs failed: one fault is one failure, not three, and the
/// advice stays about that fault (`exec_test_skips_what_a_failed_fetch_makes_meaningless`).
/// `exec-test` and `check` both follow this rule.
pub fn skipped(label: &str, failed: &str) -> Check {
    Check {
        ok: true,
        warn: false,
        skipped: true,
        label: label.into(),
        detail: format!("skipped: {failed} failed, and this check needs it to work"),
    }
}
fn warn(label: &str, detail: impl Into<String>) -> Check {
    Check {
        ok: true,
        warn: true,
        skipped: false,
        label: label.into(),
        detail: detail.into(),
    }
}

pub fn conformance(src: &ExecSource) -> Vec<Check> {
    let mut out = Vec::new();

    match check_permissions(&src.command) {
        Ok(()) => out.push(pass(
            "the connector is executable and not writable by others",
        )),
        Err(f) => {
            out.push(fail("connector permissions", f.detail));
            return out; // nothing else is meaningful
        }
    }

    let d = match describe(src) {
        Ok(d) => {
            out.push(pass(&format!(
                "describe answers: {} {} (protocol {:?})",
                d.name, d.version, d.protocol
            )));
            Some(d)
        }
        Err(f) => {
            out.push(fail(
                "describe",
                format!(
                    "{}\n     `describe` must be pure — no network, no credentials — and answer \
                     within {:?}. A connector that must reach its source to answer has \
                     implemented `check`, not `describe`.",
                    f.detail, src.limits.describe_timeout
                ),
            ));
            None
        }
    };

    if let Some(d) = &d {
        for (axis, level) in &d.narrowing {
            if !matches!(level.as_str(), "unsupported" | "best_effort" | "enforced") {
                out.push(fail(
                    "narrowing levels",
                    format!(
                        "axis `{axis}` declares `{level}`; the set is unsupported | best_effort \
                         | enforced"
                    ),
                ));
            }
        }
        if d.narrowing.is_empty() {
            out.push(fail(
                "narrowing levels",
                "describe has no `narrowing` object. Declare every axis, even as `unsupported` — \
                 it is how the host knows whether to bother sending one.",
            ));
        } else {
            out.push(pass("narrowing levels are from the closed set"));
        }
    }

    // A full fetch, validated exactly as a load would validate it.
    let budget = crate::admission::Budget::default();
    let t = Instant::now();
    // Validated and let go: a check keeps nothing of the rows.
    let fetched = fetch_within(
        src,
        &Narrowing::default(),
        "exec-test",
        &budget,
        &mut |_| Ok(()),
    );
    let fetch_failed = fetched.is_err();
    match fetched {
        Ok(f) => {
            out.push(pass(&format!(
                "fetch: {} row(s), {} byte(s) in {:?}, schema and end-of-stream both good",
                f.rows,
                f.bytes,
                t.elapsed()
            )));
            if f.batches == 1 && f.bytes > 64 * 1024 * 1024 {
                out.push(warn(
                    "streaming",
                    "the whole result came as a single RecordBatch over 64 MiB. It works, but \
                     the host cannot start work until the last byte arrives, and your own memory \
                     holds all of it. Emit batches as you produce them.",
                ));
            }
        }
        Err(f) => out.push(fail("fetch", advice_for(&f))),
    }
    if fetch_failed {
        out.push(skipped("empty case", "fetch"));
        out.push(skipped("hostile entity ids", "fetch"));
        return out;
    }

    // What the connector says it does with entity narrowing decides how the next two checks
    // read. A connector that declared `unsupported` returning everything is correct, not sloppy.
    let entity_level = d
        .as_ref()
        .and_then(|d| d.narrowing.get("entity").cloned())
        .unwrap_or_else(|| "unsupported".into());

    // The empty case. This is the single most common contract bug: an empty result is a
    // *complete stream with no batches*, and exiting with nothing on stdout is not that.
    let nothing = Narrowing {
        entities: Some((
            src.schema
                .iter()
                .find(|f| f.ty == "string")
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "id".into()),
            vec!["benchy-exec-test-matches-nothing".into()],
        )),
        ..Narrowing::default()
    };
    match fetch_within(src, &nothing, "exec-test-empty", &budget, &mut |_| Ok(())) {
        Ok(f) if f.rows == 0 => out.push(pass(
            "a narrowing that matches nothing gives an empty stream, not an error",
        )),
        Ok(f) if entity_level == "unsupported" => out.push(pass(&format!(
            "an unmatched id returned {} row(s), which is correct for `entity: unsupported` — \
             the host filters after ingest",
            f.rows
        ))),
        Ok(f) => out.push(warn(
            "empty case",
            format!(
                "you declared `entity: {entity_level}`, but an id that should match nothing \
                 still returned {} row(s). At `best_effort` that is permitted and costs bytes; \
                 at `enforced` it is a bug — you promised no row outside the narrowing.",
                f.rows
            ),
        )),
        Err(f) => out.push(fail(
            "empty case",
            format!(
                "{}\n     An unknown id must yield **no rows, never an error**: an id list \
                 outlives the ids in it, and an error per stale id fails a whole load and leaks \
                 which ids exist. Write the schema, write no batches, close the stream.",
                advice_for(&f)
            ),
        )),
    }

    // Hostile ids. They arrive in the document, never in argv, and must be ordinary strings.
    let hostile = Narrowing {
        entities: Some((
            src.schema
                .iter()
                .find(|f| f.ty == "string")
                .map(|f| f.name.clone())
                .unwrap_or_else(|| "id".into()),
            // All legal ids by the protocol — the host rejects NULs and control characters before a
            // connector ever sees one, so putting those here would test the host, not this
            // connector. These are the ones that *do* arrive.
            vec![
                "--drop-everything".into(),
                "-rf /".into(),
                "$(id)".into(),
                "'; DROP TABLE x --".into(),
                "../../etc/passwd".into(),
                "\u{202e}gnp.exe".into(),
                "x".repeat(512),
            ],
        )),
        ..Narrowing::default()
    };
    match fetch_within(src, &hostile, "exec-test-hostile", &budget, &mut |_| Ok(())) {
        Ok(_) => out.push(pass("hostile entity ids are treated as ordinary strings")),
        Err(f) if f.code == "E_NARROWING" => out.push(warn(
            "hostile entity ids",
            format!(
                "refused as E_NARROWING rather than returning no rows: {}. That is allowed, but \
                 an unknown id is not an error: it yields no rows.",
                f.detail
            ),
        )),
        Err(f) => out.push(fail(
            "hostile entity ids",
            format!(
                "{}\n     Treat narrowing values as untrusted: a host may take them from its \
                 own users. Parameterise your queries and URL-encode your paths; never \
                 concatenate one into SQL, a shell command or a URL. Unknown ids yield no rows.",
                advice_for(&f)
            ),
        )),
    }

    out
}

/// Turn a host-side refusal into something a connector author can act on.
/// What `exec-test` tells the author to do about `f`. Every code the host produces has an answer
/// here; `exec_test_names_a_fix_for_every_failure` fails when a new one does not.
pub fn advice_for(f: &Fault) -> String {
    let extra = match f.code {
        "E_HOST_TRUNCATED" => {
            "\n     Close the stream. In pyarrow that is `writer.close()` (or leaving the `with` \
             block); the end-of-stream marker is what it emits. Without it a reader cannot tell \
             a finished stream from a crashed one — a cut at a message boundary parses clean and \
             returns half your rows."
        }
        "E_HOST_NO_SCHEMA" => {
            "\n     stdout carried no readable Arrow stream. Either you wrote nothing (an empty \
             result is a *complete stream with zero batches*, not empty stdout), or something \
             else got onto stdout — one stray `print()` corrupts it. All logging goes to stderr."
        }
        "E_HOST_SCHEMA_MISMATCH" => {
            "\n     The operator's declaration is authoritative and yours is advisory: the host \
             validates against benchy.toml, never against your `describe`. Field set is compared \
             by name, case-sensitively; an extra column is refused rather than dropped."
        }
        "E_HOST_COMPRESSED" => {
            "\n     Send the stream uncompressed or LZ4-compressed; zstd is not decoded. From \
             ClickHouse: `SETTINGS output_format_arrow_compression_method = 'lz4_frame'`; from \
             pyarrow: `IpcWriteOptions(compression='lz4')` or none."
        }
        "E_HOST_TIMEOUT" => {
            "\n     Emit your first batch sooner, or raise `deadline_secs` for this source if \
             the work genuinely takes that long."
        }
        "E_HOST_OVERSIZE" => {
            "\n     The stream passed its byte cap. Nothing was truncated: narrow what the \
             connector returns, or raise `max_bytes` for this source if the table really is \
             that large."
        }
        "E_TOO_LARGE" => {
            "\n     More rows than the cap allows. Nothing was truncated: narrow the fetch — a \
             shorter window, fewer entities — or raise `max_rows` for this source."
        }
        "E_DATA" => {
            "\n     An attached file is an Arrow *streaming* IPC stream (not the file format), \
             closed so it ends with the end-of-stream marker, uncompressed or LZ4, with every column \
             named once and typed from the vocabulary in docs/EXEC-CONNECTOR-PROTOCOL.md. In \
             pyarrow: `pa.ipc.new_stream(path, schema)`, then `close()`."
        }
        "E_LIMIT" => {
            "\n     Attach less: narrow the data before handing it over, or split it into tables \
             that each fit. `hello` reports the limits, and every attach counts against the \
             session's budget, replacements included."
        }
        "E_HOST_PROTOCOL" => {
            "\n     stdout must carry exactly what the subcommand asks for and nothing else: one \
             JSON document for `describe`, one Arrow *streaming* IPC stream for `fetch` (not the \
             file format). Log to stderr."
        }
        "E_HOST_COMPAT" => {
            "\n     `describe` must list 1 in `protocol`: this benchy speaks benchy-exec/1."
        }
        "E_HOST_UNCLASSIFIED" => {
            "\n     The connector exited non-zero without saying why. Write \
             `{\"type\":\"error\",\"code\":\"E_...\",\"message\":\"...\"}` to stderr before \
             exiting, so the operator log has the cause."
        }
        "E_HOST_CONNECTOR_PERMS" => {
            "\n     The command must be a regular executable file that only you, or root, can \
             write."
        }
        "E_CONFIG" => "\n     Fix it in benchy.toml; the message above names what is wrong.",
        "E_NARROWING" => {
            "\n     The request did not fit: at most 1024 entity ids of at most 512 bytes each, \
             no control characters, and an input document under 1 MiB."
        }
        "E_INTERNAL" => "\n     This is a bug in benchy, not in your connector.",
        // A built-in source's (src/sources/): the source said so, and the message above is its.
        "E_AUTH" => {
            "\n     The source refused the credentials. Set them in the source's `env` from \
             variables in benchy's environment, and check the user may read what is asked for."
        }
        "E_UNAVAILABLE" => {
            "\n     The source could not be reached, or was too busy to answer. Check the address \
             and that it is up and reachable from this machine."
        }
        "E_TIMEOUT" => {
            "\n     The source stopped the query at the deadline. Narrow what is fetched, or raise \
             `deadline_secs` for this source."
        }
        "E_SCHEMA" => {
            "\n     A pinned `[[source.schema]]` column is missing from the source or loads as \
             another type. Pin what it loads as, or leave its type to be read."
        }
        _ => "",
    };
    format!("[{}] {}{extra}", f.code, f.detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, ArrayRef, DictionaryArray, Int32Array, StringArray};

    #[test]
    fn a_connectors_stderr_line_is_bounded() {
        // stderr was read a line at a time, and a line is as long as the connector makes it: one
        // that never writes a newline grew one string without bound, and an `error` event of a
        // gigabyte was kept whole as the verdict. A line is kept to its first 64 KiB, the rest
        // read and dropped, and the lines after it arrive as they were.
        let mut input = vec![b'x'; 3 * MAX_STDERR_LINE + 17];
        input.extend_from_slice(b"\n{\"type\":\"error\",\"code\":\"E_AUTH\"}\n\xff\xfe tail\n");
        let mut lines = Vec::new();
        each_line(input.as_slice(), |l| lines.push(l.to_string()));
        assert_eq!(
            lines.len(),
            3,
            "{:?}",
            lines.iter().map(|l| l.len()).collect::<Vec<_>>()
        );
        assert!(lines[0].len() <= MAX_STDERR_LINE + 1, "{}", lines[0].len());
        assert!(is_error_event(&lines[1]), "{}", lines[1]);
        // Not UTF-8 is log text too, not the end of stderr.
        assert!(lines[2].ends_with(" tail\n"), "{:?}", lines[2]);
    }

    #[test]
    fn a_large_dictionary_costs_each_batch_only_its_keys() {
        // A dictionary is sent once and every batch after it may pick from it with a key or two.
        // Counting what a batch unpacks to once read every value of the dictionary again, for
        // every batch: a 5.2 MB stream of 6,000 one-row batches over a million-entry dictionary
        // took 15.7 s of CPU, against 0.4 s with a one-entry dictionary. What a key picks is read
        // from the dictionary's offsets, so a batch costs its keys and nothing more.
        let values: ArrayRef = Arc::new(StringArray::from(vec![""; 4_000_000]));
        let dict = DictionaryArray::try_new(Int32Array::from(vec![7]), values).unwrap();
        let schema = Schema::new(vec![Field::new("s", dict.data_type().clone(), false)]);
        let batch =
            RecordBatch::try_new(Arc::new(schema), vec![Arc::new(dict) as ArrayRef]).unwrap();
        let started = Instant::now();
        let mut total = 0;
        for _ in 0..1000 {
            total += unpacked_size(&batch);
        }
        assert_eq!(
            total,
            1000 * 4,
            "a four-byte offset per row, and an empty value"
        );
        // Generous: the old count took over ten seconds here, this one microseconds.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?} for 1,000 one-row batches",
            started.elapsed()
        );
    }
}
