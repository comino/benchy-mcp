# Eval — what the bench costs an agent

Rendered by `eval/report.py` from the raw results in `eval/results/`, which hold every run's answer, counts and transcript — tool output cut at 4,000 characters, with the machine's own paths and user name replaced by placeholders. **Regenerate it rather than citing it**: it describes these models, this data and these tasks.

```sh
cargo build --release
python3 eval/fetch_uci.py eval/uci                  # 261 MB download, 36 MB kept
python3 eval/test_eval.py                           # the scorer, and the jail holds
OPENROUTER_API_KEY=... python3 eval/run.py --fresh --duckdb /path/to/duckdb \
    --models anthropic/claude-haiku-4.5,openai/gpt-5-mini --repeat 3
python3 eval/report.py
```

The shell arm needs Linux 6.7 or later, for its jail, and the DuckDB CLI; the run refuses rather than measure a shell it cannot confine.

- **Data** — 24 real meters, every 15 minutes of 2014, 840,960 rows; its quirks left in (see the traps below, and the data's source at the end)
- **Tasks** — 11, in `eval/tasks.toml`, each run 3 times per model and arm; step cap 25
- **Arms** — `bench`: benchy-mcp over MCP, with its own instructions. `shell`: a POSIX shell with the DuckDB CLI in a directory holding the data, jailed by Landlock, with a paragraph of the same kind about its own surface — the DuckDB dialect, how to find the data, how to keep tables between calls; its output reaches the model as a terminal would show it, without the CLI's colour codes
- **Setup** — benchy-mcp 0.1.0; DuckDB CLI v1.5.5 (Variegata) d8cdaa33fd; Linux 6.8.0-138-generic aarch64; temperature 0 requested
- **Measured** — 2026-09-30

## Steps

Model calls until the answer, summed over every task and repeat — the measurement.

| model | `bench` steps | `shell` steps | `bench` correct | `shell` correct |
|---|---:|---:|---:|---:|
| `anthropic/claude-haiku-4.5` | 150 | 195 | 30/33 | 30/33 |
| `openai/gpt-5-mini` | 153 | 184 | 32/33 | 32/33 |

- `anthropic/claude-haiku-4.5`: the bench took **45 fewer steps** than the shell's 195 (23% fewer).
- `openai/gpt-5-mini`: the bench took **31 fewer steps** than the shell's 184 (17% fewer).

## Where the steps go

*Looking* is finding the data and its shape: benchy's `describe`, or a shell command that lists the directory or reads the data file without DuckDB — `ls`, `head readings.csv` — or runs only a `DESCRIBE`, a `SUMMARIZE` or a `SELECT * … LIMIT n` preview. *Computing* is every other tool call, reading a file the model wrote itself included. *Answering* is the call that ends a run. The last column counts tool outputs that were an error — a mistyped column, a quoting slip — each of which cost the call after it.

| model | arm | steps | looking | computing | answering | outputs that were an error |
|---|---|---:|---:|---:|---:|---:|
| `claude-haiku-4.5` | `bench` | 150 | 33 | 84 | 33 | 3 |
| `claude-haiku-4.5` | `shell` | 195 | 90 | 72 | 33 | 0 |
| `gpt-5-mini` | `bench` | 153 | 33 | 87 | 33 | 6 |
| `gpt-5-mini` | `shell` | 184 | 66 | 85 | 33 | 2 |

**Against the grain** — tasks where the bench took more steps than the shell, by the median over repeats: `stopped_date` for `claude-haiku-4.5` (9 against 7); `top_meter` for `gpt-5-mini` (7 against 5).

## Cost

Tokens are summed over every step, so the arm that re-reads its context most pays most; `cached` is what the provider reported serving from its prompt cache.

| model | arm | tokens in | cached | tokens out | wall | cost (USD) | ran out of steps |
|---|---|---:|---:|---:|---:|---:|---:|
| `anthropic/claude-haiku-4.5` | `bench` | 309,252 | 0 | 20,640 | 326s | 0.41 | 0 |
| `anthropic/claude-haiku-4.5` | `shell` | 352,808 | 0 | 23,832 | 404s | 0.47 | 0 |
| `openai/gpt-5-mini` | `bench` | 252,035 | 117,248 | 34,232 | 585s | 0.11 | 0 |
| `openai/gpt-5-mini` | `shell` | 237,356 | 109,952 | 32,898 | 685s | 0.10 | 0 |

## Per task

Steps — the median over repeats, with the least and most — and how many answers matched the ground truth.

| task | `claude-haiku-4.5` bench | `claude-haiku-4.5` shell | `gpt-5-mini` bench | `gpt-5-mini` shell |
|---|---|---|---|---|
| `total_mwh` | 5 (5) / 3 of 3 | 7 (6–8) / 3 of 3 | 4 (4–7) / 2 of 3 | 6 (6–7) / 2 of 3 |
| `top_meter` | 3 (3) / 3 of 3 | 5 (5) / 3 of 3 | 7 (4–8) / 3 of 3 | 5 (5) / 3 of 3 |
| `dead_meter` | 3 (3) / 3 of 3 | 5 (5) / 3 of 3 | 3 (3) / 3 of 3 | 4 (4–5) / 3 of 3 |
| `last_to_start` | 4 (4) / 3 of 3 | 7 (7) / 3 of 3 | 6 (3–6) / 3 of 3 | 7 (5–8) / 3 of 3 |
| `stopped_date` | 9 (9) / 3 of 3 | 7 (7) / 3 of 3 | 5 (5–6) / 3 of 3 | 6 (5–6) / 3 of 3 |
| `clock_forward` | 5 (5) / 3 of 3 | 6 (6) / 3 of 3 | 4 (4–5) / 3 of 3 | 5 (5–7) / 3 of 3 |
| `clock_back` | 6 (6) / 3 of 3 | 7 (7) / 3 of 3 | 5 (5–9) / 3 of 3 | 6 (5–12) / 3 of 3 |
| `peak_time` | 3 (3) / 3 of 3 | 4 (4) / 3 of 3 | 3 (3) / 3 of 3 | 5 (4–5) / 3 of 3 |
| `busiest_month` | 3 (3) / 3 of 3 | 4 (4) / 3 of 3 | 6 (6) / 3 of 3 | 6 (5–6) / 3 of 3 |
| `weekday_ratio` | 4 (4) / 0 of 3 | 7 (7) / 0 of 3 | 3 (3) / 3 of 3 | 5 (4–5) / 3 of 3 |
| `steadiest` | 5 (5) / 3 of 3 | 6 (6) / 3 of 3 | 4 (3–4) / 3 of 3 | 5 (5) / 3 of 3 |

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

## Reading this

**Steps are the measurement.** Correctness is recorded and *not claimed*: eleven tasks cannot support a statement about accuracy, and a miss is one model's bad afternoon rather than evidence about a surface.

**The sample is small.** Two models, eleven tasks, one dataset, one machine. Repeats are at temperature 0 as requested, and some models then repeat themselves step for step, so for those three runs are closer to one; a reasoning model may not honour the temperature at all. The spread per task is above.

**What differs between the arms is the surface.** The same model, the same data, the same engine — DuckDB — and guidance of the same kind about each surface: a catalogue, tables that persist between calls and results that come back bounded, against a command line where each `duckdb` starts empty unless told to keep a file.

**What this does not measure**: other models, other data, larger data, or sessions where scratch tables pay off across questions. Every task here starts fresh.

## The data

Data: Artur Trindade, *ElectricityLoadDiagrams20112014*, UCI Machine Learning Repository (2015), https://doi.org/10.24432/C58C86, licensed CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/). Modified: 24 of the 370 clients, the intervals stamped in 2014, one row per meter and interval; excerpts appear in the committed transcripts.
