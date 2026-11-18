//! Built-in control methods, the backend trait and helpers shared by backends.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::protocol::ApiError;
use crate::config::Secret;
use crate::events::{Event, EventFilter, EventLog};
use crate::logging::LoggingHandle;

/// Longest `events.poll` wait.
pub const MAX_POLL_TIMEOUT_MS: u64 = 60_000;
/// Default / maximum events returned per poll.
pub const DEFAULT_POLL_LIMIT: usize = 500;
/// Hard cap on `limit`.
pub const MAX_POLL_LIMIT: usize = 5_000;
/// Hard cap on `logs.tail` lines.
pub const MAX_LOG_LINES: usize = 5_000;

/// Description of one API method, for `methods.list` and the MCP bridge.
#[derive(Debug, Clone, PartialEq, serde::Serialize, Deserialize)]
pub struct MethodInfo {
    /// Method name, e.g. `call.dial`.
    pub name: String,
    /// One-line description (shown to LLMs: say what it does and what it returns).
    pub description: String,
    /// JSON Schema (draft 2020-12) of the params object.
    pub params_schema: Value,
}

impl MethodInfo {
    /// Builds a method description.
    pub fn new(name: &str, description: &str, params_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            params_schema,
        }
    }
}

/// The engine side of the API. The control server handles the built-ins itself and delegates
/// every other method here. Implementations must be cancel-safe: the future is dropped when
/// the client disconnects.
#[async_trait]
pub trait ControlBackend: Send + Sync {
    /// Executes `method`. Return [`ApiError::method_not_found`] for unknown names.
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError>;

    /// Methods this backend provides (with JSON Schemas).
    fn methods(&self) -> Vec<MethodInfo>;

    /// `call.stream`: validates `params` and prepares the stream. The server sends the returned
    /// value as the last JSON line of the connection, then hands the raw byte streams to the
    /// session. Not a regular request: the connection stops speaking JSON afterwards.
    async fn open_stream(
        &self,
        params: Value,
    ) -> Result<(Value, Box<dyn StreamSession>), ApiError> {
        let _ = params;
        Err(ApiError::method_not_found("call.stream"))
    }
}

/// A binary session that takes over a control connection after `call.stream`.
#[async_trait]
pub trait StreamSession: Send {
    /// Runs until the stream ends. `rd` keeps any bytes the client pipelined behind the request
    /// line; `conn` is cancelled on server shutdown or endpoint retirement. Dropping `wr`
    /// closes the connection.
    async fn run(
        self: Box<Self>,
        rd: Box<dyn AsyncRead + Send + Unpin>,
        wr: Box<dyn AsyncWrite + Send + Unpin>,
        conn: CancellationToken,
    );

    /// The connection died before [`StreamSession::run`] could start (the result line could not
    /// be written, or the write half could not be recovered). Releases the session; the default
    /// just drops it. `reason` is the `closed` reason (`"connection_lost"`).
    async fn abort(self: Box<Self>, reason: &'static str) {
        let _ = reason;
    }
}

/// Authenticated request lines above [`super::server::MAX_LINE_BYTES`] processed at once.
pub const MAX_BIG_REQUESTS: usize = 4;

/// How long a request line larger than [`super::server::MAX_LINE_BYTES`] waits for a
/// big-request permit before the line is discarded and answered with an error.
pub const BIG_PERMIT_WAIT: Duration = Duration::from_secs(30);
/// How long a partly received big line may stall (no bytes at all) before the connection is
/// closed.
pub const BIG_READ_IDLE: Duration = Duration::from_secs(10);
/// Overall time a big line may take to arrive once it holds a big-request permit, however
/// steadily bytes trickle in; the connection is closed on expiry. With [`BIG_READ_IDLE`] this
/// bounds how long a slow client can hold a permit.
pub const BIG_READ_TOTAL: Duration = Duration::from_secs(60);

tokio::task_local! {
    /// The big-request permit of the request being served, if its line exceeded 1 MiB.
    pub(super) static BIG_PERMIT: RefCell<Option<OwnedSemaphorePermit>>;
}

