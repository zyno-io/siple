//! Lines: the left rail (wide / medium) and the tab strip (narrow). One entry per call, in a
//! stable slot order (L1, L2, ...). Clicking a held line makes it the active line
//! (`call.activate`); every click also selects it for the stage.

use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke};
use siple_sip::{CallDirection, CallInfo, CallState, EndCause, TransferPhase};

use super::{Cx, UiAction, UiState, display_name, session_for, state_tone};
use crate::snapshot::{call_elapsed_secs, format_duration};
use crate::theme::{FS_BODY, FS_MICRO, FS_SMALL, R_MD, SP_S, Tone, icon as ic};
use crate::widgets::{icon, paint_lamp, section_title, soft_button};

/// Glyph, tone and one-line text of a line.
pub(super) fn line_summary(cx: &Cx, st: &UiState, c: &CallInfo) -> (&'static str, Tone, String) {
    let p = cx.p;
    let now = cx.vm.now;
    let tone = state_tone(p, c.state);
    let timer = format_duration(call_elapsed_secs(c, now));
    let session = session_for(cx, st, &c.id);
    let (glyph, tone, mut text) = match c.state {
        CallState::Ringing if c.direction == CallDirection::Inbound => (
            ic::PHONE_INCOMING,
            tone,
            format!("Ringing \u{b7} {}", c.account),
        ),
        CallState::Ringing => (ic::PHONE_INCOMING, tone, "Ringing".to_string()),
        CallState::Dialing => (ic::PHONE_OUTGOING, tone, "Calling\u{2026}".to_string()),
        CallState::Early if c.early_media => (ic::PHONE_OUTGOING, tone, "Early media".to_string()),
        CallState::Early => (ic::PHONE_OUTGOING, tone, "Ringing\u{2026}".to_string()),
        CallState::Active => (ic::PHONE, tone, timer),
        CallState::Held => {
            let since = c.hold_changed_at.map_or_else(
                || call_elapsed_secs(c, now),
                |t| (now - t).num_seconds().max(0),
            );
            (
                ic::PAUSE,
                tone,
                format!("On hold {}", format_duration(since)),
            )
        }
        CallState::RemoteHeld => (ic::PAUSE, tone, "Held by other side".to_string()),
        CallState::Ended => (ic::PHONE_X, tone, ended_text(c)),
    };
    if let Some(s) = session
        && c.state != CallState::Ended
    {
        if matches!(
            s.phase,
            TransferPhase::Referring | TransferPhase::Transferring
        ) {
            text = format!("Transferring to {}\u{2026}", s.target_uri);
            return (ic::PHONE_TRANSFER, p.accent_tone(), text);
        }
        if s.phase.is_consult() || !s.phase.is_terminal() {
            text.push_str(" \u{b7} transfer");
        }
    }
    (glyph, tone, text)
}

/// End cause as the rail shows it: "Transferred", "Missed", "Busy 486", "Declined".
fn ended_text(c: &CallInfo) -> String {
    match &c.end {
        Some(e) => match e.cause {
            EndCause::Transferred => "Transferred".into(),
            EndCause::Missed => "Missed".into(),
            EndCause::Rejected if e.code == 486 => "Busy 486".into(),
            EndCause::Rejected => "Declined".into(),
            EndCause::Hangup | EndCause::RemoteHangup => "Ended".into(),
            _ => format!("{} {}", e.code, e.reason).trim().to_string(),
        },
        None => "Ended".into(),
    }
}

/// Accessible name of a line: `Line 1, Bob Martin, 4:32, desk-1001`.
fn access_label(cx: &Cx, st: &UiState, c: &CallInfo) -> String {
    let (_, _, text) = line_summary(cx, st, c);
    let slot = st.slots.get(&c.id).copied().unwrap_or(0);
    format!("Line {slot}, {}, {text}, {}", display_name(c), c.account)
}

