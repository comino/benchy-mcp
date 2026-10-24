# Bughunt — 2026-10-24

Added **29 regression tests**, each reproduced red before its fix, and one IPC recovery unit
test. The hunt began at commit `bff46d2`. The first 20 tests cover HTTP, queries, profiles and
configuration; nine more cover credential disclosure and malformed Arrow streams. Tests
exercise individual APIs, real local HTTP servers, table ingestion, and the shipped binary.
Accepted failures are assertion failures or a reproduced parser panic, not compilation errors
or missing services. Subagents split the investigation and subsequent fixing campaign across
HTTP/credentials, queries/configuration, and Arrow streams; the parent fixed profiles and
independently verified the regressions.

Run every new regression, including targets after the first failure:

```sh
cargo test --release --no-fail-fast \
  --test bughunt_host --test bughunt_http \
  --test bughunt_profile --test bughunt_query \
  --test bughunt_security_http --test bughunt_security_stream -- --nocapture
cargo test --release --lib a_credential_cut_inside_utf8_is_redacted_at_both_body_edges
```

The fixing campaign retains all original assertions. No new test is ignored or marked
`should_panic`, and existing tests and dependencies are unchanged. Mutation testing was not
needed to expose these failures. Source line references below refer to the pre-fix code at
`bff46d2`; the descriptions record the original failures.

## Highest-impact findings

- HTTP redirects stop validating destinations after the first allowed cross-origin hop.
- A percent-encoded credential survives stripping and reaches the next HTTP origin.
- ClickHouse errors echo passwords into logs; truncation can expose credential prefixes in
  both HTTP and ClickHouse diagnostics.
- Marker-like bytes in a value let an unterminated Arrow file or pipe commit; channel recovery
  can consume the next stream and attach the wrong later dataset.
- A valid Link header can silently commit only the first page of a dataset.
- Large 128-bit integers render as different numbers: one loses a decimal digit, another
  becomes negative.
- Two source names differing only in ASCII case pass validation and startup despite sharing
  the same SQL identifier.

## HTTP: [tests/bughunt_http.rs](tests/bughunt_http.rs)

