#!/usr/bin/env python3
"""Measure what the bench costs an agent, against the same agent with a shell and the DuckDB CLI.

    cargo build --release
    python3 eval/fetch_uci.py eval/uci
    OPENROUTER_API_KEY=... python3 eval/run.py --fresh --duckdb /path/to/duckdb-1.5.5 --repeat 3

Two arms, one task set, the same public data (`eval/fetch_uci.py`: 24 real meters, every 15
minutes of 2014):

  bench   the agent gets benchy-mcp over MCP: describe, query, get_result, dialect
  shell   the agent gets a POSIX shell in a directory holding the data, with the DuckDB CLI.
          Each command runs in a Landlock jail (`eval/jail.py`): the data read-only, one
          scratch directory, no TCP. A model with an unconfined shell on your machine is a
          model that can delete your files. Linux 6.7 or later; the run refuses otherwise.

Both arms get the same system prompt, and guidance of the same kind about their own surface: benchy's MCP instructions for the bench, an equivalent paragraph for the shell — the
DuckDB version and dialect, how to find the data, how to keep tables between calls. Every task
starts fresh: a new bench, or a new scratch directory.

The primary measurement is **steps**: model calls until the answer. Tokens and wall time are
recorded too. Correctness is recorded as a secondary signal and never claimed: thirteen tasks
cannot support a statement about accuracy, and the ground truth is one expert's SQL, not a
consensus.

What this is not: a benchmark of models, a claim that SQL beats Python, or evidence about any
dataset but this one.
"""
import argparse
import datetime
import getpass
import hashlib
import http.client
import json
import os
import platform
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
JAIL = os.path.join(ROOT, "eval", "jail.py")
ENDPOINT = "https://openrouter.ai/api/v1/chat/completions"
TEMPERATURE = 0       # requested; a reasoning model may not honour it
TOOL_OUTPUT = 20000   # what a tool result may put into the context
KEPT_OUTPUT = 4000    # what the committed transcript keeps of each

SYSTEM = """You are answering questions about electricity meter data.

Work with the tools you have. When you are confident, reply with your reasoning followed by a
final line in exactly this form, and nothing after it:

ANSWER: <the value>

Give the bare value on that line — a number without units, or a name or date exactly as asked
for. Do not explain on the ANSWER line."""


# --------------------------------------------------------------------------- the two arms

class Bench:
    """benchy-mcp over MCP stdio."""

    name = "bench"

    def __init__(self, a):
        self.p = subprocess.Popen([a.binary, a.data], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                  text=True, bufsize=1)
        self.n = 0
        init = self._call("initialize", {"protocolVersion": "2026-07-28", "capabilities": {},
                                         "clientInfo": {"name": "benchy-eval", "version": "0"}})
        self.instructions = init["result"].get("instructions", "")
        self.private = [(os.path.abspath(a.data), "<data>"), (ROOT, "<repo>")]
        self._call("notifications/initialized", notify=True)
        listed = self._call("tools/list", {})["result"]["tools"]
        self.tools = [{"type": "function",
                       "function": {"name": t["name"],
                                    "description": t.get("description", ""),
                                    "parameters": t.get("inputSchema", {"type": "object"})}}
                      for t in listed]

    def _call(self, method, params=None, notify=False):
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        if not notify:
            self.n += 1
            msg["id"] = self.n
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()
        if notify:
            return None
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise SystemExit("benchy closed the pipe")
            m = json.loads(line)
            if m.get("id") == msg["id"]:
                return m

    def call(self, name, args):
        r = self._call("tools/call", {"name": name, "arguments": args})["result"]
        return "".join(c.get("text", "") for c in r.get("content", []))

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=10)
        except Exception:                                             # noqa: BLE001
            self.p.kill()


