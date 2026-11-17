//! Header: brand, one chip per account (click: account menu with enable switch, DND,
//! registration details, voicemail), the voicemail total and the extensions chevron. Also the
//! "config not applied" banner shown at the top of the stage.

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke};
use siple_sip::AccountStatus;

use super::{Cx, LayoutMode, UiAction, UiState, reg_pill_label, reg_tone, short_time};
use crate::snapshot::{ConfigErrorView, RegClass, mwi_label, reg_class, reg_label, reg_tooltip};
use crate::theme::{
    self, FS_BODY, FS_MICRO, FS_SMALL, FS_TITLE, Palette, R_LG, R_MD, SP_XS, icon as ic,
};
use crate::widgets::{
    Chevron, account_switch, chip, hairline, icon, icon_fill, mono, paint_chevron,
    paint_focus_ring, paint_lamp, pill, strong,
};

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let compact = cx.mode == LayoutMode::Narrow;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        // Brand mark: the app icon's handset on the accent.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(R_MD), p.accent);
        ui.painter().text(
            rect.center() + egui::vec2(0.0, 1.0),
            egui::Align2::CENTER_CENTER,
            ic::PHONE,
            egui::FontId::new(16.0, theme::icons_fill()),
            p.on_accent,
        );
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
        st.ext_open = Some(!open);
    }
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
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(label);
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
    ui.add(
        egui::Label::new(
            RichText::new(reg_tooltip(a, cx.vm.now))
                .size(FS_SMALL)
                .color(p.muted),
        )
        .wrap(),
    );
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
    if class == RegClass::Failed {
        ui.horizontal(|ui| {
            ui.label(icon(ic::WARNING_CIRCLE, 15.0).color(p.err.ink));
            ui.label(RichText::new(reg_label(a)).size(FS_SMALL).color(p.err.ink));
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
