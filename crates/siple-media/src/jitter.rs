//! Pull-based adaptive jitter buffer.
//!
//! ## Model
//!
//! * `push` stores *encoded* packets indexed by extended sequence number and
//!   feeds the delay estimator. Nothing is decoded on the push path.
//! * `pull` is driven by the playback clock (every 20 ms). It decodes packets
//!   into an internal PCM queue (so any sender ptime of 10-60 ms works) and
//!   returns exactly one 20 ms PCM frame at the codec's PCM rate.
//! * Packet-to-packet RTP timestamps define the timeline: a missing packet
//!   with a later neighbor is concealed (Opus FEC from the next packet when
//!   possible, otherwise PLC) for exactly the timestamp gap; an *empty*
//!   buffer (underrun) conceals one frame and the inserted audio permanently
//!   adds that much playout delay (the "grow quickly" path).
//!
//! ## Adaptive delay
//!
//! Per packet we compute the relative transit time (arrival minus RTP time)
//! and keep a 3 s window. `delay = transit - window_min` approximates queueing
//! delay; the target is the 97th percentile of that plus 10 ms, clamped to
//! `[min_ms, max_ms]`. Increases apply immediately; decreases are smoothed
//! (1 % per packet). A late packet or an underrun that ends in a recovery
//! raises a floor by 20 ms that decays 2 ms/s. The RFC 3550 jitter estimate is
//! reported in the stats.
//!
//! Shrinking when the buffered depth exceeds `target + 30 ms`: a whole
//! 20 ms frame is dropped if it is quiet (< -47 dBFS); when it exceeds
//! `target + 60 ms` (or `max_ms + 20 ms`) 5 ms chunks are dropped with a 2 ms
//! crossfade even if the signal is active. This bounds the buffer under
//! sender/receiver clock drift.
//!
//! ## Resync and probation
//!
//! The stream restarts (buffer flushed, re-buffering to the target) on SSRC
//! change, sequence jump > 100, timestamp jump (> 1 s plus the wall time since
//! the last accepted packet), and on a marker packet while we were starved or
//! in comfort noise. SSRC change and jumps also set a flag, see
//! [`JitterBuffer::take_decoder_reset`]: **call it after `push` and
//! recreate/reset the decoder before the next `pull`** when it returns true.
//!
//! A jump or SSRC change never resyncs on a single packet (RFC 3550 A.1
//! style probation). The offending packet becomes a *candidate*; it is
//! confirmed after [`PROBATION_PKTS`] in-sequence packets from the candidate.
//! Until then the candidate packets are held (up to 16, oldest evicted) and,
//! on confirmation, **replayed into the new stream**, so a genuine source
//! switch loses no audio. A lone stale duplicate never confirms. A candidate
//! with a *different SSRC* is additionally held back while the current SSRC
//! delivered a packet within the last 200 ms, so two interleaved sources do
//! not thrash (the current one keeps playing). `push` returns
//! [`PushOutcome::Probation`] for held packets.
//!
//! ## Non-audio packets
//!
//! telephone-event and CN packets share the RTP sequence space but are not
//! pushed. **The engine must call [`JitterBuffer::note_non_audio`] for each
//! such packet** so the sequence numbers they consume are not counted as loss
//! and no concealment is fabricated. As a second guard, a sequence gap whose
//! timestamps show no gap inserts no concealment.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::codec::AudioDecoder;
use crate::goertzel::rms_dbfs;

const MAX_LOSS_MS: i64 = 200;
const MAX_PACKETS: usize = 512;
const WINDOW: Duration = Duration::from_secs(3);
const SOFT_SHRINK_MS: f32 = 30.0;
const HARD_SHRINK_MS: f32 = 60.0;
const QUIET_DBFS: f32 = -47.0;
const DROP_QUANTUM_MS: usize = 5;
const CROSSFADE_MS: usize = 2;
/// In-sequence packets a candidate stream needs before we switch to it.
pub const PROBATION_PKTS: u32 = 2;
/// Max packets held for a candidate (replayed on confirmation).
const CAND_BUF: usize = 16;
/// A different SSRC may only displace the current one after this much silence.
const SSRC_SILENCE: Duration = Duration::from_millis(200);
/// A candidate not continued within this time is forgotten.
const CAND_EXPIRY: Duration = Duration::from_secs(1);
/// Cap on delay-estimator window entries.
const MAX_TRANSITS: usize = 512;

/// Counters and gauges.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct JitterStats {
    /// Packets passed to `push` (including duplicates and late).
    pub received: u64,
    /// Sequence numbers skipped as lost (reduced again if one arrives late).
    pub lost: u64,
    /// Packets that arrived after being declared lost.
    pub late: u64,
    pub duplicate: u64,
    /// Concealment operations (PLC), including underruns.
    pub concealed: u64,
    /// Lost frames recovered from in-band FEC.
    pub fec_recovered: u64,
    pub resyncs: u64,
    /// Packets discarded during probation (never played).
    pub probation_dropped: u64,
    pub decode_errors: u64,
    /// Total audio removed by delay shrinking, ms.
    pub compressed_ms: u64,
    /// Buffered audio (PCM queue + queued packets) at the last pull, ms.
    pub current_delay_ms: u32,
    pub target_delay_ms: u32,
    /// RFC 3550 interarrival jitter, ms.
    pub jitter_ms: f32,
}

/// What `push` did with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Queued,
    /// Queued after the stream was restarted.
    Resync,
    Duplicate,
    /// Arrived after its slot was already skipped.
    Late,
    /// SSRC change or seq/ts jump not yet confirmed: the packet is held as a
    /// candidate (and replayed if the new stream is confirmed).
    Probation,
}

/// Result of one `pull`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullInfo {
    /// Remote audio is flowing (false before the first packet, while
    /// re-buffering, and after > 200 ms of continuous loss).
    pub rx_active: bool,
    /// The frame contains concealment.
    pub concealed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Buffering,
    Playing,
}

struct Pkt {
    ts: i64,
    payload: Vec<u8>,
}

struct CandPkt {
    seq: u16,
    ts: u32,
    marker: bool,
    payload: Vec<u8>,
    arrival: Instant,
}

/// A possible new stream awaiting confirmation (probation).
struct Candidate {
    ssrc: u32,
    last_seq: u16,
    last_ts: u32,
    last_arrival: Instant,
    count: u32,
    pkts: VecDeque<CandPkt>,
}

pub struct JitterBuffer {
    clock: i64,
    pcm_rate: i64,
    frame: usize,
    min_ms: f32,
    max_ms: f32,
    state: State,
    ssrc: Option<u32>,

    // sequence / timestamp extension (relative to the highest-seq packet)
    ref_seq: u16,
    ref_seq_ext: i64,
    ref_ts: u32,
    ref_ts_ext: i64,
    /// The reference packet was an audio packet (valid for ptime estimation).
    ref_audio: bool,
    last_arrival: Option<Instant>,
    cand: Option<Candidate>,
    /// Extended seqs of noted non-audio packets not yet passed.
    non_audio: BTreeSet<i64>,

    packets: BTreeMap<i64, Pkt>,
    next_seq: i64,
    last_end_ts: Option<i64>,
    est_ts: i64,
    skipped: BTreeSet<i64>,
    cn_active: bool,

    pcm: VecDeque<i16>,
    loss_run: usize,
    inserted: usize,
    underrun_run: bool,
    first_arrival: Instant,
    frame_concealed: bool,

