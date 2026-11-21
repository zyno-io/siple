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
- `build` says which edition answers: `channel` is `direct`, `mas` (Mac App Store, sandboxed) or
  `msix` (Microsoft Store package); `store` is true for store editions, which are compiled without the GitHub update source (they
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

<!-- Generated by scripts/gen-api-docs.sh from the problem catalog (crates/siple-problem); do not edit. -->

| kind | Message | What to do | SIP answers |
|---|---|---|---|
| <a id="problem-auth_failed"></a>`auth_failed` | Wrong username or password | Check the account's username, auth username and password. | REGISTER: 401, 403, 407; INVITE: 401, 407; SUBSCRIBE: 401, 407; in-dialog: 401, 407 |
| <a id="problem-auth_foreign_refused"></a>`auth_foreign_refused` | Password not sent to an unknown server | A server outside your provider asked for your password; if your provider uses it, set auth_foreign = true for this account. | — |
| <a id="problem-user_not_found"></a>`user_not_found` | Account not found on the server | Check the username and the domain. | REGISTER: 404, 410, 484, 604 |
| <a id="problem-account_config"></a>`account_config` | Account settings are incomplete | Check the account's domain, username and transport. | — |
| <a id="problem-port_in_use"></a>`port_in_use` | Local port already in use | Another app is using the account's local port; close it or change local_port for this account. | — |
| <a id="problem-dns_failed"></a>`dns_failed` | Can't find the server | Check the domain or registrar for typos, and your internet connection. | — |
| <a id="problem-network_unreachable"></a>`network_unreachable` | No network connection | Check your Wi-Fi or cable; Siple reconnects when the network is back. | — |
| <a id="problem-connection_failed"></a>`connection_failed` | Can't connect to the server | Check the server address and port, and that the server accepts this transport. | — |
| <a id="problem-no_response"></a>`no_response` | The server did not answer | Check the server address and your firewall; with UDP, try TCP or TLS. | 408 |
| <a id="problem-tls_untrusted"></a>`tls_untrusted` | The server's certificate is not trusted | If it is your server, trust its certificate from the account test; otherwise ask your provider. | — |
| <a id="problem-tls_config"></a>`tls_config` | Secure connection settings are invalid | Check the account's TLS certificate, key, CA file and fingerprint. | — |
| <a id="problem-server_error"></a>`server_error` | The server had a problem | Try again in a moment; if it keeps happening, contact your provider. | 500, 502-599 |
| <a id="problem-busy"></a>`busy` | Busy | The other side is on another call; try again later. | INVITE: 486, 600; in-dialog: 486, 600 |
| <a id="problem-declined"></a>`declined` | Call declined | The other side declined the call; try again later. | INVITE: 603, 607, 608; in-dialog: 603, 607, 608 |
| <a id="problem-number_not_found"></a>`number_not_found` | Number not found | Check the number and try again. | INVITE: 404, 410, 484, 604; in-dialog: 404, 410, 484, 604 |
| <a id="problem-unavailable"></a>`unavailable` | Not available right now | The other side is offline or not taking calls; try again later. | INVITE: 480; in-dialog: 480 |
| <a id="problem-forbidden"></a>`forbidden` | Not allowed | The server does not allow this for your account; ask your provider or PBX administrator. | INVITE: 403; SUBSCRIBE: 403, 603, 607, 608; in-dialog: 403 |
| <a id="problem-media_mismatch"></a>`media_mismatch` | No common audio format | Check the account's codecs and SRTP setting, or ask your provider which ones it supports. | INVITE: 415, 488, 606; in-dialog: 415, 488, 606 |
| <a id="problem-not_supported"></a>`not_supported` | Not supported by the other side | The other phone or the PBX cannot do this; ask your administrator if you need it. | INVITE: 405, 420, 421, 501; in-dialog: 405, 420, 421, 489, 501 |
| <a id="problem-try_again"></a>`try_again` | Busy with another request | Wait a moment and try again. | 491 |
| <a id="problem-call_gone"></a>`call_gone` | The call no longer exists | The other side or the server already ended it; call again if needed. | INVITE: 481; in-dialog: 481 |
| <a id="problem-call_dropped"></a>`call_dropped` | The call dropped | The other side stopped responding; call again. | — |
| <a id="problem-network_lost"></a>`network_lost` | Call lost when the network changed | Call again once the account shows registered. | — |
| <a id="problem-media_failed"></a>`media_failed` | Call audio could not start | Check the selected microphone and speaker, then try again. | — |
| <a id="problem-extension_not_found"></a>`extension_not_found` | Extension not found | Check the extension number in the account's extension list. | SUBSCRIBE: 404, 410, 484, 604 |
| <a id="problem-blf_unsupported"></a>`blf_unsupported` | Line status not available | The PBX does not report the status of this extension; ask your administrator or remove it. | SUBSCRIBE: 405, 415, 489, 501 |
| <a id="problem-refused"></a>`refused` | Refused by the server | The details show the server's answer; your provider can tell you what it means. | any other 3xx-6xx |
| <a id="problem-other"></a>`other` | Something went wrong | The details and the log have more information. | — |

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
| `account.reconnecting` | account | `{reason}`: the account is re-probing and re-registering (`network_changed`/`os:*`/`wake`/`route_changed`/`manual`, `send_error`, `keepalive_timeout`) |
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

Generated by `siple-cli methods --markdown` (82 methods). Params objects reject unknown fields.
Account ids come from the config; call ids look like `c1`, `c2`.

### `hello`

Handshake; must be the first message. Returns server version, current event seq, `instance_id` / `started_at` and `build` ({channel: direct|mas|msix, sandboxed, store, version}: which edition answers) (a different instance_id than before means the application restarted: event seqs and call ids reset).

| Param | Type | Required | Description |
|---|---|---|---|
| `client` | string | no | Client name for logs. |
| `token` | string | no | Per-run token from runtime.json (mandatory on TCP). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "client": {
      "description": "Client name for logs.",
      "type": "string"
    },
    "token": {
      "description": "Per-run token from runtime.json (mandatory on TCP).",
      "type": "string"
    }
  },
  "type": "object"
}
```

</details>

### `ping`

Liveness check; returns {pong:true}.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `methods.list`

Lists all methods with descriptions and JSON Schemas for their params.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `events.poll`

Long-poll the event log. Returns {events, next_seq, truncated}; pass next_seq as `since` on the next call. A `since` ahead of the log (the instance restarted) fails with invalid_params; call events.current_seq.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Only events of this account id. |
| `call_id` | string | no | Only events of this call id. |
| `kinds` | array | no | Event kinds; exact (`call.state`) or prefix with trailing `*` (`call.*`). |
| `limit` | integer | no | Maximum events to return (default 500). |
| `since` | integer | no | Return events with seq greater than this. Default: current seq (only new events). |
| `timeout_ms` | integer | no | Long-poll: wait up to this long for a matching event. Default 0 (no wait). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Only events of this account id.",
      "type": "string"
    },
    "call_id": {
      "description": "Only events of this call id.",
      "type": "string"
    },
    "kinds": {
      "description": "Event kinds; exact (`call.state`) or prefix with trailing `*` (`call.*`).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "limit": {
      "description": "Maximum events to return (default 500).",
      "maximum": 5000,
      "minimum": 1,
      "type": "integer"
    },
    "since": {
      "description": "Return events with seq greater than this. Default: current seq (only new events).",
      "minimum": 0,
      "type": "integer"
    },
    "timeout_ms": {
      "description": "Long-poll: wait up to this long for a matching event. Default 0 (no wait).",
      "maximum": 60000,
      "minimum": 0,
      "type": "integer"
    }
  },
  "type": "object"
}
```

</details>

### `events.subscribe`

Start receiving matching events as `event` notifications on this connection (replaces any previous subscription). Returns the current seq.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Only events of this account id. |
| `call_id` | string | no | Only events of this call id. |
| `kinds` | array | no | Event kinds; exact (`call.state`) or prefix with trailing `*` (`call.*`). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Only events of this account id.",
      "type": "string"
    },
    "call_id": {
      "description": "Only events of this call id.",
      "type": "string"
    },
    "kinds": {
      "description": "Event kinds; exact (`call.state`) or prefix with trailing `*` (`call.*`).",
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "type": "object"
}
```

</details>

### `events.unsubscribe`

Stop the push subscription on this connection.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `events.current_seq`

Seq of the most recent event; use as `since` baseline before acting.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `logs.tail`

Recent application log lines (oldest first).

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Only lines tagged with this account id. |
| `lines` | integer | no | Default 100. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Only lines tagged with this account id.",
      "type": "string"
    },
    "lines": {
      "description": "Default 100.",
      "maximum": 5000,
      "minimum": 1,
      "type": "integer"
    }
  },
  "type": "object"
}
```

</details>

### `status`

Overall state: instance_id and started_at (a changed instance_id means a restart: event seqs and call ids reset), accounts (registration, which carries `problem` {kind, title, hint} when failed; dnd, mwi summary, blf list, pending_rebuild), calls, active_line (id of the un-held device-bound call that owns the microphone and speaker, or null), network {generation: count of network changes seen}, audio (effective devices from [audio] with source config|default, active devices, mic_permission), build {channel: direct|mas|msix, sandboxed, store: a store edition (compiled without the GitHub update source), version}, speech {tts, stt: whether [speech] tts_command / stt_command are set}, config (path, applied_at, last_error), update {enabled, current, available, latest, release_url, source, action, store_url, install_state: the update check's summary, see app.update_status}, event_seq.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `account.list`

Status of all accounts.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `account.get`

Status and redacted config of one account (status includes `srtp_optional_downgraded`: a list of next-hop destinations `host:port[:transport]` that refused an `srtp = "optional"` offer and then answered the plain RTP/AVP retry with a final 2xx; calls to them offer plain RTP/AVP first). Errors: not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id from the config. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id from the config.",
      "type": "string"
    }
  },
  "required": [
    "account"
  ],
  "type": "object"
}
```

</details>

### `account.register`

Register now (also re-enables registration after account.unregister). Returns event_seq; follow with events.poll for `account.registration`.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id from the config. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id from the config.",
      "type": "string"
    }
  },
  "required": [
    "account"
  ],
  "type": "object"
}
```

</details>

### `account.unregister`

Remove our registration binding and stop refreshing until account.register.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id from the config. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id from the config.",
      "type": "string"
    }
  },
  "required": [
    "account"
  ],
  "type": "object"
}
```

</details>

### `account.reregister`

Re-register one account immediately: re-probe the route and next hop, redial a TCP/TLS connection, reset the back-off and send REGISTER now (also re-enables registration after account.unregister; leaves `waiting_for_network` if a route exists). Emits `account.reconnecting`, then `account.registration`. Established calls on a moved local address are re-INVITEd onto the new path (`call.reconnecting` / `call.reconnected`, or `call.state` ended with cause `network_lost`; see [network] move_calls). Returns event_seq.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id from the config. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id from the config.",
      "type": "string"
    }
  },
  "required": [
    "account"
  ],
  "type": "object"
}
```

</details>

### `network.refresh`

