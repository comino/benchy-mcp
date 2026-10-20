#!/usr/bin/env python3
"""Write a release's compat.json from its bundles, and check a binary against one.

    python3 packaging/compat.py write <dist>                 # > compat.json
    python3 packaging/compat.py check <compat.json> <binary>

`write` takes `<dist>`, holding every benchy-mcp-<platform>.tar.gz of the release. The object is
the one this machine's bundle prints for `--version --json`, which asks each field of the code
that decides it, and `min_glibc`, which no binary can say of itself: the newest GLIBC_ symbol
version any Linux bundle's binary or library needs, read from their ELF version needs. It
refuses a release whose bundles are not exactly the platforms the binary names, or whose Linux
binaries disagree with it.

`check` is what each smoke test runs on its platform: that binary prints what compat.json says,
and on Linux needs no newer glibc than `min_glibc`.
"""

import json
import pathlib
import platform as host
import re
import subprocess
import sys
import tarfile
import tempfile

LINUX_FILES = ("benchy-mcp", "libduckdb.so")


class Refused(Exception):
    pass


def glibc_needed(readelf_v):
    """The newest GLIBC_x.y a `readelf -V` listing's version needs name, as a tuple; () for none.
    Only the needs section: a library's own version definitions are not what it asks of the
    system."""
    needs = readelf_v.split("Version needs section", 1)
    if len(needs) < 2:
        return ()
    found = re.findall(r"Name: GLIBC_(\d+(?:\.\d+)+)\b", needs[1])
    return max((tuple(int(n) for n in v.split(".")) for v in found), default=())


def readelf(path):
    return subprocess.run(["readelf", "-V", "--wide", str(path)], capture_output=True, text=True,
                          check=True).stdout


def version_json(binary):
    out = subprocess.run([str(binary), "--version", "--json"], capture_output=True, text=True)
    if out.returncode != 0:
        raise Refused(f"{binary} --version --json failed: {out.stderr.strip()}")
    try:
        return json.loads(out.stdout)
    except json.JSONDecodeError as e:
        raise Refused(f"{binary} --version --json is not JSON: {e}") from None


def this_platform():
    os_name = {"Linux": "linux", "Darwin": "macos"}[host.system()]
    arch = host.machine().lower()
    arch = {"amd64": "x86_64", "arm64": "aarch64" if os_name == "linux" else "arm64",
            "aarch64": "aarch64" if os_name == "linux" else "arm64"}.get(arch, arch)
    return f"{os_name}-{arch}"


def unpack(tarball, into, names):
    """The files `names` of a bundle, by name, so an entry's path never decides where it lands."""
    got = {}
    with tarfile.open(tarball) as t:
        for m in t.getmembers():
            name = m.name.rsplit("/", 1)[-1]
            if m.isfile() and m.name.count("/") == 1 and name in names:
                dest = pathlib.Path(into) / name
                dest.write_bytes(t.extractfile(m).read())
                dest.chmod(0o755)
                got[name] = dest
    missing = set(names) - set(got)
    if missing:
        raise Refused(f"{tarball.name} has no {', '.join(sorted(missing))}")
    return got


def write(dist, run=version_json, needs=readelf):
    dist = pathlib.Path(dist)
    bundles = {p.name[len("benchy-mcp-"):-len(".tar.gz")]: p
               for p in dist.glob("benchy-mcp-*.tar.gz")}
    mine = this_platform()
    if mine not in bundles:
        raise Refused(f"no bundle for this machine's platform, {mine}, in {dist}")
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        (tmp / mine).mkdir()
        names = ("benchy-mcp",) + (("libduckdb.so",) if mine.startswith("linux-") else
                                   ("libduckdb.dylib",))
        ran = unpack(bundles[mine], tmp / mine, names)
        compat = run(ran["benchy-mcp"])
        if sorted(compat.get("platforms", [])) != sorted(bundles):
            raise Refused(f"the binary names platforms {compat.get('platforms')}, and the "
                          f"release has bundles for {sorted(bundles)}")
        newest = ()
        for plat in sorted(p for p in bundles if p.startswith("linux-")):
            d = tmp / f"elf-{plat}"
            d.mkdir()
            for f in unpack(bundles[plat], d, LINUX_FILES).values():
                newest = max(newest, glibc_needed(needs(f)))
    if not newest:
        raise Refused("no Linux bundle names a GLIBC_ version it needs")
    compat["min_glibc"] = ".".join(map(str, newest))
    return compat


def check(compat, binary, run=version_json, needs=readelf):
    want = {k: v for k, v in compat.items() if k != "min_glibc"}
    got = run(binary)
    if got != want:
        raise Refused(f"{binary} says {json.dumps(got)}, and compat.json {json.dumps(want)}")
    if this_platform().startswith("linux-"):
        floor = tuple(int(n) for n in compat["min_glibc"].split("."))
        lib = pathlib.Path(binary).parent / "libduckdb.so"
        for f in [pathlib.Path(binary)] + ([lib] if lib.is_file() else []):
            need = glibc_needed(needs(f))
            if need > floor:
                raise Refused(f"{f.name} needs GLIBC_{'.'.join(map(str, need))}, newer than "
                              f"compat.json's min_glibc {compat['min_glibc']}")


def main(argv):
    try:
        if len(argv) == 3 and argv[1] == "write":
            print(json.dumps(write(argv[2])))
            return 0
        if len(argv) == 4 and argv[1] == "check":
            check(json.loads(pathlib.Path(argv[2]).read_text()), argv[3])
            print(f"{argv[3]} agrees with {argv[2]}")
            return 0
    except (Refused, subprocess.CalledProcessError) as e:
        print(f"compat: {e}", file=sys.stderr)
        return 1
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
