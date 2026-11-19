//! The first-run wizard: account (tested with `account.test` before it is
//! written), audio (devices, live meter, speaker and echo test) and done (links, tray).
//!
//! Shown when no enabled, non-example account exists and `state.toml` has no
//! `first_run_completed`; Skip and Finish record that flag. The account is written with
//! `ConfigEdit::AddAccount` (replacing the shipped example when it is untouched).

use eframe::egui::{self, Align, Layout, Margin, RichText};
use siple_core::audio::MicPermission;
use siple_core::config::edit::ConfigEdit;

use super::draft::{self, DraftForm};
use super::form::{group, message_box, primary_button, small_button};
use super::rpc::{self, RpcReply, RpcRequest, RpcTag, TestRun};
use super::settings::sec_toggle_pub;
use super::{Cx, UiAction, UiState, audio};
use crate::snapshot::Snapshot;
use crate::theme::{FS_BODY, FS_SMALL, FS_TITLE, SP_L, SP_S};
use crate::widgets::{brand_mark, strong};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Step {
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
    st.wizard.running || (!st.wizard.dismissed && st.wizard.decided == Some(true))
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

    fn finish(&mut self, out: &mut Vec<UiAction>) {
        self.running = false;
        self.dismissed = true;
        self.form.password.clear();
        self.tested = None;
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
                            replace_example: true,
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
                self.step = Step::Audio;
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
                                    Step::Account => 1,
                                    Step::Audio => 2,
                                    Step::Done => 3,
                                };
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    brand_mark(ui, 32.0);
                                    ui.label(
                                        strong("Welcome to Siple", FS_TITLE + 4.0).color(p.text),
                                    );
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(
                                            RichText::new(format!("Step {step} of 3"))
                                                .size(FS_SMALL)
                                                .color(p.muted),
                                        );
                                    });
                                });
                                match st.wizard.step {
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
            message_box(ui, if r.ok { p.ok } else { p.err }, &r.message);
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
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if primary_button(ui, p, "Test & continue", w.form.complete() && !running).clicked() {
                go = true;
            }
            if small_button(ui, p, "Skip, I'll edit config.toml", true).clicked() {
                w.finish(out);
            }
        });
    });
    if go && w.form.complete() {
        w.test = Some(TestRun::Running);
        w.tested = Some(w.form.clone());
        out.push(UiAction::Rpc(RpcRequest {
            tag: RpcTag::TestDraft,
            method: "account.test",
            params: serde_json::json!({ "draft": w.form.test_draft() }),
        }));
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
    let v = serde_json::to_value(&cx.vm.snap.cfg.ui).unwrap_or_default();
    ui.label(strong("You're ready to make calls", FS_BODY + 1.0).color(p.text));
    group(ui, p, "", |ui| {
        sec_toggle_pub(
            ui,
            p,
            out,
            "ui",
            "handle_links",
            "Open sip:, tel: and callto: links with Siple",
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
