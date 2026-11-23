//! The Settings page: an in-window page opened from the header, with a compact
//! live-call strip so a call can still be muted or ended.
//!
//! Save model: account and extension edits are staged in [`SettingsState`] and sent as one
//! `config.edit` by Apply (with the `base_hash` of the file they were read from); toggles and
//! pickers elsewhere apply at once through [`rpc::set_section`]. The GUI never writes the file any
//! other way, and every edit goes through the same control-API method an agent uses.
//!
//! An unparsable file makes the page read-only (the config-error banner explains); a file that
//! parses but is invalid stays editable, the edit's own result is the gate and its field errors
//! (`errors[].path`) are shown next to the fields.

mod accounts;
mod audio_page;
mod extensions;
pub(crate) mod pages;

pub(super) use pages::sec_toggle_pub;

use std::collections::BTreeMap;

use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText, Stroke};
use siple_core::config::edit::{AccountField, ConfigEdit, EditValue};
use siple_core::config::{BlfConfig, Secret};

use super::form::{primary_button, small_button};
use super::rpc::{self, RpcReply, RpcTag, TestRun};
use super::{Cx, UiAction, UiState, header, wizard};
use crate::snapshot::Snapshot;
use crate::theme::{FS_BODY, FS_SMALL, FS_TITLE, Palette, R_MD, SP_L, SP_S, SP_XS, icon as ic};
use crate::widgets::{icon, mono, solid_button, strong};

/// What a conflicting `config.edit` tells the user.
pub const CONFLICT_MSG: &str =
    "The config file changed; your edits were kept, review and apply again";

/// The sections of the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Accounts,
    Audio,
    Calls,
    Extensions,
    Shortcuts,
    Appearance,
    Notifications,
    Advanced,
    Logging,
    Developer,
}

impl Section {
    pub const ALL: [Section; 10] = [
        Section::Accounts,
        Section::Audio,
        Section::Calls,
        Section::Extensions,
        Section::Shortcuts,
        Section::Appearance,
        Section::Notifications,
        Section::Advanced,
        Section::Logging,
        Section::Developer,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Accounts => "Accounts",
            Section::Audio => "Audio",
            Section::Calls => "Calls & lines",
            Section::Extensions => "Extensions",
            Section::Shortcuts => "Shortcuts",
            Section::Appearance => "Appearance",
            Section::Notifications => "Notifications",
            Section::Advanced => "Advanced",
            Section::Logging => "Logging & privacy",
            Section::Developer => "Developer",
        }
    }
}

/// Settings state that survives between frames.
#[derive(Debug, Default)]
pub struct SettingsState {
    pub open: bool,
    pub section: Section,
    /// The account shown in Accounts.
    pub account: Option<String>,
    /// The account whose extensions are edited.
    pub blf_account: Option<String>,
    /// Staged account fields (`account id` -> field -> value).
    pub edits: BTreeMap<String, BTreeMap<AccountField, EditValue>>,
    /// Staged passwords (never echoed back into a field).
    pub passwords: BTreeMap<String, Secret>,
    /// Staged extension lists.
    pub blf: BTreeMap<String, Vec<BlfConfig>>,
    /// Hash of the config file the staged edits were made against.
    pub base_hash: Option<String>,
    /// The file hash as of our own last write (or last look while nothing was staged): an
    /// instant edit of ours moves `base_hash` along only while the two agree.
    synced: Option<String>,
    /// `current_hash` of the last conflict: what "Keep mine" rebases onto.
    conflict_hash: Option<String>,
    /// Problems the engine reported for the last edit: `(path, message)`.
    pub errors: Vec<(String, String)>,
    pub notice: Option<String>,
    /// A "Check for updates" is in flight.
    pub update_checking: bool,
    /// An Apply is in flight.
    pub applying: bool,
    /// `account.test` results per account id.
    pub tests: BTreeMap<String, TestRun>,
    /// The "Add account" form.
    pub add: Option<super::draft::DraftForm>,
    pub add_errors: Vec<(String, String)>,
    pub confirm_remove: Option<String>,
    /// Calls queued for the next frame (password storage, the edit after a keychain write).
    pub outbox: Vec<UiAction>,
    /// The Apply in flight.
    job: Option<ApplyJob>,
    /// The keychain password of the account the add form is creating.
    pub(super) add_pw: Option<(String, Secret)>,
    /// "Add from setup link": the open paste field and what was wrong with the last try.
    pub paste: Option<String>,
    pub paste_error: Option<String>,
    /// An account to select once it appears in the list (added from a setup link).
    pub select_after: Option<String>,
    /// A setup link built for "Copy setup link", copied to the clipboard on the next frame.
    pub copy_pending: Option<String>,
    /// What "Copy setup link" did for an account: `(id, text)`.
    pub link_note: Option<(String, String)>,
}

