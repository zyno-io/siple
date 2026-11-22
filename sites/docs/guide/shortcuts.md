# Keyboard shortcuts

Answer, hold, transfer and switch calls without the mouse, and change the keys to suit you.

Shortcuts work while the Siple window has focus. OS-wide (global) hotkeys are not available. The keys below show the default
for your computer: in `config.toml`, `Mod` means Cmd on a Mac and Ctrl on Windows and Linux.

## Calls

| What it does | Default | Action name |
|---|---|---|
| Answer the selected ringing call, else the newest ringing one | <Keys combo="Mod+Shift+A" /> | `answer` |
| Hang up the selected line; cancel a dialing call or consult; decline a ringing call | <Keys combo="Mod+Shift+E" /> | `hangup` |
| Mute or unmute | <Keys combo="Mod+Shift+M" /> | `mute` |
| Hold or resume | <Keys combo="Mod+Shift+H" /> | `hold` |
| Open the transfer panel | <Keys combo="Mod+Shift+T" /> | `transfer` |

## Lines

| What it does | Default | Action name |
|---|---|---|
| Select line 1 to 9 | <Keys combo="Mod+1" /> to <Keys combo="Mod+9" /> | `line_1` to `line_9` |
| Select the previous or next line | <Keys combo="Mod+[" /> and <Keys combo="Mod+]" /> | `line_prev`, `line_next` |

## Around the window

| What it does | Default | Action name |
|---|---|---|
| Focus the dial field | <Keys combo="Mod+L" /> | `focus_dial` |
| Focus the extensions filter | <Keys combo="/" /> | `filter_extensions` |
| Open or close Settings | <Keys combo="Mod+," /> | `settings` |

## Type digits during a call

While a line is **Active** and no text field has focus, digits, `*` and `#` send keypad tones (DTMF). Otherwise they type
into the dial field.

Pressing <Keys combo="Esc" /> only closes popovers and the transfer panel. It never hangs up.

## Change a shortcut

**Settings › Shortcuts** lists every shortcut and marks the ones you changed as **(custom)**. It also lists **Shortcut
problems**, such as two actions sharing one key. To change a key, edit `config.toml`: choose **Open config** in
**Settings › Advanced**, or in the bar at the bottom of the window (turn on **Settings › Developer › Show developer tools**). Changes apply while Siple runs.

::: details In config.toml
Override any action in `[ui.shortcuts]`, using the action names from the tables above:

```toml
[ui.shortcuts]
answer = "Mod+Shift+A"
mute = "Alt+M"
```

A chord is modifiers and one key joined by `+`, case-insensitive. Modifiers: `Mod`, `Ctrl`, `Alt` (Option), `Shift`, `Cmd`.
`Mod` is Cmd on macOS and Ctrl on Windows and Linux. Keys: a letter or digit, punctuation such as `,` `.` `/` `[` `]`, `Plus`,
a name (`Enter`, `Tab`, `Space`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`,
`Right`) or `F1` to `F12`.

These are config errors: an unknown action name, a malformed chord, `Escape` as the key, a bare digit with no modifier,
`Mod+C` / `Mod+V` / `Mod+X`, and two actions sharing one effective chord. The full rules are in the
[`[ui]` reference](/reference/configuration#ui).
:::
