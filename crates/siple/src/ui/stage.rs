//! Stage: the selected line (name, URI, account, state, media chips, Mute / Hold / Keypad /
//! Transfer, Hang up), the call-waiting banners above it, the dial bar ("+ New call") and the
//! idle stage (dial field and keypad).

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke};
use siple_sip::{CallDirection, CallInfo, CallState, EndCause, TransferPhase, TransferSession};

use super::transfer;
use super::{
    Composer, Cx, UiAction, UiState, combo_icon, display_name, header, session_for, state_tone,
    visible_session,
};
use crate::snapshot::{
    RegClass, call_elapsed_secs, call_state_label, format_duration, reg_class, reg_label,
};
use crate::theme::{
    self, FS_BODY, FS_DISPLAY, FS_ICON, FS_MICRO, FS_MONO, FS_SMALL, FS_TIMER, Palette, R_LG, R_MD,
    SP_S, SP_XS, icon as ic,
};
use crate::widgets::{
    chip, icon, icon_fill, keypad_key, mono, panel, pill, raised_card, soft_button, solid_button,
    strong,
};

/// Banners shown at once; the rest collapse into "+N more".
const MAX_BANNERS: usize = 3;

/// Config error and call-waiting banners: pinned above the stage and above the extensions tab.
pub(super) fn pinned(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    if let Some(err) = &cx.vm.snap.config_error {
        header::config_banner(ui, cx.p, err, out);
    }
    let selected = cx.selected(st);
    banners(ui, cx, st, selected, out);
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    pinned(ui, cx, st, out);
    let selected = cx.selected(st);
    let Some(c) = selected else {
        idle(ui, cx, st, out);
        return;
    };
    let composing = st.composer.as_ref().is_some_and(|k| k.call_id == c.id);
    if composing {
        transfer::composer(ui, cx, st, c, out);
    }
    if let Some(s) = session_for(cx, st, &c.id) {
        transfer::session_panel(ui, cx, st, s, out);
    }
    line_card(ui, cx, st, c, out);
    if st.new_call_open {
        dial_bar(ui, cx, st, out);
    } else if soft_button(ui, p, ic::PHONE_OUTGOING, "+ New call", None, true, 0.0).clicked() {
        super::rail::open_new_call(cx, st);
    }
}

// ---- decisions shared by the buttons and the keyboard shortcuts ------------------------------

/// A line the user can act on: established (talking, held here or held by the far end).
fn is_live(c: &CallInfo) -> bool {
    matches!(
        c.state,
        CallState::Active | CallState::Held | CallState::RemoteHeld
    )
}

/// The ringing call a plain "answer" targets: the selected one, else the newest waiting call
/// (the top banner).
pub(super) fn answer_target<'a>(cx: &Cx<'a>, st: &UiState) -> Option<&'a CallInfo> {
    cx.selected(st).filter(|c| is_waiting(c)).or_else(|| {
        cx.calls
            .iter()
            .copied()
            .filter(|c| is_waiting(c))
            .max_by_key(|c| c.created_at)
    })
}

/// Answering `c`: with another line in progress, hold that line (the safe call-waiting answer;
/// "Answer & end" stays a deliberate button); otherwise a plain answer.
pub(super) fn answer_action(c: &CallInfo, current: Option<&CallInfo>) -> UiAction {
    match current.filter(|cur| cur.id != c.id) {
        Some(_) => UiAction::AnswerHold(c.id.clone()),
        None => UiAction::Answer(c.id.clone()),
    }
}

/// Declining a ringing call (`[calls] decline_code` applies when another call is live).
pub(super) fn decline_action(c: &CallInfo) -> UiAction {
    UiAction::Reject(c.id.clone())
}

/// The live transfer session when `c` is its dialing / connected consult leg: ending that leg
/// is "Cancel - return to <original>".
pub(super) fn consult_leg<'a>(
    session: Option<&'a TransferSession>,
    c: &CallInfo,
) -> Option<&'a TransferSession> {
    session.filter(|s| {
        s.consult.as_deref() == Some(c.id.as_str())
            && s.phase.is_consult()
            && s.phase != TransferPhase::ConsultFailed
    })
}

