//! `benchy-mcp` — an agent workbench for time-series analytics, served over MCP on stdio.

use benchy_mcp::supervisor::{Source, Supervisor};
use benchy_mcp::{engine, mcp, trace, Caps};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
benchy-mcp — an agent workbench for time-series analytics over the data you already have.
It loads files, ClickHouse tables, URLs or a connector's output before the agent's first call; the
agent explores them with SQL and gets small answers back.

USAGE
  benchy-mcp <dir>                    every .csv/.tsv/.parquet/.json/.ndjson/.jsonl
                                      at the top of <dir>, but no dotfile, becomes a
                                      table named after the file; what is skipped is
                                      logged on stderr
  benchy-mcp --source name=path ...   name files explicitly
  benchy-mcp --config benchy.toml     ClickHouse tables, URLs, exec connectors, per-source limits,
                                      narrowing, session budget, SQL helpers ([helpers]: views and
                                      macros the agent calls like built-ins; with <dir> too)
  benchy-mcp host                     serve the host protocol on stdio instead: the host
                                      attaches data (docs/HOST-PROTOCOL.md)
  benchy-mcp doctor [--probe] [--json]  report what this machine can enforce, and which
                                      DuckDB library this binary loaded; --json probes, and
                                      prints one JSON object for a host to check
  benchy-mcp check <cfg> <name>       check one configured source without a model: for
                                      ClickHouse reach, sign in, schema, estimate, one row;
                                      for HTTP the request, the first page, the schema to pin
  benchy-mcp exec-test <cfg> <name>   check one connector against the contract

OPTIONS
  --max-rows N        rows per result page                       (default 200)
  --max-bytes N       bytes per response, as the client gets it  (default 8192, at least 512)
  --max-cell-bytes N  bytes of one value before it is cut        (default 256)
  --deadline N        seconds per query batch                    (default 20)
  --hard-grace N      seconds past the deadline before the engine is killed (default 10)
  --memory SIZE       DuckDB's memory budget in the engine       (default 1GB)
  --memory-ceiling SIZE  the most resident memory the engine may hold, planning included;
                      past it the engine is killed and rebuilt (default --memory and half
                      again, and at least 256MiB over it); or [engine] memory_ceiling in a
                      config. A host may lower it for its session, never raise it.
                      Linux only: elsewhere it is not enforced, and not reported.
  --loader-memory-ceiling SIZE  the most resident memory one loader may hold, with the
                      connector it runs and what that starts; past it the load fails and
                      attaches nothing (default 1GB); or [loader] memory_ceiling in a config.
                      A host may lower it for its session (protocol 6), never raise it.
                      Linux only, as --memory-ceiling.
  --load-timeout N    seconds one source may take to load       (default 300)
  --attach-rows N     with `host`: rows one attachment may hold  (default 10000000)
  --attach-bytes N    with `host`: bytes one attachment may be   (default 268435456)
  --session-rows N    with `host`: rows a session may attach     (default 50000000)
  --session-bytes N   with `host`: bytes a session may attach    (default 2147483648)
                      A host may lower each for its session, never raise it. `host --config`
                      takes a config's [attach] and [budget] tables instead, and no sources;
                      its [engine] and [helpers] too, the helpers made before the first attach
  --stream-channel FD with `host`: descriptor FD, which the host passes as one more inherited
                      pipe, is the session's stream channel: the host writes Arrow streams
                      into it back to back, and each attach with `stream: channel` takes
                      the next one. No descriptor passing, so a host on Node.js or Java can
                      stream. Off unless named: an inherited descriptor is not a channel.
  --allow-unsandboxed serve even where Landlock cannot confine the engine
  --require-full-sandbox  refuse to start unless Landlock confines the engine fully,
                      TCP included (Linux 6.7 or later); wins over --allow-unsandboxed
  --require-memory-ceilings  refuse to start unless the engine's and the loaders'
                      memory ceilings are enforced (Linux)
  --probe             with `doctor`: measure containment instead of reporting it
  --json              with `doctor`: measure it, and say so as JSON: a verdict and each mechanism
  -h, --help          this
  -V, --version       benchy's version and the DuckDB library's

Serves MCP on stdio: an MCP client starts it, not you. Name the binary by its full path:
built from source, it is on no PATH, and desktop apps do not read your shell's PATH anyway.
For example
  claude mcp add benchy -- /abs/path/to/benchy-mcp /abs/path/to/data
