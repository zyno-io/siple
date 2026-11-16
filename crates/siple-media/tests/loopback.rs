//! End-to-end media loopback: tone/DTMF -> codec -> RTP -> SRTP -> lossy,
//! jittery, reordering network -> SRTP -> jitter buffer -> decoder -> detector.

use std::time::{Duration, Instant};

use siple_media::codec::{AudioCodec, EncoderConfig, OpusEncoderConfig, make_encoder};
use siple_media::dtmf::{DtmfDetector, DtmfEvent, DtmfGenerator};
use siple_media::goertzel::detect;
use siple_media::jitter::{JitterBuffer, JitterStats};
use siple_media::rtp::RtpHeader;
use siple_media::srtp::{SrtpContext, SrtpKeyMaterial, SrtpSuite};
use siple_media::tone::{ToneGenerator, ToneSpec};

const FRAMES: usize = 250; // 5 s
const OPUS_PT: u8 = 111;
const TE_PT: u8 = 101;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Net {
    rng: Rng,
    loss_pct: f64,
    max_jitter_ms: f64,
    /// Probability of an extra 20-40 ms delay (forces reordering).
    reorder_p: f64,
}

impl Net {
    fn new(seed: u64, loss_pct: f64) -> Self {
        Self {
            rng: Rng(seed | 1),
            loss_pct,
            max_jitter_ms: 60.0,
            reorder_p: 0.05,
        }
    }

    /// Arrival time for a packet sent at `send_ms`, or `None` if dropped.
    fn transit(&mut self, send_ms: f64) -> Option<f64> {
        if self.rng.unit() * 100.0 < self.loss_pct {
            return None;
        }
        let mut d = 10.0 + self.rng.unit() * self.max_jitter_ms;
        if self.rng.unit() < self.reorder_p {
            d += 20.0 + self.rng.unit() * 20.0;
        }
        Some(send_ms + d)
    }
}

/// Box-filter decimation of 48 kHz to `rate`.
fn resample_from_48k(src: &[i16], rate: u32) -> Vec<i16> {
    let f = (48000 / rate) as usize;
    src.chunks_exact(f)
        .map(|c| (c.iter().map(|&x| x as i32).sum::<i32>() / f as i32) as i16)
        .collect()
}

fn tone_48k(samples: usize) -> Vec<i16> {
    ToneGenerator::new(ToneSpec::sine(1000.0), 10000).next_frame(samples)
}

fn rtp_pt(codec: AudioCodec) -> u8 {
    codec.static_payload_type().unwrap_or(OPUS_PT)
}

fn srtp_pair(suite: SrtpSuite) -> (SrtpContext, SrtpContext) {
    let key = SrtpKeyMaterial::generate(suite).to_base64();
    (
        SrtpContext::from_sdes_key(suite, &key).unwrap(),
        SrtpContext::from_sdes_key(suite, &key).unwrap(),
    )
}

struct AudioResult {
    ratio: f32,
    level: f32,
    stats: JitterStats,
    active_frames: usize,
}

