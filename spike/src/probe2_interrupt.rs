// REQ-8: a runaway query must lose the query, not the bench.
// Case A: execution-phase runaway -> interrupt should cancel it and leave the workspace intact.
// Case B: planner-phase blowup    -> if interrupt CANNOT stop it, REQ-8's out-of-process kill
//                                    is required, not merely prudent.
use duckdb::Connection;
use std::thread;
use std::time::{Duration, Instant};

fn peak_rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM:")).map(|l| l.to_string()))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        .map(|kb| kb / 1024.0).unwrap_or(f64::NAN)
}

fn nested_map(depth: usize) -> String {
    let mut ctes = vec!["l0 AS (SELECT 1 m)".to_string()];
    for i in 1..=depth {
        ctes.push(format!("l{i} AS (SELECT MAP([m],[m]) m FROM l{})", i - 1));
    }
    format!("WITH {} SELECT m FROM l{depth}", ctes.join(", "))
}

fn main() {
    println!("=== Case A: execution-phase runaway (recursive CTE), 2s deadline ===");
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("SET memory_limit='512MB'; CREATE TABLE loaded AS SELECT range AS a FROM range(1000);").unwrap();
        let h = conn.interrupt_handle();
        let t = Instant::now();
        thread::spawn(move || { thread::sleep(Duration::from_secs(2)); h.interrupt(); });
        let r = conn.execute_batch(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t) SELECT sum(n) FROM t");
        println!("  query returned after {:>6.2}s -> {}", t.elapsed().as_secs_f64(),
            match &r { Ok(_) => "Ok (unexpected)".into(), Err(e) => format!("{}", e).lines().next().unwrap_or("").to_string() });
        // the workspace must have survived
        let n: i64 = conn.query_row("SELECT count(*) FROM loaded", [], |r| r.get(0)).unwrap();
        let two: i64 = conn.query_row("SELECT 1+1", [], |r| r.get(0)).unwrap();
        println!("  connection still usable: SELECT 1+1 = {two}");
        println!("  loaded table intact:     count = {n}");
        println!("  VERDICT: {}", if n == 1000 && two == 2 { "PASS - query lost, bench survived" } else { "FAIL" });
    }

    println!("\n=== Case B: planner-phase blowup (nested MAP, depth 18), 2s deadline ===");
    {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("SET memory_limit='512MB';").unwrap();
        let h = conn.interrupt_handle();
        let t = Instant::now();
        thread::spawn(move || { thread::sleep(Duration::from_secs(2)); h.interrupt(); });
        let sql = nested_map(18);
        let r = conn.prepare(&sql);           // the blowup happens HERE, inside prepare
        let elapsed = t.elapsed().as_secs_f64();
        println!("  prepare() returned after {:>6.2}s -> {}", elapsed,
            match &r { Ok(_) => "Ok".into(), Err(e) => format!("{}", e).lines().next().unwrap_or("").to_string() });
        println!("  peak RSS: {:.0} MB (memory_limit was 512MB)", peak_rss_mb());
        println!("  VERDICT: {}", if elapsed > 3.0 {
            "the 2s interrupt did NOT stop planning -> out-of-process kill REQUIRED (REQ-8 layer 2)"
        } else { "planning finished before the deadline; raise depth to reproduce" });
    }
}
