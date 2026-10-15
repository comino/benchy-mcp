//! The engine process: one bench, one loop, no MCP.
//!
//! It exists so the server can **kill it**. REQ-8: an in-process interrupt cannot stop a query
//! that is still being *planned* — measured at 7.8 s against a 2 s interrupt, and 402 MB against
//! a 512 MB limit — because planning never reaches the point where the interrupt flag is
//! checked. The only thing that bounds that is a process boundary and SIGKILL.

use crate::proto::{read_frame, write_frame, Reply, Request};
use crate::{Caps, Workspace};
use std::io::{stdin, stdout};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Every exit goes through here. `process::exit` runs no destructors, and the workspace's `Drop`
/// is what deletes the loaded data from disk — so the bench is dropped first, explicitly.
fn quit(bench: Option<Workspace>) -> ! {
    drop(bench);
    std::process::exit(0)
}

/// What this engine must delete if its server dies: the session's directory.
static HOLDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn holding(path: &Path) {
    if let Ok(mut h) = HOLDING.lock() {
        h.push(path.to_path_buf());
    }
}

/// A killed server cleans up nothing, so the engine watches for it. When a process's parent
/// dies it is re-parented, and `getppid` changes; then everything the session keeps — the
/// store, the engine's workspace, a loader's staging — is the engine's to remove before it goes.
/// A running connector is its loader's to stop: the engine runs none.
///
/// There are two watches, one either side of the seal. Landlock confines the thread that asks
/// and the threads it starts afterwards, never a thread already running — so the watch that
/// covers loading must be stopped before the seal, or it would stay unconfined for the life of
/// the process, and the one that covers serving is started by the confined thread.
///
/// The watch sleeps on a condvar rather than in `sleep`, so stopping it is immediate. It slept
/// 100 ms at a time, and the seal joins it before confining: every engine's `Open` waited out the
/// rest of that sleep, 65–100 ms of every cold start (docs/PERFORMANCE.md, item 6), and of every
/// rebuild. `stopping_the_watch_does_not_wait_out_its_poll` is the regression test.
struct Watch {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: std::thread::JoinHandle<()>,
}

/// How often the watch looks for its server. Only how soon a killed server's session is removed.
const WATCH_EVERY: Duration = Duration::from_millis(100);

impl Watch {
    fn start(server: libc::pid_t) -> Watch {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let (lock, cv) = &*flag;
            let mut stopped = lock.lock().unwrap_or_else(|e| e.into_inner());
            while !*stopped {
                stopped = cv
                    .wait_timeout(stopped, WATCH_EVERY)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                if !*stopped && unsafe { libc::getppid() } != server {
                    if let Ok(h) = HOLDING.lock() {
                        // Confined, the engine may delete inside the session but not the session
                        // directory itself, so that one fails, last, and an empty directory stays.
                        for p in h.iter() {
                            let _ = std::fs::remove_dir_all(p);
                        }
                    }
                    unsafe { libc::_exit(0) };
                }
            }
        });
        Watch { stop, thread }
    }

    fn stop(self) {
        let (lock, cv) = &*self.stop;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
        let _ = self.thread.join();
    }
}

/// The pipe closed. A killed server closes it moments before the kernel re-parents us, so give
/// `watch_server` that moment to notice; if the server is still there, it only hung up.
fn hung_up(bench: Option<Workspace>) -> ! {
    std::thread::sleep(Duration::from_millis(500));
    quit(bench)
}

fn caps(max_rows: usize, max_bytes: usize, max_cell_bytes: usize, deadline_ms: u64) -> Caps {
    Caps {
        max_rows,
        max_bytes,
        max_cell_bytes,
        deadline: Duration::from_millis(deadline_ms),
        memory_limit: String::new(), // already applied at seal
        hard_grace: Duration::ZERO,  // the server owns the hard deadline
    }
}

