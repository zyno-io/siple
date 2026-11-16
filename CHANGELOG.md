<!-- siple-rename: keep (this file documents the legacy ZynoPhone names on purpose) -->
# Changelog

## Unreleased: signed releases

`.github/workflows/release.yml` replaces `package.yml`: a `v*` tag builds all platforms, signs the macOS app (Developer ID,
hardened runtime, notarized and stapled; `.zip` and `.dmg`) and the Windows exes (Azure Artifact Signing), writes
`SHA256SUMS`, attaches build-provenance attestations and creates a draft GitHub Release. Manual runs still build unsigned
dev packages. `scripts/package-macos.sh` now also makes a `.dmg` and gains `BUNDLE_ONLY=1`; `scripts/package-windows.ps1`
gains `-StageOnly` / `-ZipOnly -RequireSigned`. See `docs/RELEASING.md`.

## Unreleased: keyboard shortcuts

The GUI has the keyboard map: answer, hang up, mute, hold, transfer, line select (`Mod+1`-`9`, `Mod+[` / `]`), focus dial, filter extensions and Settings, plus DTMF typing on an active line. `[ui.shortcuts]` overrides each chord (hot); unknown actions and malformed chords are config errors. See `docs/CONFIG.md`.

## Unreleased: licence changed to FSL-1.1-ALv2

Siple is now source-available under the Functional Source License 1.1 with an Apache-2.0 future licence
(FSL-1.1-ALv2), replacing MIT. Each release converts to Apache-2.0 two years after publication. See `LICENSE`.
Packages now ship `LICENSE` and `THIRD-PARTY-NOTICES.html`. The one-shot ZynoPhone-to-Siple rename script and the
internal planning documents were removed from the repository.

## Unreleased: renamed from ZynoPhone to Siple

The product is now **Siple**. App ID: `io.zyno.siple`. Code, crates, binaries and packaging use the new names.

### Account diagnosis, keyring passwords, `transport = "auto"`

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

### Configuration write-back

- New JSON-RPC methods (and MCP tools) `config.view` (effective config, per-key origin `file|profile|default`, file hash, secrets
  masked) and `config.edit` (typed edits with a `base_hash` conflict check, comments in `config.toml` preserved). The GUI account switch and
  the device pickers use the same write path.
- Audio devices are settings: the pickers and `audio.set_devices` write `[audio] input/output/ringer`. The `state.toml` override ("most recent
  wins") is gone; an existing choice there is moved into `[audio]` once on first start (per slot, where it was in effect), then removed. The device source in `status` is now
  `config` or `default`, and the GUI no longer shows a "Set in config" badge.
- New keys, all hot: `[ui]` (theme, density, BLF click actions, tray options, shortcuts, ...), `[calls] decline_code`, and
  `[logging] redact_credentials`. `redact_srtp_keys` now takes effect (it was accepted but did nothing). See `docs/CONFIG.md`.
- A file that uses the new keys is rejected by older builds (unknown keys are errors).

### Settings migration

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

### Compatibility

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

### One-time effects

- macOS asks for microphone permission again (new bundle ID).
- Scripts must use the new names: `siple-cli` (was `zynophone-cli`), the `siple` binary, `siple.lock`, `siple.log`, and
  the control socket / named pipe names derived from `siple`.
- Docker image is `siple` and runs as user `siple` (same uid 10001); `SIPLE_CONFIG` replaces `ZYNOPHONE_CONFIG`.
- Linux: desktop file and icon are `io.zyno.siple`. The app declares the `sip`, `sips`, `tel` and `callto` URL schemes
  (macOS and Linux); no handler is implemented yet.
