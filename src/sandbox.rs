//! REQ-3: the process has no filesystem to reach.
//!
//! This is load-bearing rather than defence in depth. REQ-2's two mechanisms are complementary,
//! not redundant — the statement allowlist cannot stop a file read, because file-reading table
//! functions are typed `SELECT`. The redundancy comes from here.
//!
//! Measured on kernel 6.8.0: ABI V1–V4 fully enforced, V5 partially. A handled right that no rule
//! grants is refused on every path, **while an already-open descriptor keeps working** — which is
//! what makes "load, then restrict" work. `benchy-mcp doctor --probe` measures each ruleset on the
//! machine it runs on.
//!
//! Every ruleset handles the filesystem rights of the newest ABI this kernel speaks, up to V5
//! ([`fs_abi`]): V2's `Refer` (linking or renaming across directories), V3's `Truncate` and V5's
//! `IoctlDev`. Handling only V1's, as every ruleset once did, left truncate(2) by path unhandled,
//! so a confined process could empty any file this user can write
//! (REQ-3 `a_confined_process_cannot_truncate_what_it_was_not_granted`). Measured on kernel 6.8.0,
//! which speaks ABI 4: `Refer` and `Truncate` handled, every posture fully enforced, a truncate
//! outside the grants refused with `EACCES`. Only the loaders' staging directories get `Truncate`
//! ([`staging_rights`]). Nothing gets `Refer` or `IoctlDev`: traced with `strace`, no confined
//! process links, renames or ioctls a device (the supervisor moves a loader's file, unconfined),
//! and the engine's delete within its session is an unlink and a rmdir, which need neither.
//!
//! Landlock confines the thread that asks and the threads it starts afterwards, never a thread
//! already running; [`alone`] is where that is made to hold, in the engine and in every loader.
//!
//! ABI V1 is the minimum: its filesystem rights have been present since kernel 5.13.

#[cfg(target_os = "linux")]
use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, NetPort,
    Ruleset, RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetError, RulesetStatus, ABI,
};
#[cfg(target_os = "linux")]
use std::path::Path;

// Landlock is Linux's. Elsewhere the same functions say there is none, from a file of their own:
// the mutation run (.cargo/mutants.toml) is on Linux, which never compiles them, so no test there
// could see a change to them, and that file is left out of it.
#[cfg(not(target_os = "linux"))]
mod elsewhere;
#[cfg(not(target_os = "linux"))]
pub use elsewhere::{landlock_abi, restrict_loader, restrict_network_loader, restrict_to};

/// The minimum ABI whose filesystem rights this design needs.
pub const MINIMUM_ABI: &str = "V1";
/// [`MINIMUM_ABI`] as the number the kernel answers with ([`landlock_abi`]).
pub const MINIMUM_ABI_NUMBER: u32 = 1;

/// The first Landlock ABI with TCP rules (Linux 6.7). Every ruleset handles them, so below it each
/// is [`Posture::Partial`]: the filesystem confined, the network not.
pub const NETWORK_ABI: u32 = 4;

/// How much of a ruleset the kernel enforces, as `restrict_self` reported it. The engine sends
/// its own to the server, which reports it in `hello` and on the operator's log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Posture {
    /// Every requested right was enforced.
    Full,
    /// Some requested rights were not enforceable by this kernel. **This does not mean the
    /// filesystem boundary leaks** — at V5 on kernel 6.8 the status is `PartiallyEnforced` and
    /// every filesystem open is still denied. Every ruleset handles the kernel's own filesystem
    /// ABI ([`fs_abi`]) and V4's TCP rights, so what this means in practice is a kernel below
    /// [`NETWORK_ABI`]: no TCP rules, and a confined process may connect anywhere. Report the
    /// level, never a bare boolean.
    Partial,
    /// Landlock produced no restriction at all.
    None,
    /// Landlock is unavailable on this kernel.
    Unavailable(String),
}

impl Posture {
    /// Whether the filesystem boundary this design relies on is actually in force.
    pub fn is_enforced(&self) -> bool {
        matches!(self, Posture::Full | Posture::Partial)
    }

