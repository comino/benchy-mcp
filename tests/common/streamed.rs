//! A host that streams its attachments (docs/HOST-PROTOCOL.md, protocol 3): `benchy-mcp host` on a
//! Unix socket instead of pipes, so a request can carry the read end of a pipe as `SCM_RIGHTS`,
//! and the host writes the Arrow stream into the other end without a file in between.

use benchy_mcp::proto::read_frame;
use serde_json::Value;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};

/// A pipe: the read end, to send, and the write end, to write the stream into.
pub fn pipe() -> (OwnedFd, File) {
    let mut fds = [0; 2];
    #[cfg(target_os = "linux")]
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // macOS has no pipe2: close-on-exec is set after, which only a test's own exec could race.
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
    }
    unsafe { (OwnedFd::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

/// Write `bytes` on `sock`, the descriptors `fds` riding with the first of them.
pub fn send_with_fds(sock: &UnixStream, bytes: &[u8], fds: &[RawFd]) {
    let mut sent = 0;
    while sent < bytes.len() {
        let mut iov = libc::iovec {
            iov_base: bytes[sent..].as_ptr() as *mut libc::c_void,
            iov_len: bytes.len() - sent,
        };
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of_val(fds) as u32) } as usize;
        let mut control = vec![0u8; space.max(1)];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if sent == 0 && !fds.is_empty() {
            msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr() as *const u8,
                    libc::CMSG_DATA(c),
                    std::mem::size_of_val(fds),
                );
            }
        }
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        assert!(n > 0, "sendmsg: {}", std::io::Error::last_os_error());
        sent += n as usize;
    }
}

/// Where the tests put a session's stream channel: descriptor 3, which is where Node.js's
/// `spawn(..., {stdio: ['pipe', 'pipe', 'pipe', 'pipe']})` puts a fourth pipe.
pub const CHANNEL_FD: RawFd = 3;

/// Have `cmd`'s child inherit `fd` as its descriptor `at`, without close-on-exec.
pub fn inherit_as(cmd: &mut Command, fd: &OwnedFd, at: RawFd) {
    use std::os::unix::process::CommandExt;
    let from = fd.as_raw_fd();
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let placed = if from == at {
                libc::fcntl(at, libc::F_SETFD, 0)
            } else {
                libc::dup2(from, at)
            };
            if placed < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// A frame: four bytes of length, then the JSON.
pub fn frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap();
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

/// A host session over one end of a socket pair, the other end benchy's stdin and stdout.
pub struct Streamer {
    pub child: Child,
    pub sock: UnixStream,
    next: u64,
}

impl Drop for Streamer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Streamer {
    /// A session, not yet greeted.
    pub fn spawn(args: &[&str]) -> Streamer {
        Streamer::spawn_inheriting(args, None)
    }

    /// A session that also inherits the read end of a pipe as its descriptor 3, as a stream
    /// channel (`--stream-channel 3`, which `args` passes or not), and the write end.
    pub fn spawn_with_channel(args: &[&str]) -> (Streamer, File) {
        let (read, write) = pipe();
        (Streamer::spawn_inheriting(args, Some(read)), write)
    }

    fn spawn_inheriting(args: &[&str], channel: Option<OwnedFd>) -> Streamer {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let out = theirs.try_clone().unwrap();
        let mut cmd = Command::new(super::BIN);
        cmd.arg("host")
            .args(super::PLATFORM)
            .args(args)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::from(OwnedFd::from(out)))
            .stderr(Stdio::null());
        if let Some(fd) = &channel {
            inherit_as(&mut cmd, fd, CHANNEL_FD);
        }
        let child = cmd.spawn().unwrap();
        drop(channel);
        Streamer {
            child,
            sock: ours,
            next: 1000,
        }
    }

    /// A session, greeted, speaking the newest protocol it offers in `protocol`.
    pub fn start(args: &[&str], protocol: &[u64]) -> Streamer {
        let mut s = Streamer::spawn(args);
        let hello = s.call("hello", serde_json::json!({ "protocol": protocol }), &[]);
        assert_eq!(hello["ok"], true, "{hello}");
        s
    }

    /// Send a request carrying `fds`, without waiting for its reply. Returns its id.
    pub fn send(&mut self, call: &str, fields: Value, fds: &[RawFd]) -> u64 {
        self.next += 1;
        let mut req = fields;
        req["id"] = self.next.into();
        req["call"] = call.into();
        send_with_fds(&self.sock, &frame(&req), fds);
        self.next
    }

    pub fn recv(&mut self) -> Value {
        read_frame(&mut self.sock).expect("a reply")
    }

    pub fn call(&mut self, call: &str, fields: Value, fds: &[RawFd]) -> Value {
        let id = self.send(call, fields, fds);
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// Attach `bytes` as table `name`, streamed through a pipe written on a thread of its own, as
    /// a host would: the reply, and whether the writer finished — wrote everything or was told
    /// the reader had gone — rather than being left blocked on a pipe nobody reads.
    pub fn attach_stream(&mut self, name: &str, bytes: Vec<u8>) -> (Value, bool) {
        self.attach_stream_with(serde_json::json!({ "name": name, "stream": true }), bytes)
    }

    /// [`Streamer::attach_stream`] with the request's fields as given.
    pub fn attach_stream_with(&mut self, fields: Value, bytes: Vec<u8>) -> (Value, bool) {
        use std::io::Write;
        let (read, mut write) = pipe();
        let id = self.send("attach", fields, &[read.as_raw_fd()]);
        drop(read);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = write.write_all(&bytes);
            drop(write);
            let _ = tx.send(());
        });
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        let finished = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
        (reply, finished)
    }
}

