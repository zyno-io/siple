//! Agent audio I/O: play inline audio into a call, listen to what the far end says, optional
//! external speech hooks and a binary stream.
//!
//! * [`capture`]: which calls an agent is capturing audio from (the on-screen indicator);
//! * [`hub`]: per-call RX hub (timeline, history, VAD jobs, subscribers);
//! * [`wav`]: WAV / raw PCM / base64 decoding;
//! * [`speech`]: TTS / STT commands from `[speech]`;
//! * [`stream`]: `call.stream` upgrade session;
//! * [`methods`]: the API methods built on those.

pub mod capture;
pub mod hub;
pub mod methods;
pub mod speech;
pub mod stream;
pub mod wav;

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

/// Largest decoded inline audio payload.
pub const MAX_INLINE_AUDIO_BYTES: usize = 10 * 1024 * 1024;
/// Longest single clip, in seconds (stored as 48 kHz `i16`).
pub const MAX_CLIP_SECS: u32 = 120;
/// Queued plays per call.
pub const MAX_QUEUED_PLAYS: usize = 8;
/// Queued audio per call, in seconds.
pub const MAX_QUEUED_SECS: u32 = 300;
/// Concurrent `call.listen` jobs per call.
pub const MAX_LISTENS_PER_CALL: usize = 4;
/// Capacity of the RX tap ring, in 48 kHz samples (2 s).
pub const RX_TAP_SAMPLES: usize = 96_000;

/// Process-wide agent state, held in `Inner`.
#[derive(Default)]
pub struct AgentState {
    /// Call id -> stream id of the call's active `call.stream` (at most one per call).
    pub streams: Mutex<HashMap<String, u64>>,
    /// Source of stream ids.
    pub next_stream_id: AtomicU64,
    /// Calls an agent is capturing audio from (listen, rx_audio, stream RX, record).
    pub capture: std::sync::Arc<capture::CaptureRegistry>,
}
