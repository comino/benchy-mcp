//! REQ-7 `ceilings_enforced` and `neighbour_unaffected`, driven through the real MCP surface.
//!
//! These drive the shipped binary rather than the library, because a ceiling that is enforced in
//! `Workspace` and not wired to a CLI flag is a ceiling the operator does not have. Every
//! assertion here is about what a stranger gets from `benchy-mcp --memory 128MB`, not about what
//! a struct field does.

mod common;
use common::{data_dir, median, Server};
use std::time::Duration;

/// A runaway that the in-process interrupt *can* stop, so it exercises the deadline rather than
/// REQ-8's kill. The planner blowup that no timer can reach is covered by `examples/verify.py`.
const RUNAWAY: &str =
    "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t) SELECT sum(n) FROM t";

#[test]
fn ceilings_enforced() {
    let dir = data_dir();
    let mut s = Server::start(
        &dir,
        &[
            "--max-rows",
            "5",
            "--max-bytes",
            "600",
            "--deadline",
            "2",
            "--hard-grace",
            "4",
            "--memory",
            "128MB",
        ],
    );

    // 1. rows. Not an error — a bounded answer that says it is bounded, which is REQ-10's shape.
    let (err, body, _) = s.query("SELECT i FROM range(1000) t(i)");
    assert!(!err, "a capped result is an answer, not a failure: {body}");
    assert!(body.contains("TRUNCATED"), "says it was capped: {body}");
    assert!(
        body.matches('\n').count() < 20,
        "and really did stop at 5 rows: {body}"
    );
    assert!(s.alive(), "host survives the row cap");

    // 2. bytes — on a server where the row cap cannot be what cuts, or this leg would pass on
    // the row cap alone. 200 short rows are about 4 KB; the response has to stop inside 600.
    let mut b = Server::start(&dir, &["--max-rows", "1000", "--max-bytes", "600"]);
    let (err, body, _) = b.query("SELECT 'row-' || i::VARCHAR AS r FROM range(200) t(i)");
    assert!(!err, "{body}");
    let wire = serde_json::to_string(&body).unwrap().len() - 2;
    assert!(wire <= 600, "the byte cap holds on the wire: {wire} bytes");
    let rows = body.lines().filter(|l| l.starts_with("row-")).count();
    assert!(
        (1..200).contains(&rows),
        "it cut rows, not the whole answer: {rows} rows shown"
    );
    assert!(body.contains("TRUNCATED"), "and says it was capped: {body}");
    assert!(b.alive(), "host survives the byte cap");

    // 3. memory. DuckDB's own budget, named in the error, with REQ-11's fix attached.
    let (err, body, _) = s.query(
        "SELECT count(*) FROM (SELECT i, repeat('x',400)||i::VARCHAR AS s \
         FROM range(3000000) t(i) ORDER BY s) q",
    );
    assert!(err, "the memory ceiling refuses: {body}");
    assert!(body.contains("Out of Memory"), "names the ceiling: {body}");
    assert!(body.contains("--memory"), "says how to get more: {body}");
    assert!(s.alive(), "host survives the memory ceiling");

    // 4. wall deadline. The batch dies; the bench does not.
    s.query("CREATE TABLE scratch AS SELECT 1 AS n");
    let (err, body, took) = s.query(RUNAWAY);
    assert!(err, "the deadline refuses: {body}");
    assert!(body.contains("deadline"), "names the ceiling: {body}");
    assert!(
        took < Duration::from_secs(12),
        "stopped near its 2s deadline, not eventually: took {took:?}"
    );
    assert!(s.alive(), "host survives the deadline");

    // The deadline is the soft path, so the scratch survives it. That distinction is the whole
    // reason `--hard-grace` sits above `--deadline` rather than replacing it.
    let (err, body, _) = s.query("SELECT n FROM scratch");
    assert!(
        !err && body.contains('1'),
        "an interrupted batch keeps your tables: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn neighbour_unaffected() {
    // One bench per process (REQ-1), so "another session" is another process. What must hold is
    // that A's runaway cannot reach B's bench, B's results, or B's liveness.
    //
    // It is *not* a claim that B sees no slowdown. A and B compete for CPU like any two
    // processes, and on a two-core box a saturating neighbour is visible. The honest statement,
    // and the one asserted here, is that B stays correct and responsive — bounded by its own
    // ceilings, never by A's failure.
    let dir = data_dir();
    let mut a = Server::start(&dir, &["--deadline", "6", "--hard-grace", "4"]);
    let mut b = Server::start(&dir, &["--deadline", "6", "--hard-grace", "4"]);

    b.query("CREATE TABLE b_only AS SELECT 'mine' AS who");

    let probe = "SELECT count(*) c, avg(power_kw) a FROM readings";
    let quiet = median(&mut b, probe, 5);

    let wedged = std::thread::spawn(move || {
        let (err, body, _) = a.query(RUNAWAY);
        (err, body, a)
    });
    std::thread::sleep(Duration::from_millis(400)); // let A get properly into it

    let busy = median(&mut b, probe, 5);
    let (err, body, _) = b.query(probe);
    assert!(!err, "B still answers while A is wedged: {body}");
    assert!(body.contains("500"), "and answers correctly: {body}");
    let (err, body, _) = b.query("SELECT who FROM b_only");
    assert!(
        !err && body.contains("mine"),
        "B's own scratch is untouched by A's failure: {body}"
    );
    assert!(
        busy < Duration::from_secs(2),
        "B stayed responsive: {quiet:?} quiet, {busy:?} with a runaway neighbour"
    );

    let (err, body, mut a) = wedged.join().expect("A's thread");
    assert!(
        err && body.contains("deadline"),
        "A hit its own wall: {body}"
    );
    assert!(a.alive(), "and A itself survived it");

    println!("neighbour: B median {quiet:?} quiet, {busy:?} while A ran away");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_trivial_query_does_not_pay_for_the_watchdog() {
    // REQ-6. The deadline watchdog is joined at the end of every batch. While it slept in 20ms
    // ticks, that join waited out the rest of the current tick, so *every* query — `SELECT 1`
    // included — paid up to a full tick before returning.
    //
    // It hid in plain sight: `examples/capacity.py` reported 0.022s for `count(*)` at 100k rows,
    // at 1M and at 5M, and `neighbour_unaffected` measured 22.525ms quiet against 22.524ms under
    // a runaway. A number that does not move with the work is not measuring the work.
    //
    // Judged against the same round trip with no watchdog, measured in the same run. A statement
    // the allowlist refuses goes client → server → engine → classifier and back, as `SELECT 1`
    // does, and `Workspace::work` returns it before the watchdog starts. A fixed bound measured
    // the machine as well as the watchdog: `SELECT 1` < 10 ms failed at 10.08 ms under load,
    // where the round trip itself had grown.
    //
    // So the two alternate, 60 pairs, and the test takes the lower quartile of what `SELECT 1`
    // cost more than the refused statement before it. A tick is paid by nearly every `SELECT 1`,
    // so the quartile is the tick: 20.3–20.4 ms on the 20 ms tick, ten runs of ten, load average
    // 5 to 8. The load is in both halves of a pair: with 16 busy loops beside the test on four
    // cores, the quartile stayed under 1.9 ms in 20 runs (the fixed bound failed 11 of 20 there),
    // where the median of the differences reached 4.7 ms, the cost of starting and joining a
    // thread on a full machine. A machine that full can also hide a tick: the watchdog then
    // starts after `SELECT 1` has finished, and so does not wait.
    let dir = data_dir();
    let mut s = Server::start(&dir, &[]);
    let mut extra: Vec<f64> = (0..60)
        .map(|_| {
            let (refused, body, bare) = s.query("ATTACH ':memory:' AS elsewhere");
            assert!(
                refused && body.contains("ATTACH"),
                "the round trip without a watchdog is a statement refused before it runs: {body}"
            );
            let (err, body, trivial) = s.query("SELECT 1");
            assert!(!err, "{body}");
            trivial.as_secs_f64() - bare.as_secs_f64()
        })
        .collect();
    extra.sort_by(f64::total_cmp);
    let quartile = Duration::from_secs_f64(extra[extra.len() / 4].max(0.0));
    assert!(
        quartile < Duration::from_millis(5),
        "a trivial query should cost the round trip, not a watchdog tick: three quarters of 60 \
         `SELECT 1` cost {quartile:?} or more beyond a statement refused before the watchdog"
    );
    println!("watchdog: SELECT 1 beyond a refused statement, lower quartile of 60: {quartile:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
