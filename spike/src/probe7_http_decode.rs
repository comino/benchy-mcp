// docs/HTTP-SOURCE.md: which decoder is fastest for each format an HTTP source may send.
//
// One mode per run, so `perf stat -e instructions:u` measures one decoder:
//
//   probe7_http_decode gen <readings.csv> <outdir>    write the fixtures below from the CSV
//   probe7_http_decode <mode> <file> [--duck]          decode; --duck also appends into DuckDB
//
// Modes: baseline (read the file and nothing else), ipc, ndjson-arrowjson, ndjson-serde,
// ndjson-simd, pages-arrowjson, pages-serde, pages-simd, duck-csv, duck-ndjson, duck-parquet.
// DuckDB runs with `threads = 1` so its instructions compare with the single-threaded decoders.
use duckdb::arrow::array::{
    Array, ArrayRef, Float64Builder, ListArray, StringBuilder, StructArray,
    TimestampMicrosecondBuilder,
};
use duckdb::arrow::datatypes::{DataType, Field, Fields, Schema, TimeUnit};
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::Connection;
use std::io::Write;
use std::sync::Arc;

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        Field::new("meter", DataType::Utf8, true),
        Field::new("kw", DataType::Float64, true),
    ]))
}

const COLUMNS: &str = "{'ts': 'TIMESTAMP', 'meter': 'VARCHAR', 'kw': 'DOUBLE'}";
const PAGE_ROWS: usize = 10_000;

fn sink() -> Option<Connection> {
    if !std::env::args().any(|a| a == "--duck") {
        return None;
    }
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("SET threads = 1; CREATE TABLE t (ts TIMESTAMP, meter VARCHAR, kw DOUBLE)")
        .unwrap();
    Some(c)
}

struct Out {
    duck: Option<Connection>,
    rows: usize,
}

impl Out {
    fn push(&mut self, b: RecordBatch) {
        self.rows += b.num_rows();
        if let Some(c) = &self.duck {
            c.appender("t").unwrap().append_record_batch(b).unwrap();
        }
    }
}

/// Column builders for the declared schema, fed one value at a time: what a generic
/// serde_json/simd-json decoder has to do, since the schema is the operator's, known at runtime.
struct Builders {
    ts: TimestampMicrosecondBuilder,
    meter: StringBuilder,
    kw: Float64Builder,
    n: usize,
}

impl Builders {
    fn new() -> Self {
        Self {
            ts: TimestampMicrosecondBuilder::with_capacity(65_536),
            meter: StringBuilder::with_capacity(65_536, 65_536 * 8),
            kw: Float64Builder::with_capacity(65_536),
            n: 0,
        }
    }
    fn ts(&mut self, s: Option<&str>) {
        self.ts.append_option(s.map(|s| {
            duckdb::arrow::compute::kernels::cast_utils::string_to_timestamp_nanos(s).unwrap()
                / 1000
        }));
    }
    fn flush(&mut self, out: &mut Out, force: bool) {
        if !force {
            self.n += 1;
        }
        if self.n >= 65_536 || (force && self.n > 0) {
            let cols: Vec<ArrayRef> = vec![
                Arc::new(self.ts.finish()),
                Arc::new(self.meter.finish()),
                Arc::new(self.kw.finish()),
            ];
            out.push(RecordBatch::try_new(schema(), cols).unwrap());
            self.n = 0;
        }
    }
}

fn serde_row(b: &mut Builders, v: &serde_json::Value) {
    b.ts(v.get("ts").and_then(|x| x.as_str()));
    b.meter.append_option(v.get("meter").and_then(|x| x.as_str()));
    b.kw.append_option(v.get("kw").and_then(|x| x.as_f64()));
}

