//! REQ-7 `the_engines_threads_are_started_after_it_is_confined`: an engine given more than one
//! thread runs agent SQL on DuckDB's workers, and every one of them is confined.
//!
//! Landlock confines the thread that asks and the threads it starts afterwards, never one already
//! running, and DuckDB starts its workers when it opens. So the engine sends them away, confines
//! itself alone, and only then asks for its threads (`benchy_mcp::engine::seal`). This seals as
//! the engine does, in a process that is alone as the engine is — no test harness, whose threads
//! would make `sandbox::alone` refuse — and puts a function on the bench that DuckDB runs on
//! whichever thread scans the rows: it tries to open a file and a TCP connection outside the
//! bench, from that thread, past DuckDB's own lock. Every thread that ran it must be refused both.
//!
//! The test harness runs each test on a thread of its own, so the confined half runs in a process
//! of its own: this binary again, which goes no further than [`linux::BEFORE_MAIN`], before the
//! harness has started a thread. That also leaves the directory it worked in to a process that is
//! not confined, which can remove it.

#[cfg(target_os = "linux")]
mod linux {
    use libduckdb_sys as ffi;
    use std::ffi::CString;
    use std::sync::OnceLock;

    // What a thread could do, found once per thread: its id, and whether it could open the file
    // and connect to the port, as `tid << 2 | file << 1 | tcp`.
    thread_local! {
        static FOUND: i64 = attempt();
    }

    static TARGET: OnceLock<(CString, std::net::SocketAddr)> = OnceLock::new();

    fn attempt() -> i64 {
        let (file, addr) = TARGET.get().expect("set before the query");
        let tid = unsafe { libc::syscall(libc::SYS_gettid) };
        let fd = unsafe { libc::open(file.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        let file = fd >= 0;
        if file {
            unsafe { libc::close(fd) };
        }
        let tcp =
            std::net::TcpStream::connect_timeout(addr, std::time::Duration::from_secs(2)).is_ok();
        (tid << 2) | (i64::from(file) << 1) | i64::from(tcp)
    }

    unsafe extern "C" fn probe(
        _: ffi::duckdb_function_info,
        input: ffi::duckdb_data_chunk,
        output: ffi::duckdb_vector,
    ) {
        let n = ffi::duckdb_data_chunk_get_size(input) as usize;
        let out = ffi::duckdb_vector_get_data(output) as *mut i64;
        let found = FOUND.with(|f| *f);
        for i in 0..n {
            *out.add(i) = found;
        }
    }

    fn tasks() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }

    /// One query on the agent's connection, its first column read as BIGINT.
    fn column(conn: ffi::duckdb_connection, sql: &str) -> Vec<i64> {
        unsafe {
            let sql = CString::new(sql).unwrap();
            let mut r: ffi::duckdb_result = std::mem::zeroed();
            let ok =
                ffi::duckdb_query(conn, sql.as_ptr(), &mut r) == ffi::duckdb_state_DuckDBSuccess;
            if !ok {
                let e = std::ffi::CStr::from_ptr(ffi::duckdb_result_error(&mut r));
                panic!("{sql:?}: {}", e.to_string_lossy());
            }
            let rows = (0..ffi::duckdb_row_count(&mut r))
                .map(|row| ffi::duckdb_value_int64(&mut r, 0, row))
                .collect();
            ffi::duckdb_destroy_result(&mut r);
            rows
        }
    }

    /// Set in the process [`run`] starts, to the directory it works in.
    const ROOT: &str = "BENCHY_TEST_THREADS_ROOT";

    /// Run as the binary loads, before `main`: in the process [`run`] starts, the confined half,
    /// alone, and then exit. Anywhere else, nothing.
    #[used]
    #[link_section = ".init_array"]
    pub static BEFORE_MAIN: extern "C" fn() = before_main;

    extern "C" fn before_main() {
        if let Some(root) = std::env::var_os(ROOT) {
            let ok = std::panic::catch_unwind(|| confined(std::path::Path::new(&root))).is_ok();
            std::process::exit(if ok { 0 } else { 1 });
        }
    }

    pub fn run() {
        let root = std::env::temp_dir().join(format!("benchy-threads-{}", std::process::id()));
        std::fs::create_dir_all(root.join("ws")).unwrap();
        std::fs::write(root.join("outside.txt"), "not the bench's").unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .env(ROOT, &root)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&root);
        let said = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        print!("{said}");
        assert!(out.status.success() && said.contains("... ok"), "failed");
    }