/// The host's end of a stream channel: one writer thread that writes each stream given to it in
/// turn, back to back, as a host writes what it fetches, and reports each one written or not.
pub struct ChannelWriter {
    tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<std::io::Result<()>>,
}

impl ChannelWriter {
    pub fn new(mut channel: File) -> ChannelWriter {
        use std::io::Write;
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let (done_tx, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for bytes in rx {
                let _ = done_tx.send(channel.write_all(&bytes));
            }
        });
        ChannelWriter { tx: Some(tx), done }
    }

    /// Queue `bytes`, to be written once everything queued before them has been.
    pub fn feed(&self, bytes: Vec<u8>) {
        self.tx.as_ref().expect("open").send(bytes).unwrap();
    }

    /// How the oldest write not yet reported ended: written whole, or refused (`EPIPE` once benchy
    /// has closed the channel). `None` while it is still blocked, after `within`.
    pub fn written(&self, within: std::time::Duration) -> Option<std::io::Result<()>> {
        self.done.recv_timeout(within).ok()
    }

    /// Close the host's end once everything queued is written: benchy then reads its end.
    pub fn close(&mut self) {
        self.tx.take();
    }
}

/// A host session over plain pipes, as a Node.js host spawns one, with a stream channel on
/// descriptor 3. `args` says whether benchy is told about it (`--stream-channel 3`).
pub struct Channeled {
    pub child: Child,
    stdin: std::process::ChildStdin,
    stdout: std::process::ChildStdout,
    pub channel: ChannelWriter,
    next: u64,
}

impl Drop for Channeled {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Channeled {
    pub fn spawn(args: &[&str]) -> Channeled {
        Channeled::spawn_with(args, false)
    }

    /// [`Channeled::spawn`], the channel's read end non-blocking when `nonblocking`, as an event
    /// loop's pipes often are.
    pub fn spawn_with(args: &[&str], nonblocking: bool) -> Channeled {
        let (read, write) = pipe();
        if nonblocking {
            unsafe {
                let flags = libc::fcntl(read.as_raw_fd(), libc::F_GETFL);
                libc::fcntl(read.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        let mut cmd = Command::new(super::BIN);
        cmd.arg("host")
            .args(super::PLATFORM)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        inherit_as(&mut cmd, &read, CHANNEL_FD);
        let mut child = cmd.spawn().unwrap();
        drop(read);
        Channeled {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            child,
            channel: ChannelWriter::new(write),
            next: 2000,
        }
    }

    /// A session, greeted, speaking the newest protocol it offers in `protocol`; `hello` carries
    /// `fields` besides (`limits`).
    pub fn start(args: &[&str], protocol: &[u64], fields: Value) -> Channeled {
        let mut s = Channeled::spawn(args);
        let mut hello = fields;
        hello["protocol"] = serde_json::json!(protocol);
        let r = s.call("hello", hello);
        assert_eq!(r["ok"], true, "{r}");
        s
    }

    pub fn send(&mut self, call: &str, fields: Value) -> u64 {
        use std::io::Write;
        self.next += 1;
        let mut req = fields;
        req["id"] = self.next.into();
        req["call"] = call.into();
        self.stdin.write_all(&frame(&req)).unwrap();
        self.stdin.flush().unwrap();
        self.next
    }

    /// Send `body` as one frame, exactly as given: an id of the test's choosing, a key twice.
    pub fn send_body(&mut self, body: &[u8]) {
        use std::io::Write;
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        self.stdin.write_all(&out).unwrap();
        self.stdin.flush().unwrap();
    }

    pub fn recv(&mut self) -> Value {
        read_frame(&mut self.stdout).expect("a reply")
    }

    pub fn call(&mut self, call: &str, fields: Value) -> Value {
        let id = self.send(call, fields);
        let reply = self.recv();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }
}