/// Runs until stdin closes. Never returns a `Result`: a broken pipe means the server is gone.
pub fn run() -> ! {
    let server = unsafe { libc::getppid() };
    let mut watch = Some(Watch::start(server));
    let mut input = stdin().lock();
    let mut output = stdout().lock();
    let mut bench: Option<Workspace> = None;

    loop {
        let req: Request = match read_frame(&mut input) {
            Ok(r) => r,
            Err(_) => hung_up(bench.take()),
        };
        // bench/ingest.py: what each request cost the engine (src/trace.rs).
        let traced = crate::trace::on().then(|| {
            let what = match &req {
                Request::Open { .. } => "engine.open",
                Request::Attach { .. } => "engine.attach",
                Request::Describe { .. } => "engine.describe",
                _ => "engine.request",
            };
            (what, crate::trace::now())
        });
        // The names this request's texts give the agent's tools: a host's, sent with each request
        // that makes text, so a rebuilt engine needs telling nothing.
        match &req {
            Request::Query { tools, .. }
            | Request::Page { tools, .. }
            | Request::Describe { tools, .. } => crate::tools::set(tools),
            _ => crate::tools::set(&crate::tools::Names::new()),
        }
        let reply = match req {
            Request::Open {
                workspace,
                memory_limit,
                allow_unsandboxed,
                first_result_id,
                result_prefix,
            } => {
                let dir = Path::new(&workspace);
                // Everything the session keeps is in its directory — the store, this engine's
                // workspace, a loader's staging — so that is what goes if the server dies.
                holding(dir.parent().unwrap_or(dir));
                match crate::trace::span("open_sealed", || {
                    Workspace::open_sealed(dir, &memory_limit)
                })
                .map_err(|e| e.to_string())
                .and_then(|ws| {
                    if let Some(w) = watch.take() {
                        w.stop();
                    }
                    let posture =
                        crate::trace::span("confine", || confine(&ws, dir, allow_unsandboxed))?;
                    watch = Some(Watch::start(server));
                    Ok((ws, posture))
                }) {
                    Ok((mut ws, posture)) => {
                        ws.set_first_result_id(first_result_id);
                        if !result_prefix.is_empty() {
                            ws.set_result_prefix(&result_prefix);
                        }
                        // The version comes from the engine, so the server never hardcodes one
                        // (REQ-11 `version_not_hardcoded`).
                        let version = ws.engine_version();
                        bench = Some(ws);
                        Reply {
                            posture: Some(posture.describe()),
                            ..Reply::ok(version)
                        }
                    }
                    Err(e) => Reply::err(e),
                }
            }
            Request::Attach { tables } => match &mut bench {
                Some(ws) => {
                    let tables: Vec<(String, PathBuf)> = tables
                        .into_iter()
                        .map(|(name, file)| (name, PathBuf::from(file)))
                        .collect();
                    let each = ws
                        .attach_all(&tables)
                        .into_iter()
                        .map(|r| match r {
                            Ok(t) => Reply {
                                tables: vec![(t.name, t.rows)],
                                columns: t.columns,
                                ..Reply::ok("")
                            },
                            Err(e @ crate::workspace::WorkError::Taken(_)) => {
                                Reply::fail("E_NAME", e.to_string())
                            }
                            Err(e) => Reply::fail("E_DATA", e.to_string()),
                        })
                        .collect();
                    Reply {
                        each,
                        ..Reply::ok("")
                    }
                }
                None => Reply::err("the bench has not been opened"),
            },
            Request::Detach { name } => match &mut bench {
                Some(ws) => match ws.detach(&name) {
                    Ok(()) => Reply::ok(""),
                    Err(e) => Reply::fail("E_NAME", e.to_string()),
                },
                None => Reply::err("the bench has not been opened"),
            },
            Request::Helpers {
                file,
                shown,
                reads,
                macro_reads,
            } => match &mut bench {
                Some(ws) => match ws.put_helpers(Path::new(&file), shown, &reads, &macro_reads) {
                    Ok(()) => Reply::ok(""),
                    Err(e) => Reply::fail("E_DATA", e.to_string()),
                },
                None => Reply::err("the bench has not been opened"),
            },
            Request::Describe {
                budget,
                unavailable,
                ..
            } => match &bench {
                Some(ws) => Reply::ok(ws.describe_with(budget, &unavailable)),
                None => Reply::err("the bench has not been loaded"),
            },
            Request::Query {
                sql,
                max_rows,
                max_bytes,
                max_cell_bytes,
                deadline_ms,
                unavailable,
                ..
            } => match &mut bench {
                Some(ws) => {
                    let caps = caps(max_rows, max_bytes, max_cell_bytes, deadline_ms);
                    ws.set_unavailable(unavailable);
                    let reply = match ws.work(&sql, &caps) {
                        Ok(results) => {
                            let (text, shown) = crate::result::render_batch_shown(&results, &caps);
                            Reply {
                                result_ids: results.iter().map(|r| r.result_id.clone()).collect(),
                                results: shown,
                                ..Reply::ok(text)
                            }
                        }
                        // REQ-10 `a_failed_batch_returns_what_ran_before_it`: the error first, so
                        // no result can push it past the cap, then what ran, in what is left.
                        Err(e @ crate::workspace::WorkError::Partial { .. }) => {
                            let crate::workspace::WorkError::Partial { ran, error } = &e else {
                                unreachable!()
                            };
                            let code = match **error {
                                crate::workspace::WorkError::Deadline { .. } => "E_DEADLINE",
                                _ => "E_SQL",
                            };
                            let head = crate::result::fit(&e.to_string(), max_bytes / 2);
                            let left = max_bytes.saturating_sub(
                                crate::result::wire_len(&head) + crate::result::wire_len("\n\n"),
                            );
                            let rest = crate::result::render_batch(
                                ran,
                                &Caps {
                                    max_bytes: left,
                                    ..caps.clone()
                                },
                            );
                            Reply {
                                result_ids: ran.iter().map(|r| r.result_id.clone()).collect(),
                                ..Reply::fail(code, format!("{head}\n\n{rest}"))
                            }
                        }
                        // An error is part of the response too, and the cap is a promise about
                        // the response.
                        Err(e) => Reply::fail(
                            match e {
                                crate::workspace::WorkError::Deadline { .. } => "E_DEADLINE",
                                _ => "E_SQL",
                            },
                            crate::result::fit(&e.to_string(), max_bytes),
                        ),
                    };
                    // `a_pivot_type_that_cannot_be_dropped_spoils_the_engine`
                    Reply {
                        rebuild: ws.spoiled().map(str::to_string),
                        ..reply
                    }
                }
                None => Reply::err("the bench has not been loaded"),
            },
            Request::Page {
                result_id,
                offset,
                max_rows,
                max_bytes,
                max_cell_bytes,
                ..
            } => match &bench {
                Some(ws) => match ws.page(
                    &result_id,
                    offset,
                    &caps(max_rows, max_bytes, max_cell_bytes, 0),
                ) {
                    // The SQL is bounded like the text: a host may show it to the model.
                    Some(text) => Reply {
                        sql: ws
                            .sql_of(&result_id)
                            .map(|s| crate::result::fit(s, max_bytes)),
                        ..Reply::ok(text)
                    },
                    None => Reply::fail(
                        "E_NO_RESULT",
                        format!(
                        "no result {result_id} here. The last {} results are kept, and a rebuilt \
                         bench keeps none from before it — run the query again.",
                            crate::workspace::RETAINED_RESULTS
                        ),
                    ),
                },
                None => Reply::err("the bench has not been loaded"),
            },
        };
        if let Some((what, t0)) = traced {
            crate::trace::since(what, t0);
            crate::trace::usage("engine.usage");
        }
        if write_frame(&mut output, &reply).is_err() {
            hung_up(bench.take());
        }
    }
}

