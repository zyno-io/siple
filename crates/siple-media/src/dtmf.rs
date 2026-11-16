//! RFC 4733 telephone-event detection and generation, inband DTMF tones, and
//! SIP INFO DTMF body parsing.

use std::time::{Duration, Instant};

use crate::tone::{Segment, ToneGenerator, ToneSpec};

/// DTMF event timeout: if no end-bit arrives within this duration, emit the event anyway.
const DTMF_TIMEOUT: Duration = Duration::from_secs(3);

/// Packets up to this many seconds behind the last event (RTP clock) are
/// treated as reordered stragglers; further back starts a new timeline.
const TIMELINE_WINDOW_SECS: i64 = 10;

/// RFC 4733 telephone-event payload format:
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |     event     |E|R| volume    |          duration             |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
/// A completed DTMF event
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtmfEvent {
    pub digit: char,
    pub duration_ms: u32,
    pub volume: u8,
}

/// Detects DTMF events from RFC 4733 telephone-event RTP payloads.
/// Tracks state across multiple packets for a single event (start → end).
#[derive(Debug, Default)]
pub struct DtmfDetector {
    /// Current event being assembled (event_id, volume, last_duration, rtp_timestamp, wall_clock_start)
    current: Option<(u8, u8, u16, u32, Instant)>,
    /// Last completed event (event_id, rtp_timestamp) to suppress RFC 4733 redundant end-bit packets
    last_completed: Option<(u8, u32)>,
}

impl DtmfDetector {
    pub fn new() -> Self {
        Self {
            current: None,
            last_completed: None,
        }
    }

    /// Forget all event state. The engine should call this when the inbound
    /// SSRC changes (a new source has an unrelated timestamp base).
    pub fn reset(&mut self) {
        self.current = None;
        self.last_completed = None;
    }

    /// Process an RFC 4733 telephone-event RTP payload.
    /// Returns Some(DtmfEvent) when the end bit is set (event complete).
    ///
    /// `payload` is the RTP payload (4 bytes), `timestamp` is the RTP timestamp.
    /// `clock_rate` is the RTP clock rate for duration calculation.
    pub fn process(
        &mut self,
        payload: &[u8],
        timestamp: u32,
        clock_rate: u32,
    ) -> Option<DtmfEvent> {
        if payload.len() < 4 {
            return None;
        }

        let event_id = payload[0];
        let end_bit = (payload[1] >> 7) & 1 != 0;
        let volume = payload[1] & 0x3F;
        let duration = u16::from_be_bytes([payload[2], payload[3]]);

        let key = (event_id, timestamp);

        // Packets of an event that already completed (late non-end packets,
        // reordered or redundant end copies) must never reopen it.
        if self.last_completed == Some(key) {
            return None;
        }
        // Packets slightly older (RTP-timestamp-wise) than the current or last
        // completed event are reordered stragglers of a previous digit. A
        // packet far behind (> TIMELINE_WINDOW_SECS) means the sender's
        // timestamp base changed (source switch with the same SSRC): start a
        // new timeline instead of dropping every later digit.
        let window = TIMELINE_WINDOW_SECS * clock_rate.max(1) as i64;
        let behind = |reference: u32| reference.wrapping_sub(timestamp) as i32 as i64;
        let refs = [
            self.current.map(|c| c.3),
            self.last_completed.map(|(_, ts)| ts),
        ];
        let mut flushed = None;
        if refs.iter().flatten().any(|&r| behind(r) > window) {
            flushed = self.take_current(clock_rate);
            self.last_completed = None;
        } else if refs.iter().flatten().any(|&r| behind(r) > 0) {
            return None;
        }

        // Is the in-progress event the same event (same id AND timestamp)?
        let same_event = self
            .current
            .is_some_and(|c| c.0 == event_id && c.3 == timestamp);

        // Close the in-progress event if it timed out, or if this packet
        // belongs to a different event (new timestamp and/or id => the end
        // bits of the old one were lost).
        let timed_out = self.is_timed_out();
        let closed = if self.current.is_some() && (!same_event || timed_out) {
            self.take_current(clock_rate)
        } else {
            None
        }
        .or(flushed);
        if same_event && timed_out {
            // Stale event just emitted (and remembered as completed): do not
            // reopen it for this late packet.
            return closed;
        }

        self.current = Some((event_id, volume, duration, timestamp, Instant::now()));

        if end_bit {
            if closed.is_some() {
                // Prioritize the closed event. The new digit stays in
                // self.current and will be completed by a redundant end-bit
                // copy (RFC 4733 mandates 3 copies).
                return closed;
            }
            self.current = None;
            self.last_completed = Some(key);
            let digit = event_id_to_digit(event_id)?;
            return Some(DtmfEvent {
                digit,
                duration_ms: ticks_to_ms(duration, clock_rate),
                volume,
            });
        }

        closed
    }

    /// Check for timed-out DTMF events. Call this periodically (e.g., every 20ms)
    /// even when no new packets arrive, to detect events whose end-bit was lost.
    pub fn check_timeout(&mut self, clock_rate: u32) -> Option<DtmfEvent> {
        self.drain_if_timed_out(clock_rate)
    }

    /// Returns true if the detector has an active (in-progress) digit.
    pub fn has_active_digit(&self) -> bool {
        self.current.is_some()
    }

