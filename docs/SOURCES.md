# Built-in sources

A built-in source is a system benchy reads itself, without a connector: you name it in
`benchy.toml`, and benchy reads its schema, maps it to its own types, asks what the load will
cost, fetches only what the narrowing admits, and validates every row on the way in.

benchy does one job: it gets time-series data into DuckDB, the one sink, as fast and as cheaply as
it can. The built-in sources are the scope of that job:

| source | state |
|---|---|
| ClickHouse | built in (below) |
| HTTP | built in (below): a URL answering with Arrow, Parquet, CSV or NDJSON, or a JSON API in pages |

A folder of files (`benchy-mcp <dir>`, or files named and optionally declared in `benchy.toml`:
Files, below) and the [exec connector](EXEC-CONNECTOR-PROTOCOL.md) stay for everything else; no
other built-in source is planned.

Whatever the source, the agent sees tables: the same rows give the same column types, the same
`describe` and the same answers whether they came from a CSV file or from ClickHouse
(REQ-16 `the_same_rows_from_csv_and_clickhouse_describe_and_answer_identically`). Where a table
came from, and anything ClickHouse said, goes to the operator log only.

## ClickHouse

```toml
[[source]]
type = "clickhouse"
url = "http://localhost:8123"          # ClickHouse's HTTP interface: http, or https (8443)
database = "default"                   # optional, "default"
tables = ["readings", "meters"]        # each becomes a table of the same name
env = { CLICKHOUSE_USER = "${CH_USER}", CLICKHOUSE_PASSWORD = "${CH_PASSWORD}" }
  [source.narrow]
  column = "ts"
  last = "30d"
```

Or one table from a query of your own:

```toml
[[source]]
name = "daily"
type = "clickhouse"
url = "https://ch.example.com:8443"
query = "SELECT meter, toDate(ts) AS day, sum(kw) AS kwh FROM readings GROUP BY ALL"
```

Check a source before you serve it — it reaches the server, signs in, reads and maps the schema,
estimates the load and fetches one row, and names the fix for each failure:

```sh
benchy-mcp check benchy.toml readings
```

### Fields

| field | | |
|---|---|---|
| `type` | required | `"clickhouse"` |
| `url` | required | `http://` or `https://`, a host and port, and a path if a proxy needs one. No user, password or query string: they are refused, since a URL reaches logs |
| `database` | optional | default `"default"` |
| `tables` | one of | each becomes a bench table of the same name, which must be a plain identifier |
| `query` + `name` | one of | one table, named `name`, from your `SELECT`. A trailing `;` is fine. It is your SQL and runs as written, inside a subquery |
| `env` | optional | `CLICKHOUSE_USER` and `CLICKHOUSE_PASSWORD`, and nothing else; `${VAR}` reads benchy's environment |
| `[[source.schema]]` | optional | pin columns (below) |
| `[source.narrow]` | optional | the time window, the entity allowlist and the columns (below) |
| `max_rows`, `max_bytes`, `deadline_secs` | optional | per-source caps, as for an exec source: 10,000,000 rows, 256 MiB of Arrow, 300 s |
| `compression` | optional | `"zstd"`: the HTTP response zstd-compressed; `"lz4"`: Arrow's own LZ4 bodies; `"none"`. Unset: `"none"` for a server on loopback, `"zstd"` for any other. See Transfer |
| `time_column`, `series` | optional | the shape `describe` states instead of guessing (REQ-14) |

Unknown keys, and keys that do nothing on this kind of source, stop startup.

### Types

The schema is read with `DESCRIBE` and every column is converted **in the SELECT**, so what
arrives is already in benchy's types — microseconds, UTC, no dictionaries. Each
kind of value has one type on the bench, whatever the source:

