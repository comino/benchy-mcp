#!/usr/bin/env python3
"""REQ-7, capacity: how much does the bench actually hold, and what does it cost?

    python3 examples/capacity.py ./target/release/benchy-mcp [--out docs/CAPACITY.md]

Measured on the machine you run it on. No capacity claim belongs in any benchy document that is
not output from this.
"""
import csv, json, os, shutil, subprocess, sys, tempfile, threading, time

def gen(path, rows):
    with open(path, "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["observed_at", "entity", "value", "note"])
        base = 1767225600  # 2026-01-01T00:00:00Z
        for i in range(rows):
            w.writerow([
                time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(base + i * 60)),
                f"entity-{i % 500:04d}", round((i % 997) * 1.5, 2), f"n{i % 13}",
            ])

def tree(pid):
    """`pid` and every process below it: the server, its engine, and a loader while one runs."""
    parent = {}
    for e in os.listdir("/proc"):
        if e.isdigit():
            try:
                with open(f"/proc/{e}/stat") as fh:
                    parent[int(e)] = int(fh.read().rsplit(")", 1)[1].split()[1])
            except (OSError, IndexError, ValueError):
                pass
    found, i = [pid], 0
    while i < len(found):
        found += [c for c, p in parent.items() if p == found[i]]
        i += 1
    return found

def rss_kb(pids):
    total = 0
    for p in pids:
        try:
            with open(f"/proc/{p}/status") as fh:
                for line in fh:
                    if line.startswith("VmRSS:"):
                        total += int(line.split()[1])
        except OSError:
            pass
    return total

class PeakRss:
    """The session's memory at its highest, sampled every 20 ms across the whole process tree.
    Loaders parse the data and exit before the queries run, so a high-water mark read at the end
    would miss the moment memory is highest."""
    def __init__(self, pid):
        self.pid, self.peak, self.done = pid, 0, False
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()
    def run(self):
        while not self.done:
            self.peak = max(self.peak, rss_kb(tree(self.pid)))
            time.sleep(0.02)
    def stop(self):
        self.done = True
        self.thread.join()
        return self.peak

