# Siple

**The scriptable SIP softphone. Native, source-available (FSL-1.1-ALv2), for Windows, macOS and Linux.**

Siple is a desktop SIP softphone written in Rust (egui GUI, no Electron, no OpenSSL). It works with any
standards-based PBX or SIP provider (Asterisk/FreePBX, FreeSWITCH, Kamailio-fronted platforms, hosted UCaaS)
and is built to be driven by scripts and AI agents as well as by hand.

- **For desk users:** multiple lines, BLF, attended transfer, voicemail lamps, SDES-SRTP, TLS with certificate pinning.
- **For builders:** a JSON-RPC control API and an MCP server over every state and action, agent audio
  (`call.listen`, `call.say`, `call.stream`), tone and DTMF detection, play/record, SIP traces and
  vendor-profile emulation (Yealink, Polycom, Cisco, Grandstream). Whatever the GUI can do, an agent can do.

Status: pre-1.0 (see [CHANGELOG.md](CHANGELOG.md)). Repository: `https://github.com/zyno-io/siple`. Licence: [FSL-1.1-ALv2](#license) (source-available, not OSI open source).

**Documentation: https://zyno-io.github.io/siple/** (user guide, AI-agent quick start, full configuration and API reference; sources in [`website/`](website/) and [`docs/`](docs/)).

## Features

- **Multiple lines.** Several concurrent calls, one active line; the others are held automatically (device-bound
  calls only, so agent calls with `agent`/`silence`/`file`/`null` audio are exempt). Call waiting, line switching, DND, auto-answer (incl. intercom).
- **BLF and presence.** Dialog-event BLF (lamps, keys, list views), directed pickup of ringing extensions, click-to-dial or
  transfer; MWI (message-summary) lamps; generic SUBSCRIBE for any event package.
- **Transfers.** Blind, attended and semi-attended, in both roles (we transfer; we are transferred), with Replaces.
  Transfer sessions live in the engine, so the GUI, the API and agents share one state machine.
- **AI-agent control API and MCP.** Newline-delimited JSON-RPC 2.0 over a Unix socket / named pipe (optional loopback TCP
  with token), plus `siple-cli mcp`, an MCP server for Claude Code and other MCP clients. The method list, MCP tools and
  [docs/API.md](docs/API.md) are generated from one catalog, so they cannot drift. No telemetry: the only network activity Siple starts on its own is an optional, anonymous version check (GitHub for direct
  builds, Apple's lookup on the Mac App Store, Windows' Store service on the Microsoft Store; `[updates] check`, off with `check = false`; see [docs/CONFIG.md](docs/CONFIG.md#updates)). Direct downloads can install an update in place when you choose to: they then download it from GitHub and verify it first.
- **Network-change resilience.** Accounts re-probe and re-register when the OS reports an address or route change, after
  sleep/wake, or when a periodic route probe sees a different path; they wait in a `WaitingForNetwork` state when offline.
- **Live config.** Edit the TOML while Siple runs; accounts re-register or rebuild as needed and an invalid edit never
  replaces the last good config. The GUI and API edit the same file, preserving comments ([docs/CONFIG.md](docs/CONFIG.md)).
- **Transports and security.** UDP, TCP, TLS (rustls). Per account: OS roots, extra CA, SHA-256 certificate pinning or
  skip-verify; SRTP (SDES) `off | optional | mandatory` with automatic fallback; outbound proxy, rport/NAT keep-alives, DNS SRV.
- **Media.** Opus, G.722, G.711; DTMF via RFC 4733, SIP INFO or in-band; jitter buffer, PLC and RTCP.
- **Logs.** Application log plus a per-account wire-level SIP trace (toggle at runtime). Credential and SRTP-key redaction
  are options: see "Logs and secrets" in [docs/CONFIG.md](docs/CONFIG.md).

## Install

Pre-built packages are attached to GitHub Releases (`https://github.com/zyno-io/siple/releases`): a notarized macOS
`.dmg` / `.zip`, an Authenticode-signed Windows `.zip` and a Linux tarball, with `SHA256SUMS` and build-provenance
attestations. How to verify them: [docs/RELEASING.md](docs/RELEASING.md#verify-a-download). Or build from source.

### Build from source

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

### Packages

```sh
scripts/package-linux.sh            # dist/Siple-<ver>-linux-x86_64.tar.gz (run in rust:1.94-bookworm)
scripts/package-macos.sh            # dist/Siple.app (universal), dist/Siple-<ver>-macos.zip
```
```powershell
powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1              # dist\Siple-<ver>-windows-x64.zip
powershell -ExecutionPolicy Bypass -File scripts\package-windows.ps1 -Arch arm64  # dist\Siple-<ver>-windows-arm64.zip
```

Set `CODESIGN_IDENTITY` for a stable macOS signature (microphone permission is keyed to it; see
[Troubleshooting](#macos-no-microphone--silent-tx)). `scripts/gen-licenses.sh` writes `THIRD-PARTY-NOTICES.html`
(needs `cargo install cargo-about --locked`); the `package-*` scripts bundle it with `LICENSE`, warn when `cargo-about` is
missing, and fail when it is missing and `SIPLE_RELEASE=1` is set.

## Quick start

```sh
target/release/siple                          # GUI
target/release/siple-cli run --headless       # no window (CI, SSH sessions, servers)
```

On first run an example config is written (see [File locations](#file-locations)); its sample account is
**disabled**. Edit it (or start from `siple-cli config-example`), set `enabled = true`, save. No restart needed:

```toml
[[account]]
id = "desk"
username = "1001"
password = "secret"
domain = "pbx.example.test"
transport = "tls"
tls_verify = false            # or tls_fingerprint = "47:EE:..."
srtp = "optional"
```

Every key is documented in [docs/CONFIG.md](docs/CONFIG.md).

### Control it from a shell

```sh
siple-cli ctl status
siple-cli ctl call.dial '{"account":"desk","target":"9999","source":"silence","sink":"null"}'
siple-cli ctl call.wait_for '{"call_id":"c1","state":"active","timeout_ms":15000}'
siple-cli ctl call.detect_tone '{"call_id":"c1","freq_hz":1000,"timeout_ms":5000}'
siple-cli ctl call.hangup '{"call_id":"c1"}'
siple-cli events --kinds 'call.*,account.*'        # live event stream
siple-cli methods                                   # all methods (offline); --markdown / --json
```

`ctl`, `events` and `mcp` find a running instance through `runtime.json` (use `--config PATH` for an
instance started with a non-default config; or `--socket` / `--pipe` / `--tcp ADDR --token T`). The full
method reference and recipes (place a call and verify audio, voicemail, BLF, transfers) are in [docs/API.md](docs/API.md).

### Register the MCP server with Claude Code

```sh
claude mcp add siple -- /path/to/siple-cli mcp
# macOS app bundle:
claude mcp add siple -- "/Applications/Siple.app/Contents/MacOS/siple-cli" mcp
# non-default config directory:
claude mcp add siple -- /path/to/siple-cli mcp --config /path/to/config.toml
```

The MCP server speaks stdio, so any command that reaches `siple-cli mcp` works (for example `docker exec -i siple siple-cli mcp`).
Start Siple (GUI or `run --headless`) first; the tools drive that instance.

### Agent audio

An agent can speak into a call and hear it without a sound card: `call.play` (tone, file, inline WAV/PCM),
`call.listen` (voice-activity detection, optional transcript), `call.rx_audio`, `call.say` (text to speech) and a binary
duplex stream, `call.stream` (`siple-cli stream <call_id>` pipes raw PCM). Turn-taking is race-free: `say{wait}` returns an
`rx_ms` position, `listen{since_rx_ms}` hears everything after it (thinking time loses nothing; the start timeout counts from the request). After you speak, use `finished.barge_in_rx_ms` (present only when the caller interrupted, reason `barge_in`) else `finished.rx_ms` as the next `since_rx_ms`; to listen again without speaking, chain the previous `end_rx_ms`.
Speech needs your own TTS/STT programs in `[speech]` (piper, espeak-ng, `say`, SAPI, whisper.cpp; nothing is bundled,
never a shell); clips and listening need nothing. See "Audio for agents" in [docs/API.md](docs/API.md) and
[docs/CONFIG.md](docs/CONFIG.md#speech). Over MCP, audio comes back as an audio content block (never as base64 text).

## Linux notes

**Build dependencies** (Debian/Ubuntu): `sudo apt install cmake pkg-config libasound2-dev build-essential`, plus Rust 1.94+.
ALSA is a hard runtime dependency of both binaries (`libasound.so.2`); GUI libraries are loaded at runtime only by
`siple`, the headless CLI needs nothing else.

**Tarball.** `dist/Siple-<ver>-linux-x86_64.tar.gz` unpacks to `siple-<ver>/{bin,share}`. It is built in
`rust:1.94-bookworm` (glibc 2.36), so it runs on Debian 12+ and Ubuntu 23.04+. `scripts/package-linux.sh` fails if a
binary links anything beyond libasound, libc, libm and libgcc_s, and prints the highest `GLIBC_` version used. Runtime
packages: `libasound2` (`libasound2t64` on Ubuntu 24.04+/Debian 13), `ca-certificates` for `tls_verify = true`; for the
GUI also `libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-egl1 libx11-6 libx11-xcb1 libxcursor1 libxrandr2
libxi6`, a Vulkan or GL driver (`mesa-vulkan-drivers` or `libegl1 libgl1-mesa-dri`) and `xdg-utils`.

**Docker (headless CLI).** The image holds `siple-cli` only. Build with BuildKit (the docker buildx plugin) and run with
host networking so SIP and RTP need no NAT handling:

```sh
docker build -t siple:dev --target runtime -f packaging/linux/Dockerfile .
docker run -d --name siple --network host -v siple-data:/data siple:dev
docker exec siple siple-cli ctl status
claude mcp add siple -- docker exec -i siple siple-cli mcp
# from another machine: claude mcp add siple -- ssh <host> docker exec -i siple siple-cli mcp
```

`/data/config.toml` is the config (the image's `siple-cli` wrapper sets `SIPLE_CONFIG` to it unless `SIPLE_CONFIG` (or its deprecated pre-rename alias) is already set, also for `docker exec`); the container runs as uid 10001 with `--no-audio`. Without
`--network host`, publish the SIP port and the whole `[media] rtp_port_range` (default 16384-32767/udp, narrow it first)
and set `public_address` on the account. The optional `speech` target (`--target speech`) adds espeak-ng and whisper.cpp
for speech hooks; it is built locally and never published.

## File locations

| What | macOS | Windows |
|---|---|---|
| Config (`config.toml`) | `~/Library/Application Support/Siple/` | `%APPDATA%\Siple\` |
| `state.toml` (runtime state that is not a setting; absent while there is none), `runtime.json` (pid, endpoints, token), `siple.lock` | same directory | same directory |
| Control endpoint | `control.sock` in that directory (or a short path under `$TMPDIR`; on Linux `$XDG_RUNTIME_DIR` or `/tmp/siple-<euid>/`) | `\\.\pipe\siple-<user>-<hash>` |
| Logs `siple.<date>.log` | `<config dir>/logs/` | `<config dir>\logs\` |
| SIP traces `sip.<date>.log` | `<config dir>/logs/sip/<account>/` | `<config dir>\logs\sip\<account>\` |

`--config PATH` moves all of it (everything lives relative to the config file); `[general] log_dir` moves only the logs.
The SIP trace shows every message on the wire with direction, transport and addresses, retransmits and keep-alives
included; it is the first thing to read when something fails. A call's messages are also available over the API
(`call.sip_trace`).

## Troubleshooting

### macOS: no microphone / silent TX

macOS grants the microphone to **app bundles**, not to bare terminal binaries launched from random places. Use
`dist/Siple.app` (it carries `NSMicrophoneUsageDescription` and the audio-input entitlement) and accept the
prompt on the first call. `siple-cli ctl status` shows `audio.mic_permission` (`granted`, `denied`,
`not_determined`, ...). If denied: System Settings > Privacy & Security > Microphone. The grant is keyed to the code
signature: an **ad-hoc** signed build gets a new identity on every rebuild, so permission may be asked again; sign
with a stable identity (`CODESIGN_IDENTITY="..." scripts/package-macos.sh`) to keep it. For automated runs avoid the
microphone entirely with `source:"tone:440"` / `"silence"` (and `sink:"null"`).

### Windows: one-way or no audio

The first start triggers a **Windows Defender Firewall** prompt for `siple.exe`; if it was dismissed or denied,
inbound RTP is blocked (symptom: you hear nothing, or the far end hears you but not vice versa). Allow it for
private and public networks, or add a rule for the program / the `rtp_port_range` (default UDP 16384-32767).
Also consider `symmetric_rtp = true` behind NAT, and `public_address` for static NAT. SSH sessions on Windows have
no audio devices: use a desktop session, or `source`/`sink` tone/null.

### TLS to a self-signed or private PBX

- Quick and insecure: `tls_verify = false` (ignores all certificate errors).
- Better: **pin** the certificate: `tls_fingerprint = "47:EE:8D:..."` from
  `openssl x509 -in pbx.pem -noout -fingerprint -sha256`. It is accepted regardless of chain, expiry or name, which
  also covers certificates the OS verifier rejects (e.g. macOS refuses self-signed leaf certs valid longer than
  825 days). Or trust a CA: `tls_ca_file = "ca.pem"`.
- Handshake fails against an old FreeSWITCH: Siple speaks TLS 1.2/1.3 with ECDHE suites (rustls). TLS 1.0/1.1 or
  non-ECDHE-only servers are not supported.

### FreeSWITCH and SRTP

Without `rtp_secure_media`, FreeSWITCH accepts SRTP when it is offered. FreeSWITCH's stock `dialplan/default.xml` sets
`rtp_secure_media=true` (meaning mandatory) on the caller's leg when the caller's offer carries SDES crypto; it does not
pass this to the bridged leg (the `export` line is commented out). To keep SRTP optional on that leg too, change those
actions to `rtp_secure_media=optional:AES_CM_128_HMAC_SHA1_80:AES_CM_128_HMAC_SHA1_32`. With `srtp = "optional"` Siple
offers RTP/AVP plus `a=crypto` (OSRTP-style); a PBX that answers `RTP/AVP` gets plain RTP, and if it rejects the offer
outright with 488/415/606 the call is **retried automatically once with plain RTP** (`srtp_fallback: true` in the call
details). Use `srtp = "mandatory"` to test that a PBX really negotiates SRTP;
`call.get` shows the negotiated suite.

### Registration or calls fail

1. `siple-cli ctl status`: registration state and last error per account, `config.last_error`.
2. Read `logs/sip/<account>/sip.<date>.log`: look for 401/403 loops (wrong `auth_username`/`password`), 408 (nothing answered:
   firewall, wrong `registrar`/transport), 488 (codec or SRTP mismatch; check `srtp`, `codecs`).
3. Config problems: the GUI shows a banner and `config.error` events carry the exact message; the old config stays active.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

```sh
cargo test --workspace                 # unit + in-process integration (UDP/TCP/TLS peer accounts, no PBX)
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

On Linux and Windows add `-- --skip visual_tests`: the GUI snapshot images were rendered on macOS (Metal).

**Interop tests** run against a PBX you operate and are skipped unless `SIPLE_INTEROP=1`. Credentials are read from
`tests/interop/pbx.env` (git-ignored, not provided). They exercise registration, calls, hold, SRTP, transfers, BLF and MWI:

```sh
SIPLE_INTEROP=1 cargo test -p siple-sip -p siple-core --test '*interop*' -- --test-threads=1
```

When the tests run on the same host network as the PBX, set `SIPLE_INTEROP_RTP_RANGE=20000-20999` so the client's RTP
ports stay clear of the PBX's. `SIPLE_SIPP=1` enables the sipp edge-case scenarios in `tests/sipp/`.

## Repository layout

```
crates/siple-config   config schema, parsing, validation, paths, log levels (+ config.example.toml)
crates/siple-media    pure media: RTP/RTCP, SRTP, codecs, DTMF, tones, SDP offer/answer, jitter buffer
crates/siple-sip      SIP stack on rsipstack: accounts, registration, calls, transfers, subscriptions, network monitor
crates/siple-core     engine: audio devices, media thread, logging, event log, control server, reconfig
crates/siple          GUI binary (egui/wgpu); also `--headless`
crates/siple-cli      console binary: run --headless, ctl, events, mcp, methods, config-example, skill
vendor/rsipstack      vendored rsipstack 0.7.3 with Siple patches (see its SIPLE_PATCHES.md)
docs/                 ARCHITECTURE.md, CONFIG.md, API.md (generated from API-intro.md + the code)
packaging/, scripts/  icons, Info.plist, entitlements, manifest; package-*.sh, gen-api-docs.sh, gen-licenses.sh
deny.toml, about.toml dependency licence policy and licence bundle config
```

## Contributing and security

Pull requests are welcome: read [CONTRIBUTING.md](CONTRIBUTING.md) (CLA, API parity rule) and the
[Code of Conduct](CODE_OF_CONDUCT.md). Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## License

Siple is **source-available** under the [Functional Source License, Version 1.1, ALv2 Future License](LICENSE)
(FSL-1.1-ALv2), Copyright 2026 Signal24 LLC dba Zyno Consulting. This is a Fair Source licence, not an OSI-approved
open-source licence.

- **Allowed:** use it, run it, self-host it, modify it, redistribute it, and contribute to it, for any purpose except a
  competing one. Internal use, education, research and building your own products on top of the control API are all fine.
- **Not allowed:** a competing commercial use, such as selling Siple or a modified copy, offering it as a product or
  service that substitutes for Siple, or re-branding (white-labelling) it. See the full text for the exact terms.
- **Becomes Apache-2.0:** each release converts to the Apache License 2.0 two years after that release was made available.

The vendored `vendor/rsipstack` stays under its upstream MIT licence (it has its own `LICENSE`) with a small set of patches
documented in `vendor/rsipstack/SIPLE_PATCHES.md`. Dependency licenses are checked by `cargo deny` (`deny.toml`);
they are permissive (MIT/Apache-2.0/BSD/ISC/Zlib/Unicode), with MPL-2.0 for `audio_thread_priority` and font licenses for
egui's bundled fonts. There is no OpenSSL anywhere. G.711 is our own implementation (`crates/siple-media/src/g711.rs`); there are no copyleft
dependencies.