/// Hang up / Cancel / Decline of a line: reject when ringing in, cancel the session on a consult
/// leg, else hang up. `None` for an ended line.
pub(super) fn end_action(session: Option<&TransferSession>, c: &CallInfo) -> Option<UiAction> {
    if c.state == CallState::Ended {
        None
    } else if is_waiting(c) {
        Some(decline_action(c))
    } else if let Some(s) = consult_leg(session, c) {
        Some(UiAction::TransferCancel(s.id.clone()))
    } else {
        Some(UiAction::Hangup(c.id.clone()))
    }
}

/// Mute toggle; `None` while the line cannot be muted (only a talking line, or to unmute).
pub(super) fn mute_action(c: &CallInfo) -> Option<UiAction> {
    (is_live(c) && (c.state == CallState::Active || c.muted))
        .then(|| UiAction::Mute(c.id.clone(), !c.muted))
}

/// Hold / Resume; `None` while a transfer request is outstanding or an operation is pending.
/// Resuming goes through the line coordinator (never two live lines) or the session swap.
pub(super) fn hold_action(session: Option<&TransferSession>, c: &CallInfo) -> Option<UiAction> {
    let blocked = session.is_some_and(|s| s.phase.refer_outstanding());
    (is_live(c) && !blocked && c.pending.is_none()).then(|| {
        if c.hold.local {
            resume_action(session, c)
        } else {
            UiAction::Hold(c.id.clone(), true)
        }
    })
}

/// Whether the transfer composer can open for `c`.
pub(super) fn can_transfer(session: Option<&TransferSession>, c: &CallInfo) -> bool {
    is_live(c) && c.pending.is_none() && session.is_none_or(|s| s.phase.is_terminal())
}

/// Opens the transfer composer for `c` (the caller checked [`can_transfer`]).
pub(super) fn open_composer(st: &mut UiState, c: &CallInfo) {
    st.composer = Some(Composer {
        call_id: c.id.clone(),
        target: String::new(),
    });
}

// ---- call waiting ---------------------------------------------------------------------------

/// The call a waiting call would hold or end: the active line, else an established selection.
pub(super) fn current_line<'a>(
    cx: &Cx<'a>,
    selected: Option<&'a CallInfo>,
) -> Option<&'a CallInfo> {
    cx.active_call()
        .or(selected.filter(|c| {
            matches!(
                c.state,
                CallState::Active | CallState::Held | CallState::RemoteHeld
            )
        }))
        .or_else(|| {
            cx.calls.iter().copied().find(|c| {
                matches!(
                    c.state,
                    CallState::Active | CallState::Held | CallState::RemoteHeld
                )
            })
        })
}

pub(super) fn is_waiting(c: &CallInfo) -> bool {
    c.direction == CallDirection::Inbound && c.state == CallState::Ringing
}

/// Incoming calls that do not have the stage: pinned banners, newest first.
fn banners(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    selected: Option<&CallInfo>,
    out: &mut Vec<UiAction>,
) {
    let mut waiting: Vec<&CallInfo> = cx
        .calls
        .iter()
        .copied()
        .filter(|c| is_waiting(c) && selected.is_none_or(|s| s.id != c.id))
        .collect();
    waiting.sort_by_key(|c| std::cmp::Reverse(c.created_at));
    let current = current_line(cx, selected);
    for c in waiting.iter().take(MAX_BANNERS) {
        banner(ui, cx, st, c, current, out);
    }
    if waiting.len() > MAX_BANNERS {
        ui.label(
            RichText::new(format!("+{} more incoming", waiting.len() - MAX_BANNERS))
                .size(FS_SMALL)
                .color(cx.p.ring.ink),
        );
    }
}

