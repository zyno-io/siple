//! Call methods of the control API: `call.dial|answer|reject|hangup|mute|unmute|
//! dtmf|list|get|wait_for|play|stop_play|record_start|record_stop|detect_tone|expect_dtmf`,
//! `call.sip_trace` and `account.set_dnd`.
//!
//! Every state-changing result carries `event_seq`, the event-log position *before* the action,
//! so `events.poll {since: event_seq}` or `call.wait_for {since: event_seq}` cannot miss its
//! consequences.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use siple_config::{AudioSink, AudioSource, Codec, DtmfMode, SrtpPolicy};
use siple_sip::{
    AnswerMode, AnswerOptions, CallInfo, CallState, CompleteMode, DialOptions, SipError,
    TransferSession,
};
use tokio::time::Instant;

use crate::backend::AppBackend;
use crate::call_media::{MediaEntry, stats_json};
use crate::control::methods::{MAX_POLL_TIMEOUT_MS, parse_params};
use crate::control::{ApiError, MethodInfo, wait_for};
use crate::events::EventFilter;

/// Default `timeout_ms` of the waiting methods.
const DEFAULT_WAIT_MS: u64 = 10_000;
/// Longest wait of `call.wait_for`, `call.detect_tone` and `call.expect_dtmf`.
const MAX_WAIT_MS: u64 = MAX_POLL_TIMEOUT_MS;
/// Default `timeout_ms` of `call.detect_tone`.
const DEFAULT_DETECT_MS: u64 = 5_000;
/// Longest `call.dtmf` string.
const MAX_DTMF_DIGITS: usize = 32;
/// Bounds of `call.dtmf` `duration_ms`.
const DTMF_DURATION_MS: std::ops::RangeInclusive<u32> = 10..=10_000;
/// Default `min_level_dbfs` of `call.detect_tone`.
const DEFAULT_MIN_LEVEL_DBFS: f32 = -45.0;
/// How long `call.record_stop` waits for the engine to finish the file.
const RECORD_FINISH_WAIT: Duration = Duration::from_secs(2);

/// Lower-case state name as it appears in `call.state` events.
pub fn state_name(s: CallState) -> &'static str {
    match s {
        CallState::Dialing => "dialing",
        CallState::Ringing => "ringing",
        CallState::Early => "early",
        CallState::Active => "active",
        CallState::Held => "held",
        CallState::RemoteHeld => "remote_held",
        CallState::Ended => "ended",
    }
}

fn parse_state(s: &str) -> Result<CallState, ApiError> {
    [
        CallState::Dialing,
        CallState::Ringing,
        CallState::Early,
        CallState::Active,
        CallState::Held,
        CallState::RemoteHeld,
        CallState::Ended,
    ]
    .into_iter()
    .find(|st| state_name(*st).eq_ignore_ascii_case(s))
    .ok_or_else(|| {
        ApiError::invalid_params(format!(
            "unknown call state {} (dialing|ringing|early|active|held|remote_held|ended)",
            siple_config::echo(s)
        ))
    })
}

/// Maps SIP-layer errors to API errors by variant:
///
/// - unknown account / call -> `not_found`; call in the wrong state, closed account task,
///   duplicate account -> `invalid_state`; timeout -> `timeout`;
/// - bad arguments, account config and media setup problems (`Config`, `Media`,
///   `InvalidArgument`) -> `invalid_params`;
/// - a final SIP failure response to an in-dialog request (re-INVITE, INFO, REFER:
///   `SipError::Status`) -> `sip_error` with that `sip_code`;
/// - transport / DNS / TLS failures and other protocol failures -> `sip_error` with `sip_code`
///   0 (no SIP status was involved; the message says what failed). Final responses to calls
///   arrive as `call.state` `ended` events carrying `end.code`.
pub fn sip_error(e: SipError) -> ApiError {
    match e {
        SipError::UnknownAccount(_)
        | SipError::UnknownCall(_)
        | SipError::UnknownTransfer(_)
        | SipError::UnknownSubscription(_)
        | SipError::UnknownBlf(_) => ApiError::not_found(e.to_string()),
        SipError::CallState { .. } | SipError::Closed | SipError::DuplicateAccount(_) => {
            ApiError::invalid_state(e.to_string())
        }
        SipError::Timeout => ApiError::timeout(e.to_string()),
        SipError::Config(_) | SipError::Media(_) | SipError::InvalidArgument(_) => {
            ApiError::invalid_params(e.to_string())
        }
        SipError::Status { code, .. } => ApiError::sip_error(code, e.to_string()),
        SipError::Protocol(_)
        | SipError::Transport(_)
        | SipError::NoRoute(_)
        | SipError::Dns(_)
        | SipError::Tls(_) => ApiError::sip_error(0, e.to_string()),
    }
}

fn parse_opt<T: FromStr>(v: Option<&str>, what: &str) -> Result<Option<T>, ApiError>
where
    T::Err: std::fmt::Display,
{
    v.map(|s| {
        s.parse::<T>()
            .map_err(|e| ApiError::invalid_params(format!("{what}: {e}")))
    })
    .transpose()
}

fn parse_enum<T: serde::de::DeserializeOwned>(
    v: &str,
    what: &str,
    hint: &str,
) -> Result<T, ApiError> {
    serde_json::from_value(Value::String(v.to_ascii_lowercase())).map_err(|_| {
        ApiError::invalid_params(format!(
            "{what}: unknown value {} ({hint})",
            siple_config::echo(v)
        ))
    })
}

fn timeout_of(ms: Option<u64>, default_ms: u64) -> Duration {
    Duration::from_millis(ms.unwrap_or(default_ms).min(MAX_WAIT_MS))
}

fn finite(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}

pub(crate) fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

// ---- params ------------------------------------------------------------------------------

/// Longest free-form SIP target / URI string accepted (chars).
pub(crate) const MAX_TARGET_LEN: usize = 1024;
/// Largest total size of caller-supplied custom headers (names + values, bytes).
const MAX_HEADERS_BYTES: usize = 8 * 1024;

