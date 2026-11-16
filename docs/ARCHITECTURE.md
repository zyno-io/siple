# Architecture

How Siple is put together. For user-facing behaviour see [CONFIG.md](CONFIG.md) and [API.md](API.md).

## Crate map

```
siple (GUI)  ─┐
siple-cli    ─┴─►  siple-core  ─►  siple-sip  ─►  vendor/rsipstack
                       │               │
                       └───────────────┴─►  siple-media     (pure, no I/O)
                       └───────────────────►  siple-config   (schema, edit, paths)
```

| Crate | Role |
|---|---|
| `siple-config` | Config schema, parsing, validation, default paths, vendor profiles (`generic`, `yealink`, `polycom`, `cisco`, `grandstream`), typed `edit` (comment-preserving `toml_edit` writer), log-level parsing, one-time legacy-directory migration. No async. |
| `siple-media` | Sans-I/O media primitives: RTP/RTCP, SRTP (SDES), Opus/G.722/G.711, DTMF, tone generation and Goertzel detection, VAD, jitter buffer with PLC, SDP offer/answer negotiation. No sockets, no tokio; callers own pacing and transport. |
| `siple-sip` | SIP on top of rsipstack: a `SipStack` registry of per-account actors, registration state machine, calls (inbound/outbound, early media, hold, session timers, PRACK, transfers and transfer sessions, line policy), BLF/MWI/generic subscriptions, directed pickup, DNS (SRV), wire tap, and the network monitor (`net/`). Media is reached through the `MediaProvider` trait, so the crate has no audio dependency. |
| `siple-core` | The engine, `App`. Owns the stack, media engine, audio devices, event log, config watcher and actor, control server, logging, SIP trace writer, agent audio. Implements the control API (`AppBackend`). |
| `siple` | GUI binary (eframe/egui on wgpu). Also runs `--headless`. Reads engine state through a polled `Snapshot` and acts through thin wrappers (`actions.rs`). |
| `siple-cli` | Console binary: `run --headless`, `ctl`, `events`, `stream`, `mcp`, `methods`, `config-example`. The MCP bridge is hand-rolled (stdio JSON-RPC) and proxies to a running instance. |
| `vendor/rsipstack` | Upstream rsipstack 0.7.3 with Siple patches (per-connection TLS verifier, wire tap, `use_sips_for_tls`, SNI override, ...). Every change is marked `// siple:` and listed in `vendor/rsipstack/SIPLE_PATCHES.md`. Excluded from the workspace: never run `cargo fmt --all`. |

## Process, threads and actors

One process per instance, guarded by a lock file (`siple.lock`) in the config directory.

- **tokio runtime** (multi-thread) runs everything that is I/O: SIP, the control server, config watching, timers,
  one task per call receiving RTP.
- **Account actors** (`siple-sip/src/account.rs`): one tokio task per enabled account owns that account's rsipstack
  endpoint (transport layer + dialog layer), registration state machine, keep-alives and incoming-request router.
  All mutation goes through the actor's command channel; readers use a status watch. Calls are tracked by a shared
  `CallManager` and driven through the `SipStack` API.
- **Media thread** (`siple-core/src/media/thread.rs`): one dedicated `std::thread` ("siple-media", real-time priority via
  `audio_thread_priority`) does all per-frame work: capture resample, encode, SRTP protect, UDP send, jitter pull, decode,
  mix, playback resample. It is clocked by the audio device (pull model). Per-call tokio tasks only receive datagrams,
  unprotect SRTP and hand packets over a bounded channel. Public handles (`MediaEngine`, `CallMediaHandle`) are
  non-blocking command senders.
- **Audio device thread** (`audio/device_thread.rs`): owns the cpal streams; device lists and permission state are polled
  once a second by an engine task. `--no-audio` and `source`/`sink` of `silence`, `tone`, `file`, `null` or `agent` need no device.
- **Engine tasks** started by `App::start`: SIP-to-events bridge, media-to-events bridge, audio poll, the config actor and
  the config file watcher.
- **GUI**: egui runs on the main thread. A poller task refreshes a `Snapshot` about every 250 ms (immediately when the event
  log signals activity) and requests a repaint only if something changed. UI actions spawn onto the tokio runtime and report
  failures as a notice. The GUI holds the engine weakly so it never keeps `App` alive during shutdown.
- **Network monitor threads**: one OS-specific watcher thread (see below).

All engine state changes are published on the **event log** (`events.rs`): a sequence-numbered ring plus a broadcast
channel, consumed by long-poll (`events.poll`, the primary path for MCP) and streaming subscribers.

## Configuration and hot reload

`config.toml` is the single home of settings (UI choices are written back into it). Reload is serialised by one task, the
**ConfigActor** (`run_reload_actor` in `crates/siple-core/src/reconfig.rs`):

- Triggers: the file watcher (`config_watch.rs`: non-recursive watch of the parent directory so write-temp-then-rename saves
  work, filtered by name, 500 ms debounce, SHA-256 content hash to skip no-op saves), SIGHUP, `config.reload`, and typed
  `config.edit` requests. Because one task applies everything, an apply is never interleaved with another and is not cut short by a
  disconnecting client.
