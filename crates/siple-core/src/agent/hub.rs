//! Per-call RX hub: drains the media thread's RX tap ring, keeps the RX timeline (`rx_ms`) and
//! a 16 kHz history, and serves listen jobs, snapshots, barge-in watchers and stream
//! subscribers.
//!
//! [`HubCore`] is a pure synchronous state machine (unit tested with synthetic frames); the
//! async shell started by [`spawn`] ticks it from the RX tap ring.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ringbuf::HeapCons;
use ringbuf::traits::Consumer;
use siple_media::vad::{
    EnergyVad, SILENCE_DBFS, SegStatus, SegmentEnd, Segmenter, SegmenterConfig, ms_to_frames,
    peak_dbfs, rms_dbfs,
};
use tokio::sync::{mpsc, oneshot};

use crate::control::ApiError;

/// Called with a play id and the onset (RX ms of the start of the voiced run that triggered the
/// watcher) when a barge-in watcher fires; stops that play with reason `barge_in`. Built from the
/// media channel and media id, never from a call-handle clone (that would form an `Arc` cycle and
/// keep the media alive).
pub type StopPlayFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Why a hub request failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HubError {
    /// The call (and so the hub) is gone.
    #[error("the call has ended")]
    CallEnded,
    /// More than [`super::MAX_LISTENS_PER_CALL`] concurrent listens.
    #[error("too many concurrent listens on this call")]
    TooManyListens,
    /// A position or range the hub cannot honour.
    #[error("{0}")]
    BadRange(String),
    /// The hub task failed or is not implemented.
    #[error("{0}")]
    Internal(String),
}

impl From<HubError> for ApiError {
    fn from(e: HubError) -> Self {
        match e {
            HubError::CallEnded => ApiError::invalid_state(e.to_string()),
            HubError::TooManyListens => ApiError::invalid_state(e.to_string()),
            HubError::BadRange(m) => ApiError::invalid_params(m),
            HubError::Internal(m) => ApiError::internal(m),
        }
    }
}

/// Parameters of one `call.listen` job (already range-checked by the API layer).
#[derive(Debug, Clone, PartialEq)]
pub struct ListenParams {
    /// Start of the window on the RX timeline; `None` = now.
    pub since_rx_ms: Option<u64>,
    /// Give up with `NoSpeech` when no speech starts within this long.
    pub start_timeout_ms: u32,
    /// Silence that ends an utterance.
    pub end_silence_ms: u32,
    /// Longest utterance before `MaxDuration`.
    pub max_ms: u32,
    /// Voiced time needed to count as speech onset.
    pub min_speech_ms: u32,
    /// Audio kept before the onset.
    pub pre_roll_ms: u32,
    /// VAD threshold override (dBFS).
    pub threshold_dbfs: Option<f32>,
    /// Output rate of the captured audio: 16000 or 8000.
    pub sample_rate: u32,
    /// Wall-clock limit for the whole job (guards against a stalled RX timeline). `None` =
    /// `start_timeout_ms + max_ms + 5 s`.
    pub timeout_ms: Option<u32>,
}

/// Why a listen job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenEnd {
    EndOfSpeech,
    MaxDuration,
    NoSpeech,
    CallEnded,
    /// The wall-clock deadline passed (the RX timeline stalled).
    Timeout,
}

impl ListenEnd {
    /// Wire name of the `reason` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndOfSpeech => "end_of_speech",
            Self::MaxDuration => "max_duration",
            Self::NoSpeech => "no_speech",
            Self::CallEnded => "call_ended",
            Self::Timeout => "timeout",
        }
    }
}

/// Result of a listen job. All `*_rx_ms` values are on the call's RX timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct ListenOutcome {
    pub speech: bool,
    pub reason: ListenEnd,
    /// Window start (after clamping to the history).
    pub start_rx_ms: u64,
    /// Window end; the caller's next `since_rx_ms`.
    pub end_rx_ms: u64,
    pub speech_start_rx_ms: Option<u64>,
    pub speech_end_rx_ms: Option<u64>,
    pub speech_ms: u64,
    pub level_peak_dbfs: f32,
    pub level_mean_dbfs: f32,
    pub noise_floor_dbfs: f32,
    /// `since_rx_ms` was older than the history and was clamped.
    pub truncated: bool,
    /// Captured audio (mono `i16` at `ListenParams::sample_rate`), pre-roll included.
    pub samples: Vec<i16>,
}

/// Which part of the history [`HubHandle::snapshot`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRange {
    /// The most recent `ms` of audio.
    Last { ms: u64 },
    /// `since_rx_ms ..= until_rx_ms` (`None` = up to now).
    Span {
        since_rx_ms: u64,
        until_rx_ms: Option<u64>,
    },
}

/// Audio copied out of the history.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub start_rx_ms: u64,
    pub end_rx_ms: u64,
    /// The range began before the oldest retained audio.
    pub truncated: bool,
    pub level_peak_dbfs: f32,
    pub level_mean_dbfs: f32,
    /// Mono `i16` at 16 kHz.
    pub samples16: Vec<i16>,
}

/// One 20 ms (or longer, after a gap) chunk of 48 kHz RX audio for a stream subscriber.
#[derive(Debug, Clone)]
pub struct RxChunk {
    /// Position of the first sample on the 48 kHz RX timeline.
    pub pos48: u64,
    pub samples48: Arc<[i16]>,
    /// Samples the subscriber missed immediately before this chunk (channel was full).
    pub gap_before48: u64,
}

/// A stream subscriber's feed.
#[derive(Debug)]
pub struct RxSubscription {
    pub rx: mpsc::Receiver<RxChunk>,
    /// RX timeline position at subscription.
    pub start_rx_ms: u64,
}

// ---- pure core ----------------------------------------------------------------------------

/// Samples per VAD frame at the 16 kHz history rate (20 ms).
const FRAME16: usize = 320;
/// History samples per millisecond.
const PER_MS16: u64 = 16;
/// 48 kHz samples per millisecond.
const PER_MS48: u64 = 48;
/// Lowest accepted history length.
const MIN_HISTORY_SECS: u32 = 5;
/// Channel depth of a stream subscriber (about 1 s of 20 ms chunks).
const SUB_CHANNEL: usize = 64;
/// Barge-in watchers kept per call: the current clip, every queued play and some headroom for
/// watchers of finished plays not yet disarmed (the oldest inactive one is dropped beyond this).
const MAX_WATCHERS: usize = super::MAX_QUEUED_PLAYS + 1 + 4;
/// Net voiced frames (20 ms) that count as the far end starting to talk.
const BARGE_MIN_FRAMES: u32 = 6;
/// Audio kept after the last voiced frame of a listen result, in frames.
const POST_ROLL_FRAMES: u32 = 5;
/// How far in the future (ms) a listen window may start.
const MAX_FUTURE_MS: u64 = 10_000;

/// 48 kHz to 16 kHz decimator: 61-tap Blackman-windowed sinc low-pass (cut-off 7.2 kHz), exact
/// 3:1 sample count so the 16 kHz timeline stays aligned with the 48 kHz one.
struct Decimator {
    taps: Vec<f32>,
    /// Doubled ring so the newest `TAPS` inputs are always one contiguous slice.
    win: Vec<f32>,
    pos: usize,
    phase: u8,
}

