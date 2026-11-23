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
    group(ui, p, "Theme and layout", |ui| {
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
        launch_at_login_row(ui, p, out, &v, true);
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
        if siple_core::config::channel::current().can_register_links() {
            sec_toggle(
                ui,
                p,
                out,
                "ui",
                "handle_links",
                "Open sip:, sips:, tel:, callto: and siple: setup links",
                "",
                &v,
            );
        } else {
            links_automatic_note(ui, p);
        }
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
            speech_commands_label(
                !cfg.speech.tts_command.is_empty() || !cfg.speech.stt_command.is_empty(),
                siple_core::config::channel::current(),
            ),
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
        if small_button(ui, p, "Open config", true)
            .on_hover_text(cx.vm.config_path.display().to_string())
            .clicked()
        {
            out.push(UiAction::OpenConfig);
        }
    });
    group(ui, p, "Setup", |ui| {
        if small_button(ui, p, "Run setup again", true).clicked() {
            st.wizard.restart();
        }
    });
    group(ui, p, "About", |ui| {
        info_row(ui, p, "Version", &snap.update.current);
    });
    updates_group(ui, cx, st, out);
}

/// What the check does in this installation, for the switch's help text.
fn updates_help(u: &siple_core::update::UpdateStatus) -> &'static str {
    use siple_core::update::{DisabledReason, SourceName};
    match (u.source, u.disabled_reason) {
        (SourceName::Github, _) => {
            "About once an hour Siple asks siple.app for the list of releases (one anonymous request that carries only the version). About once a day it also asks GitHub as a second opinion, and asks GitHub alone when siple.app does not answer. A new version is downloaded only when you choose to install it."
        }
        (SourceName::AppStore, _) => {
            "Once a day Siple asks the App Store whether a newer version exists (one anonymous request to Apple that carries only the version). The App Store installs updates."
        }
        (SourceName::MicrosoftStore, _) => {
            "Once a day Siple asks Windows' Store service whether an update is waiting; Windows contacts Microsoft, Siple sends nothing itself. The Microsoft Store installs updates."
        }
        (SourceName::None, Some(DisabledReason::Sideloaded)) => {
            "This copy was not installed from the Microsoft Store, so there is nothing to check; update it the way you installed it."
        }
        (SourceName::None, Some(DisabledReason::ManagedByFlatpak)) => {
            "Flatpak keeps Siple up to date: updates come from Flathub through your software center or the flatpak update command. Siple makes no update check of its own."
        }
        (SourceName::None, _) => "Update checks are not available in this installation.",
    }
}

