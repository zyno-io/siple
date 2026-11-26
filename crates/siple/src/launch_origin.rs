//! Whether the OS started Siple at login (`[ui] start_hidden` applies only then). Every failure
//! to tell means [`LaunchOrigin::Manual`], which shows the window.
//!
//! Signals: the `--at-login` flag (every login entry Siple writes passes it), the MSIX startup
//! task activation (Microsoft Store builds, whose entry is fixed in the manifest) and, on macOS,
//! the `'oapp'` Apple Event's login-item property.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchOrigin {
    /// Started by the user (or anything else that is not known to be the OS at login).
    Manual,
    /// Started by the OS at login.
    Login,
}

/// Signals known before the event loop starts: `Some("flag")`, `Some("startup_task")` or `None`.
pub fn early(flag: bool) -> Option<&'static str> {
    if flag {
        Some("flag")
    } else if msix_startup_task() {
        Some("startup_task")
    } else {
        None
    }
}

/// The final answer plus the signal that decided it. Call it in the eframe creator (main thread;
/// on macOS that is inside `applicationDidFinishLaunching`, while the launch Apple Event is
/// current).
pub fn resolve(early: Option<&'static str>) -> (LaunchOrigin, &'static str) {
    #[cfg(target_os = "macos")]
    let early = early.or_else(|| crate::shell::launched_as_login_item().then_some("login_item"));
    match early {
        Some(v) => (LaunchOrigin::Login, v),
        None => (LaunchOrigin::Manual, "none"),
    }
}

/// Whether Windows activated this MSIX package through its startup task (the manifest's
/// `windows.startupTask`). Any API error counts as "no".
#[cfg(all(windows, feature = "store"))]
fn msix_startup_task() -> bool {
    use siple_core::config::channel::{self, ChannelKind};
    use windows::ApplicationModel::Activation::ActivationKind;
    use windows::ApplicationModel::AppInstance;
    if channel::current().kind != ChannelKind::Msix {
        return false;
    }
    AppInstance::GetActivatedEventArgs()
        .and_then(|a| a.Kind())
        .is_ok_and(|k| k == ActivationKind::StartupTask)
}

#[cfg(not(all(windows, feature = "store")))]
fn msix_startup_task() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_marks_a_login_launch() {
        assert_eq!(early(true), Some("flag"));
        assert_eq!(resolve(Some("flag")), (LaunchOrigin::Login, "flag"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_signal_is_a_manual_launch() {
        assert_eq!(early(false), None);
        assert_eq!(resolve(None), (LaunchOrigin::Manual, "none"));
    }
}
