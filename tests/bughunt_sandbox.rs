//! Security bughunt (PRJ-53): what a confined engine or loader can still reach.
//!
//! REQ-2: "The query engine cannot reach the filesystem or the network." REQ-3: the engine may
//! read only its workspace, a file loader only its input; from Linux 6.7 `hello` reports the
//! posture `full`, `network: true`. Each restriction is applied here to a thread of its own, as
//! `a_confined_process_cannot_truncate_what_it_was_not_granted` does, and then tries to reach a
//! listener this test holds outside every grant.
#![cfg(target_os = "linux")]

mod common;
use benchy_mcp::sandbox;
use common::unique;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Run `probe` on a thread confined by each of the server's three restrictions.
fn under_each(dir: &Path, probe: impl Fn() -> bool + Sync) -> Vec<(&'static str, bool)> {
    let (ws, stage, input) = (dir.join("ws"), dir.join("stage"), dir.join("input.csv"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(&input, "n\n1\n").unwrap();
    let restrictions: [(&str, &(dyn Fn() -> sandbox::Posture + Sync)); 3] = [
        ("engine", &|| sandbox::restrict_to(&ws, dir)),
        ("file loader", &|| sandbox::restrict_loader(&input, &stage)),
        ("network loader", &|| {
            sandbox::restrict_network_loader(&stage, &[])
        }),
    ];
    restrictions
        .iter()
        .map(|(who, restrict)| {
            let reached = std::thread::scope(|s| {
                s.spawn(|| {
                    assert!(restrict().is_enforced(), "{who} not confined");
                    probe()
                })
                .join()
                .unwrap()
            });
            (*who, reached)
        })
        .collect()
}

/// At the `full` posture (`network: true`), a confined process still sends UDP anywhere:
/// Landlock's network rules (ABI 4) handle TCP alone, and nothing else is applied.
#[test]
fn a_fully_confined_process_sends_no_udp_datagram() {
    if sandbox::landlock_abi() < sandbox::NETWORK_ABI {
        eprintln!("skipped: below ABI 4 the posture says the network is not confined");
        return;
    }
    let dir = unique("udp");
    let outside = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    outside
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let to = outside.local_addr().unwrap();
    let got = under_each(&dir, || {
        // Refused a socket at all, which the socket filter does where none is allowed.
        let Ok(sock) = std::net::UdpSocket::bind("127.0.0.1:0") else {
            return false;
        };
        let _ = sock.send_to(b"canary", to);
        let mut buf = [0u8; 16];
        matches!(outside.recv(&mut buf), Ok(6))
    });
    let _ = std::fs::remove_dir_all(&dir);
    // The network loader keeps UDP for DNS (docs/HTTP-SOURCE.md; Sven, 2026-11-04, PRJ-100):
    // seccomp cannot tell a resolver's address from another, which is a pointer.
    assert_eq!(
        got,
        [
            ("engine", false),
            ("file loader", false),
            ("network loader", true)
        ],
        "hello says `network: true`, yet a confined process delivered a UDP datagram"
    );
}

/// A Unix socket outside every grant, reached by its path: Landlock's filesystem rights do not
/// cover connect(2), so the engine talks to any service listening on one (a D-Bus session bus,
/// a Docker socket), which reads files and starts processes for it.
#[test]
fn a_confined_process_reaches_no_unix_socket_outside_its_grants() {
    let dir = unique("unixsock");
    std::fs::create_dir_all(&dir).unwrap();
    let path = std::env::temp_dir().join(format!("benchy-bughunt-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let got = under_each(&dir, || {
        let Ok(mut s) = std::os::unix::net::UnixStream::connect(&path) else {
            return false;
        };
        let _ = s.write_all(b"canary");
        drop(s);
        std::thread::sleep(Duration::from_millis(50));
        match listener.accept() {
            Ok((mut c, _)) => {
                c.set_nonblocking(false).unwrap();
                let mut buf = Vec::new();
                let _ = c.read_to_end(&mut buf);
                buf == b"canary"
            }
            Err(_) => false,
        }
    });
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        got,
        [
            ("engine", false),
            ("file loader", false),
            ("network loader", false)
        ],
        "a confined process connected to a Unix socket outside its workspace and staging"
    );
}

/// What else the socket filter holds (PRJ-100): no io_uring, which opens sockets without
/// `socket()`; no Unix socket even unbound; and for the network loader, netlink's route family for
/// the resolver, and no other netlink protocol. A `socketpair` stays: it has no peer outside.
#[test]
fn the_socket_filter_refuses_what_else_reaches_out() {
    let dir = unique("sockfilter");
    let fd = |domain: i32, ty: i32, proto: i32| {
        let fd = unsafe { libc::socket(domain, ty, proto) };
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
        fd >= 0
    };
    let ring = || {
        let r = unsafe { libc::syscall(libc::SYS_io_uring_setup, 1u32, [0u8; 120].as_mut_ptr()) };
        if r >= 0 {
            unsafe { libc::close(r as i32) };
        }
        r >= 0
    };
    // A container's own seccomp profile may refuse io_uring to everyone (bx's does): then only the
    // rest is shown here, and a host with io_uring shows the rule too (CI's runners).
    let ring_here = ring();
    if !ring_here {
        eprintln!("io_uring is unavailable here even unconfined: its refusal is not shown");
    }
    let got = under_each(&dir, || {
        let others = [
            fd(libc::AF_UNIX, libc::SOCK_DGRAM, 0),
            fd(
                libc::AF_NETLINK,
                libc::SOCK_RAW,
                libc::NETLINK_KOBJECT_UEVENT,
            ),
            fd(libc::AF_PACKET, libc::SOCK_DGRAM, 0),
            ring_here && ring(),
        ];
        others.iter().any(|o| *o)
    });
    assert_eq!(
        got,
        [
            ("engine", false),
            ("file loader", false),
            ("network loader", false)
        ],
        "a confined process opened an io_uring, a Unix socket or another family"
    );
    let routes = under_each(&dir, || {
        fd(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE)
    });
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        routes,
        [
            ("engine", false),
            ("file loader", false),
            ("network loader", true)
        ],
        "the network loader's resolver reads the interfaces over netlink's route family"
    );
}