/// `[updates]`: the switch, the channel (GitHub builds only) and the last result of the check.
fn updates_group(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let v = serde_json::to_value(&snap.cfg.updates).unwrap_or(Value::Null);
    group(ui, p, "Updates", |ui| {
        sec_toggle(
            ui,
            p,
            out,
            "updates",
            "check",
            "Check for updates",
            updates_help(&snap.update),
            &v,
        );
        let u = &snap.update;
        if u.source == siple_core::update::SourceName::Github {
            sec_toggle(
                ui,
                p,
                out,
                "updates",
                "install",
                "Install updates in the app",
                "When you choose to, Siple downloads the new version from GitHub, checks its signature and your system's code-signing check, replaces itself and restarts, never during a call. Off: the update pill opens the release page.",
                &v,
            );
            if let Some(i) = &u.install
                && !i.possible
                && let Some(r) = i.reason.as_deref()
                && r != "disabled"
            {
                info_row(
                    ui,
                    p,
                    "In-app install",
                    crate::snapshot::install_reason_text(r),
                );
            }
        }
        let automatic = snap.cfg.updates.channel.is_none();
        let cur = if automatic {
            format!("auto_{}", u.channel)
        } else {
            u.channel.clone()
        };
        let auto_label = format!("Automatic ({})", u.channel);
        let github = u.source == siple_core::update::SourceName::Github;
        if github
            && let Some(new) = choice_row(
                ui,
                p,
                "Channel",
                &cur,
                &[
                    (&format!("auto_{}", u.channel), &auto_label),
                    ("stable", "Stable releases"),
                    ("prerelease", "Stable and release candidates"),
                ],
                None,
            )
        {
            if new.starts_with("auto_") {
                out.push(rpc::config_edit(
                    RpcTag::Instant,
                    None,
                    &[siple_core::config::edit::ConfigEdit::ResetSection {
                        section: "updates".into(),
                        key: "channel".into(),
                    }],
                ));
            } else {
                out.push(rpc::set_section("updates", "channel", EditValue::Str(new)));
            }
        }
        let result = if st.settings.update_checking {
            "Checking...".to_string()
        } else if u.disabled_reason == Some(siple_core::update::DisabledReason::ManagedByFlatpak) {
            "Updated by Flatpak".to_string()
        } else if u.source == siple_core::update::SourceName::None {
            "Not available".to_string()
        } else if !u.enabled {
            "Off".to_string()
        } else if let Some(e) = &u.error {
            format!("Last check failed: {e}")
        } else if u.checked_at.is_none() {
            "Not checked yet".to_string()
        } else if let Some(i) = u
            .install
            .as_ref()
            .filter(|i| i.state != siple_core::update::InstallPhase::Idle)
        {
            use siple_core::update::InstallPhase as P;
            let v = i.version.as_deref().unwrap_or("the update");
            match i.state {
                P::Downloading => match (i.bytes, i.total) {
                    (Some(b), Some(t)) if t > 0 => {
                        format!("Downloading {v}: {}%", b.min(t) * 100 / t)
                    }
                    _ => format!("Downloading {v}"),
                },
                P::Verifying => format!("Checking {v}"),
                P::Ready => format!("{v} is ready: restart to update"),
                P::Waiting => format!("{v} is ready: restarting when the call is over"),
                P::Committing | P::Installed | P::Restarting => {
                    format!("{v} is installed: restarting")
                }
                P::Failed => format!(
                    "Update failed: {}",
                    i.error.as_deref().unwrap_or("unknown error")
                ),
                P::Idle => String::new(),
            }
        } else if u.available {
            match u.latest.as_deref() {
                Some(v) => format!("Siple {v} is available"),
                None => "A Siple update is available".to_string(),
            }
        } else {
            "You have the latest version".to_string()
        };
        info_row(ui, p, "Status", &result);
        if let Some(t) = &u.checked_at {
            info_row(ui, p, "Last checked", &crate::ui::short_time(t));
        }
        ui.horizontal(|ui| {
            if small_button(
                ui,
                p,
                "Check now",
                u.enabled && !st.settings.update_checking,
            )
            .clicked()
            {
                st.settings.update_checking = true;
                out.push(rpc::check_updates());
            }
            if let Some(offer) = snap.update_offer() {
                match offer.action {
                    siple_core::update::UpdateAction::OpenStore => {
                        if small_button(ui, p, offer.store_button(), true).clicked() {
                            out.push(UiAction::OpenStorePage);
                        }
                    }
                    siple_core::update::UpdateAction::Install => {
                        // The pill has every step; here only the start (and Cancel while it runs).
                        use siple_core::update::InstallPhase as P;
                        let version = offer.version.unwrap_or_default();
                        // Drawing a button inside a match guard would hide the side effect.
                        #[allow(clippy::collapsible_match)]
                        match offer.install.map_or(P::Idle, |i| i.state) {
                            P::Idle | P::Failed => {
                                let (label, restart) = if snap.call_active() {
                                    ("Install, restart after the call", "manual")
                                } else {
                                    ("Install and restart", "when_idle")
                                };
                                if small_button(ui, p, label, true).clicked() {
                                    st.update_initiated = Some(version.to_string());
                                    out.push(rpc::install_update(version, restart));
                                }
                            }
                            P::Downloading | P::Verifying | P::Ready | P::Waiting => {
                                if small_button(ui, p, "Cancel update", true).clicked() {
                                    out.push(rpc::cancel_update());
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        if let Some(url) = offer.release_url
                            && small_button(ui, p, "Download", true).clicked()
                        {
                            out.push(UiAction::OpenUrl(url.to_string()));
                        }
                    }
                }
            }
        });
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
        let snippet = mcp_snippet(siple_core::config::channel::current());
        ui.add(egui::Label::new(mono(&snippet, 11.5).color(p.text)).selectable(true));
        if small_button(ui, p, "Copy snippet", true).clicked() {
            ui.ctx().copy_text(snippet);
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
            "Show the log pane and the Open config / Open logs buttons in the footer, and registration and subscription times in the account and extension tooltips",
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

/// The launch-at-login switch, or in the Microsoft Store edition (where only the user can enable
/// the startup task) a faint note on where to do it. `with_hint` adds the explanation line.
pub(in crate::ui) fn launch_at_login_row(
    ui: &mut egui::Ui,
    p: &Palette,
    out: &mut Vec<UiAction>,
    v: &Value,
    with_hint: bool,
) {
    let ch = siple_core::config::channel::current();
    if !ch.launch_at_login_in_app() {
        ui.label(
            RichText::new("Turn on Siple in Windows Settings > Apps > Startup.")
                .size(FS_SMALL)
                .color(p.faint),
        );
        return;
    }
    let hint = match (with_hint, ch.kind) {
        (false, _) => "",
        (true, siple_core::config::channel::ChannelKind::MacAppStore) => {
            "If it's switched off in System Settings > General > Login Items, turn it on there."
        }
        (true, _) => "Combine with Start hidden to start in the tray.",
    };
    sec_toggle(
        ui,
        p,
        out,
        "ui",
        "launch_at_login",
        "Start Siple when you log in",
        hint,
        v,
    );
}

/// Faint one-liner shown instead of the `handle_links` toggle in editions that declare the link
/// schemes at install time.
pub(in crate::ui) fn links_automatic_note(ui: &mut egui::Ui, p: &Palette) {
    ui.label(
        egui::RichText::new("This edition registers sip:, sips:, tel:, callto: and siple: links when it's installed. If another app opens them, make Siple the default.")
            .size(FS_SMALL)
            .color(p.faint),
    );
}

/// Absolute path of the bundled `siple-cli` for an executable inside `Siple.app`
/// (`.../Siple.app/Contents/MacOS/siple`): the CLI sits next to it. `None` outside a bundle.
fn bundled_cli_path(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let dir = exe.parent()?;
    let in_bundle = dir.ends_with("Contents/MacOS")
        && dir
            .ancestors()
            .nth(2)
            .is_some_and(|b| b.extension().is_some_and(|e| e == "app"));
    in_bundle.then(|| dir.join("siple-cli"))
}

/// The MCP client configuration for `channel`: the Flatpak edition's CLI is reached through
/// `flatpak run`, the AppImage's through the file with `--cli`, the Mac App Store edition's by the
/// absolute path of the bundled CLI (not on `PATH`), every other edition has `siple-cli` on its
/// path.
fn mcp_snippet(channel: &siple_core::config::channel::Channel) -> String {
    let exe = std::env::current_exe().ok();
    mcp_snippet_for(channel, exe.as_deref())
}

/// [`mcp_snippet`] for a given running executable.
fn mcp_snippet_for(
    channel: &siple_core::config::channel::Channel,
    exe: Option<&std::path::Path>,
) -> String {
    use siple_core::config::channel::ChannelKind;
    if let Some(file) = channel.appimage {
        // The runtime option must come first; `--cli mcp` selects the CLI whatever argv[0] is.
        let mut args: Vec<&str> = channel.launch_args_prefix().to_vec();
        args.extend(["--cli", "mcp"]);
        // serde_json escapes the path.
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {"siple": {"command": file.to_string_lossy(), "args": args}}
        }))
        .unwrap_or_default()
    } else if channel.kind == ChannelKind::Flatpak {
        format!(
            "{{\n  \"mcpServers\": {{\n    \"siple\": {{\n      \"command\": \"flatpak\",\n      \"args\": [\"run\", \"--command=siple-cli\", \"{}\", \"mcp\"]\n    }}\n  }}\n}}",
            siple_core::config::APP_ID
        )
    } else if channel.kind == ChannelKind::MacAppStore {
        let cli = exe
            .and_then(bundled_cli_path)
            .unwrap_or_else(|| "/Applications/Siple.app/Contents/MacOS/siple-cli".into());
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {"siple": {"command": cli.to_string_lossy(), "args": ["mcp"]}}
        }))
        .unwrap_or_default()
    } else {
        "{\n  \"mcpServers\": {\n    \"siple\": { \"command\": \"siple-cli\", \"args\": [\"mcp\"] }\n  }\n}"
            .to_string()
    }
}

/// Value of the "Speech commands" info row: the Mac App Store and Flatpak editions cannot run them at all.
fn speech_commands_label(
    configured: bool,
    channel: &siple_core::config::channel::Channel,
) -> &'static str {
    if !channel.can_exec {
        "not available in this edition"
    } else if configured {
        "configured"
    } else {
        "(none)"
    }
}