/// `invalid_params` when a caller-supplied string is longer than [`MAX_TARGET_LEN`].
pub(crate) fn check_str_len(name: &str, v: &str) -> Result<(), ApiError> {
    if v.len() > MAX_TARGET_LEN {
        return Err(ApiError::invalid_params(format!(
            "{name} is {} bytes (max {MAX_TARGET_LEN})",
            v.len()
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DialParam {
    account: String,
    target: String,
    headers: Option<BTreeMap<String, String>>,
    codecs: Option<Vec<String>>,
    srtp: Option<String>,
    source: Option<String>,
    sink: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerParam {
    call_id: String,
    source: Option<String>,
    sink: Option<String>,
    mode: Option<String>,
    end_call_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RejectParam {
    call_id: String,
    code: Option<u16>,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CallParam {
    call_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferBlindParam {
    call_id: String,
    target: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferAttendedParam {
    call_id: String,
    consult_call_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsultStartParam {
    call_id: String,
    target: String,
    source: Option<String>,
    sink: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsultLinkParam {
    call_id: String,
    consult_call_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionParam {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteParam {
    session_id: String,
    mode: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferListParam {
    active_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DtmfParam {
    call_id: String,
    digits: String,
    mode: Option<String>,
    duration_ms: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParam {
    account: Option<String>,
    active_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForParam {
    call_id: Option<String>,
    account: Option<String>,
    state: String,
    since: Option<u64>,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordStartParam {
    call_id: String,
    path: String,
    stereo: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetectParam {
    call_id: String,
    freq_hz: f32,
    timeout_ms: Option<u64>,
    min_level_dbfs: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectDtmfParam {
    call_id: String,
    digits: String,
    timeout_ms: Option<u64>,
    since: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DndParam {
    account: String,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceParam {
    call_id: String,
}

// ---- implementation ----------------------------------------------------------------------

impl AppBackend {
    pub(crate) fn call_info(&self, id: &str) -> Result<CallInfo, ApiError> {
        self.inner
            .stack
            .call(id)
            .ok_or_else(|| ApiError::not_found(format!("unknown call {}", siple_config::echo(id))))
    }

    /// Media entry of a call, with the right error for unknown calls and calls without media.
    pub(crate) fn media_entry(&self, id: &str) -> Result<MediaEntry, ApiError> {
        let info = self.call_info(id)?;
        if info.state == CallState::Ended {
            return Err(ApiError::invalid_state(format!("call {id} has ended")));
        }
        self.inner
            .call_media
            .get(id)
            .filter(|e| !e.closed)
            .ok_or_else(|| ApiError::invalid_state(format!("call {id} has no media")))
    }

    /// Resolves a relative path against the config directory (not the process's working
    /// directory, which differs between a terminal run and the GUI).
    pub(crate) fn resolve_path(&self, p: &std::path::Path) -> std::path::PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.inner.config_dir.join(p)
        }
    }

    /// Parses an audio source spec; `file:` paths are resolved against the config directory.
    pub(crate) fn parse_source(&self, spec: Option<&str>) -> Result<Option<AudioSource>, ApiError> {
        Ok(parse_opt::<AudioSource>(spec, "source")?.map(|s| match s {
            AudioSource::File(p) => AudioSource::File(self.resolve_path(&p)),
            other => other,
        }))
    }

    /// Parses an audio sink spec; `record:` paths are resolved against the config directory.
    pub(crate) fn parse_sink(&self, spec: Option<&str>) -> Result<Option<AudioSink>, ApiError> {
        Ok(parse_opt::<AudioSink>(spec, "sink")?.map(|s| match s {
            AudioSink::Record(p) => AudioSink::Record(self.resolve_path(&p)),
            other => other,
        }))
    }

    /// Dispatches `method` (one of [`CALL_METHODS`]).
    pub(crate) async fn call_method(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "call.dial" => self.call_dial(parse_params(params)?).await,
            "call.answer" => self.call_answer(parse_params(params)?).await,
            "call.reject" => self.call_reject(parse_params(params)?).await,
            "call.hangup" => {
                let p: CallParam = parse_params(params)?;
                let seq = self.event_seq();
                self.call_info(&p.call_id)?;
                self.inner
                    .stack
                    .hangup(&p.call_id)
                    .await
                    .map_err(sip_error)?;
                Ok(json!({ "ok": true, "call_id": p.call_id, "event_seq": seq }))
            }
            "call.mute" | "call.unmute" => {
                let p: CallParam = parse_params(params)?;
                let seq = self.event_seq();
                let muted = method == "call.mute";
                self.call_info(&p.call_id)?;
                self.inner
                    .stack
                    .mute(&p.call_id, muted)
                    .await
                    .map_err(sip_error)?;
                Ok(json!({ "ok": true, "call_id": p.call_id, "muted": muted, "event_seq": seq }))
            }
            "call.hold" | "call.unhold" => {
                let p: CallParam = parse_params(params)?;
                let seq = self.event_seq();
                let hold = method == "call.hold";
                self.call_info(&p.call_id)?;
                let res = if hold {
                    self.inner.stack.hold(&p.call_id).await
                } else {
                    self.inner.stack.unhold(&p.call_id).await
                };
                res.map_err(sip_error)?;
                let info = self.call_info(&p.call_id)?;
                Ok(json!({
                    "ok": true,
                    "call_id": p.call_id,
                    "state": state_name(info.state),
                    "hold": to_json(&info.hold),
                    "event_seq": seq
                }))
            }
            "call.activate" => {
                let p: CallParam = parse_params(params)?;
                let seq = self.event_seq();
                self.call_info(&p.call_id)?;
                self.inner
                    .stack
                    .activate(&p.call_id)
                    .await
                    .map_err(sip_error)?;
                Ok(json!({
                    "ok": true,
                    "call_id": p.call_id,
                    "active_line": self.inner.stack.active_line(),
                    "event_seq": seq
                }))
            }
            "call.transfer_blind" => self.call_transfer_blind(parse_params(params)?).await,
            "call.transfer_attended" => self.call_transfer_attended(parse_params(params)?).await,
            "call.consult_start" => self.call_consult_start(parse_params(params)?).await,
            "call.consult_link" => self.call_consult_link(parse_params(params)?).await,
            "transfer.complete" => self.transfer_complete(parse_params(params)?).await,
            "transfer.swap" | "transfer.cancel" => {
                self.transfer_swap_cancel(method, parse_params(params)?)
                    .await
            }
            "transfer.get" => {
                let p: SessionParam = parse_params(params)?;
                let s = self
                    .inner
                    .stack
                    .transfer(&p.session_id)
                    .map_err(sip_error)?;
                Ok(json!({ "session": to_json(&s), "event_seq": self.event_seq() }))
            }
            "transfer.list" => self.transfer_list(parse_params(params)?),
            "call.dtmf" => self.call_dtmf(parse_params(params)?).await,
            "call.list" => self.call_list(parse_params(params)?),
            "call.get" => self.call_get(parse_params(params)?),
            "call.wait_for" => self.call_wait_for(parse_params(params)?).await,
            "call.play" => self.call_play(parse_params(params)?).await,
            "call.stop_play" => self.call_stop_play(parse_params(params)?),
            "call.record_start" => self.call_record_start(parse_params(params)?).await,
            "call.record_stop" => self.call_record_stop(parse_params(params)?).await,
            "call.detect_tone" => self.call_detect_tone(parse_params(params)?).await,
            "call.expect_dtmf" => self.call_expect_dtmf(parse_params(params)?).await,
            "call.sip_trace" => self.call_sip_trace(parse_params(params)?),
            "account.set_dnd" => self.account_set_dnd(parse_params(params)?).await,
            other => Err(ApiError::method_not_found(other)),
        }
    }

    async fn call_dial(&self, p: DialParam) -> Result<Value, ApiError> {
        check_str_len("target", &p.target)?;
        if let Some(h) = &p.headers {
            let total: usize = h.iter().map(|(k, v)| k.len() + v.len()).sum();
            if total > MAX_HEADERS_BYTES {
                return Err(ApiError::invalid_params(format!(
                    "custom headers total {total} bytes (max {MAX_HEADERS_BYTES})"
                )));
            }
        }
        let codecs = p
            .codecs
            .map(|v| {
                v.iter()
                    .map(|c| parse_enum::<Codec>(c, "codecs", "opus|g722|pcmu|pcma"))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let opts = DialOptions {
            headers: p.headers.unwrap_or_default().into_iter().collect(),
            codecs,
            srtp: p
                .srtp
                .as_deref()
                .map(|s| parse_enum::<SrtpPolicy>(s, "srtp", "off|optional|mandatory"))
                .transpose()?,
            source: self.parse_source(p.source.as_deref())?,
            via_registrar: false,
            sink: self.parse_sink(p.sink.as_deref())?,
        };
        let seq = self.event_seq();
        let id = self
            .inner
            .stack
            .dial(&p.account, &p.target, opts)
            .await
            .map_err(sip_error)?;
        Ok(json!({
            "call_id": id,
            "call": self.inner.stack.call(&id).map(|c| to_json(&c)),
            "event_seq": seq,
        }))
    }

    async fn call_answer(&self, p: AnswerParam) -> Result<Value, ApiError> {
        let mode = match (p.mode.as_deref(), p.end_call_id) {
            (None, None) => AnswerMode::Default,
            (Some("hold_others"), None) => AnswerMode::HoldOthers,
            (Some("end_others"), Some(id)) => {
                self.call_info(&id)?;
                AnswerMode::EndOthers(id)
            }
            (Some("end_others"), None) => {
                return Err(ApiError::invalid_params(
                    "mode end_others needs end_call_id (the call to hang up)",
                ));
            }
            (Some("hold_others"), Some(_)) | (None, Some(_)) => {
                return Err(ApiError::invalid_params(
                    "end_call_id is only valid with mode end_others",
                ));
            }
            (Some(other), _) => {
                return Err(ApiError::invalid_params(format!(
                    "mode must be hold_others or end_others, got {}",
                    siple_config::echo(other)
                )));
            }
        };
        let opts = AnswerOptions {
            source: self.parse_source(p.source.as_deref())?,
            sink: self.parse_sink(p.sink.as_deref())?,
            mode,
        };
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        let outcome = self
            .inner
            .stack
            .answer(&p.call_id, opts)
            .await
            .map_err(sip_error)?;
        let mut res = json!({ "ok": true, "call_id": p.call_id, "event_seq": seq });
        if outcome == siple_sip::AnswerOutcome::EndOthersPendingAck {
            res["end_others"] = json!("pending_ack");
        }
        Ok(res)
    }

    async fn call_transfer_blind(&self, p: TransferBlindParam) -> Result<Value, ApiError> {
        check_str_len("target", &p.target)?;
        if p.target.trim().is_empty() {
            return Err(ApiError::invalid_params("target must not be empty"));
        }
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        let session = self
            .inner
            .stack
            .transfer_blind(&p.call_id, p.target.trim())
            .await
            .map_err(sip_error)?;
        Ok(json!({
            "ok": true,
            "call_id": p.call_id,
            "target": p.target.trim(),
            "session_id": session.id,
            "session": to_json(&session),
            "event_seq": seq
        }))
    }

    async fn call_transfer_attended(&self, p: TransferAttendedParam) -> Result<Value, ApiError> {
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        self.call_info(&p.consult_call_id)?;
        let session = self
            .inner
            .stack
            .transfer_attended(&p.call_id, &p.consult_call_id)
            .await
            .map_err(sip_error)?;
        Ok(json!({
            "ok": true,
            "call_id": p.call_id,
            "consult_call_id": p.consult_call_id,
            "session_id": session.id,
            "session": to_json(&session),
            "event_seq": seq
        }))
    }

    async fn call_consult_start(&self, p: ConsultStartParam) -> Result<Value, ApiError> {
        check_str_len("target", &p.target)?;
        if p.target.trim().is_empty() {
            return Err(ApiError::invalid_params("target must not be empty"));
        }
        let opts = DialOptions {
            source: self.parse_source(p.source.as_deref())?,
            sink: self.parse_sink(p.sink.as_deref())?,
            ..Default::default()
        };
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        let s = self
            .inner
            .stack
            .consult_start(&p.call_id, p.target.trim(), opts)
            .await
            .map_err(sip_error)?;
        Ok(json!({
            "ok": true,
            "session_id": s.id,
            "consult_call_id": s.consult,
            "session": to_json(&s),
            "event_seq": seq
        }))
    }

    async fn call_consult_link(&self, p: ConsultLinkParam) -> Result<Value, ApiError> {
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        self.call_info(&p.consult_call_id)?;
        let s = self
            .inner
            .stack
            .consult_link(&p.call_id, &p.consult_call_id)
            .await
            .map_err(sip_error)?;
        Ok(json!({
            "ok": true,
            "session_id": s.id,
            "consult_call_id": s.consult,
            "session": to_json(&s),
            "event_seq": seq
        }))
    }

    async fn transfer_complete(&self, p: CompleteParam) -> Result<Value, ApiError> {
        let mode = match p.mode.to_ascii_lowercase().as_str() {
            "attended" => CompleteMode::Attended,
            "blind_now" => CompleteMode::BlindNow,
            other => {
                return Err(ApiError::invalid_params(format!(
                    "mode must be attended or blind_now, got {}",
                    siple_config::echo(other)
                )));
            }
        };
        let seq = self.event_seq();
        let s = self
            .inner
            .stack
            .transfer_complete(&p.session_id, mode)
            .await
            .map_err(sip_error)?;
        Ok(json!({ "ok": true, "session_id": s.id, "session": to_json(&s), "event_seq": seq }))
    }

    async fn transfer_swap_cancel(&self, method: &str, p: SessionParam) -> Result<Value, ApiError> {
        let seq = self.event_seq();
        let s = if method == "transfer.swap" {
            self.inner.stack.transfer_swap(&p.session_id).await
        } else {
            self.inner.stack.transfer_cancel(&p.session_id).await
        }
        .map_err(sip_error)?;
        Ok(json!({ "ok": true, "session_id": s.id, "session": to_json(&s), "event_seq": seq }))
    }

    fn transfer_list(&self, p: TransferListParam) -> Result<Value, ApiError> {
        let list: Vec<TransferSession> = self
            .inner
            .stack
            .transfers()
            .into_iter()
            .filter(|s| !(p.active_only.unwrap_or(false) && s.phase.is_terminal()))
            .collect();
        Ok(json!({ "transfers": to_json(&list), "event_seq": self.event_seq() }))
    }

    async fn call_reject(&self, p: RejectParam) -> Result<Value, ApiError> {
        if let Some(c) = p.code
            && !(400..700).contains(&c)
        {
            return Err(ApiError::invalid_params("code must be 400-699"));
        }
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        self.inner
            .stack
            .reject(&p.call_id, p.code, p.reason)
            .await
            .map_err(sip_error)?;
        Ok(json!({ "ok": true, "call_id": p.call_id, "event_seq": seq }))
    }

    async fn call_dtmf(&self, p: DtmfParam) -> Result<Value, ApiError> {
        let digits: String = p
            .digits
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if digits.is_empty() || !digits.chars().all(is_dtmf_digit) {
            return Err(ApiError::invalid_params(
                "digits must be non-empty and use 0-9 * # A-D",
            ));
        }
        if digits.chars().count() > MAX_DTMF_DIGITS {
            return Err(ApiError::invalid_params(format!(
                "at most {MAX_DTMF_DIGITS} digits per call.dtmf (got {})",
                digits.chars().count()
            )));
        }
        if let Some(d) = p.duration_ms
            && !DTMF_DURATION_MS.contains(&d)
        {
            return Err(ApiError::invalid_params(format!(
                "duration_ms must be between {} and {} (got {d})",
                DTMF_DURATION_MS.start(),
                DTMF_DURATION_MS.end()
            )));
        }
        let mode = p
            .mode
            .as_deref()
            .map(|m| parse_enum::<DtmfMode>(m, "mode", "rfc4733|info|inband|auto"))
            .transpose()?;
        let seq = self.event_seq();
        self.call_info(&p.call_id)?;
        self.inner
            .stack
            .send_dtmf(&p.call_id, &digits, mode, p.duration_ms)
            .await
            .map_err(sip_error)?;
        Ok(json!({ "ok": true, "call_id": p.call_id, "digits": digits, "event_seq": seq }))
    }

    fn call_list(&self, p: ListParam) -> Result<Value, ApiError> {
        if let Some(a) = &p.account {
            self.inner.stack.account_status(a).map_err(sip_error)?;
        }
        let calls: Vec<Value> = self
            .inner
            .stack
            .calls()
            .iter()
            .filter(|c| p.account.as_deref().is_none_or(|a| c.account == a))
            .filter(|c| !(p.active_only.unwrap_or(false) && c.state == CallState::Ended))
            .map(to_json)
            .collect();
        Ok(json!({ "calls": calls, "event_seq": self.event_seq() }))
    }

    fn call_get(&self, p: CallParam) -> Result<Value, ApiError> {
        let info = self.call_info(&p.call_id)?;
        let mut v = to_json(&info);
        let entry = self.inner.call_media.get(&p.call_id);
        v["media"] = entry
            .as_ref()
            .map_or(Value::Null, |e| stats_json(&e.handle.stats()));
        v["audio"] = entry.as_ref().map_or(Value::Null, |e| {
            let st = e.handle.stats();
            json!({
                "source": e.source.to_string(),
                "sink": e.sink.to_string(),
                "device_bound": e.device_bound,
                "rx_ms": st.rx_ms,
                "play": { "current": st.play_current, "queued": st.play_queued },
                "tx_stream": st.tx_stream,
                "rx_tap_dropped_ms": st.rx_tap_dropped_ms,
            })
        });
        v["event_seq"] = self.event_seq().into();
        Ok(v)
    }

    async fn call_wait_for(&self, p: WaitForParam) -> Result<Value, ApiError> {
        if p.call_id.is_none() && p.account.is_none() {
            return Err(ApiError::invalid_params("give `call_id` or `account`"));
        }
        let target = parse_state(&p.state)?;
        let timeout = timeout_of(p.timeout_ms, DEFAULT_WAIT_MS);
        let stack = &self.inner.stack;
        // Validate the scope before waiting.
        if let Some(a) = &p.account {
            stack.account_status(a).map_err(sip_error)?;
        }
        if let Some(id) = &p.call_id {
            self.call_info(id)?;
        }
        // Capture the position first, then look at the current state: anything that happens
        // after the capture is in the log, anything before it is visible in the state.
        let seq = self.event_seq();
        if p.since.is_none() {
            let current = match (&p.call_id, &p.account) {
                (Some(id), _) => stack.call(id),
                (None, Some(a)) => stack.calls().into_iter().rev().find(|c| &c.account == a),
                (None, None) => None,
            };
            if let Some(c) = current {
                if c.state == target {
                    return Ok(
                        json!({ "call": to_json(&c), "event_seq": seq, "matched_seq": null }),
                    );
                }
                if c.state == CallState::Ended && p.call_id.is_some() {
                    return Err(ended_early(&c, target));
                }
            }
        }
        let call_scoped = p.call_id.is_some();
        let filter = EventFilter {
            kinds: Some(vec!["call.state".into(), "call.incoming".into()]),
            account: p.account.clone(),
            call_id: p.call_id.clone(),
        };
        let want = state_name(target);
        let matches = |e: &crate::events::Event| {
            let st = e.data["state"].as_str();
            st == Some(want) || (call_scoped && st == Some("ended"))
        };
        let since = p.since.unwrap_or(seq);
        crate::control::methods::check_since(&self.inner.events, since)?;
        // Is the call already over? Read this *before* scanning the log: the target state (if it
        // was ever reached) was logged before the end, so a scan after this read sees it.
        let already_ended = target != CallState::Ended
            && p.call_id
                .as_deref()
                .and_then(|id| stack.call(id))
                .is_some_and(|c| c.state == CallState::Ended);
        let probe = wait_for(&self.inner.events, since, &filter, matches, Duration::ZERO).await;
        let res = match probe {
            Ok(ev) => Ok(ev),
            // Nothing in the log and nothing can happen any more: fail now, not at the timeout.
            Err(_) if already_ended => {
                if let Some(c) = p.call_id.as_deref().and_then(|id| stack.call(id)) {
                    return Err(ended_early(&c, target));
                }
                wait_for(&self.inner.events, since, &filter, matches, timeout).await
            }
            Err(_) => wait_for(&self.inner.events, since, &filter, matches, timeout).await,
        };
        match res {
            Ok(ev) => {
                if ev.data["state"].as_str() != Some(want) {
                    let info = serde_json::from_value::<Value>(ev.data.clone()).unwrap_or_default();
                    return Err(ApiError::invalid_state(format!(
                        "call {} ended ({} {}) while waiting for {want}",
                        ev.call_id.as_deref().unwrap_or("?"),
                        info["end"]["code"],
                        info["end"]["reason"].as_str().unwrap_or(""),
                    ))
                    .with_extra(json!({ "call": ev.data, "event_seq": seq })));
                }
                Ok(json!({ "call": ev.data, "event_seq": seq, "matched_seq": ev.seq }))
            }
            Err(e) => {
                let now = p
                    .call_id
                    .as_deref()
                    .and_then(|id| stack.call(id))
                    .map(|c| format!(" (call {} is {})", c.id, state_name(c.state)))
                    .unwrap_or_default();
                Err(ApiError::timeout(format!(
                    "{}; wanted {want}{now}",
                    e.message
                )))
            }
        }
    }

    async fn call_record_start(&self, p: RecordStartParam) -> Result<Value, ApiError> {
        if p.path.trim().is_empty() {
            return Err(ApiError::invalid_params("path is empty"));
        }
        let path = self.resolve_path(std::path::Path::new(&p.path));
        let entry = self.media_entry(&p.call_id)?;
        // Create the file now so an unwritable path fails the request instead of failing later
        // inside the media thread.
        let create = path.clone();
        tokio::task::spawn_blocking(move || std::fs::File::create(&create).map(drop))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|e| {
                ApiError::invalid_params(format!("cannot create {}: {e}", path.display()))
            })?;
        let seq = self.event_seq();
        entry
            .handle
            .record_start(path.clone(), p.stereo.unwrap_or(false));
        Ok(json!({
            "ok": true, "call_id": p.call_id, "path": path.display().to_string(), "event_seq": seq
        }))
    }

    async fn call_record_stop(&self, p: CallParam) -> Result<Value, ApiError> {
        let entry = self.media_entry(&p.call_id)?;
        let seq = self.event_seq();
        entry.handle.record_stop();
        let filter = EventFilter {
            kinds: Some(vec!["call.record_finished".into()]),
            account: None,
            call_id: Some(p.call_id.clone()),
        };
        let done = wait_for(
            &self.inner.events,
            seq,
            &filter,
            |_| true,
            RECORD_FINISH_WAIT,
        )
        .await
        .ok();
        Ok(json!({
            "ok": true,
            "call_id": p.call_id,
            "finished": done.is_some(),
            "path": done.as_ref().map(|e| e.data["path"].clone()),
            "event_seq": seq,
        }))
    }

    async fn call_detect_tone(&self, p: DetectParam) -> Result<Value, ApiError> {
        if !(p.freq_hz.is_finite() && p.freq_hz > 0.0 && p.freq_hz < 24_000.0) {
            return Err(ApiError::invalid_params("freq_hz must be in (0, 24000)"));
        }
        let timeout = timeout_of(p.timeout_ms, DEFAULT_DETECT_MS);
        let entry = self.media_entry(&p.call_id)?;
        let rx = entry.handle.detect_tone(
            p.freq_hz,
            timeout,
            p.min_level_dbfs.unwrap_or(DEFAULT_MIN_LEVEL_DBFS),
        );
        let r = tokio::time::timeout(timeout + Duration::from_secs(2), rx)
            .await
            .map_err(|_| ApiError::timeout("tone detector did not answer"))?
            .map_err(|_| {
                ApiError::invalid_state(format!("call {} ended during tone detection", p.call_id))
            })?;
        Ok(json!({
            "detected": r.detected,
            "level_dbfs": finite(r.level_dbfs),
            "power_ratio": finite(r.power_ratio),
            "freq_hz": p.freq_hz,
        }))
    }

    async fn call_expect_dtmf(&self, p: ExpectDtmfParam) -> Result<Value, ApiError> {
        let want = p.digits.to_ascii_uppercase();
        if want.is_empty() || !want.chars().all(is_dtmf_digit) {
            return Err(ApiError::invalid_params(
                "digits must be non-empty and use 0-9 * # A-D",
            ));
        }
        self.call_info(&p.call_id)?;
        let timeout = timeout_of(p.timeout_ms, DEFAULT_WAIT_MS);
        let deadline = Instant::now() + timeout;
        let mut cursor = p.since.unwrap_or_else(|| self.event_seq());
        crate::control::methods::check_since(&self.inner.events, cursor)?;
        // `call.state` events wake the wait when the call ends.
        let filter = EventFilter {
            kinds: Some(vec!["call.dtmf".into(), "call.state".into()]),
            account: None,
            call_id: Some(p.call_id.clone()),
        };
        let mut received = String::new();
        loop {
            // Read before polling: digits sent before the end are logged before it, so the
            // poll below sees them.
            let ended = self
                .inner
                .stack
                .call(&p.call_id)
                .is_some_and(|c| c.state == CallState::Ended);
            let remaining = if ended {
                Duration::ZERO
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            let r = self
                .inner
                .events
                .poll(cursor, &filter, 500, remaining)
                .await;
            for e in r.events.iter().filter(|e| e.kind == "call.dtmf") {
                if let Some(d) = e.data["digit"].as_str() {
                    received.push_str(&d.to_ascii_uppercase());
                }
            }
            cursor = r.next_seq;
            if received.contains(&want) {
                return Ok(json!({ "matched": true, "received": received, "event_seq": cursor }));
            }
            if ended {
                return Err(ApiError::invalid_state(format!(
                    "call {} ended while waiting for DTMF {}; received {received:?}",
                    siple_config::echo(&p.call_id),
                    siple_config::echo(&want)
                ))
                .with_extra(json!({ "received": received, "event_seq": cursor })));
            }
            if Instant::now() >= deadline {
                return Err(ApiError::timeout(format!(
                    "expected DTMF {}, received {received:?}",
                    siple_config::echo(&want)
                ))
                .with_extra(json!({ "received": received, "event_seq": cursor })));
            }
        }
    }

    fn call_sip_trace(&self, p: TraceParam) -> Result<Value, ApiError> {
        // `cN` maps to the SIP Call-ID of its dialog; anything else is taken as a raw Call-ID.
        let (sip_call_id, call_id) = match self.inner.stack.call(&p.call_id) {
            Some(c) => (c.sip_call_id, Some(p.call_id.clone())),
            None => (p.call_id.clone(), None),
        };
        let messages = self.inner.trace.messages_for_call(&sip_call_id);
        Ok(json!({
            "call_id": call_id,
            "sip_call_id": sip_call_id,
            "count": messages.len(),
            "messages": messages,
        }))
    }

    async fn account_set_dnd(&self, p: DndParam) -> Result<Value, ApiError> {
        let seq = self.event_seq();
        self.inner
            .stack
            .set_dnd(&p.account, p.enabled)
            .await
            .map_err(sip_error)?;
        let enabled = self.inner.stack.dnd(&p.account).map_err(sip_error)?;
        Ok(json!({ "ok": true, "account": p.account, "enabled": enabled, "event_seq": seq }))
    }
}

fn is_dtmf_digit(c: char) -> bool {
    matches!(c.to_ascii_uppercase(), '0'..='9' | '*' | '#' | 'A'..='D')
}

fn ended_early(c: &CallInfo, want: CallState) -> ApiError {
    let (code, reason) = c
        .end
        .as_ref()
        .map_or((0, String::new()), |e| (e.code, e.reason.clone()));
    ApiError::invalid_state(format!(
        "call {} already ended ({code} {reason}); wanted {}",
        c.id,
        state_name(want)
    ))
    .with_extra(json!({ "call": to_json(c) }))
}

/// Methods handled by [`AppBackend::call_method`].
pub const CALL_METHODS: &[&str] = &[
    "call.dial",
    "call.answer",
    "call.reject",
    "call.hangup",
    "call.mute",
    "call.unmute",
    "call.hold",
    "call.unhold",
    "call.activate",
    "call.transfer_blind",
    "call.transfer_attended",
    "call.consult_start",
    "call.consult_link",
    "transfer.complete",
    "transfer.swap",
    "transfer.cancel",
    "transfer.get",
    "transfer.list",
    "call.dtmf",
    "call.list",
    "call.get",
    "call.wait_for",
    "call.play",
    "call.stop_play",
    "call.record_start",
    "call.record_stop",
    "call.detect_tone",
    "call.expect_dtmf",
    "call.sip_trace",
    "account.set_dnd",
];

/// Schemas and descriptions of [`CALL_METHODS`].
pub fn call_methods() -> Vec<MethodInfo> {
    let call_id = json!({"type":"string","description":"Call id (c1, c2, ...) from call.dial, call.incoming events or call.list."});
    let source = json!({"type":"string","description":"Audio source: device | silence | tone:<hz> | file:<path.wav> (relative paths: the config file's directory). Default: the account's audio_source. Use tone:<hz> or file: for automated tests (no microphone)."});
    let sink = json!({"type":"string","description":"Audio sink: device | null | record:<path.wav> (relative paths: the config file's directory). Default: the account's audio_sink."});
    let timeout = |d: u64| json!({"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"description":format!("Give up after this many milliseconds (default {d}).")});
    let obj = |props: Value, required: &[&str]| json!({"type":"object","properties":props,"required":required,"additionalProperties":false});
    let by_id = || obj(json!({"call_id": call_id}), &["call_id"]);
    let session_id = json!({"type":"string","description":"Transfer session id (t1, t2, ...) from call.consult_start, call.transfer_blind, transfer.list or transfer.session events."});
    let session_by_id = || obj(json!({"session_id": session_id.clone()}), &["session_id"]);
    vec![
        MethodInfo::new(
            "call.dial",
            "Place an outgoing call. `target` is an extension / number (becomes sip:<target>@<domain>) or a full sip:/sips: URI. Returns {call_id, call (full snapshot, state dialing), event_seq} as soon as the INVITE is sent; then use call.wait_for {call_id, state:\"active\", since:event_seq} to wait for the answer. Limits: `target` <= 1024 bytes and custom `headers` <= 8 KiB in total (invalid_params); an INVITE too large for a UDP datagram fails at once with sip_error (code 0, \"transport: INVITE is ... too large\") rather than timing out. Errors: not_found (account), invalid_params, sip_error.",
            obj(
                json!({
                "account":{"type":"string","description":"Account id to call from."},
                "target":{"type":"string","description":"Extension, number, or sip: URI."},
                "headers":{"type":"object","additionalProperties":{"type":"string"},"description":"Extra INVITE headers, name -> value."},
                "codecs":{"type":"array","items":{"type":"string","enum":["opus","g722","pcmu","pcma"]},"description":"Codec preference for this call."},
                "srtp":{"type":"string","enum":["off","optional","mandatory"],"description":"SRTP policy for this call (default: account setting)."},
                "source":source.clone(),"sink":sink.clone()}),
                &["account", "target"],
            ),
        ),
        MethodInfo::new(
            "call.answer",
            "Answer a ringing inbound call (state `ringing`). Optionally override the audio source / sink for this call. A source/sink different from the call's current one recreates the call media at answer: rx_ms restarts at 0, and call.play / call.listen issued before the answer end with call_ended (issue them after the answer, or answer without source/sink). `mode` picks what happens to the other lines: omitted = the accounts' `auto_hold` setting decides whether the other device-bound calls (per [calls] hold_scope) are put on hold first; `hold_others` = hold them whatever `auto_hold` says (a failed hold is logged and the call is answered anyway); `end_others` + `end_call_id` = answer first, hold nothing, and hang `end_call_id` up only once the caller's ACK confirms the answer: the call returns success as soon as the 200 is sent, with `end_others: \"pending_ack\"`; a background task then waits for the ACK (until the INVITE transaction gives up, about 32 s) and hangs `end_call_id` up (watch its call.state ended event). If the ACK never comes `end_call_id` is untouched and becomes the active line again. If the answer itself fails (invalid_state) it is untouched too. Calls that use no audio device never hold other lines, whatever the mode. Answers that hold other lines are queued with call.activate (each hold is bounded to 8 s). Returns {ok, call_id, event_seq, end_others?}. Errors: not_found, invalid_state (not ringing), invalid_params.",
            obj(
                json!({"call_id":call_id.clone(),"source":source,"sink":sink,
                "mode":{"type":"string","enum":["hold_others","end_others"],"description":"Line handling; default: the accounts' auto_hold setting."},
                "end_call_id":{"type":"string","description":"With mode end_others: the call to hang up once this one is active."}}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "call.reject",
            "Reject a ringing inbound call with a final response (default 486 Busy Here; e.g. 603 Decline, 480). Emits call.rejected / call.state ended.",
            obj(
                json!({"call_id":call_id.clone(),
                "code":{"type":"integer","minimum":400,"maximum":699,"description":"SIP status, default 486."},
                "reason":{"type":"string","description":"Reason phrase."}}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "call.hangup",
            "End a call: CANCEL while dialing/early, 603 for an unanswered inbound call, BYE otherwise. Returns event_seq; the call.state `ended` event follows.",
            by_id(),
        ),
        MethodInfo::new(
            "call.mute",
            "Mute our microphone/source for the call (local only; RTP keeps flowing with silence).",
            by_id(),
        ),
        MethodInfo::new("call.unmute", "Undo call.mute.", by_id()),
        MethodInfo::new(
            "call.hold",
            "Put an established call on hold (re-INVITE a=sendonly, or inactive per the account's hold_style / when the remote already holds us); we stop transmitting. Returns once the remote answered: `state` is `held` and `hold` = {local, remote}. A refused re-INVITE leaves the call as it was (sip_error with the status code); 491 glare is retried automatically. While a transfer of the call is `referring` or `transferring` (see transfer.get) it fails with invalid_state; an outstanding re-INVITE of ours on the call (e.g. the automatic resume after a failed transfer) is waited for once, so a redundant request succeeds. Returns event_seq; call.state / call.updated events follow.",
            by_id(),
        ),
        MethodInfo::new(
            "call.unhold",
            "Take a call off hold (re-INVITE a=sendrecv). `state` is `active`, or `remote_held` while the remote still holds us. Fails with invalid_state while a transfer of the call is `referring` or `transferring` (see transfer.get); an outstanding re-INVITE of ours on the call (e.g. the automatic resume after a failed transfer) is waited for once, so an unhold that became redundant succeeds.",
            by_id(),
        ),
        MethodInfo::new(
            "call.activate",
            "Make `call_id` the one live line: the other un-held device-bound calls (per [calls] hold_scope; calls without an audio device and the peer leg of a call between two of our own accounts are left alone) are put on hold, the call is resumed if we hold it, and the microphone/speaker focus moves to it. If any hold fails the whole operation aborts with sip_error and nothing is resumed. Idempotent: activating the active line does nothing. A call whose transfer is `referring` / `transferring` cannot be activated (invalid_state) and is never auto-held. Queued in order with the other multi-line operations (call.answer that holds other lines, dial's auto-hold); each hold is bounded to 8 s, a line that ends meanwhile counts as held, and an outstanding re-INVITE on a line is waited for once; while it runs `pending` is `activate` on the target and `hold` on the calls being held. Returns {ok, call_id, active_line, event_seq}. Errors: not_found, invalid_state (not established, or no audio device), sip_error.",
            by_id(),
        ),
        MethodInfo::new(
            "call.transfer_blind",
            "Blind-transfer an established call to `target` (extension / number -> sip:<target>@<domain>, or a sip: URI): the call is put on hold, then REFER with Refer-To and Referred-By is sent. Returns {ok, session_id, session, event_seq} once the REFER was accepted (202); it creates a blind transfer session (see transfer.get; phases referring, transferring, then transferred / failed / unconfirmed) and emits `transfer.session` events; the `call.transfer_progress` events {call_id, code, reason, final} (100 Trying, 180, then the final 200 or a failure) stay. On a final 2xx the call ends (end.reason and end.cause `transferred`, whichever side's BYE came first). On a definitive failure the call we put on hold is taken off hold again ([calls] resume_after_failed_transfer, default true; not when another device line is live); no final NOTIFY within 32 s gives final code 408, session phase `unconfirmed` and no automatic action (a late final NOTIFY still decides). A refused REFER fails with sip_error and the status in sip_code (also reported as a final progress event). Errors: not_found, invalid_state (not established, or the call is in another transfer session), invalid_params, sip_error.",
            obj(
                json!({"call_id":call_id.clone(),
                "target":{"type":"string","description":"Extension, number or sip:/sips: URI to transfer the remote party to."}}),
                &["call_id", "target"],
            ),
        ),
        MethodInfo::new(
            "call.transfer_attended",
            "Attended transfer, legacy one-call form: `call_id` (the original call, held if it is not) is handed over to the party of `consult_call_id` (a second call of the same account). It creates (or, when call.consult_start / call.consult_link already paired these two calls, drives) a transfer session and completes it at once: REFER with Refer-To <consult party?Replaces=...> is sent on the original call (account option attended_refer_on = \"consult\": on the consult call, pointing at the original), the target is the AOR or Contact (attended_refer_target). If the consult call is still dialing/early it is CANCELed and the original is transferred blind to the party it rang (semi-attended; for an explicit choice use call.consult_start + transfer.complete). Returns {ok, session_id, session, event_seq}. Progress as call.transfer_progress on the call the REFER went to; on a final 2xx that call ends (`transferred`) and the other one is hung up after 5 s unless the transfer target ended it. A REFER that fails while the consult call is alive returns the session to `consult_connected` with `last_error`. Errors: not_found, invalid_state, invalid_params (same call / different accounts), sip_error.",
            obj(
                json!({"call_id":call_id.clone(),
                "consult_call_id":{"type":"string","description":"The consult call (c2, ...) of the same account."}}),
                &["call_id", "consult_call_id"],
            ),
        ),
        MethodInfo::new(
            "call.consult_start",
            "Start an attended transfer: put `call_id` (established) on hold, dial `target` on the same account (the consult call) and link both in a transfer session. Calls that use no audio device (agent / silence / file / null) are held too, as a phone does for a transfer, but never cause holds of other calls. `source` / `sink` default to the original call's own. Returns {ok, session_id, consult_call_id, session, event_seq} as soon as the INVITE is sent; the session phase follows the consult call (consult_dialing, consult_ringing, consult_connected, or consult_failed with the original still held: use transfer.cancel to resume it). Then call transfer.complete, transfer.swap or transfer.cancel. A call can be in one live session at a time. Queued with the other multi-line operations (call.activate); the hold is bounded to 6 s. Errors: not_found, invalid_state (not established, already in a session), invalid_params, sip_error.",
            obj(
                json!({"call_id":call_id.clone(),
                "target":{"type":"string","description":"Extension, number or sip:/sips: URI to consult."},
                "source":source.clone(),"sink":sink.clone()}),
                &["call_id", "target"],
            ),
        ),
        MethodInfo::new(
            "call.consult_link",
            "Pair two existing calls of the same account into a transfer session: `call_id` is the original (established), `consult_call_id` the consult call (dialing, early or established). Nothing is held or dialled. Returns {ok, session_id, consult_call_id, session, event_seq}. Errors: not_found, invalid_state (not established, already in a session), invalid_params (same call / different accounts: \"both calls must belong to the same account\").",
            obj(
                json!({"call_id":call_id.clone(),
                "consult_call_id":{"type":"string","description":"The consult call of the same account."}}),
                &["call_id", "consult_call_id"],
            ),
        ),
        MethodInfo::new(
            "transfer.complete",
            "Finish a consult. `mode` attended: REFER with Replaces connects the original to the consult party (needs consult_connected; while the consult still rings it fails with invalid_state \"consult not answered; use blind_now\"). `blind_now`: end the consult call (CANCEL / BYE) and REFER the original to the consult's dialled URI (semi-attended). Returns once the REFER was accepted: {ok, session_id, session, event_seq}; the phase is then transferring and ends in transferred, failed or unconfirmed (watch `transfer.session` events or poll transfer.get). A refused REFER (or a local error) fails with sip_error; the session returns to consult_connected with `last_error` (attended with the consult alive), or becomes failed and the original is resumed ([calls] resume_after_failed_transfer). Idempotent: completing a session whose REFER is already sent returns the session and sends no second REFER. Errors: not_found, invalid_state, invalid_params (mode), sip_error.",
            obj(
                json!({"session_id":session_id.clone(),
                "mode":{"type":"string","enum":["attended","blind_now"],"description":"attended = Replaces transfer of an answered consult; blind_now = cancel the consult and transfer blind to its target."}}),
                &["session_id", "mode"],
            ),
        ),
        MethodInfo::new(
            "transfer.swap",
            "During consult_connected: hold the leg you are talking to (`talking_to`), resume the other and give it the microphone / speaker focus. If the hold fails nothing is resumed and `last_error` (stage swap) is set. Returns {ok, session_id, session, event_seq}. Errors: not_found, invalid_state (not consult_connected), sip_error.",
            session_by_id(),
        ),
        MethodInfo::new(
            "transfer.cancel",
            "Abandon a consult: hang up the consult call (CANCEL or BYE), resume the original when the session held it, focus it. Phase becomes cancelled. A no-op returning the session when it is already finished; invalid_state \"transfer already handed to the PBX\" once the REFER was sent (referring, transferring, unconfirmed: SIP cannot retract an accepted REFER). Returns {ok, session_id, session, event_seq}.",
            session_by_id(),
        ),
        MethodInfo::new(
            "transfer.get",
            "One transfer session (live, or among the last 16 finished): {id, kind: blind|attended|semi, phase, account, original, consult, target_uri, refer_call, talking_to, we_held_original, last_notify {code, reason, at}, last_error {stage, code, reason}, started_at, ended_at}. Phases: consult_dialing, consult_ringing, consult_connected, consult_failed, referring, transferring, unconfirmed (no final NOTIFY after 32 s: the transfer may have happened; nothing is done automatically, a late final NOTIFY moves it on), and the terminal transferred, failed, cancelled, abandoned (the original ended before the outcome). Calls carry `transfer {session_id, role: original|consult}`. While a session is referring or transferring, call.hold / call.unhold of either call fail with invalid_state. Errors: not_found.",
            session_by_id(),
        ),
        MethodInfo::new(
            "transfer.list",
            "Transfer sessions: the last 16 finished ones, then the live ones, oldest first. Returns {transfers, event_seq}.",
            obj(
                json!({"active_only":{"type":"boolean","description":"Leave out finished sessions."}}),
                &[],
            ),
        ),
        MethodInfo::new(
            "call.dtmf",
            "Send up to 32 DTMF digits (0-9 * # A-D) on an established call; each lasts duration_ms (10-10000). `mode`: rfc4733 (telephone-event), info (SIP INFO), inband (audio tones) or auto (rfc4733 if negotiated, else info; default: config dtmf_mode). The far end sees call.dtmf events.",
            obj(
                json!({"call_id":call_id.clone(),
                "digits":{"type":"string","maxLength":32,"description":"Digits to send, 0-9 * # A-D, at most 32, e.g. \"123#\"."},
                "mode":{"type":"string","enum":["rfc4733","info","inband","auto"]},
                "duration_ms":{"type":"integer","minimum":10,"maximum":10000,"description":"Duration of each digit in ms, 10-10000 (default: config dtmf_duration_ms)."}}),
                &["call_id", "digits"],
            ),
        ),
        MethodInfo::new(
            "call.list",
            "All calls of this run (the last 64 ended ones included), oldest first: id, account, direction, state, remote party, SIP Call-ID, timestamps, end {code, reason, by}, negotiated codec / SRTP suite.",
            obj(
                json!({"account":{"type":"string","description":"Only calls of this account."},
                "active_only":{"type":"boolean","description":"Leave out ended calls."}}),
                &[],
            ),
        ),
        MethodInfo::new(
            "call.get",
            "One call: everything from call.list plus `media` (RTP/SRTP counters, jitter, RTCP, rx_level_dbfs, rx_active) and `audio` (source, sink, device_bound, rx_ms: this media's receive timeline in ms, play {current, queued}, tx_stream: a call.stream TX is attached, rx_tap_dropped_ms: cumulative RX audio the agent hub could not take, normally 0). Errors: not_found.",
            by_id(),
        ),
        MethodInfo::new(
            "call.wait_for",
            "Wait until a call reaches a state (dialing|ringing|early|active|held|remote_held|ended). Give `call_id`, or `account` (any/most recent call of the account, e.g. to catch an incoming call: state `ringing`). Race-free: if the call is already in that state and `since` is omitted it returns at once; otherwise it waits for a call.state event after `since` (default: now; pass the event_seq of the action that triggers it). With `call_id`, the call ending first (or already being over, without the state in the log after `since`) fails at once with invalid_state and the end code/reason. A `since` ahead of the event log (instance restarted) fails with invalid_params. Returns {call, event_seq, matched_seq}. Errors: timeout, invalid_state, not_found.",
            obj(
                json!({"call_id":call_id.clone(),
                "account":{"type":"string","description":"Watch calls of this account instead of one call."},
                "state":{"type":"string","enum":["dialing","ringing","early","active","held","remote_held","ended"]},
                "since":{"type":"integer","minimum":0,"description":"Only consider events with seq greater than this."},
                "timeout_ms":timeout(DEFAULT_WAIT_MS)}),
                &["state"],
            ),
        ),
        MethodInfo::new(
            "call.play",
            "Play audio into the call in place of its own source. Give exactly one of `source` (tone:<hz> | file:<path.wav> | silence | device; relative paths from the config file's directory, decoded before returning: an unreadable file fails with invalid_params; `device` stops all plays and restores the call's own source), `wav_base64` (a WAV, at most 10 MiB decoded) or `pcm_base64` + `pcm_format`. Clips are at most 600 s for `file:` sources and 120 s for inline audio. `wait:true` is capped at 120 s (default timeout min(total + 30 s, 120 s)), so it always times out for clips over about 90 s: for long clips use wait:false and the call.play_finished event (or events.poll). `loop` defaults to true for source specs and false for inline audio; `duration_ms` limits the transmitted time. `queue:true` appends behind the current play (at most 8 queued / 300 s), otherwise it replaces the current play and the queue. A play waits while the call is held, muted or not yet answered; if the answer then recreates the media (call.answer with a different source/sink) it ends with call_ended. `wait:true` (finite plays only: loop:false or duration_ms) returns when it has finished: {finished:{reason: completed|stopped|replaced|barge_in|call_ended|error, played_ms, rx_ms, barge_in_rx_ms (only for barge_in: where the caller's interruption began; use it instead of rx_ms as the next listen's since_rx_ms)}}; on timeout_ms it fails with `timeout` and the error carries play_id while the play continues. `barge_in:true` stops the play when the far end starts speaking. Returns {ok, call_id, play_id, duration_ms (null = endless), queued, rx_ms, event_seq, finished?}; every play ends with exactly one call.play_finished event.",
            obj(
                json!({"call_id":call_id.clone(),
                "source":{"type":"string","description":"tone:<hz> | file:<path.wav> | silence | device"},
                "wav_base64":{"type":"string","description":"Base64 WAV file (any rate / channels; converted)."},
                "pcm_base64":{"type":"string","description":"Base64 raw mono PCM; needs pcm_format."},
                "pcm_format":{"type":"object","properties":{
                    "sample_rate":{"type":"integer","minimum":8000,"maximum":48000},
                    "encoding":{"type":"string","enum":["s16le","f32le","mulaw","alaw"]}},
                    "required":["sample_rate","encoding"],"additionalProperties":false},
                "loop":{"type":"boolean","description":"Repeat until stopped (default true for source, false for inline audio)."},
                "duration_ms":{"type":"integer","minimum":1,"maximum":600000,"description":"Stop after this much transmitted audio."},
                "queue":{"type":"boolean","description":"Append to the play queue instead of replacing (default false)."},
                "wait":{"type":"boolean","description":"Return when the play has finished (finite plays only)."},
                "timeout_ms":{"type":"integer","minimum":0,"maximum":120000,"description":"With wait: give up after this long (default: play length + 30 s, at most 120000)."},
                "barge_in":{"type":"boolean","description":"Stop the play when the far end starts talking (default false; echo paths trigger it themselves)."},
                "gain_db":{"type":"number","minimum":-40,"maximum":20,"description":"Gain in dB (default 0)."}}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "call.stop_play",
            "Stop the current play (or the one with `play_id`; unknown / finished ids are ignored) and, by default, clear the queue; the call's own source resumes when nothing is left. The stopped play ends with call.play_finished {reason:\"stopped\"}.",
            obj(
                json!({"call_id":call_id.clone(),
                "play_id":{"type":"integer","minimum":0,"description":"Only this play (default: the current one)."},
                "clear_queue":{"type":"boolean","description":"Also drop the queued plays (default true)."}}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "call.record_start",
            "Record the audio we receive on this call to a 48 kHz 16-bit WAV file. `stereo`: left = received, right = transmitted.",
            obj(
                json!({"call_id":call_id.clone(),"path":{"type":"string","description":"Output .wav path (relative paths: the config file's directory). The file is created at once; an unwritable path fails with invalid_params."},
                "stereo":{"type":"boolean"}}),
                &["call_id", "path"],
            ),
        ),
        MethodInfo::new(
            "call.record_stop",
            "Finish the recording started with call.record_start; returns {finished, path} once the file is closed.",
            by_id(),
        ),
        MethodInfo::new(
            "call.detect_tone",
            "Listen to the decoded audio received on the call and report whether a sine tone of `freq_hz` is present (Goertzel; returns as soon as it is heard or after timeout_ms). Returns {detected, level_dbfs, power_ratio}. This proves the real RTP audio path end to end.",
            obj(
                json!({"call_id":call_id.clone(),
                "freq_hz":{"type":"number","exclusiveMinimum":0,"exclusiveMaximum":24000},
                "timeout_ms":timeout(DEFAULT_DETECT_MS),
                "min_level_dbfs":{"type":"number","description":"Minimum tone level, default -45 dBFS."}}),
                &["call_id", "freq_hz"],
            ),
        ),
        MethodInfo::new(
            "call.expect_dtmf",
            "Wait until the DTMF digits `digits` arrived in sequence on the call (call.dtmf events, RFC 4733 or INFO). Pass `since` = event_seq returned before the digits were sent, else only digits arriving after this call count. Returns {matched, received}; on timeout the error carries what was received; if the call ends first it fails at once with invalid_state.",
            obj(
                json!({"call_id":call_id.clone(),"digits":{"type":"string","description":"Expected digits, e.g. \"123#\"."},
                "timeout_ms":timeout(DEFAULT_WAIT_MS),
                "since":{"type":"integer","minimum":0,"description":"Count digits with event seq greater than this."}}),
                &["call_id", "digits"],
            ),
        ),
        MethodInfo::new(
            "call.sip_trace",
            "SIP messages (full wire records) of the call's dialog from the in-memory trace ring. Accepts a call id (c1) or a raw SIP Call-ID. Needs sip_trace enabled for the account.",
            obj(
                json!({"call_id":{"type":"string","description":"Call id (c1) or SIP Call-ID header value."}}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "account.set_dnd",
            "Set do-not-disturb for an account at runtime (lasts until the account's `dnd` config value changes). While on, incoming calls are rejected with the account's dnd_reject_code (default 486) and call.rejected{reason:\"dnd\"} is emitted. Emits account.dnd.",
            obj(
                json!({"account":{"type":"string"},"enabled":{"type":"boolean"}}),
                &["account", "enabled"],
            ),
        ),
    ]
}

#[cfg(test)]
mod dial_doc_tests {
    use super::*;

    #[test]
    fn dial_description_names_call_snapshot_and_limits() {
        let m = call_methods();
        let d = m.iter().find(|m| m.name == "call.dial").unwrap();
        assert!(
            d.description.contains("call (full snapshot"),
            "{}",
            d.description
        );
        assert!(d.description.contains("1024"), "{}", d.description);
    }
}