or, in a client's JSON config:
  {\"mcpServers\":{\"benchy\":{\"command\":\"/abs/path/to/benchy-mcp\",\"args\":[\"/abs/path/to/data\"]}}}
";

/// Everything a session keeps on disk — the store, each engine's workspace, a loader's staging —
/// lives in one directory, so however the session ends, ending it is one removal.
struct Session(PathBuf);

impl Session {
    fn create() -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt;
        let p = std::env::temp_dir().join(benchy_mcp::workspace::fresh_name("benchy"));
        // Fails if the name exists: a directory made by someone else could hold planted files.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&p)
            .map_err(|e| format!("could not create {}: {e}", p.display()))?;
        Ok(Session(p))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// SIGTERM, SIGINT and SIGHUP are how a client or a terminal stops a server, and none of them
/// runs a destructor. So they are taken off every thread and waited for on one: kill the engine,
/// remove the session, exit. Must run before any other thread or child is started, but for the
/// warden, forked before it, which unblocks its own.
fn clean_up_on_signals(session: PathBuf) {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::spawn(move || {
            let mut sig = 0;
            libc::sigwait(&set, &mut sig);
            benchy_mcp::supervisor::kill_engine_now();
            let _ = std::fs::remove_dir_all(&session);
            libc::_exit(128 + sig);
        });
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Stage timings for bench/ingest.py, on the operator's log only (src/trace.rs). The server
    // reads the variable; its engine and loaders are told by an argument, having no environment.
    if args.iter().any(|a| a == trace::FLAG) {
        trace::enable(if args.iter().any(|a| a == "--engine") {
            "engine"
        } else {
            "loader"
        });
    } else if std::env::var_os(trace::ENV).is_some_and(|v| v == "1") {
        trace::enable("server");
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("benchy-mcp {}", env!("CARGO_PKG_VERSION"));
        println!("DuckDB {}", duckdb_library().0);
        return ExitCode::SUCCESS;
    }
    // The engine is a separate process so the server can kill it when a query wedges the
    // planner, which no in-process timer can reach (REQ-8). Not a user-facing mode.
    if args.iter().any(|a| a == "--engine") {
        engine::run();
    }
    // A loader turns one source into one file for the engine to attach. Not a user-facing mode.
    if args.first().map(|s| s.as_str()) == Some("--loader") {
        benchy_mcp::loader::run();
    }
    // `doctor --probe` applies the engine's restriction in a child of its own, because the
    // restriction is irreversible. Not a user-facing mode either.
    if args.first().map(|s| s.as_str()) == Some("--probe-child") && args.len() == 3 {
        return probe_child(Path::new(&args[1]), Path::new(&args[2]));
    }
    if args.first().map(|s| s.as_str()) == Some("--probe-loader") && args.len() == 4 {
        return probe_loader_child(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
        );
    }
    if args.first().map(|s| s.as_str()) == Some("--probe-network-loader") && args.len() == 5 {
        let port = |a: &str| a.parse::<u16>().unwrap_or(0);
        return probe_network_loader_child(
            Path::new(&args[1]),
            Path::new(&args[2]),
            port(&args[3]),
            port(&args[4]),
        );
    }
    if args.first().map(|s| s.as_str()) == Some("doctor") {
        let json = args.iter().any(|a| a == "--json");
        return doctor(json || args.iter().any(|a| a == "--probe"), json);
    }
    if args.first().map(|s| s.as_str()) == Some("exec-test") {
        return exec_test(&args[1..]);
    }
    if args.first().map(|s| s.as_str()) == Some("check") {
        return check(&args[1..]);
    }

    let host = args.first().map(|s| s.as_str()) == Some("host");
    let Options {
        dir,
        config,
        named,
        caps,
        ceiling,
        loader_ceiling,
        load_timeout,
        allow_unsandboxed,
        require_full_sandbox,
        require_memory_ceilings,
        limits,
        stream_channel,
    } = match parse_options(if host { &args[1..] } else { &args }) {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    // Before anything starts: the platform decides it, and nothing a session does changes it.
    if require_memory_ceilings {
        if let Err(e) = ceilings_enforced(
            benchy_mcp::supervisor::MEMORY_CEILING_UNENFORCED,
            benchy_mcp::supervisor::LOADER_MEMORY_CEILING_UNENFORCED,
        ) {
            return fail(&e);
        }
    }
    if host {
        if dir.is_some() || !named.is_empty() {
            return fail("`host` takes no sources: the host attaches them");
        }
        // Taken before this process opens anything, so the number is still the one the host
        // passed and not a file of benchy's own that happened to get it.
        let channel = match stream_channel.map(take_channel).transpose() {
            Ok(c) => c,
            Err(e) => return fail(&e),
        };
        let (limits, helpers) = match host_limits(
            limits,
            &caps.memory_limit,
            ceiling,
            loader_ceiling,
            config.as_deref(),
        ) {
            Ok(l) => l,
            Err(e) => return fail(&e),
        };
        return serve_host(
            caps,
            load_timeout,
            allow_unsandboxed,
            require_full_sandbox,
            limits,
            helpers,
            channel,
        );
    }
    if stream_channel.is_some() {
        return fail("--stream-channel is for `host`: a host writes the streams into it");
    }
    if limits.iter().any(Option::is_some) {
        return fail(
            "--attach-rows, --attach-bytes, --session-rows and --session-bytes are what a host \
             may attach, and are for `host`; a server's sources are bounded in its config",
        );
    }
    if dir.is_none() && named.is_empty() && config.is_none() {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    }
    // The session, and its warden forked from this process, before the config is read: a
    // fork is a copy of this process, and the warden must hold none of what the config names.
    let session = match Session::create() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    if let Err(e) = trace::span("warden.spawn", || {
        benchy_mcp::supervisor::start_warden(&session.0)
    }) {
        return fail(&e);
    }
    clean_up_on_signals(session.0.clone());
    // Fail here, not in a subprocess: an operator typo in the config should be one clear line on
    // the terminal they are looking at. Read once: what the session loads is decided now.
    let sources = match &config {
        Some(c) => match std::fs::read_to_string(c) {
            Ok(text) => match benchy_mcp::config::parse(&text, c) {
                Ok(s) => Some(s),
                Err(e) => return fail(&e),
            },
            Err(e) => return fail(&format!("{}: {e}", c.display())),
        },
        None => None,
    };
    let default_budget = benchy_mcp::admission::Budget::default();
    let b = sources.as_ref().map_or(&default_budget, |s| &s.budget);
    let from_config = sources.as_ref().and_then(|s| s.memory_ceiling.clone());
    let memory = match memory(&caps.memory_limit, ceiling, from_config, config.as_deref()) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let loader_in_config = sources
        .as_ref()
        .and_then(|s| s.loader_memory_ceiling.clone());
    let loader_memory_ceiling =
        match loader_memory(loader_ceiling, loader_in_config, config.as_deref()) {
            Ok(c) => c,
            Err(e) => return fail(&e),
        };
    let limits = benchy_mcp::admission::HostLimits {
        session_rows: b.max_rows,
        session_bytes: b.max_bytes,
        memory: memory.budget,
        memory_ceiling: memory.ceiling,
        loader_memory_ceiling,
        ..Default::default()
    };
    let mut plan = Vec::new();
    if let Some(d) = &dir {
        match benchy_mcp::source::plan_dir(d) {
            Ok(files) => plan.extend(files.into_iter().map(|(name, path)| Source::File {
                name,
                path,
                timezone: None,
                declared: Default::default(),
            })),
            Err(e) => return fail(&e.to_string()),
        }
    }
    for (name, path) in &named {
        plan.push(Source::File {
            name: name.clone(),
            path: path.clone(),
            timezone: None,
            declared: Default::default(),
        });
    }
    let mut shapes = Default::default();
    let mut helpers = Vec::new();
    if let Some(s) = sources {
        shapes = s.shapes;
        helpers = s.helpers;
        plan.extend(s.files.into_iter().map(|f| Source::File {
            name: f.name,
            path: f.path,
            timezone: f.timezone,
            declared: f.declared,
        }));
        plan.extend(s.exec.into_iter().map(|e| Source::Exec(Box::new(e))));
        plan.extend(s.builtin.into_iter().map(|b| Source::BuiltIn(Box::new(b))));
    }
    if plan.is_empty() {
        return fail("nothing to load");
    }

    let mut supervisor = match trace::span("engine.start", || {
        Supervisor::start(benchy_mcp::supervisor::EngineSpec {
            session: session.0.clone(),
            memory_limit: caps.memory_limit.clone(),
            load_timeout,
            allow_unsandboxed,
            limits,
        })
    }) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    supervisor.state_shapes(shapes);
    // REQ-3 `a_session_that_must_be_fully_sandboxed_refuses_to_start_otherwise`: the engine
    // opens while the sources load, but a network loader would reach its source under the posture
    // about to be refused, so with the flag the engine's posture is waited for first. It is the
    // engine's, measured; a loader's ruleset handles the same rights on the same kernel.
    if require_full_sandbox {
        let checked = supervisor.opened().and_then(|o| {
            benchy_mcp::sandbox::require_full(&o.posture, benchy_mcp::sandbox::landlock_abi())
        });
        if let Err(e) = checked {
            return fail(&e);
        }
    }
    // Each source through a loader of its own; the engine attaches what they wrote, all in one
    // request. One that fails does not cost the others: the session starts with what loaded, the
    // operator's log has the whole reason, and the agent is told only the table's name. With
    // nothing loaded there is nothing to serve, and the server exits non-zero.
    //
    // The engine opens while the sources load (`Supervisor::start`). An engine that cannot open —
    // Landlock missing without --allow-unsandboxed, say — stops startup with the reason once the
    // loads are done, and nothing is attached to it.
    let planned = plan.len();
    let loaded = supervisor.load_all(plan);
    let opened = match supervisor.opened() {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    let mut unavailable = Vec::new();
    for (name, loaded) in loaded {
        match loaded {
            Ok(t) => eprintln!("benchy: {} ({} rows)", t.name, t.rows),
            Err(e) => {
                eprintln!("benchy: `{name}` was not loaded: {}", e.message);
                supervisor.mark_unavailable(&name);
                unavailable.push(name);
            }
        }
    }
    if supervisor.attached().is_empty() {
        return fail(&format!(
            "none of the {planned} source(s) loaded, so there is nothing to serve; each reason \
             is above"
        ));
    }
    // REQ-18: the helpers, made against the tables that loaded, before the agent's first call.
    // A helper that cannot be made stops startup, as a config mistake does; one that reads a
    // table no source provides is a mistake too. One that reads a table that failed to load waits
    // for it, as the table is listed as unavailable.
    if !helpers.is_empty() {
        supervisor.set_helpers(helpers);
        let built = match trace::span("helpers", || supervisor.build_helpers()) {
            Ok(b) => b,
            Err(e) => return fail(&format!("the helpers could not be made: {e}")),
        };
        if !built.broken.is_empty() {
            return fail(&built.broken.join("\nbenchy: "));
        }
        for (name, table) in built.waiting() {
            if !unavailable.iter().any(|u| u.eq_ignore_ascii_case(table)) {
                return fail(&format!(
                    "helper `{name}` reads `{table}`, which is no table of this session's. Name a \
                     table a source provides, unqualified."
                ));
            }
            eprintln!("benchy: helper `{name}` waits for `{table}`, which did not load");
        }
        eprintln!(
            "benchy: {} helper(s): {}",
            built.shown.len(),
            built
                .shown
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    eprintln!(
        "benchy: sealed in a separate engine process — SQL cannot reach the filesystem or the \
         network, and a query that wedges the planner costs the engine, not the server"
    );
    eprintln!("benchy: Landlock {}", opened.posture.describe());
    say_what_the_network_is(&opened.posture);
    if !opened.posture.is_enforced() {
        eprintln!(
            "benchy: running without an OS sandbox, as --allow-unsandboxed asked: the SQL layer \
             is the only boundary"
        );
    }
    // Started by hand, the server looks hung: it reads MCP frames from stdin and prints nothing
    // more. A client never gives it a terminal, so this line reaches only a person.
    if unsafe { libc::isatty(0) } == 1 {
        eprintln!(
            "benchy: serving MCP on stdio, waiting for an MCP client. This is not an interactive \
             prompt: let your client start benchy (see `benchy-mcp --help`). Ctrl-C ends it."
        );
    }

    trace::usage("server.ready");
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => return fail(&e.to_string()),
    };
    rt.block_on(async move {
        match mcp::Bench::new(supervisor, caps, opened.engine_version)
            .serve(stdio())
            .await
        {
            Ok(service) => {
                let _ = service.waiting().await;
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("MCP transport: {e}")),
        }
    })
}

/// What the command line asked for.
struct Options {
    dir: Option<PathBuf>,
    config: Option<PathBuf>,
    named: Vec<(String, PathBuf)>,
    caps: Caps,
    /// `--memory-ceiling`, where given.
    ceiling: Option<String>,
    /// `--loader-memory-ceiling`, where given.
    loader_ceiling: Option<String>,
    load_timeout: std::time::Duration,
    allow_unsandboxed: bool,
    /// `--require-full-sandbox`: start only where Landlock confines the engine fully, TCP included.
    require_full_sandbox: bool,
    /// `--require-memory-ceilings`: start only where the memory ceilings are enforced.
    require_memory_ceilings: bool,
    /// `--attach-rows`, `--attach-bytes`, `--session-rows`, `--session-bytes`, where given.
    limits: benchy_mcp::config::Stated,
    /// `--stream-channel`, where given: the descriptor a host's stream channel is on.
    stream_channel: Option<i32>,
}

/// A host's session's limits: the defaults, or what the operator set, on the command line
/// (`given`) or in the config at `config`, which for a host holds limits and helpers and no
/// sources. A limit set in both is refused, not one of them quietly ignored. And the config's
/// helpers (REQ-18), if it has any.
#[allow(clippy::type_complexity)]
fn host_limits(
    given: benchy_mcp::config::Stated,
    budget: &str,
    ceiling: Option<String>,
    loader_ceiling: Option<String>,
    config: Option<&Path>,
) -> Result<
    (
        benchy_mcp::admission::HostLimits,
        Vec<benchy_mcp::helpers::Def>,
    ),
    String,
> {
    let mut stated = given;
    let mut ceiling_in_config = None;
    let mut loader_in_config = None;
    let mut helpers = Vec::new();
    if let Some(c) = config {
        let sources = benchy_mcp::config::load(c)?;
        ceiling_in_config = sources.memory_ceiling.clone();
        loader_in_config = sources.loader_memory_ceiling.clone();
        if !sources.files.is_empty() || !sources.exec.is_empty() || !sources.builtin.is_empty() {
            return Err(format!(
                "{}: `host` takes no sources: the host attaches them. Its config holds [attach], \
                 [budget], [engine] and [helpers] alone.",
                c.display()
            ));
        }
        helpers = sources.helpers;
        for (i, from_config) in sources.limits.into_iter().enumerate() {
            if from_config.is_some() && stated[i].is_some() {
                return Err(format!(
                    "{}: a limit is set both there and on the command line; set it in one place",
                    c.display()
                ));
            }
            stated[i] = stated[i].or(from_config);
        }
    }
    let memory = memory(budget, ceiling, ceiling_in_config, config)?;
    let mut limits = benchy_mcp::admission::HostLimits {
        memory: memory.budget,
        memory_ceiling: memory.ceiling,
        loader_memory_ceiling: loader_memory(loader_ceiling, loader_in_config, config)?,
        ..Default::default()
    };
    let [attach_rows, attach_bytes, session_rows, session_bytes] = stated;
    for (slot, v) in [
        (&mut limits.attach_rows, attach_rows),
        (&mut limits.attach_bytes, attach_bytes),
        (&mut limits.session_rows, session_rows),
        (&mut limits.session_bytes, session_bytes),
    ] {
        if let Some(v) = v {
            *slot = v;
        }
    }
    Ok((limits, helpers))
}

/// DuckDB's memory budget and the engine's memory ceiling: `--memory`, and `--memory-ceiling` or
/// the config's `[engine] memory_ceiling`, never both (`supervisor::Memory`).
fn memory(
    budget: &str,
    given: Option<String>,
    in_config: Option<String>,
    config: Option<&Path>,
) -> Result<benchy_mcp::supervisor::Memory, String> {
    if given.is_some() && in_config.is_some() {
        return Err(format!(
            "{}: the memory ceiling is set both there and on the command line; set it in one place",
            config.map_or("the config".into(), |c| c.display().to_string())
        ));
    }
    benchy_mcp::supervisor::Memory::new(budget, given.or(in_config).as_deref())
}

/// The loader memory ceiling: `--loader-memory-ceiling` or the config's `[loader]
/// memory_ceiling`, never both (`supervisor::LOADER_MEMORY_CEILING`).
fn loader_memory(
    given: Option<String>,
    in_config: Option<String>,
    config: Option<&Path>,
) -> Result<u64, String> {
    if given.is_some() && in_config.is_some() {
        return Err(format!(
            "{}: the loader memory ceiling is set both there and on the command line; set it in \
             one place",
            config.map_or("the config".into(), |c| c.display().to_string())
        ));
    }
    benchy_mcp::supervisor::loader_memory_ceiling(given.or(in_config).as_deref())
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut dir: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    let mut named: Vec<(String, PathBuf)> = Vec::new();
    let mut caps = Caps::default();
    let mut ceiling: Option<String> = None;
    let mut loader_ceiling: Option<String> = None;
    let mut load_timeout = std::time::Duration::from_secs(300);
    let mut allow_unsandboxed = false;
    let mut require_full_sandbox = false;
    let mut require_memory_ceilings = false;
    let mut limits: benchy_mcp::config::Stated = [None; 4];
    let mut stream_channel = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" => match it.next() {
                Some(v) => config = Some(PathBuf::from(v)),
                None => return Err("--config needs a path to a benchy.toml".into()),
            },
            "--source" => match it.next().and_then(|s| s.split_once('=')) {
                Some((n, p)) => named.push((n.to_string(), PathBuf::from(p))),
                None => return Err("--source needs name=path".into()),
            },
            "--max-rows" => match it.next().and_then(|s| s.parse().ok()) {
                Some(n) => caps.max_rows = n,
                None => return Err("--max-rows needs a number".into()),
            },
            "--max-bytes" => match it.next().and_then(|s| s.parse::<usize>().ok()) {
                // Below this the notices that say what was cut would not fit, and a cap that
                // cannot explain itself is worse than a larger one.
                Some(n) if n < benchy_mcp::result::MIN_MAX_BYTES => {
                    return Err(format!(
                        "--max-bytes must be at least {}",
                        benchy_mcp::result::MIN_MAX_BYTES
                    ))
                }
                Some(n) => caps.max_bytes = n,
                None => return Err("--max-bytes needs a number".into()),
            },
            "--max-cell-bytes" => match it.next().and_then(|s| s.parse::<usize>().ok()) {
                Some(n) if n >= 16 => caps.max_cell_bytes = n,
                _ => return Err("--max-cell-bytes needs a number of at least 16".into()),
            },
            "--allow-unsandboxed" => allow_unsandboxed = true,
            "--require-full-sandbox" => require_full_sandbox = true,
            "--require-memory-ceilings" => require_memory_ceilings = true,
            "--stream-channel" => match it.next().and_then(|s| s.parse::<i32>().ok()) {
                Some(fd) if fd >= 3 => stream_channel = Some(fd),
                _ => {
                    return Err(
                        "--stream-channel needs a descriptor's number, 3 or more: 0, 1 and \
                                2 are the protocol's and the log's"
                            .into(),
                    )
                }
            },
            flag @ ("--attach-rows" | "--attach-bytes" | "--session-rows" | "--session-bytes") => {
                let i = [
                    "--attach-rows",
                    "--attach-bytes",
                    "--session-rows",
                    "--session-bytes",
                ]
                .iter()
                .position(|f| *f == flag)
                .expect("one of them");
                // Zero admits nothing: the first attach would fail far from the flag that did it.
                match it.next().and_then(|s| s.parse::<u64>().ok()) {
                    Some(n) if n > 0 => limits[i] = Some(n),
                    _ => return Err(format!("{flag} needs a number, at least 1")),
                }
            }
            "--load-timeout" => match it.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(n) if n > 0 => load_timeout = std::time::Duration::from_secs(n),
                _ => return Err("--load-timeout needs a number of seconds".into()),
            },
            "--deadline" => match it.next().and_then(|s| s.parse::<u64>().ok()) {
                // At zero every query fails at once.
                Some(n) if n > 0 => caps.deadline = std::time::Duration::from_secs(n),
                _ => return Err("--deadline needs a number of seconds, at least 1".into()),
            },
            "--hard-grace" => match it.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(n) => caps.hard_grace = std::time::Duration::from_secs(n),
                None => return Err("--hard-grace needs a number of seconds".into()),
            },
            "--memory" => match it.next() {
                Some(v) => caps.memory_limit = v.clone(),
                None => return Err("--memory needs a size like 1GB".into()),
            },
            "--memory-ceiling" => match it.next() {
                Some(v) => ceiling = Some(v.clone()),
                None => return Err("--memory-ceiling needs a size like 2GB".into()),
            },
            "--loader-memory-ceiling" => match it.next() {
                Some(v) => loader_ceiling = Some(v.clone()),
                None => return Err("--loader-memory-ceiling needs a size like 1GB".into()),
            },
            other if other.starts_with('-') => return Err(format!("unknown option `{other}`")),
            other => dir = Some(PathBuf::from(other)),
        }
    }
    // Checked now, so a typo stops startup whatever else is wrong; again once a config is read.
    benchy_mcp::supervisor::Memory::new(&caps.memory_limit, ceiling.as_deref())?;
    benchy_mcp::supervisor::loader_memory_ceiling(loader_ceiling.as_deref())?;
    Ok(Options {
        dir,
        config,
        named,
        caps,
        ceiling,
        loader_ceiling,
        load_timeout,
        allow_unsandboxed,
        require_full_sandbox,
        require_memory_ceilings,
        limits,
        stream_channel,
    })
}

