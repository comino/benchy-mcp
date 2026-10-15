#!/bin/sh
# Install benchy-mcp from a GitHub release. No Rust, no root.
#
#   curl -fsSL https://github.com/comino/benchy-mcp/releases/latest/download/install.sh | sh
#
# Picks the bundle for this OS and CPU, checks it against its published SHA-256, unpacks it to
# $BENCHY_LIB_DIR/<version>/ and links the command into $BENCHY_INSTALL_DIR. The bundle is the
# binary with DuckDB's library beside it; the binary finds the library through its own directory
# ($ORIGIN, @executable_path), so the two stay together and only a link goes on your PATH.
#
# Running it again is safe: it reinstalls, or moves the link to the newer version. The version
# the link pointed at before is kept, so a session that is running from it keeps working; any
# older one is removed.
#
# Environment:
#   BENCHY_VERSION         a release such as 0.1.0 (default: the latest release)
#   BENCHY_INSTALL_DIR     where the `benchy-mcp` command goes  (default: ~/.local/bin)
#   BENCHY_LIB_DIR         where the bundles go                 (default: ~/.local/lib/benchy-mcp)
#   BENCHY_DOWNLOAD_BASE   fetch the bundle and its checksum from here instead of GitHub: an
#                          http(s):// or file:// URL, or a local directory. For testing a build.
#
# Uninstall: rm -rf ~/.local/lib/benchy-mcp ~/.local/bin/benchy-mcp
#
# Verify what you install. The checksum this script checks comes from the same release as the
# bundle, so it catches a broken download, not a forged release. Each release file also carries a
# signed build-provenance attestation, made by the release workflow, which says the file was built
# by that workflow from this repository's commit. With the GitHub CLI:
#
#   gh attestation verify install.sh --repo comino/benchy-mcp
#   gh attestation verify benchy-mcp-linux-x86_64.tar.gz --repo comino/benchy-mcp
#
# The whole script is one function, called on its last line: a download cut short defines part
# of a function and runs nothing.

set -eu

REPO="comino/benchy-mcp"
BIN_DIR="${BENCHY_INSTALL_DIR:-$HOME/.local/bin}"
LIB_ROOT="${BENCHY_LIB_DIR:-$HOME/.local/lib/benchy-mcp}"
VERSION="${BENCHY_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() {
	printf 'benchy-mcp install: %s\n' "$*" >&2
	exit 1
}

platform() {
	os=$(uname -s)
	arch=$(uname -m)
	case "$os" in
	Linux)
		os=linux
		# The bundle is built against glibc. On musl (Alpine and others) it cannot start.
		if ldd --version 2>&1 | grep -qi musl; then
			die "this system uses musl libc; the release bundles need glibc, and DuckDB publishes no library for musl."
		fi
		;;
	Darwin) os=macos ;;
	*) die "no release for $os: benchy-mcp runs on Linux and macOS." ;;
	esac
	case "$arch" in
	x86_64 | amd64) arch=x86_64 ;;
	aarch64 | arm64) if [ "$os" = linux ]; then arch=aarch64; else arch=arm64; fi ;;
	*) die "no release for $os on $arch. Build from source instead (see the README)." ;;
	esac
	# A shell running under Rosetta reports x86_64 on Apple silicon; take the native build.
	if [ "$os" = macos ] && [ "$arch" = x86_64 ] &&
		[ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
		arch=arm64
	fi
	printf '%s-%s' "$os" "$arch"
}

# fetch URL-or-path DEST. HTTPS only, at TLS 1.2 or later, redirects included; plain http:// and
# file:// only when BENCHY_DOWNLOAD_BASE names one, which is for testing a build.
fetch() {
	case "$1" in
	https://*)
		if command -v curl >/dev/null 2>&1; then
			curl --proto '=https' --tlsv1.2 -fsSL --retry 3 -o "$2" "$1" || return 1
		elif command -v wget >/dev/null 2>&1; then
			wget --https-only --secure-protocol=TLSv1_2 -q -O "$2" "$1" || return 1
		else
			die "neither curl nor wget is installed; install one and run this again."
		fi
		;;
	http://* | file://*)
		if command -v curl >/dev/null 2>&1; then
			curl -fsSL --retry 3 -o "$2" "$1" || return 1
		elif command -v wget >/dev/null 2>&1; then
			wget -q -O "$2" "$1" || return 1
		else
			die "neither curl nor wget is installed; install one and run this again."
		fi
		;;
	*) cp "$1" "$2" || return 1 ;;
	esac
}

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | awk '{print $1}'
	elif command -v openssl >/dev/null 2>&1; then
		openssl dgst -sha256 -r "$1" | awk '{print $1}'
	else
		die "found no sha256sum, shasum or openssl to check the download with."
	fi
}

