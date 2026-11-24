//! Agent audio API tests: inline play / queue / stop / barge-in, listen,
//! rx_audio, limits and the speech hooks, on the two-instance loopback rig (alice calls bob
//! inside one app; `call.*` on alice's call is the "agent", bob listens).
#![cfg(unix)]

mod common;

use std::io::Cursor;
use std::time::Duration;

use base64::Engine as _;
use common::{Rig, acct, client, free_udp_port, seq, start_app, write_atomic};
use serde_json::{Value, json};
use siple_core::agent::wav::encode_wav_i16;
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::control::{ApiError, ErrorKind};
use siple_media::goertzel;

// ---- helpers -----------------------------------------------------------------------------

fn kind(e: &ClientError) -> ErrorKind {
    match e {
        ClientError::Api(a) => a.kind,
        other => panic!("expected an API error, got {other}"),
    }
}

fn api(e: &ClientError) -> &ApiError {
    match e {
        ClientError::Api(a) => a,
        other => panic!("expected an API error, got {other}"),
    }
}

fn sine(rate: u32, hz: f32, ms: u32, amp: f32) -> Vec<i16> {
    (0..rate as u64 * ms as u64 / 1000)
        .map(|n| {
            ((n as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * amp * 32767.0) as i16
        })
        .collect()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn wav_b64(hz: f32, ms: u32) -> String {
    b64(&encode_wav_i16(&sine(16_000, hz, ms, 0.5), 16_000))
}

/// Decodes the `audio.base64` WAV of a result: (rate, samples).
fn decode_audio(res: &Value) -> (u32, Vec<i16>) {
    let a = &res["audio"];
    assert_eq!(a["format"], "wav", "{res}");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(a["base64"].as_str().expect("audio.base64"))
        .unwrap();
    let mut r = hound::WavReader::new(Cursor::new(bytes)).unwrap();
    let rate = r.spec().sample_rate;
    assert_eq!(a["sample_rate"], rate);
    (rate, r.samples::<i16>().map(|s| s.unwrap()).collect())
}

fn ratio(samples: &[i16], rate: u32, hz: f32) -> f32 {
    goertzel::detect(samples, rate, hz).0
}

fn near(a: u64, b: u64, tol: u64) -> bool {
    a.abs_diff(b) <= tol
}

/// Dial alice -> bob with both sides sending silence.
async fn establish_silent(r: &mut Rig) -> (String, String) {
    let b_seq = seq(&mut r.b).await;
    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"silence","sink":"null"}),
        )
        .await
        .unwrap();
    let a_call = d["call_id"].as_str().unwrap().to_string();
    let ringing =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":b_seq,"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    r.b.call("call.answer", json!({"call_id": b_call}))
        .await
        .unwrap();
    r.a.call(
        "call.wait_for",
        json!({"call_id": a_call, "state":"active","timeout_ms":30_000}),
    )
    .await
    .unwrap();
    // Let RTP flow so both RX timelines run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (a_call, b_call)
}

async fn rx_ms(c: &mut ControlClient, call: &str) -> u64 {
    c.call("call.get", json!({"call_id": call})).await.unwrap()["audio"]["rx_ms"]
        .as_u64()
        .unwrap()
}

/// Waits for `call.play_finished` of `play_id` after `since`; returns its data.
async fn finished_event(c: &mut ControlClient, call: &str, since: u64, play_id: u64) -> Value {
    let mut cursor = since;
    for _ in 0..40 {
        let r = c
            .call(
                "events.poll",
                json!({"since":cursor,"kinds":["call.play_finished"],"call_id":call,"timeout_ms":1000}),
            )
            .await
            .unwrap();
        for e in r["events"].as_array().unwrap() {
            if e["data"]["play_id"] == play_id {
                return e["data"].clone();
            }
        }
        cursor = r["next_seq"].as_u64().unwrap();
    }
    panic!("no play_finished for {play_id}");
}

async fn finished_events(c: &mut ControlClient, call: &str, since: u64, n: usize) -> Vec<Value> {
    let mut cursor = since;
    let mut out = Vec::new();
    for _ in 0..40 {
        let r = c
            .call(
                "events.poll",
                json!({"since":cursor,"kinds":["call.play_finished"],"call_id":call,"timeout_ms":1000}),
            )
            .await
            .unwrap();
        out.extend(
            r["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["data"].clone()),
        );
        cursor = r["next_seq"].as_u64().unwrap();
        if out.len() >= n {
            return out;
        }
    }
    panic!("only {} play_finished events", out.len());
}

/// Polls `call.get` (on any client) until the call has a current play; returns its id.
/// `audio.play.current` comes from the media thread's stats snapshot, refreshed about every
/// 50 ms once that thread has taken the clip, so on a loaded machine it can trail the
/// `call.play` that issued the clip by far more than a fixed delay.
async fn wait_playing(c: &mut ControlClient, call: &str) -> u64 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let g = c.call("call.get", json!({"call_id": call})).await.unwrap();
        if let Some(id) = g["audio"]["play"]["current"].as_u64() {
            return id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "play never started: {g}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn rig_with(extra_toml: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let text = format!(
        "{}{}{extra_toml}",
        acct("alice", pa, ""),
        acct("bob", pb, "")
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let app = start_app(&path).await;
    let a = client(&app, "a").await;
    let b = client(&app, "b").await;
    Rig {
        dir,
        app,
        a,
        b,
        pa,
        pb,
        bob_uri: format!("sip:bob@127.0.0.1:{pb}"),
    }
}

// ---- play / listen -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn inline_wav_is_heard_by_the_far_end() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let since = rx_ms(&mut r.b, &b).await;
    let a_seq = seq(&mut r.a).await;
    let play = r.a.call(
        "call.play",
        json!({"call_id":a,"wav_base64":wav_b64(600.0, 1000),"wait":true,"timeout_ms":20_000}),
    );
    let listen = r.b.call(
        "call.listen",
        json!({"call_id":b,"since_rx_ms":since,"start_timeout_ms":8000,"end_silence_ms":500}),
    );
    let (play, listen) = tokio::join!(play, listen);
    let play = play.unwrap();
    let listen = listen.unwrap();

    assert!(play["play_id"].as_u64().is_some(), "{play}");
    assert_eq!(play["duration_ms"], 1000);
    assert_eq!(play["queued"], false);
    assert_eq!(play["finished"]["reason"], "completed", "{play}");
    assert!(
        near(play["finished"]["played_ms"].as_u64().unwrap(), 1000, 100),
        "{play}"
    );
    assert!(play["finished"]["rx_ms"].as_u64().unwrap() > 0);

    assert_eq!(listen["speech"], true, "{listen}");
    assert_eq!(listen["reason"], "end_of_speech");
    let ms = listen["speech_ms"].as_u64().unwrap();
    assert!((700..=1400).contains(&ms), "speech_ms {ms}: {listen}");
    assert!(listen["end_rx_ms"].as_u64().unwrap() > listen["speech_end_rx_ms"].as_u64().unwrap());
    assert_eq!(listen["truncated"], false);
    assert_eq!(listen["dtmf"], "");
    let (rate, samples) = decode_audio(&listen);
    assert_eq!(rate, 16_000);
    assert!(
        ratio(
            &samples[2_000..samples.len().saturating_sub(2_000)],
            rate,
            600.0
        ) > 0.7,
        "no 600 Hz in the returned audio"
    );
    assert!(ratio(&samples, rate, 1000.0) < 0.2);
    assert!(listen["level_dbfs"]["peak"].as_f64().unwrap() > -20.0);

    // The same finish is in alice's event log, exactly once.
    let fin = finished_event(&mut r.a, &a, a_seq, play["play_id"].as_u64().unwrap()).await;
    assert_eq!(fin["reason"], "completed");
    // And call.get shows the audio state.
    let g = r.a.call("call.get", json!({"call_id":a})).await.unwrap();
    assert!(g["audio"]["rx_ms"].as_u64().unwrap() > 0);
    assert_eq!(g["audio"]["play"]["queued"], 0);
    assert_eq!(g["audio"]["tx_stream"], false);
    assert!(g["audio"]["rx_tap_dropped_ms"].is_u64());
}

