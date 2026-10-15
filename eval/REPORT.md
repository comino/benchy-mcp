# Eval — what the bench costs an agent

Rendered by `eval/report.py` from the raw results in `eval/results/`, which hold every run's answer, counts and transcript — tool output cut at 4,000 characters, with the machine's own paths and user name replaced by placeholders. **Regenerate it rather than citing it**: it describes these models, this data and these tasks.

```sh
cargo build --release
python3 eval/fetch_uci.py eval/uci                  # 261 MB download, 36 MB kept
python3 eval/test_eval.py                           # the scorer, and the jail holds
OPENROUTER_API_KEY=... python3 eval/run.py --fresh --duckdb /path/to/duckdb \
    --models anthropic/claude-haiku-4.5,deepseek/deepseek-v4-flash-0731,google/gemini-3.8-flash,openai/gpt-5-mini --repeat 3
python3 eval/report.py
```

The shell arm needs Linux 6.7 or later, for its jail, and the DuckDB CLI; the run refuses rather than measure a shell it cannot confine.

- **Data** — 24 real meters, every 15 minutes of 2014, 840,960 rows; its quirks left in (see the traps below, and the data's source at the end)
- **Tasks** — 13, in `eval/tasks.toml`, each run 3 times per model and arm; step cap 25
- **Arms** — `bench`: benchy-mcp over MCP, with its own instructions. `shell`: a POSIX shell with the DuckDB CLI in a directory holding the data, jailed by Landlock, with a paragraph of the same kind about its own surface — the DuckDB dialect, how to find the data, how to keep tables between calls; its output reaches the model as a terminal would show it, without the CLI's colour codes
- **Setup** — benchy-mcp 0.1.0 built from `d598c96`; DuckDB CLI v1.5.5 (Variegata) d8cdaa33fd; Linux 6.8.0-138-generic aarch64; temperature 0 requested
- **Measured** — 2026-10-11

## Steps

Model calls until the answer, summed over every task and repeat — the measurement.

| model | `bench` steps | `shell` steps | `bench` correct | `shell` correct |
|---|---:|---:|---:|---:|
| `anthropic/claude-haiku-4.5` | 138 | 224 | 36/39 | 36/39 |
| `deepseek/deepseek-v4-flash-0731` | 151 | 267 | 39/39 | 39/39 |
| `google/gemini-3.8-flash` | 186 | 360 | 39/39 | 39/39 |
| `openai/gpt-5-mini` | 126 | 215 | 39/39 | 37/39 |

- `anthropic/claude-haiku-4.5`: the bench took **86 fewer steps** than the shell's 224 (38% fewer).
- `deepseek/deepseek-v4-flash-0731`: the bench took **116 fewer steps** than the shell's 267 (43% fewer).
- `google/gemini-3.8-flash`: the bench took **174 fewer steps** than the shell's 360 (48% fewer).
- `openai/gpt-5-mini`: the bench took **89 fewer steps** than the shell's 215 (41% fewer).

Steps in each repeat, all tasks summed — how far apart the runs were:

| model | `bench` | `shell` |
|---|---|---|
| `anthropic/claude-haiku-4.5` | 46, 46, 46 | 73, 78, 73 |
| `deepseek/deepseek-v4-flash-0731` | 46, 54, 51 | 84, 99, 84 |
| `google/gemini-3.8-flash` | 59, 61, 66 | 115, 120, 125 |
| `openai/gpt-5-mini` | 45, 41, 40 | 74, 71, 70 |

## Reading against computing

`describe` states the answer to five of the tasks outright — `dead_meter`, `last_to_start`, `stopped_date`, `clock_forward`, `clock_back` — because it reports dead, late-starting and stopped series and the clock changes. On those the bench arm reads an answer that the shell arm has to compute: the nearest single command it has, DuckDB's `SUMMARIZE`, gives each column's range and null share, not runs of zero per series. The other eight are computations `describe` does not answer, and are the fairer comparison of the two surfaces.

| model | tasks | `bench` steps | `shell` steps | fewer with the bench | `bench` correct | `shell` correct |
|---|---|---:|---:|---:|---:|---:|
| `claude-haiku-4.5` | named by `describe` (5) | 57 | 102 | 44% | 15/15 | 15/15 |
| `claude-haiku-4.5` | computed (8) | 81 | 122 | 34% | 21/24 | 21/24 |
| `deepseek-v4-flash-0731` | named by `describe` (5) | 60 | 126 | 52% | 15/15 | 15/15 |
| `deepseek-v4-flash-0731` | computed (8) | 91 | 141 | 35% | 24/24 | 24/24 |
| `gemini-3.8-flash` | named by `describe` (5) | 53 | 114 | 54% | 15/15 | 15/15 |
| `gemini-3.8-flash` | computed (8) | 133 | 246 | 46% | 24/24 | 24/24 |
| `gpt-5-mini` | named by `describe` (5) | 46 | 84 | 45% | 15/15 | 15/15 |
| `gpt-5-mini` | computed (8) | 80 | 131 | 39% | 24/24 | 22/24 |

## Where the steps go

*Looking* is finding the data and its shape: benchy's `describe`, or a shell command that lists the directory or reads the data file without DuckDB — `ls`, `head readings.csv` — or runs only a `DESCRIBE`, a `SUMMARIZE` or a `SELECT * … LIMIT n` preview. *Computing* is every other tool call, reading a file the model wrote itself included. *Answering* is the call that ends a run. The last column counts tool outputs that were an error — a mistyped column, a quoting slip — each of which cost the call after it.

| model | arm | steps | looking | computing | answering | outputs that were an error |
|---|---|---:|---:|---:|---:|---:|
| `claude-haiku-4.5` | `bench` | 138 | 39 | 60 | 39 | 0 |
| `claude-haiku-4.5` | `shell` | 224 | 102 | 83 | 39 | 3 |
| `deepseek-v4-flash-0731` | `bench` | 151 | 39 | 94 | 39 | 1 |
| `deepseek-v4-flash-0731` | `shell` | 267 | 87 | 200 | 39 | 7 |
| `gemini-3.8-flash` | `bench` | 186 | 39 | 108 | 39 | 0 |
| `gemini-3.8-flash` | `shell` | 360 | 115 | 206 | 39 | 2 |
| `gpt-5-mini` | `bench` | 126 | 39 | 48 | 39 | 1 |
| `gpt-5-mini` | `shell` | 215 | 81 | 95 | 39 | 3 |

**Against the grain** — tasks where the bench took more steps than the shell, by the median over repeats: `weekday_ratio` for `deepseek-v4-flash-0731` (11 against 7).

## Cost

Tokens are summed over every step, so the arm that re-reads its context most pays most; `cached` is what the provider reported serving from its prompt cache.

| model | arm | tokens in | cached | tokens out | wall | cost (USD) | ran out of steps |
|---|---|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4.5` | `bench` | 278,142 | 0 | 17,709 | 265s | 0.37 | 0 |
| `anthropic/claude-haiku-4.5` | `shell` | 412,375 | 0 | 27,421 | 470s | 0.55 | 0 |
| `deepseek/deepseek-v4-flash-0731` | `bench` | 540,181 | 340,355 | 61,375 | 731s | 0.06 | 0 |
| `deepseek/deepseek-v4-flash-0731` | `shell` | 668,964 | 354,721 | 50,689 | 781s | 0.07 | 0 |
| `google/gemini-3.8-flash` | `bench` | 409,649 | 7,904 | 51,924 | 606s | 0.50 | 0 |
| `google/gemini-3.8-flash` | `shell` | 916,938 | 242,850 | 64,066 | 1185s | 0.76 | 0 |
| `openai/gpt-5-mini` | `bench` | 155,203 | 57,472 | 27,975 | 348s | 0.08 | 0 |
| `openai/gpt-5-mini` | `shell` | 222,957 | 56,064 | 34,677 | 616s | 0.11 | 0 |

## Per task

Steps — the median over repeats, with the least and most — and how many answers matched the ground truth.

| task | `claude-haiku-4.5` bench | `claude-haiku-4.5` shell | `deepseek-v4-flash-0731` bench | `deepseek-v4-flash-0731` shell | `gemini-3.8-flash` bench | `gemini-3.8-flash` shell | `gpt-5-mini` bench | `gpt-5-mini` shell |
|---|---|---|---|---|---|---|---|---|
| `total_mwh` | 3 (3) / 3 of 3 | 6 (6) / 3 of 3 | 3 (3) / 3 of 3 | 5 (5–10) / 3 of 3 | 8 (5–11) / 3 of 3 | 24 (15–25) / 3 of 3 | 4 (3–4) / 3 of 3 | 7 (5–7) / 1 of 3 |
| `top_meter` | 3 (3) / 3 of 3 | 5 (5) / 3 of 3 | 3 (3) / 3 of 3 | 5 (5–6) / 3 of 3 | 4 (3–4) / 3 of 3 | 8 (7–9) / 3 of 3 | 3 (3–4) / 3 of 3 | 6 (4–9) / 3 of 3 |
| `dead_meter` | 3 (3) / 3 of 3 | 5 (5) / 3 of 3 | 3 (3) / 3 of 3 | 5 (4–5) / 3 of 3 | 3 (3) / 3 of 3 | 6 (5–8) / 3 of 3 | 3 (2–3) / 3 of 3 | 4 (4) / 3 of 3 |
| `last_to_start` | 4 (4) / 3 of 3 | 7 (7) / 3 of 3 | 4 (3–4) / 3 of 3 | 7 (7) / 3 of 3 | 4 (3–4) / 3 of 3 | 10 (9–13) / 3 of 3 | 4 (4) / 3 of 3 | 7 (7–8) / 3 of 3 |
| `stopped_date` | 4 (4) / 3 of 3 | 8 (8–13) / 3 of 3 | 4 (3–4) / 3 of 3 | 9 (5–14) / 3 of 3 | 4 (4–5) / 3 of 3 | 8 (7–8) / 3 of 3 | 3 (3) / 3 of 3 | 5 (4–6) / 3 of 3 |
| `clock_forward` | 4 (4) / 3 of 3 | 5 (5) / 3 of 3 | 3 (3) / 3 of 3 | 6 (6–9) / 3 of 3 | 3 (3) / 3 of 3 | 8 (6–8) / 3 of 3 | 2 (2–5) / 3 of 3 | 6 (5–7) / 3 of 3 |
| `clock_back` | 4 (4) / 3 of 3 | 7 (7–8) / 3 of 3 | 7 (6–7) / 3 of 3 | 15 (10–17) / 3 of 3 | 4 (3–4) / 3 of 3 | 6 (6) / 3 of 3 | 3 (2–3) / 3 of 3 | 6 (5–6) / 3 of 3 |
| `peak_time` | 3 (3) / 3 of 3 | 4 (4) / 3 of 3 | 3 (3) / 3 of 3 | 5 (4–5) / 3 of 3 | 5 (4–6) / 3 of 3 | 8 (7–9) / 3 of 3 | 4 (3–4) / 3 of 3 | 4 (4) / 3 of 3 |
| `busiest_month` | 4 (4) / 3 of 3 | 4 (4) / 3 of 3 | 3 (3) / 3 of 3 | 6 (6–7) / 3 of 3 | 4 (4–5) / 3 of 3 | 7 (7) / 3 of 3 | 3 (3) / 3 of 3 | 6 (6–7) / 3 of 3 |
| `weekday_ratio` | 4 (4) / 0 of 3 | 6 (5–6) / 0 of 3 | 11 (5–12) / 3 of 3 | 7 (4–8) / 3 of 3 | 8 (6–9) / 3 of 3 | 10 (8–12) / 3 of 3 | 3 (3) / 3 of 3 | 6 (5–7) / 3 of 3 |
| `steadiest` | 4 (4) / 3 of 3 | 6 (6) / 3 of 3 | 3 (3) / 3 of 3 | 5 (5–6) / 3 of 3 | 4 (4–5) / 3 of 3 | 8 (7–9) / 3 of 3 | 3 (3–5) / 3 of 3 | 4 (4–5) / 3 of 3 |
| `top_day` | 3 (3) / 3 of 3 | 5 (5) / 3 of 3 | 3 (3) / 3 of 3 | 7 (5–8) / 3 of 3 | 6 (6–8) / 3 of 3 | 10 (9–10) / 3 of 3 | 3 (3–4) / 3 of 3 | 6 (4–7) / 3 of 3 |
| `median_load` | 3 (3) / 3 of 3 | 5 (5) / 3 of 3 | 3 (3) / 3 of 3 | 6 (4–7) / 3 of 3 | 5 (4–5) / 3 of 3 | 10 (9–11) / 3 of 3 | 3 (3) / 3 of 3 | 5 (4–5) / 3 of 3 |

## Ground truth

One expert SQL answer per task (`eval/tasks.toml`), evaluated against the same data every arm sees — not a consensus between models. A number is right within its task's tolerance and no further; a name or date must be exactly the one, and a hedge between two candidates is wrong.

| task | answer | tolerance | the trap |
|---|---|---|---|
| `total_mwh` | `15699.917` | 0.3 | kw is the average power over 15 minutes: energy is kw / 4 in kWh. A plain sum is 4x too large. |
| `top_meter` | `MT_008` | exact | none — a plumbing check. |
| `dead_meter` | `MT_223` | exact | It has a row for every interval, all zero: missing data here is zeros, not missing rows. |
| `last_to_start` | `MT_347` | exact | The latest is a meter back from a pause, not a new one; the meter that never reads has no first reading at all. |
| `stopped_date` | `2014-12-18` | exact | Earlier runs of zeros — 58 days from New Year, 57 in the autumn, 12 in December — all end; only the last, 13 days, never does. |
| `clock_forward` | `2014-03-30` | exact | The hour the clocks went forward: an artefact of recording, not an outage. |
| `clock_back` | `2014-10-26` | exact | The hour the clocks went back holds two hours' consumption. It is not a peak in demand. |
| `peak_time` | `2014-02-04 19:15` | exact | The fleet's load is the sum across meters per interval, not the largest single reading. |
| `busiest_month` | `12` | 0 | none, beyond the units. |
| `weekday_ratio` | `0.9873` | 0.0005 | Near 1: this meter barely notices the weekend, so an inverted ratio is close but wrong. |
| `steadiest` | `MT_002` | exact | The top two are within 1% of each other, and the meter that never reads divides zero by zero. |
| `top_day` | `2014-12-12` | exact | The top day leads the next, 2014-12-30, by under 1%; the day of the single peak reading, 2014-02-04, is third. The answer is the same whether a timestamp starts or ends its interval. |
| `median_load` | `1813.729` | 0.1 | The mean (1792.2) is not the median, and neither is the median of single readings (44.7). |

## Reading this

**Steps are the measurement.** Correctness is recorded and *not claimed*: thirteen tasks cannot support a statement about accuracy, and a miss is one model's bad afternoon rather than evidence about a surface.

**The sample is small.** Four models, thirteen tasks, one dataset, one machine. Repeats are at temperature 0 as requested, and some models then repeat themselves step for step, so for those three runs are closer to one; a reasoning model may not honour the temperature at all. The spread per task is above.

**What differs between the arms is the surface.** The same model, the same data, the same engine — DuckDB — and guidance of the same kind about each surface: a catalogue, tables that persist between calls and results that come back bounded, against a command line where each `duckdb` starts empty unless told to keep a file. The surface includes what `describe` reports about the data's shape, which no one shell command does; the split above shows how much of the difference that is.

**What this does not measure**: other models, other data, larger data, or sessions where scratch tables pay off across questions. Every task here starts fresh.

## The data

Data: Artur Trindade, *ElectricityLoadDiagrams20112014*, UCI Machine Learning Repository (2015), https://doi.org/10.24432/C58C86, licensed CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/). Modified: 24 of the 370 clients, the intervals stamped in 2014, one row per meter and interval; excerpts appear in the committed transcripts.
