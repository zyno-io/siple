# Product

<!-- impeccable:product-schema 1 -->

> Written by an unattended agent from the owner's written brief and the Siple repository
> (github.com/zyno-io/siple: README.md, CHANGELOG.md, docs/, sites/docs/, skills/siple/SKILL.md). No interview
> round was possible; facts below are taken from those sources, and inferences are marked
> *(inferred)*. Confirm or correct them before relying on them.

## Platform

web

## Stack

Astro (static output, npm), pinned by the owner's brief: same structure and tooling as the other
Zyno marketing sites (`pixelci-sales-site`): `src/{components,data,layouts,lib,pages,styles}`,
Playwright VRT, `scripts/og.mjs`, GitHub Actions deploying `dist/` to ZynoHosting
(`www-staging.<domain>` automatically from `main`, production as a manual job). Domain: `siple.app`.

## Users

- **Desk users and the people who set up their phones** *(inferred from the README's "For desk users")*: office
  staff, receptionists and IT admins who already have a SIP account (a company PBX such as
  Asterisk/FreePBX or FreeSWITCH, or a hosted SIP/UCaaS provider) and want a native desktop
  softphone with real desk-phone features: multiple lines, BLF, transfers, hold, voicemail lamps.
- **Builders and AI-agent users** *(from the README's "For builders")*: developers, QA and voice/telephony engineers,
  and people running AI agents (Claude Code and other MCP or Agent Skills clients) who want to
  place, answer, transfer and test calls from scripts or agents, including speaking and listening
  on calls.

## Product Purpose

Siple is a desktop SIP softphone for macOS 12+, Windows 10+ (x64 and ARM64) and Linux (x86_64 and aarch64),
built so that people and AI agents can drive the same calls. The marketing site's job: explain
what Siple is in seconds, let a visitor download the right build for their computer, and send
builders to the AI-agent docs. Success = a visitor downloads and knows that agents can drive it.

## Positioning

"The scriptable SIP softphone." Whatever the GUI can do, an agent can do: every GUI capability is
a JSON-RPC method (`siple-cli ctl`), with an agent skill (`siple-cli skill`) and an optional MCP
server (`siple-cli mcp`) generated from the same catalog. Agents can speak and listen on calls
through user-configured TTS/STT programs. Native (Rust + egui, no Electron, no OpenSSL), no
telemetry, works with any standards-based PBX or SIP provider.

## Operating Context

- Distributed as GitHub Release assets: `Siple-<ver>-macos.dmg` (universal; also `.zip`),
  `Siple-<ver>-windows-x64.zip`, `Siple-<ver>-windows-arm64.zip`,
  `Siple-<ver>-linux-x86_64.tar.gz`, and from the release after 0.2.1 `Siple-<ver>-linux-x86_64.AppImage`,
  `Siple-<ver>-linux-aarch64.AppImage` and `Siple-<ver>-linux-aarch64.tar.gz` (the AppImage updates itself in
  place; the tarball is for servers and agents; glibc 2.36+), with `SHA256SUMS` and build-provenance attestations. macOS
  builds are Developer ID signed, notarized and stapled; Windows executables are
  Authenticode-signed (Azure Artifact Signing); Linux ships checksums.
- Mac App Store and Microsoft Store listings are planned but not available ("coming soon").
- Docs: https://zyno-io.github.io/siple/ (Guide, AI agents, API reference, Licence).
- Repo: https://github.com/zyno-io/siple. Contact: oss@zyno.io.

## Capabilities and Constraints

Confirmed (README, docs, CHANGELOG): multiple accounts and concurrent lines with auto-hold, call
waiting, DND, auto-answer; BLF (lamps, keys, list views) with click-to-dial, transfer and directed
pickup; MWI voicemail lamps; blind, attended and semi-attended transfer; hold; DTMF (RFC 4733,
SIP INFO, in-band); UDP/TCP/TLS (rustls) with certificate pinning, SDES-SRTP; network-change
re-registration and sleep/wake recovery; Opus, G.722, G.711; live-reloading `config.toml`
shared by GUI and API; tray mode; keyboard shortcuts; sip:/tel: links; passwords optionally in
the system keychain; per-account SIP traces; headless mode (`siple-cli run --headless`) and a
Docker image for the CLI.

New in 0.2.0 (CHANGELOG "Unreleased" on the PR branches, `docs/PROBLEMS.md`,
`sites/docs/guide/setup-links.md`): plain-language problems (title and hint, the server's answer on a
details line or hover; a stable `problem {kind, title, hint}` in the API); `siple:` setup links that
prefill an account (never a password; review, continue, type the password, test, then save);
`[ui] developer_tools` gates the footer Log / Open logs / Open config and the registration and
subscription times in tooltips; a Flatpak edition `io.zyno.siple` exists, but the Flathub
submission is deferred (never "on Flathub" or "coming to Flathub"); rsipstack 0.8.6 (stricter parsing, Kamailio
TCP/TLS registration fix).

Pre-1.0. Never claim: user counts, benchmarks, testimonials, ratings, prices, store availability,
"open source" or "free software".

## Brand Commitments

- Name: **Siple**. App icon: white handset on a teal-to-blue rounded square
  (`zyno-io/siple:packaging/icons/siple-1024.png`). The app's own accent is that teal-blue
  (`#17769A` light / `#3FB2D4` dark in `crates/siple/src/theme.rs`).
- The site uses the **PixelCI design system** ("The Review Desk": cool grey desk, white cards,
  hairlines, one accent, status as tag triplets, Inter + Font Awesome Free), pinned by the owner,
  adapted to Siple's identity.
- Licence wording: **source-available** under FSL-1.1-ALv2, converting to Apache-2.0 two years
  after each release. Licensor: Signal24 LLC dba Zyno Consulting. Never "open source".
- Sibling product: ZynoTalk (https://zyno.talk), a cloud phone system by the same company. Siple
  works with any SIP provider; ZynoTalk is never implied to be required.

## Evidence on Hand

- Real rendered app screenshots, light and dark, wide/medium/narrow:
  `zyno-io/siple:crates/siple/tests/snapshots/*.png` (two lines, transfer consult, incoming,
  BLF keys/lamps/list, idle, settings, wizard).
- Real CLI commands and method descriptions (`docs/API.md`, `skills/siple/SKILL.md`).
- No testimonials, customer logos, usage numbers, press or benchmarks exist; do not fabricate them.

## Product Principles

1. Show the real app; no mockups that drift from it.
2. One state machine for people and agents: the agent story is the same product, not an add-on.
3. Honest release state: say plainly what is signed, what is a draft, what is coming soon.
4. Plain licence language: source-available, what you can and cannot do.

## Accessibility & Inclusion

WCAG 2.2 AA: contrast in both themes, visible focus, alt text on every screenshot, reduced motion
respected, usable at 320–390px phone widths. *(inferred: no product-specific standard was stated)*
