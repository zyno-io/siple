//! Per-call media state, owned and driven exclusively by the media thread.

use std::collections::VecDeque;
use std::fs::File;
use std::io::BufWriter;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::Receiver;
use ringbuf::HeapProd;
use ringbuf::traits::Producer;
use siple_media::codec::{
    AudioCodec, AudioDecoder, AudioEncoder, EncoderConfig, OpusEncoderConfig, clamp_opus_ptime,
    make_decoder, make_encoder,
};
use siple_media::dtmf::{DtmfDetector, DtmfGenerator, DtmfPacket, inband_dtmf_samples};
use siple_media::goertzel::{ToneDetector, rms_dbfs};
use siple_media::jitter::JitterBuffer;
use siple_media::rtcp::{RtcpPacket, RtcpStats, build_report_with_cname, parse_rtcp};
use siple_media::rtp::RtpHeader;
use siple_media::sdp_neg::Negotiated;
use siple_media::srtp::{SrtcpContext, SrtpContext, SrtpSuite};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc::UnboundedSender, oneshot};

use super::rx::{Latched, RxConfig, RxCounters, RxCtl, RxMsg, RxPacket, SrtpIn, SrtpUpdate};
use super::sources::{ENGINE_RATE, TxSource};
use super::stream_tx::TxStreamCons;
use super::{
    CallMediaStats, DtmfSendMode, DtmfSource, MediaEvent, MediaEventKind, PlayEnd, RtcpSummary,
    ToneDetectResult,
};
use crate::audio::Resampler;

/// Samples per 20 ms RX frame at the engine rate.
pub(crate) const RX_FRAME_48K: usize = (ENGINE_RATE / 50) as usize;
const RTCP_INTERVAL: Duration = Duration::from_secs(5);
const STATS_INTERVAL: Duration = Duration::from_millis(50);
const RX_ACTIVE_WINDOW: Duration = Duration::from_millis(500);
const MEDIA_TIMEOUT: Duration = Duration::from_secs(5);
const CN_HOLD_INTERVAL: Duration = Duration::from_secs(1);
const DTMF_VOLUME: u8 = 10;
const DETECT_BLOCK_MS: u32 = 40;
const DETECT_MIN_RATIO: f32 = 0.5;
/// Preallocated TX stream scratch (120 ms at 48 kHz).
const STREAM_BUF_CAP: usize = 5760;
use crate::agent::MAX_QUEUED_PLAYS;

/// Static per-call configuration.
pub(crate) struct CallCfg {
    pub tx_device: bool,
    pub rx_device: bool,
    pub symmetric: bool,
    pub hold_send_cn: bool,
    pub jitter_min_ms: u32,
    pub jitter_max_ms: u32,
    pub cname: String,
}

/// Everything `MediaEngine::create_call` builds for the media thread.
pub(crate) struct CallParts {
    pub id: u64,
    pub call_id: String,
    pub events: UnboundedSender<MediaEvent>,
    pub rtp: Arc<UdpSocket>,
    pub rtcp: Arc<UdpSocket>,
    pub rx_q: Receiver<RxMsg>,
    pub rx_cfg: UnboundedSender<RxCtl>,
    pub latched: Latched,
    pub counters: Arc<RxCounters>,
    pub stop: Arc<Notify>,
    pub stats_out: Arc<ArcSwap<CallMediaStats>>,
    pub cfg: CallCfg,
    pub source: TxSource,
    pub record: Option<(PathBuf, bool)>,
    /// Producer side of the RX tap ring (consumed by the call's hub).
    pub rx_tap: HeapProd<i16>,
    /// 48 kHz samples the tap ring had no room for.
    pub rx_tap_dropped: Arc<AtomicU64>,
    pub shared: Arc<CallShared>,
}

/// Counters shared between the media thread and the call handle.
#[derive(Debug, Default)]
pub(crate) struct CallShared {
    /// Clips accepted by the handle and not yet finished (current + queued).
    pub pending_plays: AtomicUsize,
    /// Audio samples (48 kHz) of those clips; generators count as 0.
    pub pending_samples: AtomicU64,
    /// RX frames (20 ms) produced since creation.
    pub rx_frames: AtomicU64,
    /// Set (under the handle's `play_gen` lock) once the handle has been closed; later
    /// `play_clip` calls fail instead of being dropped by the media thread.
    pub closed: std::sync::atomic::AtomicBool,
    /// Set once the call is confirmed (Active); never reset. API clips are held back until
    /// then so they cannot play into early media (a 183 with SDP applies the negotiated media
    /// before the callee answers).
    pub answered: std::sync::atomic::AtomicBool,
    /// Id of the clip currently playing (0 = none), published on each clip transition. The RX
    /// hub counts a barge-in watcher only while its clip is the current one.
    pub current_clip: Arc<AtomicU64>,
    /// A recording is running (`record_start` or a `record:` sink): the on-screen recording
    /// indicator. Only the media thread writes it, and only together with `Call::record`.
    pub recording: std::sync::atomic::AtomicBool,
}

/// Where a clip's samples come from.
pub(crate) enum ClipData {
    Pcm { data: Arc<[i16]>, pos: usize },
    Gen(TxSource),
}

/// One clip in the TX clip layer.
pub(crate) struct Clip {
    /// Play id; 0 = legacy `play()` (no events).
    pub id: u64,
    data: ClipData,
    looped: bool,
    /// Sample limit (from `duration_ms`).
    limit: Option<u64>,
    played: u64,
    gain: f32,
    /// Contribution to `CallShared::pending_samples`.
    pub samples: u64,
}

impl Clip {
    pub fn new(id: u64, data: ClipData, looped: bool, limit: Option<u64>, gain_db: f32) -> Self {
        let samples = match &data {
            ClipData::Pcm { data, .. } => data.len() as u64,
            ClipData::Gen(_) => 0,
        };
        let gain = 10f32.powf(gain_db / 20.0);
        Self {
            id,
            data,
            looped,
            limit,
            played: 0,
            gain: if gain.is_finite() { gain } else { 1.0 },
            samples,
        }
    }

    pub fn played_ms(&self) -> u64 {
        self.played * 1000 / ENGINE_RATE as u64
    }

    /// Writes up to `out.len()` samples; returns (written, finished).
    fn fill(&mut self, out: &mut [f32]) -> (usize, bool) {
        let mut max = out.len();
        if let Some(l) = self.limit {
            max = max.min(l.saturating_sub(self.played) as usize);
        }
        let out = &mut out[..max];
        let gain = self.gain;
        let (n, mut done) = match &mut self.data {
            ClipData::Pcm { data, pos } => {
                let mut w = 0;
                while w < out.len() {
                    if *pos >= data.len() {
                        if self.looped && !data.is_empty() {
                            *pos = 0;
                        } else {
                            break;
                        }
                    }
                    let k = (out.len() - w).min(data.len() - *pos);
                    for (o, s) in out[w..w + k].iter_mut().zip(&data[*pos..*pos + k]) {
                        *o = *s as f32 / 32768.0 * gain;
                    }
                    *pos += k;
                    w += k;
                }
                (w, !self.looped && *pos >= data.len() || data.is_empty())
            }
            ClipData::Gen(src) => {
                src.fill(out);
                if gain != 1.0 {
                    for o in out.iter_mut() {
                        *o *= gain;
                    }
                }
                (out.len(), false)
            }
        };
        self.played += n as u64;
        if self.limit.is_some_and(|l| self.played >= l) {
            done = true;
        }
        (n, done)
    }
}

