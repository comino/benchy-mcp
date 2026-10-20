#!/usr/bin/env python3
"""Checks for packaging/compat.py and packaging/sbom.py: `python3 packaging/test_release_files.py`.

A release's compat.json says what the binary says of itself, and the oldest glibc its Linux
bundles need, measured; a release whose bundles are not the platforms the binary names, or a
binary that disagrees with compat.json, is refused. A bundle's SBOM names every crate the release
builds from Cargo.lock, scoped by whether it is in the binary, and DuckDB's library by the file
shipped and the pinned release file it came from. No network, no release build: the binary and
`readelf` are stood in for, and the crate graph is the real one, from `cargo metadata`.
"""
import hashlib
import io
import json
import os
import pathlib
import sys
import tarfile
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import compat  # noqa: E402
import duckdb_lib  # noqa: E402
import sbom  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
PLATFORMS = ["linux-x86_64", "linux-aarch64", "macos-arm64", "macos-x86_64"]
SAID = {"benchy": "9.9.9", "duckdb": "1.5.5", "host_protocols": [1, 2, 3],
        "platforms": PLATFORMS, "full_sandbox_min_landlock_abi": 4}


def needs(*versions):
    """`readelf -V` of a file needing these GLIBC_ versions, as readelf prints it."""
    lines = ["Version symbols section '.gnu.version' contains 3 entries:",
             "  000:   0 (*local*)       2 (GLIBC_9.99)",
             "Version needs section '.gnu.version_r' contains 2 entries:",
             "  000000: Version: 1  File: libc.so.6  Cnt: 3"]
    lines += [f"  0x0010:   Name: GLIBC_{v}  Flags: none  Version: 2" for v in versions]
    lines += ["  0x0020: Version: 1  File: libgcc_s.so.1  Cnt: 1",
              "  0x0030:   Name: GCC_3.0  Flags: none  Version: 3",
              "  0x0040:   Name: GLIBC_PRIVATE  Flags: none  Version: 4"]
    return "\n".join(lines)


def bundle(dist, platform, files):
    """A benchy-mcp-<platform>.tar.gz holding `files` ({name: bytes}) in its top directory."""
    with tarfile.open(pathlib.Path(dist) / f"benchy-mcp-{platform}.tar.gz", "w:gz") as t:
        for name, data in files.items():
            info = tarfile.TarInfo(f"benchy-mcp-9.9.9-{platform}/{name}")
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))


def release(dist, platforms=PLATFORMS):
    for p in platforms:
        lib = "libduckdb.so" if p.startswith("linux-") else "libduckdb.dylib"
        # Each file says which glibc it needs, so the stand-in for readelf can answer for it.
        bundle(dist, p, {"benchy-mcp": f"bin {p}".encode(), lib: f"lib {p}".encode(),
                         "README.md": b"# benchy"})


GLIBC = {"bin linux-x86_64": ("2.17", "2.34"), "lib linux-x86_64": ("2.25",),
         "bin linux-aarch64": ("2.17", "2.35"), "lib linux-aarch64": ("2.17",)}


def readelf(path):
    return needs(*GLIBC.get(pathlib.Path(path).read_text(), ()))


def says(obj):
    def run(binary):
        assert pathlib.Path(binary).name == "benchy-mcp"
        return json.loads(json.dumps(obj))
    return run


def refused(f, *args, **kw):
    try:
        f(*args, **kw)
    except (compat.Refused, sbom.Refused) as e:
        return str(e)
    raise AssertionError(f"{f.__name__} did not refuse")


def check_the_newest_glibc_needed_is_read_from_the_needs():
    assert compat.glibc_needed(needs("2.17", "2.34", "2.4")) == (2, 34)
    # 2.4 is older than 2.17: compared as numbers, not as text.
    assert compat.glibc_needed(needs("2.4", "2.17")) == (2, 17)
    # A file needing no GLIBC_ version, and a listing with no needs section, need nothing.
    assert compat.glibc_needed(needs()) == ()
    assert compat.glibc_needed("Version symbols section\n 2 (GLIBC_2.40)") == ()
    # The real readelf, on a binary of this machine, agrees with the format read here.
    if compat.this_platform().startswith("linux-"):
        assert compat.glibc_needed(compat.readelf(sys.executable)) >= (2, 17)


