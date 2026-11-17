//! User actions: thin wrappers that run SIP / media calls on the tokio runtime and report
//! failures through [`Shared::set_notice`].

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Weak};

use siple_core::App;
use siple_core::media::{LocalTone, LocalToneKind, ToneTarget};
use siple_core::state::PartialSelection;
use siple_media::tone::{Segment, ToneSpec};
use siple_sip::{AnswerMode, AnswerOptions, CompleteMode, DialOptions};
use tokio::runtime::Handle;

use crate::snapshot::Shared;

#[derive(Clone)]
pub struct Actions {
    /// Weak so the UI never keeps the engine alive: `App::shutdown` needs sole ownership, and on
    /// macOS the UI is still alive when AppKit terminates the process.
    app: Weak<App>,
    rt: Handle,
    shared: Arc<Shared>,
    panel_writer: Arc<PanelWriter>,
}

/// Latest panel choices not yet written to `state.toml`. One worker thread writes them in
/// order, so toggles never block the UI thread on file IO and a burst coalesces to its last
/// value.
#[derive(Default)]
struct PanelWriter {
    pending: std::sync::Mutex<siple_core::state::GuiPanels>,
    wake: std::sync::Condvar,
}

impl PanelWriter {
    fn lock(&self) -> std::sync::MutexGuard<'_, siple_core::state::GuiPanels> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn submit(&self, p: siple_core::state::GuiPanels) {
        let mut g = self.lock();
        g.lines_collapsed = p.lines_collapsed.or(g.lines_collapsed);
        g.extensions_open = p.extensions_open.or(g.extensions_open);
        self.wake.notify_one();
    }

    fn run(&self, app: &Weak<App>, shared: &Shared) {
        loop {
            let batch = {
                let mut g = self.lock();
                while *g == siple_core::state::GuiPanels::default() {
                    g = self.wake.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                std::mem::take(&mut *g)
            };
            let Some(app) = app.upgrade() else { return };
            if let Err(e) = app.set_gui_panels(batch) {
                shared.set_notice(format!("Cannot remember the panel layout: {e}"));
            }
        }
    }
}

impl Actions {
    pub fn new(app: Arc<App>, rt: Handle, shared: Arc<Shared>) -> Self {
        let panel_writer = Arc::new(PanelWriter::default());
        {
            let (writer, weak, shared) =
                (panel_writer.clone(), Arc::downgrade(&app), shared.clone());
            let _ = std::thread::Builder::new()
                .name("siple-gui-state".into())
                .spawn(move || writer.run(&weak, &shared));
        }
        Self {
            app: Arc::downgrade(&app),
            rt,
            shared,
            panel_writer,
        }
    }

    /// `None` once the engine is shutting down (actions are then no-ops).
    fn app(&self) -> Option<Arc<App>> {
        self.app.upgrade()
    }

    fn stack(&self) -> Option<siple_sip::SipStack> {
        self.app().map(|a| a.stack().clone())
    }