#[cfg(test)]
mod speech_label_tests {
    use super::speech_commands_label;
    use siple_core::config::channel::Channel;

    #[test]
    fn speech_row_by_channel() {
        assert_eq!(
            speech_commands_label(true, &Channel::MAC_APP_STORE),
            "not available in this edition"
        );
        assert_eq!(
            speech_commands_label(true, &Channel::FLATPAK),
            "not available in this edition"
        );
        assert_eq!(speech_commands_label(true, &Channel::DIRECT), "configured");
        assert_eq!(
            speech_commands_label(
                true,
                &Channel::appimage(std::path::Path::new("/x/Siple.AppImage"))
            ),
            "configured"
        );
        assert_eq!(speech_commands_label(false, &Channel::DIRECT), "(none)");
    }
}

#[cfg(test)]
mod mcp_snippet_tests {
    use super::mcp_snippet;
    use siple_core::config::channel::Channel;

    #[test]
    fn the_flatpak_snippet_goes_through_flatpak_run() {
        let direct = mcp_snippet(&Channel::DIRECT);
        assert!(direct.contains("\"command\": \"siple-cli\""));
        let f: serde_json::Value = serde_json::from_str(&mcp_snippet(&Channel::FLATPAK)).unwrap();
        assert_eq!(f["mcpServers"]["siple"]["command"], "flatpak");
        assert_eq!(
            f["mcpServers"]["siple"]["args"],
            serde_json::json!(["run", "--command=siple-cli", "io.zyno.siple", "mcp"])
        );
        let d: serde_json::Value = serde_json::from_str(&direct).unwrap();
        assert_eq!(d["mcpServers"]["siple"]["args"], serde_json::json!(["mcp"]));
    }

