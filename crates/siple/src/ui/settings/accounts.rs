//! Settings > Accounts: the list (add, remove, enable) and the form of the selected account.

use eframe::egui::{self, Align, Layout, RichText};
use serde_json::Value;
use siple_core::config::edit::{AccountField, ConfigEdit, EditValue};
use siple_core::config::{AccountConfig, PasswordStore};

use super::super::draft::{self, DraftForm, PROFILES, SRTP, TRANSPORTS};
use super::super::form::{
    choice_row, group, hint_line, int_row, message_box, primary_button, primary_icon_button,
    small_button, text_row, toggle_row,
};
use super::super::rpc::{self, RpcRequest, RpcTag, TestRun};
use super::super::setup_link;
use super::super::wizard::{Offer, Origin};
use super::super::{Cx, UiAction, UiState, reg_tone};
use crate::snapshot::{RegClass, reg_class};
use crate::theme::{FS_BODY, FS_MICRO, FS_SMALL, Palette, R_LG, SP_S, icon as ic};
use crate::widgets::{Chevron, account_switch, chevron_button, chip, mono, paint_lamp, strong};

/// Account keys only the config file sets; shown read-only when set.
pub(super) const FILE_ONLY_ACCOUNT_KEYS: &[&str] = &[
    "srtp_suites",
    "extra_headers",
    "attended_refer_on",
    "attended_refer_target",
    "accept_refer",
    "hold_style",
    "hold_send_cn",
    "dnd_reject_code",
    "max_calls",
    "follow_redirects",
    "session_expires",
    "min_se",
    "tls_ca_file",
    "tls_cert",
    "tls_key",
    "tls_server_name",
    "use_sips_uri",
    "outbound_proxy_route",
    "audio_source",
    "audio_sink",
    "mwi_uri",
    "blf_expires",
    "user_agent",
    "bind_address",
    "public_address",
    "keepalive_secs",
    "keepalive_method",
    "rewrite_contact",
    "codecs",
    "dtmf_mode",
    "prack",
    "session_timers",
    "expires",
    "register",
];

/// The account as JSON without its password.
fn account_json(a: &AccountConfig) -> Value {
    let mut v = serde_json::to_value(a).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("password");
    }
    v
}

fn ev(v: &Value) -> EditValue {
    match v {
        Value::Bool(b) => EditValue::Bool(*b),
        Value::Number(n) => EditValue::Int(n.as_i64().unwrap_or(0)),
        Value::String(s) => EditValue::Str(s.clone()),
        _ => EditValue::Str(String::new()),
    }
}

/// Everything a field row needs.
struct F<'a> {
    id: &'a str,
    idx: usize,
    v: Value,
    hash: &'a str,
    p: &'a Palette,
}

impl F<'_> {
    fn cur(&self, field: AccountField) -> EditValue {
        ev(&self.v[field.key()])
    }
}

fn shown(st: &UiState, f: &F, field: AccountField) -> EditValue {
    st.settings
        .pending(f.id, field)
        .cloned()
        .unwrap_or_else(|| f.cur(field))
}

fn text(ui: &mut egui::Ui, st: &mut UiState, f: &F, field: AccountField, label: &str, hint: &str) {
    let cur = f.cur(field);
    let EditValue::Str(s) = shown(st, f, field) else {
        return;
    };
    let err = st
        .settings
        .account_error(f.idx, field.key())
        .map(str::to_owned);
    if let Some(new) = text_row(ui, f.p, label, &s, hint, false, err.as_deref()) {
        st.settings
            .set_field(f.id, field, &cur, EditValue::Str(new), f.hash);
    }
}

fn choice(
    ui: &mut egui::Ui,
    st: &mut UiState,
    f: &F,
    field: AccountField,
    label: &str,
    options: &[(&str, &str)],
) {
    let cur = f.cur(field);
    let EditValue::Str(s) = shown(st, f, field) else {
        return;
    };
    let err = st
        .settings
        .account_error(f.idx, field.key())
        .map(str::to_owned);
    if let Some(new) = choice_row(ui, f.p, label, &s, options, err.as_deref()) {
        st.settings
            .set_field(f.id, field, &cur, EditValue::Str(new), f.hash);
    }
}

