//! Header: brand, one chip per account (click: account menu with enable switch, DND,
//! registration details, voicemail), the voicemail total and the extensions chevron. Also the
//! "config not applied" banner shown at the top of the stage.

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke};
use siple_core::update::UpdateAction;
use siple_sip::AccountStatus;

use super::form::problem_lines;
use super::{Cx, LayoutMode, UiAction, UiState, reg_pill_label, reg_tone, rpc, short_time};
use crate::snapshot::{
    ConfigErrorView, RegClass, mwi_label, reg_class, reg_failure, reg_label, reg_time_lines,
    reg_tooltip,
};
use crate::theme::{
    self, FS_BODY, FS_MICRO, FS_SMALL, FS_TITLE, Palette, R_LG, R_MD, SP_XS, icon as ic,
};
use crate::widgets::{
    Chevron, account_switch, brand_mark, chip, hairline, icon, icon_fill, mono, paint_chevron,
    paint_focus_ring, paint_lamp, pill, strong,
};

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let compact = cx.mode == LayoutMode::Narrow;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        // Brand mark: the app icon itself (same tile, same handset).
        brand_mark(ui, 26.0);
        ui.add_space(2.0);
        ui.label(strong(crate::APP_TITLE, FS_TITLE).color(p.text));
        ui.add_space(8.0);
        if cx.vm.snap.accounts.is_empty() {
            ui.label(RichText::new("No accounts").size(FS_SMALL).color(p.muted));
        }
        for a in &cx.vm.snap.accounts {
            ui.push_id(&a.id, |ui| account_chip(ui, cx, a, compact, out));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // Right to left: the extensions chevron sits at the far right, above the panel it toggles.
            ext_toggle(ui, cx, st);
            settings_button(ui, cx, st);
            voicemail(ui, cx);
            update_pill(ui, cx, st, out);
        });
    });
}

/// Opens the Settings page.
fn settings_button(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState) {
    let b = egui::Button::new(RichText::new("Settings").size(FS_SMALL).color(cx.p.text))
        .fill(Color32::TRANSPARENT)
        .corner_radius(CornerRadius::same(R_MD));
    let r = ui.add(b).on_hover_text("Settings (Ctrl/Cmd+,)");
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Settings"));
    if r.clicked() {
        st.settings.open = true;
    }
}

/// The Extensions control. Wide and medium: a chevron that collapses (points right, toward the
/// panel edge) or expands (points left) the right-hand panel, with the ringing count beside it.
/// Narrow: a text tab, because open swaps the whole central view.
fn ext_toggle(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState) {
    let p = cx.p;
    let open = cx.ext_open(st);
    let n = cx.ringing_extensions();
    let narrow = cx.mode == LayoutMode::Narrow;
    let r = if narrow {
        let text = match n {
            0 => "Ext".to_string(),
            n => format!("Ext {n}"),
        };
        let ink = if n > 0 { p.ring.ink } else { p.text };
        let b = egui::Button::new((
            icon(ic::DOTS_NINE, 15.0).color(if n > 0 { p.ring.ink } else { p.muted }),
            RichText::new(text).size(FS_SMALL).color(ink),
        ))
        .selected(open)
        .fill(if n > 0 {
            p.ring.soft
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(CornerRadius::same(R_MD));
        ui.add(b)
    } else {
        // Right-to-left layout: allocate the whole control, then paint count and chevron in it.
        let count = (n > 0).then(|| {
            ui.painter().layout_no_wrap(
                n.to_string(),
                egui::FontId::proportional(FS_SMALL),
                p.ring.ink,
            )
        });
        let w = 28.0 + count.as_ref().map_or(0.0, |g| g.size().x + 6.0);
        let (rect, r) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::click());
        let fill = if n > 0 {
            p.ring.soft
        } else if r.hovered() {
            p.accent_soft
        } else {
            Color32::TRANSPARENT
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same(R_MD), fill);
        let ink = if n > 0 { p.ring.ink } else { p.muted };
        let chev_x = rect.right() - 14.0;
        if let Some(g) = count {
            let pos = egui::pos2(rect.left() + 8.0, rect.center().y - g.size().y / 2.0);
            ui.painter().galley(pos, g, p.ring.ink);
        }
        paint_focus_ring(ui, p, &r, rect);
        paint_chevron(
            ui.painter(),
            egui::pos2(chev_x, rect.center().y),
            if open { Chevron::Right } else { Chevron::Left },
            10.0,
            ink,
        );
        r
    };
    let r = r.on_hover_text(if open {
        "Hide extensions"
    } else {
        "Show extensions"
    });
    r.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            true,
            open,
            match n {
                0 => "Extensions".to_string(),
                n => format!("Extensions, {n} ringing"),
            },
        )
    });
    if r.clicked() {
        if narrow {
            // The narrow tab is a transient view, not a saved panel choice.
            st.ext_tab = !open;
        } else {
            st.set_ext_panel(!open);
        }
    }
}

