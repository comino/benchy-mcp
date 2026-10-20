//! Stage timings for the ingest benchmark (`bench/ingest.py`, docs/PERFORMANCE.md). Off unless
//! the server is started with `BENCHY_TRACE_TIMINGS=1`; the server then starts its engine and
//! loaders with [`FLAG`], since neither inherits its environment.
//!
//! Every line goes to stderr, the operator's log, prefixed `benchy-trace `, one JSON object per
//! line. Nothing of it reaches the agent: the engine's stderr is the server's, and no reply
//! carries it. Off, a span costs one relaxed atomic load.
//!
//! Times are `CLOCK_MONOTONIC` in microseconds, which is one clock for every process on the
//! machine, so a child's first line minus its parent's `spawn` line is what starting a process
//! costs (exec, the dynamic linker relocating DuckDB's library, its static constructors). CPU is
//! the whole process's (`CLOCK_PROCESS_CPUTIME_ID`, every thread), because DuckDB works on many.
//! Peak memory is `ru_maxrss`, not `/proc/self/status`: a confined loader or engine may not read
//! `/proc` (REQ-3).

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

/// The argument the server adds to an engine's or a loader's command line when tracing.
pub const FLAG: &str = "--trace-timings";
/// The environment variable that turns tracing on, read by the server alone.
pub const ENV: &str = "BENCHY_TRACE_TIMINGS";

static ON: AtomicBool = AtomicBool::new(false);
static ROLE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Turn tracing on for this process, as `role` ("server", "engine", "loader"), and say when the
/// process reached `main`.
pub fn enable(role: &'static str) {
    let _ = ROLE.set(role);
    ON.store(true, Ordering::Relaxed);
    emit("main", None, &[]);
}

#[inline]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

fn clock(id: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

/// Now, in microseconds of `CLOCK_MONOTONIC`.
pub fn now() -> u64 {
    clock(libc::CLOCK_MONOTONIC)
}

fn cpu() -> u64 {
    clock(libc::CLOCK_PROCESS_CPUTIME_ID)
}

/// Run `f` as stage `what`, and say how long it took and what CPU it cost.
#[inline]
pub fn span<T>(what: &str, f: impl FnOnce() -> T) -> T {
    if !on() {
        return f();
    }
    let (t0, c0) = (now(), cpu());
    let out = f();
    let (t1, c1) = (now(), cpu());
    emit(
        what,
        Some((t0, t1)),
        &[("cpu_us", c1.saturating_sub(c0).to_string())],
    );
    out
}

/// [`span`], for a stage that shares its process with another running at the same time: the CPU
/// it reports is its own thread's, where the process's would count the other stage's too. A
/// stream's insert and the watch beside it (`crate::source::Ingest`) are each one thread —
/// DuckDB is held to one while a stream goes in — so each is said exactly.
#[inline]
pub fn span_thread<T>(what: &str, f: impl FnOnce() -> T) -> T {
    if !on() {
        return f();
    }
    let (t0, c0) = (now(), clock(libc::CLOCK_THREAD_CPUTIME_ID));
    let out = f();
    let (t1, c1) = (now(), clock(libc::CLOCK_THREAD_CPUTIME_ID));
    emit(
        what,
        Some((t0, t1)),
        &[("cpu_us", c1.saturating_sub(c0).to_string())],
    );
    out
}

/// This thread's id, for a line's `tid`: stages on two threads of one process overlap, and
/// `bench/ingest.py --record` tells their samples apart by it.
fn tid() -> u64 {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::gettid() as u64 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::id() as u64
    }
}

/// Stage `what`, begun at `t0` (from [`now`]) and over now: for a stage that is not one closure.
pub fn since(what: &str, t0: u64) {
    if on() {
        emit(what, Some((t0, now())), &[]);
    }
}

/// A moment, with numbers attached: `("rows", "123")`. Values are written as they are, so they
/// must be JSON already (a number, or a quoted string).
pub fn mark(what: &str, fields: &[(&str, String)]) {
    if on() {
        emit(what, None, fields);
    }
}