| ClickHouse | on the bench |
|---|---|
| `Int8` … `Int64`, `UInt8` … `UInt32` | BIGINT |
| `UInt64` | HUGEINT, as from every source — half its range does not fit BIGINT, and a wrapped id is a wrong answer |
| `Float32`, `Float64`, `BFloat16` | DOUBLE. `Float32` is widened exactly: `0.1` stored as a Float32 reads `0.10000000149011612` |
| `Decimal(P, S)` with P ≤ 38 | DECIMAL(P,S) |
| `Bool` | BOOLEAN |
| `Date`, `Date32` | DATE |
| `DateTime`, `DateTime64(p)`, with or without a zone | TIMESTAMP WITH TIME ZONE, in microseconds, the same instant. `DateTime64(9)` loses its nanoseconds |
| `String` | VARCHAR; bytes that are not UTF-8 become U+FFFD |
| `FixedString(N)` | VARCHAR, without its NUL padding |
| `Enum8`, `Enum16` | VARCHAR, the name |
| `UUID`, `IPv4`, `IPv6`, `Time`, `Time64` | VARCHAR, as ClickHouse writes them |
| `Int128`, `UInt128`, `Int256`, `UInt256`, `Decimal` with P > 38 | VARCHAR, the exact decimal text |
| `Array`, `Map`, `Tuple`, `Nested`, `JSON`, `Object`, `Variant`, `Dynamic`, the geo types | VARCHAR, the value as JSON text; DuckDB's `->>` and `json_extract` read it |
| `Nullable(T)`, `LowCardinality(T)`, `SimpleAggregateFunction(f, T)` | as `T`, nullable for `Nullable` |
| `AggregateFunction`, `Interval*`, `Nothing`, and anything else | not loaded. The operator log and `check` say which column and why; finalize an aggregate state in a `query` with `finalizeAggregation()` or a `-Merge` combinator |

The agent sees the bench's types only; no ClickHouse type name reaches `describe`.

A column whose name the agent could not type (a control character in it), or two that differ only
in case, refuse the load: give them other names with `AS` in a `query`.

**Pinning.** `[[source.schema]]` entries, as for an exec source, pin columns: only the pinned
columns are loaded, each must exist, and each must load as the type pinned — `kw` pinned as
`int64` when ClickHouse's `Float32` loads as `float64` refuses the load rather than converting.
`nullable = false` is checked in the rows. Without pins, every column that can be loaded is.

### Pushdown

benchy asks ClickHouse for no more than it will keep:

- **Time window** — `[source.narrow] column` with `last`, or `start` and/or `end` (RFC 3339 with
  an offset, half-open). Sent as `column >= fromUnixTimestamp64Micro({benchy_start:Int64}, 'UTC')`
  on the raw column, so ClickHouse can use its primary key. A `Date` column is compared as the
  instant its day starts in UTC.
- **Entities** — `entity_column` and `entity_ids`, sent as `column IN {benchy_ids:Array(String)}`
  (through `toString` for a column that is not text).
- **Columns** — `narrow.columns`, or the pinned columns: only those are selected.

Every value travels as a ClickHouse query parameter (`param_benchy_ids=…` on the URL), never in
the SQL text, and every identifier is backquoted with its escapes
(`values_travel_as_query_parameters_never_in_the_sql`). benchy re-applies the window and the
allowlist after ingest, as it does for every source.

Fetch narrowing is the operator's (or a host's) decision, never the model's: the agent cannot
widen or change it.

### Admission

Before the fetch, benchy asks ClickHouse for `count()` under the same narrowing, and measures the
text columns on a sample of at most 10,000 rows, to estimate the Arrow bytes. A load over the
source's `max_rows` or `max_bytes`, or over what the session's `[budget]` has left, is refused
then, before a row is sent (`an_over_budget_clickhouse_load_is_refused_before_transfer`). The
fetch is held to the same caps as it streams, so an estimate that was low costs bytes, never a
truncated table. A fetch that fails counts what it read against the budget all the same.

### Transfer

`FORMAT ArrowStream` over HTTP, read as it arrives into the validating Arrow reader an exec
connector's stdout goes through — no temporary file, no buffered body. How the rows travel is
`compression`:

