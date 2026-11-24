//! Media engine loopback tests: two calls on 127.0.0.1, negotiated with the
//! real SDP offer/answer state machine, driven on the virtual clock (no audio
//! devices needed).

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::Duration;

use siple_core::audio::AudioDevices;
use siple_core::config::{AudioSink, AudioSource};
use siple_core::media::{
    CallMediaHandle, CallMediaSpec, DtmfSendMode, MediaEngine, MediaEvent, MediaEventKind,
    ToneDetectResult,
};
use siple_media::codec::AudioCodec;
use siple_media::goertzel::detect;
use siple_media::sdp::SessionDescription;
use siple_media::sdp_neg::{LocalCaps, MediaSession, Negotiated, SrtpPolicy};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

struct Rig {
    engine: MediaEngine,
    events: UnboundedReceiver<MediaEvent>,
    a: CallMediaHandle,
    b: CallMediaHandle,
    sess_a: MediaSession,
    sess_b: MediaSession,
    neg_a: Negotiated,
    offer: SessionDescription,
}

fn caps(codec: AudioCodec, srtp: SrtpPolicy) -> LocalCaps {
    LocalCaps {
        codecs: vec![codec],
        srtp,
        ..Default::default()
    }
}

fn spec(id: &str, source: AudioSource, sink: AudioSink) -> CallMediaSpec {
    let mut s = CallMediaSpec::new(id, LO, LO);
    s.source = source;
    s.sink = sink;
    s
}

