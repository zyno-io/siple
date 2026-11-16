# Configuration overview

Siple is configured **only** by one TOML file, `config.toml`. The GUI and the control API are just writers of that file.

| OS | Default location |
|---|---|
| macOS | `~/Library/Application Support/Siple/config.toml` |
| Windows | `%APPDATA%\Siple\config.toml` |
| Linux | `$XDG_CONFIG_HOME/siple/config.toml` |

Override with `--config PATH` or the `SIPLE_CONFIG` environment variable. `siple-cli config-example` prints a commented example.

## Hot reload

Edit the file while Siple runs. The directory is watched and a change is applied after a short debounce.

- An **invalid file never replaces the last good config**. The GUI shows a banner and a `config.error` event is emitted.
- Accounts are matched by `id`: a new id starts an account, a removed id (or `enabled = false`) unregisters it.
- Each key has a reload class: hot, next call, re-register, rebuild, rebind or restart. Changes that rebuild a transport wait
  until the account has no calls. The table is in [Reload classes](/reference/configuration#reload-classes).
- Unknown keys are rejected, so typos are caught.

Force a reload with `siple-cli ctl config.reload`.

## Editing from the GUI and the API

Settings and `config.edit` change exactly the keys involved and preserve your comments, order and quoting. Writes are atomic
and validated before the file is replaced.

## A minimal account

```toml
[[account]]
id = "desk"
username = "1001"
password = "secret"
domain = "pbx.example.test"
transport = "tls"
srtp = "optional"
```

The complete list of keys, defaults and reload classes is the [Configuration reference](/reference/configuration).