    // delay estimation
    arr_base: Option<Instant>,
    ts_base: i64,
    transits: VecDeque<(Instant, i64)>,
    /// Same values as `transits`, kept sorted for O(log n) min/percentile.
    sorted: Vec<i64>,
    prev_transit: Option<i64>,
    jitter_us: f64,
    est_target: f32,
    floor: f32,
    last_floor_decay: Option<Instant>,

    reset_decoder: bool,
    stats: JitterStats,
    tmp: Vec<i16>,
}

fn signed_us(a: Instant, base: Instant) -> i64 {
    if a >= base {
        a.duration_since(base).as_micros() as i64
    } else {
        -(base.duration_since(a).as_micros() as i64)
    }
}

impl JitterBuffer {
    /// `codec_clock_rate`: RTP clock (8000 for G.711/G.722, 48000 for Opus).
    /// `pcm_rate`: decoder output rate (8000/16000/48000).
    pub fn new(codec_clock_rate: u32, pcm_rate: u32, min_ms: u32, max_ms: u32) -> Self {
        let min = min_ms.min(max_ms) as f32;
        let max = max_ms.max(min_ms) as f32;
        Self {
            clock: codec_clock_rate.max(1) as i64,
            pcm_rate: pcm_rate.max(50) as i64,
            frame: (pcm_rate / 50).max(1) as usize,
            min_ms: min,
            max_ms: max,
            state: State::Idle,
            ssrc: None,
            ref_seq: 0,
            ref_seq_ext: 0,
            ref_ts: 0,
            ref_ts_ext: 0,
            ref_audio: true,
            last_arrival: None,
            cand: None,
            non_audio: BTreeSet::new(),
            packets: BTreeMap::new(),
            next_seq: 0,
            last_end_ts: None,
            est_ts: codec_clock_rate as i64 / 50,
            skipped: BTreeSet::new(),
            cn_active: false,
            pcm: VecDeque::new(),
            loss_run: 0,
            inserted: 0,
            underrun_run: false,
            first_arrival: Instant::now(),
            frame_concealed: false,
            arr_base: None,
            ts_base: 0,
            transits: VecDeque::new(),
            sorted: Vec::new(),
            prev_transit: None,
            jitter_us: 0.0,
            est_target: min,
            floor: 0.0,
            last_floor_decay: None,
            reset_decoder: false,
            stats: JitterStats {
                target_delay_ms: min as u32,
                ..Default::default()
            },
            tmp: Vec::new(),
        }
    }

    /// Samples per output frame (20 ms at the PCM rate).
    pub fn frame_samples(&self) -> usize {
        self.frame
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// True once (and clears the flag) if the stream changed in a way that
    /// requires fresh decoder state (SSRC change, seq/ts jump).
    pub fn take_decoder_reset(&mut self) -> bool {
        std::mem::take(&mut self.reset_decoder)
    }

    /// Note a comfort-noise (CN) packet: underruns are rendered as silence and
    /// not counted as loss until the next audio packet.
    pub fn push_comfort_noise(&mut self) {
        self.cn_active = true;
        self.loss_run = 0;
    }

    fn target(&self) -> f32 {
        // Never below one packet of audio (a 60 ms sender needs > 40 ms).
        let ptime_ms = self.est_ts as f32 * 1000.0 / self.clock as f32;
        let lo = self.min_ms.max(ptime_ms.min(self.max_ms));
        self.est_target.max(self.floor).clamp(lo, self.max_ms)
    }

    fn ts_to_samples(&self, ts: i64) -> i64 {
        ts * self.pcm_rate / self.clock
    }

    fn samples_to_ts(&self, n: usize) -> i64 {
        n as i64 * self.clock / self.pcm_rate
    }

    fn ms_to_samples(&self, ms: i64) -> usize {
        (ms * self.pcm_rate / 1000).max(0) as usize
    }

    fn bump_floor(&mut self) {
        self.floor = self.floor.max((self.est_target + 20.0).min(self.max_ms));
    }

    fn start_stream(&mut self, seq: u16, ts: u32, ssrc: u32, arrival: Instant) {
        self.ssrc = Some(ssrc);
        self.ref_seq = seq;
        self.ref_seq_ext = 1 << 20;
        self.ref_ts = ts;
        self.ref_ts_ext = 1 << 40;
        self.packets.clear();
        self.pcm.clear();
        self.next_seq = self.ref_seq_ext;
        self.last_end_ts = None;
        self.skipped.clear();
        self.loss_run = 0;
        self.inserted = 0;
        self.underrun_run = false;
        self.cn_active = false;
        self.state = State::Buffering;
        self.first_arrival = arrival;
        self.arr_base = Some(arrival);
        self.ts_base = self.ref_ts_ext;
        self.transits.clear();
        self.sorted.clear();
        self.prev_transit = None;
        self.ref_audio = true;
        self.last_arrival = Some(arrival);
        self.non_audio.clear();
    }

    /// Note a non-audio packet (telephone-event, CN, ...) that consumed RTP
    /// sequence number `seq` (timestamp `ts`) on the current stream. It advances
    /// sequence tracking so the number is not counted as loss and no
    /// concealment is generated for it; while such packets keep arriving an
    /// empty buffer plays silence instead of PLC (like [`Self::push_comfort_noise`]).
    /// Ignored before the first audio packet
    /// or when far from the current sequence position.
    pub fn note_non_audio(&mut self, seq: u16, ts: u32) {
        if self.ssrc.is_none() {
            return;
        }
        let dseq = seq.wrapping_sub(self.ref_seq) as i16 as i64;
        let dts = ts.wrapping_sub(self.ref_ts) as i32 as i64;
        if dseq.abs() > 100 || dts.abs() > self.clock * 60 {
            return;
        }
        let seq_ext = self.ref_seq_ext + dseq;
        if seq_ext >= self.next_seq {
            self.non_audio.insert(seq_ext);
            while self.non_audio.len() > 256 {
                self.non_audio.pop_first();
            }
        }
        if seq_ext > self.ref_seq_ext {
            // The sender is not sending audio right now: underruns render as
            // silence (not PLC, not loss) until the next audio packet.
            self.cn_active = true;
            self.loss_run = 0;
            // Keep the seq reference moving so a long event does not look like
            // a sequence jump when audio resumes (ref_ts is left alone).
            self.ref_seq = seq;
            self.ref_seq_ext = seq_ext;
            self.ref_audio = false;
        }
    }

    /// Insert a received RTP audio packet.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        seq: u16,
        ts: u32,
        marker: bool,
        ssrc: u32,
        payload: Vec<u8>,
        arrival: Instant,
    ) -> PushOutcome {
        self.stats.received += 1;
        if self.ssrc.is_none() {
            self.start_stream(seq, ts, ssrc, arrival);
            return self.accept(seq, ts, marker, payload, arrival, false);
        }
        let dseq = seq.wrapping_sub(self.ref_seq) as i16 as i64;
        let dts = ts.wrapping_sub(self.ref_ts) as i32 as i64;
        // Timestamps may legitimately advance by the wall time since the last
        // audio packet (e.g. audio suppressed during a long DTMF event).
        let elapsed_ts = self.last_arrival.map_or(0, |t| {
            let e = arrival.saturating_duration_since(t);
            (e.as_millis().min(60_000) as i64) * self.clock / 1000
        });
        let jump = self.ssrc != Some(ssrc)
            || dseq.abs() > 100
            || dts > self.clock + elapsed_ts
            || dts < -self.clock;
        if !jump {
            self.last_arrival = Some(arrival);
            return self.accept(seq, ts, marker, payload, arrival, false);
        }
        self.probation(seq, ts, marker, ssrc, payload, arrival)
    }