Force a network-change signal now (no debounce): every account re-probes its route and re-registers at once, as after a Wi-Fi switch or wake from sleep. Returns {generation, event_seq}; emits `network.changed` and one `account.reconnecting` per registering account. When an account's local address changed, its established calls are moved with a re-INVITE (events `call.reconnecting` then `call.reconnected`; a call the peer does not follow ends with cause `network_lost`, as does every established call when `[network] move_calls = false`).

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `account.set_sip_trace`

Turn the SIP wire trace of an account on/off at runtime (lasts until its `sip_trace` config value changes).

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id from the config. |
| `enabled` | boolean | yes |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id from the config.",
      "type": "string"
    },
    "enabled": {
      "type": "boolean"
    }
  },
  "required": [
    "account",
    "enabled"
  ],
  "type": "object"
}
```

</details>

### `account.set_password`

Set (or, with an empty `password`, clear) the password of an account without echoing it. Where it is written follows the account's `password_store`: `keyring` -&gt; the system credential store (service `io.zyno.siple`, entry `account:<id>`; idempotent; fails with invalid_state `keyring unavailable; set password_store = "file"` when there is none), `file` -&gt; the `password` key of config.toml (same typed edit as `config.edit`, comments preserved). Then the config is reloaded at once so the account re-registers with the new password. Result {ok, account, password_store, applied, report, event_seq}; `report` is the reload report (account errors, if any, are listed in `report.errors`). To move an account between stores, `config.edit` set_account `password_store`: the existing password moves with it.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id from the config. |
| `password` | string | yes | New password; "" clears it. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id from the config.",
      "type": "string"
    },
    "password": {
      "description": "New password; \"\" clears it.",
      "type": "string"
    }
  },
  "required": [
    "account",
    "password"
  ],
  "type": "object"
}
```

</details>

### `account.test`

Try an account's settings with a one-off REGISTER (then unREGISTER) on a private socket, without disturbing any running account, and say what is wrong. Give exactly one of `account` (a configured account id, its stored settings and password) or `draft` ({id?, fields:{field: value}}: the same fields and checks as `config.edit` `add_account`, e.g. username, password, domain, transport, tls_verify; nothing is saved). `timeout_secs` (1-25, default 15) bounds the whole test. Result: `diagnosis` is one of `ok` {transport (what `auto` picked), auto, server, granted_secs, elapsed_ms} | `auth_failed` {code: 401/403/407 after the digest answer} | `not_found` (404) | `timeout` {transport, hint: with UDP, try TCP or TLS} | `tls_untrusted` {fingerprint: SHA-256 of the server certificate, lowercase hex, reason}: to trust that server, write it with `config.edit` set_account `tls_fingerprint` | `dns_failed` {message} | `network_unreachable` {message} | `other` {code (0 = local failure), reason}; plus `ok` (boolean), `message` (one line for a user) and, unless `ok`, `problem` {kind, title, hint} (the plain-language category; `hint` there is the stable one, while `timeout`'s own `hint` field predates it). The password is never echoed. A registrar that allows one binding per address of record may briefly drop the running account's binding; the account's next refresh restores it. A draft whose fields are invalid fails with invalid_params and data.errors [{path, message}].

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Configured account id. |
| `draft` | object | no |  |
| `timeout_secs` | integer | no | Total time limit; default 15. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "oneOf": [
    {
      "required": [
        "account"
      ]
    },
    {
      "required": [
        "draft"
      ]
    }
  ],
  "properties": {
    "account": {
      "description": "Configured account id.",
      "type": "string"
    },
    "draft": {
      "additionalProperties": false,
      "properties": {
        "fields": {
          "additionalProperties": {
            "description": "A boolean, integer or string, as the field requires.",
            "type": [
              "boolean",
              "integer",
              "string"
            ]
          },
          "description": "field name -> value (as in config.edit add_account)",
          "type": "object"
        },
        "id": {
          "type": "string"
        }
      },
      "type": "object"
    },
    "timeout_secs": {
      "description": "Total time limit; default 15.",
      "maximum": 25,
      "minimum": 1,
      "type": "integer"
    }
  },
  "type": "object"
}
```

</details>

### `audio.devices`

Available input/output devices, the effective selection (with source) and the active devices.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `audio.set_devices`

Choose microphone / speaker / ringer (at least one). Written to `[audio] input/output/ringer` of config.toml (the same typed edit as `config.edit`, comments preserved) and applied at once; fails like `config.edit` when the config file is invalid. Returns the effective devices.

| Param | Type | Required | Description |
|---|---|---|---|
| `input` | string | no | Microphone device name; "" = system default. |
| `output` | string | no | Speaker device name; "" = system default. |
| `ringer` | string | no | Ringer device name; "" = system default. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "minProperties": 1,
  "properties": {
    "input": {
      "description": "Microphone device name; \"\" = system default.",
      "type": "string"
    },
    "output": {
      "description": "Speaker device name; \"\" = system default.",
      "type": "string"
    },
    "ringer": {
      "description": "Ringer device name; \"\" = system default.",
      "type": "string"
    }
  },
  "type": "object"
}
```

</details>

### `audio.mic_level`

Live microphone level: RMS of the latest capture block in dBFS (-120 = silence), measured whether or not a call is up. Result {available, level_dbfs}; `available` is false (and the level null) while no microphone is open. Poll it for a level meter.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `audio.test_speaker`

Play a 440 Hz tone for one second on the speaker. Fails with invalid_state when the audio layer is not running.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `audio.echo_test`

Echo test of the microphone and speaker: record the microphone for `seconds` (1-10, default 3), then play the recording back on the speaker. `action` is `start` (default; fails with invalid_state while a test runs, while a call is in progress or without audio), `cancel` or `status`. Result {phase: idle|recording|playing|done|failed, elapsed_ms, total_ms (recording plus playback), peak_dbfs (once recording ended), silent (nothing audible was recorded: peak below -60 dBFS), error}. Poll with `status` until `done`.

| Param | Type | Required | Description |
|---|---|---|---|
| `action` | start \| cancel \| status | no |  |
| `seconds` | integer | no |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "action": {
      "enum": [
        "start",
        "cancel",
        "status"
      ],
      "type": "string"
    },
    "seconds": {
      "maximum": 10,
      "minimum": 1,
      "type": "integer"
    }
  },
  "type": "object"
}
```

</details>

### `config.get`

The effective configuration (passwords redacted).

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `config.view`

The effective configuration with the origin of every key, for building a settings UI or checking what an agent may change. Returns {path, hash, accepted_hash, config, provenance, secrets, editable}. `hash` is the content hash of the config file as it is on disk now: pass it as `config.edit`'s `base_hash` (it also works while the file on disk is invalid and was not accepted, so an edit can repair it). `accepted_hash` is the hash of the text the running configuration came from. `config` and `provenance` describe the accepted configuration. `config` is the effective config (vendor profiles applied) as JSON with the same sections and keys as config.toml (`account` is a list). Passwords are never returned: each account's `password` is `{"set": true|false}` and `secrets.control_token` is `{"set": true}`; such a value is not a valid input to `config.edit`. `provenance` has, per section, `{key: "file"|"default"}` and, per account id under `provenance.account`, `{key: "file"|"profile"|"default"}`: `file` = the key is set in config.toml, `profile` = the vendor profile filled a key the file omits, `default` = built-in default. `editable` lists the account fields and the sections/keys `config.edit` accepts.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `config.edit`

Changes config.toml with typed edits, then reloads it (the same path as an external edit: hot options apply at once, account changes re-register or rebuild as needed). Comments, key order and layout of the file are preserved; only the named keys change. All edits apply together or none do. Params {base_hash?, edits[]}: with `base_hash` (the `hash` of `config.view`) the call fails with invalid_state, data.reason `conflict` and data.current_hash when the file changed since it was read. Result {applied, hash, report, event_seq}: `hash` is the new base_hash, `report` the reload report {applied, changed, summary, errors}. Errors: invalid_params with data.reason `invalid` and data.errors [{path, message}] when an edit is wrong or the edited file would not validate (nothing is written); invalid_state with data.reason `file_invalid` when the file on disk does not parse (edits are refused until it is fixed; a file that parses but fails validation can still be edited, as long as the edited result validates, so an edit can repair it, e.g. disable the bad account). A read-only file is refused with internal. An absent key whose new value equals the default is not written. Edit objects, each with an `op`: {op:set_account, id, field, value} | {op:reset_account, id, field} (removes the key) | {op:add_account, draft:{id, fields:{field: value}}, replace_example?:bool} (`replace_example` replaces the shipped, untouched `example` account) | {op:remove_account, id} | {op:move_account, id, index} (moves the account to 0-based position `index` in the account list, which is the display order; an index past the end means last; comments above it move with it) | {op:rename_account, from, to} | {op:set_blf, account, entries:[{uri, label, group?, pickup?, pickup_prefix?, pickup_target?}]} (replaces the list; entries matched by uri keep their comments; an empty optional key, or pickup "" for inherit, is removed from the file) | {op:set_section, section, key, value} | {op:reset_section, section, key}. Account fields: enabled, profile, display_name, username, auth_username, password, password_store (file|keyring: with keyring the password lives in the system credential store and `password` must be empty; switching moves the existing password along), domain, registrar, outbound_proxy, transport (udp|tcp|tls|auto: SRV _sips._tcp &gt; _sip._tcp &gt; _sip._udp, else UDP 5060), local_port, tls_verify, tls_fingerprint, srtp (off|optional|mandatory), dnd, auto_answer, answer_delay_ms, allow_intercom, auto_hold, mwi (subscribe|unsolicited|off), sip_trace, pickup (off|prefix|replaces), pickup_prefix, pickup_replaces_target (caller|monitored). Sections: calls, ui (also `shortcuts.<action>`), network, logging, audio, general (only user_agent and log_level; log_dir, log_retention_days and reconfig_force are file-only), updates (check, channel: stable|prerelease; `reset_section` channel restores the automatic choice); `config.view` lists their keys under `editable`. Values are plain booleans, integers or strings, checked per field; unknown sections and keys are rejected. Everything else (extra_headers, [media], [speech], [control], ...) stays file-only.

| Param | Type | Required | Description |
|---|---|---|---|
| `base_hash` | string | no | `hash` from config.view; the edit fails with a conflict if the file changed since. |
| `edits` | array | yes |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "base_hash": {
      "description": "`hash` from config.view; the edit fails with a conflict if the file changed since.",
      "type": "string"
    },
    "edits": {
      "items": {
        "oneOf": [
          {
            "additionalProperties": false,
            "properties": {
              "field": {
                "enum": [
                  "enabled",
                  "profile",
                  "display_name",
                  "username",
                  "auth_username",
                  "password",
                  "password_store",
                  "domain",
                  "registrar",
                  "outbound_proxy",
                  "transport",
                  "local_port",
                  "tls_verify",
                  "tls_fingerprint",
                  "srtp",
                  "dnd",
                  "auto_answer",
                  "answer_delay_ms",
                  "allow_intercom",
                  "auto_hold",
                  "mwi",
                  "sip_trace",
                  "pickup",
                  "pickup_prefix",
                  "pickup_replaces_target",
                  "auth_foreign"
                ],
                "type": "string"
              },
              "id": {
                "description": "Account id.",
                "type": "string"
              },
              "op": {
                "const": "set_account"
              },
              "value": {
                "description": "A boolean, integer or string, as the field requires.",
                "type": [
                  "boolean",
                  "integer",
                  "string"
                ]
              }
            },
            "required": [
              "op",
              "id",
              "field",
              "value"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "field": {
                "enum": [
                  "enabled",
                  "profile",
                  "display_name",
                  "username",
                  "auth_username",
                  "password",
                  "password_store",
                  "domain",
                  "registrar",
                  "outbound_proxy",
                  "transport",
                  "local_port",
                  "tls_verify",
                  "tls_fingerprint",
                  "srtp",
                  "dnd",
                  "auto_answer",
                  "answer_delay_ms",
                  "allow_intercom",
                  "auto_hold",
                  "mwi",
                  "sip_trace",
                  "pickup",
                  "pickup_prefix",
                  "pickup_replaces_target",
                  "auth_foreign"
                ],
                "type": "string"
              },
              "id": {
                "description": "Account id.",
                "type": "string"
              },
              "op": {
                "const": "reset_account"
              }
            },
            "required": [
              "op",
              "id",
              "field"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "draft": {
                "additionalProperties": false,
                "properties": {
                  "fields": {
                    "additionalProperties": {
                      "description": "A boolean, integer or string, as the field requires.",
                      "type": [
                        "boolean",
                        "integer",
                        "string"
                      ]
                    },
                    "description": "field name -> value",
                    "type": "object"
                  },
                  "id": {
                    "description": "Account id.",
                    "type": "string"
                  }
                },
                "required": [
                  "id"
                ],
                "type": "object"
              },
              "op": {
                "const": "add_account"
              },
              "replace_example": {
                "type": "boolean"
              }
            },
            "required": [
              "op",
              "draft"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "id": {
                "description": "Account id.",
                "type": "string"
              },
              "op": {
                "const": "remove_account"
              }
            },
            "required": [
              "op",
              "id"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "id": {
                "description": "Account id.",
                "type": "string"
              },
              "index": {
                "description": "0-based target position; past the end means last",
                "minimum": 0,
                "type": "integer"
              },
              "op": {
                "const": "move_account"
              }
            },
            "required": [
              "op",
              "id",
              "index"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "from": {
                "description": "Account id.",
                "type": "string"
              },
              "op": {
                "const": "rename_account"
              },
              "to": {
                "type": "string"
              }
            },
            "required": [
              "op",
              "from",
              "to"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "account": {
                "description": "Account id.",
                "type": "string"
              },
              "entries": {
                "items": {
                  "additionalProperties": false,
                  "properties": {
                    "label": {
                      "type": "string"
                    },
                    "uri": {
                      "type": "string"
                    }
                  },
                  "required": [
                    "uri"
                  ],
                  "type": "object"
                },
                "type": "array"
              },
              "op": {
                "const": "set_blf"
              }
            },
            "required": [
              "op",
              "account",
              "entries"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "key": {
                "type": "string"
              },
              "op": {
                "const": "set_section"
              },
              "section": {
                "enum": [
                  "calls",
                  "ui",
                  "network",
                  "logging",
                  "audio",
                  "general",
                  "updates"
                ],
                "type": "string"
              },
              "value": {
                "description": "A boolean, integer or string, as the field requires.",
                "type": [
                  "boolean",
                  "integer",
                  "string"
                ]
              }
            },
            "required": [
              "op",
              "section",
              "key",
              "value"
            ],
            "type": "object"
          },
          {
            "additionalProperties": false,
            "properties": {
              "key": {
                "type": "string"
              },
              "op": {
                "const": "reset_section"
              },
              "section": {
                "enum": [
                  "calls",
                  "ui",
                  "network",
                  "logging",
                  "audio",
                  "general",
                  "updates"
                ],
                "type": "string"
              }
            },
            "required": [
              "op",
              "section",
              "key"
            ],
            "type": "object"
          }
        ]
      },
      "minItems": 1,
      "type": "array"
    }
  },
  "required": [
    "edits"
  ],
  "type": "object"
}
```

