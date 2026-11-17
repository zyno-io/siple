//! Update detection: tells the user a newer release exists. Detection only; nothing is
//! downloaded or installed.
//!
//! One anonymous HTTPS GET of the GitHub releases list of this project (see [`http`]), about every
//! 24 hours, with `Siple/<version>` as the only identifying header. The checker ([`Updater`]) is
//! an async task owned by the app; its status is served by `app.update_status` /
//! `app.check_updates`, summarised in `status`, and a newly found version is announced once as
//! the event `app.update_available`.
//!
//! Store builds (cargo feature `store`, for the Mac App Store and the Microsoft Store) compile all
//! of that out: only [`UpdateStatus::disabled`] and the version helpers remain.

mod version;

pub use version::{Version, current_version, effective_channel, full_version, resolve_version};

mod api;
pub(crate) use api::{UPDATE_METHODS, update_methods};

#[cfg(not(feature = "store"))]
mod checker;
#[cfg(not(feature = "store"))]
mod http;
#[cfg(not(feature = "store"))]
mod releases;

#[cfg(not(feature = "store"))]
pub use checker::{
    CHECK_INTERVAL, Clock, FetchError, FetchOutcome, INITIAL_DELAY, MIN_MANUAL_INTERVAL,
    OfflineFetcher, ReleaseFetcher, Updater, UpdaterOptions, spawn_updater,
};
#[cfg(not(feature = "store"))]
pub use http::GithubFetcher;
#[cfg(not(feature = "store"))]
pub use releases::{Release, parse_releases, pick};

use serde::Serialize;

/// Whether this build contains the update check (false for store builds).
pub const COMPILED_IN: bool = cfg!(not(feature = "store"));

/// The result of the update check, as `app.update_status` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    /// The check is on (`[updates] check`) and part of this build.
    pub enabled: bool,
    /// `stable` or `prerelease`: the channel in effect.
    pub channel: String,
    /// The running version (`0.2.0`, or `0.2.0-rc.4` for a release candidate).
    pub current: String,
    /// The newest release the channel allows, once a check has found one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// `latest` is newer than `current`.
    pub available: bool,
    /// The release page of `latest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_url: Option<String>,
    /// When `latest` was published (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// When the last successful check finished (RFC 3339).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// Why the last check failed (cleared by the next success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
            release_url: None,
            published_at: None,
            checked_at: None,
            error: None,
        }
    }
}
