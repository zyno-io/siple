//! JSON-RPC 2.0 wire types for the control API (newline-delimited JSON).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::events::Event;

/// Protocol revision reported by `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Fixed error kinds plus the JSON-RPC framework ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Unknown account/call/etc.
    NotFound,
    /// Valid request, wrong state for it.
    InvalidState,
    /// Missing or malformed parameters.
    InvalidParams,
    /// Missing/incorrect token, or `hello` not done.
    Unauthorized,
    /// SIP-level failure; `sip_code` carries the response code.
    SipError,
    /// A wait expired.
    Timeout,
    /// Bug or unexpected failure.
    Internal,
    /// Line was JSON but not a JSON-RPC request.
    InvalidRequest,
    /// Unknown method.
    MethodNotFound,
    /// Unparseable JSON (the server closes the connection instead of replying).
    ParseError,
    /// The feature is not available in this edition (e.g. `[speech]` hooks in the Mac App Store
    /// build).
    Unsupported,
}

impl ErrorKind {
    /// JSON-RPC error code.
    pub fn code(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::Internal => -32603,
            Self::NotFound => -32001,
            Self::InvalidState => -32002,
            Self::Unauthorized => -32003,
            Self::SipError => -32004,
            Self::Timeout => -32005,
            Self::Unsupported => -32006,
        }
    }

    /// The `data.kind` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::InvalidState => "invalid_state",
            Self::InvalidParams => "invalid_params",
            Self::Unauthorized => "unauthorized",
            Self::SipError => "sip_error",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
            Self::InvalidRequest => "invalid_request",
            Self::MethodNotFound => "method_not_found",
            Self::ParseError => "parse_error",
            Self::Unsupported => "unsupported",
        }
    }
}

/// An API failure, convertible to/from the JSON-RPC `error` object.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{}: {message}", kind.as_str())]
pub struct ApiError {
    /// Machine-readable kind.
    pub kind: ErrorKind,
    /// Human-readable detail.
    pub message: String,
    /// SIP response code, for [`ErrorKind::SipError`].
    pub sip_code: Option<u16>,
    /// Extra fields merged into `error.data`.
    pub extra: Option<Value>,
}

/// Longest error message (chars) sent verbatim; longer ones keep their start plus a length note,
/// so serde/path messages that quote caller-supplied strings stay bounded.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 280;

fn cap_message(m: String) -> String {
    let n = m.chars().count();
    if n <= MAX_ERROR_MESSAGE_CHARS {
        return m;
    }
    let mut out: String = m.chars().take(MAX_ERROR_MESSAGE_CHARS).collect();
    out.push_str(&format!("… ({n} chars)"));
    out
}

impl ApiError {
    /// Error of `kind`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: cap_message(message.into()),
            sip_code: None,
            extra: None,
        }
    }
    /// `not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }
    /// `invalid_state`.
    pub fn invalid_state(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidState, message)
    }
    /// `invalid_params`.
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidParams, message)
    }
    /// `unauthorized`.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unauthorized, message)
    }
    /// `sip_error` with the SIP status code.
    pub fn sip_error(code: u16, message: impl Into<String>) -> Self {
        Self {
            sip_code: Some(code),
            ..Self::new(ErrorKind::SipError, message)
        }
    }
    /// `timeout`.
    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, message)
    }
    /// `internal`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Internal, message)
    }
    /// `method_not_found`.
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            ErrorKind::MethodNotFound,
            format!("unknown method `{method}`"),
        )
    }
    /// Attaches extra `data` fields (an object is merged; anything else goes under `detail`).
    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }

    /// Attaches the plain-language `problem` to `error.data`, keeping other extra fields.
    pub fn with_problem(mut self, problem: crate::problem::Problem) -> Self {
        let p = serde_json::to_value(problem).unwrap_or(Value::Null);
        match &mut self.extra {
            Some(Value::Object(m)) => {
                m.insert("problem".into(), p);
            }
            Some(other) => {
                let detail = other.take();
                self.extra = Some(json!({ "detail": detail, "problem": p }));
            }
            None => self.extra = Some(json!({ "problem": p })),
        }
        self
    }

    /// The JSON-RPC error object.
    pub fn to_wire(&self) -> Value {
        let mut data = json!({ "kind": self.kind.as_str() });
        if let Some(c) = self.sip_code {
            data["sip_code"] = c.into();
        }
        match &self.extra {
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    data[k] = v.clone();
                }
            }
            Some(other) => data["detail"] = other.clone(),
            None => {}
        }
        json!({ "code": self.kind.code(), "message": self.message, "data": data })
    }

    /// Parses a JSON-RPC error object (client side). Unknown kinds map to `internal`.
    pub fn from_wire(v: &Value) -> Self {
        let message = v["message"].as_str().unwrap_or("").to_string();
        let kind = v["data"]["kind"]
            .as_str()
            .and_then(|k| serde_json::from_value(Value::String(k.into())).ok())
            .unwrap_or(ErrorKind::Internal);
        let sip_code = v["data"]["sip_code"]
            .as_u64()
            .and_then(|c| u16::try_from(c).ok());
        let extra = match &v["data"] {
            Value::Object(m) => {
                let rest: serde_json::Map<_, _> = m
                    .iter()
                    .filter(|(k, _)| *k != "kind" && *k != "sip_code")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                (!rest.is_empty()).then_some(Value::Object(rest))
            }
            _ => None,
        };
        Self {
            kind,
            message,
            sip_code,
            extra,
        }
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        Self::invalid_params(e.to_string())
    }
}