| `compression` | 840,960 meter rows | 10,000,000 rows of `bench/gen_data.py` | |
|---|---:|---:|---|
| `"zstd"` | 1.5 MB | 41.5 MB | the HTTP response, `Accept-Encoding: zstd`; the most CPU at both ends |
| `"lz4"` | 10.8 MB | 123.1 MB | Arrow's LZ4 frame per body, which the reader decodes under its bound |
| `"none"` | 21.9 MB | 240.0 MB | the least CPU at both ends |

Left unset, `compression` follows where the server is
(`an_unset_compression_follows_where_the_server_is`): `"none"` for one on loopback —
`localhost`, 127.0.0.0/8 or ::1, as the URL writes it; a name is not resolved — and `"zstd"` for
any other. On one machine both ends of the compression are its CPU: for 10,000,000 rows, zstd
cost ClickHouse 2.3 s of CPU to send against 0.65 s for none, and benchy 0.3 s to decode, so a
load with zstd took 7.7 s of CPU in all where none took 6.0 s (docs/PERFORMANCE.md, item 3).
Anywhere else zstd moves a sixth of the bytes, which on 1 Gbit/s is 0.3 s against 2 s, and the
server's CPU is not this machine's. A server on the same machine addressed by another name, or
on a fast private network, may do better with `compression = "none"` set.

The caps and the budget count the decoded Arrow bytes, so compression changes what crosses the
network and nothing about what may land on the bench
(`a_compressed_response_is_decoded_and_bounded`). A zstd body is decoded by C libzstd
(`src/decompress.rs`) as the reader pulls it, with its window held to 32 MiB, so a body that
decodes past the cap is refused at the cap, not decoded whole
(`a_bomb_is_decoded_only_as_far_as_it_is_read`). The response is
held to everything a connector's stream is — view columns counted by what they unpack to, nothing
after the end-of-stream marker — since a server is trusted no more than a connector
(`a_clickhouse_response_is_held_to_what_a_connectors_stream_is`).
[CAPACITY.md](CAPACITY.md) and [PERFORMANCE.md](PERFORMANCE.md) have the measurements.

### Read-only users

A user with `readonly = 1` — the usual analytics account — may not change any setting, so it is
sent none (`a_read_only_clickhouse_user_loads_what_any_user_loads`). benchy finds out by being
refused them once per table, asks ClickHouse that user's own settings, and sends every later
request without them. The rows are the same: every column is converted in the SELECT, so no
setting is needed to get benchy's types. What changes:

- **The Arrow output is ClickHouse's default.** ClickHouse 26.8 compresses Arrow bodies with LZ4
  by default and writes text as Arrow strings, which the reader takes, so `compression = "lz4"`
  and `"none"` both receive LZ4 bodies. `"zstd"` still asks for a zstd response, which ClickHouse
  sends when the user's profile has `enable_http_compression` on, as 26.8's default does;
  otherwise the response arrives uncompressed and is read as it is.
- **ClickHouse is not told the deadline.** `max_execution_time` is a setting too. benchy still
  stops waiting at `deadline_secs`; the query on the server is bounded by the user's own profile.
- **Two profile settings can refuse the load.** If the user's profile sets
  `output_format_arrow_compression_method` to anything but `lz4_frame` or `none` (zstd Arrow
  bodies are not decoded), or `output_format_arrow_string_as_string = 0` (text as binary) while a
  text column is loaded, the load is refused before a row is sent, naming the setting: set it in the profile, or give the user `readonly = 2`, which is
  still read-only and allows settings.

`check` says which way a source will load: a read-only user's "signed in" line names its mode
and the settings ClickHouse will use.

### Credentials

`CLICKHOUSE_USER` and `CLICKHOUSE_PASSWORD` come from the source's `env` — `${VAR}` interpolation
works there and nowhere else — and travel in the `X-ClickHouse-User` and `X-ClickHouse-Key`
headers. The fetch runs in a loader process of its own, which holds this source's `env` and no
other; the engine never holds a credential (`clickhouse_credentials_stay_in_the_loader`). benchy
connects to the URL as written: no proxy from the environment, and no redirect, which could carry
the headers elsewhere. https uses rustls with the Mozilla root certificates built in.