fn toggle(
    ui: &mut egui::Ui,
    st: &mut UiState,
    f: &F,
    field: AccountField,
    label: &str,
    help: &str,
) {
    let cur = f.cur(field);
    let EditValue::Bool(on) = shown(st, f, field) else {
        return;
    };
    let err = st
        .settings
        .account_error(f.idx, field.key())
        .map(str::to_owned);
    if let Some(new) = toggle_row(ui, f.p, label, on, help, err.as_deref()) {
        st.settings
            .set_field(f.id, field, &cur, EditValue::Bool(new), f.hash);
    }
}

fn int(
    ui: &mut egui::Ui,
    st: &mut UiState,
    f: &F,
    field: AccountField,
    label: &str,
    range: std::ops::RangeInclusive<i64>,
) {
    let cur = f.cur(field);
    let EditValue::Int(n) = shown(st, f, field) else {
        return;
    };
    let err = st
        .settings
        .account_error(f.idx, field.key())
        .map(str::to_owned);
    if let Some(new) = int_row(ui, f.p, label, n, range, err.as_deref()) {
        st.settings
            .set_field(f.id, field, &cur, EditValue::Int(new), f.hash);
    }
}

pub(super) fn show(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let snap = cx.vm.snap;
    let accounts = &snap.cfg.accounts;
    // An account added from a setup link is selected once it is in the list.
    if let Some(id) = st.settings.select_after.clone()
        && accounts.iter().any(|a| a.id == id)
    {
        st.settings.account = Some(id);
        st.settings.select_after = None;
    }
    if st
        .settings
        .account
        .as_ref()
        .is_none_or(|id| !accounts.iter().any(|a| &a.id == id))
    {
        st.settings.account = accounts.first().map(|a| a.id.clone());
    }
    // Header actions.
    ui.horizontal_wrapped(|ui| {
        if primary_icon_button(ui, p, ic::PLUS, "New account", st.settings.add.is_none()).clicked()
        {
            st.settings.add = Some(DraftForm::default());
        }
        if small_button(ui, p, "Add from setup link", true).clicked() {
            st.settings.paste = match st.settings.paste {
                Some(_) => None,
                None => Some(String::new()),
            };
            st.settings.paste_error = None;
        }
        let file_pw = accounts
            .iter()
            .filter(|a| a.password_store == PasswordStore::File && !a.password.expose().is_empty())
            .map(|a| a.id.clone())
            .collect::<Vec<_>>();
        let keyring = accounts
            .iter()
            .filter(|a| a.password_store == PasswordStore::Keyring)
            .map(|a| a.id.clone())
            .collect::<Vec<_>>();
        if !file_pw.is_empty() && small_button(ui, p, "Move passwords to keychain", true).clicked()
        {
            out.push(move_store(&file_pw, "keyring"));
        }
        if !keyring.is_empty()
            && small_button(ui, p, "Move passwords back to the config file", true).clicked()
        {
            out.push(move_store(&keyring, "file"));
        }
        if small_button(ui, p, "Run setup again", true).clicked() {
            st.wizard.restart();
        }
    });
    if st.settings.paste.is_some() {
        let mut offered = None;
        group(ui, p, "Add from setup link", |ui| {
            offered =
                setup_link::paste_row(ui, p, &mut st.settings.paste, &mut st.settings.paste_error);
        });
        if let Some(link) = offered {
            let id = link.account.draft().id;
            match st.wizard.offer_link(link, Origin::Pasted, snap, cx.vm.now) {
                Offer::Refused(why) => st.settings.paste_error = Some(why.to_string()),
                Offer::Opened | Offer::Replaced => {
                    st.settings.paste = None;
                    st.settings.paste_error = None;
                    st.settings.select_after = Some(id);
                }
            }
        }
    }
    if st.settings.add.is_some() {
        add_form(ui, cx, st, out);
    }
    // The list.
    for (i, a) in accounts.iter().enumerate() {
        account_row(ui, cx, st, a, (i, accounts.len()), out);
    }
    if accounts.is_empty() {
        ui.label(
            RichText::new("No accounts yet. Add one, or run the setup again.")
                .size(FS_SMALL)
                .color(p.muted),
        );
        return;
    }
    let Some(id) = st.settings.account.clone() else {
        return;
    };
    let Some((idx, a)) = accounts.iter().enumerate().find(|(_, a)| a.id == id) else {
        return;
    };
    let f = F {
        id: &a.id,
        idx,
        v: account_json(a),
        hash: &snap.config_hash,
        p,
    };
    ui.add_space(SP_S);
    ui.label(strong(&a.id, 14.0).color(p.text));
    identity(ui, cx, st, &f, a);
    group(ui, p, "Server & transport", |ui| {
        text(
            ui,
            st,
            &f,
            AccountField::Registrar,
            "Registrar",
            "Same as domain",
        );
        text(
            ui,
            st,
            &f,
            AccountField::OutboundProxy,
            "Outbound proxy",
            "Optional",
        );
        choice(ui, st, &f, AccountField::Transport, "Transport", TRANSPORTS);
        if let Some(status) = snap.accounts.iter().find(|s| s.id == a.id)
            && status.transport.is_auto()
            && let Some(t) = status.transport_effective
        {
            hint_line(
                ui,
                p,
                &format!("Auto picked {}", format!("{t:?}").to_uppercase()),
            );
        }
        int(
            ui,
            st,
            &f,
            AccountField::LocalPort,
            "Local port",
            0..=65_535,
        );
        hint_line(ui, p, "0 = pick a free port");
    });
    security(ui, cx, st, &f, a, out);
    group(ui, p, "Behaviour", |ui| {
        let dnd = snap
            .accounts
            .iter()
            .find(|s| s.id == a.id)
            .is_some_and(|s| s.dnd);
        if let Some(on) = toggle_row(ui, p, "Do not disturb", dnd, "applies at once", None) {
            out.push(UiAction::SetDnd(a.id.clone(), on));
        }
        toggle(ui, st, &f, AccountField::AutoAnswer, "Auto-answer", "");
        int(
            ui,
            st,
            &f,
            AccountField::AnswerDelayMs,
            "Auto-answer delay (ms)",
            0..=60_000,
        );
        toggle(
            ui,
            st,
            &f,
            AccountField::AllowIntercom,
            "Allow intercom",
            "",
        );
        toggle(
            ui,
            st,
            &f,
            AccountField::AutoHold,
            "Hold other calls when answering",
            "",
        );
        choice(
            ui,
            st,
            &f,
            AccountField::Mwi,
            "Voicemail indicator",
            &[
                ("subscribe", "Subscribe"),
                ("unsolicited", "Unsolicited"),
                ("off", "Off"),
            ],
        );
    });
    group(ui, p, "Pickup", |ui| {
        choice(
            ui,
            st,
            &f,
            AccountField::Pickup,
            "Pickup method",
            &[
                ("off", "Off"),
                ("prefix", "Feature code"),
                ("replaces", "Replaces"),
            ],
        );
        text(
            ui,
            st,
            &f,
            AccountField::PickupPrefix,
            "Pickup prefix",
            "**",
        );
        choice(
            ui,
            st,
            &f,
            AccountField::PickupReplacesTarget,
            "Replaces target",
            &[("caller", "Caller"), ("monitored", "Monitored")],
        );
    });
    file_only_note(ui, cx, &a.id, &f.v, out);
    remove_row(ui, cx, st, &a.id, out);
}