#[tokio::test(flavor = "multi_thread")]
async fn mulaw_pcm_input_and_8k_output() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let ulaw: Vec<u8> = sine(8_000, 600.0, 1000, 0.5)
        .into_iter()
        .map(siple_media::g711::linear_to_ulaw)
        .collect();
    let since = rx_ms(&mut r.b, &b).await;
    let play = r.a.call(
        "call.play",
        json!({"call_id":a,"pcm_base64":b64(&ulaw),
               "pcm_format":{"sample_rate":8000,"encoding":"mulaw"},"wait":true}),
    );
    let listen = r.b.call(
        "call.listen",
        json!({"call_id":b,"since_rx_ms":since,"end_silence_ms":500,"sample_rate":8000}),
    );
    let (play, listen) = tokio::join!(play, listen);
    assert_eq!(play.unwrap()["finished"]["reason"], "completed");
    let listen = listen.unwrap();
    assert_eq!(listen["speech"], true, "{listen}");
    let (rate, samples) = decode_audio(&listen);
    assert_eq!(rate, 8_000);
    assert!(ratio(&samples[1_000..samples.len() - 1_000], rate, 600.0) > 0.7);
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_keeps_order_and_replace_cuts_the_rest() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    // The first clip is long enough that the next two requests still find it playing (a short
    // one can finish before a delayed request arrives on a loaded machine, so "queued" is false).
    let mut ids = Vec::new();
    for (i, queue) in [false, true, true].into_iter().enumerate() {
        let p = r
            .a
            .call(
                "call.play",
                json!({"call_id":a,"wav_base64":wav_b64(500.0 + i as f32 * 100.0, if i == 0 { 1500 } else { 400 }),"queue":queue}),
            )
            .await
            .unwrap();
        if i > 0 {
            assert_eq!(p["queued"], true, "{p}");
        }
        ids.push(p["play_id"].as_u64().unwrap());
    }
    let fin = finished_events(&mut r.a, &a, s, 3).await;
    let got: Vec<u64> = fin.iter().map(|f| f["play_id"].as_u64().unwrap()).collect();
    assert_eq!(got, ids, "finish order");
    assert!(fin.iter().all(|f| f["reason"] == "completed"), "{fin:?}");

    // A non-queued play replaces the current one and the queue.
    let s = seq(&mut r.a).await;
    let first =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 5000)}),
        )
        .await
        .unwrap();
    let queued =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(700.0, 5000),"queue":true}),
        )
        .await
        .unwrap();
    let third =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(800.0, 300)}),
        )
        .await
        .unwrap();
    let fin = finished_events(&mut r.a, &a, s, 3).await;
    let reason = |id: &Value| fin.iter().find(|f| f["play_id"] == *id).unwrap()["reason"].clone();
    assert_eq!(reason(&first["play_id"]), "replaced");
    assert_eq!(reason(&queued["play_id"]), "replaced");
    assert_eq!(reason(&third["play_id"]), "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_play_and_looping_sources() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let p =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 10_000)}),
        )
        .await
        .unwrap();
    let id = p["play_id"].as_u64().unwrap();
    // Unknown ids are ignored.
    r.a.call("call.stop_play", json!({"call_id":a,"play_id":987_654}))
        .await
        .unwrap();
    let st =
        r.a.call("call.stop_play", json!({"call_id":a,"play_id":id}))
            .await
            .unwrap();
    assert_eq!(st["ok"], true);
    let fin = finished_event(&mut r.a, &a, s, id).await;
    assert_eq!(fin["reason"], "stopped");
    assert!(fin["played_ms"].as_u64().unwrap() < 5_000);

    // Legacy: tone: source loops, stop_play without ids stops it (and the queue).
    let s = seq(&mut r.a).await;
    let t =
        r.a.call("call.play", json!({"call_id":a,"source":"tone:700"}))
            .await
            .unwrap();
    assert!(t["duration_ms"].is_null());
    let q =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 400),"queue":true}),
        )
        .await
        .unwrap();
    r.a.call("call.stop_play", json!({"call_id":a}))
        .await
        .unwrap();
    let fin = finished_events(&mut r.a, &a, s, 2).await;
    assert!(fin.iter().all(|f| f["reason"] == "stopped"), "{fin:?}");
    assert!(fin.iter().any(|f| f["play_id"] == q["play_id"]));
    // `device` stops everything too, with no play id.
    let d =
        r.a.call("call.play", json!({"call_id":a,"source":"device"}))
            .await
            .unwrap();
    assert!(d["play_id"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn barge_in_stops_the_play_when_the_far_end_speaks() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let p =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 20_000),"barge_in":true}),
        )
        .await
        .unwrap();
    let id = p["play_id"].as_u64().unwrap();
    // Silence from bob does not cut it off.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let g = r.a.call("call.get", json!({"call_id":a})).await.unwrap();
    assert_eq!(g["audio"]["play"]["current"], id, "{g}");
    // Bob starts talking.
    r.b.call(
        "call.play",
        json!({"call_id":b,"source":"tone:800","duration_ms":3000}),
    )
    .await
    .unwrap();
    let fin = finished_event(&mut r.a, &a, s, id).await;
    assert_eq!(fin["reason"], "barge_in", "{fin}");
    assert!(fin["played_ms"].as_u64().unwrap() < 10_000);
}

