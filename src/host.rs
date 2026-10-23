//! `benchy-mcp host`: the host protocol, `benchy-host/1` to `benchy-host/6`, on stdin and
//! stdout (docs/HOST-PROTOCOL.md).
//!
//! A host runs one of these per session, puts data on the bench with `attach`, and relays the
//! agent's SQL with `query`. The pipes are the capability: only the process holding them can put
//! data on this bench, and nothing a request carries names another session.
//!
//! One thread reads. It answers `hello` and `ping` itself, so a ping is answered while a query
//! runs, and it refuses what it does not understand before anything is queued. Everything else
//! runs in order, one call at a time, because the engine does one thing at a time.
//!
//! When stdin is a Unix socket, a request can carry descriptors (`SCM_RIGHTS`): in protocol 3, the
//! read end of a pipe for each table an `attach` streams. The descriptor is the capability, as
//! the pipes are: it names nothing, and only the process that holds the socket can send one.
//!
//! A session started with `--stream-channel` has one more: a pipe it inherited, which carries the
//! host's streams back to back, each the next table's with `"stream": "channel"`. It needs no
//! socket, so a host that cannot pass a descriptor — Node.js, Java — can stream too. It is the
//! session's own, and names nothing either.

use crate::admission::HostLimits;
use crate::profile::Shape;
use crate::proto::{read_frame_bytes, write_frame};
use crate::supervisor::{Attached, Feed, LoadError, Supervisor};
use crate::Caps;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};

/// The versions this build speaks, oldest first. 2 adds `tables` to `attach`: several tables in
/// one request, loaded side by side and checked by the engine once (docs/PERFORMANCE.md R2-5).
/// 3 adds `stream` to `attach`: a table the host writes through a pipe it passes with the
/// request, with no file on either side (docs/PERFORMANCE.md R5-H1); `time_column` and `series`
/// to `attach`, the shape a host states; and `limits` to `hello`, which lowers the session's.
/// 4 adds `tool_names` to `hello`: what the session's texts call the agent's tools
/// (`crate::tools`). A new request field is a new version, so a host that sends it to a benchy that
/// does not know it learns so from `E_VERSION`.
/// 5 adds `"stream": "channel"` to `attach`: the next stream on the session's stream channel
/// (`--stream-channel`), a pipe the host passed when it started benchy, which needs no descriptor
/// passing. A new form of a request is a new version too: a host that offers 5 and is answered 4
/// knows the form is not there before it writes a byte into the channel.
/// 6 adds `loader_memory_ceiling` to `hello`'s `limits`: the most resident memory one loader may
/// hold (`supervisor::LOADER_MEMORY_CEILING`), reported, and lowered where a host asks. Reported
/// only to a session that agrees on 6, as a host may send `limits` back as it got them.
/// 7 adds `threads` to `hello`'s `limits`: the threads the engine runs agent SQL on (`--threads`),
/// reported, and lowered where a host asks; reported only to a session that agrees on 7, as 6's.
pub const PROTOCOLS: [u64; 7] = [1, 2, 3, 4, 5, 6, 7];

/// What the catalogue may take of one reply.
const CATALOGUE_BUDGET: usize = 4096;

/// What `hello` reports: fixed for the life of the session.
pub struct Session {
    pub engine: String,
    /// How Landlock confines the engine, and the kernel's Landlock ABI it was confined under.
    pub sandbox: crate::sandbox::Posture,
    pub landlock_abi: u32,
    pub caps: Caps,
    /// What the operator set; a host's `hello` may lower them.
    pub limits: HostLimits,
    /// The session's directory, which no message may name.
    pub dir: PathBuf,
    /// Whether the session has a stream channel (`--stream-channel`).
    pub channel: bool,
    /// How long one load may take (`--load-timeout`), reported for a host's own timeouts.
    pub load_timeout: std::time::Duration,
}

/// The fields each call takes in protocol `version`. A field benchy does not know is an error,
/// never ignored: a host that writes `max_row` has to be told, not served with the full caps.
fn fields_of(call: &str, version: u64) -> Option<&'static [&'static str]> {
    Some(match call {
        // `limits` came in protocol 3 and `tool_names` in 4, which are not agreed until `hello`
        // is answered: they are checked against the version agreed there.
        "hello" => &["protocol", "limits", "tool_names"],
        "ping" | "describe" => &[],
        "attach" if version >= 3 => &["name", "path", "stream", "time_column", "series", "tables"],
        "attach" if version >= 2 => &["name", "path", "tables"],
        "attach" => &["name", "path"],
        "detach" => &["name"],
        "query" => &["sql", "caps"],
        "page" => &["result_id", "offset", "caps"],
        _ => return None,
    })
}

/// A request that has passed the reader's checks.
enum Call {
    /// One table, as `name` and `path` (or, in protocol 3, `stream`: a descriptor, or in 5 the
    /// stream channel), answered as that table; or, in protocol 2, a list of them (`many`),
    /// answered with one answer each.
    Attach {
        tables: Vec<(String, Feed, Shape)>,
        many: bool,
    },
    Detach {
        name: String,
    },
    Query {
        sql: String,
        caps: Caps,
    },
    Page {
        result_id: String,
        offset: usize,
        caps: Caps,
    },
    Describe,
}

enum Msg {
    Call(u64, Call),
    /// A request refused whole that named a table from the stream channel: the channel is closed
    /// first, in its turn behind the requests before it, then the refusal is sent. The id, when
    /// the request's own, is held as in use until then.
    Refused(Option<u64>, Value),
    /// The limits a host's `hello` lowered, before any call.
    Limits(HostLimits),
    /// What a host's `hello` calls the agent's tools, before any call.
    Tools(crate::tools::Names),
    End,
}

type Out = Arc<Mutex<std::io::Stdout>>;

/// Write one reply. False when it could not be written: the host has closed its end.
fn send(out: &Out, reply: &Value) -> bool {
    let mut o = out.lock().unwrap_or_else(|p| p.into_inner());
    write_frame(&mut *o, reply).is_ok() && o.flush().is_ok()
}

fn refused(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({"id": id, "ok": false, "error": {"code": code, "message": message.into()}})
}

/// A refusal with `detail` beside its message (#26): the message is for the model, the detail
/// for the host, which then parses no prose for a fact. Left out where there is none.
fn with_detail(mut reply: Value, detail: Option<Value>) -> Value {
    if let Some(d) = detail {
        reply["error"]["detail"] = d;
    }
    reply
}