fn rig(
    codec: AudioCodec,
    srtp: SrtpPolicy,
    a_src: AudioSource,
    b_src: AudioSource,
    b_sink: AudioSink,
) -> Rig {
    let (ev_tx, events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let a = engine
        .create_call(spec("A", a_src, AudioSink::Null))
        .unwrap();
    let b = engine.create_call(spec("B", b_src, b_sink)).unwrap();
    let c = caps(codec, srtp);
    let mut sess_a = MediaSession::new(c.clone(), LO, a.local_rtp_port());
    let mut sess_b = MediaSession::new(c, LO, b.local_rtp_port());
    let offer = SessionDescription::parse(&sess_a.create_offer()).unwrap();
    let (answer, neg_b) = sess_b.apply_offer(&offer).unwrap();
    let answer = SessionDescription::parse(&answer).unwrap();
    let neg_a = sess_a.apply_answer(&answer).unwrap();
    assert_eq!(neg_a.codec, codec);
    a.apply(neg_a.clone());
    b.apply(neg_b.clone());
    Rig {
        engine,
        events,
        a,
        b,
        sess_a,
        sess_b,
        neg_a,
        offer,
    }
}

async fn detect_for(h: &CallMediaHandle, hz: f32, secs: f32) -> ToneDetectResult {
    h.detect_tone(hz, Duration::from_secs_f32(secs), -40.0)
        .await
        .unwrap()
}

/// Polls `cond` every 10 ms until it holds; panics after `secs` seconds. Waiting for the
/// condition (instead of sleeping a fixed time and then checking) keeps a stalled machine from
/// failing a test that only measures progress.
async fn until(what: &str, secs: u64, mut cond: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out after {secs} s waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for `RecordFinished` of `path` on call `call`: the WAV file is then finalized.
async fn record_finished(events: &mut UnboundedReceiver<MediaEvent>, call: &str, path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("no RecordFinished")
            .unwrap();
        if ev.call_id == call
            && matches!(&ev.kind, MediaEventKind::RecordFinished { path: p } if p == path)
        {
            return;
        }
    }
}

async fn tone_roundtrip(codec: AudioCodec, srtp: SrtpPolicy) {
    let r = rig(
        codec,
        srtp,
        AudioSource::Tone(1000.0),
        AudioSource::Tone(1500.0),
        AudioSink::Null,
    );
    let (da, db) = tokio::join!(detect_for(&r.a, 1500.0, 4.0), detect_for(&r.b, 1000.0, 4.0));
    assert!(
        db.detected,
        "{codec:?}/{srtp:?}: B did not hear 1 kHz: {db:?}"
    );
    assert!(
        da.detected,
        "{codec:?}/{srtp:?}: A did not hear 1.5 kHz: {da:?}"
    );
    // Wrong frequency must not be detected.
    let wrong = detect_for(&r.b, 1500.0, 0.6).await;
    assert!(!wrong.detected, "{wrong:?}");

    let (sa, sb) = (r.a.stats(), r.b.stats());
    assert_eq!(sa.codec, Some(codec));
    assert!(sa.tx_packets > 10 && sb.rx_packets > 10, "{sa:?} {sb:?}");
    assert_eq!(sa.srtp_auth_failures + sb.srtp_auth_failures, 0);
    assert_eq!(sa.srtp_suite.is_some(), srtp != SrtpPolicy::Off);
    assert!(sb.rx_active);
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tone_pcmu() {
    tone_roundtrip(AudioCodec::Pcmu, SrtpPolicy::Off).await;
    tone_roundtrip(AudioCodec::Pcmu, SrtpPolicy::Mandatory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tone_pcma() {
    tone_roundtrip(AudioCodec::Pcma, SrtpPolicy::Off).await;
    tone_roundtrip(AudioCodec::Pcma, SrtpPolicy::Mandatory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tone_g722() {
    tone_roundtrip(AudioCodec::G722, SrtpPolicy::Off).await;
    tone_roundtrip(AudioCodec::G722, SrtpPolicy::Mandatory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tone_opus() {
    tone_roundtrip(AudioCodec::Opus, SrtpPolicy::Off).await;
    tone_roundtrip(AudioCodec::Opus, SrtpPolicy::Mandatory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_and_mute() {
    let r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Tone(1000.0),
        AudioSource::Silence,
        AudioSink::Null,
    );
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);
    assert!(r.b.stats().rx_active);

    // Mute: RTP keeps flowing but carries silence.
    r.a.set_mute(true);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!detect_for(&r.b, 1000.0, 0.8).await.detected);
    assert!(r.b.stats().rx_active, "muted call still sends RTP");
    r.a.set_mute(false);
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);

    // Hold: no RTP at all.
    r.a.set_hold(true);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let tx_before = r.a.stats().tx_packets;
    assert!(!r.b.stats().rx_active, "rx_active must drop while held");
    assert!(!detect_for(&r.b, 1000.0, 0.8).await.detected);
    assert!(r.a.stats().tx_paused);
    assert_eq!(r.a.stats().tx_packets, tx_before);

    // Unhold: resumes (marker + timestamp jump handled by the jitter buffer).
    r.a.set_hold(false);
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);
    assert!(r.b.stats().rx_active);
    assert!(!r.a.stats().tx_paused);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reinvite_unchanged_keys_keeps_srtp() {
    let mut r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Mandatory,
        AudioSource::Tone(1000.0),
        AudioSource::Tone(1500.0),
        AudioSink::Null,
    );
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);

    // Same-offer re-negotiation: nothing changed, so keys must be kept.
    let (answer, neg_b) = r.sess_b.apply_offer(&r.offer).unwrap();
    let answer = SessionDescription::parse(&answer).unwrap();
    let neg_a = r.sess_a.apply_answer(&answer).unwrap();
    assert!(!neg_a.srtp.as_ref().unwrap().remote_changed);
    assert!(!neg_b.srtp.as_ref().unwrap().remote_changed);
    for _ in 0..3 {
        r.a.apply(neg_a.clone());
        r.b.apply(neg_b.clone());
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Media must keep flowing: more packets arrive (and pass SRTP). Waited for rather than
    // counted over a fixed 500 ms, which a stalled machine can spend without sending a packet.
    let before = r.b.stats().rx_packets;
    until("media to keep flowing", 10, || {
        r.b.stats().rx_packets > before + 10
    })
    .await;
    assert!(detect_for(&r.b, 1000.0, 3.0).await.detected);
    assert!(detect_for(&r.a, 1500.0, 3.0).await.detected);
    assert_eq!(r.a.stats().srtp_auth_failures, 0);
    assert_eq!(r.b.stats().srtp_auth_failures, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reinvite_changed_remote_key_overlaps() {
    let r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Mandatory,
        AudioSource::Tone(1000.0),
        AudioSource::Tone(1500.0),
        AudioSink::Null,
    );
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);
    assert!(detect_for(&r.a, 1500.0, 4.0).await.detected);

    // B re-keys (fresh session => fresh keys) and answers A's offer again.
    let mut sess_b2 = MediaSession::new(
        caps(AudioCodec::Pcmu, SrtpPolicy::Mandatory),
        LO,
        r.b.local_rtp_port(),
    );
    let (answer, neg_b2) = sess_b2.apply_offer(&r.offer).unwrap();
    let answer = SessionDescription::parse(&answer).unwrap();
    let mut sess_a = r.sess_a;
    let neg_a2 = sess_a.apply_answer(&answer).unwrap();
    assert!(neg_a2.srtp.as_ref().unwrap().remote_changed);
    assert_ne!(
        neg_a2.srtp.as_ref().unwrap().remote_key_b64,
        r.neg_a.srtp.as_ref().unwrap().remote_key_b64
    );

    // A switches first and must still accept B's old-key packets (overlap),
    // then B switches to the new key.
    r.a.apply(neg_a2);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        detect_for(&r.a, 1500.0, 3.0).await.detected,
        "old key overlap"
    );
    r.b.apply(neg_b2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(detect_for(&r.a, 1500.0, 3.0).await.detected, "new key");
    assert!(detect_for(&r.b, 1000.0, 3.0).await.detected);
    // Wait out the 2 s grace period: only the new key remains.
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert!(detect_for(&r.a, 1500.0, 3.0).await.detected);
    assert_eq!(r.a.stats().srtp_auth_failures, 0);
    assert_eq!(r.b.stats().srtp_auth_failures, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dtmf_rfc4733_in_order() {
    let mut r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Silence,
        AudioSource::Silence,
        AudioSink::Null,
    );
    r.a.send_dtmf("159#", DtmfSendMode::Rfc4733, 100, 70)
        .unwrap();
    assert!(r.a.send_dtmf("x", DtmfSendMode::Rfc4733, 100, 70).is_err());
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while got.len() < 4 {
        let ev = tokio::time::timeout_at(deadline, r.events.recv())
            .await
            .expect("timed out waiting for DTMF")
            .unwrap();
        if let (
            "B",
            MediaEventKind::DtmfReceived {
                digit, duration_ms, ..
            },
        ) = (ev.call_id.as_str(), ev.kind)
        {
            assert!((80..=140).contains(&duration_ms), "{duration_ms}");
            got.push(digit);
        }
    }
    assert_eq!(got, vec!['1', '5', '9', '#']);
    // No spurious duplicates from the 3x end packets.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(ev) = r.events.try_recv() {
        assert!(
            !matches!(ev.kind, MediaEventKind::DtmfReceived { .. }),
            "{ev:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dtmf_inband_is_audible() {
    let r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Silence,
        AudioSource::Silence,
        AudioSink::Null,
    );
    let det = r.b.detect_tone(697.0, Duration::from_secs(3), -45.0);
    r.a.send_dtmf("1", DtmfSendMode::Inband, 200, 50).unwrap();
    assert!(det.await.unwrap().detected);
}

fn read_wav(path: &Path) -> (hound::WavSpec, Vec<i16>) {
    let mut rd = hound::WavReader::open(path).unwrap();
    let spec = rd.spec();
    (spec, rd.samples::<i16>().map(|s| s.unwrap()).collect())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_stereo_one_second() {
    let mut r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Tone(1000.0),
        AudioSource::Silence,
        AudioSink::Null,
    );
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rx.wav");
    // One second by the engine's RX timeline (`rx_ms`: 20 ms per RX frame the media thread
    // pulls), not by the wall clock: a loaded machine stretches a fixed sleep, and the media
    // thread then drops the frames it is too far behind on.
    //
    // Jitter buffer counters from 240 ms of RX timeline before the start: a loss is counted
    // when it is handled but can fill up to ~220 ms of the frames after it (200 ms of loss plus
    // a frame), so counters read right at the start could miss silence already queued.
    // Counting from earlier only loosens the bound on silent frames below.
    let jitter0 = r.b.stats().jitter;
    let lead_in = r.b.rx_ms();
    until("an RX lead-in", 10, || r.b.rx_ms() >= lead_in + 240).await;
    let t0 = r.b.rx_ms();
    r.b.record_start(&path, true);
    // The media thread creates the file when it handles the command; every later RX frame is
    // recorded.
    until("the recording to start", 10, || path.exists()).await;
    let started = r.b.rx_ms();
    until("one second of RX audio", 30, || {
        r.b.rx_ms() >= started + 1000
    })
    .await;
    let stopping = r.b.rx_ms();
    r.b.record_stop();
    record_finished(&mut r.events, "B", &path).await;
    let t1 = r.b.rx_ms();
    // ... and from a snapshot taken after the stop.
    until("a stats snapshot after the stop", 10, || {
        r.b.stats().rx_ms >= t1
    })
    .await;
    let jitter1 = r.b.stats().jitter;
    let (spec, samples) = read_wav(&path);
    assert_eq!(
        (spec.channels, spec.sample_rate, spec.bits_per_sample),
        (2, 48000, 16)
    );
    // Exactly the RX frames pulled while recording: at least the second waited for, at most
    // what the timeline moved from before the start to after the stop (one frame of slack each
    // way for the relaxed `rx_ms` counter).
    let frames = samples.len() / 2;
    assert_eq!(frames % 960, 0, "whole 20 ms frames: {frames}");
    let ms = frames as u64 / 48;
    assert!(
        (stopping - started).saturating_sub(20) <= ms && ms <= t1 - t0 + 20,
        "recorded {ms} ms; RX timeline: {t0} before the start, {started} started, \
         {stopping} at the stop, {t1} finished"
    );
    let left: Vec<i16> = samples.iter().step_by(2).copied().collect();
    let right: Vec<i16> = samples.iter().skip(1).step_by(2).copied().collect();
    // An RX frame the jitter buffer had no packet for in time (A's or B's media work fell
    // behind, as on a loaded machine) is concealed, and silent once the 60 ms concealment runs
    // out. Such frames are not RX audio missing from the recording, and the tone stays
    // phase-coherent across them (1 kHz is 20 whole periods per 20 ms frame): judge the tone
    // on the frames that carry audio, and allow at most one silent frame per frame the jitter
    // buffer concealed or lost.
    let silent = |f: &&[i16]| f.iter().all(|&s| s.unsigned_abs() < 33); // below -60 dBFS
    let silent_frames = left.chunks(960).filter(silent).count() as u64;
    // (`lost` drops again when a packet declared lost arrives late, hence `late` too.)
    let gaps = jitter1.concealed.saturating_sub(jitter0.concealed)
        + jitter1.lost.saturating_sub(jitter0.lost)
        + jitter1.late.saturating_sub(jitter0.late);
    assert!(
        silent_frames <= gaps,
        "{silent_frames} silent RX frames of {}; jitter buffer before {jitter0:?}, after \
         {jitter1:?}",
        frames / 960
    );
    let audible: Vec<i16> = left
        .chunks(960)
        .skip(frames / 960 / 4)
        .filter(|f| !silent(f))
        .flatten()
        .copied()
        .collect();
    let (ratio, level) = detect(&audible, 48000, 1000.0);
    assert!(
        ratio > 0.8 && level > -25.0,
        "RX channel: {ratio} {level} over {} audible frames",
        audible.len() / 960
    );
    let (_, rlevel) = detect(&right, 48000, 1000.0);
    assert!(rlevel < -60.0, "TX channel should be silent: {rlevel}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_sink_mono() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sink.wav");
    let mut r = rig(
        AudioCodec::G722,
        SrtpPolicy::Off,
        AudioSource::Tone(1000.0),
        AudioSource::Silence,
        AudioSink::Record(path.clone()),
    );
    // The sink records from the call's creation (RX timeline 0). Judge the audio from where B
    // first hears the tone, for one second of the engine's RX timeline (not wall-clock sleeps,
    // which a loaded machine stretches past a late stream start).
    assert!(detect_for(&r.b, 1000.0, 4.0).await.detected);
    let heard = r.b.rx_ms();
    until("one second of RX audio", 30, || r.b.rx_ms() >= heard + 1000).await;
    r.b.close();
    // The file is finalized when the media thread tears the call down.
    record_finished(&mut r.events, "B", &path).await;
    let (spec, samples) = read_wav(&path);
    assert_eq!((spec.channels, spec.sample_rate), (1, 48000));
    let from = heard as usize * 48;
    assert!(
        samples.len() >= from + 48_000,
        "{} samples, tone heard at {heard} ms",
        samples.len()
    );
    // Judge 100 ms blocks after the detection: the tone must dominate most of them
    // (a loaded CI machine may glitch a block or two).
    let tail = &samples[from..];
    let ratios: Vec<f32> = tail
        .as_chunks::<4800>()
        .0
        .iter()
        .map(|c| detect(c, 48000, 1000.0).0)
        .collect();
    let good = ratios.iter().filter(|&&r| r > 0.8).count();
    assert!(
        good * 10 >= ratios.len() * 7,
        "tone ratios per 100 ms: {ratios:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_source_loops_and_resamples() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone700.wav");
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..8000 {
            let v = (i as f64 * 700.0 * std::f64::consts::TAU / 16000.0).sin() * 12000.0;
            w.write_sample(v as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    let r = rig(
        AudioCodec::Pcma,
        SrtpPolicy::Off,
        AudioSource::File(path.clone()),
        AudioSource::Silence,
        AudioSink::Null,
    );
    assert!(detect_for(&r.b, 700.0, 4.0).await.detected);
    // The file is 0.5 s long: still audible a second later means it loops.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(detect_for(&r.b, 700.0, 2.0).await.detected);

    // play() overrides the call's own source, stop_play() restores it.
    r.a.play(AudioSource::Tone(1200.0));
    assert!(detect_for(&r.b, 1200.0, 3.0).await.detected);
    r.a.stop_play();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(detect_for(&r.b, 700.0, 3.0).await.detected);

    // A missing file is an error at call creation.
    let err = r.engine.create_call(spec(
        "C",
        AudioSource::File(dir.path().join("nope.wav")),
        AudioSink::Null,
    ));
    assert!(err.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn port_allocation() {
    let (ev_tx, _events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let mut s = spec("p1", AudioSource::Silence, AudioSink::Null);
    s.port_range = [47200, 47203];
    let a = engine.create_call(s.clone()).unwrap();
    let b = engine.create_call(s.clone()).unwrap();
    assert_eq!(a.local_rtp_port() % 2, 0);
    assert_eq!(b.local_rtp_port() % 2, 0);
    assert_eq!(a.local_rtcp_port(), a.local_rtp_port() + 1);
    assert_ne!(a.local_rtp_port(), b.local_rtp_port());
    assert!(engine.create_call(s.clone()).is_err(), "range exhausted");
    // Closing a call releases its ports (once the media thread and RX task let go).
    a.close();
    drop(a);
    let mut ok = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if engine.create_call(s.clone()).is_ok() {
            ok = true;
            break;
        }
    }
    assert!(ok, "ports not released after close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rtcp_exchange_over_srtp() {
    let r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Mandatory,
        AudioSource::Tone(1000.0),
        AudioSource::Tone(1500.0),
        AudioSink::Null,
    );
    tokio::time::sleep(Duration::from_millis(5800)).await;
    let (sa, sb) = (r.a.stats(), r.b.stats());
    assert!(
        sa.rtcp_rx_packets >= 1 && sb.rtcp_rx_packets >= 1,
        "{sa:?} {sb:?}"
    );
    assert_eq!(sa.srtp_auth_failures + sb.srtp_auth_failures, 0);
    assert!(sa.jitter.received > 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_tones_run_without_devices() {
    use siple_core::media::{LocalTone, LocalToneKind, ToneTarget};
    let (ev_tx, _events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let id = engine.start_local_tone(LocalTone {
        kind: LocalToneKind::Ringtone,
        target: ToneTarget::Ringer,
    });
    engine.start_local_tone(LocalTone {
        kind: LocalToneKind::DtmfFeedback('5'),
        target: ToneTarget::Speaker,
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    engine.stop_local_tone(id);
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_with_comfort_noise_keeps_rx_active() {
    let (ev_tx, _events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let mut sa = spec("A", AudioSource::Tone(1000.0), AudioSink::Null);
    sa.hold_send_cn = true;
    let a = engine.create_call(sa).unwrap();
    let b = engine
        .create_call(spec("B", AudioSource::Silence, AudioSink::Null))
        .unwrap();
    let c = LocalCaps {
        codecs: vec![AudioCodec::Pcmu],
        comfort_noise: true,
        ..Default::default()
    };
    let mut ma = MediaSession::new(c.clone(), LO, a.local_rtp_port());
    let mut mb = MediaSession::new(c, LO, b.local_rtp_port());
    let offer = SessionDescription::parse(&ma.create_offer()).unwrap();
    let (answer, neg_b) = mb.apply_offer(&offer).unwrap();
    let neg_a = ma
        .apply_answer(&SessionDescription::parse(&answer).unwrap())
        .unwrap();
    assert!(neg_a.cn_pt.is_some(), "CN must be negotiated");
    a.apply(neg_a);
    b.apply(neg_b);
    assert!(detect_for(&b, 1000.0, 4.0).await.detected);

    a.set_hold(true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = a.stats().tx_packets;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let sent = a.stats().tx_packets - before;
    assert!((1..=4).contains(&sent), "CN packets while held: {sent}");
    let sb = b.stats();
    assert!(sb.rx_active, "CN keeps the stream alive");
    assert_eq!(sb.rx_unknown_pt, 0);
    assert!(!detect_for(&b, 1000.0, 0.8).await.detected);
}

// ── regression tests for review findings ────────────────────────────────

use siple_media::dtmf::DtmfGenerator;
use siple_media::rtp::RtpHeader;
use siple_media::sdp_neg::LocalHold;
use tokio::net::UdpSocket;

fn renegotiate(r: &mut Rig) {
    let offer = SessionDescription::parse(&r.sess_a.create_offer()).unwrap();
    let (ans, nb) = r.sess_b.apply_offer(&offer).unwrap();
    let na = r
        .sess_a
        .apply_answer(&SessionDescription::parse(&ans).unwrap())
        .unwrap();
    r.a.apply(na);
    r.b.apply(nb);
}

fn media_timeouts(r: &mut Rig) -> Vec<String> {
    let mut v = Vec::new();
    while let Ok(e) = r.events.try_recv() {
        if e.kind == MediaEventKind::MediaTimeout {
            v.push(e.call_id);
        }
    }
    v
}

/// After a long hold the 5 s media timeout must restart at unhold, not fire at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_media_timeout_right_after_unhold() {
    let mut r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Tone(1000.0),
        AudioSource::Tone(1500.0),
        AudioSink::Null,
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.sess_a.set_local_hold(LocalHold::SendOnly);
    r.a.set_hold(true);
    renegotiate(&mut r);
    tokio::time::sleep(Duration::from_millis(6500)).await;
    assert!(media_timeouts(&mut r).is_empty(), "timeout during hold");

    r.sess_a.set_local_hold(LocalHold::None);
    r.a.set_hold(false);
    renegotiate(&mut r);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let t = media_timeouts(&mut r);
    assert!(t.is_empty(), "spurious MediaTimeout after unhold: {t:?}");
    let before = r.a.stats().rx_packets;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(r.a.stats().rx_packets > before + 5, "media resumed");
}

fn neg_with_peer(
    caps: LocalCaps,
    local_port: u16,
    peer_port: u16,
) -> (Negotiated, MediaSession, MediaSession) {
    let mut sa = MediaSession::new(caps.clone(), LO, local_port);
    let mut sp = MediaSession::new(caps, LO, peer_port);
    let offer = SessionDescription::parse(&sa.create_offer()).unwrap();
    let (ans, _) = sp.apply_offer(&offer).unwrap();
    let n = sa
        .apply_answer(&SessionDescription::parse(&ans).unwrap())
        .unwrap();
    (n, sa, sp)
}

/// A stream that never starts times out 5 s after audio is expected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn never_started_stream_times_out() {
    let (ev_tx, mut events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let a = engine
        .create_call(spec("A", AudioSource::Silence, AudioSink::Null))
        .unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (n, _, _) = neg_with_peer(
        caps(AudioCodec::Pcmu, SrtpPolicy::Off),
        a.local_rtp_port(),
        peer.local_addr().unwrap().port(),
    );
    a.apply(n);
    tokio::time::sleep(Duration::from_millis(4000)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(3000);
    let mut fired = 0;
    while let Ok(Some(e)) = tokio::time::timeout_at(deadline, events.recv()).await {
        if e.kind == MediaEventKind::MediaTimeout {
            fired += 1;
        }
    }
    assert_eq!(fired, 1, "expected exactly one MediaTimeout");
}

/// Receive-side payload types come from `rx_*`: asymmetric PTs are accepted, the
/// send-side PT is not, DTMF events are decoded, and their sequence numbers are
/// not counted as loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rx_uses_remote_payload_types() {
    let (ev_tx, mut events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let a = engine
        .create_call(spec("A", AudioSource::Silence, AudioSink::Null))
        .unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (mut n, _, _) = neg_with_peer(
        caps(AudioCodec::Pcmu, SrtpPolicy::Off),
        a.local_rtp_port(),
        peer.local_addr().unwrap().port(),
    );
    n.rx_pt = 96;
    n.rx_te = Some((97, 8000));
    a.apply(n);
    let dest = (LO, a.local_rtp_port());
    let ssrc = 0x1234_5678;
    let (mut seq, mut ts) = (100u16, 5000u32);
    let audio = [0xffu8; 160];
    let send_audio = async |count: usize, seq: &mut u16, ts: &mut u32| {
        for _ in 0..count {
            let p = RtpHeader::build(96, *seq, *ts, ssrc, false, &audio);
            peer.send_to(&p, dest).await.unwrap();
            *seq = seq.wrapping_add(1);
            *ts = ts.wrapping_add(160);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    send_audio(10, &mut seq, &mut ts).await;
    // A digit: all packets share one timestamp.
    let ev_ts = ts;
    for p in DtmfGenerator::generate('5', 100, 10, 8000, 20).unwrap() {
        let pkt = RtpHeader::build(97, seq, ev_ts, ssrc, p.marker, &p.payload);
        peer.send_to(&pkt, dest).await.unwrap();
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(160);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    send_audio(10, &mut seq, &mut ts).await;
    // A packet with the PT we *send* with is not valid inbound audio here.
    let p = RtpHeader::build(0, seq, ts, ssrc, false, &audio);
    peer.send_to(&p, dest).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let s = a.stats();
    assert_eq!(s.rx_unknown_pt, 1, "{s:?}");
    assert!(s.rx_packets >= 20, "{s:?}");
    assert_eq!(
        s.jitter.lost, 0,
        "event packets counted as loss: {:?}",
        s.jitter
    );
    let mut digits = Vec::new();
    while let Ok(e) = events.try_recv() {
        if let MediaEventKind::DtmfReceived { digit, .. } = e.kind {
            digits.push(digit);
        }
    }
    assert_eq!(digits, vec!['5']);
}

struct P {
    pt: u8,
    ts: u32,
    marker: bool,
}

async fn capture(sock: &UdpSocket, dur: Duration) -> Vec<(std::time::Instant, P)> {
    let mut v = Vec::new();
    let end = tokio::time::Instant::now() + dur;
    let mut buf = [0u8; 2048];
    while let Ok(Ok((n, _))) = tokio::time::timeout_at(end, sock.recv_from(&mut buf)).await {
        let h = RtpHeader::parse(&buf[..n]).unwrap();
        v.push((
            std::time::Instant::now(),
            P {
                pt: h.payload_type,
                ts: h.timestamp,
                marker: h.marker,
            },
        ));
    }
    v
}

/// A codec change keeps the running RTP timestamp continuous and marks the switch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codec_change_keeps_rtp_timestamp_continuous() {
    let (ev_tx, _events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let a = engine
        .create_call(spec("A", AudioSource::Tone(1000.0), AudioSink::Null))
        .unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pport = peer.local_addr().unwrap().port();
    let (n, _, _) = neg_with_peer(
        caps(AudioCodec::G722, SrtpPolicy::Off),
        a.local_rtp_port(),
        pport,
    );
    a.apply(n);
    let p = capture(&peer, Duration::from_millis(300)).await;
    let (last_at, last) = p.last().unwrap();
    let (last_at, last_ts) = (*last_at, last.ts);
    let (n, _, _) = neg_with_peer(
        caps(AudioCodec::Opus, SrtpPolicy::Off),
        a.local_rtp_port(),
        pport,
    );
    let opus_pt = n.pt;
    a.apply(n);
    let p = capture(&peer, Duration::from_millis(300)).await;
    let (first_at, first) = p.iter().find(|(_, x)| x.pt == opus_pt).unwrap();
    let diff = first.ts.wrapping_sub(last_ts) as i32;
    let ms = first_at.duration_since(last_at).as_millis() as i32;
    assert!(first.marker, "marker on first packet after codec change");
    assert!(
        diff > 0 && diff <= 8 * ms + 2 * 960 + 2000,
        "ts jumped by {diff} over {ms} ms"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dtmf_generation_is_bounded() {
    let r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Silence,
        AudioSource::Silence,
        AudioSink::Null,
    );
    let many = "1".repeat(33);
    assert!(r.a.send_dtmf(&many, DtmfSendMode::Inband, 100, 0).is_err());
    assert!(r.a.send_dtmf("1", DtmfSendMode::Inband, 2001, 0).is_err());
    assert!(r.a.send_dtmf("1", DtmfSendMode::Inband, 100, 6000).is_err());
    assert!(r.a.send_dtmf("12", DtmfSendMode::Inband, 2000, 0).is_ok());
}

/// A file load that finishes after a later `stop_play` must not start playing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_play_file_does_not_overtake_stop() {
    let r = rig(
        AudioCodec::Pcmu,
        SrtpPolicy::Off,
        AudioSource::Silence,
        AudioSource::Silence,
        AudioSink::Null,
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tone.wav");
    {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..48000 * 2 {
            let v = (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin() * 12000.0;
            w.write_sample(v as i16).unwrap();
        }
        w.finalize().unwrap();
    }
    r.a.play(AudioSource::File(path));
    r.a.stop_play();
    let d = detect_for(&r.b, 1000.0, 1.5).await;
    assert!(!d.detected, "stale play overtook stop_play: {d:?}");
}

/// Negotiates `a` <-> `b` (both on this engine) with the real offer/answer machinery.
fn link(a: &CallMediaHandle, b: &CallMediaHandle) {
    let c = caps(AudioCodec::Pcmu, SrtpPolicy::Off);
    let mut sa = MediaSession::new(c.clone(), LO, a.local_rtp_port());
    let mut sb = MediaSession::new(c, LO, b.local_rtp_port());
    let offer = SessionDescription::parse(&sa.create_offer()).unwrap();
    let (answer, neg_b) = sb.apply_offer(&offer).unwrap();
    let neg_a = sa
        .apply_answer(&SessionDescription::parse(&answer).unwrap())
        .unwrap();
    a.apply(neg_a);
    b.apply(neg_b);
}

async fn wait_focus(engine: &MediaEngine, want: &str) {
    for _ in 0..100 {
        if engine.focus().as_deref() == Some(want) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("focus is {:?}, wanted {want}", engine.focus());
}

/// The media focus (and with it the microphone) follows hold / resume: with line B focused,
/// holding B and resuming A must feed A's TX from the capture ring, not leave the mic on B.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn focus_follows_hold_and_resume() {
    let (audio, injector) = AudioDevices::start_injected(48_000);
    let (ev_tx, _events) = unbounded_channel();
    let engine = MediaEngine::start(Some(audio), tokio::runtime::Handle::current(), ev_tx);
    let dev = |id: &str| {
        let mut s = spec(id, AudioSource::Device, AudioSink::Null);
        s.device_bound = true;
        s
    };
    let line_a = engine.create_call(dev("A")).unwrap();
    let line_b = engine.create_call(dev("B")).unwrap();
    let peer_a = engine
        .create_call(spec("PA", AudioSource::Silence, AudioSink::Null))
        .unwrap();
    let peer_b = engine
        .create_call(spec("PB", AudioSource::Silence, AudioSink::Null))
        .unwrap();
    link(&line_a, &peer_a);
    link(&line_b, &peer_b);

    // A 1 kHz "voice" on the capture device, 10 ms at a time.
    let feeder = tokio::spawn(async move {
        let mut n = 0u64;
        loop {
            let chunk: Vec<f32> = (0..480)
                .map(|i| {
                    let t = (n * 480 + i) as f32 / 48_000.0;
                    0.5 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin()
                })
                .collect();
            injector.inject(&chunk);
            n += 1;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });

    // A was answered first (focus A); then B was answered (focus B); A is held.
    wait_focus(&engine, "A").await;
    line_b.set_focus();
    wait_focus(&engine, "B").await;
    line_a.set_hold(true);
    assert!(
        detect_for(&peer_b, 1000.0, 4.0).await.detected,
        "the mic feeds B while B is the live line"
    );

    // Hold B and resume A. No explicit focus call: the engine must move it on its own.
    line_b.set_hold(true);
    line_a.set_hold(false);
    wait_focus(&engine, "A").await;
    assert!(
        detect_for(&peer_a, 1000.0, 4.0).await.detected,
        "A's TX is fed from the capture source after resume"
    );
    assert!(
        !detect_for(&peer_b, 1000.0, 0.8).await.detected,
        "held B must not carry the mic"
    );

    // With no un-held device call left, the focus stays where it was.
    line_a.set_hold(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(engine.focus().as_deref(), Some("A"));

    // The focused call going away hands the focus to a surviving device call.
    line_b.set_hold(false);
    wait_focus(&engine, "B").await;
    line_b.close();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(engine.focus().as_deref(), Some("A"));
    feeder.abort();
}
