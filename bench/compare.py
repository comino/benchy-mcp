#!/usr/bin/env python3
"""Compare two `bench/ingest.py --json` files, before and after a change.

    python3 bench/compare.py before.json after.json [metric ...]

For each scenario both files have, prints each metric's minimum over the runs, before and after,
and the change. The default metrics are the ones that decide whether a change is a win:
instructions (deterministic, the primary one), cycles, CPU time of every process together, the
loader's CPU, and the peak RSS of the loader and the engine. Wall time is left out by default:
on a shared machine it measures the neighbours.
"""
import json
import sys

DEFAULT = ["instructions", "cycles", "cpu.total_ms", "cpu.loader_ms", "cpu.engine_ms",
           "cpu.server_ms", "rss.loader_mb", "rss.engine_mb"]


def mins(runs, metric):
    vals = []
    for r in runs:
        if metric == "cpu.total_ms":
            v = sum(x for k, x in r.items() if k.startswith("cpu."))
        else:
            v = r.get(metric)
        if v is not None:
            vals.append(v)
    return min(vals) if vals else None


def main(a, b, metrics):
    before, after = json.load(open(a))["results"], json.load(open(b))["results"]
    for name in before:
        if name not in after or not before[name] or not after[name]:
            continue
        print(f"\n{name}")
        for m in metrics:
            x, y = mins(before[name], m), mins(after[name], m)
            if x is None or y is None:
                continue
            change = f"{(y - x) / x * 100:+.1f}%" if x else "-"
            print(f"  {m:24} {x:>16,.1f} {y:>16,.1f} {change:>8}")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2], sys.argv[3:] or DEFAULT)
