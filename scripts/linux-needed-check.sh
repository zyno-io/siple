#!/usr/bin/env bash
# Dependency guard of the Linux editions: fails if an ELF file's DT_NEEDED list contains anything
# outside the allowlist below, and reports the highest GLIBC_ version the files use.
#
#   scripts/linux-needed-check.sh FILE...
#
# The allowlist is the complete host contract of both Linux artifacts (tarball and AppImage): glibc,
# ALSA and libgcc_s from the host; the dynamic loader is the one of this machine's architecture
# (x86_64 or aarch64, see linux-arch.sh). The glibc floor is that of the build image (bookworm,
# 2.36), reported below. Needs objdump.
set -euo pipefail

[ "$#" -gt 0 ] || { echo "usage: $0 FILE..." >&2; exit 2; }
# shellcheck source=scripts/linux-arch.sh
. "$(dirname "$0")/linux-arch.sh"
case "$LINUX_ARCH" in
  x86_64) LOADER=ld-linux-x86-64.so.2 ;;
  aarch64) LOADER=ld-linux-aarch64.so.1 ;;
esac
ALLOWED="libasound.so.2 libc.so.6 libm.so.6 libgcc_s.so.1 $LOADER"
bad=0
maxglibc=""
for bin in "$@"; do
  needed="$(objdump -p "$bin" | awk '/NEEDED/ {print $2}')"
  echo "==> $(basename "$bin") NEEDED: ${needed//$'\n'/ }"
  for n in $needed; do
    case " $ALLOWED " in
      *" $n "*) ;;
      *) echo "ERROR: $(basename "$bin") needs unexpected library $n" >&2; bad=1 ;;
    esac
  done
  g="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
  [ -z "$maxglibc" ] || g="$(printf '%s\n%s\n' "$maxglibc" "$g" | sort -V | tail -1)"
  maxglibc="$g"
done
[ "$bad" = 0 ] || { echo "FAILED: NEEDED allowlist check ({$ALLOWED})" >&2; exit 1; }
echo "==> NEEDED allowlist check passed; highest GLIBC_ version used: GLIBC_$maxglibc"