/// REQ-7 `ceilings_that_cannot_be_enforced_are_refused_when_required`: whether a session started
/// with `--require-memory-ceilings` may go on, where the engine's ceiling and the loaders' are
/// not enforced for the reasons given, if any.
fn ceilings_enforced(engine: Option<&str>, loader: Option<&str>) -> Result<(), String> {
    let unenforced: Vec<String> = [
        ("--memory-ceiling", engine),
        ("--loader-memory-ceiling", loader),
    ]
    .into_iter()
    .filter_map(|(flag, why)| why.map(|why| format!("{flag} is not enforced here: {why}")))
    .collect();
    if unenforced.is_empty() {
        return Ok(());
    }
    Err(format!(
        "--require-memory-ceilings: {}. Run on Linux, or start without the flag.",
        unenforced.join("; ")
    ))
}

/// On the operator's log, where the posture confines files and not TCP: `describe` says only that
/// some rights are unsupported, and which ones is what an operator acts on.
fn say_what_the_network_is(posture: &benchy_mcp::sandbox::Posture) {
    if !posture.is_enforced() {
        return;
    }
    if let Some(why) = posture.shortfall(benchy_mcp::sandbox::landlock_abi()) {
        eprintln!(
            "benchy: {why}; the engine and every confined loader may open any TCP connection. \
             --require-full-sandbox refuses to start here."
        );
    }
}