/// One Apply: keychain writes that must precede the edit, the edit, and keychain writes that
/// must follow it.
struct ApplyJob {
    pre: Vec<(String, Secret)>,
    edits: Vec<ConfigEdit>,
    hash: String,
    edit_sent: bool,
    post: Vec<(String, Secret)>,
}

impl std::fmt::Debug for ApplyJob {
    /// The edits can carry a password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplyJob")
            .field("pre", &self.pre.len())
            .field("edits", &self.edits.len())
            .field("post", &self.post.len())
            .finish()
    }
}

fn set_password_action(id: &str, pw: &Secret) -> UiAction {
    UiAction::Rpc(rpc::RpcRequest {
        tag: RpcTag::SetPassword(id.to_string()),
        method: "account.set_password",
        params: serde_json::json!({"account": id, "password": pw.expose()}),
    })
}

impl SettingsState {
    pub fn dirty(&self) -> bool {
        !self.edits.is_empty() || !self.passwords.is_empty() || !self.blf.is_empty()
    }

    pub fn busy(&self) -> bool {
        self.applying
            || self.update_checking
            || self.tests.values().any(|t| matches!(t, TestRun::Running))
    }

    /// Whether `id` has staged changes (its test would not reflect the form).
    pub fn has_pending(&self, id: &str) -> bool {
        self.edits.contains_key(id) || self.passwords.contains_key(id)
    }

    /// The staged value of `field`, if any.
    pub fn pending(&self, id: &str, field: AccountField) -> Option<&EditValue> {
        self.edits.get(id)?.get(&field)
    }