struct SrtpOut {
    suite: SrtpSuite,
    key: String,
    rtp: SrtpContext,
    rtcp: SrtcpContext,
}

struct DtmfReq {
    digit: char,
    duration_ms: u32,
}

struct InbandReq {
    digit: char,
    duration_ms: u32,
    gap_samples: usize,
}

/// Longest queue of pending inband digits (excess is dropped with an error).
const MAX_INBAND_PENDING: usize = 64;

struct DtmfTx {
    packets: Vec<DtmfPacket>,
    idx: usize,
    ts: u32,
}

struct Recording {
    writer: hound::WavWriter<BufWriter<File>>,
    path: PathBuf,
    stereo: bool,
}

pub(crate) struct DetectJob {
    det: ToneDetector,
    deadline: Instant,
    min_level: f32,
    best_level: f32,
    best_ratio: f32,
    tx: Option<oneshot::Sender<ToneDetectResult>>,
}

pub(crate) struct CallState {
    pub id: u64,
    pub call_id: String,
    events: UnboundedSender<MediaEvent>,
    rtp: Arc<UdpSocket>,
    rtcp: Arc<UdpSocket>,
    rx_q: Receiver<RxMsg>,
    rx_cfg: UnboundedSender<RxCtl>,
    latched: Latched,
    /// Suite and key (`inline:...`) of the inbound SRTP state last handed to the RX task.
    rx_srtp: Option<(SrtpSuite, String)>,
    sock_v6: bool,
    family_err_reported: bool,
    counters: Arc<RxCounters>,
    stop: Arc<Notify>,
    stats_out: Arc<ArcSwap<CallMediaStats>>,
    pub cfg: CallCfg,

    // negotiated
    neg: Option<Negotiated>,
    codec: Option<AudioCodec>,
    pub ptime_ms: u32,
    pub muted: bool,
    pub held: bool,

    // TX
    source: TxSource,
    clip: Option<Clip>,
    clip_queue: VecDeque<Clip>,
    /// Where retired clips go to be freed off the media thread.
    pub(crate) reaper: Option<crossbeam_channel::Sender<Clip>>,
    tx_stream: Option<TxStreamCons>,
    stream_buf: Vec<f32>,
    rx_tap: HeapProd<i16>,
    /// Drained (`swap(0)`) by the RX hub to zero-fill its timeline; never read for stats.
    rx_tap_dropped: Arc<AtomicU64>,
    /// Cumulative samples the tap dropped, for `call.get` stats (media thread only).
    rx_tap_dropped_total: u64,
    rx_frames: u64,
    shared: Arc<CallShared>,
    ssrc: u32,
    seq: u16,
    ts: u32,
    marker_pending: bool,
    paused_since: Option<Instant>,
    last_cn: Option<Instant>,
    encoder: Option<Box<dyn AudioEncoder>>,
    enc_cfg: Option<OpusEncoderConfig>,
    tx_rs: Option<Resampler>,
    pcm_acc: Vec<i16>,
    f32_tmp: Vec<f32>,
    payload: Vec<u8>,
    frame: Vec<f32>,
    srtp_out: Option<SrtpOut>,
    dtmf_queue: VecDeque<DtmfReq>,
    dtmf_cur: Option<DtmfTx>,
    dtmf_gap: u32,
    dtmf_gap_ms: u32,
    inband_q: VecDeque<InbandReq>,
    /// Samples of the digit being played (lazily generated, one digit at a time).
    inband_cur: VecDeque<f32>,
    inband_gap: usize,
    // TX clocking (managed by the media thread loop)
    pub cap_mode: bool,
    pub fallback: bool,
    pub last_tx: Instant,
    pub next_tx: Instant,

    // RX
    jitter: Option<JitterBuffer>,
    decoder: Option<Box<dyn AudioDecoder>>,
    rx_rs: Option<Resampler>,
    rx_pcm: Vec<i16>,
    rx_f32: Vec<f32>,
    rx_acc: Vec<f32>,
    /// Last pulled 20 ms of decoded RX audio at 48 kHz.
    pub rx48: Vec<f32>,
    tap_i16: Vec<i16>,
    remote_ssrc: Option<u32>,
    dtmf_det: DtmfDetector,
    rtcp_stats: RtcpStats,
    last_rx_at: Option<Instant>,
    /// When we last started expecting inbound audio (unhold, new negotiation).
    rx_expected_since: Instant,
    rx_expected_prev: bool,
    rx_active_prev: bool,
    timeout_fired: bool,
    cn_state: bool,
    rx_level: f32,

    // taps
    record: Option<Recording>,
    tx_rec: VecDeque<f32>,
    detects: Vec<DetectJob>,

    // counters
    tx_packets: u64,
    tx_bytes: u64,
    tx_dropped: u64,
    tx_errors: u64,
    rx_packets: u64,
    rx_bytes: u64,
    rx_unknown_pt: u64,
    next_rtcp: Instant,
    next_stats: Instant,
    auth_reported: u64,
    auth_event_at: Instant,
}

fn rand_u32() -> u32 {
    let mut b = [0u8; 4];
    let _ = getrandom::fill(&mut b);
    u32::from_be_bytes(b)
}

fn f32_to_i16(v: f32) -> i16 {
    (v * 32767.0).clamp(-32768.0, 32767.0) as i16
}

