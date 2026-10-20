# Exec connector protocol

Contract version **`benchy-exec/1`**. A connector is any program that writes an Arrow stream to
stdout. This document is the contract as the host enforces it, and
`benchy-mcp exec-test <config> <source>` checks a connector against it.
[`examples/connectors/hello.sh`](../examples/connectors/hello.sh) is the smallest complete one;
[`meters.py`](../examples/connectors/meters.py) is a realistic one to copy.

## Invocation

```
<command> <subcommand> [operator arguments…]
```

The subcommand is `argv[1]`, before any operator-supplied arguments, so a shell connector can
dispatch on `$1`. No shell is involved: the host executes the resolved path directly.

| Subcommand | stdin | stdout | Called |
|---|---|---|---|
| `fetch` | one JSON document, then EOF | one Arrow IPC stream | on every load |
| `estimate` | one JSON document, then EOF | `{"rows": n, "bytes": n}` | on every load, before `fetch` |
| `describe` | closed | one JSON document | by `exec-test` only |

A connector that does not implement a subcommand exits non-zero with nothing on stdout. For
`estimate` that is fine: the fetch goes ahead, bounded by the limits below — later than an
estimate would have, which is the reason to implement one.

`describe` must be pure — no network, no credentials — and answer within 5 s:

```json
{ "protocol": [1], "name": "hello", "version": "0.1.0",
  "narrowing": { "time": "unsupported", "entity": "unsupported",
                 "columns": "unsupported", "params": "unsupported" } }
```

`name` must match `[a-z0-9][a-z0-9-]{0,62}` and `version` `[A-Za-z0-9.+-]{1,32}`. Each narrowing
level is `unsupported`, `best_effort` or `enforced`.

## The command, and who may write it

`command` comes from the operator's config. A relative path resolves against the config file's
directory — never the working directory, never `PATH`. The host refuses a command that is not a
regular executable file, belongs to a user other than you or root, is world-writable, is writable by a group other
users are in, or sits in a world-writable directory without the sticky bit: a connector holds
credentials, so one that someone else can write is a credential handover. A personal group — the
user's own, with nobody else in it — may write it.
The model can never name or influence `command` or its arguments.

## Environment

Constructed, never inherited:

| Variable | Value |
|---|---|
| `PATH` | the host's |
| `TZ` | `UTC` |
| `LC_ALL` | `C.UTF-8` |
| `TMPDIR` | the per-fetch scratch directory |
| `BENCHY_EXEC_PROTOCOL` | `1` |
| `BENCHY_SOURCE_NAME` | the source's name |
| `BENCHY_RUN_ID` | an opaque id, also in the input document |
| `BENCHY_OP` | the subcommand |

…plus the source's own `env` table from the config, and nothing else. `HOME` is absent unless the
operator lists it: `~/.aws`, `~/.pgpass` and `~/.netrc` are ambient credentials an SDK picks up
silently. `${VAR}` interpolation works only in `env`, and an unresolvable variable stops startup.

Each fetch runs in a loader: a short-lived process of the host's that starts the connector, reads
its stream and exits. It is the only process that holds the source's credentials; the engine that
runs agent SQL never does. The connector gets a fresh `0700` working directory, removed after it
exits; its own process group, so the host can signal it and its children; no blocked signals;
`PR_SET_PDEATHSIG` with `SIGKILL`, so it dies with the loader; and `PR_SET_NO_NEW_PRIVS`. When the
fetch ends — finished, failed or stopped, and however the server or the loader ended — the host
stops the whole group, so leave nothing running you still need. Other processes of the same user
can read the connector's environment through `/proc`, as they can read the config it came from:
benchy does not defend against its own user.

## Input document

```json
{
  "protocol": 1,
  "_benchy_reserved": null,
  "op": "fetch",
  "run": { "id": "4f2a", "attempt": 1 },
  "source": { "name": "readings", "config": { "base_url": "https://example.invalid" } },
  "narrowing": {
    "time":     { "column": "observed_at", "start": "2026-01-01T00:00:00Z", "end": null },
    "entities": { "column": "meter", "ids": ["lab", "office"] },
    "columns":  ["observed_at", "meter", "power_kw"],
    "params":   {}
  },
  "schema": [ { "name": "observed_at", "type": "timestamptz", "nullable": false } ],
  "state": null,
  "limits": { "max_rows": 10000000, "max_bytes": 268435456, "max_batch_bytes": 16777216,
              "deadline_ms": 300000, "cancel_grace_ms": 2000 }
}
```

