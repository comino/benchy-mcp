//! A real ClickHouse server for the tests that need one (REQ-15, REQ-16), started from the
//! official single binary named by `BENCHY_CLICKHOUSE_BIN` — once per test binary, on a free
//! port, with its data in a temporary directory, and killed with that directory removed when the
//! tests end. Without the variable a test that asks for it says it was skipped and passes: the
//! binary is 700 MB, and CI's `clickhouse` job downloads it, pinned by version and SHA-512.

use super::unique;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// What the test server's `reader` user signs in with. Distinctive, so a scan for it means
/// something.
pub const PASSWORD: &str = "benchy-ch-password-7f3a9c";

pub struct ClickHouse {
    pub url: String,
}

static CLICKHOUSE: OnceLock<Option<ClickHouse>> = OnceLock::new();
static PID: AtomicI32 = AtomicI32::new(0);
static DIR: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn stop_clickhouse() {
    let pid = PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
    }
    if let Some(d) = DIR.get() {
        let _ = std::fs::remove_dir_all(d);
    }
}

/// The shared server, started on first use. `None` when `BENCHY_CLICKHOUSE_BIN` is not set.
fn clickhouse() -> Option<&'static ClickHouse> {
    CLICKHOUSE
        .get_or_init(|| {
            let bin = std::env::var_os("BENCHY_CLICKHOUSE_BIN")?;
            Some(start(Path::new(&bin)))
        })
        .as_ref()
}

/// The shared server, or `None` with a notice that the test was skipped.
pub fn server() -> Option<&'static ClickHouse> {
    let ch = clickhouse();
    if ch.is_none() {
        eprintln!(
            "skipped: set BENCHY_CLICKHOUSE_BIN to a ClickHouse binary to run the ClickHouse \
             tests (CONTRIBUTING.md)"
        );
    }
    ch
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start(bin: &Path) -> ClickHouse {
    let dir = unique("clickhouse");
    std::fs::create_dir_all(&dir).unwrap();
    let _ = DIR.set(dir.clone());
    let port = free_port();
    let d = dir.display();
    // Only the HTTP port: a protocol with no port in the config is not served. A `reader` with a
    // password; a `ro` user whose profile forbids changing settings (`readonly = 1`), and two
    // more whose profiles also make Arrow output benchy does not read (zstd bodies, text as
    // binary); and the query log, which shows what benchy asked.
    let config = format!(
        "<clickhouse>
  <logger><level>error</level><console>0</console><log>{d}/server.log</log><errorlog>{d}/server.err</errorlog></logger>
  <listen_host>127.0.0.1</listen_host>
  <http_port>{port}</http_port>
  <path>{d}/data/</path>
  <tmp_path>{d}/tmp/</tmp_path>
  <user_files_path>{d}/user_files/</user_files_path>
  <mark_cache_size>67108864</mark_cache_size>
  <query_log><database>system</database><table>query_log</table><flush_interval_milliseconds>500</flush_interval_milliseconds></query_log>
  <users>
    <default><password></password><networks><ip>127.0.0.1</ip></networks><profile>default</profile><quota>default</quota></default>
    <reader><password>{PASSWORD}</password><networks><ip>127.0.0.1</ip></networks><profile>default</profile><quota>default</quota></reader>
    <ro><password></password><networks><ip>127.0.0.1</ip></networks><profile>ro</profile><quota>default</quota></ro>
    <ro_zstd><password></password><networks><ip>127.0.0.1</ip></networks><profile>ro_zstd</profile><quota>default</quota></ro_zstd>
    <ro_binary><password></password><networks><ip>127.0.0.1</ip></networks><profile>ro_binary</profile><quota>default</quota></ro_binary>
  </users>
  <profiles><default/><ro><readonly>1</readonly></ro>
    <ro_zstd><readonly>1</readonly><output_format_arrow_compression_method>zstd</output_format_arrow_compression_method></ro_zstd>
    <ro_binary><readonly>1</readonly><output_format_arrow_string_as_string>0</output_format_arrow_string_as_string></ro_binary>
  </profiles>
  <quotas><default/></quotas>
</clickhouse>
"
    );
    let cfg = dir.join("config.xml");
    std::fs::write(&cfg, config).unwrap();
    // Started from a thread that never ends: the parent-death signal follows the thread that
    // forked, and a test's thread ends with the test.
    let (tx, rx) = std::sync::mpsc::channel();
    let bin = bin.to_path_buf();
    std::thread::spawn(move || {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("server")
            .arg(format!("--config-file={}", cfg.display()))
            // The watchdog would fork, and the pid here would not be the server's.
            .env("CLICKHOUSE_WATCHDOG_ENABLE", "0")
            .current_dir(cfg.parent().unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("ClickHouse starts");
        tx.send(child.id() as i32).unwrap();
        // This thread lives as long as the server does, so the parent-death signal holds.
        let _ = child.wait();
    });
    let pid = rx.recv().unwrap();
    PID.store(pid, Ordering::SeqCst);
    unsafe { libc::atexit(stop_clickhouse) };
    let url = format!("http://127.0.0.1:{port}");
    let started = Instant::now();
    loop {
        let up = ureq::get(&format!("{url}/ping"))
            .call()
            .is_ok_and(|r| r.status().as_u16() == 200);
        if up {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "ClickHouse did not answer within 60s; see {d}/server.err"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    ClickHouse { url }
}

impl ClickHouse {
    /// Run `sql` as the default user and return the text it answers.
    pub fn sql(&self, sql: &str) -> String {
        let mut r = ureq::post(&format!("{}/?date_time_input_format=best_effort", self.url))
            .config()
            .http_status_as_error(false)
            .build()
            .send(sql)
            .unwrap();
        let status = r.status().as_u16();
        let text = r.body_mut().read_to_string().unwrap();
        assert_eq!(status, 200, "{sql}\n{text}");
        text
    }

    /// A database of the test's own, since the tests share one server.
    pub fn database(&self, name: &str) -> String {
        self.sql(&format!("DROP DATABASE IF EXISTS {name}"));
        self.sql(&format!("CREATE DATABASE {name}"));
        name.to_string()
    }

    /// The queries ClickHouse ran that mention `needle`, with what each read and returned.
    pub fn logged(&self, needle: &str) -> Vec<(String, u64, Vec<String>)> {
        self.sql("SYSTEM FLUSH LOGS");
        let text = self.sql(&format!(
            "SELECT query, result_rows, columns FROM system.query_log WHERE type = 'QueryFinish' \
             AND position(query, '{}') > 0 AND query NOT LIKE '%system.query_log%' FORMAT \
             JSONEachRow",
            needle.replace('\\', "\\\\").replace('\'', "\\'")
        ));
        text.lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                (
                    v["query"].as_str().unwrap().to_string(),
                    v["result_rows"]
                        .as_str()
                        .map(|s| s.parse().unwrap())
                        .or(v["result_rows"].as_u64())
                        .unwrap(),
                    v["columns"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| c.as_str().unwrap().to_string())
                        .collect(),
                )
            })
            .collect()
    }
}
