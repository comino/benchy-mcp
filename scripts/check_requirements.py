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

A test that exists is not always a test that runs. One marked `#[ignore]` runs only when asked,
so its bullet must say it is ignored, and why. One built only on Linux
(`#[cfg(target_os = "linux")]`, on the test or on its whole file) never runs on macOS, where the
suite runs too, so its bullet, its comments or its file's header must say Linux. The count of
each is printed, so the share of the evidence one platform carries is never a surprise.

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
          "spike/src/*.rs", "spike/*.md", "spike/*.py", ".cargo/mutants.toml",
          # The archive is checked like the normative docs: its prose stays as it was written,
          # but a REQ-n citation there is still a pointer a reader follows to a test, so a
          # rename updates it as it updates any other.
          "docs/history/*.md"]
TEST = re.compile(r"#\[test\]\s*(?:#\[[^\]]*\]\s*)*(?:pub\s+)?fn\s+([a-z][a-z0-9_]*)\s*\(")
FN = re.compile(r"^\s*(?:pub\s+)?fn\s+([a-z][a-z0-9_]*)\s*\(")
LINUX = 'cfg(target_os = "linux")'


def bullets(text):
    """Each bullet that names a test: its name, whether it is planned, and its whole text."""
    lines = text.splitlines()
    for i, line in enumerate(lines):
        m = BULLET.match(line)
        if not m:
            continue
        body = [line]
        for more in lines[i + 1:]:
            if not more.strip() or not more[0].isspace() or more.lstrip().startswith("- "):
                break
            body.append(more)
        yield m.group(1), bool(m.group(2)), " ".join(body)


def written_tests(text):
    """Each `#[test]` fn in a file: its name, whether it is ignored or built only on Linux, and
    whether what is written on it says Linux — the file's `//!` header, the comments above the
    test, or those that open its body, where the tests here explain themselves."""
    lines = text.splitlines()
    header = " ".join(l for l in lines if l.startswith("//!"))
    linux_file = f"#![{LINUX}]" in text
    for i, line in enumerate(lines):
        m = FN.match(line)
        if not m:
            continue
        top = i
        while top > 0 and lines[top - 1].strip().startswith(("#[", "//")):
            top -= 1
        attrs = [l.strip() for l in lines[top:i] if l.strip().startswith("#[")]
        if "#[test]" not in attrs:
            continue
        said = [l for l in lines[top:i] if l.strip().startswith("//")]
        for l in lines[i + 1:]:
            if not l.strip().startswith("//"):
                break
            said.append(l)
        yield m.group(1), {
            "ignored": any(a.startswith("#[ignore") or ("cfg_attr" in a and "ignore" in a)
                           for a in attrs),
            "linux": linux_file or any(LINUX in a for a in attrs),
            "says_linux": "Linux" in header or any("Linux" in l for l in said),
        }


def main() -> int:
    named = list(bullets((ROOT / "REQUIREMENTS.md").read_text()))
    if not named:
        print("REQUIREMENTS.md names no tests — the bullet format has probably changed")
        return 1

    tests, written, problems = set(), {}, []
    for f in list(ROOT.glob("tests/**/*.rs")) + list(ROOT.glob("src/**/*.rs")):
        text = f.read_text()
        tests.update(TEST.findall(text))
        written.update(written_tests(text))
    # Two readings of the same files: one finds that a test exists, the other what is written on
    # it. Were they to disagree, an ignored or Linux-only test could pass unseen.
    for name in sorted(tests ^ set(written)):
        problems.append(f"`{name}`: the two readings of the tests disagree on whether it is one")

    seen, ignored, linux = set(), 0, 0
    for name, planned, bullet in named:
        if name in seen:
            problems.append(f"`{name}` is named twice")
        seen.add(name)
        if planned and name in tests:
            problems.append(f"`{name}` exists now — remove its *(planned)* marker")
        if not planned and name not in tests:
            problems.append(f"`{name}` is named but no #[test] fn has that name")
        w = written.get(name)
        if planned or w is None:
            continue
        if w["ignored"]:
            ignored += 1
            if "ignored" not in bullet.lower():
                problems.append(f"`{name}` is #[ignore]d, so it runs only when asked; its bullet "
                                "must say it is ignored, and why")
        if w["linux"]:
            linux += 1
            if "Linux" not in bullet and not w["says_linux"]:
                problems.append(f"`{name}` is built only on Linux; its bullet, its comments or "
                                "its file's header must say so")

    planned_names = {name for name, planned, _ in named if planned}
    for pattern in CITING:
        for f in ROOT.glob(pattern):
            for n, line in enumerate(f.read_text().splitlines(), 1):
                for name in CITATION.findall(line):
                    if name not in tests and name not in planned_names:
                        rel = f.relative_to(ROOT)
                        problems.append(f"{rel}:{n} cites `{name}`, which is no test")

    live = sum(1 for _, p, _ in named if not p)
    print(f"{len(named)} named tests: {live} exist, {len(named) - live} planned; of those that "
          f"exist, {linux} are built only on Linux and {ignored} run only when asked")
    for p in problems:
        print(f"  FAIL  {p}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