</details>

### `config.reload`

Re-read the config file now and apply the differences (same path as the file watcher). Returns {applied, changed, summary, errors}; an invalid file fails with invalid_state and the previous config stays.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `call.dial`

Place an outgoing call. `target` is an extension / number (becomes sip:&lt;target&gt;@&lt;domain&gt;) or a full sip:/sips: URI. Returns {call_id, call (full snapshot, state dialing), event_seq} as soon as the INVITE is sent; then use call.wait_for {call_id, state:"active", since:event_seq} to wait for the answer. Limits: `target` &lt;= 1024 bytes and custom `headers` &lt;= 8 KiB in total (invalid_params); an INVITE too large for a UDP datagram fails at once with sip_error (code 0, "transport: INVITE is ... too large") rather than timing out. Errors: not_found (account), invalid_params, sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id to call from. |
| `codecs` | array | no | Codec preference for this call. |
| `headers` | object | no | Extra INVITE headers, name -&gt; value. |
| `sink` | string | no | Audio sink: device \| null \| record:&lt;path.wav&gt; (relative paths: the config file's directory). Default: the account's audio_sink. |
| `source` | string | no | Audio source: device \| silence \| tone:&lt;hz&gt; \| file:&lt;path.wav&gt; (relative paths: the config file's directory). Default: the account's audio_source. Use tone:&lt;hz&gt; or file: for automated tests (no microphone). |
| `srtp` | off \| optional \| mandatory | no | SRTP policy for this call (default: account setting). |
| `target` | string | yes | Extension, number, or sip: URI. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id to call from.",
      "type": "string"
    },
    "codecs": {
      "description": "Codec preference for this call.",
      "items": {
        "enum": [
          "opus",
          "g722",
          "pcmu",
          "pcma"
        ],
        "type": "string"
      },
      "type": "array"
    },
    "headers": {
      "additionalProperties": {
        "type": "string"
      },
      "description": "Extra INVITE headers, name -> value.",
      "type": "object"
    },
    "sink": {
      "description": "Audio sink: device | null | record:<path.wav> (relative paths: the config file's directory). Default: the account's audio_sink.",
      "type": "string"
    },
    "source": {
      "description": "Audio source: device | silence | tone:<hz> | file:<path.wav> (relative paths: the config file's directory). Default: the account's audio_source. Use tone:<hz> or file: for automated tests (no microphone).",
      "type": "string"
    },
    "srtp": {
      "description": "SRTP policy for this call (default: account setting).",
      "enum": [
        "off",
        "optional",
        "mandatory"
      ],
      "type": "string"
    },
    "target": {
      "description": "Extension, number, or sip: URI.",
      "type": "string"
    }
  },
  "required": [
    "account",
    "target"
  ],
  "type": "object"
}
```

</details>

### `call.answer`

Answer a ringing inbound call (state `ringing`). Optionally override the audio source / sink for this call. A source/sink different from the call's current one recreates the call media at answer: rx_ms restarts at 0, and call.play / call.listen issued before the answer end with call_ended (issue them after the answer, or answer without source/sink). `mode` picks what happens to the other lines: omitted = the accounts' `auto_hold` setting decides whether the other device-bound calls (per [calls] hold_scope) are put on hold first; `hold_others` = hold them whatever `auto_hold` says (a failed hold is logged and the call is answered anyway); `end_others` + `end_call_id` = answer first, hold nothing, and hang `end_call_id` up only once the caller's ACK confirms the answer: the call returns success as soon as the 200 is sent, with `end_others: "pending_ack"`; a background task then waits for the ACK (until the INVITE transaction gives up, about 32 s) and hangs `end_call_id` up (watch its call.state ended event). If the ACK never comes `end_call_id` is untouched and becomes the active line again. If the answer itself fails (invalid_state) it is untouched too. Calls that use no audio device never hold other lines, whatever the mode. Answers that hold other lines are queued with call.activate (each hold is bounded to 8 s). Returns {ok, call_id, event_seq, end_others?}. Errors: not_found, invalid_state (not ringing), invalid_params.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `end_call_id` | string | no | With mode end_others: the call to hang up once this one is active. |
| `mode` | hold_others \| end_others | no | Line handling; default: the accounts' auto_hold setting. |
| `sink` | string | no | Audio sink: device \| null \| record:&lt;path.wav&gt; (relative paths: the config file's directory). Default: the account's audio_sink. |
| `source` | string | no | Audio source: device \| silence \| tone:&lt;hz&gt; \| file:&lt;path.wav&gt; (relative paths: the config file's directory). Default: the account's audio_source. Use tone:&lt;hz&gt; or file: for automated tests (no microphone). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "end_call_id": {
      "description": "With mode end_others: the call to hang up once this one is active.",
      "type": "string"
    },
    "mode": {
      "description": "Line handling; default: the accounts' auto_hold setting.",
      "enum": [
        "hold_others",
        "end_others"
      ],
      "type": "string"
    },
    "sink": {
      "description": "Audio sink: device | null | record:<path.wav> (relative paths: the config file's directory). Default: the account's audio_sink.",
      "type": "string"
    },
    "source": {
      "description": "Audio source: device | silence | tone:<hz> | file:<path.wav> (relative paths: the config file's directory). Default: the account's audio_source. Use tone:<hz> or file: for automated tests (no microphone).",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.reject`

