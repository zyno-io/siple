# Network changes

Keep your calls and registration working when you switch Wi-Fi, connect a VPN or wake your laptop, with nothing for you to do.

## What you can expect

Siple watches for network changes, so you do not have to restart it. Your accounts reconnect on their own, and a call that
is already going usually carries on.

- **After a change, your account reconnects at once.** Siple checks the route to your phone system (the PBX, the server
  that connects your calls) and registers again without waiting for a timeout.
- **With no network, the account waits.** Siple sends nothing and shows **waiting for network** on the account, not
  **failed**. When the network comes back, it registers again.
- **A call in progress moves with you.** If your computer's address changes during a call, Siple keeps the call going,
  including its hold state and encryption.
- **A ringing call is left alone.** Only calls already answered are moved.

If a call does end after a network change, Siple records the cause `network_lost`. That happens when the other side says it
no longer knows the call, does not answer within 10 seconds, or refuses the move.

There is nothing to switch on. To see the account's state, look at the lamp next to it in the top bar. If it stays red or
amber, see [Troubleshooting](/guide/troubleshooting).

## Details

::: details How Siple notices a change
- **Detection.** Siple listens to the operating system's address and route change notifications (macOS SystemConfiguration,
  Windows `NotifyIpInterfaceChange`, Linux rtnetlink), notices sleep and wake, and runs a periodic probe (every 30 s by
  default) that acts only if the route or DNS result differs.
- **Re-register.** Accounts re-probe their route and re-register at once instead of waiting for a refresh or a timeout. With
  no route to the registrar (the server your account signs in to) an account shows `waiting_for_network`, not `failed`, and
  sends nothing until the network returns.
- **Live calls move.** When the local address changes, established calls are kept. Siple rebinds the RTP sockets (RTP is the
  stream that carries call audio) and sends a re-INVITE (a SIP message that tells the other side the new address), keeping
  hold state and SRTP keys (SRTP is encrypted RTP). A call ends with cause `network_lost` if the peer answers 481 or 408,
  does not answer within 10 s, or refuses the re-INVITE. Calls that are still ringing are left alone.
- **`transport = "auto"`** may pick a different transport after a re-probe, but never while the account has calls.
:::

::: details Picking the transport automatically
With `transport = "auto"`, Siple looks up DNS SRV records for the next hop (the outbound proxy, else the registrar, else the
domain). The first of `_sips._tcp` (TLS), `_sip._tcp` (TCP) and `_sip._udp` (UDP) that has a record wins, with that record's
hosts and ports. With no SRV record it uses UDP on port 5060. There is no NAPTR lookup. An IP address or an explicit
`host:port` skips SRV and uses UDP.

- `status` and `account.get` show `transport: "auto"` plus `transport_effective` (`udp`, `tcp` or `tls`). The window shows
  "TLS (auto)".
- The choice is made when the transport is built and again after each network re-probe. A changed answer rebuilds the
  transport and re-registers. While the account has calls, the transport is kept.
- With `auto`, `tls_verify`, `tls_ca_file` and `tls_fingerprint` apply only if TLS is picked.
:::

::: details In config.toml
Tune this under `[network]`: `watch`, `probe_secs`, `debounce_ms` and `move_calls`. Set `move_calls = false` to end
established calls with `network_lost` right away. All of these are hot, which means they apply without a restart. See
[`[network]`](/reference/configuration#network).

```toml
[network]
move_calls = true
probe_secs = 30
```

Agents can force a refresh with `network.refresh` or `account.reregister`.
:::