/// The watcher fires after ~120 ms of voiced audio and the play stops later still;
/// `finished.barge_in_rx_ms` is where the interruption began, so a listen from it keeps the
/// start of the caller's words.
#[tokio::test(flavor = "multi_thread")]
async fn barge_in_reports_the_onset_and_listen_from_it_keeps_the_start() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let p =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 20_000),"barge_in":true}),
        )
        .await
        .unwrap();
    let id = p["play_id"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t0 = rx_ms(&mut r.a, &a).await;
    r.b.call(
        "call.play",
        json!({"call_id":b,"source":"tone:800","duration_ms":2500}),
    )
    .await
    .unwrap();
    let t1 = rx_ms(&mut r.a, &a).await;
    let fin = finished_event(&mut r.a, &a, s, id).await;
    assert_eq!(fin["reason"], "barge_in", "{fin}");
    let onset = fin["barge_in_rx_ms"].as_u64().expect("barge_in_rx_ms");
    let stopped = fin["rx_ms"].as_u64().unwrap();
    // The onset is the start of the tone, ahead of the stop by at least the 120 ms run.
    assert!(onset + 100 <= stopped, "onset {onset}, stopped {stopped}");
    assert!(
        // Upper bound: bob's audio reaches us after the play call returns, a delay that grows on
        // a loaded machine (the onset < stopped check above is the tight one).
        onset + 100 >= t0 && onset <= t1 + 1500,
        "onset {onset} outside tone start window {t0}..{t1}"
    );
    // Other reasons carry no onset.
    let q =
        r.a.call(
            "call.play",
            json!({"call_id":a,"source":"tone:500","duration_ms":200,"wait":true}),
        )
        .await
        .unwrap();
    assert!(q["finished"].get("barge_in_rx_ms").is_none(), "{q}");

    let l =
        r.a.call(
            "call.listen",
            json!({"call_id":a,"since_rx_ms":onset,"end_silence_ms":400}),
        )
        .await
        .unwrap();
    assert_eq!(l["speech"], true, "{l}");
    let start = l["speech_start_rx_ms"].as_u64().unwrap();
    assert!(
        start.abs_diff(onset) <= 60,
        "speech starts at {start}, onset {onset}: {l}"
    );
    let (rate, samples) = decode_audio(&l);
    assert!(ratio(&samples, rate, 800.0) > 0.3, "{l}");
    // The audio holds the whole tone from its beginning (not just what followed the stop).
    let audio_ms = samples.len() as u64 * 1000 / rate as u64;
    assert!(
        audio_ms + 100 >= l["speech_ms"].as_u64().unwrap(),
        "{audio_ms} ms of audio for {l}"
    );
    assert!(
        l["speech_ms"].as_u64().unwrap() + 100 >= stopped - onset,
        "{l}"
    );
}

/// A `since_rx_ms` in the future extends the default wall budget by the wait, so a window that
/// fills up (`max_duration`) is not cut short as `timeout`.
#[tokio::test(flavor = "multi_thread")]
async fn listen_since_in_the_future_extends_the_wall_deadline() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let now = rx_ms(&mut r.a, &a).await;
    r.b.call("call.play", json!({"call_id":b,"source":"tone:800"}))
        .await
        .unwrap();
    // 8 s ahead + 2 s window = 10 s > start_timeout 500 + max 2000 + 5000 slack.
    let l =
        r.a.call(
            "call.listen",
            json!({"call_id":a,"since_rx_ms":now + 8_000,"start_timeout_ms":500,
                   "max_ms":2000,"audio":"none"}),
        )
        .await
        .unwrap();
    assert_eq!(l["reason"], "max_duration", "{l}");
}

