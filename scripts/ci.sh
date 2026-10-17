#!/usr/bin/env bash
# The local counterpart of .github/workflows/ci.yml. Exits non-zero on the first failure.
#
#   scripts/ci.sh
#   BENCHY_CLICKHOUSE_BIN=/path/to/clickhouse scripts/ci.sh   # and the ClickHouse tests
#
# On a small machine, CARGO_BUILD_JOBS=2 keeps the build within memory. clippy builds into
# target/clippy, so its check-level artifacts never invalidate the release build's.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n== %s\n' "$*"; }

step "cargo fmt --check"
cargo fmt --check

step "requirements name tests that exist"
python3 scripts/check_requirements.py

step "one Rust compiler everywhere"
python3 scripts/check_toolchain.py

step "DuckDB's library only as released"
python3 -I packaging/test_duckdb_lib.py
# Into both target dirs, before cargo: libduckdb-sys would otherwise download it unchecked.
python3 packaging/duckdb_lib.py fetch
CARGO_TARGET_DIR="$PWD/target/clippy" python3 packaging/duckdb_lib.py fetch

step "clippy, warnings are errors"
CARGO_TARGET_DIR="$PWD/target/clippy" CARGO_INCREMENTAL=0 cargo clippy --all-targets -- -D warnings

step "cargo test --release"
cargo test --release
python3 packaging/duckdb_lib.py check

if [ -n "${BENCHY_CLICKHOUSE_BIN:-}" ]; then
  step "the ClickHouse tests, against $BENCHY_CLICKHOUSE_BIN"
  log=$(mktemp)
  trap 'rm -f "$log"' EXIT
  cargo test --release --test clickhouse --test origin -- --nocapture 2>&1 | tee "$log"
  # A skipped test passes, so make sure none was.
  if grep -q '^skipped:' "$log"; then
    echo "a ClickHouse test was skipped" >&2
    exit 1
  fi
else
  step "the ClickHouse tests: skipped, BENCHY_CLICKHOUSE_BIN is not set"
fi

step "a real MCP client, the host protocol, the eval's scorer and jail"
cargo build --release
python3 examples/verify.py ./target/release/benchy-mcp ./examples/data
python3 examples/host.py ./target/release/benchy-mcp
if command -v node >/dev/null; then
  node examples/host-node.mjs ./target/release/benchy-mcp
else
  echo "examples/host-node.mjs: skipped, node is not installed"
fi
python3 eval/test_eval.py

step "what this machine enforces"
./target/release/benchy-mcp doctor

step "advisories, licences and sources of Cargo.lock"
if command -v cargo-deny >/dev/null; then
  cargo deny --locked check
else
  echo "cargo deny: skipped, cargo-deny is not installed (ci.yml runs it)"
fi

printf '\nci.sh: everything passed\n'
