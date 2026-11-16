# Extensions and BLF

Extensions (busy lamp field, BLF) show whether colleagues' phones are idle, ringing or busy. Add watched extensions per
account in `config.toml` or in Settings > Extensions.

<Shot name="blf-keys" alt="Extensions shown as keys" />

```toml
[[account]]
# ... account settings ...

[[account.blf]]
uri = "1004"
label = "Support queue"
group = "Support"
```

Subscriptions use the SIP dialog event package. Entries are diffed by `uri`, so adding, removing and editing them is hot.

## Views

`[ui] blf_view` selects the layout: `keys` (default), `lamps` or `list`. `blf_group_by` is `account` (default), `group` or
`none`. Use `/` to focus the filter, which matches name, extension or group.

## Click behaviour

| Situation | Setting | Default |
|---|---|---|
| Click an idle extension | `blf_click`: `dial` or `prefill` (fill the dial field) | `dial` |
| Click during a call | `blf_click_in_call`: `menu`, `consult` or `blind` | `menu` (consult, transfer or new call) |
| Click a ringing extension | `blf_click_ringing`: `pickup` or `call` | `pickup` |

## Directed pickup

While an extension is ringing, clicking it can pick up that call, provided pickup is configured for the account or entry.
Set `pickup = "prefix"` with `pickup_prefix` (for example `**`, giving `**1004`), or use `pickup_target` for queue codes.
`replaces` (SIP Replaces) is experimental and not interop-verified; prefer `prefix`. Full rules:
[Directed pickup](/reference/configuration#directed-pickup).

```toml
[[account]]
pickup = "prefix"       # off | prefix | replaces
pickup_prefix = "**"
```

If keys look stale, check the subscription state: [BLF subscription health](/reference/configuration#blf-subscription-health).
