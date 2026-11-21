# Siple patches to rsipstack 0.8.6

Base: the crates.io 0.8.6 package (upstream tag `v0.8.6`, commit `6c6dacf`). Vendored files: the
package's `Cargo.toml` (from `Cargo.toml.orig`), `LICENSE`, `README.md`, `src/`, `.gitignore` and
`.cargo_vcs_info.json`. The package's `tests/` (PJSIP / SIPp interop harness), `scripts/` and
`CHANGELOG.md` are not vendored, as `examples/` was not before.

Every change site is marked with a `// siple:` comment (grep for it). Line numbers are approximate.

Run the crate's tests from `vendor/rsipstack` (`cargo test --lib`, default features; it is not a
workspace member, so `cargo test -p rsipstack` from the root refuses its dev-dependencies). Some
upstream tests need the `websocket` feature, so they do not build with Siple's feature set.

## `Cargo.toml`
- The `[[example]]` entries `interop_proxy` / `interop_b2bua` are removed: they have no `path`,
  `examples/` is not vendored, and cargo then rejects the manifest. Nothing else differs from upstream.

## 1. Per-connection TLS server cert verifier
- `src/transport/tls.rs` ~L237-277: `TlsConfig.verifier: Option<Arc<dyn ServerCertVerifier>>`; `Debug` is now manual (verifier printed as presence only, key material redacted). `Clone`/`Default` unchanged.
- `src/transport/tls.rs` ~L648 (`TlsConnection::connect`): explicit `custom_verifier` arg wins, else `TlsConfig.verifier`. The verifier replaces root-store verification entirely (`dangerous().set_certificate_verifier`), so an empty root store is fine; upstream's "no CA certificates configured" warning (0.7.5) is skipped when a verifier is set. SNI still comes from `sni_hostname` or the target host. The platform-connector path (embedded backends) ignores the verifier.
- `src/transport/transport_layer.rs` ~L670: lookup passes `effective_config.verifier` to `connect`.
- Tests: `src/transport/tests/test_tls_verifier.rs` (self-signed local rustls server; accept-all succeeds incl. SNI check, no verifier fails, Debug). Uses upstream's openssl-CLI cert helper `test_tls_reload::generate_test_cert` (upstream made it `pub(super)`; no rcgen dev-dep).
- Rationale: pin/skip-verify per account for test PBXes with self-signed certs.

