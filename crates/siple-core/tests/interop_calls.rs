//! Calls through a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"), driven via the control API.
//! Gated: `SIPLE_INTEROP=1`. The PBX rejects OSRTP offers (RTP/AVP + a=crypto) with 488, so
//! non-TLS accounts use `srtp = "off"` (as in the siple-sip interop test). Uses users 1009 (UDP), 1010 (TCP) and 1011 (TLS) only.
#![cfg(unix)]

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

/// The tests share the PBX channel count: run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() -> bool {
    std::env::var("SIPLE_INTEROP").is_ok_and(|v| v == "1")
}

fn pbx_env() -> HashMap<String, String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/interop/pbx.env");
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn fs_channels() -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            pbx_env().get("PBX_SSH").map_or("", String::as_str),
            "docker exec siple-fs fs_cli -x 'show channels'",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Waits until FreeSWITCH reports no channels. `None` if ssh is unavailable.
fn fs_channels_drained() -> Option<bool> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = fs_channels()?;
        eprintln!("fs_cli show channels: {}", out.trim().replace('\n', " | "));
        if out.contains("0 total") {
            return Some(true);
        }
        if Instant::now() > deadline {
            return Some(false);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    app: App,
    c: ControlClient,
}

fn account(env: &HashMap<String, String>, user: &str, transport: &str, extra: &str) -> String {
    format!(
        "[[account]]\nid = \"u{user}\"\nusername = \"{user}\"\npassword = \"{}\"\n\
         domain = \"{}\"\nregistrar = \"{}\"\ntransport = \"{transport}\"\ntls_verify = false\n\
         expires = 120\nkeepalive_secs = 0\naudio_source = \"silence\"\naudio_sink = \"null\"\n{extra}\n",
        env["PBX_PASSWORD"], env["PBX_DOMAIN"], env["PBX_HOST"]
    )
}

/// `SIPLE_INTEROP_RTP_RANGE=20000-20999` keeps our RTP ports clear of FreeSWITCH's
/// 16384-16683 when both share a host network.
fn media_prefix() -> String {
    let Ok(r) = std::env::var("SIPLE_INTEROP_RTP_RANGE") else {
        return String::new();
    };
    let (a, b) = r
        .split_once('-')
        .expect("SIPLE_INTEROP_RTP_RANGE=START-END");
    format!("[media]\nrtp_port_range = [{}, {}]\n\n", a.trim(), b.trim())
}

async fn rig(accounts: &str, ids: &[&str]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("{}{accounts}", media_prefix())).unwrap();
    let mut o = AppOptions::new(&path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    let app = App::start(o).await.unwrap();
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "interop")
        .await
        .unwrap();
    // Wait for every account to register.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let l = c.call("account.list", Value::Null).await.unwrap();
        let reg = |id: &str| {
            l["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == id && a["registration"]["state"] == "registered")
        };
        if ids.iter().all(|i| reg(i)) {
            break;
        }
        assert!(Instant::now() < deadline, "registration timed out: {l}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Rig { _dir: dir, app, c }
}

async fn wait_state(c: &mut ControlClient, call_id: &str, state: &str) -> Value {
    c.call(
        "call.wait_for",
        json!({"call_id": call_id, "state": state, "timeout_ms": 20_000}),
    )
    .await
    .unwrap_or_else(|e| panic!("wait_for {call_id} {state}: {e}"))
}

/// Prints the SIP trace of a call (debugging aid for failures).
async fn dump_trace(c: &mut ControlClient, call_id: &str) {
    if let Ok(t) = c.call("call.sip_trace", json!({"call_id": call_id})).await {
        for m in t["messages"].as_array().into_iter().flatten() {
            eprintln!("{}", m.as_str().unwrap_or(""));
        }
    }
}

async fn detect(c: &mut ControlClient, call_id: &str, hz: f64) -> Value {
    c.call(
        "call.detect_tone",
        json!({"call_id": call_id, "freq_hz": hz, "timeout_ms": 8000}),
    )
    .await
    .unwrap()
}

async fn dial(c: &mut ControlClient, account: &str, target: &str, extra: Value) -> String {
    let mut p = json!({"account": account, "target": target});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    let d = c.call("call.dial", p).await.unwrap();
    d["call_id"].as_str().unwrap().to_string()
}

/// The PBX tone extension plays a 1000 Hz tone: proves audio through FreeSWITCH to us.
#[tokio::test(flavor = "multi_thread")]
async fn pbx_tone_and_echo() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let env = pbx_env();
    let mut r = rig(&account(&env, "1009", "udp", "srtp = \"off\""), &["u1009"]).await;

    // 9999: FreeSWITCH plays a tone to us.
    let call = dial(
        &mut r.c,
        "u1009",
        &env["PBX_TONE_EXT"],
        json!({"sink":"null"}),
    )
    .await;
    if r.c
        .call(
            "call.wait_for",
            json!({"call_id": call, "state":"active","timeout_ms":20_000}),
        )
        .await
        .is_err()
    {
        dump_trace(&mut r.c, &call).await;
    }
    let active = wait_state(&mut r.c, &call, "active").await;
    eprintln!("9999 negotiated: {}", active["call"]["negotiated"]);
    let d = detect(&mut r.c, &call, 1000.0).await;
    eprintln!("9999 detect 1000 Hz: {d}");
    assert_eq!(d["detected"], true, "no 1000 Hz tone from the PBX: {d}");
    let g =
        r.c.call("call.get", json!({"call_id": call}))
            .await
            .unwrap();
    assert!(g["media"]["rx_packets"].as_u64().unwrap() > 0);
    r.c.call("call.hangup", json!({"call_id": call}))
        .await
        .unwrap();
    wait_state(&mut r.c, &call, "ended").await;

    // 9196: echo. Our 700 Hz tone comes back.
    let call = dial(
        &mut r.c,
        "u1009",
        &env["PBX_ECHO_EXT"],
        json!({"source":"tone:700","sink":"null"}),
    )
    .await;
    wait_state(&mut r.c, &call, "active").await;
    let d = detect(&mut r.c, &call, 700.0).await;
    eprintln!("9196 echo detect 700 Hz: {d}");
    assert_eq!(d["detected"], true, "echo did not return 700 Hz: {d}");
    // DTMF to the PBX does not break the call.
    r.c.call(
        "call.dtmf",
        json!({"call_id": call, "digits": "5", "mode": "rfc4733"}),
    )
    .await
    .unwrap();
    r.c.call("call.hangup", json!({"call_id": call}))
        .await
        .unwrap();
    wait_state(&mut r.c, &call, "ended").await;

    // SIP trace of the PBX call has the 200 OK.
    let t =
        r.c.call("call.sip_trace", json!({"call_id": call}))
            .await
            .unwrap();
    assert!(t["count"].as_u64().unwrap() >= 4, "{t}");

    r.app.shutdown().await;
    if let Some(drained) = fs_channels_drained() {
        assert!(drained, "FreeSWITCH still has channels");
    }
}

