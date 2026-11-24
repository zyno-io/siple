//! Regression tests for review fixes in the core / control layer (see the individual test
//! names); each asserts the fixed behavior.
#![cfg(unix)]

mod common;
use common::*;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use siple_core::control::ErrorKind;
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::state::PartialSelection;
use siple_core::{App, AppOptions, LoggingMode};

fn api_kind(e: &ClientError) -> ErrorKind {
    match e {
        ClientError::Api(a) => a.kind,
        other => panic!("expected an API error, got {other}"),
    }
}

fn opts(path: &std::path::Path) -> AppOptions {
    let mut o = AppOptions::new(path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    o
}

/// A 0.2 s mono 8 kHz WAV.
fn write_wav(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 8000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..1600 {
        w.write_sample(((i as f32 * 0.3).sin() * 8000.0) as i16)
            .unwrap();
    }
    w.finalize().unwrap();
}

// ---- item 2: a failed start / rebind releases everything ----------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn failed_start_releases_listeners_and_a_second_start_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let accts = acct("a1", free_udp_port(), "");
    std::fs::write(
        &path,
        format!("[control]\ntcp_listen = \"127.0.0.1:{port}\"\n{accts}"),
    )
    .unwrap();

    let e = App::start(opts(&path))
        .await
        .err()
        .expect("start must fail");
    assert!(e.to_string().contains("cannot bind"), "{e}");
    let sock = dir.path().join("control.sock");
    assert!(
        !sock.exists(),
        "socket file left behind by the failed start"
    );
    assert!(!dir.path().join("runtime.json").exists());
    assert!(ControlClient::connect_unix(&sock).await.is_err());

    // Same process, fixed config: must succeed (the instance lock and the socket are free).
    std::fs::write(
        &path,
        format!("[control]\ntcp_listen = \"127.0.0.1:0\"\n{accts}"),
    )
    .unwrap();
    let app = App::start(opts(&path)).await.expect("second start");
    let mut c = client(&app, "t").await;
    c.call("ping", Value::Null).await.unwrap();
    drop(c);
    app.shutdown().await;
    assert!(!sock.exists(), "socket file left after shutdown");
    drop(busy);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_control_rebind_changes_nothing_and_later_rebind_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let accts = acct("a1", free_udp_port(), "");
    std::fs::write(&path, &accts).unwrap();
    let app = start_app(&path).await;
    let sock = dir.path().join("control.sock");
    let mut c = client(&app, "t").await;

    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    write_atomic(
        &path,
        &format!("[control]\ntcp_listen = \"127.0.0.1:{port}\"\n{accts}"),
    );
    let r1 = app.reload().await;
    assert!(r1.errors.iter().any(|e| e.contains("control")), "{r1:?}");
    assert!(app.control_endpoints().await.tcp.is_none());
    // The unix endpoint is untouched and the existing connection still works.
    c.call("ping", Value::Null).await.unwrap();

    write_atomic(
        &path,
        &format!("[control]\ntcp_listen = \"127.0.0.1:0\"\n{accts}"),
    );
    let r2 = app.reload().await;
    assert!(r2.errors.is_empty(), "{r2:?}");
    assert!(app.control_endpoints().await.tcp.is_some());
    c.call("ping", Value::Null).await.unwrap();

    drop(c);
    app.shutdown().await;
    assert!(!sock.exists());
    drop(busy);
}