    /// The posture as a host reads it in `hello`'s `sandbox_posture`: one of four fixed words,
    /// where [`describe`](Self::describe) is prose and may be reworded.
    pub fn name(&self) -> &'static str {
        match self {
            Posture::Full => "full",
            Posture::Partial => "partial",
            Posture::None => "none",
            Posture::Unavailable(_) => "unavailable",
        }
    }

    /// Why this posture, on a kernel with Landlock ABI `abi`, falls short of confining files and
    /// TCP both, or `None` where it does not. Only a fully enforced ruleset at [`NETWORK_ABI`] or
    /// later confines TCP: `Partial` says some rights were dropped, not which.
    pub fn shortfall(&self, abi: u32) -> Option<String> {
        match self {
            Posture::Full if abi >= NETWORK_ABI => None,
            Posture::Full | Posture::Partial if abi < NETWORK_ABI => Some(format!(
                "this kernel's Landlock ABI {abi} has no TCP rules (they came with ABI \
                 {NETWORK_ABI}, Linux 6.7): files are confined, the network is not"
            )),
            Posture::Full | Posture::Partial => {
                Some("this kernel enforces some of the ruleset's rights and not others".into())
            }
            Posture::None => Some("Landlock enforced none of the ruleset".into()),
            Posture::Unavailable(why) => Some(format!("Landlock is unavailable: {why}")),
        }
    }

    /// REQ-3 `a_kernel_without_tcp_rules_is_reported_as_partial`: the posture as `hello` reports
    /// it in `sandbox_posture`, for a host to act on without reading prose. `filesystem` is
    /// whether the filesystem boundary holds, `network` whether TCP is confined too, and `reason`
    /// why the posture falls short of both, `null` where it does not.
    pub fn report(&self, abi: u32) -> serde_json::Value {
        let reason = self.shortfall(abi);
        serde_json::json!({
            "posture": self.name(),
            "landlock_abi": abi,
            "filesystem": self.is_enforced(),
            "network": reason.is_none(),
            "reason": reason,
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Posture::Full => "fully enforced".into(),
            Posture::Partial => "enforced, some newer rights unsupported by this kernel".into(),
            Posture::None => "not enforced".into(),
            Posture::Unavailable(why) => format!("unavailable: {why}"),
        }
    }
}

/// Whether `who` — the engine, or a loader — may go on under `posture`. Without Landlock a
/// compromised process could read any file this user can, and the SQL layer would be the only
/// boundary — so running that way is the operator's explicit choice, never a silent default.
pub fn admit(posture: &Posture, allow_unsandboxed: bool, who: &str) -> Result<(), String> {
    if posture.is_enforced() || allow_unsandboxed {
        return Ok(());
    }
    Err(format!(
        "Landlock could not confine the {who} ({}). Without it a compromised {who} could read any \
         file this user can, and the SQL layer would be the only boundary. Start with \
         --allow-unsandboxed to run that way on purpose.",
        posture.describe()
    ))
}

/// REQ-3 `a_session_that_must_be_fully_sandboxed_refuses_to_start_otherwise`: whether a server
/// started with `--require-full-sandbox` may go on, its engine confined as `posture` on a kernel
/// with Landlock ABI `abi`. [`admit`] lets `Partial` through without a flag, since the filesystem
/// boundary holds; an operator who relies on the network boundary too says so with this one,
/// which is stricter than `--allow-unsandboxed` and wins over it.
pub fn require_full(posture: &Posture, abi: u32) -> Result<(), String> {
    match posture.shortfall(abi) {
        None => Ok(()),
        Some(why) => Err(format!(
            "--require-full-sandbox: Landlock does not fully confine the engine here ({why}). \
             Run on Linux 6.7 or later with Landlock enabled, or start without the flag."
        )),
    }
}

/// The Landlock ABI this kernel speaks, or 0 where it has none. ABI 4 (Linux 6.7) is the first
/// with TCP rules; below it, a best-effort ruleset confines the filesystem and not the network.
#[cfg(target_os = "linux")]
pub fn landlock_abi() -> u32 {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    u32::try_from(v).unwrap_or(0)
}

/// The filesystem rights every ruleset handles: those of the kernel's own ABI, up to V5, and V1's
/// at least. Capped at the kernel's rather than asking for V5 everywhere: a right the kernel lacks
/// cannot be enforced anyway, and asking for it would only turn a `Full` posture into `Partial`.
#[cfg(target_os = "linux")]
fn fs_abi() -> ABI {
    ABI::from(landlock_abi().clamp(1, 5) as i32)
}

/// What a loader may do beneath its staging directory: everything V1 names, and truncate. A load
/// traced with `strace` (840,960 CSV rows) truncated nothing, but DuckDB can truncate its own
/// write-ahead log there, after a commit that failed part-way, and the files are the loader's own.
/// Not `Refer`: nothing a loader writes is linked or moved across directories. Below ABI 3 there
/// is no `Truncate` to grant, nor to withhold.
#[cfg(target_os = "linux")]
fn staging_rights(abi: ABI) -> BitFlags<AccessFs> {
    grant(abi, AccessFs::from_all(ABI::V1) | AccessFs::Truncate)
}

