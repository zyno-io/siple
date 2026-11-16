//! System-wide event bus: a seq-numbered ring of the most recent events plus a broadcast
//! channel for push subscribers.
//!
//! Every state change in the engine is `emit`ted here. Clients either long-poll
//! ([`EventLog::poll`], the primary path for MCP/LLM use) or stream ([`EventLog::subscribe`]).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;
use tokio::time::Instant;

/// Ring size used by the application.
pub const DEFAULT_CAPACITY: usize = 10_000;

/// Approximate cap on the resident size of the ring (serialized payloads plus per-event
/// overhead), applied in addition to the event count. The oldest events are evicted first.
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Fixed per-event overhead estimate (struct, strings, allocator slack) added to the byte
/// accounting on top of the variable-length fields.
const EVENT_OVERHEAD: usize = 160;

/// Kind of the synthetic marker a lagging [`Subscription`] receives (seq is 0).
pub const LAGGED_KIND: &str = "events.lagged";

/// One event. `seq` is unique and strictly increasing within a process run, starting at 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Position in the log.
    pub seq: u64,
    /// RFC 3339 UTC timestamp with millisecond precision.
    pub ts: String,
    /// Event type, e.g. `call.state`.
    pub kind: String,
    /// Account id, if the event belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Call id (`c1`, ...), if the event belongs to a call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// Event-specific payload.
    #[serde(default)]
    pub data: Value,
}

/// Selects events. All present criteria must match; an empty filter matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventFilter {
    /// Exact kinds, or prefixes with a trailing `*` (`call.*`; `*` alone matches all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// Only events for this account id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Only events for this call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
}

impl EventFilter {
    /// Filter that matches every event.
    pub fn any() -> Self {
        Self::default()
    }

    /// Filter on kind patterns only.
    pub fn kinds<I, S>(kinds: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kinds: Some(kinds.into_iter().map(Into::into).collect()),
            ..Self::default()
        }
    }

    /// Whether `event` passes the filter. An empty `kinds` list means "no kind restriction".
    pub fn matches(&self, event: &Event) -> bool {
        if let Some(kinds) = &self.kinds
            && !kinds.is_empty()
            && !kinds.iter().any(|p| kind_matches(p, &event.kind))
        {
            return false;
        }
        if let Some(a) = &self.account
            && event.account.as_deref() != Some(a.as_str())
        {
            return false;
        }
        if let Some(c) = &self.call_id
            && event.call_id.as_deref() != Some(c.as_str())
        {
            return false;
        }
        true
    }
}

fn kind_matches(pattern: &str, kind: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => kind.starts_with(prefix),
        None => pattern == kind,
    }
}

/// Result of [`EventLog::poll`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollResult {
    /// Matching events with `seq > since_seq`, oldest first.
    pub events: Vec<Event>,
    /// Pass this as `since_seq` next time: nothing matching was skipped before it.
    pub next_seq: u64,
    /// True if events between `since_seq` and the oldest retained one were dropped from the
    /// ring (the client missed events).
    pub truncated: bool,
}

/// Compact stored form of an [`Event`]: the filterable fields are typed, the payload is kept
/// as serialized JSON (a `serde_json::Value` tree is ~10x larger resident) and parsed back
/// only for events actually returned or streamed.
struct Stored {
    seq: u64,
    ts: String,
    kind: String,
    account: Option<String>,
    call_id: Option<String>,
    data: Box<str>,
}

impl Stored {
    fn new(
        seq: u64,
        kind: String,
        account: Option<&str>,
        call_id: Option<&str>,
        data: &Value,
    ) -> Self {
        Self {
            seq,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            kind,
            account: account.map(str::to_owned),
            call_id: call_id.map(str::to_owned),
            data: serde_json::to_string(data)
                .unwrap_or_else(|_| "null".into())
                .into_boxed_str(),
        }
    }