/// Serde errors quote the offending value: the message is capped.
#[tokio::test(flavor = "multi_thread")]
async fn huge_bad_param_does_not_echo_back_unbounded() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    let e =
        r.a.call(
            "call.listen",
            json!({"call_id":a,"since_rx_ms":"x".repeat(12_000)}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidParams);
    let m = &api(&e).message;
    assert!(m.len() < 400, "{} chars: {m}", m.len());
    assert!(m.contains("chars)"), "{m}");
    // The MCP tool text is the compact wire error: it stays well under 400 chars too.
    let wire = serde_json::to_string(&api(&e).to_wire()).unwrap();
    assert!(wire.len() < 400, "{} chars: {wire}", wire.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn barge_in_cuts_every_queued_barge_in_clip() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            r.a.call(
                "call.play",
                json!({"call_id":a,"wav_base64":wav_b64(600.0, 5000),"queue":i > 0,"barge_in":true}),
            )
            .await
            .unwrap()["play_id"]
                .as_u64()
                .unwrap(),
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    r.b.call(
        "call.play",
        json!({"call_id":b,"source":"tone:800","duration_ms":3000}),
    )
    .await
    .unwrap();
    for (i, id) in ids.iter().enumerate() {
        let fin = finished_event(&mut r.a, &a, s, *id).await;
        assert_eq!(fin["reason"], "barge_in", "{fin}");
        if i > 0 {
            assert!(fin["played_ms"].as_u64().unwrap() < 40, "{fin}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn barge_in_ignores_speech_while_muted() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let id =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 20_000),"barge_in":true}),
        )
        .await
        .unwrap()["play_id"]
            .as_u64()
            .unwrap();
    r.a.call("call.mute", json!({"call_id":a})).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    r.b.call(
        "call.play",
        json!({"call_id":b,"source":"tone:800","duration_ms":1000}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let g = r.a.call("call.get", json!({"call_id":a})).await.unwrap();
    assert_eq!(g["audio"]["play"]["current"], id, "{g}");
    r.a.call("call.unmute", json!({"call_id":a})).await.unwrap();
    r.b.call(
        "call.play",
        json!({"call_id":b,"source":"tone:800","duration_ms":3000}),
    )
    .await
    .unwrap();
    let fin = finished_event(&mut r.a, &a, s, id).await;
    assert_eq!(fin["reason"], "barge_in", "{fin}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_timeout_keeps_playing_and_reports_the_play_id() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    let e =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 10_000),"wait":true,"timeout_ms":500}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::Timeout, "{e}");
    let extra = api(&e).extra.as_ref().unwrap();
    let id = extra["play_id"].as_u64().unwrap();
    // The play outlives the wait: it is (or, when the media thread has not taken the clip
    // yet, becomes) the current play, and it has not finished.
    assert_eq!(wait_playing(&mut r.a, &a).await, id);
    let fin =
        r.a.call(
            "events.poll",
            json!({"since":extra["event_seq"],"kinds":["call.play_finished"],"call_id":a,"timeout_ms":1000}),
        )
        .await
        .unwrap();
    assert_eq!(fin["events"], json!([]), "{fin}");
    r.a.call("call.stop_play", json!({"call_id":a}))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn wait_ends_with_the_call() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    // The default wait limit (clip + 30 s): a limit equal to the clip's length races the clip's
    // own end, which on a slow machine (media time falling behind the wall clock) comes later.
    let play = r.a.call(
        "call.play",
        json!({"call_id":a,"wav_base64":wav_b64(600.0, 20_000),"wait":true}),
    );
    let hang = async {
        // Hang up once the play is really running: on a loaded machine the 20 s WAV can take
        // longer than a fixed delay to be decoded and queued, and a hangup before that makes
        // `call.play` fail with "call has ended" instead of finishing with `call_ended`.
        wait_playing(&mut r.b, &a).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        // End the agent's own call (from the other client, as `call.play` is still waiting):
        // what is tested is the wait ending with the call, not the far end's BYE reaching us.
        // On hosted macOS runners the test process's UDP sockets were seen going dead mid-test
        // (send: EPIPE, receive: ENOTCONN), so that BYE never arrived and the play ran on.
        r.b.call("call.hangup", json!({"call_id":a})).await.unwrap();
    };
    let (play, _) = tokio::join!(play, hang);
    let play = play.unwrap();
    assert_eq!(play["finished"]["reason"], "call_ended", "{play}");
}

#[tokio::test(flavor = "multi_thread")]
async fn since_rx_ms_in_the_past_still_captures() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let since = rx_ms(&mut r.b, &b).await;
    let p =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 800),"wait":true}),
        )
        .await
        .unwrap();
    assert_eq!(p["finished"]["reason"], "completed");
    // The "LLM thinks" for 2 s; the history covers the gap.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let l =
        r.b.call(
            "call.listen",
            json!({"call_id":b,"since_rx_ms":since,"end_silence_ms":300,"audio":"none"}),
        )
        .await
        .unwrap();
    assert_eq!(l["speech"], true, "{l}");
    assert_eq!(l["reason"], "end_of_speech");
    assert!(l.get("audio").is_none(), "{l}");
    assert_eq!(l["start_rx_ms"], since);
    assert!(near(l["speech_ms"].as_u64().unwrap(), 800, 300), "{l}");
    // Chaining: the next listen from end_rx_ms sees only silence.
    let l2 =
        r.b.call(
            "call.listen",
            json!({"call_id":b,"since_rx_ms":l["end_rx_ms"],"start_timeout_ms":600}),
        )
        .await
        .unwrap();
    assert_eq!(
        (l2["speech"].clone(), l2["reason"].clone()),
        (json!(false), json!("no_speech")),
        "{l2}"
    );
    assert!(l2.get("audio").is_none());
    // A start too far ahead is rejected.
    let now = rx_ms(&mut r.b, &b).await;
    let e =
        r.b.call(
            "call.listen",
            json!({"call_id":b,"since_rx_ms":now + 20_000}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidParams, "{e}");
}

/// Turn-taking as documented: say/play{wait} -> finished.rx_ms -> (agent thinks) -> listen
/// {since_rx_ms: finished.rx_ms}. The far side starts talking 8.5 s after our audio ended, i.e.
/// past the 8 s default `start_timeout_ms` counted from `since_rx_ms`; the replayed history
/// must not burn the timeout, so the caller's reply is still heard.
#[tokio::test(flavor = "multi_thread")]
async fn slow_thinking_agent_still_hears_the_reply() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let p =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 300),"wait":true}),
        )
        .await
        .unwrap();
    let since = p["finished"]["rx_ms"].as_u64().unwrap();
    // The caller (bob) answers after 8.5 s of silence, a 400 ms 900 Hz "word".
    let mut reply = vec![0i16; 16 * 8_500];
    reply.extend(sine(16_000, 900.0, 400, 0.5));
    r.b.call(
        "call.play",
        json!({"call_id":b,"wav_base64":b64(&encode_wav_i16(&reply, 16_000))}),
    )
    .await
    .unwrap();
    // The agent "thinks" (LLM + TTS) for 9.5 s, then listens with the documented since.
    tokio::time::sleep(Duration::from_millis(9_500)).await;
    let l =
        r.a.call(
            "call.listen",
            json!({"call_id":a,"since_rx_ms":since,"end_silence_ms":300,"audio":"none"}),
        )
        .await
        .unwrap();
    assert_eq!(l["speech"], true, "{l}");
    assert_eq!(l["reason"], "end_of_speech", "{l}");
    let start = l["speech_start_rx_ms"].as_u64().unwrap();
    assert!(
        // Lower bound: past the 8 s start timeout (the point of the test). Upper bound only
        // guards against a wildly wrong start; play start and RX timeline drift under load.
        (since + 8_000..=since + 14_000).contains(&start),
        "speech at {start}, since {since}: {l}"
    );
    assert!(near(l["speech_ms"].as_u64().unwrap(), 400, 200), "{l}");
}

#[tokio::test(flavor = "multi_thread")]
async fn rx_audio_returns_the_requested_length() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    r.a.call(
        "call.play",
        json!({"call_id":a,"wav_base64":wav_b64(600.0, 3000),"wait":true}),
    )
    .await
    .unwrap();
    let x =
        r.b.call("call.rx_audio", json!({"call_id":b,"last_ms":1000}))
            .await
            .unwrap();
    assert!(near(x["duration_ms"].as_u64().unwrap(), 1000, 30), "{x}");
    assert_eq!(x["truncated"], false);
    let (rate, samples) = decode_audio(&x);
    assert_eq!(rate, 16_000);
    assert!(near(samples.len() as u64, 16_000, 500));
    assert!(near(
        x["end_rx_ms"].as_u64().unwrap() - x["start_rx_ms"].as_u64().unwrap(),
        1000,
        30
    ));
    let y =
        r.b.call(
            "call.rx_audio",
            json!({"call_id":b,"last_ms":500,"sample_rate":8000,"audio":"none"}),
        )
        .await
        .unwrap();
    assert!(y.get("audio").is_none());
    assert!(near(y["duration_ms"].as_u64().unwrap(), 500, 30));
    // since/until
    let end = x["end_rx_ms"].as_u64().unwrap();
    let z =
        r.b.call(
            "call.rx_audio",
            json!({"call_id":b,"since_rx_ms":end - 600,"until_rx_ms":end - 100,"sample_rate":8000}),
        )
        .await
        .unwrap();
    assert_eq!(z["start_rx_ms"], end - 600);
    assert_eq!(z["duration_ms"], 500);
    let (rate, samples) = decode_audio(&z);
    assert_eq!((rate, samples.len()), (8_000, 4_000));
    // A history older than rx_history_secs is clamped and flagged.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let big =
        r.b.call(
            "call.rx_audio",
            json!({"call_id":b,"last_ms":3_600_000,"audio":"none"}),
        )
        .await
        .unwrap();
    assert!(big["duration_ms"].as_u64().unwrap() > 3_000);
    // File output.
    let f =
        r.b.call(
            "call.rx_audio",
            json!({"call_id":b,"last_ms":400,"audio":"file","path":"rx.wav"}),
        )
        .await
        .unwrap();
    assert!(f["audio"]["path"].as_str().unwrap().ends_with("rx.wav"));
    let rd = hound::WavReader::open(r.dir.path().join("rx.wav")).unwrap();
    assert_eq!(rd.spec().sample_rate, 16_000);
    assert!(near(rd.duration() as u64, 6_400, 500));
    // Both selectors at once is a mistake.
    let e =
        r.b.call(
            "call.rx_audio",
            json!({"call_id":b,"last_ms":400,"since_rx_ms":10}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidParams);
}