/// A ruleset built and not yet applied.
#[cfg(target_os = "linux")]
type Built = Result<RulesetCreated, RulesetError>;

/// `want`, less what a ruleset handling ABI `abi`'s filesystem rights does not handle. A rule may
/// grant only rights its ruleset handles: landlock refuses any other (`UnhandledAccess`), which
/// would leave the process unconfined, and refusing to run, wherever the kernel is older than
/// the right (REQ-3 `every_ruleset_builds_for_every_landlock_abi`).
#[cfg(target_os = "linux")]
fn grant(abi: ABI, want: BitFlags<AccessFs>) -> BitFlags<AccessFs> {
    want & AccessFs::from_all(abi)
}

/// An empty ruleset handling ABI `abi`'s filesystem rights and, where the kernel has them, its
/// TCP rights.
#[cfg(target_os = "linux")]
fn created(abi: ABI) -> Built {
    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.handle_access(AccessNet::from_all(ABI::V4)))
        .and_then(|r| r.create())
}

/// Apply `built` to the calling thread, and say how much of it the kernel enforces.
#[cfg(target_os = "linux")]
fn seal(built: Built) -> Posture {
    match built.and_then(|r| r.restrict_self()) {
        Ok(r) => classify(r.ruleset),
        Err(e) => Posture::Unavailable(e.to_string()),
    }
}

/// Whether the calling thread is the only one in its process, which is the condition for
/// confining it: Landlock confines the thread that asks and the threads it starts afterwards,
/// never one already running, and a thread left running would stay unconfined for the life of
/// the process. A thread that is finishing gets `grace` to go.
///
/// Where there is no `/proc` there is no Landlock either, and nothing to count.
pub fn alone(grace: std::time::Duration) -> Result<(), String> {
    alone_by(
        || std::fs::read_dir("/proc/self/task").map_or(1, |d| d.count()),
        grace,
    )
}

/// [`alone`], counting with `threads`. Apart so the wait can be tested: a test process is never
/// alone — the harness has a thread of its own — so through `/proc` only the refusal can be.
fn alone_by(threads: impl Fn() -> usize, grace: std::time::Duration) -> Result<(), String> {
    let t = std::time::Instant::now();
    while threads() > 1 && t.elapsed() < grace {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    match threads() {
        1 => Ok(()),
        n => Err(format!(
            "{n} threads were running, and Landlock only confines threads started after it"
        )),
    }
}

#[cfg(target_os = "linux")]
fn classify(s: RulesetStatus) -> Posture {
    match s {
        RulesetStatus::FullyEnforced => Posture::Full,
        RulesetStatus::PartiallyEnforced => Posture::Partial,
        RulesetStatus::NotEnforced => Posture::None,
    }
}

/// Restrict this process to reading beneath `path`, and to deleting beneath `session`, which
/// holds `path`. Nothing else: no other file can be opened, created, written or removed, and —
/// where the kernel has Landlock's network rights, 6.7 and later — no TCP connection opened or
/// port listened on.
///
/// Deleting is the one write it keeps, so that an engine whose server was killed can still
/// remove the data the session holds (`engine::Watch`). The price: an engine compromised through
/// SQL could delete the store too, and the next rebuild would then fail and say so. Deleting a
/// directory's contents means listing them, so it may list — names, never contents — beneath
/// `session` too. Removing `session`
/// itself would need a right on its parent, the shared temporary directory, which it does not
/// get.
#[cfg(target_os = "linux")]
pub fn restrict_to(path: &Path, session: &Path) -> Posture {
    seal(engine_ruleset(fs_abi(), path, session))
}

/// [`restrict_to`]'s ruleset, built for filesystem ABI `abi` and not yet applied.
#[cfg(target_os = "linux")]
fn engine_ruleset(abi: ABI, path: &Path, session: &Path) -> Built {
    let remove = AccessFs::RemoveFile | AccessFs::RemoveDir | AccessFs::ReadDir;
    created(abi)
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                [path],
                grant(abi, AccessFs::from_read(ABI::V1)),
            ))
        })
        .and_then(|r| r.add_rules(path_beneath_rules([session], grant(abi, remove))))
}

