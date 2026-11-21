//! Footer: status line (registration summary, audio problems), action failure notice, the
//! audio device panel and the log pane.

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, RichText, Stroke};

use super::{Cx, UiAction, UiState, audio};
use crate::snapshot::{RegClass, reg_class, reg_label};
use crate::theme::{FS_ICON, FS_MICRO, FS_SMALL, Palette, R_MD, SP_XS, icon as ic};
use crate::widgets::{ghost_button, icon, icon_fill, mono, pill};

/// "2 of 3 registered · legacy-fs: 403 Forbidden".
pub(super) fn status_text(cx: &Cx) -> String {
    let enabled: Vec<_> = cx.vm.snap.accounts.iter().filter(|a| a.enabled).collect();
    if enabled.is_empty() {
        return "No accounts enabled".into();
    }
    let registered = enabled
        .iter()
        .filter(|a| reg_class(a) == RegClass::Registered)
        .count();
    let mut text = if registered == enabled.len() {
        if registered == 1 {
            "Registered".to_string()
        } else {
            format!("All {registered} registered")
        }
    } else {
        format!("{registered} of {} registered", enabled.len())
    };
    for a in enabled.iter().filter(|a| reg_class(a) == RegClass::Failed) {
        text.push_str(&format!(
            " \u{b7} {}: {}",
            a.id,
            reg_label(a).replace("failed: ", "")
        ));
    }
    text
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let vm = cx.vm;
    ui.spacing_mut().item_spacing.y = SP_XS;
    let dev = vm.snap.ui.developer_tools;
    if !dev && st.show_logs {
        // Developer tools went off while the pane was open: close it and stop log capture.
        st.show_logs = false;
        out.push(UiAction::LogsVisible(false));
    }
    if let Some(n) = vm.notice {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(icon_fill(ic::WARNING_CIRCLE, FS_ICON).color(p.err.led));
            ui.add(egui::Label::new(RichText::new(n).size(FS_SMALL).color(p.err.ink)).wrap());
        });
    }
    if st.audio_open {
        egui::Frame::new()
            .fill(p.inset)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(R_MD))
            .inner_margin(Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| audio::show(ui, p, vm.snap, st, &vm.live, out));
            });
        ui.add_space(2.0);
    }
    // Narrow windows give the status text its own line.
    let own_line = ui.available_width() < 600.0;
    if own_line {
        ui.add(
            egui::Label::new(RichText::new(status_text(cx)).size(FS_SMALL).color(p.muted))
                .truncate(),
        );
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        // Right-to-left so the buttons keep their size and the status text truncates.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if dev {
                let log_btn = egui::Button::new((
                    icon(ic::TERMINAL_WINDOW, FS_ICON).color(if st.show_logs {
                        p.accent
                    } else {
                        p.muted
                    }),
                    RichText::new("Log").size(FS_SMALL).color(p.text),
                ))
                .selected(st.show_logs)
                .frame_when_inactive(st.show_logs)
                .corner_radius(CornerRadius::same(R_MD));
                if ui.add(log_btn).clicked() {
                    st.show_logs = !st.show_logs;
                    out.push(UiAction::LogsVisible(st.show_logs));
                }
                if vm.has_log_dir && ghost_button(ui, p, ic::FOLDER_OPEN, "Open logs").clicked() {
                    out.push(UiAction::OpenLogs);
                }
                if ghost_button(ui, p, ic::FILE_TEXT, "Open config")
                    .on_hover_text(vm.config_path.display().to_string())
                    .clicked()
                {
                    out.push(UiAction::OpenConfig);
                }
            }
            let audio_btn = egui::Button::new((
                icon(ic::SPEAKER_HIGH, FS_ICON).color(if st.audio_open {
                    p.accent
                } else {
                    p.muted
                }),
                RichText::new("Audio").size(FS_SMALL).color(p.text),
            ))
            .selected(st.audio_open)
            .frame_when_inactive(st.audio_open)
            .corner_radius(CornerRadius::same(R_MD));
            if ui
                .add(audio_btn)
                .on_hover_text("Microphone, speaker and ringer devices")
                .clicked()
            {
                st.audio_open = !st.audio_open;
            }
            if let Some((sev, text)) = audio::issue(vm.snap) {
                let tone = match sev {
                    audio::Severity::Warn => p.warn,
                    audio::Severity::Error => p.err,
                };
                pill(ui, tone, text, true);
            }
            if !own_line {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(status_text(cx)).size(FS_SMALL).color(p.muted),
                        )
                        .truncate(),
                    );
                });
            }
        });
    });
    if st.show_logs {
        logs(ui, p, cx);
        ui.add_space(2.0);
    }
}

fn logs(ui: &mut egui::Ui, p: &Palette, cx: &Cx) {
    egui::Frame::new()
        .fill(p.inset)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(CornerRadius::same(R_MD))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(170.0)
                .stick_to_bottom(true)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if cx.vm.snap.logs.is_empty() {
                        ui.label(
                            RichText::new("No log lines yet.")
                                .size(FS_SMALL)
                                .color(p.faint),
                        );
                    }
                    for l in &cx.vm.snap.logs {
                        ui.add(egui::Label::new(mono(l, FS_MICRO).color(log_color(p, l))).wrap());
                    }
                });
        });
}

/// Log lines tinted by level (`WARN` / `ERROR` anywhere in the prefix).
fn log_color(p: &Palette, line: &str) -> Color32 {
    let head = &line[..line.floor_char_boundary(line.len().min(48))];
    if head.contains("ERROR") {
        p.err.ink
    } else if head.contains("WARN") {
        p.warn.ink
    } else {
        p.muted
    }
}
