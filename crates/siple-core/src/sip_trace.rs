//! Per-account SIP wire trace: a [`WireSink`] writing daily-rolling files and keeping
//! an in-memory per-Call-ID ring for the `call.sip_trace` API.
//!
//! Files: `<log_dir>/sip/<account_id>/sip.<YYYY-MM-DD>.log` (own directory per account, so one
//! account's retention pruning never sees another's files). Record format:
//!
//! ```text
//! ===== 2026-10-08T10:15:02.123Z SEND tls 10.0.0.5:52311 -> 203.0.113.10:5061 (1234 bytes)
//! INVITE sip:1002@pbx.example.com SIP/2.0
//! ...
//!
//! ===== 2026-10-08T10:15:32.001Z SEND tcp 10.0.0.5:52311 -> 203.0.113.10:5060 (4 bytes) keep-alive CRLFCRLF
//! ```
//!
//! The arrow always points in the direction the bytes travelled (`local -> remote` for SEND,
//! `remote -> local` for RECV). Retransmissions and unparsable input are traced like anything
//! else; nothing is redacted unless `[logging] redact_credentials` / `redact_srtp_keys` ask for it
//! (see [`crate::redact`]). Whether an account is traced at all is decided by the SIP stack,
//! which only installs the tap while `sip_trace` is on.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use siple_sip::{WireDirection, WireEvent, WireSink};
use tracing_appender::non_blocking::{ErrorCounter, NonBlocking, WorkerGuard};

use crate::logging::rolling_appender;
use crate::redact::Redactor;

/// File name prefix inside the per-account directory (`sip.<date>.log`).
pub const TRACE_PREFIX: &str = "sip";
/// Messages kept in the in-memory Call-ID ring, across all accounts and calls.
pub const RING_CAPACITY: usize = 500;

struct AccountFile {
    writer: NonBlocking,
    drops: DropTracker,
    _guard: WorkerGuard,
}

/// Minimum spacing of the "trace records dropped" WARN per account.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(10);

/// Turns the writer's cumulative dropped-line counter into per-write deltas and decides when to
/// WARN (at most once per [`DROP_WARN_INTERVAL`]).
struct DropTracker {
    counter: ErrorCounter,
    seen: usize,
    last_warn: Option<Instant>,
}

impl DropTracker {
    fn new(counter: ErrorCounter) -> Self {
        Self {
            counter,
            seen: 0,
            last_warn: None,
        }
    }

    /// Lines dropped since the last acknowledged marker, with the current counter value.
    fn pending(&self) -> Option<(usize, usize)> {
        let now = self.counter.dropped_lines();
        (now > self.seen).then(|| (now - self.seen, now))
    }

    /// Whether a WARN is due now (and records it).
    fn warn_due(&mut self, now: Instant) -> bool {
        if self
            .last_warn
            .is_some_and(|t| now.duration_since(t) < DROP_WARN_INTERVAL)
        {
            return false;
        }
        self.last_warn = Some(now);
        true
    }
}

/// The in-file marker for `n` dropped records.
fn drop_marker(timestamp: &str, n: usize) -> String {
    format!("===== {timestamp} DROPPED {n} SIP records (writer backlog) =====\n")
}

impl AccountFile {
    /// Writes `record`; when the lossy writer dropped lines since the last marker, a marker goes
    /// into the trace first and one WARN is logged (rate-limited).
    fn write_tracked(&mut self, account: &str, ts: &str, record: &str) {
        use std::io::Write;
        if let Some((n, total)) = self.drops.pending() {
            let _ = self.writer.write_all(drop_marker(ts, n).as_bytes());
            // A marker that was itself dropped stays pending (the next write retries it), but its
            // own drop is not a lost SIP record: skip it so the next marker's count stays exact.
            let now = self.drops.counter.dropped_lines();
            if now == total {
                self.drops.seen = total;
            } else if now == total + 1 {
                self.drops.seen += 1;
            }
            if self.drops.warn_due(Instant::now()) {
                tracing::warn!(%account, dropped = n, "SIP trace writer backlog: records dropped from the trace file");
            }
        }
        // One write per record keeps records contiguous; the non-blocking writer never blocks.
        let _ = self.writer.write_all(record.as_bytes());
    }
}

/// See the module docs.
pub struct SipTraceWriter {
    sip_dir: PathBuf,
    retention_days: u32,
    files: Mutex<HashMap<String, Option<AccountFile>>>,
    ring: Mutex<VecDeque<(String, String)>>,
    redactor: std::sync::Arc<Redactor>,
}

