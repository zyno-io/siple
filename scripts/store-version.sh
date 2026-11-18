#!/usr/bin/env bash
# Computes the store version numbers for a build and prints them as KEY=VALUE lines.
#
#   scripts/store-version.sh [TAG] [RUN_NUMBER]     e.g. v0.2.0-rc.1 (empty TAG = branch / dispatch build)
#   scripts/store-version.sh --self-test
#
# Output keys: KIND (final|rc|dev), LABEL, SHORT_VERSION (CFBundleShortVersionString),
#   BUNDLE_VERSION (CFBundleVersion, integer), MSIX_VERSION (X.Y.Z.W), MSIX_STORE_ELIGIBLE (true|false).
#
# Scheme (both stores only accept increasing numbers, so this is deterministic):
#   BUNDLE_VERSION = X*1000000 + Y*10000 + Z*100 + (final: 99 | rc N: N | dev: 0)   (Y, Z <= 99; 1 <= N <= 98)
#   MSIX final     X.Y.Z.0           the only form the Microsoft Store accepts (4th field is reserved for it)
#   MSIX rc N      X.Y.Z.N           sideload only
#   MSIX dev       X.Y.Z.(1 + RUN_NUMBER % 65000)   sideload only
# A tag must match the workspace version in Cargo.toml (v<version> or v<version>-rc.<n>), else exit 1.
# SIPLE_CARGO_VERSION overrides the Cargo.toml lookup (used by --self-test).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

