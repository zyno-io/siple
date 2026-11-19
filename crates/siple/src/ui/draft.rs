//! The form of a new account (Settings "Add account" and the first-run wizard).

use std::collections::BTreeMap;

use eframe::egui;
use siple_core::config::edit::{AccountDraft, AccountField, EditValue};

use super::form::{choice_row, hint_line, text_row, toggle_row};
use crate::theme::Palette;
use crate::widgets::disclosure;

/// Transport choices (`value`, label).
pub(crate) const TRANSPORTS: &[(&str, &str)] = &[
    ("auto", "Auto (SRV lookup, else UDP)"),
    ("udp", "UDP"),
    ("tcp", "TCP"),
    ("tls", "TLS"),
];
pub(crate) const SRTP: &[(&str, &str)] = &[
    ("optional", "Optional"),
    ("off", "Off"),
    ("mandatory", "Mandatory"),
];
pub(crate) const PROFILES: &[(&str, &str)] = &[
    ("generic", "Generic"),
    ("yealink", "Yealink"),
    ("polycom", "Polycom"),
    ("cisco", "Cisco"),
    ("grandstream", "Grandstream"),
];

/// What the user typed for a new account.
#[derive(Clone, PartialEq)]
pub struct DraftForm {
    pub display_name: String,
    pub username: String,
    pub password: String,
    pub domain: String,
    /// "Store in system keychain" (unticked by default).
    pub keyring: bool,
    /// The advanced block is open.
    pub advanced: bool,
    pub auth_username: String,
    pub outbound_proxy: String,
    pub transport: String,
    pub tls_verify: bool,
    pub srtp: String,
    pub profile: String,
    /// A certificate fingerprint the user chose to trust (from a failed test).
    pub tls_fingerprint: String,
}

impl std::fmt::Debug for DraftForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DraftForm")
            .field("username", &self.username)
            .field("domain", &self.domain)
            .field("password", &"<hidden>")
            .finish_non_exhaustive()
    }
}

/// Keys the form shows an error for.
const SHOWN_KEYS: &[&str] = &[
    "username",
    "password",
    "domain",
    "auth_username",
    "outbound_proxy",
    "transport",
    "srtp",
    "profile",
];

/// Errors of keys the form has no field for (a duplicate id, ...), as one-line texts.
pub(crate) fn unshown(errors: &[(String, String)]) -> Vec<String> {
    errors
        .iter()
        .filter(|(p, _)| !SHOWN_KEYS.iter().any(|k| p.ends_with(&format!(".{k}"))))
        .map(|(p, m)| {
            if p.is_empty() {
                m.clone()
            } else {
                format!("{p}: {m}")
            }
        })
        .collect()
}

impl Default for DraftForm {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            username: String::new(),
            password: String::new(),
            domain: String::new(),
            keyring: false,
            advanced: false,
            auth_username: String::new(),
            outbound_proxy: String::new(),
            transport: "auto".into(),
            tls_verify: true,
            srtp: "optional".into(),
            profile: "generic".into(),
            tls_fingerprint: String::new(),
        }
    }
}

impl DraftForm {
    /// Enough to try: a username and a domain.
    pub fn complete(&self) -> bool {
        !self.username.trim().is_empty() && !self.domain.trim().is_empty()
    }

    /// Account id for the new `[[account]]`: `<username>.<domain>` limited to `[A-Za-z0-9_.-]`.
    pub fn account_id(&self) -> String {
        let slug = |s: &str| {
            s.trim()
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
        };
        format!("{}.{}", slug(&self.username), slug(&self.domain))
    }

    fn fields(&self) -> BTreeMap<AccountField, EditValue> {
        let mut m = BTreeMap::new();
        let mut s = |f: AccountField, v: &str| {
            let v = v.trim();
            if !v.is_empty() {
                m.insert(f, EditValue::Str(v.to_string()));
            }
        };
        s(AccountField::DisplayName, &self.display_name);
        s(AccountField::Username, &self.username);
        s(AccountField::Domain, &self.domain);
        s(AccountField::AuthUsername, &self.auth_username);
        s(AccountField::OutboundProxy, &self.outbound_proxy);
        s(AccountField::Transport, &self.transport);
        s(AccountField::Srtp, &self.srtp);
        s(AccountField::Profile, &self.profile);
        s(AccountField::TlsFingerprint, &self.tls_fingerprint);
        if !self.tls_verify {
            m.insert(AccountField::TlsVerify, EditValue::Bool(false));
        }
        m
    }

