//! `call.stream` end to end on the two-account loopback rig: alice dials bob (alice sends a
//! 1000 Hz tone, bob's source is silence), the stream attaches to bob's call.
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use siple_core::App;
use siple_core::agent::stream::{FRAME_AUDIO, FRAME_JSON, encode_frame, read_client_frame};
use siple_core::control::client::{ClientError, ClientStream};
use tokio::io::{AsyncRead, AsyncWriteExt};

const RATE: u32 = 16_000;

fn tone(hz: f32, ms: usize) -> Vec<u8> {
    let n = RATE as usize * ms / 1000;
    (0..n)
        .flat_map(|i| {
            let v = (2.0 * std::f32::consts::PI * hz * i as f32 / RATE as f32).sin() * 0.5;
            ((v * 32767.0) as i16).to_le_bytes()
        })
        .collect()
}

/// The 300 ms window of s16le `pcm` with the strongest 1000 Hz. A scheduling stall on a loaded
/// machine drops RX frames and leaves clicks that spread the tone's energy over the whole capture,
/// but a delivered tone must still show up cleanly somewhere in it.
fn cleanest_window(pcm: &[u8]) -> &[u8] {
    let win = (RATE as usize * 300 / 1000 * 2).min(pcm.len());
    pcm.chunks(win)
        .filter(|c| c.len() == win)
        .max_by(|a, b| goertzel(a, 1000.0).total_cmp(&goertzel(b, 1000.0)))
        .unwrap_or(pcm)
}

fn goertzel(pcm: &[u8], hz: f32) -> f32 {
    let s: Vec<f32> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| f32::from(i16::from_le_bytes([c[0], c[1]])) / 32768.0)
        .collect();
    let w = 2.0 * std::f32::consts::PI * hz / RATE as f32;
    let (mut s1, mut s2) = (0f32, 0f32);
    for x in &s {
        let s0 = x + 2.0 * w.cos() * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let p = s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2;
    let energy: f32 = s.iter().map(|x| x * x).sum::<f32>() * s.len() as f32 / 2.0;
    if energy > 0.0 { p / energy } else { 0.0 }
}

async fn open(r: &Rig, call: &str, extra: Value) -> ClientStream {
    let mut p = json!({"call_id": call, "sample_rate": RATE});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    client(&r.app, "stream")
        .await
        .into_stream(p)
        .await
        .unwrap_or_else(|e| panic!("call.stream failed: {e}"))
}

