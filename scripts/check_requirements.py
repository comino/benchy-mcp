#!/usr/bin/env python3
"""Fail if REQUIREMENTS.md names a test that does not exist — or plans one that already does.

    python3 scripts/check_requirements.py

Every bullet of the form

    - `test_name` — what it proves
    - `test_name` *(planned)* — the gap, and what happens today

is checked against the `#[test]` functions in `tests/` and `src/`. A plain bullet must name a
test that exists; a planned bullet must name one that does not yet. Both directions matter: the
first stops the file naming evidence that is not there, the second stops a gap staying marked
open after it was closed.

Code comments and docs cite tests the same way — "REQ-6 `test_name`" — and each citation must
name a test that exists or one REQUIREMENTS.md plans, so a renamed test cannot leave a comment
pointing at nothing.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
BULLET = re.compile(r"^\s*-\s+`([a-z][a-z0-9_]*)`(\s+\*\(planned\)\*)?\s+—", re.M)
CITATION = re.compile(r"REQ-\d+:?\s+`([a-z][a-z0-9_]*)`")
CITING = ["src/**/*.rs", "tests/**/*.rs", "docs/*.md", "README.md", "CONTRIBUTING.md",
          "SECURITY.md", "corpus/*.toml", "examples/**/*.py", "examples/**/*.sh", "examples/*.toml",
          "spike/src/*.rs", "spike/*.md", "spike/*.py", ".cargo/mutants.toml"]
TEST = re.compile(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*(?:pub\s+)?fn\s+([a-z][a-z0-9_]*)\s*\(")


def main() -> int:
    named = BULLET.findall((ROOT / "REQUIREMENTS.md").read_text())
    if not named:
        print("REQUIREMENTS.md names no tests — the bullet format has probably changed")
        return 1

    tests = set()
    for f in list(ROOT.glob("tests/**/*.rs")) + list(ROOT.glob("src/**/*.rs")):
        tests.update(TEST.findall(f.read_text()))

    seen, problems = set(), []
    for name, planned in named:
        if name in seen:
            problems.append(f"`{name}` is named twice")
        seen.add(name)
        if planned and name in tests:
            problems.append(f"`{name}` exists now — remove its *(planned)* marker")
        if not planned and name not in tests:
            problems.append(f"`{name}` is named but no #[test] fn has that name")

    planned_names = {name for name, planned in named if planned}
    for pattern in CITING:
        for f in ROOT.glob(pattern):
            for n, line in enumerate(f.read_text().splitlines(), 1):
                for name in CITATION.findall(line):
                    if name not in tests and name not in planned_names:
                        rel = f.relative_to(ROOT)
                        problems.append(f"{rel}:{n} cites `{name}`, which is no test")

    live = sum(1 for _, p in named if not p)
    print(f"{len(named)} named tests: {live} exist, {len(named) - live} planned")
    for p in problems:
        print(f"  FAIL  {p}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