/// An incoming request or notification (no `id`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// Must be `"2.0"`.
    pub jsonrpc: String,
    /// Request id (number or string); absent for notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Method name.
    pub method: String,
    /// Params; `null` when absent.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl Request {
    /// A request with `id`.
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id.into()),
            method: method.into(),
            params,
        }
    }

    /// Validates a parsed JSON value as a request.
    pub fn from_value(v: Value) -> Result<Self, ApiError> {
        if !v.is_object() {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "expected a JSON-RPC request object (batches are not supported)",
            ));
        }
        let r: Request = serde_json::from_value(v)
            .map_err(|e| ApiError::new(ErrorKind::InvalidRequest, e.to_string()))?;
        if r.jsonrpc != "2.0" {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "jsonrpc must be \"2.0\"",
            ));
        }
        if let Some(id) = &r.id
            && !(id.is_number() || id.is_string() || id.is_null())
        {
            return Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "id must be a number or string",
            ));
        }
        Ok(r)
    }
}

/// Encodes a success response line (no trailing newline).
pub fn encode_result(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// Encodes an error response line (no trailing newline).
pub fn encode_error(id: &Value, err: &ApiError) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": err.to_wire() }).to_string()
}

/// Encodes a pushed event notification line (no trailing newline).
pub fn encode_event(event: &Event) -> String {
    json!({ "jsonrpc": "2.0", "method": "event", "params": event }).to_string()
}

/// A line received by a client.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Response to request `id`.
    Response {
        /// Echoed id.
        id: Value,
        /// Outcome.
        result: Result<Value, ApiError>,
    },
    /// Pushed event.
    Event(Event),
}