/// A loader's restriction: read the one input, anything beneath its own staging directory, and
/// nothing else — no other file, and no TCP connection or listening port where the kernel has
/// Landlock's network rights (6.7 and later). DuckDB writes a `.wal` and may spill next to the
/// file it builds, so the staging directory is the smallest thing that can be granted.
#[cfg(target_os = "linux")]
pub fn restrict_loader(input: &Path, stage: &Path) -> Posture {
    seal(loader_ruleset(fs_abi(), input, stage))
}

/// [`restrict_loader`]'s ruleset, built for filesystem ABI `abi` and not yet applied.
#[cfg(target_os = "linux")]
fn loader_ruleset(abi: ABI, input: &Path, stage: &Path) -> Built {
    created(abi)
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                [input],
                grant(abi, AccessFs::from_read(ABI::V1)),
            ))
        })
        .and_then(|r| r.add_rules(path_beneath_rules([stage], staging_rights(abi))))
}

/// The files a network loader may read to turn a host name into an address: glibc's
/// `getaddrinfo`, which Rust's resolver and so ureq's call, and nothing else.
///
/// Measured with `strace -f` on a confined ClickHouse loader on Ubuntu 24.04 (glibc 2.39,
/// `hosts: files dns`, kernel 6.8), reaching `localhost` and a public name over https: these five
/// are the files it opens, and none is refused. `/etc/resolv.conf` there is a link to
/// systemd-resolved's stub file; the rule follows it, since Landlock rules hold the file, not the
/// name. A file that does not exist is left out of the ruleset. Beyond these, glibc tries the nscd
/// socket (a Unix socket, not Landlock's to refuse; absent there), asks the kernel for the
/// interfaces' addresses over netlink, and sends the query over UDP to the resolver in
/// `resolv.conf`. `files` and `dns` are built into glibc since 2.34, so no `libnss_*` module is
/// loaded; a `hosts:` line naming another (`mdns4_minimal`, `resolve`) needs its library, which
/// is refused, so that source is unavailable to the loader (not measured here). TLS reads no file: ureq's `rustls`
/// feature carries its roots in the binary (webpki-roots), and none are read from disk.
pub const RESOLVER_FILES: [&str; 5] = [
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
];

/// A network loader's restriction (REQ-3 `a_network_loader_reaches_only_its_source_and_its_staging`):
/// read and write beneath its staging directory, read [`RESOLVER_FILES`], and — where the kernel
/// has Landlock's network rights, 6.7 and later — open TCP connections to `ports` and no other,
/// and listen on none. Landlock names ports, not hosts: which host is the source's own code's
/// to keep to (no proxy, no redirect). A DNS query goes over UDP, which Landlock does not
/// handle; a reply too large for UDP would be retried over TCP to port 53, which is refused.
#[cfg(target_os = "linux")]
pub fn restrict_network_loader(stage: &Path, ports: &[u16]) -> Posture {
    seal(network_ruleset(fs_abi(), stage, ports))
}