impl SipTraceWriter {
    /// A writer rooted at `<log_dir>/sip`; keeps `retention_days` daily files per account
    /// (0 = unlimited).
    pub fn new(log_dir: impl Into<PathBuf>, retention_days: u32) -> Self {
        Self {
            sip_dir: log_dir.into().join("sip"),
            retention_days,
            files: Mutex::default(),
            ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
            redactor: Redactor::new(),
        }
    }

    /// Uses `redactor` (shared with the application log) to mask each record before it is
    /// written or kept.
    pub fn with_redactor(mut self, redactor: std::sync::Arc<Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    /// Directory holding one account's trace files.
    pub fn account_dir(&self, account: &str) -> PathBuf {
        self.sip_dir.join(sanitize(account))
    }

    /// Every traced message of `call_id` still in the ring, oldest first, each as the full record
    /// (header line plus message) exactly as written to the file.
    pub fn messages_for_call(&self, call_id: &str) -> Vec<String> {
        let ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        ring.iter()
            .filter(|(id, _)| id == call_id)
            .map(|(_, rec)| rec.clone())
            .collect()
    }

    fn write_file(&self, account: &str, ts: &str, record: &str) {
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = files.entry(account.to_string()).or_insert_with(|| {
            match rolling_appender(&self.account_dir(account), TRACE_PREFIX, self.retention_days) {
                Ok(appender) => {
                    let (writer, _guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
                        .buffered_lines_limit(TRACE_BUFFERED_LINES)
                        .lossy(true)
                        .finish(appender);
                    let drops = DropTracker::new(writer.error_counter());
                    Some(AccountFile {
                        writer,
                        drops,
                        _guard,
                    })
                }
                Err(e) => {
                    tracing::warn!(%account, error = %e, "cannot open SIP trace file; trace for this account is dropped");
                    None
                }
            }
        });
        if let Some(f) = entry {
            f.write_tracked(account, ts, record);
        }
    }
}

impl WireSink for SipTraceWriter {
    fn on_wire(&self, account: &str, ev: &WireEvent<'_>) {
        let ts = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let record = format_record(&ts, ev);
        let masked = match self.redactor.apply(&record) {
            std::borrow::Cow::Owned(m) => Some(m),
            std::borrow::Cow::Borrowed(_) => None,
        };
        let record = masked.unwrap_or(record);
        if let Some(call_id) = call_id_of(ev.bytes) {
            let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
            if ring.len() == RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back((call_id, record.clone()));
        }
        self.write_file(account, &ts, &record);
    }
}

/// Per-account writer queue depth (lines, about 4x smaller than the tracing-appender default);
/// lossy so the SIP stack never blocks on disk. Drops are visible as `DROPPED` markers.
const TRACE_BUFFERED_LINES: usize = 16384;

fn sanitize(account: &str) -> String {
    account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A keep-alive is nothing but CR/LF bytes (RFC 5626 ping `CRLFCRLF` / pong `CRLF`).
fn keepalive_kind(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        b"\r\n\r\n" => Some("CRLFCRLF"),
        b"\r\n" => Some("CRLF"),
        b if !b.is_empty() && b.iter().all(|c| matches!(c, b'\r' | b'\n')) => Some("CRLF*"),
        _ => None,
    }
}

/// One trace record (header line plus message, terminated by a blank line).
pub fn format_record(timestamp: &str, ev: &WireEvent<'_>) -> String {
    let (dir, from, to) = match ev.direction {
        WireDirection::Send => ("SEND", ev.local, ev.remote),
        WireDirection::Recv => ("RECV", ev.remote, ev.local),
    };
    let head = format!(
        "===== {timestamp} {dir} {} {from} -> {to} ({} bytes)",
        ev.transport.to_string().to_ascii_lowercase(),
        ev.bytes.len()
    );
    match keepalive_kind(ev.bytes) {
        Some(kind) => format!("{head} keep-alive {kind}\n"),
        None => {
            let text = String::from_utf8_lossy(ev.bytes);
            let sep = if text.ends_with('\n') { "" } else { "\n" };
            format!("{head}\n{text}{sep}\n")
        }
    }
}

/// `Call-ID` (or compact `i`) header of a SIP message in `bytes`.
pub fn call_id_of(bytes: &[u8]) -> Option<String> {
    let head_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(bytes.len());
    // Lossy: a stray non-UTF-8 byte (e.g. in a display name) must not drop the message.
    let head = String::from_utf8_lossy(&bytes[..head_end]);
    head.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let name = name.trim();
        (name.eq_ignore_ascii_case("call-id") || name.eq_ignore_ascii_case("i"))
            .then(|| value.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use siple_sip::WireTransport;
    use std::net::SocketAddr;

    fn ev<'a>(
        direction: WireDirection,
        transport: WireTransport,
        bytes: &'a [u8],
    ) -> WireEvent<'a> {
        WireEvent {
            direction,
            transport,
            local: "10.0.0.5:52311".parse::<SocketAddr>().unwrap(),
            remote: "203.0.113.10:5061".parse::<SocketAddr>().unwrap(),
            bytes,
        }
    }

    fn msg(call_id: &str, extra: &str) -> Vec<u8> {
        format!("OPTIONS sip:x@y SIP/2.0\r\nVia: SIP/2.0/UDP a\r\nCall-ID: {call_id}\r\nCSeq: 1 OPTIONS\r\n{extra}Content-Length: 0\r\n\r\n").into_bytes()
    }

    #[test]
    fn record_format_send_recv_keepalive() {
        let m = msg("abc@h", "");
        let send = format_record(
            "2026-10-08T10:15:02.123Z",
            &ev(WireDirection::Send, WireTransport::Tls, &m),
        );
        assert!(send.starts_with(&format!("===== 2026-10-08T10:15:02.123Z SEND tls 10.0.0.5:52311 -> 203.0.113.10:5061 ({} bytes)\nOPTIONS sip:x@y SIP/2.0\r\n", m.len())), "{send}");
        assert!(send.ends_with("\r\n\r\n\n"), "{send:?}");
        let recv = format_record("T", &ev(WireDirection::Recv, WireTransport::Udp, &m));
        assert!(
            recv.starts_with("===== T RECV udp 203.0.113.10:5061 -> 10.0.0.5:52311 ("),
            "{recv}"
        );
        let ka = format_record(
            "T",
            &ev(WireDirection::Send, WireTransport::Tcp, b"\r\n\r\n"),
        );
        assert_eq!(
            ka,
            "===== T SEND tcp 10.0.0.5:52311 -> 203.0.113.10:5061 (4 bytes) keep-alive CRLFCRLF\n"
        );
        let pong = format_record("T", &ev(WireDirection::Recv, WireTransport::Tcp, b"\r\n"));
        assert!(pong.ends_with("(2 bytes) keep-alive CRLF\n"), "{pong}");
    }

    #[test]
    fn call_id_parsing() {
        assert_eq!(call_id_of(&msg("abc@h", "")).as_deref(), Some("abc@h"));
        assert_eq!(
            call_id_of(b"SIP/2.0 200 OK\r\nv: SIP/2.0/UDP a\r\ni: compact@x\r\n\r\n").as_deref(),
            Some("compact@x")
        );
        assert_eq!(
            call_id_of(b"SIP/2.0 200 OK\r\ncall-id:  lower@x \r\n\r\nCall-ID: body-not-header\r\n")
                .as_deref(),
            Some("lower@x")
        );
        assert_eq!(call_id_of(b"\r\n\r\n"), None);
        assert_eq!(call_id_of(b"garbage \xff\xfe"), None);
        // Non-UTF-8 bytes elsewhere in the head (e.g. a display name) keep the Call-ID.
        assert_eq!(
            call_id_of(
                b"MESSAGE sip:x@y SIP/2.0\r\nFrom: \"\xe9\" <sip:a@b>\r\nCall-ID: lat1@x\r\n\r\n"
            )
            .as_deref(),
            Some("lat1@x")
        );
    }

    #[test]
    fn writes_per_account_daily_files_and_ring() {
        let dir = tempfile::tempdir().unwrap();
        let w = SipTraceWriter::new(dir.path(), 14);
        let m1 = msg("call-1", "");
        let m2 = msg("call-2", "");
        w.on_wire("acct1", &ev(WireDirection::Send, WireTransport::Udp, &m1));
        w.on_wire("acct1", &ev(WireDirection::Recv, WireTransport::Udp, &m2));
        w.on_wire(
            "acct1",
            &ev(WireDirection::Send, WireTransport::Tcp, b"\r\n\r\n"),
        );
        w.on_wire("acct2", &ev(WireDirection::Send, WireTransport::Udp, &m1));

        let calls = w.messages_for_call("call-1");
        assert_eq!(calls.len(), 2, "both accounts' copies of call-1");
        assert!(calls[0].contains(" SEND udp "));
        assert_eq!(w.messages_for_call("call-2").len(), 1);
        assert!(w.messages_for_call("nope").is_empty());

        let a1 = w.account_dir("acct1");
        drop(w); // flushes the non-blocking writers
        let read = |d: &std::path::Path| {
            let files: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(files.len(), 1, "{files:?}");
            assert!(
                files[0].starts_with("sip.") && files[0].ends_with(".log"),
                "{files:?}"
            );
            std::fs::read_to_string(d.join(&files[0])).unwrap()
        };
        let text = read(&a1);
        let heads: Vec<&str> = text.lines().filter(|l| l.starts_with("=====")).collect();
        assert_eq!(heads.len(), 3, "{text}");
        assert!(heads[2].ends_with("keep-alive CRLFCRLF"));
        assert!(text.contains("Call-ID: call-1") && text.contains("Call-ID: call-2"));
        let other = read(&dir.path().join("sip").join("acct2"));
        assert_eq!(other.matches("=====").count(), 1);
    }

    #[test]
    fn ring_is_bounded_and_evicts_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let w = SipTraceWriter::new(dir.path(), 14);
        for i in 0..(RING_CAPACITY + 10) {
            let m = msg(&format!("c{i}"), "");
            w.on_wire("a", &ev(WireDirection::Send, WireTransport::Udp, &m));
        }
        assert!(w.messages_for_call("c0").is_empty(), "oldest evicted");
        assert!(w.messages_for_call("c9").is_empty());
        assert_eq!(w.messages_for_call("c10").len(), 1);
        assert_eq!(
            w.messages_for_call(&format!("c{}", RING_CAPACITY + 9))
                .len(),
            1
        );
        let total: usize = (0..RING_CAPACITY + 10)
            .map(|i| w.messages_for_call(&format!("c{i}")).len())
            .sum();
        assert_eq!(total, RING_CAPACITY);
    }