/// `--stream-channel FD`: the descriptor the host's stream channel is on, inherited, taken as the
/// session's. It must be open for reading, and a pipe, a socket or a file: what Node.js's
/// `spawn(..., {stdio: [..., 'pipe']})`, Python's `pass_fds` or a shell's `3<` gives. It is made
/// close-on-exec, so nothing benchy starts inherits it but the loader handed a copy for one
/// stream, and blocking, so that loader waits for the host to write: a pipe an event loop made is
/// often non-blocking, and the flag is the open file's, which the host has no use for here.
fn take_channel(fd: i32) -> Result<std::os::fd::OwnedFd, String> {
    use std::os::fd::FromRawFd;
    let why = |what: &str| {
        format!(
            "--stream-channel {fd}: descriptor {fd} {what}; start benchy with the channel's read \
             end inherited as descriptor {fd}"
        )
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(why("is not open"));
    }
    if !matches!(
        st.st_mode & libc::S_IFMT,
        libc::S_IFIFO | libc::S_IFSOCK | libc::S_IFREG
    ) {
        return Err(why(
            "is not a pipe, a socket or a file to read streams from",
        ));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE == libc::O_WRONLY {
        return Err(why("is not open for reading"));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(why("could not be made close-on-exec and blocking"));
    }
    // SAFETY: inherited, open, and owned by nothing else in this process.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// `benchy-mcp host`: a session whose data a host attaches, over the host protocol.
fn serve_host(
    caps: Caps,
    load_timeout: std::time::Duration,
    allow_unsandboxed: bool,
    require_full_sandbox: bool,
    limits: benchy_mcp::admission::HostLimits,
    helpers: Vec<benchy_mcp::helpers::Def>,
    channel: Option<std::os::fd::OwnedFd>,
) -> ExitCode {
    let session = match Session::create() {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    // Forked before the signal thread starts: see the server's.
    if let Err(e) = trace::span("warden.spawn", || {
        benchy_mcp::supervisor::start_warden(&session.0)
    }) {
        return fail(&e);
    }
    clean_up_on_signals(session.0.clone());
    let mut supervisor = match trace::span("engine.start", || {
        Supervisor::start(benchy_mcp::supervisor::EngineSpec {
            session: session.0.clone(),
            memory_limit: caps.memory_limit.clone(),
            load_timeout,
            allow_unsandboxed,
            // The operator's; a host may lower them in `hello`, which reports them.
            limits,
        })
    }) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let has_channel = channel.is_some();
    if let Some(fd) = channel {
        supervisor.set_channel(fd);
    }
    let prover = benchy_mcp::host::Prover::start();
    // `hello` reports the engine and its sandbox, so a host's session waits for the engine.
    let opened = match supervisor.opened() {
        Ok(o) => o,
        Err(e) => return fail(&e),
    };
    let landlock_abi = benchy_mcp::sandbox::landlock_abi();
    // REQ-3 `a_session_that_must_be_fully_sandboxed_refuses_to_start_otherwise`, before anything
    // is loaded: the helpers' job is a loader too.
    if require_full_sandbox {
        if let Err(e) = benchy_mcp::sandbox::require_full(&opened.posture, landlock_abi) {
            return fail(&e);
        }
    }
    // REQ-18: the helpers, made before the host's first call, against no table yet. One that
    // cannot be made stops startup; one that reads a table waits for the host to attach it.
    if !helpers.is_empty() {
        supervisor.set_helpers(helpers);
        let built = match trace::span("helpers", || supervisor.build_helpers()) {
            Ok(b) => b,
            Err(e) => return fail(&format!("the helpers could not be made: {e}")),
        };
        if !built.broken.is_empty() {
            return fail(&built.broken.join("\nbenchy: "));
        }
        for (name, table) in built.waiting() {
            eprintln!("benchy: helper `{name}` waits for table `{table}`");
        }
    }
    eprintln!(
        "benchy: host protocol on stdio; Landlock {}",
        opened.posture.describe()
    );
    say_what_the_network_is(&opened.posture);
    benchy_mcp::host::serve(
        supervisor,
        benchy_mcp::host::Session {
            engine: opened.engine_version,
            sandbox: opened.posture,
            landlock_abi,
            caps,
            limits,
            dir: session.0.clone(),
            channel: has_channel,
        },
        prover,
    );
    ExitCode::SUCCESS
}

/// REQ-3 `doctor_probes_the_restriction_the_server_applies`: say which layers this machine gives, and never
/// report a boolean where the answer is a level.
///
/// With `--json`, which probes, the report is one JSON object on stdout and nothing else.
fn doctor(probe: bool, json: bool) -> ExitCode {
    if json {
        return if probe_restriction(true) {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }
    println!("benchy-mcp {}", env!("CARGO_PKG_VERSION"));
    let (engine, file) = duckdb_library();
    println!("engine       : DuckDB {engine} (statement allowlist + locked configuration)");
    // A release bundle carries the library beside the binary; when the two were separated, or a
    // stray copy elsewhere wins, this is the line that shows it.
    match file {
        Some(f) => println!("               loaded from {}", f.display()),
        None => println!("               loaded from an unknown file"),
    }
    if probe {
        // Measured wherever it runs: where Landlock is missing, the probe says so and fails.
        if !probe_restriction(false) {
            return ExitCode::from(1);
        }
    } else {
        report_sandbox();
    }
    println!("limits       : a per-batch deadline (--deadline) and DuckDB's memory budget");
    println!("               (--memory), both enforced. Neither bounds query *planning*: a");
    println!("               nested-MAP blowup was measured at 7.8s against a 2s interrupt.");
    match benchy_mcp::supervisor::MEMORY_CEILING_UNENFORCED {
        None => {
            println!(
                "supervisor   : the engine is a separate process. Past --hard-grace seconds, or"
            );
            println!(
                "               past --memory-ceiling of resident memory, it is killed, and a new"
            );
            println!(
                "               one attaches what the session loaded: a wedged or swollen planner"
            );
            println!(
                "               costs the query and its scratch, not the server, and no source is"
            );
            println!("               read twice.");
        }
        // REQ-7 `a_memory_ceiling_that_cannot_be_enforced_is_not_reported`
        Some(why) => {
            println!(
                "supervisor   : the engine is a separate process. Past --hard-grace seconds it is"
            );
            println!(
                "               killed, and a new one attaches what the session loaded: a wedged"
            );
            println!("               planner costs the query and its scratch, not the server.");
            println!("               --memory-ceiling is NOT ENFORCED here: {why}.");
        }
    }
    // REQ-8 `a_loader_past_its_memory_ceiling_is_killed_and_attaches_nothing`
    match benchy_mcp::supervisor::LOADER_MEMORY_CEILING_UNENFORCED {
        None => {
            println!(
                "loaders      : a process each. Past --loader-memory-ceiling of resident memory,"
            );
            println!(
                "               a connector and what it starts included, a loader is killed and"
            );
            println!("               its load fails, attaching nothing.");
        }
        Some(why) => {
            println!("loaders      : --loader-memory-ceiling is NOT ENFORCED here: {why}.");
        }
    }
    ExitCode::SUCCESS
}

/// The DuckDB library this process runs: its version, and the file the dynamic loader took it
/// from. Asked of the library rather than written down, for the same reason the MCP instructions
/// ask (REQ-11): a version in a string literal goes stale in silence.
fn duckdb_library() -> (String, Option<PathBuf>) {
    use std::ffi::CStr;
    let version = unsafe { CStr::from_ptr(libduckdb_sys::duckdb_library_version()) }
        .to_string_lossy()
        .into_owned();
    // dlsym rather than the function's address: the address an executable sees can be a stub
    // in the executable itself, which dladdr would then name.
    let file = unsafe {
        let sym = libc::dlsym(libc::RTLD_DEFAULT, c"duckdb_library_version".as_ptr());
        let mut info: libc::Dl_info = std::mem::zeroed();
        if sym.is_null() || libc::dladdr(sym, &mut info) == 0 || info.dli_fname.is_null() {
            None
        } else {
            let name = CStr::from_ptr(info.dli_fname)
                .to_string_lossy()
                .into_owned();
            Some(std::fs::canonicalize(&name).unwrap_or_else(|_| PathBuf::from(name)))
        }
    };
    (version, file)
}

/// What `doctor` says about the sandbox without measuring it.
#[cfg(target_os = "linux")]
fn report_sandbox() {
    let enabled = std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|s| s.contains("landlock"))
        .unwrap_or(false);
    if enabled {
        println!("landlock     : present in the kernel LSM list");
        println!(
            "               minimum needed: ABI {}",
            benchy_mcp::sandbox::MINIMUM_ABI
        );
        println!("               run `benchy-mcp doctor --probe` to measure it for real");
    } else {
        println!("landlock     : NOT AVAILABLE — the engine cannot be confined here, and");
        println!("               benchy refuses to serve unless started with --allow-unsandboxed.");
        println!("               REQ-2's two SQL mechanisms still hold without it.");
    }
}

#[cfg(not(target_os = "linux"))]
fn report_sandbox() {
    println!("landlock     : Linux only, and nothing equivalent is wired up here. benchy refuses");
    println!("               to serve unless started with --allow-unsandboxed, and then the");
    println!("               SQL layer is the only boundary.");
}

/// What `probe_child` checks, in order, with the answer the engine's restriction must give.
const ENGINE_PROBES: [(&str, bool); 8] = [
    ("read in its workspace", true),
    ("read outside it", false),
    ("create a file in its workspace", false),
    ("read the store's contents", false),
    ("delete in its session", true),
    ("truncate in its workspace", false),
    ("truncate a file outside it", false),
    (TCP_PROBE, false),
];

/// What `probe_loader_child` checks, with the answer a file loader's restriction must give.
const LOADER_PROBES: [(&str, bool); 9] = [
    ("read its input", true),
    ("read anything else", false),
    ("write in its staging directory", true),
    ("write anywhere else", false),
    ("read the store's contents", false),
    ("truncate its staged files", true),
    ("truncate its input", false),
    ("truncate anything else", false),
    (TCP_PROBE, false),
];

/// What `probe_network_loader_child` checks, with the answer a network loader's restriction must
/// give (REQ-3 `a_network_loader_reaches_only_its_source_and_its_staging`).
const NETWORK_PROBES: [(&str, bool); 11] = [
    ("read the resolver's files", true),
    ("resolve a name", true),
    ("read anything else", false),
    ("write in its staging directory", true),
    ("write anywhere else", false),
    ("read the store's contents", false),
    ("truncate its staged files", true),
    ("truncate anything else", false),
    (SOURCE_PORT_PROBE, true),
    (OTHER_PORT_PROBE, false),
    (LISTEN_PROBE, false),
];

/// Whether a TCP connection to `port` may be attempted, as far as Landlock is concerned. Landlock
/// refuses with `EACCES`; a port with nothing listening refuses from the other side, after the
/// attempt was allowed; and `EPERM` is somebody else's refusal — a cgroup filter, say — not
/// Landlock's.
fn tcp_allowed(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)) {
        Ok(_) => true,
        Err(e) => e.raw_os_error() != Some(libc::EACCES),
    }
}

/// Whether `path` may be truncated by name, truncate(2), as far as Landlock is concerned: it
/// refuses with `EACCES`, where file permissions would too, so every file probed is this user's
/// own and writable.
fn truncate_allowed(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    if unsafe { libc::truncate(c.as_ptr(), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::EACCES)
}

/// Whether a TCP port may be listened on, as far as Landlock is concerned.
fn listen_allowed() -> bool {
    match std::net::TcpListener::bind(("127.0.0.1", 0)) {
        Ok(_) => true,
        Err(e) => e.raw_os_error() != Some(libc::EACCES),
    }
}

/// The probes of TCP, which Landlock can only refuse from ABI 4 (Linux 6.7).
const TCP_PROBE: &str = "open a TCP connection";
const SOURCE_PORT_PROBE: &str = "connect to its source's port";
const OTHER_PORT_PROBE: &str = "connect to another port";
const LISTEN_PROBE: &str = "listen on a port";
const TCP_PROBES: [&str; 4] = [TCP_PROBE, SOURCE_PORT_PROBE, OTHER_PORT_PROBE, LISTEN_PROBE];

/// The Landlock ABI a probe's right needs before Landlock can withhold it: TCP rules came with 4
/// (Linux 6.7), truncate(2) with 3 (Linux 6.2), everything else with 1.
fn needs(probe: &str) -> (u32, &'static str) {
    if TCP_PROBES.contains(&probe) {
        (4, "TCP rules need 4, Linux 6.7")
    } else if probe.starts_with("truncate") {
        (3, "truncate needs 3, Linux 6.2")
    } else {
        (1, "")
    }
}

/// What a probe must answer on a kernel with Landlock ABI `abi`, or `None` where the right is not
/// Landlock's to withhold there. On an older kernel a best-effort ruleset confines what that
/// kernel can, exactly as intended, and that is not a failure of what REQ-3 asks of it.
fn expected(probe: &(&str, bool), abi: u32) -> Option<bool> {
    if abi < needs(probe.0).0 {
        return None;
    }
    Some(probe.1)
}

/// What one mechanism's probe found: how the child said it was confined, and each check, with
/// what it could do and what it must (`None` where the right is not Landlock's to withhold on
/// this kernel, which does not count). `error` when the probe could not run at all.
struct Mechanism {
    name: &'static str,
    lead: &'static str,
    what: &'static str,
    posture: String,
    checks: Vec<(&'static str, Option<bool>, Option<bool>)>,
    error: Option<String>,
}

impl Mechanism {
    /// Whether this mechanism is confined as it must be: the probe ran, at least one check
    /// counted, and every check that counted held. A check that does not count is not a pass: at
    /// ABI 0 every check is skipped, and each entry said `"enforced": true` beside a verdict of
    /// `not_enforced`, from a host's report (`doctor_without_landlock_says_not_enforced`).
    fn enforced(&self) -> bool {
        self.error.is_none()
            && self.checks.iter().any(|(_, _, must)| must.is_some())
            && self
                .checks
                .iter()
                .all(|(_, got, must)| must.is_none() || got == must)
    }

    fn failed(name: &'static str, lead: &'static str, what: &'static str, e: String) -> Self {
        Mechanism {
            name,
            lead,
            what,
            posture: "not probed".into(),
            checks: Vec::new(),
            error: Some(e),
        }
    }

    /// The prose `doctor --probe` prints, added to `out`.
    fn print(&self, abi: u32, out: &mut String) {
        use std::fmt::Write;
        let lead = self.lead;
        if let Some(e) = &self.error {
            let _ = writeln!(out, "{lead}the probe could not run: {e}");
            return;
        }
        let _ = writeln!(
            out,
            "{lead}{} — {}, applied in a child",
            self.posture, self.what
        );
        for (probe, got, must) in &self.checks {
            let got = got.map_or("?", yes);
            let _ = match must {
                None => writeln!(
                    out,
                    "               {probe:<31}: {got:<3} (not Landlock's to refuse here: this \
                     kernel speaks ABI {abi}, and {})",
                    needs(probe).1
                ),
                Some(must) => writeln!(
                    out,
                    "               {probe:<31}: {got:<3} (must be {}){}",
                    yes(*must),
                    if got == yes(*must) { "" } else { "  <- wrong" }
                ),
            };
        }
    }

    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "enforced": self.enforced(),
            "posture": self.posture,
            "checks": self.checks.iter().map(|(check, got, must)| serde_json::json!({
                "check": check, "allowed": got, "must_be": must,
            })).collect::<Vec<_>>(),
            "error": self.error,
        })
    }
}

