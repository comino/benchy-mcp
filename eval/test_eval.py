#!/usr/bin/env python3
"""The harness's own checks, with no model and no key: `python3 eval/test_eval.py`.

The scorer passes the right answer and only it, and the shell arm's jail holds against what a
model with a shell could try. Linux 6.7 or later, like the shell arm itself.
"""
import os
import shutil
import sys
import tempfile
import types

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import run  # noqa: E402


def check_score():
    energy = {"type": "number", "tolerance": 0.3}
    assert run.score(energy, "15699.92", "15699.917")
    assert run.score(energy, "15,699.9", "15699.917")
    assert run.score(energy, "15700", "15699.917")
    assert not run.score(energy, "15699.37", "15699.917"), "the last interval left out"
    assert not run.score(energy, "about 15700", "15699.917"), "words around the number"
    assert not run.score(energy, "15699.9 MWh", "15699.917"), "a unit, asked not to"
    ratio = {"type": "number", "tolerance": 0.0005}
    assert run.score(ratio, "0.9873", "0.9873") and run.score(ratio, ".9872", "0.9873")
    assert not run.score(ratio, "0.9920", "0.9873"), "Monday for the weekend"
    assert not run.score(ratio, "0.99", "0.9873"), "fewer places than asked"
    month = {"type": "number", "tolerance": 0}
    assert run.score(month, "12", "12") and not run.score(month, "2014-12", "12")
    assert not run.score(month, "1,2", "12"), "a hedge, not twelve"
    assert run.score(energy, "15,699.917", "15699.917"), "thousands separated"
    name = {"type": "string"}
    assert run.score(name, "MT_223", "MT_223") and run.score(name, "`MT_223`", "MT_223")
    assert not run.score(name, "MT_223 or MT_106", "MT_223"), "a hedge"
    assert not run.score(name, "MT_106 (MT_223 if zeros count)", "MT_223"), "a hedge"
    assert not run.score(name, "MT_2230", "MT_223")
    assert run.score(name, "2014-02-04 19:15:00", "2014-02-04 19:15"), "seconds not asked for"
    assert run.score(name, "2014-02-04T19:15", "2014-02-04 19:15"), "a T between date and time"
    assert run.parse_answer("so\nAnswer: 0.9873") == "0.9873", "an answer line in any case"
    assert not run.score(name, "2014-02-04 19:30", "2014-02-04 19:15")
    assert not run.score(name, "2014-02-04", "2014-02-04 19:15"), "less than was asked"
    assert not run.score(name, "2014-03-30 or 2014-03-31", "2014-03-30"), "a hedge"
    assert not run.score(name, None, "MT_223"), "no ANSWER line"
    print("scorer: ok")


def check_redact():
    import getpass
    import socket
    # Names of its own, so the check leans neither on the machine's names nor on it having a user.
    real = socket.gethostname, getpass.getuser
    socket.gethostname, getpass.getuser = lambda: "meter-lab-7.example.org", lambda: "mlops"
    text = "Linux meter-lab-7 6.8.0 aarch64\n/srv/data/readings.csv mlops"
    try:
        said = run.redact(text, [("/srv/data", "<data>")])

        # A uid without a passwd entry, as in a container: OSError from Python 3.13, KeyError before.
        unnamed = []
        for error in (OSError("No username set in the environment"), KeyError("uid not found")):
            def no_user(error=error):
                raise error
            getpass.getuser = no_user
            unnamed.append(run.redact(text, []))
    finally:
        socket.gethostname, getpass.getuser = real
    assert "meter-lab-7" not in said, "the host name reaches a committed transcript"
    assert "/srv/data" not in said and "mlops" not in said, said
    assert "6.8.0 aarch64" in said and "readings.csv" in said, "redaction ate the content"
    for u in unnamed:
        assert "meter-lab-7" not in u and "readings.csv mlops" in u, u
    print("redact: ok")


def check_jail():
    data = tempfile.mkdtemp(prefix="benchy-jail-data-")
    outside = tempfile.mkdtemp(prefix="benchy-jail-outside-")
    with open(os.path.join(data, "readings.csv"), "w") as fh:
        fh.write("a\n1\n")
    with open(os.path.join(outside, "secret"), "w") as fh:
        fh.write("SECRET-OUTSIDE")
    # The harness holds the API key in its environment; this stands in for it.
    os.environ["BENCHY_JAIL_CANARY"] = "CANARY-IN-THE-HARNESS"
    a = types.SimpleNamespace(data=data, duckdb=shutil.which("true"), duckdb_version="none")
    shell = run.Shell(a)
    try:
        def said(cmd):
            return run.jailed(shell, cmd)

        assert "a\n1" in said("cat readings.csv"), "the data is readable"
        # The DuckDB CLI colours its output even into a pipe; the model must get the text.
        assert said("printf '\\033[90mts\\033[00m \\033[1mtimestamp\\033[0m'") == "ts timestamp", \
            "colour codes reach the model"
        assert "x" in said("echo x > mine && cat mine"), "the scratch directory is writable"
        assert "SECRET-OUTSIDE" not in said(f"cat {outside}/secret"), "a file outside"
        said(f"echo x > {outside}/planted")
        assert not os.path.exists(os.path.join(outside, "planted")), "a write outside"
        said(f"echo x > {data}/planted")
        assert not os.path.exists(os.path.join(data, "planted")), "a write to the data"
        said(f"ln -s {outside}/secret link")
        assert "SECRET-OUTSIDE" not in said("cat link"), "a link out of the scratch directory"
        assert "CANARY-IN-THE-HARNESS" not in said(
            "cat /proc/$PPID/environ /proc/*/environ 2>/dev/null | tr '\\0' '\\n'"
        ), "the harness's environment"
        tcp = said("python3 -c \"import socket; socket.create_connection(('127.0.0.1', 9), 2)\"")
        assert "PermissionError" in tcp, f"a TCP connection: {tcp}"
        assert "unlimited" not in said("ulimit -u"), "processes are bounded"
        assert "unlimited" not in said("ulimit -f"), "file sizes are bounded"
        assert run.redact(f"{run.ROOT}/eval/tasks.toml", shell.private) == "<repo>/eval/tasks.toml", \
            "the checkout's path reaches a committed transcript"
        run.preflight(a)
    finally:
        shell.close()
        shutil.rmtree(data, ignore_errors=True)
        shutil.rmtree(outside, ignore_errors=True)
    print("jail: ok")


def check_thread_count():
    # The jail's process bound starts from this uid's threads as the kernel counts them, which
    # covers every container the uid runs in, not only what this /proc shows (PRJ-33).
    import threading
    import jail
    if sys.platform != "linux" or os.geteuid() == 0:
        print("thread count: skipped, not Linux or root (root has no process bound)")
        return
    before = jail.threads_now()
    stop = threading.Event()
    extra = [threading.Thread(target=stop.wait) for _ in range(100)]
    for t in extra:
        t.start()
    try:
        during = jail.threads_now()
    finally:
        stop.set()
        for t in extra:
            t.join()
    assert during - before >= 100, f"{before} threads, then {during} with 100 more"
    print("thread count: ok")


if __name__ == "__main__":
    check_score()
    check_redact()
    check_thread_count()
    check_jail()
