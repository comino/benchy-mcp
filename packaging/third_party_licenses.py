#!/usr/bin/env python3
"""Write the licence notices of every crate compiled into the benchy-mcp binary.

A release bundle ships a binary, and most of its crates are MIT or Apache-2.0, whose terms ask
that the notice travel with every copy. This walks the dependency graph Cargo resolves for one
target — normal dependencies only, since build scripts, proc macros and dev-dependencies are not
in the binary — and copies each crate's own licence files out of its source.

    python3 packaging/third_party_licenses.py <target-triple> > THIRD-PARTY-LICENSES.txt

A few crates publish without a licence file; those are listed with the licence their manifest
declares. Fails if a crate in the binary declares no licence at all, so a gap is seen, not shipped.
"""

import json
import pathlib
import subprocess
import sys

NAMES = ("LICENSE", "LICENCE", "COPYING", "NOTICE")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    target = sys.argv[1]
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target]
        )
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]

    # Normal dependencies, transitively: what is linked into the binary.
    seen, stack = set(), [root]
    while stack:
        node = nodes[stack.pop()]
        for dep in node["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                stack.append(dep["pkg"])

    out, missing = [], []
    for pid in sorted(seen, key=lambda i: (packages[i]["name"], packages[i]["version"])):
        p = packages[pid]
        src = pathlib.Path(p["manifest_path"]).parent
        files = sorted(
            f for f in src.iterdir() if f.is_file() and f.name.upper().startswith(NAMES)
        )
        if not files:
            if not p.get("license"):
                print(f"{p['name']} {p['version']} declares no licence", file=sys.stderr)
                return 1
            missing.append(f"{p['name']} {p['version']} — {p['license']}, {p.get('repository')}")
            continue
        out.append("=" * 78)
        out.append(f"{p['name']} {p['version']} — {p.get('license') or 'see below'}")
        if p.get("repository"):
            out.append(p["repository"])
        for f in files:
            out.append("-" * 78)
            out.append(f.name)
            out.append("")
            out.append(f.read_text(encoding="utf-8", errors="replace").rstrip())
        out.append("")

    print(
        f"Licence notices of the {len(seen)} Rust crates compiled into benchy-mcp for {target},\n"
        "copied from each crate's source. DuckDB's own notice is in LICENSE-DuckDB.\n"
    )
    if missing:
        # Some crates publish without their licence file. Their licence is still declared; the
        # standard texts of MIT and Apache-2.0 appear below with the crates that do ship them.
        print("These crates publish no licence file; each declares its licence in its manifest:")
        print(*(f"  {m}" for m in missing), sep="\n")
        print()
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