    /// The error reported for `path`.
    pub fn error(&self, path: &str) -> Option<&str> {
        self.errors
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, m)| m.as_str())
    }

    pub fn account_error(&self, idx: usize, key: &str) -> Option<&str> {
        self.error(&format!("account[{idx}].{key}"))
    }

    /// Stages `new` for `field`; a value equal to the current one un-stages it.
    pub fn set_field(
        &mut self,
        id: &str,
        field: AccountField,
        current: &EditValue,
        new: EditValue,
        hash: &str,
    ) {
        if &new == current {
            if let Some(m) = self.edits.get_mut(id) {
                m.remove(&field);
                if m.is_empty() {
                    self.edits.remove(id);
                }
            }
        } else {
            self.edits
                .entry(id.to_string())
                .or_default()
                .insert(field, new);
        }
        self.touch(hash);
    }

    /// Remembers which file state the staged edits belong to; forgets it when nothing is staged.
    pub fn touch(&mut self, hash: &str) {
        if self.dirty() {
            self.base_hash.get_or_insert_with(|| hash.to_string());
            self.synced.get_or_insert_with(|| hash.to_string());
        } else {
            self.base_hash = None;
        }
    }

    /// Called every frame with the file hash the engine reports.
    pub fn observe(&mut self, hash: &str) {
        if !self.dirty() {
            self.synced = Some(hash.to_string());
        }
    }

    pub fn stage_password(&mut self, id: &str, pw: String, hash: &str) {
        if pw.is_empty() {
            self.passwords.remove(id);
        } else {
            self.passwords.insert(id.to_string(), Secret::new(pw));
        }
        self.touch(hash);
    }

    /// Stages an extensions list (equal to the stored one un-stages it).
    pub fn stage_blf(&mut self, id: &str, list: Vec<BlfConfig>, stored: &[BlfConfig], hash: &str) {
        if list == stored {
            self.blf.remove(id);
        } else {
            self.blf.insert(id.to_string(), list);
        }
        self.touch(hash);
    }

    /// Drops everything staged.
    pub fn revert(&mut self) {
        self.edits.clear();
        self.passwords.clear();
        self.blf.clear();
        self.base_hash = None;
        self.conflict_hash = None;
        self.errors.clear();
        self.notice = None;
    }

    /// The file changed on disk while edits are staged.
    pub fn external_change(&self, snap: &Snapshot) -> bool {
        self.dirty()
            && !self.applying
            && self
                .base_hash
                .as_deref()
                .is_some_and(|h| h != snap.config_hash)
    }

    /// What an Apply sends: the edits, the passwords to write to the keychain before them (an
    /// account already on the keychain: the new fields must not register with the old password)
    /// and after them (an account switching to the keychain: the edit moves the old password
    /// along first, then the new one replaces it).
    #[allow(clippy::type_complexity)]
    pub fn build_edits(
        &self,
        snap: &Snapshot,
    ) -> (
        Vec<ConfigEdit>,
        Vec<(String, Secret)>,
        Vec<(String, Secret)>,
    ) {
        let mut edits = Vec::new();
        let (mut pre, mut post) = (Vec::new(), Vec::new());
        for (id, fields) in &self.edits {
            for (field, value) in fields {
                edits.push(ConfigEdit::SetAccount {
                    id: id.clone(),
                    field: *field,
                    value: value.clone(),
                });
            }
        }
        for (id, pw) in &self.passwords {
            let on_keyring = snap
                .cfg
                .accounts
                .iter()
                .find(|a| &a.id == id)
                .is_some_and(|a| a.password_store == siple_core::config::PasswordStore::Keyring);
            let to_keyring = match self.pending(id, AccountField::PasswordStore) {
                Some(EditValue::Str(s)) => s == "keyring",
                _ => on_keyring,
            };
            match (to_keyring, on_keyring) {
                (true, true) => pre.push((id.clone(), pw.clone())),
                (true, false) => post.push((id.clone(), pw.clone())),
                (false, _) => edits.push(ConfigEdit::SetAccount {
                    id: id.clone(),
                    field: AccountField::Password,
                    value: EditValue::Str(pw.expose().to_string()),
                }),
            }
        }
        for (account, entries) in &self.blf {
            edits.push(ConfigEdit::SetBlf {
                account: account.clone(),
                entries: entries.clone(),
            });
        }
        (edits, pre, post)
    }

    /// Starts an Apply.
    pub fn apply(&mut self, snap: &Snapshot, out: &mut Vec<UiAction>) {
        let (edits, pre, post) = self.build_edits(snap);
        self.errors.clear();
        self.notice = None;
        let hash = self
            .base_hash
            .clone()
            .unwrap_or_else(|| snap.config_hash.clone());
        self.job = Some(ApplyJob {
            pre,
            edits,
            hash,
            edit_sent: false,
            post,
        });
        self.applying = true;
        self.advance_job(out);
    }

    /// Sends the next step of the Apply in flight; ends it when nothing is left.
    fn advance_job(&mut self, out: &mut Vec<UiAction>) {
        let Some(job) = self.job.as_mut() else { return };
        if let Some((id, pw)) = job.pre.first() {
            out.push(set_password_action(id, pw));
        } else if !job.edits.is_empty() && !job.edit_sent {
            job.edit_sent = true;
            out.push(rpc::config_edit(RpcTag::Apply, Some(&job.hash), &job.edits));
        } else {
            self.finish_job();
        }
    }

    /// The Apply went through: the keychain writes that follow, then a clean form.
    fn finish_job(&mut self) {
        if let Some(job) = self.job.take() {
            for (id, pw) in &job.post {
                self.outbox.push(set_password_action(id, pw));
            }
        }
        self.applying = false;
        self.revert();
    }

    /// The Apply failed: nothing it carried is written later; the edits stay staged.
    fn abort_job(&mut self) {
        self.job = None;
        self.applying = false;
    }

    fn edit_failed(&mut self, e: &rpc::RpcError, staged: bool) {
        match e.reason() {
            Some("conflict") => {
                self.notice = Some(CONFLICT_MSG.into());
                if let Some(h) = e.data["current_hash"].as_str() {
                    self.conflict_hash = Some(h.to_string());
                    if staged {
                        self.base_hash = Some(h.to_string());
                    }
                }
            }
            Some("invalid") => {
                self.errors = e.field_errors();
                if self.errors.is_empty() {
                    self.notice = Some(e.message.clone());
                }
            }
            Some("file_invalid") => {
                self.errors = e.field_errors();
                self.notice =
                    Some("The config file cannot be read; fix it first (Open config)".to_string());
            }
            _ => self.notice = Some(e.message.clone()),
        }
    }

    /// Handles the answer of a control-API call started from this page.
    pub fn on_reply(&mut self, reply: RpcReply, _snap: &Snapshot) {
        match (reply.tag, reply.result) {
            (RpcTag::Apply, Ok(_)) => self.finish_job(),
            (RpcTag::Apply, Err(e)) => {
                self.abort_job();
                self.edit_failed(&e, true);
            }
            (RpcTag::Instant, Ok(v)) => {
                // Our own write changed the file's hash: staged edits stay current.
                if let Some(h) = v["hash"].as_str() {
                    if self.base_hash.is_some() && self.base_hash == self.synced {
                        self.base_hash = Some(h.to_string());
                    }
                    self.synced = Some(h.to_string());
                }
            }
            (RpcTag::Instant, Err(e)) => self.edit_failed(&e, false),
            (RpcTag::TestAccount(id), Ok(v)) => {
                self.tests
                    .insert(id, TestRun::Done(rpc::TestResult::from_value(&v)));
            }
            (RpcTag::TestAccount(id), Err(e)) => {
                self.tests.insert(id, TestRun::Failed(e.message));
            }
            (RpcTag::AddAccount, Ok(_)) => {
                if let Some(f) = self.add.take() {
                    self.account = Some(f.account_id());
                }
                self.add_errors.clear();
                if let Some((id, pw)) = self.add_pw.take() {
                    self.outbox.push(set_password_action(&id, &pw));
                }
            }
            (RpcTag::AddAccount, Err(e)) => {
                self.add_pw = None;
                self.add_errors = e.field_errors();
                let hidden = super::draft::unshown(&self.add_errors);
                if self.add_errors.is_empty() {
                    self.notice = Some(e.message);
                } else if !hidden.is_empty() {
                    self.notice = Some(hidden.join("; "));
                }
            }
            (RpcTag::SetPassword(id), result) => {
                let before_edit = self
                    .job
                    .as_ref()
                    .is_some_and(|j| j.pre.first().is_some_and(|(p, _)| *p == id));
                match result {
                    Ok(_) if before_edit => {
                        if let Some(j) = self.job.as_mut() {
                            j.pre.remove(0);
                        }
                        let mut v = Vec::new();
                        self.advance_job(&mut v);
                        self.outbox.append(&mut v);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if before_edit {
                            self.abort_job();
                        }
                        self.notice = Some(format!(
                            "The password of {id} was not stored: {}",
                            e.message
                        ));
                    }
                }
            }
            (RpcTag::Echo, Err(e)) => self.notice = Some(e.message),
            (RpcTag::ShareLink(id), Ok(v)) => {
                let omitted: Vec<&str> = v["omitted"]
                    .as_array()
                    .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
                    .unwrap_or_default();
                let mut note = "Setup link copied. It has no password.".to_string();
                if !omitted.is_empty() {
                    note.push_str(&format!(" Not included: {}.", omitted.join(", ")));
                }
                self.copy_pending = v["link"].as_str().map(str::to_string);
                self.link_note = Some((id, note));
            }
            (RpcTag::ShareLink(id), Err(e)) => {
                self.link_note = Some((id, format!("Cannot build a setup link: {}", e.message)));
            }
            (RpcTag::CheckUpdates, _) => self.update_checking = false,
            _ => {}
        }
    }
}