fn banner(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    c: &CallInfo,
    current: Option<&CallInfo>,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    let border = Stroke::new(if cx.blink { 2.0 } else { 1.0 }, p.ring.led);
    egui::Frame::new()
        .fill(p.ring.soft)
        .stroke(border)
        .corner_radius(CornerRadius::same(R_LG))
        .inner_margin(Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = SP_S;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(icon_fill(ic::PHONE_INCOMING, 17.0).color(p.ring.ink));
                ui.label(strong(format!("Incoming on {}", c.account), FS_SMALL).color(p.ring.ink));
                if st.silenced.contains(&c.id) {
                    ui.label(RichText::new("Silenced").size(FS_SMALL).color(p.muted));
                }
            });
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(strong(display_name(c), 17.0).color(p.text)).truncate());
            });
            if display_name(c) != c.remote_uri {
                ui.add(egui::Label::new(mono(&c.remote_uri, FS_MONO).color(p.muted)).truncate());
            }
            incoming_buttons(ui, cx, st, c, current, out);
        });
}

/// Answer (& hold / & end the current line), Decline, Silence.
fn incoming_buttons(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    c: &CallInfo,
    current: Option<&CallInfo>,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    let current = current.filter(|cur| cur.id != c.id);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        match current {
            Some(cur) => {
                let short: String = display_name(cur).chars().take(18).collect();
                if solid_button(
                    ui,
                    ic::PHONE,
                    &format!("Answer & hold {short}"),
                    p.answer,
                    0.0,
                )
                .clicked()
                {
                    out.push(answer_action(c, current));
                }
                if solid_button(
                    ui,
                    ic::PHONE,
                    &format!("Answer & end {short}"),
                    p.answer,
                    0.0,
                )
                .clicked()
                {
                    out.push(UiAction::AnswerEnd(c.id.clone(), cur.id.clone()));
                }
            }
            None => {
                if solid_button(ui, ic::PHONE, "Answer", p.answer, 0.0).clicked() {
                    out.push(answer_action(c, None));
                }
            }
        }
        let decline = if current.is_some() {
            "Decline"
        } else {
            "Reject"
        };
        if solid_button(ui, ic::PHONE_X, decline, p.hangup, 0.0).clicked() {
            out.push(decline_action(c));
        }
        let silenced = st.silenced.contains(&c.id);
        if soft_button(ui, p, ic::BELL_SLASH, "Silence", None, !silenced, 0.0).clicked() {
            st.silenced.insert(c.id.clone());
            out.push(UiAction::Silence(c.id.clone()));
        }
    });
}

// ---- the selected line ----------------------------------------------------------------------