impl CallState {
    pub fn new(p: CallParts, now: Instant) -> Self {
        let sock_v6 = p.rtp.local_addr().is_ok_and(|a| a.is_ipv6());
        let mut s = Self {
            id: p.id,
            call_id: p.call_id,
            events: p.events,
            rtp: p.rtp,
            rtcp: p.rtcp,
            rx_q: p.rx_q,
            rx_cfg: p.rx_cfg,
            latched: p.latched,
            rx_srtp: None,
            sock_v6,
            family_err_reported: false,
            counters: p.counters,
            stop: p.stop,
            stats_out: p.stats_out,
            cfg: p.cfg,
            neg: None,
            codec: None,
            ptime_ms: 20,
            muted: false,
            held: false,
            source: p.source,
            clip: None,
            clip_queue: VecDeque::with_capacity(MAX_QUEUED_PLAYS + 2),
            reaper: None,
            tx_stream: None,
            stream_buf: Vec::with_capacity(STREAM_BUF_CAP),
            rx_tap: p.rx_tap,
            rx_tap_dropped: p.rx_tap_dropped,
            rx_tap_dropped_total: 0,
            rx_frames: 0,
            shared: p.shared,
            ssrc: rand_u32(),
            seq: rand_u32() as u16,
            ts: rand_u32(),
            marker_pending: true,
            paused_since: None,
            last_cn: None,
            encoder: None,
            enc_cfg: None,
            tx_rs: None,
            pcm_acc: Vec::new(),
            f32_tmp: Vec::new(),
            payload: Vec::new(),
            frame: Vec::new(),
            srtp_out: None,
            dtmf_queue: VecDeque::new(),
            dtmf_cur: None,
            dtmf_gap: 0,
            dtmf_gap_ms: 70,
            inband_q: VecDeque::new(),
            inband_cur: VecDeque::new(),
            inband_gap: 0,
            cap_mode: false,
            fallback: false,
            last_tx: now,
            next_tx: now,
            jitter: None,
            decoder: None,
            rx_rs: None,
            rx_pcm: Vec::new(),
            rx_f32: Vec::new(),
            rx_acc: Vec::new(),
            rx48: vec![0.0; RX_FRAME_48K],
            tap_i16: Vec::new(),
            remote_ssrc: None,
            dtmf_det: DtmfDetector::new(),
            rtcp_stats: RtcpStats::new(),
            last_rx_at: None,
            rx_expected_since: now,
            rx_expected_prev: false,
            rx_active_prev: false,
            timeout_fired: false,
            cn_state: false,
            rx_level: -120.0,
            record: None,
            tx_rec: VecDeque::new(),
            detects: Vec::new(),
            tx_packets: 0,
            tx_bytes: 0,
            tx_dropped: 0,
            tx_errors: 0,
            rx_packets: 0,
            rx_bytes: 0,
            rx_unknown_pt: 0,
            next_rtcp: now + RTCP_INTERVAL,
            next_stats: now,
            auth_reported: 0,
            auth_event_at: now,
        };
        if let Some((path, stereo)) = p.record {
            s.record_start(path, stereo);
        }
        s
    }

    fn emit(&self, kind: MediaEventKind) {
        let _ = self.events.send(MediaEvent {
            call_id: self.call_id.clone(),
            kind,
        });
    }

    fn error(&self, msg: String) {
        tracing::warn!(call = %self.call_id, "{msg}");
        self.emit(MediaEventKind::Error(msg));
    }

    pub fn frame48(&self) -> usize {
        self.ptime_ms as usize * (ENGINE_RATE as usize / 1000)
    }

    pub fn ptime(&self) -> Duration {
        Duration::from_millis(self.ptime_ms as u64)
    }

    /// Whether the mic (when this call has focus) feeds this call's TX.
    pub fn wants_mic(&self) -> bool {
        self.cfg.tx_device
            && self.clip.is_none()
            && self.tx_stream.is_none()
            && !self.muted
            && !self.held
            && self.neg.as_ref().is_some_and(|n| n.send_enabled)
            && matches!(self.source, TxSource::Device)
    }

    pub fn recv_enabled(&self) -> bool {
        self.neg.as_ref().is_none_or(|n| n.recv_enabled)
    }

    // ── negotiation ─────────────────────────────────────────────────────

    pub fn apply(&mut self, n: Negotiated) {
        let codec = n.codec;
        let ptime = if codec == AudioCodec::Opus {
            clamp_opus_ptime(n.ptime_ms)
        } else {
            n.ptime_ms.clamp(10, 60)
        };
        let pcm_rate = codec.pcm_sample_rate();

        // Receive side.
        let codec_changed = self.codec != Some(codec);
        if codec_changed || self.jitter.is_none() {
            let mk = || -> anyhow::Result<_> {
                Ok((
                    JitterBuffer::new(
                        codec.rtp_clock_rate(),
                        pcm_rate,
                        self.cfg.jitter_min_ms,
                        self.cfg.jitter_max_ms,
                    ),
                    make_decoder(codec)?,
                    Resampler::new(pcm_rate, ENGINE_RATE, (pcm_rate / 50) as usize)?,
                ))
            };
            match mk() {
                Ok((j, d, r)) => {
                    self.jitter = Some(j);
                    self.decoder = Some(d);
                    self.rx_rs = Some(r);
                    self.rx_acc.clear();
                }
                Err(e) => self.error(format!("cannot create {codec:?} decoder: {e}")),
            }
        }

        // Transmit side.
        let enc_cfg = (codec == AudioCodec::Opus).then(|| {
            let o = n.opus_remote.unwrap_or_default();
            OpusEncoderConfig {
                bitrate: None,
                max_bitrate: o.maxaveragebitrate,
                inband_fec: o.useinbandfec,
                packet_loss_pct: if o.useinbandfec { 5 } else { 0 },
                max_playback_rate: o.maxplaybackrate,
            }
        });
        if codec_changed
            || ptime != self.ptime_ms
            || enc_cfg != self.enc_cfg
            || self.encoder.is_none()
        {
            let mk = || -> anyhow::Result<_> {
                Ok((
                    make_encoder(
                        codec,
                        &EncoderConfig {
                            opus: enc_cfg.unwrap_or_default(),
                        },
                    )?,
                    Resampler::new(ENGINE_RATE, pcm_rate, ptime as usize * 48)?,
                ))
            };
            match mk() {
                Ok((e, r)) => {
                    self.encoder = Some(e);
                    self.tx_rs = Some(r);
                    self.pcm_acc.clear();
                }
                Err(e) => {
                    self.encoder = None;
                    self.error(format!("cannot create {codec:?} encoder: {e}"));
                }
            }
            self.enc_cfg = enc_cfg;
        }
        if codec_changed && self.codec.is_some() {
            // The running timestamp stays continuous; only the per-frame
            // increment follows the new clock. Mark the discontinuity.
            self.marker_pending = true;
        }
        self.codec = Some(codec);
        self.ptime_ms = ptime;

        // Outbound SRTP: never recreate unchanged contexts (ROC must survive).
        match &n.srtp {
            None => self.srtp_out = None,
            Some(p) => {
                let keep = self
                    .srtp_out
                    .as_ref()
                    .is_some_and(|o| o.suite == p.suite && o.key == p.local_key_b64);
                if !keep {
                    match (
                        SrtpContext::from_sdes_key(p.suite, &p.local_key_b64),
                        SrtcpContext::from_sdes_key(p.suite, &p.local_key_b64),
                    ) {
                        (Ok(rtp), Ok(rtcp)) => {
                            self.srtp_out = Some(SrtpOut {
                                suite: p.suite,
                                key: p.local_key_b64.clone(),
                                rtp,
                                rtcp,
                            });
                        }
                        (Err(e), _) | (_, Err(e)) => {
                            self.srtp_out = None;
                            self.error(format!("bad local SRTP key: {e}"));
                        }
                    }
                }
            }
        }

        // Inbound state, owned by the RX task: hand it the new configuration.
        let remote_moved = self.neg.as_ref().map(|o| o.remote_rtp) != Some(n.remote_rtp);
        if remote_moved {
            self.latched.store(None);
        }
        let mut audio_pts = vec![n.rx_pt];
        audio_pts.extend(n.rx_alt_pts.iter().copied());
        audio_pts.extend(n.rx_te.map(|t| t.0));
        audio_pts.extend(n.rx_cn_pt);
        let mut err = None;
        let srtp = match &n.srtp {
            None => {
                self.rx_srtp = None;
                SrtpUpdate::Clear
            }
            Some(p) => {
                let keep = !p.remote_changed
                    && self
                        .rx_srtp
                        .as_ref()
                        .is_some_and(|(s, k)| *s == p.suite && *k == p.remote_key_b64);
                if keep {
                    SrtpUpdate::Keep
                } else {
                    match SrtpIn::new(p.suite, &p.remote_key_b64) {
                        Ok(new) => {
                            self.rx_srtp = Some((p.suite, p.remote_key_b64.clone()));
                            SrtpUpdate::Replace(Box::new(new))
                        }
                        Err(e) => {
                            err = Some(format!("bad remote SRTP key: {e}"));
                            SrtpUpdate::Keep
                        }
                    }
                }
            }
        };
        let _ = self.rx_cfg.send(RxCtl::Config(RxConfig {
            mux: n.rtcp_mux,
            symmetric: self.cfg.symmetric,
            reset_latch: remote_moved,
            audio_pts,
            srtp,
        }));
        if let Some(e) = err {
            self.error(e);
        }
        self.neg = Some(n);
    }

