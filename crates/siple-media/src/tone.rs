//! Local tone generators (UI tones, inband DTMF) at 48 kHz mono `i16`.
//!
//! [`ToneGenerator`] is stateful and phase-continuous: successive `fill`
//! calls continue the waveform and the cadence exactly where the previous
//! call stopped, regardless of frame size. Tone edges get a short linear ramp
//! to avoid clicks.

/// Sample rate of all generated tones.
pub const TONE_SAMPLE_RATE: u32 = 48000;

/// Edge ramp length in samples (2 ms).
const RAMP: usize = (TONE_SAMPLE_RATE as usize) * 2 / 1000;

/// One cadence segment: tone on or silence, for `ms` milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub on: bool,
    pub ms: u32,
}

/// Frequencies and cadence of a tone.
#[derive(Debug, Clone, PartialEq)]
pub struct ToneSpec {
    /// Up to two simultaneous frequencies in Hz; `0.0` means unused.
    pub freqs: [f32; 2],
    /// Cadence segments, played in order.
    pub segments: Vec<Segment>,
    /// Loop the segments forever. When false, output is silence once done.
    pub repeat: bool,
}

impl ToneSpec {
    fn new(freqs: [f32; 2], cadence: &[(bool, u32)], repeat: bool) -> Self {
        Self {
            freqs,
            segments: cadence.iter().map(|&(on, ms)| Segment { on, ms }).collect(),
            repeat,
        }
    }

    /// Continuous single sine.
    pub fn sine(freq_hz: f32) -> Self {
        Self::new([freq_hz, 0.0], &[(true, 1000)], true)
    }

    /// Continuous dual tone.
    pub fn dual(f1: f32, f2: f32) -> Self {
        Self::new([f1, f2], &[(true, 1000)], true)
    }

    /// North American dial tone: 350 + 440 Hz continuous.
    pub fn dial_tone() -> Self {
        Self::dual(350.0, 440.0)
    }

    /// North American ringback: 440 + 480 Hz, 2 s on / 4 s off.
    pub fn ringback() -> Self {
        Self::new([440.0, 480.0], &[(true, 2000), (false, 4000)], true)
    }

    /// North American busy: 480 + 620 Hz, 0.5 s on / 0.5 s off.
    pub fn busy() -> Self {
        Self::new([480.0, 620.0], &[(true, 500), (false, 500)], true)
    }

    /// North American reorder (fast busy): 480 + 620 Hz, 0.25 s on / 0.25 s off.
    pub fn reorder() -> Self {
        Self::new([480.0, 620.0], &[(true, 250), (false, 250)], true)
    }

    /// Call-waiting beep: 440 Hz for 300 ms, once.
    pub fn call_waiting() -> Self {
        Self::new([440.0, 0.0], &[(true, 300)], false)
    }
}

/// Stateful, phase-continuous tone source.
#[derive(Debug, Clone)]
pub struct ToneGenerator {
    spec: ToneSpec,
    /// Peak amplitude of each component.
    amp: f32,
    phase: [f64; 2],
    step: [f64; 2],
    seg: usize,
    /// Samples already played within the current segment.
    pos: usize,
    /// True once the cadence has looped back to segment 0 at least once.
    wrapped: bool,
    done: bool,
}

impl ToneGenerator {
    /// `amplitude` is the peak amplitude of each component tone (`i16` units).
    pub fn new(spec: ToneSpec, amplitude: i16) -> Self {
        let step = spec
            .freqs
            .map(|f| std::f64::consts::TAU * f as f64 / TONE_SAMPLE_RATE as f64);
        let done = spec.segments.iter().all(|s| s.ms == 0);
        Self {
            spec,
            amp: amplitude as f32,
            phase: [0.0; 2],
            step,
            seg: 0,
            pos: 0,
            wrapped: false,
            done,
        }
    }

    /// Restart the cadence and phase.
    pub fn reset(&mut self) {
        self.phase = [0.0; 2];
        self.seg = 0;
        self.pos = 0;
        self.wrapped = false;
        self.done = self.spec.segments.iter().all(|s| s.ms == 0);
    }

    /// True once a non-repeating tone has played all of its segments.
    pub fn is_finished(&self) -> bool {
        self.done
    }

    fn seg_samples(&self, i: usize) -> usize {
        self.spec.segments[i].ms as usize * (TONE_SAMPLE_RATE as usize / 1000)
    }