    fn bytes(&self) -> usize {
        EVENT_OVERHEAD
            + self.ts.len()
            + self.kind.len()
            + self.account.as_ref().map_or(0, String::len)
            + self.call_id.as_ref().map_or(0, String::len)
            + self.data.len()
    }

    fn matches(&self, filter: &EventFilter) -> bool {
        if let Some(kinds) = &filter.kinds
            && !kinds.is_empty()
            && !kinds.iter().any(|p| kind_matches(p, &self.kind))
        {
            return false;
        }
        if let Some(a) = &filter.account
            && self.account.as_deref() != Some(a.as_str())
        {
            return false;
        }
        if let Some(c) = &filter.call_id
            && self.call_id.as_deref() != Some(c.as_str())
        {
            return false;
        }
        true
    }

    fn to_event(&self) -> Event {
        Event {
            seq: self.seq,
            ts: self.ts.clone(),
            kind: self.kind.clone(),
            account: self.account.clone(),
            call_id: self.call_id.clone(),
            data: serde_json::from_str(&self.data).unwrap_or(Value::Null),
        }
    }
}

struct Inner {
    ring: VecDeque<Arc<Stored>>,
    ring_bytes: usize,
    last_seq: u64,
}

fn random_instance_id() -> String {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        // Extremely unlikely; fall back to the clock so ids still differ between runs.
        let n = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        b = n.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The event bus. Cheap to share behind an `Arc`; all methods take `&self`.
pub struct EventLog {
    inner: Mutex<Inner>,
    capacity: usize,
    max_bytes: usize,
    tx: broadcast::Sender<Arc<Stored>>,
    instance_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

impl EventLog {
    /// Creates a log retaining the last `capacity` events (at least 1).
    pub fn new(capacity: usize) -> Self {
        Self::with_limits(capacity, DEFAULT_MAX_BYTES)
    }

    /// Creates a log retaining at most `capacity` events (at least 1) and roughly `max_bytes`
    /// of stored data; the newest event is always kept.
    pub fn with_limits(capacity: usize, max_bytes: usize) -> Self {
        let capacity = capacity.max(1);
        let (tx, _) = broadcast::channel(capacity.clamp(16, 4096));
        Self {
            inner: Mutex::new(Inner {
                ring: VecDeque::with_capacity(capacity.min(1024)),
                ring_bytes: 0,
                last_seq: 0,
            }),
            capacity,
            max_bytes,
            tx,
            instance_id: random_instance_id(),
            started_at: chrono::Utc::now(),
        }
    }

    /// Random id of this log (one per application start). Seqs and call ids are only
    /// meaningful together with it: a different id means the application restarted.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// When this log (and so, in the application, the instance) was created.
    pub fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.started_at
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends an event and wakes pollers/subscribers. Returns its seq.
    pub fn emit(
        &self,
        kind: impl Into<String>,
        account: Option<&str>,
        call_id: Option<&str>,
        data: Value,
    ) -> u64 {
        // Serialize outside the lock.
        let kind = kind.into();
        let mut stored = Stored::new(0, kind, account, call_id, &data);
        let mut g = self.lock();
        g.last_seq += 1;
        stored.seq = g.last_seq;
        let seq = stored.seq;
        let stored = Arc::new(stored);
        g.ring_bytes += stored.bytes();
        g.ring.push_back(stored.clone());
        while g.ring.len() > self.capacity || (g.ring_bytes > self.max_bytes && g.ring.len() > 1) {
            if let Some(old) = g.ring.pop_front() {
                g.ring_bytes -= old.bytes();
            }
        }
        // Sent under the lock so receivers observe events in seq order.
        let _ = self.tx.send(stored);
        seq
    }

    /// Approximate bytes currently retained by the ring.
    pub fn retained_bytes(&self) -> usize {
        self.lock().ring_bytes
    }

    /// Seq of the most recent event (0 if none yet). Use as the `since` baseline before
    /// performing an action so that waiting for its consequences cannot miss anything.
    pub fn current_seq(&self) -> u64 {
        self.lock().last_seq
    }

    /// Matching events with `seq > seq`, oldest first, at most `limit`.
    pub fn since(&self, seq: u64, filter: &EventFilter, limit: usize) -> Vec<Event> {
        self.scan(seq, filter, limit).events
    }

    fn scan(&self, since: u64, filter: &EventFilter, limit: usize) -> PollResult {
        let g = self.lock();
        let oldest = g.ring.front().map_or(g.last_seq + 1, |e| e.seq);
        let truncated = since.saturating_add(1) < oldest;
        // Ring seqs are contiguous, so the start index is computable.
        let start = since.saturating_add(1).max(oldest) - oldest;
        let mut events = Vec::new();
        let mut next_seq = g.last_seq.max(since);
        for e in g.ring.iter().skip(start.try_into().unwrap_or(usize::MAX)) {
            if e.matches(filter) {
                if events.len() >= limit {
                    // More matches remain: resume right after the last one returned.
                    next_seq = events.last().map_or(since, |l: &Event| l.seq);
                    break;
                }
                events.push(e.to_event());
            }
        }
        PollResult {
            events,
            next_seq,
            truncated,
        }
    }

    /// Long-poll: returns at once if matching events with `seq > since_seq` exist, else waits
    /// up to `timeout` for one (returning an empty result on timeout). Race-free: an event
    /// emitted between the check and the wait is not missed.
    pub async fn poll(
        &self,
        since_seq: u64,
        filter: &EventFilter,
        limit: usize,
        timeout: Duration,
    ) -> PollResult {
        let deadline = Instant::now() + timeout;
        // Subscribe before scanning: anything emitted after this point is in the channel.
        let mut rx = self.tx.subscribe();
        loop {
            let r = self.scan(since_seq, filter, limit.max(1));
            if !r.events.is_empty() || timeout.is_zero() {
                return r;
            }
            loop {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Err(_) => return self.scan(since_seq, filter, limit.max(1)),
                    Ok(Ok(e)) if e.matches(filter) && e.seq > since_seq => break,
                    Ok(Ok(_)) => {}
                    // Lagged or (impossible, we hold the sender) closed: rescan the ring.
                    Ok(Err(_)) => break,
                }
            }
        }
    }

    /// Push stream of events emitted from now on that pass `filter`.
    pub fn subscribe(&self, filter: EventFilter) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
            filter,
        }
    }
}

