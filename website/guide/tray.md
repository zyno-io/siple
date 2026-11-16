# Tray and notifications

## Tray and menu bar

With `[ui] tray = true` (default) Siple shows a tray icon (menu bar on macOS). The tooltip reflects state: idle, number of
calls, registration problem, or do not disturb.

- **Close to tray** (`close_to_tray`, default on): closing the window hides it. Only the tray menu's **Quit** (or Cmd+Q on
  macOS, Ctrl+Q on Windows and Linux) exits.
- **Hide from taskbar** (`hide_from_taskbar`, default on): no Dock icon (macOS) or taskbar button (Windows, Linux) while
  hidden. Set it to `false` to keep the Dock icon, or to minimize to the taskbar elsewhere.
- **Start hidden** (`start_hidden`) and **launch at login** (`launch_at_login`): combine them to start in the tray.
- `tray = false` makes closing the window quit the app.

Linux uses a StatusNotifierItem (KDE, XFCE, GNOME with the AppIndicator extension). Without a host, closing the window
minimizes it so the app stays reachable. On Wayland the window cannot be unmapped, so closing minimizes and `start_hidden`
has no effect; run with `WINIT_UNIX_BACKEND=x11` for full hide-to-tray.

## Raising the window

`show_on_incoming` (default on) raises the window on an incoming call; `show_on_answer` (default off) does so when a call is
answered.

## Notifications

`notifications = true` (default) shows desktop notifications for incoming calls (with Answer and Decline on Linux), missed
calls and transfer results, while the window is not focused. On macOS this needs the signed app bundle; an unbundled binary
logs once and skips them.

## Launch at login

`launch_at_login = true` writes a macOS LaunchAgent, a Windows `Run` registry entry or a Linux XDG autostart entry for the
running executable; `false` removes it. A non-default `--config` profile gets its own entry.

All keys are under `[ui]`; see the [reference](/reference/configuration#ui).
