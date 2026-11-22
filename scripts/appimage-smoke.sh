#!/usr/bin/env bash
# Smoke test of a built AppImage (x86_64 or aarch64, on a machine of that architecture) in the release job's container (no FUSE there: everything runs
# through the runtime's extract-and-run mode).
#
#   scripts/appimage-smoke.sh APPIMAGE
#
# Checks the runtime, the versions of siple and siple-cli (through --cli and through a symlink named
# siple-cli), that the GUI libraries winit dlopens are bundled (and nothing on the excludelist is), and that
# a headless engine started through the AppImage reports the appimage edition.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=scripts/appimage-pins.sh
. "$ROOT/scripts/appimage-pins.sh"
# shellcheck source=scripts/appimage-libs.sh
. "$ROOT/scripts/appimage-libs.sh"
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

echo "3. bundled GUI libraries"
mkdir "$t/x"
(cd "$t/x" && "$A" --appimage-extract >/dev/null) || fail "--appimage-extract failed"
root="$t/x/squashfs-root"
lib="$root/usr/lib"
doc="$root/usr/share/doc/siple"
excl="$(excluded_sonames "$ROOT/packaging/appimage/excludelist")"
for so in $GUI_DLOPEN_SONAMES; do
  [ -f "$lib/$so" ] || fail "$so is not bundled in usr/lib"
  grep -q "^$so " "$doc/BUNDLED-LIBRARIES.txt" || fail "$so is not listed in BUNDLED-LIBRARIES.txt"
done
[ -n "$(find "$lib" -type f -name '*.so*')" ] || fail "usr/lib has no libraries"
[ -z "$(find "$lib" ! -type f ! -type d)" ] || fail "usr/lib holds something besides regular files"
for f in "$lib"/*; do
  b="$(basename "$f")"
  ! printf '%s\n' "$excl" | grep -qxF "$b" || fail "$b is on the excludelist and must not be in usr/lib"
  # Every NEEDED of a bundled library is bundled or comes from the host (excludelist, glibc).
  for n in $(objdump -p "$f" | awk '/NEEDED/ {print $2}'); do
    [ -f "$lib/$n" ] || printf '%s\n' "$excl" | grep -qxF "$n" || fail "$b needs $n, which is neither bundled nor excluded"
  done
done
# shellcheck disable=SC2013  # package names, no spaces
for pkg in $(awk '/^lib[^ ]*\.so/ {print $2}' "$doc/BUNDLED-LIBRARIES.txt" | sort -u); do
  [ -s "$doc/bundled-licenses/$pkg/copyright" ] || fail "no licence file for the bundled package $pkg"
done
echo "   bundled: $(find "$lib" -type f | sed 's|.*/||' | sort | tr '\n' ' ')"
# Every soname that appears as a string in the binary (what dlopen is given) is bundled, excluded
# or explicitly allowed (GUI_STRINGS_ALLOWED): a new dlopen after a dependency bump fails here.
for so in $(strings -a "$root/usr/bin/siple" | grep -E '^lib[[:alnum:]_+.-]+\.so\.[0-9]+$' | sort -u); do
  [ -f "$lib/$so" ] || printf '%s\n' "$excl" | grep -qxF "$so" ||
    printf '%s\n' "${GUI_STRINGS_ALLOWED[@]}" | grep -qxF "$so" ||
    fail "usr/bin/siple mentions $so, which is not bundled, not on the excludelist and not in GUI_STRINGS_ALLOWED (scripts/appimage-libs.sh)"
done
# AppRun hands Siple the bundled path and keeps the host's for its children.
grep -q 'SIPLE_HOST_LD_LIBRARY_PATH' "$root/AppRun" || fail "AppRun does not save the host LD_LIBRARY_PATH"

echo "4. a headless engine through the AppImage"
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