**Redirect validation stops after the first allowed cross-origin hop.**
`every_redirect_hop_is_checked_after_a_listed_cross_origin_hop` observes a request to an
unlisted destination. In `src/sources/http/client.rs:481`,
`credential && self.follow(&next)?` short-circuits once credentials have been disabled.
The test uses local hosts on the same destination port; the port sandbox cannot replace
the hostname check. This violates the every-destination rule in
[HTTP-SOURCE.md, Redirects](docs/HTTP-SOURCE.md#redirects).

**Lowercase percent escapes bypass credential stripping.**
`an_echoed_secret_is_stripped_when_percent_escapes_use_lowercase_hex` observes the synthetic
credential `bughunt/secret` as a query value on the second origin, although the auth header
was removed. `without_secrets` at `src/sources/http/client.rs:374` compares encoded strings
without decoding them. Uppercase and lowercase hex escapes represent the same octet.
The documented rule removes query parameters holding a secret's value.
See [RFC 3986 §2.1](https://www.rfc-editor.org/rfc/rfc3986.html#section-2.1).

**A quoted `<` hides a next-page link.**
`a_quoted_link_title_does_not_hide_the_next_relation` gets `None` from a header with
`title="readings < 10"; rel="next"`.
`link_pagination_loads_every_page_when_a_title_contains_an_angle_bracket` proves that the
loader then reports success with **one row instead of two**.
`src/sources/http/paginate.rs:373` splits parameters at the next `<` without respecting
quotes. Quoted parameters are required by
[RFC 8288 §3](https://www.rfc-editor.org/rfc/rfc8288.html#section-3), with quoted-string
parsing in [Appendix B.4](https://www.rfc-editor.org/rfc/rfc8288.html#appendix-B.4).

**Relative pagination links use the URL before a redirect.**
`a_relative_next_link_uses_the_final_response_url_after_a_redirect` fetches `/alias`, which
redirects to `/v1/readings`. Its relative next link is requested as `/readings?page=2`
instead of `/v1/readings?page=2`, producing HTTP 404 and rejecting the load.
The pager resolves against its unchanged `current` URL at `src/sources/http/paginate.rs:259`;
the HTTP response does not convey the final URL to it. The retrieval's final URL is the base
under [RFC 3986 §5.1.3](https://www.rfc-editor.org/rfc/rfc3986.html#section-5.1.3).

## Query/results: [tests/bughunt_query.rs](tests/bughunt_query.rs)

**128-bit integers render as different numbers.**
`a_39_digit_hugeint_keeps_every_digit_or_says_it_cannot_render` selects
`170141183460469231731687303715884105727` and gets
`17014118346046923173168730371588410572`.
`an_uhugeint_above_the_signed_range_keeps_its_value_or_says_it_cannot_render` selects
`340282366920938463463374607431768211455` and gets `-1`.
Both pass through `collect_arrow` and its formatter at `src/workspace.rs:3399` without a
faithfulness guard. These are wrong values, not declared truncation. The tests allow either
the exact value or an explicit unrenderable marker, consistent with REQ-10.

**Nested values bypass type safeguards.**
`a_nested_bit_is_faithful_or_explicitly_unrenderable` gets `[04f5]` for bits `0101`;
`a_nested_bignum_is_faithful_or_explicitly_unrenderable` gets internal hexadecimal bytes;
`a_nested_timetz_keeps_its_offset_or_says_it_cannot_render` loses the `+02` offset.
`src/workspace.rs:3308` checks only the top-level logical type. REQ-10's faithful-value
or explicit-marker rule must also cover nested values.

**The final page omits the truncation notice for cut cells.**
`the_last_page_still_reports_cut_values` drives MCP `query` and `get_result`. Its last page
contains `[+4 bytes]` but ends with `the end.` and no `TRUNCATED` notice.
`src/result.rs:364` handles cut values only when rendering the entire result from offset zero;
the final-page branch at line 413 misses them. REQ-10 explicitly requires a page's lower
cell cap to say `TRUNCATED`.

**Cut headers leave the machine-readable truncation flag false.**
`a_cut_column_name_marks_the_result_truncated` cuts a 17-byte alias to a 16-byte cell cap.
The header contains `[+1 bytes]`, but `Shown.truncated` is false.
`src/result.rs:297` cuts names without contributing to the flag computed at line 430.

**BC timestamps differ from the promised DuckDB spelling.**
`a_bc_timestamptz_renders_as_duckdb_writes_it` gets `0000-01-01 00:00:00+00` while DuckDB's
VARCHAR conversion gives `0001-01-01 (BC) 00:00:00+00`.
`src/result.rs:533` prints the astronomical year directly. This is a spelling-contract
failure under `timestamp_utc`'s documented behavior, not evidence of a changed instant.

## Profiles: [tests/bughunt_profile.rs](tests/bughunt_profile.rs)

**Distinct keys merge into one series.**
`composite_series_keys_do_not_alias_on_the_internal_separator` gives two different tuples
containing `chr(31)` and sees one series with fictitious repeated timestamps.
`a_loaded_null_series_key_is_distinct_from_a_literal_record_separator` proves through the
shipped binary that SQL sees two keys, but `describe` merges NULL with a literal `chr(30)`.
The encoding at `src/profile.rs:1086` uses unescaped separators and null sentinels. REQ-14's
series counts, gaps, and repeats must describe the actual series.

**An infinite-only time column invents a finite extent.**
`an_infinite_only_time_column_does_not_invent_an_epoch_extent` gives only `-infinity` and
`infinity`; the profile says both ends are `1970-01-01 00:00:00+00`.
`src/profile.rs:945` defaults missing finite extrema to zero while counting the infinite
timestamps as timed rows. REQ-15 explicitly says profile extents omit infinities.

**Five-digit years lose part of their date.**
`dates_with_five_digit_years_keep_the_whole_date_in_the_profile` expects
`12000-01-01 to 12000-01-02` and gets `12000-01-0 to 12000-01-0`.
`Clock::at` at `src/profile.rs:480` assumes every DATE is ten bytes long. REQ-14's extent
must preserve the actual dates.

## Configuration: [tests/bughunt_host.rs](tests/bughunt_host.rs)

**ASCII-case collisions are accepted as different source names.**
`source_names_that_differ_only_by_ascii_case_are_duplicates` accepts both `readings` and
`READINGS`. `a_case_colliding_configuration_is_refused_before_serving_mcp` proves that the
shipped binary loads both files and completes an MCP handshake. DuckDB resolves those
spellings as the same SQL identifier.
`src/config.rs:307` inserts the original spelling into a case-sensitive duplicate set,
contradicting the guard's own requirement that source names be unique to the engine.

**A Unicode duration suffix panics during validation.**
`a_non_ascii_duration_suffix_is_a_config_error_not_a_panic` passes `last = "30日"` and
captures a UTF-8 boundary panic. `src/config.rs:978` splits at the last byte rather than
checking the suffix safely. The caller expects a normal invalid-duration configuration error.

## Credential disclosure: [tests/bughunt_security_http.rs](tests/bughunt_security_http.rs)

**ClickHouse error messages can disclose their own credentials.**
`a_clickhouse_error_cannot_echo_its_password_into_the_operator_log` uses a local server that
echoes the synthetic `X-ClickHouse-Key` value in a 401 response. Startup fails correctly with
`E_AUTH`, but logs the complete password. `clickhouse_check_redacts_an_echoed_password`
reproduces the disclosure through the diagnostic command. Both fixtures assert that the server
received the configured secret, preventing a configuration failure from counting as coverage.

**Truncation can make a secret escape whole-value redaction.**
`an_http_error_is_redacted_before_its_text_is_truncated` places a synthetic credential across
the 2,000-character display boundary; its first 20 characters reach the log.
`an_http_error_cannot_leak_a_secret_cut_by_the_body_read_limit` and
`a_clickhouse_error_cannot_leak_a_secret_cut_by_the_body_read_limit` put it across the 64 KiB
body-read boundary. Leading whitespace makes the retained prefix visible after trimming.
The fix must cover both display truncation and a secret only partly read from the body.
Review also reproduced a partial credential surviving when the byte limit cuts a multibyte
character: decoding inserts a replacement character that defeats a plain prefix comparison.
The unit regression `a_credential_cut_inside_utf8_is_redacted_at_both_body_edges` covers both
ends of a retained body fragment and keeps those fragments masked after lossy UTF-8 decoding.

## Arrow integrity: [tests/bughunt_security_stream.rs](tests/bughunt_security_stream.rs)

**A value's bytes are mistaken for an end-of-stream message.**
`an_eos_bit_pattern_in_a_value_does_not_finish_an_arrow_file` and
`an_eos_bit_pattern_in_a_value_does_not_finish_an_arrow_pipe` write eight integer rows without
an IPC end marker. The final value has the marker's bytes, so the raw-tail check accepts both
loads. Completion must come from the message parser, not the last eight data bytes.

**Recovery consumes another stream after a missing boundary.**
`a_second_schema_before_eos_closes_the_channel_instead_of_eating_the_next_stream` sends three
streams, with the first missing its end marker. The first attach fails, but recovery drains
through the second stream's end marker and permits the third stream as the second table.
The fixed reader marks alignment lost on the second schema and closes the channel. A separate
IPC unit test, `draining_a_truncated_stream_does_not_consume_the_next_stream`, covers recovery
starting before any read, within the schema, and after the last batch.

These tests run through the Linux binary with filesystem confinement asserted. This is an
integrity failure at the stream boundary, not evidence of a sandbox escape.

## Remediation

- Check every redirect destination; compare decoded query values when stripping credentials.
  Parse quoted Link parameters and use the final response URL for relative pagination.
- Share credential redaction with ClickHouse, redact before display truncation, and mask a
  secret prefix at a bounded body-read boundary.
- Require a parsed Arrow end marker and close a channel whose message sequence loses alignment.
- Render scalar signed/unsigned 128-bit integers from their exact bits. Recursively mark
  nested types whose Arrow representation cannot preserve their values; retain the explicit
  `::VARCHAR` workaround. Correct BC spelling and truncation flags/notices.
- Escape reserved series-key characters and fall back from the ingestion tally when a raw key
  could be ambiguous, including its NULL sentinel. Check new keys outside the per-row tally
  loop. Preserve absent finite extrema and complete date strings.
- Reject case-colliding source names and handle invalid Unicode duration suffixes safely.

## Verification

- Existing `cargo test --release --no-fail-fast`: **574 passed, zero failed, one ignored**.
  The ignored test regenerates the shipped example stream. This baseline ran before the new
  test targets were added.
- Explicit real-ClickHouse rerun of `--test clickhouse --test origin`: **27 passed**, no
  skip notices, using the cached local ClickHouse executable. These rerun existing tests;
  they are not 27 additional unique tests.
- Before any production edits, an independent combined run of the first **25 regressions
  failed all 25**, with zero passed or ignored. Evidence: `/tmp/benchy-bughunt-before-fixes.log`.
- The three additional credential tests failed before their corresponding fixes. Evidence:
  `/tmp/benchy-http-extra-red.log` (two body-read boundary failures) and
  `/tmp/benchy-clickhouse-check-red.log` (diagnostic-command disclosure).
- The UTF-8 boundary unit test failed during fix review. Evidence:
  `/tmp/benchy-utf8-boundary-red.log`.
- Final `scripts/ci.sh`, with `BENCHY_CLICKHOUSE_BIN` set: **passed**. Its full release suite
  passed **604 tests, zero failed, one existing ignored fixture-regeneration test**. That is
  the baseline's 574 plus all 30 new tests. Evidence: `/tmp/benchy-fix-ci-verified.log`.
- The final explicit ClickHouse/origin rerun passed **27 tests**, with no skip notices, against
  the real local ClickHouse server. These are reruns, not additional unique tests.
- Formatting, clippy with warnings denied, 535 named requirements tests, toolchain consistency,
  pinned DuckDB verification, packaging checks (6 library + 8 release checks), Python MCP and
  host examples, the Node host example, eval scorer/redaction/jail checks, and doctor: passed.
- An earlier CI run's extra origin rerun had two startup failures while another build was
  running. Both tests passed in the final full suite and its explicit rerun with a stable
  binary and no overlapping build. No assertions or timeouts were changed for these failures.
- `cargo deny` was skipped by the CI script because it is not installed. This run did not
  perform a dependency-advisory audit, macOS validation, or container packaging.

## Ingest measurements

Measured the unchanged baseline binary saved before remediation and the final binary with
`bench/ingest.py`, on Linux aarch64 with four cores. Each scenario loads one million rows.
After an initial before/after comparison showed unstable profile timings, rerunning the
unchanged baseline reproduced the slower timings too. The final comparison alternated the old
and new binaries, reversing their order on alternate repetitions: three measured runs of each,
each preceded by the harness's warm-up, with `--no-perf`. Following the repository's method,
these are the minimum loader CPU times, not wall times:

| Scenario | Before | After | Change |
| --- | ---: | ---: | ---: |
| Parquet, 1M rows | 457.2 ms | 462.5 ms | +1.2% |
| Exec stream, 1M rows / 500 series | 429.2 ms | 467.1 ms | +8.8% |

The stream result is a possible performance cost, not a claimed speedup. Its CPU ranges overlap
(before 429.2–493.5 ms; after 467.1–499.1 ms), and three runs on a shared VM cannot cleanly
attribute that difference to code. The profile check stays outside the per-row tally loop;
tables with reserved key bytes or a NULL sentinel conservatively use the SQL profiling path.
Evidence: `/tmp/benchy-profile-interleaved.json`, with the individual run logs and JSON files
under `/tmp/benchy-profile-paired-*`.

Two probes were discarded: the documented batch-admission reservation limitation, and a
far-future date whose explicit formatter-error marker has an ambiguous contract. This campaign
does not claim exhaustive security coverage; unverified candidates are not reported as bugs.
