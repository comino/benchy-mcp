#!/usr/bin/env python3
"""The ingest benchmark: what it costs to get data onto the bench, stage by stage.

    python3 bench/ingest.py ./target/release/benchy-mcp [--runs 5] [--scenarios csv-1m,exec-10m]
        [--clickhouse /path/to/clickhouse] [--json out.json] [--no-perf] [--record DIR]

Every scenario loads the same synthetic table (bench/gen_data.py: ts, meter, kw) a different way,
starts benchy with BENCHY_TRACE_TIMINGS=1 (src/trace.rs), asks for `describe` once, and ends the
session. Per run it reads:

- **instructions and cycles** (user space) of the whole process tree, from `perf stat`. The
  primary metric: on a shared machine instructions barely move from run to run, where time does.
  Needs `kernel.perf_event_paranoid` <= 1; without it the run says so and goes on without them.
- **CPU time** (user + sys) and **peak RSS** per process — server, engine, each loader, and the
  connector a loader reaped — from each process's own `getrusage`.
- **stages**: each span the binary traces, with its wall time and the CPU it cost the process
  (every thread); and process starts, from the parent's `spawn` mark to the child's `main`.
- **cold start**: from spawning benchy to the answer to the first `describe`. Wall time, so noisy.

N runs per scenario; the table shows the minimum (the run least disturbed by the rest of the
machine) and the spread, (max - min) / min. `--record DIR` also runs each scenario once under
`perf record` and writes, per scenario, the instructions by process and the top symbols:
that is where docs/PERFORMANCE.md's hotspot list comes from.

Scenarios: csv-1m, parquet-1m, ndjson-1m (a folder), exec-1m, exec-10m (an exec connector that
`cat`s an Arrow stream, so the connector costs next to nothing and the time is benchy's),
exec_series-1m, exec_series-10m (the same, with 500 meters reading every minute, so the times
repeat and the profile cuts the table into series), exec_intkey-1m, exec_intkey-10m
(exec_series' rows with `meter` an int64, so the key tallied is an integer, R3-10), exec_mixed-1m,
exec_mixed-10m (150 meters reading every 1, 2 or 3 minutes, each judged at its own step, #50),
exec_lz4-1m,
exec_lz4-10m (exec's rows with
each batch's buffers LZ4-compressed, as ClickHouse's `ArrowStream` sends them by default),
ch-1m, ch-10m (the built-in ClickHouse source, against a server this script starts on port 8133,
or --ch-port, from --clickhouse, with its data under scratch/, and stops by its PID), host-1m
(`benchy-mcp host`, the host attaching the Arrow stream; host4-1k, host8-1k, … attach it as N
tables, in one request where the binary speaks protocol 2), hostspool-1m and hoststream-1m (a
host that fetches the stream itself, read here from the data file in 1 MiB chunks: hostspool
writes it to a spool file in /dev/shm — tmpfs, which is memory, as a container's disk often is —
and attaches the file; hoststream writes it into a pipe it passes with a streamed attach, protocol
3, over a socket that is the session's stdin; hostchannel-1m writes it into the session's stream
channel, a pipe benchy inherits (`--stream-channel`, protocol 5), with plain pipes for the
protocol; all are timed from the spawn, fetch included, and `host.spool_mb` is what was spooled;
`--host-fetch-mbps` paces that fetch as a network would).
A ClickHouse scenario takes the
source's default `compression`; ch_zstd-10m, ch_lz4-10m, ch_none-10m (any size) set it. http-csv-1m,
http-parquet-1m, http-ndjson-1m, http-arrow-1m and http-json-1m: the built-in HTTP source (REQ-17)
fetching the same files from a server in this script, on 127.0.0.1, which reads each from the page
cache; http-json-1m is the NDJSON's rows as cursor-paged JSON, 10,000 to a page; http-csvdecl-1m
is the CSV with its columns declared, which states its dialect (R2-3); csvdecl-1m and
ndjsondecl-1m are the folder's CSV and NDJSON as a declared file source (R3-5). The server is a
thread of this script, not of benchy's process tree, so neither its instructions nor its CPU are
counted. Every kind also runs at 1k rows (csv-1k, exec-1k, host-1k, http-csv-1k, …), where what
is measured is the fixed cost of a session.
"""
import argparse
import http.server
import json
import threading
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DATA = os.path.join(ROOT, "scratch", "bench")
PYARROW = "pyarrow==26.0.0"
CH_HTTP, CH_TCP = 8133, 9033  # --ch-port moves both: the TCP port is HTTP's + 900
# perf record: one sample per this many instructions.
PERIOD = 1_000_000
SCENARIOS = ["csv-1m", "parquet-1m", "ndjson-1m", "exec-1m", "exec-10m", "ch-1m", "ch-10m",
             "host-1m", "http-csv-1m", "http-parquet-1m", "http-ndjson-1m", "http-arrow-1m",
             "http-json-1m", "exec_series-1m", "exec_series-10m"]
