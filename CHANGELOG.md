<!-- siple-rename: keep (this file documents the legacy ZynoPhone names on purpose) -->
# Changelog

## Unreleased

### New

- A badge on the call shows when an agent is listening to it: "Agent listening", "Agent transcribing" or "Agent recording" appears on the call (and in the tray menu) while an AI agent connected through the control API, MCP or `siple-cli` captures the call's audio with `call.listen`, `call.rx_audio`, `call.stream` or `call.record_start`. `call.get` and `call.list` report it as `agent_capture`.

## 0.2.5 (2026-11-23)

### New

- Direct downloads check for updates about every hour, from siple.app, instead of once a day. GitHub is asked at most once a day per run, as a cross-check of siple.app's list (a stale site can hide an update for a day at most) or instead of siple.app when it does not answer. The store editions still check once a day.
- Publishing a release candidate refreshes the update feed on siple.app at once, so the prerelease channel sees it.
- The tray / menu-bar menu shows the version under the app name.

## 0.2.4 (2026-11-22)

### Fixed

- Auto-hold in a call between two of your own accounts holds the leg that dialled. It could hold the receiving leg instead,
  depending on which leg was registered first.
- Windows: the restart after an in-place update waits the full time for the old process to exit (a wait could end a moment
  early).

## 0.2.3 (2026-11-22)

### Fixed

- Settings: the explanation next to a switch (for example under **Updates**) wraps under its label instead of running off
  the right edge of the window.

## 0.2.2 (2026-11-22)

AppImage for Linux, and Linux on ARM.

### New

- AppImage edition for Linux, x86_64 and aarch64 (`Siple-<version>-linux-x86_64.AppImage`, `-linux-aarch64.AppImage`), and an aarch64 tarball (`Siple-<version>-linux-aarch64.tar.gz`), both built natively. Download one file, `chmod +x` and run it.
  It updates itself in place like the tarball: signed checksum list, a test run of the new file, the file replaced in one
  step, and a restart only after the call. The CLI is reached with `--cli` or a small `siple-cli` wrapper script.
  Launch at login starts the AppImage file. Same baseline as the tarball (glibc 2.36, Debian 12+, Ubuntu 23.04+). It needs
  FUSE 3 (`fusermount3`), or `--appimage-extract-and-run` without it. `status.build.channel` is `appimage`.

- The in-app updater picks the Linux file of its own CPU architecture. x86_64 installs resolve the same files as before.

### Fixed

- The AppImage starts on minimal desktops that lack `libxkbcommon-x11` and the other X11/Wayland helper libraries: it now bundles
  them (only the libraries every desktop provides are taken from the host). A missing GUI library, in the AppImage or the
  tarball, now gives a clear message naming the library and its Debian/Ubuntu and Fedora package (also as a dialog when
  started from a file manager) instead of a crash.
- `ui.hide` on a desktop without a system tray (StatusNotifier) host says so, instead of asking to set `[ui] tray = true`
  when it already is.
- The Linux tarball can update itself when it was started from a terminal that is itself an AppImage.

## 0.2.1 (2026-11-21)

### Fixed

- After an in-place update, Siple reopens its window if it was open when the update restarted it, even with `start_hidden` (minimized to the taskbar counts as open).
  If it was hidden in the tray, it starts hidden again (when a tray is available).

## 0.2.0 (2026-11-21)

Plain-language errors, `siple:` setup links and a Flatpak edition.

### New

- Plain-language errors. A failed registration, call, busy lamp or transfer, and a failed action, now say what went wrong and
  what to try ("Wrong username or password", "Busy", "Extension not found") instead of a bare SIP code. The server's answer
  moves to a details line or a hover. A busy lamp whose subscription failed reads "Failed" (its tooltip and the banner above
  the extensions name the problem). `siple-cli problems` lists the catalog (`--json`, `--markdown`).
