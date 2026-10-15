#!/usr/bin/env python3
"""The probe behind `docs/DIALECT.md` and `src/hint.rs`.

    python3 spike/probe-dialect.py ./target/release/benchy-mcp

Calls each candidate against a real sealed bench and records which ones DuckDB refuses. Re-run it
after a DuckDB bump: `tests/hints.rs` catches a reworded *message*, but only this catches a
function DuckDB has newly **added**, which silently turns a correct hint into a wrong one.

`duckdb_functions()` is not a substitute. `coalesce`, `if`, `try_cast`, `extract` and
`percentile_cont` are parsed specially and do not appear in it; the first version of the mapping
table was built from that view and was wrong about six entries.
"""
import json, subprocess, sys

# One call each, because existence is the thing being measured and a compile error is an answer.
CANDIDATES = {
    "to_char": "SELECT to_char(now(),'YYYY')",
    "to_date": "SELECT to_date('2026-01-01','YYYY-MM-DD')",
    "date_format": "SELECT date_format(now(),'%Y')",
    "str_to_date": "SELECT str_to_date('x','%Y')",
    "nvl": "SELECT nvl(1,2)",
    "nvl2": "SELECT nvl2(1,2,3)",
    "isnull": "SELECT isnull(1)",
    "ifnull": "SELECT ifnull(1,2)",
    "getdate": "SELECT getdate()",
    "sysdate": "SELECT sysdate()",
    "convert": "SELECT convert(int, '1')",
    "dateadd": "SELECT dateadd(day, 1, now())",
    "charindex": "SELECT charindex('a','abc')",
    "iif": "SELECT iif(true,1,2)",
    "if": "SELECT if(true,1,2)",
    "rand": "SELECT rand()",
    "random": "SELECT random()",
    "timestampdiff": "SELECT timestampdiff(day, now(), now())",
    "datediff": "SELECT datediff('day', now(), now())",
    "try_cast": "SELECT try_cast('x' AS INT)",
    "coalesce": "SELECT coalesce(1,2)",
    "percentile_cont": "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY 1)",
    "extract": "SELECT extract(year FROM now())",
    "interval": "SELECT INTERVAL 1 DAY",
    "strftime": "SELECT strftime(now(), '%Y')",
    "strptime": "SELECT strptime('2026-01-01', '%Y-%m-%d')",
    "instr": "SELECT instr('abc','a')",
    "position": "SELECT position('a' IN 'abc')",
    "list_syntax": "SELECT [1,2,3]",
    "ilike": "SELECT 'A' ILIKE 'a'",
    "concat_pipe": "SELECT 'a'||'b'",
    "group_concat": "SELECT group_concat(x) FROM (SELECT 1 x)",
    "string_agg": "SELECT string_agg(x::VARCHAR, ',') FROM (SELECT 1 x)",
    "approx_quantile": "SELECT approx_quantile(x, 0.5) FROM (SELECT 1 x)",
}

# Shapes whose *message* a hint is keyed to. Their exact text is what `src/hint.rs` matches on.
MESSAGE_SHAPES = {
    "file function": "SELECT * FROM read_parquet('/etc/passwd')",
    "unknown table": "SELECT * FROM no_such_table",
    "unknown column": "SELECT no_such_column FROM meters",
    "double-quoted string": 'SELECT "hello"',
    "backtick identifier": "SELECT `meter` FROM meters",
    "TOP n": "SELECT TOP 5 * FROM meters",
    "FOR UPDATE": "SELECT * FROM meters FOR UPDATE",
}


def session(binary, data):
    p = subprocess.Popen([binary, data], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         text=True, bufsize=1)
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
            return
        while True:
            line = p.stdout.readline()
            if not line:
                raise SystemExit("the server closed the pipe")
            m = json.loads(line)
            if m.get("id") == msg["id"]:
                return m

    call("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                        "clientInfo": {"name": "probe-dialect", "version": "0"}})
    call("notifications/initialized", notify=True)
    return p, call


def main(binary, data):
    p, call = session(binary, data)

    def run(sql):
        r = call("tools/call", {"name": "query", "arguments": {"sql": sql}})
        res = r.get("result", {})
        body = "".join(c.get("text", "") for c in res.get("content", []))
        return bool(res.get("isError")), body

    version = run("SELECT version()")[1].splitlines()[1].strip()
    print(f"DuckDB {version}\n")

    missing, present = [], []
    for name, sql in CANDIDATES.items():
        failed, body = run(sql)
        gone = failed and "does not exist" in body
        (missing if gone else present).append(name)
    print("MISSING — a hint may map these:")
    for m in missing:
        print(f"  {m}")
    print("\nPRESENT — hinting these would be wrong:")
    print("  " + ", ".join(present))

    print("\nMESSAGE SHAPES — `src/hint.rs` matches on this exact text:")
    for label, sql in MESSAGE_SHAPES.items():
        failed, body = run(sql)
        first = body.splitlines()[0] if body else ""
        # strip any hint we already attach, so the raw DuckDB text is what gets recorded
        raw = first.split(" — ")[0]
        print(f"  {label:22s} {'ERR' if failed else 'ok '}  {raw}")

    p.stdin.close()
    p.wait(timeout=20)


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        print(__doc__)
        raise SystemExit(2)
    main(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "./examples/data")