const TAPS: usize = 61;

impl Decimator {
    fn new() -> Self {
        let m = (TAPS - 1) as f64 / 2.0;
        let fc = 7200.0 / 48000.0;
        let mut taps: Vec<f32> = (0..TAPS)
            .map(|n| {
                let x = n as f64 - m;
                let sinc = if x == 0.0 {
                    2.0 * fc
                } else {
                    (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
                };
                let a = 2.0 * std::f64::consts::PI * n as f64 / (TAPS - 1) as f64;
                let w = 0.42 - 0.5 * a.cos() + 0.08 * (2.0 * a).cos();
                (sinc * w) as f32
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        for t in &mut taps {
            *t /= sum;
        }
        Self {
            taps,
            win: vec![0.0; 2 * TAPS],
            pos: 0,
            phase: 0,
        }
    }

    /// Feeds one 48 kHz sample; every third call yields a 16 kHz sample.
    fn push(&mut self, x: i16) -> Option<i16> {
        let v = x as f32;
        self.win[self.pos] = v;
        self.win[self.pos + TAPS] = v;
        self.pos = (self.pos + 1) % TAPS;
        self.phase += 1;
        if self.phase < 3 {
            return None;
        }
        self.phase = 0;
        let w = &self.win[self.pos..self.pos + TAPS];
        let y: f32 = self.taps.iter().zip(w).map(|(a, b)| a * b).sum();
        Some(y.round().clamp(-32768.0, 32767.0) as i16)
    }
}

type ListenReply = oneshot::Sender<Result<ListenOutcome, HubError>>;

struct ListenJob {
    params: ListenParams,
    reply: ListenReply,
    /// Window start on the 16 kHz timeline.
    start16: u64,
    /// Next frame to process (absolute 16 kHz index).
    next16: u64,
    truncated: bool,
    /// All audio of the window so far (starts at `start16`).
    buf: Vec<i16>,
    vad: EnergyVad,
    seg: Segmenter,
    deadline: Instant,
}

impl ListenJob {
    fn start_ms(&self) -> u64 {
        self.start16 / PER_MS16
    }

    /// Consumes the job into its outcome. `reason` is used unless the segmenter finished by
    /// itself (`Done`).
    fn into_outcome(mut self, reason: ListenEnd) -> (ListenReply, ListenOutcome) {
        let reason = match self.seg.status() {
            SegStatus::Done(SegmentEnd::EndOfSpeech) => ListenEnd::EndOfSpeech,
            SegStatus::Done(SegmentEnd::MaxDuration) => ListenEnd::MaxDuration,
            SegStatus::Done(SegmentEnd::NoSpeech) => ListenEnd::NoSpeech,
            _ => reason,
        };
        let frames = self.seg.frames();
        let start_ms = self.start_ms();
        let end_rx_ms = start_ms + frames as u64 * 20;
        let bounds = match (
            self.seg.speech_start(),
            self.seg.speech_end().or_else(|| self.seg.finish()),
        ) {
            (Some(s), Some(e)) if e > s => Some((s, e)),
            _ => None,
        };
        let noise_floor_dbfs = self.vad.noise_floor_dbfs();
        let (speech, speech_start_rx_ms, speech_end_rx_ms, speech_ms, samples, peak, mean) =
            match bounds {
                Some((s, e)) => {
                    let pre = ms_to_frames(self.params.pre_roll_ms);
                    let from = s.saturating_sub(pre);
                    let to = (e + POST_ROLL_FRAMES).min(frames);
                    let slice = |a: u32, b: u32| {
                        let end = (b as usize * FRAME16).min(self.buf.len());
                        &self.buf[(a as usize * FRAME16).min(end)..end]
                    };
                    let core = slice(s, e);
                    (
                        true,
                        Some(start_ms + s as u64 * 20),
                        Some(start_ms + e as u64 * 20),
                        (e - s) as u64 * 20,
                        slice(from, to).to_vec(),
                        peak_dbfs(core),
                        rms_dbfs(core),
                    )
                }
                None => (
                    false,
                    None,
                    None,
                    0,
                    Vec::new(),
                    if self.buf.is_empty() {
                        SILENCE_DBFS
                    } else {
                        peak_dbfs(&self.buf)
                    },
                    if self.buf.is_empty() {
                        SILENCE_DBFS
                    } else {
                        rms_dbfs(&self.buf)
                    },
                ),
            };
        let samples = if self.params.sample_rate == 16_000 {
            samples
        } else {
            to_rate(&samples, self.params.sample_rate)
        };
        (
            self.reply,
            ListenOutcome {
                speech,
                reason,
                start_rx_ms: start_ms,
                end_rx_ms,
                speech_start_rx_ms,
                speech_end_rx_ms,
                speech_ms,
                level_peak_dbfs: peak,
                level_mean_dbfs: mean,
                noise_floor_dbfs,
                truncated: self.truncated,
                samples,
            },
        )
    }
}

/// 16 kHz `i16` to `rate` (8000 or 16000).
fn to_rate(samples16: &[i16], rate: u32) -> Vec<i16> {
    if rate == 16_000 || samples16.is_empty() {
        return samples16.to_vec();
    }
    let f: Vec<f32> = samples16.iter().map(|&s| s as f32 / 32768.0).collect();
    // Fixed in-range rates: resample cannot fail here.
    super::wav::f32_to_i16(&super::wav::resample(&f, 16_000, rate).unwrap_or_default())
}

struct Watcher {
    play_id: u64,
    vad: EnergyVad,
    next16: u64,
    run: u32,
    /// Start (16 kHz index) of the current voiced run.
    run_start16: u64,
    /// Whether the watched clip is the one currently playing; only then does RX audio count.
    active: bool,
}

struct Sub {
    tx: mpsc::Sender<RxChunk>,
    /// 48 kHz samples that did not fit since the last delivered chunk.
    pending_gap: u64,
}

/// The pure hub state machine: the RX timeline, the 16 kHz history and every job on it. Time
/// enters only through the `now` arguments.
pub struct HubCore {
    cap16: usize,
    hist: VecDeque<i16>,
    /// Absolute 16 kHz index of `hist[0]`; always a multiple of 16 (whole ms).
    hist_start16: u64,
    /// 16 kHz samples produced so far.
    total16: u64,
    /// 48 kHz samples consumed so far (the RX timeline).
    pos48: u64,
    dec: Decimator,
    jobs: Vec<ListenJob>,
    watchers: Vec<Watcher>,
    subs: Vec<Sub>,
    /// Id of the clip the media thread is playing now (0 = none).
    current_play: u64,
    closed: bool,
}

fn read_frame(hist: &VecDeque<i16>, hist_start16: u64, idx16: u64, out: &mut [i16; FRAME16]) {
    let a = idx16.saturating_sub(hist_start16) as usize;
    for (d, s) in out.iter_mut().zip(hist.range(a..a + FRAME16)) {
        *d = *s;
    }
}

impl HubCore {
    /// `history_secs` is clamped to at least 5 s.
    pub fn new(history_secs: u32) -> Self {
        Self {
            cap16: history_secs.max(MIN_HISTORY_SECS) as usize * 16_000,
            hist: VecDeque::new(),
            hist_start16: 0,
            total16: 0,
            pos48: 0,
            dec: Decimator::new(),
            jobs: Vec::new(),
            watchers: Vec::new(),
            subs: Vec::new(),
            current_play: 0,
            closed: false,
        }
    }

    /// Tells the hub which clip is playing (0 = none). A barge-in watcher only counts RX audio
    /// while its play is the current clip; counting starts from the moment it becomes current.
    /// Call before [`HubCore::push_rx`] so audio heard earlier never counts.
    pub fn set_current_play(&mut self, id: u64) {
        self.current_play = id;
        let total = self.total16;
        for w in &mut self.watchers {
            let active = id != 0 && w.play_id == id;
            if active && !w.active {
                w.next16 = total;
                w.run = 0;
            }
            if !active {
                w.run = 0;
            }
            w.active = active;
        }
    }

    /// RX timeline position in ms.
    pub fn rx_ms(&self) -> u64 {
        self.pos48 / PER_MS48
    }

    /// Whether any stream subscriber is attached (the shell then ticks faster).
    pub fn has_subscribers(&self) -> bool {
        !self.subs.is_empty()
    }

    /// Pending listen jobs.
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }

    /// Armed barge-in watchers.
    pub fn watcher_count(&self) -> usize {
        self.watchers.len()
    }

    /// Whether [`HubCore::close`] ran.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Advances the timeline by `chunk` (48 kHz) followed by `dropped` zero samples (what the
    /// media thread could not push), then runs fan-out, jobs, watchers and deadlines.
    /// Returns `(play id, onset rx_ms)` of the barge-in watchers that fired (each at most once).
    pub fn push_rx(&mut self, chunk: &[i16], dropped: u64, now: Instant) -> Vec<(u64, u64)> {
        if self.closed {
            return Vec::new();
        }
        self.jobs.retain(|j| !j.reply.is_closed());
        let total = chunk.len() as u64 + dropped;
        if total > 0 {
            if !self.subs.is_empty() {
                self.fan_out(chunk, dropped);
            }
            let zeros = (0..dropped).map(|_| 0i16);
            for x in chunk.iter().copied().chain(zeros) {
                if let Some(y) = self.dec.push(x) {
                    self.hist.push_back(y);
                    self.total16 += 1;
                }
            }
            self.pos48 += total;
        }
        self.run_jobs(now);
        let stops = self.run_watchers();
        self.trim();
        stops
    }

    fn fan_out(&mut self, chunk: &[i16], dropped: u64) {
        let mut all = Vec::with_capacity(chunk.len() + dropped as usize);
        all.extend_from_slice(chunk);
        all.resize(chunk.len() + dropped as usize, 0);
        let samples: Arc<[i16]> = all.into();
        let pos48 = self.pos48;
        self.subs.retain_mut(|s| {
            let msg = RxChunk {
                pos48,
                samples48: samples.clone(),
                gap_before48: s.pending_gap,
            };
            match s.tx.try_send(msg) {
                Ok(()) => {
                    s.pending_gap = 0;
                    true
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    s.pending_gap += samples.len() as u64;
                    true
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        });
    }

    fn run_jobs(&mut self, now: Instant) {
        let mut i = 0;
        while i < self.jobs.len() {
            let end = step_job(
                &mut self.jobs[i],
                &self.hist,
                self.hist_start16,
                self.total16,
                now,
            );
            match end {
                Some(reason) => {
                    let job = self.jobs.remove(i);
                    let (reply, out) = job.into_outcome(reason);
                    let _ = reply.send(Ok(out));
                }
                None => i += 1,
            }
        }
    }

    fn run_watchers(&mut self) -> Vec<(u64, u64)> {
        let mut fired = Vec::new();
        let mut frame = [0i16; FRAME16];
        let (hist, start, total) = (&self.hist, self.hist_start16, self.total16);
        self.watchers.retain_mut(|w| {
            if !w.active {
                return true;
            }
            while w.next16 + FRAME16 as u64 <= total {
                if w.next16 < start {
                    w.next16 = start;
                }
                read_frame(hist, start, w.next16, &mut frame);
                let frame_start16 = w.next16;
                w.next16 += FRAME16 as u64;
                if w.vad.process(&frame).voiced {
                    if w.run == 0 {
                        w.run_start16 = frame_start16;
                    }
                    w.run += 1;
                    if w.run >= BARGE_MIN_FRAMES {
                        fired.push((w.play_id, w.run_start16 / PER_MS16));
                        return false;
                    }
                } else {
                    w.run = w.run.saturating_sub(1);
                }
            }
            true
        });
        if !fired.is_empty() {
            // The caller interrupted the agent: every other armed barge-in clip (queued behind
            // the interrupted one) is cut in the same tick, so promoted clips never restart
            // their watcher from zero and stutter. Plays without a watcher are unaffected.
            let onset = fired[0].1;
            fired.extend(self.watchers.drain(..).map(|w| (w.play_id, onset)));
        }
        fired
    }

    fn trim(&mut self) {
        if self.hist.len() > self.cap16 {
            let drop = (self.hist.len() - self.cap16).div_ceil(16) * 16;
            let drop = drop.min(self.hist.len());
            self.hist.drain(..drop);
            self.hist_start16 += drop as u64;
        }
    }

    /// Starts a listen job; its result (or an error) goes to `reply`.
    pub fn add_listen(&mut self, params: ListenParams, reply: ListenReply, now: Instant) {
        if self.closed {
            let _ = reply.send(Err(HubError::CallEnded));
            return;
        }
        // Reap listens whose caller already went away so they do not count against the cap.
        self.jobs.retain(|j| !j.reply.is_closed());
        if self.jobs.len() >= super::MAX_LISTENS_PER_CALL {
            let _ = reply.send(Err(HubError::TooManyListens));
            return;
        }
        let now_ms = self.rx_ms();
        let since = params.since_rx_ms.unwrap_or(now_ms);
        if since > now_ms.saturating_add(MAX_FUTURE_MS) {
            let _ = reply.send(Err(HubError::BadRange(format!(
                "since_rx_ms {since} is ahead of the RX timeline (now {now_ms} ms, at most {MAX_FUTURE_MS} ms ahead)"
            ))));
            return;
        }
        let want16 = since.saturating_mul(PER_MS16);
        let (start16, truncated) = if want16 < self.hist_start16 {
            (self.hist_start16, true)
        } else {
            (want16, false)
        };
        let wall = params.timeout_ms.map_or_else(
            || {
                params.start_timeout_ms as u64
                    + params.max_ms as u64
                    + 5_000
                    + since.saturating_sub(now_ms)
            },
            u64::from,
        );
        // The start timeout counts from max(since, now): replayed history may still hold the
        // speech, but its duration must not consume the wait (think + TTS + playout time).
        let replayed_frames = (self.total16.saturating_sub(start16) / FRAME16 as u64) as u32;
        let seg = Segmenter::new(SegmenterConfig::from_ms(
            params.min_speech_ms,
            params.end_silence_ms,
            params.max_ms,
            params.start_timeout_ms,
        ))
        .with_timeout_from(replayed_frames);
        let mut job = ListenJob {
            vad: EnergyVad::new(params.threshold_dbfs),
            seg,
            params,
            reply,
            start16,
            next16: start16,
            truncated,
            buf: Vec::new(),
            deadline: now + Duration::from_millis(wall),
        };
        // Catch up on history right away.
        match step_job(&mut job, &self.hist, self.hist_start16, self.total16, now) {
            Some(reason) => {
                let (reply, out) = job.into_outcome(reason);
                let _ = reply.send(Ok(out));
            }
            None => self.jobs.push(job),
        }
    }

    /// Copies audio out of the history.
    pub fn snapshot(&self, range: SnapshotRange) -> Result<Snapshot, HubError> {
        if self.closed {
            return Err(HubError::CallEnded);
        }
        let end16_now = self.total16;
        let (want_start16, end16) = match range {
            SnapshotRange::Last { ms } => (
                end16_now.saturating_sub(ms.saturating_mul(PER_MS16)),
                end16_now,
            ),
            SnapshotRange::Span {
                since_rx_ms,
                until_rx_ms,
            } => {
                let now_ms = self.rx_ms();
                if since_rx_ms > now_ms {
                    return Err(HubError::BadRange(format!(
                        "since_rx_ms {since_rx_ms} is ahead of the RX timeline (now {now_ms} ms)"
                    )));
                }
                let until = until_rx_ms.unwrap_or(now_ms).min(now_ms);
                if until < since_rx_ms {
                    return Err(HubError::BadRange(
                        "until_rx_ms must not be before since_rx_ms".into(),
                    ));
                }
                (
                    since_rx_ms.saturating_mul(PER_MS16),
                    until.saturating_mul(PER_MS16).min(end16_now),
                )
            }
        };
        let truncated = want_start16 < self.hist_start16;
        // A span that ended before the retained history yields an empty (truncated) snapshot
        // at the start of the history; never an inverted range.
        let start16 = want_start16.max(self.hist_start16);
        let end16 = end16.max(start16).min(self.total16.max(start16));
        let a = (start16 - self.hist_start16) as usize;
        let b = (end16 - self.hist_start16) as usize;
        let len = self.hist.len();
        let samples16: Vec<i16> = self.hist.range(a.min(len)..b.min(len)).copied().collect();
        let (peak, mean) = if samples16.is_empty() {
            (SILENCE_DBFS, SILENCE_DBFS)
        } else {
            (peak_dbfs(&samples16), rms_dbfs(&samples16))
        };
        Ok(Snapshot {
            start_rx_ms: start16 / PER_MS16,
            end_rx_ms: end16 / PER_MS16,
            truncated,
            level_peak_dbfs: peak,
            level_mean_dbfs: mean,
            samples16,
        })
    }

    /// Attaches a stream subscriber with the default channel depth.
    pub fn add_subscriber(&mut self) -> Result<RxSubscription, HubError> {
        self.add_subscriber_with(SUB_CHANNEL)
    }

    /// Attaches a stream subscriber with a channel of `depth` chunks.
    pub fn add_subscriber_with(&mut self, depth: usize) -> Result<RxSubscription, HubError> {
        if self.closed {
            return Err(HubError::CallEnded);
        }
        let (tx, rx) = mpsc::channel(depth.max(1));
        self.subs.push(Sub { tx, pending_gap: 0 });
        Ok(RxSubscription {
            rx,
            start_rx_ms: self.rx_ms(),
        })
    }

    /// Arms a barge-in watcher for `play_id` (replacing an earlier one for the same id).
    pub fn add_watcher(
        &mut self,
        play_id: u64,
        threshold_dbfs: Option<f32>,
    ) -> Result<(), HubError> {
        if self.closed {
            return Err(HubError::CallEnded);
        }
        self.watchers.retain(|w| w.play_id != play_id);
        if self.watchers.len() >= MAX_WATCHERS {
            // Prefer the oldest watcher that is not the active clip's.
            let i = self.watchers.iter().position(|w| !w.active).unwrap_or(0);
            self.watchers.remove(i);
        }
        self.watchers.push(Watcher {
            play_id,
            vad: EnergyVad::new(threshold_dbfs),
            next16: self.total16,
            run: 0,
            run_start16: 0,
            active: play_id != 0 && self.current_play == play_id,
        });
        Ok(())
    }

    /// Disarms the watcher of `play_id`.
    pub fn remove_watcher(&mut self, play_id: u64) {
        self.watchers.retain(|w| w.play_id != play_id);
    }

    /// Ends the hub: pending listens resolve with `CallEnded` and their partial audio,
    /// subscribers and watchers are dropped. Idempotent.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        for job in std::mem::take(&mut self.jobs) {
            let (reply, out) = job.into_outcome(ListenEnd::CallEnded);
            let _ = reply.send(Ok(out));
        }
        self.subs.clear();
        self.watchers.clear();
    }
}

/// Feeds a job every complete frame available; returns why it ended, if it did.
fn step_job(
    job: &mut ListenJob,
    hist: &VecDeque<i16>,
    hist_start16: u64,
    total16: u64,
    now: Instant,
) -> Option<ListenEnd> {
    let mut frame = [0i16; FRAME16];
    while job.next16 + FRAME16 as u64 <= total16 {
        if job.next16 < hist_start16 {
            // The history moved past an unprocessed frame (an overload gap).
            job.next16 = hist_start16;
            job.truncated = true;
        }
        read_frame(hist, hist_start16, job.next16, &mut frame);
        job.next16 += FRAME16 as u64;
        job.buf.extend_from_slice(&frame);
        let voiced = job.vad.process(&frame).voiced;
        if let SegStatus::Done(e) = job.seg.push(voiced) {
            return Some(match e {
                SegmentEnd::EndOfSpeech => ListenEnd::EndOfSpeech,
                SegmentEnd::MaxDuration => ListenEnd::MaxDuration,
                SegmentEnd::NoSpeech => ListenEnd::NoSpeech,
            });
        }
    }
    (now >= job.deadline).then_some(ListenEnd::Timeout)
}

// ---- async shell ----------------------------------------------------------------------------

enum Cmd {
    Listen {
        params: ListenParams,
        reply: ListenReply,
    },
    Snapshot {
        range: SnapshotRange,
        reply: oneshot::Sender<Result<Snapshot, HubError>>,
    },
    Subscribe {
        reply: oneshot::Sender<Result<RxSubscription, HubError>>,
    },
    Watch {
        play_id: u64,
        threshold_dbfs: Option<f32>,
        reply: oneshot::Sender<Result<(), HubError>>,
    },
    Unwatch(u64),
    /// Test hook: stop draining the RX ring for a while.
    Stall(Duration),
    Close,
}

struct Shared {
    closed: AtomicBool,
    rx_ms: AtomicU64,
}

/// Cheap, cloneable handle to a call's RX hub.
#[derive(Clone)]
pub struct HubHandle {
    shared: Arc<Shared>,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl std::fmt::Debug for HubHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubHandle")
            .field("rx_ms", &self.rx_ms())
            .finish()
    }
}

impl HubHandle {
    /// Current RX timeline position in ms (monotonic per media).
    pub fn rx_ms(&self) -> u64 {
        self.shared.rx_ms.load(Ordering::Relaxed)
    }