Reject a ringing inbound call with a final response (default 486 Busy Here; e.g. 603 Decline, 480). Emits call.rejected / call.state ended.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `code` | integer | no | SIP status, default 486. |
| `reason` | string | no | Reason phrase. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "code": {
      "description": "SIP status, default 486.",
      "maximum": 699,
      "minimum": 400,
      "type": "integer"
    },
    "reason": {
      "description": "Reason phrase.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.hangup`

End a call: CANCEL while dialing/early, 603 for an unanswered inbound call, BYE otherwise. Returns event_seq; the call.state `ended` event follows.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.mute`

Mute our microphone/source for the call (local only; RTP keeps flowing with silence).

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.unmute`

Undo call.mute.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.hold`

Put an established call on hold (re-INVITE a=sendonly, or inactive per the account's hold_style / when the remote already holds us); we stop transmitting. Returns once the remote answered: `state` is `held` and `hold` = {local, remote}. A refused re-INVITE leaves the call as it was (sip_error with the status code); 491 glare is retried automatically. While a transfer of the call is `referring` or `transferring` (see transfer.get) it fails with invalid_state; an outstanding re-INVITE of ours on the call (e.g. the automatic resume after a failed transfer) is waited for once, so a redundant request succeeds. Returns event_seq; call.state / call.updated events follow.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.unhold`

Take a call off hold (re-INVITE a=sendrecv). `state` is `active`, or `remote_held` while the remote still holds us. Fails with invalid_state while a transfer of the call is `referring` or `transferring` (see transfer.get); an outstanding re-INVITE of ours on the call (e.g. the automatic resume after a failed transfer) is waited for once, so an unhold that became redundant succeeds.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.activate`

Make `call_id` the one live line: the other un-held device-bound calls (per [calls] hold_scope; calls without an audio device and the peer leg of a call between two of our own accounts are left alone) are put on hold, the call is resumed if we hold it, and the microphone/speaker focus moves to it. If any hold fails the whole operation aborts with sip_error and nothing is resumed. Idempotent: activating the active line does nothing. A call whose transfer is `referring` / `transferring` cannot be activated (invalid_state) and is never auto-held. Queued in order with the other multi-line operations (call.answer that holds other lines, dial's auto-hold); each hold is bounded to 8 s, a line that ends meanwhile counts as held, and an outstanding re-INVITE on a line is waited for once; while it runs `pending` is `activate` on the target and `hold` on the calls being held. Returns {ok, call_id, active_line, event_seq}. Errors: not_found, invalid_state (not established, or no audio device), sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.transfer_blind`

Blind-transfer an established call to `target` (extension / number -&gt; sip:&lt;target&gt;@&lt;domain&gt;, or a sip: URI): the call is put on hold, then REFER with Refer-To and Referred-By is sent. Returns {ok, session_id, session, event_seq} once the REFER was accepted (202); it creates a blind transfer session (see transfer.get; phases referring, transferring, then transferred / failed / unconfirmed) and emits `transfer.session` events; the `call.transfer_progress` events {call_id, code, reason, final} (100 Trying, 180, then the final 200 or a failure) stay. On a final 2xx the call ends (end.reason and end.cause `transferred`, whichever side's BYE came first). On a definitive failure the call we put on hold is taken off hold again ([calls] resume_after_failed_transfer, default true; not when another device line is live); no final NOTIFY within 32 s gives final code 408, session phase `unconfirmed` and no automatic action (a late final NOTIFY still decides). A refused REFER fails with sip_error and the status in sip_code (also reported as a final progress event). Errors: not_found, invalid_state (not established, or the call is in another transfer session), invalid_params, sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `target` | string | yes | Extension, number or sip:/sips: URI to transfer the remote party to. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "target": {
      "description": "Extension, number or sip:/sips: URI to transfer the remote party to.",
      "type": "string"
    }
  },
  "required": [
    "call_id",
    "target"
  ],
  "type": "object"
}
```

</details>

### `call.transfer_attended`

Attended transfer, legacy one-call form: `call_id` (the original call, held if it is not) is handed over to the party of `consult_call_id` (a second call of the same account). It creates (or, when call.consult_start / call.consult_link already paired these two calls, drives) a transfer session and completes it at once: REFER with Refer-To &lt;consult party?Replaces=...&gt; is sent on the original call (account option attended_refer_on = "consult": on the consult call, pointing at the original), the target is the AOR or Contact (attended_refer_target). If the consult call is still dialing/early it is CANCELed and the original is transferred blind to the party it rang (semi-attended; for an explicit choice use call.consult_start + transfer.complete). Returns {ok, session_id, session, event_seq}. Progress as call.transfer_progress on the call the REFER went to; on a final 2xx that call ends (`transferred`) and the other one is hung up after 5 s unless the transfer target ended it. A REFER that fails while the consult call is alive returns the session to `consult_connected` with `last_error`. Errors: not_found, invalid_state, invalid_params (same call / different accounts), sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `consult_call_id` | string | yes | The consult call (c2, ...) of the same account. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "consult_call_id": {
      "description": "The consult call (c2, ...) of the same account.",
      "type": "string"
    }
  },
  "required": [
    "call_id",
    "consult_call_id"
  ],
  "type": "object"
}
```

</details>

### `call.consult_start`

Start an attended transfer: put `call_id` (established) on hold, dial `target` on the same account (the consult call) and link both in a transfer session. Calls that use no audio device (agent / silence / file / null) are held too, as a phone does for a transfer, but never cause holds of other calls. `source` / `sink` default to the original call's own. Returns {ok, session_id, consult_call_id, session, event_seq} as soon as the INVITE is sent; the session phase follows the consult call (consult_dialing, consult_ringing, consult_connected, or consult_failed with the original still held: use transfer.cancel to resume it). Then call transfer.complete, transfer.swap or transfer.cancel. A call can be in one live session at a time. Queued with the other multi-line operations (call.activate); the hold is bounded to 6 s. Errors: not_found, invalid_state (not established, already in a session), invalid_params, sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `sink` | string | no | Audio sink: device \| null \| record:&lt;path.wav&gt; (relative paths: the config file's directory). Default: the account's audio_sink. |
| `source` | string | no | Audio source: device \| silence \| tone:&lt;hz&gt; \| file:&lt;path.wav&gt; (relative paths: the config file's directory). Default: the account's audio_source. Use tone:&lt;hz&gt; or file: for automated tests (no microphone). |
| `target` | string | yes | Extension, number or sip:/sips: URI to consult. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "sink": {
      "description": "Audio sink: device | null | record:<path.wav> (relative paths: the config file's directory). Default: the account's audio_sink.",
      "type": "string"
    },
    "source": {
      "description": "Audio source: device | silence | tone:<hz> | file:<path.wav> (relative paths: the config file's directory). Default: the account's audio_source. Use tone:<hz> or file: for automated tests (no microphone).",
      "type": "string"
    },
    "target": {
      "description": "Extension, number or sip:/sips: URI to consult.",
      "type": "string"
    }
  },
  "required": [
    "call_id",
    "target"
  ],
  "type": "object"
}
```

</details>

### `call.consult_link`

Pair two existing calls of the same account into a transfer session: `call_id` is the original (established), `consult_call_id` the consult call (dialing, early or established). Nothing is held or dialled. Returns {ok, session_id, consult_call_id, session, event_seq}. Errors: not_found, invalid_state (not established, already in a session), invalid_params (same call / different accounts: "both calls must belong to the same account").

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `consult_call_id` | string | yes | The consult call of the same account. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "consult_call_id": {
      "description": "The consult call of the same account.",
      "type": "string"
    }
  },
  "required": [
    "call_id",
    "consult_call_id"
  ],
  "type": "object"
}
```

</details>

### `transfer.complete`

Finish a consult. `mode` attended: REFER with Replaces connects the original to the consult party (needs consult_connected; while the consult still rings it fails with invalid_state "consult not answered; use blind_now"). `blind_now`: end the consult call (CANCEL / BYE) and REFER the original to the consult's dialled URI (semi-attended). Returns once the REFER was accepted: {ok, session_id, session, event_seq}; the phase is then transferring and ends in transferred, failed or unconfirmed (watch `transfer.session` events or poll transfer.get). A refused REFER (or a local error) fails with sip_error; the session returns to consult_connected with `last_error` (attended with the consult alive), or becomes failed and the original is resumed ([calls] resume_after_failed_transfer). Idempotent: completing a session whose REFER is already sent returns the session and sends no second REFER. Errors: not_found, invalid_state, invalid_params (mode), sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `mode` | attended \| blind_now | yes | attended = Replaces transfer of an answered consult; blind_now = cancel the consult and transfer blind to its target. |
| `session_id` | string | yes | Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "mode": {
      "description": "attended = Replaces transfer of an answered consult; blind_now = cancel the consult and transfer blind to its target.",
      "enum": [
        "attended",
        "blind_now"
      ],
      "type": "string"
    },
    "session_id": {
      "description": "Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events.",
      "type": "string"
    }
  },
  "required": [
    "session_id",
    "mode"
  ],
  "type": "object"
}
```

</details>

### `transfer.swap`

During consult_connected: hold the leg you are talking to (`talking_to`), resume the other and give it the microphone / speaker focus. If the hold fails nothing is resumed and `last_error` (stage swap) is set. Returns {ok, session_id, session, event_seq}. Errors: not_found, invalid_state (not consult_connected), sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `session_id` | string | yes | Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "session_id": {
      "description": "Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events.",
      "type": "string"
    }
  },
  "required": [
    "session_id"
  ],
  "type": "object"
}
```

</details>

### `transfer.cancel`

Abandon a consult: hang up the consult call (CANCEL or BYE), resume the original when the session held it, focus it. Phase becomes cancelled. A no-op returning the session when it is already finished; invalid_state "transfer already handed to the PBX" once the REFER was sent (referring, transferring, unconfirmed: SIP cannot retract an accepted REFER). Returns {ok, session_id, session, event_seq}.

| Param | Type | Required | Description |
|---|---|---|---|
| `session_id` | string | yes | Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "session_id": {
      "description": "Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events.",
      "type": "string"
    }
  },
  "required": [
    "session_id"
  ],
  "type": "object"
}
```

</details>

### `transfer.get`

One transfer session (live, or among the last 16 finished): {id, kind: blind|attended|semi, phase, account, original, consult, target_uri, refer_call, talking_to, we_held_original, last_notify {code, reason, at}, last_error {stage, code, reason, problem {kind, title, hint}}, started_at, ended_at}. Phases: consult_dialing, consult_ringing, consult_connected, consult_failed, referring, transferring, unconfirmed (no final NOTIFY after 32 s: the transfer may have happened; nothing is done automatically, a late final NOTIFY moves it on), and the terminal transferred, failed, cancelled, abandoned (the original ended before the outcome). Calls carry `transfer {session_id, role: original|consult}`. While a session is referring or transferring, call.hold / call.unhold of either call fail with invalid_state. Errors: not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `session_id` | string | yes | Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "session_id": {
      "description": "Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events.",
      "type": "string"
    }
  },
  "required": [
    "session_id"
  ],
  "type": "object"
}
```

</details>

### `transfer.list`

Transfer sessions: the last 16 finished ones, then the live ones, oldest first. Returns {transfers, event_seq}.

| Param | Type | Required | Description |
|---|---|---|---|
| `active_only` | boolean | no | Leave out finished sessions. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "active_only": {
      "description": "Leave out finished sessions.",
      "type": "boolean"
    }
  },
  "required": [],
  "type": "object"
}
```

</details>

### `call.dtmf`

Send up to 32 DTMF digits (0-9 * # A-D) on an established call; each lasts duration_ms (10-10000). `mode`: rfc4733 (telephone-event), info (SIP INFO), inband (audio tones) or auto (rfc4733 if negotiated, else info; default: config dtmf_mode). The far end sees call.dtmf events.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `digits` | string | yes | Digits to send, 0-9 * # A-D, at most 32, e.g. "123#". |
| `duration_ms` | integer | no | Duration of each digit in ms, 10-10000 (default: config dtmf_duration_ms). |
| `mode` | rfc4733 \| info \| inband \| auto | no |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "digits": {
      "description": "Digits to send, 0-9 * # A-D, at most 32, e.g. \"123#\".",
      "maxLength": 32,
      "type": "string"
    },
    "duration_ms": {
      "description": "Duration of each digit in ms, 10-10000 (default: config dtmf_duration_ms).",
      "maximum": 10000,
      "minimum": 10,
      "type": "integer"
    },
    "mode": {
      "enum": [
        "rfc4733",
        "info",
        "inband",
        "auto"
      ],
      "type": "string"
    }
  },
  "required": [
    "call_id",
    "digits"
  ],
  "type": "object"
}
```

</details>

### `call.list`

All calls of this run (the last 64 ended ones included), oldest first: id, account, direction, state, remote party, SIP Call-ID, timestamps, end {code, reason, by, problem (absent for a normal end)}, negotiated codec / SRTP suite.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Only calls of this account. |
| `active_only` | boolean | no | Leave out ended calls. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Only calls of this account.",
      "type": "string"
    },
    "active_only": {
      "description": "Leave out ended calls.",
      "type": "boolean"
    }
  },
  "required": [],
  "type": "object"
}
```

</details>

### `call.get`

One call: everything from call.list plus `media` (RTP/SRTP counters, jitter, RTCP, rx_level_dbfs, rx_active) and `audio` (source, sink, device_bound, rx_ms: this media's receive timeline in ms, play {current, queued}, tx_stream: a call.stream TX is attached, rx_tap_dropped_ms: cumulative RX audio the agent hub could not take, normally 0). Errors: not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.wait_for`