/// Releases the big-request permit (see [`MAX_BIG_REQUESTS`]) held by the request currently
/// being served, if any.
///
/// The permit bounds the memory of request lines above 1 MiB (inline audio). It is held while
/// the line is buffered and parsed; a backend that decodes big params (e.g. base64 audio in
/// `call.play` / `call.say` / `call.transcribe`) should call this right after decoding and
/// dropping the encoded form, so that a long-running request does not keep the slot. If it is
/// never called, the permit is released when the request finishes. No-op when the request had
/// no permit or when called outside a control request.
pub fn release_big_request_permit() {
    let _ = BIG_PERMIT.try_with(|p| drop(p.borrow_mut().take()));
}

/// Minimal backend for tests and `run --headless` until the real engine exists:
/// `status` (fixed object) and `echo` (returns its params).
#[derive(Debug, Default, Clone)]
pub struct StubBackend;

#[async_trait]
impl ControlBackend for StubBackend {
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "status" => Ok(json!({
                "stub": true,
                "version": crate::update::full_version(),
                "accounts": [],
                "calls": [],
            })),
            "echo" => Ok(params),
            _ => Err(ApiError::method_not_found(method)),
        }
    }

    fn methods(&self) -> Vec<MethodInfo> {
        vec![
            MethodInfo::new(
                "status",
                "Overall state: accounts, calls, audio, config.",
                json!({"type":"object","properties":{},"additionalProperties":false}),
            ),
            MethodInfo::new(
                "echo",
                "Returns its params unchanged (diagnostics).",
                json!({"type":"object"}),
            ),
        ]
    }
}

/// Everything a connection needs to serve requests.
#[derive(Clone)]
pub struct ControlContext {
    /// The event bus.
    pub events: Arc<EventLog>,
    /// Engine methods.
    pub backend: Arc<dyn ControlBackend>,
    /// For `logs.tail`; `None` makes it fail with `internal`.
    pub logging: Option<Arc<LoggingHandle>>,
    /// Per-run token (`runtime.json`). Mandatory on TCP; if a client sends one it must match.
    pub token: Secret,
    /// Permits for request lines larger than 1 MiB (bounds memory held by inline audio).
    pub big_requests: Arc<Semaphore>,
    /// Longest wait for a big-request permit ([`BIG_PERMIT_WAIT`]; overridable in tests).
    pub big_permit_wait: Duration,
    /// Longest stall while receiving a big line ([`BIG_READ_IDLE`]; overridable in tests).
    pub big_read_idle: Duration,
    /// Longest total time to receive a big line ([`BIG_READ_TOTAL`]; overridable in tests).
    pub big_read_total: Duration,
    /// The `build` object reported by `hello`.
    pub build: Value,
}

impl ControlContext {
    /// New context.
    pub fn new(
        events: Arc<EventLog>,
        backend: Arc<dyn ControlBackend>,
        logging: Option<Arc<LoggingHandle>>,
        token: Secret,
    ) -> Self {
        Self {
            events,
            backend,
            logging,
            token,
            big_requests: Arc::new(Semaphore::new(MAX_BIG_REQUESTS)),
            big_permit_wait: BIG_PERMIT_WAIT,
            big_read_idle: BIG_READ_IDLE,
            big_read_total: BIG_READ_TOTAL,
            build: crate::build::build_info(siple_config::channel::current()),
        }
    }

    /// Reports `channel` in `hello.build` instead of the process's own.
    pub fn with_channel(mut self, channel: &siple_config::channel::Channel) -> Self {
        self.build = crate::build::build_info(channel);
        self
    }

    /// Built-in methods plus the backend's, for `methods.list`.
    pub fn all_methods(&self) -> Vec<MethodInfo> {
        let mut m = builtin_methods();
        m.extend(self.backend.methods());
        m
    }

