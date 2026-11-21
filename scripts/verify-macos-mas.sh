#!/usr/bin/env bash
# shellcheck disable=SC2015  # `cond && pass || fail`: pass never fails
# Verifies a Mac App Store bundle (and optionally the .pkg built from it). Runs every check and prints a
# summary; exits 1 if any check failed.
#
#   scripts/verify-macos-mas.sh --unsigned dist/mas/Siple.app          # right after package-macos.sh MAS=1
#   scripts/verify-macos-mas.sh --local-test /tmp/copy/Siple.app       # after MAS_LOCAL_TEST=1 signing
#   scripts/verify-macos-mas.sh dist/mas/Siple.app dist/mas/Siple-<label>.pkg   # real signed output
#
# --unsigned    only build-level checks (SDK/minos, Info.plist, privacy manifest, no update-check code,
#               CLI embedded Info.plist); no signature, entitlement, profile or pkg checks.
# --local-test  signature + entitlement checks for MAS_LOCAL_TEST=1 output (no identifier keys, no profile).
#
# Needs: vtool, codesign, PlistBuddy, otool (Xcode command line tools); pkgutil for the .pkg.
#
# The CLI embedded-Info.plist check needs crates/siple-cli/build.rs (plan task T5) to have landed in the
# build; until then it FAILS with an explicit message. It is a hard requirement: without it the sandbox traps
# siple-cli at start.
set -uo pipefail

MODE=full
while [ $# -gt 0 ]; do
  case "$1" in
    --unsigned) MODE=unsigned; shift ;;
    --local-test) MODE=local; shift ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    *) break ;;
  esac
done
[ "${MAS_LOCAL_TEST:-0}" = "1" ] && [ "$MODE" = full ] && MODE=local
APP="${1:?usage: verify-macos-mas.sh [--unsigned|--local-test] <Siple.app> [out.pkg]}"
PKGFILE="${2:-}"
GROUP="VUDU4KEQ99.io.zyno.siple"
PB=/usr/libexec/PlistBuddy
GUI="$APP/Contents/MacOS/siple"
CLI="$APP/Contents/MacOS/siple-cli"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
FAILS=0

pass() { echo "  ok    $1"; }
fail() { echo "  FAIL  $1"; FAILS=$((FAILS + 1)); }
check() { # description, command...
  local d="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$d"; else fail "$d"; fi
}
plist_get() { "$PB" -c "Print :$2" "$1" 2>/dev/null; }

[ -d "$APP/Contents/MacOS" ] || { echo "error: $APP is not an app bundle" >&2; exit 1; }