    /// Runs `fut` on the runtime; an `Err` becomes a notice. Wakes the poller afterwards.
    fn spawn<F, E>(&self, what: &'static str, fut: F)
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display,
    {
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            if let Err(e) = fut.await {
                tracing::warn!("{what} failed: {e}");
                shared.set_notice(format!("{what} failed: {e}"));
            }
            shared.wake.notify_one();
        });
    }

    pub fn dial(&self, account: &str, target: &str) {
        let Some(stack) = self.stack() else { return };
        let (account, target) = (account.to_string(), target.trim().to_string());
        self.spawn("Dial", async move {
            stack
                .dial(&account, &target, DialOptions::default())
                .await
                .map(|_| ())
        });
    }

    /// Directed pickup of a ringing BLF extension (errors surface as a notice).
    pub fn pickup(&self, account: &str, blf_uri: &str) {
        let Some(stack) = self.stack() else { return };
        let (account, uri) = (account.to_string(), blf_uri.to_string());
        self.spawn("Pick up", async move {
            stack.pickup(&account, &uri).await.map(|_| ())
        });
    }

    /// Calls a monitored BLF entry on its account (`blf.dial`: the entry must exist).
    pub fn blf_dial(&self, account: &str, blf_uri: &str) {
        let Some(stack) = self.stack() else { return };
        let (account, uri) = (account.to_string(), blf_uri.to_string());
        self.spawn("Dial", async move {
            stack.blf_dial(&account, &uri).await.map(|_| ())
        });
    }

    pub fn answer(&self, call_id: &str) {
        self.answer_with(call_id, AnswerMode::Default);
    }

    /// Answers with a call-waiting mode: hold the others, or end one named call once this one is
    /// active.
    pub fn answer_with(&self, call_id: &str, mode: AnswerMode) {
        let Some(stack) = self.stack() else { return };
        let id = call_id.to_string();
        self.spawn("Answer", async move {
            let opts = AnswerOptions {
                mode,
                ..AnswerOptions::default()
            };
            stack.answer(&id, opts).await.map(|_| ())
        });
    }

    /// Makes `call_id` the active line (holds the others, resumes it).
    pub fn activate(&self, call_id: &str) {
        let Some(stack) = self.stack() else { return };
        let id = call_id.to_string();
        self.spawn("Switch line", async move { stack.activate(&id).await });
    }

    pub fn reject(&self, call_id: &str) {
        // With another call live this is a call-waiting decline: `[calls] decline_code`.
        let Some(app) = self.app() else { return };
        let id = call_id.to_string();
        self.spawn("Reject", async move { app.reject_call(&id).await });
    }

    pub fn hangup(&self, call_id: &str) {
        let Some(stack) = self.stack() else { return };
        let id = call_id.to_string();
        self.spawn("Hang up", async move { stack.hangup(&id).await });
    }

    pub fn mute(&self, call_id: &str, muted: bool) {
        let Some(stack) = self.stack() else { return };
        let id = call_id.to_string();
        self.spawn("Mute", async move { stack.mute(&id, muted).await });
    }

    pub fn dtmf(&self, call_id: &str, digit: char) {
        let Some(stack) = self.stack() else { return };
        let id = call_id.to_string();
        self.spawn("DTMF", async move {
            stack.send_dtmf(&id, &digit.to_string(), None, None).await
        });
    }

    /// Puts the call on hold (`held`) or takes it off hold.
    pub fn hold(&self, call_id: &str, held: bool) {
        let Some(stack) = self.stack() else { return };
        let id = call_id.to_string();
        if held {
            self.spawn("Hold", async move { stack.hold(&id).await });
        } else {
            self.spawn("Resume", async move { stack.unhold(&id).await });
        }
    }

    /// Blind transfer of `call_id` to `target` (number or sip: URI).
    pub fn transfer_blind(&self, call_id: &str, target: &str) {
        let Some(stack) = self.stack() else { return };
        let (id, target) = (call_id.to_string(), target.trim().to_string());
        self.spawn("Transfer", async move {
            stack.transfer_blind(&id, &target).await.map(|_| ())
        });
    }

    /// Starts a consult: holds `call_id` and dials `target` on its account (transfer session).
    pub fn consult_start(&self, call_id: &str, target: &str) {
        let Some(stack) = self.stack() else { return };
        let (id, target) = (call_id.to_string(), target.trim().to_string());
        self.spawn("Consult", async move {
            stack
                .consult_start(&id, &target, DialOptions::default())
                .await
                .map(|_| ())
        });
    }

    /// Links an existing second call as the consult call of `call_id`.
    pub fn consult_link(&self, call_id: &str, consult_id: &str) {
        let Some(stack) = self.stack() else { return };
        let (id, consult) = (call_id.to_string(), consult_id.to_string());
        self.spawn("Consult", async move {
            stack.consult_link(&id, &consult).await.map(|_| ())
        });
    }

    pub fn transfer_complete(&self, session_id: &str, mode: CompleteMode) {
        let Some(stack) = self.stack() else { return };
        let id = session_id.to_string();
        self.spawn("Transfer", async move {
            stack.transfer_complete(&id, mode).await.map(|_| ())
        });
    }

    pub fn transfer_swap(&self, session_id: &str) {
        let Some(stack) = self.stack() else { return };
        let id = session_id.to_string();
        self.spawn(
            "Swap",
            async move { stack.transfer_swap(&id).await.map(|_| ()) },
        );
    }

    pub fn transfer_cancel(&self, session_id: &str) {
        let Some(stack) = self.stack() else { return };
        let id = session_id.to_string();
        self.spawn("Cancel transfer", async move {
            stack.transfer_cancel(&id).await.map(|_| ())
        });
    }

    /// Runtime do-not-disturb switch of an account (lasts until the config's `dnd` changes).
    pub fn set_dnd(&self, account: &str, enabled: bool) {
        let Some(stack) = self.stack() else { return };
        let account = account.to_string();
        self.spawn("Do not disturb", async move {
            stack.set_dnd(&account, enabled).await
        });
    }

    /// Plays a 440 Hz tone for one second on the speaker.
    pub fn test_speaker(&self) {
        let tone = LocalTone {
            kind: LocalToneKind::Custom(ToneSpec {
                freqs: [440.0, 0.0],
                segments: vec![Segment { on: true, ms: 1000 }],
                repeat: false,
            }),
            target: ToneTarget::Speaker,
        };
        if let Some(app) = self.app() {
            app.media().start_local_tone(tone);
        }
    }

    /// Writes a device choice into `[audio]` of `config.toml` and applies it (the config file is
    /// the only home of settings; an invalid file refuses the change with a notice).
    pub fn set_devices(&self, sel: &PartialSelection) {
        let Some(app) = self.app() else { return };
        let sel = sel.clone();
        self.spawn("Audio device change", async move {
            app.set_devices(&sel).await.map(|_| ())
        });
    }

    /// Runs a control-API call in process and queues its reply for the UI.
    pub fn rpc(&self, req: crate::ui::rpc::RpcRequest) {
        let Some(app) = self.app() else { return };
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            let result = app
                .call(req.method, req.params)
                .await
                .map_err(|e| crate::ui::rpc::RpcError::from_api(&e));
            shared.push_reply(crate::ui::rpc::RpcReply {
                tag: req.tag,
                result,
            });
            shared.wake.notify_one();
        });
    }

    /// Live microphone level (dBFS).
    pub fn mic_level(&self) -> Option<f32> {
        self.app()?.media().mic_level_dbfs()
    }

    /// Progress of the echo test.
    pub fn echo_status(&self) -> siple_core::media::EchoStatus {
        self.app()
            .map(|a| a.media().echo_test_status())
            .unwrap_or_default()
    }

    /// Whether the first-run wizard was finished or skipped before.
    pub fn first_run_completed(&self) -> bool {
        self.app().is_none_or(|a| a.first_run_completed())
    }

    /// The panel choices remembered from earlier launches.
    pub fn gui_panels(&self) -> siple_core::state::GuiPanels {
        self.app().map(|a| a.gui_panels()).unwrap_or_default()
    }

    /// Remembers the panel choices (`None` slots are left alone). Returns at once; a worker
    /// thread does the file IO.
    pub fn set_gui_panels(&self, panels: siple_core::state::GuiPanels) {
        self.panel_writer.submit(panels);
    }

    /// Records that the first-run wizard was finished or skipped.
    pub fn set_first_run_completed(&self) {
        if let Some(app) = self.app()
            && let Err(e) = app.set_first_run_completed()
        {
            self.shared
                .set_notice(format!("Cannot record the setup state: {e}"));
        }
    }

    /// Records that the user skipped release `version` in the update pill.
    #[cfg(not(feature = "store"))]
    pub fn skip_update(&self, version: &str) {
        if let Some(app) = self.app()
            && let Err(e) = app.skip_update(version)
        {
            self.shared
                .set_notice(format!("Cannot record the skipped version: {e}"));
        }
        self.shared.wake.notify_one();
    }

    /// Asks macOS for microphone access when the app is bundled and the answer is still open.
    pub fn request_mic_access(&self) {
        #[cfg(target_os = "macos")]
        {
            let bundled = std::env::current_exe()
                .is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"));
            if bundled
                && siple_core::audio::mic_permission()
                    == siple_core::audio::MicPermission::NotDetermined
            {
                let _ = std::thread::Builder::new()
                    .name("mic-permission".into())
                    .spawn(|| {
                        let r = siple_core::audio::request_mic_permission(
                            std::time::Duration::from_secs(24 * 3600),
                        );
                        tracing::info!(result = ?r, "microphone permission request answered");
                    });
            }
        }
    }

    pub fn set_account_enabled(&self, account: &str, enabled: bool) {
        let Some(app) = self.app() else { return };
        let account = account.to_string();
        self.spawn("Account toggle", async move {
            let report = app.set_account_enabled(&account, enabled).await;
            if report.applied && report.errors.is_empty() {
                Ok(())
            } else {
                Err(report.errors.join("; "))
            }
        });
    }
}

