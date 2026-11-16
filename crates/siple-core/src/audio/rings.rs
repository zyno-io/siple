//! SPSC rings (mono `f32` at the device rate) and the allocation-free,
//! lock-free conversion code that runs inside cpal callbacks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::Thread;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// Frames converted per inner-loop iteration (stack buffer size).
const CHUNK: usize = 256;

/// Wakes a sleeping consumer thread from audio callbacks.
///
/// The notify path is lock-free and allocation-free: an atomic flag plus
/// `Thread::unpark` on the registered thread. The consumer (media thread)
/// calls [`Notifier::register_current_thread`] once and then
/// [`Notifier::wait_timeout`] in its loop.
#[derive(Default)]
pub struct Notifier {
    flag: AtomicBool,
    thread: ArcSwapOption<Thread>,
}

impl Notifier {
    /// New notifier with no registered thread.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register the calling thread as the one to wake.
    pub fn register_current_thread(&self) {
        self.thread.store(Some(Arc::new(std::thread::current())));
    }

    /// Signal that data is available / space was freed. Safe for RT callbacks.
    pub fn notify(&self) {
        self.flag.store(true, Ordering::Release);
        if let Some(t) = self.thread.load().as_ref() {
            t.unpark();
        }
    }

    /// Sleep until notified or `timeout` elapses. Returns `true` if a
    /// notification was pending/received. Must be called from the registered
    /// thread to be woken early.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.flag.swap(false, Ordering::AcqRel) {
            return true;
        }
        std::thread::park_timeout(timeout);
        self.flag.swap(false, Ordering::AcqRel)
    }
}

/// A sample type cpal may hand us; converts to/from normalised `f32`.
pub trait AudioSample: cpal::SizedSample + Copy + Send + 'static {
    /// To `-1.0..=1.0`.
    fn to_f32(self) -> f32;
    /// From `-1.0..=1.0` (clamped). `from_f32(0.0)` is the format's silence.
    fn from_f32(v: f32) -> Self;
}

impl AudioSample for f32 {
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        v.clamp(-1.0, 1.0)
    }
}

impl AudioSample for i16 {
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32 / 32768.0
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
    }
}

impl AudioSample for u16 {
    #[inline]
    fn to_f32(self) -> f32 {
        (self as f32 - 32768.0) / 32768.0
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 32767.0 + 32768.0).round() as u16
    }
}

impl AudioSample for i32 {
    #[inline]
    fn to_f32(self) -> f32 {
        (self as f64 / 2_147_483_648.0) as f32
    }
    #[inline]
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) as f64 * 2_147_483_647.0).round() as i32
    }
}

/// Counters shared between callbacks and the status API; survive rebuilds.
#[derive(Default)]
pub struct RingCounters {
    /// Capture samples dropped because the ring was full.
    pub overflows: AtomicU64,
    /// Playback callbacks that ran short of data (after the first data arrived).
    pub underflows: AtomicU64,
}

/// Callback-side writer of the capture ring.
pub struct CaptureWriter {
    prod: HeapProd<f32>,
    channels: usize,
    counters: Arc<RingCounters>,
    notifier: Arc<Notifier>,
}

impl CaptureWriter {
    /// Downmix interleaved `data` (N channels, any supported format) to mono
    /// and push it. Counts samples that did not fit. No allocation, no locks.
    pub fn write_interleaved<T: AudioSample>(&mut self, data: &[T]) {
        let ch = self.channels.max(1);
        let mut buf = [0.0f32; CHUNK];
        let mut dropped = 0usize;
        for block in data.chunks(CHUNK * ch) {
            let frames = block.len() / ch;
            for (o, frame) in buf.iter_mut().zip(block.chunks_exact(ch)) {
                let mut sum = 0.0f32;
                for s in frame {
                    sum += s.to_f32();
                }
                *o = sum / ch as f32;
            }
            let pushed = self.prod.push_slice(&buf[..frames]);
            dropped += frames - pushed;
        }
        if dropped > 0 {
            self.counters
                .overflows
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        self.notifier.notify();
    }
}

/// Consumer end of the capture ring (used by the media thread).
pub struct CaptureReader {
    cons: HeapCons<f32>,
}

impl CaptureReader {
    /// Pop up to `out.len()` mono samples.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        self.cons.pop_slice(out)
    }
    /// Samples currently buffered.
    pub fn available(&self) -> usize {
        self.cons.occupied_len()
    }
}