PAGE_ROWS = 10_000
# hostspool, hoststream and hostchannel: the host's fetch, paced to this many MB/s (--host-fetch-mbps); as fast
# as the page cache gives it when 0.
FETCH_MBPS = 0
# Any kind also runs at 1k (csv-1k, exec-1k, host-1k, …), not by default: with next to nothing to
# load, what is left is what every session pays before its first `describe` — process starts,
# the engine's open, the attach check.
ROWS = {"1k": 1_000, "1m": 1_000_000, "10m": 10_000_000}
# The kinds bench/gen_data.py writes as one Arrow IPC stream file, rather than a folder.
STREAMS = ("arrows", "series", "lz4", "intkey", "mixed")
# Caps raised so 10,000,000 rows fit: the defaults are 10,000,000 rows and 256 MiB per fetch.
CAPS = "max_rows = 20_000_000\nmax_bytes = 4_294_967_296\n"
BUDGET = "[budget]\nrows = 100_000_000\nbytes = 8_589_934_592\n"


def data(rows, kind):
    """The path of `kind` (csv, parquet, ndjson: a folder; arrows: a file) at `rows`, made if
    missing."""
    d = os.path.join(DATA, str(rows))
    path = {"arrows": os.path.join(d, "readings.arrows"),
            "series": os.path.join(d, "readings-series.arrows"),
            "lz4": os.path.join(d, "readings-lz4.arrows"),
            "intkey": os.path.join(d, "readings-intkey.arrows"),
            "mixed": os.path.join(d, "readings-mixed.arrows")}.get(kind, os.path.join(d, kind))
    probe = path if kind in STREAMS else os.path.join(path, f"readings.{kind}")
    if not os.path.exists(probe):
        print(f"  making {probe} …", file=sys.stderr)
        subprocess.run(["uv", "run", "--no-project", "--with", PYARROW, "python",
                        os.path.join(ROOT, "bench", "gen_data.py"), DATA, str(rows), kind],
                       check=True)
    return path


# ---------------------------------------------------------------------------------- ClickHouse

class ClickHouse:
    """A server of our own, on our own ports, stopped by the PID we started."""

    def __init__(self, binary):
        self.binary = binary
        self.dir = os.path.join(ROOT, "scratch", "ch-bench")
        os.makedirs(self.dir, exist_ok=True)
        self.log = open(os.path.join(self.dir, "server.out"), "ab")
        self.p = subprocess.Popen(
            [binary, "server", "--", f"--path={self.dir}/", f"--http_port={CH_HTTP}",
             f"--tcp_port={CH_TCP}", "--listen_host=127.0.0.1", "--mysql_port=0",
             "--postgresql_port=0", "--interserver_http_port=0",
             f"--logger.log={self.dir}/server.log", f"--logger.errorlog={self.dir}/server.err",
             f"--user_files_path={self.dir}/user_files"],
            stdout=self.log, stderr=self.log, cwd=self.dir)
        for _ in range(300):
            try:
                if self.sql("SELECT 1").strip() == "1":
                    return
            except OSError:
                time.sleep(0.2)
        self.stop()
        raise SystemExit("ClickHouse did not start; see " + self.dir)

    def sql(self, q):
        # POST: ClickHouse runs a GET read-only.
        req = urllib.request.Request(f"http://127.0.0.1:{CH_HTTP}/", data=q.encode())
        try:
            with urllib.request.urlopen(req, timeout=600) as r:
                return r.read().decode()
        except urllib.error.HTTPError as e:
            raise SystemExit(f"ClickHouse refused {q[:60]!r}: {e.read().decode()[:400]}")

    def table(self, rows):
        name = f"readings_{rows}"
        have = self.sql(f"EXISTS TABLE {name}").strip()
        if have != "1" or self.sql(f"SELECT count() FROM {name}").strip() != str(rows):
            self.sql(f"DROP TABLE IF EXISTS {name}")
            self.sql(f"CREATE TABLE {name} (ts DateTime64(6, 'UTC'), meter LowCardinality(String), "
                     f"kw Float64) ENGINE = MergeTree ORDER BY (meter, ts)")
            parquet = os.path.join(data(rows, "parquet"), "readings.parquet")
            with open(parquet, "rb") as fh:
                req = urllib.request.Request(
                    f"http://127.0.0.1:{CH_HTTP}/?query=" +
                    urllib.parse.quote(f"INSERT INTO {name} FORMAT Parquet"), data=fh)
                req.add_header("Content-Length", str(os.path.getsize(parquet)))
                urllib.request.urlopen(req, timeout=600).read()
            self.sql(f"OPTIMIZE TABLE {name} FINAL")
        return name

    def cpu_ticks(self):
        """CPU of the server's whole tree: `clickhouse server` is a watchdog with the server as
        its child. User and system, every thread, in clock ticks."""
        stat = {}
        for e in os.listdir("/proc"):
            if e.isdigit():
                try:
                    with open(f"/proc/{e}/stat") as fh:
                        f = fh.read().rsplit(")", 1)[1].split()
                    stat[int(e)] = (int(f[1]), int(f[11]) + int(f[12]))
                except (OSError, IndexError, ValueError):
                    pass
        tree, total = [self.p.pid], 0
        while tree:
            pid = tree.pop()
            total += stat.get(pid, (0, 0))[1]
            tree += [c for c, (ppid, _) in stat.items() if ppid == pid]
        return total

    def stop(self):
        if self.p.poll() is None:
            self.p.terminate()  # our PID only, never by name
            try:
                self.p.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()


