-- SQL helpers for the bundled data, examples/data: views and macros the agent calls like
-- built-ins. A config's [helpers] names them, as examples/benchy.toml does:
--
--   benchy-mcp --config examples/benchy.toml
--
-- Each helper is one CREATE VIEW or CREATE MACRO, and the `--` lines directly above it, joined
-- into one line of at most 200 bytes, are what `describe` says of it. Tables are named unqualified: they resolve when the helper is called.
--
-- A macro reads no table: DuckDB expands a macro inside the caller's query, where a CTE of the
-- caller's named like the table would stand in for it, so benchy refuses a helper macro whose
-- body reads one. A view is bound on its own and reads the table. So what reads a table is a
-- view, which the agent filters with SQL of its own, and a macro is a formula, or a table macro
-- over its arguments alone (`range`, `generate_series`, `unnest`, VALUES).

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

-- Each reading with how long it held, until the meter's next reading (NULL for its last):
-- `sum(epoch(held)) FILTER (WHERE power_kw > 3) / 3600` is the hours above 3 kW.
CREATE VIEW reading_spans AS
SELECT meter, observed_at, power_kw,
       lead(observed_at) OVER (PARTITION BY meter ORDER BY observed_at) - observed_at AS held
FROM meters;

-- Every hour from start_at up to, not including, end_at, as `hour`: join it to `hourly` to find
-- the hours a meter has no readings.
CREATE MACRO hours_of(start_at, end_at) AS TABLE
SELECT range AS hour FROM range(start_at::TIMESTAMPTZ, end_at::TIMESTAMPTZ, INTERVAL 1 HOUR);