Wait until a call reaches a state (dialing|ringing|early|active|held|remote_held|ended). Give `call_id`, or `account` (any/most recent call of the account, e.g. to catch an incoming call: state `ringing`). Race-free: if the call is already in that state and `since` is omitted it returns at once; otherwise it waits for a call.state event after `since` (default: now; pass the event_seq of the action that triggers it). With `call_id`, the call ending first (or already being over, without the state in the log after `since`) fails at once with invalid_state and the end code/reason/problem. A `since` ahead of the event log (instance restarted) fails with invalid_params. Returns {call, event_seq, matched_seq}. Errors: timeout, invalid_state, not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Watch calls of this account instead of one call. |
| `call_id` | string | no | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `since` | integer | no | Only consider events with seq greater than this. |
| `state` | dialing \| ringing \| early \| active \| held \| remote_held \| ended | yes |  |
| `timeout_ms` | integer | no | Give up after this many milliseconds (default 10000). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Watch calls of this account instead of one call.",
      "type": "string"
    },
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "since": {
      "description": "Only consider events with seq greater than this.",
      "minimum": 0,
      "type": "integer"
    },
    "state": {
      "enum": [
        "dialing",
        "ringing",
        "early",
        "active",
        "held",
        "remote_held",
        "ended"
      ],
      "type": "string"
    },
    "timeout_ms": {
      "description": "Give up after this many milliseconds (default 10000).",
      "maximum": 60000,
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "state"
  ],
  "type": "object"
}
```

</details>

### `call.play`

Play audio into the call in place of its own source. Give exactly one of `source` (tone:&lt;hz&gt; | file:&lt;path.wav&gt; | silence | device; relative paths from the config file's directory, decoded before returning: an unreadable file fails with invalid_params; `device` stops all plays and restores the call's own source), `wav_base64` (a WAV, at most 10 MiB decoded) or `pcm_base64` + `pcm_format`. Clips are at most 600 s for `file:` sources and 120 s for inline audio. `wait:true` is capped at 120 s (default timeout min(total + 30 s, 120 s)), so it always times out for clips over about 90 s: for long clips use wait:false and the call.play_finished event (or events.poll). `loop` defaults to true for source specs and false for inline audio; `duration_ms` limits the transmitted time. `queue:true` appends behind the current play (at most 8 queued / 300 s), otherwise it replaces the current play and the queue. A play waits while the call is held, muted or not yet answered; if the answer then recreates the media (call.answer with a different source/sink) it ends with call_ended. `wait:true` (finite plays only: loop:false or duration_ms) returns when it has finished: {finished:{reason: completed|stopped|replaced|barge_in|call_ended|error, played_ms, rx_ms, barge_in_rx_ms (only for barge_in: where the caller's interruption began; use it instead of rx_ms as the next listen's since_rx_ms)}}; on timeout_ms it fails with `timeout` and the error carries play_id while the play continues. `barge_in:true` stops the play when the far end starts speaking. Returns {ok, call_id, play_id, duration_ms (null = endless), queued, rx_ms, event_seq, finished?}; every play ends with exactly one call.play_finished event.

| Param | Type | Required | Description |
|---|---|---|---|
| `barge_in` | boolean | no | Stop the play when the far end starts talking (default false; echo paths trigger it themselves). |
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `duration_ms` | integer | no | Stop after this much transmitted audio. |
| `gain_db` | number | no | Gain in dB (default 0). |
| `loop` | boolean | no | Repeat until stopped (default true for source, false for inline audio). |
| `pcm_base64` | string | no | Base64 raw mono PCM; needs pcm_format. |
| `pcm_format` | object | no |  |
| `queue` | boolean | no | Append to the play queue instead of replacing (default false). |
| `source` | string | no | tone:&lt;hz&gt; \| file:&lt;path.wav&gt; \| silence \| device |
| `timeout_ms` | integer | no | With wait: give up after this long (default: play length + 30 s, at most 120000). |
| `wait` | boolean | no | Return when the play has finished (finite plays only). |
| `wav_base64` | string | no | Base64 WAV file (any rate / channels; converted). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "barge_in": {
      "description": "Stop the play when the far end starts talking (default false; echo paths trigger it themselves).",
      "type": "boolean"
    },
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "duration_ms": {
      "description": "Stop after this much transmitted audio.",
      "maximum": 600000,
      "minimum": 1,
      "type": "integer"
    },
    "gain_db": {
      "description": "Gain in dB (default 0).",
      "maximum": 20,
      "minimum": -40,
      "type": "number"
    },
    "loop": {
      "description": "Repeat until stopped (default true for source, false for inline audio).",
      "type": "boolean"
    },
    "pcm_base64": {
      "description": "Base64 raw mono PCM; needs pcm_format.",
      "type": "string"
    },
    "pcm_format": {
      "additionalProperties": false,
      "properties": {
        "encoding": {
          "enum": [
            "s16le",
            "f32le",
            "mulaw",
            "alaw"
          ],
          "type": "string"
        },
        "sample_rate": {
          "maximum": 48000,
          "minimum": 8000,
          "type": "integer"
        }
      },
      "required": [
        "sample_rate",
        "encoding"
      ],
      "type": "object"
    },
    "queue": {
      "description": "Append to the play queue instead of replacing (default false).",
      "type": "boolean"
    },
    "source": {
      "description": "tone:<hz> | file:<path.wav> | silence | device",
      "type": "string"
    },
    "timeout_ms": {
      "description": "With wait: give up after this long (default: play length + 30 s, at most 120000).",
      "maximum": 120000,
      "minimum": 0,
      "type": "integer"
    },
    "wait": {
      "description": "Return when the play has finished (finite plays only).",
      "type": "boolean"
    },
    "wav_base64": {
      "description": "Base64 WAV file (any rate / channels; converted).",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.stop_play`

Stop the current play (or the one with `play_id`; unknown / finished ids are ignored) and, by default, clear the queue; the call's own source resumes when nothing is left. The stopped play ends with call.play_finished {reason:"stopped"}.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `clear_queue` | boolean | no | Also drop the queued plays (default true). |
| `play_id` | integer | no | Only this play (default: the current one). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "clear_queue": {
      "description": "Also drop the queued plays (default true).",
      "type": "boolean"
    },
    "play_id": {
      "description": "Only this play (default: the current one).",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.record_start`

Record the audio we receive on this call to a 48 kHz 16-bit WAV file. `stereo`: left = received, right = transmitted.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `path` | string | yes | Output .wav path (relative paths: the config file's directory). The file is created at once; an unwritable path fails with invalid_params. |
| `stereo` | boolean | no |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "path": {
      "description": "Output .wav path (relative paths: the config file's directory). The file is created at once; an unwritable path fails with invalid_params.",
      "type": "string"
    },
    "stereo": {
      "type": "boolean"
    }
  },
  "required": [
    "call_id",
    "path"
  ],
  "type": "object"
}
```

</details>

### `call.record_stop`

Finish the recording started with call.record_start; returns {finished, path} once the file is closed.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.detect_tone`

Listen to the decoded audio received on the call and report whether a sine tone of `freq_hz` is present (Goertzel; returns as soon as it is heard or after timeout_ms). Returns {detected, level_dbfs, power_ratio}. This proves the real RTP audio path end to end.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `freq_hz` | number | yes |  |
| `min_level_dbfs` | number | no | Minimum tone level, default -45 dBFS. |
| `timeout_ms` | integer | no | Give up after this many milliseconds (default 5000). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "freq_hz": {
      "exclusiveMaximum": 24000,
      "exclusiveMinimum": 0,
      "type": "number"
    },
    "min_level_dbfs": {
      "description": "Minimum tone level, default -45 dBFS.",
      "type": "number"
    },
    "timeout_ms": {
      "description": "Give up after this many milliseconds (default 5000).",
      "maximum": 60000,
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "call_id",
    "freq_hz"
  ],
  "type": "object"
}
```

</details>

### `call.expect_dtmf`

Wait until the DTMF digits `digits` arrived in sequence on the call (call.dtmf events, RFC 4733 or INFO). Pass `since` = event_seq returned before the digits were sent, else only digits arriving after this call count. Returns {matched, received}; on timeout the error carries what was received; if the call ends first it fails at once with invalid_state.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1, c2, ...) from call.dial, call.incoming events or call.list. |
| `digits` | string | yes | Expected digits, e.g. "123#". |
| `since` | integer | no | Count digits with event seq greater than this. |
| `timeout_ms` | integer | no | Give up after this many milliseconds (default 10000). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1, c2, ...) from call.dial, call.incoming events or call.list.",
      "type": "string"
    },
    "digits": {
      "description": "Expected digits, e.g. \"123#\".",
      "type": "string"
    },
    "since": {
      "description": "Count digits with event seq greater than this.",
      "minimum": 0,
      "type": "integer"
    },
    "timeout_ms": {
      "description": "Give up after this many milliseconds (default 10000).",
      "maximum": 60000,
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "call_id",
    "digits"
  ],
  "type": "object"
}
```

</details>

### `call.sip_trace`

SIP messages (full wire records) of the call's dialog from the in-memory trace ring. Accepts a call id (c1) or a raw SIP Call-ID. Needs sip_trace enabled for the account.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes | Call id (c1) or SIP Call-ID header value. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "description": "Call id (c1) or SIP Call-ID header value.",
      "type": "string"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `account.set_dnd`

Set do-not-disturb for an account at runtime (lasts until the account's `dnd` config value changes). While on, incoming calls are rejected with the account's dnd_reject_code (default 486) and call.rejected{reason:"dnd"} is emitted. Emits account.dnd.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes |  |
| `enabled` | boolean | yes |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "type": "string"
    },
    "enabled": {
      "type": "boolean"
    }
  },
  "required": [
    "account",
    "enabled"
  ],
  "type": "object"
}
```

</details>

### `blf.list`