fn move_store(ids: &[String], store: &str) -> UiAction {
    let edits: Vec<ConfigEdit> = ids
        .iter()
        .map(|id| ConfigEdit::SetAccount {
            id: id.clone(),
            field: AccountField::PasswordStore,
            value: EditValue::Str(store.into()),
        })
        .collect();
    rpc::config_edit(RpcTag::Instant, None, &edits)
}

fn account_row(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    a: &AccountConfig,
    (pos, count): (usize, usize),
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    let status = cx.vm.snap.accounts.iter().find(|s| s.id == a.id);
    let selected = st.settings.account.as_deref() == Some(a.id.as_str());
    egui::Frame::new()
        .fill(if selected { p.accent_soft } else { p.surface })
        .stroke(egui::Stroke::new(
            1.0,
            if selected { p.accent } else { p.border },
        ))
        .corner_radius(egui::CornerRadius::same(R_LG))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if let Some(s) = status {
                    let class = reg_class(s);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    paint_lamp(
                        ui,
                        rect.center(),
                        reg_tone(p, class),
                        class != RegClass::Inactive,
                        9.0,
                    );
                }
                let r = ui.add(
                    egui::Button::new((
                        strong(&a.id, FS_BODY).color(if selected { p.accent } else { p.text }),
                        RichText::new(format!("{}@{}", a.username, a.domain))
                            .size(FS_SMALL)
                            .color(p.muted),
                    ))
                    .frame(false),
                );
                r.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Button,
                        true,
                        selected,
                        format!("Account {}", a.id),
                    )
                });
                if r.clicked() {
                    st.settings.account = Some(a.id.clone());
                }
                let transport = status
                    .and_then(|s| s.transport_effective)
                    .map_or_else(|| format!("{:?}", a.transport), |t| format!("{t:?}"))
                    .to_uppercase();
                chip(ui, p, None, mono(transport, FS_MICRO), None);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let sw = account_switch(ui, p, a.enabled, &format!("Enable {}", a.id));
                    if sw.clicked() {
                        out.push(UiAction::SetAccountEnabled(a.id.clone(), !a.enabled));
                    }
                    // Right to left: down, then up, so they read up / down / switch.
                    for (dir, delta, enabled) in [
                        (Chevron::Down, 1, pos + 1 < count),
                        (Chevron::Up, -1, pos > 0),
                    ] {
                        let name = if delta < 0 { "up" } else { "down" };
                        let label = format!("Move {} {name}", a.id);
                        let r = chevron_button(ui, p, dir, enabled)
                            .on_hover_text(format!("Move this account {name} in the list"));
                        r.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &label)
                        });
                        if enabled && r.clicked() {
                            out.push(rpc::config_edit(
                                RpcTag::Instant,
                                None,
                                &[ConfigEdit::MoveAccount {
                                    id: a.id.clone(),
                                    index: pos.saturating_add_signed(delta),
                                }],
                            ));
                        }
                    }
                });
            });
        });
}