compute() { # tag run cargo_version
  local tag="$1" run="${2:-0}" cargo="$3"
  local ver rc="" kind label
  [[ "$cargo" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || { echo "error: Cargo version '$cargo' is not X.Y.Z" >&2; return 1; }
  local x="${BASH_REMATCH[1]}" y="${BASH_REMATCH[2]}" z="${BASH_REMATCH[3]}"
  if [ "$y" -gt 99 ] || [ "$z" -gt 99 ]; then
    echo "error: minor and patch must be <= 99 for the BUNDLE_VERSION scheme ($cargo)" >&2; return 1
  fi
  if [ -n "$tag" ]; then
    if [[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)-rc\.([0-9]+)$ ]]; then
      ver="${BASH_REMATCH[1]}"; rc="${BASH_REMATCH[2]}"; kind=rc
      # no leading zeros; 1..98
      if ! [[ "$rc" =~ ^[1-9][0-9]*$ ]] || [ "$rc" -gt 98 ]; then
        echo "error: rc number must be 1..98 ($tag)" >&2; return 1
      fi
    elif [[ "$tag" =~ ^v([0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
      ver="${BASH_REMATCH[1]}"; kind=final
    else
      echo "error: tag '$tag' is not v<X.Y.Z> or v<X.Y.Z>-rc.<N>" >&2; return 1
    fi
    if [ "$ver" != "$cargo" ]; then
      echo "error: tag $tag does not match the Cargo.toml version $cargo" >&2; return 1
    fi
    label="${tag#v}"
  else
    kind=dev
    [[ "$run" =~ ^[0-9]+$ ]] || { echo "error: RUN_NUMBER must be an integer for a dev build" >&2; return 1; }
    label="$cargo-dev$run"
  fi
  local suffix msix
  case "$kind" in
    final) suffix=99; msix="$x.$y.$z.0" ;;
    rc)    suffix=$((10#$rc)); msix="$x.$y.$z.$((10#$rc))" ;;
    dev)   suffix=0; msix="$x.$y.$z.$((1 + 10#$run % 65000))" ;;
  esac
  echo "KIND=$kind"
  echo "LABEL=$label"
  echo "SHORT_VERSION=$cargo"
  echo "BUNDLE_VERSION=$((x * 1000000 + y * 10000 + z * 100 + suffix))"
  echo "MSIX_VERSION=$msix"
  echo "MSIX_STORE_ELIGIBLE=$([ "$kind" = final ] && echo true || echo false)"
}

cargo_version() {
  sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$ROOT/Cargo.toml" | head -1
}

self_test() {
  local fail=0
  check() { # name expected_output_oneline tag run cargo
    local out
    out="$(compute "$3" "$4" "$5" 2>/dev/null | tr '\n' ' ')" || out="ERROR"
    if [ "$out" != "$2" ]; then
      echo "FAIL $1: got [$out] want [$2]"; fail=1
    else
      echo "ok   $1"
    fi
  }
  fails() { # name tag run cargo
    if compute "$2" "$3" "$4" >/dev/null 2>&1; then echo "FAIL $1: expected an error"; fail=1; else echo "ok   $1"; fi
  }
  check "v0.2.0-rc.1" "KIND=rc LABEL=0.2.0-rc.1 SHORT_VERSION=0.2.0 BUNDLE_VERSION=20001 MSIX_VERSION=0.2.0.1 MSIX_STORE_ELIGIBLE=false " v0.2.0-rc.1 0 0.2.0
  check "v0.2.0-rc.2" "KIND=rc LABEL=0.2.0-rc.2 SHORT_VERSION=0.2.0 BUNDLE_VERSION=20002 MSIX_VERSION=0.2.0.2 MSIX_STORE_ELIGIBLE=false " v0.2.0-rc.2 0 0.2.0
  check "v0.2.0" "KIND=final LABEL=0.2.0 SHORT_VERSION=0.2.0 BUNDLE_VERSION=20099 MSIX_VERSION=0.2.0.0 MSIX_STORE_ELIGIBLE=true " v0.2.0 0 0.2.0
  check "v0.2.1" "KIND=final LABEL=0.2.1 SHORT_VERSION=0.2.1 BUNDLE_VERSION=20199 MSIX_VERSION=0.2.1.0 MSIX_STORE_ELIGIBLE=true " v0.2.1 0 0.2.1
  check "dev run 7" "KIND=dev LABEL=0.2.0-dev7 SHORT_VERSION=0.2.0 BUNDLE_VERSION=20000 MSIX_VERSION=0.2.0.8 MSIX_STORE_ELIGIBLE=false " "" 7 0.2.0
  check "dev run wraps" "KIND=dev LABEL=0.2.0-dev65000 SHORT_VERSION=0.2.0 BUNDLE_VERSION=20000 MSIX_VERSION=0.2.0.1 MSIX_STORE_ELIGIBLE=false " "" 65000 0.2.0
  check "1.10.3 final" "KIND=final LABEL=1.10.3 SHORT_VERSION=1.10.3 BUNDLE_VERSION=1100399 MSIX_VERSION=1.10.3.0 MSIX_STORE_ELIGIBLE=true " v1.10.3 0 1.10.3
  fails "tag/Cargo mismatch" v0.2.1 0 0.2.0
  fails "rc.0" v0.2.0-rc.0 0 0.2.0
  fails "rc.99" v0.2.0-rc.99 0 0.2.0
  fails "rc.01" v0.2.0-rc.01 0 0.2.0
  fails "bad tag" v0.2 0 0.2.0
  fails "minor > 99" v0.100.0 0 0.100.0
  fails "patch > 99" v0.2.100 0 0.2.100
  fails "dev non-numeric run" "" abc 0.2.0
  # ordering: rc.1 < rc.2 < final < next rc.1
  local a b c d
  a="$(compute v0.2.0-rc.1 0 0.2.0 | sed -n 's/^BUNDLE_VERSION=//p')"
  b="$(compute v0.2.0-rc.2 0 0.2.0 | sed -n 's/^BUNDLE_VERSION=//p')"
  c="$(compute v0.2.0 0 0.2.0 | sed -n 's/^BUNDLE_VERSION=//p')"
  d="$(compute v0.2.1-rc.1 0 0.2.1 | sed -n 's/^BUNDLE_VERSION=//p')"
  if [ "$a" -lt "$b" ] && [ "$b" -lt "$c" ] && [ "$c" -lt "$d" ]; then echo "ok   monotonic"; else echo "FAIL monotonic"; fail=1; fi
  if [ "$fail" = 0 ]; then echo "self-test passed"; else echo "self-test FAILED" >&2; return 1; fi
}

case "${1:-}" in
  --self-test) self_test ;;
  -h|--help) sed -n '2,17p' "$0" ;;
  *) compute "${1:-}" "${2:-0}" "${SIPLE_CARGO_VERSION:-$(cargo_version)}" ;;
esac
