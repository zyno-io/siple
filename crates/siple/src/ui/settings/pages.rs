//! The smaller Settings pages: calls, shortcuts, appearance, notifications, advanced, logging and
//! developer. Toggles and pickers apply at once as one-key `config.edit` calls.

use eframe::egui::{self, RichText};
use serde_json::Value;
use siple_core::config::edit::EditValue;

use super::super::form::{choice_row, group, info_row, int_row, small_button, toggle_row};
use super::super::rpc::{self, RpcTag};
use super::super::{Cx, UiAction, UiState};
use super::accounts::FILE_ONLY_ACCOUNT_KEYS;
use crate::snapshot::Snapshot;
use crate::theme::{FS_SMALL, Palette};
use crate::widgets::mono;

pub(super) fn ui_json(snap: &Snapshot) -> Value {
    serde_json::to_value(&snap.cfg.ui).unwrap_or(Value::Null)
}

/// A switch bound to a boolean key of a section.
#[allow(clippy::too_many_arguments)]
pub(super) fn sec_toggle(
    ui: &mut egui::Ui,
    p: &Palette,
    out: &mut Vec<UiAction>,
    section: &str,
    key: &str,
    label: &str,
    help: &str,
    v: &Value,
) {
    let on = v[key].as_bool().unwrap_or(false);
    if let Some(new) = toggle_row(ui, p, label, on, help, None) {
        out.push(rpc::set_section(section, key, EditValue::Bool(new)));
    }
}

/// A drop-down bound to an enum key of a section.
#[allow(clippy::too_many_arguments)]
pub(super) fn sec_choice(
    ui: &mut egui::Ui,
    p: &Palette,
    out: &mut Vec<UiAction>,
    section: &str,
    key: &str,
    label: &str,
    v: &Value,
    options: &[(&str, &str)],
) {
    let cur = v[key].as_str().unwrap_or_default();
    if let Some(new) = choice_row(ui, p, label, cur, options, None) {
        out.push(rpc::set_section(section, key, EditValue::Str(new)));
    }
}

pub(super) fn calls(ui: &mut egui::Ui, cx: &Cx, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let v = serde_json::to_value(&cx.vm.snap.cfg.calls).unwrap_or(Value::Null);
    group(ui, p, "Lines", |ui| {
        sec_choice(
            ui,
            p,
            out,
            "calls",
            "hold_scope",
            "Hold scope",
            &v,
            &[("all", "All accounts"), ("account", "Same account only")],
        );
        ui.label(
            RichText::new("Which calls are put on hold when you switch to another line.")
                .size(FS_SMALL)
                .color(p.faint),
        );
        let code = v["decline_code"].as_i64().unwrap_or(486);
        if let Some(n) = int_row(ui, p, "Decline code", code, 400..=699, None) {
            out.push(rpc::set_section("calls", "decline_code", EditValue::Int(n)));
        }
        ui.label(
            RichText::new("Answer sent when you decline a second call while on a call.")
                .size(FS_SMALL)
                .color(p.faint),
        );
        sec_toggle(
            ui,
            p,
            out,
            "calls",
            "resume_after_failed_transfer",
            "Resume the held call after a failed transfer",
            "",
            &v,
        );
    });
}

/// A chord as shown to the user: `Mod` spelled out for this platform (Cmd on macOS, Ctrl
/// elsewhere).
pub(crate) fn chord_text(chord: &siple_core::config::shortcuts::Chord, mac: bool) -> String {
    let mut c = chord.clone();
    let mut out = String::new();
    if std::mem::take(&mut c.modifier) {
        out.push_str(if mac { "Cmd+" } else { "Ctrl+" });
    }
    out.push_str(&c.to_string());
    out
}