    fn gone(&self) -> HubError {
        if self.is_closed() {
            HubError::CallEnded
        } else {
            HubError::Internal("the RX hub task stopped".into())
        }
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, HubError>>) -> Cmd,
    ) -> Result<T, HubError> {
        if self.is_closed() {
            return Err(HubError::CallEnded);
        }
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).map_err(|_| self.gone())?;
        rx.await.map_err(|_| self.gone())?
    }

    /// Runs a listen job. Dropping the future cancels the job.
    pub async fn listen(&self, params: ListenParams) -> Result<ListenOutcome, HubError> {
        self.request(|reply| Cmd::Listen { params, reply }).await
    }

    /// Copies audio out of the history.
    pub async fn snapshot(&self, range: SnapshotRange) -> Result<Snapshot, HubError> {
        self.request(|reply| Cmd::Snapshot { range, reply }).await
    }

    /// Subscribes to 48 kHz RX audio; the bounded channel never blocks the hub (a full channel
    /// shows up as `gap_before48`).
    pub async fn subscribe(&self) -> Result<RxSubscription, HubError> {
        self.request(|reply| Cmd::Subscribe { reply }).await
    }

    /// Arms a barge-in watcher: when the far end starts speaking, the hub calls the
    /// [`StopPlayFn`] given to [`spawn`] once with `play_id`.
    pub async fn watch_barge_in(
        &self,
        play_id: u64,
        threshold_dbfs: Option<f32>,
    ) -> Result<(), HubError> {
        self.request(|reply| Cmd::Watch {
            play_id,
            threshold_dbfs,
            reply,
        })
        .await
    }

    /// Disarms the barge-in watcher of `play_id` (no-op if none).
    pub fn unwatch_barge_in(&self, play_id: u64) {
        let _ = self.tx.send(Cmd::Unwatch(play_id));
    }

    /// Test hook: the hub stops draining the RX tap ring for `d` (overflowing it when longer
    /// than the ring holds), then catches up and zero-fills what was dropped.
    #[doc(hidden)]
    pub fn stall_for_tests(&self, d: Duration) {
        let _ = self.tx.send(Cmd::Stall(d));
    }

    /// Ends the hub: pending jobs resolve with `CallEnded`, subscriber senders are dropped.
    /// Idempotent.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        let _ = self.tx.send(Cmd::Close);
    }

    /// Whether [`HubHandle::close`] ran.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
}