#[tokio::test(flavor = "multi_thread")]
async fn listen_reports_dtmf_in_the_window() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let since = rx_ms(&mut r.b, &b).await;
    r.a.call(
        "call.dtmf",
        json!({"call_id":a,"digits":"42","mode":"rfc4733"}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let l =
        r.b.call(
            "call.listen",
            json!({"call_id":b,"since_rx_ms":since,"start_timeout_ms":1500,"audio":"none"}),
        )
        .await
        .unwrap();
    assert_eq!(l["dtmf"], "42", "{l}");
}

#[tokio::test(flavor = "multi_thread")]
async fn listen_ends_with_the_call_and_limits_concurrency() {
    let mut r = rig_with("").await;
    let (_a, b) = establish_silent(&mut r).await;
    let mut tasks = Vec::new();
    for i in 0..4 {
        let mut c = client(&r.app, &format!("l{i}")).await;
        let b = b.clone();
        tasks.push(tokio::spawn(async move {
            c.call(
                "call.listen",
                json!({"call_id":b,"start_timeout_ms":30_000,"audio":"none"}),
            )
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    let e =
        r.b.call("call.listen", json!({"call_id":b,"audio":"none"}))
            .await
            .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidState, "{e}");
    r.b.call("call.hangup", json!({"call_id":b})).await.unwrap();
    for t in tasks {
        let l = tokio::time::timeout(Duration::from_secs(10), t)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(l["reason"], "call_ended", "{l}");
        assert_eq!(l["speech"], false);
    }
}

// ---- limits and bad input ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn queued_play_barge_in_ignores_speech_heard_before_it_started() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let first =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 2500)}),
        )
        .await
        .unwrap()["play_id"]
            .as_u64()
            .unwrap();
    let second =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 600),"queue":true,"barge_in":true}),
        )
        .await
        .unwrap();
    let second = second["play_id"].as_u64().unwrap();
    // Bob talks while the first clip plays; he is silent again by the time the second starts.
    r.b.call(
        "call.play",
        json!({"call_id":b,"source":"tone:800","duration_ms":1200}),
    )
    .await
    .unwrap();
    let f1 = finished_event(&mut r.a, &a, s, first).await;
    assert_eq!(f1["reason"], "completed", "{f1}");
    let f2 = finished_event(&mut r.a, &a, s, second).await;
    assert_eq!(f2["reason"], "completed", "{f2}");
    assert!(f2["played_ms"].as_u64().unwrap() >= 500, "{f2}");
}

