//! Clip layer, TX stream and RX timeline tests, on the media_engine rig:
//! A transmits to B on 127.0.0.1 over the virtual clock.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use siple_core::agent::MAX_QUEUED_PLAYS;
use siple_core::config::{AudioSink, AudioSource};
use siple_core::media::{
    CallMediaHandle, CallMediaSpec, ClipSource, MediaEngine, MediaEvent, MediaEventKind, PlayEnd,
    PlayError, PlayRequest,
};
use siple_media::codec::AudioCodec;
use siple_media::sdp::SessionDescription;
use siple_media::sdp_neg::{LocalCaps, MediaSession, SrtpPolicy};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// Generous: the CI/dev machine can be heavily loaded.
const WAIT: Duration = Duration::from_secs(15);

struct Rig {
    engine: MediaEngine,
    events: UnboundedReceiver<MediaEvent>,
    a: CallMediaHandle,
    b: CallMediaHandle,
}

fn rig(a_src: AudioSource) -> Rig {
    let r = rig_unanswered(a_src);
    r.a.mark_answered();
    r.b.mark_answered();
    r
}

/// Like [`rig`], but the calls are not yet answered (clips are held back).
fn rig_unanswered(a_src: AudioSource) -> Rig {
    let (ev_tx, events) = unbounded_channel();
    let engine = MediaEngine::start(None, tokio::runtime::Handle::current(), ev_tx);
    let mk = |id: &str, src: AudioSource| {
        let mut s = CallMediaSpec::new(id, LO, LO);
        s.source = src;
        s.sink = AudioSink::Null;
        engine.create_call(s).unwrap()
    };
    let a = mk("A", a_src);
    let b = mk("B", AudioSource::Silence);
    let caps = LocalCaps {
        codecs: vec![AudioCodec::Pcmu],
        srtp: SrtpPolicy::Off,
        ..Default::default()
    };
    let mut sess_a = MediaSession::new(caps.clone(), LO, a.local_rtp_port());
    let mut sess_b = MediaSession::new(caps, LO, b.local_rtp_port());
    let offer = SessionDescription::parse(&sess_a.create_offer()).unwrap();
    let (answer, neg_b) = sess_b.apply_offer(&offer).unwrap();
    let answer = SessionDescription::parse(&answer).unwrap();
    let neg_a = sess_a.apply_answer(&answer).unwrap();
    a.apply(neg_a);
    b.apply(neg_b);
    Rig {
        engine,
        events,
        a,
        b,
    }
}

fn sine_i16(hz: f32, ms: u32) -> Arc<[i16]> {
    let n = 48 * ms as usize;
    (0..n)
        .map(|i| ((i as f32 * hz * std::f32::consts::TAU / 48000.0).sin() * 10000.0) as i16)
        .collect()
}

fn sine_f32(hz: f32, ms: u32) -> Vec<f32> {
    let n = 48 * ms as usize;
    (0..n)
        .map(|i| (i as f32 * hz * std::f32::consts::TAU / 48000.0).sin() * 0.3)
        .collect()
}

fn pcm_req(data: Arc<[i16]>, queue: bool) -> PlayRequest {
    PlayRequest {
        source: ClipSource::Pcm(data),
        looped: false,
        duration_ms: None,
        queue,
        gain_db: 0.0,
    }
}

fn looped_req(queue: bool) -> PlayRequest {
    PlayRequest {
        looped: true,
        ..pcm_req(sine_i16(900.0, 100), queue)
    }
}

#[derive(Debug, PartialEq)]
struct Fin {
    id: u64,
    reason: PlayEnd,
    played_ms: u64,
}

async fn next_fin(events: &mut UnboundedReceiver<MediaEvent>) -> Fin {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = tokio::time::timeout(left, events.recv())
            .await
            .expect("timed out waiting for PlayFinished")
            .expect("event channel closed");
        if let MediaEventKind::PlayFinished {
            play_id,
            reason,
            played_ms,
            ..
        } = ev.kind
        {
            assert_eq!(ev.call_id, "A");
            return Fin {
                id: play_id,
                reason,
                played_ms,
            };
        }
    }
}

/// Drains for `d` and returns every PlayFinished seen.
async fn fins_within(events: &mut UnboundedReceiver<MediaEvent>, d: Duration) -> Vec<Fin> {
    let mut out = Vec::new();
    let end = Instant::now() + d;
    while let Ok(Some(ev)) =
        tokio::time::timeout(end.saturating_duration_since(Instant::now()), events.recv()).await
    {
        if let MediaEventKind::PlayFinished {
            play_id,
            reason,
            played_ms,
            ..
        } = ev.kind
        {
            out.push(Fin {
                id: play_id,
                reason,
                played_ms,
            });
        }
    }
    out
}

