#!/usr/bin/env bash
# Prove a release bundle works where nothing was built: no target/, no Rust, no DuckDB elsewhere.
#
#   packaging/smoke.sh <dir holding benchy-mcp-<platform>.tar.gz and .sha256> <platform>
#
# Unpacks into a fresh directory and runs what a user runs, then installs it with install.sh from
# a local web server, the way a user's curl would fetch it. A broken rpath, a missing library or
# a checksum that does not match fails here, before a release is published.
set -euo pipefail

dist=$(cd "$1" && pwd) platform=$2
repo=$(cd "$(dirname "$0")/.." && pwd)
asset="benchy-mcp-$platform.tar.gz"
work=$(mktemp -d)
trap 'rm -rf "$work"; [ -n "${server:-}" ] && kill "$server" 2>/dev/null || true' EXIT

step() { printf '\n== %s\n' "$*"; }
sum() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | awk '{print $1}'; }

step "checksum"
[ "$(sum "$dist/$asset")" = "$(awk '{print $1}' "$dist/$asset.sha256")" ]
echo ok

step "unpack into a fresh directory"
mkdir "$work/fresh"
tar -xzf "$dist/$asset" -C "$work/fresh"
bundle=$(echo "$work"/fresh/benchy-mcp-*)
ls -la "$bundle"
bin="$bundle/benchy-mcp"

step "every file in it is the one packaged"
(cd "$bundle" && if command -v sha256sum >/dev/null; then sha256sum -c --quiet SHA256SUMS; else shasum -a 256 -c --quiet SHA256SUMS; fi)
grep -q "checked before the build" "$bundle/README.md"
echo ok
case "$platform" in
linux-*) lib=libduckdb.so; ldd "$bin" ;;
macos-*) lib=libduckdb.dylib; otool -L "$bin"; lipo -info "$bundle/$lib"; codesign -dv "$bin" 2>&1 || true ;;
esac

step "--version reports Cargo.toml's version"
want=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n 1)
"$bin" --version
[ "$("$bin" --version | head -n 1)" = "benchy-mcp $want" ]

step "doctor, and the library it loaded is the one beside it"
"$bin" doctor | tee "$work/doctor"
loaded=$(sed -n 's/^ *loaded from //p' "$work/doctor")
[ "$loaded" = "$(cd "$bundle" && pwd -P)/$lib" ] || { echo "loaded $loaded, not the bundle's" >&2; exit 1; }

step "a real MCP handshake and the host protocol against the unpacked binary"
python3 "$repo/examples/verify.py" "$bin" "$repo/examples/data"
python3 "$repo/examples/host.py" "$bin"

step "without its library beside it, the binary does not start"
mv "$bundle/$lib" "$work/$lib"
if "$bin" --version >/dev/null 2>&1; then
	echo "it started without $lib beside it: it loads a copy from somewhere else" >&2
	exit 1
fi
echo "refused, as it should"
mv "$work/$lib" "$bundle/$lib"

step "install.sh from a local web server, twice"
port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$dist" >/dev/null 2>&1 &
server=$!
for _ in $(seq 50); do curl -fs "http://127.0.0.1:$port/" >/dev/null && break; sleep 0.1; done
export BENCHY_DOWNLOAD_BASE="http://127.0.0.1:$port"
export BENCHY_INSTALL_DIR="$work/home/bin" BENCHY_LIB_DIR="$work/home/lib/benchy-mcp"
sh "$repo/install.sh"
sh "$repo/install.sh" >/dev/null
ls -la "$BENCHY_INSTALL_DIR" "$BENCHY_LIB_DIR"
installed="$BENCHY_INSTALL_DIR/benchy-mcp"
[ -L "$installed" ]
"$installed" --version
"$installed" doctor | grep "loaded from"
# Through the link: $ORIGIN and @executable_path must resolve to the bundle, not to bin/.
python3 "$repo/examples/verify.py" "$installed" "$repo/examples/data" >/dev/null
echo "verify.py passed through the installed link"

step "install.sh refuses a bundle whose checksum does not match"
mkdir "$work/bad"
cp "$dist/$asset" "$work/bad/"
printf '%064d  %s\n' 0 "$asset" >"$work/bad/$asset.sha256"
if BENCHY_DOWNLOAD_BASE="$work/bad" BENCHY_INSTALL_DIR="$work/bad/bin" \
	BENCHY_LIB_DIR="$work/bad/lib" sh "$repo/install.sh" 2>"$work/bad.err"; then
	echo "install.sh accepted a bundle with the wrong checksum" >&2
	exit 1
fi
cat "$work/bad.err"
[ ! -e "$work/bad/bin/benchy-mcp" ]

if [ "${platform%%-*}" = macos ]; then
	# What Gatekeeper does. curl sets no quarantine attribute, so nothing asks; a browser
	# download does, and then the first run is decided by Gatekeeper. Reported, not asserted:
	# a CI runner's Gatekeeper policy need not be a desktop's.
	step "Gatekeeper (reported, not asserted)"
	echo "spctl --status: $(spctl --status 2>&1 || true)"
	echo "quarantine on the installed files: $(xattr -p com.apple.quarantine "$(readlink "$installed")" 2>&1 || true)"
	spctl -a -vv -t exec "$bin" 2>&1 || true
	xattr -w com.apple.quarantine "0081;$(printf '%x' "$(date +%s)");Safari;" "$bin"
	xattr -w com.apple.quarantine "0081;$(printf '%x' "$(date +%s)");Safari;" "$bundle/$lib"
	if "$bin" --version 2>&1; then
		echo "quarantined by hand: --version still ran here"
	else
		echo "quarantined by hand: --version did not run (exit $?)"
	fi
fi

step "all passed for $platform"
