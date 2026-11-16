//! Settings > Audio: device pickers with the live meter, speaker test and echo test.

use eframe::egui::{self, RichText};
use siple_core::audio::MicPermission;

use super::super::form::{group, small_button};
use super::super::{Cx, UiAction, UiState, audio};
use crate::theme::FS_SMALL;

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    group(ui, p, "Devices", |ui| {
        if snap.audio.is_none() {
            let reason = st.audio_disabled_reason.as_deref().map_or_else(
                || "The audio layer is not running.".to_string(),
                |r| format!("Audio disabled: {r}"),
            );
            ui.label(RichText::new(reason).size(FS_SMALL).color(p.warn.ink));
        }
        audio::pickers(ui, p, snap, st, &cx.vm.live, out);
        audio::echo_test(ui, p, snap, &cx.vm.live, out);
    });
    if snap.mic == MicPermission::NotDetermined
        && small_button(ui, p, "Allow microphone access", true).clicked()
    {
        out.push(UiAction::RequestMicAccess);
    }
    ui.label(
        RichText::new("Changes are saved to [audio] in config.toml and apply at once.")
            .size(FS_SMALL)
            .color(p.faint),
    );
}