// ---- item 9: only changed endpoints restart -------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn control_change_keeps_unchanged_connections_and_answers_config_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let accts = acct("a1", free_udp_port(), "");
    std::fs::write(&path, &accts).unwrap();
    let app = start_app(&path).await;
    let mut c = client(&app, "k").await;
    let mut sub = client(&app, "sub").await;
    sub.call("events.subscribe", json!({})).await.unwrap();

    // Adds a TCP endpoint; the unix socket is unchanged.
    write_atomic(
        &path,
        &format!("[control]\ntcp_listen = \"127.0.0.1:0\"\n{accts}"),
    );
    let r = c
        .call("config.reload", Value::Null)
        .await
        .expect("reply must arrive");
    assert_eq!(r["applied"], true);
    assert!(
        r["summary"].to_string().contains("control endpoints"),
        "{r}"
    );
    // The subscriber on the unchanged socket is still connected and still gets events.
    let first = tokio::time::timeout(Duration::from_secs(3), sub.next_event())
        .await
        .expect("subscriber starved")
        .expect("subscriber connection dropped by a control change");
    assert!(first.seq > 0);
    c.call("account.set_dnd", json!({"account":"a1","enabled":true}))
        .await
        .unwrap();
    // (drain until the dnd event shows up on the subscription)
    let mut saw = false;
    while let Ok(Ok(e)) = tokio::time::timeout(Duration::from_secs(3), sub.next_event()).await {
        if e.kind == "account.dnd" {
            saw = true;
            break;
        }
    }
    assert!(saw, "subscription broken after control change");

    // Removing TCP again: the unix connections survive that too.
    write_atomic(&path, &accts);
    let r = c.call("config.reload", Value::Null).await.unwrap();
    assert_eq!(r["applied"], true);
    assert!(app.control_endpoints().await.tcp.is_none());
    c.call("ping", Value::Null).await.unwrap();
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reload_reply_survives_the_requesters_own_endpoint_changing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let accts = acct("a1", free_udp_port(), "");
    std::fs::write(&path, &accts).unwrap();
    let app = start_app(&path).await;
    let old_sock = dir.path().join("control.sock");
    let mut c = client(&app, "k").await;

    let new_sock = dir.path().join("moved.sock");
    write_atomic(
        &path,
        &format!(
            "[control]\nunix_socket = \"{}\"\n{accts}",
            new_sock.display()
        ),
    );
    // This request arrives over the endpoint that is being replaced.
    let r = c
        .call("config.reload", Value::Null)
        .await
        .expect("reply lost");
    assert_eq!(r["applied"], true, "{r}");
    assert!(new_sock.exists());
    assert!(!old_sock.exists(), "old socket file not removed");
    // New connections go to the new endpoint (runtime.json was rewritten).
    let mut c2 = client(&app, "k2").await;
    c2.call("ping", Value::Null).await.unwrap();
    // The old connection is closed after the grace period.
    let closed = tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            if c.call("ping", Value::Null).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "retired endpoint kept its connections forever"
    );
    drop(c2);
    app.shutdown().await;
    assert!(!new_sock.exists());
}

// ---- item 1: [media] changes reach the accounts ---------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn media_codecs_change_applies_to_next_call() {
    let mut r = rig("").await;
    let (a, _b) = establish(&mut r).await;
    let g = r.a.call("call.get", json!({"call_id": a})).await.unwrap();
    assert_ne!(
        g["negotiated"]["codec"], "pcma",
        "precondition: default codecs"
    );
    r.a.call("call.hangup", json!({"call_id": a}))
        .await
        .unwrap();
    r.a.call("call.wait_for", json!({"call_id": a, "state":"ended"}))
        .await
        .unwrap();

    let text = format!("[media]\ncodecs = [\"pcma\"]\n{}", r.accounts_text(""));
    write_atomic(&r.path(), &text);
    let rep = r.app.reload().await;
    assert!(rep.errors.is_empty(), "{rep:?}");
    assert!(
        rep.summary.iter().any(|s| s.contains("media settings")),
        "{rep:?}"
    );
    let (a2, _b2) = establish(&mut r).await;
    let g = r.a.call("call.get", json!({"call_id": a2})).await.unwrap();
    assert_eq!(g["negotiated"]["codec"], "pcma", "{g}");
    r.app.shutdown().await;
}