async fn hears(h: &CallMediaHandle, hz: f32, secs: f32) -> bool {
    h.detect_tone(hz, Duration::from_secs_f32(secs), -40.0)
        .await
        .unwrap()
        .detected
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn play_once_completes_once() {
    let mut r = rig(AudioSource::Silence);
    let det = r.b.detect_tone(1000.0, Duration::from_secs(10), -40.0);
    let id =
        r.a.play_clip(pcm_req(sine_i16(1000.0, 600), false))
            .unwrap();
    assert!(id > 0);
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason), (id, PlayEnd::Completed));
    assert!(
        (560..=640).contains(&f.played_ms),
        "played_ms {}",
        f.played_ms
    );
    assert!(det.await.unwrap().detected);
    // Exactly one event per id.
    assert!(
        fins_within(&mut r.events, Duration::from_millis(400))
            .await
            .is_empty()
    );
    // Stopping a finished id is a no-op.
    r.a.stop_play_id(Some(id), false);
    assert!(
        fins_within(&mut r.events, Duration::from_millis(200))
            .await
            .is_empty()
    );
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_order_and_ids() {
    let mut r = rig(AudioSource::Silence);
    let ids: Vec<u64> = (0..3)
        .map(|_| r.a.play_clip(pcm_req(sine_i16(800.0, 200), true)).unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    for id in &ids {
        let f = next_fin(&mut r.events).await;
        assert_eq!((f.id, f.reason), (*id, PlayEnd::Completed));
    }
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_marks_old_replaced() {
    let mut r = rig(AudioSource::Silence);
    let first = r.a.play_clip(looped_req(false)).unwrap();
    let queued = r.a.play_clip(looped_req(true)).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second =
        r.a.play_clip(pcm_req(sine_i16(1000.0, 200), false))
            .unwrap();
    let f1 = next_fin(&mut r.events).await;
    let f2 = next_fin(&mut r.events).await;
    assert_eq!((f1.id, f1.reason), (first, PlayEnd::Replaced));
    assert_eq!((f2.id, f2.reason), (queued, PlayEnd::Replaced));
    let f3 = next_fin(&mut r.events).await;
    assert_eq!((f3.id, f3.reason), (second, PlayEnd::Completed));
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_by_id_and_clear_queue() {
    let mut r = rig(AudioSource::Silence);
    let ids: Vec<u64> = (0..4)
        .map(|i| r.a.play_clip(looped_req(i > 0)).unwrap())
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Unknown id: no-op.
    r.a.stop_play_id(Some(987_654), false);
    // Stop a queued one.
    r.a.stop_play_id(Some(ids[2]), false);
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason, f.played_ms), (ids[2], PlayEnd::Stopped, 0));
    // Stop the current one: the next queued clip starts.
    r.a.stop_play_id(Some(ids[0]), false);
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason), (ids[0], PlayEnd::Stopped));
    assert!(f.played_ms > 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(r.a.stats().play_current, Some(ids[1]));
    // Stop all.
    r.a.stop_play_id(None, true);
    let rest = fins_within(&mut r.events, Duration::from_millis(500)).await;
    let got: Vec<(u64, PlayEnd)> = rest.iter().map(|f| (f.id, f.reason)).collect();
    assert_eq!(
        got,
        vec![(ids[1], PlayEnd::Stopped), (ids[3], PlayEnd::Stopped)]
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let st = r.a.stats();
    assert_eq!((st.play_current, st.play_queued), (None, 0));
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clip_pauses_while_held() {
    let mut r = rig(AudioSource::Silence);
    let id =
        r.a.play_clip(pcm_req(sine_i16(1000.0, 1500), false))
            .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    r.a.set_hold(true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Held for longer than the remaining clip would take.
    assert!(
        fins_within(&mut r.events, Duration::from_millis(2000))
            .await
            .is_empty()
    );
    r.a.set_hold(false);
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason), (id, PlayEnd::Completed));
    assert!(
        (1400..=1600).contains(&f.played_ms),
        "played_ms {}",
        f.played_ms
    );
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rx_ms_tracks_wall_time() {
    let r = rig(AudioSource::Silence);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (t0, m0) = (Instant::now(), r.b.rx_ms());
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (dt, dm) = (t0.elapsed().as_millis() as u64, r.b.rx_ms() - m0);
    // The virtual clock skips ahead when the thread is starved, so the low side is loose.
    assert!(dm * 2 >= dt && dm <= dt + 300, "wall {dt} ms, rx_ms +{dm}");
    assert_eq!(dm % 20, 0);
    let st = r.b.stats();
    assert!(st.rx_ms > 0 && st.rx_ms <= r.b.rx_ms());
    // The RX tap ring is only drained once a hub consumer exists; the drop counter is
    // therefore not asserted here.
    let _ = st.rx_tap_dropped_ms;
    drop(r.engine);
}

/// A stalled hub overflows the RX tap ring: `rx_tap_dropped_ms` (cumulative, for stats) must
/// show it even though the hub drains its own counter, and the hub timeline stays aligned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rx_tap_overflow_is_counted_and_timeline_stays_aligned() {
    let r = rig(AudioSource::Silence);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(r.b.stats().rx_tap_dropped_ms, 0);
    // The ring holds 2 s; stall well past it.
    r.b.hub().stall_for_tests(Duration::from_millis(3500));
    tokio::time::sleep(Duration::from_millis(4200)).await;
    let st = r.b.stats();
    // Nominally ~1.5 s (3.5 s stall minus the 2 s ring); only require that drops were counted
    // so a loaded machine cannot flake this.
    assert!(
        st.rx_tap_dropped_ms > 0,
        "dropped {} ms",
        st.rx_tap_dropped_ms
    );
    // Stays visible after the hub drained and zero-filled its own counter.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let st2 = r.b.stats();
    assert!(st2.rx_tap_dropped_ms >= st.rx_tap_dropped_ms);
    // The hub must catch up with the media timeline (zero-filling the dropped span): poll up to
    // 1 s for the two to agree within 200 ms.
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let (hub, media) = (r.b.hub().rx_ms(), r.b.rx_ms());
        if hub.abs_diff(media) <= 200 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hub rx_ms {hub} vs media rx_ms {media}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tx_stream_attach_clear_detach() {
    let r = rig(AudioSource::Tone(1500.0));
    assert!(hears(&r.b, 1500.0, 5.0).await, "base source audible");

    let mut s = r.a.attach_tx_stream();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(r.a.stats().tx_stream);
    let tone = sine_f32(1000.0, 1500);
    assert_eq!(s.push(&tone), tone.len());
    assert_eq!(s.vacant() + s.buffered(), 96_000);
    assert!(hears(&r.b, 1000.0, 5.0).await, "stream audible");
    assert!(s.consumed() > 0);

    // Let the ring run dry: underruns are counted and the base tone stays suppressed.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert!(s.underrun_samples() > 0);
    assert!(
        !hears(&r.b, 1500.0, 0.6).await,
        "base source must stay replaced"
    );

    // Clear drops what is buffered.
    s.push(&sine_f32(1000.0, 1800));
    s.request_clear();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.buffered(), 0);
    assert!(s.cleared_samples() > 0);
    assert_eq!(s.consumed(), s.pushed());

    // Detach restores the base source.
    r.a.detach_tx_stream();
    assert!(hears(&r.b, 1500.0, 5.0).await, "base source restored");
    assert!(!r.a.stats().tx_stream);
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clip_overrides_stream_and_gain() {
    let mut r = rig(AudioSource::Silence);
    let mut s = r.a.attach_tx_stream();
    s.push(&sine_f32(1000.0, 1900));
    let id = r.a.play_clip(PlayRequest {
        looped: true,
        duration_ms: Some(500),
        gain_db: -6.0,
        ..pcm_req(sine_i16(1700.0, 100), false)
    });
    let id = id.unwrap();
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason), (id, PlayEnd::Completed));
    assert!(
        (460..=540).contains(&f.played_ms),
        "played_ms {}",
        f.played_ms
    );
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_ends_current_and_queued() {
    let mut r = rig(AudioSource::Silence);
    let ids: Vec<u64> = (0..3)
        .map(|i| r.a.play_clip(looped_req(i > 0)).unwrap())
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    r.a.close();
    let fins = fins_within(&mut r.events, Duration::from_millis(1000)).await;
    let got: Vec<(u64, PlayEnd)> = fins.iter().map(|f| (f.id, f.reason)).collect();
    assert_eq!(
        got,
        ids.iter()
            .map(|i| (*i, PlayEnd::CallEnded))
            .collect::<Vec<_>>()
    );
    assert!(r.a.hub().is_closed());
    // Playing on a closed handle fails instead of vanishing (no waiter can hang).
    assert_eq!(
        r.a.play_clip(pcm_req(sine_i16(1000.0, 100), false))
            .unwrap_err(),
        PlayError::Closed
    );
    assert_eq!(
        r.a.play_clip(pcm_req(sine_i16(1000.0, 100), true))
            .unwrap_err(),
        PlayError::Closed
    );
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clip_sent_before_close_still_gets_call_ended() {
    let mut r = rig(AudioSource::Silence);
    let id =
        r.a.play_clip(pcm_req(sine_i16(1000.0, 5000), false))
            .unwrap();
    r.a.close();
    let fins = fins_within(&mut r.events, Duration::from_millis(1000)).await;
    assert!(
        fins.iter()
            .any(|f| f.id == id && f.reason == PlayEnd::CallEnded)
    );
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_clear_keeps_audio_pushed_after_request() {
    let r = rig(AudioSource::Silence);
    let mut s = r.a.attach_tx_stream();
    tokio::time::sleep(Duration::from_millis(250)).await;
    // Push A, clear, push B with no media tick in between (pushes are back to back).
    let a = sine_f32(1000.0, 1000);
    let b = sine_f32(1000.0, 200);
    s.push(&a);
    s.request_clear();
    s.push(&b);
    let (pushed, mut ok) = (s.pushed(), false);
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if s.consumed() == pushed {
            ok = true;
            break;
        }
    }
    assert!(ok, "consumed {} of {}", s.consumed(), pushed);
    // A was discarded (minus anything the thread had already played), B was not.
    assert!(s.cleared_samples() <= a.len() as u64);
    assert!(s.cleared_samples() + (b.len() as u64) <= pushed);
    assert!(s.consumed() - s.cleared_samples() >= b.len() as u64);
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_full_is_refused() {
    let mut r = rig(AudioSource::Silence);
    // One current clip plus MAX_QUEUED_PLAYS waiting.
    let mut ids = Vec::new();
    for _ in 0..=MAX_QUEUED_PLAYS {
        ids.push(r.a.play_clip(looped_req(true)).unwrap());
    }
    assert_eq!(
        r.a.play_clip(looped_req(true)).unwrap_err(),
        PlayError::QueueFull
    );
    // A replacing play is still accepted.
    let id =
        r.a.play_clip(pcm_req(sine_i16(1000.0, 100), false))
            .unwrap();
    let fins = fins_within(&mut r.events, Duration::from_millis(800)).await;
    let replaced = fins
        .iter()
        .filter(|f| f.reason == PlayEnd::Replaced)
        .count();
    assert_eq!(replaced, ids.len());
    assert!(
        fins.iter()
            .any(|f| f.id == id && f.reason == PlayEnd::Completed)
    );
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_stop_for_a_finished_play_keeps_the_queue() {
    let mut r = rig(AudioSource::Silence);
    let a = r.a.play_clip(pcm_req(sine_i16(800.0, 200), false)).unwrap();
    let b = r.a.play_clip(looped_req(true)).unwrap();
    let c = r.a.play_clip(looped_req(true)).unwrap();
    // Hold B and C back by stopping nothing: A finishes by itself, B starts.
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason), (a, PlayEnd::Completed));
    tokio::time::sleep(Duration::from_millis(100)).await;
    // A late stop for the finished A (default clear_queue = true) must not touch B or C.
    r.a.stop_play_id(Some(a), true);
    // An unknown id behaves the same.
    r.a.stop_play_id(Some(987_654), true);
    assert!(
        fins_within(&mut r.events, Duration::from_millis(400))
            .await
            .is_empty()
    );
    let st = r.a.stats();
    assert_eq!((st.play_current, st.play_queued), (Some(b), 1));
    // Stopping the current clip by its id with clear_queue still clears C.
    r.a.stop_play_id(Some(b), true);
    let got: Vec<(u64, PlayEnd)> = fins_within(&mut r.events, Duration::from_millis(500))
        .await
        .iter()
        .map(|f| (f.id, f.reason))
        .collect();
    assert_eq!(got, vec![(b, PlayEnd::Stopped), (c, PlayEnd::Stopped)]);
    drop(r.engine);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clips_wait_for_the_answer() {
    let mut r = rig_unanswered(AudioSource::Tone(1500.0));
    assert!(!r.a.is_answered());
    let id =
        r.a.play_clip(pcm_req(sine_i16(1000.0, 300), false))
            .unwrap();
    // Negotiated media (early media) is flowing, yet the clip stays queued and the base
    // source keeps playing.
    assert!(hears(&r.b, 1500.0, 5.0).await, "base source audible");
    assert!(
        fins_within(&mut r.events, Duration::from_millis(800))
            .await
            .is_empty(),
        "the clip must not play before the answer"
    );
    assert_eq!(r.a.stats().play_current, Some(id));
    r.a.mark_answered();
    let f = next_fin(&mut r.events).await;
    assert_eq!((f.id, f.reason), (id, PlayEnd::Completed));
    assert!(
        (260..=340).contains(&f.played_ms),
        "played_ms {}",
        f.played_ms
    );
    drop(r.engine);
}
