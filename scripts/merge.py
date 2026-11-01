#!/usr/bin/env python3
"""Merge a pull request the way CLAUDE.md's release path says.

    scripts/merge.py <pr number>

It merges only when `bx merge-check` passes for the PR's head (a green gate record for that exact
tree, on top of origin's main) and every GitHub Actions check on that head finished green: the
bx profile is "CI green required". The merge is pinned to that head, so a push in between fails it.
"""
import json
import subprocess
import sys

GREEN = {"success", "skipped", "neutral"}


def gh(*args):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def not_green(runs):
    """What keeps CI from counting as green, or None."""
    runs = [r for r in runs if r["app"]["slug"] == "github-actions"]
    if not runs:
        return "no GitHub Actions checks"
    # A check still running has no conclusion yet.
    bad = [f"{r['name']}: {r['conclusion'] or r['status']}" for r in runs if r["conclusion"] not in GREEN]
    return ", ".join(bad) or None


def main(pr):
    head = json.loads(gh("pr", "view", pr, "--json", "headRefOid"))["headRefOid"]
    if subprocess.run(["bx", "merge-check", head]).returncode:
        sys.exit(f"merge.py: bx merge-check refused {head}")
    # The endpoint returns the latest run of each check, so a green re-run replaces a red one.
    runs = json.loads(gh("api", f"repos/{{owner}}/{{repo}}/commits/{head}/check-runs?per_page=100"))
    if problem := not_green(runs["check_runs"]):
        sys.exit(f"merge.py: CI on {head[:7]} is not green: {problem}")
    gh("pr", "merge", pr, "--rebase", "--match-head-commit", head)
    print(f"merge.py: merged #{pr} at {head}")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        main(sys.argv[1])
    except subprocess.CalledProcessError as e:
        sys.exit(f"merge.py: {' '.join(e.cmd)} failed: {e.stderr.strip()}")
