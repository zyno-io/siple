//! Media engine: per-call RTP/RTCP/SRTP sessions, the real-time media thread
//! (pull-model clocking), local UI tones, recording and test hooks.
//!
//! Threading:
//! * one dedicated `std::thread` ("siple-media", real-time priority) does
//!   all per-frame work: capture resample, encode, SRTP protect, UDP send,
//!   jitter pull, decode, mix, playback resample;
//! * one tokio task per call receives datagrams, SRTP-unprotects them and
//!   hands them to the media thread over a bounded channel;
//! * the public handles ([`MediaEngine`], [`CallMediaHandle`]) are cheap,
//!   non-blocking command senders.

mod call;
pub mod engine;
pub mod ports;
mod rt;
mod rx;
pub(crate) mod sources;
pub mod stream_tx;
mod thread;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;

use siple_media::codec::AudioCodec;
use siple_media::jitter::JitterStats;
use siple_media::srtp::SrtpSuite;
use siple_media::tone::ToneSpec;

use crate::config::{AudioSink, AudioSource};

pub use engine::{CallMediaHandle, MediaEngine, PlayError};
pub use stream_tx::TxStreamProducer;

/// Where a received DTMF digit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtmfSource {
    Rfc4733,
}

/// How [`CallMediaHandle::send_dtmf`] transmits digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DtmfSendMode {
    /// RFC 4733 telephone-event packets (requires a negotiated PT).
    Rfc4733,
    /// Dual tones mixed into the audio stream.
    Inband,
}

/// Event emitted by the media layer.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaEvent {
    /// Empty for engine-level events.
    pub call_id: String,
    pub kind: MediaEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaEventKind {
    DtmfReceived {
        digit: char,
        duration_ms: u32,
        source: DtmfSource,
    },
    /// Remote audio started (`true`) or stopped (`false`) arriving.
    RxActive(bool),
    /// No RTP for 5 s while not on hold (emitted once per outage).
    MediaTimeout,
    /// Cumulative inbound SRTP/SRTCP authentication failures (rate limited).
    SrtpAuthFailures(u64),
    RecordFinished {
        path: PathBuf,
    },
    /// A clip finished or was cut short. Exactly one per play id.
    PlayFinished {
        play_id: u64,
        reason: PlayEnd,
        /// Audio actually transmitted for this play.
        played_ms: u64,
        /// RX timeline position (see [`CallMediaStats::rx_ms`]) when it ended.
        rx_ms: u64,
        /// Only for reason `barge_in`: RX position where the far end's interruption began.
        barge_in_rx_ms: Option<u64>,
    },
    Error(String),
}

/// Why a play ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayEnd {
    /// Played to the end (or to its duration limit).
    Completed,
    /// Stopped by the caller.
    Stopped,
    /// Replaced by a newer non-queued play.
    Replaced,
    /// Cut off by the far end speaking.
    BargeIn,
    /// The call ended first.
    CallEnded,
    Error,
}

impl PlayEnd {
    /// Wire name used in `call.play_finished` events.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Stopped => "stopped",
            Self::Replaced => "replaced",
            Self::BargeIn => "barge_in",
            Self::CallEnded => "call_ended",
            Self::Error => "error",
        }
    }
}

/// Where a clip's audio comes from.
#[derive(Debug, Clone)]
pub enum ClipSource {
    /// Decoded audio: 48 kHz mono `i16`.
    Pcm(Arc<[i16]>),
    /// A generator (`tone:` / `silence`); `file:` specs are decoded by the caller into `Pcm`.
    Spec(AudioSource),
}

/// One clip for [`CallMediaHandle`] to transmit in place of the call's base source.
#[derive(Debug, Clone)]
pub struct PlayRequest {
    pub source: ClipSource,
    /// Repeat until stopped (or `duration_ms`).
    pub looped: bool,
    /// Stop after this much transmitted audio.
    pub duration_ms: Option<u32>,
    /// Append to the queue instead of replacing the current clip and queue.
    pub queue: bool,
    /// Linear gain applied in dB.
    pub gain_db: f32,
}

/// Outcome of [`CallMediaHandle::detect_tone`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToneDetectResult {
    pub detected: bool,
    /// Tone level (RMS dBFS) of the best / detecting block.
    pub level_dbfs: f32,
    /// Fraction of block power at the tone frequency (about 1.0 = pure tone).
    pub power_ratio: f32,
}

/// RTCP-derived quality figures.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RtcpSummary {
    /// Cumulative packets lost on the inbound stream (our view).
    pub rx_cumulative_lost: u32,
    /// RFC 3550 interarrival jitter of the inbound stream (our view), ms.
    pub rx_jitter_ms: f32,
    /// Packets lost on our outbound stream per the peer's last report.
    pub remote_loss: Option<i32>,
    /// Peer-reported jitter of our outbound stream, ms.
    pub remote_jitter_ms: Option<f32>,
    pub rtt_ms: Option<f64>,
}