pub(super) fn shortcuts(ui: &mut egui::Ui, cx: &Cx) {
    use siple_core::config::shortcuts::Shortcut;
    let p = cx.p;
    let overrides = &cx.vm.snap.ui.shortcuts;
    let title = if cfg!(target_os = "macos") {
        "Keyboard shortcuts (Cmd = Mod)"
    } else {
        "Keyboard shortcuts (Ctrl = Mod)"
    };
    let mac = cfg!(target_os = "macos");
    let conflicts = siple_core::config::shortcuts::conflicts(overrides, mac);
    let inert = siple_core::config::shortcuts::inert(overrides, mac);
    if !conflicts.is_empty() || !inert.is_empty() {
        group(ui, p, "Shortcut problems", |ui| {
            for (a, b) in &conflicts {
                let line = format!(
                    "Conflict: {} and {} are both {}; {} wins",
                    a.name(),
                    b.name(),
                    chord_text(&a.effective(overrides), mac),
                    a.name(),
                );
                ui.label(RichText::new(line).size(FS_SMALL).color(p.err.ink));
            }
            for s in &inert {
                let line = format!(
                    "{} is {}, which never fires here (Cmd is the macOS Command key; use Mod or Ctrl)",
                    s.name(),
                    chord_text(&s.effective(overrides), mac),
                );
                ui.label(RichText::new(line).size(FS_SMALL).color(p.warn.ink));
            }
        });
    }
    group(ui, p, title, |ui| {
        for s in Shortcut::all() {
            let chord = chord_text(&s.effective(overrides), mac);
            let custom = s.is_overridden(overrides);
            let value = if custom {
                format!("{chord}  (custom)")
            } else {
                chord
            };
            info_row(ui, p, &s.label(), &value);
        }
        ui.label(
            RichText::new(
                "Digits, * and # send tones while a line is active, else they type into the dial \
                 field. Esc only closes popovers and the transfer composer. Change a chord in \
                 [ui.shortcuts] of the config file (action names: answer, hangup, mute, hold, \
                 transfer, line_1..line_9, line_prev, line_next, focus_dial, filter_extensions, \
                 settings).",
            )
            .size(FS_SMALL)
            .color(p.faint),
        );
    });
}

pub(super) fn appearance(ui: &mut egui::Ui, cx: &Cx, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let v = ui_json(cx.vm.snap);
    group(ui, p, "Appearance", |ui| {
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "theme",
            "Theme",
            &v,
            &[
                ("system", "Follow the system"),
                ("light", "Light"),
                ("dark", "Dark"),
            ],
        );
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "density",
            "Density",
            &v,
            &[("comfortable", "Comfortable"), ("compact", "Compact")],
        );
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "reduced_motion",
            "Reduced motion",
            &v,
            &[
                ("system", "Follow the system"),
                ("on", "On"),
                ("off", "Off"),
            ],
        );
    });
}

pub(super) fn notifications(ui: &mut egui::Ui, cx: &Cx, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let v = ui_json(cx.vm.snap);
    group(ui, p, "Tray and window", |ui| {
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "tray",
            "Show in the tray / menu bar",
            "",
            &v,
        );
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "close_to_tray",
            "Closing the window keeps Siple running",
            "",
            &v,
        );
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "hide_from_taskbar",
            "Hide from the taskbar / Dock while hidden",
            "",
            &v,
        );
        sec_toggle(ui, p, out, "ui", "start_hidden", "Start hidden", "", &v);
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "show_on_incoming",
            "Show the window on an incoming call",
            "",
            &v,
        );
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "show_on_answer",
            "Show the window when a call is answered",
            "",
            &v,
        );
    });
    group(ui, p, "Notifications and links", |ui| {
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "notifications",
            "Desktop notifications",
            "",
            &v,
        );
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "handle_links",
            "Open sip:, sips:, tel: and callto: links",
            "",
            &v,
        );
    });
}

pub(super) fn logging(ui: &mut egui::Ui, cx: &Cx, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let lv = serde_json::to_value(&snap.cfg.logging).unwrap_or(Value::Null);
    group(ui, p, "Privacy", |ui| {
        sec_toggle(
            ui,
            p,
            out,
            "logging",
            "redact_credentials",
            "Redact credentials in logs and traces",
            "",
            &lv,
        );
        sec_toggle(
            ui,
            p,
            out,
            "logging",
            "redact_srtp_keys",
            "Redact SRTP keys in logs and traces",
            "",
            &lv,
        );
    });
    group(ui, p, "Log level", |ui| {
        let cur = snap.cfg.general.log_level.clone();
        let levels = [
            ("error", "Error"),
            ("warn", "Warning"),
            ("info", "Info"),
            ("debug", "Debug"),
            ("trace", "Trace"),
        ];
        if levels.iter().any(|(v, _)| *v == cur) {
            if let Some(new) = choice_row(ui, p, "Log level", &cur, &levels, None) {
                out.push(rpc::set_section(
                    "general",
                    "log_level",
                    EditValue::Str(new),
                ));
            }
        } else {
            info_row(
                ui,
                p,
                "Log level",
                &format!("{cur} (a filter set in the config file)"),
            );
        }
        if cx.vm.has_log_dir && small_button(ui, p, "Open logs", true).clicked() {
            out.push(UiAction::OpenLogs);
        }
    });
    group(ui, p, "SIP trace per account", |ui| {
        for a in &snap.cfg.accounts {
            if let Some(on) = toggle_row(ui, p, &format!("Trace {}", a.id), a.sip_trace, "", None) {
                out.push(rpc::config_edit(
                    RpcTag::Instant,
                    None,
                    &[siple_core::config::edit::ConfigEdit::SetAccount {
                        id: a.id.clone(),
                        field: siple_core::config::edit::AccountField::SipTrace,
                        value: EditValue::Bool(on),
                    }],
                ));
            }
        }
        if snap.cfg.accounts.is_empty() {
            ui.label(RichText::new("No accounts.").size(FS_SMALL).color(p.muted));
        }
    });
}

