# JSON-RPC basics

The control API is JSON-RPC 2.0, one JSON object per line (UTF-8, no batches), over a local socket. This page is the short
version; the [API reference](/agents/api-reference) has everything.

## Connect

| Platform | Endpoint |
|---|---|
| macOS, Linux | Unix domain socket, default `<config dir>/control.sock` (mode 0600) |
| Windows | Named pipe, default `\\.\pipe\siple-<user>-<hash of config dir>` (current user only) |
| Any, optional | Loopback TCP, `[control] tcp_listen = "127.0.0.1:5079"`, off by default, token always required |

The actual endpoints and the per-run token are in `runtime.json` in the config directory. The `siple-cli` subcommands read it
for you.

## Say hello first

The first message on every connection must be `hello`:

```json
{"jsonrpc":"2.0","id":1,"method":"hello","params":{"token":"<from runtime.json>","client":"my-script"}}
```

The token is optional on the socket or pipe and required on TCP. The reply has an `instance_id`; if it changes, Siple restarted
and event sequence numbers and call ids (`c1`, `c2`, ...) start over.

## Call methods

```sh
siple-cli ctl status
siple-cli ctl call.dial '{"account":"desk","target":"9999","source":"silence","sink":"null"}'
siple-cli ctl call.hangup '{"call_id":"c1"}'
siple-cli events --kinds 'call.*,account.*'
siple-cli methods                # the catalog, offline
```

Errors are standard JSON-RPC errors whose `error.data.kind` is machine-readable: `not_found`, `invalid_state`, `unauthorized`,
`sip_error`, `timeout`, `invalid_params`, and so on. See [Errors](/agents/api-reference#errors).

## Wait without races

Every state-changing result carries `event_seq`, the event-log position before the action. Wait with `since = event_seq` and
no consequence of the action can be missed:

```text
call.dial {account:"desk", target:"9999"}              -> {call_id:"c1", event_seq:120}
call.wait_for {call_id:"c1", state:"active", since:120, timeout_ms:15000}
```

Use `events.poll` for anything else and chain `next_seq` between polls. Details:
[Race-free waiting](/agents/api-reference#race-free-waiting).

## Next

- [Events](/agents/api-reference#events) and their kinds.
- [Audio for agents](/agents/api-reference#audio-for-agents).
- [Method reference](/agents/api-reference#method-reference).
