//! In-process control-API calls requested by the views (Settings, wizard).
//!
//! The views only describe a call as an [`RpcRequest`]; [`super::PhoneUi`] runs it with
//! `App::call` (the same dispatch a control client reaches) and feeds the [`RpcReply`] back, so
//! the GUI has no capability an agent lacks and the views stay testable without an engine.

use serde_json::{Value, json};
use siple_core::config::edit::{ConfigEdit, EditValue};

use super::UiAction;

/// What a reply is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcTag {
    /// The batched Apply of the account / extensions forms.
    Apply,
    /// A toggle or picker that applies at once.
    Instant,
    /// `account.test` of a stored account.
    TestAccount(String),
    /// `account.test` of the wizard's draft.
    TestDraft,
    /// `config.edit` add_account of the Settings add form.
    AddAccount,
    /// `config.edit` add_account of the wizard.
    WizardAdd,
    /// `account.set_password` (keyring accounts) for the account.
    SetPassword(String),
    /// `audio.echo_test`.
    Echo,
    /// `app.check_updates` ("Check for updates" in Settings).
    CheckUpdates,
    /// `app.install_update` (the update pill, Settings).
    InstallUpdate,
    /// `app.cancel_update`.
    CancelUpdate,
}

/// A call to a control-API method.
#[derive(Clone, PartialEq)]
pub struct RpcRequest {
    pub tag: RpcTag,
    pub method: &'static str,
    pub params: Value,
}

/// The parameters can carry a password, so they are never printed.
impl std::fmt::Debug for RpcRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcRequest")
            .field("tag", &self.tag)
            .field("method", &self.method)
            .field("params", &"<hidden>")
            .finish()
    }
}

/// A failed call, as the control API reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    /// `invalid_params`, `invalid_state`, ...
    pub kind: String,
    pub message: String,
    /// `error.data` (`reason`, `errors`, `current_hash`).
    pub data: Value,
}

impl RpcError {
    #[cfg(test)]
    pub fn new(kind: &str, message: &str, data: Value) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            data,
        }
    }

    pub fn from_api(e: &siple_core::control::ApiError) -> Self {
        Self {
            kind: e.kind.as_str().into(),
            message: e.message.clone(),
            data: e.extra.clone().unwrap_or(Value::Null),
        }
    }

    /// `data.reason`: `conflict`, `invalid` or `file_invalid` for `config.edit`.
    pub fn reason(&self) -> Option<&str> {
        self.data["reason"].as_str()
    }

    /// `data.errors[]` as `(path, message)`.
    pub fn field_errors(&self) -> Vec<(String, String)> {
        self.data["errors"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| match e.as_str() {
                        // Unreadable-file errors are plain strings.
                        Some(msg) => (String::new(), msg.to_string()),
                        None => (
                            e["path"].as_str().unwrap_or_default().to_string(),
                            e["message"].as_str().unwrap_or_default().to_string(),
                        ),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The answer to an [`RpcRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct RpcReply {
    pub tag: RpcTag,
    pub result: Result<Value, RpcError>,
}

/// `config.edit` with `edits` (and `base_hash` when the form was read from a known file state).
pub fn config_edit(tag: RpcTag, base_hash: Option<&str>, edits: &[ConfigEdit]) -> UiAction {
    let mut params = json!({ "edits": edits });
    if let Some(h) = base_hash {
        params["base_hash"] = h.into();
    }
    UiAction::Rpc(RpcRequest {
        tag,
        method: "config.edit",
        params,
    })
}

/// An instant `config.edit` of one section key (`[ui]`, `[calls]`, `[logging]`, ...).
pub fn set_section(section: &str, key: &str, value: EditValue) -> UiAction {
    config_edit(
        RpcTag::Instant,
        None,
        &[ConfigEdit::SetSection {
            section: section.into(),
            key: key.into(),
            value,
        }],
    )
}

/// `app.check_updates`.
pub fn check_updates() -> UiAction {
    UiAction::Rpc(RpcRequest {
        tag: RpcTag::CheckUpdates,
        method: "app.check_updates",
        params: json!({}),
    })
}

/// `app.install_update`: `restart` is `when_idle`, `now` or `manual`.
pub fn install_update(version: &str, restart: &'static str) -> UiAction {
    UiAction::Rpc(RpcRequest {
        tag: RpcTag::InstallUpdate,
        method: "app.install_update",
        params: json!({"version": version, "restart": restart}),
    })
}

/// `app.cancel_update`.
pub fn cancel_update() -> UiAction {
    UiAction::Rpc(RpcRequest {
        tag: RpcTag::CancelUpdate,
        method: "app.cancel_update",
        params: json!({}),
    })
}

/// The edits carried by a `config.edit` request.
#[cfg(test)]
pub fn edits_of(req: &RpcRequest) -> Vec<ConfigEdit> {
    serde_json::from_value(req.params["edits"].clone()).unwrap_or_default()
}

/// What `account.test` found.
#[derive(Debug, Clone, PartialEq)]
pub struct TestResult {
    /// `ok`, `auth_failed`, `not_found`, `timeout`, `tls_untrusted`, `dns_failed`,
    /// `network_unreachable`, `other`.
    pub diagnosis: String,
    pub ok: bool,
    /// One line for a user.
    pub message: String,
    /// SHA-256 of the server certificate (`tls_untrusted`).
    pub fingerprint: Option<String>,
}

impl TestResult {
    pub fn from_value(v: &Value) -> Self {
        let d = &v["diagnosis"];
        let kind = d
            .as_str()
            .or_else(|| d["kind"].as_str())
            .or_else(|| d["diagnosis"].as_str())
            .unwrap_or("other");
        Self {
            diagnosis: kind.to_string(),
            ok: v["ok"].as_bool().unwrap_or(false),
            message: v["message"].as_str().unwrap_or_default().to_string(),
            fingerprint: d["fingerprint"]
                .as_str()
                .or_else(|| v["fingerprint"].as_str())
                .map(str::to_string),
        }
    }
}

/// An account test in flight or finished.
#[derive(Debug, Clone, PartialEq)]
pub enum TestRun {
    Running,
    Done(TestResult),
    /// The call itself failed (invalid draft, keyring unavailable, ...).
    Failed(String),
}