    /// Fill `out` with the next samples (silence once finished).
    pub fn fill(&mut self, out: &mut [i16]) {
        for o in out.iter_mut() {
            if self.done {
                *o = 0;
                continue;
            }
            let mut len = self.seg_samples(self.seg);
            while self.pos >= len {
                self.pos = 0;
                self.seg += 1;
                if self.seg >= self.spec.segments.len() {
                    if !self.spec.repeat {
                        self.done = true;
                        break;
                    }
                    self.seg = 0;
                    self.wrapped = true;
                }
                len = self.seg_samples(self.seg);
            }
            if self.done {
                *o = 0;
                continue;
            }
            let seg = self.spec.segments[self.seg];
            if seg.on {
                let mut v = 0.0f64;
                for k in 0..2 {
                    if self.spec.freqs[k] > 0.0 {
                        v += self.phase[k].sin();
                    }
                }
                let ramp = RAMP.min(len / 2).max(1);
                // No edge ramp where the neighbouring segment is also "on"
                // (same frequencies), including across the cadence wrap of a
                // continuous tone; that would modulate the tone every cycle.
                let n = self.spec.segments.len();
                let prev_on = if self.seg > 0 {
                    self.spec.segments[self.seg - 1].on
                } else {
                    self.wrapped && self.spec.segments[n - 1].on
                };
                let next_on = if self.seg + 1 < n {
                    self.spec.segments[self.seg + 1].on
                } else {
                    self.spec.repeat && self.spec.segments[0].on
                };
                let fade_in = if prev_on {
                    1.0
                } else {
                    self.pos.min(ramp) as f32 / ramp as f32
                };
                let fade_out = if next_on {
                    1.0
                } else {
                    (len - 1 - self.pos).min(ramp) as f32 / ramp as f32
                };
                let g = fade_in.min(fade_out).min(1.0);
                *o = (v as f32 * self.amp * g).round().clamp(-32768.0, 32767.0) as i16;
            } else {
                *o = 0;
            }
            // Phase keeps running through silence, so cadence edges stay
            // phase-continuous with respect to a free-running oscillator.
            for k in 0..2 {
                self.phase[k] += self.step[k];
                if self.phase[k] > std::f64::consts::TAU {
                    self.phase[k] -= std::f64::consts::TAU;
                }
            }
            self.pos += 1;
            if !self.spec.repeat
                && self.seg + 1 >= self.spec.segments.len()
                && self.pos >= self.seg_samples(self.seg)
            {
                self.done = true;
            }
        }
    }

