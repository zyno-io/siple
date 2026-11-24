#!/usr/bin/env bash
# Generates THIRD-PARTY-NOTICES.html (all dependency license texts) for binary distributions.
#
#   scripts/gen-licenses.sh [--if-available] [output-file]   default: dist/THIRD-PARTY-NOTICES.html
#
# Needs cargo-about:  cargo install cargo-about --locked
# Configuration: about.toml (accepted licenses, must match deny.toml) and about.hbs (template).
# The output is a build artifact: it is not committed. The package-*.sh scripts call this with
# --if-available and ship the result inside the package.
#
# --if-available: when cargo-about is missing, warn and exit 0 (dev builds), unless SIPLE_RELEASE=1,
# in which case it fails loudly (release builds must never ship without notices).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

IF_AVAILABLE=0
if [ "${1:-}" = "--if-available" ]; then IF_AVAILABLE=1; shift; fi
OUT="${1:-$ROOT/dist/THIRD-PARTY-NOTICES.html}"

if ! cargo about --version >/dev/null 2>&1; then
  msg="cargo-about is not installed. Install it with: cargo install cargo-about --locked"
  if [ "$IF_AVAILABLE" = 1 ] && [ "${SIPLE_RELEASE:-0}" != "1" ]; then
    echo "WARNING: $msg" >&2
    echo "WARNING: skipping THIRD-PARTY-NOTICES.html (dev build; set SIPLE_RELEASE=1 to make this fatal)" >&2
    exit 0
  fi
  echo "ERROR: $msg" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT")"
cargo about generate --locked -c about.toml about.hbs > "$OUT"
echo "wrote $OUT"
