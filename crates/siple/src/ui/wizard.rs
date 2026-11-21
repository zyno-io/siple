//! The first-run wizard: account (tested with `account.test` before it is
//! written), audio (devices, live meter, speaker and echo test) and done (links, tray).
//!
//! Shown when no enabled, non-example account exists and `state.toml` has no
//! `first_run_completed`; Skip and Finish record that flag. The account is written with
//! `ConfigEdit::AddAccount` (replacing the shipped example when it is untouched).

use chrono::{DateTime, Utc};
use eframe::egui::{self, Align, Layout, Margin, RichText};
use siple_core::audio::MicPermission;
use siple_core::config::edit::ConfigEdit;
use siple_core::provision::SetupLink;

use super::draft::{self, DraftForm};
use super::form::{group, message_box, primary_button, small_button, test_result_box};
use super::rpc::{self, RpcReply, RpcRequest, RpcTag, TestRun};
use super::settings::sec_toggle_pub;
use super::{Cx, UiAction, UiState, audio, setup_link};
use crate::snapshot::Snapshot;
use crate::theme::{FS_BODY, FS_SMALL, FS_TITLE, SP_L, SP_S};
use crate::widgets::{brand_mark, mono, strong};

/// How long the Review screen waits before Continue works, so a link cannot be clicked through
/// by a stray click or keypress.
pub const REVIEW_ARM_MS: i64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Step {
    /// A setup link, shown for checking before anything is filled in.
    Review,
    #[default]
    Account,
    Audio,
    Done,
}

#[derive(Debug, Default)]
pub struct WizardState {
    /// The wizard was finished or skipped (restored from `state.toml` at startup).
    pub dismissed: bool,
    /// The wizard is on screen (stays up while the new account appears in the config).
    pub running: bool,
    pub step: Step,
    pub form: DraftForm,
    /// The draft the running (or last) test was started with; this is what gets saved.
    tested: Option<DraftForm>,
    /// Whether the wizard is needed, decided once on the first frame.
    decided: Option<bool>,
    pub test: Option<TestRun>,
    pub errors: Vec<(String, String)>,
    pub notice: Option<String>,
    /// Calls queued for the next frame.
    pub outbox: Vec<UiAction>,
    /// The account was written; its keychain password is being stored.
    awaiting_pw: bool,
    /// The setup link under review or being filled in. Never holds a password.
    pub link: Option<SetupLink>,
    /// The wizard was opened for a link on a configured install: a short "Set up an account"
    /// flow (review, form, done) without the audio steps and without the welcome.
    pub link_only: bool,
    /// When the Review screen appeared (or the link changed); Continue works
    /// [`REVIEW_ARM_MS`] later.
    pub review_at: Option<DateTime<Utc>>,
    /// The last frame the Review screen was drawn in; a gap while Continue is still locked means
    /// the window was away, so the delay starts again.
    pub review_seen: Option<DateTime<Utc>>,
    /// The link was replaced while it was on screen: shown as a warning.
    pub link_replaced: bool,
    /// The paste field of the account step, when open.
    pub paste: Option<String>,
    pub paste_error: Option<String>,
}

/// Where a link came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The OS, another launch or `app.open_uri`: untrusted, never replaces typing in progress.
    Opened,
    /// The user pasted it into a field on purpose.
    Pasted,
}

/// What [`WizardState::offer_link`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    /// The Review screen is up.
    Opened,
    /// The link on the Review screen was replaced (the delay starts again).
    Replaced,
    /// Not taken; the text says why (shown as a notice).
    Refused(&'static str),
}

/// The shipped example account, untouched in its main values.
fn is_shipped_example(a: &siple_core::config::AccountConfig) -> bool {
    a.id == "example" && a.domain == "pbx.example.com"
}

/// No enabled account other than the shipped example.
pub fn needs_setup(snap: &Snapshot) -> bool {
    !snap
        .cfg
        .accounts
        .iter()
        .any(|a| a.enabled && !is_shipped_example(a))
}

/// Whether the wizard replaces the window.
pub fn active(st: &UiState) -> bool {
    st.wizard.showing()
}

