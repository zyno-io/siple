#!/usr/bin/env bash
# Signs the MAS bundle (dist/mas/Siple.app from `MAS=1 scripts/package-macos.sh`) for the Mac App Store and
# wraps it in a signed installer package for Transporter / App Store Connect.
#
#   scripts/sign-macos-mas.sh dist/mas/Siple.app dist/mas/Siple-<label>-mas.pkg
#   MAS_LOCAL_TEST=1 scripts/sign-macos-mas.sh /tmp/copy-of/Siple.app        # local sandbox test, no pkg
#
# Environment:
#   MAS_APP_IDENTITY        "Apple Distribution: ..." name or SHA-1 of the app signing identity
#   MAS_INSTALLER_IDENTITY  "3rd Party Mac Developer Installer: ..." / "Mac Installer Distribution: ..." name
#   MAS_PROFILE             path to the Mac App Store Connect .provisionprofile for io.zyno.siple
#   MAS_KEYCHAIN            optional keychain holding the identities (CI: a temporary keychain)
#   MAS_LOCAL_TEST=1        sign with ANY identity (MAS_APP_IDENTITY; a Developer ID is fine) using entitlements
#                           WITHOUT the application/team identifier keys, no profile, no pkg. Only for running
#                           the sandboxed app on this Mac; the result can never be uploaded. Signs IN PLACE:
#                           work on a copy of the bundle.
#
# Signs inside-out (never --deep): the bundled siple-cli with its own identifier and entitlements, then the bundle.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PKG="$ROOT/packaging/macos"
APP="${1:?usage: sign-macos-mas.sh <Siple.app> [out.pkg]}"
OUT="${2:-}"
LOCAL="${MAS_LOCAL_TEST:-0}"
APP_GROUP_ID="VUDU4KEQ99.io.zyno.siple"

[ -d "$APP/Contents/MacOS" ] || { echo "error: $APP is not an app bundle" >&2; exit 1; }
: "${MAS_APP_IDENTITY:?MAS_APP_IDENTITY not set}"
KC=()
[ -n "${MAS_KEYCHAIN:-}" ] && KC=(--keychain "$MAS_KEYCHAIN")
if [ "$LOCAL" != "1" ]; then
  : "${MAS_INSTALLER_IDENTITY:?MAS_INSTALLER_IDENTITY not set}"
  : "${MAS_PROFILE:?MAS_PROFILE not set}"
  [ -n "$OUT" ] || { echo "error: output .pkg path required" >&2; exit 1; }
  [ -f "$MAS_PROFILE" ] || { echo "error: profile not found: $MAS_PROFILE" >&2; exit 1; }
fi

# SHA-1 of the signing identity's certificate.
app_sha1() {
  if [[ "$MAS_APP_IDENTITY" =~ ^[0-9A-Fa-f]{40}$ ]]; then
    echo "$MAS_APP_IDENTITY" | tr 'a-f' 'A-F'; return
  fi
  security find-identity -v -p codesigning ${MAS_KEYCHAIN:+"$MAS_KEYCHAIN"} \
    | awk -v n="$MAS_APP_IDENTITY" 'index($0, "\"" n) { print $2; exit }'
}

CLI_ENT="$PKG/entitlements-mas-cli.plist"
APP_ENT="$PKG/entitlements-mas.plist"
SIGN_OPTS=(--force --options runtime --timestamp)
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT

if [ "$LOCAL" = "1" ]; then
  echo "==> MAS_LOCAL_TEST=1: no identifier keys, no profile, no pkg (NOT uploadable)"
  APP_ENT="$TMP/entitlements-mas-local.plist"
  cp "$PKG/entitlements-mas.plist" "$APP_ENT"
  /usr/libexec/PlistBuddy -c "Delete :com.apple.application-identifier" "$APP_ENT"
  /usr/libexec/PlistBuddy -c "Delete :com.apple.developer.team-identifier" "$APP_ENT"
  SIGN_OPTS=(--force --options runtime --timestamp=none)
else
  # 1. The profile must match what we are about to sign: app id, expiry, signing certificate.
  security cms -D -i "$MAS_PROFILE" > "$TMP/profile.plist"
  SHA1="$(app_sha1)"
  [ -n "$SHA1" ] || { echo "error: identity '$MAS_APP_IDENTITY' not found in the keychain" >&2; exit 1; }
  python3 -I - "$TMP/profile.plist" "$APP_GROUP_ID" "$SHA1" <<'PY'
import datetime, hashlib, plistlib, sys
path, want_id, sha1 = sys.argv[1:4]
p = plistlib.load(open(path, "rb"))
got = p.get("Entitlements", {}).get("com.apple.application-identifier")
if got != want_id:
    sys.exit(f"error: profile application identifier is {got!r}, expected {want_id!r}")
exp = p["ExpirationDate"]
now = datetime.datetime.now(exp.tzinfo) if exp.tzinfo else datetime.datetime.utcnow()
days = (exp - now).days
if days < 0:
    sys.exit(f"error: provisioning profile expired {-days} days ago ({exp:%Y-%m-%d})")
if days < 30:
    print(f"::warning::provisioning profile expires in {days} days ({exp:%Y-%m-%d}); renew it")
certs = {hashlib.sha1(c).hexdigest().upper() for c in p.get("DeveloperCertificates", [])}
if sha1.upper() not in certs:
    sys.exit("error: the signing certificate is not listed in the provisioning profile (DeveloperCertificates)")
print(f"==> profile OK: {got}, expires {exp:%Y-%m-%d} ({days} days), contains the signing certificate")
PY
  # 2. Embed it.
  cp "$MAS_PROFILE" "$APP/Contents/embedded.provisionprofile"
fi

find "$APP" -print0 | xargs -0 xattr -c

echo "==> signing siple-cli (io.zyno.siple-cli)"
codesign "${SIGN_OPTS[@]}" ${KC[@]+"${KC[@]}"} --sign "$MAS_APP_IDENTITY" \
  --identifier io.zyno.siple-cli --entitlements "$CLI_ENT" "$APP/Contents/MacOS/siple-cli"
echo "==> signing $APP"
codesign "${SIGN_OPTS[@]}" ${KC[@]+"${KC[@]}"} --sign "$MAS_APP_IDENTITY" --entitlements "$APP_ENT" "$APP"
codesign --verify --strict --deep "$APP"

if [ "$LOCAL" = "1" ]; then
  echo "==> signed for local sandbox testing only: $APP"
  exit 0
fi

echo "==> productbuild -> $OUT"
rm -f "$OUT"
productbuild --component "$APP" /Applications --sign "$MAS_INSTALLER_IDENTITY" ${KC[@]+"${KC[@]}"} "$OUT"
pkgutil --check-signature "$OUT" | sed -n '1,4p'
echo "==> $OUT"
