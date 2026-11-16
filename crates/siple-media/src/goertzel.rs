//! Goertzel single-frequency tone detection and level metering.
//!
//! Levels are in dBFS where 0 dBFS is a sample magnitude of 32768 *RMS*
//! (so a full-scale sine reads about -3 dBFS). [`rms_dbfs`] and the tone
//! level from [`ToneDetector`] use the same reference, so they are directly
//! comparable.

const FLOOR_DBFS: f32 = -120.0;
const FULL_SCALE: f64 = 32768.0;

fn to_dbfs(rms: f64) -> f32 {
    if rms <= 0.0 {
        return FLOOR_DBFS;
    }
    ((20.0 * (rms / FULL_SCALE).log10()) as f32).max(FLOOR_DBFS)
}

/// RMS level of `samples` in dBFS (floor -120 for silence/empty).
pub fn rms_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return FLOOR_DBFS;
    }
    let sum: f64 = samples.iter().map(|&s| (s as f64).powi(2)).sum();
    to_dbfs((sum / samples.len() as f64).sqrt())
}

/// Result of one Goertzel block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockResult {
    /// Fraction of the block's total power that sits at the target frequency
    /// (about 1.0 for a pure tone, near 0 for noise).
    pub power_ratio: f32,
    /// RMS level of the target-frequency component, dBFS.
    pub level_dbfs: f32,
}

struct Acc {
    coeff: f64,
    s1: f64,
    s2: f64,
    energy: f64,
}

impl Acc {
    fn new(sample_rate: u32, freq_hz: f32) -> Self {
        let w = std::f64::consts::TAU * freq_hz as f64 / sample_rate as f64;
        Self {
            coeff: 2.0 * w.cos(),
            s1: 0.0,
            s2: 0.0,
            energy: 0.0,
        }
    }

    #[inline]
    fn push(&mut self, x: i16) {
        let x = x as f64;
        let s0 = x + self.coeff * self.s1 - self.s2;
        self.s2 = self.s1;
        self.s1 = s0;
        self.energy += x * x;
    }

    fn finish(&mut self, n: usize) -> BlockResult {
        let n = n as f64;
        let p = self.s1 * self.s1 + self.s2 * self.s2 - self.coeff * self.s1 * self.s2;
        // Tone amplitude A = 2|X|/N, mean square = A^2/2 = 2|X|^2/N^2.
        let tone_ms = (2.0 * p.max(0.0)) / (n * n);
        let total_ms = self.energy / n;
        let ratio = if total_ms > 0.0 {
            (tone_ms / total_ms).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let r = BlockResult {
            power_ratio: ratio as f32,
            level_dbfs: to_dbfs(tone_ms.sqrt()),
        };
        self.s1 = 0.0;
        self.s2 = 0.0;
        self.energy = 0.0;
        r
    }
}

/// Streaming single-frequency detector working on fixed-size blocks.
pub struct ToneDetector {
    acc: Acc,
    block_len: usize,
    n: usize,
    last: Option<BlockResult>,
}

impl ToneDetector {
    /// `block_ms` sets the analysis block (e.g. 20-100 ms); a longer block
    /// narrows the detection bandwidth to about `1000 / block_ms` Hz.
    pub fn new(sample_rate: u32, freq_hz: f32, block_ms: u32) -> Self {
        Self {
            acc: Acc::new(sample_rate, freq_hz),
            block_len: ((sample_rate as u64 * block_ms as u64 / 1000) as usize).max(1),
            n: 0,
            last: None,
        }
    }

    /// Feed samples. Returns the tone level (dBFS) of the **last block
    /// completed** during this call, or `None` if no block completed.
    /// Feed chunks no larger than one block to see every block.
    pub fn feed(&mut self, samples: &[i16]) -> Option<f32> {
        let mut out = None;
        for &x in samples {
            self.acc.push(x);
            self.n += 1;
            if self.n == self.block_len {
                let r = self.acc.finish(self.n);
                self.n = 0;
                self.last = Some(r);
                out = Some(r.level_dbfs);
            }
        }
        out
    }

    /// Full result (ratio and level) of the most recently completed block.
    pub fn last_block(&self) -> Option<BlockResult> {
        self.last
    }

