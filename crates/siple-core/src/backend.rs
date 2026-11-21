//! [`AppBackend`]: the engine side of the control API.
//!
//! Implemented: `status`, `account.*` (list/get/register/reregister/unregister/set_sip_trace/
//! set_dnd), `network.refresh`,
//! `audio.devices`, `audio.set_devices`, `config.get`, `config.view`, `config.edit`,
//! `config.reload`, and the call methods of
//! [`crate::calls`] (`call.dial|answer|reject|hangup|mute|unmute|dtmf|list|get|wait_for|play|
//! stop_play|record_start|record_stop|detect_tone|expect_dtmf|sip_trace`).
//! Every state-changing result carries `event_seq`, the event-log position *before* the action,
//! so `events.poll {since: event_seq}` cannot miss its consequences.
//!
//! Subscriptions ([`crate::subscriptions`]): `blf.list`, `blf.dial`, `call.pickup`, `mwi.get`, `subscribe`, `unsubscribe`,
//! `subscriptions.list`. `status` lists a `blf` array and the `mwi` summary per account.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::methods::{AGENT_METHODS, agent_methods};
use crate::app::Inner;
use crate::audio::mic_permission;
use crate::calls::{CALL_METHODS, call_methods, sip_error as sip_err};
use crate::config::edit::ConfigEdit;
use crate::control::methods::{StreamSession, parse_params};
use crate::control::{ApiError, ControlBackend, ErrorKind, MethodInfo};
use crate::reconfig::EditFailure;
use crate::state::PartialSelection;
use crate::subscriptions::{SUBSCRIPTION_METHODS, subscription_methods};
use crate::ui_host::{UI_METHODS, ui_methods};
use crate::update::{UPDATE_METHODS, update_methods};

/// See the module docs.
pub struct AppBackend {
    pub(crate) inner: Arc<Inner>,
}