main() {
	PLATFORM=$(platform)
	ASSET="benchy-mcp-$PLATFORM.tar.gz"
	if [ -n "${BENCHY_DOWNLOAD_BASE:-}" ]; then
		BASE="${BENCHY_DOWNLOAD_BASE%/}"
	elif [ "$VERSION" = latest ]; then
		BASE="https://github.com/$REPO/releases/latest/download"
	else
		BASE="https://github.com/$REPO/releases/download/v${VERSION#v}"
	fi

	link="$BIN_DIR/benchy-mcp"
	if [ -e "$link" ] && [ ! -L "$link" ]; then
		die "$link exists and is not a link this script made. Move it away, or set BENCHY_INSTALL_DIR."
	fi

	TMP=$(mktemp -d 2>/dev/null || mktemp -d -t benchy-mcp)
	STAGE=""
	cleanup() {
		rm -rf "$TMP"
		if [ -n "$STAGE" ]; then rm -rf "$STAGE"; fi
	}
	trap cleanup EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM

	say "Downloading $ASSET from $BASE"
	fetch "$BASE/$ASSET" "$TMP/$ASSET" ||
		die "could not download $BASE/$ASSET. Check the version (BENCHY_VERSION=$VERSION) and your connection."
	fetch "$BASE/$ASSET.sha256" "$TMP/$ASSET.sha256" ||
		die "could not download $BASE/$ASSET.sha256, so the bundle cannot be checked. Nothing was installed."

	expected=$(awk 'NR == 1 {print $1}' "$TMP/$ASSET.sha256")
	actual=$(sha256 "$TMP/$ASSET")
	if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
		die "checksum mismatch for $ASSET: expected ${expected:-nothing}, got $actual. Nothing was installed."
	fi
	say "Checksum OK ($actual)"

	mkdir -p "$LIB_ROOT" || die "cannot create $LIB_ROOT"
	# Unpacked beside its final place, so moving it there is a rename, not a copy.
	STAGE="$LIB_ROOT/.incoming.$$"
	rm -rf "$STAGE"
	mkdir "$STAGE"
	tar -xzf "$TMP/$ASSET" -C "$STAGE" || die "could not unpack $ASSET."
	set -- "$STAGE"/*/benchy-mcp
	[ $# -eq 1 ] && [ -x "$1" ] || die "$ASSET does not hold the bundle this script expects."
	unpacked=$(dirname "$1")

	# Run it before linking it: a bundle that cannot start here (an older glibc, say) is reported now,
	# with the loader's own message, instead of the first time a client tries it.
	if ! out=$("$unpacked/benchy-mcp" --version 2>&1); then
		die "the downloaded benchy-mcp does not run on this system:
	$out"
	fi
	version=$(printf '%s\n' "$out" | awk 'NR == 1 {print $2}')
	[ -n "$version" ] || die "the downloaded benchy-mcp did not report a version: $out"

	case "$version" in
	*[!0-9A-Za-z.+-]* | .*) die "the downloaded benchy-mcp reported an unexpected version: $version" ;;
	esac

	dest="$LIB_ROOT/$version"
	previous=""
	if [ -L "$link" ]; then previous=$(dirname "$(readlink "$link")"); fi

	rm -rf "$dest"
	mv "$unpacked" "$dest"
	mkdir -p "$BIN_DIR" || die "cannot create $BIN_DIR"
	# A new link renamed over the old one, so the command is never missing in between.
	ln -s "$dest/benchy-mcp" "$BIN_DIR/.benchy-mcp.$$"
	mv -f "$BIN_DIR/.benchy-mcp.$$" "$link"

	for old in "$LIB_ROOT"/*; do
		[ -d "$old" ] || continue
		[ "$old" = "$dest" ] && continue
		[ "$old" = "$previous" ] && continue
		# Only what an earlier run put here: a bundle, binary and library together. A
		# BENCHY_LIB_DIR pointed at a directory with other things in it loses none of them.
		[ -x "$old/benchy-mcp" ] || continue
		[ -f "$old/libduckdb.so" ] || [ -f "$old/libduckdb.dylib" ] || continue
		rm -rf "$old"
	done

	"$link" --version >/dev/null 2>&1 || die "installed, but $link does not run. Run it by hand to see why."
	say "Installed benchy-mcp $version: $link -> $dest/benchy-mcp"

	case ":$PATH:" in
	*":$BIN_DIR:"*) cmd=benchy-mcp ;;
	*)
		cmd="$link"
		say ""
		say "$BIN_DIR is not on your PATH. Add it, or use the full path:"
		say "  export PATH=\"$BIN_DIR:\$PATH\""
		;;
	esac
	say ""
	say "Next, check what this machine enforces:"
	say "  $cmd doctor"
	if [ "${PLATFORM%%-*}" = macos ]; then
		say ""
		say "macOS: benchy has no OS sandbox here. On Linux, Landlock keeps the query engine and every"
		say "file parser from reading anything but their own files; on macOS nothing does, and the SQL"
		say "layer is the only boundary between the agent's SQL and your files. So benchy refuses to"
		say "serve unless started with --allow-unsandboxed, which says you accept that. Read what it"
		say "means before you add it:"
		say "  https://github.com/$REPO#limits"
	else
		say "Then give it to your agent. The full path works whether or not a client sees your PATH:"
		say "  claude mcp add benchy -- $link $dest/example-data"
		say "(that is the bundled example data; point it at a folder of your own the same way)"
	fi
}

main "$@"
