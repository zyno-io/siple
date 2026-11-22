---
name: siple
description: Control the Siple SIP softphone from the command line with `siple-cli`. Use when the user asks to place, answer, hold, transfer or hang up phone calls, send DTMF, speak into or listen to a call, check extension (BLF) status, voicemail or registration, or change Siple's accounts and settings.
---

# Siple softphone

Siple is a desktop SIP softphone. Everything it shows and does is available through
`siple-cli ctl <method> [JSON params]`: each call connects to the running Siple, runs one
method, prints the JSON result and exits. No MCP server or other long-running helper is needed.

## Before you start

1. **Find `siple-cli`.** Try `siple-cli --version` first. If it is not on `PATH`:
   - macOS: `/Applications/Siple.app/Contents/MacOS/siple-cli`
   - Windows: `siple-cli.exe` in the same folder as `siple.exe` (wherever the user unzipped Siple;
     ask if you cannot find it)
   - Linux: `bin/siple-cli` inside the extracted `siple-<version>` folder
   - Linux AppImage: `<path>/Siple….AppImage --cli` (or a `siple-cli` wrapper script that runs it with `--cli`; a symlink fails under AppImageLauncher)
   - Linux Flatpak: `flatpak run --command=siple-cli io.zyno.siple` (file arguments only under `~/.var/app/io.zyno.siple/`)
2. **Siple must be running.** `siple-cli ctl status` fails when it is not: with
   `no running instance found`, or with a connection error if Siple crashed or was killed. Ask the user to start the Siple app. Only if they
   want no GUI, start `siple-cli run --headless` yourself (it keeps running and must stay running
   to receive calls).
3. **Check accounts.** `siple-cli ctl status` returns `accounts` (with `registration.state`),
   `calls`, `transfers`, `audio`, `instance_id` and `event_seq`.

## Updating Siple

`siple-cli ctl app.update_status` says whether a newer release exists and whether it can be installed in place
(`install.possible`); `app.install_update {"version": "<latest>"}` downloads, verifies and installs it and restarts Siple
(`restart`: `when_idle` default, `now`, or `manual`), never during a call. It restarts the app the user is using and the GUI shows
that an agent started it, so only call it when the user asked for the update; `app.cancel_update` stops it before the files are
replaced.

## Calling a method

```sh
siple-cli ctl status
siple-cli ctl call.dial '{"account":"office","target":"1002"}'
siple-cli ctl config.edit @edits.json        # params from a file (PowerShell: '@edits.json', quoted)
echo '{"call_id":"c1"}' | siple-cli ctl call.get -   # params from stdin
```

- Success: the result as JSON on stdout, exit code 0.
- Failure: exit code 1 and JSON on stderr. From Siple: a JSON-RPC error
  `{"code", "message", "data": {"kind", ...}}` where `data.kind` is `not_found`, `invalid_state`,
  `invalid_params`, `sip_error` (with `sip_code`), `timeout`, `unauthorized`, `method_not_found`,
  `unsupported` or `internal`. `unsupported` means this edition cannot do it: the Mac App Store
  and Flatpak editions have no `[speech]` hooks (`call.say`, transcription, `speech.*`). A SIP-level failure also has `data.problem {kind, title, hint}`: tell the
  user the `title` and the `hint`, and match on `kind` (never on the text). The same object is on a failed
  registration (`registration.problem`), on `end.problem` of an ended call, on `subscription.last_problem` of a
  BLF entry and on `account.test` results (`problem`). From the CLI itself (Siple not running, unreadable params, connection lost):
  `{"error": "<message>"}` with no `kind`.
- On Windows PowerShell, quoting JSON is fragile: write params to a file and pass it quoted,
  `siple-cli ctl config.edit '@edits.json'` (a bare `@` is PowerShell syntax), or pipe it:
  `Get-Content edits.json | siple-cli ctl config.edit -`.

## Discovering methods

There are about 75 methods. Do not guess names or parameters; look them up (offline, always matches
the installed version):

```sh
siple-cli methods                                   # name + description of every method
siple-cli methods --json | jq '.[] | select(.name == "call.transfer_attended")'
siple-cli methods --json | jq -r '.[].name' | grep '^call\.'
```

Groups: `status`, `account.*`, `call.*` (dial, answer, hangup, hold, transfer, DTMF, play/say,
listen, record, pickup), `transfer.*` (an attended transfer in progress), `blf.*` (extensions),
`mwi.get` (voicemail), `audio.*` (devices, mic level, echo test), `config.*`, `events.*`, `speech.*`,
`ui.*` (show/hide the window), `logs.tail`.

## Waiting without a background process

Every action returns an `event_seq`. Pass it as `since` to a wait so nothing is missed:

```sh
r=$(siple-cli ctl call.dial '{"account":"office","target":"1002"}')
call=$(jq -r .call_id <<<"$r"); seq=$(jq -r .event_seq <<<"$r")
siple-cli ctl call.wait_for "{\"call_id\":\"$call\",\"state\":\"active\",\"since\":$seq,\"timeout_ms\":30000}"
```

- `call.wait_for` waits for a call state (`ringing`, `active`, `held`, `ended`, ...); with
  `account` instead of `call_id` it catches an incoming call: `{"account":"office","state":"ringing"}`.
- `events.poll {"since": N, "timeout_ms": M}` long-polls the event log and returns
  `{events, next_seq}`; call it again with `since: next_seq`. Get a starting point with
  `events.current_seq`. Filter with `kinds` (e.g. `["call.*"]`), `account` or `call_id`.
- Prefer these over `siple-cli events`, which streams until killed.
- If `instance_id` in `status` changes, Siple restarted: call ids (`c1`, `c2`, ...) and event
  sequence numbers start over.

## Common tasks

| Task | Method |
| --- | --- |
| Place a call | `call.dial {account, target}` then `call.wait_for {call_id, state: "active", since}` |
| Answer / reject | `call.answer {call_id}` / `call.reject {call_id}` |
| Hang up | `call.hangup {call_id}` |
| Hold / resume | `call.hold` / `call.unhold {call_id}` |
| Mute | `call.mute` / `call.unmute {call_id}` |
| Send DTMF | `call.dtmf {call_id, digits}` |
| Blind transfer | `call.transfer_blind {call_id, target}` |
| Consult, then transfer | `call.consult_start`, then `transfer.complete` / `transfer.cancel` (see `methods`) |
| Speak / listen | `call.say {call_id, text, wait: true}` then `call.listen {call_id, since_rx_ms}` (needs `[speech]` commands in config) |
| Extension status | `blf.list` (or `accounts[].blf` in `status`); call one with `blf.dial` |
| Pick up a ringing extension | `call.pickup` |
| Test an account | `account.test {account}` |
| Change settings | `config.edit` (typed edits; comments in config.toml are kept), e.g. `move_account {id, index}` |
| Set a password | `account.set_password {account, password}` (never put passwords in `config.edit` output you show) |
| Add an account from a setup link | `provision.parse {link}`, then `account.test` with its `draft` plus a `password` in `draft.fields`, then `config.edit add_account` with the same draft (a link never carries a password; ask the user for it) |

## Rules

- Calls reach real people and can cost money. Dial only numbers the user gave you or clearly asked
  for, and confirm before calling anyone else, transferring, or hanging up a call the user is on.
- Recording or transcribing a call may require the other party's consent; ask the user first.
- Never print or log account passwords or the control token from `runtime.json`.
- Change settings with `config.edit` or `account.*` methods, not by rewriting `config.toml` by hand.