    /// RFC 3550 A.1-style probation for an SSRC change or seq/ts jump.
    fn probation(
        &mut self,
        seq: u16,
        ts: u32,
        marker: bool,
        ssrc: u32,
        payload: Vec<u8>,
        arrival: Instant,
    ) -> PushOutcome {
        let pkt = CandPkt {
            seq,
            ts,
            marker,
            payload,
            arrival,
        };
        let clock = self.clock;
        let continues = self.cand.as_ref().is_some_and(|c| {
            let ds = seq.wrapping_sub(c.last_seq) as i16;
            let dt = ts.wrapping_sub(c.last_ts) as i32 as i64;
            c.ssrc == ssrc
                && (1..=3).contains(&ds)
                && dt > 0
                && dt <= clock
                && arrival.saturating_duration_since(c.last_arrival) <= CAND_EXPIRY
        });
        if continues {
            let c = self.cand.as_mut().expect("continues implies candidate");
            c.last_seq = seq;
            c.last_ts = ts;
            c.last_arrival = arrival;
            c.count += 1;
            c.pkts.push_back(pkt);
            if c.pkts.len() > CAND_BUF {
                c.pkts.pop_front();
                self.stats.probation_dropped += 1;
            }
        } else {
            if let Some(old) = self.cand.take() {
                self.stats.probation_dropped += old.pkts.len() as u64;
            }
            self.cand = Some(Candidate {
                ssrc,
                last_seq: seq,
                last_ts: ts,
                last_arrival: arrival,
                count: 1,
                pkts: VecDeque::from([pkt]),
            });
        }
        let confirmed = self
            .cand
            .as_ref()
            .is_some_and(|c| c.count >= PROBATION_PKTS);
        let current_alive = self.ssrc != Some(ssrc)
            && self
                .last_arrival
                .is_some_and(|t| arrival.saturating_duration_since(t) <= SSRC_SILENCE);
        if !confirmed || current_alive {
            return PushOutcome::Probation;
        }
        let Some(c) = self.cand.take() else {
            return PushOutcome::Probation;
        };
        let mut pkts = c.pkts.into_iter();
        let Some(first) = pkts.next() else {
            return PushOutcome::Probation;
        };
        self.start_stream(first.seq, first.ts, ssrc, first.arrival);
        self.stats.resyncs += 1;
        self.reset_decoder = true;
        self.accept(
            first.seq,
            first.ts,
            first.marker,
            first.payload,
            first.arrival,
            true,
        );
        for p in pkts {
            self.accept(p.seq, p.ts, p.marker, p.payload, p.arrival, false);
        }
        PushOutcome::Resync
    }

    /// Store an audio packet that belongs to the current stream.
    fn accept(
        &mut self,
        seq: u16,
        ts: u32,
        marker: bool,
        payload: Vec<u8>,
        arrival: Instant,
        mut resynced: bool,
    ) -> PushOutcome {
        if marker && self.state == State::Playing && (self.loss_run > 0 || self.cn_active) {
            // Talkspurt start after starvation/silence: restart timing.
            let ssrc = self.ssrc.unwrap_or(0);
            self.start_stream(seq, ts, ssrc, arrival);
            self.stats.resyncs += 1;
            resynced = true;
        }
        let dseq = seq.wrapping_sub(self.ref_seq) as i16 as i64;
        let dts = ts.wrapping_sub(self.ref_ts) as i32 as i64;
        let seq_ext = self.ref_seq_ext + dseq;
        let ts_ext = self.ref_ts_ext + dts;
        self.cn_active = false;

        if seq_ext < self.next_seq {
            if self.state == State::Buffering && self.next_seq - seq_ext <= 100 {
                self.next_seq = seq_ext; // reordered before the first packet
            } else if self.skipped.remove(&seq_ext) {
                self.stats.late += 1;
                self.stats.lost = self.stats.lost.saturating_sub(1);
                self.bump_floor();
                return PushOutcome::Late;
            } else {
                self.stats.duplicate += 1;
                return PushOutcome::Duplicate;
            }
        }
        if self.packets.contains_key(&seq_ext) {
            self.stats.duplicate += 1;
            return PushOutcome::Duplicate;
        }
        if seq_ext > self.ref_seq_ext {
            if self.ref_audio && seq_ext == self.ref_seq_ext + 1 && ts_ext > self.ref_ts_ext {
                let dt = ts_ext - self.ref_ts_ext;
                if dt >= self.clock / 400 && dt <= self.clock * 3 / 25 {
                    self.est_ts = dt;
                }
            }
            self.ref_seq = seq;
            self.ref_seq_ext = seq_ext;
            self.ref_ts = ts;
            self.ref_ts_ext = ts_ext;
            self.ref_audio = true;
        }
        self.non_audio.remove(&seq_ext);
        self.packets.insert(
            seq_ext,
            Pkt {
                ts: ts_ext,
                payload,
            },
        );
        while self.packets.len() > MAX_PACKETS {
            if let Some((s, _)) = self.packets.pop_first() {
                self.stats.lost += 1;
                self.next_seq = self.next_seq.max(s + 1);
                self.last_end_ts = None;
            }
        }
        self.update_estimator(ts_ext, arrival);
        if resynced {
            PushOutcome::Resync
        } else {
            PushOutcome::Queued
        }
    }

    fn update_estimator(&mut self, ts_ext: i64, arrival: Instant) {
        let Some(base) = self.arr_base else { return };
        let arr_us = signed_us(arrival, base);
        let ts_us = (ts_ext - self.ts_base) * 1_000_000 / self.clock;
        let transit = arr_us - ts_us;
        if let Some(prev) = self.prev_transit {
            let d = (transit - prev).abs() as f64;
            self.jitter_us += (d - self.jitter_us) / 16.0;
        }
        self.prev_transit = Some(transit);
        self.stats.jitter_ms = (self.jitter_us / 1000.0) as f32;

        self.transits.push_back((arrival, transit));
        let at = self.sorted.partition_point(|&x| x < transit);
        self.sorted.insert(at, transit);
        while let Some(&(t, v)) = self.transits.front() {
            if self.transits.len() > MAX_TRANSITS || arrival.saturating_duration_since(t) > WINDOW {
                self.transits.pop_front();
                if let Ok(i) = self.sorted.binary_search(&v) {
                    self.sorted.remove(i);
                }
            } else {
                break;
            }
        }
        if self.sorted.len() >= 10 {
            let min = self.sorted[0];
            let q = self.sorted[(self.sorted.len() - 1) * 97 / 100] - min;
            let p = q as f32 / 1000.0 + 10.0;
            let p = p.clamp(self.min_ms, self.max_ms);
            if p > self.est_target {
                self.est_target = p;
            } else {
                self.est_target += (p - self.est_target) * 0.01;
            }
        }
        match self.last_floor_decay {
            None => self.last_floor_decay = Some(arrival),
            Some(t) if arrival.saturating_duration_since(t) >= Duration::from_secs(1) => {
                self.floor = (self.floor - 2.0).max(0.0);
                self.last_floor_decay = Some(arrival);
            }
            _ => {}
        }
        self.stats.target_delay_ms = self.target() as u32;
    }

    // ── playout ─────────────────────────────────────────────────────────

