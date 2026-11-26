#!/bin/sh
# Install the CedeGrid command-line tool into ~/.local/bin, for this user only.
#
# One pinned version: its tarball is checked against the release's SHA256SUMS before anything is
# unpacked, and a mismatch, a missing checksum or a missing tool stops the install with nothing
# installed. It needs no root and refuses to run as root.
#
#   curl -fsSL https://github.com/sidcatkr/CedeGrid/releases/download/v<version>/install.sh | sh
#
# CEDEGRID_VERSION, CEDEGRID_TARGET and CEDEGRID_RELEASE_URL override the version, the target and
# where the files come from; a URL other than https:// is accepted only on this host's loopback.
set -eu

version=${CEDEGRID_VERSION:-@VERSION@}
case $version in
'' | @*) echo "cedegrid install: no version is set; download install.sh from a release" >&2; exit 1 ;;
esac

fail() {
	echo "cedegrid install: $1; nothing was installed" >&2
	exit 1
}

[ "$(id -u)" != 0 ] || fail "it runs as root; run it as your own user, without sudo"

if [ -n "${CEDEGRID_TARGET:-}" ]; then
	target=$CEDEGRID_TARGET
else
	case "$(uname -s)/$(uname -m)" in
	Linux/x86_64 | Linux/amd64) target=x86_64-unknown-linux-gnu ;;
	Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu ;;
	*) fail "there is no prebuilt CLI for $(uname -s) $(uname -m); use Homebrew or cargo install --locked cedegrid" ;;
	esac
fi

base=${CEDEGRID_RELEASE_URL:-https://github.com/sidcatkr/CedeGrid/releases/download/v$version}
case $base in
https://*) ;;
http://127.0.0.1:* | http://localhost:*) ;;
*) fail "the release URL must use https" ;;
esac

if command -v curl >/dev/null 2>&1; then
	fetch() { curl -fsSL --proto '=https,http' -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
	fetch() { wget -q -O "$2" "$1"; }
else
	fail "neither curl nor wget is on PATH"
fi

if command -v sha256sum >/dev/null 2>&1; then
	digest() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
	digest() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
	fail "neither sha256sum nor shasum is on PATH to check the download"
fi

tarball=cedegrid-$version-$target.tar.gz
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fetch "$base/SHA256SUMS" "$work/SHA256SUMS" || fail "SHA256SUMS could not be downloaded from $base"
fetch "$base/$tarball" "$work/$tarball" || fail "$tarball could not be downloaded from $base"

expected=$(grep "  $tarball\$" "$work/SHA256SUMS" | cut -d ' ' -f 1 || true)
[ -n "$expected" ] || fail "SHA256SUMS has no line for $tarball"
actual=$(digest "$work/$tarball")
[ "$actual" = "$expected" ] || fail "$tarball does not match its SHA256SUMS line"

mkdir "$work/unpacked"
tar -xzf "$work/$tarball" -C "$work/unpacked"
binary=$work/unpacked/cedegrid-$version-$target/cedegrid
[ -f "$binary" ] || fail "$tarball holds no cedegrid-$version-$target/cedegrid"

bin=$HOME/.local/bin
mkdir -p "$bin"
cp "$binary" "$bin/.cedegrid.$$"
chmod 755 "$bin/.cedegrid.$$"
mv -f "$bin/.cedegrid.$$" "$bin/cedegrid"
echo "Installed cedegrid $version into $bin/cedegrid."

case ":$PATH:" in
*":$bin:"*) ;;
*) echo "$bin is not on PATH; add it, for example with: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
