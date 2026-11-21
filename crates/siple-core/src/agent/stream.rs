//! `call.stream`: binary audio over an upgraded control connection.
//!
//! After the result line the connection carries frames in both directions: a 4-byte big-endian
//! header `type << 24 | len` (len <= [`MAX_FRAME`]) and the payload. `0x01` is AUDIO, `0x02` is a
//! JSON object. Server to client JSON events carry an `event` field, client commands a `cmd`.
//!
//! Tasks of one session: a *reader* (parses frames, applies socket backpressure), a *writer*
//! (the only owner of the write half, 10 s per write), an *rx* task (hub chunks to encoded
//! frames, never blocks the hub) and the *main loop* (TX ring feeding, marks, clear, call
//! events). Everything ends through one cancellation token tree rooted at the connection
//! token.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::agent::hub::RxSubscription;
use crate::agent::wav::{PcmEncoding, f32_to_i16};
use crate::app::Inner;
use crate::audio::Resampler;
use crate::control::methods::StreamSession;
use crate::control::{ApiError, MethodInfo};
use crate::events::{EventFilter, Subscription};
use crate::media::{CallMediaHandle, TxStreamProducer};
use siple_media::g711;

/// Frame type: audio payload.
pub const FRAME_AUDIO: u8 = 0x01;
/// Frame type: JSON object payload.
pub const FRAME_JSON: u8 = 0x02;
/// Largest frame payload (also the largest JSON command).
pub const MAX_FRAME: usize = 1 << 20;
/// Allowed `sample_rate` values.
pub const RATES: [u32; 4] = [8000, 16000, 24000, 48000];
/// Allowed `frame_ms` values.
pub const FRAME_MS: [u32; 4] = [10, 20, 40, 60];
/// Timeout of one socket write.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Engine rate of the TX ring and the hub (Hz).
const ENGINE: u32 = 48_000;
/// Largest effective TX buffer (the ring holds 2 s).
const RING_MS: u32 = 2_000;
/// How long `clear` waits for the media thread to apply it.
const CLEAR_WAIT: Duration = Duration::from_millis(150);
/// Pending outbound frames before RX audio is dropped (slow client).
const OUT_QUEUE: usize = 128;
/// Marks that may be outstanding at once.
const MAX_MARKS: usize = 1024;
/// Interval of the main loop's housekeeping tick.
const TICK: Duration = Duration::from_millis(10);