- An apply loads and validates the file, **diffs the effective config against the applied one**, then applies the difference per
  class: hot, next call, re-register, rebuild (deferred while the account has calls, unless `[general] reconfig_force`), rebind
  (control listeners) or restart. An invalid file never replaces the last good config; it emits `config.error`.
- `config.edit` (`siple-config/src/edit.rs`) changes exactly the named keys with `toml_edit`, checks an optional `base_hash` for
  conflicts, re-validates, writes atomically (temp file, fsync, rename), then triggers the reload and returns the report. The
  watcher then sees an unchanged hash. Accounts are matched by `id`.

The class table is in [CONFIG.md](CONFIG.md#reload-classes).

## Control API and MCP

- **Transport** (`siple-core/src/control/`): JSON-RPC 2.0, newline-delimited, over a per-user Unix socket (macOS/Linux) or named
  pipe with a current-user DACL (Windows), optionally loopback TCP with a mandatory token. Endpoints and the per-run token are
  published in `runtime.json` (mode 0600). The first message must be `hello`.
- **One catalog.** `method_catalog()` in `crates/siple-core/src/backend.rs` is the single source: the control server's built-ins
  (`hello`, `ping`, `methods.list`, `events.*`, `logs.tail`) plus `backend_methods()` with JSON schemas. It needs no running
  instance. Three things are generated from it:
  1. `methods.list` on a live instance,
  2. the MCP tool list (`siple-cli mcp`, which serves the static catalog when no instance is running and sends
     `tools/list_changed` once a live list differs),
  3. the method reference in `docs/API.md` (`scripts/gen-api-docs.sh`).
- **`AppBackend`** implements `ControlBackend` over `App`. Method families live next to their feature (`calls.rs`,
  `subscriptions.rs`, `agent/methods.rs`, `reconfig.rs`).
- **Parity rule.** The GUI, JSON-RPC and MCP must reach the same behaviour, so behaviour lives in the engine and a new feature
  ships with its method, events and MCP tool (see [CONTRIBUTING.md](../CONTRIBUTING.md)).
- **MCP bridge** (`siple-cli/src/mcp.rs`): requests run concurrently on their own control connection with time limits; only
  idempotent reads are retried after a lost connection; if the instance restarted (`instance_id` changed) the next result says so.
- **Agent audio** (`siple-core/src/agent/`): a per-call RX hub (timeline, history, VAD jobs), WAV/PCM decoding, optional external
  TTS/STT programs from `[speech]` (never via a shell), and the binary `call.stream` upgrade.

## Media engine

SDP is negotiated in `siple-media` (`sdp_neg`): codec order, DTMF payload, SRTP suite (`optional` offers RTP/AVP plus `a=crypto`
and the stack retries once with plain RTP on 488/415/606). `siple-core/src/call_media.rs` implements the SIP layer's
`MediaProvider`: it allocates one engine call per SIP call from the *current* `[media]` settings and records the handle in a
registry keyed by call id, which the API uses for `call.play`, recording and tone detection. Per call, a rx task feeds the media
thread; the thread mixes sources (device, tone, file, agent stream, silence) and sinks (device, null, recorder, agent hub).
Port allocation honours `[media] rtp_port_range`.

## Network-change handling

`crates/siple-sip/src/net/` (`NetService`, created by `SipStack`) turns three signals into debounced events with a monotonically
increasing generation:

1. **OS address/route events** from a `NetMonitor`: macOS SystemConfiguration (`os_macos.rs`), Windows `NotifyIpInterfaceChange` and
   friends (`os_windows.rs`), Linux rtnetlink (`os_linux.rs`, raw libc, no extra libraries). If a monitor cannot start, the service
   degrades to probing only.
2. **Sleep/wake**, detected portably by a 5 s ticker that sees a wall-clock jump beyond 20 s.
3. **A periodic route probe** (`probe_secs`): which source address would the OS choose; every account also re-probes its own next hop.

Triggers coalesce for `debounce_ms` (sliding, capped at 4x) and are published as `NetEvent::Changed` or `NetEvent::Probe`. Each
account actor reacts by re-probing; on a changed path it re-registers (and re-subscribes BLF/MWI), on no path it sits in
`WaitingForNetwork`. `network.refresh` raises the same signal manually. OS access sits behind the `NetMonitor`, `NetProbe` and
`WallClock` traits so the logic is unit-tested with fakes. Interface filtering (ignoring tunnels and the like) is in
`ignored_interface` and `relevant_addr`.

## Logging and secrets

`siple-core/src/logging.rs` sets up the application log (daily files, retention, runtime level changes). The per-account wire
trace (`sip_trace.rs`) is fed by rsipstack's `WireTap` and writes one file tree per account; the tap is installed only while
`sip_trace` is on. `redact.rs` masks credentials and SRTP keys when `[logging] redact_credentials` / `redact_srtp_keys` are set.
Plaintext passwords are never logged. There is no telemetry.

## Testing

Most tests run in-process against peer-mode accounts on loopback (UDP/TCP/TLS) with no PBX. Gated suites: PBX interop
(`SIPLE_INTEROP=1`), soak (`SIPLE_SOAK=1`) and sipp scenarios (`SIPLE_SIPP=1`, `tests/sipp/`). GUI snapshots use `egui_kittest`
with wgpu and reference images in `crates/siple/tests/snapshots/` (macOS/Metal).
