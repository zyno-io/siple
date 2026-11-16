# Siple patches to rsipstack 0.7.3

Every change site is marked with a `// siple:` comment (grep for it). Line numbers are approximate.

## 1. Per-connection TLS server cert verifier
- `src/transport/tls.rs` ~L225-255: `TlsConfig.verifier: Option<Arc<dyn ServerCertVerifier>>`; `Debug` is now manual (verifier printed as presence only, key material redacted). `Clone`/`Default` unchanged.
- `src/transport/tls.rs` ~L659 (`TlsConnection::connect`): explicit `custom_verifier` arg wins, else `TlsConfig.verifier`. The verifier replaces root-store verification entirely (`dangerous().set_certificate_verifier`), so an empty root store is fine. SNI still comes from `sni_hostname` or the target host.
- `src/transport/transport_layer.rs` ~L430: lookup passes `effective_config.verifier` to `connect`.
- Tests: `src/transport/tests/test_tls_verifier.rs` (self-signed local rustls server; accept-all succeeds incl. SNI check, no verifier fails, Debug). Uses the existing openssl-CLI cert helper (no rcgen dev-dep).
- Rationale: pin/skip-verify per account for test PBXes with self-signed certs.

## 2. WireTap (raw wire-level trace)
- New `src/transport/wire_tap.rs`: `WireTap`, `WireEvent`, `WireDirection`, `WireTapSlot` (shared Arc<RwLock<Option<Arc<dyn WireTap>>>>). Re-exported from `transport`.
- `TransportLayer::set_wire_tap(Arc<dyn WireTap>)` / `clear_wire_tap()` (transport_layer.rs ~L258). Applied to existing listeners/connections immediately and to anything added later via `add_listener`/`add_connection` (outbound lookup, accepted TCP/TLS). `SipConnection::set_wire_tap` (connection.rs ~L206) for per-connection use.
- UDP (`udp.rs`): tap on every received datagram right after whitelist and before keepalive/parse handling; on send in `send`, `send_raw`, and the keepalive CRLF reply.
- Streams (`stream.rs`): `StreamConnectionInner.wire_tap`; `send_message` serializes once and taps; `send_raw` taps; `SipCodec` has an optional frame hook that fires per framed message / keepalive before parse; on a framing error the unconsumed buffer is tapped.
- Stream receive events are per framed message (not per TCP read). Not covered: WebSocket, Channel, bytes dropped by the whitelist.
- Cost when no tap is installed: one uncontended RwLock read per message.
- Tests: `src/transport/tests/test_wire_tap.rs`.

## 3. `use_sips_for_tls`
- `EndpointOption.use_sips_for_tls: bool` (default `true` = upstream), endpoint.rs ~L59/77.
- `EndpointInner::sip_addr_to_uri()` (endpoint.rs ~L567): `SipAddr` -> `Uri`, downgrading `sips:` to `sip:` for TLS when flag is false (`;transport=tls` is already present). Used by `get_record_route_with_addr` and the dialog `remote_uri` seed (invitation.rs ~L882).
- `DialogLayer::build_local_contact` (dialog_layer.rs ~L537) and `make_invite_request` Contact (invitation.rs ~L482) are gated by the flag.
- REGISTER already used `sip:` upstream (registration.rs hardcodes `Scheme::Sip`), unchanged.
- Test: `test_use_sips_for_tls_disabled_keeps_sip_scheme` in `src/dialog/tests/test_dialog_layer.rs`.

## Per-destination SNI override (`sni_by_addr`)
- `src/transport/tls.rs` ~L245: `TlsConfig.sni_by_addr: HashMap<SocketAddr, String>` (included in the manual `Debug`; `Clone`/`Default` unchanged).
- `src/transport/transport_layer.rs` `lookup` (TLS branch, ~L426): when the resolved destination (IP host + port, default 5061) has an entry, its value becomes the SNI / verification name for that connection; otherwise upstream behaviour (`sni_hostname` if set, else the original domain, else the IP).
- Rationale: Siple sets `tls_server_name` / the registrar host as the SNI of the registrar (or outbound-proxy) connection only; with the layer-wide `sni_hostname` that name was pinned onto every TLS target (calls to other hosts, OPTIONS to peers).
- Test: `test_sni_by_addr_applies_only_to_listed_destination` in `src/transport/tests/test_tls_verifier.rs`.

## `Transaction::receive` ends when the endpoint is cancelled
- `src/transaction/transaction.rs` `receive` (~L760): the wait for the next transaction event races the endpoint's cancel token (only the cancel-safe `tu_receiver.recv()` is raced; queued events are still delivered first) and returns `None` once it is cancelled. `src/transaction/endpoint.rs`: `EndpointInner::cancelled()` accessor.
- Rationale: transactions end through timers, and the timers stop with the endpoint. Tasks draining a transaction (e.g. the Timer M drain spawned after a 2xx to INVITE in `do_invite_async`) then waited forever, holding the transport connection and endpoint: a removed / rebuilt account leaked its UDP socket (fixed `local_port` -> EADDRINUSE) after any call.
- Test: `src/transaction/tests/test_cancel_receive.rs`.