/// What a failed query's reply says beside its message: why the engine went, from the
/// supervisor; past the deadline, why too, and that nothing was lost; and, where the batch
/// stopped at a statement, which, with the results of the statements before it as a query reply
/// gives them.
fn query_detail(r: &crate::proto::Reply) -> Option<Value> {
    let mut detail = r.detail.clone();
    if detail.is_none() && r.code.as_deref() == Some("E_DEADLINE") {
        detail = Some(json!({"reason": "deadline", "lost": []}));
    }
    if let Some(n) = r.statement {
        let d = detail.get_or_insert_with(|| json!({}));
        d["statement"] = n.into();
        d["result_ids"] = json!(r.result_ids);
        d["results"] = json!(r.results);
    }
    detail
}

fn caps_json(c: &Caps) -> Value {
    json!({
        "max_rows": c.max_rows,
        "max_bytes": c.max_bytes,
        "max_cell_bytes": c.max_cell_bytes,
        "deadline_secs": c.deadline.as_secs(),
    })
}

/// What proves table names (`check_name`): an in-memory DuckDB of the server's own, holding
/// nothing else.
///
/// Opening it is DuckDB's whole start — its catalog of built-in functions, its buffer manager, its
/// threads — about 50 M instructions, which is why a host's server spent 82 M instructions on a
/// session where the MCP server spends 25 M (docs/history/PERFORMANCE-rounds.md, round 1 item 8).
/// It is opened on a thread
/// of its own while the engine opens, so the session's start waits for neither in turn.
pub struct Prover(std::thread::JoinHandle<Option<duckdb::Connection>>);

impl Prover {
    pub fn start() -> Prover {
        Prover(std::thread::spawn(|| {
            crate::trace::span("host.prover", || duckdb::Connection::open_in_memory().ok())
        }))
    }
}

/// Serve the host protocol until stdin closes.
pub fn serve(mut supervisor: Supervisor, session: Session, prover: Prover) {
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Arc<Mutex<HashSet<u64>>> = Arc::new(Mutex::new(HashSet::new()));
    let prover = prover.0.join().ok().flatten();
    let (tx, rx) = mpsc::channel();
    let hello = hello_reply(&session);
    {
        let out = out.clone();
        let pending = pending.clone();
        let caps = session.caps.clone();
        let limits = session.limits;
        let channel = session.channel;
        std::thread::spawn(move || {
            read_requests(&out, &pending, &tx, &hello, &caps, (limits, channel))
        });
    }
    // Every request sent is answered, in order, until a reply cannot be written: a host that has
    // closed its end of the replies reads nothing more, so nothing more is run for it.
    loop {
        let (id, call) = match rx.recv() {
            Ok(Msg::Call(id, call)) => (id, call),
            Ok(Msg::Refused(id, reply)) => {
                supervisor.close_channel(REFUSED_CHANNEL);
                if let Some(id) = id {
                    pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                }
                if !send(&out, &reply) {
                    break;
                }
                continue;
            }
            Ok(Msg::Limits(limits)) => {
                supervisor.lower_limits(limits);
                continue;
            }
            Ok(Msg::Tools(names)) => {
                supervisor.name_tools(names);
                continue;
            }
            Ok(Msg::End) | Err(_) => break,
        };
        let reply = run(&mut supervisor, &session, prover.as_ref(), id, call);
        // Free before answering: a host may reuse an id the moment its reply arrives.
        pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
        if !send(&out, &reply) {
            break;
        }
    }
}

fn limits_json(l: &HostLimits) -> Value {
    limits_json_where(l, crate::supervisor::MEMORY_CEILING_UNENFORCED)
}

/// The limits as `hello` reports them, on a platform where the memory ceiling is not enforced for
/// the reason `unenforced` gives, if any: there it is `null`, never a number nothing applies
/// (`a_memory_ceiling_that_cannot_be_enforced_is_not_reported`).
fn limits_json_where(l: &HostLimits, unenforced: Option<&str>) -> Value {
    json!({
        "attach_rows": l.attach_rows,
        "attach_bytes": l.attach_bytes,
        "attachments": l.attachments,
        "session_rows": l.session_rows,
        "session_bytes": l.session_bytes,
        "memory": l.memory,
        "memory_ceiling": if unenforced.is_some() { None } else { Some(l.memory_ceiling) },
    })
}

fn hello_reply(s: &Session) -> Value {
    hello_reply_where(s, crate::supervisor::MEMORY_CEILING_UNENFORCED)
}

/// What a host sets its own timeouts from, which the caps do not say: how long one load may take
/// and how long past the deadline a query may run before the engine is killed. Beside the caps,
/// not in them: a host may hand the caps back in a `query`, which takes the four it has.
fn timeouts_json(caps: &Caps, load_timeout: std::time::Duration) -> Value {
    json!({
        "load_timeout_secs": load_timeout.as_secs(),
        "hard_grace_secs": caps.hard_grace.as_secs(),
    })
}

/// `host --print-limits`: what `hello` would report to a session agreeing on the newest protocol,
/// started with these caps, limits and load timeout — for a host to check a flag set before it
/// starts a session (`print_limits_says_what_hello_would`).
pub fn effective(caps: &Caps, limits: &HostLimits, load_timeout: std::time::Duration) -> Value {
    let unenforced = crate::supervisor::MEMORY_CEILING_UNENFORCED;
    let newest = PROTOCOLS[PROTOCOLS.len() - 1];
    let mut reply = json!({
        "protocol": newest,
        "caps": caps_json(caps),
        "limits": limits_json_where(limits, unenforced),
        "timeouts": timeouts_json(caps, load_timeout),
    });
    if let Some(why) = unenforced {
        reply["unenforced"] = json!({ "memory_ceiling": why });
    }
    for_version(
        &mut reply,
        limits,
        newest,
        crate::supervisor::LOADER_MEMORY_CEILING_UNENFORCED,
    );
    reply
}

fn hello_reply_where(s: &Session, unenforced: Option<&str>) -> Value {
    let mut reply = json!({
        "ok": true,
        // The version agreed, filled in when it is.
        "protocol": Value::Null,
        "benchy": env!("CARGO_PKG_VERSION"),
        "engine": format!("DuckDB {}", s.engine),
        // Prose, for a person; `sandbox_posture` is what a host decides on, in fixed words
        // (`hello_says_how_the_engine_is_confined`). A reply field, so every version gets it.
        "sandbox": s.sandbox.describe(),
        "sandbox_posture": s.sandbox.report(s.landlock_abi),
        "caps": caps_json(&s.caps),
        "limits": limits_json_where(&s.limits, unenforced),
        "timeouts": timeouts_json(&s.caps, s.load_timeout),
    });
    // Beside `limits`, not in it: a host may send `limits` back to lower them, and a field
    // `hello` does not take there is refused.
    if let Some(why) = unenforced {
        reply["unenforced"] = json!({ "memory_ceiling": why });
    }
    reply
}