    /// Runs a method that needs no per-connection state (everything but `hello` and
    /// `events.subscribe|unsubscribe`, which the server handles), else delegates to the backend.
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "ping" => Ok(json!({ "pong": true })),
            "methods.list" => Ok(json!({ "methods": self.all_methods() })),
            "events.current_seq" => Ok(json!({ "seq": self.events.current_seq() })),
            "events.poll" => self.events_poll(params).await,
            "logs.tail" => self.logs_tail(params),
            _ => {
                let mut v = self.backend.call(method, params).await?;
                // `event_seq` is only meaningful together with the instance that issued it.
                if let Value::Object(m) = &mut v
                    && m.contains_key("event_seq")
                {
                    m.insert("instance_id".into(), self.events.instance_id().into());
                }
                Ok(v)
            }
        }
    }

    async fn events_poll(&self, params: Value) -> Result<Value, ApiError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            since: Option<u64>,
            timeout_ms: Option<u64>,
            limit: Option<usize>,
            kinds: Option<Vec<String>>,
            account: Option<String>,
            call_id: Option<String>,
        }
        let p: P = parse_params(params)?;
        if let Some(s) = p.since {
            check_since(&self.events, s)?;
        }
        let since = p.since.unwrap_or_else(|| self.events.current_seq());
        let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(0).min(MAX_POLL_TIMEOUT_MS));
        let limit = p
            .limit
            .unwrap_or(DEFAULT_POLL_LIMIT)
            .clamp(1, MAX_POLL_LIMIT);
        let filter = EventFilter {
            kinds: p.kinds,
            account: p.account,
            call_id: p.call_id,
        };
        let r = self.events.poll(since, &filter, limit, timeout).await;
        Ok(serde_json::to_value(r).expect("PollResult serializes"))
    }

    fn logs_tail(&self, params: Value) -> Result<Value, ApiError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            lines: Option<usize>,
            account: Option<String>,
        }
        let p: P = parse_params(params)?;
        let log = self
            .logging
            .as_ref()
            .ok_or_else(|| ApiError::internal("logging is not available"))?;
        let n = p.lines.unwrap_or(100).clamp(1, MAX_LOG_LINES);
        let lines = match p.account {
            // Filter before truncating so `lines` matching lines come back when they exist.
            Some(a) => {
                let mut v: Vec<String> = log
                    .tail(crate::logging::RING_CAPACITY)
                    .into_iter()
                    .filter(|l| line_has_account(l, &a))
                    .collect();
                let skip = v.len().saturating_sub(n);
                v.drain(..skip);
                v
            }
            None => log.tail(n),
        };
        Ok(json!({ "lines": lines }))
    }
}

/// Deserializes request params; absent (`null`) params mean an empty object.
pub fn parse_params<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, ApiError> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| ApiError::invalid_params(e.to_string()))
}

/// Rejects a `since` that is ahead of the event log. That only happens when the caller holds a
/// position from a previous run of the application (the log restarts at 0), and silently
/// waiting for events that will never come would hang it.
pub fn check_since(log: &EventLog, since: u64) -> Result<(), ApiError> {
    let current = log.current_seq();
    if since > current {
        return Err(ApiError::invalid_params(format!(
            "since ({since}) is ahead of this instance's event log (current seq {current}) — \
             the instance probably restarted; call events.current_seq"
        ))
        .with_extra(json!({ "current_seq": current, "instance_id": log.instance_id() })));
    }
    Ok(())
}

/// Waits for the first event with `seq > since` that passes `filter` and `predicate`,
/// including events already in the log (so capture `current_seq()` *before* acting and the
/// wait cannot miss a fast reaction). Errors with `timeout` after `timeout`.
///
/// If the ring wrapped past `since` meanwhile, the lost events cannot be examined.
pub async fn wait_for(
    log: &EventLog,
    since: u64,
    filter: &EventFilter,
    predicate: impl Fn(&Event) -> bool,
    timeout: Duration,
) -> Result<Event, ApiError> {
    check_since(log, since)?;
    let deadline = Instant::now() + timeout;
    let mut since = since;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let r = log.poll(since, filter, MAX_POLL_LIMIT, remaining).await;
        if let Some(e) = r.events.iter().find(|e| predicate(e)) {
            return Ok(e.clone());
        }
        since = r.next_seq;
        if Instant::now() >= deadline {
            return Err(ApiError::timeout(format!(
                "no matching event within {} ms",
                timeout.as_millis()
            )));
        }
    }
}

fn filter_props() -> Value {
    json!({
        "kinds": {"type":"array","items":{"type":"string"},
            "description":"Event kinds; exact (`call.state`) or prefix with trailing `*` (`call.*`)."},
        "account": {"type":"string","description":"Only events of this account id."},
        "call_id": {"type":"string","description":"Only events of this call id."}
    })
}