#[tokio::test(flavor = "multi_thread")]
async fn late_stop_for_a_finished_play_keeps_the_queue_playing() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    let s = seq(&mut r.a).await;
    let first =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 300)}),
        )
        .await
        .unwrap()["play_id"]
            .as_u64()
            .unwrap();
    let mut rest = Vec::new();
    for _ in 0..2 {
        rest.push(
            r.a.call(
                "call.play",
                json!({"call_id":a,"wav_base64":wav_b64(600.0, 1500),"queue":true}),
            )
            .await
            .unwrap()["play_id"]
                .as_u64()
                .unwrap(),
        );
    }
    let f = finished_event(&mut r.a, &a, s, first).await;
    assert_eq!(f["reason"], "completed", "{f}");
    // Default clear_queue = true, but the id is finished: nothing else is touched.
    r.a.call("call.stop_play", json!({"call_id":a,"play_id":first}))
        .await
        .unwrap();
    for id in rest {
        let f = finished_event(&mut r.a, &a, s, id).await;
        assert_eq!(f["reason"], "completed", "{f}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn play_wait_before_the_answer_completes_only_after_it() {
    let mut r = rig_with("").await;
    // Inbound: bob plays on his ringing call; the clip must not finish before he answers.
    let b_seq = seq(&mut r.b).await;
    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"silence","sink":"null"}),
        )
        .await
        .unwrap();
    let a_call = d["call_id"].as_str().unwrap().to_string();
    let ringing =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":b_seq,"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    let mut b2 = client(&r.app, "b2").await;
    let bc = b_call.clone();
    let t0 = std::time::Instant::now();
    let play = tokio::spawn(async move {
        b2.call(
            "call.play",
            json!({"call_id":bc,"wav_base64":wav_b64(600.0, 300),"wait":true,"timeout_ms":20_000}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!play.is_finished(), "the play finished before the answer");
    r.b.call("call.answer", json!({"call_id": b_call}))
        .await
        .unwrap();
    let res = tokio::time::timeout(Duration::from_secs(15), play)
        .await
        .expect("play never finished")
        .unwrap();
    // Either played after the answer, or refused outright; never a premature completion.
    if let Ok(v) = res {
        assert_eq!(v["finished"]["reason"], "completed", "{v}");
        assert!(t0.elapsed() >= Duration::from_millis(1500));
    }
    let _ = a_call;
}

/// Documented behaviour (docs/API-intro.md): answering with a different source/sink recreates
/// the call's media. A play and a listen issued before the answer end with `call_ended`, and
/// the RX timeline (`rx_ms`) restarts near 0.
#[tokio::test(flavor = "multi_thread")]
async fn answer_with_a_new_source_recreates_media_and_ends_pending_audio() {
    let mut r = rig_with("").await;
    let b_seq = seq(&mut r.b).await;
    r.a.call(
        "call.dial",
        json!({"account":"alice","target":r.bob_uri,"source":"silence","sink":"null"}),
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
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    let s = seq(&mut r.b).await;
    // A long pending play (no wait) and a pending listen on the pre-answer media.
    let play =
        r.b.call(
            "call.play",
            json!({"call_id":b_call,"wav_base64":wav_b64(600.0, 20_000)}),
        )
        .await
        .unwrap();
    let play_id = play["play_id"].as_u64().unwrap();
    let mut b2 = client(&r.app, "b2").await;
    let bc = b_call.clone();
    let listen = tokio::spawn(async move {
        b2.call(
            "call.listen",
            json!({"call_id":bc,"start_timeout_ms":30_000}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    r.b.call(
        "call.answer",
        json!({"call_id": b_call, "source":"tone:700"}),
    )
    .await
    .unwrap();
    let fin = finished_event(&mut r.b, &b_call, s, play_id).await;
    assert_eq!(fin["reason"], "call_ended", "{fin}");
    let l = tokio::time::timeout(Duration::from_secs(10), listen)
        .await
        .expect("listen never resolved")
        .unwrap()
        .unwrap();
    assert_eq!(l["reason"], "call_ended", "{l}");
    // The new media's timeline starts over.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rx = rx_ms(&mut r.b, &b_call).await;
    assert!(rx < 1400, "rx_ms {rx} did not restart after the answer");
    // Audio issued after the answer works on the new media.
    let p =
        r.b.call(
            "call.play",
            json!({"call_id":b_call,"wav_base64":wav_b64(500.0, 300),"wait":true}),
        )
        .await
        .unwrap();
    assert_eq!(p["finished"]["reason"], "completed", "{p}");
}

#[tokio::test(flavor = "multi_thread")]
async fn long_file_source_and_inline_limits() {
    // Decoding minutes of audio in a debug build can take long on a loaded machine; the default
    // 30 s request limit is about the client, not what this test checks.
    const DECODE: Duration = Duration::from_secs(180);
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    // A 130 s file source (longer than the inline cap) is still accepted, as it always was.
    let path = r.dir.path().join("long.wav");
    std::fs::write(&path, encode_wav_i16(&vec![0i16; 130 * 8000], 8000)).unwrap();
    let p =
        r.a.call_timeout(
            "call.play",
            json!({"call_id":a,"source":format!("file:{}", path.display()),"loop":false}),
            DECODE,
        )
        .await
        .unwrap();
    assert!(p["play_id"].as_u64().unwrap() > 0, "{p}");
    assert!(
        near(p["duration_ms"].as_u64().unwrap(), 130_000, 100),
        "{p}"
    );
    r.a.call("call.stop_play", json!({"call_id":a}))
        .await
        .unwrap();
    // A file longer than the queue cap (300 s) plays as a replace / on an idle call, but is
    // refused when it would queue behind another clip.
    let huge = r.dir.path().join("huge.wav");
    std::fs::write(&huge, encode_wav_i16(&vec![0i16; 310 * 8000], 8000)).unwrap();
    let src = format!("file:{}", huge.display());
    // (A looping tone keeps the call busy however long decoding the file takes.)
    r.a.call("call.play", json!({"call_id":a,"source":"tone:500"}))
        .await
        .unwrap();
    let e =
        r.a.call_timeout(
            "call.play",
            json!({"call_id":a,"source":src,"loop":false,"queue":true}),
            DECODE,
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidState, "{e}");
    let p =
        r.a.call_timeout(
            "call.play",
            json!({"call_id":a,"source":src,"loop":false}),
            DECODE,
        )
        .await
        .unwrap();
    assert!(p["play_id"].as_u64().unwrap() > 0, "{p}");
    r.a.call("call.stop_play", json!({"call_id":a}))
        .await
        .unwrap();

    // Inline rx_audio beyond 120 s is refused; file output or none is fine.
    let e =
        r.a.call("call.rx_audio", json!({"call_id":a,"last_ms":121_000}))
            .await
            .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidParams, "{e}");
    let ok =
        r.a.call(
            "call.rx_audio",
            json!({"call_id":a,"last_ms":121_000,"audio":"none"}),
        )
        .await
        .unwrap();
    assert!(ok["audio"].is_null(), "{ok}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_input_is_invalid_params() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    let good = wav_b64(600.0, 200);
    let pcm = b64(&[0u8; 1600]);
    let fmt = json!({"sample_rate":8000,"encoding":"s16le"});
    let cases: Vec<(&str, Value)> = vec![
        ("call.play", json!({"call_id":a})),
        (
            "call.play",
            json!({"call_id":a,"source":"tone:500","wav_base64":good}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"pcm_base64":pcm,"pcm_format":fmt}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":"!!not base64!!"}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":b64(b"RIFFnonsense")}),
        ),
        ("call.play", json!({"call_id":a,"pcm_base64":pcm})),
        (
            "call.play",
            json!({"call_id":a,"pcm_format":fmt,"source":"silence"}),
        ),
        (
            "call.play",
            json!({"call_id":a,"pcm_base64":pcm,"pcm_format":{"sample_rate":7000,"encoding":"s16le"}}),
        ),
        (
            "call.play",
            json!({"call_id":a,"pcm_base64":pcm,"pcm_format":{"sample_rate":8000,"encoding":"opus"}}),
        ),
        (
            "call.play",
            json!({"call_id":a,"pcm_base64":b64(&[0u8; 3]),"pcm_format":fmt}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"duration_ms":600_001}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"duration_ms":0}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"gain_db":21}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"gain_db":-41}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"timeout_ms":120_001}),
        ),
        (
            "call.play",
            json!({"call_id":a,"source":"tone:500","wait":true}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"loop":true,"wait":true}),
        ),
        (
            "call.play",
            json!({"call_id":a,"wav_base64":good,"bogus":1}),
        ),
        ("call.play", json!({"call_id":a,"source":"bogus:1"})),
        // > 120 s: 130 s of mu-law at 8 kHz
        (
            "call.play",
            json!({"call_id":a,"pcm_base64":b64(&vec![0xffu8; 130 * 8000]),
                             "pcm_format":{"sample_rate":8000,"encoding":"mulaw"}}),
        ),
        ("call.listen", json!({"call_id":b,"sample_rate":44_100})),
        ("call.listen", json!({"call_id":b,"audio":"file"})),
        (
            "call.listen",
            json!({"call_id":b,"audio":"base64","path":"x.wav"}),
        ),
        ("call.listen", json!({"call_id":b,"audio":"mp3"})),
        (
            "call.listen",
            json!({"call_id":b,"audio":"file","path":"/no/such/dir/x.wav"}),
        ),
        ("call.listen", json!({"call_id":b,"max_ms":120_001})),
        ("call.listen", json!({"call_id":b,"timeout_ms":180_001})),
        ("call.listen", json!({"call_id":b,"end_silence_ms":0})),
        ("call.listen", json!({"call_id":b,"threshold_dbfs":5})),
        ("call.rx_audio", json!({"call_id":b,"last_ms":0})),
        ("call.rx_audio", json!({"call_id":b,"until_rx_ms":10})),
        ("call.say", json!({"call_id":a,"text":""})),
        ("call.say", json!({"call_id":a,"text":"x".repeat(2001)})),
        ("speech.synthesize", json!({"text":"  "})),
        ("speech.transcribe", json!({})),
        (
            "speech.transcribe",
            json!({"path":"a.wav","wav_base64":good}),
        ),
    ];
    for (m, p) in cases {
        let e = r.a.call(m, p.clone()).await.unwrap_err();
        assert_eq!(kind(&e), ErrorKind::InvalidParams, "{m} {p}: {e}");
    }
    // An existing but unwritable directory is refused up front, before any capture / synthesis.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let ro = r.dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Running as root ignores the mode: nothing to test then.
        if std::fs::File::create(ro.join("probe")).is_err() {
            let p = ro.join("x.wav").display().to_string();
            for (m, params) in [
                (
                    "call.listen",
                    json!({"call_id":b,"audio":"file","path":p.clone()}),
                ),
                (
                    "call.rx_audio",
                    json!({"call_id":b,"audio":"file","path":p.clone()}),
                ),
                ("speech.synthesize", json!({"text":"hi","path":p.clone()})),
            ] {
                let e = r.a.call(m, params).await.unwrap_err();
                assert_eq!(kind(&e), ErrorKind::InvalidParams, "{m}: {e}");
                assert!(e.to_string().contains("cannot write"), "{m}: {e}");
            }
            assert!(!ro.join("x.wav").exists());
        }
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // 14 MiB of base64 is more than 10 MiB decoded: refused before decoding.
    let e =
        r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":"A".repeat(14 * 1024 * 1024 + 4096)}),
        )
        .await
        .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::InvalidParams, "{e}");
    // Queue overflow: the 9th queued play beyond the current one is refused.
    r.a.call(
        "call.play",
        json!({"call_id":a,"wav_base64":wav_b64(600.0, 20_000)}),
    )
    .await
    .unwrap();
    let mut refused = false;
    for _ in 0..12 {
        let q =
            r.a.call(
                "call.play",
                json!({"call_id":a,"wav_base64":wav_b64(600.0, 1000),"queue":true}),
            )
            .await;
        if let Err(e) = q {
            assert_eq!(kind(&e), ErrorKind::InvalidState, "{e}");
            refused = true;
            break;
        }
    }
    assert!(refused, "queue never filled");
    // Unknown call / no speech configured.
    let e =
        r.a.call("call.play", json!({"call_id":"c999","wav_base64":good}))
            .await
            .unwrap_err();
    assert_eq!(kind(&e), ErrorKind::NotFound);
    for (m, p) in [
        ("call.say", json!({"call_id":a,"text":"hello"})),
        ("speech.synthesize", json!({"text":"hello"})),
        (
            "speech.transcribe",
            json!({"wav_base64":wav_b64(600.0, 200)}),
        ),
        ("call.listen", json!({"call_id":b,"transcribe":true})),
    ] {
        let e = r.a.call(m, p).await.unwrap_err();
        assert_eq!(kind(&e), ErrorKind::InvalidState, "{m}: {e}");
    }
}

