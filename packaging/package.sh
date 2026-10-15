#!/usr/bin/env bash
# Turn one release build into a bundle: the binary, DuckDB's library beside it, the licences.
#
#   BENCHY_BUNDLE=1 cargo build --release --locked --target <triple>
#   packaging/package.sh <triple> <platform> <outdir>
#
# Writes <outdir>/benchy-mcp-<platform>.tar.gz and its .sha256. The asset name carries no
# version, so https://github.com/<repo>/releases/latest/download/<asset> always names the newest;
# the directory inside it does. Refuses a binary whose rpath reaches outside its own directory:
# an installed copy would otherwise look for the library on the build machine first.
set -euo pipefail

target=$1 platform=$2 out=$3
root=$(cd "$(dirname "$0")/.." && pwd)
bin="$root/target/$target/release/benchy-mcp"
[ -x "$bin" ] || { echo "no release build at $bin" >&2; exit 1; }

version=$(cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" |
	python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="benchy-mcp"))')
# A tag names the version it releases; Cargo.toml is what the binary reports. They must agree.
if [ "${GITHUB_REF_TYPE:-}" = tag ] && [ "${GITHUB_REF_NAME:-}" != "v$version" ]; then
	echo "tag ${GITHUB_REF_NAME} does not match Cargo.toml's version $version" >&2
	exit 1
fi

# libduckdb-sys fetched the library into target/duckdb-download/<triple>/<duckdb version>/.
libdir=$(find "$root/target/duckdb-download/$target" -mindepth 1 -maxdepth 1 -type d | head -n 1)
duckdb=$(basename "$libdir")
case "$target" in
*-linux-*) lib=libduckdb.so ;;
*-apple-darwin) lib=libduckdb.dylib ;;
*) echo "no bundle layout for $target" >&2; exit 1 ;;
esac
[ -f "$libdir/$lib" ] || { echo "no $lib in $libdir" >&2; exit 1; }

name="benchy-mcp-$version-$platform"
stage="$out/$name"
rm -rf "$stage"
mkdir -p "$stage"
cp "$bin" "$stage/"
case "$target" in
*-linux-*)
	cp "$libdir/$lib" "$stage/"
	rpath=$(readelf -d "$stage/benchy-mcp" | sed -n 's/.*(RUNPATH).*\[\(.*\)\]/\1/p; s/.*(RPATH).*\[\(.*\)\]/\1/p')
	if [ "$rpath" != '$ORIGIN' ]; then
		echo "the binary's runpath is '$rpath', not \$ORIGIN alone: build with BENCHY_BUNDLE=1" >&2
		exit 1
	fi
	;;
*-apple-darwin)
	# DuckDB ships one universal library for both architectures; keep only the one this bundle
	# runs on, which halves it. Each slice carries its own signature, so thinning leaves it valid.
	arch=${target%%-*}
	[ "$arch" = aarch64 ] && arch=arm64
	lipo "$libdir/$lib" -thin "$arch" -output "$stage/$lib"
	rpaths=$(otool -l "$stage/benchy-mcp" | awk '/cmd LC_RPATH/ {getline; getline; print $2}')
	if [ "$rpaths" != '@executable_path' ]; then
		echo "the binary's rpaths are '$rpaths', not @executable_path alone: build with BENCHY_BUNDLE=1" >&2
		exit 1
	fi
	otool -L "$stage/benchy-mcp" | grep -q '@rpath/libduckdb.dylib' ||
		{ echo "the binary does not load libduckdb through @rpath" >&2; exit 1; }
	;;
esac

cp "$root/LICENSE" "$stage/LICENSE"
cp "$root/CHANGELOG.md" "$stage/CHANGELOG.md"
cp "$root/packaging/README.md" "$stage/README.md"
# The README's first question is asked of this data; 12 KB, so it travels with the binary.
cp -R "$root/examples/data" "$stage/example-data"
# DuckDB's notice, from the release whose library this is.
curl -fsSL --retry 3 -o "$stage/LICENSE-DuckDB" \
	"https://raw.githubusercontent.com/duckdb/duckdb/v$duckdb/LICENSE"
grep -q "MIT\|Permission is hereby granted" "$stage/LICENSE-DuckDB" ||
	{ echo "LICENSE-DuckDB does not look like DuckDB's MIT licence" >&2; exit 1; }
(cd "$root" && python3 packaging/third_party_licenses.py "$target") >"$stage/THIRD-PARTY-LICENSES.txt"

asset="benchy-mcp-$platform.tar.gz"
tar -C "$out" -czf "$out/$asset" "$name"
(
	cd "$out"
	if command -v sha256sum >/dev/null; then sha256sum "$asset"; else shasum -a 256 "$asset"; fi
) >"$out/$asset.sha256"
rm -rf "$stage"
echo "$out/$asset: benchy-mcp $version, DuckDB $duckdb"
cat "$out/$asset.sha256"
