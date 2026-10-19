# DuckDB on the bench — the deltas that actually cost a step

<!-- maintainers -->
REQ-11. Two audiences: an agent that just got an error, and whoever maintains `src/hint.rs`.
The `dialect` tool serves this file with every block like this one left out
(`the_dialect_reference_is_written_for_the_agent`), so what is only for maintainers goes in one.

**This file is not in the per-call payload.** A dialect primer in the instructions would repeat
at every step, which is why the guidance lives in the *error* instead. The short version travels
in the server instructions; this is the long version, fetched only when someone wants it.

Measured against **DuckDB v1.5.5**, the version benchy links. The tool names the running engine's
version above what it serves, and `version_not_hardcoded` asserts the instructions do too,
because this drifts between releases.

It names benchy's own tools, `describe`, `query` and `get_result`, as its MCP server calls them.
A host serves this file itself (the host protocol has no `dialect` call), and a host whose
`hello` renames the tools (`tool_names`, docs/HOST-PROTOCOL.md) rewrites those names in its copy:
benchy's other texts follow `tool_names`, this one does not.
<!-- /maintainers -->

## What is different about this engine *because it is a bench*

These are benchy's rules, not DuckDB's, and no amount of dialect knowledge gets around them:

| | |
|---|---|
| No file or network functions | `read_csv`, `read_parquet`, `read_json`, `ATTACH`, `INSTALL`, `httpfs` — all unavailable. The data is already loaded as tables; `describe` names them. |
| Some table functions are refused | Those that change the engine (`enable_logging`, `enable_profiling`, `checkpoint`, `truncate_duckdb_logs`, …), show its workings (`duckdb_logs`, `duckdb_memory`, `duckdb_extensions`, `duckdb_secrets`, …) or run SQL from a string (`query('…')`, `json_execute_serialized_sql`): nothing in the batch runs, and the refusal says why. Write the query out instead of `query('…')`; `query_table('name')` reads a table named in a string. `range`, `generate_series`, `unnest`, `json_each`, `summary`, `histogram`, `pragma_table_info` and the catalog's `duckdb_tables()`, `duckdb_columns()` and the rest work. |
| Loaded tables are read-only | DuckDB enforces it: `UPDATE`, `INSERT`, `DELETE`, `ALTER` and `DROP` against them fail. `CREATE TABLE mine AS SELECT * FROM loaded` makes a copy you can change. |
| No file output | `COPY … TO` and `EXPORT DATABASE` are refused. Results come back through `query`; `get_result` pages a long one. |
| No ENUM is kept | `CREATE TYPE`, a table or view with an `ENUM` column, nested or not, and a DEFAULT, generated column, CHECK, index, view or macro that casts to an `ENUM`, are refused and undone, `ENUM(` inside a string literal of what is kept included. Use `VARCHAR`. An `ENUM` inside a query that keeps nothing, and `PIVOT`, work as usual. |
| Your own tables are free | `CREATE TABLE`/`VIEW`/`MACRO` land in `memory.main` and persist between calls, until a hard kill takes the engine. A name already taken is refused: for a table or view, a loaded table's, a helper's, a table's a helper reads or a system view's like `sqlite_master`; for a macro, a helper's or a built-in function's like `lower` or `range`. Pick another, such as `my_readings`. A table or view may be named like a function: `CREATE TABLE range …` works, and `FROM range` then reads it. |
| Helpers are the operator's | Views and macros the operator provides are listed in `describe` under "Helpers". Call them like built-ins. They cannot be dropped, changed or replaced, and their names are taken. In a batch that calls a helper macro (or a macro of yours that calls one), or that makes a view or macro, a CTE may not be named like a table a helper macro reads (`WITH meters AS (…)` when one reads `meters`), in any statement: DuckDB expands the macro inside your query, where your CTE would stand in for the table. Pick another CTE name, such as `my_meters`. `query_table('name')` works. |
| Results are capped | Default 200 rows / 8 KB. Truncation is always stated. Aggregate rather than paging. |
| One batch, one deadline | Several statements per `query` call; the deadline covers the batch, not each statement. If one fails, nothing after it runs; the ones before it keep their effects, and their results come back with the error. |

## Functions other engines have and DuckDB does not

Each of these returns `Catalog Error: Scalar Function with name X does not exist!`, and the
error names the DuckDB form.

<!-- maintainers -->
Every row was verified by calling it: the "missing" list is what returned that error on v1.5.5.
`src/hint.rs` fires on exactly these, and `tests/hints.rs` keeps the two in step.
<!-- /maintainers -->

| you wrote | DuckDB |
|---|---|
| `to_char(ts, fmt)` | `strftime(ts, '%Y-%m')`; for numbers, `printf` |
| `to_date(s, fmt)` | `strptime(s, '%Y-%m-%d')::DATE` |
| `date_format(ts, fmt)` | `strftime(ts, fmt)` |
| `str_to_date(s, fmt)` | `strptime(s, fmt)` |
| `nvl(a, b)` | `coalesce(a, b)` |
| `nvl2(x, a, b)` | `CASE WHEN x IS NOT NULL THEN a ELSE b END` |
| `isnull(x)` | `x IS NULL`; for the two-argument T-SQL form, `coalesce` |
| `getdate()`, `sysdate()` | `now()` |
| `convert(type, x)` | `CAST(x AS type)` or `x::type` |
| `dateadd(day, 1, ts)` | `ts + INTERVAL 1 DAY` |
| `charindex(n, h)` | `position(n IN h)` or `instr(h, n)` |
| `iif(c, a, b)` | `CASE WHEN c THEN a ELSE b END` — or `if(c, a, b)`, which DuckDB *does* have |
| `rand()` | `random()` |
| `timestampdiff(day, a, b)` | `date_diff('day', a, b)` |
| `SELECT TOP 5 ...` | `SELECT ... LIMIT 5` |
| `` `backticked` `` | `"double quoted"` — backticks are not identifier quotes |
| `"a string"` | `'a string'` — double quotes mean an identifier |
| `... FOR UPDATE` | nothing to lock; the bench is yours alone |

