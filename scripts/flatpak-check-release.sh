#!/usr/bin/env bash
# Fails when the AppStream metainfo lacks the release being prepared or tagged.
#
#   scripts/flatpak-check-release.sh [TAG]
#
# The version comes from Cargo.toml (same sed as release.yml). A TAG, when given, must be
# v<version> or v<version>-rc.N. For a final tag, or when the first heading of CHANGELOG.md is
# `## <version> (YYYY-MM-DD)`, packaging/linux/io.zyno.siple.metainfo.xml must contain
# `<release version="<version>" date="YYYY-MM-DD"`. `## Unreleased` has nothing to check.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TAG="${1:-}"
META=packaging/linux/io.zyno.siple.metainfo.xml
version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
[ -n "$version" ] || { echo "error: cannot read the version from Cargo.toml" >&2; exit 1; }

final=0
if [ -n "$TAG" ]; then
  case "$TAG" in
    "v$version") final=1 ;;
    "v$version"-rc.[0-9]*) ;;
    *) echo "error: tag $TAG is not v$version or v$version-rc.N" >&2; exit 1 ;;
  esac
fi

heading="$(grep -m1 '^## ' CHANGELOG.md || true)"
date=""
if [[ "$heading" =~ ^##\ $version\ \(([0-9]{4}-[0-9]{2}-[0-9]{2})\)$ ]]; then
  date="${BASH_REMATCH[1]}"
fi

if [ "$final" = 0 ] && [ -z "$date" ]; then
  echo "nothing to check ($heading)"
  exit 0
fi
if [ -z "$date" ]; then
  echo "error: CHANGELOG.md has no '## $version (YYYY-MM-DD)' heading for the tag $TAG" >&2
  exit 1
fi
if ! grep -qF "<release version=\"$version\" date=\"$date\"" "$META"; then
  echo "error: $META has no <release version=\"$version\" date=\"$date\"...>: add it (newest first)" >&2
  exit 1
fi
echo "$META lists release $version ($date)"
