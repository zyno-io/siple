//! Public media API: [`MediaEngine`] (one media thread, local tones) and
//! [`CallMediaHandle`] (one per call).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::{ArcSwap, ArcSwapOption};
use crossbeam_channel::Sender;
use siple_media::sdp_neg::Negotiated;
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc::UnboundedSender, oneshot};

use ringbuf::HeapRb;
use ringbuf::traits::Split;

use super::call::{CallCfg, CallParts, CallShared, Clip, ClipData};
use super::ports::allocate_pair;
use super::rx::{self, Latched, RxCounters};
use super::sources::{ENGINE_RATE, TxSource};
use super::stream_tx::{self, TX_STREAM_SAMPLES, TxStreamProducer};
use super::thread::{self, CallCmd, Cmd};
use super::{
    CallMediaSpec, CallMediaStats, ClipSource, DtmfSendMode, LocalTone, MediaEvent, PlayEnd,
    PlayRequest, ToneDetectResult, ToneId,
};
use crate::agent::hub::{self, HubHandle, StopPlayFn};
use crate::agent::{MAX_QUEUED_PLAYS, MAX_QUEUED_SECS, RX_TAP_SAMPLES};
use crate::audio::AudioHandle;
use crate::config::{AudioSink, AudioSource};

/// Most digits accepted by one inband `send_dtmf` call.
pub const MAX_INBAND_DIGITS: usize = 32;
/// Longest inband tone per digit.
pub const MAX_INBAND_DIGIT_MS: u32 = 2000;
/// Longest gap between digits.
pub const MAX_DTMF_GAP_MS: u32 = 5000;

/// Bound on the RX task -> media thread queue (about 10 s of 20 ms packets).
const RX_QUEUE: usize = 512;

struct Chan {
    tx: Sender<Cmd>,
    thread: Thread,
}

impl Chan {
    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
        self.thread.unpark();
    }
}

/// Owns the media thread. Dropping it stops the thread (calls are closed).
pub struct MediaEngine {
    chan: Arc<Chan>,
    runtime: tokio::runtime::Handle,
    events: UnboundedSender<MediaEvent>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    next_call: AtomicU64,
    next_tone: AtomicU64,
    /// Engine-global play ids (allocated before the command is sent).
    next_play: Arc<AtomicU64>,
    /// Id (`cN`) of the call that owns the microphone, published by the media thread.
    focus: Arc<ArcSwapOption<String>>,
    /// Microphone level and echo-test progress, published by the media thread.
    meters: Arc<super::Meters>,
}

impl MediaEngine {
    /// Start the media thread. `audio = None` runs everything on the virtual
    /// clock (device-bound calls then behave as silence/null).
    pub fn start(
        audio: Option<AudioHandle>,
        runtime: tokio::runtime::Handle,
        events: UnboundedSender<MediaEvent>,
    ) -> MediaEngine {
        let (tx, rx) = crossbeam_channel::unbounded();
        let focus = Arc::new(ArcSwapOption::empty());
        let meters = Arc::new(super::Meters::default());
        let join = thread::spawn(audio, events.clone(), rx, focus.clone(), meters.clone())
            .expect("spawn media thread");
        MediaEngine {
            chan: Arc::new(Chan {
                tx,
                thread: join.thread().clone(),
            }),
            runtime,
            events,
            join: Mutex::new(Some(join)),
            next_call: AtomicU64::new(1),
            next_tone: AtomicU64::new(1),
            next_play: Arc::new(AtomicU64::new(1)),
            focus,
            meters,
        }
    }

    /// RMS level of the latest microphone block in dBFS (-120 = silence); `None` while no
    /// microphone is open. Updated whenever capture runs, call or not.
    pub fn mic_level_dbfs(&self) -> Option<f32> {
        let v = f32::from_bits(self.meters.mic_level.load(Ordering::Relaxed));
        (!v.is_nan()).then_some(v)
    }

    /// Starts the echo test: records the microphone for `secs` (1-10), then plays the recording
    /// on the speaker. Fails while a test is running.
    pub fn echo_test_start(&self, secs: u32) -> Result<(), String> {
        let secs = secs.clamp(1, 10);
        {
            let mut st = self.meters.echo.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(
                st.phase,
                super::EchoPhase::Recording | super::EchoPhase::Playing
            ) {
                return Err("an echo test is already running".into());
            }
            *st = super::EchoStatus {
                phase: super::EchoPhase::Recording,
                total_ms: u64::from(secs) * 2000,
                ..Default::default()
            };
        }
        self.chan.send(Cmd::EchoStart(secs));
        Ok(())
    }

