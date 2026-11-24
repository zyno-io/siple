# Security policy

## Reporting a vulnerability

Please report security problems privately. Do not open a public issue or pull request.

- Preferred: GitHub private vulnerability reporting, "Report a vulnerability" under the repository's
  **Security** tab (`https://github.com/zyno-io/siple/security/advisories/new`).
- Or email oss@zyno.io.

Include the Siple version, OS, a description of the impact and steps to reproduce. Remove passwords and
tokens from any SIP trace you attach.

You can expect an acknowledgment within 5 business days and a status update within 14 days. We
will coordinate a fix and a disclosure date with you and credit you in the release notes unless you
prefer otherwise.

## Supported versions

Siple is pre-1.0. Only the latest released minor version receives security fixes.

| Version | Supported |
|---|---|
| latest release | yes |
| older releases | no |

## Release signing key

Releases carry `SHA256SUMS.minisig`, a [minisign](https://jedisct1.github.io/minisign/) signature of `SHA256SUMS` (the trusted comment
is `siple <tag> SHA256SUMS`). Direct-download builds use it to install updates in place, and you can check a download with it:

```sh
minisign -Vm SHA256SUMS -P RWRY7rDN9/UXId/KmnBcB1BTI/HNfTk0mM1OD+bxK4JUKDG5RLZGYIIH
```

- Key id (fingerprint): `2117F5F7CDB0EE58`
- Public key: `RWRY7rDN9/UXId/KmnBcB1BTI/HNfTk0mM1OD+bxK4JUKDG5RLZGYIIH` (also `packaging/minisign.pub` in the repository) <!-- gitleaks:allow (public key) -->

A different key means a different project or a compromise: do not trust it, and report it. A key rotation is announced in the release
notes one release ahead. The in-app updater also requires the operating system's publisher check (macOS Developer ID and
notarization, Windows Authenticode) before it replaces anything; see `docs/CONFIG.md`, "In-place install".

## Scope notes

In scope: the SIP/RTP/SRTP/TLS handling, the local control API (Unix socket, named pipe, loopback TCP
with token), the MCP bridge, config handling, the release artifacts and the in-app updater's download and verification.

Things that are deliberate, documented behaviors rather than vulnerabilities:

- `tls_verify = false` disables certificate checking for that account.
- SIP traces and logs can contain credentials unless `[logging] redact_credentials` and
  `redact_srtp_keys` are enabled (see `docs/CONFIG.md`, "Logs and secrets"). Plaintext account passwords are never logged.
- Anyone who can already run code as the same OS user can use that user's control socket.
- Speech hooks (`[speech]`) run programs the user configured; they are never run through a shell.