/// Schema of `call.stream` (not served by `call()`; it needs a connection upgrade).
pub fn method_info() -> MethodInfo {
    MethodInfo::new(
        "call.stream",
        "Upgrades this control connection to a binary duplex audio stream of one call \
         (4-byte header type<<24|len; 0x01 audio, 0x02 JSON). Last JSON line is the result; \
         use `siple-cli stream` or ControlClient::into_stream.",
        json!({
            "type": "object",
            "properties": {
                "call_id": {"type": "string"},
                "rx": {"type": "boolean", "default": true},
                "tx": {"type": "boolean", "default": true},
                "sample_rate": {"type": "integer", "enum": RATES, "default": 16000},
                "encoding": {"type": "string", "enum": ["s16le", "f32le", "mulaw", "alaw"], "default": "s16le"},
                "frame_ms": {"type": "integer", "enum": FRAME_MS, "default": 20},
                "tx_buffer_ms": {"type": "integer", "minimum": 40, "maximum": 10000, "default": 2000, "description": "TX jitter buffer in ms. Accepted 40..10000 but the effective maximum is 2000 (the ring size); the result line reports the effective value."},
                "hangup_on_close": {"type": "boolean", "default": false}
            },
            "required": ["call_id"],
            "additionalProperties": false
        }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    call_id: String,
    rx: Option<bool>,
    tx: Option<bool>,
    sample_rate: Option<u32>,
    encoding: Option<String>,
    frame_ms: Option<u32>,
    tx_buffer_ms: Option<u32>,
    hangup_on_close: Option<bool>,
}

/// Encodes one frame (header plus payload).
pub fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(u32::from(kind) << 24 | payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

/// Client side helper: reads the next frame as `(type, payload)`; `Ok(None)` on a clean end.
/// Frames of unknown types >= 0x80 are skipped.
pub async fn read_client_frame<R: AsyncRead + Unpin + ?Sized>(
    rd: &mut R,
) -> Result<Option<(u8, Vec<u8>)>, String> {
    match read_frame(rd).await {
        Frame::Audio(b) => Ok(Some((FRAME_AUDIO, b))),
        Frame::Json(b) => Ok(Some((FRAME_JSON, b))),
        Frame::Eof => Ok(None),
        Frame::Bad(m) | Frame::Lost(_, m) => Err(m),
    }
}

fn json_frame(v: &Value) -> Vec<u8> {
    encode_frame(FRAME_JSON, v.to_string().as_bytes())
}

/// One parsed client frame.
#[derive(Debug, PartialEq)]
enum Frame {
    Audio(Vec<u8>),
    Json(Vec<u8>),
    /// Clean end of input at a frame boundary.
    Eof,
    /// Unknown frame type, oversized frame or truncated input.
    Bad(String),
    /// The peer went away: `client_eof` (input ended inside a frame) or `connection_lost`
    /// (reset / broken pipe, e.g. a client closing a socket that still holds unread data).
    Lost(&'static str, String),
}

/// Classifies a read error: a vanished peer is a disconnect, not a protocol violation.
fn read_failure(what: &str, e: &std::io::Error) -> Frame {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    match e.kind() {
        UnexpectedEof => Frame::Lost("client_eof", format!("{what}: {e}")),
        ConnectionReset | BrokenPipe | ConnectionAborted => {
            Frame::Lost("connection_lost", format!("{what}: {e}"))
        }
        _ => Frame::Bad(format!("{what}: {e}")),
    }
}

/// Reads the next frame. Unknown types >= 0x80 are skipped.
async fn read_frame<R: AsyncRead + Unpin + ?Sized>(rd: &mut R) -> Frame {
    loop {
        let mut h = [0u8; 4];
        let mut got = 0;
        while got < 4 {
            match rd.read(&mut h[got..]).await {
                Ok(0) if got == 0 => return Frame::Eof,
                Ok(0) => {
                    return Frame::Lost("client_eof", "truncated frame header".into());
                }
                Ok(n) => got += n,
                Err(e) => return read_failure("read error", &e),
            }
        }
        let v = u32::from_be_bytes(h);
        let (kind, len) = ((v >> 24) as u8, (v & 0x00FF_FFFF) as usize);
        if len > MAX_FRAME {
            return Frame::Bad(format!("frame of {len} bytes exceeds {MAX_FRAME}"));
        }
        if !matches!(kind, FRAME_AUDIO | FRAME_JSON) && kind < 0x80 {
            return Frame::Bad(format!("unknown frame type {kind:#04x}"));
        }
        let mut buf = vec![0u8; len];
        if let Err(e) = rd.read_exact(&mut buf).await {
            return read_failure("truncated frame", &e);
        }
        match kind {
            FRAME_AUDIO => return Frame::Audio(buf),
            FRAME_JSON => return Frame::Json(buf),
            _ => {} // ignorable extension frame
        }
    }
}

fn decode_f32(enc: PcmEncoding, b: &[u8]) -> Vec<f32> {
    match enc {
        PcmEncoding::S16le => b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
            .collect(),
        PcmEncoding::F32le => b
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| {
                let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                if v.is_finite() {
                    v.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect(),
        PcmEncoding::Mulaw => b
            .iter()
            .map(|&x| f32::from(g711::ulaw_to_linear(x)) / 32768.0)
            .collect(),
        PcmEncoding::Alaw => b
            .iter()
            .map(|&x| f32::from(g711::alaw_to_linear(x)) / 32768.0)
            .collect(),
    }
}

/// Encodes `-1..1` samples as raw PCM in `enc` (appended to `out`).
pub fn encode_f32(enc: PcmEncoding, s: &[f32], out: &mut Vec<u8>) {
    match enc {
        PcmEncoding::F32le => {
            for v in s {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        _ => {
            for v in f32_to_i16(s) {
                match enc {
                    PcmEncoding::S16le => out.extend_from_slice(&v.to_le_bytes()),
                    PcmEncoding::Mulaw => out.push(g711::linear_to_ulaw(v)),
                    _ => out.push(g711::linear_to_alaw(v)),
                }
            }
        }
    }
}

/// Streaming resampler that drops its algorithmic delay once, so the output stays aligned.
struct StreamResampler {
    rs: Resampler,
    skip: usize,
    tmp: Vec<f32>,
}

impl StreamResampler {
    fn new(from: u32, to: u32) -> Self {
        let rs = Resampler::new(from, to, (from / 100).max(1) as usize)
            .unwrap_or_else(|_| Resampler::new(from, from, 1).expect("passthrough resampler"));
        let skip = rs.delay_frames();
        Self {
            rs,
            skip,
            tmp: Vec::new(),
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.rs.is_passthrough() {
            out.extend_from_slice(input);
            return;
        }
        self.tmp.clear();
        self.rs.process(input, &mut self.tmp);
        let skip = self.skip.min(self.tmp.len());
        self.skip -= skip;
        out.extend_from_slice(&self.tmp[skip..]);
    }

    fn reset(&mut self) {
        self.rs.reset();
        self.skip = self.rs.delay_frames();
    }
}

/// Counters shared by the session's tasks.
#[derive(Default)]
struct Stats {
    rx_dropped_ms: AtomicU64,
}

/// Registry entry plus TX attachment of one stream; cleans up on every exit path.
struct Reg {
    inner: Arc<Inner>,
    handle: CallMediaHandle,
    call_id: String,
    account: String,
    stream_id: u64,
    attached: bool,
    reason: &'static str,
    /// rx, tx, sample_rate, encoding: repeated on the `closed` event.
    fmt: Value,
    /// `release` ran: the slot is free and `closed` was emitted.
    released: bool,
}

impl Reg {
    /// Detaches TX, frees the registry slot and emits `call.stream closed` (with the current
    /// `reason`) exactly once. Idempotent; also run by `Drop`. Anything that must act on the
    /// call on behalf of this stream (the `hangup_on_close` hang-up) has to happen before it:
    /// once the slot is free a new stream may attach to the same call.
    fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if self.attached {
            self.attached = false;
            self.handle.detach_tx_stream();
        }
        {
            let mut g = self
                .inner
                .agent
                .streams
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if g.get(&self.call_id) == Some(&self.stream_id) {
                g.remove(&self.call_id);
            }
        }
        self.inner
            .events
            .emit("call.stream", Some(&self.account), Some(&self.call_id), {
                let mut v = self.fmt.clone();
                v["stream_id"] = self.stream_id.into();
                v["state"] = "closed".into();
                v["reason"] = self.reason.into();
                v
            });
    }
}

impl Drop for Reg {
    fn drop(&mut self) {
        self.release();
    }
}

/// Validates `call.stream` params and prepares the session. Returns the result line for the
/// client and the session that will own the connection afterwards.
pub(crate) async fn open_stream(
    inner: &Arc<Inner>,
    params: Value,
) -> Result<(Value, Box<dyn StreamSession>), ApiError> {
    let p: Params = serde_json::from_value(params).map_err(ApiError::from)?;
    let rx = p.rx.unwrap_or(true);
    let tx = p.tx.unwrap_or(true);
    if !rx && !tx {
        return Err(ApiError::invalid_params("rx and tx are both false"));
    }
    let rate = p.sample_rate.unwrap_or(16_000);
    if !RATES.contains(&rate) {
        return Err(ApiError::invalid_params(
            "sample_rate must be 8000, 16000, 24000 or 48000",
        ));
    }
    let enc_name = p.encoding.as_deref().unwrap_or("s16le");
    let enc = PcmEncoding::parse(enc_name)
        .ok_or_else(|| ApiError::invalid_params("encoding must be s16le, f32le, mulaw or alaw"))?;
    let frame_ms = p.frame_ms.unwrap_or(20);
    if !FRAME_MS.contains(&frame_ms) {
        return Err(ApiError::invalid_params(
            "frame_ms must be 10, 20, 40 or 60",
        ));
    }
    let tx_buffer_ms = p.tx_buffer_ms.unwrap_or(2000);
    if !(40..=10_000).contains(&tx_buffer_ms) {
        return Err(ApiError::invalid_params(
            "tx_buffer_ms must be in 40..=10000",
        ));
    }

    let info = inner.stack.call(&p.call_id).ok_or_else(|| {
        ApiError::not_found(format!("unknown call {}", siple_config::echo(&p.call_id)))
    })?;
    let state = serde_json::to_value(info.state).unwrap_or(Value::Null);
    match state.as_str() {
        Some("ended") => {
            return Err(ApiError::invalid_state(format!(
                "call {} has ended",
                p.call_id
            )));
        }
        Some("ringing") => {
            return Err(ApiError::invalid_state(format!(
                "call {} is still ringing; answer it first",
                p.call_id
            )));
        }
        _ => {}
    }
    let handle = inner
        .call_media
        .get(&p.call_id)
        .filter(|e| !e.closed)
        .ok_or_else(|| ApiError::invalid_state(format!("call {} has no media", p.call_id)))?
        .handle;

    // Subscribe before taking the seq so no event between them is lost.
    let events = inner.events.subscribe(EventFilter {
        kinds: Some(vec!["call.state".into(), "call.dtmf".into()]),
        account: None,
        call_id: Some(p.call_id.clone()),
    });
    let event_seq = inner.events.current_seq();

    let stream_id = {
        let mut g = inner
            .agent
            .streams
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if g.contains_key(&p.call_id) {
            return Err(ApiError::invalid_state(format!(
                "call {} already has a stream",
                p.call_id
            )));
        }
        let id = inner.agent.next_stream_id.fetch_add(1, Ordering::Relaxed) + 1;
        g.insert(p.call_id.clone(), id);
        id
    };
    let frame_samples = (rate * frame_ms / 1000) as usize;
    let frame_bytes = frame_samples * enc.sample_bytes();
    let tx_buffer_eff = tx_buffer_ms.min(RING_MS);
    let result = json!({
        "stream_id": stream_id,
        "sample_rate": rate,
        "encoding": enc.as_str(),
        "channels": 1,
        "frame_ms": frame_ms,
        "frame_bytes": frame_bytes,
        "tx_buffer_ms": tx_buffer_eff,
        "rx_ms": handle.rx_ms(),
        "event_seq": event_seq,
    });
    let fmt = json!({"rx": rx, "tx": tx, "sample_rate": rate, "encoding": enc.as_str()});
    let mut open = fmt.clone();
    open["stream_id"] = stream_id.into();
    open["state"] = "open".into();
    inner
        .events
        .emit("call.stream", Some(&info.account), Some(&p.call_id), open);
    let reg = Reg {
        inner: inner.clone(),
        handle,
        call_id: p.call_id,
        account: info.account,
        stream_id,
        attached: false,
        reason: "connection_lost",
        fmt,
        released: false,
    };
    let session = CallStream {
        reg,
        events,
        rx,
        tx,
        rate,
        enc,
        frame_ms,
        frame_bytes,
        tx_cap: (tx_buffer_eff as usize) * 48,
        hangup_on_close: p.hangup_on_close.unwrap_or(false),
    };
    Ok((result, Box::new(session)))
}

/// A prepared stream (one per call). Dropping it without running releases the call.
pub struct CallStream {
    reg: Reg,
    events: Subscription,
    rx: bool,
    tx: bool,
    rate: u32,
    enc: PcmEncoding,
    frame_ms: u32,
    frame_bytes: usize,
    /// Effective TX buffer limit, in 48 kHz samples.
    tx_cap: usize,
    hangup_on_close: bool,
}

/// TX-side state of the main loop.
struct TxState {
    prod: TxStreamProducer,
    rs: StreamResampler,
    /// Resampled samples waiting for room in the ring.
    pending: Vec<f32>,
    off: usize,
    /// Marks: (ring `pushed` position at mark time, name). A mark fires once the ring's
    /// `consumed` counter (played + discarded) reaches its position.
    marks: VecDeque<(u64, String)>,
    clearing: Option<Clearing>,
    cap: usize,
}

struct Clearing {
    deadline: Instant,
    /// Ring `pushed` position at the request; the clear is done once `consumed` reaches it.
    watermark: u64,
    /// `cleared_samples()` at the request.
    cleared_before: u64,
    /// Samples discarded locally (not yet in the ring) at the request.
    local_dropped: u64,
}

impl TxState {
    fn pending_len(&self) -> usize {
        self.pending.len() - self.off
    }

    fn push_pending(&mut self) {
        if self.pending_len() == 0 {
            return;
        }
        let room = self
            .cap
            .saturating_sub(self.prod.buffered())
            .min(self.prod.vacant());
        let n = room.min(self.pending_len());
        if n > 0 {
            let took = self.prod.push(&self.pending[self.off..self.off + n]);
            self.off += took;
        }
        if self.off == self.pending.len() {
            self.pending.clear();
            self.off = 0;
        }
    }
}

#[async_trait::async_trait]
impl StreamSession for CallStream {
    async fn run(
        self: Box<Self>,
        rd: Box<dyn AsyncRead + Send + Unpin>,
        wr: Box<dyn AsyncWrite + Send + Unpin>,
        conn: CancellationToken,
    ) {
        let mut s = *self;
        let reason = s.session(rd, wr, &conn).await;
        // `session` already released the registration (detached TX, freed the slot, emitted
        // `call.stream closed`) after the `hangup_on_close` hang-up; this is a no-op then.
        s.reg.reason = reason;
    }

    async fn abort(self: Box<Self>, reason: &'static str) {
        let mut s = *self;
        s.reg.reason = reason;
        // Same order as `session`: hang up BEFORE the slot is released.
        if s.hangup_on_close && reason_hangs_up(reason) {
            let _ = s.reg.inner.stack.hangup(&s.reg.call_id).await;
        }
        s.reg.release();
    }
}

impl CallStream {
    async fn session(
        &mut self,
        mut rd: Box<dyn AsyncRead + Send + Unpin>,
        mut wr: Box<dyn AsyncWrite + Send + Unpin>,
        conn: &CancellationToken,
    ) -> &'static str {
        let stop = conn.child_token();
        let stats = Arc::new(Stats::default());
        let handle = self.reg.handle.clone();
        let (out, mut out_rx) = mpsc::channel::<Vec<u8>>(OUT_QUEUE);

        // Writer: sole owner of the write half.
        let wstop = stop.clone();
        let mut writer = tokio::spawn(async move {
            while let Some(buf) = out_rx.recv().await {
                let w = async {
                    wr.write_all(&buf).await?;
                    wr.flush().await
                };
                if !matches!(tokio::time::timeout(WRITE_TIMEOUT, w).await, Ok(Ok(()))) {
                    wstop.cancel();
                    return;
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), wr.shutdown()).await;
        });

        // Reader.
        let (frames_tx, mut frames) = mpsc::channel::<Frame>(4);
        let rstop = stop.clone();
        let reader = tokio::spawn(async move {
            loop {
                let f = tokio::select! {
                    _ = rstop.cancelled() => return,
                    f = read_frame(&mut rd) => f,
                };
                let last = matches!(f, Frame::Eof | Frame::Bad(_) | Frame::Lost(..));
                tokio::select! {
                    _ = rstop.cancelled() => return,
                    r = frames_tx.send(f) => if r.is_err() { return },
                }
                if last {
                    return;
                }
            }
        });

        let mut tx_state = None;
        if self.tx {
            let prod = handle.attach_tx_stream();
            self.reg.attached = true;
            tx_state = Some(TxState {
                prod,
                rs: StreamResampler::new(self.rate, ENGINE),
                pending: Vec::new(),
                off: 0,
                marks: VecDeque::new(),
                clearing: None,
                cap: self.tx_cap,
            });
        }

        // RX.
        let mut rx_task = None;
        if self.rx {
            match handle.hub().subscribe().await {
                Ok(RxSubscription { rx, .. }) => {
                    rx_task = Some(tokio::spawn(rx_loop(
                        rx,
                        self.rate,
                        self.enc,
                        self.frame_ms,
                        self.frame_bytes,
                        out.clone(),
                        stats.clone(),
                        stop.clone(),
                    )));
                }
                Err(e) => {
                    // Documented fallback: continue as a TX-only stream and say so.
                    let _ = out
                        .send(json_frame(
                            &json!({"event": "rx_unavailable", "error": e.to_string()}),
                        ))
                        .await;
                }
            }
        }

        // The call may have ended between open and run.
        let mut reason: &'static str = "";
        let mut end_payload = Value::Null;
        if call_ended(&self.reg) {
            reason = "call_ended";
            end_payload = self
                .reg
                .inner
                .stack
                .call(&self.reg.call_id)
                .and_then(|c| serde_json::to_value(c.end).ok())
                .unwrap_or(Value::Null);
        }

        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_check = Instant::now();
        'main: while reason.is_empty() {
            let can_take = tx_state
                .as_ref()
                .is_none_or(|t| t.pending_len() == 0 && t.clearing.is_none());
            tokio::select! {
                biased;
                _ = conn.cancelled() => reason = "server_shutdown",
                _ = stop.cancelled() => reason = "connection_lost",
                f = frames.recv(), if can_take => {
                    let Some(f) = f else { reason = "connection_lost"; continue };
                    match self
                        .on_frame(f, &mut tx_state, &out, &stats, &handle)
                        .await
                    {
                        Ok(()) => {}
                        Err(r) => reason = r,
                    }
                }
                ev = self.events.next() => {
                    let Some(ev) = ev else { reason = "server_shutdown"; continue };
                    match ev.kind.as_str() {
                        "call.dtmf" => {
                            let v = json!({"event": "dtmf", "digit": ev.data["digit"], "via": ev.data["via"]});
                            if !send_json(&out, &v).await { reason = "connection_lost"; continue 'main; }
                        }
                        "call.state" => {
                            let st = ev.data["state"].as_str().unwrap_or("");
                            if st == "ended" {
                                reason = "call_ended";
                                end_payload = ev.data["end"].clone();
                            } else {
                                let v = json!({"event": "state", "state": st});
                                if !send_json(&out, &v).await { reason = "connection_lost"; continue 'main; }
                            }
                        }
                        _ => {} // events.lagged: the periodic check below covers an end
                    }
                }
                _ = tick.tick() => {
                    if let Some(t) = tx_state.as_mut() {
                        let r = service_tx(t, &out, &handle, self.rate).await;
                        if let Err(r) = r { reason = r; continue 'main; }
                    }
                    if last_check.elapsed() >= Duration::from_millis(250) {
                        last_check = Instant::now();
                        if call_ended(&self.reg) { reason = "call_ended"; }
                    }
                }
            }
        }

        if let Some(h) = &rx_task {
            h.abort();
        }
        if let Some(h) = rx_task {
            let _ = h.await;
        }
        reader.abort();
        self.reg.reason = reason;
        // Hang up BEFORE releasing: once the slot is free a new stream may attach, and this
        // stream's hang-up must not hit that one.
        if self.hangup_on_close && reason_hangs_up(reason) {
            let _ = self.reg.inner.stack.hangup(&self.reg.call_id).await;
        }
        // Free the slot and TX (and emit `closed`) before telling the client it ended, so an
        // immediate reopen or `call.get` right after `ended` sees the stream as gone.
        self.reg.release();
        if reason != "connection_lost" {
            let mut v = json!({"event": "ended", "reason": match reason {
                "call_ended" => "call_ended",
                "server_shutdown" => "server_shutdown",
                "protocol_error" => "protocol_error",
                _ => "closed",
            }});
            if reason == "call_ended" && !end_payload.is_null() {
                v["end"] = end_payload;
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), out.send(json_frame(&v))).await;
        }
        drop(out);
        if tokio::time::timeout(Duration::from_secs(5), &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
        reason
    }

    async fn on_frame(
        &mut self,
        f: Frame,
        tx_state: &mut Option<TxState>,
        out: &mpsc::Sender<Vec<u8>>,
        stats: &Arc<Stats>,
        handle: &CallMediaHandle,
    ) -> Result<(), &'static str> {
        match f {
            Frame::Eof => Err("client_eof"),
            Frame::Lost(reason, m) => {
                tracing::debug!("stream client gone: {m}");
                Err(reason)
            }
            Frame::Bad(m) => {
                tracing::debug!("stream protocol error: {m}");
                let _ = send_json(out, &json!({"event": "error", "message": m})).await;
                Err("protocol_error")
            }
            Frame::Audio(b) => {
                let Some(t) = tx_state.as_mut() else {
                    return Ok(()); // tx disabled: ignore
                };
                let sb = self.enc.sample_bytes();
                if b.len() % sb != 0 || b.len() > self.rate as usize * sb {
                    let m = "audio frame must be whole samples and at most 1 s";
                    let _ = send_json(out, &json!({"event": "error", "message": m})).await;
                    return Err("protocol_error");
                }
                let f = decode_f32(self.enc, &b);
                t.rs.process(&f, &mut t.pending);
                t.push_pending();
                Ok(())
            }
            Frame::Json(b) => {
                let v: Value = match serde_json::from_slice(&b) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = send_json(
                            out,
                            &json!({"event":"error","message":format!("bad JSON: {e}")}),
                        )
                        .await;
                        return Ok(());
                    }
                };
                match v["cmd"].as_str().unwrap_or("") {
                    "mark" => {
                        let name = v["name"].as_str().unwrap_or("").to_string();
                        let err = match tx_state.as_mut() {
                            None => Some("tx is disabled"),
                            Some(t) if t.marks.len() >= MAX_MARKS => {
                                Some("too many outstanding marks")
                            }
                            Some(t) if name.len() > 256 => {
                                let _ = t;
                                Some("mark name too long")
                            }
                            Some(t) => {
                                // `can_take` guarantees nothing is pending, so every sample
                                // of the frames before the mark is in the ring.
                                t.marks.push_back((t.prod.pushed(), name));
                                None
                            }
                        };
                        if let Some(m) = err {
                            let _ = send_json(out, &json!({"event":"error","message":m})).await;
                        }
                    }
                    "clear" => {
                        if let Some(t) = tx_state.as_mut() {
                            if t.clearing.is_none() {
                                let local_dropped = t.pending_len() as u64;
                                t.pending.clear();
                                t.off = 0;
                                t.rs.reset();
                                let cleared_before = t.prod.cleared_samples();
                                let watermark = t.prod.pushed();
                                t.prod.request_clear();
                                t.clearing = Some(Clearing {
                                    deadline: Instant::now() + CLEAR_WAIT,
                                    watermark,
                                    cleared_before,
                                    local_dropped,
                                });
                            }
                        } else {
                            let _ =
                                send_json(out, &json!({"event":"cleared","dropped_ms":0})).await;
                        }
                    }
                    "stats" => {
                        let (buf, under) = tx_state.as_ref().map_or((0, 0), |t| {
                            (
                                t.prod.buffered() + t.pending_len(),
                                t.prod.underrun_samples(),
                            )
                        });
                        let _ = send_json(
                            out,
                            &json!({
                                "event": "stats",
                                "tx_buffer_ms": buf as u64 / 48,
                                "tx_underrun_ms": under / 48,
                                "rx_dropped_ms": stats.rx_dropped_ms.load(Ordering::Relaxed),
                                "rx_ms": handle.rx_ms(),
                            }),
                        )
                        .await;
                    }
                    "close" => return Err("client_close"),
                    other => {
                        let shown: String = other.chars().take(64).collect();
                        let ell = if other.chars().nth(64).is_some() {
                            "..."
                        } else {
                            ""
                        };
                        let m = format!("unknown command {shown:?}{ell}");
                        let _ = send_json(out, &json!({"event":"error","message":m})).await;
                    }
                }
                Ok(())
            }
        }
    }
}

fn call_ended(reg: &Reg) -> bool {
    match reg.inner.stack.call(&reg.call_id) {
        None => true,
        Some(c) => {
            serde_json::to_value(c.state)
                .ok()
                .as_ref()
                .and_then(Value::as_str)
                == Some("ended")
                || reg
                    .inner
                    .call_media
                    .get(&reg.call_id)
                    .is_none_or(|e| e.closed)
        }
    }
}

/// Queues a JSON event; `false` when the client is not reading or gone.
async fn send_json(out: &mpsc::Sender<Vec<u8>>, v: &Value) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, out.send(json_frame(v))).await,
        Ok(Ok(()))
    )
}

/// Periodic TX work: feeds the ring, finishes a `clear`, emits due marks.
async fn service_tx(
    t: &mut TxState,
    out: &mpsc::Sender<Vec<u8>>,
    handle: &CallMediaHandle,
    _rate: u32,
) -> Result<(), &'static str> {
    if let Some(c) = &t.clearing {
        // Done once the media thread consumed everything pushed before the clear (played or
        // discarded), or after the cap (e.g. media not negotiated yet: the ring is not drained
        // until it is, and the clear then applies later).
        let consumed = t.prod.consumed();
        if consumed >= c.watermark || Instant::now() >= c.deadline {
            let c = t.clearing.take().expect("checked");
            let applied = t.prod.cleared_samples().saturating_sub(c.cleared_before);
            // Whatever the media thread has not got to yet will still be discarded.
            let outstanding = c.watermark.saturating_sub(consumed);
            let dropped = applied + c.local_dropped + outstanding;
            let rx_ms = handle.rx_ms();
            for (_, name) in std::mem::take(&mut t.marks) {
                if !send_json(out, &json!({"event":"mark","name":name,"rx_ms":rx_ms})).await {
                    return Err("connection_lost");
                }
            }
            if !send_json(out, &json!({"event":"cleared","dropped_ms":dropped / 48})).await {
                return Err("connection_lost");
            }
        }
        return Ok(());
    }
    t.push_pending();
    let consumed = t.prod.consumed();
    while let Some((target, _)) = t.marks.front() {
        if consumed < *target {
            break;
        }
        let (_, name) = t.marks.pop_front().expect("front exists");
        let rx_ms = handle.rx_ms();
        if !send_json(out, &json!({"event":"mark","name":name,"rx_ms":rx_ms})).await {
            return Err("connection_lost");
        }
    }
    Ok(())
}

/// Hub chunks to exact `frame_ms` frames. Never blocks on the client: frames that do not fit
/// the outbound queue are counted and announced with `rx_gap`.
#[allow(clippy::too_many_arguments)]
async fn rx_loop(
    mut rx: mpsc::Receiver<crate::agent::hub::RxChunk>,
    rate: u32,
    enc: PcmEncoding,
    frame_ms: u32,
    frame_bytes: usize,
    out: mpsc::Sender<Vec<u8>>,
    stats: Arc<Stats>,
    stop: CancellationToken,
) {
    let mut rs = StreamResampler::new(ENGINE, rate);
    let mut acc: Vec<u8> = Vec::new();
    let mut gap_ms: u64 = 0;
    let mut f32s = Vec::new();
    let mut rs_out = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = stop.cancelled() => return,
            c = rx.recv() => match c { Some(c) => c, None => return },
        };
        if chunk.gap_before48 > 0 {
            let ms = chunk.gap_before48 / 48;
            stats.rx_dropped_ms.fetch_add(ms, Ordering::Relaxed);
            gap_ms += ms;
        }
        f32s.clear();
        f32s.extend(chunk.samples48.iter().map(|&s| f32::from(s) / 32768.0));
        rs_out.clear();
        rs.process(&f32s, &mut rs_out);
        encode_f32(enc, &rs_out, &mut acc);
        while acc.len() >= frame_bytes {
            if gap_ms > 0
                && out
                    .try_send(json_frame(
                        &json!({"event": "rx_gap", "dropped_ms": gap_ms}),
                    ))
                    .is_ok()
            {
                gap_ms = 0;
            }
            let frame = encode_frame(FRAME_AUDIO, &acc[..frame_bytes]);
            acc.drain(..frame_bytes);
            if gap_ms > 0 || out.try_send(frame).is_err() {
                // Queue full (slow reader) or gap notice still pending.
                stats
                    .rx_dropped_ms
                    .fetch_add(u64::from(frame_ms), Ordering::Relaxed);
                gap_ms += u64::from(frame_ms);
            }
        }
    }
}

