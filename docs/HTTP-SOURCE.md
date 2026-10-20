# HTTP as a built-in source — design

**Status: built** (REQ-17), as designed here, with the owner's decisions below. The code is
`src/sources/http/`: `mod.rs` checks the config, `client.rs` makes the requests, `paginate.rs`
says where the next page is, and `load.rs` puts each format on the bench. "As built" at the end
says where the build departs from this design, and what it measured.

benchy's one job here is ingestion speed: a URL's rows on the bench in as few instructions and
copies as possible, into DuckDB, with every guarantee the other sources keep. Two shapes of
source are covered:

- **Bulk:** one request (GET, or POST with a body) answers with an Arrow IPC stream, a Parquet
  file, CSV or NDJSON, possibly gzip- or zstd-compressed.
- **Paginated JSON:** a REST API answers with JSON pages, the rows at a path in each, paged by
  cursor, offset/limit, page number, `Link` header or a next URL in the body.

What does not change: the agent sees tables, never sources (REQ-15). The same data makes the
same table, types, `describe` and answers as a file of it would; the URL, the host, the API's
messages and why a load failed go to the operator log. The narrowing (time window, entities,
columns) is the operator's or the host's, never the model's. The engine never holds a
credential (REQ-5).

## Config

Every key below is parsed and checked today. Unknown keys, keys of another kind of source, and
keys of another paging style stop startup, as for every source.

### Bulk formats

```toml
[[source]]
name = "readings"
type = "http"
url = "https://data.example.com/export/readings.arrows"
format = "arrow"                      # "arrow" | "parquet" | "csv" | "ndjson" | "json"
```

```toml
[[source]]
name = "readings"
type = "http"
url = "https://data.example.com/readings.parquet"
format = "parquet"
max_rows = 50_000_000                 # per-source caps, as for every fetched source
max_bytes = 2_147_483_648
deadline_secs = 600
```

```toml
[[source]]
name = "readings"
type = "http"
url = "https://data.example.com/readings.csv.gz"
format = "csv"
compression = "gzip"                  # the body is a gzip file whatever Content-Encoding says
timezone = "Europe/Copenhagen"        # zone-less timestamps in the body, as for a file
```

```toml
[[source]]
name = "readings"
type = "http"
url = "https://data.example.com/query"
format = "ndjson"
method = "POST"
body = '{"site": "north", "fields": ["ts", "meter", "kw"]}'
headers = { Accept = "application/x-ndjson" }
env = { TOKEN = "${EXPORT_TOKEN}" }
auth = { bearer = "TOKEN" }
```

### Paginated JSON

`[source.pages]` says where the rows are (`rows`, a dotted path; `""` when the page is the array)
and how to ask for the next page (`next`). Without `next` there is one request.

```toml
# Cursor: {"data": [...], "meta": {"next_cursor": "abc"}} — the cursor goes back as ?cursor=abc.
[[source]]
name = "readings"
type = "http"
url = "https://api.example.com/v1/readings"
format = "json"
params = { site = "north" }           # literal query parameters
env = { KEY = "${API_KEY}" }
auth = { header = "X-Api-Key", env = "KEY" }
  [source.pages]
  rows = "data"
  next = "cursor"
  cursor = "meta.next_cursor"         # a missing, null or "" cursor ends the source
  cursor_param = "cursor"
```

```toml
# Offset and limit: [...] — ?offset=0&limit=1000, then ?offset=<rows so far>.
  [source.pages]
  rows = ""
  next = "offset"
  offset_param = "offset"
  limit_param = "limit"
  page_size = 1000
```

```toml
# Page number: {"items": [...]} — ?page=1&per_page=100, ?page=2, ...
  [source.pages]
  rows = "items"
  next = "page"
  page_param = "page"
  first_page = 1                      # default 1
  size_param = "per_page"             # optional, with page_size
  page_size = 100
```

```toml
# RFC 8288 Link header: Link: <https://api.example.com/v1/readings?page=2>; rel="next"
  [source.pages]
  rows = "items"
  next = "link"
  max_pages = 500                     # default 10,000
```

```toml
# Next URL in the body: {"results": [...], "links": {"next": "/v1/readings?after=42"}}
  [source.pages]
  rows = "results"
  next = "url"
  url = "links.next"
```

### Fields