    /// The draft `account.test` tries (the password as typed).
    pub fn test_draft(&self) -> AccountDraft {
        let mut fields = self.fields();
        if !self.password.is_empty() {
            fields.insert(
                AccountField::Password,
                EditValue::Str(self.password.clone()),
            );
        }
        AccountDraft {
            id: self.account_id(),
            fields,
        }
    }

    /// The draft `add_account` writes: with the keychain option the password stays out of the
    /// file (it is stored with `account.set_password` afterwards).
    pub fn add_draft(&self) -> AccountDraft {
        let mut fields = self.fields();
        if self.keyring {
            fields.insert(
                AccountField::PasswordStore,
                EditValue::Str("keyring".into()),
            );
        } else if !self.password.is_empty() {
            fields.insert(
                AccountField::Password,
                EditValue::Str(self.password.clone()),
            );
        }
        AccountDraft {
            id: self.account_id(),
            fields,
        }
    }
}

/// The error of the field `key` among `config.edit` / `account.test` problems.
pub(crate) fn field_error<'a>(errors: &'a [(String, String)], key: &str) -> Option<&'a str> {
    let suffix = format!(".{key}");
    errors
        .iter()
        .find(|(p, _)| p.ends_with(&suffix) || p == key)
        .map(|(_, m)| m.as_str())
}

/// Draws the form. `errors` are the problems the engine reported for the last try.
pub(crate) fn show(
    ui: &mut egui::Ui,
    p: &Palette,
    f: &mut DraftForm,
    errors: &[(String, String)],
    keyring_label: &str,
) {
    if let Some(v) = text_row(
        ui,
        p,
        "Display name",
        &f.display_name,
        "Optional",
        false,
        None,
    ) {
        f.display_name = v;
    }
    if let Some(v) = text_row(
        ui,
        p,
        "Username / extension",
        &f.username,
        "1001",
        false,
        field_error(errors, "username"),
    ) {
        f.username = v;
    }
    if let Some(v) = text_row(
        ui,
        p,
        "Password",
        &f.password,
        "",
        true,
        field_error(errors, "password"),
    ) {
        f.password = v;
    }
    if let Some(v) = toggle_row(ui, p, keyring_label, f.keyring, "", None) {
        f.keyring = v;
    }
    if let Some(v) = text_row(
        ui,
        p,
        "Domain / server",
        &f.domain,
        "pbx.example.com",
        false,
        field_error(errors, "domain"),
    ) {
        f.domain = v;
    }
    let head = disclosure(ui, p, f.advanced, "Advanced options");
    if head.clicked() {
        f.advanced = !f.advanced;
    }
    if !f.advanced {
        return;
    }
    if let Some(v) = text_row(
        ui,
        p,
        "Auth username",
        &f.auth_username,
        "Same as username",
        false,
        field_error(errors, "auth_username"),
    ) {
        f.auth_username = v;
    }
    if let Some(v) = text_row(
        ui,
        p,
        "Outbound proxy",
        &f.outbound_proxy,
        "Optional",
        false,
        field_error(errors, "outbound_proxy"),
    ) {
        f.outbound_proxy = v;
    }
    if let Some(v) = choice_row(
        ui,
        p,
        "Transport",
        &f.transport,
        TRANSPORTS,
        field_error(errors, "transport"),
    ) {
        f.transport = v;
    }
    if let Some(v) = toggle_row(ui, p, "Verify TLS certificate", f.tls_verify, "", None) {
        f.tls_verify = v;
    }
    if let Some(v) = choice_row(ui, p, "SRTP", &f.srtp, SRTP, field_error(errors, "srtp")) {
        f.srtp = v;
    }
    if let Some(v) = choice_row(
        ui,
        p,
        "Phone profile",
        &f.profile,
        PROFILES,
        field_error(errors, "profile"),
    ) {
        f.profile = v;
    }
    if !f.tls_fingerprint.is_empty() {
        hint_line(
            ui,
            p,
            &format!("Trusting the server certificate {}", f.tls_fingerprint),
        );
    }
}
