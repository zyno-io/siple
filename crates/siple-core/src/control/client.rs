//! Minimal async control-API client (used by `siple-cli ctl|events`, later the MCP bridge).

use std::collections::VecDeque;
#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::protocol::{ApiError, Incoming, Request, decode_incoming};
use crate::events::Event;

type BoxRead = Box<dyn AsyncRead + Send + Unpin>;
type BoxWrite = Box<dyn AsyncWrite + Send + Unpin>;

/// Client-side failure.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Transport failure.
    #[error("connection error: {0}")]
    Io(#[from] std::io::Error),
    /// Server sent something that is not valid protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server answered with an API error.
    #[error("{0}")]
    Api(ApiError),
    /// The server closed the connection.
    #[error("connection closed by server")]
    Closed,
    /// No response within the request's time limit. The request may still have been executed.
    #[error("no response to `{method}` within {secs} s (the request may still have run)")]
    Timeout {
        /// The method.
        method: String,
        /// The limit that elapsed, in whole seconds.
        secs: u64,
    },
}

/// Time limit for methods that are not waits.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum limit of the waiting methods; the actual limit is the requested `timeout_ms` plus a
/// margin when that is larger.
pub const WAIT_REQUEST_TIMEOUT: Duration = Duration::from_secs(65);

/// Limit of `call.hold`, `call.unhold` and `call.transfer*`: they wait for a re-INVITE or REFER
/// that may end in Timer B (32 s), on top of a dialog-confirm wait and glare retries.
const HOLD_REQUEST_TIMEOUT: Duration = Duration::from_secs(50);

/// Limit of `call.answer`, `call.dial`, `call.pickup`, `blf.dial` and `call.activate`: they may put several other lines on
/// hold first (8 s each, then a bounded retry) and `end_others` waits up to 5 s for the ACK.
const LINE_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Worst-case time of one TTS / STT command (the configured maximum) plus a 5 s margin. All
/// speech-related allowances below derive from it so they cannot drift from the config limit.
const SPEECH_JOB_ALLOWANCE: Duration =
    Duration::from_millis(siple_config::SPEECH_TIMEOUT_MAX_MS as u64 + 5_000);
/// Extra time `call.say` may spend in the TTS command.
const SAY_EXTRA: Duration = SPEECH_JOB_ALLOWANCE;
/// Extra time for an STT command run inside a request.
const TRANSCRIBE_EXTRA: Duration = SPEECH_JOB_ALLOWANCE;
/// Limit of `speech.*` (TTS or STT plus margin).
const SPEECH_REQUEST_TIMEOUT: Duration = SPEECH_JOB_ALLOWANCE;

