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
rules: [Directed pickup](/reference/configuration#directed-pickup).

::: details In config.toml
Set `pickup = "prefix"` with `pickup_prefix` (for example `**`, giving `**1004`), or use `pickup_target` for queue codes.
`replaces` is the experimental SIP Replaces method.

```toml
[[account]]
pickup = "prefix"       # off | prefix | replaces
pickup_prefix = "**"
```
:::

## If the lamps look out of date

If keys look stale, check the subscription state: [BLF subscription health](/reference/configuration#blf-subscription-health).

Next: [Keyboard shortcuts](/guide/shortcuts).