/// Callback-side reader of a playback ring.
pub struct PlaybackReader {
    cons: HeapCons<f32>,
    channels: usize,
    counters: Option<Arc<RingCounters>>,
    primed: bool,
    notifier: Arc<Notifier>,
}

impl PlaybackReader {
    /// Fill interleaved `out` from the mono ring, duplicating to every channel.
    /// Underflow is padded with silence and counted once per callback (only
    /// after the first samples have arrived, so an idle device is not noise).
    pub fn fill_interleaved<T: AudioSample>(&mut self, out: &mut [T]) {
        let ch = self.channels.max(1);
        let mut buf = [0.0f32; CHUNK];
        let mut short = false;
        for block in out.chunks_mut(CHUNK * ch) {
            let frames = block.len() / ch;
            let got = self.cons.pop_slice(&mut buf[..frames]);
            if got > 0 {
                self.primed = true;
            }
            if got < frames {
                short = true;
                buf[got..frames].fill(0.0);
            }
            for (frame, v) in block.chunks_exact_mut(ch).zip(buf.iter()) {
                let s = T::from_f32(*v);
                frame.fill(s);
            }
            // Trailing partial frame (should not happen): silence.
            let rem = block.len() % ch;
            if rem > 0 {
                let n = block.len();
                block[n - rem..].fill(T::from_f32(0.0));
            }
        }
        if short
            && self.primed
            && let Some(c) = &self.counters
        {
            c.underflows.fetch_add(1, Ordering::Relaxed);
        }
        self.notifier.notify();
    }
}

/// Producer end of a playback ring (used by the media thread).
pub struct PlaybackWriter {
    prod: HeapProd<f32>,
}

impl PlaybackWriter {
    /// Push up to `samples.len()` mono samples; returns the number accepted.
    pub fn write(&mut self, samples: &[f32]) -> usize {
        self.prod.push_slice(samples)
    }
    /// Samples currently queued.
    pub fn fill(&self) -> usize {
        self.prod.occupied_len()
    }
    /// Ring capacity in samples.
    pub fn capacity(&self) -> usize {
        self.prod.capacity().get()
    }
}

/// Ring capacity: ~250 ms at `rate`, at least 4096 samples.
fn ring_capacity(rate: u32) -> usize {
    ((rate as usize) / 4).max(4096)
}

/// Create a capture ring for a device with `channels` channels at `rate`.
pub fn capture_ring(
    rate: u32,
    channels: u16,
    counters: Arc<RingCounters>,
    notifier: Arc<Notifier>,
) -> (CaptureWriter, CaptureReader) {
    let (prod, cons) = HeapRb::<f32>::new(ring_capacity(rate)).split();
    (
        CaptureWriter {
            prod,
            channels: channels as usize,
            counters,
            notifier,
        },
        CaptureReader { cons },
    )
}