    fn confined(root: &std::path::Path) {
        assert_eq!(
            tasks(),
            1,
            "this process must start alone, as the engine does"
        );
        let workspace = root.join("ws");
        let outside = root.join("outside.txt");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        TARGET
            .set((
                CString::new(outside.to_str().unwrap()).unwrap(),
                listener.local_addr().unwrap(),
            ))
            .unwrap();
        // Unconfined, a thread can do both, or the refusals below would prove nothing.
        let before = std::thread::spawn(attempt).join().unwrap();
        assert_eq!(
            before & 3,
            3,
            "unconfined, the file and the port are reachable"
        );

        let (ws, posture) = benchy_mcp::engine::seal(&workspace, "1GB", 4, false, || {})
            .unwrap_or_else(|e| panic!("the engine's seal: {e}"));
        assert!(posture.is_enforced(), "{posture:?}");
        let tcp_confined = benchy_mcp::sandbox::landlock_abi() >= benchy_mcp::sandbox::NETWORK_ABI;

        let conn = ws.agent_connection();
        unsafe {
            let f = ffi::duckdb_create_scalar_function();
            let name = CString::new("probe").unwrap();
            ffi::duckdb_scalar_function_set_name(f, name.as_ptr());
            let mut bigint = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT);
            ffi::duckdb_scalar_function_add_parameter(f, bigint);
            ffi::duckdb_scalar_function_set_return_type(f, bigint);
            ffi::duckdb_scalar_function_set_function(f, Some(probe));
            let registered = ffi::duckdb_register_scalar_function(conn, f);
            ffi::duckdb_destroy_logical_type(&mut bigint);
            let mut f = f;
            ffi::duckdb_destroy_scalar_function(&mut f);
            assert_eq!(
                registered,
                ffi::duckdb_state_DuckDBSuccess,
                "the probe goes on"
            );
        }
        // Enough row groups (122,880 rows each) for every worker to take some.
        column(
            conn,
            "CREATE TABLE t AS SELECT range AS i FROM range(8000000)",
        );
        let found = column(conn, "SELECT DISTINCT probe(i) FROM t");
        let main = unsafe { libc::syscall(libc::SYS_gettid) };
        let workers: Vec<i64> = found.iter().filter(|f| *f >> 2 != main).copied().collect();
        assert!(
            !workers.is_empty(),
            "the scan ran on DuckDB's workers, not on this thread alone: {found:?}"
        );
        // `/proc` is outside the bench too, so the threads are counted by what ran the probe.
        assert!(
            found.len() <= 4,
            "no more than the four threads asked for: {found:?}"
        );
        for f in &found {
            let who = if f >> 2 == main {
                "the engine's own thread"
            } else {
                "a worker"
            };
            assert_eq!(
                f & 2,
                0,
                "{who} ({}) opened a file outside the bench",
                f >> 2
            );
            if tcp_confined {
                assert_eq!(f & 1, 0, "{who} ({}) opened a TCP connection", f >> 2);
            }
        }
        println!(
            "the_engines_threads_are_started_after_it_is_confined ... ok ({} threads ran agent SQL, \
             {} of them workers, every one confined)",
            found.len(),
            workers.len()
        );
        drop(ws);
    }
}

/// DuckDB 1.5.5 starts its workers when it opens (three beside the opening thread on four
/// cores) and again when `threads` is set. On four threads the probe ran on four, three of them
/// workers, and every one was refused both the file and the connection. Mutation-checked: with
/// `threads` set before the confinement the engine refuses to serve (`4 threads were running`),
/// and with the check that the engine is alone also removed, a worker opened the file.
#[test]
#[cfg(target_os = "linux")]
fn the_engines_threads_are_started_after_it_is_confined() {
    linux::run();
}
