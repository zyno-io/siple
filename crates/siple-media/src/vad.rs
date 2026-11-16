//! Energy voice-activity detection for the agent `call.listen` path.
//!
//! Pure and allocation-free per frame. [`EnergyVad`] turns one 20 ms frame of `i16` samples into
//! a level (dBFS) and a voiced/unvoiced decision with an adaptive noise floor and hysteresis;
//! [`Segmenter`] is a frame-index state machine over those decisions that finds speech onset,
//! end of speech, maximum duration and "no speech" timeouts. Tones count as speech.

/// Frame duration the defaults and the ms helpers assume.
pub const FRAME_MS: u32 = 20;
/// Lowest value reported for digital silence instead of `-inf`.
pub const SILENCE_DBFS: f32 = -120.0;
/// Noise floor clamp, lower bound.
pub const NOISE_MIN_DBFS: f32 = -90.0;
/// Noise floor clamp, upper bound.
pub const NOISE_MAX_DBFS: f32 = -35.0;
/// Default absolute threshold.
pub const DEFAULT_THRESHOLD_DBFS: f32 = -45.0;
/// How far above the noise floor speech must be.
pub const NOISE_MARGIN_DB: f32 = 10.0;
/// Threshold drop while already in speech.
pub const HYSTERESIS_DB: f32 = 3.0;
/// Noise floor rise per non-speech frame that is louder than the floor.
pub const NOISE_RISE_DB: f32 = 0.05;
/// Initial noise floor estimate.
pub const NOISE_INIT_DBFS: f32 = -60.0;

/// Number of whole frames covering `ms` (rounded up).
pub fn ms_to_frames(ms: u32) -> u32 {
    ms.div_ceil(FRAME_MS)
}

/// RMS level of `frame` in dBFS (full-scale `i16` = 0 dBFS). Empty or all-zero input gives
/// [`SILENCE_DBFS`], never `-inf` or NaN.
pub fn rms_dbfs(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return SILENCE_DBFS;
    }
    let mut sum = 0.0f64;
    for &s in frame {
        let v = f64::from(s);
        sum += v * v;
    }
    let rms = (sum / frame.len() as f64).sqrt() / 32768.0;
    if rms <= 0.0 {
        return SILENCE_DBFS;
    }
    ((20.0 * rms.log10()) as f32).max(SILENCE_DBFS)
}

/// Peak level of `frame` in dBFS ([`SILENCE_DBFS`] for silence).
pub fn peak_dbfs(frame: &[i16]) -> f32 {
    let peak = frame
        .iter()
        .map(|&s| i32::from(s).unsigned_abs())
        .max()
        .unwrap_or(0);
    if peak == 0 {
        return SILENCE_DBFS;
    }
    ((20.0 * (f64::from(peak) / 32768.0).log10()) as f32).max(SILENCE_DBFS)
}

/// Per-frame result of [`EnergyVad::process`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadFrame {
    /// RMS level of the frame in dBFS.
    pub level_dbfs: f32,
    /// Whether the frame is above the (hysteretic) threshold.
    pub voiced: bool,
}

/// Adaptive-noise-floor energy detector.
#[derive(Debug, Clone)]
pub struct EnergyVad {
    threshold_dbfs: f32,
    noise_dbfs: f32,
    in_speech: bool,
}

impl EnergyVad {
    /// `threshold_dbfs` is the absolute floor of the threshold (default −45).
    pub fn new(threshold_dbfs: Option<f32>) -> Self {
        let t = threshold_dbfs.filter(|t| t.is_finite());
        Self {
            threshold_dbfs: t.unwrap_or(DEFAULT_THRESHOLD_DBFS),
            noise_dbfs: NOISE_INIT_DBFS,
            in_speech: false,
        }
    }

    /// Current noise floor estimate in dBFS.
    pub fn noise_floor_dbfs(&self) -> f32 {
        self.noise_dbfs
    }

    /// Threshold that applies to the next frame.
    pub fn current_threshold_dbfs(&self) -> f32 {
        let t = self.threshold_dbfs.max(self.noise_dbfs + NOISE_MARGIN_DB);
        if self.in_speech { t - HYSTERESIS_DB } else { t }
    }

