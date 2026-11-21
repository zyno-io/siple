#!/usr/bin/env bash
# Smoke test of a built Flatpak bundle, inside the build image (as root, --privileged).
#
#   scripts/flatpak-smoke.sh BUNDLE
#
# Installs the bundle and checks, inside real sandboxes: the versions, files the app relies on in the
# runtime, and that `siple-cli` in a second sandbox reaches a headless engine through runtime.json and
# the control socket in ~/.var/app/io.zyno.siple (the same path as on the host).
set -euo pipefail

BUNDLE="$(cd "$(dirname "${1:?usage: $0 BUNDLE}")" && pwd)/$(basename "$1")"
APP=io.zyno.siple
version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$(dirname "$0")/../Cargo.toml" | head -1)"

export XDG_RUNTIME_DIR=/run/user/0
mkdir -p "$XDG_RUNTIME_DIR"
chmod 0700 "$XDG_RUNTIME_DIR"

flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak install -y --noninteractive --reinstall "$BUNDLE"

exec dbus-run-session -- bash -euo pipefail -s "$APP" "$version" <<'INNER'
APP="$1"; version="$2"
fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }
cli() { flatpak run --command=siple-cli "$APP" "$@"; }

echo "1. versions"
cli --version | grep -qF "$version" || fail "siple-cli --version lacks $version"
flatpak run --command=siple "$APP" --version | grep -qF "$version" || fail "siple --version lacks $version"

echo "2. files the app relies on"
flatpak run --command=sh "$APP" -c '
  command -v xdg-open >/dev/null &&
  test -s /etc/ssl/certs/ca-certificates.crt &&
  grep -qs pulse /etc/alsa/conf.d/99-pulseaudio-default.conf &&
  test -s /app/share/licenses/io.zyno.siple/THIRD-PARTY-NOTICES.html' \
  || fail "xdg-open, CA bundle, ALSA->pulse config or notices missing in the sandbox"

echo "3. a CLI in a second sandbox reaches the engine"
flatpak run --command=siple-cli "$APP" run --headless --no-audio >/tmp/siple-engine.log 2>&1 &
ok=0
for _ in $(seq 1 30); do
  if cli ctl ping >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || { cat /tmp/siple-engine.log >&2; fail "ctl ping from a second sandbox did not succeed in 30 s"; }
cli ctl status | python3 -c '
import json, sys
b = json.load(sys.stdin)["build"]
assert b["channel"] == "flatpak" and b["sandboxed"] is True and b["store"] is True, b'
cli ctl app.update_status | python3 -c '
import json, sys
u = json.load(sys.stdin)
assert u["enabled"] is False and u["source"] == "none" and u["disabled_reason"] == "managed_by_flatpak", u'

echo "4. the config dir is the same path inside and on the host"
d="$HOME/.var/app/$APP/config/siple"
test -f "$d/config.toml" || fail "$d/config.toml does not exist on the host side"
test -S "$d/control.sock" || fail "$d/control.sock is not a socket on the host side"

flatpak kill "$APP" || true
echo "smoke test passed"
INNER
