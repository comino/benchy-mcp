-- SQL helpers for the bundled data, examples/data: views and macros the agent calls like
-- built-ins. A config's [helpers] names them, as examples/benchy.toml does:
--
--   benchy-mcp --config examples/benchy.toml
--
-- Each helper is one CREATE VIEW or CREATE MACRO, and the `--` lines directly above it are what
-- `describe` says of it. Tables are named unqualified: they resolve when the helper is called.
--
-- A table macro reads its arguments once, in a FROM-less CTE, and uses them from there. DuckDB
-- substitutes an argument's expression into the macro's body, so a body that names its arguments
-- next to a table alias of its own binds the caller's `m.meter` to the macro's own `m`. See
-- "Writing a table macro" in docs/DIALECT.md.

-- Readings per meter per hour: mean, peak and how many there were.
CREATE VIEW hourly AS
SELECT meter,
       date_trunc('hour', observed_at) AS hour,
       avg(power_kw) AS mean_kw,
       max(power_kw) AS peak_kw,
       count(*) AS readings
FROM meters
GROUP BY ALL;

-- Dew point in °C from air temperature in °C and relative humidity in % (Magnus formula).
CREATE MACRO dewpoint(t, rh) AS
    243.04 * (ln(rh / 100) + 17.625 * t / (243.04 + t))
    / (17.625 - ln(rh / 100) - 17.625 * t / (243.04 + t));

-- Hours one meter spent above a power threshold in kW within [start_at, end_at), each reading
-- held until the next one.
CREATE MACRO hours_above(series_id, threshold, start_at, end_at) AS TABLE
WITH arg AS (
    SELECT series_id AS series_id, threshold AS threshold,
           start_at::TIMESTAMPTZ AS start_at, end_at::TIMESTAMPTZ AS end_at
),
held AS (
    SELECT m.power_kw > arg.threshold AS above,
           lead(m.observed_at) OVER (ORDER BY m.observed_at) - m.observed_at AS lasted
    FROM arg, meters m
    WHERE m.meter = arg.series_id
      AND m.observed_at >= arg.start_at AND m.observed_at < arg.end_at
)
SELECT coalesce(sum(epoch(lasted)) FILTER (WHERE above), 0) / 3600 AS hours
FROM held;
