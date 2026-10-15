# Contributing

Benchy is a work in progress prototype under the [MIT license](LICENSE). Development is active,
and frequent breaking changes are expected across the CLI, configuration, tools and protocols.
There is no stable API or backward-compatibility guarantee yet. Document breaking changes in
`CHANGELOG.md` and update the affected examples and reference docs with the code.

## The one rule

**Every fix starts from a failing test, and no test is loosened to go green.** A test only counts
as evidence if it fails when the thing it guards is removed — a test that stays green without its
guard is decoration.

## Where things live

| | |
|---|---|
| [`REQUIREMENTS.md`](REQUIREMENTS.md) | what benchy is held to. Eighteen requirements, each naming the tests that prove it |
| [`docs/`](docs/) | the connector and host protocols, the built-in sources (ClickHouse, HTTP), the dialect reference the `dialect` tool serves, measured capacity |
| [`corpus/escape.toml`](corpus/escape.toml) | the escape corpus, shared by the tests |
| [`scripts/check_requirements.py`](scripts/check_requirements.py) | fails if REQUIREMENTS.md names a test that does not exist, or plans one that already does |
| [`scripts/ci.sh`](scripts/ci.sh) | everything CI would run, run here: the one verification a change gets. See [Before you push](#before-you-push) |
| [`bench/`](bench/) | the ingest benchmark: the same table loaded every way, with instructions, CPU time and peak memory per process and per stage. [docs/PERFORMANCE.md](docs/PERFORMANCE.md) has the method, the baseline and the backlog |
| [`packaging/`](packaging/), [`install.sh`](install.sh) | bundles: how one is packed, the smoke test it must pass on a machine that built nothing, and the installer. Not in use: there is no release. See [Releasing](#releasing) |
| [`spike/`](spike/) | throwaway probes that answered requirement-blocking questions. Not product code |
| [`eval/`](eval/) | measures what the bench costs an agent against the same agent with a jailed shell and the DuckDB CLI, on public data. The run needs an API key; its scorer and jail checks, `eval/test_eval.py`, run in `scripts/ci.sh` |

## How it fits together

One session is a handful of processes, each holding as little as it can:

| process | holds | source |
|---|---|---|
| the server | the MCP or host protocol, and the supervisor: it starts, kills and rebuilds the engine, runs loaders, keeps the session's store and its budget | `src/main.rs`, `src/mcp.rs`, `src/host.rs`, `src/supervisor.rs`, `src/config.rs`, `src/admission.rs` |
| the engine | the agent's SQL, over tables attached read-only; sealed and empty before anything arrives, confined by Landlock, no environment, no credential | `src/engine.rs`, `src/workspace.rs`, `src/guard.rs`, `src/lockdown.rs`, `src/sandbox.rs`, `src/result.rs`, `src/hint.rs`, `src/tools.rs` (what its texts call the agent's tools) |
| a loader | one source, turned into one DuckDB file in the bench's own types, then gone; it profiles the table it wrote (a stream's as its rows go in, `profile::Watch`, on a thread beside the insert started after the loader is confined) and leaves the profile in the table's comment, which `describe` renders. A file or Arrow loader is confined to its input and staging directory; a built-in source's loader (ClickHouse, HTTP) holds that source's credentials and is confined to its staging directory, the resolver's files and TCP to its source's port; a connector loader holds that connector's credentials and is not confined. A helpers job makes the operator's SQL helpers (REQ-18) in a file of their own, against empty stand-ins of the tables, under the engine's lock and confined to its staging directory; the engine attaches it read-only | `src/loader.rs`, `src/helpers.rs`, `src/source.rs`, `src/exec.rs`, `src/sources/` (built-in sources, ClickHouse and HTTP), `src/profile.rs`, `src/ipc.rs` (an Arrow stream, read bounded by what it decodes to), `src/canonical.rs` (the one type mapping every source goes through) |
| the warden | nothing: forked from the server before the config is read, its environment zeroed, it removes the session's directory once the server has gone, however it went | `src/supervisor.rs` |

`src/proto.rs` is the frame between the server and the processes it starts; `src/trace.rs` the
stage timings `bench/ingest.py` reads, off unless `BENCHY_TRACE_TIMINGS=1`; `src/lib.rs` is the
crate root.

## Building

```sh
cargo build --release          # the first build fetches DuckDB's release library into target/
cargo test --release
python3 examples/verify.py ./target/release/benchy-mcp ./examples/data
```

To link a `libduckdb.so` and `duckdb.h` you already have, which also skips the download:

```sh
export DUCKDB_LIB_DIR=/path DUCKDB_INCLUDE_DIR=/path LD_LIBRARY_PATH=/path
```

Do not switch DuckDB to its `bundled` feature. The crates.io sources cannot build ICU, and
without it a TIMESTAMPTZ column loses date_trunc, time_bucket and interval arithmetic.

Use the stable Rust toolchain, and `--release` for tests too: the integration tests drive the
built binary, and `scripts/ci.sh` tests the release build.

