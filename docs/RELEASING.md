# Releasing

A release is a `v*` tag. `.github/workflows/release.yml` builds Linux, macOS and Windows (x64 and arm64), signs (and on macOS
notarizes), writes `SHA256SUMS`, attaches build-provenance attestations (public repo only: GitHub stores them for
private repos only on paid plans, so they are skipped before launch), and creates a **draft** GitHub Release.
You review the draft and publish it by hand.

## Cut a release

1. Bump `version` under `[workspace.package]` in `Cargo.toml`, update `CHANGELOG.md`, merge to `main`.
2. Tag `v<version>` (the `preflight` job fails if the tag and `Cargo.toml` disagree) and push it:
   `git tag -s v0.2.0 -m "Siple 0.2.0" && git push origin v0.2.0`.
   For a release candidate tag `v<version>-rc.<n>` (e.g. `v0.2.0-rc.1`): the draft is marked as a pre-release
   and the packages keep the numeric version (macOS and Windows version fields must be numeric).
3. Nothing to approve: the `release` environment only limits its secrets to `v*` tags (no required reviewers, by the
   owner's choice), so a pushed tag signs and drafts the release automatically.
4. Check the draft release (4 packages: Linux, macOS, Windows x64 and arm64; `THIRD-PARTY-NOTICES.html`, `SHA256SUMS`), edit
   the generated notes, publish.

`workflow_dispatch` (Actions tab, "Release") builds **unsigned** dev packages as `unsigned-*` run artifacts. It uses
no secrets, signs nothing and publishes nothing. A tag without all secrets below fails (it never ships unsigned).

## How it is laid out

| Job | Runner | Secrets | What |
| --- | --- | --- | --- |
| `preflight` | ubuntu | no | tag == `v` + workspace version |
| `linux`, `macos`, `windows`, `licenses` | per OS | no | cargo build + package. On tags the macOS/Windows jobs stop before signing. `windows` is an x64/arm64 matrix; arm64 is cross-compiled on the x64 runner |
| `macos-sign` | macos | `release` env | rcodesign: sign `.app` -> notarize -> staple -> zip + dmg -> sign, notarize, staple dmg |
| `windows-sign` | windows | `release` env | per arch: Azure Artifact Signing of `siple.exe` + `siple-cli.exe`, verify, then zip |
| `release` | ubuntu | `release` env | `SHA256SUMS`, attestations, `gh release create --draft` |

`flatpak.yml` (no secrets) is a separate run, like `store.yml`: it builds, lints and smoke-tests the Flatpak bundles as workflow
artifacts and never blocks `release.yml`. See "Flatpak (Flathub)".

Secrets are only in jobs that never run `cargo` (a malicious build script cannot read them). macOS is signed on a
macOS runner because the bundle is built there, `rcodesign` runs natively, and `hdiutil` (dmg) and `xcrun stapler` /
`spctl` (verification) need macOS.

Repository settings (done): the environment `release` with deployment tags limited to `v*` and no required reviewers
(releases are automatic on tag); the secrets below live in that **environment**, not in repository secrets.

The CLA workflow (`.github/workflows/cla.yml`) stores signatures in `signatures/cla.json` on the branch
`cla-signatures`. That branch must exist (created 2026-11-17 as an orphan branch) and must stay outside any branch
protection or ruleset, or signatures cannot be recorded. Before merging an outside pull request, check its author
is listed in that file.

## Secrets (environment `release`)

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE_P12` | base64 of the "Developer ID Application" certificate + private key, exported as `.p12` |
| `APPLE_CERTIFICATE_PASSWORD` | the password set on that export |
| `APP_STORE_CONNECT_API_KEY` | base64 of the JSON `{"issuer_id": "...", "key_id": "...", "private_key": "-----BEGIN PRIVATE KEY-----\n..."}` |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | service principal allowed to sign with the Artifact Signing profile |

**Developer ID certificate.** In Keychain Access, "My Certificates", select `Developer ID Application: <Team>` (expand it
to include the key), File > Export Items > `.p12`, set a password. `base64 -i cert.p12 | pbcopy` (macOS) is the secret
value. Delete the `.p12` afterwards.

**App Store Connect API key.** App Store Connect > Users and Access > Integrations > Team Keys, role Developer (enough
for notarization). Download `AuthKey_<KEYID>.p8` once. Build the JSON with the issuer ID shown on that page:
`jq -n --arg i ISSUER --arg k KEYID --rawfile p AuthKey_KEYID.p8 '{issuer_id:$i,key_id:$k,private_key:$p}' | base64`.
The workflow feeds it to `rcodesign encode-app-store-connect-api-key`.

**Azure Artifact Signing.** Account `Signal24` (endpoint `https://eus.codesigning.azure.net/`), certificate profile
`zc-distribution` (both set in `release.yml`). Create an app registration + client secret and give its service
principal the "Artifact Signing Certificate Profile Signer" role on the account or profile:
`az ad sp create-for-rbac --name siple-release-signing` then assign the role. The output maps to the three `AZURE_*`
secrets (`tenant`, `appId`, `password`). Rotate the secret before it expires.

## macOS details

- Signing: `scripts/sign-macos.sh` (rcodesign, hardened runtime, `packaging/macos/entitlements.plist`; nested
  `siple-cli` gets identifier `io.zyno.siple-cli`). Notarization + stapling: `scripts/notarize-macos.sh`. The zip and
  dmg are made by `scripts/dist-macos.sh`. All three skip with a message when the credential env vars are unset, so
  `scripts/package-macos.sh` keeps working for dev builds; `REQUIRE_SIGNING=1` (set by the workflow) turns a skip into
  an error.
- Entitlements: only `com.apple.security.device.audio-input` (microphone). The app is not sandboxed, uses Metal
  through wgpu (no JIT), loads no third-party dylibs, and the network, Keychain and tray need no entitlement.
  Add nothing else unless a notarized build demonstrably fails without it.
- `rcodesign` 0.29.0 is pinned by sha256 in the workflow. To bump: download the new
  `apple-codesign-<v>-macos-universal.tar.gz` from the `indygreg/apple-platform-rs` release, verify it against the
  release page, update version and hash.

## Version of release candidates

The packages keep the numeric Cargo version (macOS and Windows version fields must be numeric), so `0.2.0-rc.1` and `0.2.0-rc.2`
would both report `0.2.0`. To tell them apart, the workflow sets `SIPLE_BUILD_TAG` (workflow-level `env`, the tag name, tag builds
only; empty on manual runs) and `siple-core` reads it at compile time with `option_env!`
(`crates/siple-core/src/update/version.rs`): when it is a valid version that starts with the Cargo version, its `-rc.N` suffix
becomes part of the full version shown by `status.version`, `siple --version` and the Settings page, and used by the update check.
A tag for another version is ignored. Packaging scripts need no change.

## Update check, and store builds

Where the update check looks is decided at run time from how the process was installed (`[updates]` in `docs/CONFIG.md`; detection
only): direct-download builds read the GitHub releases list, the Mac App Store edition asks Apple's lookup of its listing, the
Microsoft Store edition asks Windows' Store service; the Flatpak is a store build (`--features store`) that makes no check at all
(Flatpak updates it, `disabled_reason: managed_by_flatpak`). Releases that should be offered on GitHub must be **published** (drafts are
skipped), tagged `v<semver>`; release candidates `v<semver>-rc.N` are marked as GitHub pre-releases and are only offered to users on the
`prerelease` channel (the default for someone already running a release candidate).

Builds for the Mac App Store and the Microsoft Store update through their store: they never read the GitHub releases list and never
download or install anything themselves. Build them with the cargo feature `store` (off by default):

```sh
cargo build --release --locked -p siple -p siple-cli --features store
```

The feature is declared on `siple-core`, `siple` and `siple-cli` (the last two forward it). It removes code, it does not select
behaviour: with it the GitHub source (its API request and releases parser) is not compiled, and neither is any downloader or
installer. The store update checks stay in every build, so they can be tested everywhere; only the call into Windows' Store service
(the `windows` crate, enabled by the feature) is Windows-and-`store` only. A store build run outside a store package has no update
source (`app.update_status`: `source: none`, `disabled_reason: store_build_outside_store`), and a direct build inside a package has
none either (`direct_build_in_package`). `scripts/verify-macos-mas.sh` checks the Mac App Store binary: no GitHub API path, no
release download URL, no `SHA256SUMS.minisig`, and the App Store lookup present. CI runs clippy and the `siple-core` tests with the
feature. The store packaging builds with it: see "Store packages" below.

### Release signature

The `release` job signs `SHA256SUMS` with minisign (`SHA256SUMS.minisig`, uploaded with the other assets and attested). The secret
key is the `MINISIGN_SECRET_KEY` secret of the `release` environment (the complete secret key file, no password); the public key
is `packaging/minisign.pub` (key id `2117F5F7CDB0EE58`). The trusted comment is `siple <tag> SHA256SUMS` (for example
`siple v0.1.2 SHA256SUMS`), and the job verifies the signature with the committed public key before the draft release is created.
Keep an offline backup of the key. The app embeds the public key in `RELEASE_KEYS` (`crates/siple-core/src/update/sums.rs`, a list so
that a key can be rotated: ship the next key in a release, then sign with it in a later one).

### Release asset contract (an API of the in-app updater)

Direct builds install updates in place from these assets (see "In-place install" in `docs/CONFIG.md`). Changing a name, a layout or the
signature is a change of the updater's interface: old versions in the field stop installing until they are updated by hand. Keep:

| Asset | Contract |
|---|---|
| `SHA256SUMS` | Lines `<64 lowercase hex>␠␠<file name>`, plain file names, each ending in a newline. One line per asset. |
| `SHA256SUMS.minisig` | minisign signature (prehashed) of `SHA256SUMS` by the release key, trusted comment exactly `siple <tag> SHA256SUMS`. A release without it is not installed in place. |
| `Siple-<version>-macos.zip` | `<version>` is the full version (`0.2.0`, `0.2.0-rc.1`). Exactly one top-level entry, `Siple.app`, made by `ditto -c -k --keepParent`; app identifier `io.zyno.siple`, `Contents/MacOS/siple-cli` identifier `io.zyno.siple-cli`, Developer ID team `VUDU4KEQ99`, notarized; `CFBundleShortVersionString` is the numeric core (`0.2.0` for `0.2.0-rc.1`). |
| `Siple-<core>-windows-x64.zip`, `Siple-<core>-windows-arm64.zip` | `<core>` is the numeric core (a release candidate keeps `0.2.0`). Flat zip: `siple.exe`, `siple-cli.exe`, `config.example.toml`, `LICENSE`, `README.md`, `THIRD-PARTY-NOTICES.html`, `docs/API.md`, `docs/CONFIG.md`. Both exes Authenticode-signed by Signal24 LLC (Artifact Signing, RFC 3161 timestamp), PE machine of the architecture, `FileVersion` the numeric core. |
| `Siple-<core>-linux-x86_64.tar.gz` | One top directory `siple-<core>/` with `bin/siple`, `bin/siple-cli`, `share/...`, `LICENSE`, `THIRD-PARTY-NOTICES.html`; regular files and directories only. `bin/siple --version` and `bin/siple-cli --version` print the full version. |

The updater accepts the full-version or the numeric-core spelling of a file name (the release candidates mix them) and requires
exactly one match per platform. The Windows signer pins (publisher name, the Artifact Signing identity EKU, the Microsoft Identity
Verification root) are in `crates/siple-core/src/update/install/windows.rs`; the macOS requirement is in `install/macos.rs`. When the
signing identity changes, ship an app that accepts both before releasing with the new one.

## Store packages (Mac App Store, Microsoft Store)

`.github/workflows/store.yml` runs on `v*` tags and on `workflow_dispatch`, as its own run (it never blocks or fails
`release.yml`). It only produces **workflow artifacts**; nothing is uploaded to a store. Both packages are built with
`--features store`.

| Artifact | Contents | Retention |
| --- | --- | --- |
| `store-macos-appstore-<label>` | `Siple-<label>-mas.pkg` (signed), `SHA256SUMS`, `SUBMIT.md` | final 90 days, rc 30 |
| `store-windows-msix-<label>` | unsigned `Siple_<ver>_x64_arm64.msixbundle`; only with the `MSIX_*` variables set and a Store-eligible version (`X.Y.Z.0`) | 90 days |
| `sideload-windows-msix-<label>` | the same bundle test-signed with a throwaway certificate, `siple-test.cer`, `install-test.ps1`, README | 30 days |
| `mas-unsigned-app`, `msix-stage-<arch>` | intermediates for the signing and packaging jobs | 7 days |

Jobs: `version` (runs `scripts/store-version.sh --self-test`), `mas-build` (no secrets: `MAS=1 BUNDLE_ONLY=1
scripts/package-macos.sh`, then `verify-macos-mas.sh --unsigned`), `mas-sign` (environment `store`, runs no cargo),
`msix-build` (x64 and arm64, `package-windows.ps1 -StageOnly -Store`), `msix-package` (no secrets at all), `summary`.

### Download and submit

```sh
gh run download <run-id> -n store-macos-appstore-0.2.0-rc.1      # or the Actions UI
gh run download <run-id> -n store-windows-msix-0.2.0
```

- **Mac App Store.** Open Transporter, sign in with the developer-team Apple ID, drag in `Siple-<label>-mas.pkg` (or + then
  Deliver). It appears under App Store Connect > Siple > TestFlight once processed. An rc goes to TestFlight; a final tag
  can be added to a version and submitted for review. "Missing Compliance" on a build is the export-compliance question
  (see below).
- **Microsoft Store.** Partner Center > Siple > Submission > Packages: upload the `.msixbundle`. It is unsigned on purpose
  (the Store signs it). Only a final tag (or a dispatch with `msix_store_version`) produces one.
- **Sideload test (Windows).** From the `sideload-windows-msix-<label>` artifact, in an elevated PowerShell:
  `.\install-test.ps1` (imports the `.cer` into `LocalMachine\TrustedPeople`, then `Add-AppxPackage
  -ForceUpdateFromAnyVersion`); `.\install-test.ps1 -Uninstall` removes the package and the certificate. The sideload
  package carries the same identity as the Store one: uninstall one before installing the other. Uninstalling an MSIX
  deletes its LocalState (config, logs).

### Version scheme (`scripts/store-version.sh`)

Both stores reject a number lower than one already uploaded, so the numbers are computed from the tag, deterministically:

- `CFBundleVersion` = `X*1000000 + Y*10000 + Z*100 + (final: 99, rc N: N, branch build: 0)`; minor and patch must be
  <= 99 and rc N in 1..98 (otherwise the script fails). `CFBundleShortVersionString` = the Cargo version.
- MSIX: final `X.Y.Z.0` (the only form the Microsoft Store accepts: it reserves the fourth field); rc `X.Y.Z.N` (sideload
  only); branch build `X.Y.Z.(1 + run % 65000)` (sideload only).

| Tag | CFBundleVersion | MSIX | Use |
| --- | --- | --- | --- |
| `v0.2.0-rc.1` | 20001 | 0.2.0.1 | TestFlight; MSIX sideload |
| `v0.2.0-rc.2` | 20002 | 0.2.0.2 | same |
| `v0.2.0` | 20099 | 0.2.0.0 | both stores |
| `v0.2.1` | 20199 | 0.2.1.0 | both stores |

Consequences: re-running the workflow on a tag gives the same numbers, so uploading it again is a "redundant binary": tag a
new rc to ship a fix. Release trains must ship in order. The tag must match `Cargo.toml` (the `version` job fails
otherwise).

### Setup (owner prerequisites)

GitHub environment `store`: deployment tags limited to `v*`, no required reviewers (same as `release`). It holds the
Mac App Store signing secrets. Nothing for the Microsoft Store is secret.

| Kind | Name | Value |
| --- | --- | --- |
| secret (`store` env) | `MAS_CERTS_P12` | base64 of ONE `.p12` holding both identities: **Apple Distribution** and **Mac Installer Distribution** (Xcode > Settings > Accounts > Manage Certificates creates them; export both in Keychain Access) |
| secret (`store` env) | `MAS_CERTS_PASSWORD` | the password of that `.p12` |
| secret (`store` env) | `MAS_PROVISIONING_PROFILE` | base64 of the **Mac App Store Connect** distribution profile for `io.zyno.siple` (the Apple Distribution certificate, the app group). Renew yearly; the signing step warns 30 days ahead and fails if expired |
| secret (`store` env) | `APP_STORE_CONNECT_API_KEY` | only for `MAS_VALIDATE`: same base64 JSON as in "Secrets" above |
| variable | `STORE_MAS_ENABLED` | `true` once the secrets exist: from then on missing secrets fail the job instead of skipping it |
| variable | `STORE_MSIX_ENABLED` | `true` once the `MSIX_*` variables exist (same semantics) |
| variable | `MAS_VALIDATE` | `true` to run `xcrun altool --validate-app` after the artifact is uploaded (needs the App Store Connect app record and `APP_STORE_CONNECT_API_KEY`) |
| variable | `MSIX_IDENTITY_NAME` | Partner Center > Product identity > Package/Identity/Name |
| variable | `MSIX_PUBLISHER` | Package/Identity/Publisher (`CN=<GUID>`) |
| variable | `MSIX_PUBLISHER_DISPLAY_NAME` | Package/Properties/PublisherDisplayName |
| variable | `MSIX_DISPLAY_NAME` | optional, default `Siple` |

These values must match Partner Center exactly or the upload is rejected. Before the first upload also create the App ID
`io.zyno.siple` (with the App Groups capability) and the App Store Connect app record (the API cannot create it),
and reserve the app name in Partner Center.

Skip behaviour: while a `*_ENABLED` variable is unset, a gated job prints a notice and stays green (`mas-sign` skips its
steps; `msix-package` builds only the placeholder-identity sideload package). On a branch (`workflow_dispatch`) `mas-sign`
does not run at all. If the secrets arrive after a tag was pushed, re-run the workflow with `workflow_dispatch` on that tag
(the tag ref satisfies the `store` environment rule). `msix_store_version` (dispatch input, `X.Y.Z.0`) lets a branch build
produce a Store-eligible bundle for a first Partner Center certification test.

### Scripts

| Script | What |
| --- | --- |
| `scripts/store-version.sh [TAG] [RUN] / --self-test` | the version numbers above, as `KEY=VALUE` |
| `MAS=1 scripts/package-macos.sh` | `--features store`, `MACOSX_DEPLOYMENT_TARGET=13.0`, store Info.plist patches, privacy manifest; stops at `dist/mas/Siple.app` |
| `scripts/sign-macos-mas.sh <app> <pkg>` | profile checks, inside-out `codesign` (`siple-cli` first, never `--deep`), `productbuild`. `MAS_LOCAL_TEST=1` signs a copy with any identity and no identifier keys, for running the sandboxed app locally |
| `scripts/verify-macos-mas.sh [--unsigned\|--local-test] <app> [pkg]` | SDK/minos, universal, Info.plist, no update-check code, the CLI's embedded Info.plist, entitlements, signatures, pkg |
| `scripts/package-windows.ps1 -Store` | stage with `--features store` (`dist\Siple-<v>-windows-<arch>-store`) |
| `scripts/package-msix.ps1 [-TestSign]` | manifest from `packaging/windows/AppxManifest.xml.in`, `makepri`, `makeappx pack` + `bundle`, validation, optional test-signed sideload copy |
| `scripts/install-test.ps1` | trust the test certificate and install / uninstall the sideload bundle |
| `python scripts/make-icons.py --msix-only` | regenerate `packaging/windows/msix-assets/` (checked in) |

### Export compliance and remaining work

Export compliance: `MAS=1 scripts/package-macos.sh` sets `ITSAppUsesNonExemptEncryption = false` (standard algorithms only),
so TestFlight builds do not wait at "Missing Compliance".

What is left before the store editions are listed: [STORE.md](STORE.md).

## Flatpak (Flathub)

`.github/workflows/flatpak.yml` runs on `v*` tags, on `workflow_dispatch`, weekly (runtime, Rust extension and linter drift) and on
pull requests that touch the packaging files. It is its own run (it never blocks `release.yml`) and has no secrets. It builds
`packaging/flatpak/io.zyno.siple.yml` on x86_64 and aarch64 natively, inside Flathub's build image
(`ghcr.io/flathub-infra/flatpak-github-actions:freedesktop-26.08`, pinned by digest), like Flathub does (screenshot mirroring;
without `--sandbox`, which refuses the manifest's local `type: dir` source: the Flathub-rendered manifest uses a git source and
builds with it), with `--features store`. It runs `desktop-file-validate`, `appstreamcli validate`, `flatpak-builder-lint`
on the manifest and the repo, checks that the binaries carry no updater markers, and smoke-tests the bundle in the container.

Output is **workflow artifacts only**: `flatpak-x86_64` and `flatpak-aarch64` (`Siple-<version>-<arch>.flatpak`, for test installs
with `flatpak install --user ./Siple-*.flatpak`) and `flatpak-sources`. A bundle never updates itself, so it is never attached to
a release and is not part of the updater's asset contract.

Local build, on a Linux host with docker (no flatpak needed; everything runs in containers):

```sh
scripts/flatpak-local.sh            # cargo-sources.json, notices, build, lint, smoke test -> dist/flatpak/
scripts/flatpak-local.sh --clean    # remove flatpak-work/, dist/flatpak/ and the docker volume
```

The notices need `cargo-about` on the host; without it the local build uses a placeholder.

### Release checklist

1. When `CHANGELOG.md` gets the version heading, add the `<release version="X.Y.Z" date="YYYY-MM-DD">` entry (newest first) to
   `packaging/linux/io.zyno.siple.metainfo.xml`. CI enforces it (`scripts/flatpak-check-release.sh`); the screenshot URLs there point
   at the goldens of an earlier tag and stay valid.
2. After the GitHub Release is published, run `scripts/flathub-manifest.sh vX.Y.Z`. It writes `dist/flathub/io.zyno.siple.yml` (git
   `tag` + `commit` source, the notices from the release with the sha256 of the signed `SHA256SUMS`) and `cargo-sources.json`
   (regenerated from the tag's `Cargo.lock`). Open a pull request with both in `flathub/io.zyno.siple`. The script needs `minisign`
   to verify the release's `SHA256SUMS` signature and fails without it; `--no-verify` skips that check with a loud warning (the
   notices checksum is then unauthenticated: never submit such output).

The first submission is a pull request against the `new-pr` branch of `flathub/flathub`, with the finish-args justifications from
the comments in the manifest in the description. Flathub verification needs the token Flathub gives published at
`https://zyno.io/.well-known/org.flathub.VerifiedApps.txt`. Release candidates are not for Flathub stable. There is no
`x-checker-data` or automerge: the checker cannot regenerate `cargo-sources.json` or the notices checksum.

### Bumping the pins

- **Generator** (`scripts/flatpak-cargo-sources.sh`): choose a commit of `flatpak/flatpak-builder-tools`, read
  `cargo/flatpak-cargo-generator.py` there, then set `GEN_COMMIT` and `GEN_SHA256` (`shasum -a 256`). The script verifies the
  generated sources against `Cargo.lock` crate by crate (archives, per-crate checksum files and the cargo config; anything else
  fails, see `scripts/flatpak-cargo-sources.sh --self-test`). Its Python dependencies are hash-pinned in
  `scripts/flatpak-cargo-requirements.txt` (regenerate with `uv pip compile --generate-hashes`, see the header of that file).
- **Build image**: take the new digest of the `freedesktop-<runtime>` tag (`docker buildx imagetools inspect`) and change it in
  `.github/workflows/flatpak.yml` and `scripts/flatpak-local.sh`.
- **Runtime** (`runtime-version` and the `rust-stable` extension in the manifest): yearly; the linter's `eolruntime` check warns
  when it is time. Keep the build image tag in step.

## Verify a download

macOS:

```sh
codesign -dv --verbose=4 /Applications/Siple.app        # Authority=Developer ID Application, flags=...(runtime)
codesign --verify --deep --strict /Applications/Siple.app
spctl -a -vv /Applications/Siple.app                    # accepted, source=Notarized Developer ID
xcrun stapler validate /Applications/Siple.app
```

Windows (PowerShell, after extracting the zip):

```powershell
Get-AuthenticodeSignature .\siple.exe, .\siple-cli.exe | Format-List Status, SignerCertificate, TimeStamperCertificate
# Status must be Valid
```

Any platform (checksums, the minisign signature and build provenance; the attestation proves the file was built by this
repository's release workflow from the tagged commit). The public key is in `packaging/minisign.pub` and `SECURITY.md`:

```sh
minisign -Vm SHA256SUMS -P RWRY7rDN9/UXId/KmnBcB1BTI/HNfTk0mM1OD+bxK4JUKDG5RLZGYIIH   # needs SHA256SUMS.minisig next to it
sha256sum -c SHA256SUMS --ignore-missing          # shasum -a 256 -c on macOS
gh attestation verify Siple-<version>-linux-x86_64.tar.gz --repo zyno-io/siple
gh attestation verify SHA256SUMS --repo zyno-io/siple
```

## Not covered yet

- Linux packages are not signed beyond checksums + attestations (no `.deb`/`.rpm`/AppImage yet). The Flathub listing is pending (see "Flatpak (Flathub)").
- The Windows zip is signed inside (exes); the zip itself is covered by `SHA256SUMS` and the attestation.