### These do work

Other engines' spellings that DuckDB has too: `ifnull`, `if`, `coalesce`,
`try_cast`, `percentile_cont`, `extract`, `INTERVAL`, `ILIKE`, `||`, `LIMIT ... OFFSET`,
`group_concat`, `string_agg`, `listagg`, `array_agg`, `median`, `datediff`, `date_diff`,
`datepart`, `date_trunc`, `strftime`, `strptime`, `instr`, `position`, `split_part`,
`regexp_replace`, `generate_series`, `unnest`, `greatest`, `least`, `decode`, `epoch`,
`last_day`, `dayname`, `monthname`, `quarter`, `weekofyear`, `approx_quantile`, `uuid`,
`gen_random_uuid`, `json_extract`, `json_value`, `[bracketed]` list syntax.

`duckdb_functions()` is **not** the authority on whether something works: `coalesce`, `if`,
`try_cast`, `extract` and `percentile_cont` are parsed specially and do not appear there. Call
the function; read the error.

<!-- maintainers -->
The first pass of the table above was built from `duckdb_functions()` and was wrong about six
entries.
<!-- /maintainers -->

## What DuckDB has that is worth reaching for

Not deltas — reasons the bench is worth using at all.

| | |
|---|---|
| `ASOF JOIN` | join to the most recent row at or before a timestamp, without a window-function dance |
| `QUALIFY` | filter on a window function without a subquery |
| `GROUP BY ALL`, `ORDER BY ALL` | no repeating the projection |
| `SELECT * EXCLUDE (a, b)`, `* REPLACE (x*2 AS x)` | trim or patch a wide projection in place |
| `time_bucket(INTERVAL '15 minutes', ts)` | regular buckets without arithmetic on epochs |
| `list_*` / `struct_*`, `UNNEST` | nested data without a join explosion |
| `PIVOT` / `UNPIVOT` | as statements, not hand-rolled conditional aggregation |
| `approx_quantile`, `reservoir_sample` | cheap distribution answers on large tables |
| `regr_slope`, `corr`, `covar_pop` | regression in the engine rather than in the agent's head |
| `union_by_name` | stack tables whose columns do not line up |

## Writing a table macro: read the arguments once

On DuckDB 1.5.5 a macro's argument is substituted into its body as an expression, not passed as
a value. If the body has a table alias of the same name as one in the caller's query, the
argument binds to the macro's alias:

```sql
CREATE MACRO above(series_id, threshold) AS TABLE
SELECT count(*) AS n FROM meters m WHERE m.meter = series_id AND m.power_kw > threshold;

SELECT m.meter, a.n FROM rooms m, LATERAL above(m.meter, 0) a;
-- every meter gets the count of all of them: inside the body, `m.meter = m.meter`
```

Read the arguments once, in a CTE with no `FROM`, where the caller's alias is the only one in
scope, and use them from there:

```sql
CREATE MACRO above(series_id, threshold) AS TABLE
WITH arg AS (SELECT series_id AS series_id, threshold AS threshold)
SELECT count(*) AS n FROM arg, meters m
WHERE m.meter = arg.series_id AND m.power_kw > arg.threshold;
```

A `LATERAL` call then gives each row what a call with that row's value gives. The example helpers
in `examples/helpers.sql` are written this way (`a_lateral_macro_call_equals_the_per_id_call`).

The body is expanded inside the caller's query in the same way, so a table it names binds to a
CTE of the caller's of that name: `WITH meters AS (…) SELECT * FROM above('lab', 3)` counts the
CTE's rows. A view is bound on its own and reads the table. A CTE named like a table one of the
operator's macros reads is refused for this reason, in a SELECT or in a subquery of an UPDATE,
DELETE, MERGE or INSERT (`a_cte_cannot_stand_in_for_a_table_a_helper_macro_reads`,
`a_cte_in_any_statement_cannot_stand_in_for_a_table_a_helper_macro_reads`), in a batch that
calls such a macro, a macro of yours that calls one, or makes a view or macro
(`a_cte_named_like_a_helpers_table_is_refused_only_beside_a_helper_call`); in a macro of your
own, name its tables so no CTE of the query that calls it takes them.

<!-- maintainers -->
## Regenerating this

The probes that produced the measured lists are single-purpose scripts: start the server on
`examples/data`, call each candidate, record which raise `does not exist`. Re-run them after a
DuckDB bump — `tests/hints.rs` will catch a changed *message*, but only a fresh probe catches a
function DuckDB has newly *added*, which turns a correct hint into a wrong one.
<!-- /maintainers -->