class Shell:
    """A shell and the DuckDB CLI, jailed. See the module docstring."""

    name = "shell"
    TIMEOUT = 60

    def __init__(self, a):
        self.data = os.path.abspath(a.data)
        self.duckdb = a.duckdb
        self.work = tempfile.mkdtemp(prefix="benchy-eval-shell-")
        # A model with a shell sees real paths — a symlink's target, its own PATH — and may type
        # them back; the transcript keeps placeholders, longest path first.
        self.private = [(self.data, "<data>"), (self.work, "<work>"), (ROOT, "<repo>")]
        cli = os.path.dirname(os.path.abspath(a.duckdb))
        if cli not in ("/bin", "/usr/bin", "/usr/local/bin"):
            self.private.append((cli, "<duckdb-dir>"))
        for f in sorted(os.listdir(self.data)):
            os.symlink(os.path.join(self.data, f), os.path.join(self.work, f))
        # The shell's counterpart of benchy's MCP instructions: the same kind of guidance, about
        # its own surface.
        self.instructions = (
            "A shell: the data files are in the current directory — `ls` lists them, and "
            "`duckdb -c \"DESCRIBE SELECT * FROM 'readings.csv'\"` shows a CSV's columns. SQL is "
            f"DuckDB {a.duckdb_version}, installed as `duckdb` — QUALIFY, GROUP BY ALL, "
            "SELECT * EXCLUDE, ASOF JOIN and PIVOT all work, // is integer division, and lists "
            "are 1-indexed; it reads a CSV directly, as in SELECT * FROM 'readings.csv'. "
            "`duckdb -c` works in a database that is gone when the command ends; "
            "`duckdb work.duckdb -c ...` keeps the tables you create in that file for later "
            f"commands. Output is cut at {TOOL_OUTPUT:,} characters, and each command may take "
            f"{self.TIMEOUT} seconds. Read an error before retrying: DuckDB's errors usually "
            "say what to change.")
        self.tools = [{"type": "function", "function": {
            "name": "sh",
            "description": (f"Run a shell command in the data directory and return what it "
                            f"prints, stdout and stderr, cut at {TOOL_OUTPUT:,} characters."),
            "parameters": {"type": "object",
                           "properties": {"command": {"type": "string"}},
                           "required": ["command"]}}}]

    def call(self, name, args):
        if name != "sh":
            return f"no such tool: {name}"
        return jailed(self, str(args.get("command", "")))

    def close(self):
        shutil.rmtree(self.work, ignore_errors=True)


def jailed(shell, command):
    """One command in the jail, as the shell arm runs it."""
    env = {"PATH": f"{os.path.dirname(shell.duckdb)}:/usr/bin:/bin", "HOME": shell.work,
           "TMPDIR": shell.work, "LANG": "C.UTF-8"}
    p = subprocess.Popen(
        [sys.executable, JAIL, "--read", shell.data, "--write", shell.work,
         "--exec", shell.duckdb, "--", "sh", "-c", command],
        cwd=shell.work, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, start_new_session=True)
    try:
        out, _ = p.communicate(timeout=shell.TIMEOUT)
        tail = f"\n[exit {p.returncode}]" if p.returncode else ""
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        out, _ = p.communicate()
        tail = f"\n[stopped after {shell.TIMEOUT}s]"
    # The DuckDB CLI colours its tables even into a pipe, and a column name wrapped in escape codes
    # next to its type reads as the type: the model gets the text a terminal would show.
    out = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", out)
    return out.decode("utf-8", "replace") + tail


ARMS = {"bench": Bench, "shell": Shell}


# --------------------------------------------------------------------------- the loop

def chat(model, messages, tools, key, retries=4):
    body = json.dumps({"model": model, "messages": messages, "tools": tools,
                       "temperature": TEMPERATURE, "usage": {"include": True}}).encode()
    req = urllib.request.Request(ENDPOINT, data=body, headers={
        "Authorization": f"Bearer {key}", "Content-Type": "application/json",
        "X-Title": "benchy-mcp eval"})
    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=300) as r:
                return json.load(r)
        # A dropped connection is as transient as a 5xx: retried, never the end of a run.
        except (urllib.error.URLError, http.client.HTTPException, ConnectionError,
                TimeoutError) as e:
            detail = ""
            if isinstance(e, urllib.error.HTTPError):
                detail = e.read()[:300].decode("utf-8", "replace")
                if e.code < 500 and e.code != 429:
                    raise SystemExit(f"model call failed ({e.code}): {detail}")
            if attempt == retries - 1:
                raise SystemExit(f"model call failed after {retries} tries: {e} {detail}")
            time.sleep(2 ** attempt)
    raise SystemExit("unreachable")


