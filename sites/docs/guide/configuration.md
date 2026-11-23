# Settings and config.toml

Understand how the Settings window and the `config.toml` file relate, and where to find and edit the file.

## Settings and config.toml are the same thing

Everything you change in **Settings** is saved to one text file called `config.toml`. Siple is configured by that file, and
the Settings window and the control API are writers of it. A few things live elsewhere: passwords you keep in the system
keychain, runtime state such as the setup having run (`state.toml`), and the **Do not disturb** switch, which lasts until
Siple restarts or the account's `dnd` value in the file changes.

So you can change a setting in the window, or edit the file, or both. Most people never need the file. You need it for a few
advanced options that have no switch in the window. **Settings › Advanced** lists these under **Set in the config file only**
and has an **Open config** button.

## Where the file is

Select **Open config** (in **Settings › Advanced**, or at the bottom of the window after turning on **Settings › Developer › Show developer tools**) to open it. Hover over the button to see the
full path. By default it lives here:

| OS | Default location |
|---|---|
| macOS | `~/Library/Application Support/Siple/config.toml` |
| Windows | `%APPDATA%\Siple\config.toml` |
| Linux | `$XDG_CONFIG_HOME/siple/config.toml` |

To use a different file, start Siple with `--config PATH` or set the `SIPLE_CONFIG` environment variable.
`siple-cli config-example` prints a commented example.

## Edit the file while Siple runs

You do not need to restart. Siple watches the folder, and a change applies after a short pause.

- **A mistake never breaks Siple.** An invalid file never replaces the last good config. The window shows a **Config not
  applied** banner and Siple keeps using your previous settings. See [Troubleshooting](/guide/troubleshooting#your-changes-to-configtoml-dont-take-effect).
- **Typos are caught.** Unknown keys are rejected.
- **Accounts are matched by `id`.** A new id starts an account. A removed id, or `enabled = false`, unregisters it.
- **Some changes wait.** Changes that rebuild a transport wait until the account has no calls.

::: tip Edits from the window keep your file tidy
Settings changes only the keys involved and preserves your comments, order and quoting. Writes are atomic and validated
before the file is replaced.
:::

## Passwords in the system keyring

`password_store = "keyring"` keeps an account's password in your operating system's credential store instead of `config.toml`.
It is off by default.

- **Where:** macOS Keychain, Windows Credential Manager, or the Linux Secret Service (GNOME Keyring, KWallet). The entry is
  service `io.zyno.siple`, user `account:<id>`.
- **Setting it:** leave `password` empty in the file (a non-empty value is an error). Set the password in Settings, or with the
  `account.set_password` API method. Siple then reloads and registers with it.
- **When it is read:** once per reload, never on each REGISTER (a Keychain read can prompt). Disabled accounts are not read.
- **No credential store, or no entry** (headless Linux, Docker): that account is switched off and the error appears in
  `status.config.last_error`, the log and the `config.error` event. Siple never falls back to a plaintext password. Other
  accounts keep working.
- **Edits stay in step:** switching `password_store` moves the password between the file and the entry. Renaming an account
  moves the entry, and removing the account deletes it.
- **macOS:** Keychain access is tied to the code signature. Unsigned development builds ask again after every rebuild.

Passwords never appear in logs, API results or error messages.

## Editing the file from the window or the API

The Settings window and the `config.edit` API method change `config.toml` through one writer. Each write:

1. Fails with a conflict if the file changed since you read it (`base_hash` from `config.view`).
2. Refuses if the file does not parse. A file that parses but is invalid can still be edited, so you can fix it.
3. Changes only the named keys, keeping comments, blank lines, key order and quoting.
4. Re-validates and reports problems with the path of the key.
5. Replaces the file atomically, reloads and returns what changed.

A key you did not set, and whose new value equals its default, is not added. New accounts and sections are appended with only
the keys that differ from the default. `config.view` shows where each value comes from (`file`, `profile` or `default`) but
never a password or the control token. An older build rejects a file with keys it does not know. The
full edit schema is in the [API reference](/agents/api-reference).

## Details

::: details Reload classes
Each key has a reload class: hot, next call, re-register, rebuild, rebind or restart. The table is in
[Reload classes](/reference/configuration#reload-classes).

Force a reload with `siple-cli ctl config.reload`.
:::

::: details A minimal account
```toml
[[account]]
id = "desk"
username = "1001"
password = "secret"
domain = "pbx.example.test"
transport = "tls"
srtp = "optional"
```
:::

::: details Editing from the API
`config.edit` changes exactly the keys involved, the same way Settings does. It preserves your comments, order and quoting,
and validates before the file is replaced.
:::

The complete list of keys, defaults and reload classes is the [Configuration reference](/reference/configuration).