impl AppBackend {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        Self { inner }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountParam {
    account: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPasswordParam {
    account: String,
    /// The new password; "" clears it.
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestDraft {
    /// Name used in messages only; default `draft`.
    #[serde(default)]
    id: String,
    #[serde(default)]
    fields: std::collections::BTreeMap<
        crate::config::edit::AccountField,
        crate::config::edit::EditValue,
    >,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountTestParam {
    account: Option<String>,
    draft: Option<TestDraft>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SipTraceParam {
    account: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetDevicesParam {
    input: Option<String>,
    output: Option<String>,
    ringer: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EchoTestParam {
    action: Option<String>,
    seconds: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigEditParam {
    base_hash: Option<String>,
    edits: Vec<ConfigEdit>,
}

/// Maps a refused edit to an API error: `conflict` (`invalid_state`), `invalid` (`invalid_params`
/// with every `{path, message}`), `file_invalid` (`invalid_state`) or `internal`.
fn edit_error(f: EditFailure) -> ApiError {
    match &f {
        EditFailure::Conflict { current_hash } => ApiError::invalid_state(f.to_string())
            .with_extra(json!({ "reason": "conflict", "current_hash": current_hash })),
        EditFailure::Invalid { errors } => ApiError::invalid_params(f.to_string())
            .with_extra(json!({ "reason": "invalid", "errors": errors })),
        EditFailure::FileInvalid { errors } => ApiError::invalid_state(f.to_string())
            .with_extra(json!({ "reason": "file_invalid", "errors": errors })),
        EditFailure::Io(_) | EditFailure::Stopping => ApiError::internal(f.to_string()),
    }
}

impl AppBackend {
    pub(crate) fn event_seq(&self) -> u64 {
        self.inner.events.current_seq()
    }

    fn status(&self) -> Value {
        let i = &self.inner;
        let audio = json!({
            "enabled": i.audio.is_some(),
            "backend": i.audio_mode.backend,
            "disabled_reason": i.audio_mode.disabled_reason,
            "mic_permission": mic_permission(),
            "effective": i.effective_devices(),
            "active": i.audio.as_ref().map(|a| a.status()),
        });
        let speech = i.config.load().speech.clone();
        let applied_at = *i
            .applied_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let last_error = i
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        json!({
            "version": crate::update::full_version(),
            "build": crate::build::build_info(&i.channel),
            "update": i.update_summary(),
            "pid": std::process::id(),
            "headless": i.headless,
            "started_at": i.started_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "instance_id": i.events.instance_id(),
            "accounts": self.accounts_json(),
            "calls": i.stack.calls(),
            "transfers": i.stack.transfers(),
            "network": { "generation": i.stack.network_generation() },
            "active_line": i.stack.active_line(),
            "audio": audio,
            "speech": {
                "tts": !speech.tts_command.is_empty(),
                "stt": !speech.stt_command.is_empty(),
            },
            "config": {
                "path": i.config_path.display().to_string(),
                "applied_at": applied_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "last_error": last_error,
            },
            "event_seq": self.event_seq(),
        })
    }

    /// Account statuses (mwi summary included) each with its `blf` list.
    fn accounts_json(&self) -> Vec<Value> {
        let stack = &self.inner.stack;
        stack
            .accounts_status()
            .into_iter()
            .map(|s| {
                let blf = stack.blf_status(Some(&s.id));
                let mut v = serde_json::to_value(&s).unwrap_or(Value::Null);
                v["blf"] = serde_json::to_value(blf).unwrap_or(Value::Null);
                v
            })
            .collect()
    }

    fn account_get(&self, p: AccountParam) -> Result<Value, ApiError> {
        let status = self
            .inner
            .stack
            .account_status(&p.account)
            .map_err(sip_err)?;
        let mut v = serde_json::to_value(status).map_err(|e| ApiError::internal(e.to_string()))?;
        let cfg = self.inner.config.load().redacted();
        v["config"] = cfg
            .accounts
            .iter()
            .find(|a| a.id == p.account)
            .and_then(|a| serde_json::to_value(a).ok())
            .unwrap_or(Value::Null);
        Ok(v)
    }

    async fn audio_devices(&self) -> Result<Value, ApiError> {
        let inner = &self.inner;
        let (list, active) = match &inner.audio {
            Some(a) => {
                let a = a.clone();
                tokio::task::spawn_blocking(move || (a.list(), Some(a.status())))
                    .await
                    .map_err(|e| ApiError::internal(e.to_string()))?
            }
            None => (Default::default(), None),
        };
        Ok(json!({
            "inputs": list.inputs,
            "outputs": list.outputs,
            "effective": inner.effective_devices(),
            "active": active,
            "mic_permission": mic_permission(),
        }))
    }

    async fn audio_set_devices(&self, p: SetDevicesParam) -> Result<Value, ApiError> {
        let sel = PartialSelection {
            input: p.input,
            output: p.output,
            ringer: p.ringer,
        };
        if sel.is_empty() {
            return Err(ApiError::invalid_params(
                "give at least one of `input`, `output`, `ringer` (\"\" = system default)",
            ));
        }
        let seq = self.event_seq();
        let eff = self
            .inner
            .set_user_devices(&sel)
            .await
            .map_err(edit_error)?;
        Ok(json!({
            "effective": eff,
            "persisted": true,
            "event_seq": seq,
        }))
    }

    /// `audio.mic_level`.
    fn audio_mic_level(&self) -> Value {
        let level = self.inner.media.mic_level_dbfs();
        json!({ "available": level.is_some(), "level_dbfs": level })
    }

    /// `audio.test_speaker`: a 440 Hz tone for one second on the speaker.
    fn audio_test_speaker(&self) -> Result<Value, ApiError> {
        use crate::media::{LocalTone, LocalToneKind, ToneTarget};
        use siple_media::tone::{Segment, ToneSpec};
        if self.inner.audio.is_none() {
            return Err(ApiError::invalid_state("the audio layer is not running"));
        }
        self.inner.media.start_local_tone(LocalTone {
            kind: LocalToneKind::Custom(ToneSpec {
                freqs: [440.0, 0.0],
                segments: vec![Segment { on: true, ms: 1000 }],
                repeat: false,
            }),
            target: ToneTarget::Speaker,
        });
        Ok(json!({ "ok": true }))
    }

    /// `audio.echo_test`: start / cancel / status of the record-and-play-back test.
    fn audio_echo_test(&self, p: EchoTestParam) -> Result<Value, ApiError> {
        let media = &self.inner.media;
        match p.action.as_deref().unwrap_or("start") {
            "start" => {
                if self.inner.audio.is_none() {
                    return Err(ApiError::invalid_state("the audio layer is not running"));
                }
                if self
                    .inner
                    .stack
                    .calls()
                    .iter()
                    .any(|c| c.state != siple_sip::CallState::Ended)
                {
                    return Err(ApiError::invalid_state(
                        "a call is in progress; the echo test needs the microphone",
                    ));
                }
                media
                    .echo_test_start(p.seconds.unwrap_or(3))
                    .map_err(ApiError::invalid_state)?;
            }
            "cancel" => media.echo_test_stop(),
            "status" => {}
            other => {
                return Err(ApiError::invalid_params(format!(
                    "`action` must be start, cancel or status, not {other:?}"
                )));
            }
        }
        serde_json::to_value(media.echo_test_status())
            .map_err(|e| ApiError::internal(e.to_string()))
    }

    async fn config_edit(&self, p: ConfigEditParam) -> Result<Value, ApiError> {
        if p.edits.is_empty() {
            return Err(ApiError::invalid_params(
                "give at least one edit in `edits`",
            ));
        }
        let seq = self.event_seq();
        let report = self
            .inner
            .request_edit(p.edits, p.base_hash, true, "config.edit")
            .await
            .map_err(edit_error)?;
        Ok(json!({
            "applied": report.applied,
            "hash": report.hash,
            "report": report,
            "event_seq": seq,
        }))
    }

    /// `account.set_password`: the keyring entry when the account's `password_store` is
    /// `keyring` (idempotent), else the `password` key of the file (the `config.edit` path);
    /// then a forced reload so the account re-registers. The password is never echoed.
    async fn account_set_password(&self, p: SetPasswordParam) -> Result<Value, ApiError> {
        use crate::config::PasswordStore;
        let store = self
            .inner
            .config
            .load()
            .accounts
            .iter()
            .find(|a| a.id == p.account)
            .map(|a| a.password_store)
            .ok_or_else(|| ApiError::not_found(format!("unknown account {:?}", p.account)))?;
        if crate::config::edit::is_password_mask(&p.password) {
            return Err(ApiError::invalid_params(
                "that is the mask of a hidden password, not a password",
            ));
        }
        let seq = self.event_seq();
        let report = match store {
            PasswordStore::Keyring => {
                let vault = crate::config::secrets::vault();
                let stored = if p.password.is_empty() {
                    vault.delete(&p.account)
                } else {
                    vault.set(&p.account, &p.password)
                };
                stored.map_err(|e| ApiError::invalid_state(e.to_string()))?;
                self.inner
                    .request_reload(true, "account.set_password")
                    .await
            }
            PasswordStore::File => {
                let edit = ConfigEdit::SetAccount {
                    id: p.account.clone(),
                    field: crate::config::edit::AccountField::Password,
                    value: crate::config::edit::EditValue::Str(p.password),
                };
                self.inner
                    .request_edit(vec![edit], None, true, "account.set_password")
                    .await
                    .map_err(edit_error)?
            }
        };
        Ok(json!({
            "ok": true,
            "account": p.account,
            "password_store": store,
            "applied": report.applied,
            "report": report,
            "event_seq": seq,
        }))
    }

    /// `account.test`: see the method doc. The account under test is the stored one (resolved
    /// password included) or the draft; a private socket is used, running accounts are untouched.
    async fn account_test(&self, p: AccountTestParam) -> Result<Value, ApiError> {
        use crate::config::PasswordStore;
        let cfg = match (p.account, p.draft) {
            (Some(id), None) => {
                let a = self
                    .inner
                    .config
                    .load()
                    .accounts
                    .iter()
                    .find(|a| a.id == id)
                    .cloned()
                    .ok_or_else(|| ApiError::not_found(format!("unknown account {id:?}")))?;
                if a.password_store == PasswordStore::Keyring && a.password.is_empty() {
                    return Err(ApiError::invalid_state(
                        "the keyring password of this account is not available; set it with account.set_password",
                    ));
                }
                a
            }
            (None, Some(d)) => {
                let draft = crate::config::edit::AccountDraft {
                    id: d.id,
                    fields: d.fields,
                };
                crate::config::edit::account_from_draft(&draft).map_err(|issues| {
                    let errors: Vec<_> = issues
                        .iter()
                        .map(|i| json!({"path": i.path, "message": i.message}))
                        .collect();
                    ApiError::invalid_params(
                        issues
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; "),
                    )
                    .with_extra(json!({ "reason": "invalid", "errors": errors }))
                })?
            }
            _ => {
                return Err(ApiError::invalid_params(
                    "give exactly one of `account` (a configured account id) or `draft`",
                ));
            }
        };
        let secs = p.timeout_secs.unwrap_or(15);
        if !(1..=25).contains(&secs) {
            return Err(ApiError::invalid_params(
                "`timeout_secs` must be between 1 and 25",
            ));
        }
        let d = self
            .inner
            .stack
            .test_account(cfg, std::time::Duration::from_secs(secs))
            .await;
        let mut v = serde_json::to_value(&d).map_err(|e| ApiError::internal(e.to_string()))?;
        v["ok"] = d.is_ok().into();
        v["message"] = d.message().into();
        Ok(v)
    }

    async fn config_reload(&self) -> Result<Value, ApiError> {
        let seq = self.event_seq();
        let report = self.inner.request_reload(true, "config.reload").await;
        if !report.applied {
            return Err(ApiError::new(
                ErrorKind::InvalidState,
                "the config file was rejected; the previous configuration stays in effect",
            )
            .with_extra(json!({ "errors": report.errors, "event_seq": seq })));
        }
        let mut v = serde_json::to_value(&report).map_err(|e| ApiError::internal(e.to_string()))?;
        v["event_seq"] = seq.into();
        Ok(v)
    }

    /// Placeholder for the call API. Returning `method_not_found` keeps unwired methods out of
    /// `methods.list` and gives callers a clear message.
    ///
    fn unwired(method: &str) -> Option<ApiError> {
        let planned = method.starts_with("call.") || matches!(method, "account.set_dnd");
        planned.then(|| {
            ApiError::new(
                ErrorKind::MethodNotFound,
                format!("`{method}` is not implemented yet"),
            )
        })
    }
}

const CONFIG_EDIT_DOC: &str = "Changes config.toml with typed edits, then reloads it (the same path as an external edit: hot options apply at once, account changes re-register or rebuild as needed). Comments, key order and layout of the file are preserved; only the named keys change. All edits apply together or none do. Params {base_hash?, edits[]}: with `base_hash` (the `hash` of `config.view`) the call fails with invalid_state, data.reason `conflict` and data.current_hash when the file changed since it was read. Result {applied, hash, report, event_seq}: `hash` is the new base_hash, `report` the reload report {applied, changed, summary, errors}. Errors: invalid_params with data.reason `invalid` and data.errors [{path, message}] when an edit is wrong or the edited file would not validate (nothing is written); invalid_state with data.reason `file_invalid` when the file on disk does not parse (edits are refused until it is fixed; a file that parses but fails validation can still be edited, as long as the edited result validates, so an edit can repair it, e.g. disable the bad account). A read-only file is refused with internal. An absent key whose new value equals the default is not written. Edit objects, each with an `op`: {op:set_account, id, field, value} | {op:reset_account, id, field} (removes the key) | {op:add_account, draft:{id, fields:{field: value}}, replace_example?:bool} (`replace_example` replaces the shipped, untouched `example` account) | {op:remove_account, id} | {op:move_account, id, index} (moves the account to 0-based position `index` in the account list, which is the display order; an index past the end means last; comments above it move with it) | {op:rename_account, from, to} | {op:set_blf, account, entries:[{uri, label, group?, pickup?, pickup_prefix?, pickup_target?}]} (replaces the list; entries matched by uri keep their comments; an empty optional key, or pickup \"\" for inherit, is removed from the file) | {op:set_section, section, key, value} | {op:reset_section, section, key}. Account fields: enabled, profile, display_name, username, auth_username, password, password_store (file|keyring: with keyring the password lives in the system credential store and `password` must be empty; switching moves the existing password along), domain, registrar, outbound_proxy, transport (udp|tcp|tls|auto: SRV _sips._tcp > _sip._tcp > _sip._udp, else UDP 5060), local_port, tls_verify, tls_fingerprint, srtp (off|optional|mandatory), dnd, auto_answer, answer_delay_ms, allow_intercom, auto_hold, mwi (subscribe|unsolicited|off), sip_trace, pickup (off|prefix|replaces), pickup_prefix, pickup_replaces_target (caller|monitored). Sections: calls, ui (also `shortcuts.<action>`), network, logging, audio, general (only user_agent and log_level; log_dir, log_retention_days and reconfig_force are file-only), updates (check, channel: stable|prerelease; `reset_section` channel restores the automatic choice); `config.view` lists their keys under `editable`. Values are plain booleans, integers or strings, checked per field; unknown sections and keys are rejected. Everything else (extra_headers, [media], [speech], [control], ...) stays file-only.";

fn config_edit_schema() -> Value {
    let value = json!({"description":"A boolean, integer or string, as the field requires.","type":["boolean","integer","string"]});
    let id = json!({"type":"string","description":"Account id."});
    let field = json!({"type":"string","enum": crate::config::edit::AccountField::ALL.map(crate::config::edit::AccountField::key)});
    let section = json!({"type":"string","enum": crate::config::edit::editable_sections()});
    let op = |name: &str, props: Value, required: Vec<&str>| {
        let mut p = props;
        p["op"] = json!({"const": name});
        let mut req = vec!["op"];
        req.extend(required);
        json!({"type":"object","properties":p,"required":req,"additionalProperties":false})
    };
    json!({"type":"object","properties":{
        "base_hash":{"type":"string","description":"`hash` from config.view; the edit fails with a conflict if the file changed since."},
        "edits":{"type":"array","minItems":1,"items":{"oneOf":[
            op("set_account", json!({"id":id,"field":field,"value":value}), vec!["id","field","value"]),
            op("reset_account", json!({"id":id,"field":field}), vec!["id","field"]),
            op("add_account", json!({"draft":{"type":"object","properties":{"id":id,"fields":{"type":"object","description":"field name -> value","additionalProperties":value}},"required":["id"],"additionalProperties":false},"replace_example":{"type":"boolean"}}), vec!["draft"]),
            op("remove_account", json!({"id":id}), vec!["id"]),
            op("move_account", json!({"id":id,"index":{"type":"integer","minimum":0,"description":"0-based target position; past the end means last"}}), vec!["id","index"]),
            op("rename_account", json!({"from":id,"to":{"type":"string"}}), vec!["from","to"]),
            op("set_blf", json!({"account":id,"entries":{"type":"array","items":{"type":"object","properties":{"uri":{"type":"string"},"label":{"type":"string"}},"required":["uri"],"additionalProperties":false}}}), vec!["account","entries"]),
            op("set_section", json!({"section":section,"key":{"type":"string"},"value":value}), vec!["section","key","value"]),
            op("reset_section", json!({"section":section,"key":{"type":"string"}}), vec!["section","key"]),
        ]}}
    },"required":["edits"],"additionalProperties":false})
}

/// The methods [`AppBackend`] serves (everything but the control server's built-ins), with
/// their schemas. The single source for `methods.list` and for [`method_catalog`].
pub fn backend_methods() -> Vec<MethodInfo> {
    let none = || json!({"type":"object","properties":{},"additionalProperties":false});
    let account = |extra: Value| {
        let mut props =
            json!({"account":{"type":"string","description":"Account id from the config."}});
        let mut required = vec![json!("account")];
        if let Value::Object(m) = extra {
            for (k, v) in m {
                props[&k] = v;
                required.push(json!(k));
            }
        }
        json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
    };
    let device = |what: &str| json!({"type":"string","description":format!("{what} device name; \"\" = system default.")});
    vec![
            MethodInfo::new(
                "status",
                "Overall state: instance_id and started_at (a changed instance_id means a restart: event seqs and call ids reset), accounts (registration, dnd, mwi summary, blf list, pending_rebuild), calls, active_line (id of the un-held device-bound call that owns the microphone and speaker, or null), network {generation: count of network changes seen}, audio (effective devices from [audio] with source config|default, active devices, mic_permission), build {channel: direct|mas|msix, sandboxed, store: a store edition (compiled without the GitHub update source), version}, speech {tts, stt: whether [speech] tts_command / stt_command are set}, config (path, applied_at, last_error), update {enabled, current, available, latest, release_url, source, action, store_url, install_state: the update check's summary, see app.update_status}, event_seq.",
                none(),
            ),
            MethodInfo::new("account.list", "Status of all accounts.", none()),
            MethodInfo::new(
                "account.get",
                "Status and redacted config of one account (status includes `srtp_optional_downgraded`: a list of next-hop destinations `host:port[:transport]` that refused an `srtp = \"optional\"` offer and then answered the plain RTP/AVP retry with a final 2xx; calls to them offer plain RTP/AVP first). Errors: not_found.",
                account(json!({})),
            ),
            MethodInfo::new(
                "account.register",
                "Register now (also re-enables registration after account.unregister). Returns event_seq; follow with events.poll for `account.registration`.",
                account(json!({})),
            ),
            MethodInfo::new(
                "account.unregister",
                "Remove our registration binding and stop refreshing until account.register.",
                account(json!({})),
            ),
            MethodInfo::new(
                "account.reregister",
                "Re-register one account immediately: re-probe the route and next hop, redial a TCP/TLS connection, reset the back-off and send REGISTER now (also re-enables registration after account.unregister; leaves `waiting_for_network` if a route exists). Emits `account.reconnecting`, then `account.registration`. Established calls on a moved local address are re-INVITEd onto the new path (`call.reconnecting` / `call.reconnected`, or `call.state` ended with cause `network_lost`; see [network] move_calls). Returns event_seq.",
                account(json!({})),
            ),
            MethodInfo::new(
                "network.refresh",
                "Force a network-change signal now (no debounce): every account re-probes its route and re-registers at once, as after a Wi-Fi switch or wake from sleep. Returns {generation, event_seq}; emits `network.changed` and one `account.reconnecting` per registering account. When an account's local address changed, its established calls are moved with a re-INVITE (events `call.reconnecting` then `call.reconnected`; a call the peer does not follow ends with cause `network_lost`, as does every established call when `[network] move_calls = false`).",
                none(),
            ),
            MethodInfo::new(
                "account.set_sip_trace",
                "Turn the SIP wire trace of an account on/off at runtime (lasts until its `sip_trace` config value changes).",
                account(json!({"enabled":{"type":"boolean"}})),
            ),
            MethodInfo::new(
                "account.set_password",
                "Set (or, with an empty `password`, clear) the password of an account without echoing it. Where it is written follows the account's `password_store`: `keyring` -> the system credential store (service `io.zyno.siple`, entry `account:<id>`; idempotent; fails with invalid_state `keyring unavailable; set password_store = \"file\"` when there is none), `file` -> the `password` key of config.toml (same typed edit as `config.edit`, comments preserved). Then the config is reloaded at once so the account re-registers with the new password. Result {ok, account, password_store, applied, report, event_seq}; `report` is the reload report (account errors, if any, are listed in `report.errors`). To move an account between stores, `config.edit` set_account `password_store`: the existing password moves with it.",
                account(json!({"password":{"type":"string","description":"New password; \"\" clears it."}})),
            ),
            MethodInfo::new(
                "account.test",
                "Try an account's settings with a one-off REGISTER (then unREGISTER) on a private socket, without disturbing any running account, and say what is wrong. Give exactly one of `account` (a configured account id, its stored settings and password) or `draft` ({id?, fields:{field: value}}: the same fields and checks as `config.edit` `add_account`, e.g. username, password, domain, transport, tls_verify; nothing is saved). `timeout_secs` (1-25, default 15) bounds the whole test. Result: `diagnosis` is one of `ok` {transport (what `auto` picked), auto, server, granted_secs, elapsed_ms} | `auth_failed` {code: 401/403/407 after the digest answer} | `not_found` (404) | `timeout` {transport, hint: with UDP, try TCP or TLS} | `tls_untrusted` {fingerprint: SHA-256 of the server certificate, lowercase hex, reason}: to trust that server, write it with `config.edit` set_account `tls_fingerprint` | `dns_failed` {message} | `network_unreachable` {message} | `other` {code (0 = local failure), reason}; plus `ok` (boolean) and `message` (one line for a user). The password is never echoed. A registrar that allows one binding per address of record may briefly drop the running account's binding; the account's next refresh restores it. A draft whose fields are invalid fails with invalid_params and data.errors [{path, message}].",
                json!({"type":"object","properties":{
                    "account":{"type":"string","description":"Configured account id."},
                    "draft":{"type":"object","properties":{
                        "id":{"type":"string"},
                        "fields":{"type":"object","description":"field name -> value (as in config.edit add_account)","additionalProperties":{"description":"A boolean, integer or string, as the field requires.","type":["boolean","integer","string"]}}
                    },"additionalProperties":false},
                    "timeout_secs":{"type":"integer","minimum":1,"maximum":25,"description":"Total time limit; default 15."}},
                    "oneOf":[{"required":["account"]},{"required":["draft"]}],"additionalProperties":false}),
            ),
            MethodInfo::new(
                "audio.devices",
                "Available input/output devices, the effective selection (with source) and the active devices.",
                none(),
            ),
            MethodInfo::new(
                "audio.set_devices",
                "Choose microphone / speaker / ringer (at least one). Written to `[audio] input/output/ringer` of config.toml (the same typed edit as `config.edit`, comments preserved) and applied at once; fails like `config.edit` when the config file is invalid. Returns the effective devices.",
                json!({"type":"object","properties":{
                    "input":device("Microphone"),"output":device("Speaker"),"ringer":device("Ringer")},
                    "minProperties":1,"additionalProperties":false}),
            ),
            MethodInfo::new(
                "audio.mic_level",
                "Live microphone level: RMS of the latest capture block in dBFS (-120 = silence), measured whether or not a call is up. Result {available, level_dbfs}; `available` is false (and the level null) while no microphone is open. Poll it for a level meter.",
                none(),
            ),
            MethodInfo::new(
                "audio.test_speaker",
                "Play a 440 Hz tone for one second on the speaker. Fails with invalid_state when the audio layer is not running.",
                none(),
            ),
            MethodInfo::new(
                "audio.echo_test",
                "Echo test of the microphone and speaker: record the microphone for `seconds` (1-10, default 3), then play the recording back on the speaker. `action` is `start` (default; fails with invalid_state while a test runs, while a call is in progress or without audio), `cancel` or `status`. Result {phase: idle|recording|playing|done|failed, elapsed_ms, total_ms (recording plus playback), peak_dbfs (once recording ended), silent (nothing audible was recorded: peak below -60 dBFS), error}. Poll with `status` until `done`.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["start","cancel","status"]},
                    "seconds":{"type":"integer","minimum":1,"maximum":10}},
                    "additionalProperties":false}),
            ),
            MethodInfo::new(
                "config.get",
                "The effective configuration (passwords redacted).",
                none(),
            ),
            MethodInfo::new(
                "config.view",
                "The effective configuration with the origin of every key, for building a settings UI or checking what an agent may change. Returns {path, hash, accepted_hash, config, provenance, secrets, editable}. `hash` is the content hash of the config file as it is on disk now: pass it as `config.edit`'s `base_hash` (it also works while the file on disk is invalid and was not accepted, so an edit can repair it). `accepted_hash` is the hash of the text the running configuration came from. `config` and `provenance` describe the accepted configuration. `config` is the effective config (vendor profiles applied) as JSON with the same sections and keys as config.toml (`account` is a list). Passwords are never returned: each account's `password` is `{\"set\": true|false}` and `secrets.control_token` is `{\"set\": true}`; such a value is not a valid input to `config.edit`. `provenance` has, per section, `{key: \"file\"|\"default\"}` and, per account id under `provenance.account`, `{key: \"file\"|\"profile\"|\"default\"}`: `file` = the key is set in config.toml, `profile` = the vendor profile filled a key the file omits, `default` = built-in default. `editable` lists the account fields and the sections/keys `config.edit` accepts.",
                none(),
            ),
            MethodInfo::new(
                "config.edit",
                CONFIG_EDIT_DOC,
                config_edit_schema(),
            ),
            MethodInfo::new(
                "config.reload",
                "Re-read the config file now and apply the differences (same path as the file watcher). Returns {applied, changed, summary, errors}; an invalid file fails with invalid_state and the previous config stays.",
                none(),
            ),
        ]
        .into_iter()
        .chain(call_methods())
        .chain(subscription_methods())
        .chain(agent_methods())
        .chain(ui_methods())
        .chain(crate::provision::provision_methods())
        .chain(update_methods())
        .chain(std::iter::once(crate::agent::stream::method_info()))
        .collect()
}

/// Every method a running instance offers (`methods.list`): the control server's built-ins plus
/// [`backend_methods`]. Needs no instance, so the MCP bridge can list tools while Siple is
/// not running; it cannot drift from the live list because both come from here.
pub fn method_catalog() -> Vec<MethodInfo> {
    let mut m = crate::control::methods::builtin_methods();
    m.extend(backend_methods());
    m
}

#[async_trait]
impl ControlBackend for AppBackend {
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let i = &self.inner;
        match method {
            "status" => {
                let _: Value = params;
                Ok(self.status())
            }
            "account.list" => Ok(json!({ "accounts": i.stack.accounts_status() })),
            "account.get" => self.account_get(parse_params(params)?),
            "account.register" => {
                let p: AccountParam = parse_params(params)?;
                let seq = self.event_seq();
                i.stack.register_now(&p.account).map_err(sip_err)?;
                Ok(json!({ "ok": true, "event_seq": seq }))
            }
            "account.unregister" => {
                let p: AccountParam = parse_params(params)?;
                let seq = self.event_seq();
                i.stack.unregister(&p.account).map_err(sip_err)?;
                Ok(json!({ "ok": true, "event_seq": seq }))
            }
            "account.reregister" => {
                let p: AccountParam = parse_params(params)?;
                let seq = self.event_seq();
                i.stack.reregister(&p.account).map_err(sip_err)?;
                Ok(json!({ "ok": true, "event_seq": seq }))
            }
            "network.refresh" => {
                let _: Value = params;
                let seq = self.event_seq();
                let generation = i.stack.network_refresh().await;
                Ok(json!({ "generation": generation, "event_seq": seq }))
            }
            "account.test" => self.account_test(parse_params(params)?).await,
            "account.set_password" => self.account_set_password(parse_params(params)?).await,
            "account.set_sip_trace" => {
                let p: SipTraceParam = parse_params(params)?;
                let seq = self.event_seq();
                i.stack
                    .set_sip_trace(&p.account, p.enabled)
                    .map_err(sip_err)?;
                Ok(json!({ "ok": true, "enabled": p.enabled, "event_seq": seq }))
            }
            "audio.devices" => self.audio_devices().await,
            "audio.set_devices" => self.audio_set_devices(parse_params(params)?).await,
            "audio.mic_level" => {
                let _: Value = params;
                Ok(self.audio_mic_level())
            }
            "audio.test_speaker" => {
                let _: Value = params;
                self.audio_test_speaker()
            }
            "audio.echo_test" => self.audio_echo_test(parse_params(params)?),
            "config.get" => Ok(json!({
                "path": i.config_path.display().to_string(),
                "config": i.config.load().redacted(),
            })),
            "config.view" => {
                let _: Value = params;
                Ok(i.config_view())
            }
            "config.edit" => self.config_edit(parse_params(params)?).await,
            "config.reload" => self.config_reload().await,
            m if CALL_METHODS.contains(&m) => self.call_method(m, params).await,
            m if SUBSCRIPTION_METHODS.contains(&m) => self.subscription_method(m, params).await,
            m if AGENT_METHODS.contains(&m) => self.agent_method(m, params).await,
            m if UI_METHODS.contains(&m) => self.ui_method(m, params).await,
            m if crate::provision::PROVISION_METHODS.contains(&m) => {
                self.provision_method(m, params).await
            }
            m if UPDATE_METHODS.contains(&m) => self.update_method(m, params).await,
            // Needs a connection upgrade, which only the control server can do.
            "call.stream" => Err(ApiError::invalid_state(
                "`call.stream` upgrades the connection to binary; use `siple-cli stream` \
                 or open_stream from a control client",
            )),
            other => Err(Self::unwired(other).unwrap_or_else(|| ApiError::method_not_found(other))),
        }
    }

    fn methods(&self) -> Vec<MethodInfo> {
        backend_methods()
    }

    async fn open_stream(
        &self,
        params: Value,
    ) -> Result<(Value, Box<dyn StreamSession>), ApiError> {
        crate::agent::stream::open_stream(&self.inner, params).await
    }
}