# ---------------------------------------------------------------------------------------- HTTP

def pages(rows):
    """The NDJSON's rows as cursor-paged JSON, PAGE_ROWS to a page, made if missing: the number
    of pages."""
    d = os.path.join(DATA, str(rows), "pages")
    done = os.path.join(d, "done")
    if not os.path.exists(done):
        os.makedirs(d, exist_ok=True)
        with open(os.path.join(data(rows, "ndjson"), "readings.ndjson")) as fh:
            lines = [l.rstrip("\n") for l in fh]
        n = (len(lines) + PAGE_ROWS - 1) // PAGE_ROWS
        for i in range(n):
            nxt = f'"{i + 1}"' if i + 1 < n else "null"
            with open(os.path.join(d, f"page-{i}.json"), "w") as out:
                out.write('{"data": [' + ",".join(lines[i * PAGE_ROWS:(i + 1) * PAGE_ROWS]) +
                          '], "meta": {"next_cursor": ' + nxt + "}}")
        with open(done, "w") as fh:
            fh.write(str(n))
    return int(open(done).read())


class Files(http.server.BaseHTTPRequestHandler):
    """/<rows>/<kind> answers with that file; /<rows>/pages?cursor=<i> with page i."""

    def do_GET(self):
        path, _, query = self.path.partition("?")
        _, rows, kind = path.split("/")
        rows = int(rows)
        if kind == "pages":
            pages(rows)
            i = urllib.parse.parse_qs(query).get("cursor", ["0"])[0]
            f = os.path.join(DATA, str(rows), "pages", f"page-{i}.json")
        elif kind == "arrows":
            f = data(rows, "arrows")
        else:
            f = os.path.join(data(rows, kind), f"readings.{kind}")
        body = open(f, "rb").read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


SERVER = []


def files_url():
    if not SERVER:
        srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Files)
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        SERVER.append(srv)
    return f"http://127.0.0.1:{SERVER[0].server_address[1]}"


# ------------------------------------------------------------------------------------ sessions

def is_ch(name):
    """A ClickHouse scenario: ch-<size>, or ch_<compression>-<size>."""
    return name.startswith(("ch-", "ch_"))