// ---- item 3: restart detection --------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn instance_id_in_hello_status_and_results_and_stale_since_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, acct("a1", free_udp_port(), "")).unwrap();
    let app = start_app(&path).await;
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.clone().unwrap())
        .await
        .unwrap();
    let hello = c.hello(Some(app.token().expose()), "t").await.unwrap();
    let id = hello["instance_id"].as_str().unwrap().to_string();
    assert_eq!(id.len(), 16);
    assert!(hello["started_at"].is_string());
    assert_eq!(c.instance_id(), Some(id.as_str()));

    let st = c.call("status", Value::Null).await.unwrap();
    assert_eq!(st["instance_id"], id);
    assert!(st["started_at"].is_string());
    let r = c
        .call("account.set_dnd", json!({"account":"a1","enabled":true}))
        .await
        .unwrap();
    assert_eq!(
        r["instance_id"], id,
        "state-changing results carry the instance id"
    );
    assert!(r["event_seq"].is_u64());

    // `since` ahead of the log: the previous run's position.
    let cur = c.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();
    let e = c
        .call(
            "events.poll",
            json!({"since": cur + 523, "timeout_ms": 5000}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidParams);
    assert!(e.to_string().contains("events.current_seq"), "{e}");
    assert!(e.to_string().contains("restarted"), "{e}");
    let e = c
        .call(
            "call.wait_for",
            json!({"account":"a1","state":"ringing","since": cur + 523,"timeout_ms":5000}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidParams);
    // A valid `since` (== current) still works and returns promptly with no events.
    let t = Instant::now();
    let ok = c
        .call("events.poll", json!({"since": cur, "timeout_ms": 50}))
        .await
        .unwrap();
    assert_eq!(ok["events"], json!([]));
    assert!(t.elapsed() < Duration::from_secs(2));
    drop(c);
    app.shutdown().await;

    // A new start has a new id.
    let app2 = start_app(&path).await;
    let mut c2 = client(&app2, "t2").await;
    let st2 = c2.call("status", Value::Null).await.unwrap();
    assert_ne!(st2["instance_id"], id);
    drop(c2);
    app2.shutdown().await;
}

// ---- item 5: call.dtmf bounds ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn dtmf_arguments_are_bounded() {
    let mut r = rig("").await;
    let (a, b) = establish(&mut r).await;
    let t = Instant::now();
    for (digits, dur) in [
        ("12", json!(3_000_000_000u64)),
        ("12", json!(4_294_967_295u64)),
        ("1", json!(20_000)),
        ("1", json!(9)),
        ("1", json!(0)),
    ] {
        let e =
            r.a.call(
                "call.dtmf",
                json!({"call_id": a, "digits": digits, "mode": "info", "duration_ms": dur}),
            )
            .await
            .unwrap_err();
        assert_eq!(api_kind(&e), ErrorKind::InvalidParams, "{dur}: {e}");
    }
    for digits in [
        "1".repeat(33),
        "12x".to_string(),
        "".to_string(),
        "1e".to_string(),
    ] {
        let e =
            r.a.call("call.dtmf", json!({"call_id": a, "digits": digits}))
                .await
                .unwrap_err();
        assert_eq!(api_kind(&e), ErrorKind::InvalidParams, "{digits:?}: {e}");
    }
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "validation must be immediate"
    );

    // The edges are accepted.
    let s = seq(&mut r.b).await;
    r.a.call(
        "call.dtmf",
        json!({"call_id": a, "digits": "1#A", "mode": "rfc4733", "duration_ms": 10}),
    )
    .await
    .unwrap();
    r.b.call(
        "call.expect_dtmf",
        json!({"call_id": b, "digits": "1#A", "since": s, "timeout_ms": 5000}),
    )
    .await
    .unwrap();
    r.app.shutdown().await;
}