async fn send_audio(wr: &mut (impl AsyncWriteExt + Unpin), pcm: &[u8], paced: bool) {
    for f in pcm.chunks(640) {
        if wr.write_all(&encode_frame(FRAME_AUDIO, f)).await.is_err() {
            return; // stream closed: the caller's RX / detect assertions report it
        }
        if paced {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn send_cmd(wr: &mut (impl AsyncWriteExt + Unpin), cmd: Value) {
    wr.write_all(&encode_frame(FRAME_JSON, cmd.to_string().as_bytes()))
        .await
        .unwrap();
}

/// Next JSON event, skipping audio; `None` at EOF.
async fn next_event(rd: &mut (impl AsyncRead + Unpin)) -> Option<Value> {
    loop {
        let f = tokio::time::timeout(Duration::from_secs(15), read_client_frame(rd))
            .await
            .expect("timed out waiting for a stream frame")
            .expect("stream protocol error");
        if let (FRAME_JSON, b) = f? {
            return Some(serde_json::from_slice(&b).unwrap());
        }
    }
}

async fn expect_event(rd: &mut (impl AsyncRead + Unpin), name: &str) -> Value {
    loop {
        let v = next_event(rd)
            .await
            .unwrap_or_else(|| panic!("EOF before `{name}`"));
        if v["event"] == name {
            return v;
        }
    }
}

/// Starts `call.detect_tone` on its own connection right away (the returned task is already
/// running), so detection overlaps the tone instead of starting when the result is awaited.
async fn spawn_detect(app: &App, call: &str, hz: u32) -> tokio::task::JoinHandle<Value> {
    let mut c = client(app, "detect").await;
    let call = call.to_string();
    let h = tokio::spawn(async move {
        c.call(
            "call.detect_tone",
            json!({"call_id": call, "freq_hz": hz, "timeout_ms": 8000}),
        )
        .await
        .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    h
}

#[tokio::test]
async fn bot_tx_tone_is_heard_by_the_far_side() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;
    let mut st = open(&r, &b_call, json!({"rx": false})).await;
    assert_eq!(st.info["frame_bytes"], 640);
    let mut wr = st.wr;
    let pump = tokio::spawn(async move {
        send_audio(&mut wr, &tone(600.0, 4000), true).await;
        wr
    });
    let d =
        r.a.call(
            "call.detect_tone",
            json!({"call_id": a_call, "freq_hz": 600, "timeout_ms": 8000}),
        )
        .await
        .unwrap();
    assert_eq!(d["detected"], true, "{d}");
    let _wr = pump.await.unwrap();
    let _ = &mut st.rd;
}

#[tokio::test]
async fn mark_arrives_after_playout_and_clear_drops_audio() {
    let mut r = rig("").await;
    let (_a, b_call) = establish(&mut r).await;
    let mut st = open(&r, &b_call, json!({"rx": false})).await;

    // 1.2 s sent at once: the mark can only come once it has played out.
    let t0 = Instant::now();
    send_audio(&mut st.wr, &tone(600.0, 1200), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"mark","name":"m1"})).await;
    let m = expect_event(&mut st.rd, "mark").await;
    assert_eq!(m["name"], "m1");
    assert!(
        t0.elapsed() >= Duration::from_millis(900),
        "{:?}",
        t0.elapsed()
    );
    assert!(t0.elapsed() < Duration::from_secs(8));

    // Clear: queued audio and its mark are dropped promptly.
    let t_sent = Instant::now();
    send_audio(&mut st.wr, &tone(600.0, 1800), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"mark","name":"m2"})).await;
    let t1 = Instant::now();
    send_cmd(&mut st.wr, json!({"cmd":"clear"})).await;
    let m = expect_event(&mut st.rd, "mark").await;
    assert_eq!(m["name"], "m2");
    let c = expect_event(&mut st.rd, "cleared").await;
    let dropped = c["dropped_ms"].as_u64().unwrap();
    // Playout runs in real time, so what is left to drop shrinks with the time the clear took to
    // arrive; on a loaded machine that is not a fixed 900 ms. (Frame granularity: 100 ms slack.)
    let played = t_sent.elapsed().as_millis() as u64;
    let min_dropped = 1800u64.saturating_sub(played + 100).clamp(1, 900);
    assert!(
        (min_dropped..=1800).contains(&dropped),
        "dropped_ms {dropped} (min {min_dropped}, {played} ms since send)"
    );
    assert!(t1.elapsed() < Duration::from_secs(10));

    // The stream keeps working after a clear.
    send_cmd(&mut st.wr, json!({"cmd":"stats"})).await;
    let s = expect_event(&mut st.rd, "stats").await;
    assert!(s["tx_buffer_ms"].as_u64().unwrap() < 200, "{s}");
    send_audio(&mut st.wr, &tone(600.0, 200), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"mark","name":"m3"})).await;
    assert_eq!(expect_event(&mut st.rd, "mark").await["name"], "m3");
}

#[tokio::test]
async fn far_side_hangup_sends_ended_then_eof_and_frees_the_call() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;
    let mut st = open(&r, &b_call, json!({"rx": false})).await;
    r.a.call("call.hangup", json!({"call_id": a_call}))
        .await
        .unwrap();
    let e = expect_event(&mut st.rd, "ended").await;
    assert_eq!(e["reason"], "call_ended");
    assert!(next_event(&mut st.rd).await.is_none(), "expected EOF");
    // Nothing is left registered.
    let err =
        r.b.call("call.stream", json!({"call_id": b_call}))
            .await
            .unwrap_err();
    assert!(
        matches!(err, ClientError::Api(ref e) if e.to_string().contains("ended")),
        "{err}"
    );
}