/// How the pill and its menu look for the offer's stage.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// A newer version exists; nothing started.
    Offer,
    /// Downloading or verifying (`percent` while the size is known).
    Progress(Option<u32>),
    /// Verified, waiting for a restart to be asked for.
    Ready,
    /// Verified, restarting when the calls are over.
    WaitingForCall,
    /// The new files are in place and the restart is under way.
    Restarting,
    /// The new files are in place; Siple could not restart itself.
    RestartByHand,
    /// The install failed; nothing was changed.
    Failed,
}

fn stage_of(offer: &crate::snapshot::UpdateOffer<'_>) -> Stage {
    use siple_core::update::InstallPhase as P;
    let Some(i) = offer.install else {
        return Stage::Offer;
    };
    match i.state {
        P::Idle => Stage::Offer,
        P::Downloading => Stage::Progress(
            i.total
                .filter(|t| *t > 0)
                .map(|t| (i.bytes.unwrap_or(0).min(t) * 100 / t) as u32),
        ),
        P::Verifying => Stage::Progress(None),
        P::Ready => Stage::Ready,
        P::Waiting => Stage::WaitingForCall,
        P::Committing | P::Restarting => Stage::Restarting,
        P::Installed if i.error.is_some() => Stage::RestartByHand,
        P::Installed => Stage::Restarting,
        P::Failed => Stage::Failed,
    }
}

