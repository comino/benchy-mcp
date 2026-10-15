#!/usr/bin/env python3
"""REQ-16, measured: what a ClickHouse table costs to put on the bench, six ways.

    python3 examples/capacity_clickhouse.py ./target/release/benchy-mcp \
        --url http://127.0.0.1:8123 [--meters eval/uci/readings.csv] [--rows 10000000] [--runs 3]

For each table — the eval's public meter data when `--meters` is given, and `--rows` synthetic
rows of examples/capacity.py's shape made by ClickHouse itself with numbers() — it starts benchy
six ways and measures, from start to the answer to the first `describe`: the built-in source
with each `compression` (zstd on the wire, Arrow's LZ4, none), an exec connector that pipes `curl`'s
ArrowStream through, and CSV and Parquet exports of the same rows as files. It reports wall
time, CPU time (user and system, of the server and every process it reaped), the session's peak
RSS (server, engine and loader together, sampled every 20 ms), the largest single process's peak
RSS as the kernel recorded it (at load, the loader's), and the bytes that crossed the network.
`--runs N` runs each path N times and reports the median of each figure. It creates the
database `benchy_capacity` on that server and drops it at the end. Standard library only.
"""
import argparse, json, os, shutil, stat, subprocess, sys, tempfile, threading, time
import urllib.parse, urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from capacity import PeakRss  # noqa: E402

DB = "benchy_capacity"


def ch(url, sql, data=None):
    """Run `sql`; with `data`, the SQL goes on the URL and the data is the body."""
    if data is None:
        q = urllib.request.Request(url + "/", data=sql.encode(), method="POST")
    else:
        q = urllib.request.Request(
            url + "/?date_time_input_format=best_effort&query=" + urllib.parse.quote(sql),
            data=data, method="POST")
    with urllib.request.urlopen(q) as r:
        return r.read()


def setup(url, meters, rows):
    ch(url, f"DROP DATABASE IF EXISTS {DB}")
    ch(url, f"CREATE DATABASE {DB}")
    tables = []
    if meters:
        ch(url, f"CREATE TABLE {DB}.meters (ts DateTime, meter LowCardinality(String), "
                "kw Float32) ENGINE = MergeTree ORDER BY (meter, ts)")
        with open(meters, "rb") as fh:
            ch(url, f"INSERT INTO {DB}.meters FORMAT CSVWithNames", data=fh.read())
        tables.append("meters")
    if rows:
        ch(url, f"CREATE TABLE {DB}.synthetic (observed_at DateTime, entity LowCardinality(String), "
                "value Float64, note String) ENGINE = MergeTree ORDER BY (entity, observed_at)")
        ch(url, f"INSERT INTO {DB}.synthetic SELECT toDateTime('2026-01-01 00:00:00', 'UTC') + "
                f"number * 60, concat('entity-', leftPad(toString(number % 500), 4, '0')), "
                f"round((number % 997) * 1.5, 2), concat('n', toString(number % 13)) "
                f"FROM numbers({rows})")
        tables.append("synthetic")
    return tables


def columns(url, table):
    out = ch(url, f"DESCRIBE TABLE {DB}.{table} FORMAT JSONEachRow").decode()
    return [(r["name"], r["type"]) for r in map(json.loads, out.splitlines())]


