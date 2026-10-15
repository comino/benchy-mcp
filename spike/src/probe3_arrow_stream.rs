// REQ-6: Arrow in, batch by batch, never buffered whole.
use duckdb::arrow::array::{Float64Array, Int64Array, StringArray};
use duckdb::arrow::datatypes::{DataType, Field, Schema};
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::Connection;
use std::sync::Arc;
use std::time::Instant;

fn rss_mb(field: &str) -> f64 {
    std::fs::read_to_string("/proc/self/status").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(field)).map(|l| l.to_string()))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<f64>().ok()))
        .map(|kb| kb / 1024.0).unwrap_or(f64::NAN)
}

fn batch(schema: &Arc<Schema>, start: i64, rows: i64) -> RecordBatch {
    let ids: Int64Array = (start..start + rows).collect::<Vec<_>>().into();
    let vals: Float64Array = (start..start + rows).map(|i| i as f64 * 1.5).collect::<Vec<_>>().into();
    let tags: StringArray = (start..start + rows).map(|i| format!("entity-{:05}", i % 500)).collect::<Vec<_>>().into();
    RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(vals), Arc::new(tags)]).unwrap()
}

fn main() {
    const BATCHES: i64 = 400;
    const ROWS: i64 = 10_000;
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("v", DataType::Float64, false),
        Field::new("entity", DataType::Utf8, false),
    ]));

    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE loaded (id BIGINT, v DOUBLE, entity VARCHAR)").unwrap();

    let one = batch(&schema, 0, ROWS);
    let per_batch_mb = one.get_array_memory_size() as f64 / 1048576.0;
    let logical_mb = per_batch_mb * BATCHES as f64;
    let before = rss_mb("VmRSS:");
    println!("rows: {}  batches: {} x {}  per-batch {:.2} MB  logical total {:.0} MB",
        BATCHES * ROWS, BATCHES, ROWS, per_batch_mb, logical_mb);
    println!("RSS before ingest: {:.0} MB", before);

    let t = Instant::now();
    {
        let mut app = conn.appender("loaded").unwrap();
        for b in 0..BATCHES {
            // one batch alive at a time; it is dropped at the end of each iteration
            app.append_record_batch(batch(&schema, b * ROWS, ROWS)).unwrap();
        }
    }
    let secs = t.elapsed().as_secs_f64();

    let n: i64 = conn.query_row("SELECT count(*) FROM loaded", [], |r| r.get(0)).unwrap();
    let after = rss_mb("VmRSS:");
    let peak = rss_mb("VmHWM:");
    println!("ingested {} rows in {:.2}s ({:.0} k rows/s)", n, secs, (n as f64 / secs) / 1000.0);
    println!("RSS after: {:.0} MB   peak: {:.0} MB   growth: {:.0} MB vs {:.0} MB logical",
        after, peak, after - before, logical_mb);
    let overhead = (peak - before) / logical_mb;
    println!("peak growth / logical = {:.2}x", overhead);
    println!("VERDICT: {}", if overhead < 1.6 {
        "PASS - bounded; no whole-stream copy (the table itself must live in memory)"
    } else { "INVESTIGATE - peak suggests more than one copy" });

    // read side: streaming vs collecting
    let mut stmt = conn.prepare("SELECT entity, count(*) c, avg(v) a FROM loaded GROUP BY entity").unwrap();
    let t = Instant::now();
    let mut rows = 0usize;
    for rb in stmt.stream_arrow([]).unwrap() { rows += rb.num_rows(); }
    println!("stream_arrow: {} groups in {:.2}s (streaming, not collected)", rows, t.elapsed().as_secs_f64());
}
