# Updates

Siple tells you when a newer release is available. It only tells you: nothing is downloaded or installed for you. A small
**Update 0.2.0** pill appears in the header; click it for **Download** and **Release notes** (both open the release page in your
browser) or **Skip this version** (that version stays hidden; a newer one shows again). Settings, Advanced shows the version you
run, the result of the last check and a **Check now** button.

## What is checked, and what is sent

About 30 seconds after Siple starts, and then every 24 hours, it makes one HTTPS request to GitHub:

```
GET https://api.github.com/repos/zyno-io/siple/releases?per_page=20
User-Agent: Siple/<your version>
Accept: application/vnd.github+json
```

That is all. There is no identifier, no account or call information, no install ID and no cookie, and the answer is only the public
list of releases. GitHub, like any web server, sees your IP address. When the list has not changed since the last check, GitHub
answers "not modified" (Siple sends the ETag it got before), so the request costs almost nothing. If GitHub limits requests
(HTTP 429 or 403), Siple waits as long as GitHub asks before trying again. A failed check (offline, a firewall) is logged and shown
in Settings; it never opens a dialog.

This is the only network request Siple makes on its own. Siple has no telemetry.

## Which releases are offered

Siple offers the highest version that is newer than the one you run. By default only final releases (`0.2.0`) count. If you run a
release candidate (`0.2.0-rc.1`), release candidates count too, so you are moved on to `rc.2` and then to `0.2.0`.
Set `channel` to choose explicitly.

## Turn it off or change it

Settings, Advanced, "Check for updates", or in `config.toml` (hot reloadable):

```toml
[updates]
check = false            # never contact GitHub
# channel = "prerelease" # stable | prerelease; unset follows the build you run
```

Agents: `app.update_status`, `app.check_updates` and the event `app.update_available`; see the
[API reference](/agents/api-reference).

## Store builds

Builds from the Mac App Store or the Microsoft Store have no update check at all (the code is not compiled in, the pill and the
settings are absent) and update through the store. `[updates]` in the config file is accepted and ignored there.