    pub fn set_hold(&mut self, hold: bool) {
        self.held = hold;
    }

    /// The local address moved (network change): transmit from, and receive on, these sockets.
    /// The codec state, SRTP contexts, SSRC / sequence numbers, jitter buffer and the latched
    /// remote address stay, so the stream continues without a re-key or a discontinuity.
    pub fn rebind(&mut self, rtp: Arc<UdpSocket>, rtcp: Arc<UdpSocket>) {
        self.sock_v6 = rtp.local_addr().is_ok_and(|a| a.is_ipv6());
        self.family_err_reported = false;
        self.rtp = rtp.clone();
        self.rtcp = rtcp.clone();
        let _ = self.rx_cfg.send(RxCtl::Rebind(rtp, rtcp));
    }

    /// Retires a clip: settles the pending gauges, emits `PlayFinished` and hands the clip to
    /// the reaper thread so its sample buffer is never freed on the real-time media thread
    /// (when the reaper queue is full the clip is dropped in place).
    fn emit_play_finished(&self, clip: Clip, reason: PlayEnd) {
        self.emit_play_finished_at(clip, reason, None);
    }

    fn emit_play_finished_at(&self, clip: Clip, reason: PlayEnd, barge_in_rx_ms: Option<u64>) {
        self.shared.pending_plays.fetch_sub(1, Ordering::Relaxed);
        self.shared
            .pending_samples
            .fetch_sub(clip.samples, Ordering::Relaxed);
        if clip.id != 0 {
            self.emit(MediaEventKind::PlayFinished {
                play_id: clip.id,
                reason,
                played_ms: clip.played_ms(),
                rx_ms: self.rx_frames * 20,
                barge_in_rx_ms: barge_in_rx_ms.filter(|_| reason == PlayEnd::BargeIn),
            });
        }
        if let Some(r) = &self.reaper {
            let _ = r.try_send(clip);
        }
    }

    /// Ends the current clip and starts the next queued one.
    fn finish_current(&mut self, reason: PlayEnd) {
        if let Some(c) = self.clip.take() {
            self.emit_play_finished(c, reason);
        }
        self.clip = self.clip_queue.pop_front();
        self.publish_clip();
    }

    /// Publishes the current clip id to [`CallShared::current_clip`]; 0 when there is none or
    /// it is not audible this frame (waiting for the answer, held, muted, TX paused).
    fn publish_clip(&self) {
        let id = self
            .clip
            .as_ref()
            .filter(|_| self.clips_may_play() && self.tx_flows())
            .map_or(0, |c| c.id);
        self.shared.current_clip.store(id, Ordering::Release);
    }

    /// `PlayClip`: replaces the current clip and queue, or appends.
    pub fn play_clip(&mut self, clip: Clip, queue: bool) {
        if queue {
            if self.clip.is_none() {
                self.clip = Some(clip);
                self.publish_clip();
            } else if self.clip_queue.len() < MAX_QUEUED_PLAYS {
                // Never grows the deque (reserved up front).
                self.clip_queue.push_back(clip);
            } else {
                self.emit_play_finished(clip, PlayEnd::Error);
                self.error("clip queue full".into());
            }
            return;
        }
        if let Some(c) = self.clip.take() {
            self.emit_play_finished(c, PlayEnd::Replaced);
        }
        while let Some(c) = self.clip_queue.pop_front() {
            self.emit_play_finished(c, PlayEnd::Replaced);
        }
        self.clip = Some(clip);
        self.publish_clip();
    }

    /// `StopPlay`: no-op for unknown or finished ids (including `clear_queue`: the queue is
    /// only cleared when `id` is `None` or actually matched the current or a queued clip).
    pub fn stop_play(
        &mut self,
        id: Option<u64>,
        clear_queue: bool,
        reason: PlayEnd,
        barge_in_rx_ms: Option<u64>,
    ) {
        let cur_match = match id {
            Some(id) => self.clip.as_ref().is_some_and(|c| c.id == id),
            None => true,
        };
        let mut matched = id.is_none() || cur_match;
        if cur_match {
            if let Some(c) = self.clip.take() {
                self.emit_play_finished_at(c, reason, barge_in_rx_ms);
            }
            if !clear_queue {
                self.clip = self.clip_queue.pop_front();
            }
        } else if let Some(id) = id
            && let Some(i) = self.clip_queue.iter().position(|c| c.id == id)
            && let Some(c) = self.clip_queue.remove(i)
        {
            matched = true;
            self.emit_play_finished_at(c, reason, barge_in_rx_ms);
        }
        if clear_queue && matched {
            while let Some(c) = self.clip_queue.pop_front() {
                self.emit_play_finished_at(c, reason, barge_in_rx_ms);
            }
        }
        self.publish_clip();
    }

    pub fn attach_tx_stream(&mut self, s: Option<TxStreamCons>) {
        self.tx_stream = s;
    }

    pub fn queue_dtmf(
        &mut self,
        digits: &[char],
        mode: DtmfSendMode,
        duration_ms: u32,
        gap_ms: u32,
    ) {
        self.dtmf_gap_ms = gap_ms;
        match mode {
            DtmfSendMode::Rfc4733 => {
                if self.neg.as_ref().is_some_and(|n| n.te.is_none()) {
                    self.error("telephone-event not negotiated; RFC 4733 DTMF dropped".into());
                    return;
                }
                for &digit in digits {
                    self.dtmf_queue.push_back(DtmfReq { digit, duration_ms });
                }
            }
            DtmfSendMode::Inband => {
                let gap_samples = gap_ms as usize * (ENGINE_RATE as usize / 1000);
                for &digit in digits {
                    if self.inband_q.len() >= MAX_INBAND_PENDING {
                        self.error("inband DTMF queue full; digits dropped".into());
                        break;
                    }
                    self.inband_q.push_back(InbandReq {
                        digit,
                        duration_ms,
                        gap_samples,
                    });
                }
            }
        }
    }