/// "Update 0.2.0": a quiet pill, only while a newer release (not skipped) exists. Click opens a
/// small menu.
///
/// - Direct builds that can install: "Install and restart" ("Install, restart after the call" while
///   a call is active), Release notes, Skip this version. While it runs the pill reads
///   "Updating 45%" (Cancel), then "Restart to update" (disabled during a call), "Update failed"
///   (Try again, Download from GitHub).
/// - Direct builds that cannot (package manager, no write access, ...): Download, Release notes,
///   Skip this version, with a line saying why.
/// - Mac App Store: Open App Store, Skip this version. Microsoft Store (no version known): Open
///   Microsoft Store, Not now (this session only; absent for a mandatory update).
///
/// An install that this window did not ask for (an agent called `app.install_update`) is labelled
/// as such and can be cancelled from here.
fn update_pill(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let Some(offer) = cx.vm.snap.update_offer() else {
        return;
    };
    if offer.version.is_none() && st.update_not_now && !offer.mandatory {
        return;
    }
    let p = cx.p;
    let stage = stage_of(&offer);
    let by_agent = stage != Stage::Offer
        && stage != Stage::Failed
        && st.update_initiated.as_deref() != offer.version;
    let in_call = cx.vm.snap.call_active();
    let text = match (stage, offer.version) {
        (Stage::Progress(pct), _) => {
            let base = match pct {
                Some(p) => format!("Updating {p}%"),
                None => "Updating".to_string(),
            };
            if by_agent {
                format!("{base} (agent)")
            } else {
                base
            }
        }
        (Stage::Ready, _) if by_agent => "Restart pending (agent)".to_string(),
        (Stage::Ready, _) => "Restart to update".to_string(),
        (Stage::WaitingForCall, _) => "Restart after the call".to_string(),
        (Stage::Restarting, _) => "Restarting".to_string(),
        (Stage::RestartByHand, _) => "Restart to finish".to_string(),
        (Stage::Failed, _) => "Update failed".to_string(),
        (Stage::Offer, Some(v)) if cx.mode != LayoutMode::Narrow => format!("Update {v}"),
        (Stage::Offer, _) => "Update".to_string(),
    };
    let galley = ui.painter().layout_no_wrap(
        text.clone(),
        egui::FontId::new(FS_SMALL, theme::semibold()),
        p.accent,
    );
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(galley.size().x + 20.0, 24.0),
        egui::Sense::click(),
    );
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    ui.painter().rect(
        rect,
        CornerRadius::same(255),
        p.accent_soft,
        Stroke::new(
            1.0,
            if open || resp.hovered() {
                p.accent
            } else {
                p.accent_soft
            },
        ),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(
        egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        p.accent,
    );
    paint_focus_ring(ui, p, &resp, rect);
    let label = match (stage, offer.version) {
        (Stage::Offer, Some(v)) => format!("Siple {v} is available"),
        (Stage::Offer, None) => "A Siple update is available".to_string(),
        _ => text,
    };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label.clone()));
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(label);
    egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(240.0);
            ui.spacing_mut().item_spacing.y = SP_XS + 2.0;
            let title = match offer.version {
                Some(v) => format!("New version {v}"),
                None => "An update is available".to_string(),
            };
            ui.label(strong(title, FS_BODY).color(p.text));
            ui.label(
                RichText::new(format!("You are running {}.", cx.vm.snap.update.current))
                    .size(FS_SMALL)
                    .color(p.muted),
            );
            if by_agent {
                ui.label(
                    RichText::new("An agent started this update.")
                        .size(FS_SMALL)
                        .color(p.warn.ink),
                );
            }
            if let Some(e) = &st.update_error {
                ui.label(RichText::new(e.as_str()).size(FS_SMALL).color(p.err.ink));
            }
            hairline(ui, p);
            let version = offer.version.unwrap_or_default().to_string();
            let url = offer.release_url.unwrap_or_default().to_string();
            match (offer.action, stage) {
                (UpdateAction::OpenStore, _) => {
                    if ui.button(offer.store_button()).clicked() {
                        out.push(UiAction::OpenStorePage);
                        ui.close();
                    }
                }
                (UpdateAction::Install, Stage::Offer | Stage::Failed) => {
                    if stage == Stage::Failed {
                        let why = offer
                            .install
                            .and_then(|i| i.error.as_deref())
                            .unwrap_or("The update could not be installed.");
                        ui.label(RichText::new(why).size(FS_SMALL).color(p.err.ink));
                    }
                    let (item, restart) = if in_call {
                        ("Install, restart after the call", "manual")
                    } else {
                        ("Install and restart", "when_idle")
                    };
                    let item = if stage == Stage::Failed {
                        "Try again"
                    } else {
                        item
                    };
                    if ui.button(item).clicked() {
                        st.update_initiated = Some(version.clone());
                        st.update_error = None;
                        out.push(rpc::install_update(&version, restart));
                        ui.close();
                    }
                    if stage == Stage::Failed && ui.button("Download from GitHub").clicked() {
                        out.push(UiAction::OpenUrl(url.clone()));
                        ui.close();
                    }
                    if stage == Stage::Offer && ui.button("Release notes").clicked() {
                        out.push(UiAction::OpenUrl(url.clone()));
                        ui.close();
                    }
                }
                (UpdateAction::Install, Stage::Progress(_)) => {
                    ui.label(
                        RichText::new("Downloading and checking the new version.")
                            .size(FS_SMALL)
                            .color(p.muted),
                    );
                    if ui.button("Cancel").clicked() {
                        out.push(rpc::cancel_update());
                        ui.close();
                    }
                }
                (UpdateAction::Install, Stage::Ready | Stage::WaitingForCall) => {
                    let restart_now =
                        ui.add_enabled(!in_call, egui::Button::new("Restart to update"));
                    if in_call {
                        ui.label(
                            RichText::new("Siple restarts when the call is over.")
                                .size(FS_SMALL)
                                .color(p.muted),
                        );
                    }
                    if restart_now.clicked() {
                        st.update_initiated = Some(version.clone());
                        st.update_error = None;
                        out.push(rpc::install_update(&version, "now"));
                        ui.close();
                    }
                    if stage == Stage::Ready && !in_call && ui.button("Restart when idle").clicked()
                    {
                        out.push(rpc::install_update(&version, "when_idle"));
                        ui.close();
                    }
                    if ui.button("Cancel").clicked() {
                        out.push(rpc::cancel_update());
                        ui.close();
                    }
                }
                (UpdateAction::Install, Stage::Restarting) => {
                    ui.label(
                        RichText::new("Siple is restarting into the new version.")
                            .size(FS_SMALL)
                            .color(p.muted),
                    );
                }
                (UpdateAction::Install, Stage::RestartByHand) => {
                    let why = offer
                        .install
                        .and_then(|i| i.error.as_deref())
                        .unwrap_or("Restart Siple to use the new version.");
                    ui.label(RichText::new(why).size(FS_SMALL).color(p.muted));
                }
                _ => {
                    // `update_offer` only returns `OpenReleasePage` with a page to open.
                    for item in ["Download", "Release notes"] {
                        if ui.button(item).clicked() {
                            out.push(UiAction::OpenUrl(url.clone()));
                            ui.close();
                        }
                    }
                    if let Some(i) = offer.install
                        && let Some(r) = i.reason.as_deref()
                    {
                        ui.label(
                            RichText::new(crate::snapshot::install_reason_text(r))
                                .size(FS_SMALL)
                                .color(p.muted),
                        );
                    }
                }
            }
            if matches!(stage, Stage::Offer | Stage::Failed) {
                // Drawing a button inside a match guard would hide the side effect.
                #[allow(clippy::collapsible_match)]
                match offer.version {
                    Some(v) => {
                        if ui.button("Skip this version").clicked() {
                            out.push(UiAction::SkipUpdate(v.to_string()));
                            ui.close();
                        }
                    }
                    None if !offer.mandatory => {
                        if ui.button("Not now").clicked() {
                            st.update_not_now = true;
                            ui.close();
                        }
                    }
                    None => {}
                }
            }
        });
}

