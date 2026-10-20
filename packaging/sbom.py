#!/usr/bin/env python3
"""Write the CycloneDX SBOM of one release bundle: every crate built into it, and DuckDB's library.

    python3 packaging/sbom.py <target-triple> <platform> <bundle-dir> > benchy-mcp-<platform>.cdx.json

`<bundle-dir>` is the bundle as package.sh stages it, holding the binary and the library it ships,
whose SHA-256 the SBOM records. The crates are those Cargo resolves for the target from Cargo.lock,
each with the checksum Cargo.lock pins its .crate file to. A crate that only builds the binary —
a build script's, a proc macro's — is listed with scope `excluded`: it ran on the build machine
and is not in what ships. Dev-dependencies are left out; no release build compiles them.

Written here rather than by cargo-cyclonedx: that would be one more tool from crates.io run in the
release, and it knows nothing of the library libduckdb-sys links, which the bundle ships and which
is pinned by packaging/duckdb.sha256 rather than by Cargo.lock.
"""

import hashlib
import json
import os
import pathlib
import re
import subprocess
import sys
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import duckdb_lib  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
# A fixed namespace for the serial number, so the same bundle from the same commit gets the same
# one: the SBOM is reproducible, and a different bundle never shares it.
NAMESPACE = uuid.UUID("6f6a1f3e-2b0c-4d1e-9a57-5bd1c0f0e0a1")


class Refused(Exception):
    pass


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def lock_checksums(lock_text):
    """{(name, version): checksum} for each registry package in Cargo.lock."""
    sums = {}
    for block in lock_text.split("[[package]]")[1:]:
        fields = dict(re.findall(r'^(\w+) = "(.*)"$', block, re.M))
        if "checksum" in fields:
            sums[(fields["name"], fields["version"])] = fields["checksum"]
    return sums


def spdx(expression):
    """A manifest's licence as an SPDX expression: Cargo still accepts the old `MIT/Apache-2.0`,
    which SPDX spells `MIT OR Apache-2.0`."""
    return " OR ".join(part.strip() for part in expression.split("/"))


def shipped(meta):
    """The packages of `meta` the release builds, as {id: scope}: `required` for what is compiled
    into the binary, `excluded` for what only builds it."""
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]

    def proc_macro(pid):
        return any("proc-macro" in t["kind"] for t in packages[pid]["targets"])

    def walk(start, follow):
        seen, stack = set(), list(start)
        while stack:
            for dep in nodes[stack.pop()]["deps"]:
                if follow(dep) and dep["pkg"] not in seen:
                    seen.add(dep["pkg"])
                    stack.append(dep["pkg"])
        return seen

    def kinds(dep):
        return {k["kind"] for k in dep["dep_kinds"]}

    # Into the binary: normal dependencies, but not through a proc macro, which runs in rustc.
    required = walk([root], lambda d: None in kinds(d) and not proc_macro(d["pkg"]))
    # Everything the build compiles: a build script's dependencies too, and all a proc macro uses.
    built = walk([root], lambda d: bool(kinds(d) - {"dev"}))
    built.discard(root)
    return {pid: "required" if pid in required else "excluded" for pid in built}, root


def crate_component(p, scope, checksums):
    if p.get("source") != CRATES_IO:
        raise Refused(f"{p['name']} {p['version']} is not from crates.io: {p.get('source')}")
    purl = f"pkg:cargo/{p['name']}@{p['version']}"
    c = {"type": "library", "bom-ref": purl, "name": p["name"], "version": p["version"],
         "scope": scope, "purl": purl}
    checksum = checksums.get((p["name"], p["version"]))
    if not checksum:
        raise Refused(f"Cargo.lock pins no checksum for {p['name']} {p['version']}")
    c["hashes"] = [{"alg": "SHA-256", "content": checksum}]
    if not p.get("license"):
        raise Refused(f"{p['name']} {p['version']} declares no licence")
    c["licenses"] = [{"expression": spdx(p["license"])}]
    if p.get("repository"):
        c["externalReferences"] = [{"type": "vcs", "url": p["repository"]}]
    return c