    /// Cancels a running echo test and resets its status.
    pub fn echo_test_stop(&self) {
        self.chan.send(Cmd::EchoStop);
    }

    /// Progress and outcome of the echo test.
    pub fn echo_test_status(&self) -> super::EchoStatus {
        self.meters
            .echo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The call that currently owns the microphone (media focus), by its call id. The focus
    /// follows answers and resumes and leaves a held call for an un-held device call; `None`
    /// when no device-bound call exists. A held call keeps the focus only while no un-held
    /// device call exists.
    pub fn focus(&self) -> Option<String> {
        self.focus.load().as_deref().cloned()
    }

    /// Allocate sockets and register a call. The negotiated parameters
    /// arrive later through [`CallMediaHandle::apply`]; the ports are
    /// available immediately so the SDP can carry them.
    pub fn create_call(&self, spec: CallMediaSpec) -> Result<CallMediaHandle> {
        let source = TxSource::load(&spec.source)
            .with_context(|| format!("audio source {}", spec.source))?;
        let pair = allocate_pair(spec.bind_ip, spec.port_range)?;
        let rtp_port = pair.rtp_port;
        let (rtp, rtcp) = {
            let _g = self.runtime.enter();
            (
                Arc::new(UdpSocket::from_std(pair.rtp)?),
                Arc::new(UdpSocket::from_std(pair.rtcp)?),
            )
        };
        let id = self.next_call.fetch_add(1, Ordering::Relaxed);
        let (q_tx, q_rx) = crossbeam_channel::bounded(RX_QUEUE);
        let (cfg_tx, cfg_rx) = tokio::sync::mpsc::unbounded_channel();
        let latched: Latched = Arc::new(arc_swap::ArcSwapOption::empty());
        let counters = Arc::new(RxCounters::default());
        let stop = Arc::new(Notify::new());
        self.runtime.spawn(rx::run(
            rtp.clone(),
            rtcp.clone(),
            cfg_rx,
            latched.clone(),
            counters.clone(),
            q_tx,
            stop.clone(),
        ));
        let stats = Arc::new(ArcSwap::from_pointee(CallMediaStats::default()));
        let record = match &spec.sink {
            AudioSink::Record(p) => Some((p.clone(), false)),
            _ => None,
        };
        let (rx_tap, tap_cons) = HeapRb::<i16>::new(RX_TAP_SAMPLES).split();
        let rx_tap_dropped = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(CallShared::default());
        // Built from the channel + media id, not a handle clone (that would be an Arc cycle).
        let stop_play: StopPlayFn = {
            let chan = self.chan.clone();
            Arc::new(move |play_id, onset_rx_ms| {
                chan.send(Cmd::Call(
                    id,
                    CallCmd::StopPlay {
                        id: Some(play_id),
                        clear_queue: false,
                        reason: PlayEnd::BargeIn,
                        barge_in_rx_ms: Some(onset_rx_ms),
                    },
                ))
            })
        };
        let current_clip = shared.current_clip.clone();
        let hub = hub::spawn(
            &self.runtime,
            tap_cons,
            rx_tap_dropped.clone(),
            current_clip,
            stop_play,
            spec.rx_history_secs,
        );
        let parts = CallParts {
            id,
            call_id: spec.call_id.clone(),
            events: self.events.clone(),
            rtp,
            rtcp,
            rx_q: q_rx,
            rx_cfg: cfg_tx,
            latched,
            counters,
            stop,
            stats_out: stats.clone(),
            cfg: CallCfg {
                tx_device: spec.device_bound && spec.source == AudioSource::Device,
                rx_device: spec.device_bound && spec.sink == AudioSink::Device,
                symmetric: spec.symmetric_rtp,
                hold_send_cn: spec.hold_send_cn,
                jitter_min_ms: spec.jitter_min_ms,
                jitter_max_ms: spec.jitter_max_ms,
                cname: spec.cname.clone(),
            },
            source,
            record,
            rx_tap,
            rx_tap_dropped,
            shared: shared.clone(),
        };
        self.chan.send(Cmd::AddCall(Box::new(parts)));
        Ok(CallMediaHandle {
            inner: Arc::new(HandleInner {
                id,
                call_id: spec.call_id,
                chan: self.chan.clone(),
                runtime: self.runtime.clone(),
                events: self.events.clone(),
                local: Mutex::new(LocalAddr {
                    bind_ip: spec.bind_ip,
                    advertise_ip: spec.advertise_ip,
                    rtp_port,
                }),
                port_range: spec.port_range,
                stats,
                play_gen: Mutex::new(0),
                hub,
                shared,
                next_play: self.next_play.clone(),
            }),
        })
    }

    /// Start a local UI tone (ringtone, ringback, ...). Non-repeating tones
    /// end by themselves.
    pub fn start_local_tone(&self, tone: LocalTone) -> ToneId {
        let id = ToneId(self.next_tone.fetch_add(1, Ordering::Relaxed));
        self.chan.send(Cmd::StartTone(id, tone));
        id
    }

    pub fn stop_local_tone(&self, id: ToneId) {
        self.chan.send(Cmd::StopTone(id));
    }
}

impl Drop for MediaEngine {
    fn drop(&mut self) {
        self.chan.send(Cmd::Shutdown);
        let j = self.join.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(j) = j
            && j.thread().id() != std::thread::current().id()
        {
            let _ = j.join();
        }
    }
}

#[derive(Clone, Copy)]
struct LocalAddr {
    bind_ip: IpAddr,
    advertise_ip: IpAddr,
    rtp_port: u16,
}

struct HandleInner {
    id: u64,
    call_id: String,
    chan: Arc<Chan>,
    runtime: tokio::runtime::Handle,
    events: UnboundedSender<MediaEvent>,
    /// Where the call's media lives now (changes with [`CallMediaHandle::rebind`]).
    local: Mutex<LocalAddr>,
    /// `[media] rtp_port_range`, for a rebind.
    port_range: [u16; 2],
    stats: Arc<ArcSwap<CallMediaStats>>,
    /// Generation of the latest `play`/`stop_play`; commands are sent while
    /// holding it so a slow file load can never overtake a later command.
    play_gen: Mutex<u64>,
    hub: HubHandle,
    shared: Arc<CallShared>,
    next_play: Arc<AtomicU64>,
}

impl Drop for HandleInner {
    fn drop(&mut self) {
        self.hub.close();
        self.chan.send(Cmd::Close(self.id));
    }
}

/// Why [`CallMediaHandle::play_clip`] refused a clip.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayError {
    /// Too many clips (or too much audio) already pending on this call.
    #[error("play queue is full")]
    QueueFull,
    /// The source cannot be played (bad file, `device`, ...).
    #[error("{0}")]
    Source(String),
    /// The call's media has been closed.
    #[error("call media is closed")]
    Closed,
}

