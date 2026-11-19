# Updates

Siple tells you when a newer version is available. A small **Update 0.2.0** pill appears in the header. What it does depends on
how you installed Siple:

- **Direct download** (zip, dmg, tarball): **Install and restart** downloads the new version, checks it and installs it in place
  (see below). Where that is not possible, **Download** and **Release notes** open the release page in your browser.
- **Mac App Store**: **Open App Store** opens Siple's page there; the App Store installs the update.
- **Microsoft Store**: **Open Microsoft Store** opens Siple's page there. The Store does not tell a version number, so the pill just
  reads **Update**; the Store installs the update.

**Skip this version** hides that version (a newer one shows again); on the Microsoft Store **Not now** hides the pill for this
session (not when the Store marks the update mandatory). Settings, Advanced shows the version you run, the result of the last
check, an **Install updates in the app** switch (direct downloads) and a **Check now** button.

## Installing in place (direct downloads)

Nothing is downloaded until you choose **Install and restart** (or a script or agent you run calls `app.install_update`). Then Siple
downloads the new version from GitHub, checks it, replaces itself and restarts:

1. It downloads `SHA256SUMS`, `SHA256SUMS.minisig` and the archive for your system from the project's GitHub release (HTTPS, only
   `User-Agent: Siple/<version>` is sent).
2. It checks the minisign signature of Siple's release key on the checksum list, the archive's SHA-256, and your system's check of
   the publisher: on macOS the Developer ID signature and Apple's notarization; on Windows the Authenticode signature of Signal24 LLC.
   On Linux the signed list is the check. If anything is wrong, nothing is changed and the pill offers the release page.
3. It replaces the app as late as possible and restarts, **never during a call**: with a call going on the pill reads
   "Install, restart after the call" and then "Restart to update"; otherwise Siple waits for five quiet seconds first. Your accounts,
   settings and passwords are not touched, and the app stays where it is (login item and `sip:` links keep working).

If Siple cannot replace itself, the pill opens the release page and says why: installed with a package manager (Homebrew, winget,
Scoop, ...), in a folder you cannot write to (Program Files, `/usr`), run from a disk image or a quarantined download, in a container,
Flatpak or Snap, or without a window. Siple never asks for administrator rights. Turn the feature off with
`[updates] install = false`. The Mac App Store and Microsoft Store editions never install anything themselves.

## What is checked, and what is sent

About 30 seconds after Siple starts, and then every 24 hours, Siple makes one check. Where it looks depends on the edition:

**Direct download**: one HTTPS request to GitHub (the check; the files above are downloaded only when you install).

```
GET https://api.github.com/repos/zyno-io/siple/releases?per_page=20
User-Agent: Siple/<your version>
Accept: application/vnd.github+json
```

When the list has not changed since the last check, GitHub answers "not modified" (Siple sends the ETag it got before), so the
request costs almost nothing. If GitHub limits requests (HTTP 429 or 403), Siple waits as long as GitHub asks before trying again.

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

In every case there is no identifier, no account or call information, no install ID and no cookie. The server (GitHub or Apple)
sees your IP address, as any web server does. A failed check (offline, a firewall) is logged and shown in Settings; it never opens
a dialog.

This is the only network activity Siple starts on its own. Siple has no telemetry.

## Which releases are offered

Direct builds: Siple offers the highest version that is newer than the one you run. By default only final releases (`0.2.0`)
count. If you run a release candidate (`0.2.0-rc.1`), release candidates count too, so you are moved on to `rc.2` and then to
`0.2.0`. Set `channel` to choose explicitly. The stores offer what they publish; `channel` does not apply there.

## Turn it off or change it

Settings, Advanced, "Check for updates", or in `config.toml` (hot reloadable), on every edition:

```toml
[updates]
check = false            # never check: no request to GitHub or Apple, no question to the Store
# install = false        # direct builds: do not install in place; the pill opens the release page
# channel = "prerelease" # direct builds only: stable | prerelease; unset follows the build you run
```

Agents: `app.update_status` (with `source`, `action`, `store_url` and `install`), `app.check_updates`, `app.install_update`,
`app.cancel_update` and the events `app.update_available`, `app.update_progress` and `app.updated`; an install that an agent starts
is shown in the window, with a Cancel;
see the [API reference](/agents/api-reference). Opening a store page is something only the GUI does; agents read `action` and
`store_url`.
