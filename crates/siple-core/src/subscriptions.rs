//! Subscription methods of the control API: `blf.list`, `blf.dial`, `call.pickup`,
//! `mwi.get`, `subscribe`, `unsubscribe`, `subscriptions.list`. The matching events (`blf.state`, `account.mwi`,
//! `subscription.notify`, `subscription.state`) are mapped in [`crate::app::sip_event_to_wire`].

use serde::Deserialize;
use serde_json::{Value, json};

use siple_sip::DialOptions;

use crate::backend::AppBackend;
use crate::calls::sip_error;
use crate::control::methods::parse_params;
use crate::control::{ApiError, MethodInfo};

/// Methods handled by [`AppBackend::subscription_method`].
pub const SUBSCRIPTION_METHODS: &[&str] = &[
    "blf.list",
    "blf.dial",
    "call.pickup",
    "mwi.get",
    "subscribe",
    "unsubscribe",
    "subscriptions.list",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptAccount {
    account: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlfCallParam {
    account: String,
    uri: String,
    source: Option<String>,
    sink: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscribeParam {
    account: String,
    uri: String,
    event: String,
    accept: Option<String>,
    expires: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnsubscribeParam {
    sub_id: String,
}

/// Schemas and descriptions of [`SUBSCRIPTION_METHODS`].
pub fn subscription_methods() -> Vec<MethodInfo> {
    let account_opt = |what: &str| {
        json!({"type":"object","properties":{
            "account":{"type":"string","description":what}},
            "additionalProperties":false})
    };
    let blf_call_schema = || {
        json!({"type":"object","properties":{
            "account":{"type":"string","description":"Account id the BLF entry belongs to."},
            "uri":{"type":"string","description":"The BLF entry's `uri` exactly as configured / listed by blf.list."},
            "source":{"type":"string","description":"Audio source: device | silence | tone:<hz> | file:<path.wav>. Default: the account's audio_source."},
            "sink":{"type":"string","description":"Audio sink: device | null | record:<path.wav>. Default: the account's audio_sink."}},
            "required":["account","uri"],"additionalProperties":false})
    };
    vec![
        MethodInfo::new(
            "blf.list",
            "Busy-lamp-field state of the monitored extensions (`[[account.blf]]`): per entry uri, label, group, state (idle|ringing|dialing|busy|held|unknown|failed), the dialogs behind it (state, direction, remote identity, call-id, tags) and updated_at. Also `pickup` (effective directed-pickup method: off|prefix|replaces; pickup is offered while state is ringing and this is not off), `subscription` {phase: pending|active|retrying|failed, last_code, last_reason, expires_at, retry_at} (the health of the SUBSCRIBE behind the entry; `failed` = refused for good, `retrying` = another attempt is scheduled at retry_at) and `stale` (true while retrying, or when no NOTIFY / refresh arrived within 30 s after expires_at: the state shown is the last known one and may be out of date). Omit `account` for all accounts. Errors: not_found.",
            account_opt("Account id; default all accounts."),
        ),
        MethodInfo::new(
            "blf.dial",
            "Call a monitored BLF entry on its account: what a click on the key does. Same as call.dial {account, target: uri} but the entry must exist in `[[account.blf]]`. Returns {call_id, call, event_seq} like call.dial; the call then goes through the normal dial path (auto_hold rules apply; a non-device `source`/`sink` never holds device calls). Errors: not_found (account or BLF entry), invalid_params, sip_error.",
            blf_call_schema(),
        ),
        MethodInfo::new(
            "call.pickup",
            "Directed pickup of a ringing monitored extension. Allowed only while the BLF entry `uri` (as in blf.list) is `ringing` and pickup is configured for it: the method is the entry's `pickup` override, else the account's `pickup` (off|prefix|replaces). `prefix` dials `pickup_target` if set, else `<pickup_prefix><extension>` on the account (default prefix `**`, e.g. `**1004`). `replaces` (experimental; PBX support varies) sends an INVITE with `Require: replaces` and `Replaces: <call-id>;to-tag=<remote-tag>;from-tag=<local-tag>;early-only` to the caller (dialog-info <remote> target, RFC 5359 section 2.16; account `pickup_replaces_target = \"monitored\"` sends it to the monitored URI instead, same tags). Every pickup is routed through the account's registrar / outbound proxy whatever host the target names. Refused while the entry is stale or its subscription is not active. Returns {call_id, call, event_seq} like call.dial; a refusal (404, 480, 486, ...) arrives as call.state ended with end.code. Errors: not_found (account or BLF entry), invalid_state (pickup not configured, not ringing, stale, or `replaces` without usable dialog-info), invalid_params, sip_error.",
            blf_call_schema(),
        ),
        MethodInfo::new(
            "mwi.get",
            "Message-waiting status: {waiting, account_uri, voice{new,old,urgent_new,urgent_old}, other{class: counts}, updated_at}, or null until the first message-summary NOTIFY arrived (or with mwi=\"off\"). With `account` returns {account, mwi}; without it {mwi:[...]} for every account that has one. Errors: not_found.",
            account_opt("Account id; default all accounts."),
        ),
        MethodInfo::new(
            "subscribe",
            "Create a generic SUBSCRIBE for any event package (presence, as-feature-event, ...). It is kept alive (refresh at 85% of the granted expiry, resubscribe when the notifier allows) until `unsubscribe`. Every NOTIFY becomes a `subscription.notify` event with the raw body; state changes are `subscription.state` events. Returns {sub_id, event_seq}. Errors: not_found (account), invalid_params.",
            json!({"type":"object","properties":{
                "account":{"type":"string","description":"Account id."},
                "uri":{"type":"string","description":"Extension, user@host, or full sip: URI (extensions get the account's domain)."},
                "event":{"type":"string","description":"Event package, e.g. \"presence\", \"dialog\", \"message-summary\"."},
                "accept":{"type":"string","description":"Accept header; default depends on the package."},
                "expires":{"type":"integer","minimum":1,"description":"Requested lifetime in seconds (default 600)."}},
                "required":["account","uri","event"],"additionalProperties":false}),
        ),
        MethodInfo::new(
            "unsubscribe",
            "End a subscription made with `subscribe` (sends Expires: 0, bounded). BLF and MWI subscriptions are managed through the config. Returns {ok, event_seq}. Errors: not_found, invalid_params.",
            json!({"type":"object","properties":{
                "sub_id":{"type":"string","description":"Subscription id from `subscribe` or `subscriptions.list`."}},
                "required":["sub_id"],"additionalProperties":false}),
        ),
        MethodInfo::new(
            "subscriptions.list",
            "All subscriptions (BLF and MWI included): id, account, uri, event, accept, kind (blf|mwi|generic), state (pending|active|terminated|failed{code,reason}), expires_at, last_notify_at. Omit `account` for all accounts. Errors: not_found.",
            account_opt("Account id; default all accounts."),
        ),
    ]
}

impl AppBackend {
    fn require_account(&self, account: Option<&str>) -> Result<(), ApiError> {
        if let Some(a) = account {
            self.inner.stack.account_status(a).map_err(sip_error)?;
        }
        Ok(())
    }

    /// Dispatches `method` (one of [`SUBSCRIPTION_METHODS`]).
    pub(crate) async fn subscription_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        let stack = &self.inner.stack;
        match method {
            "blf.list" => {
                let p: OptAccount = parse_params(params)?;
                self.require_account(p.account.as_deref())?;
                Ok(json!({ "blf": stack.blf_status(p.account.as_deref()) }))
            }
            "blf.dial" | "call.pickup" => {
                let p: BlfCallParam = parse_params(params)?;
                crate::calls::check_str_len("uri", &p.uri)?;
                let opts = DialOptions {
                    source: self.parse_source(p.source.as_deref())?,
                    sink: self.parse_sink(p.sink.as_deref())?,
                    ..Default::default()
                };
                let seq = self.event_seq();
                let id = if method == "blf.dial" {
                    stack.blf_dial_with(&p.account, &p.uri, opts).await
                } else {
                    stack.pickup_with(&p.account, &p.uri, opts).await
                }
                .map_err(sip_error)?;
                Ok(json!({
                    "call_id": id,
                    "call": stack.call(&id).map(|c| crate::calls::to_json(&c)),
                    "event_seq": seq,
                }))
            }
            "mwi.get" => {
                let p: OptAccount = parse_params(params)?;
                self.require_account(p.account.as_deref())?;
                match p.account {
                    Some(a) => Ok(json!({ "account": a, "mwi": stack.mwi_status(&a) })),
                    None => {
                        let all: Vec<_> = stack
                            .accounts_status()
                            .iter()
                            .filter_map(|s| stack.mwi_status(&s.id))
                            .collect();
                        Ok(json!({ "mwi": all }))
                    }
                }
            }
            "subscribe" => {
                let p: SubscribeParam = parse_params(params)?;
                if p.expires == Some(0) {
                    return Err(ApiError::invalid_params("expires must be at least 1"));
                }
                crate::calls::check_str_len("uri", &p.uri)?;
                let seq = self.event_seq();
                let id = stack
                    .subscribe(&p.account, &p.uri, &p.event, p.accept.as_deref(), p.expires)
                    .map_err(sip_error)?;
                Ok(json!({ "sub_id": id, "event_seq": seq }))
            }
            "unsubscribe" => {
                let p: UnsubscribeParam = parse_params(params)?;
                let seq = self.event_seq();
                stack.unsubscribe(&p.sub_id).await.map_err(sip_error)?;
                Ok(json!({ "ok": true, "event_seq": seq }))
            }
            "subscriptions.list" => {
                let p: OptAccount = parse_params(params)?;
                self.require_account(p.account.as_deref())?;
                Ok(json!({ "subscriptions": stack.subscriptions(p.account.as_deref()) }))
            }
            other => Err(ApiError::method_not_found(other)),
        }
    }
}