/// What `hello` adds to its reply for a session that agrees on `version`, beside what every
/// version has: from 6, the loader memory ceiling, `null` where it is not enforced for the reason
/// `unenforced` gives (`a_host_lowers_the_loader_ceiling_and_cannot_raise_it`); from 7, the
/// engine's threads (`a_host_lowers_the_engines_threads_and_cannot_raise_them`).
fn for_version(reply: &mut Value, l: &HostLimits, version: u64, unenforced: Option<&str>) {
    if version < 6 {
        return;
    }
    if version >= 7 {
        reply["limits"]["threads"] = l.threads.into();
    }
    reply["limits"]["loader_memory_ceiling"] = match unenforced {
        Some(_) => Value::Null,
        None => l.loader_memory_ceiling.into(),
    };
    if let Some(why) = unenforced {
        if !reply["unenforced"].is_object() {
            reply["unenforced"] = json!({});
        }
        reply["unenforced"]["loader_memory_ceiling"] = why.into();
    }
}

/// A JSON value read as sent: a key given twice, at any depth, is an error — serde_json would
/// quietly keep the last, and a request with two `sql`s would run one of them.
struct Strict(Value);

impl<'de> serde::Deserialize<'de> for Strict {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(StrictVisitor).map(Strict)
    }
}

struct StrictVisitor;

impl<'de> serde::de::Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
        Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
    }
    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Strict(v)) = seq.next_element()? {
            items.push(v);
        }
        Ok(Value::Array(items))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut m = serde_json::Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let Strict(v) = map.next_value()?;
            if m.contains_key(&k) {
                return Err(serde::de::Error::custom(format!(
                    "the key `{k}` is given twice"
                )));
            }
            m.insert(k, v);
        }
        Ok(Value::Object(m))
    }
}