| field | | |
|---|---|---|
| `type` | required | `"http"` |
| `name` | required | the table |
| `url` | required | `http://` or `https://`. No user, password, query string or fragment: a URL reaches logs, and a query string is where tokens hide. Literal parameters go in `params`, a secret one in `auth` |
| `format` | required | `"arrow"`, `"parquet"`, `"csv"`, `"ndjson"`, `"json"` |
| `method`, `body` | optional | `"GET"` (default) or `"POST"`; `body` only with POST, sent as written, no `${…}` |
| `headers` | optional | literal headers. `Authorization`, `Proxy-Authorization` and `Cookie` are refused (they carry credentials: use `auth`); `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Accept-Encoding`, `TE`, `Upgrade` are benchy's |
| `params` | optional | literal query parameters. One benchy sets (paging, pushdown, `auth`) is refused |
| `env`, `auth` | optional | the credential: below |
| `compression` | optional | `"auto"` (default: as `Content-Encoding` says), `"gzip"`, `"zstd"`, `"none"` |
| `delimiter` | `csv` only | one character, `","` unless set |
| `[source.pages]` | `json` only | where the rows are and how to page; required for `json`, refused for every other format |
| `[source.narrow]` | optional | the time window, entity allowlist and columns, as for every fetched source, re-applied after ingest |
| `[source.pushdown]` | optional | the query parameters the narrowing travels in: below |
| `[[source.schema]]` | optional | declare or pin columns: see Schema |
| `max_rows`, `max_bytes`, `deadline_secs` | optional | 10,000,000 rows, 256 MiB, 300 s, for the whole source: every page, retry and wait |
| `retries` | optional | per request, default 3, at most 10 |
| `rate_limit` | optional | `{ requests = 10, per_secs = 1 }`: requests are spaced to it |
| `redirect_hosts` | optional | hosts a redirect may leave the origin for, without the credential |
| `timezone`, `time_column`, `series` | optional | as for a file |

## Credentials

A secret has one way in: benchy's environment, through `env` (the only place `${VAR}` is
interpolated, and an unresolved one aborts startup), named by `auth`:

| `auth` | sends |
|---|---|
| `{ bearer = "TOKEN" }` | `Authorization: Bearer <env TOKEN>` |
| `{ header = "X-Api-Key", env = "KEY" }` | `X-Api-Key: <env KEY>` |
| `{ param = "api_key", env = "KEY" }` | `?api_key=<env KEY>`, for APIs that take nothing else |
| `{ user = "USER", password = "PASS" }` | HTTP Basic |

Checked at startup (`an_http_secret_comes_only_from_env_through_auth`): an `env` key `auth` does
not read is refused as handed to nothing, an `auth` key `env` does not set is refused, `${…}`
anywhere else is refused, and no refusal repeats a value.

At run time the secret lives in the loader that fetches this source and nowhere else, as
ClickHouse's does (`clickhouse_credentials_stay_in_the_loader`): the server resolves it and hands
it to that loader's job; the engine never receives it. **Redaction:** every URL the operator log
prints is printed with the `auth` parameter's value replaced by `***`, and a log line is built from
the origin and path, never from request headers. An API's error body goes to the operator log
(truncated to 2,000 characters, as ClickHouse's is) after every occurrence of each `auth` value has
been replaced, since some APIs echo the key back in their errors.

## Requests

### Pushdown

The narrowing is the operator's, and so is how it is spelled on the API:

```toml
  [source.narrow]
  column = "ts"
  start = "2026-01-01T00:00:00Z"
  end = "2026-02-01T00:00:00Z"
  entity_column = "meter"
  entity_ids = ["MT_002", "MT_004"]
  columns = ["ts", "meter", "kw"]
  [source.pushdown]
  start = "from"                      # ?from=<window start>
  end = "to"                          # ?to=<window end>
  time_format = "rfc3339"             # "rfc3339" (default) | "unix" | "unix_ms" | "date"
  entities = "meter"                  # ?meter=MT_002,MT_004
  columns = "fields"                  # ?fields=ts,meter,kw (or the pinned schema's columns)
```

A pushdown parameter without the narrowing it carries is refused at startup; so is one that
collides with a paging or `auth` parameter. The values are percent-encoded on the URL; nothing is
spliced into a body. Whatever the API does with them, the narrowing is **re-applied after ingest**,
as for every source: an API that ignores `from` costs bytes, never correctness.

### Timeouts, retries, rate limits

- **One deadline:** `deadline_secs` bounds the whole source — every page, retry and backoff.
  Each request's ureq timeout is what is left of it; connecting is held to 10 s within that.
- **Retries:** at most `retries` per request, only before any byte of that response has gone into
  the table. Retried: connection failures, timeouts, 408, 429, 500, 502, 503, 504. Not retried:
  every other status (401 and 403 are `E_AUTH`, other 4xx `E_CONFIG`). The wait is
  `Retry-After` when the response has one (seconds or an HTTP date), else full-jitter exponential
  backoff from 250 ms, capped at 10 s; a wait that would pass the deadline fails now with
  `E_TIMEOUT` instead of sleeping into it. POST is retried too: the operator's body is a query,
  and an API whose POST is not idempotent is not a data source.
- **A bulk body that breaks mid-stream is not retried** (`E_HOST_TRUNCATED`): its rows are already
  in the table's uncommitted transaction, and resuming with `Range` is a later, measured change. A JSON page is
  read whole before it is decoded, so a page that breaks is retried like a failed request.
- **Rate limit:** `rate_limit = { requests, per_secs }` spaces request starts by
  `per_secs / requests`; a 429 slows down further by its `Retry-After`.

### Redirects

benchy follows redirects itself, with ureq's own turned off (`max_redirects(0)`, as for
ClickHouse): ureq strips only `Authorization` on a cross-host redirect, and would carry an
`X-Api-Key` or an `auth` parameter to wherever a redirect pointed.