echo "== build: Info.plist =="
IP="$APP/Contents/Info.plist"
[ "$(plist_get "$IP" LSMinimumSystemVersion)" = "13.0" ] && pass "LSMinimumSystemVersion 13.0" || fail "LSMinimumSystemVersion is not 13.0"
[ "$(plist_get "$IP" LSApplicationCategoryType)" = "public.app-category.business" ] && pass "category business" || fail "LSApplicationCategoryType"
[ "$(plist_get "$IP" CFBundleIdentifier)" = "io.zyno.siple" ] && pass "CFBundleIdentifier io.zyno.siple" || fail "CFBundleIdentifier"
bv="$(plist_get "$IP" CFBundleVersion)"
[[ "$bv" =~ ^[0-9]+$ ]] && pass "CFBundleVersion integer ($bv)" || fail "CFBundleVersion '$bv' is not an integer"
sv="$(plist_get "$IP" CFBundleShortVersionString)"
[[ "$sv" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] && pass "CFBundleShortVersionString $sv" || fail "CFBundleShortVersionString '$sv' is not X.Y.Z"
enc="$(plist_get "$IP" ITSAppUsesNonExemptEncryption)"
[ -n "$enc" ] && pass "ITSAppUsesNonExemptEncryption=$enc" || fail "ITSAppUsesNonExemptEncryption missing"
check "PrivacyInfo.xcprivacy present and valid" plutil -lint "$APP/Contents/Resources/PrivacyInfo.xcprivacy"
check "NSMicrophoneUsageDescription present" "$PB" -c "Print :NSMicrophoneUsageDescription" "$IP"
urltypes="$("$PB" -c "Print :CFBundleURLTypes" "$IP" 2>&1)"
for scheme in sip sips tel callto siple; do
  grep -qxE "[[:space:]]*$scheme" <<<"$urltypes" && pass "URL scheme $scheme declared" || fail "CFBundleURLTypes does not declare the $scheme scheme"
done

echo "== build: binaries (SDK >= 26.0, minos 13.0, both architectures) =="
for bin in "$GUI" "$CLI"; do
  n="$(basename "$bin")"
  if [ ! -f "$bin" ]; then fail "$n missing"; continue; fi
  vt="$(vtool -show-build "$bin" 2>&1)"
  entries="$(grep -c 'LC_BUILD_VERSION' <<<"$vt")"
  [ "$entries" -ge 1 ] || { fail "$n: no LC_BUILD_VERSION"; continue; }
  bad_minos="$(awk '/^ *minos /{ if ($2 != "13.0") print $2 }' <<<"$vt")"
  [ -z "$bad_minos" ] && pass "$n minos 13.0 ($entries slice(s))" || fail "$n minos is $bad_minos, expected 13.0"
  bad_sdk="$(awk '/^ *sdk /{ split($2, a, "."); if (a[1] + 0 < 26) print $2 }' <<<"$vt")"
  [ -z "$bad_sdk" ] && pass "$n SDK >= 26" || fail "$n SDK $bad_sdk is below 26.0 (Apple upload minimum)"
  archs="$(lipo -archs "$bin" 2>/dev/null)"
  if [[ "$archs" == *arm64* && "$archs" == *x86_64* ]]; then pass "$n universal ($archs)"
  elif [ "${MAS_ALLOW_SINGLE_ARCH:-0}" = "1" ]; then pass "$n arch: $archs (single arch allowed)"
  else fail "$n is not universal (archs: $archs)"; fi
done

echo "== build: store feature =="
# A store binary checks for updates against the App Store only. Markers (all in crates/siple-core/src/update/):
#   absent:  the GitHub releases API path (github.rs), any release asset download URL, the minisign signature
#            name (the in-app installer, phase 2) - compiled out by --features store.
#   present: the App Store lookup (appstore.rs: HOST and PATH are separate constants).
# (Not the bare github.com host: the config example text and the release page link mention it.)
check_store_markers() {
  local bin="$1" n="$2"
  [ -f "$bin" ] || return 0
  local m
  for m in '/repos/zyno-io/siple/releases' 'releases/download/' 'SHA256SUMS.minisig'; do
    if grep -aq -- "$m" "$bin"; then fail "$n contains '$m': built without --features store?"
    else pass "$n has no '$m'"; fi
  done
  for m in 'itunes.apple.com' '/lookup?id=6836164262'; do
    if grep -aq -- "$m" "$bin"; then pass "$n has the App Store lookup marker '$m'"
    else fail "$n lacks '$m': the App Store update check is missing"; fi
  done
}
check_store_markers "$GUI" siple
check_store_markers "$CLI" siple-cli

echo "== build: siple-cli embedded Info.plist (needed by the sandbox; crates/siple-cli/build.rs, task T5) =="
if [ -f "$CLI" ]; then
  sect="$(otool -s __TEXT __info_plist "$CLI" 2>/dev/null | grep -c '^[0-9a-f]\{8,\}')"
  if [ "$sect" -gt 0 ] && grep -aq '<key>CFBundleIdentifier</key>' "$CLI" && grep -aq 'io\.zyno\.siple-cli' "$CLI"; then
    pass "siple-cli has a __TEXT,__info_plist section with CFBundleIdentifier io.zyno.siple-cli"
  else
    fail "siple-cli has NO embedded __TEXT,__info_plist (expected CFBundleIdentifier io.zyno.siple-cli). It comes from crates/siple-cli/build.rs with --features store (plan task T5); a sandboxed CLI without it is killed at start"
  fi
fi

if [ "$MODE" = unsigned ]; then
  echo
  [ "$FAILS" = 0 ] && { echo "verify (unsigned): all checks passed"; exit 0; }
  echo "verify (unsigned): $FAILS check(s) FAILED" >&2; exit 1
fi

echo "== signature =="
check "codesign --verify --strict --deep" codesign --verify --strict --deep "$APP"
cdi="$(codesign -dvv "$CLI" 2>&1)"
grep -q 'Identifier=io.zyno.siple-cli' <<<"$cdi" && pass "siple-cli Identifier=io.zyno.siple-cli" || fail "siple-cli identifier"
grep -q 'Info.plist entries=[1-9]' <<<"$cdi" && pass "siple-cli signature binds its Info.plist" || fail "siple-cli: no 'Info.plist entries=' in codesign -dv (embedded plist missing?)"
grep -qE 'flags=0x[0-9a-f]+\(runtime\)' <<<"$cdi" && pass "siple-cli hardened runtime" || fail "siple-cli hardened runtime"

echo "== entitlements =="
codesign -d --entitlements :- "$APP" > "$TMP/app.ent" 2>/dev/null
codesign -d --entitlements :- "$CLI" > "$TMP/cli.ent" 2>/dev/null
for k in com.apple.security.app-sandbox com.apple.security.network.client com.apple.security.network.server com.apple.security.device.audio-input; do
  [ "$(plist_get "$TMP/app.ent" "$k")" = "true" ] && pass "app: $k" || fail "app: $k missing"
done
plist_get "$TMP/app.ent" "com.apple.security.application-groups" | grep -q "$GROUP" && pass "app: application-groups $GROUP" || fail "app: application-groups $GROUP"
for k in com.apple.security.app-sandbox com.apple.security.network.client; do
  [ "$(plist_get "$TMP/cli.ent" "$k")" = "true" ] && pass "cli: $k" || fail "cli: $k missing"
done
plist_get "$TMP/cli.ent" "com.apple.security.application-groups" | grep -q "$GROUP" && pass "cli: application-groups $GROUP" || fail "cli: application-groups"
[ "$(plist_get "$TMP/cli.ent" "com.apple.security.network.server")" = "true" ] && fail "cli: unexpected network.server"
if plist_get "$TMP/cli.ent" "com.apple.application-identifier" >/dev/null; then fail "cli: has com.apple.application-identifier (ITMS-90885)"; else pass "cli: no application-identifier"; fi
if plist_get "$TMP/cli.ent" "com.apple.developer.team-identifier" >/dev/null; then fail "cli: has team-identifier"; else pass "cli: no team-identifier"; fi
if [ "$MODE" = local ]; then
  if plist_get "$TMP/app.ent" "com.apple.application-identifier" >/dev/null; then fail "local test: app has application-identifier (would not launch without a profile)"; else pass "local test: app has no application-identifier"; fi
else
  [ "$(plist_get "$TMP/app.ent" "com.apple.application-identifier")" = "$GROUP" ] && pass "app: application-identifier $GROUP" || fail "app: application-identifier"
  [ "$(plist_get "$TMP/app.ent" "com.apple.developer.team-identifier")" = "VUDU4KEQ99" ] && pass "app: team-identifier" || fail "app: team-identifier"
  [ -f "$APP/Contents/embedded.provisionprofile" ] && pass "embedded.provisionprofile present" || fail "embedded.provisionprofile missing"
  auth="$(codesign -dvv "$APP" 2>&1 | sed -n 's/^Authority=//p' | head -1)"
  [[ "$auth" == "Apple Distribution:"* ]] && pass "app signed by $auth" || fail "app authority is '$auth', expected Apple Distribution"
fi

if [ "$MODE" = full ]; then
  echo "== installer package =="
  if [ -z "$PKGFILE" ]; then fail "no .pkg given (pass it as the second argument, or use --local-test)"
  else
    sig="$(pkgutil --check-signature "$PKGFILE" 2>&1)"
    grep -q 'Status: signed' <<<"$sig" && pass "pkg signed" || fail "pkg not signed"
    grep -qE '3rd Party Mac Developer Installer|Mac Installer Distribution' <<<"$sig" && pass "pkg signed with an installer distribution certificate" || fail "pkg certificate is not a Mac installer distribution certificate"
    if pkgutil --expand-full "$PKGFILE" "$TMP/pkg" >/dev/null 2>&1; then
      inner="$(find "$TMP/pkg" -maxdepth 3 -name Siple.app -type d | head -1)"
      if [ -n "$inner" ]; then
        check "pkg payload app verifies" codesign --verify --strict --deep "$inner"
        [ "$(plist_get "$inner/Contents/Info.plist" CFBundleVersion)" = "$bv" ] && pass "pkg payload CFBundleVersion $bv" || fail "pkg payload CFBundleVersion differs"
      else fail "Siple.app not found in the pkg payload"; fi
    else fail "pkgutil --expand-full failed"; fi
  fi
fi

echo
[ "$FAILS" = 0 ] && { echo "verify: all checks passed"; exit 0; }
echo "verify: $FAILS check(s) FAILED" >&2; exit 1
