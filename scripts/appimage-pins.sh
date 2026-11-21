# shellcheck shell=bash
# Pins of the AppImage type2-runtime, sourced by package-appimage.sh and appimage-smoke.sh.
# https://github.com/AppImage/type2-runtime/releases/tag/20251108
# Bump: docs/RELEASING.md, "Bumping the pins", AppImage runtime. Never use the `continuous` tag.
# shellcheck disable=SC2034  # used by the scripts that source this file
RUNTIME_RELEASE=20251108
RUNTIME_COMMIT=dd6cebedcbddde9c82f89b011e8e1d40b6e43868
RUNTIME_KEY_FPR=570C77ACEA40C0F1B758902CBF96CCA56490F695

RUNTIME_X86_64_SIZE=944632
RUNTIME_X86_64_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
RUNTIME_X86_64_SIG_SHA256=de8262d60b5f82bcff11b77598aedef86d2c375fc246d6a675f6cac68b6dfda7

RUNTIME_AARCH64_SIZE=936456
RUNTIME_AARCH64_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444
RUNTIME_AARCH64_SIG_SHA256=512946512baf245b3304e974ca4f4c19bcdbee3a73dc6e3bfae642b92f7e2c6f

# runtime_pins ARCH (x86_64|aarch64): sets RUNTIME_URL, RUNTIME_SIZE, RUNTIME_SHA256 and
# RUNTIME_SIG_SHA256 for that architecture.
runtime_pins() {
  local up
  case "$1" in
    x86_64) up=X86_64 ;;
    aarch64) up=AARCH64 ;;
    *) echo "ERROR: no AppImage runtime pinned for $1" >&2; return 1 ;;
  esac
  RUNTIME_URL="https://github.com/AppImage/type2-runtime/releases/download/$RUNTIME_RELEASE/runtime-$1"
  local v
  for v in SIZE SHA256 SIG_SHA256; do
    eval "RUNTIME_$v=\$RUNTIME_${up}_$v"
  done
}