#[tokio::test]
async fn preconditions_and_reuse() {
    let mut r = rig("").await;
    // Ringing call -> invalid_state, connection stays usable.
    let b_seq = seq(&mut r.b).await;
    r.a.call(
        "call.dial",
        json!({"account":"alice","target":r.bob_uri,"source":"tone:1000","sink":"null"}),
    )
    .await
    .unwrap();
    let ringing =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":b_seq,"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let ring_call = ringing["call"]["id"].as_str().unwrap().to_string();
    let mut c = client(&r.app, "pre").await;
    let e = c
        .call("call.stream", json!({"call_id": ring_call}))
        .await
        .unwrap_err();
    assert!(
        matches!(e, ClientError::Api(ref a) if a.kind == siple_core::control::ErrorKind::InvalidState),
        "{e}"
    );
    assert!(c.call("ping", Value::Null).await.is_ok());
    // Unknown call, bad params, no direction.
    for p in [
        json!({"call_id": "c999"}),
        json!({"call_id": ring_call, "sample_rate": 44100}),
        json!({"call_id": ring_call, "rx": false, "tx": false}),
    ] {
        assert!(c.call("call.stream", p).await.is_err());
        assert!(c.call("ping", Value::Null).await.is_ok());
    }
    r.b.call("call.answer", json!({"call_id": ring_call}))
        .await
        .unwrap();
    r.a.call(
        "call.wait_for",
        json!({"account":"alice","state":"active","timeout_ms":30_000}),
    )
    .await
    .unwrap();

    // Active now: first stream works; a second one is refused; in-flight request refused.
    let mut st = open(&r, &ring_call, json!({"rx": false})).await;
    let e = c
        .call("call.stream", json!({"call_id": ring_call}))
        .await
        .unwrap_err();
    assert!(
        matches!(e, ClientError::Api(ref a) if a.to_string().contains("already")),
        "{e}"
    );
    assert!(c.call("ping", Value::Null).await.is_ok());

    // Events: open / closed with a reason.
    let seq0 = seq(&mut c).await;
    send_cmd(&mut st.wr, json!({"cmd":"close"})).await;
    let e = expect_event(&mut st.rd, "ended").await;
    assert_eq!(e["reason"], "closed");
    assert!(next_event(&mut st.rd).await.is_none());
    let ev = c
        .call(
            "events.poll",
            json!({"since": seq0 - 1, "kinds": ["call.stream"], "timeout_ms": 5000}),
        )
        .await
        .unwrap();
    let closed = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["data"]["state"] == "closed")
        .unwrap_or_else(|| panic!("no closed event: {ev}"));
    assert_eq!(closed["data"]["reason"], "client_close");

    // The call is free again (I4) and the base source is back: reopen works.
    let st2 = open(&r, &ring_call, json!({"rx": false})).await;
    drop(st2);
}

#[tokio::test]
async fn hangup_on_close_is_opt_in() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;
    // Default: dropping the connection leaves the call up.
    let st = open(&r, &b_call, json!({"rx": false})).await;
    drop(st);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let g =
        r.b.call("call.get", json!({"call_id": b_call}))
            .await
            .unwrap();
    assert_eq!(g["state"], "active");
    // Opt-in: closing hangs the call up.
    let mut st = open(&r, &b_call, json!({"rx": false, "hangup_on_close": true})).await;
    send_cmd(&mut st.wr, json!({"cmd":"close"})).await;
    let w =
        r.a.call(
            "call.wait_for",
            json!({"call_id": a_call, "state": "ended", "timeout_ms": 8000}),
        )
        .await;
    assert!(w.is_ok(), "{w:?}");
}

#[tokio::test]
async fn dtmf_and_state_events_are_forwarded() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;
    let mut st = open(&r, &b_call, json!({"rx": false})).await;
    r.a.call("call.dtmf", json!({"call_id": a_call, "digits": "5"}))
        .await
        .unwrap();
    let d = expect_event(&mut st.rd, "dtmf").await;
    assert_eq!(d["digit"], "5");
}

/// Depends on the RX hub (B3): until it is merged `subscribe()` fails and the stream falls back
/// to TX-only with an `rx_unavailable` notice, so this test fails.
#[tokio::test]
async fn rx_frames_carry_the_far_side_tone() {
    let mut r = rig("").await;
    let (_a, b_call) = establish(&mut r).await;
    let mut st = open(&r, &b_call, json!({"tx": false})).await;
    let mut pcm = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(40);
    while pcm.len() < 640 * 100 && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), read_client_frame(&mut st.rd)).await {
            Ok(Ok(Some((FRAME_AUDIO, b)))) => {
                assert_eq!(b.len(), 640, "frames are exactly frame_ms");
                pcm.extend(b);
            }
            Ok(Ok(Some((_, b)))) => {
                let v: Value = serde_json::from_slice(&b).unwrap();
                assert_ne!(v["event"], "rx_unavailable", "{v}");
            }
            other => panic!("stream ended early: {other:?}"),
        }
    }
    assert!(
        pcm.len() >= 640 * 100,
        "only {} bytes of RX audio",
        pcm.len()
    );
    // Skip the first 0.5 s (ramp-up), then expect 1000 Hz and not 2500 Hz.
    let tail = cleanest_window(&pcm[16_000..]);
    assert!(
        goertzel(tail, 1000.0) > 0.5,
        "1000 Hz ratio {}",
        goertzel(tail, 1000.0)
    );
    assert!(goertzel(tail, 2500.0) < 0.05);
}

