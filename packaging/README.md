# benchy-mcp

An agent workbench for time-series analytics over the data you already have: an MCP server that
loads your data into a private DuckDB workspace before the agent's first call.
The project, its documentation and its source: https://github.com/comino/benchy-mcp

**Work in progress prototype — expect frequent breaking changes.** Benchy is under heavy
development and is not ready for production. CLI options, configuration, tools, protocols and
result formats may change without backward compatibility. Keep the version you integrate with
pinned and review the changelog before updating. Benchy is licensed under MIT; see `LICENSE`.

## What is in this directory

| file | |
|---|---|
| `benchy-mcp` | the server |
| `libduckdb.so` / `libduckdb.dylib` | DuckDB's official release library, unmodified apart from keeping only this machine's architecture on macOS. The binary loads it from its own directory, so keep the two together |
| `LICENSE` | benchy-mcp's licence (MIT) |
| `LICENSE-DuckDB` | DuckDB's licence (MIT) |
| `THIRD-PARTY-LICENSES.txt` | the licences of the Rust crates compiled into the binary |
| `CHANGELOG.md` | what changed in each release |
| `SHA256SUMS` | the SHA-256 of every other file here, as shipped |
| `example-data/` | two small CSV files to try it on |

## Run it

Keep this directory where it is and put a link to the binary on your PATH, or run it from here:

```sh
./benchy-mcp --version
./benchy-mcp doctor              # what this machine enforces, and which library was loaded
./benchy-mcp "$PWD/example-data" # serves MCP on stdio: let your MCP client start it
```

`install.sh` from the release does the same: it unpacks this directory to
`~/.local/lib/benchy-mcp/<version>/` and links `~/.local/bin/benchy-mcp` to it.

On Linux, Landlock confines the engine. macOS has no equivalent wired up: there benchy serves
only when started with `--allow-unsandboxed`, and the SQL layer is then the only boundary.