- Setup links: an administrator can send a `siple:provision?d=...` link that fills in an account. Clicking it (or pasting it
  into **Paste a setup link** in the setup, or **Add from setup link** in Settings › Accounts) opens a **Check this setup
  link** review screen that leads with the server the account would connect to. Nothing is saved or sent until the user
  continues (Continue unlocks one second after the screen appears), types the password and runs the test. A link carries
  only nine account fields (`username`, `domain`, `display_name`, `auth_username`, `registrar`, `outbound_proxy`,
  `transport`, `srtp`, `profile`), never a password, and any other key is refused. A link's `display_name` may not contain
  quotes, backslashes, angle brackets or invisible characters, and hosts may not use IPv4 shorthand (`127.1`,
  `2130706433`); the password must be typed before Test; a new link is refused once the user has continued past Review.
  See `website/guide/setup-links.md`.
  - Settings › Accounts has **Copy setup link**: a link for an existing account, without its password.
  - New `siple-cli setup-link build|parse` (offline).
  - `siple:` is registered on every edition (Info.plist, the MSIX manifest, the `.desktop` file, and under HKCU on the
    Windows zip, where `handle_links = false` removes it).
- The account form of the setup and of Settings › Accounts › New account has a **Registrar** field.
- Flatpak edition `io.zyno.siple`, in preparation for Flathub: built from source on the freedesktop 26.08 runtime
  for x86_64 and aarch64; CI builds a test bundle. Sandboxed: network, audio (PulseAudio / PipeWire), tray,
  notifications and the Secret Service, no access to your home folder. Settings live in
  `~/.var/app/io.zyno.siple/config/siple/`. Flatpak updates it (no update check of its own); `[speech]` commands are
  not available; `launch_at_login` goes through the desktop's Background portal; the CLI is
  `flatpak run --command=siple-cli io.zyno.siple`.
- With developer tools on, the account button's tooltip shows when it registered, the last renewal and when the registration
  expires, and BLF key tooltips show when the subscription was confirmed, the last refresh and its expiry.

### Changed

- Developer tools (`[ui] developer_tools`, **Settings › Developer**) now gate the footer **Log**, **Open logs** and **Open config**
  buttons. The Settings pages, the account menu and the config-error banner keep their own Open config / Open logs buttons.

### Fixed