    /// Classifies one frame (nominally 20 ms) and updates the noise floor.
    pub fn process(&mut self, frame: &[i16]) -> VadFrame {
        let level = rms_dbfs(frame);
        let voiced = level > self.current_threshold_dbfs();
        if !voiced && !self.in_speech {
            // Noise floor adapts only outside speech: fast down, slow up.
            if level < self.noise_dbfs {
                self.noise_dbfs = 0.7 * self.noise_dbfs + 0.3 * level;
            } else {
                self.noise_dbfs += NOISE_RISE_DB;
            }
            self.noise_dbfs = self.noise_dbfs.clamp(NOISE_MIN_DBFS, NOISE_MAX_DBFS);
        }
        self.in_speech = voiced;
        VadFrame {
            level_dbfs: level,
            voiced,
        }
    }
}

/// Segmentation limits, in frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmenterConfig {
    /// Voiced frames (net of decrements) that confirm an onset (≥ 1).
    pub min_speech: u32,
    /// Consecutive unvoiced frames after speech that end it (≥ 1).
    pub end_silence: u32,
    /// Maximum speech length, counted from the speech start.
    pub max_speech: u32,
    /// Frames to wait for an onset before giving up.
    pub start_timeout: u32,
}

impl SegmenterConfig {
    /// Builds a config from milliseconds (rounded up to whole frames).
    pub fn from_ms(
        min_speech_ms: u32,
        end_silence_ms: u32,
        max_ms: u32,
        start_timeout_ms: u32,
    ) -> Self {
        Self {
            min_speech: ms_to_frames(min_speech_ms).max(1),
            end_silence: ms_to_frames(end_silence_ms).max(1),
            max_speech: ms_to_frames(max_ms).max(1),
            start_timeout: ms_to_frames(start_timeout_ms).max(1),
        }
    }
}

/// Why a segment finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentEnd {
    /// `end_silence` of quiet followed speech.
    EndOfSpeech,
    /// Speech reached `max_speech`.
    MaxDuration,
    /// No onset within `start_timeout`.
    NoSpeech,
}

/// State after a [`Segmenter::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegStatus {
    /// No speech confirmed yet.
    Waiting,
    /// Inside confirmed speech.
    Speech,
    /// Finished; further pushes keep returning this.
    Done(SegmentEnd),
}