/// [`restrict_network_loader`]'s ruleset, built for filesystem ABI `abi` and not yet applied.
#[cfg(target_os = "linux")]
fn network_ruleset(abi: ABI, stage: &Path, ports: &[u16]) -> Built {
    created(abi)
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                RESOLVER_FILES,
                grant(abi, AccessFs::ReadFile.into()),
            ))
        })
        .and_then(|r| r.add_rules(path_beneath_rules([stage], staging_rights(abi))))
        .and_then(|r| {
            r.add_rules(
                ports.iter().map(|&p| {
                    Ok::<_, landlock::RulesetError>(NetPort::new(p, AccessNet::ConnectTcp))
                }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::alone_by;
    use std::time::{Duration, Instant};

    /// A grant of `Truncate` once went into every loader's ruleset whatever the kernel, and on
    /// kernels before 6.2 (ABI 1 and 2), whose rulesets do not handle it, landlock refused the
    /// rule: every loader went unconfined, or refused to run. Each grant is now cut to what its
    /// ruleset handles.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_ruleset_builds_for_every_landlock_abi() {
        // A rule granting a right its ruleset does not handle is refused, and the process then
        // goes unconfined, or refuses to run. The handled rights follow the kernel's ABI, so
        // each restriction is built here for every ABI it may meet, V1 (Linux 5.13) to V5,
        // whatever this kernel speaks — built and filled, never applied to this test.
        use super::{engine_ruleset, loader_ruleset, network_ruleset};
        use landlock::ABI;
        let dir = std::env::temp_dir().join(format!("benchy-abi-{}", std::process::id()));
        let (ws, stage, input) = (dir.join("ws"), dir.join("stage"), dir.join("input.csv"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(&input, "n\n1\n").unwrap();
        for abi in [ABI::V1, ABI::V2, ABI::V3, ABI::V4, ABI::V5] {
            for (who, built) in [
                ("engine", engine_ruleset(abi, &ws, &dir)),
                ("file loader", loader_ruleset(abi, &input, &stage)),
                ("network loader", network_ruleset(abi, &stage, &[8123])),
            ] {
                if let Err(e) = built {
                    panic!("the {who}'s ruleset for {abi:?}: {e}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_kernel_without_tcp_rules_is_reported_as_partial() {
        // Below ABI 4 (Linux 6.7) every ruleset handles TCP rights the kernel lacks, and comes
        // back `PartiallyEnforced` (landlock 0.4.7: an unsupported access under BestEffort makes
        // the state `Partial`). Its prose said only "some newer rights unsupported", and
        // `admit` lets it through, so nothing said the engine could open any connection. No
        // kernel before 6.7 is to be had here, so the posture is given with its ABI.
        use super::{require_full, Posture};
        let partial = Posture::Partial.report(3);
        assert_eq!(partial["posture"], "partial", "{partial}");
        assert_eq!(partial["landlock_abi"], 3, "{partial}");
        assert_eq!(partial["filesystem"], true, "{partial}");
        assert_eq!(partial["network"], false, "{partial}");
        assert!(
            partial["reason"]
                .as_str()
                .is_some_and(|r| r.contains("TCP") && r.contains("6.7")),
            "{partial}"
        );
        let err = require_full(&Posture::Partial, 3).expect_err("no TCP rules");
        assert!(
            err.contains("--require-full-sandbox") && err.contains("TCP"),
            "{err}"
        );
        // Partial at an ABI with TCP rules is some other right dropped: still not full.
        assert_eq!(Posture::Partial.report(4)["network"], false);
        assert!(require_full(&Posture::Partial, 5).is_err());
        // And it does not blame TCP rules the kernel has.
        for abi in [4, 5] {
            let r = Posture::Partial.report(abi);
            assert!(
                r["reason"].as_str().is_some_and(
                    |r| r.contains("some of the ruleset's rights") && !r.contains("TCP")
                ),
                "{r}"
            );
        }
        // Full is full only with TCP rules to enforce.
        let full = Posture::Full.report(4);
        assert_eq!(full["posture"], "full", "{full}");
        assert_eq!(full["network"], true, "{full}");
        assert!(full["reason"].is_null(), "{full}");
        assert_eq!(require_full(&Posture::Full, 4), Ok(()));
        assert!(require_full(&Posture::Full, 3).is_err());
        for (p, name) in [
            (Posture::None, "none"),
            (Posture::Unavailable("gone".into()), "unavailable"),
        ] {
            let r = p.report(4);
            assert_eq!(r["posture"], name, "{r}");
            assert_eq!(r["filesystem"], false, "{r}");
            assert_eq!(r["network"], false, "{r}");
            assert!(r["reason"].is_string(), "{r}");
            assert!(require_full(&p, 4).is_err(), "{name}");
        }
    }

    #[test]
    fn a_process_already_alone_is_confined_at_once() {
        // Every engine and every file loader asks, so a check that waited out its grace when
        // there was nothing to wait for would cost each of them that grace.
        let t = Instant::now();
        assert_eq!(alone_by(|| 1, Duration::from_secs(5)), Ok(()));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn a_thread_that_finishes_within_the_grace_is_waited_for() {
        // DuckDB's threads take a moment to go after `SET threads = 1`; refusing on the first
        // count would refuse a process that is about to be alone.
        let t = Instant::now();
        let threads = || {
            if t.elapsed() < Duration::from_millis(200) {
                2
            } else {
                1
            }
        };
        assert_eq!(alone_by(threads, Duration::from_secs(5)), Ok(()));
    }

    #[test]
    fn a_thread_that_stays_past_the_grace_is_refused() {
        // The other thread here leaves after three seconds, so a check that ignored its grace
        // ends too — passing, which is the failure — rather than hanging the suite.
        let t = Instant::now();
        let threads = || {
            if t.elapsed() < Duration::from_secs(3) {
                2
            } else {
                1
            }
        };
        let err = alone_by(threads, Duration::from_millis(100)).expect_err("still running");
        assert!(err.starts_with("2 threads were running"), "{err}");
        assert!(t.elapsed() >= Duration::from_millis(100), "it waited");
    }
}