def check_compat_json_is_the_binarys_word_and_the_measured_glibc():
    with tempfile.TemporaryDirectory() as dist:
        release(dist)
        got = compat.write(dist, says(SAID), readelf)
        # Every Linux file is read, either architecture's, binary and library both.
        assert got == dict(SAID, min_glibc="2.35"), got
        GLIBC["lib linux-x86_64"] = ("2.38",)
        try:
            assert compat.write(dist, says(SAID), readelf)["min_glibc"] == "2.38"
        finally:
            GLIBC["lib linux-x86_64"] = ("2.25",)


def check_a_release_is_exactly_the_platforms_the_binary_names():
    with tempfile.TemporaryDirectory() as dist:
        release(dist, [p for p in PLATFORMS if p != "macos-x86_64"])
        assert "macos-x86_64" in str(SAID["platforms"])
        refused(compat.write, dist, says(SAID), readelf)
    with tempfile.TemporaryDirectory() as dist:
        release(dist, PLATFORMS + ["linux-riscv64"])
        refused(compat.write, dist, says(SAID), readelf)
    with tempfile.TemporaryDirectory() as dist:
        release(dist, [p for p in PLATFORMS if p != compat.this_platform()])
        assert "this machine" in refused(compat.write, dist, says(SAID), readelf)


def check_a_binary_that_disagrees_with_compat_json_is_refused():
    written = dict(SAID, min_glibc="2.35")
    with tempfile.TemporaryDirectory() as d:
        binary = pathlib.Path(d) / "benchy-mcp"
        binary.write_text("bin linux-aarch64")
        compat.check(written, binary, says(SAID), readelf)
        for field, other in [("benchy", "9.9.10"), ("host_protocols", [1, 2]),
                             ("duckdb", "1.5.6"), ("full_sandbox_min_landlock_abi", 5)]:
            refused(compat.check, written, binary, says(dict(SAID, **{field: other})), readelf)
        # A field the binary has and compat.json does not is a disagreement too.
        refused(compat.check, written, binary, says(dict(SAID, extra=1)), readelf)
        if compat.this_platform().startswith("linux-"):
            # On Linux, a binary or the library beside it that needs a newer glibc than
            # compat.json says is refused.
            refused(compat.check, dict(written, min_glibc="2.34"), binary, says(SAID), readelf)
            (pathlib.Path(d) / "libduckdb.so").write_text("lib x")
            GLIBC["lib x"] = ("2.36",)
            refused(compat.check, written, binary, says(SAID), readelf)


def real_sbom(triple="x86_64-unknown-linux-gnu"):
    with tempfile.TemporaryDirectory() as d:
        d = pathlib.Path(d)
        (d / "benchy-mcp").write_bytes(b"the binary")
        (d / duckdb_lib.ARCHIVES[triple][1]).write_bytes(b"the library")
        return sbom.sbom(triple, "linux-x86_64", d, sbom.cargo_metadata(triple),
                         (REPO / "Cargo.lock").read_text())


def check_the_sbom_names_every_crate_the_release_builds():
    doc = real_sbom()
    assert doc["bomFormat"] == "CycloneDX" and doc["specVersion"] == "1.5"
    crates = {c["name"]: c for c in doc["components"] if c["purl"].startswith("pkg:cargo/")}
    lock = (REPO / "Cargo.lock").read_text()
    # Each with the checksum Cargo.lock pins it to.
    sums = sbom.lock_checksums(lock)
    for c in crates.values():
        assert c["hashes"] == [{"alg": "SHA-256", "content": sums[(c["name"], c["version"])]}], c
        assert c["licenses"][0]["expression"] and "/" not in c["licenses"][0]["expression"], c
    # In the binary, and only building it: serde is compiled in, its derive macro runs in rustc,
    # and a build script's dependency (cc, which libduckdb-sys's uses) ships nothing.
    assert crates["serde"]["scope"] == "required"
    assert crates["duckdb"]["scope"] == "required"
    assert crates["serde_derive"]["scope"] == "excluded"
    assert crates["cc"]["scope"] == "excluded"
    # Another OS's crates are not built for this target.
    assert not {"windows-sys", "winapi"} & set(crates), sorted(crates)
    assert 100 < len(crates) < len(sums), len(crates)


