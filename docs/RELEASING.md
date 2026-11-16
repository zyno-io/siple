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
3. The signing jobs and the `release` job wait for approval of the `release` environment (if reviewers are set).
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

Secrets are only in jobs that never run `cargo` (a malicious build script cannot read them). macOS is signed on a
macOS runner because the bundle is built there, `rcodesign` runs natively, and `hdiutil` (dmg) and `xcrun stapler` /
`spctl` (verification) need macOS.

Repository settings (do once): create the environment `release` with deployment branches/tags limited to `v*` and
required reviewers; put the secrets below in that **environment**, not in repository secrets.

## Secrets (environment `release`)

Same names and formats as `sp2p`, so the same values work if the owner allows reuse (see open questions in the PR).

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
- `rcodesign` 0.27.0 is pinned by sha256 in the workflow. To bump: download the new
  `apple-codesign-<v>-macos-universal.tar.gz` from the `indygreg/apple-platform-rs` release, verify it against the
  release page, update version and hash.

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

Any platform (checksums and build provenance; the attestation proves the file was built by this repository's release
workflow from the tagged commit):

```sh
sha256sum -c SHA256SUMS --ignore-missing          # shasum -a 256 -c on macOS
gh attestation verify Siple-<version>-linux-x86_64.tar.gz --repo zyno-io/siple
gh attestation verify SHA256SUMS --repo zyno-io/siple
```

## Not covered yet

- Linux packages are not signed beyond checksums + attestations (no `.deb`/`.rpm`/AppImage yet).
- The Windows zip is signed inside (exes); the zip itself is covered by `SHA256SUMS` and the attestation.
