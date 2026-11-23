//! The media thread: capture, per-call TX/RX clocking, mixing, playback.
//!
//! One loop iteration (woken by the audio notifier, a command, or a timeout
//! of at most [`MAX_SLEEP`]):
//! 1. apply pending commands;
//! 2. drain the capture ring -> 48 kHz mic buffer;
//! 3. TX: capture-paced for the focused device call, absolute-deadline
//!    virtual clock for everything else (and as fallback);
//! 4. RX: drain per-call packet queues;
//! 5. playback: keep the speaker ring ~40 ms full (device calls + local
//!    tones), pull non-device sinks on a 20 ms virtual clock;
//! 6. housekeeping (events, RTCP, stats).

use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_channel::Receiver;
use siple_media::dtmf::dtmf_frequencies;
use siple_media::tone::{Segment, ToneGenerator, ToneSpec};
use tokio::sync::oneshot;

use super::call::{CallShared, CallState, Clip, RX_FRAME_48K};
use super::sources::ENGINE_RATE;
use super::stream_tx::TxStreamCons;
use super::{
    DtmfSendMode, LocalTone, LocalToneKind, MediaEvent, MediaEventKind, PlayEnd, ToneDetectResult,
    ToneId, ToneTarget,
};
use crate::audio::{AudioHandle, Resampler};
use crate::media::call::CallParts;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc::UnboundedSender;

const MAX_SLEEP: Duration = Duration::from_millis(5);
const MIN_SLEEP: Duration = Duration::from_micros(200);
const VIRT_RX_STEP: Duration = Duration::from_millis(20);
const TONE_AMPLITUDE: i16 = 6000;
/// Mic backlog kept per call (in ptime frames) before old audio is dropped.
const MAX_MIC_FRAMES: usize = 6;

pub(crate) enum CallCmd {
    Apply(Box<siple_media::sdp_neg::Negotiated>),
    /// New RTP / RTCP sockets after a network change.
    Rebind(Arc<tokio::net::UdpSocket>, Arc<tokio::net::UdpSocket>),
    Hold(bool),
    Mute(bool),
    Dtmf {
        digits: Vec<char>,
        mode: DtmfSendMode,
        duration_ms: u32,
        gap_ms: u32,
    },
    PlayClip {
        clip: Clip,
        queue: bool,
        /// The handle's gauges the clip was counted in (to settle them if the call is gone).
        shared: Arc<CallShared>,
    },
    /// No-op for unknown or finished ids.
    StopPlay {
        id: Option<u64>,
        clear_queue: bool,
        reason: PlayEnd,
        /// Onset of the interruption for reason `barge_in`.
        barge_in_rx_ms: Option<u64>,
    },
    AttachTxStream(Option<TxStreamCons>),
    RecordStart {
        path: PathBuf,
        stereo: bool,
        /// Sent once the file was created (or creating it failed) and the recording flag is set.
        ack: Option<oneshot::Sender<()>>,
    },
    RecordStop,
    Detect {
        freq_hz: f32,
        timeout: Duration,
        min_level_dbfs: f32,
        tx: oneshot::Sender<ToneDetectResult>,
    },
}

pub(crate) enum Cmd {
    AddCall(Box<CallParts>),
    Call(u64, CallCmd),
    Close(u64),
    SetFocus(u64),
    StartTone(ToneId, LocalTone),
    StopTone(ToneId),
    /// Record the microphone for `secs`, then play it back.
    EchoStart(u32),
    EchoStop,
    Shutdown,
}

/// A running echo test: the recording (48 kHz mono) and the playback position.
struct EchoRun {
    buf: Vec<f32>,
    target: usize,
    playing: bool,
    pos: usize,
}

struct ActiveTone {
    id: ToneId,
    target: ToneTarget,
    tgen: ToneGenerator,
    buf: Vec<i16>,
}

