//! The control-API face of the update check: `app.update_status`, `app.check_updates`, and for
//! direct builds `app.install_update` / `app.cancel_update`.

use serde_json::{Value, json};

#[cfg(not(feature = "store"))]
use super::install::Restart;
use super::{UpdateStatus, full_version};
use crate::app::Inner;
use crate::backend::AppBackend;
use crate::control::ApiError;
use crate::control::methods::MethodInfo;
#[cfg(not(feature = "store"))]
use crate::control::methods::parse_params;

/// Names served by [`AppBackend::update_method`].
pub(crate) const UPDATE_METHODS: &[&str] = &[
    "app.update_status",
    "app.check_updates",
    "app.install_update",
    "app.cancel_update",
];

const INSTALL_DOC: &str = "Downloads, verifies and installs the offered Siple release in place, then restarts into it (direct downloads on macOS, Windows and Linux; the Mac App Store, Microsoft Store and Flatpak editions report `unsupported`: the store or Flathub updates them). Returns at once with the result of `app.update_status` (its `install` object is the progress); the work runs in the background and is reported by the events `app.update_progress` {version, state, bytes?, total?, error?, reason?} (at most twice a second) and, after the restart, `app.updated` {from, to}. The request is refused unless `[updates] check` and `[updates] install` are on and `version` equals the `latest` of `app.update_status`. What is downloaded: the release's checksum list, its minisign signature and the archive for this system, from the project's GitHub release (redirected to GitHub's asset hosts), each over HTTPS with `User-Agent: Siple/<version>`; the checksum list must carry a valid minisign signature of Siple's release key (and name this release), the archive must match its listed SHA-256, and the new app must pass the operating system's code-signature check of the vendor (macOS: Developer ID and notarization; Windows: Authenticode with the pinned signer) before anything is replaced. Nothing is replaced, and Siple is never restarted, while a call, a transfer or an echo test is active: `when_idle` waits for 5 s of quiet and `now` is refused with `call_active`. Never elevates: a copy that is not writable by the user, is managed by a package manager, runs from a disk image or container, or is a headless instance cannot install (`status.install.reason`) and the offer opens the release page instead. Errors: invalid_state with data.reason `disabled`, `not_offered` {latest}, `install_not_possible` {why, detail, release_url}, `busy` (another version is being installed), `call_active`; `unsupported` in store and Flatpak editions. Calling it again for the version being installed only changes `restart`; after a failure it retries. A GUI shows the pending install (and a notice with Cancel when an agent started it).";

const RESULT_DOC: &str = "Result {enabled (the check is on: `[updates] check`, and there is a source for this build and package), channel (`stable`|`prerelease`, the one in effect; the stores always report `stable`), current (the running version, `0.2.0-rc.4` for a release candidate), latest? (newest release the channel allows, once a check found one; absent for the Microsoft Store, which does not tell a version), available (latest is newer than current; Microsoft Store: an update is waiting), source (`github`|`app_store`|`microsoft_store`|`none`), action (what the offer does: `install` (direct builds that can install in place: `app.install_update`)|`open_release_page`|`open_store`|`none` while nothing is available), release_url? (GitHub source), store_url? (store sources, while an update is available; opening it is a GUI action), disabled_reason? (why source is `none`: `direct_build_in_package`|`store_build_outside_store`|`sideloaded`|`managed_by_flatpak`), mandatory? (Microsoft Store: the waiting update is mandatory), published_at?, checked_at? (last successful check, RFC 3339), error? (why the last check failed; cleared by the next success), install? (direct builds: {possible, reason? (`disabled`|`headless`|`release_not_signed`|`no_asset`|`unsupported_platform`|`unsigned_build`|`not_bundle`|`not_writable`|`package_manager`|`container`|`translocated`|`read_only_volume`|`not_owner`|`root`|`system_install`|`sandboxed`|`in_use`), detail?, state (`idle`|`downloading`|`verifying`|`ready`|`waiting`|`committing`|`installed`|`restarting`|`failed`), version?, bytes?, total?, restart? (`when_idle`|`now`|`manual`), error?, error_kind?})}. The source follows the distribution channel at run time. Direct builds: one anonymous HTTPS GET of the project's GitHub releases list. Mac App Store: one anonymous HTTPS GET of Apple's lookup of Siple's listing. Microsoft Store: Siple asks Windows' Store service about its own package (Windows contacts Microsoft; Siple sends nothing). Flatpak: no check; Flatpak updates the app from Flathub. The two HTTPS requests carry `User-Agent: Siple/<version>` and nothing else that identifies the install. Checking downloads nothing. A newly found version is announced once as the event `app.update_available` {version (null for the Microsoft Store), url (release page or store page), action}.";