/// REQ-3 `doctor_probes_the_restriction_the_server_applies`: build a session the way the server
/// does, have a child apply the engine's own restriction to it and another a file loader's, and
/// check each right they grant and each they withhold. Measured, not advised. Printed as prose,
/// or as JSON for a host to check at container start
/// (`doctor_says_in_json_what_each_mechanism_enforces`).
fn probe_restriction(json: bool) -> bool {
    use std::os::unix::fs::DirBuilderExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let session = std::env::temp_dir().join(format!("benchy-probe-{}-{nanos}", std::process::id()));
    let outside = session.with_extension("outside");
    let input = session.with_extension("input");
    let made = std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&session)
        .and_then(|_| std::fs::create_dir(session.join("ws")))
        .and_then(|_| std::fs::create_dir(session.join("store")))
        .and_then(|_| std::fs::create_dir(session.join("stage")))
        .and_then(|_| std::fs::write(session.join("ws/inside"), "in"))
        .and_then(|_| std::fs::write(session.join("store/held"), "held"))
        .and_then(|_| std::fs::write(&outside, "out"))
        .and_then(|_| std::fs::write(&input, "a,b\n1,2\n"));
    // Asked once, so every probe is judged against the same kernel.
    let abi = benchy_mcp::sandbox::landlock_abi();
    const ENGINE: (&str, &str, &str) =
        ("engine", "landlock     : ", "the engine's own restriction");
    const LOADER: (&str, &str, &str) = (
        "file_loader",
        "               ",
        "a file loader's own restriction",
    );
    let mechanisms = match made {
        Ok(()) => {
            let engine = probe(
                "--probe-child",
                &[&session, &outside],
                ENGINE,
                &ENGINE_PROBES,
                abi,
            );
            // Its own session: the engine's child may have deleted in this one.
            let _ = std::fs::write(session.join("store/held"), "held");
            let loader = probe(
                "--probe-loader",
                &[&session, &input, &outside],
                LOADER,
                &LOADER_PROBES,
                abi,
            );
            let _ = std::fs::write(session.join("store/held"), "held");
            let network = probe_network(&session, &outside, abi);
            vec![engine, loader, network]
        }
        Err(e) => vec![Mechanism::failed(
            ENGINE.0,
            ENGINE.1,
            ENGINE.2,
            e.to_string(),
        )],
    };
    let _ = std::fs::remove_dir_all(&session);
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_file(&input);
    let (all, said) = report(abi, &mechanisms, json);
    print!("{said}");
    all
}