/// Schemas of the methods the control server implements itself.
pub fn builtin_methods() -> Vec<MethodInfo> {
    let mut poll_props = filter_props();
    poll_props["since"] = json!({"type":"integer","minimum":0,
        "description":"Return events with seq greater than this. Default: current seq (only new events)."});
    poll_props["timeout_ms"] = json!({"type":"integer","minimum":0,"maximum":MAX_POLL_TIMEOUT_MS,
        "description":"Long-poll: wait up to this long for a matching event. Default 0 (no wait)."});
    poll_props["limit"] = json!({"type":"integer","minimum":1,"maximum":MAX_POLL_LIMIT,
        "description":"Maximum events to return (default 500)."});
    vec![
        MethodInfo::new(
            "hello",
            "Handshake; must be the first message. Returns server version, current event seq, `instance_id` / `started_at` and `build` ({channel: direct|mas|msix, sandboxed, store, version}: which edition answers) (a different instance_id than before means the application restarted: event seqs and call ids reset).",
            json!({"type":"object","properties":{
                "token":{"type":"string","description":"Per-run token from runtime.json (mandatory on TCP)."},
                "client":{"type":"string","description":"Client name for logs."}},
                "additionalProperties":false}),
        ),
        MethodInfo::new(
            "ping",
            "Liveness check; returns {pong:true}.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        MethodInfo::new(
            "methods.list",
            "Lists all methods with descriptions and JSON Schemas for their params.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        MethodInfo::new(
            "events.poll",
            "Long-poll the event log. Returns {events, next_seq, truncated}; pass next_seq as `since` on the next call. A `since` ahead of the log (the instance restarted) fails with invalid_params; call events.current_seq.",
            json!({"type":"object","properties":poll_props,"additionalProperties":false}),
        ),
        MethodInfo::new(
            "events.subscribe",
            "Start receiving matching events as `event` notifications on this connection (replaces any previous subscription). Returns the current seq.",
            json!({"type":"object","properties":filter_props(),"additionalProperties":false}),
        ),
        MethodInfo::new(
            "events.unsubscribe",
            "Stop the push subscription on this connection.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        MethodInfo::new(
            "events.current_seq",
            "Seq of the most recent event; use as `since` baseline before acting.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        MethodInfo::new(
            "logs.tail",
            "Recent application log lines (oldest first).",
            json!({"type":"object","properties":{
                "lines":{"type":"integer","minimum":1,"maximum":MAX_LOG_LINES,"description":"Default 100."},
                "account":{"type":"string","description":"Only lines tagged with this account id."}},
                "additionalProperties":false}),
        ),
    ]
}

/// Whether a log line carries the field `account=<id>` (or `account="<id>"`) as a whole
/// value: the key starts a token and the value is followed by a delimiter or the line end,
/// so `account=a1` does not match `account=a10`.
fn line_has_account(line: &str, id: &str) -> bool {
    const KEY: &str = "account=";
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(i) = line[from..].find(KEY) {
        let start = from + i;
        from = start + KEY.len();
        if start > 0 && !matches!(bytes[start - 1], b' ' | b'{' | b',' | b'(' | b'\t') {
            continue;
        }
        let rest = &line[from..];
        let ok = match rest.strip_prefix('"') {
            Some(q) => q.strip_prefix(id).is_some_and(|r| r.starts_with('"')),
            None => rest.strip_prefix(id).is_some_and(|r| {
                r.chars()
                    .next()
                    .is_none_or(|c| matches!(c, ' ' | ',' | '}' | ')' | ';' | '\t'))
            }),
        };
        if ok {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod account_filter_tests {
    use super::line_has_account;

    #[test]
    fn matches_whole_values_only() {
        assert!(line_has_account("INFO x: registered account=a1", "a1"));
        assert!(line_has_account("INFO x: account=a1 state=ok", "a1"));
        assert!(line_has_account("INFO x{account=a1}: msg", "a1"));
        assert!(line_has_account("INFO x{account=a1,call=2}: msg", "a1"));
        assert!(line_has_account("INFO hello account=\"a1\"", "a1"));
        assert!(!line_has_account("INFO account=a10 state=ok", "a1"));
        assert!(!line_has_account("INFO account=\"a10\"", "a1"));
        assert!(!line_has_account("INFO subaccount=a1", "a1"));
        assert!(!line_has_account("INFO nothing", "a1"));
        // A later occurrence still matches.
        assert!(line_has_account("account=a10 then account=a1", "a1"));
    }
}