/// Normal tick period.
const TICK: Duration = Duration::from_millis(20);
/// Tick period while stream subscribers exist.
const TICK_STREAM: Duration = Duration::from_millis(10);

fn make_interval(period: Duration) -> tokio::time::Interval {
    let mut iv = tokio::time::interval(period);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    iv
}

/// Starts the hub for one call.
///
/// * `ring`: consumer of the media thread's RX tap (48 kHz mono `i16`, 20 ms frames);
/// * `dropped`: samples the media thread could not push (the hub zero-fills them so its
///   timeline stays aligned);
/// * `current_play`: id of the clip the media thread is playing (0 = none), published on each
///   clip transition; barge-in watchers only count while their play is current;
/// * `stop_play`: see [`StopPlayFn`];
/// * `history_secs`: `[media].rx_history_secs`.
pub fn spawn(
    rt: &tokio::runtime::Handle,
    mut ring: HeapCons<i16>,
    dropped: Arc<AtomicU64>,
    current_play: Arc<AtomicU64>,
    stop_play: StopPlayFn,
    history_secs: u32,
) -> HubHandle {
    let shared = Arc::new(Shared {
        closed: AtomicBool::new(false),
        rx_ms: AtomicU64::new(0),
    });
    let (tx, mut rx) = mpsc::unbounded_channel::<Cmd>();
    let task_shared = shared.clone();
    rt.spawn(async move {
        let mut core = HubCore::new(history_secs);
        let mut period = TICK;
        let mut interval = make_interval(period);
        let mut scratch = vec![0i16; 4800];
        let mut chunk: Vec<i16> = Vec::with_capacity(4800);
        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    match cmd {
                        None | Some(Cmd::Close) => break,
                        Some(Cmd::Listen { params, reply }) => {
                            core.add_listen(params, reply, Instant::now());
                        }
                        Some(Cmd::Snapshot { range, reply }) => {
                            let _ = reply.send(core.snapshot(range));
                        }
                        Some(Cmd::Subscribe { reply }) => {
                            let _ = reply.send(core.add_subscriber());
                        }
                        Some(Cmd::Watch { play_id, threshold_dbfs, reply }) => {
                            let _ = reply.send(core.add_watcher(play_id, threshold_dbfs));
                        }
                        Some(Cmd::Unwatch(id)) => core.remove_watcher(id),
                        Some(Cmd::Stall(d)) => tokio::time::sleep(d).await,
                    }
                }
                _ = interval.tick() => {
                    chunk.clear();
                    loop {
                        let n = ring.pop_slice(&mut scratch);
                        if n == 0 {
                            break;
                        }
                        chunk.extend_from_slice(&scratch[..n]);
                    }
                    // Read after draining: whatever was dropped came after the audio drained.
                    let lost = dropped.swap(0, Ordering::Relaxed);
                    core.set_current_play(current_play.load(Ordering::Acquire));
                    for (id, onset_rx_ms) in core.push_rx(&chunk, lost, Instant::now()) {
                        stop_play(id, onset_rx_ms);
                    }
                    task_shared.rx_ms.store(core.rx_ms(), Ordering::Relaxed);
                    let want = if core.has_subscribers() { TICK_STREAM } else { TICK };
                    if want != period {
                        period = want;
                        interval = make_interval(period);
                    }
                }
            }
        }
        // Publish the final state, then resolve everything pending.
        task_shared.closed.store(true, Ordering::Release);
        core.close();
    });
    HubHandle { shared, tx }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: usize, amp: f32) -> Vec<i16> {
        (0..ms * 48)
            .map(|n| {
                ((n as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48_000.0).sin() * amp * 32767.0)
                    as i16
            })
            .collect()
    }

    fn quiet(ms: usize) -> Vec<i16> {
        vec![0; ms * 48]
    }

    fn feed(core: &mut HubCore, audio: &[i16], now: Instant) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for c in audio.chunks(960) {
            out.extend(core.push_rx(c, 0, now));
        }
        out
    }

    fn ids(fired: &[(u64, u64)]) -> Vec<u64> {
        fired.iter().map(|f| f.0).collect()
    }

    fn lp(since: Option<u64>) -> ListenParams {
        ListenParams {
            since_rx_ms: since,
            start_timeout_ms: 5_000,
            end_silence_ms: 400,
            max_ms: 30_000,
            min_speech_ms: 120,
            pre_roll_ms: 100,
            threshold_dbfs: None,
            sample_rate: 16_000,
            timeout_ms: None,
        }
    }

    fn listen(
        core: &mut HubCore,
        p: ListenParams,
        now: Instant,
    ) -> oneshot::Receiver<Result<ListenOutcome, HubError>> {
        let (tx, rx) = oneshot::channel();
        core.add_listen(p, tx, now);
        rx
    }

    fn near(a: u64, b: u64, tol: u64) -> bool {
        a.abs_diff(b) <= tol
    }

    #[test]
    fn decimator_keeps_count_and_passes_tone() {
        let mut d = Decimator::new();
        let x = tone(200, 0.5);
        let y: Vec<i16> = x.iter().filter_map(|&s| d.push(s)).collect();
        assert_eq!(y.len(), x.len() / 3);
        let rms = rms_dbfs(&y[400..]);
        assert!((rms - rms_dbfs(&x[1200..])).abs() < 1.0, "{rms}");
    }

    #[test]
    fn listen_live_with_pre_roll() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        feed(&mut core, &quiet(500), now);
        let mut rx = listen(&mut core, lp(None), now);
        assert!(rx.try_recv().is_err());
        feed(&mut core, &quiet(500), now);
        feed(&mut core, &tone(1000, 0.4), now);
        feed(&mut core, &quiet(1000), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert!(o.speech);
        assert_eq!(o.reason, ListenEnd::EndOfSpeech);
        assert_eq!(o.start_rx_ms, 500);
        assert!(near(o.speech_start_rx_ms.unwrap(), 1000, 40), "{o:?}");
        assert!(near(o.speech_end_rx_ms.unwrap(), 2000, 40), "{o:?}");
        assert!(near(o.speech_ms, 1000, 60));
        // 100 ms pre-roll + 1 s + 100 ms post-roll at 16 kHz
        assert!(
            near(o.samples.len() as u64, 19_200, 1_000),
            "{}",
            o.samples.len()
        );
        // The job ends when the end silence elapsed; the next listen starts there.
        assert!(near(o.end_rx_ms, 2400, 60), "{o:?}");
        assert!(!o.truncated);
        assert!(o.level_peak_dbfs > -15.0 && o.level_mean_dbfs > -25.0);
    }

    #[test]
    fn listen_since_in_the_past_catches_up_and_clamps_pre_roll_to_window() {
        let now = Instant::now();
        let mut core = HubCore::new(10);
        feed(&mut core, &quiet(1000), now);
        feed(&mut core, &tone(600, 0.4), now);
        feed(&mut core, &quiet(1500), now);
        // Speech started right at `since`: nothing earlier than the window start is returned.
        let mut rx = listen(&mut core, lp(Some(1000)), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert!(o.speech);
        assert_eq!(o.start_rx_ms, 1000);
        assert!(near(o.speech_start_rx_ms.unwrap(), 1000, 40));
        assert!(near(o.samples.len() as u64, (600 + 100) * 16, 800));
        // Chained listen from end_rx_ms sees silence only.
        let mut p = lp(Some(o.end_rx_ms));
        p.start_timeout_ms = 300;
        let mut rx2 = listen(&mut core, p, now);
        feed(&mut core, &quiet(600), now);
        let o2 = rx2.try_recv().unwrap().unwrap();
        assert!(!o2.speech);
        assert_eq!(o2.reason, ListenEnd::NoSpeech);
        assert!(o2.samples.is_empty());
    }

    /// Audit probe: the agent speaks until `end_rx_ms`, thinks, and the caller answers 3 s after
    /// the reply ended. The listen is issued 6 s / 9 s after `end_rx_ms` with since = end_rx_ms;
    /// the replayed history must not burn the 8 s start timeout.
    #[test]
    fn replayed_history_does_not_consume_start_timeout() {
        // (caller starts talking this long after `since`, listen issued this long after it)
        for (speech_at, issued_after_ms) in [
            (3_000usize, 6_000usize),
            (3_000, 9_000),
            (9_000, 12_000),
            (9_000, 15_000),
        ] {
            let now = Instant::now();
            let mut core = HubCore::new(30);
            feed(&mut core, &quiet(1000), now);
            let end_rx = core.rx_ms();
            feed(&mut core, &quiet(speech_at), now);
            feed(&mut core, &tone(1000, 0.4), now);
            feed(&mut core, &quiet(issued_after_ms - speech_at - 1000), now);
            let mut p = lp(Some(end_rx));
            p.start_timeout_ms = 8_000;
            let o = listen(&mut core, p, now).try_recv().unwrap().unwrap();
            assert!(o.speech, "{issued_after_ms}: {o:?}");
            assert_eq!(o.reason, ListenEnd::EndOfSpeech, "{issued_after_ms}");
            assert!(
                near(o.speech_start_rx_ms.unwrap(), end_rx + speech_at as u64, 40),
                "{o:?}"
            );
        }
        // Still times out: 8 s of quiet counted from the request, not from `since`.
        let now = Instant::now();
        let mut core = HubCore::new(30);
        feed(&mut core, &quiet(10_000), now);
        let mut p = lp(Some(0));
        p.start_timeout_ms = 1_000;
        let mut rx = listen(&mut core, p, now);
        assert!(rx.try_recv().is_err(), "must wait a full timeout from now");
        feed(&mut core, &quiet(900), now);
        assert!(rx.try_recv().is_err());
        feed(&mut core, &quiet(200), now);
        assert_eq!(rx.try_recv().unwrap().unwrap().reason, ListenEnd::NoSpeech);
    }

    #[test]
    fn speech_longer_than_history_still_works() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let mut rx = listen(&mut core, lp(None), now);
        feed(&mut core, &tone(7000, 0.4), now);
        feed(&mut core, &quiet(600), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert_eq!(o.reason, ListenEnd::EndOfSpeech);
        assert!(near(o.speech_ms, 7000, 80));
        assert!(o.samples.len() > 7000 * 16);
    }

    #[test]
    fn max_duration_and_8k_output() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let mut p = lp(None);
        p.max_ms = 500;
        p.sample_rate = 8_000;
        let mut rx = listen(&mut core, p, now);
        feed(&mut core, &tone(2000, 0.4), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert_eq!(o.reason, ListenEnd::MaxDuration);
        assert!(o.speech);
        assert!(near(o.speech_ms, 500, 60));
        assert!(
            near(o.samples.len() as u64, 8 * 500, 500),
            "{}",
            o.samples.len()
        );
    }

    #[test]
    fn truncated_when_older_than_history() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        feed(&mut core, &quiet(8000), now);
        let mut p = lp(Some(0));
        p.start_timeout_ms = 100;
        // The timeout counts from the request, so a little live audio is needed.
        let mut rx = listen(&mut core, p, now);
        feed(&mut core, &quiet(200), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert!(o.truncated);
        assert!(near(o.start_rx_ms, 3000, 20), "{}", o.start_rx_ms);
        assert_eq!(o.reason, ListenEnd::NoSpeech);
        // Within the history: not truncated.
        let mut p = lp(Some(5000));
        p.start_timeout_ms = 100;
        let mut rx = listen(&mut core, p, now);
        feed(&mut core, &quiet(200), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert!(!o.truncated);
        assert_eq!(o.start_rx_ms, 5000);
    }

    #[test]
    fn since_too_far_ahead_is_bad_range() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        feed(&mut core, &quiet(1000), now);
        let r = listen(&mut core, lp(Some(1000 + 10_001)), now)
            .try_recv()
            .unwrap();
        assert!(matches!(r, Err(HubError::BadRange(_))));
        // A near-future start waits for the timeline.
        let mut rx = listen(&mut core, lp(Some(1500)), now);
        assert!(rx.try_recv().is_err());
        feed(&mut core, &quiet(500), now);
        feed(&mut core, &tone(500, 0.4), now);
        feed(&mut core, &quiet(600), now);
        let o = rx.try_recv().unwrap().unwrap();
        assert_eq!(o.start_rx_ms, 1500);
        assert!(o.speech);
    }

    #[test]
    fn dropped_samples_are_zero_filled_and_keep_alignment() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        core.push_rx(&tone(100, 0.5), 0, now);
        assert_eq!(core.rx_ms(), 100);
        core.push_rx(&[], 200 * 48, now);
        assert_eq!(core.rx_ms(), 300);
        core.push_rx(&tone(100, 0.5), 0, now);
        assert_eq!(core.rx_ms(), 400);
        let s = core.snapshot(SnapshotRange::Last { ms: 400 }).unwrap();
        assert_eq!(s.samples16.len(), 400 * 16);
        let mid = &s.samples16[(100 + 10) * 16..(300 - 10) * 16];
        assert!(mid.iter().all(|&x| x.abs() < 50), "gap is not silent");
        assert!(rms_dbfs(&s.samples16[20 * 16..90 * 16]) > -15.0);
        assert!(rms_dbfs(&s.samples16[320 * 16..390 * 16]) > -15.0);
    }

    #[test]
    fn snapshot_ranges() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        feed(&mut core, &quiet(1000), now);
        feed(&mut core, &tone(1000, 0.4), now);
        let s = core.snapshot(SnapshotRange::Last { ms: 1000 }).unwrap();
        assert_eq!((s.start_rx_ms, s.end_rx_ms), (1000, 2000));
        assert_eq!(s.samples16.len(), 16_000);
        assert!(!s.truncated);
        assert!(s.level_mean_dbfs > -15.0);
        // Longer than the call so far: everything, not truncated.
        let s = core.snapshot(SnapshotRange::Last { ms: 4000 }).unwrap();
        assert_eq!((s.start_rx_ms, s.end_rx_ms, s.truncated), (0, 2000, false));
        let s = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 500,
                until_rx_ms: Some(1500),
            })
            .unwrap();
        assert_eq!(
            (s.start_rx_ms, s.end_rx_ms, s.samples16.len()),
            (500, 1500, 16_000)
        );
        assert!(s.level_peak_dbfs > -15.0 && s.level_mean_dbfs < s.level_peak_dbfs);
        let s = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 1900,
                until_rx_ms: Some(9000),
            })
            .unwrap();
        assert_eq!((s.start_rx_ms, s.end_rx_ms), (1900, 2000));
        assert!(matches!(
            core.snapshot(SnapshotRange::Span {
                since_rx_ms: 3000,
                until_rx_ms: None
            }),
            Err(HubError::BadRange(_))
        ));
        assert!(matches!(
            core.snapshot(SnapshotRange::Span {
                since_rx_ms: 1500,
                until_rx_ms: Some(1000)
            }),
            Err(HubError::BadRange(_))
        ));
        feed(&mut core, &quiet(5000), now);
        let s = core.snapshot(SnapshotRange::Last { ms: 6500 }).unwrap();
        assert!(s.truncated);
        assert!(near(s.start_rx_ms, 2000, 20), "{}", s.start_rx_ms);
        assert_eq!(s.end_rx_ms, 7000);
    }

    #[test]
    fn slow_subscriber_gets_a_gap_and_never_blocks() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let mut sub = core.add_subscriber_with(1).unwrap();
        assert!(core.has_subscribers());
        for _ in 0..4 {
            core.push_rx(&tone(20, 0.3), 0, now);
        }
        let first = sub.rx.try_recv().unwrap();
        assert_eq!(
            (first.pos48, first.gap_before48, first.samples48.len()),
            (0, 0, 960)
        );
        assert!(sub.rx.try_recv().is_err());
        core.push_rx(&tone(20, 0.3), 0, now);
        let next = sub.rx.try_recv().unwrap();
        assert_eq!(next.pos48, 4 * 960);
        assert_eq!(next.gap_before48, 3 * 960);
        // A dropped receiver is removed.
        drop(sub);
        core.push_rx(&tone(20, 0.3), 0, now);
        assert!(!core.has_subscribers());
    }

    #[test]
    fn barge_in_fires_once() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        core.add_watcher(7, None).unwrap();
        core.set_current_play(7);
        assert!(feed(&mut core, &quiet(300), now).is_empty());
        let fired = feed(&mut core, &tone(400, 0.4), now);
        assert_eq!(fired.iter().map(|f| f.0).collect::<Vec<_>>(), vec![7]);
        // The onset is the start of the tone (300 ms in), not the frame that completed the run.
        assert!(fired[0].1.abs_diff(300) <= 40, "onset {}", fired[0].1);
        assert!(feed(&mut core, &tone(400, 0.4), now).is_empty());
        assert_eq!(core.watcher_count(), 0);
        // Short blips do not fire.
        core.add_watcher(8, None).unwrap();
        core.set_current_play(8);
        assert!(feed(&mut core, &tone(40, 0.4), now).is_empty());
        assert!(feed(&mut core, &quiet(400), now).is_empty());
        core.remove_watcher(8);
        assert_eq!(core.watcher_count(), 0);
    }

    #[test]
    fn barge_in_only_counts_while_the_play_is_current() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        // Play 1 is current; play 2 is queued behind it with its watcher already armed.
        core.set_current_play(1);
        core.add_watcher(2, None).unwrap();
        // Far-end speech while play 1 plays must not fire play 2's watcher.
        assert!(feed(&mut core, &tone(600, 0.4), now).is_empty());
        assert_eq!(core.watcher_count(), 1);
        // Play 2 becomes current in the middle of ongoing speech: counting starts afresh,
        // so the speech heard earlier does not count but the continuing speech does.
        core.set_current_play(2);
        assert_eq!(ids(&feed(&mut core, &tone(400, 0.4), now)), vec![2]);

        // A watcher armed while its clip is already current is active at once.
        core.set_current_play(3);
        core.add_watcher(3, None).unwrap();
        assert_eq!(ids(&feed(&mut core, &tone(400, 0.4), now)), vec![3]);

        // Losing the current status resets the onset count.
        core.set_current_play(4);
        core.add_watcher(4, None).unwrap();
        assert!(feed(&mut core, &tone(60, 0.4), now).is_empty());
        core.set_current_play(0);
        assert!(feed(&mut core, &tone(400, 0.4), now).is_empty());
        assert_eq!(core.watcher_count(), 1);
    }

    #[test]
    fn barge_in_cuts_all_queued_barge_in_plays() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        core.set_current_play(1);
        core.add_watcher(1, None).unwrap();
        core.add_watcher(2, None).unwrap();
        core.add_watcher(3, None).unwrap();
        let mut fired = feed(&mut core, &tone(400, 0.4), now);
        fired.sort();
        assert_eq!(fired.iter().map(|f| f.0).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(fired.iter().all(|f| f.1 == fired[0].1), "{fired:?}");
        assert_eq!(core.watcher_count(), 0);
    }

    #[test]
    fn watcher_cap_keeps_the_active_one() {
        let mut core = HubCore::new(5);
        core.set_current_play(1);
        core.add_watcher(1, None).unwrap();
        for id in 2..=(MAX_WATCHERS as u64 + 5) {
            core.add_watcher(id, None).unwrap();
        }
        assert_eq!(core.watcher_count(), MAX_WATCHERS);
        assert!(core.watchers.iter().any(|w| w.play_id == 1 && w.active));
        const { assert!(MAX_WATCHERS > crate::agent::MAX_QUEUED_PLAYS + 1) };
    }

    fn snap_core() -> HubCore {
        // 5 s history fed with 8 s: the retained history starts at 3000 ms.
        let mut core = HubCore::new(5);
        feed(&mut core, &tone(8000, 0.3), Instant::now());
        core
    }

    #[test]
    fn snapshot_span_ending_before_history_is_empty_and_truncated() {
        let core = snap_core();
        let s = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 0,
                until_rx_ms: Some(1000),
            })
            .unwrap();
        assert!(s.truncated);
        assert!(s.samples16.is_empty());
        assert_eq!(s.start_rx_ms, s.end_rx_ms);
        assert_eq!(s.start_rx_ms, 3000);
    }

    #[test]
    fn snapshot_span_straddling_history_start_is_clamped() {
        let core = snap_core();
        let s = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 2000,
                until_rx_ms: Some(4000),
            })
            .unwrap();
        assert!(s.truncated);
        assert_eq!(s.start_rx_ms, 3000);
        assert_eq!(s.end_rx_ms, 4000);
        assert_eq!(s.samples16.len(), 1000 * 16);
    }

    #[test]
    fn snapshot_future_and_zero_length_spans() {
        let core = snap_core();
        assert!(matches!(
            core.snapshot(SnapshotRange::Span {
                since_rx_ms: 9000,
                until_rx_ms: None,
            }),
            Err(HubError::BadRange(_))
        ));
        // until past now is clamped to now.
        let s = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 7000,
                until_rx_ms: Some(99_000),
            })
            .unwrap();
        assert_eq!((s.start_rx_ms, s.end_rx_ms), (7000, 8000));
        assert!(!s.truncated);
        let z = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 5000,
                until_rx_ms: Some(5000),
            })
            .unwrap();
        assert!(z.samples16.is_empty() && !z.truncated);
        assert_eq!(z.start_rx_ms, z.end_rx_ms);
        // Extreme values never overflow or panic.
        let _ = core.snapshot(SnapshotRange::Last { ms: u64::MAX });
        let _ = core.snapshot(SnapshotRange::Span {
            since_rx_ms: 0,
            until_rx_ms: Some(u64::MAX),
        });
    }

    #[test]
    fn snapshot_of_an_empty_hub_does_not_panic() {
        let core = HubCore::new(5);
        let s = core.snapshot(SnapshotRange::Last { ms: 1000 }).unwrap();
        assert!(s.samples16.is_empty());
        let s = core
            .snapshot(SnapshotRange::Span {
                since_rx_ms: 0,
                until_rx_ms: Some(0),
            })
            .unwrap();
        assert!(s.samples16.is_empty());
    }

    #[test]
    fn close_resolves_jobs_with_call_ended() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let mut talking = listen(&mut core, lp(None), now);
        let mut sub = core.add_subscriber().unwrap();
        feed(&mut core, &quiet(300), now);
        let mut p = lp(Some(core.rx_ms()));
        p.min_speech_ms = 60;
        let mut speaking = listen(&mut core, p, now);
        feed(&mut core, &tone(500, 0.4), now);
        core.close();
        assert!(core.is_closed());
        let o = speaking.try_recv().unwrap().unwrap();
        assert_eq!(o.reason, ListenEnd::CallEnded);
        assert!(o.speech && !o.samples.is_empty());
        assert!(talking.try_recv().unwrap().unwrap().speech);
        while sub.rx.try_recv().is_ok() {}
        assert!(matches!(
            sub.rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        let mut late = listen(&mut core, lp(None), now);
        assert_eq!(late.try_recv().unwrap().unwrap_err(), HubError::CallEnded);
        core.close();

        // A job that heard nothing resolves without speech.
        let mut core = HubCore::new(5);
        let mut waiting = listen(&mut core, lp(None), now);
        feed(&mut core, &quiet(100), now);
        core.close();
        let o = waiting.try_recv().unwrap().unwrap();
        assert_eq!(
            (o.reason, o.speech, o.end_rx_ms),
            (ListenEnd::CallEnded, false, 100)
        );
    }

    #[test]
    fn dropped_receiver_is_reaped_and_limit_enforced() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let rx = listen(&mut core, lp(None), now);
        assert_eq!(core.job_count(), 1);
        drop(rx);
        core.push_rx(&quiet(20), 0, now);
        assert_eq!(core.job_count(), 0);
        let _keep: Vec<_> = (0..super::super::MAX_LISTENS_PER_CALL)
            .map(|_| listen(&mut core, lp(None), now))
            .collect();
        let mut over = listen(&mut core, lp(None), now);
        assert_eq!(
            over.try_recv().unwrap().unwrap_err(),
            HubError::TooManyListens
        );
    }

    #[test]
    fn add_listen_reaps_closed_jobs_before_cap_check() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let mut rxs: Vec<_> = (0..super::super::MAX_LISTENS_PER_CALL)
            .map(|_| listen(&mut core, lp(None), now))
            .collect();
        // One caller goes away; no push_rx happened since.
        drop(rxs.pop());
        let mut again = listen(&mut core, lp(None), now);
        assert!(again.try_recv().is_err(), "accepted, still pending");
        assert_eq!(core.job_count(), super::super::MAX_LISTENS_PER_CALL);
    }

    #[test]
    fn wall_clock_deadline_times_out_a_stalled_timeline() {
        let now = Instant::now();
        let mut core = HubCore::new(5);
        let mut p = lp(None);
        p.timeout_ms = Some(1000);
        let mut rx = listen(&mut core, p, now);
        core.push_rx(&[], 0, now + Duration::from_millis(900));
        assert!(rx.try_recv().is_err());
        core.push_rx(&[], 0, now + Duration::from_millis(1100));
        let o = rx.try_recv().unwrap().unwrap();
        assert_eq!((o.reason, o.speech), (ListenEnd::Timeout, false));
    }

    #[tokio::test]
    async fn shell_runs_listen_and_barge_in_from_the_ring() {
        use ringbuf::HeapRb;
        use ringbuf::traits::{Producer, Split};
        let (mut prod, cons) = HeapRb::<i16>::new(96_000).split();
        let stopped = Arc::new(std::sync::Mutex::new(Vec::new()));
        let st = stopped.clone();
        let hub = spawn(
            &tokio::runtime::Handle::current(),
            cons,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(3)),
            Arc::new(move |id, _| st.lock().unwrap().push(id)),
            5,
        );
        hub.watch_barge_in(3, None).await.unwrap();
        let job = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.listen(lp(None)).await })
        };
        let mut sub = hub.subscribe().await.unwrap();
        let audio: Vec<i16> = quiet(200)
            .into_iter()
            .chain(tone(600, 0.4))
            .chain(quiet(800))
            .collect();
        for c in audio.chunks(960) {
            prod.push_slice(c);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let o = tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(o.reason, ListenEnd::EndOfSpeech);
        assert!(near(o.speech_ms, 600, 100), "{o:?}");
        assert_eq!(*stopped.lock().unwrap(), vec![3]);
        assert!(hub.rx_ms() >= 1000);
        assert!(sub.rx.recv().await.is_some());
        let snap = hub.snapshot(SnapshotRange::Last { ms: 500 }).await.unwrap();
        assert!(snap.samples16.len() <= 500 * 16);
        hub.close();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(hub.listen(lp(None)).await.unwrap_err(), HubError::CallEnded);
    }
}
