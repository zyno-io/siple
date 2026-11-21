//! The Review screen of a `siple:provision` setup link, and the paste field that opens it.
//!
//! The screen leads with the server the account would connect to, because that is the one
//! thing a malicious link can use against the user: it is where the password and the calls
//! would go. It sends and writes nothing; Continue (which works one second after the screen
//! appears or the link changes) only opens the account form, prefilled but without a password.

use chrono::Duration as ChronoDuration;
use eframe::egui::{self, Align, FontId, Layout, RichText};
use siple_core::provision::{SetupAccount, SetupLink};

use super::draft::{DraftForm, PROFILES, SRTP, TRANSPORTS};
use super::form::{group, message_box, primary_button, small_button, text_row};
use super::wizard::{REVIEW_ARM_MS, Step};
use super::{Cx, UiAction, UiState};
use crate::theme::{self, FS_BODY, FS_SMALL, FS_TITLE, Palette, SP_S};
use crate::widgets::{hairline, mono, strong};

/// Width of the label column of the detail rows.
const KEY_W: f32 = 150.0;

fn label_of(table: &[(&str, &str)], value: &str) -> String {
    table
        .iter()
        .find(|(v, _)| *v == value)
        .map_or(value, |(_, t)| *t)
        .to_string()
}

/// One `label: value` row.
fn kv(ui: &mut egui::Ui, p: &Palette, key: &str, value: &str, monospace: bool) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.allocate_ui_with_layout(
            egui::vec2(KEY_W, 0.0),
            Layout::left_to_right(Align::TOP),
            |ui| {
                ui.set_width(KEY_W);
                ui.add(
                    egui::Label::new(RichText::new(key).size(FS_SMALL).color(p.muted)).truncate(),
                );
            },
        );
        let text = if monospace {
            mono(value, FS_SMALL).color(p.text)
        } else {
            RichText::new(value).size(FS_SMALL).color(p.text)
        };
        ui.add(egui::Label::new(text).wrap());
    });
}

/// The server, large and selectable; it may wrap anywhere so a long name cannot hide its end.
fn server_name(ui: &mut egui::Ui, p: &Palette, hop: &str) {
    let mut job = egui::text::LayoutJob::single_section(
        hop.to_string(),
        egui::TextFormat {
            font_id: FontId::new(FS_TITLE + 2.0, theme::mono_medium()),
            color: p.text,
            ..Default::default()
        },
    );
    job.wrap.break_anywhere = true;
    let r = ui.add(egui::Label::new(job).selectable(true));
    r.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("Server: {hop}"))
    });
}

/// The details of the link, always the same rows so nothing is hidden by being absent.
fn details(ui: &mut egui::Ui, p: &Palette, a: &SetupAccount) {
    use siple_core::provision::{profile_name, srtp_name, transport_name};
    kv(
        ui,
        p,
        "Your address",
        &format!("{}@{}", a.username, a.domain),
        true,
    );
    let t = a.transport.map_or("auto", transport_name);
    kv(ui, p, "Transport", &label_of(TRANSPORTS, t), false);
    let s = a.srtp.map_or("optional", srtp_name);
    kv(ui, p, "SRTP", &label_of(SRTP, s), false);
    let pr = a.profile.map_or("generic", profile_name);
    kv(ui, p, "Phone profile", &label_of(PROFILES, pr), false);
    kv(
        ui,
        p,
        "Password",
        "not in the link; you type it next",
        false,
    );
    if !a.display_name.is_empty() {
        kv(ui, p, "Display name", &a.display_name, false);
    }
    if !a.auth_username.is_empty() {
        kv(ui, p, "Auth username", &a.auth_username, true);
    }
    if !a.registrar.is_empty() {
        kv(ui, p, "Registrar", &a.registrar, true);
    }
    if !a.outbound_proxy.is_empty() {
        kv(ui, p, "Outbound proxy", &a.outbound_proxy, true);
    }
}

