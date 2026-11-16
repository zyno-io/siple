//! Settings > Extensions: BLF view and click behaviour (`[ui]`, instant) and the per-account
//! extension lists (`set_blf`, staged until Apply; reorder with the buttons or Alt+Up / Alt+Down).

use eframe::egui::{self, RichText};
use siple_core::config::BlfConfig;

use super::super::form::{choice_row, group, small_button, text_row_focus};
use super::super::{Cx, UiAction, UiState};
use super::pages::{sec_choice, ui_json};
use crate::theme::FS_SMALL;

enum Move {
    Up(usize),
    Down(usize),
    Remove(usize),
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let u = ui_json(snap);
    group(ui, p, "View and click behaviour", |ui| {
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "blf_view",
            "Layout",
            &u,
            &[("keys", "Keys"), ("lamps", "Lamps"), ("list", "List")],
        );
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "blf_click",
            "Click on an idle extension",
            &u,
            &[("dial", "Call it"), ("prefill", "Fill the dial field")],
        );
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "blf_click_in_call",
            "Click during a call",
            &u,
            &[
                ("menu", "Ask"),
                ("consult", "Consult transfer"),
                ("blind", "Blind transfer"),
            ],
        );
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "blf_click_ringing",
            "Click on a ringing extension",
            &u,
            &[("pickup", "Pick up"), ("call", "Call it")],
        );
        sec_choice(
            ui,
            p,
            out,
            "ui",
            "blf_group_by",
            "Group by",
            &u,
            &[
                ("account", "Account"),
                ("group", "Group"),
                ("none", "No grouping"),
            ],
        );
    });
    let accounts = &snap.cfg.accounts;
    if accounts.is_empty() {
        return;
    }
    if st
        .settings
        .blf_account
        .as_ref()
        .is_none_or(|id| !accounts.iter().any(|a| &a.id == id))
    {
        st.settings.blf_account = accounts.first().map(|a| a.id.clone());
    }
    let id = st.settings.blf_account.clone().unwrap_or_default();
    let Some((idx, acct)) = accounts.iter().enumerate().find(|(_, a)| a.id == id) else {
        return;
    };
    let ids: Vec<(String, String)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.id.clone()))
        .collect();
    let opts: Vec<(&str, &str)> = ids.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    group(ui, p, "Extensions per account", |ui| {
        if let Some(v) = choice_row(ui, p, "Account", &id, &opts, None) {
            st.settings.blf_account = Some(v);
        }
        let stored = &acct.blf;
        let mut list = st
            .settings
            .blf
            .get(&id)
            .cloned()
            .unwrap_or_else(|| stored.clone());
        let before = list.clone();
        let mut mv: Option<Move> = None;
        let last = list.len().saturating_sub(1);
        for (i, e) in list.iter_mut().enumerate() {
            let n = i + 1;
            ui.push_id(("blf", i), |ui| {
                egui::Frame::new()
                    .fill(p.inset)
                    .corner_radius(egui::CornerRadius::same(crate::theme::R_MD))
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let err = |k: &str| {
                            st.settings
                                .error(&format!("account[{idx}].blf[{i}].{k}"))
                                .map(str::to_owned)
                        };
                        let mut focus = false;
                        let mut field = |ui: &mut egui::Ui,
                                         label: String,
                                         v: &mut String,
                                         hint: &str,
                                         key: &str| {
                            let (new, f) =
                                text_row_focus(ui, p, &label, v, hint, false, err(key).as_deref());
                            focus |= f;
                            if let Some(new) = new {
                                *v = new;
                            }
                        };
                        field(ui, format!("Label {n}"), &mut e.label, "Reception", "label");
                        field(
                            ui,
                            format!("Extension {n}"),
                            &mut e.uri,
                            "1002 or sip:1002@host",
                            "uri",
                        );
                        field(ui, format!("Group {n}"), &mut e.group, "Optional", "group");
                        egui::CollapsingHeader::new(
                            RichText::new(format!("Pickup overrides {n}")).size(FS_SMALL),
                        )
                        .id_salt(("pickup", i))
                        .show(ui, |ui| {
                            let cur = serde_json::to_value(e.pickup)
                                .ok()
                                .and_then(|v| v.as_str().map(str::to_owned))
                                .unwrap_or_default();
                            if let Some(v) = choice_row(
                                ui,
                                p,
                                &format!("Pickup {n}"),
                                &cur,
                                &[
                                    ("", "Account default"),
                                    ("off", "Off"),
                                    ("prefix", "Feature code"),
                                    ("replaces", "Replaces"),
                                ],
                                err("pickup").as_deref(),
                            ) {
                                e.pickup = serde_json::from_value(serde_json::Value::String(v))
                                    .unwrap_or_default();
                            }
                            field(
                                ui,
                                format!("Pickup prefix {n}"),
                                &mut e.pickup_prefix,
                                "Account default",
                                "pickup_prefix",
                            );
                            field(
                                ui,
                                format!("Pickup target {n}"),
                                &mut e.pickup_target,
                                "<prefix><extension>",
                                "pickup_target",
                            );
                        });
                        ui.horizontal(|ui| {
                            let name = if e.label.is_empty() {
                                e.uri.clone()
                            } else {
                                e.label.clone()
                            };
                            let name = if name.is_empty() {
                                format!("extension {n}")
                            } else {
                                name
                            };
                            let btn = |ui: &mut egui::Ui, text: &str, enabled: bool, what: &str| {
                                let r = small_button(ui, p, text, enabled);
                                r.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::WidgetType::Button,
                                        enabled,
                                        format!("{what} {name}"),
                                    )
                                });
                                r.clicked()
                            };
                            if btn(ui, "Up", i > 0, "Move up") {
                                mv = Some(Move::Up(i));
                            }
                            if btn(ui, "Down", i < last, "Move down") {
                                mv = Some(Move::Down(i));
                            }
                            if btn(ui, "Remove", true, "Remove") {
                                mv = Some(Move::Remove(i));
                            }
                        });
                        if focus {
                            let (up, down) = ui.input_mut(|inp| {
                                (
                                    inp.consume_key(egui::Modifiers::ALT, egui::Key::ArrowUp),
                                    inp.consume_key(egui::Modifiers::ALT, egui::Key::ArrowDown),
                                )
                            });
                            if up && i > 0 {
                                mv = Some(Move::Up(i));
                            } else if down && i < last {
                                mv = Some(Move::Down(i));
                            }
                        }
                    });
            });
        }
        match mv {
            Some(Move::Up(i)) => list.swap(i, i - 1),
            Some(Move::Down(i)) => list.swap(i, i + 1),
            Some(Move::Remove(i)) => {
                list.remove(i);
            }
            None => {}
        }
        if small_button(ui, p, "Add extension", true).clicked() {
            list.push(BlfConfig::default());
        }
        if list.is_empty() {
            ui.label(
                RichText::new("No extensions on this account.")
                    .size(FS_SMALL)
                    .color(p.muted),
            );
        }
        if list != before {
            st.settings.stage_blf(&id, list, stored, &snap.config_hash);
        }
    });
}
