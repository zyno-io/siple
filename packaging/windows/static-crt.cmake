# CMake toolchain file for Windows release builds: link C/C++ dependencies built through the
# `cmake` crate (libopus) against the static MSVC runtime, matching `+crt-static` in
# .cargo/config.toml.
#
# Why: libopus requires CMake >= 3.16, so policy CMP0091 is NEW and CMake picks the runtime from
# CMAKE_MSVC_RUNTIME_LIBRARY (default: the DLL runtime, /MD) instead of the /MT flag the `cmake`
# crate passes. A /MD libopus in a /MT link fails with "unresolved external symbol __imp_floor".
#
# scripts/package-windows.ps1 sets CMAKE_TOOLCHAIN_FILE_x86_64-pc-windows-msvc to this file; the
# `cmake` crate forwards it (target-specific, so host build scripts are unaffected).
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
