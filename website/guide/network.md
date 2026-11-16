# Network changes

Siple watches for network changes so you do not have to restart it when you move between Wi-Fi networks, connect a VPN or
wake the laptop.

- **Detection.** OS address and route change notifications (macOS SystemConfiguration, Windows `NotifyIpInterfaceChange`, Linux
  rtnetlink), sleep/wake detection, and a periodic probe (every 30 s by default) that acts only if the route or DNS result
  differs.
- **Re-register.** Accounts re-probe their route and re-register at once instead of waiting for a refresh or a timeout. With
  no route to the registrar an account shows `waiting_for_network`, not `failed`, and sends nothing until the network returns.
- **Live calls move.** When the local address changes, established calls are kept: Siple rebinds the RTP sockets and sends
  a re-INVITE with the new address, keeping hold state and SRTP keys. A call ends with cause `network_lost` if the peer
  answers 481 or 408, does not answer within 10 s, or refuses the re-INVITE. Calls that are still ringing are left alone.
- **`transport = "auto"`** may pick a different transport after a re-probe, but never while the account has calls.

Tune it under `[network]`: `watch`, `probe_secs`, `debounce_ms`, `move_calls` (set `false` to end established calls with
`network_lost` right away). All are hot. See [`[network]`](/reference/configuration#network).

Force a refresh from the API with `network.refresh` or `account.reregister`.