def check_a_crate_is_scoped_by_how_the_release_reaches_it():
    # root -> lib (normal) -> inner (normal); root -> derive (normal, a proc macro) -> syn;
    # lib -> cc (build); root -> tester (dev). benchy has no dev-dependencies today, so this graph
    # is made up, as one would be in Cargo's own output.
    def pkg(name, kind="lib"):
        return {"id": name, "name": name, "version": "1.0.0", "targets": [{"kind": [kind]}]}

    def dep(name, kind=None):
        return {"pkg": name, "dep_kinds": [{"kind": kind, "target": None}]}

    meta = {
        "packages": [pkg("root", "bin"), pkg("lib"), pkg("inner"), pkg("derive", "proc-macro"),
                     pkg("syn"), pkg("cc"), pkg("tester")],
        "resolve": {"root": "root", "nodes": [
            {"id": "root", "deps": [dep("lib"), dep("derive"), dep("tester", "dev")]},
            {"id": "lib", "deps": [dep("inner"), dep("cc", "build")]},
            {"id": "inner", "deps": []}, {"id": "derive", "deps": [dep("syn")]},
            {"id": "syn", "deps": []}, {"id": "cc", "deps": []}, {"id": "tester", "deps": []}]},
    }
    scopes, root = sbom.shipped(meta)
    assert root == "root"
    assert scopes == {"lib": "required", "inner": "required", "derive": "excluded",
                      "syn": "excluded", "cc": "excluded"}, scopes


def check_the_sbom_names_duckdbs_library_as_shipped_and_as_released():
    doc = real_sbom()
    (lib,) = [c for c in doc["components"] if c["name"] == "libduckdb"]
    version = duckdb_lib.version_from_lock((REPO / "Cargo.lock").read_text())
    pins = duckdb_lib.parse_pins(duckdb_lib.PINS.read_text())
    assert lib["version"] == version and lib["purl"] == f"pkg:github/duckdb/duckdb@v{version}"
    assert lib["hashes"] == [{"alg": "SHA-256",
                              "content": hashlib.sha256(b"the library").hexdigest()}]
    (dist,) = [r for r in lib["externalReferences"] if r["type"] == "distribution"]
    assert dist["url"].endswith(f"/v{version}/libduckdb-linux-amd64.zip"), dist
    assert dist["hashes"][0]["content"] == pins[(version, "libduckdb-linux-amd64.zip")]
    # macOS ships one architecture of the universal file: its own hash, the universal file's pin.
    mac = real_sbom("aarch64-apple-darwin")
    (mlib,) = [c for c in mac["components"] if c["name"] == "libduckdb"]
    assert mlib["externalReferences"][0]["hashes"][0]["content"] == \
        pins[(version, "libduckdb-osx-universal.zip")]
    # The graph: benchy-mcp on its direct dependencies, libduckdb-sys on the library.
    deps = {d["ref"]: d["dependsOn"] for d in doc["dependencies"]}
    sys_ref = next(c["bom-ref"] for c in doc["components"] if c["name"] == "libduckdb-sys")
    assert lib["bom-ref"] in deps[sys_ref]
    assert any(r.startswith("pkg:cargo/duckdb@") for r in deps["benchy-mcp"])
    refs = {c["bom-ref"] for c in doc["components"]} | {"benchy-mcp"}
    assert all(set(on) <= refs for on in deps.values())
    me = doc["metadata"]["component"]
    assert me["hashes"][0]["content"] == hashlib.sha256(b"the binary").hexdigest()
    assert me["purl"] == f"pkg:github/comino/benchy-mcp@v{me['version']}", me
    assert me["externalReferences"] == [{"type": "vcs",
                                         "url": "https://github.com/comino/benchy-mcp"}], me
    # The same bundle from the same commit is the same SBOM, serial number included.
    assert real_sbom() == doc


def check_an_sbom_needs_the_files_it_vouches_for():
    with tempfile.TemporaryDirectory() as d:
        meta = sbom.cargo_metadata("x86_64-unknown-linux-gnu")
        lock = (REPO / "Cargo.lock").read_text()
        (pathlib.Path(d) / "benchy-mcp").write_bytes(b"the binary")
        assert "libduckdb.so" in refused(sbom.sbom, "x86_64-unknown-linux-gnu", "linux-x86_64",
                                         d, meta, lock)
        # A crate whose checksum Cargo.lock does not pin is not vouched for.
        (pathlib.Path(d) / "libduckdb.so").write_bytes(b"the library")
        unpinned = lock.replace('name = "serde"\nversion', 'name = "serde-renamed"\nversion', 1)
        assert "serde" in refused(sbom.sbom, "x86_64-unknown-linux-gnu", "linux-x86_64", d,
                                  meta, unpinned)


if __name__ == "__main__":
    checks = [v for k, v in sorted(globals().items()) if k.startswith("check_")]
    for c in checks:
        c()
        print(f"ok  {c.__name__}")
    print(f"test_release_files.py: {len(checks)} checks passed")