// ---- routing ----------------------------------------------------------------------------------

/// Draws the wizard or the Settings page instead of the main window; `false` = draw the shell.
pub(in crate::ui) fn route(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    out: &mut Vec<UiAction>,
) -> bool {
    out.append(&mut st.settings.outbox);
    if super::keys::take(
        ui.ctx(),
        &cx.vm.snap.ui.shortcuts,
        siple_core::config::shortcuts::Shortcut::Settings,
    ) {
        st.settings.open = !st.settings.open;
    }
    st.wizard.decide(cx.vm.snap, out);
    if wizard::active(st) {
        super::stage::capture_banner(ui, cx, st);
        wizard::show(ui, cx, st, out);
        return true;
    }
    if st.settings.open {
        super::stage::capture_banner(ui, cx, st);
        show(ui, cx, st, out);
        return true;
    }
    false
}

fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let narrow = ui.available_width() < 640.0;
    // "Copy setup link": the link built for the account is put on the clipboard once.
    if let Some(link) = st.settings.copy_pending.take() {
        ui.ctx().copy_text(link);
    }
    st.settings.observe(&snap.config_hash);
    egui::Panel::top("settings_top")
        .frame(
            egui::Frame::new()
                .fill(p.surface)
                .inner_margin(Margin::symmetric(16, 10)),
        )
        .show_separator_line(true)
        .show(ui, |ui| top_bar(ui, cx, st, out));
    if st.settings.dirty()
        || st.settings.notice.is_some()
        || !st.settings.errors.is_empty()
        || st.settings.external_change(snap)
    {
        egui::Panel::bottom("settings_apply")
            .frame(
                egui::Frame::new()
                    .fill(p.surface)
                    .inner_margin(Margin::symmetric(16, 8)),
            )
            .show_separator_line(true)
            .show(ui, |ui| apply_bar(ui, cx, st, out));
    }
    if !narrow {
        egui::Panel::left("settings_nav")
            .exact_size(190.0)
            .resizable(false)
            .show_separator_line(true)
            .frame(
                egui::Frame::new()
                    .fill(p.surface)
                    .inner_margin(Margin::same(SP_S as i8 + 2)),
            )
            .show(ui, |ui| nav(ui, p, st, true));
    }
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::ZERO))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(Margin::same(SP_L as i8))
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = SP_S;
                            if narrow {
                                nav(ui, p, st, false);
                                ui.add_space(SP_XS);
                            }
                            content(ui, cx, st, out);
                        });
                });
        });
}