def scenario_args(name, work, ch):
    """The command line for `name`, after the binary, writing any config into `work`."""
    kind, size = name.rsplit("-", 1)
    rows = ROWS[size]
    if kind in ("csv", "parquet", "ndjson"):
        return [data(rows, kind)]
    if kind in ("csvdecl", "ndjsondecl"):
        fmt = kind[:-len("decl")]
        # Each declared as its sniffer types it: `ts` is TIMESTAMPTZ to the CSV sniffer, and a
        # zone-less TIMESTAMP, read in UTC, to the JSON one, whose cast then needs no ICU.
        ts = "timestamptz" if fmt == "csv" else "timestamp"
        cfg = os.path.join(work, "benchy.toml")
        with open(cfg, "w") as fh:
            fh.write(f"[[source]]\nname = \"readings\"\ntype = \"file\"\n"
                     f"path = \"{os.path.join(data(rows, fmt), 'readings.' + fmt)}\"\n"
                     f"  [[source.schema]]\n  name = \"ts\"\n  type = \"{ts}\"\n"
                     "  [[source.schema]]\n  name = \"meter\"\n  type = \"string\"\n"
                     "  [[source.schema]]\n  name = \"kw\"\n  type = \"float64\"\n")
        return ["--config", cfg]
    if kind in ("exec", "exec_series", "exec_lz4", "exec_intkey", "exec_mixed"):
        stream = data(rows, {"exec": "arrows", "exec_series": "series", "exec_lz4": "lz4",
                             "exec_intkey": "intkey", "exec_mixed": "mixed"}[kind])
        connector = os.path.join(work, "cat.sh")
        with open(connector, "w") as fh:
            fh.write("#!/bin/sh\n# fetch: the stream as it is; anything else: not implemented\n"
                     f"[ \"$1\" = fetch ] || exit 2\nexec cat '{stream}'\n")
        os.chmod(connector, 0o700)
        cfg = os.path.join(work, "benchy.toml")
        with open(cfg, "w") as fh:
            fh.write(f"{BUDGET}\n[[source]]\nname = \"readings\"\ntype = \"exec\"\n"
                     f"command = \"{connector}\"\n{CAPS}"
                     "  [[source.schema]]\n  name = \"ts\"\n  type = \"timestamptz\"\n"
                     "  [[source.schema]]\n  name = \"meter\"\n"
                     f"  type = \"{'int64' if kind == 'exec_intkey' else 'string'}\"\n"
                     "  [[source.schema]]\n  name = \"kw\"\n  type = \"float64\"\n")
        return ["--config", cfg]
    if is_ch(name):
        if ch is None:
            return None
        table = ch.table(rows)
        cfg = os.path.join(work, "benchy.toml")
        with open(cfg, "w") as fh:
            fh.write(f"{BUDGET}\n[[source]]\nname = \"readings\"\ntype = \"clickhouse\"\n"
                     f"url = \"http://127.0.0.1:{CH_HTTP}\"\n"
                     f"query = \"SELECT ts, toString(meter) AS meter, kw FROM {table}\"\n{CAPS}"
                     + (f"compression = \"{kind[3:]}\"\n" if kind.startswith("ch_") else ""))
        return ["--config", cfg]
    if kind in ("host", "hostspool", "hoststream", "hostchannel") or re.fullmatch(r"host\d+", kind):
        return ["host"]
    if kind.startswith("http-"):
        fmt = kind[len("http-"):]
        declared = fmt == "csvdecl"
        if declared:
            fmt = "csv"
        what = {"arrow": "arrows", "json": "pages"}.get(fmt, fmt)
        # Made before the session starts, so making it is not timed.
        if what == "pages":
            pages(rows)
        elif what == "arrows":
            data(rows, "arrows")
        else:
            data(rows, what)
        paged = ("[source.pages]\nrows = \"data\"\nnext = \"cursor\"\n"
                 "cursor = \"meta.next_cursor\"\ncursor_param = \"cursor\"\n"
                 if fmt == "json" else "")
        cfg = os.path.join(work, "benchy.toml")
        with open(cfg, "w") as fh:
            fh.write(f"{BUDGET}\n[[source]]\nname = \"readings\"\ntype = \"http\"\n"
                     f"url = \"{files_url()}/{rows}/{what}\"\nformat = \"{fmt}\"\n{CAPS}{paged}"
                     + ("  [[source.schema]]\n  name = \"ts\"\n  type = \"timestamptz\"\n"
                     "  [[source.schema]]\n  name = \"meter\"\n  type = \"string\"\n"
                     "  [[source.schema]]\n  name = \"kw\"\n  type = \"float64\"\n" if declared else ""))
        return ["--config", cfg]
    raise SystemExit(f"unknown scenario {name}")