/// What is wrong with a request before it is run: the id to answer, a code, a message.
type Refusal = (Value, &'static str, String);

/// The most a request may be. A request is SQL or a path; a megabyte is more than any needs, and
/// every frame is parsed — a larger one is not read at all.
const MAX_REQUEST: u32 = 1 << 20;

/// What reading one frame came to.
enum Read {
    /// A request, checked: its id, its call, its fields.
    Request(u64, String, serde_json::Map<String, Value>),
    /// A JSON object that is not a request benchy takes: answered, with the id when there is one.
    Refused(Refusal),
    /// Not a JSON object benchy can read: the session cannot go on.
    Unreadable,
}

/// A request's id, call and fields, checked: an id that is a whole number, a call that exists,
/// only the fields it takes, and no key given twice. Parsed once, exactly as sent; leniently a
/// second time only when that fails, to find the id to answer.
fn vet(body: &[u8], version: u64) -> Read {
    let mut req = match serde_json::from_slice::<Strict>(body) {
        Ok(Strict(Value::Object(m))) => m,
        Ok(_) => return Read::Unreadable,
        Err(e) => {
            return match serde_json::from_slice::<Value>(body) {
                Ok(Value::Object(loose)) => Read::Refused((
                    loose.get("id").cloned().unwrap_or(Value::Null),
                    "E_REQUEST",
                    format!("the request could not be read: {e}"),
                )),
                _ => Read::Unreadable,
            }
        }
    };
    match checked(&mut req, version) {
        Ok((id, call)) => Read::Request(id, call, req),
        Err(refusal) => Read::Refused(refusal),
    }
}

/// The id, call and fields of a request read as sent.
fn checked(
    req: &mut serde_json::Map<String, Value>,
    version: u64,
) -> Result<(u64, String), Refusal> {
    let raw_id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(id) = req.remove("id").as_ref().and_then(Value::as_u64) else {
        return Err((
            raw_id,
            "E_REQUEST",
            "every request needs an `id`: a whole number".into(),
        ));
    };
    let call = match req.remove("call") {
        Some(Value::String(c)) => c,
        _ => {
            return Err((
                id.into(),
                "E_REQUEST",
                "every request needs a `call`".into(),
            ))
        }
    };
    let Some(known) = fields_of(&call, version) else {
        return Err((
            id.into(),
            "E_REQUEST",
            format!(
                "there is no call `{call}`; the calls are hello, attach, detach, query, page, \
                 describe and ping"
            ),
        ));
    };
    if let Some(extra) = req.keys().find(|k| !known.contains(&k.as_str())) {
        let takes = if known.is_empty() {
            "nothing but its id".to_string()
        } else {
            known.join(", ")
        };
        return Err((
            id.into(),
            "E_REQUEST",
            format!("`{call}` has no field `{extra}`; it takes {takes}"),
        ));
    }
    Ok((id, call))
}

/// The most descriptors one request may carry: one a table, and a request lists at most
/// `MAX_ATTACHMENTS` tables.
const MAX_FDS: usize = crate::supervisor::MAX_ATTACHMENTS;

/// The session's stdin, read without a buffer: exactly the bytes of one frame at a time, so the
/// descriptors that come while a frame is read are that frame's. A host sends a request's
/// descriptors with the request's own bytes, and the kernel delivers them with the first of
/// those bytes; a read that went on into the next frame could take the next frame's too.
struct Input {
    /// Whether stdin is a Unix socket, the one thing a descriptor can come over.
    socket: bool,
    /// What has come with the frame being read.
    fds: Vec<OwnedFd>,
    /// More came than a request may carry, or than there was room for: the kernel closed the
    /// rest, and the frame is refused.
    truncated: bool,
}

impl Input {
    fn stdin() -> Input {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let socket =
            unsafe { libc::fstat(0, &mut st) } == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK;
        Input {
            socket,
            fds: Vec::new(),
            truncated: false,
        }
    }

    /// The descriptors that came with the frame just read.
    fn take(&mut self) -> (Vec<OwnedFd>, bool) {
        (
            std::mem::take(&mut self.fds),
            std::mem::replace(&mut self.truncated, false),
        )
    }
}

impl std::io::Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.socket {
            let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            return if n < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(n as usize)
            };
        }
        let space = unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<i32>()) as u32) };
        // u64s, so the control buffer is aligned for the headers in it.
        let mut control = vec![0u64; (space as usize).div_ceil(8)];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = (control.len() * 8) as _;
        // Close-on-exec from the start: no loader or engine started meanwhile inherits one.
        #[cfg(target_os = "linux")]
        let n = unsafe { libc::recvmsg(0, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        // macOS has no MSG_CMSG_CLOEXEC: each descriptor is made close-on-exec as it is taken
        // below, so a loader started in that moment can inherit one. Off Linux there is no
        // sandbox either, and the server refuses to serve without --allow-unsandboxed.
        #[cfg(not(target_os = "linux"))]
        let n = unsafe { libc::recvmsg(0, &mut msg, 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            self.truncated = true;
        }
        // SAFETY: walking the headers the kernel wrote into `control`, within `msg_controllen`.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const i32;
                    let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                        / std::mem::size_of::<i32>();
                    for i in 0..count {
                        let fd = std::ptr::read_unaligned(data.add(i));
                        #[cfg(not(target_os = "linux"))]
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                        self.fds.push(OwnedFd::from_raw_fd(fd));
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        if self.fds.len() > MAX_FDS {
            self.truncated = true;
        }
        Ok(n as usize)
    }
}

/// The reader. Ends the session on EOF, on a frame that cannot be read — a byte stream cannot be
/// resynchronised — and on a session that does not open with a well-formed `hello` both sides
/// speak: a host that cannot open a session has nothing else it can do in one.
fn read_requests(
    out: &Out,
    pending: &Mutex<HashSet<u64>>,
    tx: &mpsc::Sender<Msg>,
    hello: &Value,
    caps: &Caps,
    (mut limits, channel): (HostLimits, bool),
) {
    let mut input = Input::stdin();
    // The version agreed by `hello`; until then, only `hello` is taken, which is the same in
    // every version.
    let mut agreed: Option<u64> = None;
    let in_use = |id: u64| {
        pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&id)
    };
    loop {
        let body = match read_frame_bytes(&mut input, MAX_REQUEST) {
            Ok(b) => b,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::UnexpectedEof {
                    eprintln!("benchy: host: ending the session, a frame could not be read: {e}");
                }
                break;
            }
        };
        // Every descriptor a refused request brought is closed with it, before it is answered, so
        // a host's writer is told nobody reads rather than left waiting.
        let (mut fds, truncated) = input.take();
        let vetted = vet(&body, agreed.unwrap_or(PROTOCOLS[0]));
        let takes_fds =
            agreed.is_some() && matches!(&vetted, Read::Request(_, call, _) if call == "attach");
        if truncated || (!fds.is_empty() && !takes_fds) {
            fds.clear();
            let id = match &vetted {
                Read::Request(id, ..) => Some(*id),
                Read::Refused((id, ..)) => id.as_u64(),
                Read::Unreadable => None,
            };
            let why = if truncated {
                format!("a request carries at most {MAX_FDS} descriptors")
            } else {
                "this request takes no descriptor; only a streamed `attach` does".to_string()
            };
            let id = id.filter(|n| !in_use(*n)).map_or(Value::Null, Value::from);
            let going = refuse(
                out,
                tx,
                pending,
                channel,
                &body,
                refused(id, "E_REQUEST", why),
            );
            if !going || agreed.is_none() || matches!(vetted, Read::Unreadable) {
                break;
            }
            continue;
        }
        let (id, call, req) = match vetted {
            Read::Request(id, call, req) => (id, call, req),
            Read::Unreadable => {
                eprintln!(
                    "benchy: host: ending the session, a frame was not a JSON object benchy can read"
                );
                break;
            }
            Read::Refused((id, code, message)) => {
                // Not with an id still in use: the host would see two replies carrying it.
                let reply = match id.as_u64() {
                    Some(n) if in_use(n) => refused(
                        Value::Null,
                        code,
                        format!("{message} (and id {n} belongs to a request still being answered)"),
                    ),
                    _ => refused(id, code, message),
                };
                if !refuse(out, tx, pending, channel, &body, reply) || agreed.is_none() {
                    break;
                }
                continue;
            }
        };
        if agreed.is_none() {
            if call != "hello" {
                send(
                    out,
                    &refused(id.into(), "E_REQUEST", "the first request must be `hello`"),
                );
                break;
            }
            let Some(offered) = req.get("protocol").and_then(Value::as_array) else {
                send(
                    out,
                    &refused(
                        id.into(),
                        "E_REQUEST",
                        "`protocol` is the list of versions the host speaks, like [1]",
                    ),
                );
                break;
            };
            // The newest version both speak.
            let Some(newest) = PROTOCOLS
                .iter()
                .rev()
                .find(|v| offered.iter().any(|p| p.as_u64() == Some(**v)))
            else {
                send(
                    out,
                    &with_detail(
                        refused(
                            id.into(),
                            "E_VERSION",
                            format!("this benchy speaks protocols {PROTOCOLS:?}"),
                        ),
                        Some(json!({ "protocols": PROTOCOLS })),
                    ),
                );
                break;
            };
            let mut reply = hello.clone();
            if let Some(asked) = req.get("limits") {
                let lowered = match asked {
                    _ if *newest < 3 => Err(format!(
                        "`limits` came in protocol 3, and this session speaks {newest}"
                    )),
                    Value::Object(asked)
                        if *newest < 6 && asked.contains_key("loader_memory_ceiling") =>
                    {
                        Err(format!(
                            "`limits.loader_memory_ceiling` came in protocol 6, and this session \
                             speaks {newest}"
                        ))
                    }
                    Value::Object(asked) if *newest < 7 && asked.contains_key("threads") => {
                        Err(format!(
                            "`limits.threads` came in protocol 7, and this session speaks {newest}"
                        ))
                    }
                    Value::Object(asked) => limits.lowered(asked),
                    _ => Err("`limits` must be an object".to_string()),
                };
                match lowered {
                    Ok(l) => limits = l,
                    Err(e) => {
                        send(out, &refused(id.into(), "E_REQUEST", e));
                        break;
                    }
                }
                if tx.send(Msg::Limits(limits)).is_err() {
                    break;
                }
                reply["limits"] = limits_json(&limits);
            }
            if let Some(asked) = req.get("tool_names") {
                let names = if *newest < 4 {
                    Err(format!(
                        "`tool_names` came in protocol 4, and this session speaks {newest}"
                    ))
                } else {
                    crate::tools::parse(asked)
                };
                match names {
                    Ok(names) => {
                        if tx.send(Msg::Tools(names)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        send(out, &refused(id.into(), "E_REQUEST", e));
                        break;
                    }
                }
            }
            for_version(
                &mut reply,
                &limits,
                *newest,
                crate::supervisor::LOADER_MEMORY_CEILING_UNENFORCED,
            );
            reply["id"] = id.into();
            reply["protocol"] = (*newest).into();
            // The reader's replies end the session as the call loop's do: a host that cannot
            // read this one has gone (`a_reply_the_reader_cannot_write_ends_the_session`).
            if !send(out, &reply) {
                break;
            }
            agreed = Some(*newest);
            continue;
        }
        if in_use(id) {
            fds.clear();
            // Answered without the id, which its own request's reply will carry.
            let reply = refused(
                Value::Null,
                "E_REQUEST",
                format!("id {id} belongs to a request still being answered"),
            );
            if !refuse(out, tx, pending, channel, &body, reply) {
                break;
            }
            continue;
        }
        match call.as_str() {
            "hello" => {
                if !send(
                    out,
                    &refused(id.into(), "E_REQUEST", "`hello` comes once, first"),
                ) {
                    break;
                }
                continue;
            }
            "ping" => {
                if !send(out, &json!({"id": id, "ok": true})) {
                    break;
                }
                continue;
            }
            _ => {}
        }
        let version = agreed.unwrap_or(PROTOCOLS[0]);
        let most = limits.attachments as usize;
        let streams = Streams {
            fds: &mut fds,
            socket: input.socket,
            channel,
        };
        let parsed = match parse(&call, &req, caps, (version, most), streams) {
            Ok(c) => c,
            Err(e) => {
                fds.clear();
                if !refuse(
                    out,
                    tx,
                    pending,
                    channel,
                    &body,
                    refused(id.into(), "E_REQUEST", e),
                ) {
                    break;
                }
                continue;
            }
        };
        pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id);
        if tx.send(Msg::Call(id, parsed)).is_err() {
            break;
        }
    }
    // No more requests. Those already queued are still answered.
    let _ = tx.send(Msg::End);
}