## 2. WireTap (raw wire-level trace)
- New `src/transport/wire_tap.rs`: `WireTap`, `WireEvent`, `WireDirection`, `WireTapSlot` (shared Arc<RwLock<Option<Arc<dyn WireTap>>>>). Re-exported from `transport`.
- `TransportLayer::set_wire_tap(Arc<dyn WireTap>)` / `clear_wire_tap()` (transport_layer.rs ~L387). Applied to existing listeners/connections immediately and to anything added later via `add_listener`/`add_connection` (outbound lookup, accepted TCP/TLS). `SipConnection::set_wire_tap` (connection.rs ~L212) for per-connection use.
- UDP (`udp.rs`): tap on every received datagram right after whitelist and before keepalive/parse handling; on send in `send` (this includes upstream's stateless 400 to a malformed request), `send_raw`, and the keepalive CRLF reply.
- Streams (`stream.rs`): `StreamConnectionInner.wire_tap`; `send_message` serializes once and taps; `send_raw` taps; `SipCodec` has an optional frame hook that fires per framed message / keepalive (since 0.8 the head is parsed before the frame is complete, so a message is tapped once its whole frame is in); on a framing error (bad head, missing Content-Length, oversize) the unconsumed buffer is tapped.
- Stream receive events are per framed message (not per TCP read). Not covered: WebSocket, Channel, TlsPlatform (embedded), bytes dropped by the whitelist.
- Cost when no tap is installed: one uncontended RwLock read per message.
- Tests: `src/transport/tests/test_wire_tap.rs`.

## 3. `use_sips_for_tls`
- `EndpointOption.use_sips_for_tls: bool` (default `true` = upstream), endpoint.rs ~L59/77.
- `EndpointInner::sip_addr_to_uri()` (endpoint.rs ~L734): `SipAddr` -> `Uri`, downgrading `sips:` to `sip:` for TLS when flag is false (`;transport=tls` is already present). Used by `get_record_route_with_addr` (~L759) and the dialog `remote_uri` seed (invitation.rs ~L903).
- `DialogLayer::build_local_contact` (dialog_layer.rs ~L659) and `make_invite_request` Contact (invitation.rs ~L470) are gated by the flag.
- REGISTER already used `sip:` upstream (registration.rs hardcodes `Scheme::Sip`), unchanged.
- Test: `test_use_sips_for_tls_disabled_keeps_sip_scheme` in `src/dialog/tests/test_dialog_layer.rs`.

## Per-destination SNI override (`sni_by_addr`)
- `src/transport/tls.rs` ~L255: `TlsConfig.sni_by_addr: HashMap<SocketAddr, String>` (included in the manual `Debug`; `Clone`/`Default` unchanged).
- `src/transport/transport_layer.rs` `lookup` (TLS branch, ~L655): when the resolved destination (IP host + port, default 5061) has an entry, its value becomes the SNI / verification name for that connection; otherwise upstream behaviour (`sni_hostname` if set, else the original domain, else the IP).
- Rationale: Siple sets `tls_server_name` / the registrar host as the SNI of the registrar (or outbound-proxy) connection only; with the layer-wide `sni_hostname` that name was pinned onto every TLS target (calls to other hosts, OPTIONS to peers).
- Test: `test_sni_by_addr_applies_only_to_listed_destination` in `src/transport/tests/test_tls_verifier.rs`.

## `Transaction::receive` ends when the endpoint is cancelled
- `src/transaction/transaction.rs` `receive_with_response_observer` (~L936; `receive` delegates to it since 0.8.1): the wait for the next transaction event races the endpoint's cancel token (only the cancel-safe `tu_receiver.recv()` is raced; queued events are still delivered first) and returns `None` once it is cancelled. `src/transaction/endpoint.rs` ~L271: `EndpointInner::cancelled()` accessor.
- Rationale: transactions end through timers, and the timers stop with the endpoint. Tasks draining a transaction (e.g. the Timer M drain spawned after a 2xx to INVITE in `do_invite_async`) then waited forever, holding the transport connection and endpoint: a removed / rebuilt account leaked its UDP socket (fixed `local_port` -> EADDRINUSE) after any call. Upstream 0.8.6 still has no such exit.
- Test: `src/transaction/tests/test_cancel_receive.rs`.

## Last remote CANCEL on server INVITE dialogs
- `src/dialog/dialog.rs`: `DialogInner.remote_cancel: Mutex<Option<Request>>` (both constructors). `src/dialog/invite_dialog.rs` (`InviteDialog::last_remote_cancel()`, ~L105; recorded in the `Method::Cancel` arm of `handle_invite` ~L1207) and `src/dialog/server_dialog.rs` (same arm ~L895): the CANCEL request is stored right before `Terminated(UacCancel)` is notified, like `remote_ack`.
- Rationale: the CANCEL is consumed by the INVITE server transaction, so its `Reason` header (RFC 3326, `SIP;cause=200;text="Call completed elsewhere"`) never reached the application; Siple uses it to tell "answered elsewhere" from "missed".
- Test: `src/dialog/tests/test_cancel_reason.rs`.

## Phase 12 (100rel as UAS, session timers): no vendor changes
Reliable 18x are ordinary provisional responses sent through `InviteDialog::ringing()` (extra `Require`/`RSeq` headers; a retransmission is the same call again), and PRACK is matched on `RAck` by Siple before the dialog layer sees it (`crates/siple-sip/src/calls/reliable.rs`); session-timer refreshes use `InviteDialog::update()` / `reinvite()` (`calls/timer.rs`). rsipstack's own `handle_prack` (always 200) is therefore never reached for inbound calls.

## Network change: swap a running listener, retire stream flows, mutable dialog Contact (Network B2)
- `src/transport/transport_layer.rs`: `TransportLayer::swap_listener(old: Option<&SipAddr>, new: SipConnection)` (~L237) adds `new` to the lookup list **and starts its receive loop** (`add_transport` only takes effect for listeners present when `Endpoint::serve` starts; `serve_listener` is `pub(super)`), then removes `old`. New transactions pick `new` from the moment it returns (lookup and `get_via` read the live list); in-flight transactions keep their connection. Stopping the old socket's loop is the caller's job (cancel the token it passed to `UdpConnection`). `TransportLayer::retire_stream_connections()` (~L270) retires every cached TCP/TLS/WS connection (cancel token + removed from the cache, via the existing `retire_connection`; since 0.8 the cache holds a list per remote address and is flattened, and Channel connections, which upstream's `retire_connection` now also retires, are skipped), so the next lookup dials fresh from the new local address and dialogs stop choosing the dead flow for affinity. Each retired connection is also `close()`d in the background (cancelling its token only marks the flow; the read task and socket would otherwise live until the runtime is dropped).
- `src/dialog/dialog.rs`: `DialogInner.local_contact` is now `Mutex<Option<Uri>>` (was `Option<Uri>`); the readers (`make_request_with_vias`, `make_response`, the forked-branch BYE, the snapshot) lock it, the two constructors wrap it. `src/dialog/invite_dialog.rs`: `InviteDialog::set_local_contact(Uri)`, `local_contact()`, and `clear_flow_affinity()` (forgets the recorded server connection, which would otherwise pin in-dialog requests to a dead TCP/TLS flow until its token is cancelled).
- Rationale: a network change moves the local address under live calls. Siple rebinds the UDP socket in place, republishes the Contact and sends a re-INVITE per call instead of tearing the endpoint (and every dialog) down.
- Test: `src/dialog/tests/test_swap_listener.rs` (the re-INVITE after a swap leaves from the new socket with the new Via and Contact; a request sent to the new socket reaches the endpoint). `TransportLayer` is no longer `Clone` upstream (0.7.10), so the test reaches it through `endpoint.inner.transport_layer`.
- `TransportLayer::reactivate_listener(old, new)` (~L257, follow-up): re-installs a swapped-out listener whose receive loop still runs (lookup list only, no second loop). Used when the network flaps back to an address whose old socket still lingers.

## Credential guard (`Credential.guard`)
- `src/dialog/authenticate.rs`: `Credential.guard: Option<AuthGuard>` (`AuthGuard(Arc<dyn Fn(&Transaction, &Response) -> Result<(), String>>)`, ~L105-115). `handle_client_authenticate` (~L245; the single funnel for INVITE, in-dialog requests, REGISTER, upstream's stale-nonce retry and siple's own client transactions) calls it first; `Err(msg)` returns `Error::DialogError(msg, id, <401|407>)` without computing any digest.
- Every `Credential { .. }` literal (docs, README, tests) gained `guard: None`.
- Rationale: siple only answers a challenge when the transaction's actual destination (`Transaction::destination` after `send`: the resolved / connected peer address) is one of the account's provider addresses. The decision needs the per-request destination, so it cannot be made once at dialog creation; request-URI, Contact and Route are peer-controlled and are not consulted.

## Backports from upstream main (drop when 0.8.7+ is vendored)
Both are merged on upstream `main` (head `4ff2bd2`) but in no release yet. Applied verbatim, with
their upstream tests; only the `// siple:` markers were added.

### Quoted `;` in parameter values (upstream PR #259, commit `021e308`)
- `src/sip/uri.rs` ~L716-746: `split_unquoted_semicolons`; `parse_params` (~L759) splits only outside
  quoted strings (quoted-pairs honoured) and refuses an unterminated quoted value.
- Test: `quoted_contact_params_may_contain_semicolons` in `src/sip/headers/typed/contact.rs` (a full
  REGISTER 200 OK).
- Why Siple needs it: Kamailio with `received_avp` answers a REGISTER over TCP / TLS with
  `Contact: <...;transport=TLS>;...;received="sip:IP:port;transport=tls"`. 0.8.6 split the quoted
  value, refused the whole 200 OK (0.8.6's stricter header validation) and closed the connection,
  so the account never registered. 0.7.3 accepted the message. Siple test:
  `tcp_register_ok_with_quoted_received_contact_param_registers` in
  `crates/siple-sip/tests/network.rs` (fails without this backport).

### CANCEL follows the INVITE's destination (upstream PR #261, commits `71dfc26`, `30fe239`, `a84b322`; issue #260)
- `src/dialog/dialog.rs`: `DialogInner.invite_destination: Mutex<Option<SipAddr>>` (~L370; both
  constructors); `send_dialog_request` (~L1295) gives a CANCEL that destination when the request has
  no live connection (a live flow supplies its own peer address).
- `src/dialog/invitation.rs` ~L907: `create_client_invite_dialog` records `tx.destination`.
- Tests: `src/dialog/tests/test_cancel_destination.rs` (verbatim) and
  `src/dialog/tests/test_cancel_connection.rs` (upstream's integration test `tests/cancel_connection.rs`,
  moved into the crate because `tests/` is not vendored; only `rsipstack::` paths became `crate::`).
- Why Siple needs it: Siple always sets `InviteOption::destination` (registrar / outbound proxy, or
  the resolved target). Without this the CANCEL went wherever the Request-URI resolved, so with an
  outbound proxy, or a registrar host that differs from the domain, it never reached the callee's
  side and the callee kept ringing. This bug was already in 0.7.3.

## Dropped in the 0.8.6 update
- **ACK routes of a 2xx registered before the 2xx is sent** (`Transaction::respond`): upstream has the same fix since 0.7.4, commit `a890578` "fix(transaction): add the 2xx ACK route before sending the 2xx" (issue #192), with a guard that removes the route when the send fails or is cancelled. Siple's version and its test (`test_server_invite_ack_right_after_2xx_is_delivered`) were dropped; upstream's `test_server_invite_ack_during_final_response_send` covers it.
- `test_tls_reload::generate_test_cert` visibility: upstream made it `pub(super)` (0.7.4), which the verifier tests need.
