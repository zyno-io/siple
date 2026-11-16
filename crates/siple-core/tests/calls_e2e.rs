//! Call control end to end, no PBX: two peer-mode (`register = false`) accounts in ONE app call
//! each other over loopback UDP, driven only through the control API.
#![cfg(unix)]

use std::time::Duration;

mod common;

use common::free_udp_port;
use serde_json::{Value, json};
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::{App, AppOptions, LoggingMode};

struct Rig {
    _dir: tempfile::TempDir,
    app: App,
    a: ControlClient,
    b: ControlClient,
    /// SIP URI of the account `bob`.
    bob_uri: String,
}

async fn client(app: &App, name: &str) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), name).await.unwrap();
    c
}

async fn rig(account_extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let acct = |id: &str, port: u16| {
        format!(
            "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\npassword = \"pw\"\n\
             domain = \"127.0.0.1\"\ntransport = \"udp\"\nregister = false\n\
             bind_address = \"127.0.0.1\"\nlocal_port = {port}\n\
             audio_source = \"silence\"\naudio_sink = \"null\"\n{account_extra}\n"
        )
    };
    let text = format!("{}{}", acct("alice", pa), acct("bob", pb));
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let mut o = AppOptions::new(&path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    let app = App::start(o).await.unwrap();
    let a = client(&app, "alice-ctl").await;
    let b = client(&app, "bob-ctl").await;
    Rig {
        _dir: dir,
        app,
        a,
        b,
        bob_uri: format!("sip:bob@127.0.0.1:{pb}"),
    }
}

async fn wait_state(c: &mut ControlClient, call_id: &str, state: &str) -> Value {
    c.call(
        "call.wait_for",
        json!({"call_id": call_id, "state": state, "timeout_ms": 30_000}),
    )
    .await
    .unwrap_or_else(|e| panic!("wait_for {call_id} {state}: {e}"))
}

async fn detect(c: &mut ControlClient, call_id: &str, hz: f64) -> Value {
    c.call(
        "call.detect_tone",
        json!({"call_id": call_id, "freq_hz": hz, "timeout_ms": 5000}),
    )
    .await
    .unwrap()
}

/// A dials B (tone:1000 / null), B answers (tone:1500 / null); both active. Returns
/// `(a_call, b_call)`.
async fn establish(r: &mut Rig) -> (String, String) {
    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"tone:1000","sink":"null"}),
        )
        .await
        .unwrap();
    let a_call = d["call_id"].as_str().unwrap().to_string();
    assert!(d["event_seq"].as_u64().is_some());

    let ringing =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":d["event_seq"],"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    assert_eq!(ringing["call"]["direction"], "inbound");
    assert_eq!(ringing["call"]["account"], "bob");

    // The incoming call is announced with account + call_id on the envelope.
    let polled = r
        .b
        .call(
            "events.poll",
            json!({"since": d["event_seq"], "kinds":["call.incoming"], "account":"bob", "timeout_ms": 2000}),
        )
        .await
        .unwrap();
    let ev = &polled["events"][0];
    assert_eq!(ev["kind"], "call.incoming");
    assert_eq!(ev["call_id"], b_call.as_str());
    assert_eq!(ev["account"], "bob");
    assert_eq!(ev["data"]["state"], "ringing");

    r.b.call(
        "call.answer",
        json!({"call_id": b_call, "source":"tone:1500", "sink":"null"}),
    )
    .await
    .unwrap();
    let a_active = wait_state(&mut r.a, &a_call, "active").await;
    wait_state(&mut r.b, &b_call, "active").await;
    assert_eq!(a_active["call"]["state"], "active");
    (a_call, b_call)
}