    fn inband_active(&self) -> bool {
        !self.inband_cur.is_empty() || self.inband_gap > 0 || !self.inband_q.is_empty()
    }

    /// Next inband DTMF sample, generating one digit's audio at a time.
    fn inband_sample(&mut self) -> f32 {
        loop {
            if let Some(v) = self.inband_cur.pop_front() {
                return v;
            }
            if self.inband_gap > 0 {
                self.inband_gap -= 1;
                return 0.0;
            }
            let Some(req) = self.inband_q.pop_front() else {
                return 0.0;
            };
            match inband_dtmf_samples(req.digit, req.duration_ms) {
                Some(smp) => {
                    self.inband_cur
                        .extend(smp.iter().map(|&v| v as f32 / 32768.0));
                    self.inband_gap = req.gap_samples;
                }
                None => self.error(format!("invalid DTMF digit {:?}", req.digit)),
            }
        }
    }

    // ── TX ──────────────────────────────────────────────────────────────

    /// Whether this tick's frame will actually be transmitted as audio.
    fn tx_flows(&self) -> bool {
        let Some(n) = self.neg.as_ref() else {
            return false;
        };
        !self.held
            && !self.muted
            && n.send_enabled
            && self.dtmf_cur.is_none()
            && (n.te.is_none() || self.dtmf_queue.is_empty())
    }

    /// Order: inband DTMF > stream TX drain > mute/hold zeros > clip > stream > base source.
    fn fill_frame(&mut self, frame: &mut [f32], mic: Option<&[f32]>) {
        // Stream TX is drained in real time whenever negotiated, even if its audio is unused.
        let mut stream_got = None;
        if self.neg.is_some()
            && let Some(s) = self.tx_stream.as_mut()
        {
            s.check_clear();
            self.stream_buf.clear();
            self.stream_buf.resize(frame.len(), 0.0);
            let got = s.pop(&mut self.stream_buf);
            self.stream_buf[got..].fill(0.0);
            stream_got = Some(got);
        }
        // Tracks audibility every frame (answer, hold, mute) so barge-in only hears audible clips.
        self.publish_clip();
        if self.inband_active() {
            for o in frame.iter_mut() {
                *o = self.inband_sample();
            }
            return;
        }
        if self.muted || self.held {
            return; // zeros
        }
        let flows = self.tx_flows();
        let mut at = 0;
        if self.clip.is_some() {
            if !flows {
                return; // paused: zeros
            }
            // API clips wait for the answer (legacy id-0 clips replace the base source and
            // keep playing); until then the stream / base source carries on.
            if self.clips_may_play() {
                self.play_clips(frame, &mut at);
            }
            if at >= frame.len() {
                return;
            }
        }
        let rest = &mut frame[at..];
        if let Some(s) = self.tx_stream.as_ref() {
            if flows {
                if let Some(got) = stream_got {
                    // Only the part of the frame the clip did not cover needs audio.
                    s.add_underrun(at + rest.len() - got.max(at));
                }
                rest.copy_from_slice(&self.stream_buf[at..at + rest.len()]);
            }
            return;
        }
        match &mut self.source {
            TxSource::Device => {
                if let Some(m) = mic {
                    rest.copy_from_slice(&m[at..]);
                }
            }
            other => other.fill(rest),
        }
    }

    /// Whether the current clip may be transmitted: the call is answered, or the clip is a
    /// legacy (id 0) base-source replacement.
    fn clips_may_play(&self) -> bool {
        self.shared.answered.load(Ordering::Acquire)
            || self.clip.as_ref().is_some_and(|c| c.id == 0)
    }

    /// Fills `frame[*at..]` from the current clip (and the queue behind it), advancing `at`.
    fn play_clips(&mut self, frame: &mut [f32], at: &mut usize) {
        while *at < frame.len() {
            let Some(c) = self.clip.as_mut() else { break };
            let (n, done) = c.fill(&mut frame[*at..]);
            *at += n;
            if done {
                self.finish_current(PlayEnd::Completed);
            } else if n == 0 {
                break;
            }
        }
    }

    /// Produce and send one ptime of TX. `mic` is a frame of exactly
    /// `frame48()` mic samples when this call is capture-paced.
    pub fn tx_tick(&mut self, now: Instant, mic: Option<&[f32]>) {
        let n48 = self.frame48();
        let mut frame = std::mem::take(&mut self.frame);
        frame.clear();
        frame.resize(n48, 0.0);
        self.fill_frame(&mut frame, mic);
        if self.record.as_ref().is_some_and(|r| r.stereo) {
            self.tx_rec.extend(frame.iter().copied());
            let cap = ENGINE_RATE as usize;
            if self.tx_rec.len() > cap {
                let extra = self.tx_rec.len() - cap;
                self.tx_rec.drain(..extra);
            }
        }
        let sendable = !self.held && self.neg.as_ref().is_some_and(|n| n.send_enabled);
        if sendable {
            self.resume_tx(now);
            self.send_tick(now, &frame);
        } else {
            self.pause_tx(now);
        }
        self.frame = frame;
    }

    fn pause_tx(&mut self, now: Instant) {
        let since = *self.paused_since.get_or_insert(now);
        if !(self.held && self.cfg.hold_send_cn) {
            return;
        }
        let Some((cn_pt, clock)) = self
            .neg
            .as_ref()
            .and_then(|n| n.cn_pt.map(|p| (p, n.clock_rate)))
        else {
            return;
        };
        if self
            .last_cn
            .is_some_and(|t| now.duration_since(t) < CN_HOLD_INTERVAL)
        {
            return;
        }
        self.last_cn = Some(now);
        let ticks = (now.duration_since(since).as_millis() as u64 * clock as u64 / 1000) as u32;
        let ts = self.ts.wrapping_add(ticks);
        // RFC 3389: one byte, noise level in -dBov (127 = quietest).
        self.send_rtp(cn_pt, false, ts, &[127]);
    }

    fn resume_tx(&mut self, now: Instant) {
        if let Some(since) = self.paused_since.take() {
            let clock = self.codec.map_or(8000, |c| c.rtp_clock_rate() as u64);
            let ticks = (now.duration_since(since).as_millis() as u64 * clock / 1000) as u32;
            self.ts = self.ts.wrapping_add(ticks);
            self.marker_pending = true;
            if let Some(r) = self.tx_rs.as_mut() {
                r.reset();
            }
            self.pcm_acc.clear();
        }
    }

