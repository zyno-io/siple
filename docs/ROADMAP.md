# Roadmap

Open work and known limitations, grouped by area. Nothing here is scheduled. Before starting a large item, open an issue
so the design can be agreed first. Every feature follows the [API parity rule](../CONTRIBUTING.md#api-parity-rule): a
JSON-RPC method, events and an MCP tool in the same change.

Store editions (Mac App Store, Microsoft Store, Flathub) have their own page: [STORE.md](STORE.md).

## Known bugs and limitations

- **Silence** does not stop the ringtone or the call-waiting tone; it only marks the banner "Silenced" (the engine has
  no way to silence ringing yet; `UiAction::Silence` in `crates/siple/src/ui/mod.rs` is a no-op).
- **Do not disturb** switched from the header, the tray or Settings is a runtime override (`account.set_dnd`) and is lost
  on restart. Persist it to `config.toml` (`config.edit`, field `dnd`) like every other setting.
- **Quit** does not ask while calls are live. Ask "End N calls and quit?" with Cancel as the default.
- **Notifications:** Answer / Decline buttons exist on Linux only. The Windows zip has no AUMID, so its toasts appear under
  PowerShell's app id (the MSIX build uses its package AUMID).
- **Microphone permission (macOS)** is still requested at startup (`crates/siple/src/main.rs`), before the first-run
  wizard's audio step, which has its own button.
- **Auth guard and provider hostnames.** The digest-auth guard (`crates/siple-sip/src/authz.rs`) answers a challenge only
  when the request's resolved IP is one the account's proxy or registrar resolved to (refreshed at startup, on every probe
  and on network changes). A genuine provider address is refused when the provider routes through a hostname the probe
  never resolved (another edge named in a Record-Route), or when DNS rotates between probes; the user sees
  "Authentication refused: ... (set auth_foreign = true ...)". Fix: when rsipstack resolves a destination, keep the
  hostname with the address and carry it through the transaction send, the digest retry
  (`vendor/rsipstack/src/dialog/authenticate.rs`) and the in-dialog fallback (`dialog/dialog.rs`), so the guard also accepts "resolved
  from a configured provider name". This stays safe for a peer-supplied Record-Route: if an attacker names the provider's
  host, DNS sends the request to the real provider. About 50 to 100 lines in 3 or 4 vendored files, plus an entry in
  `vendor/rsipstack/SIPLE_PATCHES.md`. Re-resolving at challenge time instead would put a DNS round trip in every
  challenge.
- **Stale contact after a network change.** Re-registration is fast, but without RFC 5626 outbound (`+sip.instance`,
  `reg-id`) the previous Contact stays at the registrar until it expires. Add outbound when the registrar supports it,
  otherwise unregister the old Contact once after a successful re-register.
- **`account.set_password`** writes the keyring outside the config actor, so a concurrent account rename can race it
  (worst case: the password has to be set again).
- **Linux Secret Service:** a credential store that is down at first use stays "unavailable" until restart (keyring 4
  caches its lazy init). Add a retry path.
- **Validator:** a `local_port` conflict between a `transport = "auto"` account and a TCP/TLS account is not detected.
- **`pickup = "replaces"`** is experimental and not interop-verified.
- **No echo cancellation or noise suppression**: a headset is assumed. **No STUN or ICE**: NAT handling is rport, Contact
  rewrite, keep-alives, `public_address` and `symmetric_rtp`.
- **Linux tarball and AppImage** (x86_64 and aarch64, both built natively; 32-bit ARM is not built) need glibc 2.36 (Debian 12, Ubuntu 23.04 and later); Ubuntu 22.04 is excluded until the
  Flatpak is on Flathub.
- **crates.io:** the crates cannot be published while `vendor/rsipstack` carries Siple patches (path dependency).

## Desktop app

- Restore window position and size (size only on Wayland).
- Settings > Shortcuts is read-only: add an editor and opt-in global hotkeys (XDG GlobalShortcuts portal on Wayland,
  unavailable where the portal is missing).
- `[ui] reduced_motion = "system"` behaves like `off`: follow the OS setting.
- Always-on-top and a compact mini window.
- Call history (persisted, searchable, missed-call badge), also as suggestions in the transfer composer.
- Contacts: local, CardDAV, LDAP (rustls).
- USB headset buttons (HID telephony page: hook, mute, ring LED) via `hidapi`.
- Voicemail: dial the voicemail number from the MWI indicator and the tray menu (needs a per-account voicemail target, or
  the `Message-Account` from MWI).
- Windows: a "Microphone blocked?" link to `ms-settings:privacy-microphone` when the microphone is silent.
- Wizard: "Import from another Siple install".
- Translation: Fluent bundles and a CJK font fallback (IBM Plex has no CJK).
- Screen-reader audit with VoiceOver, NVDA and Orca. AccessKit is enabled; the audit has not been done. Goal: answer, hang
  up and transfer with a screen reader alone.
- Crash reports: panics only, a redacted local report, ask before each send, never in store editions.

## SIP, media and BLF

- RFC 5626 outbound and stale-contact removal (see above).
- Echo cancellation and noise suppression; until then, a "headset recommended" hint when the output is built-in speakers.
- STUN-assisted public address.
- Replaces pickup: an interop matrix (FreeSWITCH, Asterisk, FreePBX, 3CX), then drop the "experimental" label.
- BLF: RLS lists (RFC 4662) instead of one SUBSCRIBE per key; presence (PIDF) and DND/away lamps; park/retrieve,
  speed-dial and DND-toggle key types; a detachable Extensions window; drag a line onto a key to transfer.
- Local three-way conference (media mixer).
- Provider presets: files that prefill the wizard for a hosted PBX.

## Control API and agents

- `app.quit {unregister: true}` with CLI and MCP parity. Today agents and scripts stop Siple by killing the process, which
  skips the unREGISTER.
- `skills/siple/SKILL.md`: teach agents to read `status.build`, where `siple-cli` lives in each edition (table in
  [STORE.md](STORE.md#where-agents-find-siple-cli-per-edition)), and the audio-pipe recipes for editions without `[speech]`
  (`siple-cli stream`, `call.listen {audio: "base64"}`).

## Platforms and packaging

- Packages: Homebrew cask and `siple-cli` formula, winget, `.deb` / `.rpm`. The Flathub submission of the
  Flatpak build is deferred (see [STORE.md](STORE.md#flathub)).
- Windows arm64: libopus builds as portable C (no NEON). The `cmake` crate does not set `CMAKE_SYSTEM_PROCESSOR` with this
  toolchain file, and its default `ARM64` would not match opus's `(arm|aarch64)` check anyway. To enable NEON, set
  `CMAKE_SYSTEM_NAME Windows` and `CMAKE_SYSTEM_PROCESSOR aarch64` in `packaging/windows/static-crt.cmake` when
  `CMAKE_GENERATOR_PLATFORM` is `ARM64`, and build-test it. Low priority at voice bitrates.
- Release candidate asset names differ: the macOS zip carries the full version (`Siple-0.2.0-rc.1-macos.zip`), Windows and
  Linux the numeric core (`Siple-0.2.0-windows-x64.zip`). One scheme would be simpler, but asset names are part of the
  in-app updater's contract ([RELEASING.md](RELEASING.md#release-asset-contract-an-api-of-the-in-app-updater)): released
  versions must keep finding their files.
- macOS minimum: the store edition needs macOS 13, the direct build 12. Moving the direct build to 13 would allow
  `SMAppService` for launch at login instead of a LaunchAgent plist.
- Manual checks on real desktops:
  - Windows: tray, notifications and launch at login (verified on macOS and Linux only).
  - Linux, verified so far headless (Debian 12, Xvfb + openbox, session D-Bus). Not yet: a GNOME (AppIndicator
    extension) or KDE panel tray, Wayland sessions, notification Answer / Decline with a real notification server,
    `[ui] handle_links` registration (`xdg-mime`), KWallet and other Secret Service providers, real ALSA / PulseAudio /
    PipeWire devices, and whether a window-manager iconify also triggers X11's stale focus report (with
    `hide_from_taskbar = false` that could flip `visible`).
- Removals planned for 0.4: the `ZYNOPHONE_CONFIG` alias and the detection of an old build's `zynophone.lock`.
- Upstream the patches in `vendor/rsipstack/SIPLE_PATCHES.md`; that unblocks crates.io and shrinks the vendored copy.

## Engine maintenance, tests and CI

- Replace `chrono` with `jiff` (RUSTSEC-2026-0543: chrono is unmaintained; ignored in `deny.toml` until then). About 40
  files: timestamps and RFC 3339 output in events, logs and `state.toml`; keep the formats byte-identical.
- End-to-end test for moving a TCP/TLS call after a network change (the call-move tests are UDP only).
- A public interop fixture (docker-compose FreeSWITCH and Asterisk) in CI, so interop tests run without a private PBX.
  On top of it: real interface churn in a privileged Linux container, and a soak that flaps an interface 200 times and
  checks for leaked sockets, exactly one registrar binding and no duplicate subscriptions.
- `ci.yml` never compiles `aarch64-pc-windows-msvc`; only `release.yml` and `store.yml` do (a cross build of about 35
  minutes), so an arm64-only break surfaces at release time.
- Load-sensitive tests that pass when run alone: `control::tests::non_reading_client_connection_is_released`,
  `network_api::network_section_reloads_hot_without_re_registering` (an extra REGISTER under heavy load),
  `media_engine::record_stereo_one_second` / `record_sink_mono`, `prack::mode_matrix_decides_reliability`,
  `config_edit::username_edit_reregisters_ui_edit_does_not_and_comments_survive` (about 12 s alone).
- `visual_tests::visual_transfer_composer` differs by a few pixels on Windows (baselines are rendered on macOS / Metal);
  CI runs the visual tests on macOS only.

## Docs and website

- The site is a single "latest" version built from `main`. Release-pinned docs would need a tag-driven build.
- Store-edition pages in the user guide once an edition is published (see [STORE.md](STORE.md)).

## Mobile

Not started; a separate project if it happens. egui is not the path: native SwiftUI (CallKit, PushKit) and Compose
(ConnectionService, FCM) shells over the Rust core through UniFFI, an external-frames audio backend, and an RFC 8599-style
push gateway in front of the PBX (iOS cannot keep a SIP registration alive in the background). Groundwork that would be
needed in this repo: a `Phone` facade over `App` with a pure-data types crate (no tokio or egui types in signatures),
feature gates for `control-api`, `mcp`, `agent`, `speech`, `config-watch` and desktop audio (mobile builds only; desktop
builds keep everything), and a CI `cargo check` of `siple-sip` and `siple-media` for iOS and Android targets.

## Decisions that constrain new work

- **Toolkit.** The desktop GUI stays on egui/eframe. Revisit only if the screen-reader audit hits structural problems
  AccessKit labels cannot fix, if right-to-left or complex-script locales are needed, or if Wayland multi-window
  behavior is required. The fallback is a UI-only shell (system webview) over the same engine; the media stack stays in
  Rust.
- **Linux linkage.** No GTK, libappindicator or libdbus: tray, notifications and keyring use pure-Rust D-Bus (zbus), GUI
  libraries are loaded at run time, and `siple --headless` / `siple-cli` must run without them.
  `scripts/package-linux.sh` fails on unexpected `NEEDED` libraries.
- **Configuration.** Every setting is a `config.toml` key, picked up while Siple runs (see the reload classes in [CONFIG.md](CONFIG.md)); the GUI and `config.edit` only write that file.
  `state.toml` holds runtime state that is not a setting.
- **Telemetry.** None. The only request Siple starts on its own is the optional, anonymous update check.