/// Voicemail total over the accounts that report MWI.
fn voicemail(ui: &mut egui::Ui, cx: &Cx) {
    let (mut new, mut old, mut any) = (0, 0, false);
    for m in cx.vm.snap.accounts.iter().filter_map(|a| a.mwi.as_ref()) {
        new += m.new;
        old += m.old;
        any = true;
    }
    if !any {
        return;
    }
    let p = cx.p;
    let waiting = new > 0;
    chip(
        ui,
        p,
        Some(ic::ENVELOPE_SIMPLE),
        if waiting {
            strong(format!("VM {new}"), FS_MICRO + 0.5)
        } else {
            RichText::new("VM").size(FS_MICRO + 0.5)
        },
        waiting.then_some(p.warn),
    )
    .on_hover_text(if waiting {
        format!("Voicemail: {new} new / {old} old")
    } else {
        "Voicemail: no new messages".to_string()
    });
}

/// Account chip: lamp (+ name unless compact). Click opens the account menu.
fn account_chip(
    ui: &mut egui::Ui,
    cx: &Cx,
    a: &AccountStatus,
    compact: bool,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    let class = reg_class(a);
    let tone = reg_tone(p, class);
    let galley = ui.painter().layout_no_wrap(
        a.id.clone(),
        egui::FontId::new(FS_SMALL, theme::semibold()),
        if a.enabled { p.text } else { p.faint },
    );
    let w = if compact {
        30.0
    } else {
        10.0 + 8.0 + 6.0 + galley.size().x + 10.0 + if a.dnd { 16.0 } else { 0.0 }
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 26.0), egui::Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), egui::Popup::default_response_id(&resp));
    let fill = if open || resp.hovered() {
        p.inset
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(R_MD),
        fill,
        Stroke::new(1.0, if open { p.border_strong } else { p.border }),
        egui::StrokeKind::Inside,
    );
    paint_lamp(
        ui,
        egui::pos2(rect.left() + 14.0, rect.center().y),
        tone,
        class != RegClass::Inactive,
        8.0,
    );
    if !compact {
        let text_x = rect.left() + 28.0;
        let galley_w = galley.size().x;
        ui.painter().galley(
            egui::pos2(text_x, rect.center().y - galley.size().y / 2.0),
            galley,
            p.text,
        );
        if a.dnd {
            ui.painter().text(
                egui::pos2(text_x + galley_w + 10.0, rect.center().y),
                egui::Align2::CENTER_CENTER,
                ic::BELL_SLASH,
                egui::FontId::new(FS_SMALL + 1.0, theme::icons_fill()),
                p.err.ink,
            );
        }
    }
    let label = format!(
        "Account {}: {}{}",
        a.id,
        reg_label(a),
        if a.dnd { ", do not disturb" } else { "" }
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label.clone()));
    let mut tip = label;
    // The label already carries the problem's title: add its hint and details, then the times.
    let mut lines = reg_failure(a)
        .map(|f| f.lines().split_off(1))
        .unwrap_or_default();
    lines.extend(reg_time_lines(a));
    for l in lines {
        tip.push('\n');
        tip.push_str(&l);
    }
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip);
    egui::Popup::from_toggle_button_response(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| account_menu(ui, cx, a, out));
}