That loader is confined on Linux (REQ-3 `a_network_loader_reaches_only_its_source_and_its_staging`):
before its first request it may write only its staging directory, read only the resolver's files
(`/etc/resolv.conf`, `/etc/hosts`, `/etc/nsswitch.conf`, `/etc/host.conf`, `/etc/gai.conf`) and,
from Linux 6.7, connect only to the URL's port, so a parser subverted by what the server sends
reaches nothing else. A `hosts:` line in `/etc/nsswitch.conf` that needs an NSS module library
(`mdns4_minimal`, `resolve`) cannot load it there; `files` and `dns` are built into glibc.
`benchy-mcp doctor --probe` measures that restriction on your machine. `benchy-mcp check` runs
in your shell, unconfined.

### Errors

| what happened | code | |
|---|---|---|
| wrong user or password, a user not allowed from this host, no `SELECT` grant | `E_AUTH` | |
| nothing listening, DNS, TLS, the server out of memory or too busy | `E_UNAVAILABLE` | |
| an unknown database, table or column, a syntax error in `query`, a read-only user whose profile makes Arrow output benchy does not read | `E_CONFIG` | |
| past `deadline_secs`, on either side | `E_TIMEOUT` / `E_HOST_TIMEOUT` | ClickHouse is told the deadline too, as `max_execution_time`, unless the user is read-only |
| over a cap or the budget | `E_TOO_LARGE` | before the fetch when the estimate shows it |
| a pinned column missing or of another type | `E_SCHEMA` | |

The operator log gets ClickHouse's whole message, with its error code
(`clickhouse_failures_map_to_fault_codes`) — also for an exception ClickHouse writes into the body
after the stream has started, which fails the load rather than leaving the rows before it. The session starts with the tables that loaded,
and `describe` lists one that did not as not loaded, by its name alone: the agent is told
nothing about ClickHouse (REQ-15, `a_clickhouse_error_reaches_the_operator_not_the_agent`).

## HTTP

A URL that answers with one body — Arrow IPC, Parquet, CSV or NDJSON, gzip- or zstd-compressed
or not — or a JSON API that answers in pages, the rows at a path in each (REQ-17).
[HTTP-SOURCE.md](HTTP-SOURCE.md) is the design, with the measurements behind each choice.

```toml
[[source]]
name = "readings"
type = "http"
url = "https://data.example.com/readings.csv.gz"
format = "csv"                        # "arrow" | "parquet" | "csv" | "ndjson" | "json"
compression = "gzip"                  # the body is a gzip file whatever Content-Encoding says
timezone = "Europe/Copenhagen"        # zone-less timestamps in the body, as for a file
```

```toml
[[source]]
name = "readings"
type = "http"
url = "https://api.example.com/v1/readings"
format = "json"
params = { site = "north" }           # literal query parameters
env = { KEY = "${API_KEY}" }
auth = { header = "X-Api-Key", env = "KEY" }
  [source.pages]                      # {"data": [...], "meta": {"next_cursor": "abc"}}
  rows = "data"
  next = "cursor"                     # "cursor" | "offset" | "page" | "link" | "url"
  cursor = "meta.next_cursor"
  cursor_param = "cursor"
```

### Fields