/// Why a refused request closes the stream channel.
const REFUSED_CHANNEL: &str = "a request that named tables from it was refused whole, before \
     their streams were read, so where the next stream in the channel starts is not known";

/// Answer a request refused whole. One that names a table from the session's stream channel
/// (`"stream": "channel"`, anywhere in it) closes the channel, as a stream that loses its place
/// does: nothing of it was read, and a host may have written its streams ahead, so the next
/// attach would take one of them under its own name, `ok: true` — one table's rows named as
/// another's (`a_refused_request_that_names_the_channel_closes_it`). Which streams a refusal left
/// in the channel is not the host's to work out. Such a refusal is answered in its turn, behind
/// the requests before it, which read their own streams first. False when the session cannot go
/// on.
fn refuse(
    out: &Out,
    tx: &mpsc::Sender<Msg>,
    pending: &Mutex<HashSet<u64>>,
    channel: bool,
    body: &[u8],
    mut reply: Value,
) -> bool {
    if !channel || !names_channel(body) {
        return send(out, &reply);
    }
    reply["error"]["channel"] = "closed".into();
    let said = reply["error"]["message"].as_str().unwrap_or_default();
    reply["error"]["message"] =
        format!("{said}. The stream channel is closed: {REFUSED_CHANNEL}").into();
    let id = reply["id"].as_u64();
    if let Some(id) = id {
        pending.lock().unwrap_or_else(|p| p.into_inner()).insert(id);
    }
    tx.send(Msg::Refused(id, reply)).is_ok()
}

/// Whether a request's body asks for a table from the stream channel: a `"stream": "channel"`
/// anywhere in it, a key given twice included, which is how a host that wrote ahead meant it.
/// Read only for a request that is refused, so a request that is served pays nothing for it.
fn names_channel(body: &[u8]) -> bool {
    /// Whether what it visits holds a `"stream": "channel"`; `.0` when it visits a `stream`'s own
    /// value.
    struct Visit(bool);
    /// Any value, and whether it holds a `"stream": "channel"`.
    struct Holds(bool);
    /// A `stream`'s value, and whether it is `"channel"`.
    struct StreamValue(bool);
    impl<'de> serde::Deserialize<'de> for Holds {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Visit(false)).map(Holds)
        }
    }
    impl<'de> serde::Deserialize<'de> for StreamValue {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Visit(true)).map(StreamValue)
        }
    }
    impl<'de> serde::de::Visitor<'de> for Visit {
        type Value = bool;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("JSON")
        }
        fn visit_bool<E>(self, _: bool) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_i64<E>(self, _: i64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_u64<E>(self, _: u64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_f64<E>(self, _: f64) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_unit<E>(self) -> Result<bool, E> {
            Ok(false)
        }
        fn visit_str<E>(self, v: &str) -> Result<bool, E> {
            Ok(self.0 && v == "channel")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
            let mut found = false;
            while let Some(Holds(f)) = seq.next_element()? {
                found |= f;
            }
            Ok(found)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<bool, A::Error> {
            let mut found = false;
            while let Some(k) = map.next_key::<String>()? {
                found |= if k == "stream" {
                    map.next_value::<StreamValue>()?.0
                } else {
                    map.next_value::<Holds>()?.0
                };
            }
            Ok(found)
        }
    }
    serde_json::from_slice::<Holds>(body).is_ok_and(|f| f.0)
}

fn string(req: &serde_json::Map<String, Value>, field: &str) -> Result<String, String> {
    match req.get(field) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("`{field}` must be a string")),
        None => Err(format!("`{field}` is missing")),
    }
}

/// The caps a request asks for: the session's, lowered where it says so and never raised. A
/// response under 512 bytes, or a value cut under 16, could not say what it cut.
fn lowered(req: &serde_json::Map<String, Value>, session: &Caps) -> Result<Caps, String> {
    let mut caps = session.clone();
    let Some(asked) = req.get("caps") else {
        return Ok(caps);
    };
    let Value::Object(asked) = asked else {
        return Err("`caps` must be an object".into());
    };
    for (k, v) in asked {
        let Some(n) = v.as_u64().map(|n| n as usize) else {
            return Err(format!("`caps.{k}` must be a whole number"));
        };
        if k == "deadline_secs" {
            let session_secs = caps.deadline.as_secs() as usize;
            if n > session_secs {
                return Err(format!(
                    "`caps.deadline_secs` may lower the session's {session_secs} but never raise it"
                ));
            }
            if n < 1 {
                return Err("`caps.deadline_secs` must be at least 1".into());
            }
            caps.deadline = std::time::Duration::from_secs(n as u64);
            continue;
        }
        let (slot, floor) = match k.as_str() {
            "max_rows" => (&mut caps.max_rows, 1),
            "max_bytes" => (&mut caps.max_bytes, crate::result::MIN_MAX_BYTES),
            "max_cell_bytes" => (&mut caps.max_cell_bytes, 16),
            other => {
                return Err(format!(
                    "`caps` has no field `{other}`; it takes max_rows, max_bytes, \
                     max_cell_bytes and deadline_secs"
                ))
            }
        };
        if n > *slot {
            return Err(format!(
                "`caps.{k}` may lower the session's {} but never raise it",
                *slot
            ));
        }
        if n < floor {
            return Err(format!("`caps.{k}` must be at least {floor}"));
        }
        *slot = n;
    }
    Ok(caps)
}