    /// Discard partial-block state.
    pub fn reset(&mut self) {
        self.acc.s1 = 0.0;
        self.acc.s2 = 0.0;
        self.acc.energy = 0.0;
        self.n = 0;
        self.last = None;
    }
}

/// One-shot detection over the whole of `samples`.
/// Returns `(power_ratio, level_dbfs)` as in [`BlockResult`].
pub fn detect(samples: &[i16], sample_rate: u32, freq_hz: f32) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, FLOOR_DBFS);
    }
    let mut acc = Acc::new(sample_rate, freq_hz);
    for &x in samples {
        acc.push(x);
    }
    let r = acc.finish(samples.len());
    (r.power_ratio, r.level_dbfs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f64, amp: f64, n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| ((i as f64 * hz * std::f64::consts::TAU / rate as f64).sin() * amp) as i16)
            .collect()
    }

    /// Deterministic white noise (xorshift).
    fn noise(n: usize, amp: i32) -> Vec<i16> {
        let mut x: u32 = 0x1234_5678;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                ((x % (2 * amp as u32 + 1)) as i32 - amp) as i16
            })
            .collect()
    }

    #[test]
    fn detects_pure_tone() {
        let s = sine(8000, 1000.0, 10000.0, 800);
        let (ratio, level) = detect(&s, 8000, 1000.0);
        assert!(ratio > 0.99, "{ratio}");
        assert!((level - rms_dbfs(&s)).abs() < 0.2);
        // 10000 peak => rms 7071 => -13.3 dBFS
        assert!((level - -13.3).abs() < 0.3, "{level}");
    }

    #[test]
    fn rejects_other_frequencies_and_noise() {
        let s = sine(8000, 1000.0, 10000.0, 800);
        assert!(detect(&s, 8000, 1300.0).0 < 0.02);
        let n = noise(8000, 8000);
        let (ratio, _) = detect(&n, 8000, 1000.0);
        assert!(ratio < 0.05, "noise ratio {ratio}");
    }

    #[test]
    fn detects_tone_in_noise() {
        let mut s = sine(8000, 425.0, 6000.0, 4000);
        for (a, b) in s.iter_mut().zip(noise(4000, 1500)) {
            *a = a.saturating_add(b);
        }
        let (ratio, level) = detect(&s, 8000, 425.0);
        assert!(ratio > 0.8, "{ratio}");
        assert!((level - -17.7).abs() < 1.0, "{level}"); // 6000/sqrt2 = 4243 rms
    }

    #[test]
    fn silence_and_empty() {
        assert_eq!(detect(&[0; 100], 8000, 1000.0), (0.0, -120.0));
        assert_eq!(detect(&[], 8000, 1000.0), (0.0, -120.0));
        assert_eq!(rms_dbfs(&[]), -120.0);
        assert_eq!(rms_dbfs(&[0; 10]), -120.0);
    }

    #[test]
    fn rms_levels() {
        let full = sine(48000, 1000.0, 32767.0, 4800);
        assert!((rms_dbfs(&full) - -3.01).abs() < 0.05);
        let sq = vec![32767i16; 100];
        assert!(rms_dbfs(&sq) > -0.01);
        let half = sine(48000, 1000.0, 16384.0, 4800);
        assert!((rms_dbfs(&half) - -9.03).abs() < 0.05);
    }

    #[test]
    fn streaming_blocks() {
        let mut d = ToneDetector::new(8000, 1000.0, 20); // 160-sample blocks
        let tone = sine(8000, 1000.0, 10000.0, 800);
        // chunk smaller than a block: only emits on completion
        assert_eq!(d.feed(&tone[..100]), None);
        let lvl = d.feed(&tone[100..160]).expect("block complete");
        assert!((lvl - -13.3).abs() < 0.5, "{lvl}");
        assert!(d.last_block().unwrap().power_ratio > 0.95);
        // silence block
        let lvl = d.feed(&[0; 160]).unwrap();
        assert!(lvl < -100.0);
        assert!(d.last_block().unwrap().power_ratio < 0.01);
        // off-frequency
        let other = sine(8000, 2000.0, 10000.0, 160);
        let lvl = d.feed(&other).unwrap();
        assert!(lvl < -35.0, "{lvl}");
    }
}
