#!/usr/bin/env bash
# Turn one release build into a bundle: the binary, DuckDB's library beside it, the licences.
#
#   python3 packaging/duckdb_lib.py fetch <triple>
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

# `packaging/duckdb_lib.py fetch` put DuckDB's release into target/duckdb-download/<triple>/
# <version>/ before the build, checked against packaging/duckdb.sha256. Check it again, and the
# copy cargo linked the tests against: the directory is the one for Cargo.lock's DuckDB, never
# whichever version a listing of the cache happens to return first.
libdir=$(python3 "$root/packaging/duckdb_lib.py" check "$target")
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
# DuckDB's notice, from the release whose library this is, pinned like the library.
python3 "$root/packaging/duckdb_lib.py" license "$stage/LICENSE-DuckDB" >/dev/null
(cd "$root" && python3 packaging/third_party_licenses.py "$target") >"$stage/THIRD-PARTY-LICENSES.txt"
# Where the library came from, for whoever has only the bundle: the release file it was taken
# from and the SHA-256 that file was checked against before the build.
zip=$(basename "$(ls "$libdir"/libduckdb-*.zip)")
pin=$(awk -v f="v$duckdb/$zip" '$2 == f {print $1}' "$root/packaging/duckdb.sha256")
[ -n "$pin" ] || { echo "no pin for $zip in packaging/duckdb.sha256" >&2; exit 1; }
cat >>"$stage/README.md" <<EOF

## Where DuckDB's library came from

\`$lib\` is from DuckDB $duckdb's release file
<https://github.com/duckdb/duckdb/releases/download/v$duckdb/$zip>,
checked before the build against SHA-256
\`$pin\`.

\`SHA256SUMS\` lists every other file in this directory as shipped; \`sha256sum -c SHA256SUMS\`
(or \`shasum -a 256 -c SHA256SUMS\`) checks them.
EOF
sums=$(
	cd "$stage"
	find . -type f | sed 's|^\./||' | LC_ALL=C sort | while IFS= read -r f; do
		if command -v sha256sum >/dev/null; then sha256sum "$f"; else shasum -a 256 "$f"; fi
	done
)
printf '%s\n' "$sums" >"$stage/SHA256SUMS"

asset="benchy-mcp-$platform.tar.gz"
tar -C "$out" -czf "$out/$asset" "$name"
(
	cd "$out"
	if command -v sha256sum >/dev/null; then sha256sum "$asset"; else shasum -a 256 "$asset"; fi
) >"$out/$asset.sha256"
rm -rf "$stage"
echo "$out/$asset: benchy-mcp $version, DuckDB $duckdb ($zip, SHA-256 $pin)"
cat "$out/$asset.sha256"