- To the same origin (scheme, host, port): followed with the credential, at most 5 times.
- To a host and port in `redirect_hosts` (an entry is a name, with `:port` when the port is not
  the scheme's), over https, or over http when the source itself is http — never from https down
  to http: followed **without** the `auth` header, the `auth` parameter, or any query parameter
  holding a secret's value, which an API may echo into the URL it redirects to — the shape of a
  presigned object-store URL behind an API.
- Anywhere else: refused, naming the origin only (`E_CONFIG`).

The same origin rule holds for a next page's URL (`Link`, or `next = "url"`):
`a_next_url_on_another_origin_is_refused`.

### TLS and the client

ureq 3 with rustls and ring, already a dependency for ClickHouse: no OpenSSL, the Mozilla roots
built in, no proxy from the environment (the loader's environment is constructed and empty). HTTP
compression is asked for with `Accept-Encoding: zstd, gzip` under `compression = "auto"`, and
decoded by benchy, not ureq, so the decode is bounded (below), in `src/decompress.rs`, which
ClickHouse's source uses too: `Encoding::from_header` reads `Content-Encoding`, and
`decoder(encoding, body)` decodes as the reader pulls. zstd is C libzstd there (the `zstd` crate);
gzip is `flate2` on the zlib-rs backend beside it, measured at 0.53 s of CPU for a
72 MB gzip body where miniz_oxide took 0.66 s (docs/history/PERFORMANCE-rounds.md, round 1 item 3).

## The fastest path for each format

Measured on the eval's public meter data (840,960 rows of `ts, meter, kw`) on a shared 4-core
ARM64 machine (Linux 6.8), DuckDB 1.5.5 at `threads = 1` so every path runs on one core, with
`perf stat -e instructions:u`, the mean of three runs. The probe is
`spike/src/probe7_http_decode.rs`; each mode reads its input from a file already in the page
cache, so what is measured is decoding, not the network.

Inputs: CSV 36.1 MB, NDJSON 56.3 MB, the same rows as 85 JSON pages of 10,000 (56.3 MB),
Parquet 1.7 MB, Arrow IPC 22.3 MB. "Into DuckDB" ends with the rows in an in-memory DuckDB table;
"to Arrow" ends with Arrow batches in memory. Reading the 56 MB file alone is 0.02 G
instructions, so staging a body to a file or piping it costs next to nothing.

| format | path | to Arrow | into DuckDB | CPU into DuckDB |
|---|---|---:|---:|---:|
| Arrow IPC | arrow-rs `StreamReader` (the `read_arrow` path), then DuckDB's Arrow appender | 0.06 G | 1.56 G | 0.61 s |
| Parquet | DuckDB `read_parquet` on the staged body | — | **0.60 G** | 0.17 s |
| CSV | DuckDB `read_csv`, columns declared, from a file | — | 2.83 G | 0.53 s |
| CSV | the same, from a pipe | — | **2.87 G** | 0.53 s |
| NDJSON | DuckDB `read_ndjson`, columns declared, from a file | — | 2.35 G | 0.52 s |
| NDJSON | the same, from a pipe | — | **2.40 G** | 0.56 s |
| NDJSON | the same, schema sniffed by DuckDB, from a pipe | — | 3.31 G | 0.73 s |
| NDJSON | arrow-json `Reader` with the declared schema | 2.31 G | 3.40 G | 0.64 s |
| NDJSON | serde_json `Value` per line into builders | 4.31 G | 5.40 G | 0.89 s |
| NDJSON | simd-json borrowed value per line into builders | 4.68 G | 5.83 G | 0.95 s |
| JSON pages | arrow-json, each page decoded as `{data: [struct]}`, the list's child taken as the batch | **2.67 G** | **3.86 G** | 0.75 s |
| JSON pages | simd-json per page into builders | 2.81 G | 3.97 G | 1.06 s |
| JSON pages | serde_json `Value` per page into builders | 5.50 G | 6.80 G | 1.72 s |
| JSON pages | DuckDB `read_ndjson` of the pages, `unnest(data)`, declared | — | 17.29 G | 2.91 s |
| JSON pages | the same, sniffed | — | 13.00 G | 2.18 s |

What decided each format:

- **DuckDB's own readers beat decoding in Rust wherever they apply.** For NDJSON, `read_ndjson`
  decodes *and* inserts in 2.4 G instructions; arrow-json needs 2.3 G to decode alone and 3.4 G
  with the insert. The Arrow-to-DuckDB step costs about 1.1–1.5 G instructions per 840,960 rows
  (the Arrow IPC row: 0.06 G to decode, 1.56 G with the insert). That step is what F2's ingest
  rewrite is working on; if it gets much cheaper, the NDJSON row should be measured again.
- **DuckDB reads CSV and NDJSON from a pipe at the same cost as from a file** (2.87 G against
  2.83 G; 2.40 G against 2.35 G), sniffing included, in instructions; but on one thread, where
  a file is parsed on every one. So a CSV or NDJSON body was first written into a FIFO as it
  arrived, and is now spooled to a file in the staging directory and read as that file is (R3-3,
  "As built" below).
- **Parquet cannot stream:** its metadata is at the end, and DuckDB refuses a FIFO ("cannot be
  efficiently supported"). The body is written to the staging directory, then read — 0.6 G
  instructions for 840,960 rows, the cheapest format by far. A Rust Parquet decoder would bring
  the `parquet` crate and its codecs for nothing.
- **JSON pages: arrow-json.** DuckDB's JSON reader is fast on rows, and 5x slower on the same rows
  nested in pages (`unnest` of a list of structs, 13–17 G). arrow-json decodes a page whose
  schema is `{data: list<struct<declared columns>>}` in one pass, ignoring every other key, and
  the list's child array *is* the batch: no copy. serde_json is twice the instructions; simd-json
  is close on pages and worse on lines, and would be a new dependency, so it is not worth one.
  Measured again on DuckDB 1.5.5 (R4-3), its reader on the spooled pages takes 2.48 G, now less
  than arrow-json and the insert together, but only once every page is fetched and spooled:
  later to the first `describe` than decoding pages as they arrive, so arrow-json stays.
- **Arrow: the existing `read_arrow` path**, unchanged: schema validation, the byte bound counted
  on what views and dictionaries unpack to, the end-of-stream marker.

Wall times on this shared machine moved by up to 2x between runs; instructions moved by under
0.1%, which is why they are the number this design is decided on.

### What each format does

| format | body goes | decoded by | schema |
|---|---|---|---|
| `arrow` | streamed | `exec::read_arrow`, as ClickHouse's stream | the stream's, validated against a declaration when there is one |
| `parquet` | to `stage/http-body.parquet`, bounded by `max_bytes` as it is written | DuckDB `read_parquet`, after the footer's row count is checked | the file's |
| `csv` | spooled to `stage/http-body.csv`, bounded by `max_bytes` as it is written, removed once read | DuckDB `read_csv_auto`, the file's reader, sniffing as for a file; declared, its dialect stated (`delimiter`, RFC 4180's quote and escape) | DuckDB's sniff, or the declaration as `types`, by name |
| `ndjson` | spooled to `stage/http-body.ndjson`, the same | DuckDB `read_json_auto`, the file's reader | DuckDB's sniff, or the declaration as `columns` |
| `json` | each page read whole by a fetch thread, scanned for its cursor, the next asked for at once; the loop guard compares it with the page before | arrow-json, per page, rows at `rows`, on decoder threads, taken into the table in page order | the declaration, or DuckDB's sniff of the first page, pinned |

Every path ends where every source does: the narrowing re-applied, the canonical mapping, the
table written on one thread (REQ-15). Batches — an Arrow stream's, a JSON page's — go into the
table as they arrive, through `source::Ingest`, in one transaction committed when the source
succeeds; a body DuckDB reads itself is written as a file of its format is: a staged Parquet body
straight from DuckDB's reader into the table, a spooled CSV or NDJSON body as the same file goes:
a CSV or NDJSON body through a staging table, held to 256 MiB of memory and spilling past it,
whatever its size (R4-2; an NDJSON body over 64 MiB was read straight in on one thread before)
(`a_spooled_body_is_staged_or_read_straight_in_as_its_file_is`). Decompression (`gzip`/`zstd`, by header or by
`compression`) sits between the socket and the spooled or staged file or the page buffer, counted
against `max_bytes` as it decodes. The cursor or next URL of a page, and its row count, are
found by a scan that skips every other value (`src/sources/http/skim.rs`), so the next page is
asked for while this one decodes (R4-3); the arrow-json decode reads the cursor too, as one more
column of the page's schema, and must agree with the scan, or the source fails.

CSV and NDJSON through DuckDB's readers are what makes them origin-blind by construction: the
same reader, with the same sniffer, as the same file on disk: the spool is named as the file
would be (`http-body.csv`, `http-body.ndjson`), and read by the very reader text a file of that
name is. The one cost is that a declared schema is enforced by DuckDB's cast rather than by
REQ-6's validator: a value that does not cast fails the load, naming the column, which is the same
outcome. A CSV declaration goes in `types`, by column name, never `columns`, which DuckDB applies
by position and would load one column's values into another for a body in another order. A
declared CSV's dialect is stated rather than sniffed — `delimiter` (`,` unless set), `"` as quote
and escape — so the sniffer reads only the header and the date formats: 66 M instructions for
a 1,000-row body instead of 462 M, and a 1,000-row session's first `describe` 34% sooner
(docs/history/PERFORMANCE-rounds.md, R2-3). A body in another dialect fails, naming `delimiter`.

## Schema

- **Declared (optional for every format).** `[[source.schema]]` as for an exec connector, in the
  same vocabulary. For `arrow` and `json` the declaration is the decoder's schema and a body that
  breaks it is refused naming the column (REQ-6). For `csv` it is DuckDB's `types`, by column
  name, and for `ndjson` DuckDB's `columns`; a value that does not cast fails the load naming the column. For `parquet` it pins: each
  pinned column must exist and load as the type pinned, as for ClickHouse
  (`a_pinned_schema_that_does_not_match_refuses_the_load`), and only pinned columns are loaded.
- **Inferred when not declared.** Never by a second inference engine: a schema benchy infers must
  be the one DuckDB would infer from the same rows as a file, or origin-blindness breaks (arrow-
  json reads `"2014-01-01 00:00:00"` as a string, DuckDB as a `TIMESTAMP`). So `parquet`, `csv` and
  `ndjson` are read by DuckDB's own readers, which infer as they do for a file; `arrow` carries its
  schema; and for `json` the first page is written to the staging directory and sniffed with
  DuckDB's `read_json` (the type of its `rows` column, a list of structs), and that schema is
  arrow-json's for every page after. A later page that does not fit fails the source
  (`E_SCHEMA`), naming the column, rather than loading as some other type. `benchy-mcp check`
  prints the inferred schema as `[[source.schema]]` entries to pin.
- **Canonical types.** Every column goes through `src/canonical.rs` like every other source:
  integers to `BIGINT` (`UBIGINT` to `HUGEINT`), floats to `DOUBLE`, timestamps to
  `TIMESTAMP WITH TIME ZONE` in microseconds, zone-less ones read in `timezone`.

## Bounds

| bound | default | counts | when passed |
|---|---|---|---|
| `max_rows` | 10,000,000 | rows decoded, every page | `E_TOO_LARGE`, refused, nothing loaded |
| `max_bytes` | 256 MiB | the body **after** Content-Encoding and `compression` are undone, every page; an Arrow body's dictionary and view columns by what they unpack to, as for every stream | `E_TOO_LARGE`, at the cap: a decompression bomb is stopped at `max_bytes`, never decoded whole |
| `max_pages` | 10,000 | requests that returned a page | `E_TOO_LARGE`: refused, never truncated |
| loop guard | — | a cursor or URL already followed; a page byte for byte the one before | `E_HOST_PROTOCOL` |
| the session's `[budget]` | as configured | what was read, charged as it is read | `E_TOO_LARGE`, naming the budget |
| `deadline_secs` | 300 s | the whole source | `E_TIMEOUT` |

`max_bytes` counts the decoded body rather than the Arrow it becomes because the decoded body is
known as it streams, for every format alike, and is what the reader has to hold; for `arrow` the
two are the same number, which is what ClickHouse's `max_bytes` already counts. Parquet is the one
format whose decoded size can far exceed its body, so its footer's row count is checked against
`max_rows` and the budget before a row group is read
(`a_parquet_body_over_max_rows_is_refused_from_its_footer`).

**Budget on failure (M1).** What a fetch read is spent on the budget whether or not it loads
(`a_failed_fetch_still_spends_what_it_read`): every page read before a failure, and the bytes of a
failed page. The loader reports it as a connector loader does; the server charges it before the
next source is fetched (`a_failed_http_fetch_is_charged_when_served`).

**Admission.** HTTP has no `count()`, and does not go through `load_within`: a bulk body sent
uncompressed with a `Content-Length` is refused on that length, past `max_bytes` or what the
budget has left, before it is read; any other body is held to the caps as it streams, so a
missing length costs bytes, never a truncated table.

## Failure semantics

No truncation, ever. A failed request past its retries, a page that does not parse or does not
fit the schema, a body cut short (no end-of-stream marker for Arrow, a short `Content-Length` or a
missing last chunk for the rest), a cap, the loop guard or `max_pages`: each fails the whole
source, and the table, with every page before it, is rolled back or dropped. The session starts with the
other sources (`one_failed_source_does_not_cost_the_others`); `describe` lists the table as not
available, by name and nothing more.

| what happened | code |
|---|---|
| 401, 403 | `E_AUTH` |
| connect, DNS, TLS, 5xx, 408 and 429 past their retries | `E_UNAVAILABLE` |
| other 4xx, a redirect off the origin, a body that is not its `format` | `E_CONFIG` |
| past `deadline_secs` | `E_TIMEOUT` |
| a cap, `max_pages`, the budget | `E_TOO_LARGE` |
| a column missing or of another type | `E_SCHEMA` |
| a loop, a body cut short | `E_HOST_PROTOCOL` / `E_HOST_TRUNCATED` |

## Containment

**Where it runs.** In a loader process of its own, as the ClickHouse source does (`Job::BuiltIn`
in `src/loader.rs`, `BuiltIn::Http` beside `BuiltIn::Clickhouse`): started by the server,
holding this source's `env` and no other's, its rows validated, canonicalised and written to the
staging DuckDB file, which the engine then attaches read-only. The engine has no network and
never sees the URL or the credential.

**How it is confined.** Unlike an exec connector, this loader runs only benchy's own code, so it
is confined, and it parses a remote server's bytes with arrow-json, flate2 and libzstd, which
makes it the process most worth confining (decision 1 below). Every built-in network loader,
ClickHouse and HTTP alike, goes through one confinement point, `loader::builtin`: it opens
DuckDB first, as a file loader does, then, as its only thread and before its first request,
Landlocks itself with `sandbox::restrict_network_loader(stage, ports)`: read-write its staging
directory, read the resolver's files (`sandbox::RESOLVER_FILES`: `/etc/resolv.conf`,
`/etc/hosts`, `/etc/nsswitch.conf`, `/etc/host.conf`, `/etc/gai.conf`, measured with `strace`
as what glibc's resolver opens) and nothing else, and — on Linux 6.7+, Landlock ABI 4 —
TCP-connect only to `BuiltIn::ports()` and listen on none (REQ-3
`a_network_loader_reaches_only_its_source_and_its_staging`; for HTTP, REQ-17
`an_http_loader_is_confined_before_its_first_request`). For HTTP those ports are the URL's and
each `redirect_hosts` entry's: the port it names, or 443 (and 80 when the source itself is
http). The threads a load starts later — the JSON fetch and decoder threads (R4-3), DuckDB's
readers on a spool — start after the restriction, so they inherit it. TLS reads no file: the
roots are compiled in (webpki-roots). DNS queries go over UDP, which Landlock does not handle; a
reply too large for UDP, retried over TCP to port 53, is refused. Landlock cannot restrict by
host; the origin check does that. Measured on a 1M-row ClickHouse load: the confinement costs
two spans of about 0.1 ms each and is lost in the run's 2% instruction noise.

## Origin-blind: the tests that prove it

- `every_http_format_loads_as_the_same_table_as_its_file` — REQ-15's fixture served by the test
  server (`tests/common/http.rs`) as Arrow, Parquet, CSV, NDJSON and as cursor-paged JSON (pages
  of 100 of its 288 rows), each against the same fixture loaded as a file: `describe` and the six queries
  of `the_same_data_looks_the_same_from_every_source`, `typeof` among them, byte for byte.
- `an_http_table_has_the_physical_layout_of_a_file_table` — `pragma_database_size()`,
  `pragma_storage_info` and the catalog views on 840,000 rows, the JSON 10,000 to a page, as
  `the_physical_layout_says_nothing_of_where_data_came_from` checks for the other five.
- `an_http_error_reaches_the_operator_not_the_agent` — a 500 whose body names the host and echoes
  the key: the operator log has the status and the redacted body; the agent's `describe` and a
  query naming the table say only that it is not available.

The test server: `tests/common/http.rs` answers each request with a test's closure and
records it — status, headers, `Link`, `Retry-After`, a chunked body, a body cut short, a delay —
and `the_fake_http_server_answers_and_records_each_request` holds it to that.

## Decisions

The owner settled these on 2026-10-11, each as recommended. What follows each number is
the question and the reasoning, kept for why. All are built as decided; 4, 5 and 7 say what
would reopen them.


1. **Confine the HTTP loader with Landlock?** It runs only benchy's code but parses a remote
   server's bytes. **Decided: yes, for HTTP and ClickHouse alike**, as Containment above describes (staging directory, the
   resolver's files, TCP to the URL's and the redirect hosts' ports only). Built: REQ-3
   `a_network_loader_reaches_only_its_source_and_its_staging`, and for HTTP REQ-17
   `an_http_loader_is_confined_before_its_first_request`.
2. **What does `max_bytes` count?** **Decided: the body after decompression**, for every
   format (above). The alternative, the Arrow it becomes, is not known until DuckDB has read a
   CSV or NDJSON body, so it could not stop a load as it streams.
3. **Redirects across origins.** **Decided: same-origin only, plus an explicit
   `redirect_hosts` list followed without the credential** (for presigned object-store URLs). No
   wildcard hosts.
4. **A next-page URL on another origin** (some APIs page through a different host).
   **Decided: refuse, as built.** If a real API needs it, the same `redirect_hosts` list
   could admit it without the credential, which such an API would then not get.
5. **Short pages.** Offset and page-number paging end only at an empty page, which costs one
   request more than stopping at a short one, but never drops rows when an API caps its page size
   below `page_size`. **Decided: keep it; no opt-in to stop early.** An API that answers past
   its end with an error rather than an empty page would need a `stop_on_status = 404` key; add it
   when one is met.
6. **Inference for JSON.** **Decided: DuckDB's sniff of the first page, pinned for the rest**,
   so an inferred HTTP table types exactly as the file would. A later page with a wider type fails
   the source rather than reinterpreting earlier pages; `check` prints the schema to pin.
7. **A bulk body that breaks mid-stream.** **Decided: fail (no retry) for now**; resuming with
   `Range` against an `ETag` is a later change, measured against a real flaky server.
8. **An API that refuses a pushdown parameter** (a 400 naming it). The ClickHouse source met the same shape with
   ClickHouse's read-only users: the settings were a preference, not a need, so a refused request
   is retried without them. Here too correctness never rests on pushdown — the narrowing is
   re-applied after ingest. **Decided: no automatic retry**: a 400 cannot say which
   parameter it disliked in any standard way, and quietly fetching everything can be a
   thousand-fold more bytes. Fail with `E_CONFIG`, naming the parameters sent, and let `check`
   show the request.
9. **Pipelining pages.** Cursor and link paging are sequential by nature; offset and page-number
   paging could fetch the next page while one decodes. **Decided: one page ahead, on one
   extra thread**, for every style where the next request is known before the page is decoded,
   measured before it lands; no wider fan-out, which an API's rate limit would punish.
   **Built (R4-3), for every style:** a scan finds each page's cursor or next URL without
   decoding it, so cursor and link paging fetch ahead too. Still one request at a time, with up
   to four pages waiting for their decode on decoder threads; no fan-out ("As built" below).

## As built

Built in the order the design set: `BuiltIn::Http` and Arrow, then Parquet, CSV and NDJSON, then
paginated JSON, then the origin-blind tests. Where the build departs from the text above, or
settles what it left open:

- **Pages are fetched ahead of their decode** (R4-3, 2026-10-12; decision 9 built for every
  style, measured first). What it saves is not only the network's round trip: against the local
  harness server the decode itself was most of the path (arrow-json's tape decoder 46% of
  `http.fetch`). A fetch thread reads each page, scans it for its rows and cursor without
  decoding them, and asks for the next; two decoder threads (DuckDB's count less two, one to
  three) decode them; the load's thread takes the batches in page order. Still one request at a
  time, so `rate_limit` and an API's own limit see what they saw; at most four pages wait between
  the fetch and the table (69 MB loader peak at 1M rows, against 63). A page's decode must agree
  with its scan, and a page the scan cannot read plainly is decoded before the next is asked
  for. Faults keep their page order: a failed page is reported, as that page, only after every
  page before it went in, and whatever was fetched past it is not loaded
  (`json_pages_are_fetched_ahead_of_their_decode_and_enter_in_order`). http-json-1m's first
  `describe` 1,131 / 1,174 → 675 / 740 ms (minimum / median), for 11% more instructions
  (docs/history/PERFORMANCE-rounds.md, R4-3).
- **CSV and NDJSON are spooled, then read on every thread** (R3-3, 2026-10-12). Through the FIFO
  they were first written into, DuckDB read a body in order on one thread whatever `threads`
  said (so the read was set to one: with the loader's four, the 1M-row NDJSON body cost 9.08 G
  instructions against 7.05 G with one), and its first `describe` came 0.3 s (CSV) and 0.55 s
  (NDJSON) after the same file's. Spooled to a file in the staging directory as it arrives, and
  read as that file, measured interleaved with the FIFO, ten runs each, minimum / median:
  http-csv-1m 1,088 / 1,215 → 881 / 1,023 ms, http-ndjson-1m 1,274 / 1,438 → 981 / 1,109 ms,
  instructions within 0.2%, the NDJSON loader's peak 173 → 145 MB. The spool is written to disk
  (the loader wrote 4.3 → 42.7 MB and 64.6 MB) and costs 44 ms and 65 ms of the load
  (`http.spool`); that is all an overlap of download and parse could win back, against a
  growing file DuckDB's readers would take for a whole one, so the two are not overlapped. The
  spool is held to `max_bytes` as it is written, by the reader that decodes the body, removed
  once read, loaded or not, and a body that broke off is refused before DuckDB reads any of it
  (`a_csv_or_ndjson_body_is_spooled_to_staging_within_max_bytes_and_removed`). **Disk:** one body
  at a time per loader, up to the source's `max_bytes` (256 MiB by default).
- **JSON integers are cast strictly.** arrow-json reads a number that is not an integer into an
  integer column through `f64` and truncates it: `2.5` loaded as 2 (arrow-json 58.4, measured).
  Integer columns are decoded as their text and cast by arrow's strict cast, so a wider value
  fails the source (`a_later_json_page_with_a_wider_type_fails_the_source`).
- **JSON numbers and booleans read into text columns** (`coerce_primitive`), as DuckDB reads them
  into a `VARCHAR`; so a numeric cursor works too.
- **A sniffed column the vocabulary has no token for** (a struct, a list, a UUID, a column null on
  every row of the first page) refuses the source, naming it, rather than loading a type the same
  rows from a file would not have. Declare it.
- **A redirect host names its port** when it is not the scheme's, since the confined loader may
  connect only to ports it was told of; and a plain-http source may redirect to a listed host over
  http (never https down to http), which is what lets the tests run without TLS.
- **Two config checks found in the build**: columns both pinned and named in `narrow.columns`
  (refused, as for ClickHouse), and a `rows` path that is `""` or overlaps the cursor's.

Measured with `bench/ingest.py` (docs/PERFORMANCE.md) on its 1M-row table, served by a local
server in the harness's own process, three runs each, minima; 4-core aarch64, Linux 6.8. The file
baselines are the same data as a folder; Arrow's is the exec connector `cat`ting the same stream.

| format | HTTP instructions | file instructions | HTTP loader CPU | file loader CPU | HTTP loader peak RSS |
|---|---:|---:|---:|---:|---:|
| Arrow | 3.21 G | 3.21 G (exec) | 0.59 s | 0.60 s | 70 MB |
| Parquet | 3.69 G | 3.70 G | 0.76 s | 0.82 s | 103 MB |
| CSV | 6.28 G | 6.36 G | 1.25 s | 1.12 s | 119 MB |
| NDJSON | 7.05 G | 7.05 G | 1.49 s | 1.33 s | 177 MB |
| JSON pages, 10,000 rows each | 6.31 G | 7.05 G (NDJSON) | 1.20 s | 1.33 s | 72 MB |

Instructions match the file within the runs' spread for every bulk format; the JSON pages cost
less than the same rows as an NDJSON file. The CSV and NDJSON loaders spend 0.1–0.2 s more CPU
than the file's: moving the body through the pipe, in the kernel and on the writer thread, which
`perf stat`'s user-space instructions do not count. NDJSON's peak memory is higher than the
file's 143 MB. (Before R3-3; spooled, both are read as their files are, above.)
