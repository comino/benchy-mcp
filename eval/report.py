#!/usr/bin/env python3
"""Render eval/REPORT.md from the raw results `eval/run.py` wrote to eval/results/. No model calls.

    python3 eval/report.py
"""
import collections
import glob
import json
import os
import re
import statistics
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

ATTRIBUTION = (
    "Data: Artur Trindade, *ElectricityLoadDiagrams20112014*, UCI Machine Learning Repository "
    "(2015), https://doi.org/10.24432/C58C86, licensed CC BY 4.0 "
    "(https://creativecommons.org/licenses/by/4.0/). Modified: 24 of the 370 clients, the "
    "intervals stamped in 2014, one row per meter and interval; excerpts appear in the "
    "committed transcripts.")


LOOK = re.compile(r"describe|summarize|pragma|show tables|select \* from '[^']*'\s+limit\s+\d+",
                  re.I)
WORK = re.compile(r"\b(sum|avg|count|min|max|group by|where)\b", re.I)
FAILED = re.compile(r"Error|error:|does not exist|not found")
WORDS = ["no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
         "eleven", "twelve", "thirteen", "fourteen", "fifteen"]


def word(n):
    return WORDS[n] if n < len(WORDS) else str(n)


def kinds(run):
    """Where a run's steps went. Looking: benchy's `describe` or `dialect`; a shell command that
    lists the directory, or reads the data file without DuckDB, or runs only a `DESCRIBE`, a
    `SUMMARIZE` or a `SELECT * … LIMIT n` preview. Computing: every other tool call — reading a
    file the model wrote itself included. Answering: the one call with no tool. And how many
    tool outputs were an error."""
    c = collections.Counter()
    for m in run["transcript"]:
        if m["role"] == "tool":
            if FAILED.search(m.get("content") or ""):
                c["errors"] += 1
            continue
        for call in m["tool_calls"]:
            args = call.get("arguments") or ""
            if call["name"] == "sh":
                if "duckdb" in args:
                    looking = bool(LOOK.search(args)) and not WORK.search(args)
                else:
                    looking = bool(re.search(r"\bls\b", args)) or "readings.csv" in args
            else:
                looking = call["name"] in ("describe", "dialect")
            c["looking" if looking else "computing"] += 1
        if not m["tool_calls"]:
            c["answering"] += 1
    return c