```sh
cargo test --release --test escape          # one test file, tests/escape.rs
cargo test --release some_test_name         # every test whose name contains this
```

## Before you push

**Verification is local.** GitHub Actions is not relied on: its workflows stay in
`.github/workflows/` as a record of what a clean machine runs, but a change is checked by running
the same steps here, with one script:

```sh
scripts/ci.sh                                          # exits non-zero on the first failure
BENCHY_CLICKHOUSE_BIN=/path/to/clickhouse scripts/ci.sh  # and the ClickHouse tests
```

It runs what `.github/workflows/ci.yml` runs, in this order: `cargo fmt --check`,
`scripts/check_requirements.py`, clippy with warnings as errors, `cargo test --release`, the
ClickHouse tests when `BENCHY_CLICKHOUSE_BIN` is set (and fails if one of them was skipped),
`examples/verify.py`, `examples/host.py`, `examples/host-node.mjs` (when `node` is installed),
`eval/test_eval.py` (Linux 6.7 or later: the eval's jail is Landlock) and `doctor`. Run it before opening a pull request, and say in the pull
request that it passed.

clippy builds into its own `CARGO_TARGET_DIR`, `target/clippy`, so its check-level artifacts and
the DuckDB library it fetches stay apart from the release build: about 700 MB on one Linux
aarch64 machine. On a small machine, `CARGO_BUILD_JOBS=2` keeps the build within memory.

The ClickHouse tests are `tests/clickhouse.rs` and the ClickHouse tests in `tests/origin.rs`;
without `BENCHY_CLICKHOUSE_BIN` they pass saying they were skipped. See
[Built-in sources](#built-in-sources). macOS is not checked by the script; run the suite there by
hand when a change touches the sandbox or the build. Linux-only calls (`pipe2`, `prctl`,
`clearenv`, `MSG_CMSG_CLOEXEC`) sit behind `#[cfg(target_os = "linux")]`, with an equivalent off
Linux. From a Linux machine, the crate and its tests can be type-checked for macOS without
linking: `rustup target add aarch64-apple-darwin`, then `cargo check --all-targets --target
aarch64-apple-darwin` (or `cargo clippy` the same way). The C dependencies need a C compiler for
the target; `zig cc -target aarch64-macos`, named in `CC_aarch64_apple_darwin` through a wrapper
that drops `-arch`, `--target=` and `-mmacosx-version-min=`, is enough. DuckDB's macOS library is
downloaded by the build as for Linux.

A change that could make ingest slower is measured with `bench/ingest.py` before and after, and
the pull request gives both numbers ([docs/PERFORMANCE.md](docs/PERFORMANCE.md)).

`cargo fmt` reflows long string literals and multi-line calls. If you patched a file by matching
on its text, re-run the tests after formatting: a text-anchored edit that silently missed is an
easy bug to ship.

## Releasing

No packaged release has been published yet; building from source is the way to try the prototype.
What follows describes the machinery for publishing one. A prototype release still carries no
stability or backward-compatibility guarantee.

A release is a tag. Set the version in `Cargo.toml`, give it a section in `CHANGELOG.md` headed
`## [<version>]` (the release notes are that section alone, so move what is under Unreleased
into it), merge, then push `v<version>`. `.github/workflows/release.yml` builds each platform with
`BENCHY_BUNDLE=1`, which leaves `$ORIGIN` (`@executable_path` on macOS) as the binary's only
rpath, packs it with DuckDB's library beside it (`packaging/package.sh`), unpacks every bundle on
a runner that built nothing and runs `doctor`, `verify.py`, `host.py` and `install.sh` against it
(`packaging/smoke.sh`), and only then publishes. The workflow refuses a tag that differs from
`Cargo.toml`'s version. `workflow_dispatch`, or a pull request that touches packaging, runs the
same build and smoke test without publishing; `gh run download` fetches the bundles, and
`BENCHY_DOWNLOAD_BASE=<dir> sh install.sh` installs one from a local directory. To pack one on
your own machine, build with `BENCHY_BUNDLE=1` and run `packaging/package.sh` as its header says.

## Workflows and actions

Every action in `.github/workflows/` is pinned to a full commit SHA, with the version it is in a
comment: `uses: actions/checkout@<40 hex digits> # v4.4.0`. A tag can be moved to other code by
whoever controls the action's repository; a commit cannot. To update one, look the tag up and
dereference it to its commit (an annotated tag points at a tag object first):

```sh
gh api repos/actions/checkout/git/ref/tags/v4.4.0 --jq .object   # type "tag"? then:
gh api repos/actions/checkout/git/tags/<sha> --jq .object        # until the type is "commit"
```

Pick a release at least two weeks old, and read what changed. `dtolnay/rust-toolchain` has no
release tags, so it is pinned to a commit of its `stable` branch and given `toolchain: stable`.

Only the release job may sign: it alone has `id-token: write` and `attestations: write`, and it
runs in the `release` environment. The repository's owner should give that environment required
reviewers (Settings → Environments → `release`), so a pushed tag waits for a person before
anything is published.

## Mutation testing

The one rule is checked by machine. `.github/workflows/mutants.yml` runs
[cargo-mutants](https://mutants.rs) weekly, and on demand, over what the containment rests on:
`guard`, `lockdown`, `sandbox`, `admission` and `loader`. It changes the code one mutant at a
time — a function body replaced, a comparison flipped, a field left out — and runs the suite
against each. **It is a gate:** a mutant the suite does not notice, or one that hangs it, fails
the run, and the fix is the test that should have caught it.

`.cargo/mutants.toml` says what is mutated, and lists the few mutants no test can tell from the
original — an equivalent operator, a name nothing reaches — each with the measurement that says
why. Add to it only with that reason, and match by function and change, never by line number.
Code that exists only off Linux, where the mutants never run, goes in a file the globs there leave
out, as `src/sandbox/elsewhere.rs` does.

The whole set takes about 50 minutes across four shards on a hosted runner, and much longer on
one machine; it is not part of `scripts/ci.sh`.
To check one mutant locally (the config supplies the files and `--release`):

```sh
cargo install cargo-mutants@27.1.0 --locked
cargo mutants --list                                  # every mutant, by name
cargo mutants -F 'replace > with >= in arrow'         # the ones matching a regex
cargo mutants -F 'in arrow' -- --test loader          # ...against one test file
```

To check a new test by hand, make the change cargo-mutants names, run the test, and see it fail.

## Changing a requirement

A requirement names its acceptance tests. When one is not met yet, its test is marked
*(planned)* and the bullet says what happens today; remove the marker in the same change that
makes the test exist. `scripts/check_requirements.py` enforces both directions.

Requirements here have been falsified by measurement before. That is the expected way for one to
change: **measure first, then rewrite the requirement to say what is true.** Do not quietly widen
a test.

## Built-in sources

A built-in source is one benchy reads itself, without a connector: ClickHouse, in
`src/sources/clickhouse.rs`, and HTTP, in `src/sources/http/`. Those two are the scope: other systems are reached
through an exec connector, and the one sink is DuckDB. Each is a module behind the `Fetch` trait in `src/sources/mod.rs` —
plan (read the schema and map it to the vocabulary), estimate, fetch (an Arrow stream handed to
`exec::read_arrow`, which holds it to what a connector's stdout is held to) — and a variant of
`BuiltIn`. Admission, the budget and the narrowing re-applied after ingest are shared
(`sources::load_within`); its config keys are parsed in `src/config.rs`, where `inert_keys` says
which kinds of source each key applies to. [docs/SOURCES.md](docs/SOURCES.md) is the operator's
reference, and changes with the code. HTTP has a load of its own (`src/sources/http/load.rs`),
since three of its formats are read by DuckDB itself rather than streamed; it ends in the same
table writers, `source::load_read` and `source::Ingest`. [docs/HTTP-SOURCE.md](docs/HTTP-SOURCE.md)
is its design, and `tests/common/http.rs` the scripted server its tests run against.

The ClickHouse integration tests (`tests/clickhouse.rs`) start a real server from the official
single binary, on a free port, with its data in a temporary directory, and say they were skipped
when `BENCHY_CLICKHOUSE_BIN` is not set. To run them, fetch the official
single binary of the version `.github/workflows/ci.yml` pins (`.github/workflows/ci.yml`, job `clickhouse`, which also
checks the amd64 build's SHA-512), for your CPU, and point the variable at it:

```sh
BENCHY_CLICKHOUSE_BIN=/path/to/clickhouse \
  cargo test --release --test clickhouse --test origin -- --nocapture 2>&1 | tee test.log
grep '^skipped:' test.log      # prints nothing when every test ran
```

`examples/capacity_clickhouse.py` measures a ClickHouse table four ways — the built-in source
with and without zstd, a `curl` exec connector, and a CSV export — against a server you name.

## Writing a connector

A connector is any program that writes an Arrow stream to stdout.
[`examples/connectors/hello.sh`](examples/connectors/hello.sh) is a complete one in shell — that
is the floor, on purpose. Check yours before wiring it up:

```sh
benchy-mcp exec-test ./benchy.toml my-source
```

The rules that bite, in the order people trip over them: stdout is the Arrow stream and **nothing
else**; close the stream, because the end-of-stream marker is what separates finished from
crashed; no rows is a complete stream with **zero batches**, not empty stdout; and treat
narrowing values as untrusted — parameterise your queries.
[`docs/EXEC-CONNECTOR-PROTOCOL.md`](docs/EXEC-CONNECTOR-PROTOCOL.md) is the whole contract.

## Adding to the escape corpus

New attack, new `[[case]]`. Record what each mechanism does with it *in isolation*, and the
statement `kind` the classifier reads — that is what makes a future DuckDB upgrade a failing test
rather than a silent change of posture.