fn content(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let read_only = snap.config_unparsable;
    if let Some(err) = &snap.config_error {
        header::config_banner(ui, p, err, out);
    }
    if read_only {
        super::form::message_box(
            ui,
            p.warn,
            "The config file cannot be parsed, so Settings is read-only. Fix the file (Open config); this page updates when it parses again.",
        );
    }
    ui.label(strong(st.settings.section.label(), FS_TITLE + 3.0).color(p.text));
    let section = st.settings.section;
    let locked = read_only || st.settings.applying;
    ui.add_enabled_ui(!locked, |ui| match section {
        Section::Accounts => accounts::show(ui, cx, st, out),
        Section::Audio => audio_page::show(ui, cx, st, out),
        Section::Calls => pages::calls(ui, cx, out),
        Section::Extensions => extensions::show(ui, cx, st, out),
        Section::Shortcuts => pages::shortcuts(ui, cx),
        Section::Appearance => pages::appearance(ui, cx, out),
        Section::Notifications => pages::notifications(ui, cx, out),
        Section::Advanced => pages::advanced(ui, cx, st, out),
        Section::Logging => pages::logging(ui, cx, out),
        Section::Developer => pages::developer(ui, cx, out),
    });
}

/// The section list: arrow keys move between sections (Tab into an entry selects it).
fn nav(ui: &mut egui::Ui, p: &Palette, st: &mut UiState, vertical: bool) {
    let layout = if vertical {
        Layout::top_down_justified(Align::LEFT)
    } else {
        Layout::left_to_right(Align::Center).with_main_wrap(true)
    };
    ui.with_layout(layout, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 2.0);
        for s in Section::ALL {
            let selected = st.settings.section == s;
            let r = ui.add(
                egui::Button::new(if selected {
                    strong(s.label(), FS_BODY).color(p.accent)
                } else {
                    RichText::new(s.label()).size(FS_BODY).color(p.text)
                })
                .fill(if selected {
                    p.accent_soft
                } else {
                    egui::Color32::TRANSPARENT
                })
                .stroke(Stroke::NONE)
                .corner_radius(CornerRadius::same(R_MD))
                .min_size(egui::vec2(0.0, 30.0)),
            );
            r.widget_info(|| {
                egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, s.label())
            });
            if r.clicked() || r.gained_focus() {
                st.settings.section = s;
            }
        }
    });
}

fn top_bar(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let back = ui.add(
            egui::Button::new((
                icon(ic::ARROW_LEFT, FS_BODY + 1.0).color(p.muted),
                RichText::new("Back").size(FS_BODY).color(p.text),
            ))
            .corner_radius(CornerRadius::same(R_MD)),
        );
        back.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Back"));
        if back.clicked() {
            st.settings.open = false;
            st.settings.add = None;
            st.settings.add_pw = None;
        }
        ui.label(strong("Settings", FS_TITLE).color(p.text));
        call_strip(ui, cx, st, out);
    });
}

