#!/usr/bin/env python3
"""Every build names the same Rust compiler: `python3 scripts/check_toolchain.py`.

rust-toolchain.toml pins it for cargo, locally and on the runners. The workflows install it by
name, the Dockerfile builds in the image of that version, and Cargo.toml's rust-version says so
to anyone building with something else. A native bundle and the image of the same tag are then
built by the same compiler, and so is a rebuild of that tag later. Fails naming each place that
disagrees with rust-toolchain.toml, or that names a channel such as `stable` instead of a version.
"""

import pathlib
import re
import sys
import tomllib

REPO = pathlib.Path(__file__).resolve().parent.parent


def main() -> int:
    path = REPO / "rust-toolchain.toml"
    if not path.is_file():
        print("rust-toolchain.toml is missing", file=sys.stderr)
        return 1
    want = tomllib.loads(path.read_text())["toolchain"]["channel"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", want):
        print(f"rust-toolchain.toml names {want!r}, not an exact version", file=sys.stderr)
        return 1

    found = []
    cargo = tomllib.loads((REPO / "Cargo.toml").read_text())["package"].get("rust-version")
    found.append(("Cargo.toml rust-version", cargo))
    for m in re.finditer(r"^FROM rust:([^@\s]+)", (REPO / "Dockerfile").read_text(), re.M):
        found.append(("Dockerfile FROM rust", m.group(1).split("-")[0]))
    for wf in sorted((REPO / ".github/workflows").glob("*.yml")):
        text = wf.read_text()
        uses = len(re.findall(r"uses: dtolnay/rust-toolchain@", text))
        given = re.findall(r"toolchain: ([^,}\s]+)", text)
        if uses != len(given):
            found.append((f"{wf.name}: a rust-toolchain step without a toolchain", None))
        found += [(f"{wf.name} toolchain:", g.strip("\"'")) for g in given]

    bad = [(where, got) for where, got in found if got != want]
    for where, got in bad:
        print(f"{where} is {got!r}, rust-toolchain.toml is {want!r}", file=sys.stderr)
    if bad:
        return 1
    print(f"check_toolchain.py: Rust {want} in {len(found)} places")
    return 0


if __name__ == "__main__":
    sys.exit(main())