fn line_card(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, c: &CallInfo, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let now = cx.vm.now;
    let tone = state_tone(p, c.state);
    let incoming_ring = is_waiting(c);
    let ringing = matches!(c.state, CallState::Ringing | CallState::Early);
    let border = if incoming_ring {
        Stroke::new(1.5, p.ring.led)
    } else {
        Stroke::new(1.0, p.border)
    };
    let ended = c.state == CallState::Ended;
    let session = session_for(cx, st, &c.id);
    raised_card(p, border).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if ended {
            ui.set_opacity(0.6);
        }
        ui.spacing_mut().item_spacing.y = SP_XS;
        // Row 1: direction + account, state pill.
        ui.horizontal(|ui| {
            let (glyph, dir) = match c.direction {
                CallDirection::Inbound => (ic::PHONE_INCOMING, "Incoming"),
                CallDirection::Outbound => (ic::PHONE_OUTGOING, "Outgoing"),
            };
            ui.spacing_mut().item_spacing.x = 5.0;
            ui.label(icon(glyph, FS_SMALL + 1.0).color(p.muted));
            ui.label(
                RichText::new(format!("{dir} on {}", c.account))
                    .size(FS_SMALL)
                    .color(p.muted),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let t = if ringing && !cx.blink {
                    crate::widgets::dimmed(tone)
                } else {
                    tone
                };
                let label = if c.reconnecting {
                    "Reconnecting call\u{2026}"
                } else {
                    call_state_label(c.state)
                };
                pill(ui, t, label, true);
            });
        });
        // Row 2: who + timer.
        let who = display_name(c);
        ui.horizontal(|ui| {
            let timer = format_duration(call_elapsed_secs(c, now));
            let timer_w = 70.0;
            let name_w = (ui.available_width() - timer_w).max(60.0);
            ui.allocate_ui_with_layout(
                egui::vec2(name_w, 30.0),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.add(egui::Label::new(strong(who, FS_DISPLAY).color(p.text)).truncate());
                },
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let color = if c.state == CallState::Active {
                    p.text
                } else {
                    p.muted
                };
                ui.label(
                    RichText::new(timer)
                        .family(theme::mono_medium())
                        .size(FS_TIMER)
                        .color(color),
                );
            });
        });
        if who != c.remote_uri {
            ui.add(egui::Label::new(mono(&c.remote_uri, FS_MONO).color(p.muted)).truncate());
        }
        media_chips(ui, cx, c, ended);
        if ended {
            ui.add_space(SP_S);
            ui.horizontal(|ui| {
                if soft_button(ui, p, ic::PHONE_OUTGOING, "Call back", None, true, 0.0).clicked() {
                    out.push(UiAction::Dial(c.account.clone(), c.remote_uri.clone()));
                }
                if soft_button(ui, p, ic::PHONE_X, "Dismiss", None, true, 0.0).clicked() {
                    st.dismissed_calls.insert(c.id.clone());
                }
            });
            return;
        }
        ui.add_space(SP_S + 2.0);
        let full = ui.available_width();
        let gap = 6.0;
        ui.spacing_mut().item_spacing = egui::vec2(gap, gap);
        if incoming_ring {
            let current = current_line(cx, Some(c));
            incoming_buttons(ui, cx, st, c, current, out);
            return;
        }
        if c.direction == CallDirection::Outbound
            && matches!(c.state, CallState::Dialing | CallState::Early)
        {
            // The consult leg of a session: same action and wording as the session panel.
            let label = match consult_leg(session, c) {
                Some(s) => format!("Cancel - return to {}", original_name(cx, s)),
                None => "Cancel".to_string(),
            };
            if solid_button(ui, ic::PHONE_X, &label, p.hangup, full).clicked()
                && let Some(action) = end_action(session, c)
            {
                out.push(action);
            }
            return;
        }
        controls(ui, cx, st, c, session, full, out);
    });
}

fn media_chips(ui: &mut egui::Ui, cx: &Cx, c: &CallInfo, ended: bool) {
    let p = cx.p;
    let has_chips = c.negotiated.is_some() || (c.muted && !ended) || (ended && c.end.is_some());
    if !has_chips {
        return;
    }
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        if let Some(n) = &c.negotiated {
            chip(
                ui,
                p,
                Some(ic::SPEAKER_HIGH),
                mono(n.codec.to_uppercase(), FS_MICRO + 0.5),
                None,
            );
            match &n.srtp_suite {
                Some(s) => {
                    chip(
                        ui,
                        p,
                        Some(ic::LOCK_SIMPLE),
                        mono(format!("SRTP {s}"), FS_MICRO + 0.5),
                        Some(p.ok),
                    )
                    .on_hover_text("Media is encrypted (SDES-SRTP)");
                }
                None => {
                    chip(
                        ui,
                        p,
                        Some(ic::LOCK_SIMPLE_OPEN),
                        mono("RTP", FS_MICRO + 0.5),
                        None,
                    )
                    .on_hover_text("Media is not encrypted");
                }
            }
        }
        if c.muted && !ended {
            chip(
                ui,
                p,
                Some(ic::MICROPHONE_SLASH),
                strong("Muted", FS_MICRO + 0.5),
                Some(p.err),
            );
        }
        if ended && let Some(end) = &c.end {
            if end.cause == EndCause::Transferred {
                chip(ui, p, None, strong("Transferred", FS_MICRO + 0.5), None);
            } else {
                chip(
                    ui,
                    p,
                    None,
                    mono(format!("{} {}", end.code, end.reason), FS_MICRO + 0.5),
                    None,
                );
            }
        }
    });
}