fn run_audio(codec: AudioCodec, suite: Option<SrtpSuite>, loss_pct: f64, seed: u64) -> AudioResult {
    let rate = codec.pcm_sample_rate();
    let spf = codec.samples_per_frame(20);
    let pcm = resample_from_48k(&tone_48k(FRAMES * 960), rate);
    let lossy = loss_pct > 0.0;
    let cfg = EncoderConfig {
        opus: OpusEncoderConfig {
            inband_fec: lossy,
            packet_loss_pct: if lossy { 10 } else { 0 },
            ..Default::default()
        },
    };
    let mut enc = make_encoder(codec, &cfg).unwrap();
    let mut srtp = suite.map(srtp_pair);
    let mut net = Net::new(seed, loss_pct);

    // Sender: encode, packetize, protect, send through the network.
    let ssrc = 0x1234_5678;
    let mut wire: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut payload = Vec::new();
    for i in 0..FRAMES {
        enc.encode(&pcm[i * spf..(i + 1) * spf], &mut payload)
            .unwrap();
        let rtp = RtpHeader::build(
            rtp_pt(codec),
            i as u16,
            i as u32 * codec.rtp_ts_per_frame(20),
            ssrc,
            i == 0,
            &payload,
        );
        let pkt = match srtp.as_mut() {
            Some((tx, _)) => {
                let p = tx.protect(&rtp).unwrap();
                assert_ne!(
                    &p[12..12 + payload.len()],
                    &payload[..],
                    "payload must be encrypted"
                );
                assert!(p.len() > rtp.len(), "auth tag appended");
                p
            }
            None => rtp,
        };
        if let Some(at) = net.transit(i as f64 * 20.0) {
            wire.push((at, pkt));
        }
    }
    wire.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    // Receiver.
    let base = Instant::now();
    let at = |ms: f64| base + Duration::from_micros((ms * 1000.0) as u64);
    let mut jb = JitterBuffer::new(codec.rtp_clock_rate(), rate, 40, 200);
    let mut dec = siple_media::codec::make_decoder(codec).unwrap();
    let mut out_pcm: Vec<Vec<i16>> = Vec::new();
    let mut active = 0;
    let mut idx = 0;
    let mut frame = Vec::new();
    let mut t = 0.0;
    while t < FRAMES as f64 * 20.0 + 400.0 {
        while idx < wire.len() && wire[idx].0 <= t {
            let (arr, raw) = &wire[idx];
            idx += 1;
            let plain = match srtp.as_mut() {
                Some((_, rx)) => rx.unprotect(raw).expect("SRTP auth must pass"),
                None => raw.clone(),
            };
            let h = RtpHeader::parse(&plain).unwrap();
            assert_eq!(h.payload_type, rtp_pt(codec));
            jb.push(
                h.sequence_number,
                h.timestamp,
                h.marker,
                h.ssrc,
                h.payload(&plain).to_vec(),
                at(*arr),
            );
            assert!(!jb.take_decoder_reset(), "no resync expected");
        }
        let info = jb.pull(dec.as_mut(), &mut frame, at(t));
        assert_eq!(frame.len(), rate as usize / 50);
        active += info.rx_active as usize;
        out_pcm.push(frame.clone());
        t += 20.0;
    }
    // Analyse 1.5 s .. 4.5 s.
    let seg: Vec<i16> = out_pcm[75..225].iter().flatten().copied().collect();
    let (ratio, level) = detect(&seg, rate, 1000.0);
    AudioResult {
        ratio,
        level,
        stats: jb.stats(),
        active_frames: active,
    }
}

const CODECS: [AudioCodec; 4] = [
    AudioCodec::Pcmu,
    AudioCodec::Pcma,
    AudioCodec::G722,
    AudioCodec::Opus,
];

fn suites() -> Vec<Option<SrtpSuite>> {
    std::iter::once(None)
        .chain(SrtpSuite::ALL.iter().copied().map(Some))
        .collect()
}

#[test]
fn tone_survives_jittery_reordering_network() {
    for codec in CODECS {
        for suite in suites() {
            let r = run_audio(codec, suite, 0.0, 0xABCD_1234);
            let ctx = format!("{codec:?} {suite:?} {:?}", r.stats);
            assert!(r.ratio > 0.9, "{ctx}: ratio {}", r.ratio);
            assert!(r.level > -20.0, "{ctx}: level {}", r.level);
            assert_eq!(r.stats.lost, 0, "{ctx}");
            assert_eq!(r.stats.resyncs, 0, "{ctx}");
            assert!(r.active_frames > FRAMES - 10, "{ctx}");
        }
    }
}

#[test]
fn tone_survives_five_percent_loss() {
    for codec in CODECS {
        for suite in suites() {
            let r = run_audio(codec, suite, 5.0, 0x5EED_0005);
            let ctx = format!("{codec:?} {suite:?} {:?}", r.stats);
            assert!(r.stats.lost >= 3, "{ctx}: loss must have occurred");
            assert!(r.ratio > 0.7, "{ctx}: ratio {}", r.ratio);
            assert!(r.level > -22.0, "{ctx}: level {}", r.level);
            assert!(r.active_frames > FRAMES - 10, "{ctx}");
            if codec == AudioCodec::Opus {
                assert!(r.stats.fec_recovered >= 1, "{ctx}: Opus FEC should engage");
            }
        }
    }
}

