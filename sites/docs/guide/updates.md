# Updates

See when a new version of Siple is available, install it, and control what Siple checks.

## When an update is available

A small pill appears in the header, for example **Update 0.2.0**. Select it to see what you can do. What it offers depends on
how you installed Siple.

| Your edition | What the pill offers |
|---|---|
| Direct download (zip, dmg, tarball, AppImage; Linux x86_64 and aarch64) | **Install and restart** downloads the new version, checks it and installs it in place. If that is not possible, **Download** and **Release notes** open the release page in your browser. |
| Mac App Store | **Open App Store** opens Siple's page there. The App Store installs the update. |
| Flatpak (Flathub) | No pill. Flatpak installs the update: use your software center or `flatpak update`. Siple makes no check of its own. |
| Microsoft Store | **Open Microsoft Store** opens Siple's page there. The Store does not tell a version number, so the pill reads **Update**. The Store installs the update. |

To hide an update you do not want yet:

- **Skip this version** hides that version. A newer version shows again.
- On the Microsoft Store, **Not now** hides the pill for this session. It does not appear when the Store marks the update
  mandatory.

## Install an update

1. Select the **Update** pill in the header.
2. Choose **Install and restart**. During a call the button reads **Install, restart after the call**.
3. Siple downloads the new version, checks it, replaces itself and restarts. Your accounts, settings and passwords are not
   touched. The AppImage replaces its own file and keeps its name; the previous one stays next to it as `<name>.old-<number>`
   until the new version has run for a minute.

Nothing is downloaded until you choose **Install and restart** (or a script or agent you run calls `app.install_update`). Siple never restarts during a call. With a call going on, the pill reads
**Install, restart after the call** and then **Restart to update**. Otherwise Siple waits for five quiet seconds first. The app
stays where it is, so your login item and `sip:` links keep working.

If you change your mind while it downloads, choose **Cancel**. If an install fails, the pill says **Update failed** and
offers **Try again** and **Download from GitHub**.

### When Siple cannot update itself

The pill opens the release page and says why. The reasons are:

- Siple was installed with a package manager (Homebrew, winget, Scoop and so on).
- It is in a folder you cannot write to (Program Files, `/usr`).
- It runs from a disk image or a quarantined download.
- It runs in a container, Flatpak or Snap, or without a window.

Siple never asks for administrator rights. The Mac App Store and Microsoft Store editions never install anything themselves.

## See your version and check now

Open **Settings › Advanced**. Under **About** you see the **Version** you run. Under **Updates** you see:

- **Check for updates**, a switch that turns the check on or off.
- **Install updates in the app**, a switch for direct downloads.
- **Channel**, for direct downloads: **Automatic**, **Stable releases** or **Stable and release candidates**.
- **Status** and **Last checked**, which show the result of the last check.
- A **Check now** button.

## Turn update checks off

Open **Settings › Advanced** and turn off **Check for updates**. This works on every edition. Siple then makes no request to
siple.app, GitHub or Apple and asks the Store nothing.

To stop only the in-app install on a direct download, turn off **Install updates in the app**. The pill then opens the
release page.

## Which releases are offered

Direct builds: Siple offers the highest version that is newer than the one you run. By default only final releases (`0.2.0`)
count. If you run a release candidate (`0.2.0-rc.1`), release candidates count too, so you are moved on to `rc.2` and then to
`0.2.0`. Set **Channel** to choose explicitly. The stores offer what they publish; the channel does not apply there.

## What is checked, and what is sent

About 30 seconds after Siple starts, and then about every hour (direct downloads) or every 24 hours (the store editions), Siple makes one check. Where it looks depends on the edition.

**Direct download**: one HTTPS request to siple.app (the check; the files described below are downloaded only when you install).

```
GET https://siple.app/updates/v1/releases.json
User-Agent: Siple/<your version>
Accept: application/json
```

