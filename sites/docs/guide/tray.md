# Tray, notifications and startup

Choose whether Siple lives in your tray or menu bar, when it shows its window, and whether it starts with your computer.

## Keep Siple running in the tray

The tray is the row of small icons by the clock on Windows and Linux. On a Mac it is the menu bar. With the tray icon on,
Siple stays available there, and the icon's tooltip tells you the state: idle, the number of calls, a registration problem,
or do not disturb.

1. Open **Settings › Notifications**.
2. Under **Tray and window**, turn on **Show in the tray / menu bar**.
3. Turn on **Closing the window keeps Siple running** if you want the close button to hide the window instead of quitting.

With both on, closing the window hides it and Siple keeps taking calls. To quit for real, choose **Quit Siple** from the
tray menu, or press <Keys combo="Mod+Q" />. To bring the window back, choose **Show Siple** from the tray menu.

If you turn off **Show in the tray / menu bar**, closing the window quits the app.

::: tip Never miss a call
Leave **Closing the window keeps Siple running** on. Otherwise closing the window quits Siple and you stop receiving
calls.
:::

## Hide Siple from the taskbar or Dock

Turn on **Hide from the taskbar / Dock while hidden** and Siple shows no Dock icon (Mac) or taskbar button (Windows,
Linux) while its window is hidden. Turn it off to keep the Dock icon, or to minimize to the taskbar on Windows and Linux.

## Start in the tray when you sign in

1. Turn on **Start hidden** under **Settings › Notifications** so Siple opens without a window.
2. Turn on launch at login. This one is a config file setting, not a switch in the window: see the details below.

Together they put Siple in your tray each time you sign in to your computer.

## Show the window for calls

Under **Tray and window**:

- **Show the window on an incoming call** is on by default. The window comes to the front when a call arrives.
- **Show the window when a call is answered** is off by default. Turn it on to raise the window when a call is answered.

## Desktop notifications

Turn on **Desktop notifications** under **Notifications and links** to see a pop-up for incoming calls, missed calls and
transfer results. Notifications appear while the Siple window is not in focus. On Linux, the incoming call pop-up has
**Answer** and **Decline** buttons. It is on by default.

On a Mac, notifications need the signed Siple app you installed from the download. A copy of the program that is not
inside the app logs a message once and shows none.

To control how phone links open, see [Phone links in your browser](/guide/links).

::: details In config.toml
All of these keys are under `[ui]`. See the [reference](/reference/configuration#ui).

| Setting in the app | Key | Default |
|---|---|---|
| **Show in the tray / menu bar** | `tray` | on |
| **Closing the window keeps Siple running** | `close_to_tray` | on |
| **Hide from the taskbar / Dock while hidden** | `hide_from_taskbar` | on |
| **Start hidden** | `start_hidden` | off |
| (config file only) | `launch_at_login` | off |
| **Show the window on an incoming call** | `show_on_incoming` | on |
| **Show the window when a call is answered** | `show_on_answer` | off |
| **Desktop notifications** | `notifications` | on |

`tray = false` makes closing the window quit the app.

`launch_at_login = true` writes a macOS LaunchAgent, a Windows `Run` registry entry or a Linux XDG autostart entry for the
running executable (the AppImage edition: the `.AppImage` file, not its temporary mount). `false` removes it. A non-default `--config` profile gets its own entry. In the Flatpak edition the
setting goes through your desktop's Background portal, which may ask once; there is one entry for the app, so the profile that
enabled it last wins.

```toml
[ui]
start_hidden = true
launch_at_login = true
```
:::

::: details Linux tray notes
Linux uses a StatusNotifierItem (KDE, XFCE, GNOME with the AppIndicator extension). Without a host for it, closing the
window minimizes it so the app stays reachable.

On Wayland the window cannot be unmapped, so closing minimizes it and **Start hidden** has no effect. For full hide-to-tray,
start Siple under XWayland: `env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET siple`.
:::
