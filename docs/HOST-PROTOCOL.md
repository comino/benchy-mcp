# Host protocol

Contract versions **`benchy-host/1`** to **`benchy-host/7`**, served by `benchy-mcp host`. In
version 2, one `attach` can put several tables on the bench. In version 3, a table can be streamed
through a pipe the host passes with the request, with no file on either side; an `attach` can
state the table's time column and series key; and `hello` can lower the session's limits
([hello](#frames), [attach](#attach)). In version 4, `hello` can give the agent's tools the
host's names ([Tool names](#tool-names-in-version-4)). In version 5, a table can come through the
session's stream channel, one more pipe benchy inherits, which needs no descriptor passing and so
no Unix socket ([Through a stream channel](#through-a-stream-channel-in-version-5)). In version 6,
`hello` reports the loader memory ceiling, and a host may lower it ([Memory](#frames)). In version
7, it reports the engine's threads, and a host may lower them ([Threads](#frames)). A host that
speaks an older version is
served as before: its requests mean what they meant, and its replies may carry fields it does not
know, which it ignores.

A *host* runs benchy for someone else. It decides what data a session may see, fetches that data
with its own credentials, and hands benchy the data — never the credentials. An agent talks to
the host; the host talks to benchy. benchy's own MCP server does the same work in-process, with
the same supervisor, loaders and engine.

## Roles

```
host ──benchy-host/7 on stdio──▶ supervisor ──frames──▶ engine (sealed, empty at start)
  │                                 │    ▲                   ▲
  ├──pipe (protocol 3)──▶ loader ───┘    └── store/ ──link──▶ ws-N/ ── ATTACH READ_ONLY, unlink
  └──stream channel (protocol 5)──▶ loader, one stream each, in turn
```

- The **host** starts one `benchy-mcp host` per session and speaks this protocol on its stdin
  and stdout: pipes, or a Unix socket, which a streamed attach needs. Only the process holding
  them can put data on the bench: capability, not name. Nothing a request carries can name
  another session, and a descriptor it carries is itself a capability.
- The **supervisor** is that process. It checks what the host sends, keeps it, and runs the
  engine: starts it, kills it past the hard deadline or the memory ceiling, rebuilds it (REQ-8).
  A host gets survivable failure without writing any of it.
- A **loader** is a short-lived process that turns one input into one DuckDB file. The loaders
  of one request run side by side.
- The **engine** runs agent SQL. It is sealed before anything is attached and never unsealed.

## Versions

A host learns what a session speaks from `hello`, at run time. What a release speaks can be
checked before that, when a host bumps its pin: in CI, before deploying. Each release has a
`compat.json`, listed in its `SHA256SUMS`, and `benchy-mcp --version --json` prints the same
object from the binary a host was handed, less `min_glibc`:

```json
{"benchy": "0.3.1", "duckdb": "1.5.5", "host_protocols": [1, 2, 3, 4, 5, 6, 7],
 "platforms": ["linux-x86_64", "linux-aarch64", "macos-arm64", "macos-x86_64"],
 "full_sandbox_min_landlock_abi": 4, "min_glibc": "2.35"}
```

- `benchy` — the release's version, as `hello`'s `benchy` says it.
- `duckdb` — the version of the DuckDB library it ships and links, as `hello`'s `engine` names it
  (`DuckDB v1.5.5`); asked of the library, not written down.
- `host_protocols` — every protocol version `hello` agrees to, oldest to newest, without gaps. A
  host whose newest version is not in it is answered an older one; one that shares none with it is
  refused with `E_VERSION`.
- `platforms` — the bundles the release has, as their names say them
  (`benchy-mcp-<platform>.tar.gz`).
- `full_sandbox_min_landlock_abi` — the oldest Landlock ABI under which the engine's posture is
  `full` with `network` true, as `sandbox_posture` reports it: below it, TCP is not confined
  (Linux 6.7 has ABI 4), and `--require-full-sandbox` refuses to start.
- `min_glibc` — the oldest glibc the Linux bundles run on: the newest `GLIBC_` symbol version the
  binary or DuckDB's library needs, read from the built files. Only in `compat.json`; no binary
  says it of itself.

Hosts ignore a field they do not know: one may be added in a later release. What the limits of
a flag set are is asked of `benchy-mcp host [flags] --print-limits`, which prints `hello`'s
`protocol`, `caps`, `limits`, `timeouts` and `unenforced` for a session that agrees on the newest
version, as JSON, and starts nothing. Flags or a config a session would refuse to start with, it
refuses the same way; the sandbox it does not open, so `--require-full-sandbox` is checked by
starting a session, or by `benchy-mcp doctor --json`.

## Frames

A four-byte big-endian length, then one JSON object. A request is at most 1 MiB — it is SQL or a
path, and every frame is parsed — and a reply is bounded by the caps. A request over that length,
or one that is not a JSON object benchy can read — nested deeper than 128, say, or holding a
number out of range — ends the session: a byte stream cannot be resynchronised.

Where stdin is a Unix socket, a request may also carry descriptors, as `SCM_RIGHTS` with the
request's own bytes: only a streamed `attach` takes them ([Streamed](#streamed-in-version-3)). A
request that carries one it does not take is refused as `E_REQUEST`, and every descriptor that
came with it is closed before the refusal is sent. benchy reads exactly one frame at a time, so a
descriptor sent with a frame's bytes is that frame's.

Every request carries an `id`, a whole number the host chooses, and every reply carries the `id`
it answers. An id may not be reused while its request is still being answered — by a `ping`
either; a request that does is refused with `"id": null`, since the request it collides with
will carry the id in its own reply. `ping` is answered at once, even while a query runs;
everything else runs one at a time, in the order it arrived, because the engine does one thing at
a time.

**Requests are strict.** A field benchy does not know is an error (`E_REQUEST`), never ignored: a
host that writes `max_row` is told, not served with the full caps. So is a key given twice, where
a JSON parser would quietly keep one of them. Replies may grow, so hosts ignore reply fields they
do not know. A new request field is a new protocol version.

The first request must be a well-formed `hello`. Anything else first — another call, a request
without an id, a field `hello` does not take, a version list that is not a list — is refused, and
the session ends: a host that cannot open a session has nothing else it can do in one.

```json
{"id": 1, "call": "hello", "protocol": [1, 2, 3, 4, 5]}
```

```json
{"id": 1, "ok": true, "protocol": 5, "benchy": "0.3.1", "engine": "DuckDB v1.5.5",
 "sandbox": "fully enforced",
 "caps": {"max_rows": 200, "max_bytes": 8192, "max_cell_bytes": 256, "deadline_secs": 20},
 "timeouts": {"load_timeout_secs": 300, "hard_grace_secs": 10},
 "limits": {"attach_rows": 10000000, "attach_bytes": 268435456, "attachments": 64,
            "session_rows": 50000000, "session_bytes": 2147483648,
            "memory": 1000000000, "memory_ceiling": 1500000000}}
```

`protocol` is the newest version both sides speak, and the session speaks it from then on: a
request field the agreed version does not have is refused like any unknown field. No version in
common is `E_VERSION`, and the session ends. A host lists every version it speaks, not only its
newest: a host that offers `[5]` to a benchy that speaks 1 to 4 is refused, where `[1, 2, 3, 4, 5]`
is answered 4 and the host knows what it may send. A second `hello` is refused as `E_REQUEST`,
and the session goes on. The caps are the command line's (`--max-rows`, `--max-bytes`,
`--max-cell-bytes`, `--deadline`). `timeouts` are what a host sets its own timeouts around: how
long one load may take before it fails (`--load-timeout`), and how long a query may run past its
deadline before the engine is killed and rebuilt (`--hard-grace`). They are beside the caps, not
in them, because a host may hand the caps back in a `query`, which takes only the four.

`sandbox` is how much of its Landlock confinement the engine's kernel enforces, as a sentence:

| `sandbox` | means |
|---|---|
| `"fully enforced"` | every right benchy asks for is confined |
| `"enforced, some newer rights unsupported by this kernel"` | confined with the rights this kernel has; one that newer kernels add is not, such as TCP before Linux 6.7 |
| `"not enforced"` | the kernel enforced nothing; served only with `--allow-unsandboxed` |
| `"unavailable: "` and the reason | Landlock could not be applied, as off Linux (`"unavailable: Landlock is Linux-only, and nothing equivalent is wired up here"`); served only with `--allow-unsandboxed` |

The first three are the whole string; the last begins with `unavailable: `.

| version | adds | refused before it |
|---|---|---|
| 1 | `hello`, `ping`, `attach` of one file (`name`, `path`), `detach`, `query`, `page`, `describe` | |
| 2 | `tables` in `attach`: several tables in one request ([Several tables](#attach)) | `tables`, as an unknown field |
| 3 | `"stream": true` in `attach`, a pipe passed with the request ([Streamed](#streamed-in-version-3)); `time_column` and `series` in `attach` ([A stated shape](#attach)); `limits` in `hello` | `stream`, `time_column`, `series` as unknown fields; `limits`, and the session ends |
| 4 | `tool_names` in `hello` ([Tool names](#tool-names-in-version-4)) | `tool_names`, and the session ends |
| 5 | `"stream": "channel"` in `attach`, the next stream on the session's stream channel ([Through a stream channel](#through-a-stream-channel-in-version-5)) | `"channel"`, as a value `stream` does not take |
| 6 | `loader_memory_ceiling` in `hello`'s `limits`, reported and lowered (**Memory** below) | `limits.loader_memory_ceiling`, and the session ends; it is not reported |
| 7 | `threads` in `hello`'s `limits`, reported and lowered (**Threads** below) | `limits.threads`, and the session ends; it is not reported |

Replies only grow: a field a reply gained in a later version (`results` beside a query's text,
`sql` on a page, `unenforced` beside the limits, `sandbox_posture` and `timeouts` in `hello`) is
sent whatever version was agreed, and a host that does not know it ignores it. The one exception is a field of
`limits`: a host may send `limits` back as it got it, and a field there that its version does not
take would end its session, so `limits.loader_memory_ceiling` (and
`unenforced.loader_memory_ceiling`) is reported only to a session that agrees on 6, and
`limits.threads` only to one that agrees on 7.

**The sandbox.** `sandbox` is prose for a person, and its wording may change between releases.
A host that decides on it reads `sandbox_posture`, how Landlock confines the session's engine,
measured as it was confined:

```json
"sandbox_posture": {"posture": "full", "landlock_abi": 4, "filesystem": true, "network": true, "reason": null}
```

- `posture` — one of four words: `full`, every right of the ruleset enforced; `partial`, some not
  (in practice a kernel before Linux 6.7, Landlock ABI 4, which has no TCP rules); `none`, nothing
  enforced; `unavailable`, no Landlock to apply (off Linux, or where the kernel refused it).
  Anything but `full` and `partial` serves only with `--allow-unsandboxed`.
- `landlock_abi` — the kernel's Landlock ABI, 0 where it has none.
- `filesystem` — whether the engine is kept to its own files: true under `full` and `partial`.
- `network` — whether it is kept off TCP too: true under `full` at ABI 4 or later. Under `partial` a
  compromised engine, file loader or ClickHouse or HTTP loader may connect anywhere; its files
  are confined all the same, and DuckDB's own network access is off (REQ-2).
- `reason` — why `network` or `filesystem` is false, as prose; `null` where both are true.

A host that relies on the network boundary need not check: the operator starts it with
`--require-full-sandbox`, and a session whose engine is not `full` with `network` true does not
start, before any table is loaded, whatever `--allow-unsandboxed` says. `--require-memory-ceilings`
does the same for the engine's and the loaders' memory ceilings: where they are not enforced (off
Linux, **Memory** below), the session does not start. Either way the reason goes to stderr and
there is no `hello` to answer. Neither is a protocol field, so neither needs a version.

**The limits are the operator's, and a host may lower them** (version 3). The operator sets the
attach and budget limits for every session of `benchy-mcp host`, raised or lowered from the
defaults above: on the command line, `--attach-rows`, `--attach-bytes`, `--session-rows` and
`--session-bytes`; or in a config given as `benchy-mcp host --config limits.toml`, which holds
`[attach]` (`rows`, `bytes`) and `[budget]` (`rows`, `bytes`), `[engine]` and `[helpers]` if any
([Helpers](#helpers)), and no sources. A limit set in both places, or set to 0, stops the start.
`attachments` is 64 and no operator setting changes it; `memory` and `memory_ceiling` are set as
**Memory** below says, and `loader_memory_ceiling` too, and `threads` as **Threads** says. A host
may then lower any of six for its session in `hello`: `attach_rows`, `attach_bytes`,
`attachments`, `session_rows`, `session_bytes` and `memory_ceiling`, from version 6 a seventh,
`loader_memory_ceiling`, and from version 7 an eighth, `threads`. It
never raises one, as it lowers the caps in a `query`, and `memory` is not
its to lower. `hello` answers with the session's:

```json
{"id": 1, "call": "hello", "protocol": [1, 2, 3], "limits": {"attach_rows": 1000000, "attachments": 8}}
```

A limit raised, set to 0, unknown (`memory` included), or given in a session that agrees on
version 1 or 2, and a `memory_ceiling` below `memory`, is `E_REQUEST`, and the session ends, as
anything wrong with `hello` does. Why the operator: a host's
load reaches 240 MB of Arrow, near the 256 MiB an attachment may be by default; the default is a
guard against a mistake, and it is the operator, who knows the loads and the machine, who moves
it.

**Memory.** `memory` is DuckDB's own budget in the engine (`--memory`), reported and not a host's
to change. `memory_ceiling` is the most resident memory the engine may hold: `--memory-ceiling`
or `[engine] memory_ceiling = "2GB"` in the config, never both, and by default `memory` and half
again, at least 256 MiB over it. While the engine answers a request the supervisor reads its
resident memory every 50 ms, and past the ceiling kills it and rebuilds it, as past the hard
deadline, whatever the query was doing: DuckDB's budget does not bound planning, a very long
string or a large ENUM type, and in a container one engine's growth would otherwise be every
session's OOM kill. A host running many sessions in one container lowers it in `hello` like any
other limit, `"limits": {"memory_ceiling": 1200000000}`, and not below `memory`, under which
every query DuckDB would refuse cleanly as out of memory would kill the engine instead.

The ceiling holds between requests too. The supervisor reads the engine's resident memory once
more as it answers, and an engine past the ceiling then is killed and rebuilt there, that call
answered `E_REBUILT`, rather than left past it, idle, for the next call to be killed for. Between
requests the engine runs nothing and nothing is watched, so an idle session costs no CPU, and
what an idle engine holds is at most what it held as it last answered. There is no separate,
lower ceiling for an idle engine: what it keeps between requests is the agent's tables, inside
`memory`, and allocator memory it gives back by itself within about a second; what DuckDB's
budget does not count, an ENUM's values, the agent cannot keep (REQ-9). A host that wants idle
sessions to sit lower sets a lower `memory_ceiling`.

The ceiling is enforced on Linux only: resident memory is read from `/proc`. Elsewhere `hello`
reports `"memory_ceiling": null` and says why beside the limits, and only the deadlines bound the
engine; a host that relies on the ceiling checks for a number:

```json
"limits": {..., "memory": 1000000000, "memory_ceiling": null},
"unenforced": {"memory_ceiling": "the engine's resident memory is read from /proc, which this platform does not have; only the deadlines bound the engine here"}
```

`unenforced` is absent where everything reported is enforced.

**The loader memory ceiling** (version 6). Every load runs in a loader of its own. A file's is
held to its stage's cap, but nothing else bounds what a connector holds: it may hold whatever it
likes before it writes a row. `loader_memory_ceiling` is the most resident memory one loader
may hold, with the connector it runs and everything that connector starts: `--loader-memory-ceiling` or
`[loader] memory_ceiling = "1GB"` in the config, never both, and by default 1 GB, well above what
a load is measured to need (the harness's 10,000,000-row loads peak at 94 to 410 MB). The
supervisor reads each loader's resident memory every 50 ms while it runs, and past the ceiling
kills it, with its connector's whole process group. The attach is `E_LIMIT`, saying it was the
loader memory ceiling, what was held, and what to do (narrow the source, or ask the operator for
a larger ceiling); nothing of the table is attached, and a stream from the channel loses its
place, which closes the channel ([Through a stream channel](#through-a-stream-channel-in-version-5)).
A host running many sessions lowers it in `hello`, `"limits": {"loader_memory_ceiling":
300000000}`; it is reported from version 6, and lowering it before 6 is `E_REQUEST`. Off Linux it
is not enforced: `"loader_memory_ceiling": null`, with `unenforced.loader_memory_ceiling` saying
why. The loaders of one attach run side by side, up to four at once, each under the ceiling.

**Threads** (version 7). `threads` is how many threads the engine runs agent SQL on:
`--threads N` or `[engine] threads = N` in the config, never both, and 1 by default, so a
runaway query takes one core of a shared machine. On a machine with cores to spare, the
operator gives each session more. A grouped aggregate over 10,000,000 rows then runs several
times quicker (docs/PERFORMANCE.md, *Engine threads*). A host running many sessions on that
machine lowers it in `hello`, `"limits": {"threads": 2}`. It is reported from version 7,
lowering it before 7 is `E_REQUEST`, and so is raising it or setting it to 0. The engine has
already opened on the operator's number when `hello` arrives, and its threads are locked as it
opens. So a host that lowers them has that engine replaced before its first call by one on its
own number, with the helpers and no table. The replacement costs one engine start, paid by the
first call. DuckDB's memory budget, `memory`, is one budget whatever the threads, and
`memory_ceiling` bounds the engine as before. More threads hold more at once inside that
budget (*Engine threads* has the measurement), so a host that lowers `memory_ceiling` close to
`memory` leaves less room on more threads. The threads are started after the engine is
confined, and each is confined as the engine is (`sandbox_posture`).

### Tool names, in version 4

benchy's texts name the agent's tools: a result's footer says to read on with `get_result`, a
hint says to run `describe`, `describe` says what is over its budget. A host relays the agent's
calls through tools of its own, and if they are called something else, those texts send its
agent to tools that do not exist. So `hello` may say what the host calls them, and every text of
the session — footers, hints, `describe`, errors — uses those names:

```json
{"id": 1, "call": "hello", "protocol": [4],
 "tool_names": {"get_result": "bench_get_result", "describe": "bench_describe"}}
```

- **The keys** are benchy's tools: `describe`, `query`, `get_result` and `dialect`. A host names
  the ones it renames; the rest keep benchy's names. Without `tool_names`, every text names
  benchy's own, as before, and benchy's MCP server always does.
- **The names** are plain: an ASCII letter, then letters, digits, `_` or `-`, at most 64 bytes, as
  tool names are on the platforms agents run on. Two tools under one name, or a tool under the
  name of another of benchy's, would send the agent to one for the other. They are labels in
  prose: benchy resolves nothing by them, and a name that could be a path or a URL is refused.
- **Refused** like anything else wrong with `hello`, as `E_REQUEST`, and the session ends: a key
  that is not a tool, a name that is not plain, two tools under one name, or `tool_names` in a
  session that agrees on version 3 or older.
- **The dialect reference is the host's to serve.** The protocol has no `dialect` call: a host
  that gives its agent one serves [DIALECT.md](DIALECT.md) itself, as benchy's MCP server does,
  with the `<!-- maintainers -->` blocks left out. That file is static, and names benchy's own
  tools (`describe`, `query`, `get_result`); `tool_names` does not reach it, so a host that
  renames them rewrites those names in its copy.
- **Why a version.** A new request field is a new version ([Frames](#frames)). `hello` is one
  request, so a benchy that speaks only 3 refuses `tool_names` as a field `hello` does not take,
  and the session ends: a host that may meet one reads `benchy-mcp --version` first. Nothing else
  changed in version 4, and a host speaking 3 is served the same.

## Errors

`{"id": 7, "ok": false, "error": {"code": "E_DATA", "message": "..."}}`. A `message` may be shown
to a model: it names no path of the session's, and none of the host's — a host knows which file
it sent.

| Code | Means |
|---|---|
| `E_REQUEST` | malformed: unknown call or field, wrong type, a cap or limit raised or below its floor, an id in use, a `tables` list that is empty, longer than `attachments` or names a table twice; a descriptor where none is taken, not one for each streamed table, or a streamed attach on a session whose stdin is not a socket; `"stream": "channel"` on a session started without `--stream-channel`, or that agreed a version before 5. A refused request that names a table from the stream channel closes the channel, and says `"channel": "closed"` |
| `E_VERSION` | no protocol version in common; the session ends |
| `E_NAME` | a table name the agent could not type, a name the agent already gave a table or view of its own, or no table of that name to detach |
| `E_DATA` | an attachment refused: not a regular file (or, streamed, a descriptor that is not a pipe, a socket or a regular file), not an Arrow stream, no end-of-stream marker or anything after it, compressed with anything but LZ4, a type outside the vocabulary, a column unnamed or named twice, a stated time column or series column it does not have, a stated time column that is not a date or timestamp. Nothing changed. |
| `E_LIMIT` | a limit reached: rows or bytes in one attachment, what the session's budget has left, the number of tables, or the loader memory ceiling (version 6 reports it; it applies in every version) |
| `E_SQL` | the SQL failed; the message is the one the agent would see |
| `E_DEADLINE` | a query passed its deadline and was interrupted, or an attach took longer than `--load-timeout`; the bench is intact |
| `E_NO_RESULT` | no retained result by that id: never produced, dropped with a detach or replace, or from before a rebuild |
| `E_REBUILT` | the engine was killed and rebuilt while this call ran — past the hard deadline, past the memory ceiling while it ran or as it answered, or because the batch left a dynamic PIVOT's type that could not be dropped; the message names which: its result is lost and the agent's own tables are gone, every attached table is back |
| `E_UNAVAILABLE` | the bench could not be rebuilt. After two failures in a row it is not tried again until a detach changes what a rebuild would attach |
| `E_INTERNAL` | benchy failed on its own side; the operator's log says why, and nothing changed |
| `E_CHANNEL` | a table from the stream channel after the channel was closed: it lost its place in an earlier stream, or an earlier request that named it was refused whole, and that answer said so with `"channel": "closed"` ([Through a stream channel](#through-a-stream-channel-in-version-5)). Nothing changed; everything else still works |

## The calls

### attach

```json
{"id": 2, "call": "attach", "name": "readings", "path": "/abs/path/readings.arrows"}
```

Puts the file on the bench as table `name`, read-only, at any point in the session.

- **The file** is an Arrow IPC *stream*, as a connector writes one. The last component of its
  path is not followed if it is a link, and it must be a regular file. It is held to what a
  connector's stream is held to, against its own schema, since a host declares none: the
  end-of-stream marker is mandatory — a stream cut at a message boundary parses as a valid, shorter
  one — and it must end the file: one stream per file, nothing after its marker. A body may be
  LZ4-compressed and a column dictionary-encoded or a view, and what the stream decodes to,
  decompressed and unpacked, counts against `attach_bytes` as it is read, as one total: its
  bodies and what its dictionary and view columns unpack to together; a zstd body is refused. Every column's type must be in the vocabulary (the "Types" table in
  [EXEC-CONNECTOR-PROTOCOL.md](EXEC-CONNECTOR-PROTOCOL.md)), and it is held in the bench's own
  types, the same as a file's or a connector's: a zone-less timestamp is read as UTC. No two
  columns may have names that differ only in case, which DuckDB does not tell apart.
- **The name** is lowercase letters, digits and underscores, starts with a letter, is at most 63
  bytes, is not a name benchy or DuckDB uses itself (`main`, `memory`, `system`, `temp`,
  `benchy_…`), and is one DuckDB takes as a table name unquoted — proven by DuckDB, not by a list:
  `left` and `join` are not reserved words, and still no query can name them. Keywords such as
  `data`, `value` or `values` are fine. A name that does not meet the rules is refused, not
  rewritten, so the host knows exactly what the agent types.
- **Replacing** is attaching a name that is already there. The old table serves until the new one
  is attached; if the new one is refused, nothing changes. Retained results are dropped, since
  some may hold the old rows: replacing is how a host takes rows away.
- **Limits.** One attachment holds at most `attach_rows` rows in `attach_bytes` bytes. Everything
  attached in a session counts against `session_rows` and `session_bytes`, replacements included,
  by what benchy measured — the rows the engine counted, the file's size before it was read, or
  a stream's bytes as its loader read them — and only once it is on the bench: a refused attach
  costs nothing. `describe` says what is left. At most `attachments` tables are on the bench at
  once; replacing one does not add one.

Reply: `{"id": 2, "ok": true, "name": "readings", "rows": 1234, "columns": [["observed_at", "TIMESTAMP WITH TIME ZONE"], ...]}`.

**A stated shape, in version 3.** `time_column` (a column's name) and `series` (a list of one or
more) name the table's time column and the columns that tell its series apart, on an attach of
one table or on any entry of `tables`, as an operator's config can name a source's:

```json
{"id": 2, "call": "attach", "name": "readings", "path": "/abs/path/readings.arrows",
 "time_column": "observed_at", "series": ["meter"]}
```

`describe` then says what they are, where a heuristic says what they "look like". They are held
to the stream's own schema before a row is written: a name it does not have, or a time column
that is not a date or a timestamp, is `E_DATA`, and nothing changes. A name given twice, a column
given as both, an empty `series`, or a value of the wrong type is `E_REQUEST`. Either may be left
out, and a heuristic then decides that one. The same data with the same stated shape gives the
same `describe` from a host's file, a host's stream, a file source and a connector
(`a_stated_shape_looks_the_same_from_every_source`).

**Several tables, in version 2.** `tables` in place of `name` and `path` lists up to
`attachments` tables, each `{"name", "path"}`:

```json
{"id": 2, "call": "attach", "tables": [{"name": "readings", "path": "/abs/path/readings.arrows"},
                                       {"name": "meters", "path": "/abs/path/meters.arrows"}]}
```

```json
{"id": 2, "ok": true, "tables": [
  {"name": "readings", "ok": true, "rows": 1234, "columns": [...]},
  {"name": "meters", "ok": false, "error": {"code": "E_DATA", "message": "..."}}]}
```

- **Each table is answered on its own**, in the order asked, exactly as an `attach` of its own
  would be: the same checks, the same codes, the same "nothing changed" on a refusal. One that is
  refused costs the others nothing. `"ok": true` on the reply says the request was carried out;
  each table's own `ok` says whether it is on the bench.
- **In order.** The count of tables and the session's budget are taken as if each table had
  been attached after the ones before it in the list: with room for one more table, the first new
  name takes it and the next is `E_LIMIT`; a table is held to the rows and bytes the ones before
  it left. A table the engine refuses at the last step has already taken its room from the ones
  after it, which may then be refused where one at a time they would not.
- **What is wrong with the list refuses the whole request** as `E_REQUEST`, and nothing is read:
  `tables` with `name` or `path` beside it, a list that is empty or longer than `attachments`, a
  name given twice (the order of the list would decide which file the table is), an entry with a
  field it does not take or without one it needs.
- **Why.** Each table's loader is a process start and a DuckDB open, and each attach is checked
  by the engine with a listing of every function DuckDB has: about 280 M instructions and 120 ms
  a table, one request at a time. A list's loaders run side by side, as many at once as the
  machine has cores and at most four, and the engine is asked once for all of them; eight
  1,000-row tables took 2.48 G instructions and 1.16 s one request at a time, and 1.63 G and
  0.61 s in one (docs/PERFORMANCE.md, R2-5).

**Streamed, in version 3.** `"stream": true` in place of `path` — on an attach of one table, or
on any entry of `tables` — and the read end of a pipe sent with the request; the host writes the
stream into the other end as it fetches it. Nothing is spooled to a file, on the host's side or
benchy's:

```json
{"id": 2, "call": "attach", "name": "readings", "stream": true}      + SCM_RIGHTS [read end]
{"id": 3, "call": "attach", "tables": [{"name": "readings", "stream": true},
                                       {"name": "meters", "path": "/abs/path/meters.arrows"}]}
                                                                      + SCM_RIGHTS [read end]
```

- **The session's stdin is a Unix socket** (a socket pair; stdout may be the same socket). A
  descriptor can come over nothing else: on pipes, `stream` is refused as `E_REQUEST`, saying so. A
  host that cannot send a descriptor uses the [stream channel](#through-a-stream-channel-in-version-5).
  [`examples/host.py`](../examples/host.py) does it in standard-library Python
  (`socket.socketpair`, `socket.send_fds`).
- **One descriptor a streamed table**, in the order of the list, sent with the request's own
  bytes, and no other: a request that streams two tables and brings one descriptor, or three, is
  refused whole as `E_REQUEST`, nothing read, and every descriptor it brought closed. A
  descriptor must be a pipe, a socket or a regular file (`E_DATA` otherwise); benchy reads it
  blocking, whatever its flags, and the host closes its own copy of the read end once sent.
- **Write, then close.** The host writes the stream into the write end and closes it after the
  end-of-stream marker: nothing may follow the marker, and the end of the pipe is how that is
  known, so an attach whose writer stays open waits until `--load-timeout`. The reply comes once
  the stream is read, and a pipe holds 64 KiB, so the host writes on a thread of its own (or
  without blocking) and reads the reply. The tables of one request are read by loaders started in
  the order of the list, at most four at once: a host that writes their streams one after another
  in that order, or each on its own thread, is never left waiting on a table not yet read.
- **Every rule of the file form holds, and the table is the same.** The marker is mandatory and
  nothing may follow it; a body may be LZ4 and not zstd; the type vocabulary applies; what the
  stream decodes to counts against `attach_bytes` as it is read; a replacement keeps the old table
  until the new one is in; a refused stream changes nothing. From its first batch on, a stream
  goes into its table by the same path as a file's, so the table, its `describe` and its physical
  layout are the file form's (REQ-15). What differs is when a rule can be checked: a file's size
  and marker are known before a batch is read; a stream's bytes are counted as they arrive and the
  read stops at the first byte past `attach_bytes` or what the budget has left, and its marker is
  found where it ends.
- **A refusal stops the reading.** The loader exits and the read end closes with it, so the
  host's next write fails with `EPIPE` (a host ignores `SIGPIPE`, as Python does) and the reply
  says why.
- **The budget.** A file's bytes are measured by the supervisor before a loader reads it. A
  stream's pass through its loader alone, which counts them as it reads, against the same bounds,
  and reports them: the one count the supervisor takes from a loader, since no byte of the stream
  passes through it. For the same stream it is the file's size.
- **`ping` is not held up.** The stream goes from the host to its loader; the reader that answers
  `ping` sees only the request.
- **Why a descriptor.** Of the three ways to stream measured in docs/PERFORMANCE.md (R5-H1), it is
  the one where no byte of the stream passes through the supervisor and no name is involved.
  Binary frames on the protocol's own stdin would copy every byte through the supervisor, and tie
  `ping` to the stream: a ping sent behind a megabyte of stream waits until a loader has read
  that megabyte. A pipe benchy made and handed back would have to be named (a FIFO's path, which
  anyone who can reach it can write to) or passed back as a descriptor all the same.

### Through a stream channel, in version 5

A descriptor can only travel over a Unix socket, and not every host can send one: Node.js has no
public API that puts a descriptor (`SCM_RIGHTS`) inside a framing of the host's own, and Java has
none at all. A stream channel needs none. The host starts benchy with one more inherited pipe,
names it on the command line, and writes Arrow streams into it back to back; each attach with
`"stream": "channel"` takes the next whole one:

```js
// Node.js: stdin and stdout for the protocol, stderr for the log, stdio[3] for the streams.
const benchy = spawn("benchy-mcp", ["host", "--stream-channel", "3"],
                     {stdio: ["pipe", "pipe", "inherit", "pipe"]});
benchy.stdio[3].write(arrowStream);           // as it is fetched
// {"id": 2, "call": "attach", "name": "readings", "stream": "channel"}
```

```json
{"id": 2, "call": "attach", "name": "readings", "stream": "channel"}
{"id": 3, "call": "attach", "tables": [{"name": "readings", "stream": "channel"},
                                       {"name": "meters", "stream": "channel"}]}
```

[`examples/host-node.mjs`](../examples/host-node.mjs) is a whole host on Node.js;
[`examples/host.py`](../examples/host.py) passes the channel with `pass_fds` at its own number.

- **Named, or not a channel.** `--stream-channel FD` names the descriptor, 3 or more, and it must
  be open for reading and a pipe, a socket or a file, or benchy does not start. A descriptor a
  session merely inherits is not read, and without the flag `"stream": "channel"` is `E_REQUEST`.
  benchy makes it blocking (an event loop's pipes often are not) and close-on-exec.
- **One stream at a time, in request order.** Requests run one at a time, and a `tables` list
  reads its channel tables in the order of the list, each loader starting once the one before has
  read its stream to the end; the tables that come by file or descriptor load beside them. Where
  the session has one CPU (a pod with `cpu: 1`), one loader runs at a time, and the tables load
  in the order of the list, channel and descriptor tables alike
  (`a_channel_and_a_descriptor_load_in_request_order_on_one_cpu`). The host writes on a thread of its own (or without blocking) and reads the replies: a pipe holds
  64 KiB. It may write ahead of the requests, or after sending them: either way a stream is only
  ever the table's it was written for, or the channel closes (below), so writing ahead can lose
  the channel, never put one table's rows under another's name. The pipe stays open between
  streams; the host closes it when it has no more.
- **Check a stream's size first.** A stream past `attach_bytes` is refused, and more than
  `attach_bytes` left of it once refused closes the channel. A host that knows a stream's size
  (a file, a fetch with a length) holds it to `attach_bytes` from `hello`'s `limits`, and to what
  `describe`'s `budget` has left, before it writes a byte, and attaches what is over by another
  form or not at all.
- **Every rule of the file form holds, and the table is the same.** The end-of-stream marker is
  mandatory; a body may be LZ4 and not zstd; the type vocabulary applies; what the stream decodes
  to counts against `attach_bytes` as it is read, and its bytes against the budget, as a
  descriptor's do; a replacement keeps the old table until the new one is in; a refused stream
  changes nothing. The stream goes into its table by the same path as a file's, so the table, its
  `describe` and its physical layout are the file form's (REQ-15). What follows a stream's marker
  is the next stream, not an error: the loader reads to the marker and not a byte further.
- **A refused stream is read past.** Whatever refuses a table — its stream, a limit, its name, the
  count of tables — its stream is still in the channel. So benchy reads it on to its end-of-stream
  marker and drops it, without decoding it, and the next attach takes the next stream; the
  table's answer is its own refusal, and the host's writes all go through. This reading is
  bounded: at most `attach_bytes` more bytes once the stream is refused, within `--load-timeout`.
- **Where its place is lost, the channel is closed, and says so.** If benchy cannot find the end
  of a stream — the channel ended before its marker (the host closed it part-way), what came is no
  sequence of Arrow messages, more is left of a refused stream than `attach_bytes`, or its load
  ran past `--load-timeout` or the loader memory ceiling — the position of the next stream is lost. benchy then closes the
  channel: the host's next write fails (`EPIPE`; on Node.js `ECONNRESET`, below), rather than
  waiting on a reader that will never come. The table's answer carries its own code with `"channel": "closed"` in its error,
  and its message says why; every channel table after it, in that request or a later one, is
  `E_CHANNEL`. The session goes on: files, descriptors, queries, `ping`.
  `{"name": "readings", "ok": false, "error": {"code": "E_DATA", "channel": "closed", "message": "readings: it does not end with the Arrow end-of-stream marker ... The stream channel is closed: the channel ended before the stream's end-of-stream marker; ..."}}`
- **A request refused whole that names the channel closes it.** A request refused as
  `E_REQUEST` reads nothing — a name given twice in `tables`, a field an entry or the call does
  not take, a list longer than `attachments`, `"channel"` before protocol 5, an id still in use,
  whatever it is. If it names a table from the channel (`"stream": "channel"`, anywhere in it),
  its streams may already be in the channel, written ahead, and the next attach would take one of
  them under its own name. So benchy closes the channel, as when it loses its place: the refusal
  carries `"channel": "closed"` and says why, and every channel table after it is `E_CHANNEL`.
  It is answered in its turn, behind the requests sent before it, which read their own streams
  first. A refused request that names no table from the channel leaves it be.
  `{"id": 4, "ok": false, "error": {"code": "E_REQUEST", "channel": "closed", "message": "`tables` names `meters` twice. The stream channel is closed: a request that named tables from it was refused whole, ..."}}`
  (`a_refused_request_that_names_the_channel_closes_it`)
- **On Node.js, give the channel an `'error'` handler.** A write after benchy has closed the
  channel fails with `ECONNRESET` there (Node 24, `stdio[3]`), not `EPIPE`, and a stream with no
  `'error'` handler takes the whole host down with it. Nor is the event the only sign: Node may
  destroy the stream as soon as benchy closes its end, before the host writes again, and may hand
  a write's error to its callback before any `'error'` event is seen (`EPIPE` on macOS). Keep the
  first error from either, and treat a destroyed channel as closed.
  [`examples/host-node.mjs`](../examples/host-node.mjs) shows both.
- **`ping` is not held up**, and no byte of a stream passes through the supervisor: each loader
  is handed a copy of the channel as its descriptor 3, and the supervisor never reads it.
- **Beside the descriptor form.** Both may be used in one session, in one request.
- **Why a version.** `stream` is a field version 3 has, but `"channel"` is a new form of it, and a
  host that writes streams into a channel benchy does not read would wait on a full pipe. A host
  offers 5 and sees 5 agreed before it writes a byte; a benchy without the channel also refuses
  `--stream-channel` at startup.
- **Why it costs what the descriptor form costs.** The loader reads the same bytes by the same
  calls: Arrow messages are already read one at a time, by their stated lengths, so stopping at
  the marker needs no read of its own, only each message's metadata followed as it passes. What
  was measured is in docs/PERFORMANCE.md (R5-H2).

**How.** A loader, confined to reading the one file and writing into its own staging directory,
validates the stream and writes one DuckDB database holding one table. The supervisor takes
nothing on the loader's word: the table's name is the request's, and the file is the one in the
staging directory, a plain file with no other name and its write-ahead log merged. It moves the
file into the session's `store/`, hard-links it into the engine's workspace, and asks the engine
to attach it. The engine attaches it `READ_ONLY` under a fresh alias, checks it holds that one
table and nothing else, puts the alias on the agent's search path after the agent's own schema,
unlinks the link, and detaches the alias it replaced.

Why each step, measured on DuckDB 1.5.5:

- The engine's workspace is the only directory it may attach from after the lock: it is named in
  `allowed_directories` before external access is turned off, the one order DuckDB accepts.
- An allowed directory is open to every file reader, not only `ATTACH` — `read_duckdb`, a plain
  `FROM 'x.duckdb'`, `read_blob`, `read_text`, `glob`. So nothing stays in it: a link lives from
  being made until the attach answers, and the engine unlinks it whatever happened. DuckDB reads
  through the handle it holds. Each engine gets a fresh workspace, so a killed one leaves nothing
  for the next.
- An attached catalog is on the agent's search path, and DuckDB resolves a function there before
  its own: a macro named `lower` in the file stood in for `lower`. So a file holding anything but
  its table — a macro, a view, a sequence, a type, a schema, an index, a second table — is
  refused before the agent can see it.
- A `READ_ONLY` catalog cannot be written, dropped or detached by the agent: `ATTACH`, `DETACH`,
  `USE` and `SET` are all outside the allowlist. The agent's own schema is first on the search
  path, where its `CREATE` lands. No agent object can take a host table's name: a `CREATE` or
  `ALTER` that would is refused and undone (REQUIREMENTS.md REQ-9), and an attach under a name
  the agent already gave a table or view of its own is refused with `E_NAME` rather than hidden
  behind it. The host chooses another name; the agent's object stays.
- The alias it replaces is detached only after the search path has moved on: detaching a catalog
  still on the path breaks every unqualified name, the agent's own tables included.

### detach

```json
{"id": 3, "call": "detach", "name": "readings"}
```

Takes a table away, with every retained result, since some may hold its rows. It always takes
effect: the table leaves the store first, so no rebuild brings it back. Copies the agent already
made into its own tables stay — revoking data completely means ending the session.

### query

```json
{"id": 5, "call": "query", "sql": "SELECT ...", "caps": {"max_rows": 50}}
```

Runs SQL exactly as the agent's `query` tool does, as the agent: it sees what the agent sees, the
agent's own tables first. It returns the same text, with the result ids, what each result holds,
and the caps that applied:

```json
{"id": 5, "ok": true, "text": "...", "result_ids": ["3fa9c1b2-7"],
 "results": [{"result_id": "3fa9c1b2-7", "rows": 500, "complete": true, "shown": 50, "truncated": true}],
 "caps": {...}}
```

`results` has one entry a result, in the order of `result_ids`, so a host acts on a result without
reading the text, whose wording may change between releases — an empty one, say, which benchy
explains only for an attached table and a host may explain for its own helpers:

- `rows` — the rows benchy holds of the result: all of them when `complete`, otherwise the first
  2,000 kept for paging, or as many as were read before reading stopped.
- `complete` — whether `rows` is every row the statement returned.
- `shown` — the rows `text` shows of it: 0 for a result the byte cap left out of the text, which
  then says how to fetch it.
- `truncated` — whether `text` is less than the whole result: a row not shown, a value cut at the
  cell cap, a row cut at the byte cap, or rows past what was kept. It is when the text says
  `TRUNCATED`.

A cap in `caps` —
`max_rows`, `max_bytes`, `max_cell_bytes` or `deadline_secs`, the four `hello` reports — may lower
the session's for this call, never raise it; `max_bytes` stays at least 512 and `max_cell_bytes`
at least 16, below which a result could not say it was cut, and `deadline_secs` at least 1.

A batch stops at the first statement that fails, and the call is refused with `E_SQL` (or
`E_DEADLINE`). The statements before it have run and keep their effects, as for the agent: the
error's message comes first, then their results, with their result ids in the text, all inside
`max_bytes`.

### page

```json
{"id": 6, "call": "page", "result_id": "3fa9c1b2-7", "offset": 200, "caps": {"max_rows": 50}}
```

Reads a retained result further, as the agent's `get_result` does:
`{"id": 6, "ok": true, "text": "...", "sql": "SELECT ..."}`. An `offset` at or past the last row
kept is not an error: the page holds no row, and its footer says so and where the rows end
("no rows at offset 10000: result_id … has 500 row(s), at offsets 0 to 499").

`sql` is the statement the result came from, as the agent wrote it — one statement of its batch,
comments left out — so a host whose tool returns a result with its SQL keeps no mapping of its
own. Comments and semicolons are found as DuckDB's lexer finds them, so every string literal
(`'…'`, `E'…'`, `$$…$$`, `$tag$…$tag$`) and quoted name is kept whole: a `--`, `/*` or `;` inside
one is its text. It is the agent's own text and nothing else: a view or macro the statement calls
is named, never expanded, so no helper's body is in it. It is bounded like the text, cut at
`max_bytes` and saying so, and benchy keeps at most 64 KiB of it. It is left out where the
statement cannot be told apart in its batch: where the split at the batch's semicolons does not
find the statements DuckDB found, no result is given another statement's SQL.

**Result ids** are opaque strings: read them, never build them. Each starts with a random prefix
of its session's own, so no two sessions issue the same id, and a host whose conversations
outlive sessions cannot resolve an old id to another session's answer: in any other session it
is `E_NO_RESULT`. The prefix names nothing — no path, no process, no session another could use.

### describe

```json
{"id": 8, "call": "describe"}
```

The bounded catalogue the agent sees, as text; the attached tables as entries; and what the
session's budget has left. The text includes the shape of each table — its time column, cadence,
series and their quirks — which the loader found when the table was attached; describing costs
nothing however large the tables are. It lists the operator's helpers too, if `host --config`
gave any ([Helpers](#helpers)). It is prose for the agent and may change between releases:
a host that needs facts reads `tables`.
`{"id": 8, "ok": true, "text": "...", "tables": [{"name": "readings", "rows": 1234, "columns": [...]}], "budget": {"rows_left": 49998766, "bytes_left": 2147401648}}`.

### ping

`{"id": 9, "call": "ping"}` is answered at once: `{"id": 9, "ok": true}`.

## Helpers

An integrator gives the agent views and macros in the config it starts the session with, not
through a call: `benchy-mcp host --config host.toml`, whose `[helpers]` names a SQL file of
`CREATE VIEW` and `CREATE MACRO` statements, or holds them inline (REQ-18, the README's "SQL
helpers", `examples/helpers.sql`). They are made before `hello` is answered, against no table
yet, and fixed for the session:

- A helper that cannot be made — named like a DuckDB built-in or with the `benchy_` prefix,
  reading a file, the network or an extension, keeping an ENUM (the agent may keep none, for the
  memory DuckDB does not count), or not a view or a macro — stops startup, and the reason goes to
  stderr.
- A helper view that reads a table not attached yet **waits** for it. `describe`
  lists it with what it waits for, a query naming it says so (`E_SQL`), and the attach of that
  table makes it; the operator's log says what still waits.
- **After a detach**, a helper made over that table stays made: `describe` lists it as before,
  not as waiting, and a query that calls it fails with `E_SQL`, DuckDB's "Table with name
  readings does not exist!", until the host attaches a table of that name again. The helper
  then reads the new table with no rebuild of the helpers.
- Every helper's name, made or waiting, is taken: an `attach` under it is refused with `E_NAME`,
  and the agent can give no object of its own that name. The agent cannot drop, change or
  replace a helper: they live in a catalog attached `READ_ONLY`.
- **Every table a helper reads is taken too**, attached or not, for as long as the session
  lasts: the agent can give no table or view of its own that name, before the first attach or
  after a detach. So a helper never reads the agent's rows in place of the host's, and the
  host's attach of the table is never refused for an object of the agent's. The tables come
  from DuckDB, not from benchy reading the SQL: a view's bound plan, and the table a waiting
  helper waits for. So a view names its tables in FROM: one that reads a table named in a string
  (`pragma_table_info(…)`, `duckdb_table_sample(…)`) stops startup, except a view over `query` or
  `query_table`, whose bound plan holds the table.
- **A helper macro reads no table.** DuckDB 1.5.5 expands a macro, table or scalar, inside the
  caller's query, so a table its body names binds to a CTE of the caller's of that name, with
  `ok: true`; a view is bound on its own and reads the table. So a helper macro whose body reads
  a table, a helper view, or a table function other than `range`, `generate_series` and
  `unnest`, or calls a helper macro that does, stops startup, and the reason says to write it as
  a view (`a_helper_macro_that_reads_a_table_is_refused_at_startup`). The body is read as
  DuckDB's parser serializes it, and what benchy cannot read counts as reading a table. No CTE of
  the agent's, then, changes a helper's answer, and the agent's SQL is not looked at for one.
- DuckDB names a waiting helper's missing tables **one at a time**: a view over `readings` and
  `weather` waits for `readings`, and says it reads `weather` only once `readings` is attached.
  `weather` is taken from then on. If the agent made a `weather` of its own before that, the
  agent keeps it: the host's attach of `weather` is refused with `E_NAME` until the agent drops
  it, and the helper waits meanwhile, reading nothing of the agent's.
- A rebuilt engine, whether the hard deadline or the memory ceiling killed the last one, is given
  the helpers before its tables.
- A helper names tables unqualified. They resolve when it is called, to the table on the bench
  then: a table's catalog is renamed by every attach, and the agent can give nothing of its own a
  table's name.

## Not in this version

- **`setup`**, or any call that adds a helper mid-session. Helpers are the integrator's, decided
  where the deployment is, and fixed for the session, like the limits; a call would let what the
  agent can call change under it. A host that wants a derived table attaches it as data.
- **`cancel`** — a query still being planned cannot be interrupted (REQ-8), so a cancel could
  only wait for the hard deadline or kill the engine, and neither is what "cancelled" says. The
  deadline bounds every query instead.

## Lifecycle

- **Start.** The supervisor makes the session directory, starts the engine, seals it empty and
  answers `hello`. Until the first attach the bench has no tables.
- **Rebuild.** Past the hard deadline, past the memory ceiling, or holding a type it cannot drop, the engine is killed. A new one attaches every table from
  `store/` again, in seconds, because the files are already DuckDB's format; no source is read
  again. The call that caused it gets `E_REBUILT`. Result ids continue and never repeat, within
  the session or across sessions.
- **End.** EOF on stdin — closing the pipe, or shutting down the socket's writing side — means no
  more requests: every request already sent is answered, in order,
  and then the session ends. A host that has closed its end of the replies too is not worked for
  further — the first reply that cannot be written ends the session, `hello`'s and `ping`'s
  included (`a_reply_the_reader_cannot_write_ends_the_session`). SIGTERM, SIGINT or SIGHUP end
  it at once: the engine is killed, the call running gets no reply, and the session directory
  goes. If the process is killed outright, the session's warden removes the directory, and the
  engine and every loader notice they have been re-parented and stop (REQ-1).

## What each process holds

- **The engine** starts with an empty environment and no file descriptor but its three, so no
  host variable, no credential and no open file reaches it (`credentials_never_cross`,
  `the_engine_inherits_no_environment_and_no_descriptor`). Landlock lets it read beneath its own
  workspace and delete, and list, beneath the session directory, and — where the kernel has
  Landlock's network rights, Linux 6.7 and later — open no TCP connection and listen on no port.
  A compromised engine could delete `store/`; the next rebuild then fails and says so.
- **A file or Arrow loader** starts with `PATH` and `TMPDIR` alone and no inherited descriptor —
  but, for a streamed attach, the read end of the host's pipe, as its descriptor 3. It reads its
  one input and writes into its own staging directory, nothing more — no TCP either, under the
  same condition — and refuses to run where Landlock cannot confine it unless benchy was started
  with `--allow-unsandboxed`. A streamed attach's loader may read no file at all: its pipe is
  already open, and its rules name its staging directory alone. A loader for a table from the
  stream channel holds a copy of the channel as its descriptor 3, and is confined the same way.
- **The supervisor** holds the stream channel, close-on-exec, and never reads it; the engine and
  the warden hold no copy, so once the supervisor closes it the host's writer is told.
- **The warden** is forked from the server before anything is loaded or configured, zeroes the
  environment it was forked with, and holds nothing but the far end of a pipe only the server
  writes to. When the pipe
  closes — the server has gone, however it went — it removes the session directory, and looks
  again over the next two seconds: a connector still running when its server went can write into
  its scratch until its loader, which notices on its own, stops it. It removes nothing but a
  directory benchy could have made as a session.
- **A connector loader** — how benchy's own server loads an exec source — runs the connector with
  that source's environment only. Credentials exist in the process that resolved them from its
  environment and in that loader, never in the engine.

A host runs `benchy-mcp host` as a user of its choosing, and `attach` reads any file that user can
read. A host that takes paths from somewhere else must check them itself.

Two paths put data on a bench, and they differ in one way: a file source — CSV, TSV, Parquet,
JSON — is read by DuckDB's own readers, nested types included; an Arrow stream, from a host or a
connector, is held to the vocabulary. Either way the table is held in the same types (REQ-15), so
the agent cannot tell which path it came by.

The agent can learn one path: its own session's, through `duckdb_databases()` and
`current_setting('allowed_directories')` (`path_disclosure_names_only_our_own_private_workspace`).
Never one of the host's.