#[test]
fn goertzel_helper_separates_tones() {
    let t = tone(1000.0, 500);
    assert!(goertzel(&t, 1000.0) > 0.5);
    assert!(goertzel(&t, 2500.0) < 0.05);
}

#[tokio::test]
async fn marks_after_a_clear_fire_at_the_playout_position() {
    let mut r = rig("").await;
    let (_a, b_call) = establish(&mut r).await;
    let mut st = open(&r, &b_call, json!({"rx": false})).await;

    // Buffer audio, clear it, then send 800 ms and a mark: the mark must wait for those
    // 800 ms to play out (a double-counted clear would fire it early).
    send_audio(&mut st.wr, &tone(600.0, 1800), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"clear"})).await;
    let c = expect_event(&mut st.rd, "cleared").await;
    assert!(c["dropped_ms"].as_u64().unwrap() > 0, "{c}");
    let t0 = Instant::now();
    send_audio(&mut st.wr, &tone(600.0, 800), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"mark","name":"after"})).await;
    let m = expect_event(&mut st.rd, "mark").await;
    assert_eq!(m["name"], "after");
    assert!(
        t0.elapsed() >= Duration::from_millis(650),
        "mark fired after only {:?}",
        t0.elapsed()
    );
    assert!(t0.elapsed() < Duration::from_secs(5));

    // Two clears in a row, then another mark.
    send_audio(&mut st.wr, &tone(600.0, 1000), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"clear"})).await;
    expect_event(&mut st.rd, "cleared").await;
    send_cmd(&mut st.wr, json!({"cmd":"clear"})).await;
    let c = expect_event(&mut st.rd, "cleared").await;
    assert_eq!(c["dropped_ms"], 0, "{c}");
    let t1 = Instant::now();
    send_audio(&mut st.wr, &tone(600.0, 500), false).await;
    send_cmd(&mut st.wr, json!({"cmd":"mark","name":"again"})).await;
    expect_event(&mut st.rd, "mark").await;
    assert!(
        t1.elapsed() >= Duration::from_millis(350),
        "{:?}",
        t1.elapsed()
    );

    // Unknown commands echo a truncated name.
    send_cmd(&mut st.wr, json!({"cmd": "x".repeat(5000)})).await;
    let e = expect_event(&mut st.rd, "error").await;
    assert!(e["message"].as_str().unwrap().len() < 200, "{e}");
}

// ---- formats, TCP transport ---------------------------------------------------------------

use siple_core::control::client::ControlClient;
use siple_media::g711::{alaw_to_linear, linear_to_alaw, linear_to_ulaw, ulaw_to_linear};

fn encode_tone(enc: &str, rate: u32, hz: f32, ms: usize) -> Vec<u8> {
    let n = rate as usize * ms / 1000;
    let mut out = Vec::new();
    for i in 0..n {
        let v = (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5;
        let s = (v * 32767.0) as i16;
        match enc {
            "s16le" => out.extend(s.to_le_bytes()),
            "f32le" => out.extend(v.to_le_bytes()),
            "mulaw" => out.push(linear_to_ulaw(s)),
            "alaw" => out.push(linear_to_alaw(s)),
            other => panic!("{other}"),
        }
    }
    out
}

fn decode_pcm(enc: &str, bytes: &[u8]) -> Vec<i16> {
    match enc {
        "s16le" => bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect(),
        "f32le" => bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| (f32::from_le_bytes([c[0], c[1], c[2], c[3]]) * 32767.0) as i16)
            .collect(),
        "mulaw" => bytes.iter().map(|&b| ulaw_to_linear(b)).collect(),
        "alaw" => bytes.iter().map(|&b| alaw_to_linear(b)).collect(),
        other => panic!("{other}"),
    }
}

/// Reads RX audio frames until `want` frames arrived, checking the exact frame size.
async fn read_frames(st: &mut ClientStream, frame_bytes: usize, want: usize) -> Vec<u8> {
    let mut pcm = Vec::new();
    let mut frames = 0;
    while frames < want {
        match tokio::time::timeout(Duration::from_secs(5), read_client_frame(&mut st.rd)).await {
            Ok(Ok(Some((FRAME_AUDIO, b)))) => {
                assert_eq!(b.len(), frame_bytes, "frames are exactly frame_ms");
                pcm.extend(b);
                frames += 1;
            }
            Ok(Ok(Some((_, b)))) => {
                let v: Value = serde_json::from_slice(&b).unwrap();
                assert_ne!(v["event"], "rx_unavailable", "{v}");
            }
            other => panic!("stream ended early: {other:?}"),
        }
    }
    pcm
}