    #[test]
    fn the_mac_app_store_snippet_uses_the_bundled_cli_path() {
        use super::{bundled_cli_path, mcp_snippet_for};
        use std::path::Path;
        let command = |exe: Option<&Path>| -> std::path::PathBuf {
            let v: serde_json::Value =
                serde_json::from_str(&mcp_snippet_for(&Channel::MAC_APP_STORE, exe)).unwrap();
            assert_eq!(v["mcpServers"]["siple"]["args"], serde_json::json!(["mcp"]));
            // Path equality ignores the separator style (Windows runs this test too).
            Path::new(v["mcpServers"]["siple"]["command"].as_str().unwrap()).to_path_buf()
        };
        assert_eq!(
            command(Some(Path::new(
                "/Applications/Siple.app/Contents/MacOS/siple"
            ))),
            Path::new("/Applications/Siple.app/Contents/MacOS/siple-cli")
        );
        assert_eq!(
            command(Some(Path::new(
                "/Users/u/Apps/Siple.app/Contents/MacOS/siple"
            ))),
            Path::new("/Users/u/Apps/Siple.app/Contents/MacOS/siple-cli")
        );
        // Not inside a bundle: the default install location.
        assert_eq!(
            command(Some(Path::new("/tmp/target/debug/siple"))),
            Path::new("/Applications/Siple.app/Contents/MacOS/siple-cli")
        );
        assert_eq!(
            command(None),
            Path::new("/Applications/Siple.app/Contents/MacOS/siple-cli")
        );
        assert!(bundled_cli_path(Path::new("/x/Contents/MacOS/siple")).is_none());
        // Other editions keep the bare name.
        assert!(
            mcp_snippet_for(
                &Channel::DIRECT,
                Some(Path::new("/A/Siple.app/Contents/MacOS/siple"))
            )
            .contains("\"command\": \"siple-cli\"")
        );
    }

    #[test]
    fn the_appimage_snippet_runs_the_file_with_cli() {
        for path in [
            "/home/u/Applications/Siple.AppImage",
            "/home/u/My \"Apps\"/Siple.AppImage",
        ] {
            let ch = Channel::appimage(Box::leak(std::path::PathBuf::from(path).into_boxed_path()));
            let v: serde_json::Value = serde_json::from_str(&mcp_snippet(&ch)).unwrap();
            assert_eq!(v["mcpServers"]["siple"]["command"], path);
            assert_eq!(
                v["mcpServers"]["siple"]["args"],
                serde_json::json!(["--cli", "mcp"])
            );
        }
    }

    #[test]
    fn an_extracted_appimage_snippet_starts_with_the_runtime_option() {
        let ch =
            Channel::appimage(std::path::Path::new("/home/u/Siple.AppImage")).with_extracted(true);
        let v: serde_json::Value = serde_json::from_str(&mcp_snippet(&ch)).unwrap();
        assert_eq!(
            v["mcpServers"]["siple"]["args"],
            serde_json::json!(["--appimage-extract-and-run", "--cli", "mcp"])
        );
    }
}