/// Taking a held line back: `call.activate`, except on a connected consult where the other leg
/// is talking (then the session swap does it).
pub(super) fn resume_action(session: Option<&TransferSession>, c: &CallInfo) -> UiAction {
    match session {
        Some(s)
            if s.phase == TransferPhase::ConsultConnected
                && s.talking_to.as_deref().is_some_and(|t| t != c.id) =>
        {
            UiAction::TransferSwap(s.id.clone())
        }
        _ => UiAction::Activate(c.id.clone()),
    }
}

fn original_name(cx: &Cx, s: &TransferSession) -> String {
    cx.call(&s.original)
        .map_or_else(|| "original".to_string(), |a| display_name(a).to_string())
}

/// Mute / Hold / Keypad / Transfer and Hang up of an established line.
fn controls(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    c: &CallInfo,
    session: Option<&TransferSession>,
    full: f32,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    let gap = 6.0;
    // Four keys in a row; two by two when the stage is too narrow for their labels.
    let key_w = if full < 460.0 {
        ((full - gap) / 2.0).floor() - 1.0
    } else {
        ((full - 3.0 * gap) / 4.0).floor() - 1.0
    };
    ui.horizontal_wrapped(|ui| {
        let (glyph, label) = if c.muted {
            (ic::MICROPHONE_SLASH, "Unmute")
        } else {
            (ic::MICROPHONE, "Mute")
        };
        let mute = mute_action(c);
        if soft_button(
            ui,
            p,
            glyph,
            label,
            c.muted.then_some(p.err),
            mute.is_some(),
            key_w,
        )
        .clicked()
            && let Some(a) = mute
        {
            out.push(a);
        }
        let (glyph, label) = if c.hold.local {
            (ic::PLAY, "Resume")
        } else {
            (ic::PAUSE, "Hold")
        };
        let hold = hold_action(session, c);
        if soft_button(
            ui,
            p,
            glyph,
            label,
            c.hold.local.then_some(p.held),
            hold.is_some(),
            key_w,
        )
        .clicked()
            && let Some(a) = hold
        {
            out.push(a);
        }
        let resp = soft_button(
            ui,
            p,
            ic::DOTS_NINE,
            "Keypad",
            None,
            c.state == CallState::Active,
            key_w,
        );
        egui::Popup::from_toggle_button_response(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                for row in ["123", "456", "789", "*0#"] {
                    ui.horizontal(|ui| {
                        for d in row.chars() {
                            if keypad_key(ui, p, d).clicked() {
                                out.push(UiAction::Dtmf(c.id.clone(), d));
                            }
                        }
                    });
                }
            });
        if soft_button(
            ui,
            p,
            ic::ARROW_BEND_UP_RIGHT,
            "Transfer",
            None,
            can_transfer(session, c),
            key_w,
        )
        .on_disabled_hover_text(if session.is_some_and(|s| !s.phase.is_terminal()) {
            "A transfer is already in progress"
        } else {
            "Transfer is not available now"
        })
        .clicked()
        {
            open_composer(st, c);
        }
    });
    if c.pending.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new("Working\u{2026}")
                    .size(FS_SMALL)
                    .color(p.muted),
            );
        });
    }
    // On the consult leg the same action reads "Cancel - return to <original>" (one meaning).
    let label = match consult_leg(session, c) {
        Some(s) => format!("Cancel - return to {}", original_name(cx, s)),
        None => "Hang up".to_string(),
    };
    if solid_button(ui, ic::PHONE_X, &label, p.hangup, full).clicked()
        && let Some(a) = end_action(session, c)
    {
        out.push(a);
    }
}

// ---- idle stage and dial bar ------------------------------------------------------------------