/// A slim top panel with the call strip, for pages that replace the shell (the wizard).
pub(in crate::ui) fn call_panel(ui: &mut egui::Ui, cx: &Cx, st: &UiState, out: &mut Vec<UiAction>) {
    if strip_call(cx).is_none() {
        return;
    }
    egui::Panel::top("call_panel")
        .frame(
            egui::Frame::new()
                .fill(cx.p.surface)
                .inner_margin(Margin::symmetric(16, 6)),
        )
        .show_separator_line(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| call_strip(ui, cx, st, out));
        });
}

/// The call the strip shows: the active line, else the first live one.
fn strip_call<'a>(cx: &Cx<'a>) -> Option<&'a siple_sip::CallInfo> {
    cx.vm
        .snap
        .active_line
        .as_ref()
        .and_then(|id| cx.call(id))
        .or_else(|| {
            cx.calls
                .iter()
                .find(|c| c.state != siple_sip::CallState::Ended)
                .copied()
        })
}

/// The live call, so it can be answered, muted or ended from here.
fn call_strip(ui: &mut egui::Ui, cx: &Cx, st: &UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let Some(c) = strip_call(cx) else { return };
    let ringing = c.direction == siple_sip::CallDirection::Inbound
        && c.state == siple_sip::CallState::Ringing;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if ringing {
            if solid_button(ui, crate::theme::icon::PHONE, "Answer", p.answer, 0.0).clicked() {
                out.push(UiAction::Answer(c.id.clone()));
            }
            if small_button(ui, p, "Decline", true).clicked() {
                out.push(UiAction::Reject(c.id.clone()));
            }
        } else {
            if solid_button(ui, crate::theme::icon::PHONE_X, "Hang up", p.hangup, 0.0).clicked() {
                out.push(UiAction::Hangup(c.id.clone()));
            }
            let label = if c.muted { "Unmute" } else { "Mute" };
            if small_button(ui, p, label, true).clicked() {
                out.push(UiAction::Mute(c.id.clone(), !c.muted));
            }
        }
        ui.add(
            egui::Label::new(
                RichText::new(format!(
                    "{} {} {}",
                    cx.slot(st, &c.id),
                    super::display_name(c),
                    crate::snapshot::format_duration(crate::snapshot::call_elapsed_secs(
                        c, cx.vm.now
                    ))
                ))
                .size(FS_SMALL)
                .color(p.text),
            )
            .truncate(),
        );
    });
}

fn apply_bar(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    ui.spacing_mut().item_spacing.y = SP_XS;
    if st.settings.external_change(snap) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("The config file changed.")
                    .size(FS_SMALL)
                    .color(p.warn.ink),
            );
            if small_button(ui, p, "Reload", true).clicked() {
                st.settings.revert();
            }
            if small_button(ui, p, "Keep mine", true).clicked() {
                st.settings.base_hash = Some(
                    st.settings
                        .conflict_hash
                        .take()
                        .unwrap_or_else(|| snap.config_hash.clone()),
                );
            }
        });
    }
    if let Some(n) = &st.settings.notice {
        ui.add(egui::Label::new(RichText::new(n).size(FS_SMALL).color(p.err.ink)).wrap());
    }
    for (path, msg) in st.settings.errors.iter().take(6) {
        let line = if path.is_empty() {
            msg.clone()
        } else {
            format!("{path}: {msg}")
        };
        ui.add(egui::Label::new(mono(line, FS_SMALL).color(p.err.ink)).wrap());
    }
    if st.settings.dirty() {
        ui.horizontal(|ui| {
            let n = st.settings.edits.values().map(BTreeMap::len).sum::<usize>()
                + st.settings.passwords.len()
                + st.settings.blf.len();
            ui.label(
                RichText::new(format!(
                    "{n} unsaved change{}",
                    if n == 1 { "" } else { "s" }
                ))
                .size(FS_SMALL)
                .color(p.muted),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let idle = !st.settings.applying;
                if primary_button(ui, p, "Apply", idle).clicked() {
                    st.settings.apply(snap, out);
                }
                if small_button(ui, p, "Revert", idle).clicked() {
                    st.settings.revert();
                }
            });
        });
    } else if (st.settings.notice.is_some() || !st.settings.errors.is_empty())
        && small_button(ui, p, "Dismiss", true).clicked()
    {
        st.settings.notice = None;
        st.settings.errors.clear();
    }
}