    #[test]
    fn unwritable_dir_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("afile");
        std::fs::write(&file, "x").unwrap();
        let w = SipTraceWriter::new(&file, 14); // `<file>/sip/...` can never be created
        let m = msg("c", "");
        w.on_wire("a", &ev(WireDirection::Send, WireTransport::Udp, &m));
        assert_eq!(w.messages_for_call("c").len(), 1, "ring still works");
    }

    #[test]
    fn account_ids_are_path_safe() {
        let w = SipTraceWriter::new("/tmp/x", 1);
        assert!(w.account_dir("../evil/id").ends_with("sip/.._evil_id"));
    }

    #[test]
    fn drop_marker_format() {
        assert_eq!(
            drop_marker("2026-10-09T10:00:00.000Z", 7),
            "===== 2026-10-09T10:00:00.000Z DROPPED 7 SIP records (writer backlog) =====\n"
        );
    }

    #[test]
    fn warn_is_rate_limited() {
        let (w, _g) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(true)
            .finish(std::io::sink());
        let mut t = DropTracker::new(w.error_counter());
        let t0 = Instant::now();
        assert!(t.warn_due(t0));
        assert!(!t.warn_due(t0 + Duration::from_secs(9)));
        assert!(t.warn_due(t0 + Duration::from_secs(10)));
    }

    /// A slow disk overflows the queue: a marker with the lost count lands in the trace.
    #[test]
    fn slow_writer_overflow_writes_marker() {
        use std::io::Write;
        use std::sync::Arc;

        #[derive(Clone)]
        struct Slow(Arc<Mutex<Vec<u8>>>);
        impl Write for Slow {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(20));
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let out = Arc::new(Mutex::new(Vec::new()));
        let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .buffered_lines_limit(2)
            .lossy(true)
            .finish(Slow(out.clone()));
        let mut f = AccountFile {
            drops: DropTracker::new(writer.error_counter()),
            writer,
            _guard: guard,
        };
        for n in 0..40 {
            f.write_tracked("a", "T", &format!("rec{n}\n"));
        }
        let dropped = f.drops.counter.dropped_lines();
        assert!(dropped > 0, "queue of 2 must overflow");
        std::thread::sleep(Duration::from_millis(500));
        f.write_tracked("a", "T", "last\n");
        drop(f);
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        assert!(text.contains("DROPPED"), "{text}");
        assert!(text.contains("SIP records (writer backlog)"), "{text}");
        assert!(text.contains("last\n"));
        // Every lost SIP record is reported exactly once, even when a marker itself was dropped.
        let kept = text.lines().filter(|l| l.starts_with("rec")).count()
            + usize::from(text.contains("last\n"));
        let reported: usize = text
            .lines()
            .filter_map(|l| l.split(" DROPPED ").nth(1))
            .map(|r| r.split(' ').next().unwrap().parse::<usize>().unwrap())
            .sum();
        assert_eq!(reported, 41 - kept, "{text}");
    }
}