// ---- speech hooks (unix stubs) -------------------------------------------------------------

#[cfg(unix)]
mod speech {
    use super::*;

    fn write_fixture(dir: &std::path::Path) {
        std::fs::write(
            dir.join("fixture.wav"),
            encode_wav_i16(&sine(16_000, 600.0, 1000, 0.5), 16_000),
        )
        .unwrap();
    }

    /// `[speech]` with a TTS stub that copies the fixture to `{out}` and an STT stub that
    /// echoes fixed text.
    const SPEECH: &str = "\n[speech]\n\
        tts_command = [\"sh\", \"-c\", \"cp fixture.wav \\\"$1\\\"\", \"sh\", \"{out}\"]\n\
        stt_command = [\"sh\", \"-c\", \"echo hello from the stub\"]\n";

    async fn rig_speech(toml: &str) -> Rig {
        let r = rig_with(toml).await;
        write_fixture(r.dir.path());
        r
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn synthesize_transcribe_and_say() {
        let mut r = rig_speech(SPEECH).await;
        // speech.synthesize: inline (16 kHz) and to a file (48 kHz).
        let s =
            r.a.call("speech.synthesize", json!({"text":"hello there"}))
                .await
                .unwrap();
        assert!(near(s["duration_ms"].as_u64().unwrap(), 1000, 60), "{s}");
        assert!(s["tts_ms"].is_u64());
        let (rate, samples) = decode_audio(&s);
        assert_eq!(rate, 16_000);
        assert!(ratio(&samples[1_000..15_000], rate, 600.0) > 0.7);
        let f =
            r.a.call("speech.synthesize", json!({"text":"hi","path":"tts.wav"}))
                .await
                .unwrap();
        assert!(f["path"].as_str().unwrap().ends_with("tts.wav"));
        assert!(f.get("audio").is_none());
        assert_eq!(
            hound::WavReader::open(r.dir.path().join("tts.wav"))
                .unwrap()
                .spec()
                .sample_rate,
            48_000
        );

        // speech.transcribe from path and inline.
        let t =
            r.a.call("speech.transcribe", json!({"path":"fixture.wav"}))
                .await
                .unwrap();
        assert_eq!(t["transcript"], "hello from the stub");
        assert!(near(t["duration_ms"].as_u64().unwrap(), 1000, 20), "{t}");
        let t =
            r.a.call(
                "speech.transcribe",
                json!({"wav_base64":wav_b64(600.0, 500),"language":"en"}),
            )
            .await
            .unwrap();
        assert_eq!(t["transcript"], "hello from the stub");
        let e =
            r.a.call(
                "speech.transcribe",
                json!({"path":"fixture.wav","language":"-x"}),
            )
            .await
            .unwrap_err();
        assert_eq!(kind(&e), ErrorKind::InvalidParams);

        // call.say with wait, heard and transcribed by bob.
        let (a, b) = establish_silent(&mut r).await;
        let since = rx_ms(&mut r.b, &b).await;
        let say = r.a.call(
            "call.say",
            json!({"call_id":a,"text":"hello bob","wait":true,"timeout_ms":20_000}),
        );
        let listen = r.b.call(
            "call.listen",
            json!({"call_id":b,"since_rx_ms":since,"end_silence_ms":500,"transcribe":true}),
        );
        let (say, listen) = tokio::join!(say, listen);
        let say = say.unwrap();
        assert_eq!(say["finished"]["reason"], "completed", "{say}");
        assert!(say["tts_ms"].is_u64());
        assert!(near(say["duration_ms"].as_u64().unwrap(), 1000, 60));
        let listen = listen.unwrap();
        assert_eq!(listen["speech"], true, "{listen}");
        assert_eq!(listen["transcript"], "hello from the stub");
        assert!(listen["stt_ms"].is_u64());
        assert!(
            listen.get("audio").is_none(),
            "transcribe defaults audio to none"
        );
        // rx_audio can transcribe too, and audio can be forced on.
        let x =
            r.b.call(
                "call.rx_audio",
                json!({"call_id":b,"last_ms":1500,"transcribe":true,"audio":"base64"}),
            )
            .await
            .unwrap();
        assert_eq!(x["transcript"], "hello from the stub");
        assert!(x["audio"]["base64"].is_string());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stt_timeout_is_a_timeout_error_and_failures_are_internal() {
        let toml = "\n[speech]\nstt_command = [\"sh\", \"-c\", \"sleep 30\"]\nstt_timeout_ms = 1000\n\
                    tts_command = [\"sh\", \"-c\", \"echo oops >&2; exit 3\"]\n";
        let mut r = rig_speech(toml).await;
        let e =
            r.a.call("speech.transcribe", json!({"path":"fixture.wav"}))
                .await
                .unwrap_err();
        assert_eq!(kind(&e), ErrorKind::Timeout, "{e}");
        let e =
            r.a.call("speech.synthesize", json!({"text":"x"}))
                .await
                .unwrap_err();
        assert_eq!(kind(&e), ErrorKind::Internal, "{e}");
        assert_eq!(api(&e).extra.as_ref().unwrap()["exit_code"], 3);
        // A failing STT after a listen keeps the window in the error.
        let (a, b) = establish_silent(&mut r).await;
        let since = rx_ms(&mut r.b, &b).await;
        let play = r.a.call(
            "call.play",
            json!({"call_id":a,"wav_base64":wav_b64(600.0, 600),"wait":true}),
        );
        let listen = r.b.call(
            "call.listen",
            json!({"call_id":b,"since_rx_ms":since,"end_silence_ms":300,"transcribe":true}),
        );
        let (_, listen) = tokio::join!(play, listen);
        let e = listen.unwrap_err();
        assert_eq!(kind(&e), ErrorKind::Timeout, "{e}");
        assert!(api(&e).extra.as_ref().unwrap()["end_rx_ms"].is_u64());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn speech_config_hot_reloads() {
        let mut r = rig_speech("").await;
        let e =
            r.a.call("speech.synthesize", json!({"text":"x"}))
                .await
                .unwrap_err();
        assert_eq!(kind(&e), ErrorKind::InvalidState);

        let accounts = r.accounts_text("");
        write_atomic(&r.path(), &format!("{accounts}{SPEECH}"));
        r.a.call("config.reload", Value::Null).await.unwrap();
        let s =
            r.a.call("speech.synthesize", json!({"text":"x"}))
                .await
                .unwrap();
        assert!(s["duration_ms"].as_u64().unwrap() > 900);

        write_atomic(
            &r.path(),
            &format!(
                "{accounts}\n[speech]\ntts_command = []\nstt_command = [\"sh\", \"-c\", \"echo second\"]\n"
            ),
        );
        r.a.call("config.reload", Value::Null).await.unwrap();
        let e =
            r.a.call("speech.synthesize", json!({"text":"x"}))
                .await
                .unwrap_err();
        assert_eq!(kind(&e), ErrorKind::InvalidState);
        let t =
            r.a.call("speech.transcribe", json!({"path":"fixture.wav"}))
                .await
                .unwrap();
        assert_eq!(t["transcript"], "second");
    }
}

// ---- agent capture indicator ---------------------------------------------------------------

/// `agent_capture` of a call as `call.get` reports it.
async fn capture_of(c: &mut ControlClient, call: &str) -> Value {
    c.call("call.get", json!({"call_id": call})).await.unwrap()["agent_capture"].clone()
}

/// Polls (bounded) until the call reports `want`; the requests under test run for seconds, so
/// the state is held long enough to be seen.
async fn wait_capture(c: &mut ControlClient, call: &str, want: Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let got = capture_of(c, call).await;
        if got == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "agent_capture stayed {got}, wanted {want}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// While a listen runs the call reports `agent_capture: ["listen"]` (also in `call.list`); it
/// is empty before and after, on the success path, after a failed request and when the call
/// ends under the listen.
#[tokio::test(flavor = "multi_thread")]
async fn agent_capture_is_set_during_a_listen_and_cleared_after() {
    let mut r = rig_with("").await;
    let (a, b) = establish_silent(&mut r).await;
    assert_eq!(capture_of(&mut r.b, &a).await, json!([]));

    // Nobody speaks: the listen ends with no_speech after its start timeout.
    let listen = r.a.call(
        "call.listen",
        json!({"call_id":a,"start_timeout_ms":1500,"audio":"none"}),
    );
    let watch = async {
        wait_capture(&mut r.b, &a, json!(["listen"])).await;
        let list = r.b.call("call.list", json!({})).await.unwrap();
        let c = list["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == a)
            .expect("the call in call.list");
        assert_eq!(c["agent_capture"], json!(["listen"]), "{c}");
    };
    let (res, ()) = tokio::join!(listen, watch);
    assert_eq!(res.unwrap()["reason"], "no_speech");
    assert_eq!(capture_of(&mut r.b, &a).await, json!([]));

    // A failing request does not leave it set.
    let e =
        r.a.call(
            "call.listen",
            json!({"call_id":a,"transcribe":true,"audio":"bogus"}),
        )
        .await
        .unwrap_err();
    let _ = kind(&e);
    assert_eq!(capture_of(&mut r.b, &a).await, json!([]));

    // The far end hangs up under a running listen: it ends and the flag is gone.
    let listen = r.a.call(
        "call.listen",
        json!({"call_id":a,"start_timeout_ms":8000,"audio":"none"}),
    );
    let hangup = async {
        wait_capture(&mut r.b, &a, json!(["listen"])).await;
        r.b.call("call.hangup", json!({"call_id":b})).await.unwrap();
    };
    let (res, ()) = tokio::join!(listen, hangup);
    // Ok(call_ended) or an error: either way the request is over.
    drop(res);
    assert_eq!(capture_of(&mut r.a, &a).await, json!([]));
}

/// A recording counts until `call.record_stop`; rotating to a new file keeps it on.
#[tokio::test(flavor = "multi_thread")]
async fn agent_capture_covers_a_recording_and_survives_rotation() {
    let mut r = rig_with("").await;
    let (a, _b) = establish_silent(&mut r).await;
    let rec = |name: &str| r.dir.path().join(name).display().to_string();
    let (pa, pb) = (rec("a.wav"), rec("b.wav"));
    r.a.call("call.record_start", json!({"call_id":a,"path":pa}))
        .await
        .unwrap();
    assert_eq!(capture_of(&mut r.a, &a).await, json!(["record"]));
    // Rotation: the old file finishes (call.record_finished) while the new one runs.
    let s = seq(&mut r.a).await;
    r.a.call("call.record_start", json!({"call_id":a,"path":pb}))
        .await
        .unwrap();
    let ev =
        r.a.call(
            "events.poll",
            json!({"since":s,"kinds":["call.record_finished"],"timeout_ms":10_000}),
        )
        .await
        .unwrap();
    assert!(!ev["events"].as_array().unwrap().is_empty(), "{ev}");
    assert_eq!(capture_of(&mut r.a, &a).await, json!(["record"]));
    r.a.call("call.record_stop", json!({"call_id":a}))
        .await
        .unwrap();
    assert_eq!(capture_of(&mut r.a, &a).await, json!([]));
}

/// A `record:` sink records the whole call and counts too.
#[tokio::test(flavor = "multi_thread")]
async fn agent_capture_covers_a_record_sink() {
    let mut r = rig_with("").await;
    let path = r.dir.path().join("sink.wav").display().to_string();
    let b_seq = seq(&mut r.b).await;
    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"silence",
                   "sink":format!("record:{path}")}),
        )
        .await
        .unwrap();
    let a = d["call_id"].as_str().unwrap().to_string();
    let ringing =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":b_seq,"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let b = ringing["call"]["id"].as_str().unwrap().to_string();
    r.b.call("call.answer", json!({"call_id": b}))
        .await
        .unwrap();
    wait_capture(&mut r.a, &a, json!(["record"])).await;
    r.a.call("call.hangup", json!({"call_id": a}))
        .await
        .unwrap();
    wait_capture(&mut r.a, &a, json!([])).await;
}