/// Frame-index state machine. Frame indices count pushes, starting at 0.
#[derive(Debug, Clone)]
pub struct Segmenter {
    cfg: SegmenterConfig,
    frames: u32,
    status: SegStatus,
    /// Net voiced count while waiting.
    run: u32,
    /// First frame of the current candidate run.
    cand_start: u32,
    speech_start: Option<u32>,
    onset_at: Option<u32>,
    /// Index of the last voiced frame in confirmed speech.
    last_voiced: u32,
    silent: u32,
    speech_end: Option<u32>,
    /// Frame index from which `start_timeout` is counted (replayed history before it can still
    /// start speech but does not consume the timeout).
    timeout_from: u32,
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig) -> Self {
        Self {
            cfg: SegmenterConfig {
                min_speech: cfg.min_speech.max(1),
                end_silence: cfg.end_silence.max(1),
                max_speech: cfg.max_speech.max(1),
                start_timeout: cfg.start_timeout.max(1),
            },
            frames: 0,
            status: SegStatus::Waiting,
            run: 0,
            cand_start: 0,
            speech_start: None,
            onset_at: None,
            last_voiced: 0,
            silent: 0,
            speech_end: None,
            timeout_from: 0,
        }
    }

    /// Counts `start_timeout` only from frame `frames` on (default 0): earlier frames are still
    /// analysed for speech but never time the wait out.
    pub fn with_timeout_from(mut self, frames: u32) -> Self {
        self.timeout_from = frames;
        self
    }

    /// Frames pushed so far (the index the next frame gets).
    pub fn frames(&self) -> u32 {
        self.frames
    }

    pub fn status(&self) -> SegStatus {
        self.status
    }

    /// Index of the first voiced frame of the confirmed speech (use it, minus pre-roll frames,
    /// as the audio start).
    pub fn speech_start(&self) -> Option<u32> {
        self.speech_start
    }

    /// Index of the frame at which the onset was confirmed (≥ [`Self::speech_start`]).
    pub fn onset_detected_at(&self) -> Option<u32> {
        self.onset_at
    }

    /// Exclusive end frame of the speech: one past the last voiced frame (or the cut for
    /// `MaxDuration`). Set once the segment is `Done` after an onset.
    pub fn speech_end(&self) -> Option<u32> {
        self.speech_end
    }

    /// Feeds one frame's voiced decision.
    pub fn push(&mut self, voiced: bool) -> SegStatus {
        if matches!(self.status, SegStatus::Done(_)) {
            return self.status;
        }
        let i = self.frames;
        self.frames += 1;
        match self.status {
            SegStatus::Waiting => {
                if voiced {
                    if self.run == 0 {
                        self.cand_start = i;
                    }
                    self.run += 1;
                    if self.run >= self.cfg.min_speech {
                        self.status = SegStatus::Speech;
                        self.speech_start = Some(self.cand_start);
                        self.onset_at = Some(i);
                        self.last_voiced = i;
                        self.silent = 0;
                        return self.check_speech_limits(i);
                    }
                } else {
                    self.run = self.run.saturating_sub(1);
                }
                if self.frames >= self.timeout_from.saturating_add(self.cfg.start_timeout) {
                    self.status = SegStatus::Done(SegmentEnd::NoSpeech);
                }
            }
            SegStatus::Speech => {
                if voiced {
                    self.last_voiced = i;
                    self.silent = 0;
                } else {
                    self.silent += 1;
                    if self.silent >= self.cfg.end_silence {
                        self.speech_end = Some(self.last_voiced + 1);
                        self.status = SegStatus::Done(SegmentEnd::EndOfSpeech);
                        return self.status;
                    }
                }
                return self.check_speech_limits(i);
            }
            SegStatus::Done(_) => {}
        }
        self.status
    }

    fn check_speech_limits(&mut self, i: u32) -> SegStatus {
        let start = self.speech_start.unwrap_or(i);
        if i + 1 - start >= self.cfg.max_speech {
            self.speech_end = Some(i + 1);
            self.status = SegStatus::Done(SegmentEnd::MaxDuration);
        }
        self.status
    }

    /// Ends the segment early (e.g. the call ended): returns the speech end if speech was
    /// confirmed, treating trailing frames as the end.
    pub fn finish(&mut self) -> Option<u32> {
        if self.speech_start.is_some() && self.speech_end.is_none() {
            self.speech_end = Some(self.last_voiced + 1);
        }
        self.speech_end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 320; // 20 ms at 16 kHz

    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f64 / (1u64 << 31) as f64) * 2.0 - 1.0
        }
    }

    fn noise(g: &mut Gen, dbfs: f32) -> Vec<i16> {
        // uniform in [-a, a] has rms a/sqrt(3)
        let a = 32768.0 * 10f64.powf(f64::from(dbfs) / 20.0) * 3f64.sqrt();
        (0..FRAME).map(|_| (g.next() * a) as i16).collect()
    }

    fn tone(dbfs: f32) -> Vec<i16> {
        let a = 32768.0 * 10f64.powf(f64::from(dbfs) / 20.0) * 2f64.sqrt();
        (0..FRAME)
            .map(|n| (a * (2.0 * std::f64::consts::PI * 440.0 * n as f64 / 16000.0).sin()) as i16)
            .collect()
    }

    /// Runs a script of (frames, kind) through VAD + segmenter; returns the segmenter.
    fn run(script: &[(usize, Option<f32>)], cfg: SegmenterConfig) -> Segmenter {
        let mut g = Gen(1);
        let mut vad = EnergyVad::new(None);
        let mut seg = Segmenter::new(cfg);
        for &(n, speech) in script {
            for _ in 0..n {
                let f = match speech {
                    Some(db) => tone(db),
                    None => noise(&mut g, -60.0),
                };
                let r = vad.process(&f);
                seg.push(r.voiced);
            }
        }
        seg
    }

    fn cfg() -> SegmenterConfig {
        SegmenterConfig::from_ms(120, 800, 30_000, 8000)
    }

    #[test]
    fn levels_and_silence() {
        assert_eq!(rms_dbfs(&[]), SILENCE_DBFS);
        assert_eq!(rms_dbfs(&[0; 320]), SILENCE_DBFS);
        assert!(rms_dbfs(&[0; 320]).is_finite());
        assert!((rms_dbfs(&[i16::MIN; 10]) - 0.0).abs() < 0.01);
        assert!((peak_dbfs(&[16384, 0]) + 6.02).abs() < 0.05);
        assert_eq!(peak_dbfs(&[]), SILENCE_DBFS);
    }

    #[test]
    fn onset_and_end_within_one_frame() {
        // 100 noise, 50 speech (-20 dBFS), 100 noise.
        let seg = run(&[(100, None), (50, Some(-20.0)), (100, None)], cfg());
        assert_eq!(seg.status(), SegStatus::Done(SegmentEnd::EndOfSpeech));
        let start = seg.speech_start().unwrap() as i64;
        let end = seg.speech_end().unwrap() as i64;
        assert!((start - 100).abs() <= 1, "start {start}");
        assert!((end - 150).abs() <= 1, "end {end}");
        // confirmed after min_speech frames
        assert_eq!(seg.onset_detected_at().unwrap() as i64, start + 5);
    }

    #[test]
    fn noise_alone_never_triggers_and_times_out() {
        let seg = run(&[(500, None)], cfg());
        assert_eq!(seg.status(), SegStatus::Done(SegmentEnd::NoSpeech));
        assert_eq!(seg.frames(), 400);
        assert!(seg.speech_start().is_none() && seg.speech_end().is_none());
    }

    #[test]
    fn isolated_clicks_rejected() {
        let mut g = Gen(7);
        let mut vad = EnergyVad::new(None);
        let mut seg = Segmenter::new(cfg());
        for n in 0..300 {
            let f = if n % 10 == 3 {
                tone(-10.0)
            } else {
                noise(&mut g, -60.0)
            };
            seg.push(vad.process(&f).voiced);
        }
        assert_eq!(seg.status(), SegStatus::Waiting);
        // two-frame bursts every 10 frames also never accumulate
        let mut seg = Segmenter::new(cfg());
        for n in 0..300 {
            let f = if n % 10 < 2 {
                tone(-10.0)
            } else {
                noise(&mut g, -60.0)
            };
            seg.push(vad.process(&f).voiced);
        }
        assert_eq!(seg.status(), SegStatus::Waiting);
    }

    #[test]
    fn gaps_within_speech_decrement_not_reset() {
        let mut seg = Segmenter::new(SegmenterConfig {
            min_speech: 4,
            end_silence: 10,
            max_speech: 1000,
            start_timeout: 1000,
        });
        // v v u v v: run = 1,2,1,2,3 -> not yet; next v -> 4 -> onset
        for v in [true, true, false, true, true] {
            assert_eq!(seg.push(v), SegStatus::Waiting);
        }
        assert_eq!(seg.push(true), SegStatus::Speech);
        assert_eq!(seg.speech_start(), Some(0));
        assert_eq!(seg.onset_detected_at(), Some(5));
    }

    #[test]
    fn hangover_bridges_short_pauses() {
        // 600 ms pause inside speech < 800 ms end_silence: one segment.
        let seg = run(
            &[
                (50, None),
                (30, Some(-20.0)),
                (30, None),
                (30, Some(-20.0)),
                (60, None),
            ],
            cfg(),
        );
        assert_eq!(seg.status(), SegStatus::Done(SegmentEnd::EndOfSpeech));
        assert_eq!(seg.speech_start(), Some(50));
        assert_eq!(seg.speech_end(), Some(140));
        // 1000 ms pause ends the first segment.
        let seg = run(
            &[(50, None), (30, Some(-20.0)), (60, None), (30, Some(-20.0))],
            cfg(),
        );
        assert_eq!(seg.speech_end(), Some(80));
    }

    #[test]
    fn max_duration() {
        let c = SegmenterConfig::from_ms(120, 800, 1000, 8000);
        let seg = run(&[(20, None), (200, Some(-20.0))], c);
        assert_eq!(seg.status(), SegStatus::Done(SegmentEnd::MaxDuration));
        assert_eq!(seg.speech_start(), Some(20));
        assert_eq!(seg.speech_end(), Some(70));
    }

    #[test]
    fn silence_and_neg_inf_input() {
        let mut vad = EnergyVad::new(None);
        for _ in 0..100 {
            let r = vad.process(&[0i16; FRAME]);
            assert!(!r.voiced);
            assert!(r.level_dbfs.is_finite());
        }
        assert!(vad.noise_floor_dbfs() >= NOISE_MIN_DBFS - 1e-3);
        assert!(vad.noise_floor_dbfs().is_finite());
        assert!(!vad.process(&[]).voiced);
        // speech still detected afterwards
        assert!(vad.process(&tone(-20.0)).voiced);
    }

    #[test]
    fn noise_floor_adapts_and_is_frozen_in_speech() {
        let mut g = Gen(3);
        let mut vad = EnergyVad::new(None);
        for _ in 0..50 {
            vad.process(&noise(&mut g, -80.0));
        }
        let low = vad.noise_floor_dbfs();
        assert!(low < -75.0, "{low}");
        for _ in 0..50 {
            vad.process(&tone(-20.0));
        }
        assert_eq!(vad.noise_floor_dbfs(), low);
        // slow rise: +0.05 per frame under a loud-ish non-speech floor
        let mut vad = EnergyVad::new(Some(-20.0));
        let before = vad.noise_floor_dbfs();
        for _ in 0..20 {
            vad.process(&noise(&mut g, -50.0));
        }
        assert!((vad.noise_floor_dbfs() - before - 1.0).abs() < 0.01);
    }

    #[test]
    fn noise_clamped_and_threshold_follows_noise() {
        let mut g = Gen(5);
        let mut vad = EnergyVad::new(Some(-80.0));
        for _ in 0..5000 {
            vad.process(&noise(&mut g, -36.0));
        }
        assert!(vad.noise_floor_dbfs() <= NOISE_MAX_DBFS);
        assert!(
            vad.current_threshold_dbfs()
                >= vad.noise_floor_dbfs() + NOISE_MARGIN_DB - HYSTERESIS_DB - 0.01
        );
    }

    #[test]
    fn hysteresis_keeps_borderline_frames() {
        let mut vad = EnergyVad::new(Some(-40.0));
        assert!(vad.process(&tone(-30.0)).voiced);
        // -42 is below the -40 threshold but above -43 while in speech
        assert!(vad.process(&tone(-42.0)).voiced);
        assert!(!vad.process(&tone(-44.0)).voiced);
        assert!(!vad.process(&tone(-42.0)).voiced);
    }

    #[test]
    fn finish_and_done_is_sticky() {
        let mut seg = Segmenter::new(SegmenterConfig {
            min_speech: 2,
            end_silence: 5,
            max_speech: 100,
            start_timeout: 10,
        });
        for _ in 0..4 {
            seg.push(true);
        }
        seg.push(false);
        assert_eq!(seg.finish(), Some(4));
        for _ in 0..10 {
            seg.push(false);
        }
        let mut s2 = Segmenter::new(SegmenterConfig::from_ms(40, 100, 1000, 100));
        for _ in 0..5 {
            s2.push(false);
        }
        assert_eq!(s2.status(), SegStatus::Done(SegmentEnd::NoSpeech));
        assert_eq!(s2.push(true), SegStatus::Done(SegmentEnd::NoSpeech));
        assert_eq!(s2.frames(), 5);
    }
}