/// Decodes a server line.
pub fn decode_incoming(line: &str) -> Result<Incoming, ApiError> {
    let v: Value = serde_json::from_str(line)
        .map_err(|e| ApiError::new(ErrorKind::ParseError, e.to_string()))?;
    if v.get("method").and_then(Value::as_str) == Some("event") {
        let ev = serde_json::from_value(v["params"].clone())
            .map_err(|e| ApiError::new(ErrorKind::InvalidRequest, e.to_string()))?;
        return Ok(Incoming::Event(ev));
    }
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    if let Some(e) = v.get("error") {
        return Ok(Incoming::Response {
            id,
            result: Err(ApiError::from_wire(e)),
        });
    }
    match v.get("result") {
        Some(r) => Ok(Incoming::Response {
            id,
            result: Ok(r.clone()),
        }),
        None => Err(ApiError::new(
            ErrorKind::InvalidRequest,
            "neither result nor error",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_round_trips_through_the_wire() {
        let e = ApiError::new(ErrorKind::Unsupported, "not in this edition");
        let w = e.to_wire();
        assert_eq!(w["code"], -32006);
        assert_eq!(w["data"]["kind"], "unsupported");
        assert_eq!(ErrorKind::Unsupported.as_str(), "unsupported");
        assert_eq!(ApiError::from_wire(&w), e);
    }

    #[test]
    fn with_problem_survives_the_wire_and_keeps_the_kind() {
        use crate::problem::Problem;
        let e = ApiError::sip_error(486, "486 Busy Here")
            .with_extra(json!({"call": "c1"}))
            .with_problem(Problem::Busy);
        let w = e.to_wire();
        assert_eq!(w["data"]["kind"], "sip_error");
        assert_eq!(w["data"]["sip_code"], 486);
        assert_eq!(w["data"]["call"], "c1");
        assert_eq!(w["data"]["problem"]["kind"], "busy");
        assert_eq!(w["data"]["problem"]["title"], "Busy");
        let back = ApiError::from_wire(&w);
        assert_eq!(back, e);
        assert_eq!(back.to_wire(), w);
        let bare = ApiError::timeout("t").with_problem(Problem::NoResponse);
        assert_eq!(bare.to_wire()["data"]["problem"]["kind"], "no_response");
    }

    #[test]
    fn request_parsing() {
        let r = Request::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping"})).unwrap();
        assert_eq!(r.method, "ping");
        assert!(r.params.is_null());
        let r =
            Request::from_value(json!({"jsonrpc":"2.0","method":"x","params":{"a":1}})).unwrap();
        assert!(r.id.is_none());
        for bad in [
            json!([1]),
            json!({"jsonrpc":"1.0","id":1,"method":"x"}),
            json!({"jsonrpc":"2.0","id":1}),
            json!({"jsonrpc":"2.0","id":{"a":1},"method":"x"}),
            json!(5),
        ] {
            let e = Request::from_value(bad).unwrap_err();
            assert_eq!(e.kind, ErrorKind::InvalidRequest);
        }
    }

    #[test]
    fn error_wire_roundtrip() {
        let e = ApiError::sip_error(486, "Busy Here").with_extra(json!({"call_id":"c1"}));
        let w = e.to_wire();
        assert_eq!(w["code"], -32004);
        assert_eq!(w["data"]["kind"], "sip_error");
        assert_eq!(w["data"]["sip_code"], 486);
        assert_eq!(w["data"]["call_id"], "c1");
        assert_eq!(ApiError::from_wire(&w), e);
        for k in [
            ErrorKind::NotFound,
            ErrorKind::InvalidState,
            ErrorKind::InvalidParams,
            ErrorKind::Unauthorized,
            ErrorKind::Timeout,
            ErrorKind::Internal,
            ErrorKind::MethodNotFound,
        ] {
            let e = ApiError::new(k, "m");
            assert_eq!(ApiError::from_wire(&e.to_wire()).kind, k);
        }
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn response_encoding_decoding() {
        let line = encode_result(&json!(7), json!({"ok":true}));
        assert!(!line.contains('\n'));
        assert_eq!(
            decode_incoming(&line).unwrap(),
            Incoming::Response {
                id: json!(7),
                result: Ok(json!({"ok":true}))
            }
        );
        let line = encode_error(&json!("a"), &ApiError::not_found("nope"));
        match decode_incoming(&line).unwrap() {
            Incoming::Response { id, result: Err(e) } => {
                assert_eq!(id, json!("a"));
                assert_eq!(e.kind, ErrorKind::NotFound);
            }
            other => panic!("{other:?}"),
        }
        let ev = Event {
            seq: 3,
            ts: "t".into(),
            kind: "call.state".into(),
            account: None,
            call_id: Some("c1".into()),
            data: json!({"state":"active","mos":4.2133333333333335,"jitter_ms":0.1,"x":-29.612384796142578}),
        };
        let line = encode_event(&ev);
        assert!(line.contains("\"method\":\"event\""));
        assert_eq!(decode_incoming(&line).unwrap(), Incoming::Event(ev));
        assert!(decode_incoming("not json").is_err());
    }
}
