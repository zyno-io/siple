//! Which calls an agent is capturing audio from right now (the "agent listening" indicator;
//! App Review guideline 2.5.14).
//!
//! Capturing means an agent request that takes the far end's audio out of Siple: `call.listen`
//! and `call.rx_audio` (returned, saved or transcribed), a `call.stream` with RX, and
//! `call.record_start` until `call.record_stop` (or the call's end). The receive-history buffer
//! that backs `since_rx_ms` is a short in-memory ring that nothing can read without such a
//! request, so it is not "recording" by itself: the indicator covers the request that reads it.
//!
//! Request-scoped captures hold a [`CaptureGuard`], so the flag is cleared on every exit path
//! (success, error, timeout, a dropped request future). The UI view additionally keeps a capture
//! visible for [`LINGER`] after it ended so a short request is not missed by a poll.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the UI keeps showing a capture after it ended.
pub const LINGER: Duration = Duration::from_millis(2_000);

/// What the agent is doing with a call's audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    /// `call.listen` / `call.rx_audio`.
    Listen,
    /// The same, with `transcribe:true` (speech-to-text).
    Transcribe,
    /// `call.stream` with RX.
    Stream,
    /// `call.record_start` .. `call.record_stop`.
    Record,
}

impl CaptureKind {
    const ALL: [CaptureKind; 4] = [Self::Listen, Self::Transcribe, Self::Stream, Self::Record];

    fn idx(self) -> usize {
        self as usize
    }

    /// Name used in `call.get` / `call.list` `agent_capture`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Listen => "listen",
            Self::Transcribe => "transcribe",
            Self::Stream => "stream",
            Self::Record => "record",
        }
    }
}

/// The kinds of capture active on one call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentCapture {
    kinds: [bool; 4],
}

impl AgentCapture {
    /// A capture of exactly these kinds (tests and previews).
    pub fn of(kinds: &[CaptureKind]) -> Self {
        let mut c = Self::default();
        for k in kinds {
            c.kinds[k.idx()] = true;
        }
        c
    }

    /// Any capture at all.
    pub fn is_active(&self) -> bool {
        self.kinds.iter().any(|k| *k)
    }

    /// Whether `kind` is active.
    pub fn has(&self, kind: CaptureKind) -> bool {
        self.kinds[kind.idx()]
    }

    /// Names of the active kinds, in a fixed order (`call.get` `agent_capture`).
    pub fn names(&self) -> Vec<&'static str> {
        CaptureKind::ALL
            .into_iter()
            .filter(|k| self.has(*k))
            .map(CaptureKind::as_str)
            .collect()
    }

    /// Short label for the call badge.
    pub fn label(&self) -> &'static str {
        if self.has(CaptureKind::Record) {
            "Agent recording"
        } else if self.has(CaptureKind::Transcribe) {
            "Agent transcribing"
        } else {
            "Agent listening"
        }
    }
}

#[derive(Default)]
struct Entry {
    counts: [u32; 4],
    /// When each kind last dropped to zero (for the UI linger).
    ended: [Option<Instant>; 4],
}

/// Per-call capture state.
#[derive(Default)]
pub struct CaptureRegistry {
    map: Mutex<HashMap<String, Entry>>,
}