fn idle(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    // A finished transfer stays visible for a while after its lines are gone.
    if let Some(s) = visible_session(cx, st, cx.vm.snap.transfers.iter()) {
        transfer::session_panel(ui, cx, st, s, out);
    }
    panel(p).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = SP_S;
        ui.horizontal(|ui| {
            ui.label(icon(ic::PHONE, 22.0).color(p.faint));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(strong("No calls", FS_BODY).color(p.text));
                ui.label(
                    RichText::new("Incoming calls ring here. Dial a number to place one.")
                        .size(FS_SMALL)
                        .color(p.muted),
                );
            });
        });
        dial_row(ui, cx, st, true, out);
        ui.add_space(SP_XS);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for row in ["123", "456", "789", "*0#"] {
                // Fixed-width row so the keys stay a block: 3 keys of 52 plus gaps.
                ui.allocate_ui_with_layout(
                    egui::vec2(3.0 * 52.0 + 12.0, 40.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        for d in row.chars() {
                            if keypad_key(ui, p, d).clicked() {
                                st.dial_target.push(d);
                            }
                        }
                    },
                );
            }
        });
    });
}

/// The dial bar inside the stage ("+ New call" expands it).
fn dial_bar(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    panel(cx.p).inner_margin(Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        dial_row(ui, cx, st, false, out);
    });
}

fn dial_row(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, large: bool, out: &mut Vec<UiAction>) {
    let p: &Palette = cx.p;
    let snap = cx.vm.snap;
    let usable: Vec<_> = snap.accounts.iter().filter(|a| a.enabled).collect();
    if !snap
        .accounts
        .iter()
        .any(|a| Some(&a.id) == st.dial_account.as_ref())
    {
        st.dial_account = usable
            .iter()
            .find(|a| reg_class(a) == RegClass::Registered)
            .or(usable.first())
            .map(|a| a.id.clone());
    }
    let mut submit = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let selected = st.dial_account.clone();
        let sel_status = usable.iter().find(|a| Some(&a.id) == selected.as_ref());
        egui::ComboBox::from_id_salt("dial_account")
            .width(112.0)
            .height(240.0)
            .selected_text(match &selected {
                Some(id) => RichText::new(id).color(p.text),
                None => RichText::new("No account").color(p.faint),
            })
            .icon(combo_icon)
            .show_ui(ui, |ui| {
                for a in &usable {
                    let text = format!("{}  {}", a.id, reg_label(a));
                    ui.selectable_value(&mut st.dial_account, Some(a.id.clone()), text)
                        .on_hover_text(&a.aor);
                }
            })
            .response
            .on_hover_text(sel_status.map(|a| a.aor.as_str()).unwrap_or("No account"));
        let can = sel_status.is_some() && !st.dial_target.trim().is_empty();
        let btn_w = 76.0;
        let edit = ui.add(
            egui::TextEdit::singleline(&mut st.dial_target)
                .id_salt("dial_target")
                .hint_text("Number or SIP URI")
                .font(egui::FontId::new(
                    if large {
                        FS_DISPLAY - 2.0
                    } else {
                        FS_BODY + 0.5
                    },
                    egui::FontFamily::Monospace,
                ))
                .margin(Margin::symmetric(10, if large { 10 } else { 7 }))
                .desired_width(ui.available_width() - btn_w - 6.0),
        );
        if std::mem::take(&mut st.focus_dial) {
            edit.request_focus();
        }
        if can && edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        let fill = if can { p.answer } else { p.inset };
        let fg = if can { Color32::WHITE } else { p.faint };
        let b = egui::Button::new((
            icon_fill(ic::PHONE, FS_ICON).color(fg),
            strong("Call", FS_BODY).color(fg),
        ))
        .fill(fill)
        .stroke(if can {
            Stroke::NONE
        } else {
            Stroke::new(1.0, p.border)
        })
        .corner_radius(CornerRadius::same(R_MD))
        .min_size(egui::vec2(btn_w, if large { 40.0 } else { 32.0 }));
        if ui.add_enabled(can, b).clicked() {
            submit = true;
        }
    });
    if submit
        && let Some(acc) = st.dial_account.clone()
        && !st.dial_target.trim().is_empty()
    {
        out.push(UiAction::Dial(acc, st.dial_target.trim().to_string()));
        st.dial_target.clear();
        st.new_call_open = false;
    }
}