/// REQ-3, in the process that actually runs the SQL. The workspace sits in the server's session
/// directory, which is where this engine may still delete.
///
/// Landlock confines the calling thread and the threads it starts afterwards, never a thread
/// already running, so this must be the only thread. The seal stopped DuckDB's workers
/// (`threads = 1`) and the caller stopped the watch. The engine loads nothing and runs no
/// connector, so nothing else should be running; if something is, it would be an unconfined
/// thread in the process that runs agent SQL, and the engine refuses to serve rather than pretend.
fn confine(
    ws: &Workspace,
    workspace: &Path,
    allow_unsandboxed: bool,
) -> Result<crate::sandbox::Posture, String> {
    crate::sandbox::alone(Duration::from_secs(2))
        .map_err(|e| format!("the engine cannot be confined: {e}"))?;
    let session = workspace.parent().unwrap_or(workspace);
    let posture = crate::sandbox::restrict_to(ws.dir(), session);
    crate::sandbox::admit(&posture, allow_unsandboxed, "engine")?;
    Ok(posture)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seal joins the watch before it confines, so a stop that waits out the watch's poll is
    /// paid by every engine's `Open`. Started, given time to settle into its wait, and stopped:
    /// sleeping in 100 ms steps, the stop took 90 ms or more; woken, it takes microseconds.
    #[test]
    fn stopping_the_watch_does_not_wait_out_its_poll() {
        let watch = Watch::start(unsafe { libc::getppid() });
        std::thread::sleep(Duration::from_millis(10));
        let t = std::time::Instant::now();
        watch.stop();
        let took = t.elapsed();
        assert!(took < Duration::from_millis(50), "stopping took {took:?}");
    }
}
