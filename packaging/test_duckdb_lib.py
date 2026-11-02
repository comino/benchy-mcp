#!/usr/bin/env python3
"""Checks for packaging/duckdb_lib.py, with no network: `python3 packaging/test_duckdb_lib.py`.

The library a build links and a bundle ships is the one DuckDB released, byte for byte: a
download that does not match its pin is refused before anything is extracted, a cache that was
tampered with is put right or refused, and the bundle takes the pinned version's directory, not
whichever one a directory listing returns first.
"""
import hashlib
import io
import os
import pathlib
import sys
import tarfile
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import duckdb_lib  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
TRIPLE = "x86_64-unknown-linux-gnu"
LIB = b"\x7fELF the library DuckDB released"
HEADER = b"/* duckdb.h */"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def archive(lib=LIB, extra=None, link=None):
    """A DuckDB 2.0 `duckdb-shared-libs-*.tar.gz`; `link` makes libduckdb.so that kind of link."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as t:
        def add(name, data=b"", kind=tarfile.REGTYPE, target=""):
            info = tarfile.TarInfo(name)
            info.type, info.linkname, info.size = kind, target, len(data)
            t.addfile(info, io.BytesIO(data) if kind == tarfile.REGTYPE else None)
        if extra:
            add(extra)
        if link:
            add("libduckdb.so", kind=link, target="/etc/passwd")
        else:
            add("libduckdb.so", lib)
        add("duckdb.h", HEADER)
        add("duckdb_extension.h", HEADER)
        add("duckdb_v2.h", HEADER)
    return buf.getvalue()


GOOD = archive()
LICENSE = b"Permission is hereby granted, free of charge"


def pins(version="2.0.0", packed=GOOD):
    return duckdb_lib.parse_pins(
        f"# a comment\n{sha(packed)}  v{version}/duckdb-shared-libs-linux-amd64.tar.gz\n"
        f"{sha(LICENSE)}  v{version}/LICENSE\n"
    )


def serving(files):
    """A download that serves `files` by URL basename and records what was asked for."""
    asked = []

    def download(url, dest):
        asked.append(url)
        pathlib.Path(dest).write_bytes(files[url.rsplit("/", 1)[1]])

    download.asked = asked
    return download


def refused(f, *args, **kw):
    try:
        f(*args, **kw)
    except duckdb_lib.Refused as e:
        return str(e)
    raise AssertionError(f"{f.__name__} did not refuse")


def check_version_comes_from_the_lock():
    lock = '[[package]]\nname = "libduckdb-sys"\nversion = "1.20000.1"\n'
    assert duckdb_lib.version_from_lock(lock) == "2.0.0"
    assert duckdb_lib.version_from_lock(lock.replace("20000", "10505")) == "1.5.5"
    assert duckdb_lib.version_from_lock(lock.replace("20000", "11203")) == "1.12.3"
    # The real lock's DuckDB has a pin for every archive a release builds, and for its licence:
    # bumping the duckdb crate without pinning the new release fails here first.
    real = duckdb_lib.version_from_lock((REPO / "Cargo.lock").read_text())
    pinned = duckdb_lib.parse_pins((REPO / "packaging/duckdb.sha256").read_text())
    for name in ("duckdb-shared-libs-linux-amd64.tar.gz", "duckdb-shared-libs-linux-arm64.tar.gz",
                 "duckdb-shared-libs-osx-universal.tar.gz", "LICENSE"):
        assert (real, name) in pinned, f"no pin for DuckDB {real}'s {name}"
        assert len(pinned[(real, name)]) == 64
    for triple in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
                   "aarch64-apple-darwin", "x86_64-apple-darwin"):
        assert duckdb_lib.ARCHIVES[triple][0] in {n for _, n in pinned}


def check_a_tampered_download_is_refused_and_nothing_extracted():
    with tempfile.TemporaryDirectory() as t:
        target = pathlib.Path(t)
        download = serving({"duckdb-shared-libs-linux-amd64.tar.gz": archive(b"\x7fELF something else")})
        msg = refused(duckdb_lib.fetch, TRIPLE, "2.0.0", pins(), target, download)
        assert "duckdb-shared-libs-linux-amd64.tar.gz" in msg and sha(GOOD) in msg, msg
        assert download.asked == [
            "https://github.com/duckdb/duckdb/releases/download/v2.0.0/duckdb-shared-libs-linux-amd64.tar.gz"
        ]
        # Not left where libduckdb-sys would reuse it, and nothing for it to link.
        assert not [f for f in target.rglob("*") if f.is_file()]


def check_a_good_download_lands_where_libduckdb_sys_reuses_it():
    with tempfile.TemporaryDirectory() as t:
        target = pathlib.Path(t)
        got = duckdb_lib.fetch(TRIPLE, "2.0.0", pins(), target, serving({"duckdb-shared-libs-linux-amd64.tar.gz": GOOD}))
        assert got == target / "duckdb-download" / TRIPLE / "v2.0.0"
        assert (got / "duckdb-shared-libs-linux-amd64.tar.gz").read_bytes() == GOOD
        assert (got / "libduckdb.so").read_bytes() == LIB
        assert (got / "duckdb.h").read_bytes() == HEADER
        # A second run downloads nothing.
        again = serving({})
        assert duckdb_lib.fetch(TRIPLE, "2.0.0", pins(), target, again) == got and not again.asked


def check_a_tampered_cache_is_dropped_or_put_right():
    with tempfile.TemporaryDirectory() as t:
        target = pathlib.Path(t)
        cache = target / "duckdb-download" / TRIPLE / "v2.0.0"
        cache.mkdir(parents=True)
        # A restored build cache holds an archive that is not DuckDB's: it is removed, not reused,
        # and the pinned one fetched in its place.
        (cache / "duckdb-shared-libs-linux-amd64.tar.gz").write_bytes(archive(b"planted"))
        # libduckdb-sys reuses an extracted library without looking at the archive at all.
        (cache / "libduckdb.so").write_bytes(b"planted")
        download = serving({"duckdb-shared-libs-linux-amd64.tar.gz": GOOD})
        duckdb_lib.fetch(TRIPLE, "2.0.0", pins(), target, download)
        assert len(download.asked) == 1
        assert (cache / "libduckdb.so").read_bytes() == LIB


def check_what_is_shipped_is_checked_again():
    with tempfile.TemporaryDirectory() as t:
        target = pathlib.Path(t)
        got = duckdb_lib.fetch(TRIPLE, "2.0.0", pins(), target, serving({"duckdb-shared-libs-linux-amd64.tar.gz": GOOD}))
        # libduckdb-sys copies the library into its OUT_DIR, which cargo puts on the loader path
        # of the test binaries; that copy is checked too.
        out = target / TRIPLE / "release" / "build" / "libduckdb-sys-0123456789abcdef" / "out"
        out.mkdir(parents=True)
        (out / "libduckdb.so").write_bytes(LIB)
        assert duckdb_lib.check(TRIPLE, "2.0.0", pins(), target) == got
        (out / "libduckdb.so").write_bytes(b"replaced after the build")
        assert "out/libduckdb.so" in refused(duckdb_lib.check, TRIPLE, "2.0.0", pins(), target)
        (out / "libduckdb.so").write_bytes(LIB)
        # DuckDB 1.x's libduckdb-sys copied it into deps/, also on the loader path: a stale copy
        # left there by an older build is refused, not loaded by the tests instead.
        deps = target / TRIPLE / "release" / "deps"
        deps.mkdir(parents=True)
        (deps / "libduckdb.so").write_bytes(LIB)
        assert duckdb_lib.check(TRIPLE, "2.0.0", pins(), target) == got
        # A second DuckDB version in the cache is not picked by accident.
        other = target / "duckdb-download" / TRIPLE / "v1.5.5"
        other.mkdir()
        (other / "libduckdb.so").write_bytes(b"old")
        assert duckdb_lib.check(TRIPLE, "2.0.0", pins(), target) == got
        (deps / "libduckdb.so").write_bytes(b"replaced after the build")
        assert "deps/libduckdb.so" in refused(duckdb_lib.check, TRIPLE, "2.0.0", pins(), target)
        (deps / "libduckdb.so").write_bytes(LIB)
        (got / "libduckdb.so").write_bytes(b"replaced after the fetch")
        refused(duckdb_lib.check, TRIPLE, "2.0.0", pins(), target)
        (got / "libduckdb.so").write_bytes(LIB)
        # Another archive holding the very same files is still not the release that was pinned.
        (got / "duckdb-shared-libs-linux-amd64.tar.gz").write_bytes(archive(extra="and one more"))
        refused(duckdb_lib.check, TRIPLE, "2.0.0", pins(), target)
        # A version with no pin is refused, never fetched unchecked.
        assert "2.1.0" in refused(duckdb_lib.fetch, TRIPLE, "2.1.0", pins(), target, serving({}))


def check_a_link_in_the_archive_is_refused():
    # A tar entry can be a symlink or a hard link, which a zip could not: never followed, and
    # nothing extracted for libduckdb-sys to link.
    for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
        linked = archive(link=kind)
        with tempfile.TemporaryDirectory() as t:
            target = pathlib.Path(t)
            download = serving({"duckdb-shared-libs-linux-amd64.tar.gz": linked})
            msg = refused(duckdb_lib.fetch, TRIPLE, "2.0.0", pins(packed=linked), target, download)
            assert "libduckdb.so" in msg, msg
            assert not (target / "duckdb-download" / TRIPLE / "v2.0.0" / "libduckdb.so").exists()


def check_the_licence_is_pinned_too():
    with tempfile.TemporaryDirectory() as t:
        dest = pathlib.Path(t) / "LICENSE-DuckDB"
        refused(duckdb_lib.license, "2.0.0", pins(), dest, serving({"LICENSE": LICENSE + b" or not"}))
        assert os.listdir(t) == []
        download = serving({"LICENSE": LICENSE})
        duckdb_lib.license("2.0.0", pins(), dest, download)
        assert dest.read_bytes() == LICENSE
        assert download.asked == ["https://raw.githubusercontent.com/duckdb/duckdb/v2.0.0/LICENSE"]


if __name__ == "__main__":
    checks = [v for k, v in sorted(globals().items()) if k.startswith("check_")]
    for c in checks:
        c()
        print(f"ok  {c.__name__}")
    print(f"test_duckdb_lib.py: {len(checks)} checks passed")