/// What this process has cost so far: CPU in user and kernel mode, its peak resident memory,
/// block I/O, and the same for every child it has reaped (a connector, for a loader).
pub fn usage(what: &str) {
    if !on() {
        return;
    }
    let get = |who| unsafe {
        let mut r: libc::rusage = std::mem::zeroed();
        libc::getrusage(who, &mut r);
        r
    };
    let us = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    let me = get(libc::RUSAGE_SELF);
    let kids = get(libc::RUSAGE_CHILDREN);
    emit(
        what,
        None,
        &[
            ("user_us", us(me.ru_utime).to_string()),
            ("sys_us", us(me.ru_stime).to_string()),
            ("maxrss_kb", maxrss_kib(me.ru_maxrss).to_string()),
            ("minflt", me.ru_minflt.to_string()),
            ("majflt", me.ru_majflt.to_string()),
            ("inblock", me.ru_inblock.to_string()),
            ("oublock", me.ru_oublock.to_string()),
            ("nvcsw", me.ru_nvcsw.to_string()),
            ("nivcsw", me.ru_nivcsw.to_string()),
            ("children_user_us", us(kids.ru_utime).to_string()),
            ("children_sys_us", us(kids.ru_stime).to_string()),
            ("children_maxrss_kb", maxrss_kib(kids.ru_maxrss).to_string()),
        ],
    );
}

/// `ru_maxrss` in KiB, the unit `maxrss_kb` says and `bench/ingest.py` reads. getrusage(2)
/// counts it in KiB on Linux and the BSDs and in bytes on macOS, where it was written unconverted
/// and a process at about 513 MB was traced as `maxrss_kb: 546504704`.
fn maxrss_kib(raw: libc::c_long) -> u64 {
    kib(raw.max(0) as u64, cfg!(target_vendor = "apple"))
}

fn kib(raw: u64, in_bytes: bool) -> u64 {
    if in_bytes {
        raw / 1024
    } else {
        raw
    }
}

fn emit(what: &str, span: Option<(u64, u64)>, fields: &[(&str, String)]) {
    let mut line = format!(
        "benchy-trace {{\"role\":\"{}\",\"pid\":{},\"tid\":{},\"what\":{}",
        ROLE.get().copied().unwrap_or("?"),
        std::process::id(),
        tid(),
        json_string(what)
    );
    match span {
        Some((t0, t1)) => line += &format!(",\"t0\":{t0},\"t1\":{t1},\"us\":{}", t1 - t0),
        None => line += &format!(",\"t\":{}", now()),
    }
    for (k, v) in fields {
        line += &format!(",\"{k}\":{v}");
    }
    line += "}\n";
    // One write, so lines from several processes on the same stderr do not interleave.
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

/// `s` as a JSON string.
pub fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_off_runs_its_work_and_says_nothing() {
        assert!(!on());
        assert_eq!(span("x", || 41 + 1), 42);
    }

    #[test]
    fn peak_memory_is_said_in_kib_on_every_platform() {
        // getrusage(2): `ru_maxrss` is KiB on Linux and bytes on macOS, where a process `ps`
        // put at about 513 MB was traced as `maxrss_kb: 546504704`.
        assert_eq!(kib(546_504_704, true), 533_696);
        assert_eq!(kib(533_696, false), 533_696);
        // And this platform's unit is the one taken: on Linux the peak agrees with the kernel's
        // own `VmHWM`, which is in kB whatever `getrusage` counts in.
        #[cfg(target_os = "linux")]
        {
            let _hold = vec![1u8; 64 << 20];
            let mut r: libc::rusage = unsafe { std::mem::zeroed() };
            unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut r) };
            let status = std::fs::read_to_string("/proc/self/status").unwrap();
            let hwm: u64 = status
                .lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
                .unwrap();
            let traced = maxrss_kib(r.ru_maxrss);
            assert!(
                traced.abs_diff(hwm) <= hwm / 10,
                "{traced} KiB, VmHWM {hwm} kB"
            );
            std::hint::black_box(&_hold);
        }
    }

    #[test]
    fn the_monotonic_clock_moves_forward_in_microseconds() {
        let a = now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now();
        assert!((1_500..1_000_000).contains(&(b - a)), "{a} {b}");
    }
}