Busy-lamp-field state of the monitored extensions (`[[account.blf]]`): per entry uri, label, group, state (idle|ringing|dialing|busy|held|unknown|failed), the dialogs behind it (state, direction, remote identity, call-id, tags) and updated_at. Also `pickup` (effective directed-pickup method: off|prefix|replaces; pickup is offered while state is ringing and this is not off), `subscription` {phase: pending|active|retrying|failed, last_code, last_reason, last_problem {kind, title, hint} (present after a failure), expires_at, retry_at} (the health of the SUBSCRIBE behind the entry; `failed` = refused for good, `retrying` = another attempt is scheduled at retry_at) and `stale` (true while retrying, or when no NOTIFY / refresh arrived within 30 s after expires_at: the state shown is the last known one and may be out of date). Omit `account` for all accounts. Errors: not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Account id; default all accounts. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id; default all accounts.",
      "type": "string"
    }
  },
  "type": "object"
}
```

</details>

### `blf.dial`

Call a monitored BLF entry on its account: what a click on the key does (decision D5). Same as call.dial {account, target: uri} but the entry must exist in `[[account.blf]]`. Returns {call_id, call, event_seq} like call.dial; the call then goes through the normal dial path (auto_hold rules apply; a non-device `source`/`sink` never holds device calls). Errors: not_found (account or BLF entry), invalid_params, sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id the BLF entry belongs to. |
| `sink` | string | no | Audio sink: device \| null \| record:&lt;path.wav&gt;. Default: the account's audio_sink. |
| `source` | string | no | Audio source: device \| silence \| tone:&lt;hz&gt; \| file:&lt;path.wav&gt;. Default: the account's audio_source. |
| `uri` | string | yes | The BLF entry's `uri` exactly as configured / listed by blf.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id the BLF entry belongs to.",
      "type": "string"
    },
    "sink": {
      "description": "Audio sink: device | null | record:<path.wav>. Default: the account's audio_sink.",
      "type": "string"
    },
    "source": {
      "description": "Audio source: device | silence | tone:<hz> | file:<path.wav>. Default: the account's audio_source.",
      "type": "string"
    },
    "uri": {
      "description": "The BLF entry's `uri` exactly as configured / listed by blf.list.",
      "type": "string"
    }
  },
  "required": [
    "account",
    "uri"
  ],
  "type": "object"
}
```

</details>

### `call.pickup`

Directed pickup of a ringing monitored extension (decision D7). Allowed only while the BLF entry `uri` (as in blf.list) is `ringing` and pickup is configured for it: the method is the entry's `pickup` override, else the account's `pickup` (off|prefix|replaces). `prefix` dials `pickup_target` if set, else `<pickup_prefix><extension>` on the account (default prefix `**`, e.g. `**1004`). `replaces` (experimental; PBX support varies) sends an INVITE with `Require: replaces` and `Replaces: <call-id>;to-tag=<remote-tag>;from-tag=<local-tag>;early-only` to the caller (dialog-info &lt;remote&gt; target, RFC 5359 section 2.16; account `pickup_replaces_target = "monitored"` sends it to the monitored URI instead, same tags). Every pickup is routed through the account's registrar / outbound proxy whatever host the target names. Refused while the entry is stale or its subscription is not active. Returns {call_id, call, event_seq} like call.dial; a refusal (404, 480, 486, ...) arrives as call.state ended with end.code and end.problem. Errors: not_found (account or BLF entry), invalid_state (pickup not configured, not ringing, stale, or `replaces` without usable dialog-info), invalid_params, sip_error.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | yes | Account id the BLF entry belongs to. |
| `sink` | string | no | Audio sink: device \| null \| record:&lt;path.wav&gt;. Default: the account's audio_sink. |
| `source` | string | no | Audio source: device \| silence \| tone:&lt;hz&gt; \| file:&lt;path.wav&gt;. Default: the account's audio_source. |
| `uri` | string | yes | The BLF entry's `uri` exactly as configured / listed by blf.list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id the BLF entry belongs to.",
      "type": "string"
    },
    "sink": {
      "description": "Audio sink: device | null | record:<path.wav>. Default: the account's audio_sink.",
      "type": "string"
    },
    "source": {
      "description": "Audio source: device | silence | tone:<hz> | file:<path.wav>. Default: the account's audio_source.",
      "type": "string"
    },
    "uri": {
      "description": "The BLF entry's `uri` exactly as configured / listed by blf.list.",
      "type": "string"
    }
  },
  "required": [
    "account",
    "uri"
  ],
  "type": "object"
}
```

</details>

### `mwi.get`

Message-waiting status: {waiting, account_uri, voice{new,old,urgent_new,urgent_old}, other{class: counts}, updated_at}, or null until the first message-summary NOTIFY arrived (or with mwi="off"). With `account` returns {account, mwi}; without it {mwi:[...]} for every account that has one. Errors: not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Account id; default all accounts. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id; default all accounts.",
      "type": "string"
    }
  },
  "type": "object"
}
```

</details>

### `subscribe`

Create a generic SUBSCRIBE for any event package (presence, as-feature-event, ...). It is kept alive (refresh at 85% of the granted expiry, resubscribe when the notifier allows) until `unsubscribe`. Every NOTIFY becomes a `subscription.notify` event with the raw body; state changes are `subscription.state` events. Returns {sub_id, event_seq}. Errors: not_found (account), invalid_params.

| Param | Type | Required | Description |
|---|---|---|---|
| `accept` | string | no | Accept header; default depends on the package. |
| `account` | string | yes | Account id. |
| `event` | string | yes | Event package, e.g. "presence", "dialog", "message-summary". |
| `expires` | integer | no | Requested lifetime in seconds (default 600). |
| `uri` | string | yes | Extension, user@host, or full sip: URI (extensions get the account's domain). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "accept": {
      "description": "Accept header; default depends on the package.",
      "type": "string"
    },
    "account": {
      "description": "Account id.",
      "type": "string"
    },
    "event": {
      "description": "Event package, e.g. \"presence\", \"dialog\", \"message-summary\".",
      "type": "string"
    },
    "expires": {
      "description": "Requested lifetime in seconds (default 600).",
      "minimum": 1,
      "type": "integer"
    },
    "uri": {
      "description": "Extension, user@host, or full sip: URI (extensions get the account's domain).",
      "type": "string"
    }
  },
  "required": [
    "account",
    "uri",
    "event"
  ],
  "type": "object"
}
```

</details>

### `unsubscribe`

End a subscription made with `subscribe` (sends Expires: 0, bounded). BLF and MWI subscriptions are managed through the config. Returns {ok, event_seq}. Errors: not_found, invalid_params.

| Param | Type | Required | Description |
|---|---|---|---|
| `sub_id` | string | yes | Subscription id from `subscribe` or `subscriptions.list`. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "sub_id": {
      "description": "Subscription id from `subscribe` or `subscriptions.list`.",
      "type": "string"
    }
  },
  "required": [
    "sub_id"
  ],
  "type": "object"
}
```

</details>

### `subscriptions.list`

All subscriptions (BLF and MWI included): id, account, uri, event, accept, kind (blf|mwi|generic), state (pending|active|terminated|failed{code,reason}), expires_at, last_notify_at. Omit `account` for all accounts. Errors: not_found.

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Account id; default all accounts. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "account": {
      "description": "Account id; default all accounts.",
      "type": "string"
    }
  },
  "type": "object"
}
```

</details>

### `call.say`

Speak `text` into the call with the configured text-to-speech command ([speech] tts_command; without one this fails with invalid_state). Same options and result as call.play (play_id, duration_ms, rx_ms, finished with wait) plus tts_ms. For turn-taking use wait:true, then call.listen {since_rx_ms: finished.barge_in_rx_ms if present else finished.rx_ms}.

| Param | Type | Required | Description |
|---|---|---|---|
| `barge_in` | boolean | no | Stop the speech when the far end starts talking (default false; echo paths trigger it themselves). |
| `call_id` | string | yes | Call id (c1, c2, ...). |
| `gain_db` | number | no | Gain in dB (default 0). |
| `queue` | boolean | no | Queue behind the current play instead of replacing it (default false). |
| `text` | string | yes |  |
| `timeout_ms` | integer | no | With wait: give up after this long (default: clip length + 30 s, at most 120000). The play keeps going; the error carries play_id. |
| `voice` | string | no | TTS voice (letters, digits and _.+:- only); default [speech] tts_voice. |
| `wait` | boolean | no | Return when the speech has finished playing (result.finished). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "barge_in": {
      "description": "Stop the speech when the far end starts talking (default false; echo paths trigger it themselves).",
      "type": "boolean"
    },
    "call_id": {
      "description": "Call id (c1, c2, ...).",
      "type": "string"
    },
    "gain_db": {
      "description": "Gain in dB (default 0).",
      "maximum": 20,
      "minimum": -40,
      "type": "number"
    },
    "queue": {
      "description": "Queue behind the current play instead of replacing it (default false).",
      "type": "boolean"
    },
    "text": {
      "maxLength": 2000,
      "minLength": 1,
      "type": "string"
    },
    "timeout_ms": {
      "description": "With wait: give up after this long (default: clip length + 30 s, at most 120000). The play keeps going; the error carries play_id.",
      "maximum": 120000,
      "minimum": 0,
      "type": "integer"
    },
    "voice": {
      "description": "TTS voice (letters, digits and _.+:- only); default [speech] tts_voice.",
      "type": "string"
    },
    "wait": {
      "description": "Return when the speech has finished playing (result.finished).",
      "type": "boolean"
    }
  },
  "required": [
    "call_id",
    "text"
  ],
  "type": "object"
}
```

</details>

### `call.listen`

Wait for the far end to say something and return it. Voice activity detection finds the utterance on the call's receive timeline. Returns {speech, reason: end_of_speech|max_duration|no_speech|call_ended|timeout, start_rx_ms, end_rx_ms, speech_start_rx_ms, speech_end_rx_ms, speech_ms, level_dbfs{peak,mean}, noise_floor_dbfs, truncated, dtmf (digits received in the window), transcript?, stt_ms?, audio?{format:wav, sample_rate, channels, duration_ms, base64|path}, event_seq}. After you speak (call.say/call.play with wait), pass its finished.barge_in_rx_ms if present (the caller interrupted: where they began), else finished.rx_ms (end of your own audio), as since_rx_ms; when listening again without speaking in between, pass the previous end_rx_ms. since_rx_ms in the past (up to rx_history_secs, at least 5 s) is analysed from recorded audio, so thinking time between turns loses nothing, and start_timeout_ms counts only from the moment of the request. rx_ms restarts if the call media is recreated at answer with a different source/sink; a listen issued before that ends with call_ended (see call.answer). With transcribe:true the audio is returned only if audio is set explicitly. Tones count as speech.