/// How long [`ControlClient::call`] waits for the response to `method`: waiting methods
/// (`events.poll`, `call.wait_for`, `call.detect_tone`, `call.expect_dtmf`) get
/// `max(65 s, timeout_ms + 5 s)`, `call.dtmf` gets time for its digits, `call.listen` its
/// wall-clock limit plus 10 s (`start_timeout_ms + max_ms + 15 s` by default; plus time for
/// transcription), `call.play` / `call.say` with `wait` their
/// `timeout_ms`, `call.rx_audio` 30 s (plus STT time with `transcribe`), `speech.*` 125 s, `call.hold` / `call.unhold` / `call.transfer*` 50 s, `call.answer` / `call.dial` / `call.pickup` / `blf.dial` / `call.activate` / `call.consult_start` / `transfer.complete|swap|cancel` 60 s, everything else 30 s.
/// A stuck request therefore cannot wedge a client forever.
pub fn request_timeout(method: &str, params: &Value) -> Duration {
    match method {
        "events.poll" | "call.wait_for" | "call.detect_tone" | "call.expect_dtmf" => {
            let ms = params["timeout_ms"].as_u64().unwrap_or(0);
            WAIT_REQUEST_TIMEOUT.max(Duration::from_millis(ms.saturating_add(5_000)))
        }
        "call.dtmf" => {
            let digits = params["digits"].as_str().map_or(0, |d| d.len()) as u64;
            let per_digit = params["duration_ms"].as_u64().unwrap_or(1_000).min(10_000) + 5_000;
            DEFAULT_REQUEST_TIMEOUT + Duration::from_millis(digits.min(64) * per_digit)
        }
        "call.listen" => {
            // The server's wall limit is `timeout_ms`, else start_timeout + max + 5 s, and its
            // outer timeout adds another 5 s: stay at least 5 s beyond that.
            let ms = params["timeout_ms"].as_u64().map_or_else(
                || {
                    (params["start_timeout_ms"].as_u64().unwrap_or(8_000))
                        .saturating_add(params["max_ms"].as_u64().unwrap_or(30_000))
                        .saturating_add(15_000)
                },
                |t| t.saturating_add(10_000),
            );
            let base = WAIT_REQUEST_TIMEOUT.max(Duration::from_millis(ms));
            if params["transcribe"].as_bool().unwrap_or(false) {
                base + TRANSCRIBE_EXTRA
            } else {
                base
            }
        }
        "call.rx_audio" => {
            // A snapshot returns at once; only the server-side STT can take long.
            if params["transcribe"].as_bool().unwrap_or(false) {
                DEFAULT_REQUEST_TIMEOUT + TRANSCRIBE_EXTRA
            } else {
                DEFAULT_REQUEST_TIMEOUT
            }
        }
        // Server side: `hold()` has an overall budget of 44 s (confirm wait, attempts, 491
        // backoff); a transfer holds first (6 s) and then waits for the REFER answer (32 s).
        "call.hold" | "call.unhold" => HOLD_REQUEST_TIMEOUT,
        // Line operations may hold several other lines first (8 s each) and wait for an ACK.
        // Consult and session operations take the coordinator, hold lines (8 s each) and, for
        // `transfer.complete`, wait for the REFER answer.
        "call.answer" | "call.dial" | "call.activate" | "call.pickup" | "blf.dial"
        | "call.consult_start" | "transfer.complete" | "transfer.swap" | "transfer.cancel" => {
            LINE_REQUEST_TIMEOUT
        }
        m if m.starts_with("call.transfer") => HOLD_REQUEST_TIMEOUT,
        "call.play" | "call.say" => {
            let say = if method == "call.say" {
                SAY_EXTRA
            } else {
                Duration::ZERO
            };
            if params["wait"].as_bool().unwrap_or(false) {
                let ms = params["timeout_ms"].as_u64().unwrap_or(120_000);
                DEFAULT_REQUEST_TIMEOUT.max(Duration::from_millis(ms.saturating_add(5_000))) + say
            } else {
                DEFAULT_REQUEST_TIMEOUT + say
            }
        }
        m if m.starts_with("speech.") => SPEECH_REQUEST_TIMEOUT,
        _ => DEFAULT_REQUEST_TIMEOUT,
    }
}

/// Whether repeating `method` after a lost connection is harmless: pure reads. State-changing
/// methods (`call.dial`, `call.dtmf`, `config.reload`, ...) must never be retried blindly, since
/// the first attempt may have been executed before the connection broke.
pub fn is_idempotent(method: &str) -> bool {
    matches!(
        method,
        "status"
            | "ping"
            | "methods.list"
            | "events.poll"
            | "events.current_seq"
            | "logs.tail"
            | "config.get"
            | "audio.devices"
            | "call.sip_trace"
            | "call.rx_audio"
            | "speech.transcribe"
    ) || method.ends_with(".list")
        || method.ends_with(".get")
}

/// The halves of an upgraded connection ([`ControlClient::into_stream`]).
pub struct ClientStream {
    /// The `call.stream` result: `stream_id`, `sample_rate`, `encoding`, `frame_bytes`, ...
    pub info: Value,
    /// Reader (keeps pipelined bytes).
    pub rd: BufReader<Box<dyn AsyncRead + Send + Unpin>>,
    /// Writer.
    pub wr: Box<dyn AsyncWrite + Send + Unpin>,
}

/// One connection to a control server. Requests are sequential (one in flight); pushed events
/// that arrive while waiting for a response are queued for [`next_event`](Self::next_event).
pub struct ControlClient {
    rd: BufReader<BoxRead>,
    wr: BoxWrite,
    next_id: u64,
    queued: VecDeque<Event>,
    hello: Option<Value>,
}