- `source.config` is copied verbatim from the operator's config. It is trusted and never holds a
  secret: a `${…}` there stops startup.
- **Treat narrowing values as untrusted.** The operator sets them, or a host, never the model:
  what a source fetches is decided by whoever runs benchy. A host may take them from its own
  users, though, so parameterise your queries, and never concatenate a value into SQL, a shell
  command or a URL. The host bounds entity ids before you see them: at most 1024, each at most
  512 bytes, no NUL or control characters.
- `time` is half-open, `[start, end)`, RFC 3339; either bound may be `null`.
- An unknown entity id yields no rows, never an error: an id list outlives the ids in it, and an
  error per stale id would fail the whole load.
- **Ignore keys you do not know.** Every document carries `_benchy_reserved`, so a connector that
  cannot is caught the first time it runs rather than the day a field is added.
- `state` is always `null` and `params` always empty in this version.

## Output: stdout

One Arrow **streaming** IPC stream and nothing else: a schema, zero or more record batches, the
end-of-stream marker.

- **Close the stream.** The end-of-stream marker is mandatory. A stream cut at a message boundary
  parses as a valid, shorter one; only the marker separates finished from crashed. A stream
  without it fails the load.
- **One stream, and the marker ends it.** After the marker, close stdout. A second stream written
  after the first — a writer restarted per page — is not read as more rows: any byte after the
  marker fails the load (`E_HOST_PROTOCOL`). Write every page as a batch of one stream.
- **No rows is a schema, zero batches and the marker** — not empty stdout. Empty stdout with exit
  0 fails the load.
- **Nothing else on stdout.** One stray `print` corrupts the stream; log to stderr.
- **Body compression: LZ4 or none.** An LZ4-compressed body is read — ClickHouse's
  `FORMAT ArrowStream` sends one by default, and it halves what a remote fetch moves. A zstd body
  is refused with `E_HOST_COMPRESSED`; from ClickHouse, add
  `SETTINGS output_format_arrow_compression_method = 'lz4_frame'` (or `'none'`). What a stream
  *decodes* to is bounded, not only what it is (see Limits): each compressed buffer must
  decompress to exactly the size it states. That size is held to what the stream has left before
  the buffer is decompressed, and the decompression stops one byte past it, so a buffer that lies
  costs at most the size it stated.

## Validation

Against the operator's declared schema, never the connector's:

| Check | Rule |
|---|---|
| Fields | set-equal by name, case-sensitive, in any order. A missing or extra column fails the load; an extra one is never silently dropped. |
| Types | the declared type, or one every value of which it holds exactly — see "Types". Never a type that could change a value. |
| Nullability | checked in the data: a column declared `nullable = false` must contain no nulls, whatever the field flag says. |

After ingest the host **re-applies** `time` and `entity` narrowing at every declared level. A row
outside the window — or with no timestamp or entity, which cannot be shown to be inside — is
dropped and counted in the operator log. `columns` and `params` are not re-applied.

## Limits

| Limit | Default | On breach |
|---|---|---|
| rows per fetch | 10,000,000 | the load is refused, never truncated |
| bytes per fetch | 256 MiB | the load is refused once the stream passes it — or once what it decodes to does: a compressed body counts what it decompresses to (a buffer whose stated size is past what is left is refused before it is decompressed), and a dictionary-encoded or view column what it unpacks to, all of it one total |
| one stderr line | 64 KiB | the rest of the line is read and dropped; the log keeps the first 64 KiB |
| wall time per fetch | 300 s | the process group is sent `SIGTERM`, then `SIGKILL` after the grace |
| `describe` or `estimate` | 5 s | the same |
| cancel grace | 2 s | — |

Per source: `max_rows`, `max_bytes`, `deadline_secs`. `max_batch_bytes` is passed as guidance and
not enforced on its own; the byte limit bounds the whole stream.

Across sources, the session has a budget: what every fetch it makes may read together, by
default 50,000,000 rows and 2 GiB. Each fetch's caps are the smaller of its own and what the
budget has left, and what each fetch read counts against it. The operator sets it in the config's
`[budget]` table, `rows` and `bytes`. A fetch that fails still counts what it read: a stream cut
short, refused past a cap or followed by an `error` event cost its source the bytes it sent.

## Control channel: stderr

NDJSON, one object per line with a `type`. A line that is not JSON, or has no `type`, is log text,
so a traceback or a library's logging never breaks the contract.

| `type` | Fields | Effect |
|---|---|---|
| `error` | `code`, `message`, `hint` | fails the load; the first one wins |
| `estimate` | `rows`, `bytes` | checked against the load budget; over it, the fetch is cancelled: `SIGTERM` to the process group, `SIGKILL` after the cancel grace |
| anything else | | operator log only |

