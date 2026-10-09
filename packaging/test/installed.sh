#!/bin/sh
# Installs Tiphys for real on this machine and checks what it finds, then
# removes the service again. It creates users, writes a sudoers file and
# starts a system service, so it is for a machine that is thrown away
# afterwards: CI runs it. It is not for a machine anyone uses.
#
#   sh packaging/test/installed.sh dist/tiphys-x86_64-unknown-linux-musl.tar.gz
set -eu

say() { printf '%s\n' "$*"; }
fail() { say "FAILED: $*" >&2; exit 1; }
ok() { say "ok: $*"; }

[ $# -eq 1 ] || fail "usage: installed.sh <release archive>"
archive=$1
owner=$(id -un)
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
state=/var/lib/tiphysd

# As the owner once the group they were added to has taken effect, which for
# a person is their next login.
as_owner() { sg tiphysd -c "$*"; }

sudo sh "$root/install.sh" --from "$archive" --owner "$owner"
sudo systemctl is-active --quiet tiphys.service || fail "the service is not running"
ok "installed, and the service is running"

[ "$(sudo stat -c '%U %a' "$state")" = "tiphysd 700" ] || fail "$state is not the daemon's alone"
sudo visudo --check --quiet || fail "the sudoers files do not check"
if sudo -u tiphys ls "$state" >/dev/null 2>&1; then
    fail "the user the agent acts as can list the state directory"
fi
ok "the state directory is the daemon's alone"

as_owner "tiphys daemon status" || fail "the owner cannot reach the daemon"
if sudo -u nobody /usr/local/bin/tiphys daemon status >/dev/null 2>&1; then
    fail "a user who is not an owner reached the daemon"
fi
ok "the owner reaches the daemon, and a stranger does not"

# A connection to a scripted model, and a key for it to be kept from.
python3 "$root/packaging/test/fake_provider.py" 18080 &
provider=$!
trap 'kill "$provider" 2>/dev/null || true' EXIT INT TERM
sudo -u tiphysd sh -c "cat > $state/config.toml" <<'CONFIG'
default_connection = "fake"

[connections.fake]
base_url = "http://127.0.0.1:18080/v1"
model = "fake/model"
local = true
CONFIG
sudo -u tiphysd sh -c "mkdir -p -m 700 $state/keys && printf 'sk-test-not-real' > $state/keys/fake && chmod 600 $state/keys/fake"
sudo systemctl restart tiphys.service
tries=0
until as_owner "tiphys daemon status" >/dev/null 2>&1; do
    tries=$((tries + 1))
    [ "$tries" -lt 40 ] || fail "the daemon did not come back after a restart"
    sleep 0.5
done

as_owner "tiphys -p 'how full is the root disk? write the answer to ~/disk.txt'" || fail "the turn did not complete"
[ "$(sudo stat -c '%U' /home/tiphys/disk.txt)" = "tiphys" ] || fail "the file was not written by the user the agent acts as"
sudo grep -q '%' /home/tiphys/disk.txt || fail "the file does not hold what df printed"
ok "a turn ran: a command and a file write, both as the user the agent acts as"

pgrep -u tiphys -f "tiphys worker" >/dev/null || fail "no worker is running as the user the agent acts as"
if sudo -u tiphys cat "$state/keys/fake" >/dev/null 2>&1; then
    fail "the user the agent acts as can read a key"
fi
ok "the worker runs as the other user, who cannot read the key"

sudo -u tiphysd env TIPHYS_HOME="$state" /usr/local/bin/tiphys log verify || fail "the action log does not verify"
[ "$(sudo -u tiphysd env TIPHYS_HOME="$state" /usr/local/bin/tiphys log | wc -l)" -ge 2 ] || fail "the action log is short"
ok "the action log is kept by the daemon and verifies"

sudo /usr/local/bin/tiphys daemon uninstall
if systemctl is-active --quiet tiphys.service; then
    fail "the service is still running after uninstall"
fi
[ ! -e /etc/sudoers.d/tiphys ] || fail "the sudoers file is still there after uninstall"
sudo test -f "$state/keys/fake" || fail "uninstall removed data"
ok "uninstalled, with the data kept"
say "All checks passed."
