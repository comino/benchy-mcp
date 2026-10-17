## What and why

<!-- The problem, and how this solves it. `Fixes #N` if there is an issue. -->

## Evidence

<!-- CONTRIBUTING.md: every fix starts from a failing test, and no test is loosened to go green. -->

- [ ] The test that fails without this change, and its output before the fix:
- [ ] It fails again when the guard it covers is removed (by hand, or cargo-mutants for
      `guard`, `lockdown`, `sandbox`, `admission`, `loader`).
- [ ] `REQUIREMENTS.md` says what is now true, naming the test; a bullet marked *(planned)* is
      unmarked by the change that adds its test. `python3 scripts/check_requirements.py` passes.
- [ ] On the ingestion or query path: `perf stat -e instructions:u` before and after
      (`bench/`), and docs/PERFORMANCE.md updated if it moved. Or: not on that path.
- [ ] `scripts/ci.sh` passes locally (CI on GitHub is not the gate).
- [ ] A line under `## Unreleased` in CHANGELOG.md.
- [ ] No host names, internal references or private numbers in code, docs, tests or messages.
