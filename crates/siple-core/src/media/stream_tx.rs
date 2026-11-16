//! TX stream ring: a producer (API / stream task) pushes 48 kHz mono `f32` audio, the media
//! thread drains it in real time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// Default ring capacity: 2 s of 48 kHz audio.
pub const TX_STREAM_SAMPLES: usize = 96_000;

/// Counters shared between the producer and the media thread.
#[derive(Debug, Default)]
pub struct TxStreamShared {
    /// Samples accepted into the ring by the producer, ever.
    pushed: AtomicU64,
    /// Samples taken from the ring by the media thread: played, discarded while gated, or
    /// discarded by a clear. Reaches `pushed` once the ring is empty.
    consumed: AtomicU64,
    /// The part of `consumed` that was discarded by clears.
    cleared: AtomicU64,
    /// Samples that were needed for transmission but missing from the ring.
    underruns: AtomicU64,
    /// Clear target: the media thread discards samples until `consumed` reaches this
    /// `pushed` watermark (audio pushed after the request is untouched).
    clear_to: AtomicU64,
}

/// Producer side of the TX stream ring.
pub struct TxStreamProducer {
    prod: HeapProd<f32>,
    shared: Arc<TxStreamShared>,
}

impl std::fmt::Debug for TxStreamProducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TxStreamProducer")
            .field("buffered", &self.buffered())
            .field("consumed", &self.consumed())
            .field("underrun_samples", &self.underrun_samples())
            .finish()
    }
}

impl TxStreamProducer {
    /// Pushes as many samples as fit; returns how many were taken.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let n = self.prod.push_slice(samples);
        // After the ring write, so any `pushed` value a clear observes is backed by samples.
        self.shared.pushed.fetch_add(n as u64, Ordering::Release);
        n
    }

    /// Total samples accepted into the ring so far.
    pub fn pushed(&self) -> u64 {
        self.shared.pushed.load(Ordering::Acquire)
    }

    /// Total samples the media thread has discarded because of clears.
    pub fn cleared_samples(&self) -> u64 {
        self.shared.cleared.load(Ordering::Relaxed)
    }

    /// Free space, in samples.
    pub fn vacant(&self) -> usize {
        self.prod.vacant_len()
    }

    /// Samples waiting to be transmitted.
    pub fn buffered(&self) -> usize {
        self.prod.occupied_len()
    }

    /// Total samples the media thread has taken from the ring so far (played out, discarded
    /// while gated, or discarded by a clear).
    pub fn consumed(&self) -> u64 {
        self.shared.consumed.load(Ordering::Relaxed)
    }

    /// Total samples transmission needed but the ring could not supply.
    pub fn underrun_samples(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Discards everything pushed before this call (applied by the media thread on its next
    /// tick). Audio pushed afterwards is kept and played.
    pub fn request_clear(&self) {
        let to = self.shared.pushed.load(Ordering::Acquire);
        self.shared.clear_to.fetch_max(to, Ordering::AcqRel);
    }
}

/// Consumer side, owned by the media thread.
pub(crate) struct TxStreamCons {
    cons: HeapCons<f32>,
    shared: Arc<TxStreamShared>,
}

impl TxStreamCons {
    /// Applies a pending clear request: skips what is buffered up to the clear watermark,
    /// counting it as consumed and cleared.
    pub fn check_clear(&mut self) {
        let to = self.shared.clear_to.load(Ordering::Acquire);
        let done = self.shared.consumed.load(Ordering::Relaxed);
        if to > done {
            let n = self.cons.occupied_len().min((to - done) as usize);
            let n = self.cons.skip(n);
            self.shared.consumed.fetch_add(n as u64, Ordering::Relaxed);
            self.shared.cleared.fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// Fills `out` from the ring; returns how many samples were available (the rest of `out`
    /// is left untouched).
    pub fn pop(&mut self, out: &mut [f32]) -> usize {
        let n = self.cons.pop_slice(out);
        self.shared.consumed.fetch_add(n as u64, Ordering::Relaxed);
        n
    }

    pub fn add_underrun(&self, samples: usize) {
        if samples > 0 {
            self.shared
                .underruns
                .fetch_add(samples as u64, Ordering::Relaxed);
        }
    }
}

/// Creates a ring of `capacity` samples.
pub(crate) fn pair(capacity: usize) -> (TxStreamProducer, TxStreamCons) {
    let (prod, cons) = HeapRb::<f32>::new(capacity).split();
    let shared = Arc::new(TxStreamShared::default());
    (
        TxStreamProducer {
            prod,
            shared: shared.clone(),
        },
        TxStreamCons { cons, shared },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_keeps_audio_pushed_after_the_request() {
        let (mut p, mut c) = pair(1000);
        let a = vec![1.0f32; 300];
        let b = vec![2.0f32; 200];
        assert_eq!(p.push(&a), 300);
        // Part of A was already played before the clear is applied.
        let mut out = [0.0f32; 100];
        assert_eq!(c.pop(&mut out), 100);
        p.request_clear();
        assert_eq!(p.push(&b), 200);
        // One tick later: A's remaining 200 samples are skipped, B survives.
        c.check_clear();
        assert_eq!(p.cleared_samples(), 200);
        let mut out = vec![0.0f32; 500];
        assert_eq!(c.pop(&mut out), 200);
        assert!(out[..200].iter().all(|&v| v == 2.0));
        assert_eq!(p.pushed(), 500);
        assert_eq!(p.consumed(), p.pushed());
        // A second tick without a new request clears nothing more.
        c.check_clear();
        assert_eq!(p.cleared_samples(), 200);
    }

    #[test]
    fn clear_with_nothing_pending_does_not_swallow_later_audio() {
        let (mut p, mut c) = pair(1000);
        p.request_clear();
        p.push(&[3.0; 50]);
        c.check_clear();
        let mut out = [0.0f32; 50];
        assert_eq!(c.pop(&mut out), 50);
        assert_eq!(p.cleared_samples(), 0);
    }
}