/// A click on a line: select it; a held line becomes the active line; the other leg of a
/// consult session swaps the session.
pub(super) fn click_line(cx: &Cx, st: &mut UiState, c: &CallInfo, out: &mut Vec<UiAction>) {
    st.selected_line = Some(c.id.clone());
    st.new_call_open = false;
    leave_extensions_tab(cx, st);
    let session = session_for(cx, st, &c.id).filter(|s| !s.phase.is_terminal());
    match session {
        // In a live session the session owns the lines: only a connected consult swaps (or,
        // with nobody talking, activates the clicked leg); every other phase only selects.
        Some(s) if s.phase == TransferPhase::ConsultConnected && c.state != CallState::Ended => {
            match s.talking_to.as_deref() {
                None => out.push(UiAction::Activate(c.id.clone())),
                Some(t) if t != c.id => out.push(UiAction::TransferSwap(s.id.clone())),
                Some(_) => {}
            }
        }
        Some(_) => {}
        None if c.hold.local && c.state == CallState::Held => {
            out.push(UiAction::Activate(c.id.clone()));
        }
        None => {}
    }
}

/// Narrow: picking a line returns from the extensions tab to the stage.
pub(super) fn leave_extensions_tab(cx: &Cx, st: &mut UiState) {
    if cx.mode == super::LayoutMode::Narrow {
        st.ext_open = Some(false);
    }
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    ui.spacing_mut().item_spacing.y = SP_S;
    let lines = cx.lines(st);
    let meta = (!lines.is_empty()).then(|| lines.len().to_string());
    section_title(ui, p, "LINES", meta.as_deref());
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height((ui.available_height() - 48.0).max(60.0))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            if lines.is_empty() {
                ui.label(
                    RichText::new("No lines in use")
                        .size(FS_SMALL)
                        .color(p.muted),
                );
            }
            for (slot, c) in &lines {
                if line_row(ui, cx, st, *slot, c) {
                    click_line(cx, st, c, out);
                }
            }
        });
    ui.add_space(2.0);
    if soft_button(
        ui,
        p,
        ic::PHONE_OUTGOING,
        "+ New call",
        None,
        true,
        ui.available_width(),
    )
    .clicked()
    {
        open_new_call(cx, st);
    }
}

/// "+ New call": opens the dial bar in the stage, defaulting to the selected line's account.
pub(super) fn open_new_call(cx: &Cx, st: &mut UiState) {
    st.new_call_open = true;
    st.ext_open = if cx.mode == super::LayoutMode::Narrow {
        Some(false)
    } else {
        st.ext_open
    };
    if let Some(c) = cx.selected(st)
        && cx
            .vm
            .snap
            .accounts
            .iter()
            .any(|a| a.enabled && a.id == c.account)
    {
        st.dial_account = Some(c.account.clone());
    }
}

