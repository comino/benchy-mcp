# Results of 2026-09-30

The first measurement of the bench, kept so the history stays inspectable. `eval/report.py` reads
only `eval/results/*.json`, so these files no longer feed `eval/REPORT.md`; `REPORT.md` here is
the report as it was rendered from them.

- **Measured with** benchy-mcp as it was when `eval/` was added and these results committed,
  before the public history began: before `describe` reported the shape of a time series, before
  origin-blind types and before the polish of what a first-time agent meets. Each file's `setup` holds the binary's SHA-256.
- **Models** `anthropic/claude-haiku-4.5` and `openai/gpt-5-mini`.
- **Tasks** the first eleven of today's `eval/tasks.toml`; `top_day` and `median_load` came later.
  The ground truth of those eleven has not changed.