/// Enable switch, DND switch, registration details, voicemail and the config shortcut.
fn account_menu(ui: &mut egui::Ui, cx: &Cx, a: &AccountStatus, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let class = reg_class(a);
    ui.set_min_width(300.0);
    ui.spacing_mut().item_spacing.y = SP_XS + 2.0;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let toggle = account_switch(ui, p, a.enabled, &format!("Enable {}", a.id));
        if toggle
            .on_hover_text(if a.enabled {
                "Disable account"
            } else {
                "Enable account"
            })
            .clicked()
        {
            out.push(UiAction::SetAccountEnabled(a.id.clone(), !a.enabled));
        }
        ui.label(strong(&a.id, FS_BODY).color(p.text));
        chip(ui, p, None, mono(transport_label(a), FS_MICRO), None);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            pill(
                ui,
                reg_tone(p, class),
                reg_pill_label(class, a),
                class != RegClass::Inactive,
            );
        });
    });
    ui.add(egui::Label::new(mono(&a.aor, FS_SMALL).color(p.muted)).truncate());
    if let Some(f) = reg_failure(a) {
        problem_lines(ui, p, p.err, &f.title, f.hint, &f.detail);
    } else {
        ui.add(
            egui::Label::new(
                RichText::new(reg_tooltip(a, cx.vm.now))
                    .size(FS_SMALL)
                    .color(p.muted),
            )
            .wrap(),
        );
    }
    hairline(ui, p);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let dnd = account_switch(ui, p, a.dnd, &format!("Do not disturb {}", a.id));
        if dnd
            .on_hover_text("Reject incoming calls on this account until the config changes it")
            .clicked()
        {
            out.push(UiAction::SetDnd(a.id.clone(), !a.dnd));
        }
        ui.label(RichText::new("Do not disturb").size(FS_BODY).color(p.text));
    });
    if let Some(m) = &a.mwi {
        let waiting = m.waiting;
        ui.horizontal(|ui| {
            ui.label(icon(ic::ENVELOPE_SIMPLE, 15.0).color(if waiting {
                p.warn.ink
            } else {
                p.muted
            }));
            ui.label(
                RichText::new(format!("Voicemail: {}", mwi_label(m)))
                    .size(FS_SMALL)
                    .color(p.text),
            );
        });
    }
    if a.pending_rebuild {
        ui.horizontal(|ui| {
            ui.label(icon(ic::ARROWS_CLOCKWISE, 15.0).color(p.warn.ink));
            ui.label(
                RichText::new("Transport changes apply when active calls end")
                    .size(FS_SMALL)
                    .color(p.warn.ink),
            );
        });
    }
    if ui.button("Edit in config file\u{2026}").clicked() {
        out.push(UiAction::OpenConfig);
        ui.close();
    }
}

// ---- config error ----------------------------------------------------------------------------

pub(super) fn config_banner(
    ui: &mut egui::Ui,
    p: &Palette,
    err: &ConfigErrorView,
    out: &mut Vec<UiAction>,
) {
    egui::Frame::new()
        .fill(p.err.soft)
        .stroke(Stroke::new(1.0, p.err.led))
        .corner_radius(CornerRadius::same(R_LG))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = SP_XS;
            ui.horizontal(|ui| {
                ui.label(icon_fill(ic::WARNING, 17.0).color(p.err.ink));
                ui.label(strong("Config not applied", FS_BODY).color(p.err.ink));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Open config").size(FS_SMALL).color(p.err.ink),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0, p.err.led))
                            .corner_radius(CornerRadius::same(R_MD)),
                        )
                        .clicked()
                    {
                        out.push(UiAction::OpenConfig);
                    }
                });
            });
            ui.label(
                RichText::new(format!(
                    "The previous config is still active. Rejected at {}.",
                    short_time(&err.at)
                ))
                .size(FS_SMALL)
                .color(p.text),
            );
            ui.add_space(2.0);
            for e in err.errors.iter().take(5) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(RichText::new("\u{2022}").size(FS_SMALL).color(p.err.ink));
                    ui.add(egui::Label::new(mono(e, 11.5).color(p.text)).wrap());
                });
            }
            if err.errors.len() > 5 {
                ui.label(
                    RichText::new(format!("and {} more", err.errors.len() - 5))
                        .size(FS_SMALL)
                        .color(p.muted),
                );
            }
        });
}

/// "UDP", or "TLS (auto)" for `transport = "auto"` once a transport was picked.
fn transport_label(a: &AccountStatus) -> String {
    let up = |t: siple_core::config::Transport| format!("{t:?}").to_uppercase();
    match (a.transport.is_auto(), a.transport_effective) {
        (true, Some(t)) => format!("{} (auto)", up(t)),
        _ => up(a.transport),
    }
}