fn simd_row(b: &mut Builders, v: &simd_json::BorrowedValue) {
    use simd_json::prelude::*;
    b.ts(v.get("ts").and_then(|x| x.as_str()));
    b.meter.append_option(v.get("meter").and_then(|x| x.as_str()));
    b.kw.append_option(v.get("kw").and_then(|x| x.cast_f64()));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args[1].as_str();
    if mode == "gen" {
        return gen(&args[2], &args[3]);
    }
    let path = &args[2];
    let mut out = Out {
        duck: if mode.starts_with("duck-") {
            None
        } else {
            sink()
        },
        rows: 0,
    };
    let t = std::time::Instant::now();
    match mode {
        "baseline" => out.rows = std::fs::read(path).unwrap().len(),
        "ipc" => {
            let f = std::io::BufReader::new(std::fs::File::open(path).unwrap());
            for b in duckdb::arrow::ipc::reader::StreamReader::try_new(f, None).unwrap() {
                out.push(b.unwrap());
            }
        }
        "ndjson-arrowjson" => {
            let f = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path).unwrap());
            let r = arrow_json::ReaderBuilder::new(schema())
                .with_batch_size(65_536)
                .build(f)
                .unwrap();
            for b in r {
                out.push(b.unwrap());
            }
        }
        "ndjson-serde" => {
            let data = std::fs::read(path).unwrap();
            let mut b = Builders::new();
            for line in data.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
                let v: serde_json::Value = serde_json::from_slice(line).unwrap();
                serde_row(&mut b, &v);
                b.flush(&mut out, false);
            }
            b.flush(&mut out, true);
        }
        "ndjson-simd" => {
            let mut data = std::fs::read(path).unwrap();
            let mut b = Builders::new();
            for line in data.split_mut(|&c| c == b'\n').filter(|l| !l.is_empty()) {
                let v = simd_json::to_borrowed_value(line).unwrap();
                simd_row(&mut b, &v);
                b.flush(&mut out, false);
            }
            b.flush(&mut out, true);
        }
        "pages-arrowjson" => {
            // The whole page is one value of {data: [row], next}: arrow-json decodes the rows
            // straight into a list of structs, and the struct's children are the columns.
            let row = Fields::from(schema().fields().clone());
            let page = Arc::new(Schema::new(vec![
                Field::new(
                    "data",
                    DataType::List(Arc::new(Field::new("item", DataType::Struct(row), true))),
                    true,
                ),
                Field::new("next", DataType::Utf8, true),
            ]));
            let data = std::fs::read(path).unwrap();
            let mut dec = arrow_json::ReaderBuilder::new(page).build_decoder().unwrap();
            for line in data.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
                dec.decode(line).unwrap();
                let one = dec.flush().unwrap().unwrap();
                let list = one.column(0).as_any().downcast_ref::<ListArray>().unwrap();
                let rows = list.values().as_any().downcast_ref::<StructArray>().unwrap();
                out.push(RecordBatch::from(rows.clone()));
            }
        }
        "pages-serde" => {
            let data = std::fs::read(path).unwrap();
            let mut b = Builders::new();
            for line in data.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
                let v: serde_json::Value = serde_json::from_slice(line).unwrap();
                for r in v["data"].as_array().unwrap() {
                    serde_row(&mut b, r);
                    b.flush(&mut out, false);
                }
            }
            b.flush(&mut out, true);
        }
        "pages-simd" => {
            use simd_json::prelude::*;
            let mut data = std::fs::read(path).unwrap();
            let mut b = Builders::new();
            for line in data.split_mut(|&c| c == b'\n').filter(|l| !l.is_empty()) {
                let v = simd_json::to_borrowed_value(line).unwrap();
                for r in v.get("data").and_then(|d| d.as_array()).unwrap() {
                    simd_row(&mut b, r);
                    b.flush(&mut out, false);
                }
            }
            b.flush(&mut out, true);
        }
        "duck-csv" | "duck-ndjson" | "duck-parquet" | "duck-pages" | "duck-ndjson-auto"
        | "duck-pages-auto" => {
            let c = Connection::open_in_memory().unwrap();
            c.execute_batch("SET threads = 1").unwrap();
            let p = path.replace('\'', "''");
            let reader = match mode {
                "duck-csv" => format!("read_csv('{p}', header = true, columns = {COLUMNS})"),
                "duck-ndjson" => format!("read_ndjson('{p}', columns = {COLUMNS})"),
                "duck-ndjson-auto" => format!("read_ndjson('{p}')"),
                // Each page one row; its rows a list of structs, unnested in order.
                "duck-pages" => format!(
                    "(SELECT unnest(r, recursive := true) FROM (SELECT unnest(data) AS r FROM \
                     read_ndjson('{p}', columns = {{'data': 'STRUCT(ts TIMESTAMP, meter VARCHAR, \
                     kw DOUBLE)[]'}})))"
                ),
                "duck-pages-auto" => format!(
                    "(SELECT unnest(r, recursive := true) FROM (SELECT unnest(data) AS r FROM \
                     read_ndjson('{p}')))"
                ),
                _ => format!("read_parquet('{p}')"),
            };
            c.execute_batch(&format!("CREATE TABLE t AS SELECT * FROM {reader}"))
                .unwrap();
            out.rows = c
                .query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0))
                .unwrap() as usize;
            let mut st = c
                .prepare("SELECT column_name || ' ' || data_type FROM duckdb_columns() WHERE table_name = 't'")
                .unwrap();
            let cols: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(|x| x.unwrap()).collect();
            eprintln!("columns: {}", cols.join(", "));
        }
        other => panic!("unknown mode {other}"),
    }
    eprintln!(
        "{mode}{}: {} rows in {:.3}s",
        if out.duck.is_some() { " +duck" } else { "" },
        out.rows,
        t.elapsed().as_secs_f64()
    );
}

fn gen(csv: &str, dir: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let c = Connection::open_in_memory().unwrap();
    let csv = csv.replace('\'', "''");
    c.execute_batch(&format!(
        "CREATE TABLE r AS SELECT * FROM read_csv('{csv}', header = true, columns = {COLUMNS});
         COPY r TO '{dir}/readings.csv' (HEADER);
         COPY r TO '{dir}/readings.parquet' (FORMAT parquet);
         COPY (SELECT strftime(ts, '%Y-%m-%dT%H:%M:%S') AS ts, meter, kw FROM r)
           TO '{dir}/readings.ndjson' (FORMAT json);"
    ))
    .unwrap();
    // Arrow IPC stream, as a connector or an HTTP endpoint would send it.
    let mut st = c.prepare("SELECT * FROM r").unwrap();
    let batches: Vec<RecordBatch> = st.query_arrow([]).unwrap().collect();
    let f = std::fs::File::create(format!("{dir}/readings.arrows")).unwrap();
    let mut w = duckdb::arrow::ipc::writer::StreamWriter::try_new(f, &batches[0].schema()).unwrap();
    for b in &batches {
        w.write(b).unwrap();
    }
    w.finish().unwrap();
    // JSON pages of PAGE_ROWS rows, one page per line: {"data": [...], "next": "..."}.
    let nd = std::fs::read_to_string(format!("{dir}/readings.ndjson")).unwrap();
    let lines: Vec<&str> = nd.lines().collect();
    let mut f = std::io::BufWriter::new(std::fs::File::create(format!("{dir}/pages.jsonl")).unwrap());
    for (i, chunk) in lines.chunks(PAGE_ROWS).enumerate() {
        write!(f, "{{\"data\":[{}],\"next\":\"cursor-{}\"}}\n", chunk.join(","), i + 1).unwrap();
    }
    let _ = Fields::empty();
    let _ = |a: &dyn Array| a.len();
}
