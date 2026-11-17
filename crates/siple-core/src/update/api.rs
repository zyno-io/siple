//! The control-API face of the update check: `app.update_status`, `app.check_updates`.

use serde_json::{Value, json};

use super::{UpdateStatus, full_version};
use crate::app::Inner;
use crate::backend::AppBackend;
use crate::control::ApiError;
use crate::control::methods::MethodInfo;

/// Names served by [`AppBackend::update_method`].
pub(crate) const UPDATE_METHODS: &[&str] = &["app.update_status", "app.check_updates"];

const RESULT_DOC: &str = "Result {enabled (the check is on: `[updates] check`, and part of this build), channel (`stable`|`prerelease`, the one in effect), current (the running version, `0.2.0-rc.4` for a release candidate), latest? (newest release the channel allows, once a check found one), available (latest is newer than current), release_url?, published_at?, checked_at? (last successful check, RFC 3339), error? (why the last check failed; cleared by the next success)}. A check is one anonymous HTTPS GET of the project's GitHub releases list (header `User-Agent: Siple/<version>`); nothing is downloaded or installed. Store builds (Mac App Store, Microsoft Store) never check: enabled is false. A newly found version is announced once as the event `app.update_available` {version, url}.";

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
                "Checks for a newer release now and returns the result of `app.update_status`. A check that ran less than 60 s ago (or a rate-limit back-off in progress) is not repeated: the last result comes back, with `error` set in the second case. Failures never raise an API error; they are reported in `error`. Does nothing while `[updates] check = false` or in a store build. {RESULT_DOC}"
            ),
            none(),
        ),
    ]
}

impl Inner {
    /// `app.update_status`.
    pub(crate) fn update_status(&self) -> UpdateStatus {
        #[cfg(not(feature = "store"))]
        {
            self.updater.status()
        }
        #[cfg(feature = "store")]
        {
            let current = super::current_version();
            let channel = super::effective_channel(self.config.load().updates.channel, &current);
            UpdateStatus::disabled(channel.as_str())
        }
    }

    /// `app.check_updates`.
    pub(crate) async fn check_updates(&self) -> UpdateStatus {
        #[cfg(not(feature = "store"))]
        {
            self.updater.check_now().await
        }
        #[cfg(feature = "store")]
        {
            self.update_status()
        }
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
        })
    }
}

impl AppBackend {
    /// Dispatches one of [`UPDATE_METHODS`].
    pub(crate) async fn update_method(&self, method: &str) -> Result<Value, ApiError> {
        let s = match method {
            "app.update_status" => self.inner.update_status(),
            "app.check_updates" => self.inner.check_updates().await,
            other => return Err(ApiError::method_not_found(other)),
        };
        serde_json::to_value(s).map_err(|e| ApiError::internal(e.to_string()))
    }
}