/// What a request's streams can come by: the descriptors that came with it, whether they could
/// (stdin is a socket), and whether the session has a stream channel.
struct Streams<'a> {
    fds: &'a mut Vec<OwnedFd>,
    socket: bool,
    channel: bool,
}

fn parse(
    call: &str,
    req: &serde_json::Map<String, Value>,
    caps: &Caps,
    (version, most): (u64, usize),
    streams: Streams,
) -> Result<Call, String> {
    Ok(match call {
        "attach" => {
            let (tables, many) = match req.get("tables") {
                None => (vec![table(req, version)?], false),
                Some(_)
                    if ["name", "path", "stream", "time_column", "series"]
                        .iter()
                        .any(|k| req.contains_key(*k)) =>
                {
                    return Err("`attach` takes one table's fields, or `tables`, not both".into())
                }
                Some(list) => (self::tables(list, version, most)?, true),
            };
            Call::Attach {
                tables: feeds(tables, std::mem::take(streams.fds), streams)?,
                many,
            }
        }
        "detach" => Call::Detach {
            name: string(req, "name")?,
        },
        "query" => Call::Query {
            sql: string(req, "sql")?,
            caps: lowered(req, caps)?,
        },
        "page" => Call::Page {
            result_id: string(req, "result_id")?,
            offset: match req.get("offset") {
                None => 0,
                Some(v) => v.as_u64().ok_or("`offset` must be a whole number")? as usize,
            },
            caps: lowered(req, caps)?,
        },
        "describe" => Call::Describe,
        other => return Err(format!("`{other}` is not handled here")),
    })
}

/// Where one table of an `attach` comes from, as asked.
enum Via {
    Path(PathBuf),
    /// A pipe whose read end comes with the request.
    Stream,
    /// The next stream on the session's stream channel.
    Channel,
}

/// One table of an `attach`, as asked: its name, where it comes from, and the shape its host
/// states.
type Asked = (String, Via, Shape);

/// One table of an `attach`: its name; its file or (protocol 3) that it is streamed; and (protocol
/// 3) the shape the host states. Which fields an entry may have at all is checked before this,
/// against the protocol agreed.
fn table(entry: &serde_json::Map<String, Value>, version: u64) -> Result<Asked, String> {
    let name = string(entry, "name")?;
    let via = match (entry.get("path"), entry.get("stream")) {
        (Some(_), Some(_)) => return Err("a table is streamed or has a `path`, not both".into()),
        (None, Some(Value::Bool(true))) => Via::Stream,
        (None, Some(Value::String(s))) if s == "channel" && version >= 5 => Via::Channel,
        (None, Some(Value::String(s))) if s == "channel" => {
            return Err(format!(
                "`\"stream\": \"channel\"` came in protocol 5, and this session speaks {version}"
            ))
        }
        (None, Some(_)) => return Err(
            "`stream` is `true`, a pipe whose read end comes with the request, or \"channel\", \
                 the session's stream channel; or it is left out"
                .into(),
        ),
        _ => Via::Path(PathBuf::from(string(entry, "path")?)),
    };
    Ok((name, via, shape(entry)?))
}

/// The shape a host states, as an operator's config states one: the time column, and the columns
/// that tell the series apart. Whether the stream has them is its loader's to check, against the
/// stream's own schema; what can be refused without it is refused here.
fn shape(entry: &serde_json::Map<String, Value>) -> Result<Shape, String> {
    let time_column = match entry.get("time_column") {
        None => None,
        Some(Value::String(t)) if !t.is_empty() => Some(t.clone()),
        Some(_) => return Err("`time_column` must be a column's name".into()),
    };
    let series = match entry.get("series") {
        None => Vec::new(),
        Some(Value::Array(list)) if !list.is_empty() => {
            let mut series: Vec<String> = Vec::with_capacity(list.len());
            for c in list {
                let Some(c) = c.as_str().filter(|c| !c.is_empty()) else {
                    return Err("`series` must be a list of columns' names".into());
                };
                if series.iter().any(|s| s == c) {
                    return Err(format!("`series` names `{c}` twice"));
                }
                if time_column.as_deref() == Some(c) {
                    return Err(format!(
                        "`{c}` is stated as both the time column and a series column"
                    ));
                }
                series.push(c.to_string());
            }
            series
        }
        Some(_) => {
            return Err("`series` must be a list of columns' names, one or more".into());
        }
    };
    Ok(Shape {
        time_column,
        series,
    })
}

/// Each table's feed: its path, the next descriptor that came with the request, or the stream
/// channel. One descriptor a streamed table, in the order of the list, and no other: anything
/// else refuses the whole request, and closes every descriptor that came with it. A table from
/// the channel needs a session that has one, and takes no descriptor.
fn feeds(
    tables: Vec<Asked>,
    fds: Vec<OwnedFd>,
    streams: Streams,
) -> Result<Vec<(String, Feed, Shape)>, String> {
    if !streams.channel && tables.iter().any(|(_, v, _)| matches!(v, Via::Channel)) {
        return Err(
            "this session has no stream channel: start `benchy-mcp host --stream-channel <fd>` \
             with the channel's read end inherited as that descriptor"
                .into(),
        );
    }
    let streamed = tables
        .iter()
        .filter(|(_, v, _)| matches!(v, Via::Stream))
        .count();
    if streamed > 0 && !streams.socket {
        return Err(
            "a streamed attach passes the pipe with the request, and a descriptor can only come \
             over a Unix socket: start the session with a socket as its stdin"
                .into(),
        );
    }
    if fds.len() != streamed {
        return Err(format!(
            "this attach streams {streamed} table(s) and came with {} descriptor(s); each \
             streamed table takes one, the read end of its pipe, sent with the request",
            fds.len()
        ));
    }
    let mut fds = fds.into_iter();
    Ok(tables
        .into_iter()
        .map(|(name, via, shape)| match via {
            Via::Path(path) => (name, Feed::Path(path), shape),
            Via::Stream => (name, Feed::Stream(fds.next().expect("counted")), shape),
            Via::Channel => (name, Feed::Channel, shape),
        })
        .collect())
}