class Mcp:
    def __init__(self, p):
        self.p, self.n = p, 0

    def call(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.n += 1
            msg["id"] = self.n
        self.p.stdin.write((json.dumps(msg) + "\n").encode())
        self.p.stdin.flush()
        if notify:
            return None
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("benchy closed its stdout")
            m = json.loads(line)
            if m.get("id") == msg["id"]:
                return m

    def describe(self):
        self.call("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                                 "clientInfo": {"name": "bench-ingest", "version": "0"}})
        self.call("notifications/initialized", notify=True)
        r = self.call("tools/call", {"name": "describe", "arguments": {}})
        body = "".join(c.get("text", "") for c in r["result"]["content"])
        if r["result"].get("isError") or "readings" not in body:
            raise RuntimeError("describe did not list the table: " + body[:300])
        return body


class Host:
    def __init__(self, p, sock=None):
        """Over the process's pipes, or over `sock`, a socket that is its stdin and stdout."""
        self.p, self.n, self.sock = p, 0, sock

    def _read(self, n):
        if self.sock is None:
            return self.p.stdout.read(n)
        buf = b""
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise RuntimeError("benchy closed the session")
            buf += chunk
        return buf

    def call(self, call, fds=(), **fields):
        self.n += 1
        body = json.dumps({"id": self.n, "call": call, **fields}).encode()
        frame = struct.pack(">I", len(body)) + body
        if self.sock is None:
            self.p.stdin.write(frame)
            self.p.stdin.flush()
        else:
            sent = socket.send_fds(self.sock, [frame], list(fds)) if fds else 0
            self.sock.sendall(frame[sent:])
        (n,) = struct.unpack(">I", self._read(4))
        return json.loads(self._read(n))

    def fetched(self, source):
        """The stream as a host fetches it: 1 MiB at a time, at --host-fetch-mbps when given, as
        a fetch over a network arrives."""
        start, got = time.monotonic(), 0
        with open(source, "rb") as f:
            while chunk := f.read(1 << 20):
                got += len(chunk)
                if FETCH_MBPS:
                    ahead = start + got / (FETCH_MBPS * 1e6) - time.monotonic()
                    if ahead > 0:
                        time.sleep(ahead)
                yield chunk

    def describe_spooled(self, source, spool):
        """Fetch `source` into the file `spool`, attach the file, and describe: what a host must
        do when an attach takes only a path. Returns the bytes spooled."""
        self.call("hello", protocol=[1])
        with open(spool, "wb") as out:
            for chunk in self.fetched(source):
                out.write(chunk)
        r = self.call("attach", name="readings", path=spool)
        if not r.get("ok"):
            raise RuntimeError(f"attach failed: {r}")
        size = os.path.getsize(spool)
        r = self.call("describe")
        if not r.get("ok") or "readings" not in r.get("text", ""):
            raise RuntimeError(f"describe failed: {r}")
        return size

    def describe_streamed(self, source):
        """Fetch `source` into a pipe benchy reads as it is written (protocol 3), and describe."""
        hello = self.call("hello", protocol=[1, 2, 3])
        if hello.get("protocol") != 3:
            raise RuntimeError(f"this binary does not stream attachments: {hello}")
        read, write = os.pipe()

        def writer():
            with os.fdopen(write, "wb") as w:
                for chunk in self.fetched(source):
                    w.write(chunk)

        t = threading.Thread(target=writer)
        # The request goes first, so the reader is there when the first byte is.
        self.n += 1
        body = json.dumps({"id": self.n, "call": "attach", "name": "readings",
                           "stream": True}).encode()
        frame = struct.pack(">I", len(body)) + body
        sent = socket.send_fds(self.sock, [frame], [read])
        self.sock.sendall(frame[sent:])
        os.close(read)
        t.start()
        (n,) = struct.unpack(">I", self._read(4))
        r = json.loads(self._read(n))
        t.join()
        if not r.get("ok"):
            raise RuntimeError(f"attach failed: {r}")
        r = self.call("describe")
        if not r.get("ok") or "readings" not in r.get("text", ""):
            raise RuntimeError(f"describe failed: {r}")

    def describe_channeled(self, source, channel):
        """Fetch `source` into the session's stream channel (protocol 5), and describe. The
        attach goes first and the host writes as it fetches, on a thread of its own."""
        hello = self.call("hello", protocol=[1, 2, 3, 4, 5])
        if hello.get("protocol") != 5:
            raise RuntimeError(f"this binary has no stream channel: {hello}")

        def writer():
            with os.fdopen(channel, "wb") as w:
                for chunk in self.fetched(source):
                    w.write(chunk)

        t = threading.Thread(target=writer)
        t.start()
        r = self.call("attach", name="readings", stream="channel")
        t.join()
        if not r.get("ok"):
            raise RuntimeError(f"attach failed: {r}")
        r = self.call("describe")
        if not r.get("ok") or "readings" not in r.get("text", ""):
            raise RuntimeError(f"describe failed: {r}")

    def describe(self, stream, n=1):
        """Attach `stream` as `n` tables (readings, readings_2, …): in one request where the
        session speaks protocol 2, one request each where it speaks only 1."""
        hello = self.call("hello", protocol=[1, 2])
        names = ["readings"] + [f"readings_{i}" for i in range(2, n + 1)]
        if hello.get("protocol") == 2 and n > 1:
            r = self.call("attach", tables=[{"name": t, "path": stream} for t in names])
            if not r.get("ok") or not all(t.get("ok") for t in r.get("tables", [])):
                raise RuntimeError(f"attach failed: {r}")
        else:
            for t in names:
                r = self.call("attach", name=t, path=stream)
                if not r.get("ok"):
                    raise RuntimeError(f"attach failed: {r}")
        r = self.call("describe")
        if not r.get("ok") or "readings" not in r.get("text", ""):
            raise RuntimeError(f"describe failed: {r}")
        return r["text"]


def perf_works():
    try:
        r = subprocess.run(["perf", "stat", "-x,", "-e", "instructions:u", "true"],
                           capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired):
        return False
    return r.returncode == 0 and "instructions" in r.stderr and "<not" not in r.stderr


def one_run(binary, name, ch, perf, record=None):
    work = tempfile.mkdtemp(prefix="bench-ingest-")
    try:
        args = scenario_args(name, work, ch)
        if args is None:
            return None
        cmd = [binary, *args]
        stat = os.path.join(work, "perf.stat")
        if record:
            # Sample times on CLOCK_MONOTONIC, the clock the trace is on, so each sample can be
            # put in the stage that was running when it was taken.
            cmd = ["perf", "record", "-q", "-k", "CLOCK_MONOTONIC", "-e", "instructions:u",
                   "-c", str(PERIOD), "-o", record, "--", *cmd]
        elif perf:
            cmd = ["perf", "stat", "-x,", "-e", "instructions:u,cycles:u", "-o", stat, "--", *cmd]
        env = dict(os.environ, BENCHY_TRACE_TIMINGS="1")
        err = open(os.path.join(work, "stderr"), "w+b")
        ch0 = ch.cpu_ticks() if ch and is_ch(name) else None
        t0 = time.clock_gettime_ns(time.CLOCK_MONOTONIC) // 1000
        kind, size = name.rsplit("-", 1)
        source = data(ROWS[size], "arrows") if name.startswith("host") else None
        spooled, spool = None, None
        channel = None
        if kind == "hoststream":
            # The session's stdin and stdout are a socket, so a request can carry a descriptor.
            ours, theirs = socket.socketpair()
            p = subprocess.Popen(cmd, stdin=theirs, stdout=theirs, stderr=err, env=env)
            theirs.close()
        elif kind == "hostchannel":
            # Plain pipes for the protocol, and one more pipe for the streams.
            ours = None
            read, channel = os.pipe()
            p = subprocess.Popen([*cmd, "--stream-channel", str(read)], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, stderr=err, env=env, pass_fds=(read,))
            os.close(read)
        else:
            ours = None
            p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err,
                                 env=env)
        try:
            if kind == "hoststream":
                Host(p, ours).describe_streamed(source)
            elif kind == "hostchannel":
                Host(p).describe_channeled(source, channel)
            elif kind == "hostspool":
                spool = os.path.join("/dev/shm", f"bench-spool-{os.getpid()}.arrows")
                spooled = Host(p).describe_spooled(source, spool)
            elif name.startswith("host"):
                Host(p).describe(source, int(kind[4:] or 1))
            else:
                Mcp(p).describe()
            t1 = time.clock_gettime_ns(time.CLOCK_MONOTONIC) // 1000
        finally:
            if spool and os.path.exists(spool):
                os.unlink(spool)
            if ours is not None:
                ours.shutdown(socket.SHUT_WR)
            else:
                p.stdin.close()
            try:
                p.wait(timeout=60)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
        ch1 = ch.cpu_ticks() if ch0 is not None else None
        err.seek(0)
        log = err.read().decode(errors="replace")
        events = [json.loads(l[len("benchy-trace "):]) for l in log.splitlines()
                  if l.startswith("benchy-trace ")]
        if not events:
            raise RuntimeError("no trace lines; is this binary built with src/trace.rs?\n" + log)
        out = {"cold_start_us": t1 - t0, "events": events, "spawned_at": t0}
        if spooled is not None:
            out["spool_bytes"] = spooled
        if ch1 is not None:
            out["clickhouse_cpu_us"] = (ch1 - ch0) * 1_000_000 // os.sysconf("SC_CLK_TCK")
        if perf and not record and os.path.exists(stat):
            for line in open(stat):
                f = line.strip().split(",")
                if len(f) > 2 and f[0].isdigit():
                    out[f[2].split(":")[0]] = int(f[0])
        return out
    finally:
        shutil.rmtree(work, ignore_errors=True)


# ---------------------------------------------------------------------------------- reporting

def summarize(run):
    """One run's numbers, flat: what the table compares across runs."""
    ev = run["events"]
    s = {"cold_start_ms": run["cold_start_us"] / 1000}
    if "spool_bytes" in run:
        s["host.spool_mb"] = run["spool_bytes"] / 2**20
    for k in ("instructions", "cycles", "clickhouse_cpu_us"):
        if k in run:
            s[k] = run[k]
    # Process starts: the parent's spawn mark to the child's first line.
    main_of = {e["pid"]: e["t"] for e in ev if e["what"] == "main"}
    role_of = {e["pid"]: e["role"] for e in ev}
    server = next((e for e in ev if e["role"] == "server" and e["what"] == "main"), None)
    if server:
        s["start.server_ms"] = (server["t"] - run["spawned_at"]) / 1000
    spawns = sorted(e["t"] for e in ev if e["what"] in ("engine.spawn", "loader.spawn"))
    for child, t in sorted(main_of.items(), key=lambda kv: kv[1]):
        if role_of[child] in ("engine", "loader"):
            before = [x for x in spawns if x <= t]
            if before:
                key = f"start.{role_of[child]}_ms"
                s[key] = s.get(key, 0) + (t - before[-1]) / 1000
    # Spans, summed by name per role (a span repeats once per table).
    for e in ev:
        if "us" in e and "t0" in e:
            k = f"{e['role']}.{e['what']}"
            s[k + "_ms"] = s.get(k + "_ms", 0) + e["us"] / 1000
            if "cpu_us" in e:
                s[k + "_cpu_ms"] = s.get(k + "_cpu_ms", 0) + e["cpu_us"] / 1000
        elif e["what"] == "loader.done":
            s["fetched_bytes"] = s.get("fetched_bytes", 0) + e["fetched_bytes"]
    # Per process: the last usage line each process wrote.
    last = {}
    for e in ev:
        if "user_us" in e:
            last[e["pid"]] = e
    for pid, e in last.items():
        r = e["role"]
        s[f"cpu.{r}_ms"] = s.get(f"cpu.{r}_ms", 0) + (e["user_us"] + e["sys_us"]) / 1000
        s[f"rss.{r}_mb"] = max(s.get(f"rss.{r}_mb", 0), e["maxrss_kb"] / 1024)
        if r == "loader" and e["children_user_us"]:
            s["cpu.connector_ms"] = s.get("cpu.connector_ms", 0) + \
                (e["children_user_us"] + e["children_sys_us"]) / 1000
            s["rss.connector_mb"] = e["children_maxrss_kb"] / 1024
        s[f"io.{r}_written_mb"] = s.get(f"io.{r}_written_mb", 0) + e["oublock"] * 512 / 2**20
    return s


def table(results):
    lines = []
    for name, runs in results.items():
        if not runs:
            continue
        sums = [summarize(r) for r in runs]
        keys = []
        for s in sums:
            keys += [k for k in s if k not in keys]
        lines.append(f"\n## {name}   ({len(runs)} runs; min, and spread = (max-min)/min)\n")
        lines.append(f"  {'metric':44} {'min':>14} {'median':>14} {'spread':>8}")
        for k in keys:
            v = sorted(s[k] for s in sums if k in s)
            lo, med = v[0], v[len(v) // 2]
            spread = f"{(v[-1] - lo) / lo * 100:.0f}%" if lo else "-"
            fmt = (lambda x: f"{x:,.0f}") if k in ("instructions", "cycles", "fetched_bytes",
                                                  "clickhouse_cpu_us") else (lambda x: f"{x:,.1f}")
            lines.append(f"  {k:44} {fmt(lo):>14} {fmt(med):>14} {spread:>8}")
    return "\n".join(lines)


def record_report(binary, name, ch, out_dir):
    """Run `name` once under `perf record` and write where its instructions went: by process, by
    stage (the innermost traced span running when the sample was taken, on the sample's thread
    if that thread traces its own stages — a stream's watch runs beside its insert — else on the
    process's main thread), by library, and by symbol for each process."""
    os.makedirs(out_dir, exist_ok=True)
    data_file = os.path.join(out_dir, f"{name}.perf.data")
    run = one_run(binary, name, ch, False, record=data_file)
    if run is None:
        return
    ev = run["events"]
    role_of = {e["pid"]: e["role"] for e in ev}
    spans = {}
    for e in ev:
        if "t0" in e:
            spans.setdefault((e["pid"], e.get("tid", e["pid"])), []).append(
                (e["t0"], e["t1"], e["what"]))
    script = subprocess.run(["perf", "script", "-i", data_file, "-F",
                             "pid,tid,time,period,ip,sym,dso"],
                            capture_output=True, text=True).stdout
    sample = re.compile(
        r"^\s*(\d+)/(\d+)\s+([\d.]+):\s+(\d+)\s+[0-9a-f]+\s+(.*?)\s+\((.*)\)\s*$")
    by_role, by_stage, by_lib, by_sym, by_stage_sym = {}, {}, {}, {}, {}

    def add(d, k, n):
        d[k] = d.get(k, 0) + n

    for line in script.splitlines():
        m = sample.match(line)
        if not m:
            continue
        pid, tid, t, n = int(m[1]), int(m[2]), float(m[3]) * 1e6, int(m[4])
        sym = re.sub(r"\+0x[0-9a-f]+$", "", m[5]) or "[unknown]"
        lib = os.path.basename(m[6])
        role = role_of.get(pid, "connector/other")
        add(by_role, role, n)
        add(by_lib, (role, lib), n)
        add(by_sym, (role, f"{lib}: {sym}"), n)
        own = spans.get((pid, tid)) or spans.get((pid, pid), [])
        inside = [sp for sp in own if sp[0] <= t <= sp[1]]
        stage = min(inside, key=lambda sp: sp[1] - sp[0])[2] if inside else "(no stage)"
        add(by_stage, f"{role}: {stage}", n)
        add(by_stage_sym, (f"{role}: {stage}", f"{lib}: {sym}"), n)

    def top(d, role, limit, total=None):
        rows = sorted(((k[1], n) for k, n in d.items() if k[0] == role), key=lambda kv: -kv[1])
        total = total or by_role[role]
        return "".join(f"  {n / total * 100:5.1f}%  {k[:150]}\n" for k, n in rows[:limit])

    with open(os.path.join(out_dir, f"{name}.txt"), "w") as fh:
        fh.write(f"# {name}: instructions:u, sampled every {PERIOD:,}\n\n## by process\n")
        for r, n in sorted(by_role.items(), key=lambda kv: -kv[1]):
            fh.write(f"{r:28} {n:>16,}\n")
        fh.write("\n## by stage (innermost traced span of that process)\n")
        for k, n in sorted(by_stage.items(), key=lambda kv: -kv[1]):
            fh.write(f"{k:44} {n:>16,}\n")
        # The stages that cost a tenth of the run or more, symbol by symbol.
        everything = sum(by_role.values())
        for k, n in sorted(by_stage.items(), key=lambda kv: -kv[1]):
            if n >= everything / 10:
                fh.write(f"\n## stage {k}: top symbols\n" + top(by_stage_sym, k, 20, n))
        for r in sorted(by_role, key=lambda r: -by_role[r]):
            fh.write(f"\n## {r}: by library\n" + top(by_lib, r, 8))
            fh.write(f"\n## {r}: top symbols\n" + top(by_sym, r, 30))
    print(f"  {name}: {os.path.join(out_dir, name + '.txt')}", file=sys.stderr)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("binary")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--scenarios", default=",".join(SCENARIOS))
    ap.add_argument("--clickhouse", default=os.environ.get("BENCHY_CLICKHOUSE_BIN"))
    ap.add_argument("--ch-port", type=int, help="ClickHouse's HTTP port (default 8133); its "
                    "TCP port is this + 900")
    ap.add_argument("--json")
    ap.add_argument("--no-perf", action="store_true")
    ap.add_argument("--record", metavar="DIR")
    ap.add_argument("--host-fetch-mbps", type=float, default=0,
                    help="pace the host's fetch in hostspool and hoststream to this many MB/s")
    a = ap.parse_args()
    global FETCH_MBPS
    FETCH_MBPS = a.host_fetch_mbps
    if a.ch_port:
        global CH_HTTP, CH_TCP
        CH_HTTP, CH_TCP = a.ch_port, a.ch_port + 900
    binary = os.path.abspath(a.binary)
    names = [n for n in a.scenarios.split(",") if n]
    perf = not a.no_perf and perf_works()
    if not a.no_perf and not perf:
        print("perf stat cannot count instructions here (kernel.perf_event_paranoid must be "
              "<= 1, and perf installed): reporting CPU time only", file=sys.stderr)
    ch = None
    if any(is_ch(n) for n in names):
        if a.clickhouse:
            ch = ClickHouse(a.clickhouse)
        else:
            print("no --clickhouse: the ch-* scenarios are skipped", file=sys.stderr)
    results = {}
    try:
        for name in names:
            runs = []
            if is_ch(name) and ch is None:
                continue
            one_run(binary, name, ch, False)  # warm the page cache; not counted
            for i in range(a.runs):
                r = one_run(binary, name, ch, perf)
                if r:
                    runs.append(r)
                print(f"  {name} run {i + 1}/{a.runs}: {r['cold_start_us'] / 1000:.0f} ms"
                      + (f", {r['instructions']:,} instructions" if r and "instructions" in r
                         else ""), file=sys.stderr)
            results[name] = runs
            if a.record:
                record_report(binary, name, ch, a.record)
    finally:
        if ch:
            ch.stop()
    machine = (f"{os.uname().sysname} {os.uname().machine}, {os.cpu_count()} cores, load "
               f"{os.getloadavg()[0]:.1f}")
    print(f"# bench/ingest.py — {machine}")
    print(table(results))
    if a.json:
        with open(a.json, "w") as fh:
            json.dump({"machine": machine, "results": {n: [summarize(r) for r in rs]
                                                      for n, rs in results.items()}}, fh,
                      indent=1)


if __name__ == "__main__":
    main()