/// Schemas of [`UPDATE_METHODS`], chained into `backend_methods()`.
pub fn update_methods() -> Vec<MethodInfo> {
    let none = || json!({"type":"object","properties":{},"additionalProperties":false});
    vec![
        MethodInfo::new(
            "app.update_status",
            &format!(
                "Whether a newer Siple release exists, as of the last check (no network access). {RESULT_DOC}"
            ),
            none(),
        ),
        MethodInfo::new(
            "app.check_updates",
            &format!(
                "Checks for a newer release now and returns the result of `app.update_status`. A check that ran less than 60 s ago (or a rate-limit back-off in progress) is not repeated: the last result comes back, with `error` set in the second case. Failures never raise an API error; they are reported in `error`. Does nothing while `[updates] check = false` or when `source` is `none`. {RESULT_DOC}"
            ),
            none(),
        ),
        MethodInfo::new(
            "app.install_update",
            INSTALL_DOC,
            json!({"type":"object","properties":{
                "version":{"type":"string","description":"The version to install; must equal `latest` of `app.update_status`."},
                "restart":{"type":"string","enum":["when_idle","now","manual"],"description":"When to swap the files in and restart: `when_idle` (default) once no call has been active for 5 s, `now` immediately (fails with `call_active` while a call is active), `manual` after the download is verified, waiting for a later call with `when_idle` or `now`."}
            },"required":["version"],"additionalProperties":false}),
        ),
        MethodInfo::new(
            "app.cancel_update",
            "Cancels an update install that has not swapped the files yet (downloading, verifying, ready, waiting for the calls to end). Result {cancelled (false when nothing was running), install (the `install` object of `app.update_status`)}. Once the new files are in place (`installed`, `restarting`) it fails with invalid_state, data.reason `too_late`. Editions from a store: unsupported.",
            none(),
        ),
    ]
}

impl Inner {
    /// `app.update_status`.
    pub(crate) fn update_status(&self) -> UpdateStatus {
        self.updater.status()
    }

    /// `app.check_updates`.
    pub(crate) async fn check_updates(&self) -> UpdateStatus {
        self.updater.check_now().await
    }

    /// `app.install_update`.
    #[cfg(not(feature = "store"))]
    pub(crate) fn install_update(&self, p: InstallParams) -> Result<UpdateStatus, ApiError> {
        let restart = match p.restart.as_deref() {
            None => Restart::WhenIdle,
            Some(r) => Restart::parse(r).ok_or_else(|| {
                ApiError::invalid_params("`restart` must be when_idle, now or manual")
            })?,
        };
        self.installer.install(&p.version, restart)?;
        Ok(self.update_status())
    }

    /// The `update` summary of `status`.
    pub(crate) fn update_summary(&self) -> Value {
        let s = self.update_status();
        json!({
            "enabled": s.enabled,
            "current": full_version(),
            "available": s.available,
            "latest": s.available.then_some(s.latest),
            "release_url": s.available.then_some(s.release_url),
            "source": s.source,
            "action": s.action,
            "store_url": s.available.then_some(s.store_url),
            "install_state": s.install.as_ref().map(|i| i.state),
        })
    }
}

/// Parameters of `app.install_update`.
#[cfg(not(feature = "store"))]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallParams {
    version: String,
    restart: Option<String>,
}

impl AppBackend {
    /// Dispatches one of [`UPDATE_METHODS`].
    pub(crate) async fn update_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        let s = match method {
            "app.update_status" | "app.check_updates" => {
                let _: Value = params;
                if method == "app.update_status" {
                    self.inner.update_status()
                } else {
                    self.inner.check_updates().await
                }
            }
            #[cfg(not(feature = "store"))]
            "app.install_update" => self.inner.install_update(parse_params(params)?)?,
            #[cfg(not(feature = "store"))]
            "app.cancel_update" => {
                let _: Value = params;
                let cancelled = self.inner.installer.cancel()?;
                return Ok(json!({
                    "cancelled": cancelled,
                    "install": self.inner.update_status().install,
                }));
            }
            // The stores update their own apps; nothing here downloads or installs.
            #[cfg(feature = "store")]
            "app.install_update" | "app.cancel_update" => {
                let _ = params;
                return Err(ApiError::new(
                    crate::control::ErrorKind::Unsupported,
                    "this edition is updated by its store",
                ));
            }
            other => return Err(ApiError::method_not_found(other)),
        };
        serde_json::to_value(s).map_err(|e| ApiError::internal(e.to_string()))
    }
}
