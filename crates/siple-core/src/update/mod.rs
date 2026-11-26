//! Update detection: tells the user a newer release exists. Direct builds (zip, dmg, tarball, AppImage) can also download,
//! verify and install it in place ([`install`], `app.install_update`); the store editions only
//! point to their store.
//!
//! Which source answers is decided at run time from the distribution channel ([`source::select`]):
//!
//! - direct builds: one anonymous HTTPS GET of the release feed of siple.app
//!   (`https://siple.app/updates/v1/releases.json`, see [`site`]); GitHub is not asked;
//! - Mac App Store: one anonymous HTTPS GET of Apple's lookup of Siple's listing;
//! - Microsoft Store: a question to Windows' Store service about this package (Siple sends
//!   nothing itself);
//! - Flatpak: no check (Flatpak updates the app from Flathub).
//!
//! Direct builds check about every hour (a static feed has no rate limit, and an ETag makes an
//! unchanged answer a 304); the store sources about every 24 hours. `Siple/<version>` is the only
//! identifying header of the HTTPS ones. The checker ([`Updater`]) is an async task owned by the app; its status is served by
//! `app.update_status` / `app.check_updates`, summarized in `status`, and a newly found version is
//! announced once as the event `app.update_available`.
//!
//! The cargo feature `store` (Mac App Store, Microsoft Store and Flatpak builds) only removes code: the
//! site source, its releases parser, the downloader and the installer. The store sources are compiled in every build so they
//! can be tested everywhere; only the Windows Store call itself needs `windows` and `store`.

mod version;

pub use version::{
    Version, current_version, effective_channel, full_version, is_release_build, resolve_version,
};

mod api;
pub(crate) use api::{UPDATE_METHODS, update_methods};

pub mod appstore;
mod checker;
#[cfg(not(feature = "store"))]
pub mod download;
mod http;
#[cfg(not(feature = "store"))]
pub mod install;
pub mod links;
pub mod msstore;
#[cfg(not(feature = "store"))]
mod releases;
#[cfg(not(feature = "store"))]
mod site;
pub mod source;
#[cfg(not(feature = "store"))]
pub mod sums;

pub use checker::{
    CHECK_INTERVAL, Clock, FetchError, FetchOutcome, INITIAL_DELAY, MIN_MANUAL_INTERVAL,
    STORE_CHECK_INTERVAL, Updater, UpdaterOptions, spawn_updater,
};
#[cfg(not(feature = "store"))]
pub use checker::{OfflineFetcher, ReleaseFetcher};
#[cfg(not(feature = "store"))]
pub use install::Installer;
#[cfg(not(feature = "store"))]
pub use releases::{Asset, Release, parse_releases, pick};
#[cfg(not(feature = "store"))]
pub use site::SiteFetcher;

use serde::Serialize;

pub use source::{DisabledReason, SourceName, UpdateSource};

/// What clicking the update offer does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    /// Download and install in place (`app.install_update`).
    Install,
    /// Open the store page (`store_url`).
    OpenStore,
    /// Open the release page (`release_url`).
    OpenReleasePage,
    /// Nothing to do (no update waiting, or no check).
    None,
}

/// Which host answered the last direct-build check (always siple.app). `status.source` stays
/// `github` (the kind of source: direct downloads, which can install); this says where the list
/// came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedSource {
    /// The release feed of siple.app.
    Site,
}

/// The result of the update check, as `app.update_status` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    /// The check is on (`[updates] check`) and available for this build and package.
    pub enabled: bool,
    /// `stable` or `prerelease`: the channel in effect (stores always report `stable`).
    pub channel: String,
    /// The running version (`0.2.0`, or `0.2.0-rc.4` for a release candidate).
    pub current: String,
    /// The newest release the channel allows, once a check has found one. The Microsoft Store
    /// does not tell a version: it is absent there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// `latest` is newer than `current` (Microsoft Store: an update is waiting).
    pub available: bool,
    /// Where the check looks: `github` (direct downloads), `app_store`, `microsoft_store` or
    /// `none`.
    pub source: SourceName,
    /// Direct downloads: the host that answered the last successful check, `site` (siple.app).
    /// Absent before the first success and for the stores.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feed: Option<FeedSource>,
    /// What opening the offer does; `none` while nothing is available.
    pub action: UpdateAction,
    /// The release page of `latest` (GitHub source).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    /// The store page of Siple, while an update is available (store sources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_url: Option<String>,
    /// Why there is no source (`direct_build_in_package`, `store_build_outside_store`,
    /// `sideloaded`, `managed_by_flatpak`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<DisabledReason>,
    /// The Microsoft Store marked the waiting update mandatory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mandatory: Option<bool>,
    /// When `latest` was published (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// When the last successful check finished (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// Why the last check failed (cleared by the next success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The in-place install (GitHub source of a direct build): whether it is possible here, and
    /// where an install in progress stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<InstallStatus>,
}

/// The step an in-place install is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallPhase {
    /// Nothing started.
    Idle,
    /// Downloading the checksum list, its signature and the archive.
    Downloading,
    /// Checking the archive and unpacking it into the staging area.
    Verifying,
    /// Verified and staged; waiting for the restart to be allowed (`restart: manual`).
    Ready,
    /// Verified and staged; waiting for the calls to end.
    Waiting,
    /// Swapping the new files in.
    Committing,
    /// The new files are in place; the restart is pending (waiting for the calls to end, or
    /// for a manual restart).
    Installed,
    /// Quitting to start the new version.
    Restarting,
    /// Stopped by an error; nothing was changed. `error` says why.
    Failed,
}

/// `status.install` of [`UpdateStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallStatus {
    /// An install can be started now.
    pub possible: bool,
    /// Why not (`disabled`, `headless`, `release_not_signed`, `no_asset`, `not_writable`,
    /// `package_manager`, `container`, `translocated`, `read_only_volume`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The reason in words, for people.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The step of the install in progress.
    pub state: InstallPhase,
    /// The version being installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Bytes downloaded so far (while `downloading`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Bytes expected (while `downloading`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// When the restart happens: `when_idle`, `now` or `manual`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,
    /// Why the install failed (`failed`), or why the restart did not start (`installed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The category of `error`: `cancelled`, `network`, `untrusted`, `io`, `unsupported`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

impl UpdateStatus {
    /// The status of a build or configuration that does not check.
    pub fn disabled(channel: &str) -> Self {
        Self {
            enabled: false,
            channel: channel.to_string(),
            current: full_version().to_string(),
            latest: None,
            available: false,
            source: SourceName::None,
            feed: None,
            action: UpdateAction::None,
            release_url: None,
            store_url: None,
            disabled_reason: None,
            mandatory: None,
            published_at: None,
            checked_at: None,
            error: None,
            install: None,
        }
    }
}