    fn send_tick(&mut self, _now: Instant, frame: &[f32]) {
        let Some(codec) = self.codec else { return };
        let ts_inc = codec.rtp_ts_per_frame(self.ptime_ms);
        let te = self.neg.as_ref().and_then(|n| n.te);
        let pt = self.neg.as_ref().map_or(0, |n| n.pt);

        // RFC 4733 events take the place of audio packets, one per tick.
        if let Some((te_pt, te_clock)) = te {
            if self.dtmf_cur.is_none() {
                if self.dtmf_gap > 0 {
                    self.dtmf_gap -= 1;
                } else if let Some(req) = self.dtmf_queue.pop_front() {
                    match DtmfGenerator::generate(
                        req.digit,
                        req.duration_ms,
                        DTMF_VOLUME,
                        te_clock,
                        self.ptime_ms,
                    ) {
                        Some(packets) => {
                            self.dtmf_cur = Some(DtmfTx {
                                packets,
                                idx: 0,
                                ts: self.ts,
                            })
                        }
                        None => self.error(format!("cannot generate DTMF {:?}", req.digit)),
                    }
                }
            }
            if let Some(mut ev) = self.dtmf_cur.take() {
                let p = &ev.packets[ev.idx];
                let (marker, payload, ts) = (p.marker, p.payload.clone(), ev.ts);
                self.send_rtp(te_pt, marker, ts, &payload);
                ev.idx += 1;
                if ev.idx < ev.packets.len() {
                    self.dtmf_cur = Some(ev);
                } else {
                    self.dtmf_gap = self.dtmf_gap_ms.div_ceil(self.ptime_ms);
                }
                self.ts = self.ts.wrapping_add(ts_inc);
                // Keep the resampler/encoder state flowing without sending.
                self.encode(frame, codec, pt, ts_inc, false);
                return;
            }
        } else if !self.dtmf_queue.is_empty() {
            self.dtmf_queue.clear();
        }
        self.encode(frame, codec, pt, ts_inc, true);
    }

    fn encode(&mut self, frame: &[f32], codec: AudioCodec, pt: u8, ts_inc: u32, send: bool) {
        let (Some(rs), Some(mut enc)) = (self.tx_rs.as_mut(), self.encoder.take()) else {
            return;
        };
        self.f32_tmp.clear();
        rs.process(frame, &mut self.f32_tmp);
        self.pcm_acc
            .extend(self.f32_tmp.iter().map(|&v| f32_to_i16(v)));
        let need = codec.samples_per_frame(self.ptime_ms);
        if !send {
            self.pcm_acc.clear();
            self.encoder = Some(enc);
            return;
        }
        if self.pcm_acc.len() > need * 4 {
            let extra = self.pcm_acc.len() - need * 2;
            self.pcm_acc.drain(..extra);
        }
        while self.pcm_acc.len() >= need {
            let mut payload = std::mem::take(&mut self.payload);
            let r = enc.encode(&self.pcm_acc[..need], &mut payload);
            self.pcm_acc.drain(..need);
            match r {
                Ok(()) => {
                    let marker = std::mem::take(&mut self.marker_pending);
                    let ts = self.ts;
                    self.send_rtp(pt, marker, ts, &payload);
                    self.ts = self.ts.wrapping_add(ts_inc);
                }
                Err(e) => {
                    self.tx_errors += 1;
                    tracing::warn!(call = %self.call_id, "encode failed: {e}");
                }
            }
            self.payload = payload;
        }
        self.encoder = Some(enc);
    }

    fn rtp_dest(&self) -> Option<SocketAddr> {
        let neg = self.neg.as_ref()?;
        let latched = if self.cfg.symmetric {
            self.latched.load().as_deref().copied()
        } else {
            None
        };
        let d = latched.unwrap_or(neg.remote_rtp);
        (!d.ip().is_unspecified() && d.port() != 0).then_some(d)
    }

    /// Make `d` sendable from our socket: v4 -> v4-mapped on an IPv6 socket;
    /// anything else across families is `None`.
    fn map_family(&self, d: SocketAddr) -> Option<SocketAddr> {
        match (self.sock_v6, d) {
            (true, SocketAddr::V4(v4)) => Some(SocketAddr::new(
                std::net::IpAddr::V6(v4.ip().to_ipv6_mapped()),
                v4.port(),
            )),
            (false, SocketAddr::V6(_)) => None,
            _ => Some(d),
        }
    }