def redact(text, private):
    """What a committed transcript keeps: the machine's own paths, user name and host name
    replaced, so results can be published. Counts and answers are never touched."""
    if not text:
        return text
    for path, placeholder in private:
        text = text.replace(path, placeholder)
    # A shell arm's model may run `hostname` or `uname -a`; the machine's name is not the
    # results'.
    host = socket.gethostname().split(".")[0]
    if host:
        text = re.sub(rf"\b{re.escape(host)}\b", "<host>", text)
    return re.sub(rf"\b{re.escape(getpass.getuser())}\b", "<user>", text)


def run_task(arm, task, model, key, max_steps):
    messages = [{"role": "system", "content": SYSTEM}]
    if arm.instructions:
        messages.append({"role": "system", "content": arm.instructions})
    messages.append({"role": "user", "content": task["question"].strip()})

    steps = tok_in = tok_out = tok_cached = 0
    cost = 0.0
    started = time.time()
    answer = None
    capped = True
    kept = []
    for _ in range(max_steps):
        r = chat(model, messages, arm.tools, key)
        steps += 1
        usage = r.get("usage") or {}
        tok_in += usage.get("prompt_tokens", 0)
        tok_out += usage.get("completion_tokens", 0)
        tok_cached += (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
        cost += float(usage.get("cost") or 0)
        choice = r["choices"][0]["message"]
        messages.append(choice)
        kept.append({"role": "assistant", "content": redact(choice.get("content"), arm.private),
                     "tool_calls": [{"name": c["function"]["name"],
                                     "arguments": redact(c["function"].get("arguments"),
                                                         arm.private)}
                                    for c in choice.get("tool_calls") or []]})
        calls = choice.get("tool_calls") or []
        if not calls:
            answer = choice.get("content") or ""
            capped = False
            break
        for c in calls:
            try:
                args = json.loads(c["function"].get("arguments") or "{}")
            except json.JSONDecodeError:
                args = {}
            try:
                out = arm.call(c["function"]["name"], args)
            except Exception as e:                                    # noqa: BLE001
                out = f"tool failed: {e!r}"
            messages.append({"role": "tool", "tool_call_id": c["id"],
                             "content": out[:TOOL_OUTPUT]})
            kept.append({"role": "tool", "content": redact(out[:KEPT_OUTPUT], arm.private),
                         "cut": max(0, len(out) - KEPT_OUTPUT)})
    return {"steps": steps, "tokens_in": tok_in, "tokens_out": tok_out,
            "tokens_cached": tok_cached, "cost": cost,
            "seconds": round(time.time() - started, 1), "raw": redact(answer or "", arm.private),
            # Running out of budget is a different outcome from answering wrongly, and the
            # report has to be able to tell them apart — a surface that makes a task
            # unaffordable has failed differently from one that makes it easy to get wrong.
            "capped": capped,
            "answer": parse_answer(answer or ""),
            "transcript": kept}


def parse_answer(text):
    m = None
    for m in re.finditer(r"^\s*\**ANSWER:?\**\s*(.+?)\s*$", text, re.MULTILINE | re.IGNORECASE):
        pass
    return m.group(1).strip().strip("`*") if m else None


NUMBER = re.compile(r"-?(?:\d+(?:\.\d+)?|\.\d+)")
GROUPED = re.compile(r"-?\d{1,3}(?:,\d{3})+(?:\.\d+)?")   # a comma only between thousands
MOMENT = re.compile(r"\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2}(?::\d{2})?)?")


def score(task, got, truth):
    """Whether an answer is the ground truth, and only it. A number must be the whole answer and
    within the task's own tolerance; a name must be exactly the one; a date or time is compared
    at the precision the question asked for. A hedge — two candidates — is never right."""
    if got is None:
        return False
    got = got.strip().strip("\"'`*").strip()
    if task["type"] == "number":
        got = got.rstrip(".")
        if GROUPED.fullmatch(got):
            got = got.replace(",", "")
        if not NUMBER.fullmatch(got):
            return False    # "1,2" is two numbers, not twelve
        return abs(float(got) - float(truth)) <= task.get("tolerance", 0) + 1e-9
    got = got.rstrip(".")
    want = str(truth).strip()
    if MOMENT.fullmatch(want) and MOMENT.fullmatch(got):
        # Seconds the question did not ask for change nothing, and neither does a `T`:
        # `2014-02-04T19:15:00` is `2014-02-04 19:15`.
        got = got.replace("T", " ")
        return got[:len(want)] == want and set(got[len(want):]) <= set(":0")
    return got.lower() == want.lower()


# --------------------------------------------------------------------------- ground truth

def ground_truth(a, tasks):
    """One expert SQL answer per task, evaluated against the same data every arm sees — and the
    instructions benchy gives, which are part of what is measured."""
    b = Bench(a)
    a.bench_instructions = b.instructions
    out = {}
    for t in tasks:
        body = b.call("query", {"sql": t["sql"].strip()})
        lines = [line for line in body.splitlines() if line.strip()]
        if len(lines) < 2 or "row(s)" in lines[1]:
            b.close()
            raise SystemExit(f"ground truth for `{t['id']}` produced no row:\n{body[:300]}")
        out[t["id"]] = lines[1].strip()
    b.close()
    return out


def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def digest(*parts):
    return hashlib.sha256("\x00".join(parts).encode()).hexdigest()


def version_of(argv):
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=30).stdout.strip()
        return out.splitlines()[0] if out else "unknown"
    except OSError:
        return "unknown"


