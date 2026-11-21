# Troubleshooting

Find your symptom below, try the likely fixes, and collect a log if you need help.

## You can't hear the other person (or they can't hear you)

Likely causes:

- Siple has no permission to use your microphone (Mac).
- The wrong microphone or speaker is selected.
- The Windows firewall is blocking call audio.
- You are behind NAT (a router that hides your computer's address) and the audio cannot find its way back.

What to do:

1. Select **Audio** at the bottom of the Siple window and check the **Microphone**, **Speaker** and **Ringer** choices.
   Then open **Settings › Audio**: talk and watch the level meter move, and try **Test speaker** and **Echo test**.
2. If a red message says microphone access was denied, restricted or not granted yet, follow [No microphone on a
   Mac](#no-microphone-on-a-mac) or the Windows step below.
3. On Windows, allow `siple.exe` in Windows Defender Firewall for private and public networks. Without that you get one-way
   audio or none. See [Install](/guide/install).

::: details In config.toml: firewall, NAT and SRTP
**Windows: one-way or no audio.** Allow `siple.exe` in Windows Defender Firewall for private and public networks, or add a
rule for UDP 16384-32767 (`[media] rtp_port_range`). Behind NAT consider `symmetric_rtp = true` and `public_address`.

**Mac: no microphone or silent transmit.** `siple-cli ctl status` shows `audio.mic_permission`.

**FreeSWITCH and SRTP.** FreeSWITCH accepts SRTP when offered. Its stock dialplan makes SRTP mandatory
(`rtp_secure_media=true`) on the caller's leg only when the caller already offers SDES crypto; the bridged leg is not
affected. To keep it optional, change those dialplan actions to
`rtp_secure_media=optional:AES_CM_128_HMAC_SHA1_80:AES_CM_128_HMAC_SHA1_32`. With `srtp = "optional"`, a 488, 415 or 606
rejection is retried once with plain RTP.
:::

## Your account won't connect

The account's lamp in the top bar is red or amber and the account does not say **registered**.

Likely causes:

- A wrong user name, auth name or password. The server keeps answering 401 or 403.
- A wrong server (domain) or transport. Nothing answers, which shows as 408.
- A firewall or no internet connection.
- You just changed network, and the account is waiting for it. It reads **waiting for network** until a route exists.

What to do:

1. Click the account in the top bar. The menu names the problem in plain words (for example **Wrong username or
   password**), says what to try, and shows the server's answer underneath (for example `403 Forbidden`).
2. Open **Settings › Accounts** and check **Username**, **Password** and **Domain** against what your IT
   team or provider gave you.
3. If it says **waiting for network**, check your Wi-Fi or cable. See [Network changes](/guide/network).
4. For a server with a self-signed certificate, see the TLS details below.
5. Still stuck? [Collect logs for a bug report](#collect-logs-for-a-bug-report). The SIP trace shows every message on the wire.

::: details In config.toml: what the SIP trace tells you
Run `siple-cli ctl status` to see registration state and last error per account, plus `config.last_error`.

In the account's SIP trace, look for 401 or 403 loops (wrong `auth_username` or `password`), 408 (nothing answered:
firewall, wrong `registrar` or transport) and 488 (codec or SRTP mismatch: check `srtp` and `codecs`).

**TLS to a self-signed PBX.** TLS is the encryption on the connection to your server. Prefer pinning (`tls_fingerprint`) or
`tls_ca_file` over `tls_verify = false`. Siple speaks TLS 1.2 and 1.3 with ECDHE suites; older-only servers are not supported.
:::

## What Siple's messages mean

When something fails, Siple names the problem in plain words and says what to try. The server's own answer (such as
`486 Busy Here`) sits on a details line or in a hover. Each message below has a short code, which is what the
[API](/agents/api-reference) and `siple-cli` report as `problem.kind`.

<!--@include: ../../docs/PROBLEMS.md-->

## Calls drop when you change networks

Likely causes:

- The network change moved your computer's address and the other side did not follow, or did not answer within 10 seconds.
- You turned off moving calls in the config file.

What to do:

1. Check that your account shows **registered** again after the change.
2. Read [Network changes](/guide/network) to see what Siple does and when a call ends with `network_lost`.
3. Ring the person again. A call that ended cannot be resumed.

::: details In config.toml
`[network] move_calls = false` ends established calls with `network_lost` right away. Set it back to `true` to keep calls
across a change. See [`[network]`](/reference/configuration#network).
:::

## Your changes to config.toml don't take effect

<Shot name="config-error" alt="The Siple window with a red Config not applied banner and an Open config button" caption="A bad config file shows a banner. The previous good settings stay active." />

Likely causes:

- The file has a mistake, such as a typo in a key or a bad value.
- The setting needs an account to have no calls before it applies.

What to do:

1. Look for the red **Config not applied** banner at the top of the Siple window. It names the problem.
2. Select **Open config**, fix the line and save. Siple reloads the file by itself.
3. If **Settings** shows "The config file changed.", choose **Reload** to pick up the file or **Keep mine** to keep your
   pending changes.

An invalid file never replaces the last good one, so Siple keeps working with your old settings meanwhile. Unknown keys are
errors, so a typo is reported rather than silently ignored. See [Settings and config.toml](/guide/configuration).

::: details For scripts
`config.last_error` in `siple-cli ctl status` and the `config.error` event carry the exact message.
:::

## No microphone on a Mac

Likely causes:

- You chose Don't Allow when macOS asked for microphone access.
- A company policy restricts microphone access.
- You are running the program outside the Siple app.

What to do:

1. Open **System Settings › Privacy & Security › Microphone**.
2. Turn **Siple** on.
3. Use the Siple app from **Applications** and accept the prompt on your first call. Then try again.

## Collect logs for a bug report

Siple keeps an application log and, for each account, a SIP trace. The SIP trace shows every message sent to and from your
server.

1. Select **Log** at the bottom of the window to read recent log lines in place (turn on **Settings › Developer › Show developer tools** first).
2. Select **Open logs** to open the log folder. The same button is under **Settings › Logging & privacy**; the one at the bottom of the window needs **Show developer tools**.
3. To get more detail, change **Log level** under **Settings › Logging & privacy** (for example to **Debug**). Change it back
   afterwards.
4. Send the files with your report. To report a problem, open an issue on
   [GitHub](https://github.com/zyno-io/siple/issues) with a redacted trace.

Do [Share a log safely](#share-a-log-safely) first.

### Where the files are

| What | macOS | Windows | Linux |
|---|---|---|---|
| Config | `~/Library/Application Support/Siple/config.toml` | `%APPDATA%\Siple\config.toml` | `$XDG_CONFIG_HOME/siple/config.toml` |
| Application log | `<config dir>/logs/siple.<date>.log` | same | same |
| SIP traces | `<config dir>/logs/sip/<account>/sip.<date>.log` | same | same |

`--config PATH` moves everything; `[general] log_dir` moves only the logs. `[general] log_level` accepts levels such as `info`
and `debug`.

## Share a log safely

Logs can contain secrets. Both protections are off by default, so turn them on before you share anything:

1. Open **Settings › Logging & privacy**.
2. Under **Privacy**, turn on **Redact credentials in logs and traces**. This hides digest responses, the control token and
   passwords.
3. Turn on **Redact SRTP keys in logs and traces**. This hides SRTP master keys.

Siple never writes plaintext passwords to a log itself, and there is no telemetry. The only network activity Siple starts on
its own is the optional [update check](/guide/updates).

Under **SIP trace per account** you can switch off the wire trace for one account with its **Trace** switch.

::: details In config.toml
```toml
[logging]
redact_credentials = true   # digest responses, the control token and passwords
redact_srtp_keys = true     # SRTP master keys
```

A per-account `sip_trace = false` switches that account's wire trace off. See
[Logs and secrets](/reference/configuration#logs-and-secrets).
:::

If nothing here helps, open an issue on [GitHub](https://github.com/zyno-io/siple/issues) with a redacted trace.
