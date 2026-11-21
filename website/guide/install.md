# Install

Download Siple for your computer and get it running.

**Download:** [siple.app](https://siple.app/#download) picks the right build for your computer. Every build is also on
[GitHub Releases](https://github.com/zyno-io/siple/releases/latest). Mac App Store, Microsoft Store and Flathub editions are
coming soon.

| Computer | Download | Needs |
|---|---|---|
| Mac (Apple silicon and Intel) | macOS (`.dmg`) | macOS 12 or later |
| Windows PC (Intel, AMD) | Windows x64 (`.zip`) | Windows 10 or later |
| Windows on ARM (Surface Pro X / 11, Snapdragon) | Windows ARM64 (`.zip`) | Windows 11 |
| Linux PC | Linux x86_64 (`.tar.gz`) | Debian 12+, Ubuntu 23.04+ (glibc 2.36 or later) |

<OsTabs>
<template #mac>

1. Open the downloaded `.dmg`.
2. Drag **Siple** onto **Applications**.
3. Open Siple from Applications, Launchpad or Spotlight.

The Mac build is signed and notarized by Apple, so it opens without warnings.

**Microphone.** macOS asks for microphone access on your first call: choose **Allow**. If you chose Don't Allow, turn Siple
on under **System Settings › Privacy & Security › Microphone**.

</template>
<template #windows>

1. Unzip the download into a folder of your own, for example `%LOCALAPPDATA%\Programs\Siple`. (A folder you can write to
   lets Siple update itself later; `Program Files` needs an administrator.)
2. Run `siple.exe`. Pin it to Start or the taskbar if you like.
3. When **Windows Defender Firewall** asks about Siple, allow it on **private and public networks**. Without this, call
   audio can be blocked: you hear nothing, or the other person hears nothing.

On a Windows on ARM device use the `arm64` download; the `x64` one also runs there, but slower. Siple is signed by
Signal24 LLC; if SmartScreen still warns, choose **More info › Run anyway**.

</template>
<template #linux>

Unpack the tarball and run Siple:

```sh
tar xzf Siple-<version>-linux-x86_64.tar.gz
cd siple-<version>
./bin/siple                    # the app
./bin/siple-cli run --headless # or: no window, for servers and automation
```

**Add it to your app launcher** (and let it open `sip:`, `tel:` and `siple:` links):

```sh
mkdir -p ~/.local/bin
cp bin/* ~/.local/bin/
install -Dm644 share/applications/io.zyno.siple.desktop ~/.local/share/applications/io.zyno.siple.desktop
install -Dm644 share/pixmaps/io.zyno.siple.png ~/.local/share/pixmaps/io.zyno.siple.png
```

The launcher entry runs `siple %u`, so `~/.local/bin` must be on your `PATH`. To make Siple the default for phone links, see [Phone links](/guide/links); for setup links run `xdg-mime default io.zyno.siple.desktop x-scheme-handler/siple`.

::: details Packages Siple needs on Linux
- **Sound:** `libasound2` (`libasound2t64` on Ubuntu 24.04+ and Debian 13), and `ca-certificates`.
- **The app window:** `libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-egl1 libx11-6 libx11-xcb1 libxcursor1
  libxrandr2 libxi6`, a Vulkan or GL driver (`mesa-vulkan-drivers`, or `libegl1 libgl1-mesa-dri`) and `xdg-utils`.
- **Headless `siple-cli` only:** just ALSA (`libasound2`).
:::

::: details Flatpak (Flathub, coming soon)
Siple will also be on Flathub as `io.zyno.siple`:

```sh
flatpak install flathub io.zyno.siple
flatpak run io.zyno.siple
```

- **Updates** come from your software center or `flatpak update`. Siple makes no update check of its own.
- **Settings folder:** `~/.var/app/io.zyno.siple/config/siple/`. To move over from the tarball, copy `config.toml` there; passwords
  kept in the system keyring carry over. If you turn on launch at login in the Flatpak, it writes `~/.config/autostart/io.zyno.siple.desktop`,
  the same file the tarball uses; the tarball leaves an entry written by the Flatpak alone (it neither overwrites nor removes it), so
  switch the setting off in the Flatpak before you go back to the tarball.
- **The CLI:** `flatpak run --command=siple-cli io.zyno.siple ctl status`. For an MCP client, see
  [Quick start with MCP](/agents/mcp-quickstart).
- **Limits.** The `[speech]` commands are not available. Files Siple reads (TLS files, `file:` audio) must be inside
  `~/.var/app/io.zyno.siple/`, or granted with `flatpak override --user --filesystem=/path:ro io.zyno.siple`.
- **Phone links:** `xdg-mime default io.zyno.siple.desktop x-scheme-handler/sip`, and the same for `sips`, `tel`, `callto` and `siple`.
- Do not run the tarball and the Flatpak at the same time with the same accounts.
- It runs on Ubuntu 22.04 too.
:::

::: details Run headless in Docker
The image holds `siple-cli` only. Build it from a checkout and run it with host networking, so calls need no NAT handling:

```sh
docker build -t siple:dev --target runtime -f packaging/linux/Dockerfile .
docker run -d --name siple --network host -v siple-data:/data siple:dev
docker exec siple siple-cli ctl status
```

The config file is `/data/config.toml`. To drive it from an agent, see [Quick start with MCP](/agents/mcp-quickstart).
:::

</template>
</OsTabs>

## Next: add your account

The first time Siple opens, a short setup asks for your account and checks your audio:
[Set up your account](/guide/first-run).

## Build from source

Prefer to build it yourself? You need Rust and CMake; the steps for each system are in
[Build from source](/project/build).
