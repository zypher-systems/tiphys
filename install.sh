#!/bin/sh
# Installs Tiphys on an Ubuntu server.
#
#   curl -fsSL https://raw.githubusercontent.com/zypher-systems/tiphys/main/install.sh | sh
#
# It downloads the release for this machine, checks it against the release's
# checksums, and puts the binary in /usr/local/bin. Then it hands over to
# `tiphys daemon install`, which sets up the service. It asks for sudo where
# it needs root and nowhere else. Your data is never touched: updating is
# running this again.
#
# Options:
#   --version vX.Y.Z   install that release instead of the latest
#   --from FILE        install from a release archive already on this machine
#   --prefix DIR       install under DIR instead of /usr/local
#   --owner NAME       the user who will talk to Tiphys (default: you)
#   --no-service       install the binary only
#   --uninstall        remove the binary and the service; data is kept
set -eu

REPO="zypher-systems/tiphys"
version=""
from=""
prefix="/usr/local"
owner=""
service=1
uninstall=0

say() { printf '%s\n' "$*"; }
die() { say "install.sh: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "this needs '$1', which is not installed"; }

while [ $# -gt 0 ]; do
    case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; version=$2; shift 2 ;;
    --from) [ $# -ge 2 ] || die "--from needs a file"; from=$2; shift 2 ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix=$2; shift 2 ;;
    --owner) [ $# -ge 2 ] || die "--owner needs a user name"; owner=$2; shift 2 ;;
    --no-service) service=0; shift ;;
    --uninstall) uninstall=1; shift ;;
    -h | --help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown option: $1" ;;
    esac
done

# Runs a command as root: directly if we are root, through sudo if not.
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        need sudo
        sudo "$@"
    fi
}

# Runs a command that writes under the prefix: as root only if it has to be.
in_prefix() {
    if [ -w "$prefix" ] || { [ ! -e "$prefix" ] && [ -w "$(dirname "$prefix")" ]; }; then
        "$@"
    else
        as_root "$@"
    fi
}

bin="$prefix/bin/tiphys"

if [ "$uninstall" -eq 1 ]; then
    if [ -x "$bin" ] && "$bin" daemon uninstall --help >/dev/null 2>&1; then
        as_root "$bin" daemon uninstall
    fi
    in_prefix rm -f "$bin"
    say "Tiphys is removed. Its data was left where it is."
    exit 0
fi

[ "$(uname -s)" = "Linux" ] || die "Tiphys runs on Linux; this is $(uname -s)"
case "$(uname -m)" in
x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
*) die "there is no release for $(uname -m)" ;;
esac
if [ -r /etc/os-release ] && ! grep -q '^ID=ubuntu$' /etc/os-release; then
    say "Note: Tiphys is built for Ubuntu. This is another system; its rules about"
    say "commands will ask about more than they need to."
fi

need tar
need sha256sum
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
archive="tiphys-$target.tar.gz"

if [ -n "$from" ]; then
    [ -r "$from" ] || die "cannot read $from"
    cp "$from" "$work/$archive"
    say "Installing from $from"
else
    need curl
    if [ -z "$version" ]; then
        # The latest release's tag, read from the redirect its page gives.
        version=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's|.*/tag/||')
        case "$version" in v[0-9]*) ;; *) die "could not find the latest release of Tiphys" ;; esac
    fi
    base="https://github.com/$REPO/releases/download/$version"
    say "Downloading Tiphys $version for $target"
    curl -fsSL -o "$work/$archive" "$base/$archive" || die "there is no $archive in release $version"
    curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS" || die "release $version has no SHA256SUMS"
    # Only the line for this archive: the others name files that are not here.
    (cd "$work" && grep " $archive\$" SHA256SUMS | sha256sum -c - >/dev/null) ||
        die "$archive does not match the release's checksum; nothing was installed"
fi

tar -C "$work" -xzf "$work/$archive" tiphys || die "$archive is not a Tiphys release archive"
installed=$("$work/tiphys" --version) || die "the downloaded binary does not run on this machine"

in_prefix mkdir -p "$prefix/bin"
# Written beside the old binary and moved over it, so a running Tiphys keeps
# the file it started from until it is restarted.
in_prefix install -m 755 "$work/tiphys" "$bin.new"
in_prefix mv -f "$bin.new" "$bin"
say "Installed $installed at $bin"

if [ "$service" -eq 0 ]; then
    say "The service was not set up. Run '$bin' to use Tiphys by itself."
    exit 0
fi
if ! "$bin" daemon install --help >/dev/null 2>&1; then
    say "This version of Tiphys does not set up a service. Run '$bin' to use it by itself,"
    say "or '$bin daemon run' to run the daemon by hand."
    exit 0
fi
[ -n "$owner" ] || owner=${SUDO_USER:-$(id -un)}
say "Setting up the service, with $owner as the owner"
as_root "$bin" daemon install --owner "$owner"