/// RX (alice's 1000 Hz arrives at bob) and TX (a 600 Hz tone reaches alice) in one format.
async fn round_trip(r: &mut Rig, enc: &str, rate: u32, frame_ms: u32) {
    let (a_call, b_call) = establish(r).await;
    let mut st = open(
        r,
        &b_call,
        json!({"sample_rate": rate, "encoding": enc, "frame_ms": frame_ms}),
    )
    .await;
    let sample_bytes = match enc {
        "f32le" => 4,
        "s16le" => 2,
        _ => 1,
    };
    let frame_bytes = (rate * frame_ms / 1000) as usize * sample_bytes;
    assert_eq!(st.info["frame_bytes"], frame_bytes, "{}", st.info);
    assert_eq!(st.info["encoding"], enc);
    assert_eq!(st.info["sample_rate"], rate);

    // TX: paced frames of exactly frame_bytes; alice must hear 600 Hz.
    let tx = encode_tone(enc, rate, 600.0, 4000);
    let mut wr = st.wr;
    let pump = tokio::spawn(async move {
        for f in tx.chunks(frame_bytes) {
            // A closed stream is reported by the RX / detect assertions below, not here.
            if wr.write_all(&encode_frame(FRAME_AUDIO, f)).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(frame_ms as u64)).await;
        }
        wr
    });
    st.wr = Box::new(tokio::io::sink());
    let det = spawn_detect(&r.app, &a_call, 600).await;
    // RX: ≥ 1.5 s of frames, skipping the first 0.5 s.
    let want = 1500 / frame_ms as usize;
    let rx = read_frames(&mut st, frame_bytes, want).await;
    let samples = decode_pcm(enc, &rx);
    let tail = &samples[samples.len() / 3..];
    // Judge the cleanest 300 ms window: a scheduling stall on a loaded machine drops RX frames and
    // leaves clicks that spread the tone's energy, but a delivered tone must still show up
    // cleanly somewhere in the capture.
    let win = (rate as usize * 300 / 1000).min(tail.len());
    let (ratio, other) = tail
        .chunks(win)
        .filter(|c| c.len() == win)
        .map(|c| {
            (
                siple_media::goertzel::detect(c, rate, 1000.0).0,
                siple_media::goertzel::detect(c, rate, 2500.0).0,
            )
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .expect("RX audio shorter than one window");
    assert!(
        ratio > 0.4,
        "{enc}@{rate}/{frame_ms}: 1000 Hz ratio {ratio}"
    );
    assert!(
        other < 0.05,
        "{enc}@{rate}/{frame_ms}: 2500 Hz ratio {other}"
    );
    let d = det.await.unwrap();
    assert_eq!(d["detected"], true, "{enc}@{rate}/{frame_ms}: {d}");
    let _ = pump.await.unwrap();
}

#[tokio::test]
async fn mulaw_8k_frame_40_round_trip() {
    let mut r = rig("").await;
    round_trip(&mut r, "mulaw", 8_000, 40).await;
}

#[tokio::test]
async fn f32le_48k_frame_10_round_trip() {
    let mut r = rig("").await;
    round_trip(&mut r, "f32le", 48_000, 10).await;
}

/// The stream over the loopback TCP listener with the token, against the real app.
#[tokio::test]
async fn stream_over_tcp_with_token() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let text = format!(
        "[control]\ntcp_listen = \"127.0.0.1:0\"\n{}{}",
        acct("alice", pa, ""),
        acct("bob", pb, "")
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let app = start_app(&path).await;
    let tcp = app.control_endpoints().await.tcp.expect("tcp endpoint");
    let a = client(&app, "a").await;
    let b = client(&app, "b").await;
    let mut r = Rig {
        dir,
        app,
        a,
        b,
        pa,
        pb,
        bob_uri: format!("sip:bob@127.0.0.1:{pb}"),
    };
    // A wrong token is refused.
    let mut bad = ControlClient::connect_tcp(&tcp).await.unwrap();
    assert!(bad.hello(Some("nope"), "bad").await.is_err());

    let (a_call, b_call) = establish(&mut r).await;
    let mut c = ControlClient::connect_tcp(&tcp).await.unwrap();
    c.hello(Some(r.app.token().expose()), "tcp-stream")
        .await
        .unwrap();
    let mut st = c
        .into_stream(json!({"call_id": b_call, "sample_rate": RATE}))
        .await
        .unwrap();
    assert_eq!(st.info["frame_bytes"], 640);
    let mut wr = st.wr;
    let pump = tokio::spawn(async move {
        send_audio(&mut wr, &tone(600.0, 4000), true).await;
        wr
    });
    st.wr = Box::new(tokio::io::sink());
    let det = spawn_detect(&r.app, &a_call, 600).await;
    let rx = read_frames(&mut st, 640, 100).await;
    assert!(goertzel(cleanest_window(&rx[(rx.len() / 3) & !1..]), 1000.0) > 0.4);
    assert_eq!(det.await.unwrap()["detected"], true);
    let _ = pump.await.unwrap();
}

#[tokio::test]
async fn open_reports_effective_tx_buffer_and_closed_event_repeats_the_format() {
    let mut r = rig("").await;
    let (_a, b_call) = establish(&mut r).await;
    let s = seq(&mut r.b).await;
    let mut st = open(
        &r,
        &b_call,
        json!({"tx_buffer_ms": 10_000, "encoding": "mulaw", "sample_rate": 8000, "rx": false}),
    )
    .await;
    assert_eq!(st.info["tx_buffer_ms"], 2000, "{}", st.info);
    send_cmd(&mut st.wr, json!({"cmd":"close"})).await;
    let _ = st.wr.shutdown().await;
    let mut cursor = s;
    let mut kinds = Vec::new();
    for _ in 0..10 {
        let ev =
            r.b.call(
                "events.poll",
                json!({"since":cursor,"kinds":["call.stream"],"call_id":b_call,"timeout_ms":1000}),
            )
            .await
            .unwrap();
        for e in ev["events"].as_array().unwrap() {
            kinds.push(e["data"].clone());
        }
        cursor = ev["next_seq"].as_u64().unwrap();
        if kinds.iter().any(|d| d["state"] == "closed") {
            break;
        }
    }
    let open = kinds.iter().find(|d| d["state"] == "open").expect("open");
    let closed = kinds
        .iter()
        .find(|d| d["state"] == "closed")
        .expect("closed");
    for d in [open, closed] {
        assert_eq!(d["rx"], false, "{d}");
        assert_eq!(d["tx"], true, "{d}");
        assert_eq!(d["sample_rate"], 8000, "{d}");
        assert_eq!(d["encoding"], "mulaw", "{d}");
    }
}

/// A client that sends `call.stream {hangup_on_close}` and vanishes without reading anything:
/// whether the server loses the result-line write (abort path) or completes the upgrade and
/// sees EOF, the call must end and exactly one `closed` event must be emitted.
#[tokio::test]
async fn vanishing_client_hangs_up_and_closes_exactly_once() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;
    let s = seq(&mut r.b).await;
    let ep = r.app.control_endpoints().await;
    let mut raw = tokio::net::UnixStream::connect(ep.unix_socket.unwrap())
        .await
        .unwrap();
    let hello = json!({"jsonrpc":"2.0","id":1,"method":"hello",
        "params":{"client":"raw","token":r.app.token().expose()}});
    let stream = json!({"jsonrpc":"2.0","id":2,"method":"call.stream",
        "params":{"call_id":b_call,"sample_rate":RATE,"rx":false,"hangup_on_close":true}});
    raw.write_all(format!("{hello}\n{stream}\n").as_bytes())
        .await
        .unwrap();
    drop(raw);
    let w =
        r.a.call(
            "call.wait_for",
            json!({"call_id": a_call, "state": "ended", "timeout_ms": 8000}),
        )
        .await;
    assert!(w.is_ok(), "{w:?}");
    let mut cursor = s;
    let mut closed = Vec::new();
    for _ in 0..4 {
        let ev =
            r.b.call(
                "events.poll",
                json!({"since":cursor,"kinds":["call.stream"],"call_id":b_call,"timeout_ms":500}),
            )
            .await
            .unwrap();
        for e in ev["events"].as_array().unwrap() {
            if e["data"]["state"] == "closed" {
                closed.push(e["data"].clone());
            }
        }
        cursor = ev["next_seq"].as_u64().unwrap();
    }
    assert_eq!(closed.len(), 1, "{closed:?}");
    let reason = closed[0]["reason"].as_str().unwrap_or_default();
    assert!(
        matches!(reason, "connection_lost" | "client_eof" | "client_close"),
        "{reason}"
    );
}