/// The tables of a protocol-2 `attach`: a list of `{"name", "path"}` — or, in protocol 3,
/// `{"name", "stream": true}` or `{"name", "stream": "channel"}` — each as strict as a request. What is wrong with the list refuses
/// the whole request, before anything is read: a list that is empty, or longer than a session
/// can hold, has nothing to say, and a name given twice would leave which file the table is to
/// the order of the list. What is wrong with one table — its name, its file — is that table's
/// alone, and answered in its place (`run`).
fn tables(list: &Value, version: u64, most: usize) -> Result<Vec<Asked>, String> {
    // `tables` came in protocol 2, and `stream` and the stated shape in 3: a protocol-2 entry with
    // either is refused by name, before any descriptor is looked at.
    let fields: &[&str] = if version >= 3 {
        &["name", "path", "stream", "time_column", "series"]
    } else {
        &["name", "path"]
    };
    let Value::Array(list) = list else {
        return Err("`tables` must be a list of {\"name\", \"path\"}".into());
    };
    if list.is_empty() || list.len() > most {
        return Err(format!(
            "`tables` lists 1 to {most} tables, the most a session holds"
        ));
    }
    let mut out: Vec<Asked> = Vec::with_capacity(list.len());
    for (i, entry) in list.iter().enumerate() {
        let Value::Object(entry) = entry else {
            return Err(format!(
                "`tables[{i}]` must be an object: {{\"name\", \"path\"}}"
            ));
        };
        if let Some(extra) = entry.keys().find(|k| !fields.contains(&k.as_str())) {
            return Err(format!(
                "`tables[{i}]` has no field `{extra}`; it takes {}",
                fields.join(", ")
            ));
        }
        let asked = table(entry, version).map_err(|e| format!("`tables[{i}]`: {e}"))?;
        if out.iter().any(|(n, _, _)| *n == asked.0) {
            return Err(format!("`tables` names `{}` twice", asked.0));
        }
        out.push(asked);
    }
    Ok(out)
}

/// A name the agent can type unquoted, exactly as the host sent it: refused rather than
/// rewritten, so the host knows what the agent sees. Proven by DuckDB, not by a list: `left`
/// and `join` are not reserved words, and still no query can name them unquoted.
fn check_name(name: &str, prover: Option<&duckdb::Connection>) -> Result<(), String> {
    let plain = !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !plain {
        return Err(format!(
            "`{name}` is not a table name the agent can type: use lowercase letters, digits and \
             underscores, starting with a letter, at most 63 bytes"
        ));
    }
    let taken = [
        "memory",
        "system",
        "temp",
        "main",
        "information_schema",
        "pg_catalog",
    ];
    if name.starts_with("benchy_") || taken.contains(&name) {
        return Err(format!("`{name}` is a name benchy or DuckDB uses itself"));
    }
    // Only letters, digits and underscores reach here, so the name is safe to write into SQL.
    let proven = prover.is_some_and(|c| {
        c.execute_batch(&format!(
            "CREATE TABLE {name} (x INTEGER); SELECT x FROM {name}; DROP TABLE {name};"
        ))
        .is_ok()
    });
    if !proven {
        return Err(format!(
            "`{name}` is a word DuckDB's SQL reads as something other than a name, so the agent \
             could not type it unquoted"
        ));
    }
    Ok(())
}

/// A message with no path of the session's and none of the host's in it: it may be shown to a
/// model, and a host already knows which file it sent.
///
/// Only absolute paths are replaced: a relative one, like `a`, occurs inside ordinary words. The
/// host's path is made absolute before anything uses it, so that is the form a message can hold.
fn scrub(message: &str, session: &Path, sent: Option<&Path>) -> String {
    let mut m = message.replace(&session.display().to_string(), "<session>");
    if let Some(p) = sent.filter(|p| p.is_absolute()) {
        m = m.replace(&p.display().to_string(), "<file>");
    }
    m
}

/// What a table is read from, checked before a loader is started for it. A file must be a regular
/// file, not reached through a link. A descriptor must be something a stream can be read from —
/// a pipe, a socket or a regular file — and is read blocking: the loader waits for the host to
/// write rather than spinning.
fn readable(feed: &Feed) -> Result<(), String> {
    match feed {
        Feed::Path(path) => {
            let regular = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
            if regular {
                Ok(())
            } else {
                Err(
                    "it is not a regular file, or could not be found; links are not followed"
                        .into(),
                )
            }
        }
        Feed::Stream(fd) => {
            let fd = fd.as_raw_fd();
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut st) } != 0 {
                return Err("its descriptor could not be examined".into());
            }
            if !matches!(
                st.st_mode & libc::S_IFMT,
                libc::S_IFIFO | libc::S_IFSOCK | libc::S_IFREG
            ) {
                return Err(
                    "its descriptor is not a pipe, a socket or a regular file to read a stream \
                     from"
                        .into(),
                );
            }
            // The flag is the open file's, shared with any copy the host kept; the host has no
            // use for a read end it handed over.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
            {
                return Err("its descriptor could not be made blocking".into());
            }
            Ok(())
        }
        // The session's own, checked when it started (`main`'s `take_channel`).
        Feed::Channel | Feed::ChannelRefused(_) => Ok(()),
    }
}

fn table_json(a: &Attached) -> Value {
    json!({"name": a.name, "rows": a.rows, "columns": a.columns})
}