| field | | |
|---|---|---|
| `url` | required | `http://` or `https://`. No user, password, query string or fragment: a URL reaches logs. Literal parameters go in `params`, a secret one in `auth` |
| `format` | required | `"arrow"`, `"parquet"`, `"csv"`, `"ndjson"`, `"json"` |
| `method`, `body` | optional | `"GET"` (default) or `"POST"`; `body` only with POST, sent as written |
| `headers` | optional | literal headers. `Authorization`, `Proxy-Authorization` and `Cookie` are refused (use `auth`); `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Accept-Encoding`, `TE`, `Upgrade` are benchy's |
| `params` | optional | literal query parameters |
| `env`, `auth` | optional | the credential: below |
| `compression` | optional | `"auto"` (default: as `Content-Encoding` says, gzip and zstd asked for), `"gzip"`, `"zstd"`, `"none"` |
| `delimiter` | `csv` only | one character, `","` unless set. A declared CSV is read with it and RFC 4180's `"` as quote and escape, not sniffed; an undeclared one is sniffed with it stated |
| `[source.pages]` | `json` only | `rows` (a dotted path; `""` when the page is the array) and `next`, with the keys of its style; `max_pages` (10,000) |
| `[source.narrow]` | optional | the window, the entity allowlist and the columns, re-applied after ingest whatever the API did |
| `[source.pushdown]` | optional | the query parameters the narrowing travels in: `start`, `end`, `time_format` (`"rfc3339"`, `"unix"`, `"unix_ms"`, `"date"`), `entities`, `columns` |
| `[[source.schema]]` | optional | declare or pin columns: see Schema |
| `max_rows`, `max_bytes`, `deadline_secs` | optional | 10,000,000 rows, 256 MiB decoded, 300 s, for the whole source |
| `retries` | optional | per request, default 3, at most 10 |
| `rate_limit` | optional | `{ requests = 10, per_secs = 1 }`: request starts spaced to it |
| `redirect_hosts` | optional | hosts a redirect may leave the origin for, without the credential: a name, with `:port` when it is not the scheme's (443, or 80 for http) |
| `timezone`, `time_column`, `series` | optional | as for a file |