/// Draws the Review step of the wizard.
pub(super) fn review(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let Some(link) = st.wizard.link.clone() else {
        st.wizard.step = Step::Account;
        return;
    };
    let a = &link.account;
    let snap = cx.vm.snap;
    let now = cx.vm.now;

    // The delay: from when the screen appeared or the link changed. A clock that went back, or
    // a window that was away while Continue was still locked, starts it again.
    let w = &mut st.wizard;
    let mut at = *w.review_at.get_or_insert(now);
    let armed_before = now >= at + ChronoDuration::milliseconds(REVIEW_ARM_MS);
    let away = w
        .review_seen
        .is_some_and(|seen| now - seen > ChronoDuration::milliseconds(750));
    if now < at || (!armed_before && away) {
        at = now;
        w.review_at = Some(at);
    }
    w.review_seen = Some(now);
    let arm_at = at + ChronoDuration::milliseconds(REVIEW_ARM_MS);
    let armed = now >= arm_at;
    if !armed {
        let left = (arm_at - now).to_std().unwrap_or_default();
        ui.ctx()
            .request_repaint_after(left + std::time::Duration::from_millis(20));
    }

    let existing = a.existing_in(&snap.cfg.accounts);
    let unparsable = snap.config_unparsable;
    let replaced = w.link_replaced;

    ui.label(strong("Check this setup link", FS_BODY + 1.0).color(p.text));
    ui.add(
        egui::Label::new(
            RichText::new(
                "It fills in your account. Nothing is saved or sent until you continue and test \
                 it. Only continue if your administrator sent it.",
            )
            .size(FS_SMALL)
            .color(p.muted),
        )
        .wrap(),
    );
    group(ui, p, "", |ui| {
        ui.label(
            RichText::new("SERVER")
                .size(crate::theme::FS_MICRO)
                .color(p.muted),
        );
        server_name(ui, p, &a.next_hop());
        ui.label(
            RichText::new("Your password and calls go to this server.")
                .size(FS_SMALL)
                .color(p.muted),
        );
    });
    for warning in a.warnings() {
        message_box(ui, p.warn, &warning.message());
    }
    hairline(ui, p);
    egui::Frame::new().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = SP_S - 2.0;
        details(ui, p, a);
    });
    if replaced {
        message_box(
            ui,
            p.warn,
            "The setup link changed while you were looking at it. Check the server again.",
        );
    }
    if let Some(id) = &existing {
        message_box(
            ui,
            p.accent_tone(),
            &format!(
                "You already have this account ({id}). Siple does not add it twice; change it in \
                 Settings \u{203a} Accounts."
            ),
        );
    }
    if unparsable {
        message_box(
            ui,
            p.err,
            "Fix the config file first: Siple cannot add an account while it cannot read it.",
        );
    }

    ui.add_space(SP_S);
    let mut cont = false;
    let mut cancel = false;
    let mut open_settings = false;
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if existing.is_some() {
                open_settings = primary_button(ui, p, "Open Settings", true).clicked();
            } else {
                let r = primary_button(ui, p, "Continue", armed && !unparsable);
                if !armed {
                    r.clone()
                        .on_disabled_hover_text("Read the server name first");
                }
                cont = r.clicked();
            }
            cancel = small_button(ui, p, "Cancel", true).clicked();
        });
    });

    let w = &mut st.wizard;
    if proceed(w, a, cont, armed, unparsable) {
        // Continued.
    } else if open_settings {
        if let Some(id) = existing {
            st.settings.account = Some(id);
        }
        st.settings.section = super::settings::Section::Accounts;
        st.settings.open = true;
        if st.wizard.link_only {
            st.wizard.cancel_link();
        } else {
            // The first-run wizard would cover Settings: the user already has the account.
            st.wizard.finish(out);
        }
    } else if cancel {
        w.cancel_link();
    }
}

/// Continue on the Review screen: opens the account form, prefilled from the link, only when the
/// delay has passed (`armed`) and the config can be written. The button is disabled otherwise, but
/// this is the check: a click that slips through (AccessKit, a forced action) does nothing.
pub(super) fn proceed(
    w: &mut super::wizard::WizardState,
    a: &SetupAccount,
    cont: bool,
    armed: bool,
    unparsable: bool,
) -> bool {
    if cont && armed && !unparsable {
        w.form = DraftForm::from_link(a);
        w.step = Step::Account;
        w.review_at = None;
        w.review_seen = None;
        w.link_replaced = false;
        return true;
    }
    false
}

/// The paste field: `text` is the open field (or `None`), `error` what was wrong with the last
/// try. `Some(link)` when the user asked to review a link that parsed.
pub(super) fn paste_row(
    ui: &mut egui::Ui,
    p: &Palette,
    text: &mut Option<String>,
    error: &mut Option<String>,
) -> Option<SetupLink> {
    let buf = text.as_ref()?.clone();
    let mut result = None;
    let mut close = false;
    if let Some(v) = text_row(
        ui,
        p,
        "Setup link",
        &buf,
        "siple:provision?d=...",
        false,
        error.as_deref(),
    ) {
        *text = Some(v);
        *error = None;
    }
    let cur = text.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        if small_button(ui, p, "Review link", !cur.trim().is_empty()).clicked() {
            match siple_core::uri::parse_pasted(&cur) {
                Ok(l) => result = Some(l),
                Err(e) => *error = Some(e.to_string()),
            }
        }
        if small_button(ui, p, "Cancel", true).clicked() {
            close = true;
        }
    });
    // The field stays open on success: the caller closes it once the link was taken, and keeps
    // it (with the reason) when the wizard cannot take the link now.
    if close {
        *text = None;
        *error = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::wizard::WizardState;

    fn account() -> SetupAccount {
        SetupAccount {
            username: "1001".into(),
            domain: "example.test".into(),
            ..Default::default()
        }
    }

    /// The disabled Continue button is only the first line: a Continue that arrives before the
    /// delay passed (or while the config cannot be read) must not open the form.
    #[test]
    fn continue_before_the_delay_or_on_an_unreadable_config_does_nothing() {
        let a = account();
        for (cont, armed, unparsable) in [
            (true, false, false),
            (true, true, true),
            (false, true, false),
        ] {
            let mut w = WizardState::default();
            assert!(!proceed(&mut w, &a, cont, armed, unparsable));
            assert_eq!(w.step, Step::Account, "default step");
            assert!(w.form.username.is_empty(), "{cont} {armed} {unparsable}");
        }
        let mut w = WizardState::default();
        w.step = Step::Review;
        assert!(!proceed(&mut w, &a, true, false, false));
        assert_eq!(w.step, Step::Review);
        assert!(w.form.username.is_empty());
        assert!(proceed(&mut w, &a, true, true, false));
        assert_eq!(w.step, Step::Account);
        assert_eq!(w.form.username, "1001");
    }
}
