//! Transfer UI: the composer (target field with BLF suggestions; Enter
//! starts a consult or a blind transfer per `[ui] transfer_enter`) and the session panel that
//! shows the engine's [`TransferSession`] state with Complete / Swap / Cancel and the failure
//! and unconfirmed messaging.

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use siple_core::config::TransferEnter;
use siple_sip::{
    BlfStatus, CallInfo, CallState, CompleteMode, TransferKind, TransferPhase, TransferSession,
};

use super::blf_panel::paint_state_glyph;
use super::rail::line_summary;
use super::{Composer, Cx, UiAction, UiState, display_name};
use crate::snapshot::{blf_state_text, blf_title, format_duration, sip_detail, transfer_failure};
use crate::theme::{FS_BODY, FS_MICRO, FS_MONO, FS_SMALL, Palette, R_MD, SP_S, Tone, icon as ic};
use crate::widgets::{icon, mono, raised_card, soft_button, solid_button, strong};

/// Suggestions shown under the target field.
const MAX_SUGGESTIONS: usize = 6;

fn modifier_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "Cmd"
    } else {
        "Ctrl"
    }
}

/// `sip:<target>@<domain of the account>` for a bare extension; a full URI as typed.
pub(super) fn resolved_uri(cx: &Cx, account: &str, target: &str) -> String {
    let t = target.trim();
    if t.contains('@') || t.to_ascii_lowercase().starts_with("sip") {
        return t.to_string();
    }
    let domain = cx
        .vm
        .snap
        .accounts
        .iter()
        .find(|a| a.id == account)
        .and_then(|a| a.aor.rsplit_once('@').map(|(_, d)| d.to_string()))
        .unwrap_or_default();
    format!("sip:{t}@{domain}")
}

fn accent_button(
    ui: &mut egui::Ui,
    p: &Palette,
    label: &str,
    enabled: bool,
    w: f32,
) -> egui::Response {
    let b = egui::Button::new(strong(label, FS_BODY).color(p.on_accent))
        .fill(p.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(R_MD))
        .min_size(egui::vec2(w, 36.0));
    ui.add_enabled(enabled, b)
}

// ---- composer ---------------------------------------------------------------------------------

/// Which suggestions match what is typed (BLF entries first, same account first).
fn suggestions<'a>(cx: &Cx<'a>, account: &str, typed: &str) -> Vec<&'a BlfStatus> {
    let q = typed.trim().to_lowercase();
    let mut v: Vec<&BlfStatus> = cx
        .vm
        .snap
        .blf
        .iter()
        .filter(|b| {
            // A bare extension resolves on the call's account, so only its own keys.
            b.account == account
                && (q.is_empty()
                    || b.uri.to_lowercase().contains(&q)
                    || b.label.to_lowercase().contains(&q))
        })
        .collect();
    v.truncate(MAX_SUGGESTIONS);
    v
}

/// Frame of the composer / session panel: inset inside the call card it acts on (`nested`), a
/// raised card of its own otherwise (the idle stage, after the lines are gone).
fn pane(p: &Palette, nested: bool, border: Stroke) -> egui::Frame {
    if nested {
        egui::Frame::new()
            .fill(p.inset)
            .stroke(border)
            .corner_radius(CornerRadius::same(R_MD))
            .inner_margin(Margin::same(10))
    } else {
        raised_card(p, border)
    }
}