    /// Produce exactly one 20 ms frame into `out` (cleared first).
    pub fn pull(
        &mut self,
        decoder: &mut dyn AudioDecoder,
        out: &mut Vec<i16>,
        now: Instant,
    ) -> PullInfo {
        out.clear();
        self.frame_concealed = false;
        let silent = PullInfo {
            rx_active: false,
            concealed: false,
        };
        match self.state {
            State::Idle => {
                out.resize(self.frame, 0);
                return silent;
            }
            State::Buffering => {
                let waited = now.saturating_duration_since(self.first_arrival);
                if waited.as_secs_f32() * 1000.0 >= self.target() {
                    self.state = State::Playing;
                } else {
                    out.resize(self.frame, 0);
                    return silent;
                }
            }
            State::Playing => {}
        }

        let frame = self.frame;
        self.fill(decoder, frame, true);
        self.maybe_shrink(decoder);
        self.stats.current_delay_ms = self.depth_ms() as u32;
        self.stats.target_delay_ms = self.target() as u32;

        let n = frame.min(self.pcm.len());
        out.extend(self.pcm.drain(..n));
        out.resize(frame, 0);
        PullInfo {
            rx_active: self.loss_run as i64 <= self.ms_to_samples(MAX_LOSS_MS) as i64,
            concealed: self.frame_concealed,
        }
    }

    fn depth_ms(&self) -> f32 {
        let mut samples = self.pcm.len() as i64;
        if let (Some((_, first)), Some((_, last))) = (
            self.packets.first_key_value(),
            self.packets.last_key_value(),
        ) {
            let start = self.last_end_ts.unwrap_or(first.ts);
            samples += self.ts_to_samples((last.ts + self.est_ts - start).max(0));
        }
        samples as f32 * 1000.0 / self.pcm_rate as f32
    }

    fn maybe_shrink(&mut self, decoder: &mut dyn AudioDecoder) {
        if self.loss_run > 0 {
            return;
        }
        let depth = self.depth_ms();
        let target = self.target();
        if depth <= target + SOFT_SHRINK_MS {
            return;
        }
        let frame = self.frame;
        self.fill(decoder, 2 * frame, false);
        if self.pcm.len() >= 2 * frame {
            let quiet = rms_dbfs(&self.pcm.make_contiguous()[..frame]) < QUIET_DBFS;
            if quiet {
                self.pcm.drain(..frame);
                self.stats.compressed_ms += 20;
                return;
            }
        }
        if depth > target + HARD_SHRINK_MS || depth > self.max_ms + 20.0 {
            let q = self.ms_to_samples(DROP_QUANTUM_MS as i64);
            let xf = self.ms_to_samples(CROSSFADE_MS as i64).min(q).max(1);
            self.fill(decoder, frame + q + xf, false);
            if self.pcm.len() >= frame + q + xf {
                let v = self.pcm.make_contiguous();
                for i in 0..xf {
                    let w = (i + 1) as f32 / (xf + 1) as f32;
                    v[q + i] = (v[i] as f32 * (1.0 - w) + v[q + i] as f32 * w) as i16;
                }
                self.pcm.drain(..q);
                self.stats.compressed_ms += DROP_QUANTUM_MS as u64;
            }
        }
    }

    fn fill(&mut self, dec: &mut dyn AudioDecoder, need: usize, allow_underrun: bool) {
        let mut guard = 0;
        while self.pcm.len() < need && guard < 256 {
            guard += 1;
            while !self.packets.contains_key(&self.next_seq)
                && self.non_audio.remove(&self.next_seq)
            {
                self.next_seq += 1;
            }
            if let Some(pkt) = self.packets.remove(&self.next_seq) {
                self.decode_packet(dec, pkt);
                continue;
            }
            let later = self
                .packets
                .range(self.next_seq + 1..)
                .next()
                .map(|(&s, p)| (s, p.ts));
            match later {
                Some((lseq, lts)) => self.handle_loss(dec, lseq, lts),
                None => {
                    if !allow_underrun {
                        break;
                    }
                    let n = need - self.pcm.len();
                    if self.cn_active {
                        self.push_silence(n);
                    } else {
                        self.conceal(dec, n);
                        self.stats.concealed += 1;
                        self.underrun_run = true;
                    }
                    self.inserted += n;
                }
            }
        }
    }

    fn push_silence(&mut self, n: usize) {
        self.pcm.extend(std::iter::repeat_n(0, n));
    }

    /// Append `n` concealed samples (silence once the loss budget is spent).
    fn conceal(&mut self, dec: &mut dyn AudioDecoder, n: usize) {
        self.frame_concealed = true;
        let budget = self
            .ms_to_samples(MAX_LOSS_MS)
            .saturating_sub(self.loss_run);
        let real = n.min(budget);
        let mut got = 0;
        if real > 0 && dec.conceal(real, &mut self.tmp).is_ok() {
            got = self.tmp.len().min(real);
            self.pcm.extend(self.tmp[..got].iter().copied());
        }
        self.push_silence(n - got);
        self.loss_run += n;
    }

    fn decode_packet(&mut self, dec: &mut dyn AudioDecoder, pkt: Pkt) {
        if let Some(le) = self.last_end_ts {
            let gap = self.ts_to_samples(pkt.ts - le);
            if gap > self.ms_to_samples(1) as i64 && gap <= self.ms_to_samples(MAX_LOSS_MS) as i64 {
                // Sender-side silence (DTX/CN) between contiguous packets.
                let need = (gap as usize).saturating_sub(self.inserted);
                self.push_silence(need);
            }
        }
        let mut ok = true;
        let n = match dec.decode(&pkt.payload, &mut self.tmp) {
            Ok(()) => {
                self.pcm.extend(self.tmp.iter().copied());
                self.tmp.len()
            }
            Err(_) => {
                ok = false;
                self.stats.decode_errors += 1;
                let est = self.ts_to_samples(self.est_ts).max(1) as usize;
                self.conceal(dec, est);
                est
            }
        };
        self.last_end_ts = Some(pkt.ts + self.samples_to_ts(n));
        self.next_seq += 1;
        if self.underrun_run {
            self.underrun_run = false;
            if self.loss_run as i64 <= self.ms_to_samples(MAX_LOSS_MS) as i64 {
                self.bump_floor();
            }
        }
        if ok {
            self.loss_run = 0;
        }
        self.inserted = 0;
    }

