#!/usr/bin/env bash
# Smoke test of a built AppImage (x86_64 or aarch64, on a machine of that architecture) in the release job's container (no FUSE there: everything runs
# through the runtime's extract-and-run mode).
#
#   scripts/appimage-smoke.sh APPIMAGE
#
# Checks the runtime, the versions of siple and siple-cli (through --cli and through a symlink named
# siple-cli), and that a headless engine started through the AppImage reports the appimage edition.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/appimage-pins.sh
. "$ROOT/scripts/appimage-pins.sh"
A="$(cd "$(dirname "${1:?usage: $0 APPIMAGE}")" && pwd)/$(basename "$1")"
version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$ROOT/Cargo.toml" | head -1)"
fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

t="$(mktemp -d)"
engine=""
cleanup() {
  [ -z "$engine" ] || kill "$engine" 2>/dev/null || true
  # The runtime never removes its extraction directory; TMPDIR keeps it in $t.
  rm -rf "$t"
}
trap cleanup EXIT
mkdir "$t/tmp" "$t/cfg"
export TMPDIR="$t/tmp" XDG_CONFIG_HOME="$t/cfg"

echo "1. runtime"
"$A" --appimage-version 2>&1 | grep -qF "type2-runtime/commit/${RUNTIME_COMMIT:0:7}" ||
  fail "the runtime is not type2-runtime ${RUNTIME_COMMIT:0:7}"

echo "2. versions"
# The runtime acts on --appimage-* only as the first argument that starts with --.
"$A" --appimage-extract-and-run --version | grep -qF "siple $version" || fail "siple --version lacks $version"
export APPIMAGE_EXTRACT_AND_RUN=1
"$A" --cli --version | grep -qF "siple-cli $version" || fail "--cli --version lacks $version"
ln -s "$A" "$t/siple-cli"
"$t/siple-cli" --version | grep -qF "siple-cli $version" || fail "the siple-cli symlink lacks $version"

echo "3. a headless engine through the AppImage"
"$A" --cli run --headless --no-audio >"$t/engine.log" 2>&1 &
engine=$!
ok=0
for _ in $(seq 1 30); do
  if "$A" --cli ctl ping >/dev/null 2>&1; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || { cat "$t/engine.log" >&2; fail "the engine did not answer ctl ping within 30 s"; }
"$A" --cli ctl status | python3 -I -c '
import json, sys
b = json.load(sys.stdin)["build"]
assert b["channel"] == "appimage", b
assert b["sandboxed"] is False, b
assert b["store"] is False, b
' || fail "status.build is not {channel: appimage, sandboxed: false, store: false}"
"$A" --cli ctl app.update_status | python3 -I -c '
import json, sys
s = json.load(sys.stdin)
assert s["source"] == "github", s
' || fail "app.update_status source is not github"
echo "SMOKE OK"
