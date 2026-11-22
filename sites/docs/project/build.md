# Build from source

Build Siple yourself from the source code on GitHub.

You need Rust 1.94 or later, CMake (it builds a static libopus) and a C compiler.

| System | Install first |
|---|---|
| macOS 12+ | Xcode command line tools (`xcode-select --install`), then `brew install cmake` |
| Windows 10+ | The Rust MSVC toolchain, Visual Studio with the C++ workload, and CMake |
| Linux x86_64, aarch64 | `sudo apt install cmake pkg-config libasound2-dev build-essential` (Debian, Ubuntu) |

```sh
git clone https://github.com/zyno-io/siple && cd siple
cargo build --release
```

This builds `target/release/siple` (the app) and `target/release/siple-cli` (the command line and headless engine).

## Packages

The release packages come from these scripts:

| System | Package | Script |
|---|---|---|
| macOS | `Siple.app` (universal), `Siple-<version>-macos.dmg` and `.zip` | `scripts/package-macos.sh` |
| Windows (x64) | `Siple-<version>-windows-x64.zip` | `scripts/package-windows.ps1` |
| Windows (ARM) | `Siple-<version>-windows-arm64.zip` | `scripts/package-windows.ps1 -Arch arm64` |
| Linux (x86_64, aarch64) | `Siple-<version>-linux-<arch>.tar.gz` | `scripts/package-linux.sh` |
| Linux (AppImage, x86_64, aarch64) | `Siple-<version>-linux-<arch>.AppImage` | `scripts/package-appimage.sh` (after `package-linux.sh`, in `rust:1.100-bookworm`) |

On macOS, run the app bundle rather than the bare binary: macOS only grants the microphone to an app bundle. A build that is
not signed and notarized is blocked by Gatekeeper the first time; right-click it and choose **Open**, or allow it under **System Settings › Privacy & Security**. Because the microphone
permission is tied to the code signature, an ad-hoc signed build may ask again after each rebuild.

Want to contribute? See [Contributing](/project/contributing).
