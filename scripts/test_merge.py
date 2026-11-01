"""scripts/merge.py merges only a gated, CI-green head, pinned to that head.

    python3 -I scripts/test_merge.py

`gh` and `bx` are stubs on PATH that log their arguments; nothing reaches GitHub or banana.
"""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

MERGE = Path(__file__).resolve().parent / "merge.py"
HEAD = "c0ffee" + "0" * 34

STUB = """#!{python}
import json, os, sys
d = os.environ["STUB_DIR"]
with open(os.path.join(d, "calls"), "a") as f:
    f.write(json.dumps([os.path.basename(sys.argv[0])] + sys.argv[1:]) + "\\n")
if sys.argv[0].endswith("bx"):
    sys.exit(int(os.environ["STUB_BX_RC"]))
if sys.argv[1:3] == ["pr", "view"]:
    print(json.dumps({{"headRefOid": "{head}"}}))
elif sys.argv[1] == "api":
    print(open(os.path.join(d, "runs.json")).read())
elif sys.argv[1:3] == ["pr", "merge"]:
    sys.exit(int(os.environ["STUB_MERGE_RC"]))
"""


def run(name, status="completed", conclusion="success", app="github-actions"):
    return {"name": name, "status": status, "conclusion": conclusion, "app": {"slug": app}}


class Merge(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        for tool in ("gh", "bx"):
            p = Path(self.dir.name, tool)
            p.write_text(STUB.format(python=sys.executable, head=HEAD))
            p.chmod(0o755)

    def merge(self, runs, bx_rc=0, merge_rc=0):
        Path(self.dir.name, "runs.json").write_text(json.dumps({"check_runs": runs}))
        env = {"PATH": self.dir.name, "STUB_DIR": self.dir.name,
               "STUB_BX_RC": str(bx_rc), "STUB_MERGE_RC": str(merge_rc)}
        p = subprocess.run([sys.executable, "-I", str(MERGE), "42"], env=env,
                           capture_output=True, text=True)
        calls = Path(self.dir.name, "calls")
        self.calls = [json.loads(l) for l in calls.read_text().splitlines()] if calls.exists() else []
        return p

    def merged(self):
        return [c for c in self.calls if c[:3] == ["gh", "pr", "merge"]]

    def test_green_head_merges_pinned_to_that_head(self):
        p = self.merge([run("test"), run("deny"), run("release", conclusion="skipped")])
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(["bx", "merge-check", HEAD], self.calls)
        self.assertEqual(self.merged(), [["gh", "pr", "merge", "42", "--rebase", "--match-head-commit", HEAD]])

    def test_checks_are_read_for_the_head_sha(self):
        self.merge([run("test")])
        api = [c for c in self.calls if c[:2] == ["gh", "api"]]
        self.assertEqual(len(api), 1)
        self.assertIn(f"/commits/{HEAD}/check-runs", api[0][2])

    def test_refused_gate_does_not_merge(self):
        p = self.merge([run("test")], bx_rc=1)
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])
        self.assertIn("merge-check", p.stderr)

    def test_a_failed_check_does_not_merge(self):
        p = self.merge([run("test"), run("macos", conclusion="failure")])
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])
        self.assertIn("macos: failure", p.stderr)

    def test_a_pending_check_does_not_merge(self):
        p = self.merge([run("test"), run("clickhouse", status="in_progress", conclusion=None)])
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])
        self.assertIn("clickhouse: in_progress", p.stderr)

    def test_cancelled_and_timed_out_are_not_green(self):
        for c in ("cancelled", "timed_out", "action_required", "stale"):
            p = self.merge([run("test", conclusion=c)])
            self.assertNotEqual(p.returncode, 0, c)
            self.assertEqual(self.merged(), [], c)

    def test_no_checks_at_all_does_not_merge(self):
        p = self.merge([])
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])

    def test_only_other_apps_checks_count_as_no_ci(self):
        p = self.merge([run("codecov", app="codecov")])
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])

    def test_a_failed_merge_fails(self):
        p = self.merge([run("test")], merge_rc=1)
        self.assertNotEqual(p.returncode, 0)

    def test_no_pr_number_is_a_usage_error(self):
        p = subprocess.run([sys.executable, "-I", str(MERGE)], capture_output=True, text=True,
                           env={"PATH": self.dir.name, "STUB_DIR": self.dir.name})
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse(Path(self.dir.name, "calls").exists())


if __name__ == "__main__":
    unittest.main()