fn identity(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, f: &F, a: &AccountConfig) {
    let p = cx.p;
    group(ui, p, "Identity", |ui| {
        text(
            ui,
            st,
            f,
            AccountField::DisplayName,
            "Display name",
            "Optional",
        );
        text(ui, st, f, AccountField::Username, "Username", "");
        text(
            ui,
            st,
            f,
            AccountField::AuthUsername,
            "Auth username",
            "Same as username",
        );
        // Password: never read back; a staged value is shown as typed.
        let staged = st
            .settings
            .passwords
            .get(f.id)
            .map(|s| s.expose().to_string())
            .unwrap_or_default();
        let hint = if a.password.expose().is_empty() && a.password_store == PasswordStore::File {
            ""
        } else {
            "unchanged"
        };
        if let Some(pw) = text_row(
            ui,
            p,
            "Password",
            &staged,
            hint,
            true,
            st.settings.account_error(f.idx, "password"),
        ) {
            st.settings.stage_password(f.id, pw, f.hash);
        }
        let keyring = matches!(shown(st, f, AccountField::PasswordStore), EditValue::Str(s) if s == "keyring");
        hint_line(
            ui,
            p,
            if keyring {
                "Stored in Keychain (the system credential store)"
            } else {
                "Stored in the config file (readable only by you)"
            },
        );
        let cur = f.cur(AccountField::PasswordStore);
        if let Some(on) = toggle_row(
            ui,
            p,
            "Store password in system keychain",
            keyring,
            "",
            None,
        ) {
            st.settings.set_field(
                f.id,
                AccountField::PasswordStore,
                &cur,
                EditValue::Str(if on { "keyring" } else { "file" }.into()),
                f.hash,
            );
        }
        text(ui, st, f, AccountField::Domain, "Domain", "pbx.example.com");
        choice(ui, st, f, AccountField::Profile, "Phone profile", PROFILES);
        let prov = &cx.vm.snap.provenance["account"][f.id]["hold_style"];
        if prov == "profile" {
            hint_line(
                ui,
                p,
                "Some values come from the vendor profile (not written to the file)",
            );
        }
    });
}