impl CaptureRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Marks `kinds` active on the call until the returned guard is dropped.
    pub fn begin(self: &Arc<Self>, call_id: &str, kinds: &[CaptureKind]) -> CaptureGuard {
        {
            let mut g = self.lock();
            let e = g.entry(call_id.to_string()).or_default();
            for k in kinds {
                e.counts[k.idx()] += 1;
            }
        }
        CaptureGuard {
            reg: self.clone(),
            call_id: call_id.to_string(),
            kinds: kinds.to_vec(),
        }
    }

    fn end(&self, call_id: &str, kinds: &[CaptureKind], now: Instant) {
        let mut g = self.lock();
        if let Some(e) = g.get_mut(call_id) {
            for k in kinds {
                let c = &mut e.counts[k.idx()];
                if *c > 0 {
                    *c -= 1;
                    if *c == 0 {
                        e.ended[k.idx()] = Some(now);
                    }
                }
            }
        }
    }

    /// Sets or clears the recording flag (not request-scoped).
    pub fn set_recording(&self, call_id: &str, on: bool) {
        let mut g = self.lock();
        let i = CaptureKind::Record.idx();
        if !on && !g.contains_key(call_id) {
            return;
        }
        let e = g.entry(call_id.to_string()).or_default();
        let was = e.counts[i] > 0;
        e.counts[i] = u32::from(on);
        if was && !on {
            e.ended[i] = Some(Instant::now());
        }
    }

    /// The call ended: nothing is captured any more. Lingering display is dropped too.
    pub fn clear_call(&self, call_id: &str) {
        self.lock().remove(call_id);
    }

    /// What is being captured on the call right now (`call.get`).
    pub fn active(&self, call_id: &str) -> AgentCapture {
        self.lock()
            .get(call_id)
            .map_or_else(AgentCapture::default, |e| {
                let mut c = AgentCapture::default();
                for k in CaptureKind::ALL {
                    c.kinds[k.idx()] = e.counts[k.idx()] > 0;
                }
                c
            })
    }

    /// What the UI shows at `now`: active captures plus those that ended less than [`LINGER`]
    /// ago. Calls without any are left out.
    pub fn visible(&self, now: Instant) -> HashMap<String, AgentCapture> {
        let mut g = self.lock();
        let mut out = HashMap::new();
        g.retain(|id, e| {
            let mut c = AgentCapture::default();
            for k in CaptureKind::ALL {
                let i = k.idx();
                let lingering =
                    e.ended[i].is_some_and(|t| now.saturating_duration_since(t) < LINGER);
                c.kinds[i] = e.counts[i] > 0 || lingering;
            }
            let keep = c.is_active();
            if keep {
                out.insert(id.clone(), c);
            }
            keep
        });
        out
    }
}

/// Keeps the capture marked while alive.
pub struct CaptureGuard {
    reg: Arc<CaptureRegistry>,
    call_id: String,
    kinds: Vec<CaptureKind>,
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        self.reg.end(&self.call_id, &self.kinds, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Arc<CaptureRegistry> {
        Arc::new(CaptureRegistry::default())
    }

    #[test]
    fn guard_sets_and_clears() {
        let r = reg();
        assert!(!r.active("c1").is_active());
        let g = r.begin("c1", &[CaptureKind::Listen, CaptureKind::Transcribe]);
        let a = r.active("c1");
        assert_eq!(a.names(), ["listen", "transcribe"]);
        assert_eq!(a.label(), "Agent transcribing");
        assert!(!r.active("c2").is_active());
        drop(g);
        assert!(!r.active("c1").is_active());
    }

    #[test]
    fn overlapping_captures_count() {
        let r = reg();
        let a = r.begin("c1", &[CaptureKind::Listen]);
        let b = r.begin("c1", &[CaptureKind::Listen]);
        drop(a);
        assert!(r.active("c1").has(CaptureKind::Listen));
        drop(b);
        assert!(!r.active("c1").is_active());
    }

    #[test]
    fn guard_clears_when_the_request_future_is_dropped() {
        let r = reg();
        let r2 = r.clone();
        let mut fut = Box::pin(async move {
            let _g = r2.begin("c1", &[CaptureKind::Listen]);
            std::future::pending::<()>().await;
        });
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(fut.as_mut(), &mut cx).is_pending());
        assert!(r.active("c1").is_active());
        drop(fut);
        assert!(!r.active("c1").is_active());
    }

    #[test]
    fn call_end_clears_and_a_late_guard_is_harmless() {
        let r = reg();
        let g = r.begin("c1", &[CaptureKind::Stream]);
        r.set_recording("c1", true);
        assert_eq!(r.active("c1").names(), ["stream", "record"]);
        assert_eq!(r.active("c1").label(), "Agent recording");
        r.clear_call("c1");
        assert!(!r.active("c1").is_active());
        drop(g);
        assert!(!r.active("c1").is_active());
        assert!(r.visible(Instant::now()).is_empty());
    }

    #[test]
    fn recording_flag_toggles() {
        let r = reg();
        r.set_recording("c1", true);
        assert!(r.active("c1").has(CaptureKind::Record));
        r.set_recording("c1", false);
        assert!(!r.active("c1").is_active());
    }

    #[test]
    fn ui_view_lingers_then_expires() {
        let r = reg();
        let g = r.begin("c1", &[CaptureKind::Listen]);
        assert!(r.visible(Instant::now())["c1"].has(CaptureKind::Listen));
        drop(g);
        let now = Instant::now();
        assert!(!r.active("c1").is_active());
        assert!(r.visible(now)["c1"].has(CaptureKind::Listen));
        assert!(
            r.visible(now + LINGER + Duration::from_millis(1))
                .is_empty()
        );
    }
}