def source_commit():
    """The commit the binary is assumed built from, so a report can name the version it measured:
    the version string stays 0.1.0 across every change. Marked when the server's sources differ
    from that commit; changes to the harness itself do not count."""
    try:
        head = subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"],
                              capture_output=True, text=True, timeout=30).stdout.strip()
        dirty = subprocess.run(["git", "-C", ROOT, "status", "--porcelain", "--", "src",
                                "Cargo.toml", "Cargo.lock", "build.rs"],
                               capture_output=True, text=True, timeout=30).stdout.strip()
    except OSError:
        return "unknown"
    return (head + (" with uncommitted changes to the server" if dirty else "")) or "unknown"


def preflight(a):
    """Refuse to run what would not be measured: a shell arm whose jail cannot apply would
    answer every command with the jail's refusal, and the results would look like a run."""
    shell = Shell(a)
    try:
        said = jailed(shell, "echo jailed")
    finally:
        shell.close()
    if said.strip() != "jailed":
        raise SystemExit(f"the shell arm's jail does not work here, so it is not run:\n{said}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", default="deepseek/deepseek-v4-flash-0731",
                    help="OpenRouter model ids, comma-separated. The published results name "
                         "theirs; this is what a new run uses unless told otherwise")
    ap.add_argument("--binary", default=os.path.join(ROOT, "target/release/benchy-mcp"))
    ap.add_argument("--duckdb", default=shutil.which("duckdb"),
                    help="the DuckDB CLI the shell arm gets (default: the one on PATH)")
    ap.add_argument("--data", default=os.path.join(ROOT, "eval/uci"))
    ap.add_argument("--tasks", default=os.path.join(ROOT, "eval/tasks.toml"))
    ap.add_argument("--only", help="comma-separated task ids to run")
    ap.add_argument("--arms", default="bench,shell")
    ap.add_argument("--repeat", type=int, default=1, help="runs of each task per arm and model")
    ap.add_argument("--max-steps", type=int, default=25)
    ap.add_argument("--out", default=os.path.join(ROOT, "eval/results"))
    ap.add_argument("--resume", action="store_true",
                    help="continue the results in --out, measured with the same setup")
    ap.add_argument("--fresh", action="store_true", help="replace the results in --out")
    a = ap.parse_args()

    key = os.environ.get("OPENROUTER_API_KEY")
    if not key:
        raise SystemExit("set OPENROUTER_API_KEY")
    if not os.path.isfile(os.path.join(a.data, "readings.csv")):
        raise SystemExit(f"no data in {a.data}: run  python3 eval/fetch_uci.py {a.data}")
    a.duckdb_version = "unknown"
    if "shell" in a.arms.split(","):
        if not a.duckdb or not os.path.isfile(a.duckdb):
            raise SystemExit("the shell arm needs the DuckDB CLI: put `duckdb` on PATH or pass "
                             "--duckdb")
        a.duckdb = os.path.realpath(a.duckdb)
        a.duckdb_version = version_of([a.duckdb, "--version"])
        if not sys.platform.startswith("linux"):
            raise SystemExit("the shell arm's jail is Landlock, which is Linux-only; it will "
                             "not hand a model an unconfined shell")
        preflight(a)

    tasks = tomllib.load(open(a.tasks, "rb"))["task"]
    if a.only:
        keep = set(a.only.split(","))
        tasks = [t for t in tasks if t["id"] in keep]
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import fetch_uci
    data_sha = sha256_of(os.path.join(a.data, "readings.csv"))
    if data_sha != fetch_uci.READINGS_SHA256:
        raise SystemExit(f"{a.data}/readings.csv is not the cut the tasks were written against "
                         "(its SHA-256 differs): run eval/fetch_uci.py again")
    truth = ground_truth(a, tasks)
    print(f"ground truth: {json.dumps(truth)}\n")
    os.makedirs(a.out, exist_ok=True)
    # What a run was measured with: resumed only onto the same, and published with the results.
    # A version string does not change when the code does, so the binary itself is hashed, and so
    # are the data, the tasks and every word of guidance each arm gets.
    shell_guidance = ""
    if "shell" in a.arms.split(","):
        probe = Shell(a)
        shell_guidance = probe.instructions
        probe.close()
    setup = {"benchy": version_of([a.binary, "--version"]),
             "source": source_commit(),
             "binary_sha256": sha256_of(a.binary),
             "duckdb_cli": a.duckdb_version,
             "kernel": f"{platform.system()} {platform.release()} {platform.machine()}",
             "max_steps": a.max_steps, "temperature_requested": TEMPERATURE,
             "data_sha256": data_sha,
             "tasks_sha256": sha256_of(a.tasks),
             "guidance_sha256": digest(SYSTEM, a.bench_instructions, shell_guidance)}

    for model in a.models.split(","):
        path = os.path.join(a.out, model.replace("/", "_") + ".json")
        runs = []
        model_truth = dict(truth)
        if os.path.exists(path):
            if not (a.resume or a.fresh):
                raise SystemExit(f"{path} exists: pass --resume to continue it, or --fresh to "
                                 "replace it")
            if a.resume:
                done = json.load(open(path))
                if done.get("setup") != setup:
                    raise SystemExit(f"{path} was measured with {done.get('setup')}, not "
                                     f"{setup}: it cannot be continued")
                # Runs of tasks outside --only stay; a run is dropped only if its task's ground
                # truth has changed since it was scored.
                runs = [r for r in done["runs"]
                        if truth.get(r["task"], done["truth"].get(r["task"]))
                        == done["truth"].get(r["task"])]
                model_truth = {**done["truth"], **truth}
                # Scored again: the scorer may have changed since, even where nothing else did.
                by_id = {t["id"]: t for t in tomllib.load(open(a.tasks, "rb"))["task"]}
                for r in runs:
                    if r["task"] in by_id:
                        r["correct"] = score(by_id[r["task"]], r["answer"], r["truth"])
        have = {(r["task"], r["arm"], r["repeat"]) for r in runs}
        for arm_name in a.arms.split(","):
            print(f"--- {model} / {arm_name} ---")
            for t in tasks:
                for rep in range(a.repeat):
                    if (t["id"], arm_name, rep) in have:
                        continue
                    arm = ARMS[arm_name](a)
                    try:
                        r = run_task(arm, t, model, key, a.max_steps)
                    finally:
                        arm.close()
                    r.update({"task": t["id"], "arm": arm_name, "repeat": rep,
                              "measured": datetime.date.today().isoformat(),
                              "truth": truth[t["id"]],
                              "correct": score(t, r["answer"], truth[t["id"]])})
                    runs.append(r)
                    mark = "ok  " if r["correct"] else ("CAP " if r["capped"] else "MISS")
                    print(f"  {t['id']:14s} {r['steps']:2d} steps  "
                          f"{r['tokens_in'] + r['tokens_out']:8,d} tok  {r['seconds']:6.1f}s  "
                          f"{mark} {str(r['answer'])[:40]}", flush=True)
                    # Written after every run, so a crash never costs a paid run twice.
                    with open(path + ".partial", "w") as fh:
                        json.dump({"model": model,
                                   "measured": datetime.date.today().isoformat(),
                                   "setup": setup, "truth": model_truth, "runs": runs},
                                  fh, indent=1)
                    os.replace(path + ".partial", path)
            print()
        print(f"wrote {path}")


if __name__ == "__main__":
    main()
