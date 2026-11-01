"""scripts/merge.py merges only a gated head whose CI, the PR's own run included, is green; pinned to it.

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
    print(json.dumps({{"headRefOid": "{head}", "baseRefName": os.environ["STUB_BASE"]}}))
elif sys.argv[1] == "api":
    print(open(os.path.join(d, "runs.json")).read())
    sys.exit(int(os.environ["STUB_API_RC"]))
elif sys.argv[1:3] == ["pr", "merge"]:
    sys.exit(int(os.environ["STUB_MERGE_RC"]))
"""


def run(name, event="pull_request", status="completed", conclusion="success"):
    return {"name": name, "event": event, "status": status, "conclusion": conclusion}


GREEN_CI = [run("CI"), run("CI", event="push"), run("Release", conclusion="neutral"),
            run("Docs", conclusion="skipped")]


class Merge(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        for tool in ("gh", "bx"):
            p = Path(self.dir.name, tool)
            p.write_text(STUB.format(python=sys.executable, head=HEAD))
            p.chmod(0o755)

    def merge(self, *pages, bx_rc=0, merge_rc=0, api_rc=0, pr="42", base="main"):
        """Run merge.py; each page is the workflow runs of one page of the API's answer."""
        Path(self.dir.name, "runs.json").write_text(json.dumps([{"workflow_runs": r} for r in pages]))
        env = {"PATH": self.dir.name, "STUB_DIR": self.dir.name,
               "STUB_BX_RC": str(bx_rc), "STUB_MERGE_RC": str(merge_rc),
               "STUB_API_RC": str(api_rc), "STUB_BASE": base}
        p = subprocess.run([sys.executable, "-I", str(MERGE), pr], env=env,
                           capture_output=True, text=True)
        calls = Path(self.dir.name, "calls")
        self.calls = [json.loads(l) for l in calls.read_text().splitlines()] if calls.exists() else []
        return p

    def merged(self):
        return [c for c in self.calls if c[:3] == ["gh", "pr", "merge"]]

    def refused(self, p, says):
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])
        self.assertIn(says, p.stderr)

    def test_green_head_merges_pinned_to_that_head(self):
        p = self.merge(GREEN_CI)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn(["bx", "merge-check", HEAD], self.calls)
        self.assertEqual(self.merged(), [["gh", "pr", "merge", "42", "--rebase", "--match-head-commit", HEAD]])

    def test_every_page_of_the_head_shas_runs_is_read(self):
        self.merge(GREEN_CI)
        api = [c for c in self.calls if c[:2] == ["gh", "api"]]
        self.assertEqual(len(api), 1)
        self.assertIn(f"head_sha={HEAD}", api[0][-1])
        self.assertIn("--paginate", api[0])
        self.assertIn("--slurp", api[0])

    def test_a_red_run_on_a_later_page_does_not_merge(self):
        self.refused(self.merge(GREEN_CI, [run("Mutants", event="push", conclusion="failure")]),
                     "Mutants (push): failure")

    def test_refused_gate_does_not_merge(self):
        for rc in (1, 2, 64, 75):
            self.refused(self.merge(GREEN_CI, bx_rc=rc), "merge-check")

    def test_a_failed_run_does_not_merge(self):
        self.refused(self.merge(GREEN_CI + [run("CI", conclusion="failure")]), "CI (pull_request): failure")

    def test_a_queued_or_running_run_does_not_merge(self):
        for status in ("queued", "in_progress", "waiting", "requested", "pending"):
            self.refused(self.merge(GREEN_CI + [run("CI", status=status, conclusion=None)]), status)

    def test_cancelled_and_timed_out_are_not_green(self):
        for c in ("cancelled", "timed_out", "action_required", "stale", "startup_failure"):
            self.refused(self.merge([run("CI"), run("CI", event="push", conclusion=c)]), c)

    def test_a_green_push_run_alone_does_not_merge(self):
        # The PR's own run (macos, release dry run) is created seconds after the PR opens.
        self.refused(self.merge([run("CI", event="push")]), "no pull_request run")

    def test_no_runs_at_all_does_not_merge(self):
        self.refused(self.merge([]), "no pull_request run")

    def test_a_pr_into_another_branch_does_not_merge(self):
        # bx merge-check proves the head on top of main; a PR into another branch merges another tree.
        p = self.merge(GREEN_CI, base="release-0.3")
        self.refused(p, "release-0.3")
        self.assertNotIn("bx", [c[0] for c in self.calls])

    def test_a_failed_runs_read_does_not_merge_on_what_it_got(self):
        # gh prints the pages it got before failing on a later one.
        p = self.merge(GREEN_CI, api_rc=1)
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(self.merged(), [])

    def test_a_failed_merge_fails(self):
        self.assertNotEqual(self.merge(GREEN_CI, merge_rc=1).returncode, 0)

    def test_only_a_pr_number_is_accepted(self):
        for pr in ("https://github.com/someone/fork/pull/42", "main", "42x", ""):
            p = self.merge(GREEN_CI, pr=pr)
            self.assertNotEqual(p.returncode, 0, pr)
            self.assertEqual(self.calls, [], pr)

    def test_no_pr_number_is_a_usage_error(self):
        p = subprocess.run([sys.executable, "-I", str(MERGE)], capture_output=True, text=True,
                           env={"PATH": self.dir.name, "STUB_DIR": self.dir.name})
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse(Path(self.dir.name, "calls").exists())


if __name__ == "__main__":
    unittest.main()
