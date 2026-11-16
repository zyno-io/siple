# Security policy

## Reporting a vulnerability

Please report security problems privately. Do not open a public issue or pull request.

- Preferred: GitHub private vulnerability reporting, "Report a vulnerability" under the repository's
  **Security** tab (`https://github.com/zyno-io/siple/security/advisories/new`).
- Or email oss@zyno.io.

Include the Siple version, OS, a description of the impact and steps to reproduce. Remove passwords and
tokens from any SIP trace you attach.

You can expect an acknowledgement within 5 business days and a status update within 14 days. We
will coordinate a fix and a disclosure date with you and credit you in the release notes unless you
prefer otherwise.

## Supported versions

Siple is pre-1.0. Only the latest released minor version receives security fixes.

| Version | Supported |
|---|---|
| latest release | yes |
| older releases | no |

## Scope notes

In scope: the SIP/RTP/SRTP/TLS handling, the local control API (Unix socket, named pipe, loopback TCP
with token), the MCP bridge, config handling and the release artifacts.

Things that are deliberate, documented behaviours rather than vulnerabilities:

- `tls_verify = false` disables certificate checking for that account.
- SIP traces and logs can contain credentials unless `[logging] redact_credentials` and
  `redact_srtp_keys` are enabled (see `docs/CONFIG.md`, "Logs and secrets"). Plaintext account passwords are never logged.
- Anyone who can already run code as the same OS user can use that user's control socket.
- Speech hooks (`[speech]`) run programs the user configured; they are never run through a shell.