#[tokio::test(flavor = "multi_thread")]
async fn call_flow_tones_dtmf_trace_hangup() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;

    // Real audio path both ways, through the codec and RTP.
    let (da, db) = (
        detect(&mut r.a, &a_call, 1500.0).await,
        detect(&mut r.b, &b_call, 1000.0).await,
    );
    assert_eq!(da["detected"], true, "A did not hear 1500 Hz: {da}");
    assert_eq!(db["detected"], true, "B did not hear 1000 Hz: {db}");
    // ... and not a tone nobody sends.
    let miss =
        r.a.call(
            "call.detect_tone",
            json!({"call_id": a_call, "freq_hz": 2500, "timeout_ms": 600}),
        )
        .await
        .unwrap();
    assert_eq!(miss["detected"], false);

    // call.get: info + media stats + audio binding.
    let g =
        r.a.call("call.get", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert_eq!(g["state"], "active");
    assert_eq!(g["audio"]["source"], "tone:1000");
    assert_eq!(g["audio"]["device_bound"], false);
    assert!(g["media"]["rx_packets"].as_u64().unwrap() > 0, "{g}");
    assert!(g["media"]["tx_packets"].as_u64().unwrap() > 0, "{g}");
    assert!(g["negotiated"]["codec"].is_string());

    // DTMF via RFC 4733: B expects what A sends (since = position before sending).
    let seq = r.a.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();
    let (sent, got) = tokio::join!(
        r.a.call(
            "call.dtmf",
            json!({"call_id": a_call, "digits": "123#", "mode": "rfc4733"})
        ),
        r.b.call(
            "call.expect_dtmf",
            json!({"call_id": b_call, "digits": "123#", "timeout_ms": 5000, "since": seq})
        )
    );
    sent.unwrap();
    let got = got.unwrap();
    assert_eq!(got["matched"], true);
    assert!(got["received"].as_str().unwrap().contains("123#"));
    let dtmf =
        r.b.call(
            "events.poll",
            json!({"since": seq, "kinds":["call.dtmf"], "call_id": b_call, "timeout_ms": 500}),
        )
        .await
        .unwrap();
    assert_eq!(dtmf["events"][0]["account"], "bob");
    assert_eq!(dtmf["events"][0]["data"]["via"], "rfc4733");

    // SIP INFO digits arrive as call.dtmf{via:"info"}.
    let seq = r.a.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();
    r.a.call(
        "call.dtmf",
        json!({"call_id": a_call, "digits": "9", "mode": "info"}),
    )
    .await
    .unwrap();
    let got =
        r.b.call(
            "call.expect_dtmf",
            json!({"call_id": b_call, "digits": "9", "timeout_ms": 5000, "since": seq}),
        )
        .await
        .unwrap();
    assert_eq!(got["received"], "9");
    let info =
        r.b.call(
            "events.poll",
            json!({"since": seq, "kinds":["call.dtmf"], "call_id": b_call}),
        )
        .await
        .unwrap();
    assert_eq!(info["events"][0]["data"]["via"], "info");

    // Wrong digits time out and report what arrived.
    let e =
        r.b.call(
            "call.expect_dtmf",
            json!({"call_id": b_call, "digits": "77", "timeout_ms": 300}),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("timeout"), "{e}");

    // Recording of the received audio.
    let wav = r._dir.path().join("rx.wav");
    r.b.call(
        "call.record_start",
        json!({"call_id": b_call, "path": wav.display().to_string()}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let rec =
        r.b.call("call.record_stop", json!({"call_id": b_call}))
            .await
            .unwrap();
    assert_eq!(rec["finished"], true, "{rec}");
    assert!(std::fs::metadata(&wav).unwrap().len() > 1000);

    // call.play swaps what A transmits: B hears 700 Hz afterwards.
    r.a.call(
        "call.play",
        json!({"call_id": a_call, "source": "tone:700"}),
    )
    .await
    .unwrap();
    assert_eq!(detect(&mut r.b, &b_call, 700.0).await["detected"], true);
    r.a.call("call.stop_play", json!({"call_id": a_call}))
        .await
        .unwrap();

    // SIP trace by cN: the INVITE is in the ring.
    let t =
        r.a.call("call.sip_trace", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert!(t["count"].as_u64().unwrap() >= 3, "{t}");
    assert!(
        t["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.as_str().unwrap().contains("INVITE sip:bob@")),
        "{t}"
    );
    // A raw SIP Call-ID is accepted too.
    let raw =
        r.a.call(
            "call.sip_trace",
            json!({"call_id": t["sip_call_id"].as_str().unwrap()}),
        )
        .await
        .unwrap();
    assert_eq!(raw["count"], t["count"]);

    // status/call.list show both calls.
    let list = r.a.call("call.list", json!({})).await.unwrap();
    assert_eq!(list["calls"].as_array().unwrap().len(), 2);
    let st = r.a.call("status", Value::Null).await.unwrap();
    assert_eq!(st["calls"].as_array().unwrap().len(), 2);
    assert_eq!(st["accounts"][0]["dnd"], false);

    // Mute is local and visible in the call.
    r.a.call("call.mute", json!({"call_id": a_call}))
        .await
        .unwrap();
    let g =
        r.a.call("call.get", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert_eq!(g["muted"], true);
    r.a.call("call.unmute", json!({"call_id": a_call}))
        .await
        .unwrap();

    // Hangup by A ends both.
    let h =
        r.a.call("call.hangup", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert!(h["event_seq"].as_u64().is_some());
    let ea = wait_state(&mut r.a, &a_call, "ended").await;
    let eb = wait_state(&mut r.b, &b_call, "ended").await;
    assert_eq!(ea["call"]["end"]["by"], "local");
    assert_eq!(eb["call"]["end"]["by"], "remote");

    // Errors map to the fixed kinds.
    for (m, p, kind) in [
        ("call.get", json!({"call_id":"c999"}), "not_found"),
        ("call.hangup", json!({"call_id":"c999"}), "not_found"),
        ("call.answer", json!({"call_id": b_call}), "invalid_state"),
        (
            "call.detect_tone",
            json!({"call_id": a_call, "freq_hz": 1000}),
            "invalid_state",
        ),
        (
            "call.dial",
            json!({"account":"nobody","target":"1"}),
            "not_found",
        ),
        (
            "call.dial",
            json!({"account":"alice","target":"1","source":"bogus"}),
            "invalid_params",
        ),
        (
            "call.dial",
            json!({"account":"alice","target":"1".repeat(70_000)}),
            "invalid_params",
        ),
        (
            "call.dial",
            json!({"account":"alice","target":"1","headers":{"X-Big":"a".repeat(70_000)}}),
            "invalid_params",
        ),
        ("call.wait_for", json!({"state":"active"}), "invalid_params"),
        (
            "call.wait_for",
            json!({"call_id": a_call, "state":"active"}),
            "invalid_state",
        ),
    ] {
        let e = r.a.call(m, p).await.unwrap_err();
        assert!(
            e.to_string().contains(kind),
            "{m}: expected {kind}, got {e}"
        );
    }
    r.app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn srtp_mandatory_both_sides() {
    let mut r = rig("srtp = \"mandatory\"").await;
    let (a_call, b_call) = establish(&mut r).await;
    let (da, db) = (
        detect(&mut r.a, &a_call, 1500.0).await,
        detect(&mut r.b, &b_call, 1000.0).await,
    );
    assert_eq!(da["detected"], true, "{da}");
    assert_eq!(db["detected"], true, "{db}");
    let g =
        r.a.call("call.get", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert!(g["negotiated"]["srtp_suite"].is_string(), "not SRTP: {g}");
    assert!(g["media"]["srtp_suite"].is_string(), "{g}");
    assert_eq!(g["media"]["srtp_auth_failures"], 0);
    r.b.call("call.hangup", json!({"call_id": b_call}))
        .await
        .unwrap();
    wait_state(&mut r.a, &a_call, "ended").await;
    r.app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dnd_rejects_with_486() {
    let mut r = rig("").await;
    let seq = r.b.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();
    let on =
        r.b.call("account.set_dnd", json!({"account":"bob","enabled":true}))
            .await
            .unwrap();
    assert_eq!(on["enabled"], true);
    let ev =
        r.b.call(
            "events.poll",
            json!({"since": seq, "kinds":["account.dnd"], "account":"bob", "timeout_ms": 2000}),
        )
        .await
        .unwrap();
    assert_eq!(ev["events"][0]["data"]["enabled"], true);
    let st = r.b.call("status", Value::Null).await.unwrap();
    let bob = st["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "bob")
        .unwrap();
    assert_eq!(bob["dnd"], true);

    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"tone:1000","sink":"null"}),
        )
        .await
        .unwrap();
    let a_call = d["call_id"].as_str().unwrap();
    let ended = wait_state(&mut r.a, a_call, "ended").await;
    assert_eq!(ended["call"]["end"]["code"], 486, "{ended}");
    let rej = r
        .b
        .call(
            "events.poll",
            json!({"since": d["event_seq"], "kinds":["call.rejected"], "account":"bob", "timeout_ms": 3000}),
        )
        .await
        .unwrap();
    let e = &rej["events"][0];
    assert_eq!(e["kind"], "call.rejected");
    assert_eq!(e["data"]["reason"], "dnd");
    assert_eq!(e["data"]["code"], 486);

    // DND off again: the next call rings.
    r.b.call("account.set_dnd", json!({"account":"bob","enabled":false}))
        .await
        .unwrap();
    let (a2, _b2) = establish(&mut r).await;
    r.a.call("call.hangup", json!({"call_id": a2}))
        .await
        .unwrap();
    r.app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reject_and_wait_for_race_freedom() {
    let mut r = rig("").await;
    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"tone:1000","sink":"null"}),
        )
        .await
        .unwrap();
    let a_call = d["call_id"].as_str().unwrap().to_string();
    let b =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let b_call = b["call"]["id"].as_str().unwrap().to_string();
    // Already ringing and no `since`: returns at once.
    let t0 = std::time::Instant::now();
    r.b.call(
        "call.wait_for",
        json!({"call_id": b_call, "state":"ringing", "timeout_ms": 5000}),
    )
    .await
    .unwrap();
    assert!(t0.elapsed() < Duration::from_millis(500));

    r.b.call("call.reject", json!({"call_id": b_call, "code": 603}))
        .await
        .unwrap();
    let ended = wait_state(&mut r.a, &a_call, "ended").await;
    assert_eq!(ended["call"]["end"]["code"], 603);
    // Waiting for a state after the call ended fails fast instead of timing out.
    let e = r
        .a
        .call(
            "call.wait_for",
            json!({"call_id": a_call, "state":"active", "since": d["event_seq"], "timeout_ms": 5000}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&e, ClientError::Api(a) if a.to_string().contains("invalid_state")),
        "{e}"
    );
    r.app.shutdown().await;
}

/// Hold / unhold through the control API with real media: while A holds (sendonly), B answers
/// recvonly, so audio stops in both directions; unhold brings both tones back.
#[tokio::test(flavor = "multi_thread")]
async fn hold_stops_audio_both_ways_and_unhold_resumes() {
    let mut r = rig("").await;
    let (a_call, b_call) = establish(&mut r).await;
    assert_eq!(detect(&mut r.a, &a_call, 1500.0).await["detected"], true);
    assert_eq!(detect(&mut r.b, &b_call, 1000.0).await["detected"], true);

    let h =
        r.a.call("call.hold", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert_eq!(h["state"], "held");
    assert_eq!(h["hold"]["local"], true);
    assert!(h["event_seq"].as_u64().is_some());
    wait_state(&mut r.b, &b_call, "remote_held").await;

    // Let what is in flight drain, then neither side hears the other.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let miss = |id: String, hz: u32| json!({"call_id": id, "freq_hz": hz, "timeout_ms": 2000});
    let (ma, mb) = tokio::join!(
        r.a.call("call.detect_tone", miss(a_call.clone(), 1500)),
        r.b.call("call.detect_tone", miss(b_call.clone(), 1000)),
    );
    assert_eq!(ma.unwrap()["detected"], false, "A still hears B while held");
    assert_eq!(mb.unwrap()["detected"], false, "B still hears A while held");

    let u =
        r.a.call("call.unhold", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert_eq!(u["state"], "active");
    wait_state(&mut r.b, &b_call, "active").await;
    assert_eq!(detect(&mut r.a, &a_call, 1500.0).await["detected"], true);
    assert_eq!(detect(&mut r.b, &b_call, 1000.0).await["detected"], true);

    // Errors: unknown call.
    let e =
        r.a.call("call.hold", json!({"call_id": "c999"}))
            .await
            .unwrap_err();
    assert!(matches!(e, ClientError::Api { .. }), "{e:?}");
    r.a.call("call.hangup", json!({"call_id": a_call}))
        .await
        .unwrap();
    wait_state(&mut r.b, &b_call, "ended").await;
}