/// One rail row; returns true when clicked.
fn line_row(ui: &mut egui::Ui, cx: &Cx, st: &UiState, slot: usize, c: &CallInfo) -> bool {
    let p = cx.p;
    let selected = st.selected_line.as_deref() == Some(c.id.as_str());
    let (glyph, tone, text) = line_summary(cx, st, c);
    let incoming = c.direction == CallDirection::Inbound && c.state == CallState::Ringing;
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 62.0), egui::Sense::click());
    let painter = ui.painter_at(rect);
    let fill = if selected {
        p.accent_soft
    } else if resp.hovered() {
        p.inset
    } else {
        Color32::TRANSPARENT
    };
    let stroke = if incoming {
        Stroke::new(
            if cx.blink || cx.reduce_motion {
                2.0
            } else {
                1.0
            },
            p.ring.led,
        )
    } else if selected {
        Stroke::new(1.0, p.accent)
    } else {
        Stroke::new(1.0, p.border)
    };
    painter.rect(
        rect,
        CornerRadius::same(R_MD),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    if c.state == CallState::Ended {
        painter.rect_filled(
            rect,
            CornerRadius::same(R_MD),
            p.surface.gamma_multiply(0.4),
        );
    }
    let x = rect.left() + 12.0;
    // Row 1: slot, name, mic marker.
    let is_active = cx.vm.snap.active_line.as_deref() == Some(c.id.as_str());
    let right_mark = if c.muted && c.state != CallState::Ended {
        Some((ic::MICROPHONE_SLASH, p.err.ink))
    } else if is_active {
        Some((ic::MICROPHONE, p.ok.ink))
    } else {
        None
    };
    let name_right = rect.right() - if right_mark.is_some() { 30.0 } else { 10.0 };
    let slot_g = super::truncate_galley(
        ui,
        &format!("L{slot}"),
        FS_SMALL,
        crate::theme::semibold(),
        p.muted,
        40.0,
    );
    let slot_w = slot_g.size().x;
    painter.galley(egui::pos2(x, rect.top() + 9.0), slot_g, p.muted);
    let name_g = super::truncate_galley(
        ui,
        display_name(c),
        FS_BODY,
        crate::theme::semibold(),
        p.text,
        (name_right - x - slot_w - 8.0).max(20.0),
    );
    painter.galley(
        egui::pos2(x + slot_w + 8.0, rect.top() + 7.0),
        name_g,
        p.text,
    );
    if let Some((g, color)) = right_mark {
        painter.text(
            egui::pos2(rect.right() - 18.0, rect.top() + 16.0),
            egui::Align2::CENTER_CENTER,
            g,
            egui::FontId::new(15.0, crate::theme::icons_fill()),
            color,
        );
    }
    // Row 2: state glyph + text. Row 3: account.
    let lit = c.state != CallState::Ended;
    let lamp_tone = if incoming && !cx.blink {
        crate::widgets::dimmed(tone)
    } else {
        tone
    };
    paint_lamp(
        ui,
        egui::pos2(x + 4.0, rect.top() + 33.0),
        lamp_tone,
        lit,
        8.0,
    );
    painter.text(
        egui::pos2(x + 16.0, rect.top() + 33.0),
        egui::Align2::LEFT_CENTER,
        glyph,
        egui::FontId::new(FS_SMALL + 1.0, crate::theme::icons_fill()),
        tone.ink,
    );
    let text_g = super::truncate_galley(
        ui,
        &text,
        FS_SMALL,
        crate::theme::semibold(),
        tone.ink,
        (rect.right() - x - 36.0).max(20.0),
    );
    painter.galley(egui::pos2(x + 34.0, rect.top() + 26.0), text_g, tone.ink);
    let acct = super::truncate_galley(
        ui,
        &c.account,
        FS_MICRO,
        egui::FontFamily::Monospace,
        p.muted,
        (rect.right() - x - 10.0).max(20.0),
    );
    painter.galley(egui::pos2(x + 16.0, rect.top() + 43.0), acct, p.muted);
    let label = access_label(cx, st, c);
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, label.clone())
    });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(access_label(cx, st, c))
        .clicked()
}

/// Narrow: one tab per line plus a "+" tab.
pub(super) fn tabs(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let lines = cx.lines(st);
    egui::ScrollArea::horizontal()
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if lines.is_empty() {
                    ui.label(
                        RichText::new("No lines in use")
                            .size(FS_SMALL)
                            .color(p.muted),
                    );
                }
                for (slot, c) in &lines {
                    let selected = st.selected_line.as_deref() == Some(c.id.as_str());
                    let (glyph, tone, _) = line_summary(cx, st, c);
                    let mut name: String = display_name(c).chars().take(12).collect();
                    if display_name(c).chars().count() > 12 {
                        name.push('\u{2026}');
                    }
                    let b = egui::Button::new((
                        icon(glyph, 14.0).color(tone.ink),
                        RichText::new(format!("L{slot} {name}"))
                            .size(FS_SMALL)
                            .color(p.text),
                    ))
                    .selected(selected)
                    .corner_radius(CornerRadius::same(R_MD));
                    let r = ui.add(b);
                    let label = access_label(cx, st, c);
                    r.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::Button,
                            true,
                            selected,
                            label.clone(),
                        )
                    });
                    if r.on_hover_text(access_label(cx, st, c)).clicked() {
                        click_line(cx, st, c, out);
                    }
                }
                let plus = ui
                    .add(
                        egui::Button::new(RichText::new("+").size(FS_BODY))
                            .min_size(egui::vec2(28.0, 0.0)),
                    )
                    .on_hover_text("New call");
                plus.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "New call")
                });
                if plus.clicked() {
                    open_new_call(cx, st);
                }
            });
        });
}