    fn handle_loss(&mut self, dec: &mut dyn AudioDecoder, lseq: i64, lts: i64) {
        let mut lost_n = 0usize;
        for s in self.next_seq..lseq {
            if !self.non_audio.remove(&s) {
                lost_n += 1;
            }
        }
        // No timestamp gap -> nothing is missing from the audio timeline
        // (the sequence numbers were consumed by non-audio packets).
        let no_ts_gap = matches!(self.last_end_ts, Some(le) if lts - le <= 0);
        let gap_ts = match self.last_end_ts {
            Some(le) if lts - le > 0 => lts - le,
            _ => self.est_ts * lost_n as i64,
        };
        let gap = self.ts_to_samples(gap_ts).max(1) as usize;
        for s in (self.next_seq..lseq).rev().take(64) {
            self.skipped.insert(s);
        }
        while self.skipped.len() > 256 {
            self.skipped.pop_first();
        }
        self.stats.lost += lost_n as u64;
        self.next_seq = lseq;
        if no_ts_gap || lost_n == 0 {
            return;
        }

        if gap > self.ms_to_samples(MAX_LOSS_MS) {
            // Long outage: do not fabricate audio, just continue from here.
            self.last_end_ts = None;
            self.inserted = 0;
            return;
        }
        self.last_end_ts = Some(lts);
        let need = gap.saturating_sub(self.inserted);
        let inserted = std::mem::take(&mut self.inserted);
        if need == 0 {
            return;
        }
        if lost_n == 1
            && inserted == 0
            && let Some(p) = self.packets.get(&lseq)
            && matches!(dec.decode_fec(&p.payload, gap, &mut self.tmp), Ok(true))
        {
            let got = self.tmp.len().min(gap);
            self.pcm.extend(self.tmp[..got].iter().copied());
            self.push_silence(gap - got);
            self.stats.fec_recovered += 1;
            return;
        }
        self.conceal(dec, need);
        self.stats.concealed += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{AudioCodec, EncoderConfig, OpusEncoderConfig, make_decoder, make_encoder};
    use crate::goertzel::detect;

    #[derive(Clone)]
    struct SimPkt {
        send_ms: f64,
        seq: u16,
        ts: u32,
        marker: bool,
        ssrc: u32,
        payload: Vec<u8>,
    }

    fn sine(rate: u32, start: usize, n: usize) -> Vec<i16> {
        (start..start + n)
            .map(|i| {
                ((i as f64 * 1000.0 * std::f64::consts::TAU / rate as f64).sin() * 8000.0) as i16
            })
            .collect()
    }

    /// Encode a continuous 1 kHz tone; `drift` > 0 means the sender clock runs fast.
    fn gen_stream(
        codec: AudioCodec,
        ptime: u32,
        n: usize,
        ssrc: u32,
        fec: bool,
        drift: f64,
    ) -> Vec<SimPkt> {
        let cfg = EncoderConfig {
            opus: OpusEncoderConfig {
                inband_fec: fec,
                packet_loss_pct: if fec { 20 } else { 0 },
                ..Default::default()
            },
        };
        let mut enc = make_encoder(codec, &cfg).unwrap();
        let rate = codec.pcm_sample_rate();
        let spf = codec.samples_per_frame(ptime);
        let mut out = Vec::new();
        let mut payload = Vec::new();
        for i in 0..n {
            let pcm = sine(rate, i * spf, spf);
            enc.encode(&pcm, &mut payload).unwrap();
            out.push(SimPkt {
                send_ms: i as f64 * ptime as f64 / (1.0 + drift),
                seq: i as u16,
                ts: (i as u32).wrapping_mul(codec.rtp_ts_per_frame(ptime)),
                marker: i == 0,
                ssrc,
                payload: payload.clone(),
            });
        }
        out
    }

    struct Run {
        frames: Vec<Vec<i16>>,
        infos: Vec<PullInfo>,
        delays: Vec<u32>,
        resets: u32,
    }

    /// Drive the buffer: deliver `arrivals` (arrival_ms, packet) and pull every 20 ms.
    fn simulate(
        codec: AudioCodec,
        jb: &mut JitterBuffer,
        mut arrivals: Vec<(f64, SimPkt)>,
        total_ms: u64,
    ) -> Run {
        arrivals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let base = Instant::now();
        let at = |ms: f64| base + Duration::from_micros((ms * 1000.0) as u64);
        let mut dec = make_decoder(codec).unwrap();
        let mut run = Run {
            frames: vec![],
            infos: vec![],
            delays: vec![],
            resets: 0,
        };
        let mut idx = 0;
        let mut out = Vec::new();
        let mut t = 0u64;
        while t < total_ms {
            while idx < arrivals.len() && arrivals[idx].0 <= t as f64 {
                let (ms, p) = &arrivals[idx];
                jb.push(p.seq, p.ts, p.marker, p.ssrc, p.payload.clone(), at(*ms));
                if jb.take_decoder_reset() {
                    run.resets += 1;
                    dec = make_decoder(codec).unwrap();
                }
                idx += 1;
            }
            let info = jb.pull(dec.as_mut(), &mut out, at(t as f64));
            assert_eq!(out.len(), jb.frame_samples(), "exact frame size");
            run.frames.push(out.clone());
            run.infos.push(info);
            run.delays.push(jb.stats().current_delay_ms);
            t += 20;
        }
        run
    }

    fn deliver(pkts: &[SimPkt], base_delay: f64) -> Vec<(f64, SimPkt)> {
        pkts.iter()
            .map(|p| (p.send_ms + base_delay, p.clone()))
            .collect()
    }

    fn tone_ratio(frames: &[Vec<i16>], from: usize, to: usize, rate: u32) -> f32 {
        let pcm: Vec<i16> = frames[from..to].iter().flatten().copied().collect();
        detect(&pcm, rate, 1000.0).0
    }

    fn jb_for(codec: AudioCodec) -> JitterBuffer {
        JitterBuffer::new(codec.rtp_clock_rate(), codec.pcm_sample_rate(), 40, 200)
    }

    const CODECS: [AudioCodec; 4] = [
        AudioCodec::Pcmu,
        AudioCodec::Pcma,
        AudioCodec::G722,
        AudioCodec::Opus,
    ];

    #[test]
    fn in_order_clean_all_codecs() {
        for codec in CODECS {
            let pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
            let mut jb = jb_for(codec);
            let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 5000);
            let s = jb.stats();
            assert_eq!(
                (s.lost, s.late, s.duplicate, s.concealed),
                (0, 0, 0, 0),
                "{codec:?} {s:?}"
            );
            assert!(
                run.infos[10..250]
                    .iter()
                    .all(|i| i.rx_active && !i.concealed)
            );
            let r = tone_ratio(&run.frames, 10, 250, codec.pcm_sample_rate());
            assert!(r > 0.9, "{codec:?} ratio {r}");
            assert!(
                s.current_delay_ms >= 20 && s.current_delay_ms <= 100,
                "{s:?}"
            );
        }
    }