    fn is_timed_out(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|c| c.4.elapsed() > DTMF_TIMEOUT)
    }

    /// Emit and clear the in-progress event, remembering it as completed so
    /// that straggler packets cannot reopen it.
    fn take_current(&mut self, clock_rate: u32) -> Option<DtmfEvent> {
        let (id, vol, dur, ts, _) = self.current.take()?;
        self.last_completed = Some((id, ts));
        event_id_to_digit(id).map(|d| DtmfEvent {
            digit: d,
            duration_ms: ticks_to_ms(dur, clock_rate),
            volume: vol,
        })
    }

    /// If the current event has been alive longer than DTMF_TIMEOUT, emit it and clear state.
    fn drain_if_timed_out(&mut self, clock_rate: u32) -> Option<DtmfEvent> {
        if self.is_timed_out() {
            self.take_current(clock_rate)
        } else {
            None
        }
    }
}

/// Convert RFC 4733 duration ticks to milliseconds (0 clock rate => 8000 Hz).
fn ticks_to_ms(ticks: u16, clock_rate: u32) -> u32 {
    let rate = if clock_rate > 0 { clock_rate } else { 8000 };
    ((ticks as u64 * 1000) / rate as u64) as u32
}

/// Longest telephone-event [`DtmfGenerator::generate`] accepts, in ms.
pub const MAX_EVENT_DURATION_MS: u32 = 10_000;

/// Generates the packet schedule for one RFC 4733 telephone-event.
///
/// This type performs no pacing: it only describes *what* to send and *when*
/// (relative to the start of the event). The caller owns the clock and the RTP
/// sequence numbers.
pub struct DtmfGenerator;

impl DtmfGenerator {
    /// Generate the telephone-event packets for one digit.
    ///
    /// RFC 4733 rules applied:
    /// - every packet of the event carries the **same RTP timestamp** (the
    ///   event start); the growing `duration` field is inside the payload;
    /// - the marker bit is set on the **first packet only**;
    /// - the final (end) packet is sent **3 times**, spaced one `ptime` apart.
    ///
    /// After the event, the caller resumes audio RTP timestamps from
    /// `event_start_ts + duration ticks (+ end-packet spacing)` as appropriate
    /// for its own timeline.
    ///
    /// - `duration_ms`: total event duration, `1..=MAX_EVENT_DURATION_MS`
    ///   (10 000); anything else returns `None`. The 16-bit RFC 4733 duration
    ///   field also caps the event at 65 535 ticks of `clock_rate` (about
    ///   8.19 s at 8 kHz, 1.36 s at 48 kHz); longer requests are **capped** to
    ///   that (RFC 4733 §2.5.1.3 long-event segmentation is deliberately not
    ///   implemented: segments need new timestamps that receivers cannot
    ///   distinguish from a new digit without the sender's full state)
    /// - `volume`: power level in -dBm0, 0..=63 (lower = louder; ~10 typical)
    /// - `clock_rate`: RTP clock rate of the negotiated telephone-event PT
    /// - `ptime_ms`: packet interval (typically 20)
    ///
    /// Returns `None` for an unknown digit, zero `ptime_ms`/`clock_rate`, or
    /// `duration_ms` of 0 or above [`MAX_EVENT_DURATION_MS`].
    pub fn generate(
        digit: char,
        duration_ms: u32,
        volume: u8,
        clock_rate: u32,
        ptime_ms: u32,
    ) -> Option<Vec<DtmfPacket>> {
        let event_id = digit_to_event_id(digit)?;

        if ptime_ms == 0
            || clock_rate == 0
            || duration_ms == 0
            || duration_ms > MAX_EVENT_DURATION_MS
        {
            return None;
        }

        // Cap so the total duration fits the 16-bit field.
        let max_ticks = u16::MAX as u64;
        let mut total = (duration_ms as u64 * clock_rate as u64) / 1000;
        let mut duration_ms = duration_ms;
        if total > max_ticks {
            total = max_ticks;
            duration_ms = ((max_ticks * 1000) / clock_rate as u64) as u32;
        }
        if total == 0 {
            return None;
        }
        let total_duration_ticks = total as u32;
        let ptime_ticks =
            ((ptime_ms as u64 * clock_rate as u64) / 1000).min(u32::MAX as u64) as u32;
        let num_packets = duration_ms.div_ceil(ptime_ms).max(1);

        let mut packets = Vec::new();

        for i in 0..num_packets {
            let is_first = i == 0;
            let is_last = i == num_packets - 1;
            let current_duration = if is_last {
                total_duration_ticks.min(u16::MAX as u32) as u16
            } else {
                (((i as u64 + 1) * ptime_ticks as u64).min(u16::MAX as u64)) as u16
            };

            // For end events, send 3 times (RFC 4733 section 2.5.1.4)
            if is_last {
                for rep in 0..3u32 {
                    packets.push(DtmfPacket {
                        payload: build_telephone_event(event_id, true, volume, current_duration),
                        marker: is_first && rep == 0,
                        is_end: true,
                        ts_offset: (i + rep).saturating_mul(ptime_ticks),
                        send_offset_ms: (i + rep).saturating_mul(ptime_ms),
                    });
                }
            } else {
                packets.push(DtmfPacket {
                    payload: build_telephone_event(event_id, false, volume, current_duration),
                    marker: is_first,
                    is_end: false,
                    ts_offset: i.saturating_mul(ptime_ticks),
                    send_offset_ms: i.saturating_mul(ptime_ms),
                });
            }
        }

        Some(packets)
    }
}

