# Troubleshooting

## Start with status and the SIP trace

1. `siple-cli ctl status` shows registration state and last error per account, plus `config.last_error`.
2. Read the SIP trace for the account: `logs/sip/<account>/sip.<date>.log` under the config directory. It shows every message on
   the wire. Look for 401 or 403 loops (wrong `auth_username` or `password`), 408 (nothing answered: firewall, wrong
   `registrar` or transport) and 488 (codec or SRTP mismatch: check `srtp` and `codecs`).
3. A bad config shows a banner in the GUI; the previous good config stays active, and the `config.error` event carries the
   exact message.

## Where the files are

| What | macOS | Windows | Linux |
|---|---|---|---|
| Config | `~/Library/Application Support/Siple/config.toml` | `%APPDATA%\Siple\config.toml` | `$XDG_CONFIG_HOME/siple/config.toml` |
| Application log | `<config dir>/logs/siple.<date>.log` | same | same |
| SIP traces | `<config dir>/logs/sip/<account>/sip.<date>.log` | same | same |

`--config PATH` moves everything; `[general] log_dir` moves only the logs. `[general] log_level` accepts levels such as `info`
and `debug`.

## Before you share a log

Both options are off by default. Turn them on first:

```toml
[logging]
redact_credentials = true   # digest responses, the control token and passwords
redact_srtp_keys = true     # SRTP master keys
```

Siple never writes plaintext passwords to a log itself, and there is no telemetry (the only network activity Siple starts on its own is the optional [update check](/guide/updates)). A per-account `sip_trace = false` switches
that account's wire trace off. See [Logs and secrets](/reference/configuration#logs-and-secrets).

## Common problems

**macOS: no microphone or silent transmit.** Use the app bundle and accept the prompt, or enable Siple under System Settings,
Privacy & Security, Microphone. `siple-cli ctl status` shows `audio.mic_permission`.

**Windows: one-way or no audio.** Allow `siple.exe` in Windows Defender Firewall for private and public networks, or add a
rule for UDP 16384-32767 (`[media] rtp_port_range`). Behind NAT consider `symmetric_rtp = true` and `public_address`.

**TLS to a self-signed PBX.** Prefer pinning (`tls_fingerprint`) or `tls_ca_file` over `tls_verify = false`. Siple speaks
TLS 1.2 and 1.3 with ECDHE suites; older-only servers are not supported.

**FreeSWITCH and SRTP.** FreeSWITCH accepts SRTP when offered. Its stock dialplan makes SRTP mandatory
(`rtp_secure_media=true`) on the caller's leg only when the caller already offers SDES crypto; the bridged leg is not
affected. To keep it optional, change those dialplan actions to
`rtp_secure_media=optional:AES_CM_128_HMAC_SHA1_80:AES_CM_128_HMAC_SHA1_32`. With `srtp = "optional"`, a 488, 415 or 606
rejection is retried once with plain RTP.

**Config edits are ignored.** An invalid file never replaces the last good one; check the banner or `config.last_error`.
Unknown keys are errors, so a typo is reported rather than silently ignored.

If nothing here helps, open an issue on [GitHub](https://github.com/zyno-io/siple/issues) with a redacted trace.