    /// Return the next `n` samples as a new vector.
    pub fn next_frame(&mut self, n: usize) -> Vec<i16> {
        let mut v = vec![0i16; n];
        self.fill(&mut v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goertzel::{detect, rms_dbfs};

    const MS: usize = 48;

    #[test]
    fn continuous_tone_has_no_envelope_dip_after_initial_ramp() {
        for spec in [ToneSpec::sine(1000.0), ToneSpec::dual(350.0, 440.0)] {
            let peak_amp = 10000.0;
            let mut g = ToneGenerator::new(spec.clone(), 10000);
            let s = g.next_frame(48000 * 3);
            // Peak over each 1 ms window (>= one period of every component).
            let win = 48;
            let peaks: Vec<f32> = s[RAMP..]
                .chunks(win)
                .filter(|c| c.len() == win)
                .map(|c| c.iter().map(|v| (*v as f32).abs()).fold(0.0, f32::max))
                .collect();
            let overall = peaks.iter().cloned().fold(0.0, f32::max);
            let min = peaks.iter().cloned().fold(f32::MAX, f32::min);
            // 1 kHz: window holds a full period, so window peak ~ amplitude.
            // Dual tones beat, so compare against a lower floor there.
            let floor = if spec.freqs[1] == 0.0 { 0.95 } else { 0.0 };
            assert!(min >= floor * overall, "dip: {min} vs {overall} ({spec:?})");
            if spec.freqs[1] == 0.0 {
                assert!(overall <= peak_amp);
            }
        }
        // Dual tone: compare second-boundary neighbourhood to its neighbours.
        let mut g = ToneGenerator::new(ToneSpec::dual(350.0, 440.0), 10000);
        let s = g.next_frame(48000 * 2);
        let rms = |x: &[i16]| {
            (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
        };
        let around = rms(&s[48000 - 480..48000 + 480]);
        let before = rms(&s[48000 - 4800..48000 - 3840]);
        assert!(around > 0.8 * before, "{around} vs {before}");
    }

    #[test]
    fn cadence_tone_still_ramps_at_edges() {
        let mut g = ToneGenerator::new(ToneSpec::busy(), 10000);
        let s = g.next_frame(48000);
        assert_eq!(s[0], 0);
        assert!(s[10].abs() < 2000);
        // Off segment is silent.
        assert!(s[30000..40000].iter().all(|v| *v == 0));
    }

    #[test]
    fn sine_has_expected_frequency_and_level() {
        let mut g = ToneGenerator::new(ToneSpec::sine(1000.0), 10000);
        let s = g.next_frame(48000 / 10);
        let (ratio, level) = detect(&s, TONE_SAMPLE_RATE, 1000.0);
        assert!(ratio > 0.99, "ratio {ratio}");
        // 10000 peak -> rms 7071 -> about -13.3 dBFS
        assert!(
            (level - rms_dbfs(&s)).abs() < 0.5,
            "{level} vs {}",
            rms_dbfs(&s)
        );
        let (off, _) = detect(&s, TONE_SAMPLE_RATE, 1500.0);
        assert!(off < 0.01);
    }

    #[test]
    fn phase_continuous_across_frame_sizes() {
        let mut a = ToneGenerator::new(ToneSpec::dual(440.0, 480.0), 8000);
        let mut b = a.clone();
        let whole = a.next_frame(5000);
        let mut parts = Vec::new();
        for n in [1, 7, 960, 333, 3699] {
            parts.extend(b.next_frame(n));
        }
        assert_eq!(whole, parts);
    }

    fn envelope_ms(spec: ToneSpec, total_ms: usize) -> Vec<bool> {
        let mut g = ToneGenerator::new(spec, 8000);
        let s = g.next_frame(total_ms * MS);
        // per 10 ms window: any sample above a threshold. Returned vector is
        // indexed in ms (each window value repeated 10x) for readable asserts.
        s.chunks(10 * MS)
            .flat_map(|c| std::iter::repeat_n(c.iter().any(|&x| x.abs() > 1000), 10))
            .collect()
    }

    #[test]
    fn ringback_cadence_2s_on_4s_off() {
        let e = envelope_ms(ToneSpec::ringback(), 12000);
        assert!(e[20..1980].iter().all(|&b| b), "on for first 2 s");
        assert!(e[2020..5980].iter().all(|&b| !b), "off for 4 s");
        assert!(e[6020..7980].iter().all(|&b| b), "on again at 6 s");
        assert!(e[8020..11980].iter().all(|&b| !b));
    }

    #[test]
    fn busy_and_reorder_cadence() {
        let e = envelope_ms(ToneSpec::busy(), 2000);
        assert!(e[20..480].iter().all(|&b| b));
        assert!(e[520..980].iter().all(|&b| !b));
        assert!(e[1020..1480].iter().all(|&b| b));
        let e = envelope_ms(ToneSpec::reorder(), 1000);
        assert!(e[20..230].iter().all(|&b| b));
        assert!(e[270..480].iter().all(|&b| !b));
        assert!(e[520..730].iter().all(|&b| b));
    }

    #[test]
    fn call_waiting_plays_once_for_300ms() {
        let mut g = ToneGenerator::new(ToneSpec::call_waiting(), 8000);
        assert!(!g.is_finished());
        let s = g.next_frame(600 * MS);
        assert!(g.is_finished());
        assert!(s[10 * MS..290 * MS].iter().any(|&x| x.abs() > 1000));
        assert!(s[310 * MS..].iter().all(|&x| x == 0));
        let (r, _) = detect(&s[10 * MS..290 * MS], TONE_SAMPLE_RATE, 440.0);
        assert!(r > 0.95);
        g.reset();
        assert!(!g.is_finished());
    }

    #[test]
    fn ringback_has_both_components() {
        let mut g = ToneGenerator::new(ToneSpec::ringback(), 8000);
        let s = g.next_frame(1000 * MS);
        let (r440, _) = detect(&s[100 * MS..], TONE_SAMPLE_RATE, 440.0);
        let (r480, _) = detect(&s[100 * MS..], TONE_SAMPLE_RATE, 480.0);
        assert!(r440 > 0.3 && r480 > 0.3, "{r440} {r480}");
    }

    #[test]
    fn edges_are_ramped() {
        let mut g = ToneGenerator::new(ToneSpec::sine(1000.0), 20000);
        let s = g.next_frame(480);
        assert_eq!(s[0], 0);
        assert!(s[1].abs() < 2000, "first samples ramp in");
    }
}
