//! Desktop notifications (`[ui] notifications`).
//!
//! - Linux: `notify-rust` over zbus (`org.freedesktop.Notifications`), with Answer / Decline
//!   actions on incoming calls where the server supports actions.
//! - Windows: `notify-rust` toast (no action buttons; the toast shows under the PowerShell app id
//!   until the installer registers an AUMID).
//! - macOS: `UNUserNotificationCenter`, only inside an app bundle (an unbundled binary has no
//!   notification identity and AppKit raises). Authorization or posting failures are logged once
//!   and notifications are then skipped quietly. A delegate shows banners even while Siple is
//!   frontmost. No action buttons yet.

use super::ShellTx;
use super::policy::Toast;

/// Title and body of a toast.
pub fn text(t: &Toast) -> (String, String) {
    match t {
        Toast::Incoming(_, who) => ("Incoming call".into(), who.clone()),
        Toast::Missed(who) => ("Missed call".into(), who.clone()),
        Toast::Transfer(head, detail) => (head.clone(), detail.clone()),
    }
}

/// The one-time first-close notice.
pub fn tray_hint() -> (String, String) {
    let place = if cfg!(target_os = "macos") {
        "menu bar"
    } else {
        "system tray"
    };
    (
        "Siple is still running".into(),
        format!("Siple keeps running in the {place}. Quit from its menu."),
    )
}

/// Escapes `&`, `<`, `>` for notification bodies.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Runs when the OS accepted a notification.
pub type OnPosted = Box<dyn FnOnce() + Send + 'static>;

/// Posts notifications. Each post may spawn a short thread.
pub struct Notifier {
    #[allow(dead_code)] // used by the Linux action handler
    tx: ShellTx,
    warned: std::sync::atomic::AtomicBool,
    /// Linux: server-side id of the notification shown for each ringing call.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    shown: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u32>>>,
}

impl Notifier {
    pub fn new(tx: ShellTx) -> Self {
        Self {
            tx,
            warned: std::sync::atomic::AtomicBool::new(false),
            shown: Default::default(),
        }
    }

    /// Logs `msg` the first time only.
    #[allow(dead_code)]
    fn warn_once(&self, msg: &str) {
        if !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!("desktop notifications unavailable: {msg}");
        }
    }

    /// Posts `toast`.
    pub fn toast(&self, toast: &Toast) {
        let (title, body) = text(toast);
        let call = match toast {
            Toast::Incoming(id, _) => Some(id.clone()),
            _ => None,
        };
        self.post(&title, &body, call, None);
    }

    /// Posts a plain message; `on_posted` runs once the OS accepted it (never on failure).
    pub fn info(&self, title: &str, body: &str, on_posted: Option<OnPosted>) {
        self.post(title, body, None, on_posted);
    }

    /// Takes back the incoming-call notification of `call_id` (the call stopped ringing).
    pub fn withdraw(&self, call_id: &str) {
        #[cfg(target_os = "linux")]
        {
            let Some(id) = self
                .shown
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(call_id)
            else {
                return;
            };
            // The Notifications spec has no "close" we can reach from here (the original
            // handle is parked in its action thread): replace it with a short-lived plain one.
            let _ = std::thread::Builder::new()
                .name("siple-notify".into())
                .spawn(move || {
                    use notify_rust::{Notification, Timeout};
                    let _ = Notification::new()
                        .id(id)
                        .appname("Siple")
                        .summary("Call ended")
                        .icon(super::ICON_NAME)
                        .timeout(Timeout::Milliseconds(1500))
                        .show();
                });
        }
        #[cfg(target_os = "macos")]
        if super::platform_bundled() {
            super::macos_notify::withdraw(&format!("siple-call-{call_id}"));
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = call_id;
    }

    #[cfg(target_os = "linux")]
    fn post(&self, title: &str, body: &str, call: Option<String>, on_posted: Option<OnPosted>) {
        use notify_rust::{Notification, Timeout};
        // The body is markup on most servers; a caller's display name is untrusted text.
        let (title, body, tx) = (title.to_string(), escape_markup(body), self.tx.clone());
        let shown = self.shown.clone();
        let r = std::thread::Builder::new()
            .name("siple-notify".into())
            .spawn(move || {
                let mut n = Notification::new();
                n.appname("Siple")
                    .summary(&title)
                    .body(&body)
                    .icon(super::ICON_NAME)
                    .timeout(Timeout::Milliseconds(if call.is_some() {
                        25_000
                    } else {
                        8_000
                    }));
                if call.is_some() {
                    n.action("answer", "Answer").action("decline", "Decline");
                }
                match n.show() {
                    Ok(handle) => {
                        if let Some(f) = on_posted {
                            f();
                        }
                        if let Some(id) = call {
                            shown
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(id.clone(), handle.id());
                            handle.wait_for_action(|action| {
                                let cmd = match action {
                                    "answer" => super::tray_model::TrayCmd::Answer(id.clone()),
                                    "decline" => super::tray_model::TrayCmd::Decline(id.clone()),
                                    _ => return,
                                };
                                tx.send(super::ShellMsg::Tray(cmd));
                            });
                            shown.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        }
                    }
                    Err(e) => tracing::debug!("notification failed: {e}"),
                }
            });
        if let Err(e) = r {
            self.warn_once(&e.to_string());
        }
    }

    #[cfg(target_os = "windows")]
    fn post(&self, title: &str, body: &str, _call: Option<String>, on_posted: Option<OnPosted>) {
        use notify_rust::Notification;
        let (title, body) = (title.to_string(), body.to_string());
        let r = std::thread::Builder::new()
            .name("siple-notify".into())
            .spawn(move || {
                let mut n = Notification::new();
                n.appname("Siple").summary(&title).body(&body);
                // A packaged app's toasts must carry the package identity, otherwise Windows
                // attributes them to PowerShell.
                if let Some(aumid) = siple_core::config::channel::current().aumid {
                    n.app_id(aumid);
                }
                match n.show() {
                    Ok(_) => {
                        if let Some(f) = on_posted {
                            f();
                        }
                    }
                    Err(e) => tracing::debug!("notification failed: {e}"),
                }
            });
        if let Err(e) = r {
            self.warn_once(&e.to_string());
        }
    }

    #[cfg(target_os = "macos")]
    fn post(&self, title: &str, body: &str, call: Option<String>, on_posted: Option<OnPosted>) {
        if !super::platform_bundled() {
            self.warn_once("not running from an app bundle (macOS needs a bundle identity)");
            return;
        }
        let ident = call.map(|c| format!("siple-call-{c}"));
        if let Err(e) = super::macos_notify::post(title, body, ident, on_posted) {
            self.warn_once(&e);
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    fn post(&self, _t: &str, _b: &str, _c: Option<String>, _o: Option<OnPosted>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_text() {
        assert_eq!(
            text(&Toast::Incoming("c1".into(), "Bob".into())),
            ("Incoming call".into(), "Bob".into())
        );
        assert_eq!(text(&Toast::Missed("Bob".into())).0, "Missed call");
        assert_eq!(
            text(&Toast::Transfer("Transfer failed".into(), "x".into())).0,
            "Transfer failed"
        );
        assert!(tray_hint().1.contains("Quit from its menu"));
        assert_eq!(
            escape_markup("AT&T <b>x</b>"),
            "AT&amp;T &lt;b&gt;x&lt;/b&gt;"
        );
    }
}