/// A single DTMF RTP packet in a [`DtmfGenerator`] schedule.
#[derive(Debug, Clone)]
pub struct DtmfPacket {
    /// 4-byte RFC 4733 telephone-event payload.
    pub payload: Vec<u8>,
    /// RTP marker bit (first packet of the event only).
    pub marker: bool,
    /// End-of-event (E) bit set; true for the 3 final retransmissions.
    pub is_end: bool,
    /// Send time relative to event start, in RTP clock ticks of the
    /// telephone-event clock. This is a **pacing** offset: the RTP header
    /// timestamp of every packet in the event is the event-start timestamp.
    pub ts_offset: u32,
    /// Send time relative to event start, in milliseconds.
    pub send_offset_ms: u32,
}

/// Build a 4-byte telephone-event payload
fn build_telephone_event(event_id: u8, end: bool, volume: u8, duration: u16) -> Vec<u8> {
    let mut buf = vec![0u8; 4];
    buf[0] = event_id;
    buf[1] = if end { 0x80 } else { 0x00 } | (volume & 0x3F);
    buf[2] = (duration >> 8) as u8;
    buf[3] = (duration & 0xFF) as u8;
    buf
}

/// Map event ID (0-15) to DTMF digit character
fn event_id_to_digit(id: u8) -> Option<char> {
    match id {
        0 => Some('0'),
        1 => Some('1'),
        2 => Some('2'),
        3 => Some('3'),
        4 => Some('4'),
        5 => Some('5'),
        6 => Some('6'),
        7 => Some('7'),
        8 => Some('8'),
        9 => Some('9'),
        10 => Some('*'),
        11 => Some('#'),
        12 => Some('A'),
        13 => Some('B'),
        14 => Some('C'),
        15 => Some('D'),
        _ => None,
    }
}

/// Map DTMF digit character to event ID (0-15)
fn digit_to_event_id(digit: char) -> Option<u8> {
    match digit {
        '0' => Some(0),
        '1' => Some(1),
        '2' => Some(2),
        '3' => Some(3),
        '4' => Some(4),
        '5' => Some(5),
        '6' => Some(6),
        '7' => Some(7),
        '8' => Some(8),
        '9' => Some(9),
        '*' => Some(10),
        '#' => Some(11),
        'A' | 'a' => Some(12),
        'B' | 'b' => Some(13),
        'C' | 'c' => Some(14),
        'D' | 'd' => Some(15),
        _ => None,
    }
}

/// Rescale the RFC 4733 duration field of a telephone-event payload from
/// `src_clock` to `dst_clock`, in place. The duration is carried in clock
/// ticks (bytes 2–3, big-endian), so forwarding DTMF between two legs whose
/// telephone-event clocks differ (e.g. WebRTC 48000 ↔ SIP 8000) requires
/// rescaling — otherwise the far end renders the wrong digit length. Event id,
/// E/R bits, and volume are left untouched. No-op when the clocks match, either
/// clock is zero, or the payload is shorter than 4 bytes.
pub fn rescale_event_duration(payload: &mut [u8], src_clock: u32, dst_clock: u32) {
    if payload.len() < 4 || src_clock == dst_clock || src_clock == 0 || dst_clock == 0 {
        return;
    }
    let duration = u16::from_be_bytes([payload[2], payload[3]]) as u64;
    let scaled = (duration * dst_clock as u64 / src_clock as u64).min(u16::MAX as u64) as u16;
    let bytes = scaled.to_be_bytes();
    payload[2] = bytes[0];
    payload[3] = bytes[1];
}

/// Check if an RTP payload type is a telephone-event PT
pub fn is_telephone_event_pt(pt: u8, negotiated_te_pt: Option<u8>) -> bool {
    if let Some(te_pt) = negotiated_te_pt {
        pt == te_pt
    } else {
        // Common default PTs for telephone-event
        pt == 101 || pt == 96 || pt == 126
    }
}

/// Standard DTMF (row, column) frequencies in Hz for a digit
/// (`0-9 * # A-D`, case-insensitive for A-D).
pub fn dtmf_frequencies(digit: char) -> Option<(f32, f32)> {
    const ROWS: [f32; 4] = [697.0, 770.0, 852.0, 941.0];
    const COLS: [f32; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
    let (r, c) = match digit.to_ascii_uppercase() {
        '1' => (0, 0),
        '2' => (0, 1),
        '3' => (0, 2),
        'A' => (0, 3),
        '4' => (1, 0),
        '5' => (1, 1),
        '6' => (1, 2),
        'B' => (1, 3),
        '7' => (2, 0),
        '8' => (2, 1),
        '9' => (2, 2),
        'C' => (2, 3),
        '*' => (3, 0),
        '0' => (3, 1),
        '#' => (3, 2),
        'D' => (3, 3),
        _ => return None,
    };
    Some((ROWS[r], COLS[c]))
}

/// Inband DTMF tone generator: dual tone at 48 kHz mono `i16`, with short
/// edge ramps. Streamable via [`ToneGenerator::fill`]; use
/// [`inband_dtmf_samples`] for a whole digit at once.
pub fn inband_dtmf_generator(digit: char, duration_ms: u32) -> Option<ToneGenerator> {
    let (lo, hi) = dtmf_frequencies(digit)?;
    if duration_ms == 0 {
        return None;
    }
    Some(ToneGenerator::new(
        ToneSpec {
            freqs: [lo, hi],
            segments: vec![Segment {
                on: true,
                ms: duration_ms,
            }],
            repeat: false,
        },
        DTMF_TONE_AMPLITUDE,
    ))
}

/// Peak amplitude of each of the two DTMF tones (about -12 dBFS each).
pub const DTMF_TONE_AMPLITUDE: i16 = 8000;

/// Render one inband DTMF digit as 48 kHz mono `i16` samples.
pub fn inband_dtmf_samples(digit: char, duration_ms: u32) -> Option<Vec<i16>> {
    let mut g = inband_dtmf_generator(digit, duration_ms)?;
    let n = (crate::tone::TONE_SAMPLE_RATE as u64 * duration_ms as u64 / 1000) as usize;
    Some(g.next_frame(n))
}

/// A DTMF digit received in a SIP INFO request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoDtmf {
    pub digit: char,
    /// Duration in milliseconds, if the body carried one.
    pub duration_ms: Option<u32>,
}

