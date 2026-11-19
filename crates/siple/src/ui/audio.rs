//! Audio device panel: microphone, speaker and ringer choices, mic permission, test tone. Opened
//! from the footer.

use eframe::egui::{self, Align, CornerRadius, Layout, Margin, RichText};
use siple_core::audio::MicPermission;
use siple_core::state::{PartialSelection, Slot};

use super::rpc::{RpcRequest, RpcTag};
use super::{LiveAudio, UiAction, UiState, combo_icon};
use crate::snapshot::{Snapshot, device_choices, selected_label};
use crate::theme::{FS_ICON, FS_MICRO, FS_SMALL, Palette, R_MD, SP_S, icon as ic};
use crate::widgets::{chip, icon, soft_button, strong};
use siple_core::media::EchoPhase;

#[derive(Clone, Copy)]
pub(super) enum Severity {
    Warn,
    Error,
}

/// The panel body (the footer owns the open / closed toggle).
pub(super) fn show(
    ui: &mut egui::Ui,
    p: &Palette,
    snap: &Snapshot,
    st: &mut UiState,
    live: &LiveAudio,
    out: &mut Vec<UiAction>,
) {
    ui.spacing_mut().item_spacing.y = SP_S;
    if snap.audio.is_none() {
        if let Some(reason) = st.audio_disabled_reason.as_deref() {
            ui.add(
                egui::Label::new(
                    RichText::new(audio_disabled_text(reason))
                        .size(FS_SMALL)
                        .color(p.muted),
                )
                .wrap(),
            );
        } else {
            ui.label(
                RichText::new("The audio layer is not running; calls have no sound.")
                    .size(FS_SMALL)
                    .color(p.warn.ink),
            );
        }
    }
    pickers(ui, p, snap, st, live, out);
}

/// Mic permission note, the three device pickers with the live meter, the speaker test and the
/// last audio error. Shared by the footer panel, Settings and the wizard.
pub(super) fn pickers(
    ui: &mut egui::Ui,
    p: &Palette,
    snap: &Snapshot,
    st: &mut UiState,
    live: &LiveAudio,
    out: &mut Vec<UiAction>,
) {
    ui.spacing_mut().item_spacing.y = SP_S;
    if let Some(msg) = mic_warning(snap.mic) {
        egui::Frame::new()
            .fill(p.err.soft)
            .corner_radius(CornerRadius::same(R_MD))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.horizontal(|ui| {
                    ui.label(icon(ic::MICROPHONE_SLASH, FS_ICON).color(p.err.ink));
                    ui.add(
                        egui::Label::new(strong(msg, FS_SMALL).color(p.err.ink)).wrap(),
                    );
                });
                ui.label(
                    RichText::new(
                        "Grant access in System Settings \u{203A} Privacy & Security \u{203A} Microphone.",
                    )
                    .size(FS_SMALL)
                    .color(p.text),
                );
            });
    }
    device_row(ui, p, snap, Slot::Input, "Microphone", out);
    level_meter(ui, p, st, live);
    device_row(ui, p, snap, Slot::Output, "Speaker", out);
    device_row(ui, p, snap, Slot::Ringer, "Ringer", out);
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + 6.0);
        if soft_button(ui, p, ic::PLAY, "Test speaker", None, true, 0.0).clicked() {
            out.push(UiAction::TestSpeaker);
        }
        ui.label(
            RichText::new("Plays a 1 s tone on the speaker")
                .size(FS_SMALL)
                .color(p.muted),
        );
    });
    if let Some(err) = snap.audio.as_ref().and_then(|a| a.last_error.as_ref()) {
        ui.horizontal(|ui| {
            ui.label(icon(ic::WARNING_CIRCLE, FS_ICON).color(p.err.ink));
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Audio error: {err}"))
                        .size(FS_SMALL)
                        .color(p.err.ink),
                )
                .wrap(),
            );
        });
    }
}

/// The muted line shown in the audio panel while the device layer is off.
fn audio_disabled_text(reason: &str) -> String {
    format!("Audio disabled: {reason}")
}

