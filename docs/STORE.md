# Store distribution

What is left before Siple is listed in the Mac App Store, the Microsoft Store and Flathub, and the technical findings
needed to finish it. How the store packages are built, signed and uploaded today is in
[RELEASING.md](RELEASING.md#store-packages-mac-app-store-microsoft-store).

**Status.** Since 0.1.1, `.github/workflows/store.yml` builds the store packages on release tags: a Mac App Store
`.pkg` (signed when `STORE_MAS_ENABLED` and its secrets are set) and, for final `X.Y.Z` tags only, an unsigned Microsoft
Store `.msixbundle` (x64 + arm64); release candidates get TestFlight and the sideload bundle only. The editions exist in code (`build.channel` `mas` / `msix`,
see [CONFIG.md](CONFIG.md#location)); neither is published yet. Since 0.2.0 the Flatpak edition
(`io.zyno.siple`, `build.channel` `flatpak`) is built by `.github/workflows/flatpak.yml` (dispatch only for now, to save CI time); the Flathub submission is pending.

## Decisions

- The direct downloads (dmg, zip, tarball) stay the full-capability default. Store editions update only through their store
  and contain no downloader or installer (`--features store`).
- Mac App Store edition: no external programs (`[speech]` methods return `unsupported`), the bundled `siple-cli` is sandboxed
  and refuses `run`. Agents that need more use the direct build or a Homebrew `siple-cli`.
- Final tags go to both stores; release candidates only to TestFlight and the MSIX sideload bundle (the Microsoft Store
  accepts only `X.Y.Z.0`). Submitting automatically on a final tag is the goal; today it is by hand.
- Linux end users get Flatpak on Flathub; the tarball stays for agents and servers.
- Listings: free, category Business (secondary Developer Tools), privacy label "Data Not Collected" (the update lookup is
  anonymous and there is no crash upload in store editions), lead with "softphone".

## Shared work (both stores)

- **Sibling guard.** Each edition has its own config dir, lock file and socket or pipe name, so nothing stops a store copy
  and a direct copy from running at once and registering the same accounts twice (confirmed for MSIX + zip: the second
  engine fails only at pipe bind, after its subscriptions started). Before SIP starts, probe the other editions'
  endpoints with `hello` (short timeout, fail open) and refuse to start when one answers. Default config only; an explicit
  `--config` stays multi-instance.
- **Cross-edition CLI discovery.** `siple-cli` reads `runtime.json` from its own default config dir only. It should also try
  the other editions' locations and report what it found (`siple-cli instances`); see the findings below for what each OS
  allows. An explicit `--config` / `SIPLE_CONFIG` keeps today's behaviour.
- **Import.** Moving from a direct install to a store edition starts with an empty config. Generalize the one-time copy in
  `crates/siple-config/src/migrate.rs` into an import: automatic on first MSIX start (`%APPDATA%\Siple`, and the legacy
  `ZynoPhone` dir), user-assisted in the Mac App Store edition (open panel), and `siple-cli config import --from DIR`.
  Keychain passwords do not move across signing identities, so imported keyring accounts need the password again.
- **Recording / "agent listening" indicator** visible during a call and reported by `call.get` (App Review guideline
  2.5.14; useful everywhere).
- **Review account.** A durable public test PBX with echo and callback extensions and a demo SIP account for the review
  notes: both stores need a working account to test calls.
- **Screenshots** at store sizes (Mac App Store 2880×1800), rendered from the visual-test scenes in
  `crates/siple/src/visual_tests.rs`.
- **Support URL** for both listings: <https://siple.app/support/> (`sites/www/src/pages/support.astro`; the privacy policy is at `/privacy/`).
- **User guide.** When an edition ships: install page, "Where are my settings stored?" in the FAQ, and the MSIX uninstall
  note below.

## Mac App Store

Open:

1. Verify the sandboxed GUI on a clean machine (macOS 13, 15 and current): window, tray, notification prompt, `sip:` links
   (Apple Event), Open config / logs, microphone prompt, config reload, and the bundled `siple-cli ctl status`
   (`build.channel = "mas"`). Use a separate macOS user: containers, keychain and privacy grants are per user.
   `MAS_LOCAL_TEST=1 scripts/sign-macos-mas.sh` signs a copy for local runs.
2. The store and direct builds share the bundle id `io.zyno.siple`. Test a TestFlight install over the direct app
   (LaunchServices, "already installed", keychain prompts). The "quit the direct build first" advice is on the support
   page.
3. Homebrew: a cask for the direct build and a `siple-cli` formula, the outside CLI for Mac App Store users.
4. Submission automation: `MAS_VALIDATE` already runs `altool --validate-app`; upload, "What's New" from `CHANGELOG.md`
   and submission are manual (Transporter, App Store Connect). Automating them needs an App Manager API key.
5. Optional: allow `/usr/bin/say` in `[speech]` (see findings). Today the edition runs no external program.

Done:

- Launch at login uses `SMAppService.mainAppService` register / unregister (`shell/autostart.rs`, `objc2-service-management`).
  It shows up in System Settings > General > Login Items. The System Settings switch wins: if the user switched it off
  there, Siple logs it and does not register again. A non-default `--config` profile leaves it alone. Not exercised in the sandbox yet (open item 1).
- The Settings > Developer MCP snippet shows the bundled CLI's absolute path, derived from the running bundle.
- `handle_links` is hidden in Settings and the first-run wizard in every packaged edition (`Channel::can_register_links`);
  a faint note says the edition registers the links at install time and to make Siple the default if another app opens them. Info.plist declares the schemes.

Findings from the App Sandbox spike (macOS 27, Developer ID signature with the sandbox and app-group entitlements):

- The engine works sandboxed: REGISTER and calls over UDP, TCP and TLS, inbound on UDP 5060, SRV from `resolv.conf`, the
  SystemConfiguration network monitor, real-time audio thread priority and the Keychain. The only sandbox denials were
  harmless `system-info vfs.disk-space`.
- Inside the sandbox `$HOME`, `dirs::config_dir()` and `std::env::temp_dir()` all point into the per-app container. The
  config dir is built from the account database home (`getpwuid`), which equals
  `containerURLForSecurityApplicationGroupIdentifier`, so an outside process computes the same path. The container `tmp`
  is unreachable from outside, so the socket fallback stays in the group container.
- An unsandboxed process without Full Disk Access can `connect()` to the control socket in the group container, with no
  prompt, but cannot read files there (silent `EPERM`). The token is optional on the Unix socket, so outside discovery
  must connect to the socket path directly and must not read `runtime.json` first. Probe only when the MAS receipt
  (`/Applications/Siple.app/Contents/_MASReceipt/receipt`) exists or the user asks. This relies on behaviour Apple could
  tighten. Untested: a terminal without Full Disk Access reading the group container (a prompt attributed to the
  terminal is possible).
- A Developer ID CLI that carries the app-group entitlement still gets `EPERM` without a provisioning profile
  (`taskgated` disallows it). The Homebrew CLI must use socket-only discovery.
- A sandboxed command-line tool traps at start without an embedded Info.plist; `crates/siple-cli/build.rs` embeds one with
  `--features store`. Its entitlements must not carry an application identifier (ITMS-90885 for an executable with an
  identifier but no profile).
- Child processes inherit the sandbox. `/usr/bin/say` and `/bin/sh` scripts kept in the config dir run; Homebrew and
  `/tmp` binaries fail with `EPERM`. Disabling `[speech]` is therefore a review-policy choice (guideline 2.5.2), not a
  technical limit. If system binaries are allowed later, report a spawn `EPERM` as `unsupported`, not `internal`.
- Files the sandboxed app writes get a `com.apple.quarantine` attribute; harmless for data files.

## Microsoft Store

Open:

1. Interactive checks in a logged-on session (over SSH everything runs in session 0, where protocol activation, the
   StartupTask broker and toasts are unavailable): `sip:` / `tel:` activation argv (the manifest passes
   `Parameters="-- &quot;%1&quot;"`), the startup task (enabled from Settings > Apps > Startup), toasts under the package
   AUMID, microphone consent, the first-run firewall prompt. Also install as a standard user (firewall rule creation).
2. Discovery: an unpackaged `siple-cli` (zip, winget) should probe
   `%LOCALAPPDATA%\Packages\<package family name>\LocalState\Siple\runtime.json` for a compiled-in list of known package
   family names (the Store and sideload identities differ). No glob: a planted `runtime.json` could capture
   `account.set_password`.
3. Uninstall deletes `LocalState` (config, accounts, logs). The support page says so; the user guide still needs it, and a
   config export is not built.
4. Certification notes: a `runFullTrust` justification (raw UDP/TCP SIP and RTP, audio devices, the local control API, the
   console CLI alias), the firewall extension, the optional WACK "Blocked executables" findings (`CreateProcessW`, and
   `cmd` / PowerShell strings in the binaries, one of them notify-rust's default app id, unused when packaged), a demo
   account, the IARC rating, privacy URL and screenshots. Only one submission can be pending at a time.
5. Submission automation: `msstore publish` on final tags (an Entra app with the Manager role). Not wired.
6. Direct Windows channels: a winget manifest for the portable zip from CI; later a sideload MSIX signed with Artifact
   Signing plus an `.appinstaller` (its `Publisher` must equal the certificate subject).

Findings from the MSIX spike (Windows 11 x64, WACK from SDK 10.0.26100):

- In a packaged process `dirs::config_dir()` returns the real `%APPDATA%`, but writes there are redirected to the package's
  `LocalCache\Roaming` when `%APPDATA%\Siple` does not exist yet, and go to the real directory when it does. The config dir
  is therefore the explicit `LocalState` path from `GetCurrentPackageFamilyName`. A stale `LocalCache\Roaming` copy of
  `runtime.json` also shadowed the real one for the packaged CLI (`invalid token`).
- Only one `Application` passes WACK. Each Application has at most one `windows.appExecutionAlias` extension and its
  aliases share one executable, so only `siple-cli.exe` gets an alias; it needs `desktop4:SupportsMultipleInstances="true"`.
  The GUI starts from Start, a link or the startup task.
- Firewall rules need one `desktop2:Extension` per executable (two `FirewallRules` in one extension fail the schema). The
  rules follow the versioned install path on update.
- `handle_links` is hidden in Settings and the wizard (the manifest declares the protocols).
- `current_exe()` is a versioned `WindowsApps` path: never persist it. HKCU link registration and the `Run` key are skipped
  when packaged.
- Named pipes cross the package boundary (full trust, no AppContainer); the pipe ACL needs no change.
- Updates keep `LocalState`; uninstall removes `%LOCALAPPDATA%\Packages\<PFN>` entirely.
- Test signing over key-based SSH: `New-SelfSignedCertificate` into `CurrentUser\My` fails with `NTE_PERM`; use
  `LocalMachine\My` and `signtool /sm`. Windows also copies the certificate into the `CA` store
  (`scripts/install-test.ps1 -Uninstall` removes it from both).

## Flathub

The Flatpak build exists since 0.2.0; the Flathub submission is **deferred** until the Mac App Store and Microsoft
Store listings are live. Flathub's
[Generative AI policy](https://docs.flathub.org/docs/for-app-authors/requirements#generative-ai-policy) forbids
AI-generated or AI-assisted content in Flathub manifests and AI-opened or AI-written submission pull requests, and new
projects are generally not accepted. A later submission needs a hand-written manifest and a hand-opened PR with the AI
disclosure. The update automation is parked on the branch `ci/flathub` (closed PR #11). How it is built and submitted:
[RELEASING.md](RELEASING.md#flatpak-flathub).

Done:

- Manifest `packaging/flatpak/io.zyno.siple.yml`, built offline from `cargo-sources.json` on the freedesktop 26.08 runtime
  (x86_64 and aarch64). `scripts/flathub-manifest.sh` renders the Flathub copy from a release tag (git source, notices
  checked against the signed `SHA256SUMS`). No `x-checker-data` bot or automerge: the checker cannot regenerate
  `cargo-sources.json` or the notices checksum.
- finish-args: network, IPC, Wayland with X11 fallback, DRI, PulseAudio, the StatusNotifier watcher (tray), notifications
  and the Secret Service.
- A `flatpak` channel in `crates/siple-config/src/channel.rs` (detected by `/.flatpak-info`): no external programs (as in the
  Mac App Store edition), launch at login through the Background portal, no self-update (`disabled_reason`
  `managed_by_flatpak`).
- Complete `packaging/linux/io.zyno.siple.metainfo.xml`: releases, screenshots, developer, URLs, a filled OARS rating.
- The CLI is documented: `flatpak run --command=siple-cli io.zyno.siple` (stdin/stdout only).

Remaining:

- Submit: a pull request against the `new-pr` branch of `flathub/flathub`, then updates through `flathub/io.zyno.siple`.
- Verified app: `zyno.io/.well-known/org.flathub.VerifiedApps.txt`.
- Once on Flathub, the Flatpak lifts the tarball's glibc 2.36 floor (Ubuntu 22.04).

## Where agents find `siple-cli`, per edition

Target state, for `skills/siple/SKILL.md` and the agent docs:

| Edition | `siple-cli` | Limits |
|---|---|---|
| Direct dmg / Homebrew cask | `/Applications/Siple.app/Contents/MacOS/siple-cli` (cask symlink) | none |
| Mac App Store | bundled sandboxed CLI, or a Homebrew `siple-cli` | bundled: no `run`, no `[speech]` |
| MSIX | `siple-cli` on `PATH` (execution alias) | none |
| zip / winget | next to `siple.exe` | none |
| Flatpak | `flatpak run --command=siple-cli io.zyno.siple` | stdin/stdout only, no `[speech]` |
| tarball | `bin/siple-cli` | none |
| AppImage | `<file> --cli`, or a `siple-cli` wrapper script that runs it (a symlink fails under AppImageLauncher) | none |
| Docker image | `siple-cli` on `PATH` (`/usr/local/bin` wrapper) | none |
