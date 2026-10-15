#!/usr/bin/env python3
"""A realistic benchy exec connector. Needs pyarrow (`pip install pyarrow`).

Copy this, replace `rows_for()` with your API call or database query, and declare the same
columns in benchy.toml. Everything else here is the contract, and it is short.

Read docs/EXEC-CONNECTOR-PROTOCOL.md for the whole thing. The four rules that actually bite:

1. **stdout is the Arrow stream and nothing else.** One `print()` to stdout corrupts it. All
   your logging goes to stderr.
2. **Finish the stream.** `writer.close()` emits the end-of-stream marker. Without it benchy
   refuses the load — a connector that dies between pages cuts at a clean message boundary,
   which every Arrow reader will happily accept as a short but valid stream.
3. **No rows is a complete stream with zero batches**, not empty stdout. Write the schema, then
   close. Exiting with nothing on stdout is an error, not an empty table.
4. **Narrowing values are untrusted.** The operator or a host sets them, and a host may take
   them from its own users. Parameterise your queries and URL-encode your paths. Never
   concatenate one into SQL, a shell command or a URL.
"""
import json
import os
import sys

import pyarrow as pa

SCHEMA = pa.schema([
    ("observed_at", pa.timestamp("us", tz="UTC")),
    ("meter", pa.string()),
    ("power_kw", pa.float64()),
])

DESCRIBE = {
    "protocol": [1],
    "name": "meters",
    "version": "0.1.0",
    # Declare only what you really do. `enforced` means no row outside the narrowing comes back;
    # benchy re-applies time and entity filters after ingest at every level anyway, so an honest
    # `best_effort` costs you nothing but bytes.
    "narrowing": {"time": "enforced", "entity": "enforced",
                  "columns": "unsupported", "params": "unsupported"},
    "estimate": "inband",
    "progress": True,
}


def log(level, message, **extra):
    print(json.dumps({"type": "log", "level": level, "message": message, **extra}),
          file=sys.stderr, flush=True)


def fail(code, message, hint=None):
    """`message` and `hint` go to the operator's log. Nothing of yours reaches the model."""
    event = {"type": "error", "code": code, "message": message}
    if hint:
        event["hint"] = hint
    print(json.dumps(event), file=sys.stderr, flush=True)
    sys.exit(1)


def rows_for(doc):
    """Replace this. Return (timestamps, meters, power) as three equal-length lists.

    `doc["narrowing"]` is what the operator or host asked for, already validated by benchy:
    entity ids are UTF-8, at most 1024 of them, none longer than 512 bytes.
    """
    import datetime as dt

    narrowing = doc.get("narrowing", {})
    wanted = set((narrowing.get("entities") or {}).get("ids") or [])
    start, end = None, None
    if t := narrowing.get("time"):
        start = dt.datetime.fromisoformat(t["start"]) if t.get("start") else None
        end = dt.datetime.fromisoformat(t["end"]) if t.get("end") else None

    base = dt.datetime(2026, 1, 1, tzinfo=dt.timezone.utc)
    ts, meters, power = [], [], []
    for i in range(8):
        at = base + dt.timedelta(minutes=15 * i)
        meter = "lab" if i % 2 == 0 else "office"
        # An unknown id yields no rows, never an error: an id list outlives the ids in it, and
        # an error per stale id fails the whole load and leaks which ids exist.
        if wanted and meter not in wanted:
            continue
        if (start and at < start) or (end and at >= end):
            continue
        ts.append(at)
        meters.append(meter)
        power.append(round(1.0 + i * 0.5, 2))
    return ts, meters, power


def fetch():
    doc = json.loads(sys.stdin.read() or "{}")
    if doc.get("op") not in (None, "fetch"):
        print(f"expected op=fetch, got {doc.get('op')!r}", file=sys.stderr)
        sys.exit(2)

    try:
        ts, meters, power = rows_for(doc)
    except Exception as e:                                    # noqa: BLE001
        fail("E_UNAVAILABLE", f"upstream read failed: {e!r}",
             hint="the meter service did not answer; try a shorter time range")

    # Declared `inband`, so benchy can refuse an over-budget load before we send the bytes.
    print(json.dumps({"type": "estimate", "rows": len(ts), "bytes": len(ts) * 32,
                      "exact": True}), file=sys.stderr, flush=True)

    table = pa.table({"observed_at": pa.array(ts, SCHEMA.field(0).type),
                      "meter": pa.array(meters, pa.string()),
                      "power_kw": pa.array(power, pa.float64())}, schema=SCHEMA)

    # `sys.stdout.buffer` — never `print`. Closing the writer is what emits the end-of-stream
    # marker, and benchy refuses a stream that lacks it.
    with pa.ipc.new_stream(sys.stdout.buffer, SCHEMA) as w:
        for chunk in table.to_batches(max_chunksize=10_000):
            w.write_batch(chunk)
    sys.stdout.buffer.flush()
    log("info", "done", rows=len(ts))


def main():
    op = sys.argv[1] if len(sys.argv) > 1 else ""
    if op == "describe":
        json.dump(DESCRIBE, sys.stdout)
    elif op == "fetch":
        fetch()
    else:
        # `estimate` lands here too: not implementing it is allowed, and the fetch goes ahead.
        print(f"usage: {os.path.basename(sys.argv[0])} describe|fetch", file=sys.stderr)
        sys.exit(2)


if __name__ == "__main__":
    main()