/// Snapshot of a call's media state (refreshed ~every 50 ms).
#[derive(Debug, Clone, Default)]
pub struct CallMediaStats {
    pub codec: Option<AudioCodec>,
    pub pt: Option<u8>,
    pub ptime_ms: u32,
    pub srtp_suite: Option<SrtpSuite>,
    pub remote_rtp: Option<std::net::SocketAddr>,
    pub tx_packets: u64,
    pub tx_bytes: u64,
    /// Sends dropped because the socket would block.
    pub tx_dropped: u64,
    pub tx_errors: u64,
    pub rx_packets: u64,
    pub rx_bytes: u64,
    pub rx_unknown_pt: u64,
    pub rx_queue_drops: u64,
    pub rx_source_drops: u64,
    /// Packets that arrived before the first `apply()`.
    pub rx_early_drops: u64,
    pub rtcp_rx_packets: u64,
    pub srtp_auth_failures: u64,
    pub jitter: JitterStats,
    pub rtcp: RtcpSummary,
    pub rx_active: bool,
    pub last_rx_at: Option<Instant>,
    /// TX is gated (hold or direction without send).
    pub tx_paused: bool,
    pub muted: bool,
    pub held: bool,
    /// RMS level of the decoded RX audio (last 20 ms), dBFS.
    pub rx_level_dbfs: f32,
    /// RX timeline: 20 ms per RX frame produced since this media was created.
    pub rx_ms: u64,
    /// Play id of the clip being transmitted.
    pub play_current: Option<u64>,
    /// Clips waiting behind the current one.
    pub play_queued: u32,
    /// A TX stream is attached.
    pub tx_stream: bool,
    /// RX audio the hub could not receive (tap ring full), ms.
    pub rx_tap_dropped_ms: u64,
}

/// Local UI tone kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalToneKind {
    Ringtone,
    Ringback,
    CallWaiting,
    Busy,
    Reorder,
    DtmfFeedback(char),
    Custom(ToneSpec),
}

/// Which output a local tone plays on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneTarget {
    /// The ringer device (mixed into the speaker when no separate ringer).
    Ringer,
    Speaker,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalTone {
    pub kind: LocalToneKind,
    pub target: ToneTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToneId(pub u64);

/// Parameters for [`MediaEngine::create_call`].
#[derive(Debug, Clone)]
pub struct CallMediaSpec {
    pub call_id: String,
    /// Local address the RTP/RTCP sockets bind to (may be unspecified).
    pub bind_ip: IpAddr,
    /// Address to advertise in SDP (informational; owned by the caller).
    pub advertise_ip: IpAddr,
    /// `[media].rtp_port_range`.
    pub port_range: [u16; 2],
    pub source: AudioSource,
    pub sink: AudioSink,
    /// Competes for mic focus / plays to the speaker when the source / sink
    /// is `Device`. When false, `Device` source behaves as silence and
    /// `Device` sink as null.
    pub device_bound: bool,
    /// RTCP SDES CNAME.
    pub cname: String,
    /// Latch the RTP destination to authenticated incoming packets.
    pub symmetric_rtp: bool,
    /// Send comfort-noise packets while held (if CN was negotiated).
    pub hold_send_cn: bool,
    pub jitter_min_ms: u32,
    pub jitter_max_ms: u32,
    /// `[media].rx_history_secs`: RX history kept by the call's hub.
    pub rx_history_secs: u32,
}

impl CallMediaSpec {
    /// Silence/null call with default port range and jitter bounds.
    pub fn new(call_id: impl Into<String>, bind_ip: IpAddr, advertise_ip: IpAddr) -> Self {
        let call_id = call_id.into();
        Self {
            cname: format!("{call_id}@siple"),
            call_id,
            bind_ip,
            advertise_ip,
            port_range: [16384, 32767],
            source: AudioSource::Silence,
            sink: AudioSink::Null,
            device_bound: false,
            symmetric_rtp: false,
            hold_send_cn: false,
            jitter_min_ms: 40,
            jitter_max_ms: 200,
            rx_history_secs: 60,
        }
    }
}

/// Phase of the microphone echo test ([`MediaEngine::echo_test_start`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoPhase {
    /// Never run (or cancelled).
    Idle,
    /// Recording the microphone.
    Recording,
    /// Playing the recording on the speaker.
    Playing,
    /// Finished.
    Done,
    /// Could not run (see `error`).
    Failed,
}

/// Progress and outcome of the echo test.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EchoStatus {
    /// Where the test is.
    pub phase: EchoPhase,
    /// Time into the whole test (recording, then playback), ms.
    pub elapsed_ms: u64,
    /// Length of the whole test, ms.
    pub total_ms: u64,
    /// Loudest sample of the recording, dBFS; set once recording ended.
    pub peak_dbfs: Option<f32>,
    /// The recording had no audible signal (peak below -60 dBFS).
    pub silent: bool,
    /// Why the test could not run.
    pub error: Option<String>,
}

impl Default for EchoStatus {
    fn default() -> Self {
        Self {
            phase: EchoPhase::Idle,
            elapsed_ms: 0,
            total_ms: 0,
            peak_dbfs: None,
            silent: false,
            error: None,
        }
    }
}

/// Values the media thread publishes for readers on other threads.
pub(crate) struct Meters {
    /// RMS level of the latest microphone block, dBFS, as `f32` bits; NaN = no microphone open.
    pub mic_level: AtomicU32,
    pub echo: Mutex<EchoStatus>,
}

impl Default for Meters {
    fn default() -> Self {
        Self {
            mic_level: AtomicU32::new(f32::NAN.to_bits()),
            echo: Mutex::new(EchoStatus::default()),
        }
    }
}
