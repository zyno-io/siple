# See who is free

Watch your colleagues' extensions, and call, transfer or pick up from them with one click.

An **extension** is a colleague's internal number, such as `1004`. Siple can show whether each one is idle, ringing or busy.
This is called **BLF** (busy lamp field), the row of lamps you may know from a desk phone. The panel on the right of the
window is headed **EXTENSIONS**. Use **Show extensions** and **Hide extensions** in the top bar to open and close it.

<Shot name="blf-keys" alt="Extensions shown as keys" caption="Extensions as keys: each shows a name, an extension and a state." />

## Add extensions to watch

1. Open **Settings › Extensions**.
2. Under **Extensions per account**, pick the **Account**.
3. Choose **Add extension**.
4. Fill in **Label** (for example Reception), **Extension** (`1002` or `sip:1002@host`) and, if you like, **Group**.
5. Choose **Apply**.

Use **Up**, **Down** and **Remove** on each entry to reorder or delete it. Changes apply without restarting Siple.

::: details In config.toml
Add watched extensions per account:

```toml
[[account]]
# ... account settings ...

[[account.blf]]
uri = "1004"
label = "Support queue"
group = "Support"
```

Subscriptions use the SIP dialog event package. Entries are diffed by `uri`, so adding, removing and editing them is hot.
:::

## Choose how they look

In the **EXTENSIONS** panel, switch between **Keys**, **Lamps** and **List**. You can also set **Layout** in
**Settings › Extensions**, under **View and click behaviour**.

<Shot name="blf-list" alt="Extensions shown as a list" caption="The List view can be sorted by Name, Ext or State." />

- **Group by** sets how extensions are grouped: **Account** (the default), **Group** or **No grouping**.
- To find someone, type in the filter box (**Filter by name, extension or group**) or press <Keys combo="/" />. It matches
  name, extension or group. The chips **All**, **Ringing**, **Busy** and **Problems** narrow the view further.
- In **List**, click **Name**, **Ext** or **State** to sort. **Ringing first** puts ringing extensions on top.

::: details In config.toml
`[ui] blf_view` selects the layout: `keys` (default), `lamps` or `list`. `blf_group_by` is `account` (default), `group` or
`none`.
:::

## Decide what a click does

Choose these in **Settings › Extensions**, under **View and click behaviour**.

| Situation | Setting | Default |
|---|---|---|
| Click an idle extension | **Click on an idle extension**: **Call it** or **Fill the dial field** | **Call it** |
| Click during a call | **Click during a call**: **Ask**, **Consult transfer** or **Blind transfer** | **Ask** (consult, transfer or new call) |
| Click a ringing extension | **Click on a ringing extension**: **Pick up** or **Call it** | **Pick up** |

::: details In config.toml
| Setting | Key and values | Default |
|---|---|---|
| Click an idle extension | `blf_click`: `dial` or `prefill` (fill the dial field) | `dial` |
| Click during a call | `blf_click_in_call`: `menu`, `consult` or `blind` | `menu` (consult, transfer or new call) |
| Click a ringing extension | `blf_click_ringing`: `pickup` or `call` | `pickup` |
:::

## Pick up a ringing extension

When an extension is ringing, clicking it can pick up that call. This works only if pickup is set up for the account or for
that entry.

1. Open **Settings › Accounts** and choose the account.
2. Under **Pickup**, set **Pickup method** to **Feature code** and enter the **Pickup prefix** (for example `**`, which
   gives `**1004`).
3. To set it for one extension only, open **Pickup overrides** on that entry in **Settings › Extensions**. Use **Pickup
   target** for queue codes.

The pickup method **Replaces** (SIP Replaces) is experimental and not interop-verified, so prefer **Feature code**. Full
rules: [Directed pickup rules](#directed-pickup-rules).

::: details In config.toml
Set `pickup = "prefix"` with `pickup_prefix` (for example `**`, giving `**1004`), or use `pickup_target` for queue codes.
`replaces` is the experimental SIP Replaces method.

```toml
[[account]]
pickup = "prefix"       # off | prefix | replaces
pickup_prefix = "**"
```
:::

## Directed pickup rules

Pickup is offered only while the extension is ringing, its lamp is up to date and a pickup method other than `off` applies
to it (the entry's own setting, else the account's). Otherwise `call.pickup` fails with `invalid_state`. A pickup is an
ordinary call, so auto-hold applies. If the PBX refuses it (404, 480, 486 and so on), the call ends with that code.

- **`prefix`** dials `pickup_target` if the entry has one, else `<pickup_prefix><extension>`. The extension is the user part of
  `uri` (`sip:1004@pbx` and `1004` both give `1004`), so the default gives `**1004`.
- **`replaces`** (experimental, not verified with other systems) sends an INVITE with `Replaces`, built from the ringing
  dialog. It goes to the caller by default. `pickup_replaces_target = "monitored"` sends it to the monitored phone instead,
  for B2BUA setups where the PBX created the callee leg. If the PBX does not send what is needed, the pickup fails with
  `invalid_state`. Use `prefix` there.
- **Routing.** Every pickup goes through the account's outbound proxy or registrar, whatever host the target names. A
  hostile NOTIFY cannot steer the call or your digest credential to another host.

```toml
[[account]]
pickup = "prefix"       # off | prefix | replaces
pickup_prefix = "**"

[[account.blf]]
uri = "1004"
label = "Support queue"
group = "Support"
pickup = "replaces"     # optional per-key override
pickup_target = ""      # optional: dial this instead of <prefix><ext>
```

## BLF subscription health

Each watched extension is its own SUBSCRIBE dialog. `blf.list` and `status` report, per entry, a `subscription` object and a
`stale` flag.

- `phase`: `pending`, `active`, `retrying` (next attempt at `retry_at`) or `failed` (the PBX refused for good; `last_code` is
  the SIP status).
- `last_problem`: plain-language `{kind, title, hint}` of the last failure. Absent while healthy.
- `subscribed_at`: when the current subscription was first confirmed. Refreshes keep it; a failure, termination or new dialog
  resets it.
- `last_refreshed_at`: the last successful refresh. Both times are `null` until confirmed and after a failure.
- `stale` is true while retrying, and when neither a NOTIFY nor a refresh arrived within 30 seconds after `expires_at`. The
  last known state stays on the lamp.

With many entries, raise `blf_expires` so there are fewer refreshes.

Next: [Keyboard shortcuts](/guide/shortcuts).