/// Whether `url` is exactly `https://github.com/zyno-io/siple/releases/tag/v<version>` with a
/// version made of `[0-9A-Za-z.-]`, no empty or dot-leading segment (so no `..`, `/.` or other path
/// trickery) and nothing after it.
#[cfg(not(feature = "store"))]
pub(crate) fn is_release_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://github.com/zyno-io/siple/releases/tag/v") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && rest.split('.').all(|seg| !seg.is_empty())
}

/// Opens a release page of this project in the default browser. Only [`is_release_url`] links are
/// opened (the link comes from a network response and is handed to the platform opener).
#[cfg(not(feature = "store"))]
pub fn open_url(url: &str) -> std::io::Result<()> {
    if !is_release_url(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a link to a Siple release",
        ));
    }
    open_path(Path::new(url), false)
}

/// Opens a file (in the default text editor) or a folder with the platform opener.
pub fn open_path(path: &Path, as_text: bool) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    {
        let mut c = Command::new("open");
        if as_text {
            c.arg("-t");
        }
        c.arg(path).spawn().map(|_| ())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = as_text;
        Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = as_text;
        Command::new("xdg-open").arg(path).spawn().map(|_| ())
    }
}

#[cfg(all(test, not(feature = "store")))]
mod url_tests {
    use super::is_release_url;

    #[test]
    fn only_release_tag_pages_are_openable() {
        for ok in [
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0-rc.1",
        ] {
            assert!(is_release_url(ok), "{ok}");
        }
        for bad in [
            "https://github.com/zyno-io/siple/../../evil/repo/releases",
            "https://github.com/zyno-io/siple/releases/tag/v../../../evil",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0/../x",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0/",
            "https://github.com/zyno-io/siple/releases/tag/v..",
            "https://github.com/zyno-io/siple/releases/tag/v0..2",
            "https://github.com/zyno-io/siple/releases/tag/v",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0?x=1",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0#a",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0%2e%2e",
            "https://github.com/zyno-io/siple/releases/tag/v0.2.0 &calc",
            "https://github.com/zyno-io/siple/releases",
            "https://github.com/zyno-io/siple-evil/releases/tag/v1.0.0",
            "http://github.com/zyno-io/siple/releases/tag/v1.0.0",
            "https://example.com/zyno-io/siple/releases/tag/v1.0.0",
            "file:///etc/passwd",
            "",
        ] {
            assert!(!is_release_url(bad), "{bad}");
        }
    }
}