    fn send_rtp(&mut self, pt: u8, marker: bool, ts: u32, payload: &[u8]) {
        let pkt = RtpHeader::build(pt, self.seq, ts, self.ssrc, marker, payload);
        self.seq = self.seq.wrapping_add(1);
        let pkt = match self.srtp_out.as_mut() {
            Some(s) => match s.rtp.protect(&pkt) {
                Ok(p) => p,
                Err(e) => {
                    self.tx_errors += 1;
                    tracing::warn!(call = %self.call_id, "SRTP protect failed: {e}");
                    return;
                }
            },
            None => pkt,
        };
        let Some(dest) = self.rtp_dest() else { return };
        let Some(dest) = self.map_family(dest) else {
            self.tx_errors += 1;
            if !self.family_err_reported {
                self.family_err_reported = true;
                self.error(format!(
                    "remote address family mismatch: cannot send to {dest} from an IPv4 socket"
                ));
            }
            return;
        };
        match self.rtp.try_send_to(&pkt, dest) {
            Ok(_) => {
                self.tx_packets += 1;
                self.tx_bytes += payload.len() as u64;
                self.rtcp_stats.record_sent(payload.len());
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => self.tx_dropped += 1,
            Err(_) => self.tx_errors += 1,
        }
    }

    // ── RX ──────────────────────────────────────────────────────────────

    pub fn drain_rx(&mut self) {
        while let Ok(msg) = self.rx_q.try_recv() {
            match msg {
                RxMsg::Rtp(p) => self.on_rtp(p),
                RxMsg::Rtcp(d) => self.on_rtcp(&d),
            }
        }
    }

    fn on_rtp(&mut self, p: RxPacket) {
        let Some(neg) = self.neg.as_ref() else {
            self.rx_unknown_pt += 1;
            return;
        };
        let clock = neg.clock_rate;
        let is_audio = p.pt == neg.rx_pt || neg.rx_alt_pts.contains(&p.pt);
        let te = neg.rx_te.filter(|t| t.0 == p.pt);
        let is_cn = neg.rx_cn_pt == Some(p.pt);
        self.last_rx_at = Some(p.arrival);
        self.timeout_fired = false;
        self.rx_packets += 1;
        self.rx_bytes += p.payload.len() as u64;
        if self.remote_ssrc.is_some_and(|s| s != p.ssrc) {
            // A new source has an unrelated timestamp base.
            self.dtmf_det.reset();
        }
        self.remote_ssrc = Some(p.ssrc);
        if is_audio {
            self.rtcp_stats
                .record_received(p.ssrc, p.seq, p.ts, p.payload.len(), clock);
            self.cn_state = false;
            if let Some(j) = self.jitter.as_mut() {
                j.push(p.seq, p.ts, p.marker, p.ssrc, p.payload, p.arrival);
                if j.take_decoder_reset()
                    && let Some(codec) = self.codec
                    && let Ok(d) = make_decoder(codec)
                {
                    self.decoder = Some(d);
                }
            }
        } else if let Some((_, te_clock)) = te {
            self.rtcp_stats
                .record_received(p.ssrc, p.seq, p.ts, p.payload.len(), te_clock);
            if let Some(j) = self.jitter.as_mut() {
                j.note_non_audio(p.seq, p.ts);
            }
            if let Some(ev) = self.dtmf_det.process(&p.payload, p.ts, te_clock) {
                self.emit(MediaEventKind::DtmfReceived {
                    digit: ev.digit,
                    duration_ms: ev.duration_ms,
                    source: DtmfSource::Rfc4733,
                });
            }
        } else if is_cn {
            self.rtcp_stats
                .record_received(p.ssrc, p.seq, p.ts, p.payload.len(), clock);
            self.cn_state = true;
            if let Some(j) = self.jitter.as_mut() {
                j.push_comfort_noise();
                j.note_non_audio(p.seq, p.ts);
            }
        } else {
            self.rx_unknown_pt += 1;
        }
    }

    fn on_rtcp(&mut self, data: &[u8]) {
        for p in parse_rtcp(data) {
            match p {
                RtcpPacket::SenderReport(sr) => {
                    self.rtcp_stats.process_sr(&sr);
                    for b in &sr.report_blocks {
                        self.rtcp_stats.process_rr(b, self.ssrc);
                    }
                }
                RtcpPacket::ReceiverReport(rr) => {
                    for b in &rr.report_blocks {
                        self.rtcp_stats.process_rr(b, self.ssrc);
                    }
                }
                RtcpPacket::Bye(_) => {}
            }
        }
    }

    /// Pull 20 ms of decoded RX audio into `self.rx48` and run the taps
    /// (level meter, tone detectors, recorder). Silence is produced while no
    /// stream is active so taps keep a continuous timeline.
    pub fn pull_rx(&mut self, now: Instant) {
        if let (Some(j), Some(d), Some(rs)) = (
            self.jitter.as_mut(),
            self.decoder.as_mut(),
            self.rx_rs.as_mut(),
        ) {
            j.pull(d.as_mut(), &mut self.rx_pcm, now);
            self.rx_f32.clear();
            self.rx_f32
                .extend(self.rx_pcm.iter().map(|&s| s as f32 / 32768.0));
            rs.process(&self.rx_f32, &mut self.rx_acc);
        }
        self.rx48.clear();
        let take = self.rx_acc.len().min(RX_FRAME_48K);
        self.rx48.extend(self.rx_acc.drain(..take));
        self.rx48.resize(RX_FRAME_48K, 0.0);
        if self.rx_acc.len() > RX_FRAME_48K * 3 {
            self.rx_acc.clear();
        }
        if !self.recv_enabled() {
            self.rx48.fill(0.0);
        }
        self.run_taps(Instant::now());
    }

    fn run_taps(&mut self, now: Instant) {
        self.tap_i16.clear();
        self.tap_i16
            .extend(self.rx48.iter().map(|&v| f32_to_i16(v)));
        self.rx_level = rms_dbfs(&self.tap_i16);
        self.rx_frames += 1;
        self.shared
            .rx_frames
            .store(self.rx_frames, Ordering::Relaxed);
        let pushed = self.rx_tap.push_slice(&self.tap_i16);
        if pushed < self.tap_i16.len() {
            let lost = (self.tap_i16.len() - pushed) as u64;
            self.rx_tap_dropped_total += lost;
            self.rx_tap_dropped.fetch_add(lost, Ordering::Relaxed);
        }

        for job in &mut self.detects {
            if job.tx.is_none() {
                continue;
            }
            if job.det.feed(&self.tap_i16).is_some()
                && let Some(b) = job.det.last_block()
            {
                if b.level_dbfs > job.best_level {
                    job.best_level = b.level_dbfs;
                    job.best_ratio = b.power_ratio;
                }
                if b.power_ratio >= DETECT_MIN_RATIO && b.level_dbfs >= job.min_level {
                    if let Some(tx) = job.tx.take() {
                        let _ = tx.send(ToneDetectResult {
                            detected: true,
                            level_dbfs: b.level_dbfs,
                            power_ratio: b.power_ratio,
                        });
                    }
                    continue;
                }
            }
            if now >= job.deadline
                && let Some(tx) = job.tx.take()
            {
                let _ = tx.send(ToneDetectResult {
                    detected: false,
                    level_dbfs: job.best_level,
                    power_ratio: job.best_ratio,
                });
            }
        }
        self.detects.retain(|j| j.tx.is_some());

        let mut failed = None;
        if let Some(rec) = self.record.as_mut() {
            let mut res = Ok(());
            for &v in &self.tap_i16 {
                res = rec.writer.write_sample(v);
                if res.is_ok() && rec.stereo {
                    let r = self.tx_rec.pop_front().map_or(0, f32_to_i16);
                    res = rec.writer.write_sample(r);
                }
                if res.is_err() {
                    break;
                }
            }
            if let Err(e) = res {
                failed = Some(format!("recording write failed: {e}"));
            }
        }
        if let Some(msg) = failed {
            self.record = None;
            self.shared.recording.store(false, Ordering::Release);
            self.error(msg);
        }
    }

    // ── test hooks ──────────────────────────────────────────────────────

    pub fn add_detect(
        &mut self,
        freq_hz: f32,
        timeout: Duration,
        min_level_dbfs: f32,
        tx: oneshot::Sender<ToneDetectResult>,
        now: Instant,
    ) {
        self.detects.push(DetectJob {
            det: ToneDetector::new(ENGINE_RATE, freq_hz, DETECT_BLOCK_MS),
            deadline: now + timeout,
            min_level: min_level_dbfs,
            best_level: -120.0,
            best_ratio: 0.0,
            tx: Some(tx),
        });
    }

    pub fn record_start(&mut self, path: PathBuf, stereo: bool) {
        // The flag stays on across a rotation (old file finished, new one started).
        self.finish_recording();
        let spec = hound::WavSpec {
            channels: if stereo { 2 } else { 1 },
            sample_rate: ENGINE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        match hound::WavWriter::create(&path, spec) {
            Ok(writer) => {
                self.tx_rec.clear();
                self.record = Some(Recording {
                    writer,
                    path,
                    stereo,
                });
                self.shared.recording.store(true, Ordering::Release);
            }
            Err(e) => {
                self.shared.recording.store(false, Ordering::Release);
                self.error(format!("cannot create {}: {e}", path.display()));
            }
        }
    }

    pub fn record_stop(&mut self) {
        self.finish_recording();
        self.shared.recording.store(false, Ordering::Release);
    }

    /// Closes the running recording's file (event `RecordFinished`) without touching the flag.
    fn finish_recording(&mut self) {
        if let Some(rec) = self.record.take() {
            let path = rec.path;
            match rec.writer.finalize() {
                Ok(()) => self.emit(MediaEventKind::RecordFinished { path }),
                Err(e) => self.error(format!("finalizing {}: {e}", path.display())),
            }
        }
    }

    // ── RTCP, events, stats ─────────────────────────────────────────────

    fn send_rtcp(&mut self) {
        let (Some(neg), Some(codec)) = (self.neg.as_ref(), self.codec) else {
            return;
        };
        if self.remote_ssrc.is_none() && self.tx_packets == 0 {
            return;
        }
        let (mux, remote_rtp, remote_rtcp) = (neg.rtcp_mux, neg.remote_rtp, neg.remote_rtcp);
        let clock = codec.rtp_clock_rate();
        let pkt = build_report_with_cname(
            self.ssrc,
            self.remote_ssrc.unwrap_or(0),
            &mut self.rtcp_stats,
            self.ts,
            clock,
            &self.cfg.cname,
        );
        let pkt = match self.srtp_out.as_mut() {
            Some(s) => match s.rtcp.protect_rtcp(&pkt) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(call = %self.call_id, "SRTCP protect failed: {e}");
                    return;
                }
            },
            None => pkt,
        };
        let (sock, dest) = if mux {
            (&self.rtp, self.rtp_dest().unwrap_or(remote_rtp))
        } else {
            let d = remote_rtcp.unwrap_or_else(|| {
                SocketAddr::new(remote_rtp.ip(), remote_rtp.port().wrapping_add(1))
            });
            (&self.rtcp, d)
        };
        if dest.ip().is_unspecified() || dest.port() == 0 {
            return;
        }
        let Some(dest) = self.map_family(dest) else {
            return;
        };
        let _ = sock.try_send_to(&pkt, dest);
    }