def run(binary, args, env=None):
    """Start benchy, answer `describe`, and return (seconds, peak RSS in kB, stderr, CPU seconds,
    largest process's peak RSS in kB). The last two are the kernel's, read as benchy is reaped:
    user and system time of the server and every child it reaped — the loader and the engine —
    and the highest resident set any one of them reached, which is the loader's at load."""
    t0 = time.time()
    p = subprocess.Popen([binary, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, bufsize=1,
                         env={**os.environ, **(env or {})})
    peak = PeakRss(p.pid)
    err = []
    threading.Thread(target=lambda: err.extend(p.stderr), daemon=True).start()
    n = [0]

    def call(method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            n[0] += 1
            msg["id"] = n[0]
        p.stdin.write(json.dumps(msg) + "\n")
        p.stdin.flush()
        if notify:
            return None
        while True:
            line = p.stdout.readline()
            if not line:
                time.sleep(0.2)
                raise SystemExit("benchy stopped:\n" + "".join(err))
            m = json.loads(line)
            if m.get("id") == msg["id"]:
                return m

    call("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                        "clientInfo": {"name": "capacity-clickhouse", "version": "0"}})
    call("notifications/initialized", notify=True)
    call("tools/call", {"name": "describe", "arguments": {}})
    took = time.time() - t0
    rss = peak.stop()
    p.stdin.close()
    _, _, usage = os.wait4(p.pid, 0)
    p.returncode = 0
    cpu = usage.ru_utime + usage.ru_stime
    return took, rss, "".join(err), cpu, usage.ru_maxrss


def wire(log):
    for line in log.splitlines():
        if "on the wire" in line:
            # benchy: source `x`: N row(s), A byte(s) as Arrow, W byte(s) on the wire, in S s
            parts = line.split(", ")
            return int(parts[1].split()[0]), int(parts[2].split()[0])
    return None, None


def median_run(binary, args, runs):
    """`run`, `runs` times: the median of each figure, and the log of the last."""
    got = [run(binary, args) for _ in range(runs)]
    mid = lambda i: sorted(g[i] for g in got)[len(got) // 2]
    return mid(0), mid(1), got[-1][2], mid(3), mid(4)


def export(url, table, work, fmt, chfmt, query=""):
    """The table as a file of its own, exported by ClickHouse."""
    d = os.path.join(work, f"{table}-{fmt}")
    os.makedirs(d)
    path = os.path.join(d, f"{table}.{fmt}")
    with open(path, "wb") as fh:
        q = urllib.request.Request(url + "/" + query,
                                   data=f"SELECT * FROM {DB}.{table} FORMAT {chfmt}".encode(),
                                   method="POST")
        with urllib.request.urlopen(q) as r:
            shutil.copyfileobj(r, fh)
    return d, os.path.getsize(path)


def measure(binary, url, table, work, runs):
    cols = columns(url, table)
    caps = "max_rows = 50_000_000\nmax_bytes = 2_000_000_000\n"
    out = {}
    for compression in ("zstd", "lz4", "none"):
        cfg = os.path.join(work, f"{table}-{compression}.toml")
        with open(cfg, "w") as fh:
            fh.write(f'[[source]]\ntype = "clickhouse"\nurl = "{url}"\ndatabase = "{DB}"\n'
                     f'tables = ["{table}"]\ncompression = "{compression}"\n{caps}')
        took, rss, log, cpu, top = median_run(binary, ["--config", cfg], runs)
        arrow, sent = wire(log)
        out[f"built-in, compression = {compression}"] = (took, rss, cpu, top, sent)
        if compression == "none":
            out["arrow"] = arrow
    # An exec connector: curl forwards ClickHouse's ArrowStream, the same SELECT benchy makes.
    exprs = []
    schema = ""
    for name, ty in cols:
        if ty.startswith("DateTime"):
            exprs.append(f"toDateTime64({name}, 6, 'UTC') AS {name}")
            schema += f'[[source.schema]]\nname = "{name}"\ntype = "timestamptz"\n'
        elif "String" in ty:
            exprs.append(f"CAST({name} AS String) AS {name}")
            schema += f'[[source.schema]]\nname = "{name}"\ntype = "string"\n'
        else:
            exprs.append(f"CAST({name} AS Float64) AS {name}")
            schema += f'[[source.schema]]\nname = "{name}"\ntype = "float64"\n'
    sql = (f"SELECT {', '.join(exprs)} FROM {DB}.{table} FORMAT ArrowStream "
           "SETTINGS output_format_arrow_compression_method='none'")
    conn = os.path.join(work, f"{table}.sh")
    with open(conn, "w") as fh:
        fh.write("#!/bin/sh\ncase \"$1\" in\n"
                 "  describe) echo '{\"protocol\":[1],\"name\":\"curl\",\"version\":\"0\","
                 "\"narrowing\":{\"time\":\"unsupported\",\"entity\":\"unsupported\","
                 "\"columns\":\"unsupported\",\"params\":\"unsupported\"}}' ;;\n"
                 f"  fetch) exec curl -sfS '{url}/' --data-binary \"{sql}\" ;;\n"
                 "  *) exit 2 ;;\nesac\n")
    os.chmod(conn, stat.S_IRWXU)
    cfg = os.path.join(work, f"{table}-exec.toml")
    with open(cfg, "w") as fh:
        fh.write(f'[[source]]\nname = "{table}"\ntype = "exec"\ncommand = "{conn}"\n{caps}{schema}')
    took, rss, _, cpu, top = median_run(binary, ["--config", cfg], runs)
    out["exec connector (curl)"] = (took, rss, cpu, top, out["arrow"])
    # The same rows as files: a CSV with ISO times in UTC, and a Parquet file.
    for name, fmt, chfmt, query in [("CSV file", "csv", "CSVWithNames",
                                     "?date_time_output_format=iso"),
                                    ("Parquet file", "parquet", "Parquet", "")]:
        d, size = export(url, table, work, fmt, chfmt, query)
        took, rss, _, cpu, top = median_run(binary, [d], runs)
        out[name] = (took, rss, cpu, top, size)
        shutil.rmtree(d, ignore_errors=True)
    return out


def main():
    a = argparse.ArgumentParser()
    a.add_argument("binary")
    a.add_argument("--url", default="http://127.0.0.1:8123")
    a.add_argument("--meters")
    a.add_argument("--rows", type=int, default=10_000_000)
    a.add_argument("--runs", type=int, default=1, help="runs per path; each figure is the median")
    o = a.parse_args()
    tables = setup(o.url, o.meters, o.rows)
    work = tempfile.mkdtemp(prefix="benchy-chcap-")
    try:
        print("| table | rows | path | start → first `describe` | CPU | peak RSS, session "
              "| peak RSS, largest process | bytes moved |")
        print("|---|---:|---|---:|---:|---:|---:|---:|")
        for t in tables:
            n = int(ch(o.url, f"SELECT count() FROM {DB}.{t}"))
            res = measure(o.binary, o.url, t, work, o.runs)
            res.pop("arrow")
            for path, (took, rss, cpu, top, moved) in res.items():
                what = "on disk" if path.endswith("file") else "on the wire"
                print(f"| {t} | {n:,} | {path} | {took:.2f} s | {cpu:.2f} s | "
                      f"{rss / 1024:,.0f} MB | {top / 1024:,.0f} MB | "
                      f"{moved / 1e6:.1f} MB {what} |", flush=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)
        ch(o.url, f"DROP DATABASE IF EXISTS {DB}")


if __name__ == "__main__":
    main()