fn security(
    ui: &mut egui::Ui,
    cx: &Cx,
    st: &mut UiState,
    f: &F,
    a: &AccountConfig,
    out: &mut Vec<UiAction>,
) {
    let p = cx.p;
    group(ui, p, "Security", |ui| {
        toggle(
            ui,
            st,
            f,
            AccountField::TlsVerify,
            "Verify TLS certificate",
            "",
        );
        text(
            ui,
            st,
            f,
            AccountField::TlsFingerprint,
            "Trusted certificate",
            "SHA-256 fingerprint (empty = use the system roots)",
        );
        choice(ui, st, f, AccountField::Srtp, "SRTP", SRTP);
        // Test registration.
        ui.horizontal(|ui| {
            let dirty = st.settings.has_pending(f.id);
            let running = matches!(st.settings.tests.get(f.id), Some(TestRun::Running));
            let b = small_button(ui, p, "Test registration", !dirty && !running);
            let b = b.on_disabled_hover_text(
                "Apply or revert your changes first: the test uses the saved settings",
            );
            if b.clicked() {
                st.settings.tests.insert(f.id.to_string(), TestRun::Running);
                out.push(UiAction::Rpc(RpcRequest {
                    tag: RpcTag::TestAccount(f.id.to_string()),
                    method: "account.test",
                    params: serde_json::json!({"account": f.id}),
                }));
            }
            let share = small_button(ui, p, "Copy setup link", !dirty);
            let share = share.on_hover_text(
                "A link that fills in this account on another computer. It never contains the password.",
            );
            if share.clicked() {
                out.push(UiAction::Rpc(RpcRequest {
                    tag: RpcTag::ShareLink(f.id.to_string()),
                    method: "provision.build",
                    params: serde_json::json!({"account": f.id}),
                }));
            }
        });
        if let Some((id, note)) = &st.settings.link_note
            && id == f.id
        {
            hint_line(ui, p, note);
        }
        test_result(ui, p, st.settings.tests.get(f.id), &a.id, out);
    });
}

/// The outcome of an account test, with the actions its diagnosis offers.
pub(in crate::ui) fn test_result(
    ui: &mut egui::Ui,
    p: &Palette,
    run: Option<&TestRun>,
    account: &str,
    out: &mut Vec<UiAction>,
) {
    match run {
        None => {}
        Some(TestRun::Running) => {
            ui.label(
                RichText::new("Testing: sending a REGISTER...")
                    .size(FS_SMALL)
                    .color(p.muted),
            );
        }
        Some(TestRun::Failed(m)) => message_box(ui, p.err, m),
        Some(TestRun::Done(r)) => {
            message_box(ui, if r.ok { p.ok } else { p.err }, &r.message);
            if r.diagnosis == "tls_untrusted"
                && let Some(fp) = &r.fingerprint
            {
                ui.horizontal(|ui| {
                    if small_button(ui, p, "Trust this certificate", true).clicked() {
                        out.push(rpc::config_edit(
                            RpcTag::Instant,
                            None,
                            &[ConfigEdit::SetAccount {
                                id: account.to_string(),
                                field: AccountField::TlsFingerprint,
                                value: EditValue::Str(fp.clone()),
                            }],
                        ));
                    }
                    if small_button(ui, p, "Turn off verification (not recommended)", true)
                        .clicked()
                    {
                        out.push(rpc::config_edit(
                            RpcTag::Instant,
                            None,
                            &[ConfigEdit::SetAccount {
                                id: account.to_string(),
                                field: AccountField::TlsVerify,
                                value: EditValue::Bool(false),
                            }],
                        ));
                    }
                });
            }
        }
    }
}