/// Linux 6.8's Landlock checks the ports of TCP sockets alone: an MPTCP socket of the network
/// loader connected to a port it was never granted, and an SCTP one had no rule at all
/// (PRJ-100, sibling-door hunt). Only TCP and UDP sockets are opened now: the rest are refused
/// before any connect.
#[test]
fn a_network_loader_opens_no_mptcp_or_sctp_socket() {
    if sandbox::landlock_abi() < sandbox::NETWORK_ABI {
        eprintln!("skipped: below ABI 4 the posture says the network is not confined");
        return;
    }
    let dir = unique("mptcp");
    let stage = dir.join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    let opened = std::thread::scope(|s| {
        s.spawn(|| {
            assert!(sandbox::restrict_network_loader(&stage, &[1]).is_enforced());
            let open = |ty: i32, proto: i32| {
                let fd = unsafe { libc::socket(libc::AF_INET, ty, proto) };
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                }
                fd >= 0
            };
            [
                ("mptcp", open(libc::SOCK_STREAM, 262)),
                ("sctp", open(libc::SOCK_STREAM, libc::IPPROTO_SCTP)),
                ("seqpacket", open(libc::SOCK_SEQPACKET, 0)),
                ("raw", open(libc::SOCK_RAW, libc::IPPROTO_ICMP)),
            ]
        })
        .join()
        .unwrap()
    });
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        opened,
        [
            ("mptcp", false),
            ("sctp", false),
            ("seqpacket", false),
            ("raw", false)
        ],
        "the network loader opened a socket Landlock's port rules do not hold"
    );
}

/// Below Landlock ABI 6 no scope keeps signals in the domain: the confined engine SIGKILLed
/// another process of its user, the server included (PRJ-100, sibling-door hunt). The engine
/// may signal itself alone now; a loader stops the connector it started.
#[test]
fn the_engine_signals_no_other_process() {
    let dir = unique("signals");
    let (ws, _) = (dir.join("ws"), std::fs::create_dir_all(dir.join("ws")));
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    let (killed, own) = std::thread::scope(|s| {
        s.spawn(|| {
            assert!(sandbox::restrict_to(&ws, &dir).is_enforced());
            let killed = unsafe { libc::kill(pid, libc::SIGKILL) } == 0;
            let own = unsafe { libc::kill(libc::getpid(), 0) } == 0;
            (killed, own)
        })
        .join()
        .unwrap()
    });
    let alive = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !killed && alive,
        "the confined engine killed another process"
    );
    assert!(own, "the engine may still signal itself");
}
