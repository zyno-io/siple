# Phone links in your browser

Make Siple the app that opens when you click a phone number or a `sip:` link on a web page.

## What happens when you click a link

Siple handles `sip:`, `sips:`, `tel:` and `callto:` links. When you click one, Siple opens, puts the number in the dial field
and brings its window to the front. It never dials by itself: you check the number and press **Call**.

If Siple is already running, the link goes to that copy. It does not open a second one.

## Turn it on or off

1. Open **Settings › Notifications**.
2. Under **Notifications and links**, switch **Open sip:, sips:, tel: and callto: links** on or off.

The setup you see on first start has the same choice, **Open sip:, tel: and callto: links with Siple**.

## Make Siple the default app for phone links

Your computer decides which app opens a link. Pick Siple once for your system.

<OsTabs>
<template #mac>

Siple declares the link types when you install it. If another app already handles a type, change the default in that app.

The switch in Settings has no effect on a Mac, because this registration happens at install time.

</template>
<template #windows>

Siple registers itself as a candidate handler for your user account. Choose it in **Settings › Default apps** in Windows.
Turning the Siple setting off removes the registration.

</template>
<template #linux>

The `.desktop` file declares the link types. Make Siple the default with:

```sh
xdg-mime default io.zyno.siple.desktop x-scheme-handler/sip
```

Repeat the command for `sips`, `tel` and `callto`. The switch in Settings has no effect on Linux, because this registration
happens at install time. The Flatpak edition works the same way: run the `xdg-mime` command above.

</template>
</OsTabs>

::: details In config.toml
The setting is `handle_links` under `[ui]`. On Windows, `handle_links = false` removes the registration. On macOS and Linux
it has no runtime effect, because registration happens at install time.

| OS | How it is registered |
|---|---|
| macOS | Declared in the app bundle's Info.plist at install time. |
| Windows | Registered under HKCU as a candidate handler. |
| Linux | The `.desktop` file declares the schemes. |

A second `siple <link>` command hands the link to the running instance. Agents can do the same through the control API with
`app.open_uri`.
:::