impl WizardState {
    /// Decides once, on the first frame, whether the wizard is needed. Later config changes (the
    /// last account being disabled, an account added by hand) never raise it again. When a real
    /// account already exists the first-run flag is recorded at once (upgraders have none).
    pub fn decide(&mut self, snap: &Snapshot, out: &mut Vec<UiAction>) {
        if self.decided.is_some() {
            return;
        }
        let needed = !self.dismissed && needs_setup(snap);
        self.decided = Some(needed);
        if !needed && !self.dismissed {
            self.dismissed = true;
            out.push(UiAction::FirstRunDone);
        }
    }

    /// "Run setup again".
    pub fn restart(&mut self) {
        *self = Self {
            dismissed: self.dismissed,
            decided: self.decided,
            running: true,
            ..Self::default()
        };
    }

    /// The wizard replaces the window (running, or the first-run wizard decided on frame one).
    fn showing(&self) -> bool {
        self.running || (!self.dismissed && self.decided == Some(true))
    }

    /// The tested account is being written.
    fn saving(&self) -> bool {
        self.tested.is_some() && matches!(&self.test, Some(TestRun::Done(r)) if r.ok)
    }

    /// Nothing typed and nothing tested: the form is empty or exactly what the current link filled.
    fn pristine(&self) -> bool {
        self.test.is_none()
            && (self.form == DraftForm::default()
                || self
                    .link
                    .as_ref()
                    .is_some_and(|l| self.form == DraftForm::from_link(&l.account)))
    }

    /// A setup link arrived. It opens the Review screen and nothing else: no form is filled, no
    /// test runs, nothing is written. A link never replaces work in progress (a test running, a
    /// password being typed, the audio or done step, or a link's form the user continued to);
    /// only a link the user pasted may replace a half-filled account form that came from no link.
    pub fn offer_link(
        &mut self,
        link: SetupLink,
        origin: Origin,
        snap: &Snapshot,
        now: DateTime<Utc>,
    ) -> Offer {
        if self.busy() || self.awaiting_pw || self.saving() {
            return Offer::Refused(
                "Wait for the current setup to finish, then open the link again",
            );
        }
        let showing = self.showing();
        // Past Review for the current link: its form (and the password typed into it) stays.
        if showing && self.step == Step::Account && self.link.is_some() {
            return Offer::Refused("Finish or cancel the current setup first");
        }
        if showing && matches!(self.step, Step::Audio | Step::Done) {
            return Offer::Refused("Finish the setup first, then open the link again");
        }
        if showing && self.step == Step::Account && origin != Origin::Pasted && !self.pristine() {
            return Offer::Refused(
                "Finish or cancel the account you are typing first, then open the link again",
            );
        }
        if self.step == Step::Review && self.link.is_some() && showing {
            self.link = Some(link);
            self.review_at = Some(now);
            self.review_seen = None;
            self.link_replaced = true;
            return Offer::Replaced;
        }
        let link_only = self.dismissed || !needs_setup(snap);
        *self = Self {
            dismissed: self.dismissed,
            decided: self.decided,
            running: if showing { self.running } else { true },
            link: Some(link),
            link_only,
            step: Step::Review,
            review_at: Some(now),
            ..Self::default()
        };
        Offer::Opened
    }

    /// The form may be tested: a username and a domain, and for an account that came from a
    /// link (which never carries a password) a typed password too.
    pub fn can_test(&self) -> bool {
        self.form.complete() && (self.link.is_none() || !self.form.password.is_empty())
    }

    /// Starts "Test" for the form. The one place that checks [`Self::can_test`], so a click, an
    /// AccessKit action or a forced `go` cannot test a link account without its password.
    pub(super) fn start_test(&mut self, out: &mut Vec<UiAction>) {
        if !self.can_test() {
            return;
        }
        self.test = Some(TestRun::Running);
        self.tested = Some(self.form.clone());
        out.push(UiAction::Rpc(RpcRequest {
            tag: RpcTag::TestDraft,
            method: "account.test",
            params: serde_json::json!({ "draft": self.form.test_draft() }),
        }));
    }

