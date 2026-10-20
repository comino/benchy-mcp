#!/usr/bin/env python3
"""The ingest benchmark's data: one synthetic meter table, written in every format benchy reads.

    uv run --no-project --with pyarrow==26.0.0 python bench/gen_data.py OUT_DIR ROWS

`bench/ingest.py` runs this itself when a file is missing. Row `i` is the same in every file:

    ts     timestamp, microseconds, UTC: 2026-01-01T00:00:00Z + i minutes
    meter  string: 'm000' … 'm499' (i % 500), the series key
    kw     double: (i % 997) * 1.5

so a load of any of them makes the same table (REQ-15), and only the way in differs. Each format
is written once per row count, into OUT_DIR/<rows>/: readings.csv, readings.parquet,
readings.ndjson (each in a folder of its own, for `benchy-mcp <dir>`) and readings.arrows (an
Arrow IPC stream, for the exec connector and the host's attach).

`series` writes readings-series.arrows: the same meters and readings, but every meter reading at
each minute, `ts` = 2026-01-01T00:00:00Z + (i // 500) minutes, so the times repeat and the
profile cuts the table into 500 series, as a table of many meters is cut.

`intkey` writes readings-intkey.arrows: `series`' rows with `meter` an int64, 1000 + i % 500,
so the profile tallies an integer key (R3-10).

`mixed` writes readings-mixed.arrows: 150 meters 'm000' … 'm149' at three cadences, as a table of
mixed sensors has them (#50): m000–m049 every minute, m050–m099 every 2 minutes and m100–m149
every 3, every one complete, in time order; `kw` as above.

`lz4` writes readings-lz4.arrows: the rows of readings.arrows, each batch's buffers compressed
with LZ4 as Arrow IPC allows, which is how ClickHouse's `ArrowStream` sends them by default.
"""
import os
import sys

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.csv as pacsv
import pyarrow.parquet as pq

CHUNK = 1_000_000
BASE_US = 1767225600 * 1_000_000  # 2026-01-01T00:00:00Z


def chunk(start, n, series=False, intkey=False):
    i = pa.array(range(start, start + n), pa.int64())
    if series:
        i = pc.divide(i, 500)
    ts = pc.add(pc.multiply(i, 60_000_000), BASE_US).cast(pa.timestamp("us", tz="UTC"))
    if intkey:
        meter = pa.array([1000 + k % 500 for k in range(start, start + n)], pa.int64())
    else:
        meter = pa.array([f"m{k % 500:03d}" for k in range(start, start + n)], pa.string())
    kw = pa.array([(k % 997) * 1.5 for k in range(start, start + n)], pa.float64())
    return pa.table({"ts": ts, "meter": meter, "kw": kw})


# `mixed`'s six minutes, which repeat: each row's minute in them and its meter.
MIXED = [(m, s) for m in range(6) for s in range(150) if m % (1 + s // 50) == 0]


def mixed_chunk(start, n):
    rows = range(start, start + n)
    minute = [i // len(MIXED) * 6 + MIXED[i % len(MIXED)][0] for i in rows]
    ts = pc.add(pc.multiply(pa.array(minute, pa.int64()), 60_000_000), BASE_US).cast(
        pa.timestamp("us", tz="UTC"))
    meter = pa.array([f"m{MIXED[i % len(MIXED)][1]:03d}" for i in rows], pa.string())
    kw = pa.array([(k % 997) * 1.5 for k in rows], pa.float64())
    return pa.table({"ts": ts, "meter": meter, "kw": kw})


def main(out, rows):
    d = os.path.join(out, str(rows))
    paths = {
        "csv": os.path.join(d, "csv", "readings.csv"),
        "parquet": os.path.join(d, "parquet", "readings.parquet"),
        "ndjson": os.path.join(d, "ndjson", "readings.ndjson"),
        "arrows": os.path.join(d, "readings.arrows"),
        "series": os.path.join(d, "readings-series.arrows"),
        "lz4": os.path.join(d, "readings-lz4.arrows"),
        "intkey": os.path.join(d, "readings-intkey.arrows"),
        "mixed": os.path.join(d, "readings-mixed.arrows"),
    }
    want = [k for k in sys.argv[3:]] or list(paths)
    todo = {k: p for k, p in paths.items() if k in want and not os.path.exists(p)}
    if not todo:
        return
    for p in todo.values():
        os.makedirs(os.path.dirname(p), exist_ok=True)
    schema = chunk(0, 1).schema
    tmp = {k: p + ".part" for k, p in todo.items()}
    csv_w = pacsv.CSVWriter(tmp["csv"], schema) if "csv" in tmp else None
    pq_w = pq.ParquetWriter(tmp["parquet"], schema) if "parquet" in tmp else None
    ipc_sink = pa.OSFile(tmp["arrows"], "wb") if "arrows" in tmp else None
    ipc_w = pa.ipc.new_stream(ipc_sink, schema) if ipc_sink else None
    js = open(tmp["ndjson"], "w") if "ndjson" in tmp else None
    series_sink = pa.OSFile(tmp["series"], "wb") if "series" in tmp else None
    series_w = pa.ipc.new_stream(series_sink, schema) if series_sink else None
    int_sink = pa.OSFile(tmp["intkey"], "wb") if "intkey" in tmp else None
    int_w = pa.ipc.new_stream(int_sink, chunk(0, 1, True, True).schema) if int_sink else None
    lz4_sink = pa.OSFile(tmp["lz4"], "wb") if "lz4" in tmp else None
    lz4_w = pa.ipc.new_stream(lz4_sink, schema, options=pa.ipc.IpcWriteOptions(
        compression="lz4")) if lz4_sink else None
    mixed_sink = pa.OSFile(tmp["mixed"], "wb") if "mixed" in tmp else None
    mixed_w = pa.ipc.new_stream(mixed_sink, schema) if mixed_sink else None
    for start in range(0, rows, CHUNK):
        t = chunk(start, min(CHUNK, rows - start))
        if csv_w:
            csv_w.write_table(t)
        if pq_w:
            pq_w.write_table(t)
        if ipc_w:
            # 64 Ki-row batches, as a connector streaming its result would send them.
            for b in t.to_batches(max_chunksize=65_536):
                ipc_w.write_batch(b)
        if lz4_w:
            for b in t.to_batches(max_chunksize=65_536):
                lz4_w.write_batch(b)
        if js:
            ts = t.column("ts").cast(pa.string()).to_pylist()
            for a, m, k in zip(ts, t.column("meter").to_pylist(), t.column("kw").to_pylist()):
                js.write(f'{{"ts":"{a.replace(" ", "T")}","meter":"{m}","kw":{k!r}}}\n')
        if series_w:
            for b in chunk(start, min(CHUNK, rows - start), series=True).to_batches(
                    max_chunksize=65_536):
                series_w.write_batch(b)
        if int_w:
            for b in chunk(start, min(CHUNK, rows - start), True, True).to_batches(
                    max_chunksize=65_536):
                int_w.write_batch(b)
        if mixed_w:
            for b in mixed_chunk(start, min(CHUNK, rows - start)).to_batches(max_chunksize=65_536):
                mixed_w.write_batch(b)
    for w in (csv_w, pq_w, ipc_w, series_w, lz4_w, int_w, mixed_w):
        if w:
            w.close()
    for sink in (ipc_sink, series_sink, lz4_sink, int_sink, mixed_sink):
        if sink:
            sink.close()
    if js:
        js.close()
    for k in todo:
        os.replace(tmp[k], todo[k])


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]))