def measure(binary, data_dir, rows):
    p = subprocess.Popen([binary, data_dir], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, bufsize=1)
    peak = PeakRss(p.pid)
    t0 = time.time()
    sealed = None
    while True:
        line = p.stderr.readline()
        if not line:
            raise SystemExit("server died during load:\n" + p.stderr.read())
        if "sealed" in line:
            sealed = time.time() - t0
            break
    n = [0]
    def call(method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None: msg["params"] = params
        if not notify:
            n[0] += 1; msg["id"] = n[0]
        p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
        if notify: return
        while True:
            line = p.stdout.readline()
            if not line: raise SystemExit("pipe closed")
            m = json.loads(line)
            if m.get("id") == msg["id"]: return m

    call("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                        "clientInfo": {"name": "capacity", "version": "0"}})
    call("notifications/initialized", notify=True)

    def timed(sql):
        t = time.time()
        r = call("tools/call", {"name": "query", "arguments": {"sql": sql}})
        body = "".join(c.get("text", "") for c in r["result"].get("content", []))
        if r["result"].get("isError"):
            raise SystemExit(f"query failed: {body[:200]}")
        return time.time() - t

    scan = timed("SELECT count(*) FROM big")
    group = timed("SELECT entity, count(*) c, avg(value) a FROM big GROUP BY 1 ORDER BY c DESC LIMIT 20")
    window = timed("SELECT entity, value, row_number() OVER (PARTITION BY entity ORDER BY observed_at) rn FROM big QUALIFY rn <= 2 LIMIT 50")
    rss = peak.stop()
    p.stdin.close(); p.wait(timeout=20)
    return sealed, scan, group, window, rss

def machine():
    import platform
    mem = "?"
    try:
        with open("/proc/meminfo") as fh:
            for line in fh:
                if line.startswith("MemTotal:"):
                    mem = f"{int(line.split()[1]) / 1048576:.1f} GB RAM"
    except OSError:
        pass
    return f"{platform.system()} {platform.machine()}, {os.cpu_count()} cores, {mem}"

def main(binary, out):
    rows_out = []
    print(f"{'rows':>10} {'file':>9} {'load':>8} {'rows/s':>10} {'count':>8} {'group by':>9} "
          f"{'window':>8} {'peak RSS':>10}")
    print("-" * 78)
    for rows in (100_000, 1_000_000, 5_000_000):
        d = tempfile.mkdtemp(prefix="benchy-cap-")
        try:
            f = os.path.join(d, "big.csv")
            gen(f, rows)
            size = os.path.getsize(f)
            load, scan, group, window, rss = measure(binary, d, rows)
        finally:
            shutil.rmtree(d, ignore_errors=True)
        rows_out.append((rows, size, load, scan, group, window, rss))
        print(f"{rows:>10,} {size/1e6:>8.0f}M {load:>7.2f}s {rows/load:>10,.0f} "
              f"{scan:>7.3f}s {group:>8.3f}s {window:>7.3f}s {rss/1024:>9.0f}M")
    print("\nLoad is a loader parsing the CSV into a DuckDB file, then the engine attaching it.")
    print("Query times are the full MCP round trip, including the result cap. Peak RSS is the")
    print("whole session's — server, engine and loader — sampled every 20 ms.")
    if out:
        write_artifact(out, binary, rows_out)
        print(f"\nwrote {out}")

def write_artifact(path, binary, rows_out):
    top = rows_out[-1]
    body = [
        "# Capacity — measured, not claimed",
        "",
        "REQ-7: capacity is measured, not claimed. **No capacity claim belongs in any benchy",
        "document that is not output from this file.** Regenerate it on your own hardware — these",
        "numbers describe the machine below and nothing else:",
        "",
        "```sh",
        # The recipe, not the path this run happened to use: that path is this machine's.
        "python3 examples/capacity.py ./target/release/benchy-mcp --out docs/CAPACITY.md",
        "```",
        "",
        f"- **Machine** — {machine()}",
        f"- **Measured** — {time.strftime('%Y-%m-%d')}",
        "- **Shape** — four columns: an ISO timestamp, a 500-value string key, a double, a short string.",
        "- **Defaults** — `--max-rows 200`, `--max-bytes 8192`, `--deadline 20`, `--memory 1GB`.",
        "",
        "| rows | csv on disk | load | load rate | `count(*)` | `GROUP BY` | window | peak RSS |",
        "|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for rows, size, load, scan, group, window, rss in rows_out:
        body.append(
            f"| {rows:,} | {size / 1e6:.0f} MB | {load:.2f} s | {rows / load / 1e6:.2f} M rows/s | "
            f"{scan * 1000:.0f} ms | {group * 1000:.0f} ms | {window * 1000:.0f} ms | {rss / 1024:.0f} MB |"
        )
    body += [
        "",
        "## Reading this",
        "",
        "**Load** is the cost you pay once, before the agent's first call: a loader process parses",
        "the CSV, infers its types and writes a DuckDB file, which the engine then attaches. It is the",
        "only number that grows with your data, and it is what the agent never has to spend a step on.",
        "The parse runs on every core and the write into the file on one, so the same rows make the",
        "same file, block for block, whatever they came from (REQ-15; a parallel write was measured and",
        "left out, docs/PERFORMANCE.md R4-1).",
        "",
        "**Query times are the full MCP round trip** — JSON in, plan, execute, cap, JSON out. `count(*)`",
        "stays flat because DuckDB answers it from metadata. The two that do real work scale with the",
        "data and stay small. What does *not* grow is the answer: capped at 200 rows either way. That",
        "is the product — the reason to put data on a bench instead of into a context window.",
        "",
        "**Peak RSS is the whole session's** — the server, its engine and the loader that parsed the",
        "file — sampled every 20 ms: the loader has exited by the time the queries run, and its moment",
        "is often the highest. It tracks the data rather than the configured memory budget, so size",
        "the OS ceiling from this column",
        "and not from `--memory`. It is also the noisiest column here: repeated runs of the same",
        "workload differ by a fifth or more, because the allocator retains pages. Size from the top",
        "of a few runs, not from one.",
        "",
        "## What the first run of this found",
        "",
        "History, not a measurement of this run. The first version of this table reported **0.022 s",
        "for `count(*)` at 100k rows, at 1M and at 5M**. A number that does not move with the work is",
        "not measuring the work. The deadline",
        "watchdog polled in 20 ms sleeps and was joined at the end of every batch, so every query —",
        "`SELECT 1` included — waited out the rest of a tick. Replacing the poll with a condvar took a",
        "trivial query from 22.2 ms to 1.9 ms, on every call an agent makes.",
        "",
        "That is what a benchmark is for. It was not written to find that, and the bug had been",
        "shipping happily through a green test suite.",
        "",
        "## What this does not measure",
        "",
        "- **Parquet**, which loads without the parse and the type inference, so it will be faster.",
        "- **Concurrency.** One bench per process by design (REQ-1). `neighbour_unaffected` shows a",
        "  runaway neighbour leaves this bench correct and responsive; the CPU is still shared.",
        "- **The ceiling.** Every size here fits; none of them found the edge. The honest statement is",
        f"  that {top[0]:,} rows of this shape cost {top[6] / 1024:.0f} MB and {top[2]:.1f} s, not that",
        f"  {top[0]:,} rows is a limit.",
    ]
    with open(path, "w") as fh:
        fh.write("\n".join(body) + "\n")

if __name__ == "__main__":
    a = sys.argv[1:]
    out = None
    if "--out" in a:
        i = a.index("--out"); out = a[i + 1]; del a[i:i + 2]
    if len(a) != 1:
        print(__doc__); raise SystemExit(2)
    main(a[0], out)