def duckdb_component(triple, bundle):
    """DuckDB's library as the bundle ships it, and the pinned release file it came from. On macOS
    the shipped library is one architecture thinned out of the universal file (package.sh), so
    its SHA-256 is the file's own, not the release file's."""
    version = duckdb_lib.version_from_lock((REPO / "Cargo.lock").read_text())
    pins = duckdb_lib.parse_pins(duckdb_lib.PINS.read_text())
    archive = duckdb_lib.archive_for(triple)
    lib = bundle / duckdb_lib.ARCHIVES[triple][1]
    if not lib.is_file():
        raise Refused(f"no {lib.name} in {bundle}")
    purl = f"pkg:github/duckdb/duckdb@v{version}"
    return {
        "type": "library", "bom-ref": purl, "name": "libduckdb", "version": version,
        "scope": "required", "purl": purl,
        "hashes": [{"alg": "SHA-256", "content": sha256(lib)}],
        "licenses": [{"license": {"id": "MIT"}}],
        "externalReferences": [
            {"type": "distribution",
             "url": duckdb_lib.RELEASE.format(version=version, name=archive),
             "hashes": [{"alg": "SHA-256",
                         "content": duckdb_lib.pinned(pins, version, archive)}]},
            {"type": "vcs", "url": "https://github.com/duckdb/duckdb"},
        ],
        "properties": [{"name": "benchy:file", "value": lib.name}],
    }


def commit_time():
    """The commit's time, so the same commit gives the same SBOM; none outside a checkout."""
    try:
        out = subprocess.run(["git", "-C", str(REPO), "log", "-1", "--format=%cI"],
                             capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return None
    return out or None


def sbom(triple, platform, bundle, meta, lock_text):
    bundle = pathlib.Path(bundle)
    packages = {p["id"]: p for p in meta["packages"]}
    scopes, root = shipped(meta)
    me = packages[root]
    binary = bundle / "benchy-mcp"
    if not binary.is_file():
        raise Refused(f"no benchy-mcp in {bundle}")
    checksums = lock_checksums(lock_text)
    order = sorted(scopes, key=lambda i: (packages[i]["name"], packages[i]["version"]))
    components = [crate_component(packages[i], scopes[i], checksums) for i in order]
    duckdb = duckdb_component(triple, bundle)
    components.append(duckdb)
    refs = [c["bom-ref"] for c in components]
    if len(set(refs)) != len(refs):
        raise Refused("two components share a name and version")

    ref = {i: f"pkg:cargo/{packages[i]['name']}@{packages[i]['version']}" for i in scopes}
    ref[root] = "benchy-mcp"
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    dependencies = []
    for pid in [root] + order:
        on = sorted({ref[d["pkg"]] for d in nodes[pid]["deps"]
                     if d["pkg"] in ref and {k["kind"] for k in d["dep_kinds"]} - {"dev"}})
        if packages[pid]["name"] == "libduckdb-sys":
            on.append(duckdb["bom-ref"])
        dependencies.append({"ref": ref[pid], "dependsOn": on})
    dependencies.append({"ref": duckdb["bom-ref"], "dependsOn": []})

    version = me["version"]
    repository = (me.get("repository") or "").rstrip("/")
    if not repository.startswith("https://github.com/"):
        raise Refused("Cargo.toml names no GitHub repository, which the release's purl is of")
    owner_repo = repository.removeprefix("https://github.com/")
    metadata = {
        "tools": {"components": [{"type": "application", "name": "packaging/sbom.py",
                                  "version": version}]},
        "component": {
            "type": "application", "bom-ref": "benchy-mcp", "name": "benchy-mcp",
            "version": version, "purl": f"pkg:github/{owner_repo}@v{version}",
            "hashes": [{"alg": "SHA-256", "content": sha256(binary)}],
            "licenses": [{"expression": spdx(me["license"])}],
            "externalReferences": [{"type": "vcs", "url": repository}],
        },
        "properties": [{"name": "benchy:platform", "value": platform},
                       {"name": "benchy:target", "value": triple}],
    }
    when = commit_time()
    if when:
        metadata["timestamp"] = when
    lock_sum = hashlib.sha256(lock_text.encode()).hexdigest()
    serial = uuid.uuid5(NAMESPACE, f"{version} {platform} {sha256(binary)} {lock_sum}")
    return {
        "bomFormat": "CycloneDX", "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{serial}", "version": 1,
        "metadata": metadata, "components": components, "dependencies": dependencies,
    }


def cargo_metadata(triple):
    return json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", triple],
        cwd=REPO))


def main(argv):
    if len(argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2
    triple, platform, bundle = argv[1:]
    try:
        doc = sbom(triple, platform, bundle, cargo_metadata(triple),
                   (REPO / "Cargo.lock").read_text())
    except (Refused, duckdb_lib.Refused) as e:
        print(f"sbom: {e}", file=sys.stderr)
        return 1
    json.dump(doc, sys.stdout, indent=1)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