impl Default for EventLog {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl std::fmt::Debug for EventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLog")
            .field("current_seq", &self.current_seq())
            .field("capacity", &self.capacity)
            .finish()
    }
}

/// A push subscription created by [`EventLog::subscribe`].
pub struct Subscription {
    rx: broadcast::Receiver<Arc<Stored>>,
    filter: EventFilter,
}

impl Subscription {
    /// Next matching event. If the subscriber fell behind, yields one [`LAGGED_KIND`] marker
    /// (seq 0, `data.missed` = number of events dropped) and then continues with the oldest
    /// event still buffered. `None` once the log is dropped.
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            match self.rx.recv().await {
                Ok(e) if e.matches(&self.filter) => return Some(e.to_event()),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    return Some(Event {
                        seq: 0,
                        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                        kind: LAGGED_KIND.into(),
                        account: None,
                        call_id: None,
                        data: serde_json::json!({ "missed": n }),
                    });
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn emit(log: &EventLog, kind: &str) -> u64 {
        log.emit(kind, None, None, Value::Null)
    }

    #[test]
    fn seq_is_monotonic_from_one() {
        let log = EventLog::new(10);
        assert_eq!(log.current_seq(), 0);
        assert_eq!(emit(&log, "a"), 1);
        assert_eq!(emit(&log, "b"), 2);
        assert_eq!(log.current_seq(), 2);
        let evs = log.since(0, &EventFilter::any(), 100);
        assert_eq!(evs.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert!(chrono::DateTime::parse_from_rfc3339(&evs[0].ts).is_ok());
        assert_eq!(log.since(1, &EventFilter::any(), 100).len(), 1);
    }

    #[test]
    fn concurrent_emit_unique_seqs() {
        let log = Arc::new(EventLog::new(10_000));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let l = log.clone();
                std::thread::spawn(move || (0..500).map(|_| emit(&l, "x")).collect::<Vec<_>>())
            })
            .collect();
        let mut all: Vec<u64> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_unstable();
        assert_eq!(all, (1..=4000).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn ring_truncation_flag() {
        let log = EventLog::new(5);
        for _ in 0..8 {
            emit(&log, "x");
        }
        // Retained: 4..=8.
        let r = log.poll(0, &EventFilter::any(), 100, Duration::ZERO).await;
        assert!(r.truncated);
        assert_eq!(r.events.first().unwrap().seq, 4);
        assert_eq!(r.next_seq, 8);
        // since=3 -> first wanted is 4 == oldest: nothing lost.
        let r = log.poll(3, &EventFilter::any(), 100, Duration::ZERO).await;
        assert!(!r.truncated);
        assert_eq!(r.events.len(), 5);
        // since=2 -> seq 3 was lost.
        assert!(
            log.poll(2, &EventFilter::any(), 100, Duration::ZERO)
                .await
                .truncated
        );
        // Fully caught up, nothing new.
        let r = log.poll(8, &EventFilter::any(), 100, Duration::ZERO).await;
        assert!(!r.truncated && r.events.is_empty() && r.next_seq == 8);
    }

    #[tokio::test]
    async fn limit_resumes_after_last_returned() {
        let log = EventLog::new(100);
        for _ in 0..5 {
            emit(&log, "x");
        }
        let r = log.poll(0, &EventFilter::any(), 2, Duration::ZERO).await;
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.next_seq, 2);
        let r = log
            .poll(r.next_seq, &EventFilter::any(), 10, Duration::ZERO)
            .await;
        assert_eq!(r.events.len(), 3);
        assert_eq!(r.next_seq, 5);
    }

    #[tokio::test]
    async fn poll_wakes_on_matching_emit_only() {
        let log = Arc::new(EventLog::new(100));
        let l2 = log.clone();
        let h = tokio::spawn(async move {
            l2.poll(
                0,
                &EventFilter::kinds(["call.*"]),
                10,
                Duration::from_secs(5),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        emit(&log, "account.registration");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!h.is_finished());
        emit(&log, "call.state");
        let r = tokio::time::timeout(Duration::from_secs(2), h)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].kind, "call.state");
        assert_eq!(r.next_seq, 2);
    }

    #[tokio::test]
    async fn poll_times_out_empty() {
        let log = EventLog::new(10);
        emit(&log, "a");
        let t = std::time::Instant::now();
        let r = log
            .poll(1, &EventFilter::any(), 10, Duration::from_millis(80))
            .await;
        assert!(r.events.is_empty() && !r.truncated && r.next_seq == 1);
        assert!(t.elapsed() >= Duration::from_millis(70));
    }

    #[test]
    fn filters_exact_prefix_account_call() {
        let log = EventLog::new(100);
        log.emit("call.state", Some("a1"), Some("c1"), json!({}));
        log.emit("call.dtmf", Some("a2"), Some("c2"), json!({}));
        log.emit("account.registration", Some("a1"), None, json!({}));
        let n = |f: EventFilter| log.since(0, &f, 100).len();
        assert_eq!(n(EventFilter::kinds(["call.*"])), 2);
        assert_eq!(n(EventFilter::kinds(["call.state"])), 1);
        assert_eq!(n(EventFilter::kinds(["call"])), 0);
        assert_eq!(n(EventFilter::kinds(["*"])), 3);
        assert_eq!(n(EventFilter::kinds(["call.dtmf", "account.*"])), 2);
        assert_eq!(n(EventFilter::kinds(Vec::<String>::new())), 3);
        let f = EventFilter {
            account: Some("a1".into()),
            ..Default::default()
        };
        assert_eq!(n(f), 2);
        let f = EventFilter {
            kinds: Some(vec!["call.*".into()]),
            account: Some("a1".into()),
            call_id: Some("c1".into()),
        };
        assert_eq!(n(f), 1);
        let f = EventFilter {
            call_id: Some("c2".into()),
            ..Default::default()
        };
        assert_eq!(log.since(0, &f, 100)[0].kind, "call.dtmf");
    }

    #[tokio::test]
    async fn subscribe_streams_and_filters() {
        let log = EventLog::new(100);
        let mut sub = log.subscribe(EventFilter::kinds(["call.*"]));
        emit(&log, "account.x");
        emit(&log, "call.state");
        let e = sub.next().await.unwrap();
        assert_eq!((e.seq, e.kind.as_str()), (2, "call.state"));
    }

    #[tokio::test]
    async fn subscribe_lag_yields_marker_then_continues() {
        let log = EventLog::new(10); // broadcast capacity clamps to 16
        let mut sub = log.subscribe(EventFilter::any());
        for _ in 0..40 {
            emit(&log, "x");
        }
        let m = sub.next().await.unwrap();
        assert_eq!(m.kind, LAGGED_KIND);
        assert_eq!(m.seq, 0);
        assert!(m.data["missed"].as_u64().unwrap() >= 24);
        let e = sub.next().await.unwrap();
        assert_eq!(e.kind, "x");
        assert!(e.seq > 1);
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn event_serde_roundtrip() {
        let log = EventLog::new(4);
        let data = json!({"state":"active","mos":4.2133333333333335,"jitter_ms":0.1,"x":-29.612384796142578});
        log.emit("call.state", Some("a"), Some("c1"), data.clone());
        let e = log.since(0, &EventFilter::any(), 1).remove(0);
        // The log stores the payload as text: floats must survive it bit for bit.
        assert_eq!(e.data, data);
        let s = serde_json::to_string(&e).unwrap();
        // Needs serde_json `float_roundtrip`: exact float equality after the round trip.
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
        let bare = EventLog::new(4);
        bare.emit("x", None, None, Value::Null);
        let s = serde_json::to_string(&bare.since(0, &EventFilter::any(), 1)[0]).unwrap();
        assert!(!s.contains("account") && !s.contains("call_id"));
    }

    #[test]
    fn byte_cap_bounds_ring_and_keeps_since_semantics() {
        let cap = 1024 * 1024;
        let log = EventLog::with_limits(100_000, cap);
        let big = json!({"pad": "x".repeat(600), "n": [1, 2, 3]});
        for i in 0..20_000u64 {
            let mut d = big.clone();
            d["i"] = json!(i);
            log.emit("call.state", Some("a"), Some("c1"), d);
        }
        assert!(log.retained_bytes() <= cap, "{}", log.retained_bytes());
        let all = log.since(0, &EventFilter::any(), usize::MAX);
        assert!(!all.is_empty() && all.len() < 20_000);
        assert_eq!(all.last().unwrap().seq, 20_000);
        // since older than the oldest retained: truncated, returns the retained tail.
        let r = log.scan(0, &EventFilter::any(), 10);
        assert!(r.truncated);
        assert_eq!(r.events[0].seq, all[0].seq);
        // since a recent seq: exact events, payload intact, not truncated.
        let r = log.scan(19_995, &EventFilter::any(), 100);
        assert!(!r.truncated);
        assert_eq!(
            r.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [19_996, 19_997, 19_998, 19_999, 20_000]
        );
        assert_eq!(r.events[0].data["i"], 19_995);
        assert_eq!(r.events[0].data["pad"].as_str().unwrap().len(), 600);
        assert_eq!(r.next_seq, 20_000);
    }

    #[test]
    fn oversized_single_event_is_kept() {
        let log = EventLog::with_limits(10, 100);
        log.emit("x", None, None, json!({"pad": "y".repeat(1000)}));
        assert_eq!(log.since(0, &EventFilter::any(), 10).len(), 1);
    }
}