pub(super) fn advanced(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let cfg = &snap.cfg;
    group(ui, p, "Set in the config file only", |ui| {
        ui.label(
            RichText::new(
                "These keys are edited in config.toml. The file reloads when you save it.",
            )
            .size(FS_SMALL)
            .color(p.faint),
        );
        info_row(
            ui,
            p,
            "RTP port range",
            &format!(
                "{}-{}",
                cfg.media.rtp_port_range[0], cfg.media.rtp_port_range[1]
            ),
        );
        info_row(
            ui,
            p,
            "Jitter buffer",
            &format!("{}-{} ms", cfg.media.jitter_min_ms, cfg.media.jitter_max_ms),
        );
        info_row(
            ui,
            p,
            "Log directory",
            if cfg.general.log_dir.is_empty() {
                "(default)"
            } else {
                &cfg.general.log_dir
            },
        );
        info_row(
            ui,
            p,
            "Log retention",
            &format!("{} days", cfg.general.log_retention_days),
        );
        info_row(
            ui,
            p,
            "Speech commands",
            if cfg.speech.tts_command.is_empty() && cfg.speech.stt_command.is_empty() {
                "(none)"
            } else {
                "configured"
            },
        );
        for a in &cfg.accounts {
            let prov = &snap.provenance["account"][a.id.as_str()];
            let set: Vec<&str> = FILE_ONLY_ACCOUNT_KEYS
                .iter()
                .copied()
                .filter(|k| prov[*k] == "file")
                .collect();
            if !set.is_empty() {
                info_row(ui, p, &a.id, &set.join(", "));
            }
        }
        if small_button(ui, p, "Open config", true).clicked() {
            out.push(UiAction::OpenConfig);
        }
    });
    group(ui, p, "Setup", |ui| {
        if small_button(ui, p, "Run setup again", true).clicked() {
            st.wizard.restart();
        }
    });
}

pub(super) fn developer(ui: &mut egui::Ui, cx: &Cx, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let v = ui_json(snap);
    let c = &snap.cfg.control;
    let dir = cx
        .vm
        .config_path
        .parent()
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    group(ui, p, "Control API", |ui| {
        info_row(
            ui,
            p,
            "Status",
            "Running: Siple serves the control API while it runs",
        );
        let sock = if c.unix_socket.is_empty() {
            format!("{} (default)", dir.join("control.sock").display())
        } else {
            c.unix_socket.clone()
        };
        info_row(ui, p, "Unix socket", &sock);
        if !c.named_pipe.is_empty() {
            info_row(ui, p, "Named pipe", &c.named_pipe);
        }
        info_row(
            ui,
            p,
            "TCP",
            if c.tcp_listen.is_empty() {
                "off"
            } else {
                &c.tcp_listen
            },
        );
        info_row(
            ui,
            p,
            "Token and endpoints",
            &dir.join("runtime.json").display().to_string(),
        );
    });
    group(ui, p, "MCP client", |ui| {
        ui.label(
            RichText::new(
                "Add this to an MCP client (for example Claude Desktop) to let an agent use Siple:",
            )
            .size(FS_SMALL)
            .color(p.muted),
        );
        let snippet = "{\n  \"mcpServers\": {\n    \"siple\": { \"command\": \"siple-cli\", \"args\": [\"mcp\"] }\n  }\n}";
        ui.add(egui::Label::new(mono(snippet, 11.5).color(p.text)).selectable(true));
        if small_button(ui, p, "Copy snippet", true).clicked() {
            ui.ctx().copy_text(snippet.to_string());
        }
    });
    group(ui, p, "Developer tools", |ui| {
        sec_toggle(
            ui,
            p,
            out,
            "ui",
            "developer_tools",
            "Show developer tools",
            "",
            &v,
        );
        ui.horizontal(|ui| {
            if small_button(ui, p, "Open config", true).clicked() {
                out.push(UiAction::OpenConfig);
            }
            if cx.vm.has_log_dir && small_button(ui, p, "Open logs", true).clicked() {
                out.push(UiAction::OpenLogs);
            }
        });
    });
}

/// [`sec_toggle`] without help text (the wizard's done step).
pub(in crate::ui) fn sec_toggle_pub(
    ui: &mut egui::Ui,
    p: &Palette,
    out: &mut Vec<UiAction>,
    section: &str,
    key: &str,
    label: &str,
    v: &Value,
) {
    sec_toggle(ui, p, out, section, key, label, "", v);
}