    /// Leaves the link: nothing was sent or written. A link-only wizard closes; the first-run
    /// wizard returns to its plain account form.
    pub fn cancel_link(&mut self) {
        let was_link_only = self.link_only;
        self.form = DraftForm::default();
        self.tested = None;
        self.test = None;
        self.errors.clear();
        self.notice = None;
        self.link = None;
        self.link_only = false;
        self.review_at = None;
        self.review_seen = None;
        self.link_replaced = false;
        self.paste = None;
        self.paste_error = None;
        self.step = Step::Account;
        if was_link_only {
            self.running = false;
        }
    }

    pub fn on_audio_step(&self) -> bool {
        self.running && self.step == Step::Audio
    }

    /// A keychain password is being stored.
    pub fn adding(&self) -> bool {
        self.awaiting_pw
    }

    pub fn busy(&self) -> bool {
        matches!(self.test, Some(TestRun::Running))
    }

    pub(super) fn finish(&mut self, out: &mut Vec<UiAction>) {
        self.running = false;
        self.dismissed = true;
        self.form.password.clear();
        self.tested = None;
        self.link = None;
        self.link_only = false;
        self.review_at = None;
        self.review_seen = None;
        self.link_replaced = false;
        self.paste = None;
        self.paste_error = None;
        self.step = Step::Account;
        out.push(UiAction::FirstRunDone);
    }

    /// Handles the answer of a call started here.
    pub fn on_reply(&mut self, reply: RpcReply) {
        match (reply.tag, reply.result) {
            (RpcTag::TestDraft, Ok(v)) => {
                let r = rpc::TestResult::from_value(&v);
                if r.ok {
                    // The draft that was tested, not whatever the form says now.
                    let draft = self.tested.as_ref().unwrap_or(&self.form).add_draft();
                    self.outbox.push(rpc::config_edit(
                        RpcTag::WizardAdd,
                        None,
                        &[ConfigEdit::AddAccount {
                            draft,
                            replace_example: !self.link_only,
                        }],
                    ));
                }
                self.test = Some(TestRun::Done(r));
            }
            (RpcTag::TestDraft, Err(e)) => {
                self.errors = e.field_errors();
                self.test = Some(TestRun::Failed(e.message));
            }
            (RpcTag::WizardAdd, Ok(_)) => {
                self.errors.clear();
                let tested = self.tested.take().unwrap_or_else(|| self.form.clone());
                if tested.keyring && !tested.password.is_empty() {
                    let id = tested.account_id();
                    self.awaiting_pw = true;
                    self.outbox.push(UiAction::Rpc(RpcRequest {
                        tag: RpcTag::SetPassword(id.clone()),
                        method: "account.set_password",
                        params: serde_json::json!({"account": id, "password": tested.password}),
                    }));
                }
                self.form.password.clear();
                // The account is in the config now; stay until the wizard is finished.
                self.running = true;
                self.step = if self.link_only {
                    Step::Done
                } else {
                    Step::Audio
                };
            }
            (RpcTag::WizardAdd, Err(e)) => {
                self.errors = e.field_errors();
                self.test = Some(TestRun::Failed(e.message));
            }
            (RpcTag::SetPassword(_), Err(e)) => {
                self.awaiting_pw = false;
                self.notice = Some(format!(
                    "The password was not stored in the keychain: {}",
                    e.message
                ));
            }
            (RpcTag::SetPassword(_), Ok(_)) => self.awaiting_pw = false,
            (RpcTag::Echo, Err(e)) => self.notice = Some(e.message),
            _ => {}
        }
    }
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    out.append(&mut st.wizard.outbox);
    super::settings::call_panel(ui, cx, st, out);
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .fill(p.bg)
                .inner_margin(Margin::same(SP_L as i8)),
        )
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let w = ui.available_width().min(580.0);
                    ui.vertical_centered(|ui| {
                        ui.allocate_ui_with_layout(
                            egui::vec2(w, 0.0),
                            Layout::top_down(Align::LEFT),
                            |ui| {
                                ui.spacing_mut().item_spacing.y = SP_S;
                                let step = match st.wizard.step {
                                    Step::Review | Step::Account => 1,
                                    Step::Audio => 2,
                                    Step::Done => 3,
                                };
                                let link_only = st.wizard.link_only;
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    brand_mark(ui, 32.0);
                                    let title = if link_only {
                                        "Set up an account"
                                    } else {
                                        "Welcome to Siple"
                                    };
                                    ui.label(strong(title, FS_TITLE + 4.0).color(p.text));
                                    if !link_only {
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(format!("Step {step} of 3"))
                                                        .size(FS_SMALL)
                                                        .color(p.muted),
                                                );
                                            },
                                        );
                                    }
                                });
                                // A refused link or other notice is otherwise invisible while
                                // the wizard replaces the window.
                                if let Some(n) = cx.vm.notice {
                                    let shown =
                                        ui.scope(|ui| message_box(ui, p.warn, &n.text)).response;
                                    if let Some(d) = &n.detail {
                                        shown.on_hover_text(format!("Details: {d}"));
                                    }
                                }
                                match st.wizard.step {
                                    Step::Review => setup_link::review(ui, cx, st, out),
                                    Step::Account => account_step(ui, cx, st, out),
                                    Step::Audio => audio_step(ui, cx, st, out),
                                    Step::Done => done_step(ui, cx, out, st),
                                }
                            },
                        );
                    });
                });
        });
}