fn tone_spec(kind: &LocalToneKind) -> Option<ToneSpec> {
    Some(match kind {
        LocalToneKind::Ringtone => ToneSpec {
            freqs: [440.0, 480.0],
            segments: vec![
                Segment { on: true, ms: 2000 },
                Segment {
                    on: false,
                    ms: 4000,
                },
            ],
            repeat: true,
        },
        LocalToneKind::Ringback => ToneSpec::ringback(),
        LocalToneKind::CallWaiting => ToneSpec::call_waiting(),
        LocalToneKind::Busy => ToneSpec::busy(),
        LocalToneKind::Reorder => ToneSpec::reorder(),
        LocalToneKind::DtmfFeedback(c) => {
            let (lo, hi) = dtmf_frequencies(*c)?;
            ToneSpec {
                freqs: [lo, hi],
                segments: vec![Segment { on: true, ms: 120 }],
                repeat: false,
            }
        }
        LocalToneKind::Custom(s) => s.clone(),
    })
}

/// Smooth saturation above 0.8 full scale, hard limit at 1.0.
fn soft_clip(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.8 {
        x
    } else {
        x.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
}

pub(crate) struct MediaThread {
    audio: Option<AudioHandle>,
    events: UnboundedSender<MediaEvent>,
    cmds: Receiver<Cmd>,
    calls: Vec<CallState>,
    focus: Option<u64>,
    /// The focused call's id (`cN`), published for [`super::MediaEngine::focus`].
    focus_out: Arc<ArcSwapOption<String>>,
    tones: Vec<ActiveTone>,
    meters: Arc<super::Meters>,
    echo: Option<EchoRun>,

    // capture
    audio_gen: u64,
    cap_rate: Option<u32>,
    cap_rs: Option<Resampler>,
    cap_scratch: Vec<f32>,
    cap_tmp: Vec<f32>,
    mic_buf: Vec<f32>,

    // playback
    play_rate: Option<u32>,
    play_rs: Option<Resampler>,
    ringer_rate: Option<u32>,
    ringer_rs: Option<Resampler>,
    mix: Vec<f32>,
    dev_out: Vec<f32>,
    virt_rx_next: Instant,
    /// Whether anything currently needs the virtual RX clock.
    virt_needed: bool,
    shutdown: bool,
    /// Retired clips are freed on a low-priority thread, off the real-time loop.
    reaper: crossbeam_channel::Sender<Clip>,
}

impl MediaThread {
    pub fn new(
        audio: Option<AudioHandle>,
        events: UnboundedSender<MediaEvent>,
        cmds: Receiver<Cmd>,
    ) -> Self {
        let (reaper, reaper_rx) = crossbeam_channel::bounded::<Clip>(64);
        // Ends when the media thread and every call have dropped their senders.
        let _ = std::thread::Builder::new()
            .name("zp-media-reaper".into())
            .spawn(move || while reaper_rx.recv().is_ok() {});
        Self {
            reaper,
            audio,
            events,
            cmds,
            calls: Vec::new(),
            focus: None,
            focus_out: Arc::new(ArcSwapOption::empty()),
            tones: Vec::new(),
            meters: Arc::default(),
            echo: None,
            audio_gen: 0,
            cap_rate: None,
            cap_rs: None,
            cap_scratch: vec![0.0; 4096],
            cap_tmp: Vec::new(),
            mic_buf: Vec::new(),
            play_rate: None,
            play_rs: None,
            ringer_rate: None,
            ringer_rs: None,
            mix: vec![0.0; RX_FRAME_48K],
            dev_out: Vec::new(),
            virt_rx_next: Instant::now(),
            virt_needed: false,
            shutdown: false,
        }
    }

    pub fn run(mut self) {
        let rt = super::rt::promote(512, ENGINE_RATE);
        if let Some(a) = &self.audio {
            a.notifier().register_current_thread();
        }
        while !self.shutdown {
            let now = Instant::now();
            self.step(now);
            let wait = self.next_wait(Instant::now());
            match &self.audio {
                Some(a) => {
                    a.notifier().wait_timeout(wait);
                }
                None => std::thread::park_timeout(wait),
            }
        }
        let now = Instant::now();
        for c in &mut self.calls {
            c.shutdown(now);
        }
        if let Some(h) = rt {
            super::rt::demote(h);
        }
    }

    fn step(&mut self, now: Instant) {
        self.handle_cmds(now);
        self.check_audio_state();
        if self.echo.is_some() && (self.cap_rs.is_none() || self.play_rs.is_none()) {
            self.echo_abort("the audio device went away");
        }
        self.pump_capture();
        self.service_tx(now);
        for c in &mut self.calls {
            c.drain_rx();
        }
        self.service_playback(now);
        self.service_virtual_rx(now);
        for c in &mut self.calls {
            c.housekeeping(now);
        }
    }

    fn next_wait(&self, now: Instant) -> Duration {
        let mut deadline = now + MAX_SLEEP;
        for c in &self.calls {
            if !c.cap_mode || c.fallback {
                deadline = deadline.min(c.next_tx);
            } else {
                deadline = deadline.min(c.last_tx + c.ptime().mul_f32(1.5));
            }
        }
        if self.virt_needed {
            deadline = deadline.min(self.virt_rx_next);
        }
        deadline.saturating_duration_since(now).max(MIN_SLEEP)
    }

    // ── commands ────────────────────────────────────────────────────────

    /// Keeps the focus on a usable call: the focused call must exist, and when it is held an
    /// un-held device call takes over (the mic must not stay bound to a held call while
    /// another line is live). With no un-held candidate the focus stays where it is.
    fn pick_focus(&mut self) {
        let cur = self
            .focus
            .and_then(|f| self.calls.iter().find(|c| c.id == f));
        let usable = cur.is_some_and(|c| !c.held);
        if !usable {
            let next = self
                .calls
                .iter()
                .find(|c| c.cfg.tx_device && !c.held)
                .map(|c| c.id);
            self.focus = match (next, cur) {
                (Some(n), _) => Some(n),
                (None, Some(c)) => Some(c.id),
                (None, None) => self.calls.iter().find(|c| c.cfg.tx_device).map(|c| c.id),
            };
        }
    }

    fn publish_focus(&self) {
        let id = self
            .focus
            .and_then(|f| self.calls.iter().find(|c| c.id == f))
            .map(|c| c.call_id.clone());
        if id.as_deref() != self.focus_out.load().as_deref().map(String::as_str) {
            self.focus_out.store(id.map(Arc::new));
        }
    }

    fn handle_cmds(&mut self, now: Instant) {
        loop {
            let cmd = match self.cmds.try_recv() {
                Ok(c) => c,
                Err(_) => break,
            };
            match cmd {
                Cmd::AddCall(parts) => {
                    let mut cs = CallState::new(*parts, now);
                    cs.reaper = Some(self.reaper.clone());
                    self.calls.push(cs);
                    self.pick_focus();
                    // The recording must not be mixed into a live call.
                    self.echo_abort("a call started");
                }
                Cmd::Call(id, cc) => {
                    if let Some(c) = self.calls.iter_mut().find(|c| c.id == id) {
                        match cc {
                            CallCmd::Apply(n) => c.apply(*n),
                            CallCmd::Rebind(rtp, rtcp) => c.rebind(rtp, rtcp),
                            CallCmd::Hold(h) => {
                                c.set_hold(h);
                                self.pick_focus();
                            }
                            CallCmd::Mute(m) => c.muted = m,
                            CallCmd::Dtmf {
                                digits,
                                mode,
                                duration_ms,
                                gap_ms,
                            } => c.queue_dtmf(&digits, mode, duration_ms, gap_ms),
                            CallCmd::PlayClip { clip, queue, .. } => c.play_clip(clip, queue),
                            CallCmd::StopPlay {
                                id,
                                clear_queue,
                                reason,
                                barge_in_rx_ms,
                            } => c.stop_play(id, clear_queue, reason, barge_in_rx_ms),
                            CallCmd::AttachTxStream(s) => c.attach_tx_stream(s),
                            CallCmd::RecordStart { path, stereo, ack } => {
                                c.record_start(path, stereo);
                                if let Some(ack) = ack {
                                    let _ = ack.send(());
                                }
                            }
                            CallCmd::RecordStop => c.record_stop(),
                            CallCmd::Detect {
                                freq_hz,
                                timeout,
                                min_level_dbfs,
                                tx,
                            } => c.add_detect(freq_hz, timeout, min_level_dbfs, tx, now),
                        }
                    } else {
                        match cc {
                            CallCmd::Detect { tx, .. } => {
                                let _ = tx.send(ToneDetectResult {
                                    detected: false,
                                    level_dbfs: -120.0,
                                    power_ratio: 0.0,
                                });
                            }
                            // The call is gone (the handle normally refuses first): settle the
                            // gauges the handle incremented so nothing waits on them.
                            CallCmd::PlayClip { clip, shared, .. } => {
                                shared.pending_plays.fetch_sub(1, Ordering::Relaxed);
                                shared
                                    .pending_samples
                                    .fetch_sub(clip.samples, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    }
                }
                Cmd::Close(id) => {
                    if let Some(pos) = self.calls.iter().position(|c| c.id == id) {
                        let mut c = self.calls.remove(pos);
                        c.shutdown(now);
                        self.pick_focus();
                    }
                }
                Cmd::SetFocus(id) => {
                    // Only device-bound calls can own the devices.
                    if self
                        .calls
                        .iter()
                        .any(|c| c.id == id && (c.cfg.tx_device || c.cfg.rx_device))
                    {
                        self.focus = Some(id);
                    }
                }
                Cmd::StartTone(id, t) => match tone_spec(&t.kind) {
                    Some(spec) => self.tones.push(ActiveTone {
                        id,
                        target: t.target,
                        tgen: ToneGenerator::new(spec, TONE_AMPLITUDE),
                        buf: Vec::new(),
                    }),
                    None => {
                        let _ = self.events.send(MediaEvent {
                            call_id: String::new(),
                            kind: MediaEventKind::Error(format!("invalid local tone {:?}", t.kind)),
                        });
                    }
                },
                Cmd::StopTone(id) => self.tones.retain(|t| t.id != id),
                Cmd::EchoStart(secs) => self.echo_start(secs),
                Cmd::EchoStop => {
                    self.echo = None;
                    *lock_echo(&self.meters) = super::EchoStatus::default();
                }
                Cmd::Shutdown => self.shutdown = true,
            }
        }
        self.publish_focus();
    }

    // ── audio device state ──────────────────────────────────────────────

    fn check_audio_state(&mut self) {
        let Some(a) = &self.audio else { return };
        let generation = a.generation();
        let rebuilt = generation != self.audio_gen;
        self.audio_gen = generation;

        let cap = a.capture_rate();
        if cap.is_none() {
            self.meters
                .mic_level
                .store(f32::NAN.to_bits(), std::sync::atomic::Ordering::Relaxed);
        }
        if cap != self.cap_rate || (rebuilt && cap.is_some()) {
            self.cap_rate = cap;
            self.cap_rs = cap.and_then(|r| {
                Resampler::new(r, ENGINE_RATE, (r / 100).max(1) as usize)
                    .map_err(|e| tracing::warn!("capture resampler: {e}"))
                    .ok()
            });
            self.mic_buf.clear();
        }
        let play = a.playback_rate();
        if play != self.play_rate || (rebuilt && play.is_some()) {
            self.play_rate = play;
            self.play_rs = play.and_then(|r| {
                Resampler::new(ENGINE_RATE, r, RX_FRAME_48K)
                    .map_err(|e| tracing::warn!("playback resampler: {e}"))
                    .ok()
            });
        }
        let ringer = if a.ringer_is_separate() {
            a.ringer_rate()
        } else {
            None
        };
        if ringer != self.ringer_rate || (rebuilt && ringer.is_some()) {
            self.ringer_rate = ringer;
            self.ringer_rs = ringer.and_then(|r| {
                Resampler::new(ENGINE_RATE, r, RX_FRAME_48K)
                    .map_err(|e| tracing::warn!("ringer resampler: {e}"))
                    .ok()
            });
        }
    }

    fn pump_capture(&mut self) {
        let (Some(a), Some(rs)) = (&self.audio, self.cap_rs.as_mut()) else {
            return;
        };
        loop {
            let n = a.capture_read(&mut self.cap_scratch);
            if n == 0 {
                break;
            }
            self.cap_tmp.clear();
            rs.process(&self.cap_scratch[..n], &mut self.cap_tmp);
            if !self.cap_tmp.is_empty() {
                let level = rms_dbfs(&self.cap_tmp);
                self.meters
                    .mic_level
                    .store(level.to_bits(), std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(e) = self.echo.as_mut()
                && !e.playing
            {
                e.buf.extend_from_slice(&self.cap_tmp);
                echo_progress(&self.meters, e);
            }
            self.mic_buf.extend_from_slice(&self.cap_tmp);
            if n < self.cap_scratch.len() {
                break;
            }
        }
    }

    // ── TX ──────────────────────────────────────────────────────────────

    fn service_tx(&mut self, now: Instant) {
        let have_mic = self.cap_rs.is_some();
        let focus = self.focus;
        let mut mic_used = false;
        for c in &mut self.calls {
            let eligible = have_mic && Some(c.id) == focus && c.wants_mic();
            if eligible != c.cap_mode {
                c.cap_mode = eligible;
                c.fallback = false;
                c.last_tx = now;
                c.next_tx = now;
            }
            if eligible {
                mic_used = true;
                let n48 = c.frame48();
                if c.fallback && self.mic_buf.len() >= n48 {
                    // Capture is back: resync on fresh audio only.
                    let keep = n48 * 2;
                    if self.mic_buf.len() > keep {
                        let drop = self.mic_buf.len() - keep;
                        self.mic_buf.drain(..drop);
                    }
                    c.fallback = false;
                    c.last_tx = now;
                }
                if !c.fallback {
                    // Bound the backlog before sending so a stall never
                    // releases an unbounded burst of old audio.
                    if self.mic_buf.len() > n48 * MAX_MIC_FRAMES {
                        let drop = self.mic_buf.len() - n48 * MAX_MIC_FRAMES;
                        self.mic_buf.drain(..drop);
                    }
                    let mut off = 0;
                    while self.mic_buf.len() - off >= n48 {
                        c.tx_tick(now, Some(&self.mic_buf[off..off + n48]));
                        off += n48;
                        c.last_tx = now;
                        c.next_tx = now + c.ptime();
                    }
                    self.mic_buf.drain(..off);
                    if now.duration_since(c.last_tx) > c.ptime().mul_f32(1.5) {
                        c.fallback = true;
                        c.next_tx = now;
                    }
                }
            }
            if !eligible || c.fallback {
                virtual_tx(c, now);
            }
        }
        if !mic_used {
            self.mic_buf.clear();
        }
    }

    // ── RX / playback ───────────────────────────────────────────────────

    fn tone_frame_add(&mut self, wanted: impl Fn(ToneTarget) -> bool) -> bool {
        let mut any = false;
        let mix = &mut self.mix;
        for t in &mut self.tones {
            if !wanted(t.target) {
                continue;
            }
            any = true;
            t.buf.resize(RX_FRAME_48K, 0);
            t.tgen.fill(&mut t.buf);
            for (m, s) in mix.iter_mut().zip(t.buf.iter()) {
                *m += *s as f32 / 32768.0;
            }
        }
        self.tones
            .retain(|t| !(wanted(t.target) && t.tgen.is_finished()));
        any
    }

    /// Starts the echo test unless the microphone or the speaker is missing.
    fn echo_start(&mut self, secs: u32) {
        let fail = |m: &Self, why: &str| {
            *lock_echo(&m.meters) = super::EchoStatus {
                phase: super::EchoPhase::Failed,
                error: Some(why.into()),
                ..Default::default()
            };
        };
        if self.cap_rs.is_none() {
            return fail(self, "no microphone is open");
        }
        if self.play_rs.is_none() {
            return fail(self, "no speaker is open");
        }
        let target = secs as usize * ENGINE_RATE as usize;
        let mut run = EchoRun {
            buf: Vec::with_capacity(target),
            target,
            playing: false,
            pos: 0,
        };
        echo_progress(&self.meters, &mut run);
        self.echo = Some(run);
    }

    /// Ends a running echo test with a reason.
    fn echo_abort(&mut self, why: &str) {
        if self.echo.take().is_some() {
            *lock_echo(&self.meters) = super::EchoStatus {
                phase: super::EchoPhase::Failed,
                error: Some(why.into()),
                ..Default::default()
            };
        }
    }

    /// Adds the next 20 ms of the echo recording to the speaker mix; `true` while it plays.
    fn echo_frame_add(&mut self) -> bool {
        let Some(e) = self.echo.as_mut().filter(|e| e.playing) else {
            return false;
        };
        for (i, m) in self.mix.iter_mut().enumerate() {
            *m += e.buf.get(e.pos + i).copied().unwrap_or(0.0);
        }
        e.pos += RX_FRAME_48K;
        echo_progress(&self.meters, e);
        if e.pos >= e.buf.len() {
            self.echo = None;
        }
        true
    }

    fn service_playback(&mut self, _now: Instant) {
        let Some(a) = self.audio.clone() else { return };
        let ringer_sep = self.ringer_rs.is_some();

        if let (Some(rate), true) = (self.play_rate, self.play_rs.is_some()) {
            let target = (rate / 25) as usize; // 40 ms
            for _ in 0..8 {
                if a.playback_fill() >= target {
                    break;
                }
                self.mix.fill(0.0);
                let mut any = false;
                let now = Instant::now();
                for c in &mut self.calls {
                    if c.cfg.rx_device {
                        c.pull_rx(now);
                        any = true;
                        for (m, s) in self.mix.iter_mut().zip(c.rx48.iter()) {
                            *m += *s;
                        }
                    }
                }
                any |= self.tone_frame_add(|t| t == ToneTarget::Speaker || !ringer_sep);
                any |= self.echo_frame_add();
                if !any {
                    break;
                }
                for m in &mut self.mix {
                    *m = soft_clip(*m);
                }
                self.dev_out.clear();
                if let Some(rs) = self.play_rs.as_mut() {
                    rs.process(&self.mix, &mut self.dev_out);
                }
                a.playback_write(&self.dev_out);
            }
        }

        if let (Some(rate), true) = (self.ringer_rate, ringer_sep) {
            let target = (rate / 25) as usize;
            for _ in 0..8 {
                if a.ringer_fill() >= target {
                    break;
                }
                self.mix.fill(0.0);
                if !self.tone_frame_add(|t| t == ToneTarget::Ringer) {
                    break;
                }
                for m in &mut self.mix {
                    *m = soft_clip(*m);
                }
                self.dev_out.clear();
                if let Some(rs) = self.ringer_rs.as_mut() {
                    rs.process(&self.mix, &mut self.dev_out);
                }
                a.ringer_write(&self.dev_out);
            }
        }
    }

    /// 20 ms virtual clock for sinks without a playback device (null/record,
    /// or no audio hardware) and for tones with no output to play on.
    fn service_virtual_rx(&mut self, now: Instant) {
        let play = self.play_rs.is_some();
        let ringer_sep = self.ringer_rs.is_some();
        let tone_consumer = |t: ToneTarget| match t {
            ToneTarget::Speaker => play,
            ToneTarget::Ringer => ringer_sep || play,
        };
        let needed = self.calls.iter().any(|c| !(c.cfg.rx_device && play))
            || self.tones.iter().any(|t| !tone_consumer(t.target));
        self.virt_needed = needed;
        if !needed {
            self.virt_rx_next = now;
            return;
        }
        while now >= self.virt_rx_next {
            let t = Instant::now();
            for c in &mut self.calls {
                if !(c.cfg.rx_device && play) {
                    c.pull_rx(t);
                }
            }
            self.mix.fill(0.0);
            self.tone_frame_add(|tg| !tone_consumer(tg));
            self.virt_rx_next += VIRT_RX_STEP;
            if now > self.virt_rx_next + 5 * VIRT_RX_STEP {
                self.virt_rx_next = now;
            }
        }
    }
}

/// Virtual-clock TX with absolute deadlines (no cumulative drift).
fn virtual_tx(c: &mut CallState, now: Instant) {
    let step = c.ptime();
    while now >= c.next_tx {
        c.tx_tick(now, None);
        c.next_tx += step;
        if now > c.next_tx + step * 5 {
            c.next_tx = now;
        }
    }
}

fn lock_echo(m: &super::Meters) -> std::sync::MutexGuard<'_, super::EchoStatus> {
    m.echo.lock().unwrap_or_else(|e| e.into_inner())
}

/// RMS level of `x` in dBFS (floor -120).
fn rms_dbfs(x: &[f32]) -> f32 {
    let ms = x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32;
    (10.0 * ms.max(1e-12).log10()).max(-120.0)
}

/// Publishes the progress of `e`; moves to playback once the recording is complete.
fn echo_progress(meters: &super::Meters, e: &mut EchoRun) {
    let rate = ENGINE_RATE as u64;
    let mut st = lock_echo(meters);
    st.total_ms = e.target as u64 * 2000 / rate;
    if !e.playing {
        if e.buf.len() >= e.target {
            e.buf.truncate(e.target);
            let peak = e.buf.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let peak_db = (20.0 * peak.max(1e-6).log10()).max(-120.0);
            e.playing = true;
            st.peak_dbfs = Some(peak_db);
            st.silent = peak_db < -60.0;
            st.phase = super::EchoPhase::Playing;
            st.elapsed_ms = e.target as u64 * 1000 / rate;
        } else {
            st.phase = super::EchoPhase::Recording;
            st.elapsed_ms = e.buf.len() as u64 * 1000 / rate;
        }
    } else if e.pos >= e.buf.len() {
        st.phase = super::EchoPhase::Done;
        st.elapsed_ms = st.total_ms;
    } else {
        st.elapsed_ms = (e.target + e.pos) as u64 * 1000 / rate;
    }
}

/// Spawn the thread (used by [`super::engine::MediaEngine::start`]).
pub(crate) fn spawn(
    audio: Option<AudioHandle>,
    events: UnboundedSender<MediaEvent>,
    cmds: Receiver<Cmd>,
    focus_out: Arc<ArcSwapOption<String>>,
    meters: Arc<super::Meters>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("siple-media".into())
        .spawn(move || {
            let mut t = MediaThread::new(audio, events, cmds);
            t.focus_out = focus_out;
            t.meters = meters;
            t.run()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    /// A speaker tone with a playback device (no consumer for the virtual
    /// clock) must not make the loop wake every MIN_SLEEP.
    #[test]
    fn device_consumed_tone_does_not_force_min_sleep() {
        let (ev_tx, _ev) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let mut t = MediaThread::new(None, ev_tx, cmd_rx);
        t.play_rs = Some(Resampler::new(ENGINE_RATE, ENGINE_RATE, RX_FRAME_48K).unwrap());
        t.tones.push(ActiveTone {
            id: ToneId(1),
            target: ToneTarget::Speaker,
            tgen: ToneGenerator::new(ToneSpec::busy(), TONE_AMPLITUDE),
            buf: Vec::new(),
        });
        let now = Instant::now();
        t.service_virtual_rx(now);
        assert!(!t.virt_needed);
        let wait = t.next_wait(now);
        assert!(wait >= Duration::from_millis(4), "spin: wait {wait:?}");

        // Without any playback device the same tone does need the virtual clock.
        t.play_rs = None;
        t.service_virtual_rx(now);
        assert!(t.virt_needed);
        assert!(t.next_wait(now) <= MAX_SLEEP);
    }

    /// A call starting, or the device going away, ends the echo test instead of mixing the
    /// recording into the call.
    #[test]
    fn echo_test_ends_when_a_call_starts_or_the_device_goes() {
        use crate::media::EchoPhase;
        let (ev_tx, _ev) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let mut t = MediaThread::new(None, ev_tx, cmd_rx);
        t.cap_rs = Some(Resampler::new(ENGINE_RATE, ENGINE_RATE, 480).unwrap());
        t.play_rs = Some(Resampler::new(ENGINE_RATE, ENGINE_RATE, RX_FRAME_48K).unwrap());
        t.echo_start(1);
        t.echo.as_mut().unwrap().playing = true;
        t.echo_abort("a call started");
        assert!(t.echo.is_none());
        assert!(!t.echo_frame_add(), "nothing is mixed in any more");
        let st = t.meters.echo.lock().unwrap().clone();
        assert_eq!(st.phase, EchoPhase::Failed);
        assert_eq!(st.error.as_deref(), Some("a call started"));
        // The device going away is caught by the step.
        t.echo_start(1);
        t.play_rs = None;
        t.step(Instant::now());
        assert!(t.echo.is_none());
        assert_eq!(
            t.meters.echo.lock().unwrap().error.as_deref(),
            Some("the audio device went away")
        );
    }

    /// The echo test records, then plays the recording back frame by frame, then is done.
    #[test]
    fn echo_test_records_then_plays_back() {
        use crate::media::EchoPhase;
        let (ev_tx, _ev) = unbounded_channel();
        let (_cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let mut t = MediaThread::new(None, ev_tx, cmd_rx);
        // Without a microphone / speaker it fails with a reason.
        t.echo_start(1);
        assert_eq!(t.meters.echo.lock().unwrap().phase, EchoPhase::Failed);
        t.cap_rs = Some(Resampler::new(ENGINE_RATE, ENGINE_RATE, 480).unwrap());
        t.play_rs = Some(Resampler::new(ENGINE_RATE, ENGINE_RATE, RX_FRAME_48K).unwrap());
        t.echo_start(1);
        assert_eq!(t.meters.echo.lock().unwrap().phase, EchoPhase::Recording);
        // Half a second in: still recording.
        let e = t.echo.as_mut().unwrap();
        e.buf
            .extend(std::iter::repeat_n(0.5, ENGINE_RATE as usize / 2));
        echo_progress(&t.meters, e);
        let st = t.meters.echo.lock().unwrap().clone();
        assert_eq!(
            (st.phase, st.elapsed_ms, st.total_ms),
            (EchoPhase::Recording, 500, 2000)
        );
        // Recording completes: peak known, playback starts.
        let e = t.echo.as_mut().unwrap();
        e.buf.extend(std::iter::repeat_n(0.5, ENGINE_RATE as usize));
        echo_progress(&t.meters, e);
        let st = t.meters.echo.lock().unwrap().clone();
        assert_eq!(st.phase, EchoPhase::Playing);
        assert!(
            !st.silent && (st.peak_dbfs.unwrap() + 6.0).abs() < 0.1,
            "{st:?}"
        );
        let mut frames = 0;
        while t.echo.is_some() {
            t.mix.fill(0.0);
            assert!(t.echo_frame_add());
            assert!((t.mix[0] - 0.5).abs() < 1e-6);
            frames += 1;
            assert!(frames <= 60);
        }
        assert_eq!(frames, 50);
        let st = t.meters.echo.lock().unwrap().clone();
        assert_eq!((st.phase, st.elapsed_ms), (EchoPhase::Done, 2000));
        assert!(!t.echo_frame_add());
        // Silence is flagged.
        t.echo_start(1);
        let e = t.echo.as_mut().unwrap();
        e.buf.resize(ENGINE_RATE as usize, 0.0);
        echo_progress(&t.meters, e);
        assert!(t.meters.echo.lock().unwrap().silent);
    }
}