// ---- item 7: device choices live in the config file -------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn device_choice_is_written_to_config_and_survives_restarts_and_unrelated_edits() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let port = free_udp_port();
    std::fs::write(
        &path,
        format!(
            "# my notes\n[audio]\noutput = \"Cfg Spk\"   # desk speakers\n{}",
            acct("a1", port, "")
        ),
    )
    .unwrap();
    {
        let app = start_app(&path).await;
        let e = app
            .set_devices(&PartialSelection {
                output: Some("User Spk".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(e.output.name, "User Spk");
        app.shutdown().await;
    }
    // The choice is in the file, in place, with the user's comments; state.toml is not involved.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("# my notes\n[audio]\noutput = \"User Spk\"   # desk speakers\n"),
        "{text}"
    );
    assert!(!dir.path().join("state.toml").exists());
    // An unrelated edit while stopped (dnd on the account) keeps it.
    std::fs::write(
        &path,
        text.replace("register = false", "register = false\ndnd = true"),
    )
    .unwrap();
    {
        let app = start_app(&path).await;
        let e = app.effective_devices();
        assert_eq!(e.output.name, "User Spk", "{e:?}");
        app.shutdown().await;
    }
    // Editing the [audio] value itself while stopped is simply what the file says.
    std::fs::write(
        &path,
        std::fs::read_to_string(&path)
            .unwrap()
            .replace("User Spk", "Other Cfg Spk"),
    )
    .unwrap();
    {
        let app = start_app(&path).await;
        assert_eq!(app.effective_devices().output.name, "Other Cfg Spk");
        app.shutdown().await;
    }
}

// ---- item 8: media failures are reported ----------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn play_and_record_report_bad_files_and_resolve_relative_paths() {
    let mut r = rig("").await;
    let (a, _b) = establish(&mut r).await;
    let s = seq(&mut r.a).await;

    let e =
        r.a.call(
            "call.play",
            json!({"call_id": a, "source": "file:/nonexistent/x.wav"}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidParams, "{e}");
    let e =
        r.a.call(
            "call.play",
            json!({"call_id": a, "source": "file:missing-relative.wav"}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidParams, "{e}");
    std::fs::write(r.dir.path().join("garbage.wav"), b"not a wav").unwrap();
    let e =
        r.a.call(
            "call.play",
            json!({"call_id": a, "source": "file:garbage.wav"}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidParams, "{e}");
    let e =
        r.a.call(
            "call.record_start",
            json!({"call_id": a, "path": "/nonexistent-dir/rec.wav"}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidParams, "{e}");

    // Relative paths are relative to the config directory, not the working directory.
    write_wav(&r.dir.path().join("beep.wav"));
    r.a.call(
        "call.play",
        json!({"call_id": a, "source": "file:beep.wav"}),
    )
    .await
    .unwrap();
    let rec =
        r.a.call(
            "call.record_start",
            json!({"call_id": a, "path": "out.wav"}),
        )
        .await
        .unwrap();
    let expected = r.dir.path().join("out.wav");
    assert_eq!(
        std::fs::canonicalize(rec["path"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&expected).unwrap()
    );
    assert!(expected.exists());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stop =
        r.a.call("call.record_stop", json!({"call_id": a}))
            .await
            .unwrap();
    assert_eq!(stop["finished"], true, "{stop}");

    // No stray media errors came out of the failed attempts.
    let ev = r.a.call("events.poll", json!({"since": s})).await.unwrap();
    let errs: Vec<_> = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "call.media_error")
        .collect();
    assert!(errs.is_empty(), "{errs:?}");
    r.app.shutdown().await;
}

// ---- item 10: wait_for / expect_dtmf fail fast when the call is over ------------------------

#[tokio::test(flavor = "multi_thread")]
async fn wait_for_since_on_an_ended_call_fails_fast() {
    let mut r = rig("").await;
    r.b.call("account.set_dnd", json!({"account":"bob","enabled":true}))
        .await
        .unwrap();
    let d =
        r.a.call("call.dial", json!({"account":"alice","target":r.bob_uri}))
            .await
            .unwrap();
    let a = d["call_id"].as_str().unwrap().to_string();
    r.a.call(
        "call.wait_for",
        json!({"call_id": a, "state":"ended","timeout_ms":30_000}),
    )
    .await
    .unwrap();
    let s = seq(&mut r.a).await;
    let t = Instant::now();
    let e =
        r.a.call(
            "call.wait_for",
            json!({"call_id": a, "state":"active","since": s, "timeout_ms": 8000}),
        )
        .await
        .unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidState, "{e}");
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "took {:?}",
        t.elapsed()
    );
    // Waiting for `ended` with an old `since` still finds the event.
    let ok =
        r.a.call(
            "call.wait_for",
            json!({"call_id": a, "state":"ended","since": 0, "timeout_ms": 2000}),
        )
        .await
        .unwrap();
    assert_eq!(ok["call"]["state"], "ended");
    r.app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn expect_dtmf_fails_fast_when_the_call_ends() {
    let mut r = rig("").await;
    let (a, b) = establish(&mut r).await;
    let mut hang = client(&r.app, "hang").await;
    let waiter = tokio::spawn(async move {
        let t = Instant::now();
        let res =
            r.a.call(
                "call.expect_dtmf",
                json!({"call_id": a, "digits": "9", "timeout_ms": 20_000}),
            )
            .await;
        (res, t.elapsed(), r)
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    hang.call("call.hangup", json!({"call_id": b}))
        .await
        .unwrap();
    let (res, took, r) = waiter.await.unwrap();
    let e = res.unwrap_err();
    assert_eq!(api_kind(&e), ErrorKind::InvalidState, "{e}");
    assert!(took < Duration::from_secs(5), "took {took:?}");
    r.app.shutdown().await;
}

// ---- item 11: SIP error mapping -------------------------------------------------------------

#[test]
fn sip_errors_map_by_variant_not_by_message_digits() {
    use siple_core::calls::sip_error;
    use siple_sip::SipError;
    let e = sip_error(SipError::Transport(
        "cannot connect to 127.0.0.1:5060: connection refused".into(),
    ));
    assert_eq!((e.kind, e.sip_code), (ErrorKind::SipError, Some(0)));
    let e = sip_error(SipError::Dns("pbx100.example.com did not resolve".into()));
    assert_eq!((e.kind, e.sip_code), (ErrorKind::SipError, Some(0)));
    let e = sip_error(SipError::Tls("handshake failed (error 404)".into()));
    assert_eq!(e.sip_code, Some(0));
    assert_eq!(
        sip_error(SipError::Media(
            "audio source file:/x.wav: No such file".into()
        ))
        .kind,
        ErrorKind::InvalidParams
    );
    assert_eq!(
        sip_error(SipError::InvalidArgument("duration".into())).kind,
        ErrorKind::InvalidParams
    );
    assert_eq!(sip_error(SipError::Timeout).kind, ErrorKind::Timeout);
    assert_eq!(
        sip_error(SipError::UnknownCall("c9".into())).kind,
        ErrorKind::NotFound
    );
}

// ---- item 14: a stale config error clears ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn config_error_clears_when_identical_content_is_restored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let text = acct("a1", free_udp_port(), "");
    std::fs::write(&path, &text).unwrap();
    let app = start_app(&path).await;
    assert!(app.last_config_error().is_none());

    std::fs::remove_file(&path).unwrap();
    let rep = app.reload_if_changed().await;
    assert!(!rep.errors.is_empty(), "{rep:?}");
    assert!(app.last_config_error().is_some());

    write_atomic(&path, &text);
    let rep = app.reload_if_changed().await;
    assert!(rep.errors.is_empty(), "{rep:?}");
    assert!(
        app.last_config_error().is_none(),
        "error from the deleted file is still reported"
    );
    app.shutdown().await;
}

// ---- call.rejected carries the call id for user rejections ----------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn user_reject_event_carries_call_id() {
    let mut r = rig("").await;
    let s = seq(&mut r.b).await;
    let d =
        r.a.call("call.dial", json!({"account":"alice","target":r.bob_uri}))
            .await
            .unwrap();
    let ringing =
        r.b.call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since": s,"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    r.b.call("call.reject", json!({"call_id": b_call, "code": 603}))
        .await
        .unwrap();
    let ev =
        r.b.call(
            "events.poll",
            json!({"since": s, "kinds": ["call.rejected"], "timeout_ms": 5000}),
        )
        .await
        .unwrap();
    let e = &ev["events"][0];
    assert_eq!(e["data"]["reason"], "user", "{ev}");
    assert_eq!(e["call_id"], b_call.as_str(), "{ev}");
    assert_eq!(e["account"], "bob");
    let _ = d;
    r.app.shutdown().await;
}
