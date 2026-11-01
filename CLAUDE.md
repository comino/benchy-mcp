# benchy-mcp

Rust MCP server: loads time-series data into a sealed DuckDB workspace for an agent's SQL. Public prototype (MIT). How it fits together, building and the one rule (every fix starts from a failing test): `CONTRIBUTING.md`. What it is held to: `REQUIREMENTS.md`. Linear: `PRJ | benchy-mcp`.

## Release path
- No test or staging environment. Done = merged to `main` with CI green and proven on bx (below).
- Branch → `bx test` green → PR → CI green → merge. CI runs test, clickhouse and deny on every push; macos on PRs and `main`; a release dry run on PRs that touch dependencies or packaging.
- **A `v<version>` tag is a public release** (`.github/workflows/release.yml`: GitHub release bundles, `ghcr.io/comino/benchy-mcp` images, `latest` moves). It is Sven's alone; never push a `v*` tag. Version and CHANGELOG procedure: `CONTRIBUTING.md` § Releasing.
- DuckDB bumps have their own procedure (`CONTRIBUTING.md` § Building).

## What is live
The newest GitHub release (`gh release list -L 1`) and the `latest` image tag on ghcr.io. `benchy-mcp --version` reports `Cargo.toml`'s version. `main` may carry unreleased changes (CHANGELOG § Unreleased).

## Required gates
- `bx test` (profile `benchy-mcp`): `scripts/ci.sh` with the ClickHouse tests and `cargo deny`; the expected count is the last green `bx record gate`.
- macOS is tested only by CI (bx runs Linux): a change to the sandbox or the build needs the macos job green.
- `scripts/ci.sh` stops at its first failure: a red step hides every later one, so fix and re-run.
- bx runs the job as uid 1000 with no passwd entry and no `USER` (homelab, 2026-11-01): tests must not assume a named user (PRJ-26, PRJ-30).

## Exercise path
The built binary through a real MCP stdio handshake against `examples/data` (`examples/verify.py`, `examples/host.py`; both run inside `scripts/ci.sh`). `benchy-mcp doctor` says what the machine enforces. No real data or credentials.

## Error signature
Errors carry an `E_*` code (`E_INTERNAL`, `E_REBUILT`, `E_DEADLINE` …) in the tool result; the server logs `benchy: …` lines on stderr. A red `deny` on a PR that didn't touch `Cargo.lock` means a new RustSec advisory.