/// The verdict on what the probes found, on a kernel with Landlock ABI `abi`: `Err` with the
/// reason when REQ-3 is not enforced, and `Ok(Some(why))` when it is, partially
/// (`doctor_without_tcp_rules_says_partial`).
fn verdict(abi: u32, mechanisms: &[Mechanism]) -> Result<Option<String>, String> {
    // First, and whatever the checks say: below the minimum every check is above the kernel's
    // ABI and skipped (`expected`), and nothing checked is not "enforced".
    let minimum = benchy_mcp::sandbox::MINIMUM_ABI_NUMBER;
    if abi < minimum {
        return Err(if abi == 0 {
            "there is no Landlock here, so nothing is confined".into()
        } else {
            format!("this kernel's Landlock ABI {abi} is below the minimum, {minimum}")
        });
    }
    if let Some(m) = mechanisms.iter().find(|m| !m.enforced()) {
        return Err(format!("the {} is not confined as it must be", m.name));
    }
    // Every check that counted held: as full as this kernel allows. Below ABI 4 that leaves TCP
    // unconfined, and the engine's `restrict_self` says `PartiallyEnforced` of the same kernel,
    // so this says what `hello`'s `sandbox_posture` does, in its words.
    Ok(benchy_mcp::sandbox::Posture::Full.shortfall(abi))
}

/// What `doctor --probe` says about what the probes found, as prose or as JSON, and whether the
/// machine enforces REQ-3; apart from the probing so that it can be held to a kernel this one is
/// not (`doctor_without_landlock_says_not_enforced`).
fn report(abi: u32, mechanisms: &[Mechanism], json: bool) -> (bool, String) {
    use std::fmt::Write;
    let verdict = verdict(abi, mechanisms);
    // Partial is a success: the server serves under it without a flag, since the filesystem
    // boundary holds, and `--require-full-sandbox` is how an operator refuses it.
    let all = verdict.is_ok();
    let (word, reason) = match &verdict {
        Ok(None) => ("enforced", None),
        Ok(Some(why)) => ("partial", Some(why.clone())),
        Err(why) => ("not_enforced", Some(why.clone())),
    };
    let mut out = String::new();
    if json {
        let (engine, file) = duckdb_library();
        let report = serde_json::json!({
            "benchy": env!("CARGO_PKG_VERSION"),
            "engine": format!("DuckDB {engine}"),
            "library": file.map(|f| f.display().to_string()),
            "landlock_abi": abi,
            "verdict": word,
            "reason": reason,
            "mechanisms": mechanisms.iter().map(Mechanism::json).collect::<Vec<_>>(),
        });
        let _ = writeln!(out, "{report:#}");
        return (all, out);
    }
    for m in mechanisms {
        m.print(abi, &mut out);
    }
    match verdict {
        Ok(None) => {
            let _ = writeln!(
                out,
                "               VERIFIED — this machine enforces REQ-3."
            );
        }
        Ok(Some(why)) => {
            let _ = writeln!(out, "               PARTIAL — {why}.");
            let _ = writeln!(
                out,
                "               The engine and every confined loader may open any TCP connection;"
            );
            let _ = writeln!(
                out,
                "               --require-full-sandbox refuses to start here."
            );
        }
        Err(why) => {
            let _ = writeln!(out, "               NOT ENFORCED — {why}.");
            let _ = writeln!(
                out,
                "               REQ-2's SQL mechanisms still hold, but the redundancy REQ-3"
            );
            let _ = writeln!(out, "               provides does not on this host.");
        }
    }
    (all, out)
}

/// The network loader's probe: two ports listened on here, one its source's and one another's,
/// so that a refused connection can only be the restriction's.
fn probe_network(session: &Path, outside: &Path, abi: u32) -> Mechanism {
    const NETWORK: (&str, &str, &str) = (
        "network_loader",
        "               ",
        "a network loader's own restriction",
    );
    let listen = || std::net::TcpListener::bind(("127.0.0.1", 0));
    let ports = listen().and_then(|a| {
        let b = listen()?;
        Ok((a.local_addr()?.port(), b.local_addr()?.port(), a, b))
    });
    let (source, other, _a, _b) = match ports {
        Ok(p) => p,
        Err(e) => {
            return Mechanism::failed(
                NETWORK.0,
                NETWORK.1,
                NETWORK.2,
                format!("the network loader's probe could not listen: {e}"),
            )
        }
    };
    let (source, other) = (source.to_string(), other.to_string());
    probe(
        "--probe-network-loader",
        &[session, outside, Path::new(&source), Path::new(&other)],
        NETWORK,
        &NETWORK_PROBES,
        abi,
    )
}

