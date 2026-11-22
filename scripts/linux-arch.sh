# shellcheck shell=bash
# Sourced by the Linux packaging scripts: sets LINUX_ARCH to the release-asset spelling of the CPU,
# `x86_64` or `aarch64` (the names of Siple-<version>-linux-<arch>.tar.gz and .AppImage), from
# `uname -m` (Linux reports `aarch64`, macOS and some tools say `arm64`). SIPLE_UNAME_M overrides
# `uname -m` for a dry run of the naming logic.
# shellcheck disable=SC2034  # used by the scripts that source this file
case "${SIPLE_UNAME_M:-$(uname -m)}" in
  x86_64 | amd64) LINUX_ARCH=x86_64 ;;
  aarch64 | arm64) LINUX_ARCH=aarch64 ;;
  *)
    echo "ERROR: unsupported CPU architecture ${SIPLE_UNAME_M:-$(uname -m)} (x86_64 and aarch64 only)" >&2
    exit 1
    ;;
esac