## Last remote CANCEL on server INVITE dialogs
- `src/dialog/dialog.rs`: `DialogInner.remote_cancel: Mutex<Option<Request>>` (both constructors). `src/dialog/invite_dialog.rs` (`InviteDialog::last_remote_cancel()`, ~L75; recorded in the `Method::Cancel` arm of `handle_invite` ~L963) and `src/dialog/server_dialog.rs` (same arm ~L911): the CANCEL request is stored right before `Terminated(UacCancel)` is notified, like `remote_ack`.
- Rationale: the CANCEL is consumed by the INVITE server transaction, so its `Reason` header (RFC 3326, `SIP;cause=200;text="Call completed elsewhere"`) never reached the application; Siple uses it to tell "answered elsewhere" from "missed".
- Test: `src/dialog/tests/test_cancel_reason.rs`.

## Phase 12 (100rel as UAS, session timers): no vendor changes
Reliable 18x are ordinary provisional responses sent through `InviteDialog::ringing()` (extra `Require`/`RSeq` headers; a retransmission is the same call again), and PRACK is matched on `RAck` by Siple before the dialog layer sees it (`crates/siple-sip/src/calls/reliable.rs`); session-timer refreshes use `InviteDialog::update()` / `reinvite()` (`calls/timer.rs`). rsipstack's own `handle_prack` (always 200) is therefore never reached for inbound calls.

## ACK routes of a 2xx are registered before the 2xx is sent
- `src/transaction/transaction.rs` `respond`: for a server INVITE answered with a 2xx, `waiting_ack` / `waiting_ack_cseq` are inserted before `connection.send` (and removed again when the send fails); the Accepted transition inserts the same entries again (idempotent).
- Rationale: they used to be registered only on entering Accepted, after the send. An ACK arriving in that gap got its own transaction key and was dropped (`endpoint.rs`, unknown ACK), so the dialog stayed unconfirmed until a 2xx retransmission was ACKed again.
- Test: `test_server_invite_ack_right_after_2xx_is_delivered` in `src/transaction/tests/test_server_invite_ack.rs` (a wire tap sends the ACK from inside the 2xx send; fails without the early registration). Run it from `vendor/rsipstack` (`cargo test --lib`); the crate is not a workspace member.

## Network change: swap a running listener, retire stream flows, mutable dialog Contact (Network B2)
- `src/transport/transport_layer.rs`: `TransportLayer::swap_listener(old: Option<&SipAddr>, new: SipConnection)` adds `new` to the lookup list **and starts its receive loop** (`add_transport` only takes effect for listeners present when `Endpoint::serve` starts; `serve_listener` is `pub(super)`), then removes `old`. New transactions pick `new` from the moment it returns (lookup and `get_via` read the live list); in-flight transactions keep their connection. Stopping the old socket's loop is the caller's job (cancel the token it passed to `UdpConnection`). `TransportLayer::retire_stream_connections()` retires every cached TCP/TLS/WS connection (cancel token + removed from the cache, via the existing `retire_connection`), so the next lookup dials fresh from the new local address and dialogs stop choosing the dead flow for affinity. Each retired connection is also `close()`d in the background (cancelling its token only marks the flow; the read task and socket would otherwise live until the runtime is dropped).
- `src/dialog/dialog.rs`: `DialogInner.local_contact` is now `Mutex<Option<Uri>>` (was `Option<Uri>`); the four readers (`make_request_with_vias`, `make_response`, the forked-branch BYE, the snapshot) lock it, the two constructors wrap it. `src/dialog/invite_dialog.rs`: `InviteDialog::set_local_contact(Uri)`, `local_contact()`, and `clear_flow_affinity()` (forgets the recorded server connection, which would otherwise pin in-dialog requests to a dead TCP/TLS flow until its token is cancelled).
- Rationale: a network change moves the local address under live calls. Siple rebinds the UDP socket in place, republishes the Contact and sends a re-INVITE per call instead of tearing the endpoint (and every dialog) down.
- Test: `src/dialog/tests/test_swap_listener.rs` (the re-INVITE after a swap leaves from the new socket with the new Via and Contact; a request sent to the new socket reaches the endpoint).
- `TransportLayer::reactivate_listener(old, new)` (follow-up): re-installs a swapped-out listener whose receive loop still runs (lookup list only, no second loop). Used when the network flaps back to an address whose old socket still lingers.

## Credential guard (`Credential.guard`)
- `src/dialog/authenticate.rs`: `Credential.guard: Option<AuthGuard>` (`AuthGuard(Arc<dyn Fn(&Transaction, &Response) -> Result<(), String>>)`). `handle_client_authenticate` (the single funnel for INVITE, in-dialog requests, REGISTER and siple's own client transactions) calls it first; `Err(msg)` returns `Error::DialogError(msg, id, <401|407>)` without computing any digest.
- Every `Credential { .. }` literal (docs, README, tests) gained `guard: None`.
- Rationale: siple only answers a challenge when the transaction's actual destination (`Transaction::destination` after `send`: the resolved / connected peer address) is one of the account's provider addresses. The decision needs the per-request destination, so it cannot be made once at dialog creation; request-URI, Contact and Route are peer-controlled and are not consulted.