/// Parse a SIP INFO body carrying DTMF.
///
/// - `application/dtmf-relay`: `Signal=5\r\nDuration=160` (Signal may also be
///   `*`, `#`, `A-D`, or the numeric event 10-15; Duration optional).
/// - `application/dtmf`: a bare digit, optionally with `Duration=` lines too.
///
/// The content type is matched loosely (case-insensitive, parameters ignored).
/// Returns `None` if the content type is not a DTMF type or no valid digit is found.
pub fn parse_info_dtmf(content_type: &str, body: &str) -> Option<InfoDtmf> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "application/dtmf-relay" | "application/dtmf" => {}
        _ => return None,
    }
    let mut digit = None;
    let mut duration_ms = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            match k.as_str() {
                "signal" => digit = parse_info_signal(v),
                "duration" => duration_ms = v.parse::<u32>().ok(),
                _ => {}
            }
        } else if digit.is_none() && ct == "application/dtmf" {
            digit = parse_info_signal(line);
        }
    }
    Some(InfoDtmf {
        digit: digit?,
        duration_ms,
    })
}

fn parse_info_signal(v: &str) -> Option<char> {
    let mut chars = v.chars();
    let c = chars.next()?;
    if chars.next().is_none() {
        let up = c.to_ascii_uppercase();
        return digit_to_event_id(up).map(|_| up);
    }
    // numeric event id, e.g. "10" for '*'
    v.parse::<u8>().ok().and_then(event_id_to_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_dtmf() {
        let mut detector = DtmfDetector::new();

        // Start of digit '5' (event_id=5, no end bit, volume=10, duration=160)
        let payload1 = build_telephone_event(5, false, 10, 160);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // Continuation (duration=320)
        let payload2 = build_telephone_event(5, false, 10, 320);
        assert!(detector.process(&payload2, 1000, 8000).is_none());

        // End bit set (duration=480)
        let payload3 = build_telephone_event(5, true, 10, 480);
        let event = detector.process(&payload3, 1000, 8000).unwrap();
        assert_eq!(event.digit, '5');
        assert_eq!(event.duration_ms, 60); // 480 ticks at 8000Hz = 60ms
        assert_eq!(event.volume, 10);
    }

    #[test]
    fn test_detect_star_hash() {
        let mut detector = DtmfDetector::new();

        let payload = build_telephone_event(10, true, 10, 1280);
        let event = detector.process(&payload, 1000, 8000).unwrap();
        assert_eq!(event.digit, '*');

        let payload = build_telephone_event(11, true, 10, 1280);
        let event = detector.process(&payload, 2000, 8000).unwrap();
        assert_eq!(event.digit, '#');
    }

    #[test]
    fn test_generate_dtmf() {
        let packets = DtmfGenerator::generate('5', 160, 10, 8000, 20).unwrap();

        // 160ms / 20ms = 8 regular packets + 3 end packets (replacing last) = 7 + 3 = 10
        assert!(
            packets.len() >= 9,
            "expected >=9 packets, got {}",
            packets.len()
        );

        // First packet should have marker bit
        assert!(packets[0].marker);

        // Last packets should have end bit
        let last = &packets[packets.len() - 1];
        // End bit is in payload byte 1 bit 7
        assert!(
            last.payload[1] & 0x80 != 0,
            "last packet should have end bit set"
        );

        // Verify payload structure
        assert_eq!(packets[0].payload.len(), 4);
        assert_eq!(packets[0].payload[0], 5); // event_id for '5'
    }

    #[test]
    fn test_roundtrip() {
        let packets = DtmfGenerator::generate('#', 100, 10, 8000, 20).unwrap();
        let mut detector = DtmfDetector::new();

        let mut detected = None;
        for pkt in &packets {
            if let Some(evt) = detector.process(&pkt.payload, 5000, 8000) {
                detected = Some(evt);
            }
        }

        let event = detected.expect("should have detected DTMF");
        assert_eq!(event.digit, '#');
    }

    #[test]
    fn test_all_digits() {
        for digit in "0123456789*#ABCD".chars() {
            let id = digit_to_event_id(digit).unwrap();
            let back = event_id_to_digit(id).unwrap();
            assert_eq!(back, digit.to_ascii_uppercase());
        }
    }

    #[test]
    fn test_invalid_digit() {
        assert!(digit_to_event_id('X').is_none());
        assert!(DtmfGenerator::generate('X', 100, 10, 8000, 20).is_none());
    }

    #[test]
    fn test_too_short_payload() {
        let mut detector = DtmfDetector::new();
        assert!(detector.process(&[0, 1, 2], 0, 8000).is_none());
    }

    #[test]
    fn test_is_telephone_event_pt_negotiated() {
        // With negotiated PT=96, only 96 should match
        assert!(is_telephone_event_pt(96, Some(96)));
        assert!(!is_telephone_event_pt(101, Some(96)));
        assert!(!is_telephone_event_pt(126, Some(96)));

        // Without negotiated PT, common defaults (101, 96, 126) should match
        assert!(is_telephone_event_pt(101, None));
        assert!(is_telephone_event_pt(96, None));
        assert!(is_telephone_event_pt(126, None));
        assert!(!is_telephone_event_pt(97, None));
    }

    #[test]
    fn test_generate_duration_field_tracks_clock_rate() {
        // The RFC 4733 duration field is in telephone-event clock ticks. 200ms
        // at 8000 Hz → 1600 ticks in the final (end-bit) packet.
        let pkts8 = DtmfGenerator::generate('1', 200, 10, 8000, 20).unwrap();
        let last8 = &pkts8[pkts8.len() - 1].payload;
        assert_eq!(u16::from_be_bytes([last8[2], last8[3]]), 1600);

        // The same 200ms at 48000 Hz → 9600 ticks, proving the field follows the
        // telephone-event clock. (The DTMF path must feed the negotiated te clock
        // here — 8000 for a SIP phone — not the audio codec clock.)
        let pkts48 = DtmfGenerator::generate('1', 200, 10, 48000, 20).unwrap();
        let last48 = &pkts48[pkts48.len() - 1].payload;
        assert_eq!(u16::from_be_bytes([last48[2], last48[3]]), 9600);
    }

    #[test]
    fn test_rescale_event_duration() {
        // 48000 → 8000: 4800 ticks (100ms) becomes 800 ticks (100ms).
        let mut p = build_telephone_event(5, true, 10, 4800);
        rescale_event_duration(&mut p, 48000, 8000);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 800);
        assert_eq!(p[0], 5, "event id preserved");
        assert_eq!(p[1], 0x80 | 10, "end bit + volume preserved");

        // 8000 → 48000: the reverse direction.
        let mut p = build_telephone_event(5, true, 10, 800);
        rescale_event_duration(&mut p, 8000, 48000);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 4800);

        // No-op when clocks match, a clock is zero, or the payload is too short.
        let mut p = build_telephone_event(5, true, 10, 800);
        rescale_event_duration(&mut p, 8000, 8000);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 800);
        rescale_event_duration(&mut p, 0, 8000);
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 800);
        let mut short = vec![1u8, 2, 3];
        rescale_event_duration(&mut short, 48000, 8000); // must not panic
    }

    #[test]
    fn test_interrupted_event() {
        let mut detector = DtmfDetector::new();

        // Start digit '1' (event_id=1, no end bit, volume=10, duration=160)
        let payload1 = build_telephone_event(1, false, 10, 160);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // Continue digit '1' (duration=320)
        let payload2 = build_telephone_event(1, false, 10, 320);
        assert!(detector.process(&payload2, 1000, 8000).is_none());

        // New digit '2' starts WITHOUT end bit for '1' (lost end packet)
        let payload3 = build_telephone_event(2, false, 10, 160);
        let event = detector.process(&payload3, 2000, 8000);
        // Digit '1' should be emitted as an interrupted event
        let event = event.expect("interrupted digit '1' should be emitted");
        assert_eq!(event.digit, '1');
        assert_eq!(event.duration_ms, 40); // 320 ticks at 8000Hz = 40ms
        assert_eq!(event.volume, 10);

        // Now end digit '2' normally
        let payload4 = build_telephone_event(2, true, 10, 480);
        let event = detector.process(&payload4, 2000, 8000).unwrap();
        assert_eq!(event.digit, '2');
        assert_eq!(event.duration_ms, 60); // 480 ticks at 8000Hz = 60ms
    }

    #[test]
    fn test_zero_clock_rate() {
        let mut detector = DtmfDetector::new();

        // With clock_rate=0, fall back to 8000 Hz for conversion
        let payload = build_telephone_event(5, true, 10, 480);
        let event = detector.process(&payload, 1000, 0).unwrap();
        assert_eq!(event.digit, '5');
        assert_eq!(event.duration_ms, 60); // 480 ticks at default 8000Hz = 60ms
        assert_eq!(event.volume, 10);
    }

    /// Test: DtmfDetector correctly detects all 16 DTMF digits (0-9, *, #, A-D).
    /// For each digit, feed an event with end_bit=true and verify the detected
    /// digit matches the expected character.
    #[test]
    fn test_all_dtmf_digits_detected() {
        let expected: &[(u8, char)] = &[
            (0, '0'),
            (1, '1'),
            (2, '2'),
            (3, '3'),
            (4, '4'),
            (5, '5'),
            (6, '6'),
            (7, '7'),
            (8, '8'),
            (9, '9'),
            (10, '*'),
            (11, '#'),
            (12, 'A'),
            (13, 'B'),
            (14, 'C'),
            (15, 'D'),
        ];

        let mut detector = DtmfDetector::new();
        for (event_id, expected_digit) in expected {
            // Use a unique timestamp per digit so the detector does not suppress
            // duplicates (last_completed dedup uses (event_id, timestamp))
            let timestamp = *event_id as u32 * 1000;
            let payload = build_telephone_event(*event_id, true, 10, 800);
            let event = detector
                .process(&payload, timestamp, 8000)
                .unwrap_or_else(|| {
                    panic!(
                        "DtmfDetector should detect event_id={} as digit '{}'",
                        event_id, expected_digit
                    )
                });
            assert_eq!(
                event.digit, *expected_digit,
                "event_id={} should map to digit '{}', got '{}'",
                event_id, expected_digit, event.digit
            );
            assert_eq!(event.volume, 10);
            assert_eq!(event.duration_ms, 100); // 800 ticks at 8000Hz = 100ms
        }
    }

    #[test]
    fn test_timeout_emits_event() {
        let mut detector = DtmfDetector::new();
        let payload = build_telephone_event(5, false, 10, 800);
        assert!(detector.process(&payload, 1000, 8000).is_none());

        // Simulate 4 seconds elapsed by replacing the start time
        if let Some(ref mut cur) = detector.current {
            cur.4 = Instant::now() - Duration::from_secs(4);
        }

        let event = detector.check_timeout(8000);
        assert!(event.is_some(), "timed-out event should be emitted");
        let event = event.unwrap();
        assert_eq!(event.digit, '5');
        assert_eq!(event.duration_ms, 100); // 800 / 8000 * 1000
        assert_eq!(event.volume, 10);

        // State should be cleared
        assert!(detector.current.is_none());
    }

    #[test]
    fn test_no_timeout_before_3_seconds() {
        let mut detector = DtmfDetector::new();
        let payload = build_telephone_event(5, false, 10, 800);
        assert!(detector.process(&payload, 1000, 8000).is_none());

        // No time manipulation — should not time out
        assert!(detector.check_timeout(8000).is_none());
        // State should still be active
        assert!(detector.current.is_some());
    }

    #[test]
    fn test_timeout_during_process_emits_old_event() {
        let mut detector = DtmfDetector::new();
        // Start digit '5'
        let payload1 = build_telephone_event(5, false, 10, 800);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // Simulate timeout
        if let Some(ref mut cur) = detector.current {
            cur.4 = Instant::now() - Duration::from_secs(4);
        }

        // Process a new digit '3' — should emit timed-out '5' first
        let payload2 = build_telephone_event(3, false, 10, 160);
        let event = detector.process(&payload2, 2000, 8000);
        assert!(event.is_some(), "timed-out digit '5' should be emitted");
        assert_eq!(event.unwrap().digit, '5');

        // Current should now be digit '3'
        assert!(detector.current.is_some());
        assert_eq!(detector.current.as_ref().unwrap().0, 3);
    }

    #[test]
    fn test_interrupted_event_not_dropped_on_end_bit() {
        let mut detector = DtmfDetector::new();

        // Start digit '1' (no end bit)
        let payload1 = build_telephone_event(1, false, 10, 160);
        assert!(detector.process(&payload1, 1000, 8000).is_none());

        // New digit '2' arrives with end-bit immediately (e.g., very short digit)
        let payload2 = build_telephone_event(2, true, 10, 480);
        let event = detector.process(&payload2, 2000, 8000);
        // Should get the interrupted '1' event, NOT the '2' end event
        let event = event.expect("interrupted digit '1' should be emitted");
        assert_eq!(event.digit, '1');

        // Digit '2' is still in self.current. A redundant end-bit copy completes it.
        let payload3 = build_telephone_event(2, true, 10, 480);
        let event = detector.process(&payload3, 2000, 8000);
        let event = event.expect("digit '2' end should be emitted on redundant copy");
        assert_eq!(event.digit, '2');
        assert_eq!(event.duration_ms, 60);
    }

    fn ev(id: u8, end: bool, dur: u16) -> [u8; 4] {
        let p = build_telephone_event(id, end, 10, dur);
        [p[0], p[1], p[2], p[3]]
    }

    #[test]
    fn late_non_end_packet_after_end_copies_does_not_reopen() {
        let mut d = DtmfDetector::new();
        assert!(d.process(&ev(5, false, 160), 1000, 8000).is_none());
        assert_eq!(d.process(&ev(5, true, 320), 1000, 8000).unwrap().digit, '5');
        assert!(d.process(&ev(5, true, 320), 1000, 8000).is_none());
        assert!(d.process(&ev(5, true, 320), 1000, 8000).is_none());
        assert!(d.process(&ev(5, false, 240), 1000, 8000).is_none());
        assert!(!d.has_active_digit());
        assert!(d.check_timeout(8000).is_none());
    }

    #[test]
    fn reordered_end_of_a_after_b_started_is_ignored() {
        let mut d = DtmfDetector::new();
        assert_eq!(d.process(&ev(1, true, 320), 1000, 8000).unwrap().digit, '1');
        assert!(d.process(&ev(2, false, 160), 5000, 8000).is_none());
        // Redundant end of '1' arrives late.
        assert!(d.process(&ev(1, true, 320), 1000, 8000).is_none());
        let e = d.process(&ev(2, true, 320), 5000, 8000).unwrap();
        assert_eq!(e.digit, '2');
        // And again after B completed too.
        assert!(d.process(&ev(1, true, 320), 1000, 8000).is_none());
        assert!(!d.has_active_digit());
    }

    #[test]
    fn repeated_digit_with_lost_end_packets_yields_two_digits() {
        let mut d = DtmfDetector::new();
        assert!(d.process(&ev(5, false, 160), 1000, 8000).is_none());
        assert!(d.process(&ev(5, false, 320), 1000, 8000).is_none());
        // All end packets lost; second '5' has a new timestamp.
        let first = d.process(&ev(5, false, 160), 4000, 8000).unwrap();
        assert_eq!(first.digit, '5');
        assert_eq!(first.duration_ms, 40);
        let second = d.process(&ev(5, true, 320), 4000, 8000).unwrap();
        assert_eq!(second.digit, '5');
        assert!(d.process(&ev(5, true, 320), 4000, 8000).is_none());
    }

    #[test]
    fn timestamp_wraparound_is_not_older() {
        let mut d = DtmfDetector::new();
        assert_eq!(
            d.process(&ev(1, true, 160), u32::MAX - 10, 8000)
                .unwrap()
                .digit,
            '1'
        );
        assert_eq!(d.process(&ev(2, true, 160), 100, 8000).unwrap().digit, '2');
    }

    #[test]
    fn generate_rejects_bad_durations_and_caps_16_bit() {
        assert!(DtmfGenerator::generate('1', 0, 10, 8000, 20).is_none());
        assert!(DtmfGenerator::generate('1', MAX_EVENT_DURATION_MS + 1, 10, 8000, 20).is_none());
        // 10 s at 48 kHz would be 480000 ticks: capped to 0xFFFF, no panic.
        let p = DtmfGenerator::generate('1', 10_000, 10, 48_000, 20).unwrap();
        let last = p.last().unwrap();
        assert_eq!(
            u16::from_be_bytes([last.payload[2], last.payload[3]]),
            0xFFFF
        );
        // Durations never decrease.
        let mut prev = 0;
        for pk in &p {
            let dur = u16::from_be_bytes([pk.payload[2], pk.payload[3]]);
            assert!(dur >= prev);
            prev = dur;
        }
        // Huge ptime / clock must not overflow.
        let _ = DtmfGenerator::generate('1', 10_000, 10, u32::MAX, 1);
        assert!(DtmfGenerator::generate('1', 100, 10, 8000, u32::MAX).is_some());
    }

    #[test]
    fn generator_schedule_fields() {
        let p = DtmfGenerator::generate('5', 100, 10, 8000, 20).unwrap();
        // 5 packets: 4 continuing + end x3 => 4 + 3 - ... last index is 4
        assert_eq!(p.len(), 4 + 3);
        assert_eq!(p.iter().filter(|x| x.marker).count(), 1);
        assert!(p[0].marker && !p[0].is_end);
        assert_eq!(p.iter().filter(|x| x.is_end).count(), 3);
        assert!(p[4..].iter().all(|x| x.is_end && !x.marker));
        assert_eq!(p[0].ts_offset, 0);
        assert_eq!(p[1].ts_offset, 160);
        assert_eq!(p[1].send_offset_ms, 20);
        assert_eq!(p[4].send_offset_ms, 80);
        assert_eq!(p[6].send_offset_ms, 120);
        // duration field grows, end carries total (100 ms = 800 ticks)
        let dur = |x: &DtmfPacket| u16::from_be_bytes([x.payload[2], x.payload[3]]);
        assert_eq!(dur(&p[0]), 160);
        assert_eq!(dur(&p[3]), 640);
        assert_eq!(dur(&p[4]), 800);
        assert_eq!(dur(&p[6]), 800);
        assert!(DtmfGenerator::generate('5', 0, 10, 8000, 20).is_none());
    }

    #[test]
    fn single_packet_event_marker_once() {
        let p = DtmfGenerator::generate('1', 20, 10, 8000, 20).unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p.iter().filter(|x| x.marker).count(), 1);
        assert!(p[0].marker && p[0].is_end);
    }

    #[test]
    fn frequency_table_complete() {
        assert_eq!(dtmf_frequencies('1'), Some((697.0, 1209.0)));
        assert_eq!(dtmf_frequencies('5'), Some((770.0, 1336.0)));
        assert_eq!(dtmf_frequencies('0'), Some((941.0, 1336.0)));
        assert_eq!(dtmf_frequencies('*'), Some((941.0, 1209.0)));
        assert_eq!(dtmf_frequencies('#'), Some((941.0, 1477.0)));
        assert_eq!(dtmf_frequencies('A'), Some((697.0, 1633.0)));
        assert_eq!(dtmf_frequencies('d'), Some((941.0, 1633.0)));
        assert_eq!(dtmf_frequencies('x'), None);
        for d in "0123456789*#ABCD".chars() {
            assert!(dtmf_frequencies(d).is_some());
        }
    }

    #[test]
    fn inband_tone_contains_both_frequencies() {
        use crate::goertzel::detect;
        for d in "0123456789*#ABCD".chars() {
            let (lo, hi) = dtmf_frequencies(d).unwrap();
            let s = inband_dtmf_samples(d, 100).unwrap();
            assert_eq!(s.len(), 4800);
            let mid = &s[480..4320];
            let (rl, _) = detect(mid, 48000, lo);
            let (rh, _) = detect(mid, 48000, hi);
            assert!(rl > 0.4 && rh > 0.4, "{d}: {rl} {rh}");
        }
        assert!(inband_dtmf_samples('x', 100).is_none());
        assert!(inband_dtmf_samples('1', 0).is_none());
    }

    #[test]
    fn inband_generator_streams_and_finishes() {
        let mut g = inband_dtmf_generator('5', 40).unwrap();
        let a = g.next_frame(960);
        assert!(!g.is_finished());
        let b = g.next_frame(960);
        assert!(g.is_finished(), "finished exactly at 40 ms");
        let c = g.next_frame(960);
        assert!(a.iter().any(|&x| x != 0));
        assert!(b.iter().any(|&x| x != 0));
        assert!(c.iter().all(|&x| x == 0));
    }

    #[test]
    fn info_dtmf_relay() {
        let r = parse_info_dtmf("application/dtmf-relay", "Signal=5\r\nDuration=160\r\n").unwrap();
        assert_eq!(
            r,
            InfoDtmf {
                digit: '5',
                duration_ms: Some(160)
            }
        );
        let r = parse_info_dtmf("Application/DTMF-Relay; charset=x", "Signal= #\r\n").unwrap();
        assert_eq!(
            r,
            InfoDtmf {
                digit: '#',
                duration_ms: None
            }
        );
        let r = parse_info_dtmf("application/dtmf-relay", "Signal=10\nDuration=250").unwrap();
        assert_eq!(r.digit, '*');
        let r = parse_info_dtmf("application/dtmf-relay", "signal=a\nduration=80").unwrap();
        assert_eq!(r.digit, 'A');
        assert!(parse_info_dtmf("application/dtmf-relay", "Duration=160").is_none());
        assert!(parse_info_dtmf("application/dtmf-relay", "Signal=Z").is_none());
        assert!(parse_info_dtmf("application/dtmf-relay", "Signal=16").is_none());
    }

    #[test]
    fn info_dtmf_plain() {
        let r = parse_info_dtmf("application/dtmf", "7").unwrap();
        assert_eq!(
            r,
            InfoDtmf {
                digit: '7',
                duration_ms: None
            }
        );
        let r = parse_info_dtmf("application/dtmf", "*\r\n").unwrap();
        assert_eq!(r.digit, '*');
        let r = parse_info_dtmf("application/dtmf", "Signal=3\r\nDuration=100").unwrap();
        assert_eq!((r.digit, r.duration_ms), ('3', Some(100)));
        assert!(parse_info_dtmf("text/plain", "5").is_none());
        assert!(parse_info_dtmf("application/dtmf", "").is_none());
    }

    #[test]
    fn backward_timestamp_jump_starts_new_timeline() {
        let pkt = |ev, end, dur| build_telephone_event(ev, end, 10, dur);
        let mut d = DtmfDetector::new();
        let ts = 3_000_000_000u32;
        assert!(d.process(&pkt(1, false, 160), ts, 8000).is_none());
        assert_eq!(d.process(&pkt(1, true, 800), ts, 8000).unwrap().digit, '1');
        // Same SSRC, new timestamp base far behind (source switch).
        let ts2 = 1_000_000_000u32;
        let mut got = vec![];
        for k in 0..5u32 {
            let t = ts2 + k * 8000;
            for p in [pkt(2, false, 160), pkt(2, true, 800), pkt(2, true, 800)] {
                if let Some(e) = d.process(&p, t, 8000) {
                    got.push(e.digit);
                }
            }
        }
        assert_eq!(got, vec!['2'; 5]);
    }

    #[test]
    fn near_reordered_straggler_still_dropped() {
        let mut d = DtmfDetector::new();
        let p = |ev, end| build_telephone_event(ev, end, 10, 800);
        assert!(d.process(&p(1, true), 100_000, 8000).is_some());
        // 1 s older than the last digit: a straggler, not a new digit.
        assert!(d.process(&p(7, true), 92_000, 8000).is_none());
        // Redundant end copy of the completed event.
        assert!(d.process(&p(1, true), 100_000, 8000).is_none());
    }

    #[test]
    fn backward_jump_flushes_in_progress_event() {
        let mut d = DtmfDetector::new();
        let p = |ev, end| build_telephone_event(ev, end, 10, 800);
        assert!(d.process(&p(3, false), 4_000_000_000, 8000).is_none());
        // New timeline while digit 3 is still open: 3 is emitted, 4 follows.
        let e = d.process(&p(4, true), 1_000_000_000, 8000).unwrap();
        assert_eq!(e.digit, '3');
        assert!(d.has_active_digit());
        assert_eq!(
            d.process(&p(4, true), 1_000_000_000, 8000).unwrap().digit,
            '4'
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut d = DtmfDetector::new();
        let p = build_telephone_event(5, true, 10, 800);
        assert!(d.process(&p, 5000, 8000).is_some());
        d.reset();
        // Slightly older timestamp is accepted after reset (new SSRC).
        assert!(d.process(&p, 4000, 8000).is_some());
    }
}
