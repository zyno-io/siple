# Siple control API

Siple exposes everything the GUI shows, plus every action, over a local control socket. Humans
use it through `siple-cli ctl`; LLM agents use it through the MCP bridge (`siple-cli mcp`),
whose tools are generated from the same method catalog.

> The **Method reference** at the end of this file is generated from the code
> (`siple-cli methods --markdown`). Do not edit it by hand: edit this intro in
> `docs/API-intro.md` and run `scripts/gen-api-docs.sh`.

## Transport and framing

| Platform | Endpoint | Security |
|---|---|---|
| macOS / Linux | Unix domain socket, default `<config dir>/control.sock`; if that path is too long for `sun_path`, falls back to `siple-<hash>.sock` in `$XDG_RUNTIME_DIR` (Linux, when it is a directory you own) or `/tmp/siple-<euid>/` (Linux, mode 0700), or `$TMPDIR` (macOS) | mode 0600 |
| Windows | Named pipe, default `\\.\pipe\siple-<user>-<hash of config dir>` | DACL: current user only, first-instance flag, no remote clients |
| any (optional) | TCP on loopback, `[control].tcp_listen = "127.0.0.1:5079"`, off by default | token **always** required |

The actual endpoints and the per-run token are written to `runtime.json` (mode 0600) in the config
directory; `siple-cli ctl|events|mcp` discover them from there (override with `--socket`,
`--pipe`, `--tcp` + `--token`, or `--config` to point at another instance's config directory).

Framing is **newline-delimited JSON-RPC 2.0**: one JSON object per line, UTF-8, at most 1 MiB per
line before `hello` and 16 MiB after it (large inline audio; see *Audio for agents*). No batches. A line that is not JSON closes the connection (this defeats cross-protocol
abuse, e.g. a browser POSTing HTTP at the port). Requests are served concurrently (up to 32 in
flight per connection); responses carry the request `id` and may arrive out of order.

### hello and the token

The first message on every connection must be `hello`; anything else gets `unauthorized` and the
connection is closed. A connection that does not complete `hello` within 10 s (3 s on TCP) is dropped.

```json
{"jsonrpc":"2.0","id":1,"method":"hello","params":{"token":"<from runtime.json>","client":"my-script"}}
{"jsonrpc":"2.0","id":1,"result":{"server":"siple","version":"0.1.0","protocol":1,
  "current_seq":42,"instance_id":"9f3c1a07d2b4e611","started_at":"2026-10-08T12:00:00.000Z",
  "build":{"channel":"direct","sandboxed":false,"store":false,"version":"0.1.0"}}}
```

- `token` is optional on the Unix socket / named pipe (the OS already restricts access) but
  mandatory on TCP. If sent it must be correct.
- `build` says which edition answers: `channel` is `direct`, `appimage` (Linux AppImage), `mas` (Mac App Store, sandboxed),
  `msix` (Microsoft Store package) or `flatpak`; `store` is true for store editions, which are compiled without the GitHub update source (they
  check their own store instead: see `app.update_status` `source`).
  `status` carries the same object. Some features are unavailable in some editions (see `unsupported`).
- `instance_id` is random per process run. **If it differs from the one you saw before, the
  application restarted**: event sequence numbers and call ids (`c1`, `c2`, ...) start over, so
  discard remembered `seq` values and call ids.

### Errors

Standard JSON-RPC error objects; the machine-readable kind is `error.data.kind`:

```json
{"jsonrpc":"2.0","id":7,"error":{"code":-32004,"message":"486 Busy Here",
  "data":{"kind":"sip_error","sip_code":486}}}
```

| `data.kind` | code | Meaning |
|---|---|---|
| `not_found` | -32001 | Unknown account, call, subscription |
| `invalid_state` | -32002 | Valid request, wrong state (answering a call that is not ringing, a wait whose call already ended, invalid config on `config.reload`) |
| `unauthorized` | -32003 | No/incorrect token, or `hello` not done |
| `sip_error` | -32004 | SIP-level failure; `data.sip_code` is the response code (0 = local failure) |
| `timeout` | -32005 | A wait expired (`call.wait_for`, `call.expect_dtmf`, ...) |
| `unsupported` | -32006 | Not available in this edition (`[speech]` hooks in the Mac App Store build: `call.say`, `call.listen`/`call.rx_audio` with `transcribe`, `speech.*`). Use `call.play`, base64 audio or `siple-cli stream` with your own TTS/STT |
| `invalid_params` | -32602 | Missing or malformed parameters (unknown fields are rejected), or a `since` ahead of the log |
| `internal` | -32603 | Bug or unexpected failure |
| `invalid_request` | -32600 | JSON, but not a JSON-RPC request |
| `method_not_found` | -32601 | Unknown method |
| `parse_error` | -32700 | Unparseable JSON (the server closes the connection instead of replying) |

Some errors carry extra fields in `data` (for example the end code and reason when a wait fails
because the call ended, or the digits received so far when `call.expect_dtmf` times out).

### Problems: plain-language errors

Failures that a person can act on also carry a `problem` object: a stable kind plus an English title and a
one-sentence hint.

```json
{"kind":"busy","title":"Busy","hint":"The other side is on another call; try again later."}
```

```json
{"code":-32004,"message":"486 Busy Here","data":{"kind":"sip_error","sip_code":486,
  "problem":{"kind":"busy","title":"Busy","hint":"The other side is on another call; try again later."}}}
```

`data.kind` is the API error class; `data.problem.kind` is the plain-language category. Match on `kind`, never on the
`title` or `hint` text, which may be reworded and will be translated. Kinds are append-only: they are never renamed or
removed, a client that meets an unknown kind should treat it as `other`, and a SIP code may later move to a more
specific kind. The same code can mean different things by request: a 403 on REGISTER is a wrong password
(`auth_failed`), a 403 on an INVITE is `forbidden`. When the kind is `other`, show the raw `reason`. Titles and hints take
no parameters: the host, account, code and fingerprint are in sibling fields. The existing fields (`code`, `reason`,
`message`, `data.kind`, `sip_code`) keep their names and values.

Where `problem` appears:

| Object | Field |
|---|---|
| `registration` in state `failed` (`status`, `account.list`, `account.get`, `account.registration` events) | `problem` (always) |
| `end` of an ended call (`call.state`, `call.get`, `call.list`, `data.call` of a failed `call.wait_for`) | `end.problem` (absent for a normal end) |
| `blf.list` entries, `status` BLF | `subscription.last_problem` (absent while healthy) |
| `transfer.session`, `transfer.get`, `transfer.list`, `status.transfers` | `last_error.problem` |
| `account.test` | `problem` (absent when `ok`) |
| JSON-RPC errors from a SIP-level failure | `error.data.problem` |

Every kind, its message and the SIP answers behind it:

<!-- problem-kinds -->

## Events

Every state change is appended to a global event log: a ring of the most recent **10 000** events,
each with a strictly increasing `seq` (starting at 1 per process run).

```json
{"seq":57,"ts":"2026-10-08T12:00:03.214Z","kind":"call.state","account":"fs-1001","call_id":"c1","data":{...}}
```

Two ways to consume them:

- `events.poll {since, timeout_ms, kinds?, account?, call_id?, limit?}` long-poll. Returns
  `{events, next_seq, truncated}`. **Primary interface for scripts and LLMs.**
  `truncated: true` means events between `since` and the oldest retained one fell out of the ring.
- `events.subscribe {kinds?, account?, call_id?}` push: matching events then arrive as
  notifications `{"jsonrpc":"2.0","method":"event","params":{<event>}}` on that connection
  (`siple-cli events` prints them one per line). A slow consumer receives a synthetic
  `events.lagged` marker (seq 0).

`kinds` entries are exact (`call.state`) or end in `*` (`call.*`, or `*` for everything).

### Event kinds

| Kind | `account` / `call_id` | `data` |
|---|---|---|
| `account.registration` | account | `{state: disabled\|unregistered\|registering\|waiting_for_network\|registered\|failed, expires_at?, granted_secs?, code?, reason?, retry_at?, problem? (failed only)}` |
| `account.mwi` | account | `{waiting, account_uri, voice:{new,old,urgent_new,urgent_old}, other:{class:counts}, updated_at}` |
| `account.dnd` | account | `{enabled}` |
| `account.reconnecting` | account | `{reason}`: the account is re-probing and re-registering (`network_changed`/`os:*`/`wake`/`route_changed`/`manual`, `send_error`, `keepalive_timeout`, `transport_dead`: the SIP socket stopped working; it is reopened on the same address, or, when that fails or it was a TCP/TLS listener, the account starts over and its calls end) |
| `app.update_available` | - | `{version, url, action}`: the update check found a release newer than the running one (once per version, remembered across restarts; `version` is e.g. `0.2.0` or `0.2.0-rc.1`, `null` for the Microsoft Store, which does not name one, then once per process; `url` the release page, or the store page; `action` is `install`, `open_release_page` or `open_store`: what the GUI offers). Not sent while `[updates] check = false` or when the installation has no update source |
| `app.update_progress` | - | `{version, state, bytes?, total?, error?, reason?}`: an in-place install (`app.install_update`) changed step (`downloading`, `verifying`, `ready`, `waiting`, `committing`, `installed`, `restarting`, `failed`, or `idle` after a cancel); `bytes` / `total` while downloading, at most twice a second; `error` and `reason` (`cancelled`, `network`, `untrusted`, `io`, `unsupported`) when it failed. Direct builds only |
| `app.updated` | - | `{from, to}`: the first start after an in-place update; emitted once by the new version. Direct builds only |
| `network.changed` | - | `{generation, reasons:[...], old_local_ip, new_local_ip}`: debounced network change (OS event, wake from sleep, different route, `network.refresh`); `old_local_ip`/`new_local_ip` are the default-route IPv4 source addresses (`null` = no route) |
| `blf.state` | account | `{uri, label, state: idle\|ringing\|dialing\|busy\|held\|unknown\|failed, dialogs:[...]}` |
| `subscription.state` | account | `{sub_id, uri, event, kind: blf\|mwi\|generic, state: pending\|active\|terminated\|failed, code?, reason?}` |
| `subscription.notify` | account | `{sub_id?, event, content_type?, body, subscription_state?, matched}` (`matched:false` = unsolicited NOTIFY) |
| `message.received` | account | `{from, content_type, body}` (SIP MESSAGE) |
| `call.incoming` | account, call | full call snapshot (below) |
| `call.state` | account, call | full call snapshot; `state` is `dialing\|ringing\|early\|active\|held\|remote_held\|ended`; when ended, `end = {code, reason, by: local\|remote, missed, q850_cause, cause}` (`cause`: `hangup\|remote_hangup\|transferred\|replaced\|rejected\|cancelled\|missed\|failed\|session_expired\|network_lost`; `problem?` when the call did not end normally) |
| `call.updated` | account, call | call snapshot after a change of call details without a state transition |
| `call.reconnecting` | account, call | `{call_id, account}`: a network change moved the account's local address and this established call is being moved onto the new path (media rebound, re-INVITE with the new Contact and `c=` outstanding, 10 s per attempt). Snapshot `reconnecting` is `true` meanwhile. Followed by `call.reconnected`, or by `call.state` ended with `end.cause = network_lost` |
| `call.reconnected` | account, call | `{call_id}`: the peer accepted the re-INVITE; the call runs on the new path (hold direction and SRTP keys kept) |
| `call.media` | account, call | negotiated media: `{codec, pt, clock_rate, ptime_ms, srtp_suite, remote_rtp, rfc4733, rtcp_mux}` |
| `call.dtmf` | account, call | `{digit, duration_ms, via: rfc4733\|info\|inband}` (digits we **received**) |
| `call.rejected` | account, call? | `{from, code, reason}`: an inbound call we rejected (`reason:"dnd"`, busy, ...) |
| `call.refer_received` | account, call | `{refer_to, referred_by, replaces}`: remote asked us to transfer |
| `call.transfer_progress` | account, call | `{code, reason, final}`: NOTIFY sipfrag progress of a transfer we started (100, 180, ..., final 2xx or failure) |
| `transfer.session` | account, call (the original) | `{session}`: a transfer session changed phase or any other field (see *Transfer sessions*) |
| `call.media_timeout` | account, call | `{}`: no RTP received for too long |
| `call.media_error` | account, call | `{message}` |
| `call.record_finished` | account, call | `{path}` (WAV closed) |
| `call.play_finished` | account, call | `{play_id, reason, played_ms, rx_ms, barge_in_rx_ms?}` (`barge_in_rx_ms` only for `barge_in`: where the interruption began): one per `call.play`/`call.say` (see *Audio for agents*) |
| `call.stream` | account, call | `{stream_id, state: open\|closed, rx, tx, sample_rate, encoding, reason? (closed only)}`: a `call.stream` attached or ended |
| `config.applied` | | `{summary:[...], source}`: a config change was applied |
| `config.error` | | `{path, stage: load\|apply, errors:[...]}`: rejected / partly failed; the last good config stays |
| `audio.devices_changed` | | the new audio device state (same shape as `audio.devices`) |
| `app.stopping` | | emitted on shutdown |

The **call snapshot** (in `call.incoming|state|updated`, and `call.get` / `call.list`):
`id, account, direction (inbound|outbound), state, remote_uri, remote_display, local_uri,
sip_call_id, created_at, answered_at, ended_at, end, hold {local, remote}, hold_changed_at, pending,
muted, negotiated, early_media, alert_info, pai, rpid, srtp_fallback, transferred_from, replaces,
transfer, reconnecting`.
`hold_changed_at` is when either hold flag last changed (null if never); `pending` is the operation in
flight on the call (`hold|resume|refer|activate`, else null).
`end.cause` is the stable category of the end (`transferred` for a successful transfer whichever side sent the BYE,
`missed` for an incoming call the caller abandoned while it rang); `end.code` / `end.reason` keep the SIP detail.
`reconnecting: true` means a network change is being followed by a re-INVITE that moves the call onto the
new network path (`call.reconnecting` .. `call.reconnected`); with `[network] move_calls = false`, or when the
peer does not follow, the call ends with `end.cause = network_lost`.
`srtp_fallback: true` means the PBX refused our SRTP (`srtp = "optional"`) offer with 488/415/606 and
the call was retried once with plain RTP.
`call.get` / `call.list` add `agent_capture`: what captures the call's audio right now, a list of `listen`
(`call.listen`, `call.rx_audio`), `transcribe` (with `transcribe:true`), `stream` (`call.stream` with RX) and `record`
(any recording of the call: `call.record_start`, or a `record:` sink on `call.dial|answer|consult_start`, `blf.dial`,
`call.pickup` or the account's `audio_sink`); empty otherwise. While it is not empty, and for two seconds after an
agent request, the app shows a badge on the call (and on its line in the rail), a banner under the header on every
screen, and a line in the tray menu and tooltip: "Agent listening", "Agent transcribing" or, for a recording, "Recording"
(the recording may come from the user's own config, so the label does not say "Agent").

## Testing an account and keyring passwords

`account.test {account | draft, timeout_secs?}` runs a one-off REGISTER (then unREGISTER) on a private socket, without disturbing any
running account, and returns a structured diagnosis; it is what the first-run wizard's "Test & continue" calls. `account` tests a
configured account (its stored settings and password); `draft` tests settings that are not saved yet (`{id?, fields:{...}}`, the fields
of `config.edit` `add_account`). The result always has `ok`, `message` (one line for a user) and `diagnosis`, and, unless `ok`, `problem`:

| `diagnosis` | Meaning | Extra fields |
|---|---|---|
| `ok` | Registered | `transport` (what `auto` picked), `auto`, `server`, `granted_secs`, `elapsed_ms` |
| `auth_failed` | 401 / 403 / 407 after the digest answer: wrong username or password | `code` |
| `not_found` | 404: the user does not exist on the server | |
| `timeout` | No answer within `timeout_secs` (default 15, max 25) | `transport`, `hint` (with UDP: try TCP or TLS; predates `problem.hint`, which says the same in stable words) |
| `tls_untrusted` | The TLS handshake was refused: certificate not trusted | `fingerprint` (SHA-256 of the server certificate, lowercase hex), `reason` |
| `dns_failed` | The server name did not resolve | (the resolver text is part of `message`) |
| `network_unreachable` | No route to the server | (the OS text is part of `message`) |
| `other` | Another SIP status or a local failure | `code` (0 = local), `reason` |

After `tls_untrusted` a UI can offer "Trust this server": `config.edit` `set_account tls_fingerprint <fingerprint>`, then test again.
The password is never echoed. A registrar that allows only one binding per address of record may briefly drop the running account's
binding while the test is registered; the account's next refresh restores it.

`account.set_password {account, password}` sets (or, with `""`, clears) an account's password without echoing it: into the system keyring
when the account has `password_store = "keyring"` (idempotent), else into the `password` key of `config.toml`; then the config is reloaded
so the account re-registers. `config.edit` `set_account password_store` moves an existing password between the two stores. See
[CONFIG.md](CONFIG.md#passwords-in-the-system-keyring).

## Race-free waiting

Every state-changing result carries `event_seq`: the event-log position **before** the action, so
every consequence of the action has a larger `seq`. The pattern is:

1. Do the action, read `event_seq` from the result.
2. Wait with `since = event_seq`. Events logged between the action and your wait are not missed,
   however fast they happened.
3. When waiting for several things in a row, chain the `next_seq` returned by each `events.poll`
   into the next one.

```text
call.dial {account:"fs-1001", target:"9999"}        -> {call_id:"c1", event_seq:120}
call.wait_for {call_id:"c1", state:"active", since:120, timeout_ms:15000}
                                                    -> {call:{...}, event_seq:..., matched_seq:124}
```

`call.dial` also returns a full `call` snapshot. Size limits: `target` (and `call.transfer_blind` target, `subscribe` uri) <= 1024 bytes, custom `headers` <= 8 KiB in total, else `invalid_params`. An INVITE too big for one UDP datagram (65507 bytes, e.g. huge account `extra_headers`) fails the dial at once with `sip_error` (sip_code 0, "transport: INVITE is ... too large for one UDP datagram"); it is not switched to TCP. Between the MTU and 64 KB, IP fragmentation carries it.

`call.wait_for` is the convenient form for call states: with `since` omitted it returns at once if
the call is already in that state; if the call *ended* before reaching the state, it fails
immediately with `invalid_state` and the end code/reason instead of waiting for the timeout.
For anything else use `events.poll {since, kinds:[...], call_id, timeout_ms}`.

`call.wait_for {account, state}` (no `call_id`) matches the first call of that account reaching the state, which
can be a leg that dies right after (for example a PBX that builds a callee leg, then rejects the offer and
tears it down). Prefer waiting on a specific `call_id`, or check the call's `state` once the wait returns.

A `since` larger than the current log position means the instance restarted; the call fails with
`invalid_params` (use `events.current_seq` for a fresh baseline and re-check `status`).

## LLM recipes

Tool names in MCP use underscores (`call.dial` -> `call_dial`); MCP exposes both `events_poll`
(the plain method) and `wait_for_event`, a convenience long-poll wrapper over `events.poll` (default
timeout 10 s, max 60 s, returns `next_seq` to chain). Examples below use method names.

**Place a call and verify audio.** The PBX test extension 9999 plays a continuous 1000 Hz tone.

```text
status                                                   # accounts registered? names/ids
call.dial {account:"fs-1001", target:"9999", source:"silence", sink:"null"}
   -> {call_id:"c1", event_seq:N}
call.wait_for {call_id:"c1", state:"active", since:N, timeout_ms:15000}
call.detect_tone {call_id:"c1", freq_hz:1000, timeout_ms:5000}
   -> {detected:true, level_dbfs:-12.4, power_ratio:0.99}   # real decoded RTP, end to end
call.get {call_id:"c1"}                                  # negotiated codec, SRTP suite, RTP stats
call.hangup {call_id:"c1"}
```

Use `source:"tone:<hz>"` or `"file:<path.wav>"` to transmit something, and `sink:"record:<path.wav>"`
or `call.record_start` to keep what was received. Without microphone access (CI, headless)
always pass `source`/`sink` explicitly.

**Answer an incoming call.**

```text
call.wait_for {account:"fs-1001", state:"ringing", timeout_ms:60000}   # or events.poll kinds:["call.incoming"]
   -> {call:{id:"c3", remote_uri:"sip:1002@...", ...}}
call.answer {call_id:"c3", source:"tone:440", sink:"null"}
call.wait_for {call_id:"c3", state:"active", since:<event_seq>}
...
call.hangup {call_id:"c3"}                # or call.reject {call_id, code:603}
```

Set `auto_answer = true` (hot-reloadable) to skip the first two steps, or `dnd` to reject.

**Leave and read voicemail via DTMF** (FreeSWITCH: `*97` own mailbox; `4000` / `*98` voicemail menu;
password defaults to the extension number).

```text
# leave a message: call an extension nobody answers; the PBX sends us to its voicemail
call.dial {account:"a", target:"1002", source:"tone:440", sink:"null"}   -> c1
call.wait_for {call_id:"c1", state:"active", since:N, timeout_ms:60000}  # voicemail greeting answers
call.dtmf {call_id:"c1", digits:"#"}               # skip the greeting; speak (tone) a few seconds
call.hangup {call_id:"c1"}

# read it back from the mailbox owner's account
call.dial {account:"b", target:"*97", source:"silence", sink:"null"}     -> c2
call.wait_for {call_id:"c2", state:"active", since:M}
call.dtmf {call_id:"c2", digits:"1002#"}           # mailbox password (default: the extension) + #
mwi.get {account:"b"}                              # {waiting, voice:{new,old,...}}; also account.mwi events
```

Pause between menu steps by waiting (`events.poll` with a short `timeout_ms`) rather than guessing at
timing; IVRs need the prompt to finish before the next digit.

**Check BLF.**

```text
blf.list {account:"fs-1001"}                       # {state: idle|ringing|busy|...} per monitored extension
# then provoke a change from another account and wait for it:
events.poll {since:N, kinds:["blf.state"], account:"fs-1001", timeout_ms:10000}
```

BLF entries come from `[[account.blf]]` in the config; adding/removing them is hot-reloadable. Each entry also reports
`subscription {phase, last_code, last_reason, last_problem, ...}` and `stale` (the state may be out of date: check these before trusting `idle`).
`blf.dial {account, uri}` calls an entry; `call.pickup {account, uri}` picks up a `ringing` one (needs `pickup` in the
config; `invalid_state` otherwise). A call is device-bound if either its source or its sink is the device: pass both a non-device `source` (e.g. `"silence"`) and `sink` (e.g. `"null"`) to keep a pickup from holding device calls.
Use `subscribe {account, uri, event:"presence"}` for any other event package.

**Several lines.** `status.active_line` is the un-held device-bound call that owns the microphone and speaker
(null when there is none). Auto-hold (answering, dialing) puts the other device-bound calls on hold: all accounts'
calls with the default `[calls] hold_scope = "all"`, only the answering account's with `"account"` (use that for
automation). Calls without an audio device are never auto-held, and neither is the other leg of a call between two
accounts of the same process. `call.activate {call_id}` switches lines: it holds the others, resumes the call and
moves the audio focus, aborting with `sip_error` (nothing resumed) if a hold fails. `call.answer` takes
`mode: "hold_others"` or `mode: "end_others", end_call_id` (answer first; the call returns `end_others: "pending_ack"` as soon as the 200 is sent, and the other call is
hung up once the caller's ACK arrives; without an ACK, about 32 s, it is left alone and is the active line again). Operations that hold several lines (activate, answer with holds, a dial's auto-hold) are queued in order and each hold is bounded to 8 s; plain `call.hold` / `call.unhold` are not queued. Calls without an audio device neither hold nor are held.

**Blind transfer** (A calls B, B transfers A to C). Drive B's side:

```text
# account "b" has an established call c5 with A
call.transfer_blind {call_id:"c5", target:"1003"}      -> {event_seq:N}  # call is held, REFER sent
events.poll {since:N, call_id:"c5", kinds:["call.transfer_progress","call.state"], timeout_ms:15000}
#   progress {code:100} ... {code:200, final:true}; call c5 then ends with end.reason and end.cause "transferred"
```

**Attended transfer** (B holds A, consults C, then connects A to C):

```text
call.dial {account:"b", target:"1003"}                 -> c6   (call c5 with A is auto-held: auto_hold)
call.wait_for {call_id:"c6", state:"active", since:N}  # talk to C
call.transfer_attended {call_id:"c5", consult_call_id:"c6"}
events.poll {since:M, kinds:["call.transfer_progress"], timeout_ms:15000}
```

**Transfer sessions.** The engine owns the state of a transfer, so every client sees the same thing. A session pairs
the original call A with its consult call B (blind transfers have no B). `call.transfer_blind` and
`call.transfer_attended` keep working and now also return `session_id` / `session`; new fields are only additive.
`call.consult_start {call_id, target, source?, sink?}` holds A (as a phone does, **also** for calls without an audio
device, which never cause holds of other calls), dials `target` on A's account (source and sink default to A's) and
returns `{session_id, consult_call_id}`; `call.consult_link {call_id, consult_call_id}` pairs two existing calls of one
account. Then `transfer.complete {session_id, mode}` (`attended`: REFER with Replaces, needs an answered consult;
`blind_now`: end the consult, refer A to its dialled URI), `transfer.swap` (hold the leg you talk to, resume the other,
move the audio focus) or `transfer.cancel` (hang up B, resume A). `transfer.get` / `transfer.list` read sessions;
`status.transfers` and `call.transfer {session_id, role}` carry them too. `last_error` is `{stage, code, reason, problem}`.

```text
call.consult_start {call_id:"c5", target:"1003"}   -> {session_id:"t1", consult_call_id:"c6"}
events.poll {kinds:["transfer.session"]}           #  consult_dialing -> consult_ringing -> consult_connected
transfer.complete {session_id:"t1", mode:"attended"}   # referring -> transferring -> transferred
```

Phases: `consult_dialing`, `consult_ringing`, `consult_connected`, `consult_failed` (B ended; A stays held;
`transfer.cancel` resumes it), `referring`, `transferring`, `unconfirmed` (no final NOTIFY after 32 s: the transfer may
have happened, nothing is done automatically, a late final NOTIFY moves it on), and the terminal `transferred`,
`failed`, `cancelled`, `abandoned` (A ended before the outcome). The last 16 terminal sessions stay readable. A failed
transfer takes A off hold again when the session held it (`[calls] resume_after_failed_transfer`, default true). While
a session is `referring` / `transferring`, `call.hold` / `call.unhold` of either call fail with `invalid_state`, and
`transfer.cancel` fails too (an accepted REFER cannot be retracted). `transfer.complete` on a session whose REFER was
already sent returns the session and sends no second REFER. One live session per call: a second start fails with
`invalid_state` naming the session (the legacy `call.transfer_*` calls replace a session that is still only
consulting).

Receiving side (we are the transferee or target): `accept_refer = true` makes us follow a REFER
automatically; watch `call.refer_received` and the new call's `transferred_from` field.

## Audio for agents

An agent can put audio into a call and take audio out of it, without a sound card. Three layers,
use the simplest that works:

| Layer | Methods | Needs |
|---|---|---|
| Clips and listening | `call.play`, `call.stop_play`, `call.listen`, `call.rx_audio` | nothing external |
| Speech | `call.say`, `call.listen{transcribe:true}`, `speech.synthesize`, `speech.transcribe` | `[speech]` hooks (below) |
| Raw duplex stream | `call.stream` | a bot that speaks PCM (not in MCP) |

- `call.play` takes exactly one of `source` (`tone:<hz>`, `file:<path>`, `silence`, as for `call.dial`),
  `wav_base64`, or `pcm_base64` + `pcm_format`. Inline clips play once; `queue:true` appends instead of replacing;
  `wait:true` returns when the play finished. `call.stop_play` stops one or all.
- `call.listen` records what the far end says and ends on silence (a small voice-activity detector;
  continuous tones count as speech). It returns the audio (`audio:"base64"`, or `"file"` with a `path`, or `"none"`)
  and, with `transcribe:true`, the transcript.
- `call.rx_audio` fetches received audio by time range without waiting for speech.

### The rx_ms timeline

Every call keeps a history of received audio (the last `rx_history_secs` seconds, default 60) on a clock called `rx_ms`: milliseconds of
audio received on this call since it started, **in audio time, not wall time**. Play and listen results
carry `rx_ms` values, and `call.listen{since_rx_ms}` starts at a point on that clock, even in the past. This makes
turn-taking race-free in the same way `event_seq` does for events: nothing the far end says while you are busy
(thinking, or making an LLM round trip) is lost, because the history covers the gap. A `since_rx_ms` older than the history is clamped
(`truncated:true`); one more than 10 s in the future is `invalid_params`.

**The timeline restarts when the media is recreated.** `call.answer` with a `source` or `sink` different from the
call's current one (for an inbound call: from what the account config gave it) creates new media at the answer, to
move the RTP port into the SDP answer. `rx_ms` then starts again at 0 for that call, and everything pending on the old
media ends: a `call.play` issued before the answer finishes with `reason:"call_ended"` (`call.play_finished`, or
`finished` of a `wait:true` play) and a `call.listen` issued before the answer returns `reason:"call_ended"`. Issue
plays and listens **after** the answer (`call.wait_for state:active`), or answer without `source`/`sink`; then the
media is never recreated and `rx_ms` keeps running. Positions (`since_rx_ms`, `finished.rx_ms`) taken before the answer are
not valid afterwards.

### Turn-taking recipe

```text
call.say    {call_id:"c1", text:"How can I help?", wait:true}
              -> {finished:{reason:"completed", rx_ms:48200}, tts_ms:310, ...}
call.listen {call_id:"c1", since_rx_ms:48200, transcribe:true}
              -> {speech:true, reason:"end_of_speech", transcript:"I need to ...", end_rx_ms:53900, ...}
call.say    {call_id:"c1", text:"Sure, ...", wait:true}                  # reply
              -> {finished:{reason:"completed", rx_ms:61400}, ...}
call.listen {call_id:"c1", since_rx_ms:61400, transcribe:true}   # finished.barge_in_rx_ms (if present) else finished.rx_ms of the say you just made
```

1. `call.say{wait:true}` (or `call.play{wait:true}`) returns `finished.rx_ms`: where the far end's audio stood
   when your audio ended.
2. `call.listen{since_rx_ms: that}` returns the next utterance and `end_rx_ms`.
3. After you speak again, the next listen uses that play/say's `finished.barge_in_rx_ms` if present (reason
   `barge_in`: where the caller's interruption began, a few frames before the watcher fired, so the start of
   their words is not lost), otherwise `finished.rx_ms` (the end of your own audio), not the previous
   `end_rx_ms`: the caller's reply starts after your audio, and `end_rx_ms` predates it. Use the previous
   `end_rx_ms` only when listening again without speaking in between.
4. Thinking and TTS time is safe: history after `since_rx_ms` is replayed to the detector, and `start_timeout_ms`
   counts only from the moment you issue the listen (or from `since_rx_ms` if that lies in the future), never from
   replayed history. Keep `since_rx_ms` within `rx_history_secs` (at least 5 s) or the window is `truncated`.

Without speech hooks the same loop works with clips: `call.play{wav_base64}` (or `source:"tone:440"`) then
`call.listen{audio:"file", path:"/tmp/in.wav"}` and process the WAV yourself. `reason` tells why a listen ended:
`end_of_speech`, `max_duration`, `no_speech` (nothing within `start_timeout_ms`), `call_ended`, `timeout`. DTMF digits seen
in the window come back in `dtmf`.

**Barge-in caveat.** `barge_in:true` stops your audio when the far end starts speaking. On an echo path
(FreeSWITCH 9196, a speakerphone, a PBX that reflects media) the far end hears itself, so barge-in triggers on your own
audio; it is therefore off by default. Use it only when the far end is a real, separate talker.

### Limits

- Inline audio at most 10 MiB decoded (the base64 length is checked first); `file:` clips at most 600 s (inline audio 120 s); at most 8 queued
  plays and 300 s queued per call; at most 4 concurrent listens per call.
- `wait:true` on `call.play` is capped at 120 s (default timeout `min(total + 30 s, 120 s)`), so it always times
  out for a clip longer than about 90 s. For long clips use `wait:false` and the `call.play_finished` event
  (or `events.poll`).
- Control line limit: 1 MiB before `hello`, **16 MiB after** an authenticated `hello`. Lines over 1 MiB must
  acquire one of 4 global "big request" permits before they are buffered, so a few large inline clips are fine but
  they are served a few at a time. For anything bigger than a few seconds use `source:"file:<path>"`
  instead of inline base64.
- `call.listen` takes at most 120 s of audio (`max_ms`) and, by default, `start_timeout_ms + max_ms + 5 s` of wall time (at most 305 s with the maximum start timeout and `max_ms`; `timeout_ms` overrides it).
- Audio in MCP results: base64 never enters the model's text context. An MCP result with `audio.base64` is returned with
  `base64_omitted:true` plus an `audio` content block when the client negotiated protocol 2025-03-26 or newer;
  otherwise use `audio:"file"` or `transcribe:true`.

### Streaming: `call.stream`

For bots that want a continuous duplex stream (voice-bot frameworks, realtime models). Not available through MCP
(use `siple-cli stream <call_id>`, which pipes raw PCM on stdin/stdout, or a client of your own).

**Upgrade.** Send `call.stream` as a normal request on an authenticated control connection (unix socket, named
pipe, or TCP with token). The reply is a JSON line, and the connection then switches to binary framing. Preconditions
(else `invalid_state` and the connection stays in JSON mode): `hello` done; no other requests in flight and no
`events.subscribe` on this connection (use a dedicated connection); the call has media and is not ringing; the call has no
stream yet (one per call).

**Params.** `call_id`; `rx` (true) and `tx` (true); `sample_rate` 16000 (8000, 16000, 24000, 48000); `encoding`
`s16le` (`f32le`, `mulaw`, `alaw`); `frame_ms` 20 (10, 20, 40, 60); `tx_buffer_ms` 2000 (accepted 40..10000, but the effective maximum is 2000: the TX ring holds 2 s, larger values are capped and the result line reports the value in effect);
`hangup_on_close` false.

**Result line.** `{stream_id, sample_rate, encoding, channels:1, frame_ms, frame_bytes, tx_buffer_ms (effective), rx_ms, event_seq}`:
the last JSON line; everything after it is binary.

**Framing**, both directions: a 4-byte big-endian header `type << 24 | len` (len at most 1 MiB), then `len` payload bytes.

- `0x01` AUDIO. Server to client frames are exactly `frame_ms` long. Client to server frames may be any whole
  number of samples up to 1 s.
- `0x02` JSON. Client commands: `{"cmd":"mark","name":"m1"}`, `{"cmd":"clear"}`, `{"cmd":"stats"}`, `{"cmd":"close"}`.
  Server events: `mark {name, rx_ms}` (playout reached the mark), `cleared {dropped_ms}`, `rx_gap {dropped_ms}`, `rx_unavailable {error}`,
  `state {state}`, `dtmf {digit, via}`, `stats {tx_buffer_ms, tx_underrun_ms, rx_dropped_ms, rx_ms}`,
  `ended {reason, end?}` (then the server closes), `error {message}`.
- `reason` of the `call.stream` closed event: `client_close`, `client_eof` (input ended, also inside a frame),
  `connection_lost` (reset / broken pipe, e.g. the client closed a socket that still held unread RX data; no
  `ended` event is sent), `protocol_error`, `call_ended`, `server_shutdown`.
- `siple-cli stream` sends `{"cmd":"mark","name":"__tx_done"}` after the last TX audio; with `--no-rx` the
  returned `mark` event ends the session. A `--tx` file with a RIFF/WAVE header is decoded and converted to
  `--rate` / `--encoding` (mono) instead of being sent as raw PCM.
- Unknown types `>= 0x80` are ignored; other unknown types are a protocol error and end the stream.

**Flow control.** TX: when the buffer is full the server stops reading, so TCP/socket backpressure reaches your
sender: you may write faster than real time and block, but pace your sends anyway for live audio. RX: a slow
reader gets `rx_gap` notices (dropped audio) and never stalls the call; a write that blocks for 10 s ends the stream.

**Mic and speaker.** A TX stream replaces the call's base audio source (the microphone is suspended for that call);
a `call.play` clip overrides the stream while it plays; when the stream detaches the base source is restored.
`mark` and `clear` mirror Twilio media streams: send audio, then a mark to learn when it has really played out;
`clear` flushes audio you queued (barge-in on your side).

Minimal Python client (standard library only): connect, hello, upgrade, then read frames and echo the audio back.

```python
import json, socket, struct, sys

rt = json.load(open(sys.argv[1]))                  # <config dir>/runtime.json
s = socket.socket(socket.AF_UNIX); s.connect(rt["control"]["unix_socket"])   # Unix only; see the note below
f = s.makefile("rwb")

def rpc(i, method, params):
    f.write((json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params}) + "\n").encode()); f.flush()
    r = json.loads(f.readline())
    if "error" in r: raise SystemExit(r["error"])
    return r["result"]

rpc(1, "hello", {"token": rt["token"], "client": "py-stream"})
info = rpc(2, "call.stream", {"call_id": sys.argv[2], "sample_rate": 16000})   # last JSON line
print(info, file=sys.stderr)

def send(kind, payload):
    f.write(struct.pack(">I", kind << 24 | len(payload)) + payload); f.flush()

while True:
    hdr = f.read(4)
    if len(hdr) < 4: break
    (word,) = struct.unpack(">I", hdr)
    kind, n = word >> 24, word & 0xFFFFFF
    body = f.read(n)
    if kind == 0x01:
        send(0x01, body)                           # echo the caller back (s16le mono)
    elif kind == 0x02:
        ev = json.loads(body); print(ev, file=sys.stderr)
        if ev["event"] == "ended": break
```

Do not run this against an echo extension (FreeSWITCH 9196, a speakerphone, a reflecting PBX): echoing RX back
to a far end that echoes it again creates a feedback loop.

(Read from `f`, the buffered file, after the upgrade: bytes may already be buffered. Do not mix it with direct `s.recv`.)

**Docker image.** `runtime.json` -> `control.unix_socket` is the path *inside the container*. The socket
(`/data/control.sock`) is mode 0600 owned by uid 10001, so a bind-mounted socket fails with EACCES for ordinary
host users. Either run the client as uid 10001 or root (for example `docker run --user 10001 ...` sharing the
volume), run it inside the container with `docker exec`, or use TCP mode (`[control] tcp_listen`, published port,
token from `runtime.json`).

This client is Unix-socket only. On Windows the control endpoint is a named pipe (`runtime.json` ->
`control.named_pipe`, `\\.\pipe\siple-<user>`) that Python's `socket` cannot open. Use the TCP listener instead, which works
on every platform: set `[control] tcp_listen = "127.0.0.1:5079"`, then connect with
`s = socket.create_connection(("127.0.0.1", 5079))` (or `rt["control"]["tcp"]`); the token from `runtime.json` is
always required on TCP, and the `hello` call in the script sends it.

### Speech hooks (`[speech]`)

`call.say` / `speech.synthesize` (text to speech) and `call.listen{transcribe}` / `speech.transcribe` (speech to
text) call **external programs that you configure**. Nothing is bundled, and both are off until `tts_command` /
`stt_command` are set (`status.speech` shows which are on). See [CONFIG.md](CONFIG.md#speech) for every option.

- argv only, **never a shell**. Placeholders are replaced inside argv elements (`{{` / `}}` are literal braces).
  TTS: `{text_file}`, `{out}`, `{voice}`. STT: `{in}` (16 kHz mono WAV), `{lang}`, `{out_base}`, `{out_txt}`.
  The same values are exported as `ZP_TEXT_FILE ZP_OUT ZP_IN ZP_VOICE ZP_LANG ZP_OUT_TXT` for script-style tools.
- The text to speak goes on **stdin** (UTF-8) and in `{text_file}`, never in argv. Voice/language names must match
  `[A-Za-z0-9][A-Za-z0-9_.+:-]{0,63}`; no API parameter changes the command itself.
- TTS output: the WAV at `{out}` if referenced, else on stdout. STT output: the file at `{out_txt}` if referenced,
  else stdout. Programs run in the config directory with your environment, in their own process group, killed on timeout. On Unix the whole process group is killed; on Windows only the direct child is killed (no Job Object), so do not point `tts_command` / `stt_command` at a wrapper `.cmd` / `.bat` script that spawns further processes: they could outlive the timeout. Run the real executable directly.
- Errors: not configured gives `invalid_state`; a timeout gives `timeout`; a non-zero exit gives `internal` with
  `exit_code` and `stderr_tail`.

```toml
[speech]
# piper (text arrives on stdin, WAV written to {out})
tts_command = ["piper", "--model", "/opt/piper/en_US-lessac-medium.onnx", "--output_file", "{out}"]

# espeak-ng
# tts_command = ["espeak-ng", "-v", "{voice}", "-w", "{out}", "-f", "{text_file}"]
# tts_voice = "en-us"

# macOS say
# tts_command = ["say", "-o", "{out}", "--file-format=WAVE", "--data-format=LEI16@22050", "-f", "{text_file}"]

# Windows SAPI: PowerShell reads the paths from the environment, so nothing is interpolated into a script
# tts_command = ["powershell", "-NoProfile", "-Command",
#   "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; $s.SetOutputToWaveFile($env:ZP_OUT); $s.Speak((Get-Content -Raw -Encoding UTF8 $env:ZP_TEXT_FILE)); $s.Dispose()"]

# whisper.cpp (transcript on stdout)
stt_command = ["whisper-cli", "-m", "/opt/whisper/ggml-base.en.bin", "-f", "{in}", "-l", "{lang}", "-nt", "-np"]
stt_language = "en"
```

espeak-ng and piper are GPL programs run as separate processes; that does not affect Siple's FSL-1.1-ALv2 licence.

### New event kinds

| Kind | `account` / `call_id` | `data` |
|---|---|---|
| `call.play_finished` | account, call | `{play_id, reason: completed\|stopped\|replaced\|barge_in\|call_ended\|error, played_ms, rx_ms, barge_in_rx_ms?}`: exactly one per play |
| `call.stream` | account, call | `{stream_id, state: open\|closed, reason?, rx, tx, sample_rate, encoding}` |

## MCP notes

Register the bridge with Claude Code:

```sh
claude mcp add siple -- /path/to/siple-cli mcp
# macOS app bundle:  "/Applications/Siple.app/Contents/MacOS/siple-cli"
# non-default config dir:  ... siple-cli mcp --config /path/to/config.toml
```

- stdio MCP server (protocol versions 2025-06-18, 2025-03-26, 2024-11-05), newline-delimited JSON-RPC.
  stdout carries only protocol messages; diagnostics go to stderr.
- One tool per control method (`status`, `account_list`, `call_dial`, ...), with the JSON Schema from
  the catalog below as the input schema, plus **`wait_for_event`**, a thin wrapper over `events.poll`
  (default timeout 10 s, max 60 s) that is the primary way to wait.
- Tool results: `content[0].text` holds the JSON result and `structuredContent` the same object;
  failures set `isError: true` with the error kind and message.
- The tool list is served from the built-in catalog even when Siple is not running; calls then fail
  with a clear "Siple is not running" message. Start the app first
  (`siple-cli run --headless` or the GUI). Once connected, `notifications/tools/list_changed`
  is sent if the live list differs.
- `call.stream` is not exposed (binary upgrade); `hello` and the event subscription methods are not either.
- Tools that return audio (`call_listen`, `call_rx_audio`, `speech_synthesize`) never put base64 in the text: it is replaced by
  `base64_omitted:true`, and an MCP `audio` content block is appended for clients that negotiated 2025-03-26 or newer.
- Requests run concurrently, so a long `wait_for_event` never blocks `status`.
- Lost connections are retried automatically **only for idempotent read methods**; a state-changing call
  (`call_dial`, ...) reports the failure instead, because it may have run. Check `status` / `call_list`.
- If Siple restarted between two calls, the next result is flagged `restarted: true`: all remembered
  `event_seq` values and call ids are invalid.

Offline helpers (no instance needed): `siple-cli methods [--markdown|--json]` prints this catalog;
`siple-cli config-example` prints the example config.

---

# Method reference

Generated by `siple-cli methods --markdown` ({N} methods). Params objects reject unknown fields.
Account ids come from the config; call ids look like `c1`, `c2`.