| Param | Type | Required | Description |
|---|---|---|---|
| `audio` | base64 \| none \| file | no | How to return the audio: base64 (a WAV in audio.base64; the default), none (the default with transcribe), or file (write to `path`). |
| `call_id` | string | yes | Call id (c1, c2, ...). |
| `end_silence_ms` | integer | no | Silence that ends the utterance (default 800). |
| `language` | string | no | STT language (letters, digits and _.+:- only); default [speech] stt_language. |
| `max_ms` | integer | no | Longest utterance (default 30000). |
| `min_speech_ms` | integer | no | Voiced time that counts as speech (default 120). |
| `path` | string | no | Output .wav path for audio:"file" (relative paths: the config file's directory). |
| `pre_roll_ms` | integer | no | Audio kept before the speech starts, within the window (default 300). |
| `sample_rate` | 16000 \| 8000 | no | Sample rate of the returned WAV (default 16000). |
| `since_rx_ms` | integer | no | Start of the window on the receive timeline (default: now). Older than the history: clamped, truncated:true. More than 10 s ahead of now: invalid_params. After speaking use finished.barge_in_rx_ms (present when the caller interrupted) else finished.rx_ms from call.play/say; listening again without speaking, use end_rx_ms from the previous listen. |
| `start_timeout_ms` | integer | no | Give up with reason no_speech when nothing is said within this long (default 8000). |
| `threshold_dbfs` | number | no | Lowest level that counts as voice (default -45; the detector also adapts to the noise floor). |
| `timeout_ms` | integer | no | Wall-clock limit for the whole call (default start_timeout_ms + max_ms + 5 s). |
| `transcribe` | boolean | no | Run the [speech] stt_command on the speech (invalid_state when not configured). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "audio": {
      "description": "How to return the audio: base64 (a WAV in audio.base64; the default), none (the default with transcribe), or file (write to `path`).",
      "enum": [
        "base64",
        "none",
        "file"
      ],
      "type": "string"
    },
    "call_id": {
      "description": "Call id (c1, c2, ...).",
      "type": "string"
    },
    "end_silence_ms": {
      "description": "Silence that ends the utterance (default 800).",
      "maximum": 10000,
      "minimum": 20,
      "type": "integer"
    },
    "language": {
      "description": "STT language (letters, digits and _.+:- only); default [speech] stt_language.",
      "type": "string"
    },
    "max_ms": {
      "description": "Longest utterance (default 30000).",
      "maximum": 120000,
      "minimum": 100,
      "type": "integer"
    },
    "min_speech_ms": {
      "description": "Voiced time that counts as speech (default 120).",
      "maximum": 5000,
      "minimum": 20,
      "type": "integer"
    },
    "path": {
      "description": "Output .wav path for audio:\"file\" (relative paths: the config file's directory).",
      "type": "string"
    },
    "pre_roll_ms": {
      "description": "Audio kept before the speech starts, within the window (default 300).",
      "maximum": 5000,
      "minimum": 0,
      "type": "integer"
    },
    "sample_rate": {
      "description": "Sample rate of the returned WAV (default 16000).",
      "enum": [
        16000,
        8000
      ],
      "type": "integer"
    },
    "since_rx_ms": {
      "description": "Start of the window on the receive timeline (default: now). Older than the history: clamped, truncated:true. More than 10 s ahead of now: invalid_params. After speaking use finished.barge_in_rx_ms (present when the caller interrupted) else finished.rx_ms from call.play/say; listening again without speaking, use end_rx_ms from the previous listen.",
      "minimum": 0,
      "type": "integer"
    },
    "start_timeout_ms": {
      "description": "Give up with reason no_speech when nothing is said within this long (default 8000).",
      "maximum": 180000,
      "minimum": 20,
      "type": "integer"
    },
    "threshold_dbfs": {
      "description": "Lowest level that counts as voice (default -45; the detector also adapts to the noise floor).",
      "maximum": 0,
      "minimum": -90,
      "type": "number"
    },
    "timeout_ms": {
      "description": "Wall-clock limit for the whole call (default start_timeout_ms + max_ms + 5 s).",
      "maximum": 180000,
      "minimum": 1,
      "type": "integer"
    },
    "transcribe": {
      "description": "Run the [speech] stt_command on the speech (invalid_state when not configured).",
      "type": "boolean"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `call.rx_audio`

Fetch audio that was already received on the call, from the receive history (rx_history_secs, at least 5 s). Give last_ms (default 5000) or since_rx_ms [+ until_rx_ms]. Returns {start_rx_ms, end_rx_ms, duration_ms, truncated, level_dbfs{peak,mean}, audio?, transcript?, stt_ms?}.

| Param | Type | Required | Description |
|---|---|---|---|
| `audio` | base64 \| none \| file | no | How to return the audio: base64 (a WAV in audio.base64; the default), none (the default with transcribe), or file (write to `path`). |
| `call_id` | string | yes | Call id (c1, c2, ...). |
| `language` | string | no |  |
| `last_ms` | integer | no | The most recent this many ms (default 5000). Not with since_rx_ms. Inline base64 audio is limited to 120 s: longer ranges need audio:"file" (or "none"). |
| `path` | string | no | Output .wav path for audio:"file" (relative paths: the config file's directory). |
| `sample_rate` | 16000 \| 8000 | no | Sample rate of the returned WAV (default 16000). |
| `since_rx_ms` | integer | no |  |
| `transcribe` | boolean | no |  |
| `until_rx_ms` | integer | no | End of the range (default now). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "audio": {
      "description": "How to return the audio: base64 (a WAV in audio.base64; the default), none (the default with transcribe), or file (write to `path`).",
      "enum": [
        "base64",
        "none",
        "file"
      ],
      "type": "string"
    },
    "call_id": {
      "description": "Call id (c1, c2, ...).",
      "type": "string"
    },
    "language": {
      "type": "string"
    },
    "last_ms": {
      "description": "The most recent this many ms (default 5000). Not with since_rx_ms. Inline base64 audio is limited to 120 s: longer ranges need audio:\"file\" (or \"none\").",
      "maximum": 3600000,
      "minimum": 1,
      "type": "integer"
    },
    "path": {
      "description": "Output .wav path for audio:\"file\" (relative paths: the config file's directory).",
      "type": "string"
    },
    "sample_rate": {
      "description": "Sample rate of the returned WAV (default 16000).",
      "enum": [
        16000,
        8000
      ],
      "type": "integer"
    },
    "since_rx_ms": {
      "minimum": 0,
      "type": "integer"
    },
    "transcribe": {
      "type": "boolean"
    },
    "until_rx_ms": {
      "description": "End of the range (default now).",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

### `speech.transcribe`

Transcribe a WAV file or inline WAV with the [speech] stt_command (invalid_state when not configured). Returns {transcript, stt_ms, duration_ms}.

| Param | Type | Required | Description |
|---|---|---|---|
| `language` | string | no |  |
| `path` | string | no | WAV file (relative paths: the config file's directory). |
| `wav_base64` | string | no | Base64 WAV (at most 10 MiB decoded). Give this or path. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "language": {
      "type": "string"
    },
    "path": {
      "description": "WAV file (relative paths: the config file's directory).",
      "type": "string"
    },
    "wav_base64": {
      "description": "Base64 WAV (at most 10 MiB decoded). Give this or path.",
      "type": "string"
    }
  },
  "required": [],
  "type": "object"
}
```

</details>

### `speech.synthesize`

Turn text into a WAV with the [speech] tts_command (invalid_state when not configured). With `path` the 48 kHz WAV is written there; otherwise audio.base64 carries a 16 kHz WAV. Returns {duration_ms, tts_ms, path|audio}.

| Param | Type | Required | Description |
|---|---|---|---|
| `path` | string | no | Output .wav path (relative paths: the config file's directory). |
| `text` | string | yes |  |
| `voice` | string | no |  |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "path": {
      "description": "Output .wav path (relative paths: the config file's directory).",
      "type": "string"
    },
    "text": {
      "maxLength": 2000,
      "minLength": 1,
      "type": "string"
    },
    "voice": {
      "type": "string"
    }
  },
  "required": [
    "text"
  ],
  "type": "object"
}
```

</details>

### `ui.show`

Show and raise the GUI window (restores the Dock / taskbar presence it gave up while hidden in the tray). Result {visible: true}. Fails with invalid_state `no GUI` when this instance runs headless.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `ui.hide`

Hide the GUI window to the tray / menu bar; the engine keeps running (registrations, calls, control API). Needs `[ui] tray = true`, else invalid_state. Result {visible: false}. Fails with invalid_state `no GUI` when headless.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `ui.state`

GUI window state: {visible, focused, tray (an icon exists), close_to_tray (closing hides), lines_collapsed (the Lines rail is the slim strip), extensions_open (the extensions panel is open; in a narrow window, where the panel is a tab, the saved or session panel choice, false when none)}. Fails with invalid_state `no GUI` when headless.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `ui.set_panels`

Collapse or expand the Lines rail and open or close the extensions panel, as the GUI chevrons do; the choice is remembered across launches (state.toml). Omitted fields are left alone. Both settings are for the wide and medium layouts: in a narrow window they are stored (and applied when the window widens) but the narrow view does not change, and the narrow extensions tab is never affected. Result: the same fields as ui.state's `lines_collapsed` and `extensions_open`, as applied (reported once the GUI processed the request; may lag one frame). Fails with invalid_state `no GUI` when headless.

| Param | Type | Required | Description |
|---|---|---|---|
| `extensions_open` | boolean | no | true = show the extensions panel, false = hide it. |
| `lines_collapsed` | boolean | no | true = slim strip of line badges, false = full Lines list. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "extensions_open": {
      "description": "true = show the extensions panel, false = hide it.",
      "type": "boolean"
    },
    "lines_collapsed": {
      "description": "true = slim strip of line badges, false = full Lines list.",
      "type": "boolean"
    }
  },
  "type": "object"
}
```

</details>

### `ui.attention`

Ask the OS to draw attention to the window (taskbar flash / Dock bounce / urgency hint) until the user focuses it. Result {ok: true}. Fails with invalid_state `no GUI` when headless.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `app.open_uri`

Hand a `sip:`, `sips:`, `tel:`, `callto:` or `siple:` link to the GUI, as the OS URL handler does, and raise the window. Dial links (`sip:`, `sips:`, `tel:`, `callto:`) are validated (URI headers and parameters other than `transport` are dropped; control characters and links over 256 bytes are refused), put into the dial field, and never dial: the user confirms with Call. A `siple:provision` setup link (up to 2048 bytes, the same checks as `provision.parse`) opens the Review screen; nothing is tested or saved until the user reads it, continues, types the password and runs Test, and no parameter skips that. If the user is already past another link's Review or a test runs, the GUI keeps that setup and shows a notice. Result {ok, kind: dial|provision, scheme, target? (dial), next_hop? (provision)}. Errors: invalid_params for a refused link, with data {reason: "invalid_link", code, field?} (the GUI also shows why, rate-limited); invalid_state `rate_limited`; invalid_state `no GUI` when headless.

| Param | Type | Required | Description |
|---|---|---|---|
| `uri` | string | yes | The link, e.g. sip:1001@pbx.example.test, tel:+15551234567 or siple:provision?d=... |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "uri": {
      "description": "The link, e.g. sip:1001@pbx.example.test, tel:+15551234567 or siple:provision?d=...",
      "maxLength": 2048,
      "type": "string"
    }
  },
  "required": [
    "uri"
  ],
  "type": "object"
}
```

</details>

### `provision.parse`

Validate a `siple:provision` setup link without any side effect: no network, no file is written, no window opens (`app.open_uri` opens the GUI's Review screen). Result {version, account (the fields the link sets), draft {id, fields}, next_hop (the host the account would connect to), warnings [{code, message}] (server_outside_domain, ip_literal, loopback, punycode, srtp_off), existing (the id of a configured account the link would duplicate, else null)}. `draft` is accepted as-is by `config.edit` `add_account`, and by `account.test` once `password` is added to `draft.fields`. A link carries only these account fields: username, domain, display_name, auth_username, registrar, outbound_proxy, transport, srtp, profile; `username` and `domain` are required. `display_name` may not contain quotes, backslashes, angle brackets or invisible characters, and a host must be a name or a full IPv4 address (not shorthand such as 127.1). It never carries a password; a `password` key, any other config key and any unknown key are refused. Links are at most 2048 bytes. Errors: invalid_params with data {reason: "invalid_link", code, field?}; code is one of too_long, not_a_setup_link, unsupported_action, bad_query, bad_encoding, bad_json, newer_version, bad_version, password_not_allowed, field_not_allowed, unknown_field, missing_field, invalid_field.

| Param | Type | Required | Description |
|---|---|---|---|
| `link` | string | yes | The setup link, e.g. siple:provision?d=eyJ2IjoxLC4uLn0. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "link": {
      "description": "The setup link, e.g. siple:provision?d=eyJ2IjoxLC4uLn0.",
      "maxLength": 2048,
      "type": "string"
    }
  },
  "required": [
    "link"
  ],
  "type": "object"
}
```

</details>

### `provision.build`

Build a `siple:provision` setup link, from a configured account (`account`: its id) or from `fields` (an object with the allowed fields, as in `provision.parse`'s `account`). The link never contains a password; `fields` with a `password` key is refused (password_not_allowed). Result {link, length, fields (what the link carries), omitted (account keys set in the config file that the link does not carry, such as password, password_store, tls_fingerprint; only with `account`)}. Links contain the user name and server names, so treat them as semi-private. Errors: not_found, invalid_params with data {reason: "invalid_link", code, field?} (for example invalid_field for a saved value a link cannot express).

| Param | Type | Required | Description |
|---|---|---|---|
| `account` | string | no | Id of a configured account. |
| `fields` | object | no | Link fields: username, domain, display_name, auth_username, registrar, outbound_proxy, transport, srtp, profile (all strings). |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "oneOf": [
    {
      "required": [
        "account"
      ]
    },
    {
      "required": [
        "fields"
      ]
    }
  ],
  "properties": {
    "account": {
      "description": "Id of a configured account.",
      "type": "string"
    },
    "fields": {
      "description": "Link fields: username, domain, display_name, auth_username, registrar, outbound_proxy, transport, srtp, profile (all strings).",
      "type": "object"
    }
  },
  "type": "object"
}
```