fn account_step(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    ui.label(strong("Add your SIP account", FS_BODY + 1.0).color(p.text));
    let errors = st.wizard.errors.clone();
    let w = &mut st.wizard;
    let testing = matches!(w.test, Some(TestRun::Running));
    if w.link.is_some() {
        // Where the password will go is what the user checked on the Review screen; this is
        // the live form, so it follows any edit.
        let hop = w.form.next_hop();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            ui.label(
                RichText::new("Filled in from a setup link. Siple will connect to")
                    .size(FS_SMALL)
                    .color(p.muted),
            );
            ui.label(mono(hop, FS_SMALL).color(p.text));
        });
    } else if !w.link_only {
        let mut offered = None;
        if w.paste.is_some() {
            offered = setup_link::paste_row(ui, p, &mut w.paste, &mut w.paste_error);
        } else if small_button(ui, p, "Paste a setup link", !testing).clicked() {
            w.paste = Some(String::new());
        }
        if let Some(l) = offered {
            match w.offer_link(l, Origin::Pasted, cx.vm.snap, cx.vm.now) {
                Offer::Refused(m) => w.paste_error = Some(m.to_string()),
                Offer::Opened | Offer::Replaced => {
                    w.paste = None;
                    w.paste_error = None;
                    return;
                }
            }
        }
    }
    group(ui, p, "", |ui| {
        // Locked while a test runs: what is saved is what was tested.
        ui.add_enabled_ui(!testing, |ui| {
            draft::show(ui, p, &mut w.form, &errors, "Store in system keychain");
        });
    });
    if let Some(n) = &w.notice {
        message_box(ui, p.err, n);
    }
    let mut retest = false;
    match &w.test {
        Some(TestRun::Running) => {
            ui.label(
                RichText::new("Testing: registering...")
                    .size(FS_SMALL)
                    .color(p.muted),
            );
        }
        Some(TestRun::Failed(m)) => message_box(ui, p.err, m),
        Some(TestRun::Done(r)) => {
            test_result_box(ui, p, r);
            if r.diagnosis == "tls_untrusted"
                && let Some(fp) = r.fingerprint.clone()
            {
                ui.horizontal_wrapped(|ui| {
                    if small_button(ui, p, "Trust this server", true).clicked() {
                        w.form.tls_fingerprint = fp;
                        retest = true;
                    }
                    if small_button(ui, p, "Turn off verification (not recommended)", true)
                        .clicked()
                    {
                        w.form.tls_verify = false;
                        retest = true;
                    }
                });
            }
        }
        None => {}
    }
    let running = matches!(w.test, Some(TestRun::Running));
    let mut go = retest;
    let save_label = if w.link_only {
        "Test & save"
    } else {
        "Test & continue"
    };
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, save_label, w.can_test() && !running).clicked() {
                go = true;
            }
            // With a link, Cancel drops it (first run: back to the empty form), so the user can
            // open a corrected link: new links are refused while one is being filled in.
            if w.link.is_some() && small_button(ui, p, "Cancel", !running).clicked() {
                w.cancel_link();
            }
            if !w.link_only && small_button(ui, p, "Skip, I'll edit config.toml", true).clicked() {
                w.finish(out);
            }
        });
    });
    if go {
        w.start_test(out);
    }
}

