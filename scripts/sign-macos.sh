#!/usr/bin/env bash
# Signs a macOS .app bundle or .dmg with rcodesign (Developer ID, hardened runtime).
#
#   scripts/sign-macos.sh dist/Siple.app
#   scripts/sign-macos.sh dist/Siple-<version>-macos.dmg
#
# Environment:
#   APPLE_CERTIFICATE_P12_PATH       PKCS#12 file with the "Developer ID Application" certificate + key.
#   APPLE_CERTIFICATE_PASSWORD_PATH  file holding the P12 password (no trailing newline).
#   REQUIRE_SIGNING=1                fail instead of skipping when the certificate is not configured
#                                    (set by the release workflow so a tag can never ship unsigned).
#
# Without the certificate this prints a notice and exits 0, so dev builds keep working.
set -euo pipefail

TARGET="${1:?usage: sign-macos.sh <Siple.app|file.dmg>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENTITLEMENTS="$ROOT/packaging/macos/entitlements.plist"

if [[ -z "${APPLE_CERTIFICATE_P12_PATH:-}" ]]; then
  if [[ "${REQUIRE_SIGNING:-0}" == "1" ]]; then
    echo "error: APPLE_CERTIFICATE_P12_PATH not set and REQUIRE_SIGNING=1" >&2
    exit 1
  fi
  echo "Skipping macOS signing: APPLE_CERTIFICATE_P12_PATH not set (dev build)"
  exit 0
fi
: "${APPLE_CERTIFICATE_PASSWORD_PATH:?APPLE_CERTIFICATE_PASSWORD_PATH not set}"
[[ -e "$TARGET" ]] || { echo "error: $TARGET not found" >&2; exit 1; }

COMMON=(--p12-file "$APPLE_CERTIFICATE_P12_PATH" --p12-password-file "$APPLE_CERTIFICATE_PASSWORD_PATH")
# rcodesign's default timestamp server is Apple's (http://timestamp.apple.com/ts01), required for notarization.

case "$TARGET" in
  *.app)
    echo "==> signing $TARGET (hardened runtime)"
    # Scoped settings: nested Mach-Os do NOT inherit flags/entitlements/identifier from the bundle,
    # so the siple-cli lines are required (not redundant) for it to get runtime + its own identifier. The main executable's identifier comes from
    # CFBundleIdentifier (io.zyno.siple). rcodesign signs nested code first, then the bundle.
    rcodesign sign "${COMMON[@]}" \
      --code-signature-flags runtime \
      --code-signature-flags Contents/MacOS/siple-cli:runtime \
      --entitlements-xml-file "$ENTITLEMENTS" \
      --entitlements-xml-file "Contents/MacOS/siple-cli:$ENTITLEMENTS" \
      --binary-identifier "Contents/MacOS/siple-cli:io.zyno.siple-cli" \
      "$TARGET"
    ;;
  *.dmg)
    echo "==> signing $TARGET"
    rcodesign sign "${COMMON[@]}" "$TARGET"
    ;;
  *)
    echo "error: unsupported target (expected .app or .dmg): $TARGET" >&2
    exit 1
    ;;
esac
echo "Signed: $TARGET"
