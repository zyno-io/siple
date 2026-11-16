# Install

Siple runs on macOS 12+, Windows 10+ (x64) and Linux x86_64. Packages are attached to
[GitHub Releases](https://github.com/zyno-io/siple/releases) once release signing is set up; until a package is
listed there, [build from source](#build-from-source).

The package scripts produce these artifacts:

| OS | Artifact | Script |
|---|---|---|
| macOS | `Siple.app` (universal), `Siple-<ver>-macos.zip` | `scripts/package-macos.sh` |
| Windows | `Siple-<ver>-windows-x64.zip` | `scripts/package-windows.ps1` |
| Linux | `Siple-<ver>-linux-x86_64.tar.gz` | `scripts/package-linux.sh` |

## macOS

Unzip and drag `Siple.app` to Applications. Use the app bundle rather than a bare binary: macOS grants the
microphone to app bundles, and the bundle carries the permission text and entitlement. Accept the microphone prompt on
your first call. If it was denied, enable it under System Settings, Privacy & Security, Microphone.

A build that is not signed and notarized is blocked by Gatekeeper on first launch. If you trust the download, right-click
the app and choose Open, or allow it under System Settings, Privacy & Security. The microphone grant is tied to the code
signature, so an ad-hoc signed build may ask again after each rebuild.

## Windows

Unzip anywhere and run `siple.exe`. Windows Defender Firewall asks about `siple.exe` on first start: allow it for private
and public networks, or inbound audio (RTP) is blocked and you hear nothing or the far end hears nothing. An unsigned
build may show a SmartScreen warning; choose More info, then Run anyway, if you trust the download.

## Linux

The tarball unpacks to `siple-<ver>/{bin,share}`. It is built against glibc 2.36, so it runs on Debian 12+ and Ubuntu 23.04+.

```sh
tar xzf Siple-<ver>-linux-x86_64.tar.gz
cd siple-<ver>
./bin/siple                 # GUI
./bin/siple-cli run --headless
```

To get a launcher entry and `sip:` / `tel:` link handling, install the desktop file and icon from the tarball:

```sh
install -Dm644 share/applications/io.zyno.siple.desktop ~/.local/share/applications/io.zyno.siple.desktop
install -Dm644 share/pixmaps/io.zyno.siple.png ~/.local/share/pixmaps/io.zyno.siple.png
```

The desktop file runs `siple %u` and declares the `sip`, `sips`, `tel` and `callto` URL schemes, so `siple` must be on your
`PATH` (for example by copying `bin/*` to `~/.local/bin`). See [sip: and tel: links](/guide/links) to make it the default handler.

Runtime packages: `libasound2` (`libasound2t64` on Ubuntu 24.04+ and Debian 13) and `ca-certificates`. The GUI also needs
`libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-egl1 libx11-6 libx11-xcb1 libxcursor1 libxrandr2 libxi6`,
a Vulkan or GL driver (`mesa-vulkan-drivers` or `libegl1 libgl1-mesa-dri`) and `xdg-utils`. The headless `siple-cli`
needs only ALSA.

### Docker (headless)

The image holds `siple-cli` only. Build it from a checkout and run it with host networking so SIP and RTP need no NAT handling:

```sh
docker build -t siple:dev --target runtime -f packaging/linux/Dockerfile .
docker run -d --name siple --network host -v siple-data:/data siple:dev
docker exec siple siple-cli ctl status
```

The config is `/data/config.toml`. See [Quick start with MCP](/agents/mcp-quickstart) for driving it from an agent.

## Build from source

Requirements: Rust 1.94+, CMake (builds a static libopus) and a C compiler.

| OS | Prerequisites |
|---|---|
| macOS 12+ | Xcode command line tools, `brew install cmake` |
| Windows 10+ (x64) | Rust MSVC toolchain, Visual Studio with the C++ workload, CMake |
| Linux x86_64 | `sudo apt install cmake pkg-config libasound2-dev build-essential` (Debian/Ubuntu) |

```sh
git clone https://github.com/zyno-io/siple && cd siple
cargo build --release        # target/release/siple (GUI) and siple-cli (console)
```

Next: [First run and accounts](/guide/first-run).
