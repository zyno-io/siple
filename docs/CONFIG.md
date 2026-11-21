# Siple configuration reference

Siple is configured **only** by a TOML file: every setting is a key in `config.toml` and hot-reloads. The GUI and the
control API (`config.edit`, see [Editing from the GUI and the API](#editing-from-the-gui-and-the-api)) are only writers of that file;
`state.toml` holds runtime state that is not a setting. The embedded, commented example
is `crates/siple-config/config.example.toml`; print it with `siple-cli config-example`. It is written
to the config directory on first run (mode 0600, because it holds passwords). The schema is
`crates/siple-config/src/schema.rs`; unknown keys are rejected (a typo is an error, not silently ignored).

## Location

| OS | Default config file |
|---|---|
| macOS | `~/Library/Application Support/Siple/config.toml` |
| Windows | `%APPDATA%\Siple\config.toml` |
| Linux | `$XDG_CONFIG_HOME/siple/config.toml` |

Store editions keep their data somewhere else, fixed by the OS sandbox or package:

| Edition | Config directory |
|---|---|
| Mac App Store | `~/Library/Group Containers/VUDU4KEQ99.io.zyno.siple/Siple/` (the app group shared with the bundled CLI; the real home from the account database, never the sandbox `$HOME`) |
| Microsoft Store (MSIX) | `%LOCALAPPDATA%\Packages\<package family name>\LocalState\Siple\` |
| Flatpak (Flathub) | `~/.var/app/io.zyno.siple/config/siple/` (Flatpak's per-app `$XDG_CONFIG_HOME`; the same path inside the sandbox and on the host) |

In the Mac App Store edition the control socket is `control.sock` in that directory; if the path would be too
long for a unix socket it falls back to `s.sock` in the group container (never a temporary directory), and
`[control] tcp_listen` is the way out when even that is too long. In the Flatpak edition the control socket is also
`control.sock` there; for a too-long path the fallback is `$XDG_RUNTIME_DIR/app/io.zyno.siple/` (shared by every instance
of the app and the host, never the sandbox's private `/tmp`). The sandbox sees only its own directory: files Siple reads
(TLS files, `file:` audio) must be inside `~/.var/app/io.zyno.siple/`, or granted with
`flatpak override --user --filesystem=/path:ro io.zyno.siple`. `status.build.channel` (`direct`, `appimage`, `mas`, `msix`,
`flatpak`) tells which edition is running. The AppImage uses the Linux default like the tarball; the runtime's portable mode
(`<file>.config` next to the file, `--appimage-portable-config`) moves it.

Override with `--config PATH` (both binaries; also for `ctl`/`events`/`mcp`) or the `SIPLE_CONFIG` environment variable (handy with `docker exec`). Lock file, `runtime.json`, `state.toml`, the default control
socket and the default log directory all live next to the config file, so different `--config` directories are
independent instances. Relative file paths inside the config (`tls_ca_file`, `tls_cert`, `tls_key`,
`file:`/`record:` audio specs, `log_dir`) are relative to the config directory.

## Reloading

Edits are picked up while Siple runs: the config directory is watched (500 ms debounce; unchanged content
is ignored). `siple-cli ctl config.reload` (and SIGHUP on macOS) forces a reload and returns what changed.

- An **invalid file never replaces the last good config**: the error is logged, a `config.error` event is emitted,
  the GUI shows a banner, and the previous settings stay in effect.
- Every applied change is logged at INFO and emitted as `config.applied` (summary of what was done).
- Runtime overrides (`account.set_dnd`, `account.set_sip_trace`) last until that account's *same field* changes
  in the file.
- Accounts are compared by `id`: a new id starts an account, a removed id (or `enabled = false`) unregisters,
  hangs up its calls and stops it. Changing an `id` is therefore remove + add.

## Editing from the GUI and the API

`config.edit` (JSON-RPC, MCP tool `config_edit`) and the GUI change the file through typed edits executed by one writer, the config
actor. A write

1. reads the file; with a `base_hash` (from `config.view`) that no longer matches, it fails with a **conflict**;
2. refuses if the file does not **parse** as it stands (fix it first; a file that parses but fails validation can still be edited, so an edit such as disabling the bad account can repair it, as long as the result validates);
3. changes exactly the named keys with `toml_edit`, so comments, blank lines, key order and quoting are preserved;
4. re-validates the result like the loader (parse, resolve paths, validate) and reports problems with the **path** of the key;
5. replaces the file atomically (temp file with the same permissions, fsync, re-read compare, rename, directory fsync);
6. reloads, then replies with the reload report and the new hash. The file watcher then sees an unchanged hash and does nothing.

Writing rules: an existing key is updated **in place** (its trailing comment stays); a key that is absent and whose new value equals its
effective default (the vendor profile's value, else the built-in default) is **not added**; new accounts and sections are **appended** with
only the keys that differ from the default; unknown sections and keys are rejected. `config.view` returns the effective config with
the origin of each key (`file`, `profile` or `default`; profiles fill only the keys the file omits, so a write pins a value), the file `hash`
and `path`. Passwords and the control token are never returned: an account's `password` is `{"set": true|false}`, and a masked value is
never accepted as input. Only the UI-editable account fields (`enabled`, `profile`, `display_name`, `username`, `auth_username`, `password`,
`password_store`, `domain`, `registrar`, `outbound_proxy`, `transport`, `local_port`, `tls_verify`, `tls_fingerprint`, `srtp`, `dnd`, `auto_answer`,
`answer_delay_ms`, `allow_intercom`, `auto_hold`, `mwi`, `sip_trace`, `pickup`, `pickup_prefix`, `pickup_replaces_target`), the
`[[account.blf]]` list (all its keys) and the keys of `[calls]`, `[ui]`, `[network]`, `[logging]`, `[audio]`
and `[general]` (`user_agent` and `log_level` only; `log_dir`, `log_retention_days` and `reconfig_force` are file-only) are editable this way. A read-only file is refused rather than replaced; everything else stays file-only. The full edit schema is in [API.md](API.md#configedit).

Downgrade note: a file with keys an older build does not know is rejected by that build (unknown keys are errors). The writer adds a key only
when it differs from its default.

### Reload classes

| Class | What happens | Active calls |
|---|---|---|
| **hot** | Applied immediately in place. | untouched |
| **next call** | Stored now; calls placed/answered afterwards use it. | untouched |
| **re-register** | A fresh REGISTER (with the new credentials/lifetime). An address-of-record change (`username`/`domain`) unregisters the old AOR first and re-subscribes BLF/MWI. | untouched |
| **rebuild** | The account's transport/sockets are torn down and rebuilt (unregister, then register again). **Deferred while the account has calls**: status shows `pending_rebuild` and it is applied when the last call ends, unless `[general] reconfig_force = true`. | hung up if forced |
| **rebind** | `[control]` listeners are restarted; only the endpoints that changed are replaced (others keep their connections). `runtime.json` is rewritten. | untouched |
| **restart** | Only takes effect at the next start of Siple (logged as "takes effect after restart"). | |

## `[general]`

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `user_agent` | string | `"Siple/<version>"` | hot | Default `User-Agent` header; an account may override it. |
| `log_level` | string | `"info"` | hot | A level (`trace\|debug\|info\|warn\|error\|off`) or full `EnvFilter` syntax (`info,siple_sip=debug`). A bare level expands to `<level>,rsipstack=warn`. Invalid values are rejected (a bare typo would otherwise silence logging). |
| `log_dir` | string | `""` | restart | Log directory. `""` = `<config dir>/logs`; relative paths are relative to the config dir. |
| `log_retention_days` | integer | `14` | restart | Delete log files older than this at startup; `0` = keep forever. |
| `reconfig_force` | bool | `false` | hot | Rebuild an account's transport on config change even while it has active calls (the calls are hung up). |

## `[logging]`

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `redact_srtp_keys` | bool | `false` | hot | Mask SRTP master keys (`inline:` of `a=crypto` lines) in the application log and SIP traces. Leave off while debugging key exchange. |
| `redact_credentials` | bool | `false` | hot | Mask the `response`, `nonce` and `cnonce` values of digest `Authorization` / `Proxy-Authorization` headers, the control token and every configured account password (4 characters or longer) in the application log and SIP traces. Username, realm and URI stay readable. |

### Logs and secrets

What reaches the logs is your choice, and today's defaults are kept: the per-account SIP trace is on (`sip_trace`), and neither
redaction option is on. Turn them on before sharing a log or a trace (the bug-report template asks for a trace with credentials redacted):

```toml
[logging]
redact_credentials = true   # digest responses, the control token and passwords
redact_srtp_keys = true     # SRTP master keys

[general]
log_level = "info"          # trace/debug show more; log_dir and log_retention_days as before
# and per account: sip_trace = false switches that account's wire trace off
```

Plaintext passwords are never written to a log or trace by Siple itself, whatever these options say (a digest `response` is a hash, not the
password); the options only mask what already reached a line. A password shorter than 4 characters is not searched for literally. There
is no telemetry; the only network activity Siple starts on its own is the optional update check ([`[updates]`](#updates)).

## `[control]`

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `unix_socket` | string | `""` | rebind | Unix socket (macOS/Linux). `""` = `<config dir>/control.sock`, or a short path if that is too long for `sun_path` (~100 bytes): `$TMPDIR` on macOS; on Linux `$XDG_RUNTIME_DIR`, else a private `/tmp/siple-<uid>/` (0700; refused if it is a symlink or owned by someone else). Mode 0600. |
| `named_pipe` | string | `""` | rebind | Windows named pipe; must start with `\\.\pipe\`. `""` = `\\.\pipe\siple-<user>-<hash of config dir>`. |
| `tcp_listen` | string | `""` | rebind | Loopback TCP listener, e.g. `"127.0.0.1:5079"`; `""` = off. Non-loopback addresses are rejected. The token from `runtime.json` is **always** required on TCP. |

## `[network]`

Network-change resilience. Accounts re-probe their route and re-register as soon as the network changes, instead of waiting for the next refresh or a REGISTER timeout. All options are **hot**: they apply to the running monitor and never re-register an account by themselves.

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `watch` | bool | `true` | hot | Watch OS address/route changes (macOS SystemConfiguration, Windows `NotifyIpInterfaceChange`, Linux rtnetlink) and detect sleep/wake (a wall-clock jump of more than 20 s). `false` leaves only the periodic probe and the API. A watcher that fails to start degrades to probe-only with one INFO log. |
| `probe_secs` | int | `30` | hot | Re-probe the route and DNS of every account (and the default route) every N seconds; only a *different* result acts. `0` = events only, else 5..3600. |
| `debounce_ms` | int | `1500` | hot | Coalesce change triggers for this long before acting (interfaces flap while Wi-Fi roams or a VPN connects). 100..60000. |
| `move_calls` | bool | `true` | hot | After a network change that moves the local address (or the registrar), keep established calls: rebind their RTP sockets to the new address and send a re-INVITE with the new Contact and SDP `c=` (hold direction and SRTP keys are kept). The call ends with cause `network_lost` when the peer answers 481 / 408, gives no final answer within 10 s, or refuses the re-INVITE. `false` ends established calls with `network_lost` at once. Calls that are still ringing are left alone. |

With no route to the registrar an account shows `waiting_for_network` (not `failed`) and sends nothing until the network returns. API: `network.refresh`, `account.reregister`; events `network.changed`, `account.reconnecting`, `call.reconnecting`, `call.reconnected`.

## `[media]`

Shared by all accounts. All options are **next call** (new offers/answers use them); the DTMF options
(`dtmf_mode`, `dtmf_duration_ms`, `dtmf_gap_ms`) also apply to calls in progress. Running audio streams are
not renegotiated.

| Option | Type | Default | Description |
|---|---|---|---|
| `rtp_port_range` | `[start, end]` | `[16384, 32767]` | Inclusive range for RTP/RTCP port pairs. `start` must be even and >= 1024; range at least 2 ports. Open it in firewalls. |
| `codecs` | array | `["opus","g722","pcmu","pcma"]` | Preference order: `opus`, `g722`, `pcmu`, `pcma`. At least one; no duplicates. |
| `jitter_min_ms` | int | `40` | Lower bound of the adaptive jitter buffer. Must not exceed `jitter_max_ms`. |
| `jitter_max_ms` | int | `200` | Upper bound; 1..1000. |
| `comfort_noise` | bool | `false` | Use CN (RFC 3389) if offered by the peer. |
| `symmetric_rtp` | bool | `false` | Latch the RTP destination to the source of authenticated incoming packets (NAT help). |
| `rtcp_mux` | bool | `false` | Offer/use `a=rtcp-mux`. |
| `rx_history_secs` | int | `60` | Seconds of received audio (kept at 16 kHz, about 32 KB/s per call) available to `call.listen` / `call.rx_audio`; 5..600. |
| `codec_answer_order` | `offer\|local` | `"offer"` | Answering: `offer` = first codec in the caller's offer that we support (desk-phone behaviour); `local` = first codec in our list present in the offer. |
| `dtmf_mode` | `rfc4733\|info\|inband\|auto` | `"auto"` | RFC 4733 telephone-event, SIP INFO (`application/dtmf-relay`), in-band tones, or auto (RFC 4733 if negotiated, else INFO). |
| `dtmf_duration_ms` | int | `100` | Duration of a sent digit; 10..10000. |
| `dtmf_gap_ms` | int | `70` | Silence between digits; at most 5000. |

## `[audio]`

Default devices (**hot**) and the backend switch (**restart**). The GUI's device pickers and `audio.set_devices` write `input`, `output` and `ringer` of
this section through `config.edit`'s write path (comments preserved), so this section is the only place a device is chosen (status shows the
effective device and its source: `config` or `default`). Device names are matched exactly as listed by `audio.devices`. An unplugged device
falls back to the system default until it returns; the setting is not changed.
Older builds kept a GUI choice in `state.toml` that overrode this section. On the first start of a build with this change such a choice is written
into `[audio]` once slot by slot exactly where it was in effect under that rule (a choice newer than the `[audio]` value it saw overwrites that value; an `[audio]` value that was newer stays) and the legacy data is removed from `state.toml`.

| Option | Type | Default | Description |
|---|---|---|---|
| `backend` | `auto\|none\|always` | `"auto"` | `none` never opens sound devices (headless servers, containers; calls still work with `silence`/`tone:`/`file:` sources). On Linux `auto` also turns audio off, with one INFO line (`status.audio.disabled_reason`), unless `/dev/snd/pcmC*`, `$XDG_RUNTIME_DIR/pulse/native`, `$XDG_RUNTIME_DIR/pipewire-0`, `$PULSE_RUNTIME_PATH/native`, `$PIPEWIRE_RUNTIME_DIR/pipewire-0` exists, or `PULSE_SERVER` / `PIPEWIRE_REMOTE` is set. `auto` probes **once at startup**, so a USB headset plugged in after boot, or PulseAudio configured only through `client.conf` `default-server`, is not seen: set `always` to skip the probe and always start the device thread (the macOS/Windows behaviour). `--no-audio` (both binaries) forces `none`. On Linux the device list hides ALSA pseudo-PCMs (`null`, `dmix`, `dsnoop`, `surround*`, `iec958`, `front`, rate/route plugins) and keeps `default`, `pipewire`, `pulse`, `sysdefault`, `hw`, `plughw`, `hdmi`; alsa-lib's own error messages go to the log at DEBUG (target `alsa`). Restart required. |
| `input` | string | `""` | Microphone; `""` = system default. |
| `output` | string | `""` | Speaker; `""` = system default. |
| `ringer` | string | `""` | Ringer; `""` = same as `output`. |

## `[calls]`

Multi-line behaviour shared by all accounts. **Hot**.

| Option | Type | Default | Description |
|---|---|---|---|
| `decline_code` | int | `486` | SIP status the GUI's Reject sends for an incoming call while another call is live on any account (call waiting; the far end of the same call, such as a self-call between two local accounts, does not count); a lone call is rejected with 486. `call.reject` takes an explicit `code`. 400-699. |
| `hold_scope` | `all\|account` | `"all"` | Which calls an auto-hold puts on hold when a call is answered, dialed or activated (`call.activate`). `all`: every account's device-bound calls, so at most one device-bound call is un-held in the process. `account`: only the calls of the account that answers/dials; set this for automation and for two local accounts that call each other. |
| `resume_after_failed_transfer` | bool | `true` | A transfer that failed for good (final non-2xx, REFER refused, local error) takes the original call off hold again when the transfer put it on hold and no other device line is live. A transfer with no result after 32 s (`unconfirmed`) is never resumed automatically. |

Always exempt from auto-hold: calls that use no audio device (agent/automation sources and sinks), and the other leg
of a call between two accounts of the same process (equal SIP Call-ID). A call that the remote has put on hold
(`remote_held`) can be auto-held like an active one. Per-account `auto_hold = false` exempts that account's calls from
being auto-held and stops it from triggering an auto-hold. An explicit `call.activate`, or `call.answer` with
`mode = "hold_others"`, is a user request rather than an auto-hold: it ignores `auto_hold` and holds every other
device-bound call in scope (same exemptions as above).

## `[ui]`

Desktop UI settings, all **hot** with no effect on accounts or calls. The Settings page of the GUI writes this table; the GUI starts to read
each key as the matching feature arrives. A key you leave out keeps its default.

| Option | Type | Default | Description |
|---|---|---|---|
| `theme` | `system\|light\|dark` | `"system"` | Colour theme. |
| `density` | `comfortable\|compact` | `"comfortable"` | Row density. |
| `reduced_motion` | `system\|on\|off` | `"system"` | Reduce animation. |
| `blf_view` | `keys\|lamps\|list` | `"keys"` | Layout of monitored extensions. |
| `blf_click` | `dial\|prefill` | `"dial"` | Click on an idle BLF: call at once, or fill the dial field. |
| `blf_click_in_call` | `menu\|consult\|blind` | `"menu"` | Click on a BLF during a call. |
| `blf_click_ringing` | `pickup\|call` | `"pickup"` | Click on a ringing BLF (pickup applies only where a pickup method is configured). |
| `blf_group_by` | `account\|group\|none` | `"account"` | BLF grouping. |
| `transfer_enter` | `consult\|blind` | `"consult"` | Enter in the transfer composer. |
| `tray` | bool | `true` | Tray / menu-bar mode. |
| `close_to_tray` | bool | `true` | Closing the window hides it. |
| `hide_from_taskbar` | bool | `true` | No taskbar button / Dock icon while hidden. |
| `start_hidden` | bool | `false` | Start with the window hidden. An in-place update restart ignores this and restores the window state at the moment of the restart: hidden only if it was hidden to the tray (and a tray exists), otherwise shown. |
| `show_on_incoming` | bool | `true` | Raise the window on an incoming call. |
| `show_on_answer` | bool | `false` | Raise the window when a call is answered. |
| `notifications` | bool | `true` | Desktop notifications. |
| `handle_links` | bool | `true` | Handle `sip:`, `sips:`, `tel:`, `callto:` and `siple:` (setup) links. |
| `launch_at_login` | bool | `false` | Start Siple when you log in (macOS LaunchAgent, Windows `Run` key, Linux XDG autostart). The AppImage edition starts the `.AppImage` file. Combine with `start_hidden = true` to start in the tray. |
| `developer_tools` | bool | `false` | Show the Log pane and the Open logs / Open config buttons in the window footer, and the registration / subscription times (registered, last renewal, expires) in the account and BLF key tooltips. The control API and MCP stay on either way. |

Desktop shell notes (tray, links, login item):

- **Tray / close-to-tray.** With `tray = true` and `close_to_tray = true` closing the window hides it; only the tray menu's *Quit* (or Cmd+Q on macOS, Ctrl+Q on Windows / Linux, SIGTERM) exits. While hidden there is no Dock icon (macOS, `hide_from_taskbar`) and no taskbar button (Windows, Linux). `hide_from_taskbar = false` keeps the Dock icon on macOS and minimizes to the taskbar elsewhere. `tray = false` makes close quit. On Linux the tray is a StatusNotifierItem (KDE, XFCE, GNOME with the AppIndicator extension); without a host, closing minimizes instead so the app stays reachable. On Wayland sessions the window cannot be unmapped (winit limitation), so closing minimizes there too and `start_hidden` has no effect; start it under XWayland for full hide-to-tray: `env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET siple` (winit uses Wayland whenever either variable is set).
- **Links.** `sip:`, `sips:`, `tel:` and `callto:` links put the number in the dial field and raise the window; they never dial. `siple:provision` setup links open a review screen and change nothing until the user continues, types the password and tests the account (see [setup links guide](../website/guide/setup-links.md)). A second `siple <link>` hands the link to the running instance. Windows: Siple registers itself under HKCU as a candidate for `sip`, `sips`, `tel` and `callto` (choose it in Settings > Default apps) and registers `siple` directly, since nothing else claims it; `handle_links = false` removes both registrations. macOS and Linux register links through the app bundle / `.desktop` file at install time (Linux: `xdg-mime default io.zyno.siple.desktop x-scheme-handler/sip`, and likewise for `sips`, `tel`, `callto` and `siple`), so the key has no runtime effect there.
- **Launch at login.** `launch_at_login = true` writes `~/Library/LaunchAgents/io.zyno.siple.plist` (macOS), `HKCU\...\Run` (Windows) or `~/.config/autostart/io.zyno.siple.desktop` (Linux) for the running executable (the AppImage edition: the `.AppImage` file, not its temporary mount); `false` removes it. A non-default `--config` profile has its own entry (the id plus a short hash of the config path) that starts that profile, so profiles never touch each other's entry. Pair with `start_hidden = true`. In the Mac App Store and Microsoft Store editions both `handle_links` and `launch_at_login` are no-ops (logged at info): the package manifest declares the links, and MSIX users switch the startup task on in Windows Settings > Apps > Startup. In the Flatpak edition `launch_at_login` goes through the desktop's Background portal: the desktop may ask once, the portal writes `~/.config/autostart/io.zyno.siple.desktop` on the host (`flatpak run --command=siple io.zyno.siple`), there is one entry for the app (so the profile that enabled it last wins), and `false` removes it if this profile registered it; a start with the default `false` asks nothing. `handle_links` is a no-op there, as on the Linux tarball and AppImage.
- **Notifications.** Incoming (with Answer / Decline on Linux), missed calls and transfer results appear while the window is not focused. macOS needs the signed app bundle (an unbundled binary logs once and skips them). The control API can show or hide the window (`ui.show`, `ui.hide`, `ui.state`, `ui.attention`, `app.open_uri`) when a GUI runs.

`[ui.shortcuts]` maps an action name to a key chord such as `answer = "Mod+Shift+A"`; actions not listed keep their defaults. **Hot** (read every frame; Settings > Shortcuts shows the chord in effect). An unknown action name or a malformed chord is a config error.

| Action | Default | Does |
|---|---|---|
| `answer` | `Mod+Shift+A` | Answer the selected ringing call, else the newest ringing one (with another line live: answer and hold it) |
| `hangup` | `Mod+Shift+E` | Hang up the selected line; cancel a dialing call or consult (returns to the original); decline a ringing call |
| `mute` | `Mod+Shift+M` | Mute / unmute |
| `hold` | `Mod+Shift+H` | Hold / resume (resume swaps lines, never two live calls) |
| `transfer` | `Mod+Shift+T` | Open the transfer composer |
| `line_1` ... `line_9` | `Mod+1` ... `Mod+9` | Select that line slot; a held line becomes the active line (a connected consult swaps) |
| `line_prev`, `line_next` | `Mod+[`, `Mod+]` | Select the previous / next line (selection only, wraps) |
| `focus_dial` | `Mod+L` | Focus the dial field (opens it when collapsed) |
| `filter_extensions` | `/` | Focus the extensions filter (ignored while typing in a field) |
| `settings` | `Mod+,` | Open / close Settings |

Chord syntax: modifiers and one key joined by `+`, case-insensitive: `Mod+Shift+A`, `Mod+,`, `Alt+F4`, `/`. Modifiers: `Mod` (Cmd on macOS, Ctrl elsewhere), `Ctrl`, `Alt` (Option), `Shift`, `Cmd` (the macOS Command key itself). Keys: a letter or digit, one of ``, . / ; ' [ ] \ - = ` ``, `Plus`, `Enter`, `Tab`, `Space`, `Escape`, `Backspace`, `Delete`, `Insert`, `Home`, `End`, `PageUp`, `PageDown`, `Up`, `Down`, `Left`, `Right` or `F1`-`F12`. Shift and Alt must match exactly for letters, digits and named keys (`Mod+M` is not `Mod+Shift+M`); on punctuation keys such as `/` Shift is ignored unless the chord names it (it is part of typing `/` on many layouts). Digit and bracket chords also match the physical key, so `Mod+]` works where the layout moves it. These are errors: `Escape` as the key (Esc only closes popovers), a digit with no Mod / Ctrl / Cmd / Alt (it is typed), `Mod+C` / `Mod+V` / `Mod+X` (clipboard), and a chord that is the same key press as another action's effective chord (`Ctrl+X` is `Mod+X` off macOS, `Cmd+X` is on macOS). `Cmd` off macOS never fires and is a warning; Settings > Shortcuts lists problems. Remove a bad entry with `config.edit` `reset_section ui shortcuts.<name>`.

Typing: digits, `*` and `#` send DTMF while the stage shows an Active line and no text field has focus; otherwise they type into the dial field (opening it when collapsed). Esc only closes popovers and the transfer composer; it never hangs up or cancels a consult. OS-wide (global) hotkeys are not available.

## `[speech]`

Optional external text-to-speech and speech-to-text commands, used by `call.say`, `call.listen{transcribe}` and
`speech.*`. **Hot** (each job snapshots the section when it starts). Off by default; nothing is bundled.
The Mac App Store and Flatpak editions cannot run external commands: the section is still accepted (with a warning in the log),
but those methods fail with `unsupported` and Settings shows "not available in this edition".

| Option | Type | Default | Description |
|---|---|---|---|
| `tts_command` | array | `[]` | argv of the TTS program; `[]` = TTS off. Placeholders: `{text_file}` (UTF-8 text file), `{out}` (WAV to write), `{voice}`. If `{out}` is not referenced the WAV is read from stdout. |
| `tts_timeout_ms` | int | `30000` | Kill the process (group) after this long; 1000..120000. |
| `tts_voice` | string | `""` | Default voice for `{voice}`. |
| `stt_command` | array | `[]` | argv of the STT program; `[]` = STT off. Placeholders: `{in}` (16 kHz mono 16-bit WAV), `{lang}`, `{out_base}`, `{out_txt}`. If `{out_txt}` is not referenced the transcript is read from stdout. |
| `stt_timeout_ms` | int | `60000` | As `tts_timeout_ms`; 1000..120000. |
| `stt_language` | string | `"en"` | Default language for `{lang}`. |

Rules: no shell is ever involved; placeholders are replaced inside argv elements only, `{{` and `}}` produce literal
braces, and an unknown placeholder is a config error. `tts_voice`, `stt_language` and any per-request voice/language
must match `[A-Za-z0-9][A-Za-z0-9_.+:-]{0,63}` (no leading `-`). Text to speak is passed on stdin and in `{text_file}`,
never in argv.

## `[updates]`

The update check: Siple tells you when a newer release exists. **Hot.** A pill in the header (and Settings > Advanced) offers it.
Direct downloads (zip, dmg, tarball, AppImage) can also **install it in place**, when you or a script or agent you run chooses to (see
"In-place install" below); otherwise the pill links to the release page. The Mac App Store and Microsoft Store editions open their
store page and never install anything themselves. Where the check looks depends on how Siple was installed. `check = false` turns
all of it off, on every edition.

| Option | Type | Default | Description |
|---|---|---|---|
| `check` | bool | `true` | Look for a newer release about 30 seconds after start (not when a check found nothing newer less than 24 hours ago), then every 24 hours of wall-clock time, so time spent asleep counts (and on `app.check_updates` or "Check now"; a manual check restarts the 24 hours). |
| `install` | bool | `true` | Direct downloads only. Allow the in-place install: the offer then downloads, verifies and installs the new version when you choose it (the pill, or `app.install_update`). `false`: the offer opens the release page. Hot: turning it off stops an install that has not replaced any file yet. Ignored by the store editions, which have no installer. `check = false` also stops installs. |
| `channel` | `stable\|prerelease` | unset | GitHub (direct) builds only; the stores always follow their single published track. `stable` offers final releases only; `prerelease` also offers release candidates (`-rc.N`). Unset = automatic: `prerelease` while the running build is itself a release candidate, otherwise `stable`. `reset_section updates channel` returns to automatic. |

What is sent, by edition:

- **Direct download** (zip, dmg, tarball, AppImage, package manager): one anonymous HTTPS `GET https://api.github.com/repos/zyno-io/siple/releases?per_page=20`
  with the headers `User-Agent: Siple/<version>` and `Accept: application/vnd.github+json` (plus `If-None-Match` with the ETag of the
  previous answer, so an unchanged list costs almost nothing). GitHub sees your IP address, as any web server does.
- **Mac App Store**: one anonymous HTTPS `GET https://itunes.apple.com/lookup?id=6836164262` (Apple's public lookup of Siple's own
  listing), about once a day, with only the header `User-Agent: Siple/<version>`. Apple sees your IP address. The answer is used only
  when it is Siple's listing (id and bundle id match) and its minimum macOS is at most yours; links in it are never followed. A
  TestFlight build makes no request. Apple's listing can lag a release by up to a day.
- **Microsoft Store**: Siple asks Windows' Store service whether an update of its own package is waiting. Windows contacts Microsoft as
  it does for the Store's own updates; Siple adds nothing and sends no request of its own. The Store does not name a version, so the
  pill reads "Update". A copy that was not installed from the Store (sideloaded) does not check.
- **Flatpak**: nothing. Flatpak updates Siple from Flathub (`disabled_reason: managed_by_flatpak`).

Nothing else is sent: no identifier, account, install ID or usage data, and no cookies. Failures (offline, rate limited, odd
answer) are logged and shown in Settings; they never raise a dialog. API: `app.update_status` (with `source`, `action`, `store_url`,
`disabled_reason`, `install`), `app.check_updates`, `app.install_update`, `app.cancel_update`, the events `app.update_available`,
`app.update_progress` and `app.updated`, and the `update` summary of `status`. The user guide has a page, "Updates".

### In-place install (direct downloads)

Only when you choose it (the pill's **Install and restart**, or an agent calling `app.install_update`), and only with `check` and
`install` on, Siple downloads the new version from the project's GitHub release, over HTTPS, each request with only
`User-Agent: Siple/<version>`: `SHA256SUMS`, `SHA256SUMS.minisig` and the one archive for your system
(`Siple-<version>-macos.zip`, `-windows-x64.zip` / `-windows-arm64.zip`, `-linux-x86_64.tar.gz` / `-linux-aarch64.tar.gz`, or `-linux-x86_64.AppImage` / `-linux-aarch64.AppImage` for the AppImage edition; the one of the running CPU) from
`https://github.com/zyno-io/siple/releases/download/v<version>/<file>`. That URL is built from the version and file names the
release list reported, never taken from the list; GitHub redirects once to its asset hosts (at most three redirects, HTTPS only,
GitHub hosts only). Nothing is sent that identifies you beyond the IP address any download shows.

Before anything is replaced, all of this must hold, otherwise nothing changes and the pill offers the release page:

1. `SHA256SUMS` carries a valid [minisign](https://jedisct1.github.io/minisign/) signature (`SHA256SUMS.minisig`) of Siple's release
   key (compiled into the app, also `packaging/minisign.pub`), and its signed comment names this release (an old release's list
   cannot be replayed). A release without `SHA256SUMS.minisig` is not installed in place, on any system.
2. The archive's SHA-256 equals its entry in that list, and its size equals what the release lists.
3. The new app passes your system's check of Siple's publisher: macOS, the Developer ID code signature of the app and of
   `siple-cli` (our team) plus Apple's notarization; Windows, Authenticode with a valid timestamp from the pinned publisher
   (Signal24 LLC through Microsoft Artifact Signing); Linux, the signed list only (the files are not otherwise signed), plus a test
   run of the new `siple` and `siple-cli` (`--version`), which must print the expected version (AppImage: a test run of the new
   file, extracted without FUSE into the staging folder, as `siple` and as `siple-cli`).
4. The archive layout is exactly what a release contains (no extra files, no paths outside the install folder, no links). The
   AppImage download must be a type 2 AppImage of the running CPU (x86-64 or aarch64).

The running copy must pass the same publisher check, so a development build or an unsigned copy never replaces itself.

Siple restarts into the new version only when no call, transfer or echo test is active, and has been quiet for 5 seconds
(`restart: when_idle`, the default); the files are replaced as late as possible, just before the restart, so the running copy is
never modified during a call. The GUI quits as the tray's Quit does and starts the new version, which waits for the old process
to end (`--wait-pid`). Your config, accounts, logs and keychain entries are never touched; the app keeps its path, so the login
item, the `sip:` handler and firewall rules keep working. Replaced files stay (macOS: the old app in a hidden temporary folder;
Windows and Linux: `*.old-<number>` files; the AppImage is replaced by one rename and the old one is kept as `<name>.old-<number>` next to it) until the new version has run for a minute, then they are removed; if the new version
does not start, the previous copy is still there. Linux keeps the folder name `siple-<old version>`.

In-place install is **not possible**, and the offer opens the release page with a reason (`install.reason` of
`app.update_status`, shown in the pill), when Siple would need more rights than yours or is not in a place it can replace:
installed with a package manager (Homebrew, winget, Scoop, Chocolatey, Nix), in Program Files or `/usr` or another folder your
user cannot write, owned by another user, run as root, run from a disk image or a quarantined download (macOS), in a container,
Flatpak, Snap, headless (no window to restart into), a development build, or `install = false`. Siple never asks for
administrator rights. The Mac App Store, Microsoft Store and Flatpak editions contain no downloader or installer: `app.install_update`
answers `unsupported` there.

## `[[account]]`

Repeat the table for each account. Only `id`, `username` and `domain` are needed for a working account; the shipped
example is `enabled = false` so a fresh install never tries to register.

### Identity and registration

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `id` | string | (required) | identity | Unique (case-insensitive); `[A-Za-z0-9_.-]` only. Used in API calls and file names (`logs/sip/<id>/sip.<date>.log`). |
| `enabled` | bool | `true` | disable: immediate; enable: rebuild | `true -> false` disables at once (calls hung up, binding removed). `false -> true` starts the account (a rebuild). |
| `profile` | `generic\|yealink\|polycom\|cisco\|grandstream` | `"generic"` | hot | Vendor preset, see [Vendor profiles](#vendor-profiles). Fills the options the file leaves out; explicit options win. |
| `display_name` | string | `""` | hot | Display name in the From header. |
| `username` | string | (required if enabled) | re-register | User part of the AOR. No whitespace or `@`. |
| `auth_username` | string | `""` | re-register | Digest username; `""` = `username`. |
| `password` | string | `""` | re-register | Digest password. Redacted (`***`) in `config.get` / logs. Must be empty with `password_store = "keyring"`. |
| `password_store` | `file\|keyring` | `"file"` | re-register | Where the password lives: `file` = the `password` key above (default); `keyring` = the system credential store, see [Passwords in the system keyring](#passwords-in-the-system-keyring). |
| `domain` | string | (required if enabled) | re-register | Domain of the AOR, bare host. Also the registrar if `registrar` is empty; in that case a change also moves the next hop and is a **rebuild**. |
| `registrar` | string | `""` | rebuild | Next hop / request-URI host (`host[:port]`); `""` = `domain`. |
| `register` | bool | `true` | re-register | `false` = peer mode: no REGISTER, but calls still work (direct IP / PBX with IP auth). |
| `expires` | int | `300` | re-register | Requested registration lifetime in seconds; 10..86400. |
| `user_agent` | string | `""` | hot | Overrides `[general].user_agent`; `""` = inherit. |
| `extra_headers` | table | `{}` | hot | Headers added to outgoing requests, e.g. `{ "X-Test" = "1" }`. Names must be valid tokens; `Via`, `From`, `To`, `Call-ID`, `CSeq`, `Contact`, `Content-Length` are reserved. |

### Transport, NAT and TLS

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `transport` | `udp\|tcp\|tls\|auto` | `"udp"` | rebuild | Signalling transport. `auto` picks it from DNS SRV, see [`transport = "auto"`](#transport--auto). |
| `local_port` | int | `0` | rebuild | Local SIP port; `0` = ephemeral. Two enabled accounts cannot share a non-zero port (UDP and TCP/TLS are separate port spaces). |
| `bind_address` | string | `""` | rebuild | Local IP to bind and advertise; `""` = auto (interface toward the next hop). Must be an IP. |
| `public_address` | string | `""` | rebuild | IP/host to advertise in Via/Contact/SDP without binding to it (static NAT). |
| `rewrite_contact` | bool | `true` | re-register | Learn the registrar-observed IP/port from the REGISTER response's Via `received`/`rport` and immediately re-register with that Contact. `false` keeps the configured Contact. An explicit `public_address` disables automatic rewriting. |
| `outbound_proxy` | string | `""` | rebuild | Forced next hop for all requests, `host[:port]`. |
| `outbound_proxy_route` | bool | `false` | rebuild | Also add `Route: <proxy;lr>`. |
| `keepalive_secs` | int | `30` | hot | NAT keep-alive interval; `0` = off, else 5..3600. |
| `keepalive_method` | `crlf\|options` | `"crlf"` | hot | Double-CRLF ping or SIP OPTIONS. `options` also detects dead flows: any SIP response counts as alive. |
| `keepalive_fail_threshold` | int | `2` | hot | With `keepalive_method = "options"`: this many consecutive unanswered keep-alives mark the flow dead; the account re-probes and re-registers at once. `0` = never, max 20. |
| `use_sips_uri` | bool | `false` | rebuild | On TLS use a `sips:` Contact instead of `sip:...;transport=tls`. |
| `tls_verify` | bool | `true` | rebuild | Verify the server certificate (OS trust store + `tls_ca_file`). `false` **ignores certificate errors** (test PBXs). An untrusted certificate fails registration with the problem `tls_untrusted`; `account.test` also reports its fingerprint, ready for `tls_fingerprint`. |
| `tls_ca_file` | string | `""` | rebuild | PEM file with extra trusted CAs. |
| `tls_server_name` | string | `""` | rebuild | SNI / verification name override (registrar/proxy only). |
| `tls_fingerprint` | string | `""` | rebuild | **Pin** the server's leaf certificate: SHA-256 of its DER as 64 hex digits (colons optional). That exact certificate is accepted regardless of chain, expiry or name (handshake signatures are still checked); `tls_verify` and `tls_ca_file` are then not consulted. Get it with `openssl x509 -in pbx.pem -noout -fingerprint -sha256`. |
| `tls_cert` / `tls_key` | string | `""` | rebuild | PEM certificate and key for the TLS listener / client auth; `""` = self-signed. Must be set together. A missing file is a warning, not an error. |

Changing `rewrite_contact` clears the learned Contact and re-registers over the existing transport; active calls continue. With rewriting enabled, later registration refreshes keep using the learned Contact until the connection changes. The observed public address remains available in account status when rewriting is disabled.

### Passwords in the system keyring

`password_store = "keyring"` keeps an account's password in the operating system's credential store instead of `config.toml`. It is
opt-in and selected by the file itself (the only setting whose *value* lives outside the file); the default stays the password in the
0600 file.

- Backends: macOS Keychain, Windows Credential Manager, Linux Secret Service (GNOME Keyring, KWallet) over a pure-Rust D-Bus
  client; Siple does not link libdbus.
- Entry: service `io.zyno.siple`, user `account:<id>`.
- With `keyring`, `password` must be empty (a validation error otherwise). Set the password with the `account.set_password` API
  method (or the settings UI); it writes the entry, then reloads so the account registers with it.
- The loader reads the entry **once per reload** into memory, never per REGISTER (a Keychain access can prompt). Disabled accounts are
  not read.
- **No credential store** (headless Linux, Docker) or no entry: that account is switched off in memory and the reload reports
  `account <id>: keyring unavailable; set password_store = "file"` (or `no password in the keyring ...`) in `status.config.last_error`,
  the log and the `config.error` event. There is never a silent fallback to a plaintext password. Other accounts are unaffected.
- `config.edit` keeps the entry in step: `set_account password_store` moves the existing password between the file and the entry,
  `rename_account` moves the entry, `remove_account` deletes it, and `add_account` with `password_store = "keyring"` stores the draft's
  `password` in the entry instead of the file.
- macOS ties keychain access to the code signature: unsigned development builds ask again after every rebuild; signed builds do not.
- Passwords never appear in logs, API results or error messages.

### `transport = "auto"`

The account selects its transport from DNS SRV records of the next hop (the outbound proxy, else the registrar, else the domain): the
first of `_sips._tcp` (TLS), `_sip._tcp` (TCP) and `_sip._udp` (UDP) that has a record wins, with that record's targets and ports.
Without any SRV record it uses UDP on port 5060. There is no NAPTR lookup. An IP address or an explicit `host:port` skips SRV and uses
UDP.

- `status` / `account.get` report `transport: "auto"` and `transport_effective` (`udp|tcp|tls`) once a transport was picked; the GUI shows
  it as "TLS (auto)".
- The choice is made when the transport is built and again whenever the network is re-probed ([`[network]`](#network)): a changed answer
  rebuilds the transport and re-registers, like any other change of next hop. While the account has calls the transport is kept; the next
  probe without calls re-selects.
- With `auto` and `tls_verify`, `tls_ca_file`, `tls_fingerprint` etc. apply only if TLS is the one picked.

### Media and SRTP

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `srtp` | `off\|optional\|mandatory` | `"optional"` | next call | `off`: plain RTP only (SAVP-only offers get 488). `optional`: offer RTP/AVP plus `a=crypto` (OSRTP-style) and use SRTP if the peer does too; if the PBX refuses the offer (488/415/606) the call is retried once with plain RTP (`srtp_fallback` in the call snapshot); once the plain retry gets a final 2xx the refusal is remembered per destination (next hop `host:port[:transport]`, max 64): later calls to it offer plain RTP first, and if that is refused (488/415/606) they retry once with the OSRTP offer and forget the destination when that retry gets a final 2xx (a redirect re-evaluates for the new destination; calls cancelled while ringing change nothing); `srtp_optional_downgraded` in `status` / `account.get` lists the remembered destinations; cleared when `srtp`, `registrar`, `outbound_proxy`, `domain`, `transport`, `codecs` or `srtp_suites` change, and on an account rebuild or re-probe rebuild. Some PBXes (FreeSWITCH without `rtp_allow_crypto_in_avp=true`) build the callee leg before refusing, so the first call rings the callee twice: against such a PBX use `srtp = "off"` or `"mandatory"`. `mandatory`: RTP/SAVP only; offers without acceptable crypto get 488. |
| `srtp_suites` | array | `["AES_CM_128_HMAC_SHA1_80","AES_CM_128_HMAC_SHA1_32"]` | next call | Offered/accepted SDES suites in preference order; also `AES_256_CM_HMAC_SHA1_80/32` (alias `AES_CM_256_...`). Required (non-empty) unless `srtp = "off"`. |
| `codecs` | array | `[]` | next call | Codec order override; `[]` = inherit `[media].codecs`. |
| `dtmf_mode` | string | `""` | next call | `rfc4733\|info\|inband\|auto` override; `""` = inherit `[media].dtmf_mode`. |
| `audio_source` | string | `"device"` | next call | Default transmitted audio: `device` (microphone), `silence`, `tone:<hz>`, `file:<path.wav>` (looped). For headless/CI runs use `tone:` or `silence`. |
| `audio_sink` | string | `"device"` | next call | Default received audio: `device` (speaker), `null`, `record:<path.wav>`. |

### Call behaviour

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `dnd` | bool | `false` | hot | Do not disturb: reject incoming calls (`call.rejected` with reason `dnd`). Also settable at runtime (`account.set_dnd`) until this field changes. |
| `dnd_reject_code` | int | `486` | hot | SIP status for DND rejects; 400..699. |
| `auto_answer` | bool | `false` | hot | Answer incoming calls automatically. |
| `answer_delay_ms` | int | `0` | hot | Delay before auto-answering; at most 60000. |
| `allow_intercom` | bool | `false` | hot | Also auto-answer on `Call-Info: answer-after` / `Alert-Info` intercom/paging requests. |
| `auto_hold` | bool | `true` | hot | Answering or dialing puts the other active device-bound calls on hold (all accounts' with `[calls] hold_scope = "all"`, this account's with `"account"`). `false` also exempts this account's calls from being auto-held. |
| `hold_style` | `sendonly\|inactive` | `"sendonly"` | hot | SDP direction when we hold. `sendonly` becomes `inactive` automatically if the remote already holds us. |
| `hold_send_cn` | bool | `false` | hot | Send periodic comfort noise while holding (keeps PBX media timeouts quiet). |
| `follow_redirects` | bool | `true` | hot | Follow 3xx on outgoing calls (max 3 hops). |
| `auth_foreign` | bool | `false` | hot | Answer digest challenges (401/407) from servers that are not your provider. Default: a challenge is answered only when the address the request was actually sent to belongs to your provider: the `outbound_proxy` / registrar / `domain` addresses and every address the outbound proxy / registrar resolved to (all SRV / A results, kept while the runtime for those hosts lives; the AOR `domain` is not resolved when a separate `registrar` is set). The request-URI, Contact and Record-Route are never trusted. Otherwise the call / request fails with `Authentication refused: <ip> is not your provider (set auth_foreign = true ...)` (problem kind `auth_foreign_refused`) and no `Authorization` header is sent. `true` = answer everything (a rogue host can then collect a crackable digest of your password); needed only if your provider uses servers outside that set, e.g. an unrelated Record-Route edge. |
| `max_calls` | int | `4` | hot | Concurrent calls; further INVITEs get 486. At least 1. |
| `prack` | `disabled\|supported\|required` | `"supported"` | hot | Reliable provisional responses (RFC 3262) when we answer. `disabled`: never (`420` to `Require: 100rel`, `100rel` not advertised). `supported`: reliable 180 only if the caller sends `Require: 100rel`. `required`: reliable 180 whenever the caller lists `100rel` in `Supported` or `Require`. The 200 waits for the PRACK; the 180 is retransmitted at T1, 2*T1, ... and the call is rejected with 504 after 64*T1 without PRACK. Outgoing calls PRACK reliable 18x unless `disabled`. |
| `session_timers` | bool | `true` | hot | RFC 4028. Advertises `Supported: timer`; sends `Session-Expires`/`Min-SE` on outgoing calls; as UAS honours the caller's interval (`422` + `Min-SE` below `min_se`) and picks the refresher (the caller if it supports `timer`, else us). The refresher role refreshes at half the interval with UPDATE (if the peer's `Allow` has it) or a re-INVITE with an unchanged SDP (`o=` version stays); a peer that fails to refresh before `interval - min(32, interval/3)` gets a BYE (`Reason: SIP;cause=408`, call ends with code 408). `false`: `Session-Expires` ignored, `Require: timer` gets `420`. |
| `session_expires` | int | `1800` | hot | `Session-Expires` requested on outgoing calls, seconds; 1..86400 and at least `min_se`. |
| `min_se` | int | `90` | hot | `Min-SE`: shortest interval accepted (and sent on outgoing calls), seconds; 1..86400. RFC 4028 sets a floor of 90; lower values are allowed so tests can run short sessions. |
| `accept_refer` | bool | `true` | hot | Accept incoming REFER (act as transferee). |
| `attended_refer_target` | `aor\|contact` | `"aor"` | hot | Identity in the `Refer-To` URI of attended transfers. |
| `attended_refer_on` | `original\|consult` | `"original"` | hot | Dialog carrying the attended-transfer REFER (Replaces points at the other call). |

### Subscriptions, tracing

| Option | Type | Default | Reload | Description |
|---|---|---|---|---|
| `mwi` | `subscribe\|unsolicited\|off` | `"subscribe"` | hot (resubscribes) | `subscribe`: SUBSCRIBE to `message-summary` (unsolicited NOTIFYs are accepted too). `unsolicited`: only accept unsolicited NOTIFYs. `off`: ignore MWI. |
| `mwi_uri` | string | `""` | hot (resubscribes) | URI to subscribe on; `""` = own AOR. |
| `blf_expires` | int | `600` | hot (resubscribes) | Requested expiry of BLF subscriptions; 10..86400. |
| `pickup` | `off\|prefix\|replaces` | `"off"` | hot | Directed pickup of ringing BLF extensions (`call.pickup`, the GUI "Pick up" action). `off`: not offered. `prefix`: dial `<pickup_prefix><extension>` on this account (or the entry's `pickup_target`). `replaces` (**experimental**, PBX support varies): INVITE the monitored URI with `Replaces`. Per-entry override: `[[account.blf]] pickup`. No resubscribe. |
| `pickup_replaces_target` | `caller\|monitored` | `"caller"` | hot | With `pickup = "replaces"` (experimental): INVITE the caller (RFC 5359) or the monitored phone. |
| `pickup_prefix` | string | `"**"` | hot | Feature code dialled before the extension with `pickup = "prefix"` (`**` is the FreePBX / FreeSWITCH directed-pickup code; Asterisk setups often use `*8` for group pickup). No whitespace. |
| `sip_trace` | bool | `true` | hot | Write the wire-level SIP trace `<log dir>/sip/<id>/sip.<date>.log` (retransmits, keep-alives and unparsable messages included) and keep a per-call ring for `call.sip_trace`. Runtime override: `account.set_sip_trace`. |

### `[[account.blf]]`

Busy-lamp-field watch list; add the tables directly after the account they belong to. Adding, removing and
editing entries is **hot** (the list is diffed by `uri`). Only a changed `uri` resubscribes; `label`, `group` and
all `pickup*` keys apply immediately without touching the subscription.

| Option | Type | Default | Description |
|---|---|---|---|
| `uri` | string | (required) | Extension or full SIP URI to monitor (`Event: dialog`). Unique within the account; no whitespace. |
| `label` | string | `""` | Human-readable label shown in the GUI and status. |
| `group` | string | `""` | Group the entry belongs to in the GUI and in `blf.list`; `""` = grouped by account. |
| `pickup` | `""\|off\|prefix\|replaces` | `""` | Per-entry override of the account's `pickup`; `""` = inherit. `off` hides pickup for this entry. |
| `pickup_prefix` | string | `""` | Per-entry override of the account's `pickup_prefix`; `""` = inherit. |
| `pickup_target` | string | `""` | With `prefix`: dial exactly this instead of `<pickup_prefix><extension>` (queue pickup codes, park slots). Extension, `user@host` or `sip:` URI. |

### Directed pickup

Pickup is offered only while the entry is **ringing** (an incoming early dialog at the monitored phone), its
state is fresh (not `stale`, subscription `active`) and the resolved method (entry override, else account) is not
`off`; otherwise `call.pickup` fails with `invalid_state`. It
goes through the normal dial path, so `auto_hold` applies as for any call; a refusal (404, 480, 486, ...) ends the call
with that code.

- **`prefix`** dials `pickup_target` if set, else `<pickup_prefix><extension>` on the entry's account. The extension is
  the user part of `uri` (`sip:1004@pbx` and `1004` both give `1004`). Example: `**1004`.
- **`replaces`** (**experimental**, not interop-verified): INVITE with `Require: replaces` and
  `Replaces: <call-id>;to-tag=<remote-tag>;from-tag=<local-tag>;early-only`, built from the `dialog-info` of the
  ringing dialog. The tags are those of the UA that originated the dialog (RFC 3891 reads them from the receiver's
  point of view; dialog-info reports the monitored phone's view, so its `remote-tag` is the originator's local
  tag). By default (RFC 5359 §2.16) the INVITE goes to the **caller**: the `<remote>` target of the dialog-info,
  falling back to the remote identity; RFC 3891 §4 forbids replacing an early dialog on a UA that did not originate
  it. `pickup_replaces_target = "monitored"` sends it to the monitored URI instead (B2BUA setups where the PBX
  originated the callee leg): only the Request-URI changes, the tags stay the same. Call-id, tags and target must be
  well-formed (RFC 3261 `word` / `token`, no control or format characters; NOTIFY content is untrusted) and the
  notifier must send them; otherwise the pickup fails with `invalid_state`. Use `prefix` there.
- **Routing.** Every pickup is sent through the account's outbound proxy / registrar, whatever host the target
  names, so a NOTIFY cannot steer the INVITE (or the digest credential) to another host. A target taken from a
  NOTIFY is reduced to `sip[s]:user@host[:port]`: URI parameters (`maddr`, `transport`, ...) and `?headers` are dropped.

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

### BLF subscription health

`blf.list` (and `status`) report per entry `subscription {phase, last_code, last_reason, last_problem, expires_at, subscribed_at, last_refreshed_at, retry_at}` and
`stale`. `last_problem` is the plain-language `{kind, title, hint}` of the last failure (absent while healthy). `subscribed_at` is when the current subscription was first confirmed (a 2xx to SUBSCRIBE); refreshes keep it, while a failure, a termination or a new dialog (resubscribe, also after a 481 to a refresh) resets it. `last_refreshed_at` is the last 2xx SUBSCRIBE of that dialog; both are `null` until confirmed and after a failure. `phase` is `pending`, `active`, `retrying` (an attempt is scheduled at `retry_at`) or `failed` (refused for
good, `last_code` is the SIP status). `stale` is true while retrying (the last known `state` stays and `retry_at` says when the next attempt runs) and when neither a NOTIFY nor a refresh arrived
within 30 s after `expires_at`; the last known `state` is kept. With many entries consider a longer `blf_expires`: each
entry is its own SUBSCRIBE dialog.

## Vendor profiles

`profile = "yealink" | "polycom" | "cisco" | "grandstream"` makes an account behave like a desk-phone family by filling in
defaults. **Precedence: explicit option in the file > profile value > built-in default.** An option counts as explicit when
its key is present in the account's table; for the two "empty = inherit" options (`user_agent`, `dtmf_mode`) an empty string
counts as not set, so the shipped example (which lists every key) can simply gain a `profile` line.

Profiles are resolved while the file is parsed (so a reload re-resolves them); `config.get` shows the resolved values, and
saving that text back makes them explicit. The bundles are best-effort approximations of factory behaviour (the
User-Agent strings are realistic, the protocol details are not certified); override any single value in the account.

| Option | `generic` (built-in) | `yealink` | `polycom` | `cisco` | `grandstream` |
|---|---|---|---|---|---|
| `user_agent` | `[general].user_agent` | `Yealink SIP-T54W 96.86.0.70` | `PolycomVVX-VVX_450-UA/6.4.4.5239` | `Cisco-CP-8845-3PCC/12.0.1` | `Grandstream GXP2170 1.0.11.74` |
| `hold_style` | `sendonly` | `sendonly` | `sendonly` | `inactive` | `sendonly` |
| `dtmf_mode` | inherit (`auto`) | `rfc4733` | `rfc4733` | `rfc4733` | `rfc4733` |
| `dnd_reject_code` | `486` | `480` | `486` | `480` | `486` |
| `attended_refer_target` (Refer-To form) | `aor` | `aor` | `contact` | `contact` | `aor` |
| `prack` | `supported` | `disabled` | `supported` | `supported` | `disabled` |
| `session_timers` | `true` | `false` | `true` | `true` | `true` |

`Allow` is the same for all profiles; `Supported` follows the bundle (`replaces`, plus `100rel` unless `prack = "disabled"`,
plus `timer` with `session_timers`).

## Validation summary

A file is rejected (nothing is applied) when: it is not valid TOML or has unknown keys; account ids are empty, malformed
or duplicated; an enabled account lacks `username`/`domain`; ranges above are violated; `tls_fingerprint` is not a SHA-256
hex string; `password_store = "keyring"` has a non-empty `password`; `session_expires` is below `min_se`; `tls_cert` and `tls_key` are not set together; `control.tcp_listen` is not a loopback address; `network.probe_secs` / `network.debounce_ms` / `keepalive_fail_threshold` are out of range; `log_level` is
not a valid filter; `calls.decline_code` is not 400-699 or a `[ui.shortcuts]` entry has an unknown action or a malformed chord; or `[speech]` has an unknown placeholder, an out-of-range timeout or a malformed voice/language. Missing TLS files only produce warnings.

## Files written next to the config

| File | Purpose |
|---|---|
| `state.toml` | App-owned runtime state that is not a setting (settings, audio devices included, live in `config.toml`). Today it holds `first_run_completed`: the GUI's first-run wizard was finished or skipped (delete the file to see the wizard again). Absent while there is none. Safe to delete. |
| `runtime.json` | App-owned, mode 0600: pid, control endpoints, per-run token. Removed on clean shutdown (a stale one after a crash is harmless). |
| `siple.lock` | Single-instance lock (one instance per config directory). |
| `control.sock` | Control socket (macOS/Linux), unless overridden or too long a path. |
| `logs/siple.<date>.log`, `logs/sip/<id>/sip.<date>.log` | Application log and per-account SIP traces (daily rolling). |
