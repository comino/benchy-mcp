#!/usr/bin/env python3
"""Merge a pull request the way CLAUDE.md's release path says.

    scripts/merge.py <pr number>

It merges only when `bx merge-check` passes for the PR's head (a green gate record for that exact
tree, on top of origin's main) and every GitHub Actions workflow run on that head finished green,
the PR's own run included: the bx profile is "CI green required". The merge is pinned to that
head, so a push in between fails it.
"""
import json
import subprocess
import sys

GREEN = {"success", "skipped", "neutral"}


def gh(*args):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def not_green(runs):
    """What keeps the head's workflow runs from counting as green, or None."""
    # The PR's own run (macos, the release dry run) appears a few seconds after the PR opens.
    if not any(r["event"] == "pull_request" for r in runs):
        return "no pull_request run yet"
    # A run still queued or running has no conclusion yet; a re-run reports its latest attempt.
    bad = [f"{r['name']} ({r['event']}): {r['conclusion'] or r['status']}"
           for r in runs if r["conclusion"] not in GREEN]
    return ", ".join(bad) or None


def main(pr):
    view = json.loads(gh("pr", "view", pr, "--json", "headRefOid,baseRefName"))
    head = view["headRefOid"]
    # merge-check proves the head on top of main; a PR into another branch would merge another tree.
    if view["baseRefName"] != "main":
        sys.exit(f"merge.py: #{pr} goes into {view['baseRefName']}, not main")
    if subprocess.run(["bx", "merge-check", head]).returncode:
        sys.exit(f"merge.py: bx merge-check refused {head}")
    pages = json.loads(gh("api", "--paginate", "--slurp",
                          f"repos/{{owner}}/{{repo}}/actions/runs?head_sha={head}&per_page=100"))
    if problem := not_green([r for p in pages for r in p["workflow_runs"]]):
        sys.exit(f"merge.py: CI on {head[:7]} is not green: {problem}")
    gh("pr", "merge", pr, "--rebase", "--match-head-commit", head)
    print(f"merge.py: merged #{pr} at {head}")


if __name__ == "__main__":
    # A number only: a PR URL could name another repository than the one the runs are read from.
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        sys.exit(__doc__)
    try:
        main(sys.argv[1])
    except subprocess.CalledProcessError as e:
        sys.exit(f"merge.py: {' '.join(e.cmd)} failed: {e.stderr.strip()}")
