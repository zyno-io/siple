# CMake toolchain file for Windows release builds: link C/C++ dependencies built through the
# `cmake` crate (libopus) against the static MSVC runtime, matching `+crt-static` in
# .cargo/config.toml.
#
# Why: libopus requires CMake >= 3.16 (policy CMP0091 NEW), so the runtime comes from
# CMAKE_MSVC_RUNTIME_LIBRARY, not from the /MT flag the `cmake` crate passes. And libopus sets
# that variable itself from its own option OPUS_STATIC_RUNTIME (default OFF = DLL runtime, /MD).
# A /MD libopus in a /MT link fails with "unresolved external symbol __imp_floor". The cache
# entry below is set before libopus's `option()` runs, so it wins; CMAKE_MSVC_RUNTIME_LIBRARY
# covers any other CMake-built dependency.
#
# scripts/package-windows.ps1 sets CMAKE_TOOLCHAIN_FILE_x86_64-pc-windows-msvc to this file; the
# `cmake` crate forwards it (target-specific, so host build scripts are unaffected).
set(OPUS_STATIC_RUNTIME ON CACHE BOOL "Static MSVC runtime (Siple release builds)" FORCE)
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