pub(super) fn composer(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    c: &CallInfo,
    nested: bool,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    let Some(mut comp) = st.composer.take() else {
        return;
    };
    let mut close = false;
    let others: Vec<&CallInfo> = cx
        .calls
        .iter()
        .copied()
        .filter(|o| {
            o.id != c.id
                && o.account == c.account
                && matches!(
                    o.state,
                    CallState::Active | CallState::Held | CallState::RemoteHeld
                )
                && !cx.vm.snap.transfers.iter().any(|s| {
                    !s.phase.is_terminal()
                        && (s.original == o.id || s.consult.as_deref() == Some(o.id.as_str()))
                })
        })
        .collect();
    pane(p, nested, Stroke::new(1.0, p.accent)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = SP_S;
        ui.horizontal(|ui| {
            ui.label(strong(format!("Transfer {}", display_name(c)), FS_BODY).color(p.text));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("via {}", c.account))
                        .size(FS_SMALL)
                        .color(p.muted),
                );
            });
        });
        let id = ui.id().with("transfer_target");
        let edit = ui.add(
            egui::TextEdit::singleline(&mut comp.target)
                .id(id)
                .hint_text("Number or SIP URI")
                .font(egui::FontId::new(
                    FS_MONO + 1.0,
                    egui::FontFamily::Monospace,
                ))
                .margin(Margin::symmetric(10, 7))
                .desired_width(ui.available_width()),
        );
        // Read before re-focusing: requesting focus again would hide the Enter that left it.
        let enter_pressed = edit.lost_focus();
        if ui.memory(|m| m.focused().is_none()) && !enter_pressed {
            edit.request_focus();
        }
        // BLF entries first, each with its live lamp so a busy target is visible.
        let sugg = suggestions(cx, &c.account, &comp.target);
        if !sugg.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for b in sugg {
                    if suggestion(ui, cx, b).clicked() {
                        comp.target = b.uri.clone();
                    }
                }
            });
        }
        if !others.is_empty() {
            ui.label(
                RichText::new("Or connect with a call already on the line:")
                    .size(FS_SMALL)
                    .color(p.muted),
            );
            ui.horizontal_wrapped(|ui| {
                for o in &others {
                    let l = format!("{} ({})", display_name(o), cx.slot(st, &o.id));
                    if soft_button(ui, p, ic::PHONE_TRANSFER, &l, None, true, 0.0).clicked() {
                        out.push(UiAction::ConsultLink(c.id.clone(), o.id.clone()));
                        close = true;
                    }
                }
            });
        }
        let target = comp.target.trim().to_string();
        let can = !target.is_empty();
        if can {
            // The arrow is its own label so screen readers hear the address alone.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(icon(ic::ARROW_RIGHT, FS_SMALL).color(p.muted));
                ui.add(
                    egui::Label::new(
                        mono(resolved_uri(cx, &c.account, &target), FS_SMALL).color(p.muted),
                    )
                    .truncate(),
                );
            });
        }
        let consult_default = cx.vm.snap.ui.transfer_enter == TransferEnter::Consult;
        let m = modifier_name();
        let (consult_hint, blind_hint) = if consult_default {
            ("Enter".to_string(), format!("{m}+Enter"))
        } else {
            (format!("{m}+Enter"), "Enter".to_string())
        };
        let mut chosen: Option<bool> = None; // Some(true) = consult
        ui.horizontal_wrapped(|ui| {
            if accent_button(ui, p, &format!("Consult first ({consult_hint})"), can, 0.0).clicked()
            {
                chosen = Some(true);
            }
            if soft_button(
                ui,
                p,
                ic::ARROW_BEND_UP_RIGHT,
                &format!("Transfer now ({blind_hint})"),
                None,
                can,
                0.0,
            )
            .clicked()
            {
                chosen = Some(false);
            }
            if soft_button(ui, p, ic::X, "Close", None, true, 0.0).clicked() {
                close = true;
            }
        });
        // A bare Enter makes the field lose focus; Mod+Enter keeps it (egui only treats an
        // unmodified Enter as the return key).
        let (enter, cmd) = ui.input(|i| (i.key_pressed(egui::Key::Enter), i.modifiers.command));
        if can && enter && (enter_pressed || (cmd && edit.has_focus())) {
            chosen = Some(consult_default != cmd);
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            close = true;
        }
        if let Some(consult) = chosen {
            out.push(if consult {
                UiAction::ConsultStart(c.id.clone(), target)
            } else {
                UiAction::TransferBlind(c.id.clone(), target)
            });
            close = true;
        }
    });
    if !close {
        st.composer = Some(Composer {
            call_id: comp.call_id,
            target: comp.target,
        });
    }
}

/// One BLF suggestion: state glyph, extension and name, state text.
fn suggestion(ui: &mut egui::Ui, cx: &Cx, b: &BlfStatus) -> egui::Response {
    let p = cx.p;
    let (tone, _) = super::blf_panel::blf_tone(p, b.state);
    let title = if blf_title(b) == b.uri {
        b.uri.clone()
    } else {
        format!("{} {}", b.uri, blf_title(b))
    };
    let state = blf_state_text(b);
    let galley = ui.painter().layout_no_wrap(
        title,
        egui::FontId::new(FS_SMALL, crate::theme::semibold()),
        p.text,
    );
    let state_galley = ui.painter().layout_no_wrap(
        state.to_lowercase(),
        egui::FontId::new(FS_SMALL, egui::FontFamily::Proportional),
        p.muted,
    );
    let gap = 6.0;
    let size = egui::vec2(galley.size().x + gap + state_galley.size().x + 36.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    ui.painter().rect(
        rect,
        CornerRadius::same(R_MD),
        if resp.hovered() { p.inset } else { p.surface },
        Stroke::new(1.0, p.border_strong),
        egui::StrokeKind::Inside,
    );
    paint_state_glyph(
        ui,
        egui::pos2(rect.left() + 14.0, rect.center().y),
        b.state,
        tone,
        9.0,
        true,
    );
    let title_w = galley.size().x;
    ui.painter().galley(
        egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
        galley,
        p.text,
    );
    ui.painter().galley(
        egui::pos2(
            rect.left() + 26.0 + title_w + gap,
            rect.center().y - state_galley.size().y / 2.0,
        ),
        state_galley,
        p.muted,
    );
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("Transfer to {} ({}), {}", blf_title(b), b.uri, state),
        )
    });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---- session panel ----------------------------------------------------------------------------

