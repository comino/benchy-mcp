#!/usr/bin/env python3
"""Drive benchy-mcp through a real MCP stdio handshake. No model, no API key.

    python3 examples/verify.py ./target/release/benchy-mcp ./examples/data

Exits non-zero if anything the README promises is not true.
"""
import json, subprocess, sys, os

# Where Landlock is missing, benchy refuses to serve unless told to go on without it. Saying so
# here keeps this script usable there; the sandbox check at the end reports what is missing.
PLATFORM = [] if sys.platform.startswith("linux") else ["--allow-unsandboxed"]

def main(binary, data):
    env = dict(os.environ)
    # a short deadline, so the runaway check does not sit here for the 20s default
    p = subprocess.Popen([binary, *PLATFORM, data, "--deadline", "3"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, bufsize=1, env=env)
    n = [0]
    def call(method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None: msg["params"] = params
        if not notify:
            n[0] += 1; msg["id"] = n[0]
        p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
        if notify: return None
        while True:
            line = p.stdout.readline()
            if not line:
                raise SystemExit(f"server closed the pipe\n--- stderr ---\n{p.stderr.read()}")
            m = json.loads(line)
            if m.get("id") == msg["id"]: return m

    def tool(name, **args):
        r = call("tools/call", {"name": name, "arguments": args})
        res = r.get("result", r)
        body = "".join(c.get("text", "") for c in res.get("content", []))
        return res.get("isError", False), body

    failures = []
    def check(label, ok, detail=""):
        print(("  ok   " if ok else "  FAIL ") + label + (f"  {detail}" if not ok and detail else ""))
        if not ok: failures.append(label)

    init = call("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                               "clientInfo": {"name": "benchy-verify", "version": "0"}})
    call("notifications/initialized", notify=True)
    info = init["result"]
    print(f"server: {info['serverInfo']['name']} {info['serverInfo']['version']}  "
          f"protocol {info['protocolVersion']}\n")

    check("instructions warn that file functions are unavailable",
          "unavailable" in (info.get("instructions") or ""))

    tools = call("tools/list", {})["result"]["tools"]
    names = [t["name"] for t in tools]
    check("the expected tools, and no load tool",
          names == ["describe", "query", "get_result", "dialect"], str(names))
    check("the dialect reference is on demand, not in every payload",
          "to_char" not in json.dumps(tools), "the mapping table is riding in the tool list")

    err, desc = tool("describe")
    check("describe names the tables", not err and "meters" in desc and "rooms" in desc)
    check("describe carries no filesystem path", "/" not in desc.replace("\n", ""), desc[:80])

    err, body = tool("query", sql="SELECT sum(power_kw)*0.25 AS kwh FROM meters WHERE meter='lab'")
    check("a real analytical answer", not err and "5.0" in body, body[:120])
    check("the result is addressable", "result_id" in body)

    # Values must arrive intact. TIMESTAMP WITH TIME ZONE shipped blank once — not an error, an
    # empty cell — which is the one failure the agent cannot see. See tests/render.rs.
    err, ts = tool("query", sql="SELECT observed_at FROM meters ORDER BY 1 LIMIT 1")
    first = [l for l in ts.splitlines() if l and "observed_at" not in l and "row(s)" not in l]
    got = first[0].strip() if first else ""
    check("a timestamp column is not silently blank", bool(got), repr(ts[:120]))
    check("...and carries the actual instant", got.startswith("2026-"), repr(got))
    err2, back = tool("query",
                      sql=f"SELECT count(*) c FROM meters WHERE observed_at = TIMESTAMPTZ '{got}'")
    check("...and what is printed is something the engine accepts back",
          not err2 and "\n1" in back, back[:90])

    rid = body.split("result_id ")[1].split()[0].rstrip(".,")
    err, again = tool("get_result", result_id=rid)
    check("the result can be fetched again", not err and "5.0" in again)

    err, body = tool("query", sql="CREATE TABLE scratch AS SELECT meter, count(*) n FROM meters GROUP BY 1; SELECT * FROM scratch ORDER BY meter")
    check("scratch tables work and a batch returns every result", not err and "lab" in body)

    err, body = tool("query", sql="SELECT * FROM scratch")
    check("scratch persists between calls", not err and "lab" in body)

    for sql, label in [
        ("UPDATE meters SET power_kw = 0", "UPDATE on loaded data refused"),
        ("DELETE FROM meters", "DELETE on loaded data refused"),
        ("DROP TABLE meters", "DROP of a loaded table refused"),
        ("INSERT INTO meters VALUES (NULL, 'x', 1)", "INSERT into loaded data refused"),
    ]:
        err, body = tool("query", sql=sql)
        check(label, err, body[:90])
    err, body = tool("query", sql="SELECT count(*) FROM meters")
    check("...and the data is untouched", not err and "8" in body, body[:60])

    for sql, label in [
        ("ATTACH '/etc/hostname' AS e", "ATTACH refused"),
        ("SELECT * FROM read_text('/etc/hostname')", "read_text blocked"),
        ("SELECT * FROM read_csv('/etc/passwd')", "read_csv blocked"),
        ("INSTALL httpfs", "INSTALL refused"),
        ("SET enable_external_access = true", "unlocking refused"),
        ("SELECT * FROM read_csv('https://example.invalid/x.csv')", "network blocked"),
    ]:
        err, body = tool("query", sql=sql)
        check(label, err, body[:90])

    err, body = tool("query", sql="ATTACH '/etc/hostname' AS e")
    check("a refusal says what is allowed", "SELECT" in body and "already loaded" in body, body[:120])

    err, body = tool("query", sql="SELECT i FROM range(100000) t(i)")
    check("results are capped and say so", not err and "TRUNCATED" in body, body[-120:])

    err, body = tool("query", sql="WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM t) SELECT sum(n) FROM t")
    check("a runaway query is stopped by the deadline", err and "deadline" in body, body[:100])
    err, body = tool("query", sql="SELECT count(*) FROM meters")
    check("...and the bench survives it", not err and "8" in body, body[:60])

    p.stdin.close(); p.wait(timeout=10)

    # REQ-8, in its own server because it needs a short hard grace. A deeply nested type
    # expression takes seconds to PLAN, and the planner never checks the interrupt flag — so
    # nothing inside the engine can stop it. The server takes the process away instead.
    q = subprocess.Popen([binary, *PLATFORM, data, "--deadline", "1", "--hard-grace", "1"],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                         bufsize=1, env=env)
    m = [0]
    def qcall(method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None: msg["params"] = params
        if not notify:
            m[0] += 1; msg["id"] = m[0]
        q.stdin.write(json.dumps(msg) + "\n"); q.stdin.flush()
        if notify: return
        while True:
            line = q.stdout.readline()
            if not line: raise SystemExit("blowup server closed the pipe")
            r = json.loads(line)
            if r.get("id") == msg["id"]: return r
    def qtool(name, **a):
        r = qcall("tools/call", {"name": name, "arguments": a})["result"]
        return r.get("isError", False), "".join(c.get("text", "") for c in r.get("content", []))

    qcall("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                         "clientInfo": {"name": "benchy-verify", "version": "0"}})
    qcall("notifications/initialized", notify=True)
    ctes = ["l0 AS (SELECT 1 m)"] + [f"l{i} AS (SELECT MAP([m],[m]) m FROM l{i-1})"
                                     for i in range(1, 19)]
    blowup = "WITH " + ", ".join(ctes) + " SELECT m FROM l18"

    qtool("query", sql="CREATE TABLE scratch AS SELECT 1 AS n")
    err, body = qtool("query", sql=blowup)
    check("a planner blowup is stopped by the supervisor", err and "planned" in body, body[:110])
    check("...and it says the bench was rebuilt", "rebuilt" in body, body[:110])
    err, body = qtool("query", sql="SELECT count(*) FROM meters")
    check("...the server survived and the bench still works", not err and "8" in body, body[:70])
    err, body = qtool("query", sql="SELECT n FROM scratch")
    check("...and the scratch really is gone, as the message said", err, body[:70])
    q.stdin.close(); q.wait(timeout=15)

    # REQ-4/REQ-5: an exec connector is a *subprocess* that speaks Arrow IPC. This drives the
    # shipped shell connector, which is the floor of the contract — if that works, the contract
    # is not asking much of a connector author.
    cfg = os.path.join(os.path.dirname(os.path.abspath(__file__)), "benchy.toml")
    if os.path.exists(cfg):
        e = subprocess.Popen([binary, *PLATFORM, "--config", cfg, "--deadline", "3"],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, bufsize=1, env=env)
        en = [0]
        def ecall(method, params=None, notify=False):
            msg = {"jsonrpc": "2.0", "method": method}
            if params is not None: msg["params"] = params
            if not notify:
                en[0] += 1; msg["id"] = en[0]
            e.stdin.write(json.dumps(msg) + "\n"); e.stdin.flush()
            if notify: return None
            while True:
                line = e.stdout.readline()
                if not line:
                    raise SystemExit("exec server closed the pipe\n" + e.stderr.read())
                m = json.loads(line)
                if m.get("id") == msg["id"]: return m
        def etool(name, **args):
            r = ecall("tools/call", {"name": name, "arguments": args})["result"]
            return r.get("isError", False), "".join(c.get("text", "") for c in r.get("content", []))

        ecall("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                             "clientInfo": {"name": "benchy-verify", "version": "0"}})
        ecall("notifications/initialized", notify=True)
        err, desc = etool("describe")
        check("a connector's table is in the catalogue", not err and "readings" in desc, desc[:120])
        check("...with no path to the connector in it", "connectors/" not in desc, desc[:120])
        err, body = etool("query", sql="SELECT count(*) c, count(DISTINCT meter) m FROM readings")
        check("...and its rows are queryable", not err and "4" in body, body[:90])
        err, body = etool("query",
                          sql="SELECT r.meter, avg(r.power_kw) FROM readings r "
                              "JOIN rooms m USING (meter) GROUP BY 1 ORDER BY 1")
        check("...and join a file source, which is the point of one bench",
              not err and "lab" in body, body[:90])
        e.stdin.close(); e.wait(timeout=15)

    # The sandbox is a claim about the process, not about SQL, so prove it separately.
    d = subprocess.run([binary, "doctor", "--probe"], capture_output=True, text=True, env=env)
    out = d.stdout
    landlock = next((l for l in out.splitlines() if l.startswith("landlock")), "")
    if "unavailable" in landlock:
        print("  ..   sandbox not available on this host (reported, not hidden)")
    else:
        check("the process really cannot read outside its workspace", "VERIFIED" in out,
              out.strip().replace("\n", " | ")[:160])
    print()
    if failures:
        print(f"{len(failures)} check(s) failed"); return 1
    print("all checks passed")
    return 0

if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(__doc__); raise SystemExit(2)
    raise SystemExit(main(sys.argv[1], sys.argv[2]))
