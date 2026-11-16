//! Streaming mono `f32` rate conversion on top of rubato's synchronous FFT
//! resampler (fixed input chunk, exact rational ratio, low delay).

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler as _};

/// Resampler construction failure.
#[derive(Debug, thiserror::Error)]
#[error("cannot create {from} Hz -> {to} Hz resampler (chunk {chunk}): {reason}")]
pub struct ResampleInitError {
    from: u32,
    to: u32,
    chunk: usize,
    reason: String,
}

/// Streaming mono resampler. Accepts arbitrary input lengths; internally
/// buffers a remainder and processes whole `chunk_frames` input chunks.
pub struct Resampler {
    from: u32,
    to: u32,
    inner: Option<Fft<f32>>,
    chunk: usize,
    pending: Vec<f32>,
    scratch: Vec<f32>,
}

impl Resampler {
    /// `chunk_frames` is the input chunk size in frames at `from` Hz (e.g.
    /// `from / 100` for 10 ms). Output length per chunk follows from the
    /// ratio. If `from == to` the resampler is a passthrough.
    pub fn new(from: u32, to: u32, chunk_frames: usize) -> Result<Self, ResampleInitError> {
        let chunk = chunk_frames.max(1);
        let err = |reason: String| ResampleInitError {
            from,
            to,
            chunk,
            reason,
        };
        if from == 0 || to == 0 {
            return Err(err("sample rate must be > 0".into()));
        }
        let (inner, scratch) = if from == to {
            (None, Vec::new())
        } else {
            let r = Fft::<f32>::new(from as usize, to as usize, chunk, 1, FixedSync::Input)
                .map_err(|e| err(e.to_string()))?;
            let n = rubato::Resampler::output_frames_max(&r);
            (Some(r), vec![0.0; n])
        };
        Ok(Self {
            from,
            to,
            inner,
            chunk,
            pending: Vec::with_capacity(chunk * 2),
            scratch,
        })
    }

    /// Configured input chunk size in frames.
    pub fn chunk_frames(&self) -> usize {
        self.chunk
    }

    /// Input rate (Hz).
    pub fn from_rate(&self) -> u32 {
        self.from
    }

    /// Output rate (Hz).
    pub fn to_rate(&self) -> u32 {
        self.to
    }

    /// True when `from == to` (no processing).
    pub fn is_passthrough(&self) -> bool {
        self.inner.is_none()
    }

    /// Input samples held back waiting for a full chunk.
    pub fn pending_input(&self) -> usize {
        self.pending.len()
    }

    /// Algorithmic delay in output frames.
    pub fn delay_frames(&self) -> usize {
        self.inner.as_ref().map_or(0, |r| r.output_delay())
    }

    /// Drop buffered state (call after a device rebuild / gap).
    pub fn reset(&mut self) {
        self.pending.clear();
        if let Some(r) = &mut self.inner {
            r.reset();
        }
    }

    /// Feed `input` and append all complete output to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let Some(r) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return;
        };
        self.pending.extend_from_slice(input);
        let mut consumed = 0usize;
        loop {
            let need = r.input_frames_next();
            if self.pending.len() - consumed < need {
                break;
            }
            let out_cap = self.scratch.len();
            let (Ok(inb), Ok(mut outb)) = (
                InterleavedSlice::new(&self.pending[consumed..consumed + need], 1, need),
                InterleavedSlice::new_mut(&mut self.scratch[..], 1, out_cap),
            ) else {
                break;
            };
            match r.process_into_buffer(&inb, &mut outb, None) {
                Ok((nin, nout)) => {
                    consumed += nin.max(1).min(need);
                    out.extend_from_slice(&self.scratch[..nout]);
                }
                Err(_) => {
                    // Cannot happen with correctly sized buffers; drop the chunk.
                    consumed += need;
                }
            }
        }
        self.pending.drain(..consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Estimated frequency from rising zero crossings, ignoring `skip` leading samples.
    fn freq(x: &[f32], rate: u32, skip: usize) -> f32 {
        let x = &x[skip..];
        let mut crossings: Vec<f32> = Vec::new();
        for i in 1..x.len() {
            if x[i - 1] < 0.0 && x[i] >= 0.0 {
                let frac = -x[i - 1] / (x[i] - x[i - 1]);
                crossings.push((i - 1) as f32 + frac);
            }
        }
        let (first, last) = (crossings[0], crossings[crossings.len() - 1]);
        (crossings.len() - 1) as f32 * rate as f32 / (last - first)
    }

    fn run(from: u32, to: u32, chunk: usize, hz: f32, secs: f32, feed: usize) -> Vec<f32> {
        let mut r = Resampler::new(from, to, chunk).unwrap();
        let input = sine(from, hz, (from as f32 * secs) as usize);
        let mut out = Vec::new();
        for piece in input.chunks(feed) {
            r.process(piece, &mut out);
        }
        out
    }

    #[test]
    fn frequency_and_length_preserved() {
        for &(from, to) in &[
            (44_100u32, 48_000u32),
            (48_000, 44_100),
            (16_000, 48_000),
            (48_000, 16_000),
            (96_000, 48_000),
            (48_000, 96_000),
            (24_000, 48_000),
        ] {
            let chunk = (from / 100) as usize; // 10 ms
            let out = run(from, to, chunk, 440.0, 1.0, 333); // odd feed size
            let expect = to as f32; // 1 s
            let slack = (to / 100 * 2 + 64) as f32; // pending + delay
            assert!(
                (out.len() as f32 - expect).abs() < slack,
                "{from}->{to}: len {} vs {expect}",
                out.len()
            );
            let f = freq(&out, to, (to / 10) as usize);
            assert!((f - 440.0).abs() < 1.0, "{from}->{to}: freq {f}");
            let peak = out[to as usize / 5..]
                .iter()
                .fold(0.0f32, |m, v| m.max(v.abs()));
            assert!((peak - 0.5).abs() < 0.05, "{from}->{to}: peak {peak}");
        }
    }

    #[test]
    fn twenty_ms_chunks() {
        let out = run(44_100, 48_000, 882, 1000.0, 1.0, 1000);
        let f = freq(&out, 48_000, 4800);
        assert!((f - 1000.0).abs() < 2.0, "{f}");
    }

    #[test]
    fn passthrough_is_identity() {
        let mut r = Resampler::new(48_000, 48_000, 480).unwrap();
        assert!(r.is_passthrough());
        let mut out = Vec::new();
        r.process(&[1.0, 2.0, 3.0], &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn remainder_is_kept_across_calls() {
        let mut r = Resampler::new(16_000, 48_000, 160).unwrap();
        let mut out = Vec::new();
        r.process(&vec![0.0; 100], &mut out);
        assert!(out.is_empty());
        assert_eq!(r.pending_input(), 100);
        r.process(&vec![0.0; 100], &mut out);
        assert_eq!(out.len(), 480);
        assert_eq!(r.pending_input(), 40);
    }

    #[test]
    fn rejects_zero_rate() {
        assert!(Resampler::new(0, 48_000, 10).is_err());
    }
}