/// Close reasons that honour `hangup_on_close`; `server_shutdown` and `call_ended` do not.
fn reason_hangs_up(reason: &str) -> bool {
    matches!(
        reason,
        "client_close" | "client_eof" | "connection_lost" | "protocol_error"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_skip_extensions() {
        let mut wire = Vec::new();
        wire.extend(encode_frame(FRAME_AUDIO, &[1, 2, 3, 4]));
        wire.extend(encode_frame(0x90, b"ignored"));
        wire.extend(encode_frame(FRAME_JSON, br#"{"cmd":"stats"}"#));
        let mut r: &[u8] = &wire;
        assert_eq!(read_frame(&mut r).await, Frame::Audio(vec![1, 2, 3, 4]));
        assert_eq!(
            read_frame(&mut r).await,
            Frame::Json(br#"{"cmd":"stats"}"#.to_vec())
        );
        assert_eq!(read_frame(&mut r).await, Frame::Eof);
    }

    #[tokio::test]
    async fn bad_frames_are_rejected() {
        let mut r: &[u8] = &[0x05, 0, 0, 0];
        assert!(matches!(read_frame(&mut r).await, Frame::Bad(_)));
        let mut r: &[u8] = &[0x01, 0x20, 0, 0];
        assert!(matches!(read_frame(&mut r).await, Frame::Bad(_)));
        // Input ending inside a frame is a disconnect (client_eof), not a protocol error.
        let mut r: &[u8] = &[0x01, 0, 0, 4, 1, 2];
        assert!(matches!(
            read_frame(&mut r).await,
            Frame::Lost("client_eof", _)
        ));
        let mut r: &[u8] = &[0x01, 0];
        assert!(matches!(
            read_frame(&mut r).await,
            Frame::Lost("client_eof", _)
        ));
    }

    /// A reader failing with ECONNRESET (a client closing a socket with unread data).
    struct Resetting;
    impl AsyncRead for Resetting {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::ConnectionReset.into()))
        }
    }

    #[tokio::test]
    async fn connection_reset_is_a_disconnect_not_a_protocol_error() {
        assert!(matches!(
            read_frame(&mut Resetting).await,
            Frame::Lost("connection_lost", _)
        ));
    }

    #[test]
    fn header_layout() {
        assert_eq!(encode_frame(FRAME_JSON, b"ab")[..4], [0x02, 0, 0, 2]);
    }

    #[test]
    fn encodings_round_trip() {
        let src: Vec<f32> = (0..64).map(|i| (i as f32 / 64.0) - 0.5).collect();
        for enc in [
            PcmEncoding::S16le,
            PcmEncoding::F32le,
            PcmEncoding::Mulaw,
            PcmEncoding::Alaw,
        ] {
            let mut b = Vec::new();
            encode_f32(enc, &src, &mut b);
            assert_eq!(b.len(), src.len() * enc.sample_bytes());
            let back = decode_f32(enc, &b);
            for (a, c) in src.iter().zip(&back) {
                assert!((a - c).abs() < 0.03, "{enc:?}: {a} vs {c}");
            }
        }
    }
}
