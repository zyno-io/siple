#!/usr/bin/env bash
# Builds the Flatpak on a Linux host that has docker but no flatpak. Everything runs in containers;
# outputs stay in this checkout (flatpak-work/, dist/flatpak/) plus one named docker volume that
# caches the runtime and the Rust extension.
#
#   scripts/flatpak-local.sh [--clean]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Same image as .github/workflows/flatpak.yml (pinned by digest; see docs/RELEASING.md to bump).
IMAGE=ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-26.08@sha256:6ed6921bb5a9e1a916989971844f159b854b7b225880c381fa34f7a158863039
VOL="siple-flatpak-$(id -u)"

if [ "${1:-}" = "--clean" ]; then
  docker run --rm -v "$ROOT":/src "$IMAGE" rm -rf /src/flatpak-work /src/dist/flatpak
  docker volume rm "$VOL" >/dev/null 2>&1 || true
  exit 0
fi

# 1. Offline cargo sources, as the invoking user (no root-owned files). Helper image python:3.13-slim,
# pinned by its multi-arch index digest (resolved 2026-11-21 with `docker buildx imagetools inspect python:3.13-slim`).
PY_IMAGE=python:3.13-slim@sha256:e48f59cdba69a3cbd9ed2cf0f348d90d45a8d350d98134cf1070bc106ee9ed90
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$ROOT":/src -w /src "$PY_IMAGE" \
  scripts/flatpak-cargo-sources.sh

# 2. Notices (cargo-about when installed, else a placeholder: development builds only).
scripts/gen-licenses.sh --if-available packaging/flatpak/THIRD-PARTY-NOTICES.html
if [ ! -s packaging/flatpak/THIRD-PARTY-NOTICES.html ]; then
  echo "WARNING: writing a placeholder instead of the third-party notices (development build)" >&2
  echo "<p>development build: notices not generated</p>" > packaging/flatpak/THIRD-PARTY-NOTICES.html
fi

# 3. Build, lint and smoke-test.
docker run --rm --privileged --name "siple-flatpak-$(id -u)-$$" \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" -e SIPLE_TAG="${SIPLE_TAG:-}" \
  -v "$VOL":/var/lib/flatpak -v "$ROOT":/src -w /src "$IMAGE" scripts/flatpak-build.sh