/// A problem worth showing on the (possibly collapsed) audio header.
pub(super) fn issue(snap: &Snapshot) -> Option<(Severity, &'static str)> {
    if snap.audio.is_none() {
        return Some((Severity::Warn, "Not running"));
    }
    if matches!(snap.mic, MicPermission::Denied | MicPermission::Restricted) {
        return Some((Severity::Error, "Mic blocked"));
    }
    let a = snap.audio.as_ref()?;
    if a.last_error.is_some() {
        return Some((Severity::Error, "Audio error"));
    }
    let fallback = [&a.input, &a.output, &a.ringer]
        .iter()
        .any(|d| d.as_ref().is_some_and(|d| d.fallback_to_default));
    if fallback {
        return Some((Severity::Warn, "Device missing"));
    }
    if snap.mic == MicPermission::NotDetermined {
        return Some((Severity::Warn, "Mic not granted"));
    }
    None
}

pub(super) const LABEL_W: f32 = 78.0;

/// Dark end of the meter scale: -60 dBFS lights nothing, 0 dBFS everything.
const METER_FLOOR_DB: f32 = -60.0;
const METER_SEGS: usize = 24;

/// Lit segments for a level (dBFS).
pub(super) fn meter_segments(db: f32) -> usize {
    (((db - METER_FLOOR_DB) / -METER_FLOOR_DB) * METER_SEGS as f32)
        .round()
        .clamp(0.0, METER_SEGS as f32) as usize
}

/// The live microphone level: a segmented bar that rises at once and falls smoothly.
fn level_meter(ui: &mut egui::Ui, p: &Palette, st: &mut UiState, live: &LiveAudio) {
    let target = live.mic_dbfs.unwrap_or(-120.0);
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    st.meter_db = if target > st.meter_db {
        target
    } else {
        (st.meter_db - 45.0 * dt).max(target)
    };
    if live.mic_dbfs.is_some() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(50));
    }
    // Hug the microphone combo above; a plain allocation, not a row (rows are 32 pt tall).
    ui.add_space(-(SP_S - 2.0));
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, 6.0), egui::Sense::hover());
    let left = rect.left() + LABEL_W + 6.0;
    let gap = 2.0;
    let seg_w = (rect.right() - left - gap * (METER_SEGS as f32 - 1.0)) / METER_SEGS as f32;
    let lit = meter_segments(st.meter_db);
    for i in 0..METER_SEGS {
        let x = left + i as f32 * (seg_w + gap);
        let r = egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(seg_w, 6.0));
        let color = if i >= lit {
            p.border
        } else if i >= METER_SEGS - 3 {
            p.err.led
        } else if i >= METER_SEGS - 8 {
            p.warn.led
        } else {
            p.ok.led
        };
        ui.painter().rect_filled(r, CornerRadius::same(1), color);
    }
    let text = match live.mic_dbfs {
        Some(db) => format!("Microphone level: {} dBFS", db.round() as i32),
        None => "Microphone level: no microphone open".to_string(),
    };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, text.clone()));
    resp.on_hover_text(text);
    ui.add_space(2.0);
}

