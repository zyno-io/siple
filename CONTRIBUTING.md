# Contributing to Siple

Thanks for helping. Siple is a source-available (FSL-1.1-ALv2) SIP softphone with a scriptable control API. Small,
focused pull requests are easiest to review. For anything large, open an issue first.

Looking for something to work on? [docs/ROADMAP.md](docs/ROADMAP.md) lists open work and known bugs;
[docs/STORE.md](docs/STORE.md) covers what is left for the app stores and Flathub.

By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md). Security problems go
through [SECURITY.md](SECURITY.md), not public issues.

## Build

Requirements: Rust 1.94+ (edition 2024), CMake and a C compiler (libopus is built from source).

| OS | Extra packages |
|---|---|
| Linux (Debian/Ubuntu) | `sudo apt install cmake pkg-config libasound2-dev build-essential` |
| macOS | Xcode command line tools, `brew install cmake` |
| Windows | Rust MSVC toolchain, Visual Studio C++ workload, CMake |

```sh
cargo build --workspace
cargo run -p siple                       # GUI
cargo run -p siple-cli -- run --headless # no window
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the crates fit together.

## Test

```sh
cargo test --workspace
```

- Unit and in-process integration tests need no PBX and no audio device.
- On Linux and Windows skip the GUI snapshot tests: `cargo test --workspace -- --skip visual_tests`.
  The reference images are rendered on macOS (Metal), so the visual tests only pass there.
- Interop tests run against a real PBX and are skipped unless `SIPLE_INTEROP=1`. See
  [Interop tests](#interop-tests). Most contributors can ignore them; CI does not run them.
- `SIPLE_SIPP=1` enables the sipp edge-case scenarios in `tests/sipp/` (needs `sipp` on `PATH`, or set
  `SIPLE_SIPP_BIN` to its path).

### Interop tests

The interop tests (`crates/siple-sip/tests/interop*.rs`, `crates/siple-core/tests/{interop_*,soak}.rs`) talk to a
FreeSWITCH or Asterisk test PBX that you run yourself. They are skipped unless gated by an env var:

| Variable | Meaning |
|---|---|
| `SIPLE_INTEROP=1` | enable the interop tests (run with `-- --test-threads=1`; they share PBX users) |
| `SIPLE_SOAK=1` | enable the long SRTP soak test |
| `SIPLE_INTEROP_STT=1`, `SIPLE_INTEROP_STT_CMD` | optional speech-to-text check of an IVR prompt |
| `SIPLE_INTEROP_RTP_RANGE` | RTP port range to use, if the PBX is behind a firewall |
| `SIPLE_WIRE=1` | print the SIP trace |

The PBX details are read from `tests/interop/pbx.env` (git-ignored), one `KEY=value` per line:
`PBX_HOST`, `PBX_DOMAIN`, `PBX_PASSWORD`, `PBX_ECHO_EXT`, `PBX_TONE_EXT` and optionally `PBX_SSH`
(an ssh target used to read registration state with `fs_cli`; skipped when unset). The tests expect
users 1001 and 1003-1005 on the PBX and an echo and a tone extension.

### Visual snapshots

GUI changes are covered by images in `crates/siple/tests/snapshots/`. When a change is meant to alter
the UI, regenerate them on macOS and commit the PNGs with the change:

```sh
UPDATE_SNAPSHOTS=1 cargo test -p siple visual
```

Look at the changed images before committing. The `*.diff.png`, `*.new.png` and `*.old.png`
scratch files are git-ignored.

## Style and checks

Run all of these before opening a PR. CI runs the same set.

```sh
# Format per package. Do NOT run `cargo fmt --all`: it reformats vendor/rsipstack.
cargo fmt -p siple-problem -p siple-media -p siple-config -p siple-core -p siple-sip -p siple -p siple-cli

cargo clippy --workspace --all-targets -- -D warnings

cargo deny check          # licences, advisories, sources (install: cargo install cargo-deny --locked)
```

Dependencies must have an OSI-approved permissive licence from the allow list in `deny.toml`
(no OpenSSL; TLS is rustls). Adding a new licence or an advisory exception needs a reason in the PR.

`vendor/rsipstack` is a patched upstream copy. Every change there is marked `// siple:` and listed in
`vendor/rsipstack/SIPLE_PATCHES.md`. Prefer fixing things in `siple-sip` when you can.

## API parity rule

Siple is driven by a GUI, a JSON-RPC control API and an MCP server. **Every new feature must ship
with its JSON-RPC method, any events it needs, and the matching MCP tool in the same change.**
GUI-only behaviour is not accepted.

In practice:

1. Put the behaviour in the engine (`siple-core`, `siple-sip`), not in the GUI crate.
2. Register the method and its JSON schema in `crates/siple-core/src/backend.rs`
   (`backend_methods`). The MCP tools and `docs/API.md` are generated from that catalog.
3. Regenerate the API reference: `scripts/gen-api-docs.sh`. Edit prose in `docs/API-intro.md`, never the
   generated part of `docs/API.md`.
4. Document new config keys in `docs/CONFIG.md` and `crates/siple-config/config.example.toml`.
5. Add tests, including one through the control API where it makes sense.

## Commits and pull requests

- Conventional-style subjects: `feat: ...`, `fix: ...`, `docs: ...`, `ci: ...`, `refactor: ...`, `test: ...`, `chore: ...`.
- Add a line to `CHANGELOG.md` for user-visible changes.
- Branch from the default branch and keep the PR to one logical change.
- Never put real credentials, hostnames or SIP traces with passwords in issues, tests or fixtures.
  Use `example.test` names and RFC 5737 addresses (`192.0.2.0/24`).

### Contributor License Agreement

Before your first pull request can be merged, sign the [Siple Contributor License Agreement](CLA.md)
(CLA). You keep the copyright in your work; the CLA gives Signal24 LLC dba Zyno Consulting a
licence to use your contribution, including under other licences than FSL-1.1-ALv2 (for example a
commercial licence, or a future change of the project licence).

Signing takes one comment: the CLA bot replies on your first pull request with the exact sentence to
post. It records your signature once for all your future contributions. If you contribute on behalf
of an employer that owns your work, contact oss@zyno.io first.

Cryptographic commit signing is welcome but optional, and `Signed-off-by` lines are not required.

## Reporting bugs

Use the bug template. Include your OS, Siple version, PBX type and, for signalling problems, the SIP
trace of the failing call (`siple-cli ctl call.sip_trace` or the `logs/sip/<account>/` files) with
passwords, `Authorization` headers and SRTP `a=crypto` keys removed.