#[test]
fn tone_survives_ten_percent_loss_without_srtp() {
    for codec in CODECS {
        let r = run_audio(codec, None, 10.0, 0xDEAD_BEEF);
        assert!(r.ratio > 0.55, "{codec:?} ratio {} {:?}", r.ratio, r.stats);
    }
}

#[test]
fn srtp_rejects_tampered_packets() {
    let (mut tx, mut rx) = srtp_pair(SrtpSuite::AesCm128HmacSha1_80);
    let rtp = RtpHeader::build(0, 1, 160, 9, false, &[0x55; 160]);
    let mut p = tx.protect(&rtp).unwrap();
    p[40] ^= 1;
    assert!(rx.unprotect(&p).is_err());
}

// ── DTMF ────────────────────────────────────────────────────────────────

fn run_dtmf(digits: &str, suite: Option<SrtpSuite>, seed: u64, reorder: bool) -> Vec<DtmfEvent> {
    let mut srtp = suite.map(srtp_pair);
    let mut net = Net::new(seed, 0.0);
    net.reorder_p = if reorder { 0.3 } else { 0.0 };
    net.max_jitter_ms = if reorder { 30.0 } else { 0.0 };
    let ssrc = 77;
    let mut seq = 1000u16;
    let mut wire: Vec<(f64, Vec<u8>)> = Vec::new();
    let mut event_start_ts = 160_000u32;
    let mut event_start_ms = 0.0;
    for d in digits.chars() {
        let pkts = DtmfGenerator::generate(d, 160, 10, 8000, 20).unwrap();
        for p in &pkts {
            // All packets of an event share the event-start timestamp.
            let rtp = RtpHeader::build(TE_PT, seq, event_start_ts, ssrc, p.marker, &p.payload);
            seq = seq.wrapping_add(1);
            let pkt = match srtp.as_mut() {
                Some((tx, _)) => tx.protect(&rtp).unwrap(),
                None => rtp,
            };
            let send = event_start_ms + p.send_offset_ms as f64;
            wire.push((net.transit(send).unwrap(), pkt));
        }
        // 70 ms inter-digit gap after the 3 end packets.
        event_start_ts += 8000 / 2; // 500 ms between event starts
        event_start_ms += 500.0;
    }
    wire.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut det = DtmfDetector::new();
    let mut got = Vec::new();
    for (_, raw) in wire {
        let plain = match srtp.as_mut() {
            Some((_, rx)) => rx.unprotect(&raw).unwrap(),
            None => raw,
        };
        let h = RtpHeader::parse(&plain).unwrap();
        assert_eq!(h.payload_type, TE_PT);
        if let Some(ev) = det.process(h.payload(&plain), h.timestamp, 8000) {
            got.push(ev);
        }
    }
    got
}

#[test]
fn dtmf_rfc4733_loopback() {
    for suite in suites() {
        for reorder in [false, true] {
            let got = run_dtmf("123#", suite, 0xFEED, reorder);
            let digits: String = got.iter().map(|e| e.digit).collect();
            assert_eq!(digits, "123#", "{suite:?} reorder={reorder}: {got:?}");
            for e in &got {
                assert_eq!(e.duration_ms, 160);
                assert_eq!(e.volume, 10);
            }
        }
    }
}

#[test]
fn dtmf_all_digits() {
    let got = run_dtmf("0123456789*#ABCD", None, 99, false);
    let digits: String = got.iter().map(|e| e.digit).collect();
    assert_eq!(digits, "0123456789*#ABCD");
}

#[test]
fn dtmf_late_packet_after_end_is_not_a_duplicate() {
    let mut det = DtmfDetector::new();
    let ts = 5000;
    // start, end, then a reordered middle packet of the same event, then the next digit.
    let start = [1u8, 10, 0, 160];
    let mid = [1u8, 10, 0x01, 0x40];
    let end = [1u8, 0x80 | 10, 0x01, 0x40];
    assert!(det.process(&start, ts, 8000).is_none());
    assert_eq!(det.process(&end, ts, 8000).unwrap().digit, '1');
    assert!(det.process(&mid, ts, 8000).is_none());
    assert!(det.process(&end, ts, 8000).is_none());
    let two_end = [2u8, 0x80 | 10, 0x01, 0x40];
    assert_eq!(det.process(&two_end, ts + 4000, 8000).unwrap().digit, '2');
    assert!(!det.has_active_digit());
}