    #[test]
    fn startup_is_silent_then_plays() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 50, 1, false, 0.0);
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, deliver(&pkts, 0.0), 1000);
        assert!(run.frames[0].iter().all(|&s| s == 0));
        assert!(!run.infos[0].rx_active);
        assert!(run.infos[10].rx_active);
    }

    #[test]
    fn reorder_adjacent_swaps() {
        for codec in [AudioCodec::Pcmu, AudioCodec::Opus] {
            let pkts = gen_stream(codec, 20, 400, 1, false, 0.0);
            let arr: Vec<_> = pkts
                .iter()
                .map(|p| {
                    // every 7th packet is delayed by 25 ms (arrives after its successor)
                    let d = if p.seq % 7 == 3 { 35.0 } else { 10.0 };
                    (p.send_ms + d, p.clone())
                })
                .collect();
            let mut jb = jb_for(codec);
            let run = simulate(codec, &mut jb, arr, 8200);
            let s = jb.stats();
            assert_eq!(s.lost, 0, "{codec:?} {s:?}");
            assert_eq!(s.late, 0, "{codec:?} {s:?}");
            let r = tone_ratio(&run.frames, 25, 380, codec.pcm_sample_rate());
            assert!(r > 0.9, "{codec:?} {r} {s:?}");
        }
    }

    #[test]
    fn loss_bursts_are_concealed() {
        for codec in CODECS {
            let pkts = gen_stream(codec, 20, 400, 1, false, 0.0);
            let arr: Vec<_> = deliver(&pkts, 10.0)
                .into_iter()
                .filter(|(_, p)| !(100..106).contains(&p.seq) && p.seq != 200 && p.seq != 300)
                .collect();
            let mut jb = jb_for(codec);
            let run = simulate(codec, &mut jb, arr, 8200);
            let s = jb.stats();
            assert_eq!(s.lost, 8, "{codec:?} {s:?}");
            assert!(s.concealed >= 3, "{codec:?} {s:?}");
            assert!(run.infos[20..380].iter().all(|i| i.rx_active));
            let r = tone_ratio(&run.frames, 20, 380, codec.pcm_sample_rate());
            assert!(r > 0.6, "{codec:?} ratio {r}");
        }
    }

    #[test]
    fn opus_fec_recovers_isolated_losses() {
        let codec = AudioCodec::Opus;
        let pkts = gen_stream(codec, 20, 400, 1, true, 0.0);
        let arr: Vec<_> = deliver(&pkts, 10.0)
            .into_iter()
            .filter(|(_, p)| p.seq % 25 != 12)
            .collect();
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 8200);
        let s = jb.stats();
        assert_eq!(s.lost, 16, "{s:?}");
        // decode_fec now reports false when the next packet has no LBRR
        // data (libopus emits it for only ~1/3 of pure-tone frames), so
        // only those losses count as FEC-recovered; the rest are concealed.
        assert!(s.fec_recovered >= 1, "{s:?}");
        assert!(s.fec_recovered + s.concealed >= 16, "{s:?}");
        let r = tone_ratio(&run.frames, 20, 380, 48000);
        assert!(r > 0.85, "{r}");
    }

    #[test]
    fn long_loss_goes_silent_and_recovers() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 400, 1, false, 0.0);
        let arr: Vec<_> = deliver(&pkts, 10.0)
            .into_iter()
            .filter(|(_, p)| !(100..130).contains(&p.seq)) // 600 ms hole
            .collect();
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 8200);
        // Frames well inside the hole: silence + inactive.
        let hole = &run.frames[116..126];
        assert!(hole.iter().flatten().all(|&s| s == 0));
        assert!(run.infos[116..126].iter().all(|i| !i.rx_active));
        // Recovered afterwards.
        assert!(run.infos[300..380].iter().all(|i| i.rx_active));
        let r = tone_ratio(&run.frames, 300, 380, 8000);
        assert!(r > 0.9, "{r}");
        assert!(jb.stats().lost >= 25);
    }

    #[test]
    fn jitter_spike_grows_target_and_recovers() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 750, 1, false, 0.0);
        let arr: Vec<_> = pkts
            .iter()
            .map(|p| {
                // 150 ms spike on packets 200..210 (arrive in a bunch)
                let d = if (200..210).contains(&p.seq) {
                    160.0 - (p.seq - 200) as f64 * 20.0
                } else {
                    10.0
                };
                (p.send_ms + d, p.clone())
            })
            .collect();
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 15200);
        let s = jb.stats();
        assert!(s.concealed >= 1, "spike must underrun at least once: {s:?}");
        assert!(s.target_delay_ms >= 40 && s.target_delay_ms <= 200, "{s:?}");
        // Output continues and is tonal again well after the spike.
        let r = tone_ratio(&run.frames, 600, 740, 8000);
        assert!(r > 0.9, "{r}");
        assert!(
            run.delays.iter().all(|&d| d <= 260),
            "{:?}",
            run.delays.iter().max()
        );
    }

    #[test]
    fn random_jitter_target_adapts() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 1500, 1, false, 0.0);
        let mut x: u32 = 0xC0FFEE;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x % 1000) as f64 / 1000.0
        };
        let arr: Vec<_> = pkts
            .iter()
            .map(|p| (p.send_ms + rnd() * 60.0, p.clone()))
            .collect();
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 30_200);
        let s = jb.stats();
        assert!(s.target_delay_ms >= 55 && s.target_delay_ms <= 120, "{s:?}");
        assert!(s.jitter_ms > 8.0 && s.jitter_ms < 35.0, "{s:?}");
        // After adaptation (second half) almost no concealment.
        let concealed_late = run.infos[750..].iter().filter(|i| i.concealed).count();
        assert!(concealed_late <= 15, "{concealed_late} {s:?}");
        assert!(run.infos[100..].iter().all(|i| i.rx_active));
    }

    #[test]
    fn mismatched_ptime() {
        for (codec, ptime) in [
            (AudioCodec::Pcmu, 10),
            (AudioCodec::Pcmu, 30),
            (AudioCodec::Pcmu, 60),
            (AudioCodec::G722, 40),
            (AudioCodec::Opus, 10),
            (AudioCodec::Opus, 40),
            (AudioCodec::Opus, 60),
        ] {
            let n = (6000 / ptime) as usize;
            let pkts = gen_stream(codec, ptime, n, 1, false, 0.0);
            let mut jb = jb_for(codec);
            let run = simulate(codec, &mut jb, deliver(&pkts, 5.0), 6000);
            let s = jb.stats();
            assert_eq!((s.lost, s.concealed), (0, 0), "{codec:?}/{ptime} {s:?}");
            let r = tone_ratio(&run.frames, 10, 270, codec.pcm_sample_rate());
            assert!(r > 0.9, "{codec:?}/{ptime} ratio {r}");
        }
    }

    #[test]
    fn mismatched_ptime_with_loss() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 60, 100, 1, false, 0.0);
        let arr: Vec<_> = deliver(&pkts, 5.0)
            .into_iter()
            .filter(|(_, p)| p.seq != 50)
            .collect();
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 6000);
        let s = jb.stats();
        assert_eq!(s.lost, 1);
        // One 60 ms slot: at most an underrun frame plus the gap remainder.
        assert!((1..=2).contains(&s.concealed), "{s:?}");
        assert!(tone_ratio(&run.frames, 10, 280, 8000) > 0.7);
    }

    #[test]
    fn timestamp_jump_resyncs() {
        let codec = AudioCodec::Pcmu;
        let mut pkts = gen_stream(codec, 20, 400, 1, false, 0.0);
        for p in pkts.iter_mut().filter(|p| p.seq >= 150) {
            p.ts = p.ts.wrapping_add(8000 * 30); // 30 s jump, seq continues
        }
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 8200);
        let s = jb.stats();
        assert_eq!(s.resyncs, 1, "{s:?}");
        assert_eq!(run.resets, 1);
        assert!(run.infos[350..].iter().all(|i| i.rx_active));
        assert!(tone_ratio(&run.frames, 330, 395, 8000) > 0.9);
        assert!(run.delays.iter().all(|&d| d <= 200));
    }

    #[test]
    fn seq_jump_resyncs() {
        let codec = AudioCodec::Pcmu;
        let mut pkts = gen_stream(codec, 20, 400, 1, false, 0.0);
        for p in pkts.iter_mut().filter(|p| p.seq >= 150) {
            p.seq = p.seq.wrapping_add(5000);
        }
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 8200);
        assert_eq!(jb.stats().resyncs, 1);
        assert!(tone_ratio(&run.frames, 330, 395, 8000) > 0.9);
    }

    #[test]
    fn ssrc_change_resyncs_and_requests_decoder_reset() {
        for codec in [AudioCodec::Pcmu, AudioCodec::Opus] {
            let mut a = gen_stream(codec, 20, 400, 1, false, 0.0);
            for p in a.iter_mut().filter(|p| p.seq >= 150) {
                p.ssrc = 2;
                p.seq = p.seq.wrapping_add(900); // new random seq base
                p.ts = p.ts.wrapping_add(123_456);
            }
            let mut jb = jb_for(codec);
            let run = simulate(codec, &mut jb, deliver(&a, 10.0), 8200);
            assert_eq!(jb.stats().resyncs, 1);
            assert_eq!(run.resets, 1);
            assert!(tone_ratio(&run.frames, 330, 395, codec.pcm_sample_rate()) > 0.9);
        }
    }

    #[test]
    fn seq_wraparound() {
        let codec = AudioCodec::Pcmu;
        let mut pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        for p in pkts.iter_mut() {
            p.seq = p.seq.wrapping_add(65_450);
            p.ts = p.ts.wrapping_add(u32::MAX - 20_000);
        }
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 6000);
        let s = jb.stats();
        assert_eq!((s.lost, s.resyncs, s.concealed), (0, 0, 0), "{s:?}");
        assert!(tone_ratio(&run.frames, 10, 280, 8000) > 0.9);
    }

    #[test]
    fn duplicates_and_late_are_counted() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        let mut arr = deliver(&pkts, 10.0);
        // duplicate every 10th
        let dups: Vec<_> = arr
            .iter()
            .filter(|(_, p)| p.seq % 10 == 0 && p.seq > 0)
            .map(|(t, p)| (*t + 1.0, p.clone()))
            .collect();
        let ndup = dups.len() as u64;
        arr.extend(dups);
        // packet 100 arrives 600 ms late (far too late)
        for (t, p) in arr.iter_mut() {
            if p.seq == 101 {
                *t += 600.0;
            }
        }
        let mut jb = jb_for(codec);
        simulate(codec, &mut jb, arr, 6200);
        let s = jb.stats();
        assert!(s.duplicate >= ndup - 1, "{s:?}");
        assert_eq!(s.late, 1, "{s:?}");
        assert_eq!(s.lost, 0, "late arrival offsets the loss: {s:?}");
    }

    #[test]
    fn dtx_gap_with_marker() {
        for codec in [AudioCodec::Pcmu, AudioCodec::Opus] {
            let mut pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
            // 2 s of silence after packet 99: seq continues, ts/time jump, marker on resume
            for p in pkts.iter_mut().filter(|p| p.seq >= 100) {
                p.send_ms += 2000.0;
                p.ts = p.ts.wrapping_add(codec.rtp_clock_rate() * 2);
                p.marker = p.seq == 100;
            }
            let mut jb = jb_for(codec);
            let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 9000);
            // inactive during the silence
            assert!(
                run.infos[2000 / 20 + 40..2000 / 20 + 90]
                    .iter()
                    .any(|i| !i.rx_active)
            );
            assert!(
                run.frames[2000 / 20 + 70..2000 / 20 + 90]
                    .iter()
                    .flatten()
                    .all(|&s| s == 0)
            );
            // active & tonal at the end
            assert!(run.infos[330..380].iter().all(|i| i.rx_active));
            let r = tone_ratio(&run.frames, 330, 380, codec.pcm_sample_rate());
            assert!(r > 0.9, "{codec:?} {r}");
            assert!(jb.stats().resyncs >= 1);
        }
    }

    #[test]
    fn comfort_noise_is_not_loss() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 100, 1, false, 0.0);
        let mut jb = jb_for(codec);
        let base = Instant::now();
        let mut dec = make_decoder(codec).unwrap();
        for p in &pkts {
            jb.push(p.seq, p.ts, false, 1, p.payload.clone(), base);
        }
        let mut out = Vec::new();
        jb.pull(dec.as_mut(), &mut out, base + Duration::from_millis(60));
        // Drain everything, then signal CN and keep pulling: no PLC, still active.
        for i in 0..110 {
            jb.pull(
                dec.as_mut(),
                &mut out,
                base + Duration::from_millis(80 + i * 20),
            );
        }
        let before = jb.stats().concealed;
        jb.push_comfort_noise();
        let mut last = None;
        for i in 0..40 {
            last = Some(jb.pull(
                dec.as_mut(),
                &mut out,
                base + Duration::from_millis(3000 + i * 20),
            ));
        }
        assert!(last.unwrap().rx_active);
        assert_eq!(jb.stats().concealed, before);
        assert!(out.iter().all(|&s| s == 0));
    }

    fn drift_run(drift: f64) -> (Run, JitterStats) {
        let codec = AudioCodec::Pcmu;
        let n = 3000;
        let pkts = gen_stream(codec, 20, n, 1, false, drift);
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 59_000);
        let s = jb.stats();
        (run, s)
    }

    #[test]
    fn clock_drift_sender_fast_stays_bounded() {
        let (run, s) = drift_run(0.005);
        let max = *run.delays.iter().max().unwrap();
        assert!(max <= 140, "max delay {max} {s:?}");
        assert!(s.compressed_ms > 100, "shrinking must engage: {s:?}");
        assert!(s.lost == 0 && s.resyncs == 0, "{s:?}");
        assert!(run.infos[100..].iter().all(|i| i.rx_active));
        assert!(tone_ratio(&run.frames, 2000, 2900, 8000) > 0.8);
    }

    #[test]
    fn clock_drift_sender_slow_stays_bounded() {
        let (run, s) = drift_run(-0.005);
        let max = *run.delays.iter().max().unwrap();
        assert!(max <= 200, "max delay {max} {s:?}");
        // Underruns insert delay; they must be sparse (about one per few seconds).
        assert!(s.concealed <= 40, "{s:?}");
        assert!(s.resyncs == 0, "{s:?}");
        assert!(run.infos[100..].iter().all(|i| i.rx_active));
        assert!(tone_ratio(&run.frames, 2000, 2900, 8000) > 0.8);
    }

    #[test]
    fn quiet_frames_are_dropped_to_shrink() {
        let codec = AudioCodec::Pcmu;
        // Silence stream arriving in one big burst: deep buffer, quiet content.
        let enc_silence = vec![0xFFu8; 160]; // PCMU digital silence
        let base = Instant::now();
        let mut jb = JitterBuffer::new(8000, 8000, 40, 200);
        let mut dec = make_decoder(codec).unwrap();
        for i in 0..40u16 {
            jb.push(i, i as u32 * 160, false, 9, enc_silence.clone(), base);
        }
        let mut out = Vec::new();
        for i in 0..30 {
            jb.pull(
                dec.as_mut(),
                &mut out,
                base + Duration::from_millis(100 + i * 20),
            );
        }
        let s = jb.stats();
        assert!(s.compressed_ms >= 200, "{s:?}");
        assert!(s.current_delay_ms <= 120, "{s:?}");
    }

    #[test]
    fn garbage_input_does_not_panic() {
        let codec = AudioCodec::Opus;
        let mut jb = jb_for(codec);
        let mut dec = make_decoder(codec).unwrap();
        let base = Instant::now();
        let mut out = Vec::new();
        let mut x: u32 = 99;
        for i in 0..400u64 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let len = (x % 40) as usize;
            let payload: Vec<u8> = (0..len).map(|k| (x >> (k % 24)) as u8).collect();
            jb.push(
                (x >> 8) as u16 % 300,
                x.wrapping_mul(7),
                x & 1 == 0,
                5 + (x % 3),
                payload,
                base + Duration::from_millis(i * 7),
            );
            if jb.take_decoder_reset() {
                dec = make_decoder(codec).unwrap();
            }
            jb.pull(dec.as_mut(), &mut out, base + Duration::from_millis(i * 20));
            assert_eq!(out.len(), 960);
        }
    }

    // ── probation / non-audio / estimator regressions ───────────────────

    fn silent_frames(f: &[Vec<i16>], from: usize, to: usize) -> usize {
        f[from..to]
            .iter()
            .filter(|x| x.iter().all(|&s| s == 0))
            .count()
    }

    #[test]
    fn single_stale_packet_does_not_resync() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        let mut arr = deliver(&pkts, 10.0);
        // A stale duplicate of packet 10 while the stream is at ~150.
        let mut stray = pkts[10].clone();
        stray.marker = false;
        arr.push((3010.5, stray));
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 6000);
        let s = jb.stats();
        assert_eq!((s.resyncs, run.resets), (0, 0), "{s:?}");
        assert_eq!(s.probation_dropped, 0, "{s:?}"); // still only held, never replayed
        assert_eq!(silent_frames(&run.frames, 150, 165), 0);
        assert!(tone_ratio(&run.frames, 20, 280, 8000) > 0.9);
    }

    #[test]
    fn single_ssrc_change_packet_does_not_resync() {
        let codec = AudioCodec::Pcmu;
        let pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        let mut arr = deliver(&pkts, 10.0);
        let mut other = pkts[5].clone();
        other.ssrc = 99;
        arr.push((2005.0, other));
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 6000);
        assert_eq!((jb.stats().resyncs, run.resets), (0, 0));
    }

    #[test]
    fn interleaved_ssrcs_do_not_thrash() {
        let codec = AudioCodec::Pcmu;
        let a = gen_stream(codec, 20, 200, 1, false, 0.0);
        let mut b = gen_stream(codec, 20, 200, 2, false, 0.0);
        for p in b.iter_mut() {
            p.seq = p.seq.wrapping_add(30000);
            p.ts = p.ts.wrapping_add(1_000_000);
        }
        let mut arr = deliver(&a, 10.0);
        arr.extend(b.iter().map(|p| (p.send_ms + 13.0, p.clone())));
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 4000);
        let s = jb.stats();
        assert_eq!((s.resyncs, run.resets), (0, 0), "{s:?}");
        assert_eq!(silent_frames(&run.frames, 20, 190), 0, "{s:?}");
        assert!(tone_ratio(&run.frames, 20, 190, 8000) > 0.9);
    }

    #[test]
    fn candidate_ssrc_wins_after_current_goes_silent() {
        let codec = AudioCodec::Pcmu;
        let a = gen_stream(codec, 20, 100, 1, false, 0.0); // stops at 2 s
        let mut b = gen_stream(codec, 20, 300, 2, false, 0.0);
        for p in b.iter_mut() {
            p.seq = p.seq.wrapping_add(7000);
            p.ts = p.ts.wrapping_add(555_555);
            p.send_ms += 1900.0; // overlaps A for ~100 ms, continues after
        }
        let mut arr = deliver(&a, 10.0);
        arr.extend(deliver(&b, 10.0));
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, arr, 9000);
        assert_eq!((jb.stats().resyncs, run.resets), (1, 1), "{:?}", jb.stats());
        assert!(tone_ratio(&run.frames, 330, 395, 8000) > 0.9);
    }

    #[test]
    fn probation_packets_are_replayed_after_confirmation() {
        // Source switch with no gap: the first candidate packets are not lost.
        let codec = AudioCodec::Pcmu;
        let mut pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        for p in pkts.iter_mut().filter(|p| p.seq >= 100) {
            p.seq = p.seq.wrapping_add(5000);
        }
        let mut jb = jb_for(codec);
        let run = simulate(codec, &mut jb, deliver(&pkts, 10.0), 6000);
        let s = jb.stats();
        assert_eq!(s.resyncs, 1, "{s:?}");
        assert_eq!(s.probation_dropped, 0, "{s:?}");
        assert_eq!(s.lost, 0, "{s:?}");
        assert!(tone_ratio(&run.frames, 230, 290, 8000) > 0.9);
    }

    #[test]
    fn non_audio_packets_are_not_loss() {
        let codec = AudioCodec::Pcmu;
        let mut pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        // 160 ms DTMF: audio 100..108 suppressed (ts keeps running), 3 end
        // packets use extra seqs; audio resumes at seq 111.
        pkts.retain(|p| !(100..108).contains(&p.seq));
        for p in pkts.iter_mut().filter(|p| p.seq >= 108) {
            p.seq = p.seq.wrapping_add(3);
        }
        // With note_non_audio the event's seqs are neither lost nor concealed
        // (the timestamp gap is rendered as sender-side silence).
        let mut jb = jb_for(codec);
        let base = Instant::now();
        let at = |ms: f64| base + Duration::from_micros((ms * 1000.0) as u64);
        let mut dec = make_decoder(codec).unwrap();
        let mut out = Vec::new();
        type Ev = (f64, Option<SimPkt>, Option<(u16, u32)>);
        let mut events: Vec<Ev> = Vec::new();
        for p in &pkts {
            events.push((p.send_ms + 10.0, Some(p.clone()), None));
        }
        for k in 0..11u16 {
            // te packets with seq 100..110, event timestamp = start of event
            let at_ms = if k < 8 {
                2010.0 + k as f64 * 20.0
            } else {
                2162.0 + (k - 8) as f64
            };
            events.push((at_ms, None, Some((100 + k, 100 * 160))));
        }
        events.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut i = 0;
        let mut t = 0u64;
        while t < 6000 {
            while i < events.len() && events[i].0 <= t as f64 {
                let (ms, p, na) = &events[i];
                if let Some(p) = p {
                    jb.push(p.seq, p.ts, p.marker, p.ssrc, p.payload.clone(), at(*ms));
                }
                if let Some((sq, ts)) = na {
                    jb.note_non_audio(*sq, *ts);
                }
                i += 1;
            }
            jb.pull(dec.as_mut(), &mut out, at(t as f64));
            t += 20;
        }
        let s = jb.stats();
        assert_eq!((s.lost, s.concealed, s.resyncs), (0, 0, 0), "{s:?}");
    }

    #[test]
    fn long_non_audio_run_does_not_look_like_a_jump() {
        // 3 s DTMF (150 seqs, audio suppressed) must not force a resync.
        let codec = AudioCodec::Pcmu;
        let mut jb = jb_for(codec);
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let mut dec = make_decoder(codec).unwrap();
        let pl = vec![0xFFu8; 160];
        for i in 0..50u16 {
            jb.push(i, i as u32 * 160, false, 1, pl.clone(), at(i as u64 * 20));
        }
        for k in 0..150u16 {
            jb.note_non_audio(50 + k, 50 * 160);
        }
        for i in 0..50u16 {
            let sq = 200 + i;
            jb.push(
                sq,
                (200 + i as u32) * 160,
                false,
                1,
                pl.clone(),
                at(1000 + 3000 + i as u64 * 20),
            );
        }
        let mut out = Vec::new();
        jb.pull(dec.as_mut(), &mut out, at(4100));
        let s = jb.stats();
        assert_eq!(s.resyncs, 0, "{s:?}");
        assert!(!jb.take_decoder_reset());
        assert_eq!(s.lost, 0, "{s:?}");
    }

    #[test]
    fn contiguous_timestamps_with_seq_gap_insert_no_concealment() {
        let codec = AudioCodec::Pcmu;
        let mut pkts = gen_stream(codec, 20, 300, 1, false, 0.0);
        let mut extra: u16 = 0;
        for p in pkts.iter_mut() {
            if (100..110).contains(&p.seq) {
                extra += 1;
            }
            p.seq = p.seq.wrapping_add(extra);
        }
        let mut jb = jb_for(codec);
        simulate(codec, &mut jb, deliver(&pkts, 10.0), 6000);
        assert_eq!(jb.stats().concealed, 0, "{:?}", jb.stats());
    }

    #[test]
    fn estimator_window_is_capped_and_sorted() {
        let mut jb = JitterBuffer::new(8000, 8000, 40, 200);
        let base = Instant::now();
        for i in 0..5_000usize {
            jb.push(
                i as u16,
                i as u32 * 160,
                false,
                7,
                vec![0xFF; 160],
                base + Duration::from_micros(i as u64 * 50),
            );
        }
        assert!(jb.transits.len() <= MAX_TRANSITS);
        assert_eq!(jb.transits.len(), jb.sorted.len());
        assert!(jb.sorted.windows(2).all(|w| w[0] <= w[1]));
    }
}