    fn rx_is_active(&self, now: Instant) -> bool {
        self.last_rx_at
            .is_some_and(|t| now.saturating_duration_since(t) < RX_ACTIVE_WINDOW)
            || (self.cn_state && self.last_rx_at.is_some() && !self.timeout_fired)
    }

    /// Whether we currently expect inbound audio (not held, remote not
    /// holding, receive direction negotiated).
    fn rx_expected(&self) -> bool {
        !self.held
            && self
                .neg
                .as_ref()
                .is_some_and(|n| !n.remote_hold && n.recv_enabled)
    }

    /// Periodic non-audio work: events, RTCP, stats publication.
    pub fn housekeeping(&mut self, now: Instant) {
        let active = self.rx_is_active(now);
        if active != self.rx_active_prev {
            self.rx_active_prev = active;
            self.emit(MediaEventKind::RxActive(active));
        }
        let expected = self.rx_expected();
        if expected && !self.rx_expected_prev {
            // Audio is wanted again (unhold, new negotiation): the timeout
            // clock restarts now, not at the last packet before the hold.
            self.rx_expected_since = now;
            self.timeout_fired = false;
        }
        self.rx_expected_prev = expected;
        if expected && !self.timeout_fired {
            let since = self
                .last_rx_at
                .map_or(self.rx_expected_since, |t| t.max(self.rx_expected_since));
            if now.saturating_duration_since(since) > MEDIA_TIMEOUT {
                self.timeout_fired = true;
                self.emit(MediaEventKind::MediaTimeout);
            }
        }
        if let Some(te_clock) = self.neg.as_ref().and_then(|n| n.rx_te).map(|t| t.1)
            && let Some(ev) = self.dtmf_det.check_timeout(te_clock)
        {
            self.emit(MediaEventKind::DtmfReceived {
                digit: ev.digit,
                duration_ms: ev.duration_ms,
                source: DtmfSource::Rfc4733,
            });
        }
        let fails = self.counters.auth_failures.load(Ordering::Relaxed);
        if fails > self.auth_reported
            && now.duration_since(self.auth_event_at) >= Duration::from_secs(1)
        {
            self.auth_reported = fails;
            self.auth_event_at = now;
            self.emit(MediaEventKind::SrtpAuthFailures(fails));
        }
        for job in &mut self.detects {
            if now >= job.deadline
                && let Some(tx) = job.tx.take()
            {
                let _ = tx.send(ToneDetectResult {
                    detected: false,
                    level_dbfs: job.best_level,
                    power_ratio: job.best_ratio,
                });
            }
        }
        self.detects.retain(|j| j.tx.is_some());
        if now >= self.next_rtcp {
            self.next_rtcp = now + RTCP_INTERVAL;
            self.send_rtcp();
        }
        if now >= self.next_stats {
            self.next_stats = now + STATS_INTERVAL;
            self.publish_stats(now);
        }
    }

    fn publish_stats(&self, now: Instant) {
        let c = &self.counters;
        let clock = self.codec.map_or(8000, |c| c.rtp_clock_rate()) as f32;
        let rtcp = RtcpSummary {
            rx_cumulative_lost: self.rtcp_stats.cumulative_lost(),
            rx_jitter_ms: self.rtcp_stats.jitter as f32 / 1000.0,
            remote_loss: self
                .rtcp_stats
                .remote_receiver_report()
                .map(|r| r.packets_lost),
            remote_jitter_ms: self
                .rtcp_stats
                .remote_receiver_report()
                .map(|r| r.jitter as f32 * 1000.0 / clock),
            rtt_ms: self.rtcp_stats.rtt_ms,
        };
        let s = CallMediaStats {
            codec: self.codec,
            pt: self.neg.as_ref().map(|n| n.pt),
            ptime_ms: self.ptime_ms,
            srtp_suite: self.srtp_out.as_ref().map(|o| o.suite),
            remote_rtp: self.neg.as_ref().map(|n| n.remote_rtp),
            tx_packets: self.tx_packets,
            tx_bytes: self.tx_bytes,
            tx_dropped: self.tx_dropped,
            tx_errors: self.tx_errors,
            rx_packets: self.rx_packets,
            rx_bytes: self.rx_bytes,
            rx_unknown_pt: self.rx_unknown_pt,
            rx_queue_drops: c.queue_drops.load(Ordering::Relaxed),
            rx_source_drops: c.source_drops.load(Ordering::Relaxed),
            rx_early_drops: c.early_drops.load(Ordering::Relaxed),
            rtcp_rx_packets: c.rtcp_packets.load(Ordering::Relaxed),
            srtp_auth_failures: c.auth_failures.load(Ordering::Relaxed),
            jitter: self.jitter.as_ref().map(|j| j.stats()).unwrap_or_default(),
            rtcp,
            rx_active: self.rx_is_active(now),
            last_rx_at: self.last_rx_at,
            tx_paused: self.paused_since.is_some(),
            muted: self.muted,
            held: self.held,
            rx_level_dbfs: self.rx_level,
            rx_ms: self.rx_frames * 20,
            play_current: self.clip.as_ref().map(|c| c.id).filter(|&id| id != 0),
            play_queued: self.clip_queue.len() as u32,
            tx_stream: self.tx_stream.is_some(),
            rx_tap_dropped_ms: self.rx_tap_dropped_total / (ENGINE_RATE as u64 / 1000),
        };
        self.stats_out.store(Arc::new(s));
    }

    /// Tear down: stop the RX task, finish the recording, fail pending detects.
    pub fn shutdown(&mut self, now: Instant) {
        self.stop_play(None, true, PlayEnd::CallEnded, None);
        self.tx_stream = None;
        self.record_stop();
        for job in &mut self.detects {
            if let Some(tx) = job.tx.take() {
                let _ = tx.send(ToneDetectResult {
                    detected: false,
                    level_dbfs: job.best_level,
                    power_ratio: job.best_ratio,
                });
            }
        }
        self.detects.clear();
        self.publish_stats(now);
        self.stop.notify_one();
    }
}
