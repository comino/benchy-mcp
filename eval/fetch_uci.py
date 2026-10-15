#!/usr/bin/env python3
"""Fetch the public dataset the eval runs on, and cut out the part its tasks use. Deterministic.

    python3 eval/fetch_uci.py eval/uci            # downloads 261 MB, keeps 36 MB, deletes the rest
    python3 eval/fetch_uci.py eval/uci --zip F    # from a copy of the archive you already have

The data is ElectricityLoadDiagrams20112014 (Artur Trindade, UCI Machine Learning Repository,
2015, https://doi.org/10.24432/C58C86), licensed CC BY 4.0: the power 370 Portuguese electricity
clients drew in every 15-minute interval from 2011 to 2014, in kW.

It is real meter data, with what real meter data has, and none of it planted:

- A reading is a meter's average power over a 15-minute interval, in kW: energy in kWh is kW / 4,
  and a naive sum of kW is off by exactly 4x. The source does not say whether a timestamp starts
  or ends its interval, and the data points both ways — its first reading is stamped 00:15, while
  the hour lost in March is stamped 01:00 to 01:45 — so no task depends on it.
- Clients join late: several meters read zero until partway through 2014.
- Clients leave, or pause: one meter here stops before the year is out, after gaps of weeks;
  another read zero all year.
- The clock changes. On the day in March that loses an hour, the hour's readings are zero; on the
  day in October that gains one, one hour's readings hold two hours of consumption.

The cut is the 35,040 intervals stamped in 2014, so "in 2014" means every row, whichever way the
timestamps are read, for 24 meters: the first 20 that read all year, two that start in 2014
(MT_106, MT_109), one that pauses and then stops (MT_347) and one that never reads (MT_223).
One file, `readings.csv`, with columns `ts,meter,kw`, one row per meter and interval.

Data: Artur Trindade, ElectricityLoadDiagrams20112014, UCI Machine Learning Repository (2015),
https://doi.org/10.24432/C58C86, CC BY 4.0 (https://creativecommons.org/licenses/by/4.0/).
Modified: 24 of the 370 clients, 2014 only, one row per meter and interval.
"""
import argparse
import hashlib
import io
import os
import sys
import urllib.request
import zipfile

URL = "https://archive.ics.uci.edu/static/public/321/electricityloaddiagrams20112014.zip"
ZIP_SHA256 = "f6c4d0e0df12ecdb9ea008dd6eef3518adb52c559d04a9bac2e1b81dcfc8d4e1"
MEMBER = "LD2011_2014.txt"
METERS = [
    "MT_002", "MT_004", "MT_005", "MT_006", "MT_007", "MT_008", "MT_009", "MT_010", "MT_011",
    "MT_012", "MT_013", "MT_014", "MT_015", "MT_016", "MT_017", "MT_018", "MT_019", "MT_020",
    "MT_021", "MT_022", "MT_106", "MT_109", "MT_223", "MT_347",
]
FIRST, LAST = "2014-01-01 00:00:00", "2014-12-31 23:45:00"
# What the cut must come to, so a run can be compared with a published one.
READINGS_SHA256 = "fa23f39e3cf1533ec878e2394d7e74f7f3295051e909d49a839c66d6fdcb588e"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def download(dest):
    print(f"downloading {URL} ...", file=sys.stderr)
    with urllib.request.urlopen(URL, timeout=600) as r, open(dest, "wb") as out:
        while chunk := r.read(1 << 20):
            out.write(chunk)


def cut(archive, out_dir):
    with zipfile.ZipFile(archive) as z:
        src = io.TextIOWrapper(z.open(MEMBER), encoding="utf-8")
        header = [h.strip('"') for h in src.readline().rstrip("\n").split(";")]
        cols = [header.index(m) for m in METERS]
        rows = 0
        path = os.path.join(out_dir, "readings.csv")
        with open(path, "w", newline="\n") as out:
            out.write("ts,meter,kw\n")
            for line in src:
                parts = line.rstrip("\n").split(";")
                ts = parts[0].strip('"')
                if ts < FIRST:
                    continue
                if ts > LAST:
                    break
                for m, i in zip(METERS, cols):
                    # The source writes decimals with a comma.
                    out.write(f"{ts},{m},{parts[i].replace(',', '.')}\n")
                    rows += 1
    return path, rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out", help="directory to write readings.csv into")
    ap.add_argument("--zip", help="an already downloaded copy of the archive")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    archive = a.zip or os.path.join(a.out, "download.zip")
    if not a.zip:
        download(archive)
    got = sha256(archive)
    if got != ZIP_SHA256:
        if not a.zip:
            os.remove(archive)
        raise SystemExit(f"the archive's SHA-256 is {got}, expected {ZIP_SHA256}: not the "
                         "dataset the tasks were written against")
    path, rows = cut(archive, a.out)
    if not a.zip:
        os.remove(archive)
    expected = len(METERS) * 35040
    if rows != expected:
        raise SystemExit(f"wrote {rows} rows, expected {expected}")
    cut_sha = sha256(path)
    if cut_sha != READINGS_SHA256:
        raise SystemExit(f"{path}: SHA-256 {cut_sha}, expected {READINGS_SHA256}")
    print(f"{path}: {rows} rows, sha256 {cut_sha}")


if __name__ == "__main__":
    main()