fn leg_name(cx: &Cx, id: Option<&str>, fallback: &str) -> String {
    id.and_then(|i| cx.call(i))
        .map_or_else(|| fallback.to_string(), |c| display_name(c).to_string())
}

/// The plain-language reason of a failed session (`Busy`); the raw text when the session has no
/// named problem, else "no reason given".
fn failure_text(s: &TransferSession) -> String {
    if let Some(f) = transfer_failure(s) {
        f.title
    } else if let Some(e) = &s.last_error {
        sip_detail(e.code, &e.reason)
    } else {
        "no reason given".to_string()
    }
}

/// Hover text with the server's raw answer behind a failure note.
fn failure_details(s: &TransferSession) -> Option<String> {
    transfer_failure(s)
        .map(|f| f.detail)
        .filter(|d| !d.is_empty())
        .map(|d| format!("Details: {d}"))
}

/// One leg of the pair: name, state text, "Talking" marker.
fn leg(ui: &mut egui::Ui, cx: &Cx, st: &UiState, id: Option<&str>, name: &str, talking: bool) {
    let p = cx.p;
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let slot = id.map(|i| cx.slot(st, i)).filter(|s| !s.is_empty());
        let title = match slot {
            Some(s) => format!("{name} ({s})"),
            None => name.to_string(),
        };
        ui.add(egui::Label::new(strong(title, FS_BODY).color(p.text)).truncate());
        if let Some(c) = id.and_then(|i| cx.call(i)) {
            let (_, tone, text) = line_summary(cx, st, c);
            ui.horizontal(|ui| {
                ui.label(RichText::new(text).size(FS_SMALL).color(tone.ink));
                if talking {
                    ui.label(icon(ic::MICROPHONE, FS_SMALL + 1.0).color(p.ok.ink))
                        .on_hover_text("You are talking to this call");
                    ui.label(strong("Talking", FS_MICRO).color(p.ok.ink));
                }
            });
        }
    });
}