- From rsipstack 0.8.6 (updated from 0.7.3):
  - Display names in From, To and Contact are written with `"` and `\` escaped, so a name containing a quote can no
    longer break the header. Received headers, URIs and Content-Length are checked against the RFC 3261 grammar. Over
    UDP a message that fails is dropped; over TCP or TLS (where a Content-Length is now required) it closes the
    connection, and Siple reconnects and registers again.
  - Digest authentication: `qop=auth` is sent unquoted, `MD5-sess` / `SHA-256-sess` and `nc` above 9 work, and a
    `stale=true` challenge is answered again.
  - Over UDP, requests other than INVITE (REGISTER, BYE, OPTIONS...) keep being retransmitted after a 100 Trying, as
    RFC 3261 asks, and a timer race that could stall transactions is gone.
  - In-dialog requests follow the Contact a re-INVITE or UPDATE refreshes, out-of-order requests get a 500,
    and UPDATE works before the call is answered.
  - IPv6: a listener on `::` also serves IPv4, IPv4 peers show as IPv4, and `Via received` is written per RFC.
- Two fixes from rsipstack's main branch, carried until its next release:
  - Registering over TCP or TLS works with Kamailio registrars that put the observed flow in the Contact as
    `received="sip:IP:port;transport=tls"` (the quoted `;` split the parameter, and rsipstack 0.8.6 refused the answer).
  - Hanging up an outgoing call while it rings now sends the CANCEL to the same next hop as the INVITE (outbound
    proxy or registrar). Before, it went wherever the dialled address resolved, so with an outbound proxy, or a
    registrar host other than the domain, the other side could keep ringing.

### API

- A `problem {kind, title, hint}` object on failed registrations (`registration.problem`), ended calls (`end.problem`),
  BLF subscriptions (`subscription.last_problem`), transfer errors (`last_error.problem`), `account.test` results (`problem`)
  and SIP-level JSON-RPC errors (`error.data.problem`). Existing fields keep their names and values. Kinds are stable; see
  `docs/PROBLEMS.md`.
- New methods `provision.parse` and `provision.build`. `app.open_uri` now also takes `siple:provision` links (up to
  2048 bytes), opens the review screen only (no parameter skips it), reports `kind` (`dial` or `provision`), and its
  `invalid_params` errors carry `data.code` (and `data.field`) for a refused link.
- `blf.list` `subscription` carries `subscribed_at` and `last_refreshed_at`.
- `build.channel` can be `flatpak`; `app.update_status` `disabled_reason` can be `managed_by_flatpak`.

### Dependencies and packaging

- Dependencies updated, and rsipstack to 0.8.6 (fixes above).
- Linux: hicolor app icons and a complete AppStream file (releases, screenshots, content rating) in `packaging/linux/`;
  the tarball's `share/metainfo` file gains the same metadata.

## 0.1.2 (2026-11-19)

- Store editions now tell you about updates. The Mac App Store edition asks Apple's public lookup of Siple's listing about
  once a day (one anonymous request carrying only `User-Agent: Siple/<version>`); the Microsoft Store edition asks Windows' Store
  service, which contacts Microsoft (Siple sends nothing itself). The header pill then reads "Open App Store" or "Open Microsoft
  Store". Direct builds are unchanged: GitHub, as before. `[updates] check = false` turns every edition's check off.
- API: `app.update_status` and `status.update` report `source` (`github`, `app_store`, `microsoft_store`, `none`), `action`,
  `store_url`, `disabled_reason` and `mandatory`; `app.update_available` carries `action` and a `null` version for the
  Microsoft Store. Store editions report `enabled: true`.
- Release assets: `SHA256SUMS.minisig`, a minisign signature of `SHA256SUMS` (public key in `packaging/minisign.pub`).
- The `siple` binary accepts a hidden `--wait-pid <pid>` option (waits for that process to exit before starting).
- Direct downloads on macOS, Windows and Linux can now install an update in place: the update pill's **Install and restart**
  (or `app.install_update`) downloads the new version from GitHub, checks the minisign signature of `SHA256SUMS`, the file's SHA-256
  and your system's publisher check (macOS Developer ID and notarization; Windows Authenticode with a pinned publisher), replaces
  Siple and restarts it, never during a call. An in-place install needs a release that carries `SHA256SUMS.minisig` (0.1.2 and later),
  so updating from 0.1.1 still goes through the release page once. A copy installed with a package manager, in a folder you cannot write, from a disk image, in a container or
  headless still opens the release page, with the reason. Siple never asks for administrator rights. New `[updates] install`
  (default `true`) turns it off. The Mac App Store and Microsoft Store editions contain no downloader or installer.
- API: `app.install_update`, `app.cancel_update`, the events `app.update_progress` and `app.updated`, the `install` object of
  `app.update_status` and `install_state` in `status.update`; `action` can now be `install`. See `docs/CONFIG.md`, "In-place install".
- Privacy: the direct downloads download the new version from GitHub when you, or a script or agent you run, chooses to install it.
- New app icon: the same handset the app uses on its Call and Answer buttons, centred on a lit teal-to-blue tile with a
  status lamp; the small sizes (16 to 40 px, tray, header) use a simplified mark. The header brand mark is now the icon itself.
- Interface polish: text fields share the 32 pt height of the drop-downs and buttons beside them; the Extensions view switch is
  one segmented control; panels are divided by single lines (no doubled borders); icons replace the text arrows and plus
  signs ("New call", "New account", "Back"); Stop echo test shows a stop icon; the advanced account options open with a
  disclosure arrow. Settings' "+ Add account" is now "New account".

## 0.1.1 (2026-11-18)

First release for the Mac App Store and the Microsoft Store. The direct downloads behave as before.

- Store editions: a sandboxed Mac App Store build (macOS 13+, config in the app group container) and a Microsoft
  Store MSIX bundle (x64 and arm64, config in the package's LocalState). Both are updated by their store and make no
  update check. The Mac App Store edition has no `[speech]` hooks.
- API: `hello` and `status` report `build.channel` (`direct`, `mas` or `msix`); new error kind `unsupported`
  (code -32006) for methods an edition cannot run.
- Privacy policy: https://siple.app/privacy/.

## 0.1.0 (2026-11-17)

First public release: a native SIP softphone for macOS (universal, notarized), Windows (x64 and arm64, signed) and Linux
(x86_64), under FSL-1.1-ALv2.

- Several accounts and lines, hold, mute, DTMF, attended and blind transfer, busy lamps (BLF), TLS and SRTP.
- Everything the GUI does is a JSON-RPC method: `siple-cli ctl`, an MCP server (`siple-cli mcp`) and an agent skill
  (`siple-cli skill`) for LLM agents that prefer a CLI to a running server.
- The Lines rail collapses (collapsed by default) and accounts can be reordered (Settings > Accounts, `config.edit` op `move_account`).
- Update check against GitHub Releases on direct-download builds (below).
- Windows zips now use `/` in entry names, so `unzip` on macOS and Linux extracts the `docs/` folder correctly.
- Contributions are accepted under a CLA (`CLA.md`); see `CONTRIBUTING.md`.

### Update check

Siple can tell you that a newer release exists (direct-download builds only; detection, nothing is downloaded or installed). `[updates]`
has `check = true` and `channel` (`stable` or `prerelease`; unset follows the running build). One anonymous HTTPS GET of the GitHub
releases list, 30 s after start and then every 24 h, uses ETags and backs off when rate limited. A pill in the header
("Update 0.2.0": Download, Release notes, Skip this version) and a Settings > Advanced section show the result. API:
`app.update_status`, `app.check_updates`, the event `app.update_available` and `status.update`. Release-candidate builds now know
their own `-rc.N` (the release workflow passes the tag as `SIPLE_BUILD_TAG`), shown in `status.version` and `--version`. The `store`
cargo feature (Mac App Store, Microsoft Store builds) compiles the check out; see `docs/RELEASING.md`.

### Signed releases

`.github/workflows/release.yml` replaces `package.yml`: a `v*` tag builds all platforms, signs the macOS app (Developer ID,
hardened runtime, notarized and stapled; `.zip` and `.dmg`) and the Windows exes (Azure Artifact Signing), writes
`SHA256SUMS`, attaches build-provenance attestations and creates a draft GitHub Release. Manual runs still build unsigned
dev packages. `scripts/package-macos.sh` now also makes a `.dmg` and gains `BUNDLE_ONLY=1`; `scripts/package-windows.ps1`
gains `-StageOnly` / `-ZipOnly -RequireSigned`. See `docs/RELEASING.md`.

### Keyboard shortcuts

The GUI has the keyboard map: answer, hang up, mute, hold, transfer, line select (`Mod+1`-`9`, `Mod+[` / `]`), focus dial, filter extensions and Settings, plus DTMF typing on an active line. `[ui.shortcuts]` overrides each chord (hot); unknown actions and malformed chords are config errors. See `docs/CONFIG.md`.

### Licence changed to FSL-1.1-ALv2

Siple is now source-available under the Functional Source License 1.1 with an Apache-2.0 future licence
(FSL-1.1-ALv2), replacing MIT. Each release converts to Apache-2.0 two years after publication. See `LICENSE`.
Packages now ship `LICENSE` and `THIRD-PARTY-NOTICES.html`. The one-shot ZynoPhone-to-Siple rename script and the
internal planning documents were removed from the repository.

### Renamed from ZynoPhone to Siple

The product is now **Siple**. App ID: `io.zyno.siple`. Code, crates, binaries and packaging use the new names.

#### Account diagnosis, keyring passwords, `transport = "auto"`

- New JSON-RPC method (and MCP tool) `account.test {account | draft}`: a one-off REGISTER/unREGISTER on a private socket that returns a
  structured diagnosis (`ok`, `auth_failed`, `not_found`, `timeout`, `tls_untrusted` with the server certificate's SHA-256 fingerprint,
  `dns_failed`, `network_unreachable`, `other`), bounded to 15 s by default. Running accounts are not disturbed.
- New `[[account]] password_store = "file" | "keyring"` (default `file`). With `keyring` the password lives in the system credential store
  (macOS Keychain, Windows Credential Manager, Linux Secret Service over pure-Rust D-Bus, no libdbus), entry `io.zyno.siple` /
  `account:<id>`, and `password` must be empty. Read once per reload; without a credential store the account fails loudly and never falls
  back to plaintext. New method `account.set_password {account, password}`; `config.edit` moves, renames and deletes the entry along with
  the account and accepts `password_store`.
- New `transport = "auto"`: DNS SRV `_sips._tcp` > `_sip._tcp` > `_sip._udp`, else UDP 5060 (no NAPTR). `status` / `account.get` gain
  `transport_effective`; a network re-probe may re-select (never under live calls).
- A file that uses the new keys or values is rejected by older builds (unknown keys and variants are errors).

#### Configuration write-back

- New JSON-RPC methods (and MCP tools) `config.view` (effective config, per-key origin `file|profile|default`, file hash, secrets
  masked) and `config.edit` (typed edits with a `base_hash` conflict check, comments in `config.toml` preserved). The GUI account switch and
  the device pickers use the same write path.
- Audio devices are settings: the pickers and `audio.set_devices` write `[audio] input/output/ringer`. The `state.toml` override ("most recent
  wins") is gone; an existing choice there is moved into `[audio]` once on first start (per slot, where it was in effect), then removed. The device source in `status` is now
  `config` or `default`, and the GUI no longer shows a "Set in config" badge.
- New keys, all hot: `[ui]` (theme, density, BLF click actions, tray options, shortcuts, ...), `[calls] decline_code`, and
  `[logging] redact_credentials`. `redact_srtp_keys` now takes effect (it was accepted but did nothing). See `docs/CONFIG.md`.
- A file that uses the new keys is rejected by older builds (unknown keys are errors).

#### Settings migration

- On first start with the default config location, if `<config>/Siple/config.toml` does not exist but the legacy
  `<config>/ZynoPhone/config.toml` does (Linux: `siple` / `zynophone`), the legacy directory is copied to the new one.
  - Not applied when `--config`, `SIPLE_CONFIG` or `ZYNOPHONE_CONFIG` is given.
  - Quit the old ZynoPhone first. If its lock is held, startup stops with "Quit ZynoPhone to migrate settings".
  - Copied recursively with permissions preserved. Not copied: the lock file, `runtime.json`, sockets, `logs/`.
  - `Siple/MIGRATED_FROM` records the legacy path; the legacy directory gets `MOVED_TO_SIPLE.txt` and is never deleted,
    so a downgrade keeps working.
  - Config text is not rewritten. An explicit `user_agent = "ZynoPhone/0.1"` stays until you reset it.
  - Directory permissions (including the config dir itself) are carried over, e.g. a 0700 dir stays 0700.
  - Interrupted migrations leave `Siple.migrating-<pid>`; these are removed on the next start.

#### Compatibility

- `ZYNOPHONE_CONFIG` still works as an alias of `SIPLE_CONFIG` and prints one deprecation warning. It will be removed
  in 0.4.
- JSON-RPC method and event names and MCP tool names are unchanged. MCP `serverInfo.name` is now `siple`.
- The `hello` result's `server` field is now `"siple"`.
- `log_level` directives that name `zynophone_*` targets (e.g. `zynophone_sip=debug`) must be renamed to `siple_*`;
  the old targets no longer match anything.
- If both `SIPLE_CONFIG` and a different `ZYNOPHONE_CONFIG` are set, `SIPLE_CONFIG` wins and a warning is logged.
- An old build running in the same config directory (an explicit `--config`) is still detected through its
  `zynophone.lock` while the new one runs, through 0.3.x.
- Docker: the image no longer bakes `SIPLE_CONFIG`; its `siple-cli` wrapper (entrypoint and `docker exec`) defaults to `/data/config.toml` only when neither
  `SIPLE_CONFIG` nor `ZYNOPHONE_CONFIG` is set.
- Default `User-Agent` is now `Siple/<version>` (was `ZynoPhone/0.1`).

#### One-time effects

- macOS asks for microphone permission again (new bundle ID).
- Scripts must use the new names: `siple-cli` (was `zynophone-cli`), the `siple` binary, `siple.lock`, `siple.log`, and
  the control socket / named pipe names derived from `siple`.
- Docker image is `siple` and runs as user `siple` (same uid 10001); `SIPLE_CONFIG` replaces `ZYNOPHONE_CONFIG`.
- Linux: desktop file and icon are `io.zyno.siple`. The app declares the `sip`, `sips`, `tel` and `callto` URL schemes
  (macOS and Linux); links put the number in the dial field, see `[ui] handle_links`.