impl ControlClient {
    fn from_parts(rd: BoxRead, wr: BoxWrite) -> Self {
        Self {
            rd: BufReader::new(rd),
            wr,
            next_id: 1,
            queued: VecDeque::new(),
            hello: None,
        }
    }

    /// Wraps any duplex stream.
    pub fn from_stream<S>(s: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, w) = tokio::io::split(s);
        Self::from_parts(Box::new(r), Box::new(w))
    }

    /// Connects to a unix socket.
    #[cfg(unix)]
    pub async fn connect_unix(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::from_stream(
            tokio::net::UnixStream::connect(path).await?,
        ))
    }

    /// Connects to a named pipe, retrying briefly while all instances are busy.
    #[cfg(windows)]
    pub async fn connect_pipe(name: &str) -> Result<Self, ClientError> {
        use tokio::net::windows::named_pipe::ClientOptions;
        // ERROR_PIPE_BUSY
        const BUSY: i32 = 231;
        let mut tries = 0;
        loop {
            match ClientOptions::new().open(name) {
                Ok(c) => return Ok(Self::from_stream(c)),
                Err(e) if e.raw_os_error() == Some(BUSY) && tries < 20 => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Connects over loopback TCP.
    pub async fn connect_tcp(addr: &str) -> Result<Self, ClientError> {
        let s = tokio::net::TcpStream::connect(addr).await?;
        let _ = s.set_nodelay(true);
        Ok(Self::from_stream(s))
    }

    /// Connects to whichever endpoint of `ep` is native to this platform (falling back to TCP).
    pub async fn connect_endpoints(
        ep: &crate::instance::ControlEndpoints,
    ) -> Result<Self, ClientError> {
        #[cfg(unix)]
        if let Some(p) = &ep.unix_socket {
            return Self::connect_unix(Path::new(p)).await;
        }
        #[cfg(windows)]
        if let Some(n) = &ep.named_pipe {
            return Self::connect_pipe(n).await;
        }
        if let Some(a) = &ep.tcp {
            return Self::connect_tcp(a).await;
        }
        Err(ClientError::Protocol("no usable control endpoint".into()))
    }

    async fn send(&mut self, req: &Request) -> Result<(), ClientError> {
        let mut line = serde_json::to_string(req).expect("request serializes");
        line.push('\n');
        self.wr.write_all(line.as_bytes()).await?;
        self.wr.flush().await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Incoming, ClientError> {
        let mut line = String::new();
        if self.rd.read_line(&mut line).await? == 0 {
            return Err(ClientError::Closed);
        }
        decode_incoming(line.trim_end()).map_err(|e| ClientError::Protocol(e.message))
    }

    /// Performs `hello`. Returns the server's result.
    pub async fn hello(&mut self, token: Option<&str>, client: &str) -> Result<Value, ClientError> {
        let mut p = json!({ "client": client });
        if let Some(t) = token {
            p["token"] = t.into();
        }
        let r = self.call("hello", p).await?;
        self.hello = Some(r.clone());
        Ok(r)
    }

    /// The result of the last successful [`hello`](Self::hello): server version, `instance_id`,
    /// `started_at`, `current_seq`.
    pub fn server_info(&self) -> Option<&Value> {
        self.hello.as_ref()
    }

    /// `instance_id` from the handshake; changes whenever the application restarts.
    pub fn instance_id(&self) -> Option<&str> {
        self.hello.as_ref()?["instance_id"].as_str()
    }

    /// Sends a request and waits for its response, at most [`request_timeout`].
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let limit = request_timeout(method, &params);
        self.call_timeout(method, params, limit).await
    }

    /// Like [`call`](Self::call) with an explicit time limit.
    pub async fn call_timeout(
        &mut self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, ClientError> {
        match tokio::time::timeout(limit, self.call_inner(method, params)).await {
            Ok(r) => r,
            Err(_) => Err(ClientError::Timeout {
                method: method.to_string(),
                secs: limit.as_secs(),
            }),
        }
    }

    async fn call_inner(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&Request::new(id, method, params)).await?;
        loop {
            match self.recv().await? {
                Incoming::Event(e) => self.queued.push_back(e),
                // The client is sequential, so a null-id error (the server's reply to a request
                // it could not even attribute, e.g. "server busy") answers the one in flight.
                Incoming::Response { id: rid, result }
                    if rid == json!(id) || (rid.is_null() && result.is_err()) =>
                {
                    return result.map_err(ClientError::Api);
                }
                Incoming::Response { .. } => {}
            }
        }
    }

    /// Upgrades the connection with `call.stream` (hello must be done). On success the
    /// connection speaks the binary framing; the returned reader still holds any
    /// bytes that arrived behind the result line.
    pub async fn into_stream(mut self, params: Value) -> Result<ClientStream, ClientError> {
        let info = self.call("call.stream", params).await?;
        Ok(ClientStream {
            info,
            rd: self.rd,
            wr: self.wr,
        })
    }

    /// Next pushed event (after `events.subscribe`).
    pub async fn next_event(&mut self) -> Result<Event, ClientError> {
        if let Some(e) = self.queued.pop_front() {
            return Ok(e);
        }
        loop {
            if let Incoming::Event(e) = self.recv().await? {
                return Ok(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn agent_audio_timeouts() {
        let s = Duration::from_secs;
        // listen: default window is 8 s + 30 s, so the 65 s floor applies
        assert_eq!(request_timeout("call.listen", &json!({})), s(65));
        assert_eq!(
            request_timeout("call.listen", &json!({"timeout_ms": 180_000})),
            s(190)
        );
        // server wall = start + max + 5 s, outer timeout + 5 s: client waits 15 s over
        assert_eq!(
            request_timeout(
                "call.listen",
                &json!({"start_timeout_ms": 60_000, "max_ms": 120_000})
            ),
            s(195)
        );
        assert_eq!(
            request_timeout("call.listen", &json!({"transcribe": true})),
            s(65 + 125)
        );
        // rx_audio: instant unless transcribing
        assert_eq!(request_timeout("call.rx_audio", &json!({})), s(30));
        assert_eq!(
            request_timeout("call.rx_audio", &json!({"transcribe": true})),
            s(30 + 125)
        );
        // play / say
        assert_eq!(request_timeout("call.hold", &json!({})), s(50));
        assert_eq!(request_timeout("call.unhold", &json!({})), s(50));
        assert_eq!(request_timeout("call.transfer_blind", &json!({})), s(50));
        assert_eq!(request_timeout("call.transfer_attended", &json!({})), s(50));
        for m in [
            "call.consult_start",
            "transfer.complete",
            "transfer.swap",
            "transfer.cancel",
        ] {
            assert_eq!(request_timeout(m, &json!({})), s(60), "{m}");
        }
        assert_eq!(request_timeout("call.consult_link", &json!({})), s(30));
        assert_eq!(request_timeout("transfer.get", &json!({})), s(30));
        assert!(is_idempotent("transfer.get") && is_idempotent("transfer.list"));
        assert!(!is_idempotent("transfer.complete") && !is_idempotent("call.consult_start"));
        assert_eq!(request_timeout("call.play", &json!({})), s(30));
        assert_eq!(
            request_timeout("call.play", &json!({"wait": true, "timeout_ms": 60_000})),
            s(65)
        );
        assert_eq!(request_timeout("call.say", &json!({})), s(30 + 125));
        assert_eq!(
            request_timeout("call.say", &json!({"wait": true, "timeout_ms": 120_000})),
            s(125 + 125)
        );
        assert_eq!(request_timeout("speech.synthesize", &json!({})), s(125));
        assert_eq!(request_timeout("speech.transcribe", &Value::Null), s(125));
        assert_eq!(
            SPEECH_JOB_ALLOWANCE,
            Duration::from_millis(u64::from(siple_config::SPEECH_TIMEOUT_MAX_MS) + 5_000)
        );
        assert!(is_idempotent("call.rx_audio") && is_idempotent("speech.transcribe"));
        assert!(!is_idempotent("speech.synthesize") && !is_idempotent("call.listen"));
    }
}