</details>

### `app.update_status`

Whether a newer Siple release exists, as of the last check (no network access). Result {enabled (the check is on: `[updates] check`, and there is a source for this build and package), channel (`stable`|`prerelease`, the one in effect; the stores always report `stable`), current (the running version, `0.2.0-rc.4` for a release candidate), latest? (newest release the channel allows, once a check found one; absent for the Microsoft Store, which does not tell a version), available (latest is newer than current; Microsoft Store: an update is waiting), source (`github`|`app_store`|`microsoft_store`|`none`), action (what the offer does: `install` (direct builds that can install in place: `app.install_update`)|`open_release_page`|`open_store`|`none` while nothing is available), release_url? (GitHub source), store_url? (store sources, while an update is available; opening it is a GUI action), disabled_reason? (why source is `none`: `direct_build_in_package`|`store_build_outside_store`|`sideloaded`), mandatory? (Microsoft Store: the waiting update is mandatory), published_at?, checked_at? (last successful check, RFC 3339), error? (why the last check failed; cleared by the next success), install? (direct builds: {possible, reason? (`disabled`|`headless`|`release_not_signed`|`no_asset`|`unsupported_platform`|`unsigned_build`|`not_bundle`|`not_writable`|`package_manager`|`container`|`translocated`|`read_only_volume`|`not_owner`|`root`|`system_install`|`sandboxed`|`in_use`), detail?, state (`idle`|`downloading`|`verifying`|`ready`|`waiting`|`committing`|`installed`|`restarting`|`failed`), version?, bytes?, total?, restart? (`when_idle`|`now`|`manual`), error?, error_kind?})}. The source follows the distribution channel at run time. Direct builds: one anonymous HTTPS GET of the project's GitHub releases list. Mac App Store: one anonymous HTTPS GET of Apple's lookup of Siple's listing. Microsoft Store: Siple asks Windows' Store service about its own package (Windows contacts Microsoft; Siple sends nothing). The two HTTPS requests carry `User-Agent: Siple/<version>` and nothing else that identifies the install. Checking downloads nothing. A newly found version is announced once as the event `app.update_available` {version (null for the Microsoft Store), url (release page or store page), action}.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `app.check_updates`

Checks for a newer release now and returns the result of `app.update_status`. A check that ran less than 60 s ago (or a rate-limit back-off in progress) is not repeated: the last result comes back, with `error` set in the second case. Failures never raise an API error; they are reported in `error`. Does nothing while `[updates] check = false` or when `source` is `none`. Result {enabled (the check is on: `[updates] check`, and there is a source for this build and package), channel (`stable`|`prerelease`, the one in effect; the stores always report `stable`), current (the running version, `0.2.0-rc.4` for a release candidate), latest? (newest release the channel allows, once a check found one; absent for the Microsoft Store, which does not tell a version), available (latest is newer than current; Microsoft Store: an update is waiting), source (`github`|`app_store`|`microsoft_store`|`none`), action (what the offer does: `install` (direct builds that can install in place: `app.install_update`)|`open_release_page`|`open_store`|`none` while nothing is available), release_url? (GitHub source), store_url? (store sources, while an update is available; opening it is a GUI action), disabled_reason? (why source is `none`: `direct_build_in_package`|`store_build_outside_store`|`sideloaded`), mandatory? (Microsoft Store: the waiting update is mandatory), published_at?, checked_at? (last successful check, RFC 3339), error? (why the last check failed; cleared by the next success), install? (direct builds: {possible, reason? (`disabled`|`headless`|`release_not_signed`|`no_asset`|`unsupported_platform`|`unsigned_build`|`not_bundle`|`not_writable`|`package_manager`|`container`|`translocated`|`read_only_volume`|`not_owner`|`root`|`system_install`|`sandboxed`|`in_use`), detail?, state (`idle`|`downloading`|`verifying`|`ready`|`waiting`|`committing`|`installed`|`restarting`|`failed`), version?, bytes?, total?, restart? (`when_idle`|`now`|`manual`), error?, error_kind?})}. The source follows the distribution channel at run time. Direct builds: one anonymous HTTPS GET of the project's GitHub releases list. Mac App Store: one anonymous HTTPS GET of Apple's lookup of Siple's listing. Microsoft Store: Siple asks Windows' Store service about its own package (Windows contacts Microsoft; Siple sends nothing). The two HTTPS requests carry `User-Agent: Siple/<version>` and nothing else that identifies the install. Checking downloads nothing. A newly found version is announced once as the event `app.update_available` {version (null for the Microsoft Store), url (release page or store page), action}.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `app.install_update`

Downloads, verifies and installs the offered Siple release in place, then restarts into it (direct downloads on macOS, Windows and Linux; the Mac App Store and Microsoft Store editions report `unsupported`: the store updates them). Returns at once with the result of `app.update_status` (its `install` object is the progress); the work runs in the background and is reported by the events `app.update_progress` {version, state, bytes?, total?, error?, reason?} (at most twice a second) and, after the restart, `app.updated` {from, to}. The request is refused unless `[updates] check` and `[updates] install` are on and `version` equals the `latest` of `app.update_status`. What is downloaded: the release's checksum list, its minisign signature and the archive for this system, from the project's GitHub release (redirected to GitHub's asset hosts), each over HTTPS with `User-Agent: Siple/<version>`; the checksum list must carry a valid minisign signature of Siple's release key (and name this release), the archive must match its listed SHA-256, and the new app must pass the operating system's code-signature check of the vendor (macOS: Developer ID and notarization; Windows: Authenticode with the pinned signer) before anything is replaced. Nothing is replaced, and Siple is never restarted, while a call, a transfer or an echo test is active: `when_idle` waits for 5 s of quiet and `now` is refused with `call_active`. Never elevates: a copy that is not writable by the user, is managed by a package manager, runs from a disk image or container, or is a headless instance cannot install (`status.install.reason`) and the offer opens the release page instead. Errors: invalid_state with data.reason `disabled`, `not_offered` {latest}, `install_not_possible` {why, detail, release_url}, `busy` (another version is being installed), `call_active`; `unsupported` in store editions. Calling it again for the version being installed only changes `restart`; after a failure it retries. A GUI shows the pending install (and a notice with Cancel when an agent started it).

| Param | Type | Required | Description |
|---|---|---|---|
| `restart` | when_idle \| now \| manual | no | When to swap the files in and restart: `when_idle` (default) once no call has been active for 5 s, `now` immediately (fails with `call_active` while a call is active), `manual` after the download is verified, waiting for a later call with `when_idle` or `now`. |
| `version` | string | yes | The version to install; must equal `latest` of `app.update_status`. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "restart": {
      "description": "When to swap the files in and restart: `when_idle` (default) once no call has been active for 5 s, `now` immediately (fails with `call_active` while a call is active), `manual` after the download is verified, waiting for a later call with `when_idle` or `now`.",
      "enum": [
        "when_idle",
        "now",
        "manual"
      ],
      "type": "string"
    },
    "version": {
      "description": "The version to install; must equal `latest` of `app.update_status`.",
      "type": "string"
    }
  },
  "required": [
    "version"
  ],
  "type": "object"
}
```

</details>

### `app.cancel_update`

Cancels an update install that has not swapped the files yet (downloading, verifying, ready, waiting for the calls to end). Result {cancelled (false when nothing was running), install (the `install` object of `app.update_status`)}. Once the new files are in place (`installed`, `restarting`) it fails with invalid_state, data.reason `too_late`. Editions from a store: unsupported.

Params: none.

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {},
  "type": "object"
}
```

</details>

### `call.stream`

Upgrades this control connection to a binary duplex audio stream of one call (4-byte header type&lt;&lt;24|len; 0x01 audio, 0x02 JSON). Last JSON line is the result; use `siple-cli stream` or ControlClient::into_stream.

| Param | Type | Required | Description |
|---|---|---|---|
| `call_id` | string | yes |  |
| `encoding` | s16le \| f32le \| mulaw \| alaw | no |  |
| `frame_ms` | 10 \| 20 \| 40 \| 60 | no |  |
| `hangup_on_close` | boolean | no |  |
| `rx` | boolean | no |  |
| `sample_rate` | 8000 \| 16000 \| 24000 \| 48000 | no |  |
| `tx` | boolean | no |  |
| `tx_buffer_ms` | integer | no | TX jitter buffer in ms. Accepted 40..10000 but the effective maximum is 2000 (the ring size); the result line reports the effective value. |

<details><summary>JSON Schema</summary>

```json
{
  "additionalProperties": false,
  "properties": {
    "call_id": {
      "type": "string"
    },
    "encoding": {
      "default": "s16le",
      "enum": [
        "s16le",
        "f32le",
        "mulaw",
        "alaw"
      ],
      "type": "string"
    },
    "frame_ms": {
      "default": 20,
      "enum": [
        10,
        20,
        40,
        60
      ],
      "type": "integer"
    },
    "hangup_on_close": {
      "default": false,
      "type": "boolean"
    },
    "rx": {
      "default": true,
      "type": "boolean"
    },
    "sample_rate": {
      "default": 16000,
      "enum": [
        8000,
        16000,
        24000,
        48000
      ],
      "type": "integer"
    },
    "tx": {
      "default": true,
      "type": "boolean"
    },
    "tx_buffer_ms": {
      "default": 2000,
      "description": "TX jitter buffer in ms. Accepted 40..10000 but the effective maximum is 2000 (the ring size); the result line reports the effective value.",
      "maximum": 10000,
      "minimum": 40,
      "type": "integer"
    }
  },
  "required": [
    "call_id"
  ],
  "type": "object"
}
```

</details>