An `error` event fails the load even when the connector exits 0 with a complete stream. A
non-zero exit without one is reported as unclassified, with the tail of stderr.

**Whoever stopped first is the verdict.** When benchy refuses a stream before the connector has
closed it — a column the declaration does not have, a cap, a zstd body — it stops reading, and the
connector, still writing, dies of the broken pipe: SIGPIPE (a shell reports 141), curl's exit 23,
or an `error` event about the pipe from a connector that catches it. That is a consequence, not the
cause, so benchy's refusal is reported, and the connector's exit and any event it sent go beside it
in the operator log. When the connector closed its stream first, its own `error` event, if it sent
one, is the verdict, since it knows why it stopped.

`code` is one of `E_CONFIG`, `E_AUTH`, `E_UNAVAILABLE`, `E_RATE_LIMIT`, `E_TIMEOUT`,
`E_NARROWING`, `E_TOO_LARGE`, `E_SCHEMA`, `E_INTERNAL`, `E_CANCELLED`; anything else is treated
as `E_INTERNAL`.

**What the model sees: nothing of yours.** `code`, `message` and `hint` go to the operator log —
a message may carry URLs, account ids and stack traces, and a language model cannot distrust a
string. The agent is told only that the table is not available this session; the rest of the
session starts with what loaded.

## Types

A closed vocabulary. Each token is the Arrow type a connector sends, a set of types it may send
instead — every one of whose values the token holds exactly — and the type the bench holds. The
bench holds every source's data in the same types, files included, so a table says nothing of
where it came from.

| Token | Arrow | May also be sent as | Held as |
|---|---|---|---|
| `bool` | `Boolean` | | `BOOLEAN` |
| `int8` … `int64` | `Int8` … `Int64` | a signed integer of fewer bits, an unsigned one of fewer bits | `BIGINT` |
| `uint8` … `uint32` | `UInt8` … `UInt32` | an unsigned integer of fewer bits | `BIGINT` |
| `uint64` | `UInt64` | an unsigned integer of fewer bits | `HUGEINT` |
| `float32`, `float64` | `Float32`, `Float64` | `Float32` for `float64` | `DOUBLE` |
| `decimal(p,s)` | `Decimal128(p,s)`, `1 ≤ p ≤ 38` | a `Decimal128` with no more digits either side of the point | `DECIMAL(p,s)` |
| `string` | `Utf8` | `LargeUtf8`, `Utf8View`, a dictionary of any of them | `VARCHAR` |
| `binary` | `Binary` | `LargeBinary`, `BinaryView` | `BLOB` |
| `date` | `Date32` | | `DATE` |
| `time` | `Time64(µs)` | | `TIME` |
| `timestamp` | `Timestamp(µs)`, no zone | `Timestamp` at s, ms or ns, no zone | `TIMESTAMP WITH TIME ZONE`, read in the source's `timezone` |
| `timestamptz` | `Timestamp(µs, UTC)` | `Timestamp` at s, ms or ns, any zone | `TIMESTAMP WITH TIME ZONE` |

- **A zone-less timestamp is read as UTC** unless the source sets `timezone` (an IANA name, such
  as `Europe/Copenhagen`) in the operator's config; then it is read as that zone's local time.
  A zoned one is an instant whatever its zone, and neither stands in for the other: a zone-less
  stream where `timestamptz` is declared is refused, and the other way round.
- **Nanoseconds** are accepted, and checked in the data: the bench holds microseconds, so a value
  with sub-microsecond digits is refused, naming the column, rather than rounded.
- **A dictionary** is unpacked into its values, and its unpacked size counts against the byte
  limit as it arrives, added to what the stream's bodies decode to: a few bytes of keys can each
  name a long value. A dictionary may be large
  and sent once; each batch costs only the keys it carries.
- **A view** (`Utf8View`, `BinaryView`) counts the length each view states against the byte limit
  as it arrives: a view is 16 bytes whatever it points at, and many views may point at the same
  bytes.
- From ClickHouse: `DateTime` arrives as `UInt32` seconds, not a timestamp — select
  `toDateTime64(col, 3, 'UTC')` instead; `DateTime64` arrives zoned; `LowCardinality(String)`
  arrives as a plain string unless `output_format_arrow_low_cardinality_as_dictionary` is on.

Nested types are not in this version.

## Reloads

A validated fetch is kept for the life of the session. If the engine is killed and rebuilt, the
new one attaches what was kept rather than calling the connector again.
