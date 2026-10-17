#!/usr/bin/env python3
"""Fetch DuckDB's release library checked against its pin, before cargo links it.

libduckdb-sys's build script downloads `libduckdb-<platform>.zip` with a plain GET and no
checksum, and a bundle and an image ship what it unpacked. This puts the pinned, checked zip
where that build script looks first (target/duckdb-download/<triple>/<version>/), so it reuses
the file instead of downloading, and unpacks the library itself from the checked zip, because the
build script reuses an unpacked library without looking at the zip at all.

    python3 packaging/duckdb_lib.py fetch [<triple>]     # before `cargo build`; prints the dir
    python3 packaging/duckdb_lib.py check [<triple>]     # after it: what will ship still matches
    python3 packaging/duckdb_lib.py license <file>       # DuckDB's LICENSE for that release, checked

The version is the one Cargo.lock's libduckdb-sys encodes; the SHA-256 of each file is in
packaging/duckdb.sha256. CARGO_TARGET_DIR is honoured as cargo and libduckdb-sys honour it.
"""

import hashlib
import os
import pathlib
import re
import subprocess
import sys
import zipfile

REPO = pathlib.Path(__file__).resolve().parent.parent
PINS = REPO / "packaging" / "duckdb.sha256"
RELEASE = "https://github.com/duckdb/duckdb/releases/download/v{version}/{name}"
LICENSE = "https://raw.githubusercontent.com/duckdb/duckdb/v{version}/LICENSE"

# The archive libduckdb-sys picks for each target, and the library in it. The same table as its
# build script's LibduckdbArchive::for_target.
ARCHIVES = {
    "x86_64-unknown-linux-gnu": ("libduckdb-linux-amd64.zip", "libduckdb.so"),
    "aarch64-unknown-linux-gnu": ("libduckdb-linux-arm64.zip", "libduckdb.so"),
    "aarch64-apple-darwin": ("libduckdb-osx-universal.zip", "libduckdb.dylib"),
    "x86_64-apple-darwin": ("libduckdb-osx-universal.zip", "libduckdb.dylib"),
}
# Unpacked by name, so an entry's path in the zip never decides where it is written. The 75 MB
# static library is neither linked nor shipped.
HEADERS = ("duckdb.h", "duckdb_extension.h")


class Refused(Exception):
    pass


def version_from_lock(lock):
    """DuckDB's version from libduckdb-sys's: 1.MMmmpp.x is DuckDB MM.mm.pp, as its build script
    decodes it (1.10505.0 is DuckDB 1.5.5)."""
    m = re.search(r'name = "libduckdb-sys"\nversion = "1\.(\d+)\.\d+"', lock)
    if not m:
        raise Refused("Cargo.lock has no libduckdb-sys 1.x")
    n = int(m.group(1))
    return f"{n // 10000}.{n // 100 % 100}.{n % 100}"


def parse_pins(text):
    """`<sha256>  v<version>/<file>` lines, as sha256sum writes them; # starts a comment."""
    pins = {}
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        digest, path = line.split(None, 1)
        version, name = path.split("/", 1)
        pins[(version.removeprefix("v"), name)] = digest.lower()
    return pins


def pinned(pins, version, name):
    try:
        return pins[(version, name)]
    except KeyError:
        raise Refused(
            f"no SHA-256 pinned for DuckDB {version}'s {name} in packaging/duckdb.sha256"
        ) from None


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def curl(url, dest):
    subprocess.run(["curl", "-fsSL", "--retry", "3", "-o", str(dest), url], check=True)


def cache_dir(target_dir, triple, version):
    return pathlib.Path(target_dir) / "duckdb-download" / triple / version


def members(triple):
    return (ARCHIVES[triple][1],) + HEADERS


def archive_for(triple):
    if triple not in ARCHIVES:
        raise Refused(f"no DuckDB release library for {triple}")
    return ARCHIVES[triple][0]


def fetch(triple, version, pins, target_dir, download=curl):
    name = archive_for(triple)
    want = pinned(pins, version, name)
    cache = cache_dir(target_dir, triple, version)
    cache.mkdir(parents=True, exist_ok=True)
    zip_path = cache / name
    if zip_path.exists() and sha256(zip_path) != want:
        # A restored cache, or an unchecked download from before this script: never reused.
        print(f"duckdb_lib: {zip_path} is not the pinned file; fetching it again", file=sys.stderr)
        zip_path.unlink()
    if not zip_path.exists():
        part = cache / (name + ".part")
        download(RELEASE.format(version=version, name=name), part)
        got = sha256(part)
        if got != want:
            part.unlink()
            raise Refused(f"{name} from DuckDB {version} has SHA-256 {got}, pinned {want}")
        part.rename(zip_path)
    with zipfile.ZipFile(zip_path) as z:
        for member in members(triple):
            (cache / member).write_bytes(z.read(member))
    return cache


def check(triple, version, pins, target_dir):
    """Refuse unless the zip, what was unpacked from it and cargo's copies of the library are
    exactly the pinned release."""
    name = archive_for(triple)
    want = pinned(pins, version, name)
    cache = cache_dir(target_dir, triple, version)
    zip_path = cache / name
    if not zip_path.is_file() or sha256(zip_path) != want:
        raise Refused(f"{zip_path} is missing or is not the pinned DuckDB {version} {name}")
    lib = ARCHIVES[triple][1]
    with zipfile.ZipFile(zip_path) as z:
        for member in members(triple):
            if (cache / member).read_bytes() != z.read(member):
                raise Refused(f"{cache / member} is not the one in {name}")
        released = z.read(lib)
    # libduckdb-sys copies the library into the profile's deps/, which the tests load it from.
    target_dir = pathlib.Path(target_dir)
    for copy in sorted(target_dir.glob(f"*/deps/{lib}")) + sorted(target_dir.glob(f"*/*/deps/{lib}")):
        if copy.read_bytes() != released:
            raise Refused(f"{copy.relative_to(target_dir)} is not the one in {name}")
    return cache


def license(version, pins, dest, download=curl):
    want = pinned(pins, version, "LICENSE")
    dest = pathlib.Path(dest)
    part = dest.with_name(dest.name + ".part")
    download(LICENSE.format(version=version), part)
    got = sha256(part)
    if got != want:
        part.unlink()
        raise Refused(f"DuckDB {version}'s LICENSE has SHA-256 {got}, pinned {want}")
    part.rename(dest)


def host_triple():
    out = subprocess.check_output(["rustc", "-vV"], text=True)
    return next(line.split(": ", 1)[1] for line in out.splitlines() if line.startswith("host: "))


def main(argv):
    if len(argv) not in (2, 3) or argv[1] not in ("fetch", "check", "license"):
        print(__doc__, file=sys.stderr)
        return 2
    version = version_from_lock((REPO / "Cargo.lock").read_text())
    pins = parse_pins(PINS.read_text())
    target_dir = pathlib.Path(os.environ.get("CARGO_TARGET_DIR") or REPO / "target").resolve()
    try:
        if argv[1] == "license":
            if len(argv) != 3:
                print(__doc__, file=sys.stderr)
                return 2
            license(version, pins, argv[2])
            print(argv[2])
            return 0
        triple = argv[2] if len(argv) == 3 else host_triple()
        run = fetch if argv[1] == "fetch" else check
        print(run(triple, version, pins, target_dir))
    except Refused as e:
        print(f"duckdb_lib: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