/// Handle to one call's media. Cloneable; the call is closed when the last
/// clone is dropped (or on [`close`](Self::close)). All methods are
/// non-blocking.
#[derive(Clone)]
pub struct CallMediaHandle {
    inner: Arc<HandleInner>,
}

impl CallMediaHandle {
    fn cmd(&self, c: CallCmd) {
        self.inner.chan.send(Cmd::Call(self.inner.id, c));
    }

    pub fn call_id(&self) -> &str {
        &self.inner.call_id
    }

    /// Local RTP port to advertise in SDP (even).
    pub fn local_rtp_port(&self) -> u16 {
        self.local().rtp_port
    }

    /// Local RTCP port (`rtp + 1`).
    pub fn local_rtcp_port(&self) -> u16 {
        self.local().rtp_port + 1
    }

    pub fn bind_ip(&self) -> IpAddr {
        self.local().bind_ip
    }

    /// Address to put in SDP `c=` / `o=` (as given at creation, or at the last rebind).
    pub fn advertise_ip(&self) -> IpAddr {
        self.local().advertise_ip
    }

    pub fn local_rtp_addr(&self) -> SocketAddr {
        let l = self.local();
        SocketAddr::new(l.advertise_ip, l.rtp_port)
    }

    fn local(&self) -> LocalAddr {
        *self.inner.local.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Moves the call's media to a new local address (network change): allocates a fresh
    /// RTP/RTCP pair on `bind_ip` from `[media] rtp_port_range` and switches transmit and
    /// receive to it. Codec state, SRTP contexts, SSRC / sequence numbers and the jitter buffer
    /// stay. Returns the new RTP port to advertise in SDP; on error nothing changed.
    pub fn rebind(&self, bind_ip: IpAddr, advertise_ip: IpAddr) -> Result<u16> {
        let pair = allocate_pair(bind_ip, self.inner.port_range)?;
        let rtp_port = pair.rtp_port;
        let (rtp, rtcp) = {
            let _g = self.inner.runtime.enter();
            (
                Arc::new(UdpSocket::from_std(pair.rtp)?),
                Arc::new(UdpSocket::from_std(pair.rtcp)?),
            )
        };
        {
            let mut l = self.inner.local.lock().unwrap_or_else(|e| e.into_inner());
            *l = LocalAddr {
                bind_ip,
                advertise_ip,
                rtp_port,
            };
        }
        self.cmd(CallCmd::Rebind(rtp, rtcp));
        Ok(rtp_port)
    }

    /// (Re)configure from a completed offer/answer. Safe to call on every
    /// re-INVITE: unchanged SRTP keys keep their contexts (ROC survives).
    pub fn apply(&self, negotiated: Negotiated) {
        self.cmd(CallCmd::Apply(Box::new(negotiated)));
    }

    /// Gate TX (sockets stay bound; RTCP continues).
    pub fn set_hold(&self, hold: bool) {
        self.cmd(CallCmd::Hold(hold));
    }

    /// Send silence instead of the source while keeping RTP flowing.
    pub fn set_mute(&self, mute: bool) {
        self.cmd(CallCmd::Mute(mute));
    }

    /// Queue DTMF digits (`0-9 * # A-D`), paced on the media thread.
    pub fn send_dtmf(
        &self,
        digits: &str,
        mode: DtmfSendMode,
        duration_ms: u32,
        gap_ms: u32,
    ) -> Result<()> {
        let digits: Vec<char> = digits.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.is_empty() {
            bail!("no DTMF digits");
        }
        if let Some(bad) = digits
            .iter()
            .find(|c| siple_media::dtmf::dtmf_frequencies(**c).is_none())
        {
            bail!("invalid DTMF digit {bad:?}");
        }
        if duration_ms == 0 || duration_ms > siple_media::dtmf::MAX_EVENT_DURATION_MS {
            bail!("invalid DTMF duration {duration_ms} ms");
        }
        if gap_ms > MAX_DTMF_GAP_MS {
            bail!("DTMF gap {gap_ms} ms exceeds {MAX_DTMF_GAP_MS} ms");
        }
        if mode == DtmfSendMode::Inband {
            if digits.len() > MAX_INBAND_DIGITS {
                bail!(
                    "too many inband DTMF digits ({} > {MAX_INBAND_DIGITS})",
                    digits.len()
                );
            }
            if duration_ms > MAX_INBAND_DIGIT_MS {
                bail!(
                    "inband DTMF digit duration {duration_ms} ms exceeds {MAX_INBAND_DIGIT_MS} ms"
                );
            }
        }
        self.cmd(CallCmd::Dtmf {
            digits,
            mode,
            duration_ms,
            gap_ms,
        });
        Ok(())
    }

    /// Hands a clip to the media thread, keeping the pending gauges in step.
    /// Callers hold the `play_gen` lock.
    fn send_clip(&self, clip: Clip, queue: bool) {
        let sh = &self.inner.shared;
        sh.pending_plays.fetch_add(1, Ordering::Relaxed);
        sh.pending_samples
            .fetch_add(clip.samples, Ordering::Relaxed);
        self.cmd(CallCmd::PlayClip {
            clip,
            queue,
            shared: sh.clone(),
        });
    }

    /// Replace the TX source mid-call (`Device` restores the call's own
    /// source). WAV files are loaded off the media thread; load errors are
    /// reported as [`MediaEventKind::Error`](super::MediaEventKind::Error).
    /// Compatibility wrapper: loops until stopped and emits no `PlayFinished`.
    pub fn play(&self, source: AudioSource) {
        match source {
            AudioSource::Device => self.stop_play(),
            AudioSource::File(path) => {
                let gen_at_call = self.bump_play_gen();
                let this = self.clone();
                self.inner.runtime.spawn_blocking(move || {
                    let loaded = TxSource::load(&AudioSource::File(path.clone()));
                    let g = this
                        .inner
                        .play_gen
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    // Superseded by a later play / stop_play: drop.
                    if *g != gen_at_call {
                        return;
                    }
                    match loaded {
                        Ok(s) => {
                            this.send_clip(Clip::new(0, ClipData::Gen(s), true, None, 0.0), false)
                        }
                        Err(e) => {
                            let _ = this.inner.events.send(MediaEvent {
                                call_id: this.inner.call_id.clone(),
                                kind: super::MediaEventKind::Error(format!(
                                    "play {}: {e}",
                                    path.display()
                                )),
                            });
                        }
                    }
                });
            }
            other => match TxSource::load(&other) {
                Ok(s) => {
                    let mut g = self
                        .inner
                        .play_gen
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    *g += 1;
                    self.send_clip(Clip::new(0, ClipData::Gen(s), true, None, 0.0), false);
                }
                Err(e) => tracing::warn!("play: {e}"),
            },
        }
    }

    fn bump_play_gen(&self) -> u64 {
        let mut g = self
            .inner
            .play_gen
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *g += 1;
        *g
    }

    /// Stops the current clip and clears the queue (compatibility wrapper).
    pub fn stop_play(&self) {
        let mut g = self
            .inner
            .play_gen
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *g += 1;
        self.cmd(CallCmd::StopPlay {
            id: None,
            clear_queue: true,
            reason: PlayEnd::Stopped,
            barge_in_rx_ms: None,
        });
    }

    /// Whether an API clip is playing or queued right now (live gauge, unlike the ~50 ms stats
    /// snapshot): a clip queued now waits instead of starting at once.
    pub fn play_busy(&self) -> bool {
        self.inner.shared.pending_plays.load(Ordering::Relaxed) > 0
    }

    /// Submits a clip and returns its play id. The id is allocated before the command is sent;
    /// exactly one [`MediaEventKind::PlayFinished`](super::MediaEventKind::PlayFinished) follows.
    /// With `queue` the clip waits behind the current one (error when the queue is full);
    /// otherwise it replaces the current clip and queue. A `Spec(File)` source is decoded here
    /// (blocking); callers normally pass decoded `Pcm`.
    pub fn play_clip(&self, req: PlayRequest) -> Result<u64, PlayError> {
        let data = match req.source {
            ClipSource::Pcm(d) => ClipData::Pcm { data: d, pos: 0 },
            ClipSource::Spec(AudioSource::Device) => {
                return Err(PlayError::Source("cannot play the device source".into()));
            }
            ClipSource::Spec(AudioSource::File(p)) => {
                let v = super::sources::load_wav_48k(&p)
                    .map_err(|e| PlayError::Source(format!("{e:#}")))?;
                let pcm: Arc<[i16]> = v
                    .iter()
                    .map(|&x| (x * 32767.0).clamp(-32768.0, 32767.0) as i16)
                    .collect();
                ClipData::Pcm { data: pcm, pos: 0 }
            }
            ClipSource::Spec(other) => ClipData::Gen(
                TxSource::load(&other).map_err(|e| PlayError::Source(format!("{e:#}")))?,
            ),
        };
        let limit = req
            .duration_ms
            .map(|ms| ms as u64 * (ENGINE_RATE as u64 / 1000));
        let id = self.inner.next_play.fetch_add(1, Ordering::Relaxed);
        let clip = Clip::new(id, data, req.looped, limit, req.gain_db);
        let mut g = self
            .inner
            .play_gen
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.inner.shared.closed.load(Ordering::Acquire) {
            return Err(PlayError::Closed);
        }
        if req.queue {
            let sh = &self.inner.shared;
            let plays = sh.pending_plays.load(Ordering::Relaxed);
            let samples = sh.pending_samples.load(Ordering::Relaxed);
            // One current clip plus MAX_QUEUED_PLAYS waiting.
            // Nothing pending means the clip starts at once (it is not really queued), so a
            // long file clip is fine then; memory stays bounded by the clip cap (600 s).
            if plays > MAX_QUEUED_PLAYS
                || (plays > 0
                    && samples + clip.samples > MAX_QUEUED_SECS as u64 * ENGINE_RATE as u64)
            {
                return Err(PlayError::QueueFull);
            }
        } else {
            *g += 1; // supersedes a pending legacy file load
        }
        self.send_clip(clip, req.queue);
        Ok(id)
    }

    /// Marks the call as answered (confirmed): queued and future API clips may now be
    /// transmitted. Idempotent; never reset.
    pub fn mark_answered(&self) {
        self.inner.shared.answered.store(true, Ordering::Release);
    }

    /// A recording of the call is running (`call.record_start`, or a `record:` sink).
    pub fn is_recording(&self) -> bool {
        self.inner.shared.recording.load(Ordering::Acquire)
    }

    /// Whether [`mark_answered`](Self::mark_answered) ran.
    pub fn is_answered(&self) -> bool {
        self.inner.shared.answered.load(Ordering::Acquire)
    }

    /// Stops play `id` (no-op if unknown or finished), or the current clip when `None`, with
    /// reason `stopped`. `clear_queue` also drops every queued clip.
    pub fn stop_play_id(&self, id: Option<u64>, clear_queue: bool) {
        let mut g = self
            .inner
            .play_gen
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if id.is_none() {
            *g += 1;
        }
        self.cmd(CallCmd::StopPlay {
            id,
            clear_queue,
            reason: PlayEnd::Stopped,
            barge_in_rx_ms: None,
        });
    }

    /// Attaches a TX stream (48 kHz mono `f32`, 2 s ring) that replaces the base source while
    /// no clip plays. Replaces a previously attached stream.
    pub fn attach_tx_stream(&self) -> TxStreamProducer {
        let (prod, cons) = stream_tx::pair(TX_STREAM_SAMPLES);
        self.cmd(CallCmd::AttachTxStream(Some(cons)));
        prod
    }

    /// Detaches the TX stream; the base source resumes.
    pub fn detach_tx_stream(&self) {
        self.cmd(CallCmd::AttachTxStream(None));
    }

    /// The call's RX hub.
    pub fn hub(&self) -> &HubHandle {
        &self.inner.hub
    }

    /// RX timeline position in ms (20 ms per RX frame since the media was created).
    pub fn rx_ms(&self) -> u64 {
        self.inner.shared.rx_frames.load(Ordering::Relaxed) * 20
    }

    /// Record decoded RX audio to a 48 kHz 16-bit WAV (`stereo`: L = RX, R = TX).
    pub fn record_start(&self, path: impl Into<PathBuf>, stereo: bool) {
        self.cmd(CallCmd::RecordStart {
            path: path.into(),
            stereo,
        });
    }

    /// Finish the recording ([`MediaEventKind::RecordFinished`](super::MediaEventKind::RecordFinished) follows).
    pub fn record_stop(&self) {
        self.cmd(CallCmd::RecordStop);
    }

    /// Goertzel-detect `freq_hz` on decoded RX audio. Resolves as soon as a
    /// 40 ms block holds the tone (power ratio >= 0.5, level >=
    /// `min_level_dbfs`), or with `detected: false` after `timeout`.
    pub fn detect_tone(
        &self,
        freq_hz: f32,
        timeout: Duration,
        min_level_dbfs: f32,
    ) -> oneshot::Receiver<ToneDetectResult> {
        let (tx, rx) = oneshot::channel();
        self.cmd(CallCmd::Detect {
            freq_hz,
            timeout,
            min_level_dbfs,
            tx,
        });
        rx
    }

    /// Latest stats snapshot (refreshed about every 50 ms).
    pub fn stats(&self) -> CallMediaStats {
        (**self.inner.stats.load()).clone()
    }

    /// Give this (device-bound) call the microphone.
    pub fn set_focus(&self) {
        self.inner.chan.send(Cmd::SetFocus(self.inner.id));
    }

    /// Close the call now (idempotent; also happens on drop of the last clone).
    pub fn close(&self) {
        {
            // Under the play_gen lock so a concurrent play_clip either lands before the Close
            // command (FIFO: it still gets `CallEnded`) or sees the flag and errors.
            let _g = self
                .inner
                .play_gen
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.inner.shared.closed.store(true, Ordering::Release);
            self.inner.chan.send(Cmd::Close(self.inner.id));
        }
        self.inner.hub.close();
    }
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<CallMediaHandle>();
    f::<MediaEngine>();
    let _ = anyhow!("x");
}