/// Create a playback ring for a device with `channels` channels at `rate`.
pub fn playback_ring(
    rate: u32,
    channels: u16,
    counters: Option<Arc<RingCounters>>,
    notifier: Arc<Notifier>,
) -> (PlaybackWriter, PlaybackReader) {
    let (prod, cons) = HeapRb::<f32>::new(ring_capacity(rate)).split();
    (
        PlaybackWriter { prod },
        PlaybackReader {
            cons,
            channels: channels as usize,
            counters,
            primed: false,
            notifier,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(channels: u16) -> (CaptureWriter, CaptureReader, Arc<RingCounters>) {
        let c = Arc::new(RingCounters::default());
        let (w, r) = capture_ring(8000, channels, c.clone(), Notifier::new());
        (w, r, c)
    }

    #[test]
    fn downmix_stereo_f32() {
        let (mut w, mut r, _) = cap(2);
        w.write_interleaved(&[1.0f32, 0.0, 0.5, -0.5, -1.0, -1.0]);
        let mut out = [0.0f32; 8];
        assert_eq!(r.read(&mut out), 3);
        assert_eq!(&out[..3], &[0.5, 0.0, -1.0]);
    }

    #[test]
    fn downmix_i16_and_u16_and_i32() {
        let (mut w, mut r, _) = cap(1);
        w.write_interleaved(&[16384i16, -16384]);
        w.write_interleaved(&[32768u16 + 16384, 32768 - 16384]);
        w.write_interleaved(&[1_073_741_824i32, -1_073_741_824]);
        let mut out = [0.0f32; 8];
        assert_eq!(r.read(&mut out), 6);
        for (i, v) in out[..6].iter().enumerate() {
            let want = if i % 2 == 0 { 0.5 } else { -0.5 };
            assert!((v - want).abs() < 1e-4, "{i}: {v}");
        }
    }

    #[test]
    fn downmix_longer_than_chunk() {
        let (mut w, mut r, _) = cap(3);
        let frames = CHUNK * 2 + 17;
        let data: Vec<f32> = (0..frames * 3).map(|i| (i / 3) as f32 / 1000.0).collect();
        w.write_interleaved(&data);
        let mut out = vec![0.0f32; frames];
        assert_eq!(r.read(&mut out), frames);
        for (i, v) in out.iter().enumerate() {
            assert!((v - i as f32 / 1000.0).abs() < 1e-5);
        }
    }

    #[test]
    fn capture_overflow_counted() {
        let (mut w, r, c) = cap(1);
        let n = ring_capacity(8000) + 100;
        w.write_interleaved(&vec![0.1f32; n]);
        assert_eq!(c.overflows.load(Ordering::Relaxed), 100);
        assert_eq!(r.available(), ring_capacity(8000));
    }

    #[test]
    fn playback_duplicates_and_underflows() {
        let c = Arc::new(RingCounters::default());
        let (mut w, mut r) = playback_ring(8000, 2, Some(c.clone()), Notifier::new());
        // Idle device before any data: no underflow counted.
        let mut out = [1.0f32; 6];
        r.fill_interleaved(&mut out);
        assert_eq!(out, [0.0; 6]);
        assert_eq!(c.underflows.load(Ordering::Relaxed), 0);

        w.write(&[0.25, -0.5]);
        let mut out = [9.0f32; 6]; // 3 frames, only 2 available
        r.fill_interleaved(&mut out);
        assert_eq!(out, [0.25, 0.25, -0.5, -0.5, 0.0, 0.0]);
        assert_eq!(c.underflows.load(Ordering::Relaxed), 1);

        w.write(&[0.5, 0.5, 0.5]);
        r.fill_interleaved(&mut out);
        assert_eq!(out, [0.5; 6]);
        assert_eq!(c.underflows.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn playback_u16_silence_is_midpoint() {
        let (_w, mut r) = playback_ring(8000, 1, None, Notifier::new());
        let mut out = [0u16; 4];
        r.fill_interleaved(&mut out);
        assert_eq!(out, [32768; 4]);
    }

    #[test]
    fn sample_roundtrip_extremes() {
        assert_eq!(i16::from_f32(1.5), 32767);
        assert_eq!(i16::from_f32(-1.5), -32767);
        assert_eq!(u16::from_f32(0.0), 32768);
        assert_eq!(i32::from_f32(1.0), i32::MAX);
        assert!((i16::MIN.to_f32() + 1.0).abs() < 1e-6);
    }

    #[test]
    fn notifier_wakes_waiter() {
        let n = Notifier::new();
        n.register_current_thread();
        let n2 = n.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            n2.notify();
        });
        let t = std::time::Instant::now();
        assert!(n.wait_timeout(Duration::from_secs(5)));
        assert!(t.elapsed() < Duration::from_secs(2));
        h.join().unwrap();
        assert!(!n.wait_timeout(Duration::from_millis(1)));
    }
}