fn audio_step(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    ui.label(strong("Check your audio", FS_BODY + 1.0).color(p.text));
    if let Some(n) = &st.wizard.notice {
        message_box(ui, p.err, n);
    }
    group(ui, p, "", |ui| {
        audio::pickers(ui, p, snap, st, &cx.vm.live, out);
        audio::echo_test(ui, p, snap, &cx.vm.live, out);
    });
    if snap.mic == MicPermission::NotDetermined
        && small_button(ui, p, "Allow microphone access", true).clicked()
    {
        out.push(UiAction::RequestMicAccess);
    }
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, "Continue", true).clicked() {
                st.wizard.step = Step::Done;
            }
            if small_button(ui, p, "Skip", true).clicked() {
                st.wizard.step = Step::Done;
            }
        });
    });
}

fn done_step(ui: &mut egui::Ui, cx: &Cx, out: &mut Vec<UiAction>, st: &mut UiState) {
    let p = cx.p;
    if st.wizard.link_only {
        ui.label(strong("Account added", FS_BODY + 1.0).color(p.text));
        if let Some(n) = &st.wizard.notice {
            message_box(ui, p.warn, n);
        }
        ui.label(
            RichText::new("It is saved in config.toml and is registering now.")
                .size(FS_SMALL)
                .color(p.muted),
        );
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, p, "Close", true).clicked() {
                    st.wizard.finish(out);
                }
            });
        });
        return;
    }
    let v = serde_json::to_value(&cx.vm.snap.cfg.ui).unwrap_or_default();
    ui.label(strong("You're ready to make calls", FS_BODY + 1.0).color(p.text));
    group(ui, p, "", |ui| {
        sec_toggle_pub(
            ui,
            p,
            out,
            "ui",
            "handle_links",
            "Open phone and setup links with Siple",
            &v,
        );
        sec_toggle_pub(ui, p, out, "ui", "tray", "Show in the tray / menu bar", &v);
        sec_toggle_pub(
            ui,
            p,
            out,
            "ui",
            "notifications",
            "Desktop notifications",
            &v,
        );
    });
    ui.label(
        RichText::new("Every setting lives in config.toml; change them any time in Settings.")
            .size(FS_SMALL)
            .color(p.faint),
    );
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, "Finish", true).clicked() {
                st.wizard.finish(out);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use siple_core::provision::SetupAccount;

    fn link() -> SetupLink {
        SetupLink::new(SetupAccount {
            username: "1001".into(),
            domain: "example.test".into(),
            ..Default::default()
        })
    }

    /// A link carries no password: whatever clicks Test (a button, AccessKit, a forced `go`),
    /// the test starts only with one typed.
    #[test]
    fn a_link_account_is_not_tested_without_a_password() {
        let mut w = WizardState {
            link: Some(link()),
            ..WizardState::default()
        };
        w.form = DraftForm::from_link(&link().account);
        assert!(w.form.complete());
        assert!(!w.can_test());
        let mut out = Vec::new();
        w.start_test(&mut out);
        assert!(out.is_empty(), "no account.test RPC");
        assert!(w.test.is_none() && w.tested.is_none());
        w.form.password = "pw".into();
        assert!(w.can_test());
        w.start_test(&mut out);
        assert_eq!(out.len(), 1);
        assert!(w.busy());
    }

    /// Without a link the first-run wizard keeps testing passwordless (open) servers.
    #[test]
    fn a_plain_form_does_not_need_a_password() {
        let mut w = WizardState::default();
        w.form.username = "u".into();
        w.form.domain = "example.test".into();
        assert!(w.can_test());
    }
}