fn add_form(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let errors = st.settings.add_errors.clone();
    let mut submit = false;
    let mut cancel = false;
    group(ui, p, "New account", |ui| {
        if let Some(form) = st.settings.add.as_mut() {
            draft::show(ui, p, form, &errors, "Store password in system keychain");
            ui.horizontal(|ui| {
                submit = primary_button(ui, p, "Add account", form.complete()).clicked();
                cancel = small_button(ui, p, "Cancel", true).clicked();
            });
        }
    });
    if cancel {
        st.settings.add = None;
        st.settings.add_errors.clear();
    }
    if submit && let Some(form) = &st.settings.add {
        let id = form.account_id();
        let edits = [ConfigEdit::AddAccount {
            draft: form.add_draft(),
            replace_example: false,
        }];
        out.push(rpc::config_edit(RpcTag::AddAccount, None, &edits));
        if form.keyring && !form.password.is_empty() {
            st.settings.add_pw = Some((id, siple_core::config::Secret::new(form.password.clone())));
        }
    }
}

fn file_only_note(ui: &mut egui::Ui, cx: &Cx, id: &str, v: &Value, out: &mut Vec<UiAction>) {
    let p = cx.p;
    let prov = &cx.vm.snap.provenance["account"][id];
    let set: Vec<String> = FILE_ONLY_ACCOUNT_KEYS
        .iter()
        .filter(|k| prov[**k] == "file")
        .map(|k| {
            let n = v[*k]
                .as_object()
                .map(|o| o.len())
                .or_else(|| v[*k].as_array().map(Vec::len));
            n.map_or_else(|| (*k).to_string(), |n| format!("{k} ({n})"))
        })
        .collect();
    if set.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!("Set in config file only: {}", set.join(", ")))
                .size(FS_SMALL)
                .color(p.muted),
        );
        if small_button(ui, p, "Open config", true).clicked() {
            out.push(UiAction::OpenConfig);
        }
    });
}

fn remove_row(ui: &mut egui::Ui, cx: &Cx, st: &mut UiState, id: &str, out: &mut Vec<UiAction>) {
    let p = cx.p;
    if st.settings.confirm_remove.as_deref() == Some(id) {
        message_box(
            ui,
            p.warn,
            &format!(
                "Remove {id}? Its [[account]] block (and the comments above it) is deleted from the config file."
            ),
        );
        ui.horizontal(|ui| {
            if small_button(ui, p, "Remove account", true).clicked() {
                out.push(rpc::config_edit(
                    RpcTag::Instant,
                    None,
                    &[ConfigEdit::RemoveAccount { id: id.to_string() }],
                ));
                st.settings.confirm_remove = None;
                st.settings.account = None;
                st.settings.edits.remove(id);
                st.settings.passwords.remove(id);
                st.settings.blf.remove(id);
                st.settings.touch(&cx.vm.snap.config_hash);
            }
            if small_button(ui, p, "Keep", true).clicked() {
                st.settings.confirm_remove = None;
            }
        });
    } else if small_button(ui, p, "Remove account", true).clicked() {
        st.settings.confirm_remove = Some(id.to_string());
    }
}