def main():
    tasks = tomllib.load(open(os.path.join(ROOT, "eval/tasks.toml"), "rb"))["task"]
    paths = sorted(glob.glob(os.path.join(ROOT, "eval/results/*.json")))
    results = [json.load(open(p)) for p in paths]
    if not results:
        raise SystemExit("no results in eval/results/: run eval/run.py first")
    arms = []
    for res in results:
        for r in res["runs"]:
            if r["arm"] not in arms:
                arms.append(r["arm"])
    repeats = max(r["repeat"] for res in results for r in res["runs"]) + 1
    setup = results[0].get("setup", {})

    def runs(res, arm, task=None):
        return [r for r in res["runs"] if r["arm"] == arm and (task is None or r["task"] == task)]

    def short(res):
        return res["model"].split("/")[-1]

    lines = [
        "# Eval — what the bench costs an agent",
        "",
        "Rendered by `eval/report.py` from the raw results in `eval/results/`, which hold every "
        "run's answer, counts and transcript — tool output cut at 4,000 characters, with the "
        "machine's own paths and user name replaced by placeholders. **Regenerate it rather than "
        "citing it**: it describes these models, this data and these tasks.",
        "",
        "```sh",
        "cargo build --release",
        "python3 eval/fetch_uci.py eval/uci                  # 261 MB download, 36 MB kept",
        "python3 eval/test_eval.py                           # the scorer, and the jail holds",
        "OPENROUTER_API_KEY=... python3 eval/run.py --fresh --duckdb /path/to/duckdb \\",
        f"    --models {','.join(r['model'] for r in results)} --repeat {repeats}",
        "python3 eval/report.py",
        "```",
        "",
        "The shell arm needs Linux 6.7 or later, for its jail, and the DuckDB CLI; the run "
        "refuses rather than measure a shell it cannot confine.",
        "",
        "- **Data** — 24 real meters, every 15 minutes of 2014, 840,960 rows; its quirks left in "
        "(see the traps below, and the data's source at the end)",
        f"- **Tasks** — {len(tasks)}, in `eval/tasks.toml`, each run {repeats} times per model "
        f"and arm; step cap {setup.get('max_steps', '?')}",
        "- **Arms** — `bench`: benchy-mcp over MCP, with its own instructions. `shell`: a POSIX "
        "shell with the DuckDB CLI in a directory holding the data, jailed by Landlock, with a "
        "paragraph of the same kind about its own surface — the DuckDB dialect, how to find the "
        "data, how to keep tables between calls; its output reaches the model as a terminal would "
        "show it, without the CLI's colour codes",
        f"- **Setup** — {setup.get('benchy', '?')}"
        + (f" built from `{setup['source']}`" if setup.get("source") else "")
        + f"; DuckDB CLI {setup.get('duckdb_cli', '?')}; "
        f"{setup.get('kernel', '?')}; temperature {setup.get('temperature_requested', '?')} "
        "requested",
        f"- **Measured** — {', '.join(sorted({r['measured'] for r in results}))}",
        "",
        "## Steps",
        "",
        "Model calls until the answer, summed over every task and repeat — the measurement.",
        "",
        "| model | " + " | ".join(f"`{a}` steps" for a in arms) + " | " +
        " | ".join(f"`{a}` correct" for a in arms) + " |",
        "|---|" + "---:|" * (2 * len(arms)),
    ]
    for res in results:
        steps = [sum(r["steps"] for r in runs(res, a)) for a in arms]
        right = [f"{sum(r['correct'] for r in runs(res, a))}/{len(runs(res, a))}" for a in arms]
        lines.append(f"| `{res['model']}` | " + " | ".join(str(s) for s in steps) + " | " +
                     " | ".join(right) + " |")
    if set(arms) >= {"bench", "shell"}:
        lines.append("")
        for res in results:
            b = sum(r["steps"] for r in runs(res, "bench"))
            s = sum(r["steps"] for r in runs(res, "shell"))
            if s:
                fewer = s - b
                more = "fewer" if fewer >= 0 else "more"
                lines.append(f"- `{res['model']}`: the bench took **{abs(fewer)} {more} steps** "
                             f"than the shell's {s} ({abs(fewer) / s * 100:.0f}% {more}).")

        # A sum over three repeats hides how far apart the repeats were; say it.
        lines += ["", "Steps in each repeat, all tasks summed — how far apart the runs were:", "",
                  "| model | " + " | ".join(f"`{a}`" for a in arms) + " |",
                  "|---|" + "---|" * len(arms)]
        for res in results:
            cells = []
            for a in arms:
                per = collections.Counter()
                for r in runs(res, a):
                    per[r["repeat"]] += r["steps"]
                cells.append(", ".join(str(per[k]) for k in sorted(per)))
            lines.append(f"| `{res['model']}` | " + " | ".join(cells) + " |")

        named = [t["id"] for t in tasks if t.get("named_by_describe")]
        if named:
            rest = [t["id"] for t in tasks if not t.get("named_by_describe")]
            lines += ["", "## Reading against computing", "",
                      f"`describe` states the answer to {word(len(named))} of the tasks outright — "
                      + ", ".join(f"`{t}`" for t in named) + " — because it reports dead, "
                      "late-starting and stopped series and the clock changes. On those the bench "
                      "arm reads an answer that the shell arm has to compute: the nearest single "
                      "command it has, DuckDB's `SUMMARIZE`, gives each column's range and null "
                      "share, not runs of zero per series. The other "
                      f"{word(len(rest))} are computations `describe` does not answer, and are the "
                      "fairer comparison of the two surfaces.", "",
                      "| model | tasks | `bench` steps | `shell` steps | fewer with the bench | "
                      "`bench` correct | `shell` correct |",
                      "|---|---|---:|---:|---:|---:|---:|"]
            for res in results:
                for label, ids in ((f"named by `describe` ({len(named)})", named),
                                   (f"computed ({len(rest)})", rest)):
                    b = [r for r in runs(res, "bench") if r["task"] in ids]
                    s_ = [r for r in runs(res, "shell") if r["task"] in ids]
                    bs, ss = sum(r["steps"] for r in b), sum(r["steps"] for r in s_)
                    pct = f"{(ss - bs) / ss * 100:.0f}%" if ss else "—"
                    lines.append(f"| `{short(res)}` | {label} | {bs} | {ss} | {pct} | "
                                 f"{sum(r['correct'] for r in b)}/{len(b)} | "
                                 f"{sum(r['correct'] for r in s_)}/{len(s_)} |")

        lines += ["", "## Where the steps go", "",
                  "*Looking* is finding the data and its shape: benchy's `describe`, or a shell "
                  "command that lists the directory or reads the data file without DuckDB — `ls`, "
                  "`head readings.csv` — or runs only a `DESCRIBE`, a `SUMMARIZE` or a `SELECT * … "
                  "LIMIT n` preview. *Computing* is every other tool call, reading a file the model "
                  "wrote itself included. *Answering* is the call that ends a run. The last column counts tool "
                  "outputs that were an error — a mistyped column, a quoting slip — each of which "
                  "cost the call after it.", "",
                  "| model | arm | steps | looking | computing | answering | outputs that were "
                  "an error |", "|---|---|---:|---:|---:|---:|---:|"]
        for res in results:
            for a in arms:
                rs = runs(res, a)
                total = collections.Counter()
                for r in rs:
                    total += kinds(r)
                lines.append(f"| `{short(res)}` | `{a}` | {sum(r['steps'] for r in rs)} | "
                             f"{total['looking']} | {total['computing']} | {total['answering']} "
                             f"| {total['errors']} |")

        # The rows that go the other way are the most informative in the table, and the ones a
        # reader skims past in a sum. Name them.
        against = []
        for res in results:
            for t in tasks:
                b = [r["steps"] for r in runs(res, "bench", t["id"])]
                s_ = [r["steps"] for r in runs(res, "shell", t["id"])]
                if b and s_ and statistics.median(b) > statistics.median(s_):
                    against.append(f"`{t['id']}` for `{short(res)}` "
                                   f"({statistics.median(b):g} against {statistics.median(s_):g})")
        lines += ["", "**Against the grain** — tasks where the bench took more steps than the "
                  "shell, by the median over repeats: " + ("; ".join(against) if against else
                                                          "none") + "."]

    lines += ["", "## Cost", "",
              "Tokens are summed over every step, so the arm that re-reads its context most pays "
              "most; `cached` is what the provider reported serving from its prompt cache.", "",
              "| model | arm | tokens in | cached | tokens out | wall | cost (USD) | "
              "ran out of steps |",
              "|---|---|---:|---:|---:|---:|---:|---:|"]
    for res in results:
        for a in arms:
            rs = runs(res, a)
            lines.append(
                f"| `{res['model']}` | `{a}` | {sum(r['tokens_in'] for r in rs):,} | "
                f"{sum(r.get('tokens_cached', 0) for r in rs):,} | "
                f"{sum(r['tokens_out'] for r in rs):,} | {sum(r['seconds'] for r in rs):.0f}s | "
                f"{sum(r.get('cost', 0) for r in rs):.2f} | {sum(r['capped'] for r in rs)} |")

    lines += ["", "## Per task", "",
              "Steps — the median over repeats, with the least and most — and how many answers "
              "matched the ground truth.", "",
              "| task | " + " | ".join(f"`{short(res)}` {a}"
                                      for res in results for a in arms) + " |",
              "|---|" + "---|" * (len(results) * len(arms))]
    for t in tasks:
        cells = []
        for res in results:
            for a in arms:
                rs = runs(res, a, t["id"])
                if not rs:
                    cells.append("—")
                    continue
                st = [r["steps"] for r in rs]
                spread = f"{min(st)}–{max(st)}" if min(st) != max(st) else f"{min(st)}"
                ok = sum(r["correct"] for r in rs)
                capped = sum(r["capped"] for r in rs)
                cells.append(f"{statistics.median(st):g} ({spread}) / {ok} of {len(rs)}"
                             + (f", {capped} capped" if capped else ""))
        lines.append(f"| `{t['id']}` | " + " | ".join(cells) + " |")

    truth = results[0]["truth"]
    lines += ["", "## Ground truth", "",
              "One expert SQL answer per task (`eval/tasks.toml`), evaluated against the same "
              "data every arm sees — not a consensus between models. A number is right within its "
              "task's tolerance and no further; a name or date must be exactly the one, and a "
              "hedge between two candidates is wrong.", "",
              "| task | answer | tolerance | the trap |", "|---|---|---|---|"]
    for t in tasks:
        tol = t.get("tolerance", "—") if t["type"] == "number" else "exact"
        lines.append(f"| `{t['id']}` | `{truth.get(t['id'], '')}` | {tol} | "
                     f"{t.get('trap', '')} |")

    lines += [
        "", "## Reading this", "",
        "**Steps are the measurement.** Correctness is recorded and *not claimed*: "
        f"{word(len(tasks))} tasks "
        "cannot support a statement about accuracy, and a miss is one model's bad afternoon "
        "rather than evidence about a surface.",
        "",
        f"**The sample is small.** {word(len(results)).capitalize()} "
        f"model{'s' if len(results) != 1 else ''}, {word(len(tasks))} tasks, one dataset, one "
        "machine. Repeats are "
        "at temperature 0 as requested, and some models then repeat themselves step for step, "
        "so for those three runs are closer to one; a reasoning model may not honour the "
        "temperature at all. The spread per task is above.",
        "",
        "**What differs between the arms is the surface.** The same model, the same data, the "
        "same engine — DuckDB — and guidance of the same kind about each surface: a catalogue, "
        "tables that persist between calls and results that come back bounded, against a "
        "command line where each `duckdb` starts empty unless told to keep a file. The surface "
        "includes what `describe` reports about the data's shape, which no one shell command "
        "does; the split above shows how much of the difference that is.",
        "",
        "**What this does not measure**: other models, other data, larger data, or sessions "
        "where scratch tables pay off across questions. Every task here starts fresh.",
        "",
        "## The data",
        "",
        ATTRIBUTION,
    ]
    out = os.path.join(ROOT, "eval/REPORT.md")
    with open(out, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