/// Run one probe child and collect what it could and could not do.
fn probe(
    flag: &str,
    args: &[&Path],
    (name, lead, what): (&'static str, &'static str, &'static str),
    probes: &[(&'static str, bool)],
    abi: u32,
) -> Mechanism {
    let answers = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .arg(flag)
            .args(args)
            .output()
    });
    let out = match answers {
        Ok(o) => o,
        Err(e) => return Mechanism::failed(name, lead, what, e.to_string()),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let posture = lines.next().unwrap_or("no answer").to_string();
    let checks = probes
        .iter()
        .map(|p| (p.0, lines.next().map(|l| l == "yes"), expected(p, abi)))
        .collect();
    Mechanism {
        name,
        lead,
        what,
        posture,
        checks,
        error: None,
    }
}

/// The engine's half of `probe_restriction`, in a process of its own: restrict exactly as the
/// engine does, then try each thing in `ENGINE_PROBES` and answer yes or no, one per line.
fn probe_child(session: &Path, outside: &Path) -> ExitCode {
    use benchy_mcp::sandbox;
    let ws = session.join("ws");
    let posture = sandbox::restrict_to(&ws, session);
    println!("{}", posture.describe());
    let answers = [
        std::fs::read(ws.join("inside")).is_ok(),
        std::fs::read(outside).is_ok(),
        std::fs::write(ws.join("created"), "x").is_ok(),
        // Before the delete below, so a "no" means refused rather than gone.
        std::fs::read(session.join("store/held")).is_ok(),
        std::fs::remove_file(session.join("store/held")).is_ok(),
        truncate_allowed(&ws.join("inside")),
        truncate_allowed(outside),
        tcp_allowed(9),
    ];
    for a in answers {
        println!("{}", yes(a));
    }
    ExitCode::SUCCESS
}

/// A file loader's half: restrict exactly as a file loader does, then try each thing in
/// `LOADER_PROBES`.
fn probe_loader_child(session: &Path, input: &Path, outside: &Path) -> ExitCode {
    use benchy_mcp::sandbox;
    let stage = session.join("stage");
    let posture = sandbox::restrict_loader(input, &stage);
    println!("{}", posture.describe());
    let answers = [
        std::fs::read(input).is_ok(),
        std::fs::read(outside).is_ok(),
        std::fs::write(stage.join("written"), "x").is_ok(),
        std::fs::write(session.join("written"), "x").is_ok(),
        std::fs::read(session.join("store/held")).is_ok(),
        truncate_allowed(&stage.join("written")),
        truncate_allowed(input),
        truncate_allowed(outside),
        tcp_allowed(9),
    ];
    for a in answers {
        println!("{}", yes(a));
    }
    ExitCode::SUCCESS
}

/// A network loader's half: restrict exactly as a built-in source's loader does, its source a
/// ClickHouse server on `source`, then try each thing in `NETWORK_PROBES`.
fn probe_network_loader_child(session: &Path, outside: &Path, source: u16, other: u16) -> ExitCode {
    use benchy_mcp::sandbox;
    let stage = session.join("stage");
    // The ports as a loader gets them, from its source's URL.
    let ports: Vec<u16> = benchy_mcp::sources::port_of(&format!("http://127.0.0.1:{source}"))
        .into_iter()
        .collect();
    let posture = sandbox::restrict_network_loader(&stage, &ports);
    println!("{}", posture.describe());
    let resolver = sandbox::RESOLVER_FILES
        .iter()
        .map(Path::new)
        .filter(|f| f.exists())
        .all(|f| std::fs::read(f).is_ok());
    let resolves = {
        use std::net::ToSocketAddrs;
        ("localhost", 0)
            .to_socket_addrs()
            .is_ok_and(|mut a| a.next().is_some())
    };
    let answers = [
        resolver,
        resolves,
        std::fs::read(outside).is_ok(),
        std::fs::write(stage.join("written"), "x").is_ok(),
        std::fs::write(session.join("written"), "x").is_ok(),
        std::fs::read(session.join("store/held")).is_ok(),
        truncate_allowed(&stage.join("written")),
        truncate_allowed(outside),
        tcp_allowed(source),
        tcp_allowed(other),
        listen_allowed(),
    ];
    for a in answers {
        println!("{}", yes(a));
    }
    ExitCode::SUCCESS
}

fn yes(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("benchy: {msg}");
    ExitCode::from(1)
}

/// `benchy-mcp exec-test <config> <source>` — check a connector against the contract, without a
/// server and without a model.
///
/// A connector author otherwise debugs by starting a whole MCP server and reading a rejection
/// written for an operator. This runs the same validation the loader runs and says what to
/// change.
fn exec_test(args: &[String]) -> ExitCode {
    let (Some(cfg), Some(want)) = (args.first(), args.get(1)) else {
        eprintln!("usage: benchy-mcp exec-test <benchy.toml> <source-name>");
        return ExitCode::from(2);
    };
    let sources = match benchy_mcp::config::load(std::path::Path::new(cfg)) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let Some(src) = sources.exec.iter().find(|s| &s.name == want) else {
        let names: Vec<&str> = sources.exec.iter().map(|s| s.name.as_str()).collect();
        return fail(&format!(
            "no exec source named `{want}` in {cfg}. Found: {}",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ));
    };

    println!("exec-test {} — {}\n", src.name, src.command.display());
    let failed = print_checks(&benchy_mcp::exec::conformance(src));
    if failed > 0 {
        println!("{failed} check(s) failed. See docs/EXEC-CONNECTOR-PROTOCOL.md.");
        return ExitCode::FAILURE;
    }
    println!("all checks passed — this connector is loadable.");
    ExitCode::SUCCESS
}

/// The checks of `exec-test` and `check`, one style for both: a mark, the label, the detail
/// indented under it. Returns how many failed.
fn print_checks(checks: &[benchy_mcp::exec::Check]) -> usize {
    let mut failed = 0;
    for c in checks {
        let mark = if !c.ok {
            failed += 1;
            "FAIL"
        } else if c.skipped {
            "skip"
        } else if c.warn {
            "warn"
        } else {
            " ok "
        };
        println!("  {mark}  {}", c.label);
        if !c.detail.is_empty() {
            println!("        {}", c.detail.replace('\n', "\n   "));
        }
    }
    println!();
    failed
}

/// `benchy-mcp check <config> <source>` — one source, each step of its load, without a server
/// or a model. A built-in source is reached, signed in to, described, estimated and asked for one
/// row; an exec source gets `exec-test`'s conformance checks.
fn check(args: &[String]) -> ExitCode {
    let (Some(cfg), Some(want)) = (args.first(), args.get(1)) else {
        eprintln!("usage: benchy-mcp check <benchy.toml> <source-name>");
        return ExitCode::from(2);
    };
    let sources = match benchy_mcp::config::load(std::path::Path::new(cfg)) {
        Ok(s) => s,
        Err(e) => return fail(&e),
    };
    let (title, checks) = if let Some(b) = sources.builtin.iter().find(|b| b.name() == want) {
        (b.origin(), b.check())
    } else if let Some(e) = sources.exec.iter().find(|e| &e.name == want) {
        (
            format!("exec {}", e.command.display()),
            benchy_mcp::exec::conformance(e),
        )
    } else {
        let mut names: Vec<&str> = sources.builtin.iter().map(|b| b.name()).collect();
        names.extend(sources.exec.iter().map(|e| e.name.as_str()));
        return fail(&format!(
            "no ClickHouse, HTTP or exec source named `{want}` in {cfg}. Found: {}",
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ));
    };
    println!("check {want} — {title}\n");
    let failed = print_checks(&checks);
    if failed > 0 {
        println!("{failed} check(s) failed.");
        return ExitCode::FAILURE;
    }
    println!("all checks passed — `{want}` is loadable.");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::{expected, report, Mechanism, ENGINE_PROBES, LOADER_PROBES, NETWORK_PROBES};

    /// The three mechanisms as the probe finds them where Landlock confines nothing: every right
    /// allowed, and judged against what `expected` asks at `abi`.
    fn unconfined(abi: u32) -> Vec<Mechanism> {
        [
            ("engine", &ENGINE_PROBES[..]),
            ("file_loader", &LOADER_PROBES[..]),
            ("network_loader", &NETWORK_PROBES[..]),
        ]
        .into_iter()
        .map(|(name, probes)| Mechanism {
            name,
            lead: "",
            what: "",
            posture: "unrestricted".into(),
            checks: probes
                .iter()
                .map(|p| (p.0, Some(true), expected(p, abi)))
                .collect(),
            error: None,
        })
        .collect()
    }

    /// The three mechanisms as the probe finds them where Landlock confines all it can at `abi`:
    /// each check that counts held, and each that does not allowed.
    fn confined(abi: u32) -> Vec<Mechanism> {
        unconfined(abi)
            .into_iter()
            .map(|mut m| {
                for c in &mut m.checks {
                    c.1 = c.2.or(Some(true));
                }
                m
            })
            .collect()
    }

    #[test]
    fn doctor_without_landlock_says_not_enforced() {
        // Measured before the fix on a machine without Landlock (ABI 0): every right is above the
        // kernel's ABI, so every check was skipped, and nothing checked passed as "enforced" —
        // "VERIFIED" in prose, `"verdict": "enforced"` in JSON, exit status 0.
        let (ok, text) = report(0, &unconfined(0), false);
        assert!(!ok, "{text}");
        assert!(text.contains("NOT ENFORCED"), "{text}");
        assert!(!text.contains("VERIFIED"), "{text}");
        let (ok, json) = report(0, &unconfined(0), true);
        assert!(!ok, "{json}");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["verdict"], "not_enforced", "{json}");
        assert_eq!(v["landlock_abi"], 0, "{json}");
        assert!(v["reason"].as_str().unwrap().contains("Landlock"), "{json}");
        // Each mechanism says the same as the verdict: at ABI 0 none was checked, so none is
        // enforced (a host's report: every entry said `"enforced": true`).
        let each = v["mechanisms"].as_array().expect("mechanisms");
        assert_eq!(each.len(), 3, "{json}");
        for m in each {
            assert_eq!(m["enforced"], false, "{m}");
        }
        assert_eq!(
            format!("V{}", benchy_mcp::sandbox::MINIMUM_ABI_NUMBER),
            benchy_mcp::sandbox::MINIMUM_ABI
        );
        // and a kernel that has it, confining as it must, is still enforced
        let (ok, text) = report(4, &confined(4), false);
        assert!(ok, "{text}");
        assert!(text.contains("VERIFIED"), "{text}");
        let (_, json) = report(1, &confined(1), true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        for m in v["mechanisms"].as_array().expect("mechanisms") {
            assert_eq!(m["enforced"], true, "{m}");
        }
        // And a mechanism whose check fails is not enforced, while the others are.
        let mut one_wrong = confined(1);
        let wrong = one_wrong[1]
            .checks
            .iter_mut()
            .find(|c| c.2 == Some(false))
            .expect("a right the loader must not have");
        wrong.1 = Some(true);
        let (ok, json) = report(1, &one_wrong, true);
        assert!(!ok, "{json}");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let flags: Vec<_> = v["mechanisms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["enforced"].clone())
            .collect();
        assert_eq!(flags, [true, false, true], "{json}");
    }

    #[test]
    fn doctor_without_tcp_rules_says_partial() {
        // Below ABI 4 (Linux 6.7) the TCP checks do not count, every other one held, and the
        // verdict was `enforced` and "VERIFIED", where `hello`'s `sandbox_posture` says `partial`
        // of the same kernel: files confined, the network not. No kernel before 6.7 is to be had
        // here, so the ABI is given.
        for abi in [1, 3] {
            let (ok, text) = report(abi, &confined(abi), false);
            // The server serves under it without a flag, so the exit status is success.
            assert!(ok, "{text}");
            assert!(text.contains("PARTIAL"), "{text}");
            assert!(!text.contains("VERIFIED"), "{text}");
            assert!(text.contains("TCP") && text.contains("6.7"), "{text}");
            let (ok, json) = report(abi, &confined(abi), true);
            assert!(ok, "{json}");
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v["verdict"], "partial", "{json}");
            assert_eq!(
                v["reason"],
                benchy_mcp::sandbox::Posture::Partial.report(abi)["reason"],
                "the reason `hello` gives: {json}"
            );
            for m in v["mechanisms"].as_array().expect("mechanisms") {
                assert_eq!(m["enforced"], true, "{m}");
            }
        }
        // With TCP rules it is enforced, with no reason.
        let (ok, json) = report(4, &confined(4), true);
        assert!(ok, "{json}");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["verdict"], "enforced", "{json}");
        assert!(v["reason"].is_null(), "{json}");
        // And a check that fails is not enforced, partial kernel or not.
        let mut wrong = confined(3);
        let c = wrong[0]
            .checks
            .iter_mut()
            .find(|c| c.2 == Some(false))
            .expect("a right the engine must not have");
        c.1 = Some(true);
        let (ok, json) = report(3, &wrong, true);
        assert!(!ok, "{json}");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["verdict"], "not_enforced", "{json}");
    }

    #[test]
    fn the_doctor_json_has_the_documented_keys() {
        // A host checks a container at start by these keys, and the README once said only "a
        // verdict and each mechanism". Its table now lists them, and this holds the JSON to it:
        // a key added, renamed or dropped fails here until the table says so.
        let readme = include_str!("../README.md");
        let table = readme
            .split_once("The JSON has these keys")
            .expect("the README documents the doctor's JSON")
            .1;
        let (mut top, mut each) = (Vec::new(), Vec::new());
        let rows = table
            .lines()
            .skip_while(|l| !l.starts_with('|'))
            .take_while(|l| l.starts_with('|'));
        for row in rows {
            let Some(key) = row.strip_prefix("| `").and_then(|r| r.split_once('`')) else {
                continue;
            };
            match key.0.strip_prefix("mechanisms[].") {
                Some(k) => each.push(k.to_string()),
                None => top.push(key.0.to_string()),
            }
        }
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().expect("an object").keys().cloned().collect();
            k.sort();
            k
        };
        top.sort();
        each.sort();
        let mut mechanisms = unconfined(4);
        mechanisms.push(Mechanism::failed("engine", "", "", "could not run".into()));
        let (_, json) = report(4, &mechanisms, true);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(keys(&v), top, "{json}");
        for m in v["mechanisms"].as_array().unwrap() {
            assert_eq!(keys(m), each, "{m}");
            for c in m["checks"].as_array().unwrap() {
                assert_eq!(keys(c), ["allowed", "check", "must_be"], "{c}");
            }
        }
        assert!(
            table.contains(r#"`{"check", "allowed", "must_be"}`"#),
            "the README names a check's keys"
        );
        // And every verdict the report gives (`doctor_without_tcp_rules_says_partial`).
        let row = table
            .lines()
            .find(|l| l.starts_with("| `verdict`"))
            .expect("the verdict's row");
        for (abi, m) in [(4, confined(4)), (3, confined(3)), (0, unconfined(0))] {
            let (_, json) = report(abi, &m, true);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            let said = format!("`{}`", v["verdict"]);
            assert!(row.contains(&said), "{said} is not in the README: {row}");
        }
    }

    #[test]
    fn ceilings_that_cannot_be_enforced_are_refused_when_required() {
        // `--require-memory-ceilings` refuses wherever either ceiling is not enforced, and says
        // which and why; the platform is given, as only off Linux are they not.
        use super::ceilings_enforced;
        assert_eq!(ceilings_enforced(None, None), Ok(()));
        let e = ceilings_enforced(Some("no /proc"), None).unwrap_err();
        assert!(
            e.contains("--memory-ceiling is not enforced here: no /proc"),
            "{e}"
        );
        assert!(!e.contains("--loader-memory-ceiling"), "{e}");
        let e = ceilings_enforced(None, Some("no /proc")).unwrap_err();
        assert!(e.contains("--loader-memory-ceiling is not enforced"), "{e}");
        let e = ceilings_enforced(Some("a"), Some("b")).unwrap_err();
        assert!(e.starts_with("--require-memory-ceilings: "), "{e}");
        assert!(e.contains(": a;") && e.contains(": b."), "{e}");
        // and this platform is the one the flag is checked against
        assert_eq!(
            ceilings_enforced(
                benchy_mcp::supervisor::MEMORY_CEILING_UNENFORCED,
                benchy_mcp::supervisor::LOADER_MEMORY_CEILING_UNENFORCED
            )
            .is_ok(),
            cfg!(target_os = "linux")
        );
    }

    #[test]
    fn the_tcp_probe_counts_only_where_landlock_can_refuse_it() {
        // Landlock's TCP rules arrived with ABI 4 (Linux 6.7). Before that the filesystem is
        // confined exactly as intended, and a TCP connection is simply not Landlock's to refuse:
        // the doctor must not call that a failure of REQ-3.
        let tcp_probes = [
            "open a TCP connection",
            "connect to its source's port",
            "connect to another port",
            "listen on a port",
        ];
        for probes in [&ENGINE_PROBES[..], &LOADER_PROBES[..], &NETWORK_PROBES[..]] {
            for p in probes {
                let tcp = tcp_probes.contains(&p.0);
                assert_eq!(
                    expected(p, 3),
                    if tcp { None } else { Some(p.1) },
                    "{}",
                    p.0
                );
                assert_eq!(expected(p, 4), Some(p.1), "{}", p.0);
            }
        }
    }

    #[test]
    fn the_truncate_probes_count_only_where_landlock_can_refuse_it() {
        // truncate(2) became Landlock's to refuse with ABI 3 (Linux 6.2); before that the doctor
        // reports it and does not count it.
        let mut seen = 0;
        for probes in [&ENGINE_PROBES[..], &LOADER_PROBES[..], &NETWORK_PROBES[..]] {
            for p in probes.iter().filter(|p| p.0.starts_with("truncate")) {
                seen += 1;
                assert_eq!(expected(p, 2), None, "{}", p.0);
                assert_eq!(expected(p, 3), Some(p.1), "{}", p.0);
            }
        }
        assert_eq!(seen, 7, "every restriction has its truncate probes");
    }
}