/// The echo test: record a few seconds, play them back.
pub(super) fn echo_test(
    ui: &mut egui::Ui,
    p: &Palette,
    snap: &Snapshot,
    live: &LiveAudio,
    out: &mut Vec<UiAction>,
) {
    let e = &live.echo;
    let running = matches!(e.phase, EchoPhase::Recording | EchoPhase::Playing);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(LABEL_W + 6.0);
        if running {
            if soft_button(ui, p, ic::STOP, "Stop echo test", None, true, 0.0).clicked() {
                out.push(UiAction::Rpc(RpcRequest {
                    tag: RpcTag::Echo,
                    method: "audio.echo_test",
                    params: serde_json::json!({"action": "cancel"}),
                }));
            }
        } else if soft_button(
            ui,
            p,
            ic::MICROPHONE,
            "Echo test",
            None,
            snap.audio.is_some(),
            0.0,
        )
        .on_hover_text("Records 3 s from the microphone, then plays it back")
        .clicked()
        {
            out.push(UiAction::Rpc(RpcRequest {
                tag: RpcTag::Echo,
                method: "audio.echo_test",
                params: serde_json::json!({"action": "start", "seconds": 3}),
            }));
        }
        let (text, color) = match e.phase {
            EchoPhase::Idle => ("Records 3 s, then plays it back".to_string(), p.muted),
            EchoPhase::Recording => ("Recording: say something".to_string(), p.ring.ink),
            EchoPhase::Playing => ("Playing back".to_string(), p.accent),
            EchoPhase::Done if e.silent => (
                "Nothing was recorded: check the microphone and its permission".to_string(),
                p.err.ink,
            ),
            EchoPhase::Done => (
                "Done. If you heard yourself, audio works".to_string(),
                p.ok.ink,
            ),
            EchoPhase::Failed => (
                format!(
                    "Echo test failed: {}",
                    e.error.as_deref().unwrap_or("unknown error")
                ),
                p.err.ink,
            ),
        };
        ui.add(egui::Label::new(RichText::new(text).size(FS_SMALL).color(color)).wrap());
    });
}

fn device_row(
    ui: &mut egui::Ui,
    p: &Palette,
    snap: &Snapshot,
    slot: Slot,
    label: &str,
    out: &mut Vec<UiAction>,
) {
    let eff = snap.effective.slot(slot);
    let (list, default_label) = match slot {
        Slot::Input => (&snap.devices.inputs, "System default"),
        Slot::Output => (&snap.devices.outputs, "System default"),
        Slot::Ringer => (&snap.devices.outputs, "Same as speaker"),
    };
    let choices = device_choices(list, &eff.name, default_label);
    let fallback = snap.audio.as_ref().is_some_and(|a| {
        let d = match slot {
            Slot::Input => &a.input,
            Slot::Output => &a.output,
            Slot::Ringer => &a.ringer,
        };
        d.as_ref().is_some_and(|d| d.fallback_to_default)
    });
    let mut picked: Option<String> = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.allocate_ui_with_layout(
            egui::vec2(LABEL_W, 28.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.set_width(LABEL_W);
                ui.label(RichText::new(label).size(FS_SMALL).color(p.muted));
            },
        );
        let w = ui.available_width();
        egui::ComboBox::from_id_salt(("dev", label))
            .width(w)
            .truncate()
            .icon(combo_icon)
            .selected_text(RichText::new(selected_label(&choices, &eff.name)).color(p.text))
            .show_ui(ui, |ui| {
                for c in &choices {
                    if ui.selectable_label(c.value == eff.name, &c.label).clicked() {
                        picked = Some(c.value.clone());
                    }
                }
            });
    });
    // Every choice is in config.toml now (D3), so there is no "Set in config" badge; only a
    // device that went missing is flagged.
    if fallback {
        ui.add_space(-(SP_S - 4.0));
        ui.horizontal(|ui| {
            ui.add_space(LABEL_W + 6.0);
            ui.spacing_mut().item_spacing.x = 6.0;
            chip(
                ui,
                p,
                Some(ic::WARNING),
                RichText::new("Not found, using default").size(FS_MICRO + 0.5),
                Some(p.warn),
            );
        });
    }
    if let Some(v) = picked.filter(|v| *v != eff.name) {
        let mut sel = PartialSelection::default();
        match slot {
            Slot::Input => sel.input = Some(v),
            Slot::Output => sel.output = Some(v),
            Slot::Ringer => sel.ringer = Some(v),
        }
        out.push(UiAction::SetDevices(sel));
    }
}

fn mic_warning(p: MicPermission) -> Option<&'static str> {
    match p {
        MicPermission::Denied => {
            Some("Microphone access was denied; the other side will hear silence.")
        }
        MicPermission::Restricted => Some("Microphone access is restricted by policy."),
        MicPermission::NotDetermined => Some("Microphone access has not been granted yet."),
        MicPermission::Granted | MicPermission::NotApplicable => None,
    }
}