/// 1010 (TCP) calls 1011 (TLS) through the PBX; tones in both directions. Tries SRTP
/// mandatory on 1011 first and falls back to off if the PBX bridge cannot do it.
#[tokio::test(flavor = "multi_thread")]
async fn bridged_call_tcp_to_tls() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let env = pbx_env();
    let mut used = "mandatory";
    let mut outcome = bridged(&env, "mandatory").await;
    if let Err(e) = &outcome {
        eprintln!("srtp=mandatory on 1011 failed: {e}\nretrying with srtp=off");
        used = "off";
        outcome = bridged(&env, "off").await;
    }
    eprintln!("bridged call srtp policy used on 1011: {used}");
    outcome.unwrap_or_else(|e| panic!("bridged call failed: {e}"));
    if let Some(drained) = fs_channels_drained() {
        assert!(drained, "FreeSWITCH still has channels");
    }
}

async fn bridged(env: &HashMap<String, String>, srtp_1011: &str) -> Result<(), String> {
    let cfg = format!(
        "{}{}",
        account(env, "1010", "tcp", "srtp = \"off\""),
        account(env, "1011", "tls", &format!("srtp = \"{srtp_1011}\""))
    );
    let mut r = rig(&cfg, &["u1010", "u1011"]).await;
    let result = async {
        let seq = r.c.call("events.current_seq", Value::Null).await.unwrap()["seq"].clone();
        let a = dial(
            &mut r.c,
            "u1010",
            "1011",
            json!({"source":"tone:1000","sink":"null"}),
        )
        .await;
        let ringing =
            r.c.call(
                "call.wait_for",
                json!({"account":"u1011","state":"ringing","since":seq,"timeout_ms":15_000}),
            )
            .await
            .map_err(|e| format!("1011 never rang: {e}"))?;
        let b = ringing["call"]["id"].as_str().unwrap().to_string();
        r.c.call(
            "call.answer",
            json!({"call_id": b, "source":"tone:1500","sink":"null"}),
        )
        .await
        .map_err(|e| format!("answer: {e}"))?;
        r.c.call(
            "call.wait_for",
            json!({"call_id": a, "state":"active","timeout_ms":15_000}),
        )
        .await
        .map_err(|e| format!("1010 not active: {e}"))?;
        r.c.call(
            "call.wait_for",
            json!({"call_id": b, "state":"active","timeout_ms":15_000}),
        )
        .await
        .map_err(|e| format!("1011 not active: {e}"))?;
        let ga = r.c.call("call.get", json!({"call_id": a})).await.unwrap();
        let gb = r.c.call("call.get", json!({"call_id": b})).await.unwrap();
        eprintln!(
            "1010 negotiated {} | 1011 negotiated {}",
            ga["negotiated"], gb["negotiated"]
        );
        let (da, db) = (
            detect(&mut r.c, &a, 1500.0).await,
            detect(&mut r.c, &b, 1000.0).await,
        );
        eprintln!("1010 hears 1500: {da}\n1011 hears 1000: {db}");
        if da["detected"] != true || db["detected"] != true {
            return Err(format!("tones not detected both ways: {da} / {db}"));
        }
        Ok::<_, String>((a, b))
    }
    .await;
    // Always hang up.
    if let Ok((a, b)) = &result {
        let _ = r.c.call("call.hangup", json!({"call_id": a})).await;
        wait_state(&mut r.c, a, "ended").await;
        wait_state(&mut r.c, b, "ended").await;
    } else {
        let l = r.c.call("call.list", json!({"active_only": true})).await;
        for c in l.iter().flat_map(|l| l["calls"].as_array().unwrap()) {
            let _ = r.c.call("call.hangup", json!({"call_id": c["id"]})).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    r.app.shutdown().await;
    result.map(|_| ())
}
