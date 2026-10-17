# Security

benchy-mcp is an experimental prototype. It has **not been independently security-audited**, and
it changes often. Its containment is the point of it, so a report that breaks it is welcome, but
do not rely on it as your only boundary for data that must not leak; the README's "Limits"
section says what it does not cover.

## Reporting a vulnerability

Report privately, through GitHub's private vulnerability reporting: the repository's
**Security** tab, then **Report a vulnerability**. Please do not open a public issue, pull
request or discussion for anything below until a fix is released.

Say which version (`benchy-mcp --version`), the platform and what `benchy-mcp doctor` prints, and
the smallest config, data and SQL or host-protocol frames that show it. A case for
`corpus/escape.toml` is the ideal form for a SQL escape. This is a one-maintainer project: expect a
first reply within about a week. A confirmed report gets a failing test, a fix in a release, and
credit in CHANGELOG.md unless you would rather not.

## What counts

A security bug here is a way across a boundary the project claims to hold
([REQUIREMENTS.md](REQUIREMENTS.md) is normative), for example:

- **Containment escapes.** SQL, a data file or a connector's output that reads or writes the
  filesystem or the network, undoes DuckDB's lockdown, gets past the statement guard, or runs
  outside the Landlock sandbox on Linux (REQ-2, REQ-3, REQ-6).
- **Cross-session access.** One session seeing or changing another session's data, scratch
  tables, results or files, or a tool or host-protocol parameter that names a path, workspace,
  session, URL or database (REQ-1, REQ-13).
- **Credential exposure.** A source credential or an exec connector's environment reaching the
  engine, the agent, a result, an error message or a log (REQ-5, REQ-15).
- **The release itself.** A bundle, image or install path that ships something other than what
  the release workflow built, such as a DuckDB library that is not DuckDB's release file.

Not security bugs, though still worth an ordinary issue: anything on native macOS, which has no
OS sandbox and serves only with `--allow-unsandboxed`; what an exec connector does, since it runs
with its process's rights and is trusted by the operator who configured it; the drop-in posture,
where the agent can already edit the config; and resource exhaustion beyond the ceilings the
README documents.

## Supported versions

Only the latest release is supported: fixes go into a new release, not into older ones. The
`main` branch may be ahead of it.
