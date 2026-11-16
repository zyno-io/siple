# sip: and tel: links

Siple can handle `sip:`, `sips:`, `tel:` and `callto:` links. Opening one puts the number in the dial field and raises the
window. It **never dials by itself**. A second `siple <link>` hands the link to the running instance.

| OS | How it is registered |
|---|---|
| macOS | Declared in the app bundle's Info.plist at install time. If another app already handles a scheme, change the default in that app. |
| Windows | Siple registers itself under HKCU as a candidate handler; pick it in Settings > Default apps. `handle_links = false` removes the registration. |
| Linux | The `.desktop` file declares the schemes. Make it the default, for example `xdg-mime default io.zyno.siple.desktop x-scheme-handler/sip` (repeat for `sips`, `tel`, `callto`). |

On macOS and Linux `[ui] handle_links` has no runtime effect, because registration happens at install time.

Agents can do the same through the control API: `app.open_uri`.