The answer is a static list of the latest releases. When it has not changed since the last check, the server answers "not modified"
(Siple sends the ETag it got before), so the request costs almost nothing. If siple.app does not answer, answers with an error or
sends a list Siple cannot read, the same check asks GitHub instead:

```
GET https://api.github.com/repos/zyno-io/siple/releases?per_page=20
User-Agent: Siple/<your version>
Accept: application/vnd.github+json
```

GitHub is asked at most once per 24 hours while Siple runs, either way. When siple.app answers, that one request is a cross-check: the
two lists are merged and, for a release both list, GitHub's entry is used. So a siple.app that is out of date (or wrong) can hide an
update for about a day at most, and a release candidate published since the site was last rebuilt shows up within a day. When
siple.app does not answer and GitHub was already asked less than 24 hours ago, Siple does not ask again: the check fails
(shown in Settings), retries after 5, 10, 20 and 40 minutes, then hourly, against siple.app only, and keeps the last list it had. A
siple.app outage therefore costs GitHub at most one request per installation per day.

If GitHub limits requests (HTTP 429 or 403), Siple waits as long as GitHub asks before trying again. The list only says which
version exists; an install still downloads from the project's GitHub release and is checked against its signed checksum list (see below).

**Mac App Store**: one HTTPS request to Apple's public lookup of Siple's own listing.

```
GET https://itunes.apple.com/lookup?id=6836164262
User-Agent: Siple/<your version>
```

Siple uses the answer only if it is Siple's listing and your macOS is new enough for it; it never follows a link from the answer
(the page it opens is fixed). Apple's listing can show a new version up to a day after it is released. A TestFlight build makes no
request.

**Microsoft Store**: Siple asks Windows' Store service whether an update of Siple is waiting. Windows contacts Microsoft the way it
does for the Store's own updates; Siple sends no request of its own. A copy that was not installed from the Store (a sideloaded
package) does not check.

In every case there is no identifier, no account or call information, no install ID and no cookie. The server (siple.app, GitHub or Apple)
sees your IP address, as any web server does (siple.app's web server logs each request, so a running install shows up there about once an hour as IP address, time and version). A failed check (offline, a firewall) is logged and shown in Settings; it never opens
a dialog.

This is the only network activity Siple starts on its own. Siple has no telemetry.

### What an in-place install downloads

When you choose **Install and restart**, Siple does this:

1. It downloads `SHA256SUMS`, `SHA256SUMS.minisig` and the archive for your system from the project's GitHub release (HTTPS, only
   `User-Agent: Siple/<version>` is sent).
2. It checks the minisign signature of Siple's release key on the checksum list, the archive's SHA-256, and your system's check of
   the publisher: on macOS the Developer ID signature and Apple's notarization; on Windows the Authenticode signature of Signal24 LLC.
   On Linux the signed list is the check. If anything is wrong, nothing is changed and the pill offers the release page.
3. It replaces the app as late as possible and restarts, never during a call. The new version opens its window if it was open
   when it restarted, even with `start_hidden`; if Siple was hidden in the tray it starts hidden again.

## Details

::: details In config.toml
The same settings live under `[updates]`. They are hot reloadable, which means they apply without a restart, on every edition.

```toml
[updates]
check = false            # never check: no request to GitHub or Apple, no question to the Store
# install = false        # direct builds: do not install in place; the pill opens the release page
# channel = "prerelease" # direct builds only: stable | prerelease; unset follows the build you run
```

Turn off in-place installs with `[updates] install = false`.
:::

::: details For agents and scripts
Agents can use `app.update_status` (with `source`, `action`, `store_url` and `install`), `app.check_updates`,
`app.install_update`, `app.cancel_update` and the events `app.update_available`, `app.update_progress` and `app.updated`. An
install that an agent starts is shown in the window, with a **Cancel**. See the [API reference](/agents/api-reference).
Opening a store page is something only the GUI does; agents read `action` and `store_url`.
:::