fn run(
    supervisor: &mut Supervisor,
    session: &Session,
    prover: Option<&duckdb::Connection>,
    id: u64,
    call: Call,
) -> Value {
    let from_reply = |r: crate::proto::Reply, default: &str, ok: Value| -> Value {
        if r.ok {
            let mut v = ok;
            v["id"] = id.into();
            v["ok"] = true.into();
            v
        } else {
            let detail = query_detail(&r);
            with_detail(
                refused(
                    id.into(),
                    r.code.as_deref().unwrap_or(default),
                    scrub(&r.body, &session.dir, None),
                ),
                detail,
            )
        }
    };
    match call {
        Call::Attach { tables, many } => {
            // Each table is checked here, then every one that passes goes to the supervisor in
            // one call: answered one by one, in the order asked.
            let mut answers: Vec<Option<Result<Attached, LoadError>>> =
                Vec::with_capacity(tables.len());
            let mut sent: Vec<(String, Feed, Shape)> = Vec::new();
            // Each table's name, and the path a message could name: a file's, absolute from here
            // on, which is what the loader opens. A pipe has none.
            let mut asked: Vec<(String, Option<PathBuf>)> = Vec::with_capacity(tables.len());
            for (name, feed, shape) in tables {
                let feed = match feed {
                    Feed::Path(path) => Feed::Path(std::path::absolute(&path).unwrap_or(path)),
                    stream => stream,
                };
                asked.push((
                    name.clone(),
                    match &feed {
                        Feed::Path(path) => Some(path.clone()),
                        _ => None,
                    },
                ));
                if let Err(e) = check_name(&name, prover) {
                    let e = LoadError {
                        code: "E_NAME",
                        message: e,
                        closed_channel: None,
                        detail: None,
                    };
                    // Its stream is in the channel all the same: read past, so the next table
                    // takes the next one.
                    if matches!(feed, Feed::Channel) {
                        answers.push(None);
                        sent.push((name, Feed::ChannelRefused(e), shape));
                    } else {
                        answers.push(Some(Err(e)));
                    }
                    continue;
                }
                if let Err(e) = readable(&feed) {
                    answers.push(Some(Err(LoadError {
                        code: "E_DATA",
                        message: e,
                        closed_channel: None,
                        detail: None,
                    })));
                    continue;
                }
                answers.push(None);
                sent.push((name, feed, shape));
            }
            let mut attached = supervisor.attach_all(sent).into_iter();
            let each = asked.iter().zip(answers).map(|((name, path), a)| {
                let a = a.unwrap_or_else(|| attached.next().expect("one answer per table sent"));
                match a {
                    Ok(a) => {
                        let mut v = table_json(&a);
                        v["ok"] = true.into();
                        v
                    }
                    Err(LoadError {
                        code,
                        message,
                        closed_channel,
                        detail,
                    }) => {
                        let mut message = scrub(&message, &session.dir, path.as_deref());
                        let mut error = json!({"code": code});
                        // The table whose stream lost the channel its place says so, beside its
                        // own reason: the channel is closed from here on.
                        if let Some(why) = closed_channel {
                            message = format!("{message}. The stream channel is closed: {why}");
                            error["channel"] = "closed".into();
                        }
                        error["message"] = message.into();
                        if let Some(d) = detail {
                            error["detail"] = d;
                        }
                        json!({"name": name, "ok": false, "error": error})
                    }
                }
            });
            if many {
                json!({"id": id, "ok": true, "tables": each.collect::<Vec<_>>()})
            } else {
                // Protocol 1's reply, for one table: the answer itself.
                let mut one = each.last().expect("one table");
                one["id"] = id.into();
                if one["ok"] == false {
                    one.as_object_mut().expect("an object").remove("name");
                }
                one
            }
        }
        Call::Detach { name } => {
            let r = supervisor.detach(&name);
            from_reply(r, "E_NAME", json!({}))
        }
        Call::Query { sql, caps } => {
            let r = supervisor.query(sql, &caps);
            let ok = json!({"text": r.body, "result_ids": r.result_ids, "results": r.results,
                            "caps": caps_json(&caps)});
            from_reply(r, "E_SQL", ok)
        }
        Call::Page {
            result_id,
            offset,
            caps,
        } => {
            let r = supervisor.page(result_id, offset, &caps);
            let mut ok = json!({"text": r.body});
            if let Some(sql) = &r.sql {
                ok["sql"] = sql.as_str().into();
            }
            from_reply(r, "E_NO_RESULT", ok)
        }
        Call::Describe => {
            let r = supervisor.describe(CATALOGUE_BUDGET.min(session.caps.max_bytes));
            let tables: Vec<Value> = supervisor.attached().iter().map(table_json).collect();
            let (rows_left, bytes_left) = supervisor.budget_left();
            let ok = json!({"text": r.body, "tables": tables,
                            "budget": {"rows_left": rows_left, "bytes_left": bytes_left}});
            from_reply(r, "E_UNAVAILABLE", ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scrub;
    use std::path::Path;

    #[test]
    fn a_memory_ceiling_that_cannot_be_enforced_is_not_reported() {
        let session = super::Session {
            engine: "v1.5.5".into(),
            sandbox: crate::sandbox::Posture::None,
            landlock_abi: 0,
            caps: Default::default(),
            limits: Default::default(),
            dir: "/nowhere".into(),
            channel: false,
            load_timeout: std::time::Duration::from_secs(300),
        };
        let ceiling = session.limits.memory_ceiling;
        // where it is enforced (Linux), the reply is as it was
        let here = super::hello_reply_where(&session, None);
        assert_eq!(here["limits"]["memory_ceiling"], ceiling);
        assert!(here.get("unenforced").is_none(), "{here}");
        // elsewhere, no number nothing applies, and why
        let elsewhere = super::hello_reply_where(&session, Some("no /proc"));
        assert!(
            elsewhere["limits"]["memory_ceiling"].is_null(),
            "{elsewhere}"
        );
        assert_eq!(elsewhere["unenforced"]["memory_ceiling"], "no /proc");
        assert_eq!(elsewhere["limits"]["memory"], session.limits.memory);
        // and this platform is the one it says it is
        assert_eq!(
            crate::supervisor::MEMORY_CEILING_UNENFORCED.is_none(),
            cfg!(target_os = "linux")
        );
    }

    #[test]
    fn a_loader_memory_ceiling_that_cannot_be_enforced_is_not_reported() {
        // As the engine's: read from /proc, so enforced on Linux only, and elsewhere reported
        // as `null` with why, from protocol 6, the first that reports it.
        let limits = crate::admission::HostLimits::default();
        let base = || serde_json::json!({"limits": {"memory_ceiling": 1}});
        let mut here = base();
        super::for_version(&mut here, &limits, 6, None);
        assert_eq!(
            here["limits"]["loader_memory_ceiling"],
            limits.loader_memory_ceiling
        );
        assert!(here.get("unenforced").is_none(), "{here}");
        let mut elsewhere = base();
        super::for_version(&mut elsewhere, &limits, 6, Some("no /proc"));
        assert!(
            elsewhere["limits"]["loader_memory_ceiling"].is_null(),
            "{elsewhere}"
        );
        assert_eq!(elsewhere["unenforced"]["loader_memory_ceiling"], "no /proc");
        // Before 6 neither is there: a host may send `limits` back as it got them.
        let mut older = base();
        super::for_version(&mut older, &limits, 5, Some("no /proc"));
        assert_eq!(older, base());
        assert_eq!(
            crate::supervisor::LOADER_MEMORY_CEILING_UNENFORCED.is_none(),
            cfg!(target_os = "linux")
        );
    }

    #[test]
    fn a_message_names_no_path_of_the_sessions_or_the_hosts() {
        // DuckDB names the files it could not open, and a loader's error can name its input.
        let session = Path::new("/tmp/benchy-1-abc-0");
        let sent = Path::new("/srv/host/customer-7/readings.arrows");
        let said = scrub(
            "could not attach /tmp/benchy-1-abc-0/ws-2/5.duckdb, from /srv/host/customer-7/readings.arrows",
            session,
            Some(sent),
        );
        assert_eq!(
            said,
            "could not attach <session>/ws-2/5.duckdb, from <file>"
        );
    }

    #[test]
    fn a_short_path_does_not_garble_a_message() {
        // A path the host sent relative, like `a`, occurs inside ordinary words. Only an absolute
        // path — which is what the host's path becomes before anything uses it — is replaced.
        let message = "readings: it does not end with the Arrow end-of-stream marker";
        let said = scrub(
            message,
            Path::new("/tmp/benchy-1-abc-0"),
            Some(Path::new("a")),
        );
        assert_eq!(said, message);
    }
}