The paging styles: `cursor` (`cursor`, the path to it, and `cursor_param`; a missing, null or empty
cursor ends), `offset` (`offset_param`, `limit_param`, `page_size`; advances by the rows received
and ends at an empty page), `page` (`page_param`, `first_page`, optionally `size_param` with
`page_size`; ends at an empty page), `link` (RFC 8288 `rel="next"`), `url` (`url`, the path to the
next page's URL in the body). A cursor or URL sent twice, a page the same as the one before, a
next URL on another origin and more than `max_pages` pages each fail the source; none ends it
early.

### Credentials

The secret comes from benchy's environment through `env`, named by `auth`, and nowhere else:
`{ bearer = "TOKEN" }`, `{ header = "X-Api-Key", env = "KEY" }`, `{ param = "api_key", env =
"KEY" }` or `{ user = "USER", password = "PASS" }` (Basic). It is held by the loader that fetches
this source, never the engine. Every log line and refusal is built from the URL's origin and
path, and has every secret — as written, percent-encoded, and Basic's base64 — replaced by `***`,
an API's error body included.

### Formats and schema

| format | how it loads | schema |
|---|---|---|
| `arrow` | streamed into the reader an exec connector's stdout goes through | the stream's own, held to the vocabulary; or the declaration |
| `parquet` | staged in the loader's staging directory, its footer's row count checked, then DuckDB's `read_parquet` | the file's; a declaration pins columns |
| `csv`, `ndjson` | spooled to a file in the loader's staging directory (at most `max_bytes`, removed once read), then read by DuckDB's own reader as the same file is | DuckDB's sniff, as for the file; or the declaration, by column name |
| `json` | each page read whole and decoded by arrow-json into a batch, on threads of their own while the next pages are fetched, the batches taken in page order | DuckDB's sniff of the first page, pinned for the rest; or the declaration |

A declared or pinned column must be there and load as its type; only declared columns load. A
declared CSV is read in the dialect stated — `delimiter`, `"` as quote and escape, a header row
— which skips most of DuckDB's sniffing; a body in another dialect fails the load, naming
`delimiter` when the header does not split into the declared columns. A
JSON page whose value does not fit the first page's type (`2.5` where the first held integers)
fails the source, naming the column: declare the wider type. `benchy-mcp check` prints the
schema a load read as `[[source.schema]]` entries to pin.

### Requests, redirects, errors

One deadline, `deadline_secs`, bounds the whole source. A request is retried, up to `retries`,
after a connection failure, a timeout, 408, 429, 500, 502, 503 or 504, waiting `Retry-After` or a
jittered backoff from 250 ms, never past the deadline; a bulk body that breaks off after its
first byte is not. A 400 to a request carrying pushdown parameters fails at once, naming them.
Redirects are followed on the source's origin with the credential, to a `redirect_hosts` host
and port without it, never from https to http, and nowhere else. The loader may connect only to
the URL's port and the redirect hosts' (REQ-3).

| what happened | code |
|---|---|
| 401, 403 | `E_AUTH` |
| connect, DNS, TLS, 5xx, 408 and 429 past their retries | `E_UNAVAILABLE` |
| another 4xx, a redirect off the origin, a body that is not its format | `E_CONFIG` |
| past `deadline_secs` | `E_TIMEOUT` |
| a cap, `max_pages`, the budget, a Parquet footer over `max_rows` | `E_TOO_LARGE` |
| a column missing or of another type | `E_SCHEMA` |
| a loop; a body cut short | `E_HOST_PROTOCOL`; `E_HOST_TRUNCATED` |

No failure leaves part of a table. What a failed fetch read is spent on the session's budget.

## Files

A folder, `benchy-mcp <dir>`, makes a table of every `.csv`, `.tsv`, `.parquet`, `.json`,
`.ndjson` and `.jsonl` file in it, each read by DuckDB's own reader and sniffed: no config, no
schema. A file named in `benchy.toml` may also be declared, as the operator's choice:

```toml
[[source]]
name = "readings"
type = "file"
path = "./data/readings.csv"
delimiter = ";"                       # a .csv's or .tsv's; "," or a tab unless set
timezone = "Europe/Copenhagen"        # zone-less timestamps, as for any file
  [[source.schema]]
  name = "ts"
  type = "timestamp"
  [[source.schema]]
  name = "meter"
  type = "string"
```

| field | | |
|---|---|---|
| `path` | required | relative to the config file's directory |
| `timezone`, `time_column`, `series` | optional | the zone of zone-less timestamps; the shape `describe` states (REQ-14) |
| `[[source.schema]]` | optional | declare the columns, in the exec vocabulary: only they load, each must be there and load as its type, a `nullable = false` one must hold no null |
| `delimiter` | `.csv`, `.tsv` only | one character, stated to the reader, declared or not |

Declared, a file is read as an HTTP body of its format is (R3-5): a CSV or TSV in its stated
dialect — `delimiter`, `"` as quote and escape, a header row — with the declared types, its
columns matched by the header's names in any order, so the sniffer reads only the header; an
NDJSON or JSON file with the declared `columns`, not sniffed at all; a Parquet file with its own
types, which the declaration pins. A file in another shape fails the load naming why — a header
not split into the declared columns names `delimiter`, a missing column or a value not of its
type names the column — and is never loaded as nulls or as another column's values
(`a_declared_file_that_does_not_match_fails_naming_why`). Declared as its sniffer would type it,
a file makes the table the sniffed read makes
(`a_declared_file_loads_as_the_sniffed_file_does`); a 1,000-row CSV loads with 42% fewer
instructions, a 1,000,000-row NDJSON file with 19% fewer (docs/PERFORMANCE.md, R3-5). DuckDB's
JSON sniffer types a zone-less time and one ending in `Z` as `timestamp`; declared
`timestamptz`, an NDJSON column is cast by ICU, whose start costs about 130 M instructions.

Unknown keys, and keys that do nothing on a file source, stop startup.

## Adding a built-in source

ClickHouse and HTTP are the scope; no other is planned. A
source is a module in `src/sources/` that implements `Fetch` — plan (read and map the schema),
estimate, fetch (an Arrow stream into `exec::read_arrow`) — plus a variant of `BuiltIn` and its
config keys in `src/config.rs`. The load around it, admission and the narrowing re-applied after
ingest are shared (`sources::load_within`). HTTP is the exception: it has no count, and DuckDB
reads three of its formats itself, so it has a load of its own (`src/sources/http/load.rs`),
ending in the same table writers (`source::Ingest`).
