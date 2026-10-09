#!/bin/sh
# Builds the release archive for one target: dist/tiphys-<target>.tar.gz and
# its checksum. The release workflow runs this for each target, and CI runs it
# on every push, so the build a release depends on is never tried for the
# first time on the day of a release.
#
#   packaging/build.sh x86_64-unknown-linux-musl
set -eu

say() { printf '%s\n' "$*"; }
die() { say "build.sh: $*" >&2; exit 1; }

[ $# -eq 1 ] || die "usage: packaging/build.sh <target>"
target=$1
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

cargo build --release --locked -p tiphys-cli --target "$target"
bin="target/$target/release/tiphys"
[ -x "$bin" ] || die "no binary at $bin"

# A release binary has to run on a server with nothing installed for it.
case "$target" in
*-musl)
    if ldd "$bin" >/dev/null 2>&1; then
        die "$bin is dynamically linked; a release binary must be static"
    fi
    ;;
esac

# The version the binary reports is the version of the workspace.
wanted=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
reported=$("$bin" --version)
[ "$reported" = "tiphys $wanted" ] || die "the binary says '$reported', Cargo.toml says $wanted"

mkdir -p dist
archive="tiphys-$target.tar.gz"
tar -C "target/$target/release" -czf "dist/$archive" tiphys
(cd dist && sha256sum "$archive" > "$archive.sha256")
say "built dist/$archive ($reported)"