pub(super) fn session_panel(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    s: &TransferSession,
    nested: bool,
    out: &mut Vec<UiAction>,
) {
    use TransferPhase::*;
    let p = cx.p;
    let a_name = leg_name(cx, Some(&s.original), "the call");
    let b_name = leg_name(cx, s.consult.as_deref(), &s.target_uri);
    let (tone, title): (Tone, &str) = match s.phase {
        Transferred => (p.ok, "Transferred"),
        Failed | ConsultFailed => (p.err, "Transfer failed"),
        Unconfirmed => (p.warn, "Transfer unconfirmed"),
        Cancelled | Abandoned => (p.idle, "Transfer ended"),
        Referring | Transferring => (p.warn, "Transferring"),
        _ => (p.held, "Transfer"),
    };
    pane(p, nested, Stroke::new(1.5, tone.led)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing = egui::vec2(8.0, SP_S);
        ui.horizontal(|ui| {
            ui.label(icon(ic::PHONE_TRANSFER, 17.0).color(tone.ink));
            ui.label(strong(title, FS_BODY).color(tone.ink));
            ui.label(
                RichText::new(format!("via {}", s.account))
                    .size(FS_SMALL)
                    .color(p.muted),
            );
        });
        let consult_live = matches!(s.phase, ConsultDialing | ConsultRinging | ConsultConnected);
        if consult_live || s.phase.refer_outstanding() || s.phase == Unconfirmed {
            let b_id = s.consult.as_deref();
            let talking_b = s.talking_to.is_some() && s.talking_to.as_deref() == b_id;
            ui.horizontal_top(|ui| {
                let w = (ui.available_width() - 36.0) / 2.0;
                ui.allocate_ui(egui::vec2(w, 0.0), |ui| {
                    leg(ui, cx, st, Some(&s.original), &a_name, consult_live && s.talking_to.as_deref() == Some(s.original.as_str()))
                });
                ui.label(icon(ic::ARROW_BEND_UP_RIGHT, 18.0).color(p.muted));
                ui.allocate_ui(egui::vec2(w, 0.0), |ui| {
                    leg(ui, cx, st, b_id, &b_name, consult_live && talking_b)
                });
            });
        }
        let note = |ui: &mut egui::Ui, t: String, color: Color32| {
            ui.add(egui::Label::new(RichText::new(t).size(FS_SMALL).color(color)).wrap())
        };
        // A failure note: the plain-language text, the server's raw answer on hover.
        let failure_note = |ui: &mut egui::Ui, t: String| {
            let r = note(ui, t, p.err.ink);
            match failure_details(s) {
                Some(d) => r.on_hover_text(d),
                None => r,
            }
        };
        let sid = s.id.clone();
        match s.phase {
            ConsultDialing | ConsultRinging => {
                let t = if s.phase == ConsultDialing {
                    format!("Calling {b_name}\u{2026}")
                } else {
                    format!(
                        "{b_name} hasn't answered. \"Transfer now\" sends {a_name} to {b_name}'s line and you leave."
                    )
                };
                note(ui, t, p.text);
                ui.horizontal_wrapped(|ui| {
                    if solid_button(ui, ic::ARROW_BEND_UP_RIGHT, "Transfer now (blind)", p.accent, 0.0)
                        .clicked()
                    {
                        out.push(UiAction::TransferComplete(sid.clone(), CompleteMode::BlindNow));
                    }
                    if soft_button(
                        ui,
                        p,
                        ic::PHONE_X,
                        &format!("Cancel - return to {a_name}"),
                        None,
                        true,
                        0.0,
                    )
                    .clicked()
                    {
                        out.push(UiAction::TransferCancel(sid.clone()));
                    }
                });
                note(
                    ui,
                    format!("Complete transfer becomes available when {b_name} answers."),
                    p.muted,
                );
            }
            ConsultConnected => {
                // `talking_to = None`: both legs are held (consult_link); nobody is talking.
                let nobody = s.talking_to.is_none();
                let talking_b = s.talking_to.as_deref() == s.consult.as_deref() && !nobody;
                if s.last_error.is_some() {
                    let who = if nobody {
                        "Both calls are on hold".to_string()
                    } else if talking_b {
                        format!("You're still talking to {b_name}. {a_name} is on hold")
                    } else {
                        format!("You're talking to {a_name}. {b_name} is on hold")
                    };
                    failure_note(
                        ui,
                        format!("Transfer failed: {}. {who}.", failure_text(s)),
                    );
                }
                if accent_button(
                    ui,
                    p,
                    &format!("Complete transfer: {a_name} \u{2192} {b_name}"),
                    true,
                    ui.available_width(),
                )
                .clicked()
                {
                    out.push(UiAction::TransferComplete(sid.clone(), CompleteMode::Attended));
                }
                ui.horizontal_wrapped(|ui| {
                    if nobody {
                        if soft_button(ui, p, ic::PLAY, &format!("Resume {a_name}"), None, true, 0.0)
                            .clicked()
                        {
                            out.push(UiAction::Activate(s.original.clone()));
                        }
                        if let Some(b) = &s.consult
                            && soft_button(ui, p, ic::PLAY, &format!("Talk to {b_name}"), None, true, 0.0)
                                .clicked()
                        {
                            out.push(UiAction::Activate(b.clone()));
                        }
                    } else {
                        let other = if talking_b { &a_name } else { &b_name };
                        if soft_button(
                            ui,
                            p,
                            ic::PHONE_TRANSFER,
                            &format!("Swap to {other}"),
                            None,
                            true,
                            0.0,
                        )
                        .clicked()
                        {
                            out.push(UiAction::TransferSwap(sid.clone()));
                        }
                    }
                    let cancel = if talking_b || nobody {
                        format!("Cancel - return to {a_name}")
                    } else {
                        format!("Cancel - drop {b_name}")
                    };
                    if soft_button(ui, p, ic::PHONE_X, &cancel, None, true, 0.0).clicked() {
                        out.push(UiAction::TransferCancel(sid.clone()));
                    }
                });
            }
            ConsultFailed => {
                failure_note(
                    ui,
                    format!(
                        "The call to {b_name} failed ({}). {a_name} is still on hold.",
                        failure_text(s)
                    ),
                );
                ui.horizontal_wrapped(|ui| {
                    if soft_button(
                        ui,
                        p,
                        ic::PHONE_X,
                        &format!("Cancel - return to {a_name}"),
                        None,
                        true,
                        0.0,
                    )
                    .clicked()
                    {
                        out.push(UiAction::TransferCancel(sid.clone()));
                    }
                });
            }
            Referring => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    note(ui, "Sending the transfer request\u{2026}".into(), p.text);
                });
                note(
                    ui,
                    "Swap, Complete and Cancel are unavailable while the transfer is in progress."
                        .into(),
                    p.muted,
                );
            }
            Transferring => {
                let status = s.last_notify.as_ref().map_or_else(
                    || "waiting for the PBX".to_string(),
                    |n| {
                        format!(
                            "PBX: {} {} ({})",
                            n.code,
                            n.reason,
                            format_duration((cx.vm.now - n.at).num_seconds())
                        )
                    },
                );
                ui.horizontal(|ui| {
                    ui.spinner();
                    note(ui, format!("Transferring\u{2026} {status}"), p.text);
                });
                if soft_button(ui, p, ic::PHONE_X, "Leave now", None, true, 0.0)
                    .on_hover_text("You leave; the PBX finishes the transfer and the result is not shown")
                    .clicked()
                {
                    // Hang up the leg the REFER went out on: with refer-on-consult the target replaces
                    // the original's dialog, which must stay alive.
                    let leaver = s.refer_call.clone().unwrap_or_else(|| s.original.clone());
                    out.push(UiAction::Hangup(leaver));
                }
            }
            Unconfirmed => {
                note(
                    ui,
                    format!(
                        "No confirmation from the PBX after 32 s. {a_name} may already be with {b_name}. {a_name} is on hold here."
                    ),
                    p.warn.ink,
                );
                ui.horizontal_wrapped(|ui| {
                    if soft_button(ui, p, ic::PLAY, &format!("Resume {a_name}"), None, true, 0.0)
                        .clicked()
                    {
                        out.push(UiAction::Activate(s.original.clone()));
                    }
                    if soft_button(ui, p, ic::PHONE_X, &format!("Hang up {a_name}"), None, true, 0.0)
                        .clicked()
                    {
                        out.push(UiAction::Hangup(s.original.clone()));
                    }
                });
            }
            Transferred => {
                let at = s
                    .ended_at
                    .map(|t| {
                        t.with_timezone(&chrono::Local)
                            .format(" at %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default();
                note(
                    ui,
                    format!("Transferred {a_name} to {b_name}{at}. The lines are closed."),
                    p.ok.ink,
                );
                dismiss(ui, p, st, s);
            }
            Failed => {
                let target = if s.kind == TransferKind::Blind {
                    s.target_uri.clone()
                } else {
                    b_name.clone()
                };
                let resumed = if s.we_held_original {
                    format!(" {a_name} is back on the line (resumed).")
                } else {
                    String::new()
                };
                failure_note(
                    ui,
                    format!("Transfer to {target} failed: {}.{resumed}", failure_text(s)),
                );
                ui.horizontal_wrapped(|ui| {
                    let live = cx.call(&s.original).is_some_and(|c| c.state != CallState::Ended);
                    if soft_button(
                        ui,
                        p,
                        ic::ARROW_BEND_UP_RIGHT,
                        "Transfer\u{2026}",
                        None,
                        live,
                        0.0,
                    )
                    .clicked()
                    {
                        st.composer = Some(Composer {
                            call_id: s.original.clone(),
                            target: String::new(),
                        });
                        st.selected_line = Some(s.original.clone());
                        st.dismissed_sessions.insert(s.id.clone());
                    }
                    if soft_button(ui, p, ic::PHONE_X, "Dismiss", None, true, 0.0).clicked() {
                        st.dismissed_sessions.insert(s.id.clone());
                    }
                });
            }
            Cancelled => {
                note(ui, format!("Transfer canceled. {a_name} is back on the line."), p.muted);
                dismiss(ui, p, st, s);
            }
            Abandoned => {
                note(ui, "Transfer abandoned: the call ended before there was an outcome.".into(), p.muted);
                dismiss(ui, p, st, s);
            }
        }
    });
}

fn dismiss(ui: &mut egui::Ui, p: &Palette, st: &mut UiState, s: &TransferSession) {
    if soft_button(ui, p, ic::PHONE_X, "Dismiss", None, true, 0.0).clicked() {
        st.dismissed_sessions.insert(s.id.clone());
    }
}
