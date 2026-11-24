#!/usr/bin/env bash
# Builds the Flatpak like Flathub does (minus --sandbox, see below), lints it and smoke-tests it.
#
#   scripts/flatpak-build.sh [--arch ARCH] [--no-smoke]
#
# Runs as root inside Flathub's build image started with --privileged (CI: .github/workflows/flatpak.yml;
# a Linux host with docker: scripts/flatpak-local.sh). ARCH defaults to `flatpak --default-arch`.
# Needs packaging/flatpak/cargo-sources.json and THIRD-PARTY-NOTICES.html (scripts/flatpak-cargo-sources.sh
# and scripts/gen-licenses.sh). Writes flatpak-work/ and dist/flatpak/Siple-<label>-<arch>.flatpak.
# Environment: SIPLE_TAG (the release tag, for the file name), HOST_UID / HOST_GID (chown the outputs).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

ARCH="$(flatpak --default-arch)"
SMOKE=1
while [ $# -gt 0 ]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --no-smoke) SMOKE=0; shift ;;
    *) echo "usage: $0 [--arch ARCH] [--no-smoke]" >&2; exit 2 ;;
  esac
done

APP_ID=io.zyno.siple
MANIFEST=packaging/flatpak/io.zyno.siple.yml
EXCEPTIONS=packaging/flatpak/lint-exceptions.json
WORK=flatpak-work
for f in packaging/flatpak/cargo-sources.json packaging/flatpak/THIRD-PARTY-NOTICES.html; do
  [ -s "$f" ] || { echo "error: $f is missing or empty (see scripts/flatpak-cargo-sources.sh, scripts/gen-licenses.sh)" >&2; exit 1; }
done

version="$(sed -n '/^\[workspace.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -1)"
[ -n "$version" ] || { echo "error: cannot read the version from Cargo.toml" >&2; exit 1; }
LABEL="${SIPLE_TAG:-}"
LABEL="${LABEL#v}"
LABEL="${LABEL:-$version}"

echo "::group::validate desktop file, AppStream metadata and manifest"
desktop-file-validate packaging/linux/io.zyno.siple.desktop
appstreamcli validate --no-net --explain packaging/linux/io.zyno.siple.metainfo.xml
flatpak-builder-lint --exceptions --user-exceptions "$EXCEPTIONS" manifest "$MANIFEST"
echo "::endgroup::"

flatpak remote-add --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo

echo "::group::flatpak-builder ($ARCH)"
# Flathub builds with --sandbox, which refuses a local `type: dir` source outside the manifest's
# directory (this manifest's source is the checkout, `../..`), so the in-repo build leaves it out. Builds
# never have network access either way (the cargo sources are offline); the rendered Flathub manifest
# (scripts/flathub-manifest.sh) uses a git source and is built with --sandbox there.
# --compose-url-policy=full: absolute screenshot and icon URLs in the AppStream catalog (the linter
# fails appstream-external-screenshot-url / appstream-remote-icon-not-mirrored on the default partial URLs).
flatpak-builder --arch="$ARCH" --compose-url-policy=full --force-clean --disable-rofiles-fuse \
  --install-deps-from=flathub --state-dir="$WORK/state" \
  --mirror-screenshots-url=https://dl.flathub.org/media/ \
  --repo="$WORK/repo" "$WORK/build-$ARCH" "$MANIFEST"
echo "::endgroup::"

# Without this the repo lint fails appstream-screenshots-not-mirrored-in-ostree.
ostree commit --repo="$WORK/repo" --canonical-permissions --branch="screenshots/$ARCH" \
  "$WORK/build-$ARCH/files/share/app-info/media"

echo "::group::flatpak-builder-lint repo"
flatpak-builder-lint --exceptions --user-exceptions "$EXCEPTIONS" repo "$WORK/repo"
echo "::endgroup::"

# A store build has no updater: none of its markers may be in the binaries (cf. scripts/verify-macos-mas.sh).
for bin in "$WORK/build-$ARCH/files/bin/siple" "$WORK/build-$ARCH/files/bin/siple-cli"; do
  [ -f "$bin" ] || { echo "error: $bin was not built" >&2; exit 1; }
  for marker in '/repos/zyno-io/siple/releases' 'releases/download/' 'SHA256SUMS.minisig'; do
    if grep -qF -- "$marker" "$bin"; then
      echo "error: $bin contains the updater marker '$marker': the store feature did not take effect" >&2
      exit 1
    fi
  done
done
echo "no updater markers in the binaries"

mkdir -p dist/flatpak
BUNDLE="dist/flatpak/Siple-$LABEL-$ARCH.flatpak"
flatpak build-bundle --arch="$ARCH" --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
  "$WORK/repo" "$BUNDLE" "$APP_ID"
ls -l "$BUNDLE"

if [ "$SMOKE" = 1 ] && [ "$ARCH" = "$(flatpak --default-arch)" ]; then
  scripts/flatpak-smoke.sh "$BUNDLE"
fi

if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ]; then
  chown -R "$HOST_UID:$HOST_GID" dist/flatpak "$WORK" packaging/flatpak 2>/dev/null || true
fi
echo "built $BUNDLE"
