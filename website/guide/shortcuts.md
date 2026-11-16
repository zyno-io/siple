# Keyboard shortcuts

`Mod` is Cmd on macOS and Ctrl on Windows and Linux. Shortcuts work while the Siple window has focus; OS-wide (global)
hotkeys are not available.

| Action | Default | Does |
|---|---|---|
| `answer` | `Mod+Shift+A` | Answer the selected ringing call, else the newest ringing one |
| `hangup` | `Mod+Shift+E` | Hang up the selected line; cancel a dialing call or consult; decline a ringing call |
| `mute` | `Mod+Shift+M` | Mute / unmute |
| `hold` | `Mod+Shift+H` | Hold / resume |
| `transfer` | `Mod+Shift+T` | Open the transfer composer |
| `line_1 ... line_9` | `Mod+1 ... Mod+9` | Select that line |
| `line_prev / line_next` | `Mod+[ / Mod+]` | Previous / next line |
| `focus_dial` | `Mod+L` | Focus the dial field |
| `filter_extensions` | `/` | Focus the extensions filter |
| `settings` | `Mod+,` | Open / close Settings |

While a line is Active and no text field has focus, digits, `*` and `#` send DTMF; otherwise they type into the dial field.
Esc only closes popovers and the transfer composer: it never hangs up.

## Changing a shortcut

Override any action in `[ui.shortcuts]` (also under Settings > Shortcuts). Changes are hot.

```toml
[ui.shortcuts]
answer = "Mod+Shift+A"
mute = "Alt+M"
```

A chord is modifiers and one key joined by `+`, case-insensitive. Modifiers: `Mod`, `Ctrl`, `Alt` (Option), `Shift`, `Cmd`.
Keys: a letter or digit, punctuation such as `,` `.` `/` `[` `]`, `Plus`, a name (`Enter`, `Tab`, `Space`, `Backspace`,
`Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right`) or `F1` to `F12`.

These are config errors: an unknown action name, a malformed chord, `Escape` as the key, a bare digit with no modifier,
`Mod+C` / `Mod+V` / `Mod+X`, and two actions sharing one effective chord. The full rules are in the
[`[ui]` reference](/reference/configuration#ui).
