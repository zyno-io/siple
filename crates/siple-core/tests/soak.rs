//! Long SRTP soak through a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"). Gated: `SIPLE_SOAK=1`
//! (never part of a default run; takes ~26 minutes). User 1012 calls the echo extension 9196
//! over TLS with `srtp = mandatory` and `source = tone:700`, and checks every 60 s that the echoed
//! 700 Hz tone is still detected and that no SRTP authentication failure was counted. At 50 pps
//! the 16-bit RTP sequence number wraps after ~21.8 min, which crosses the SRTP rollover counter
//! (ROC) increment in both directions; the check at 25 min sits well past it.
#![cfg(unix)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

const CHECK_EVERY: Duration = Duration::from_secs(60);
const TOTAL: Duration = Duration::from_secs(25 * 60);

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

#[tokio::test(flavor = "multi_thread")]
async fn srtp_soak_crosses_the_rtp_sequence_wrap() {
    if !std::env::var("SIPLE_SOAK").is_ok_and(|v| v == "1") {
        eprintln!("skipped (set SIPLE_SOAK=1; runs ~26 min)");
        return;
    }
    let env = pbx_env();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[[account]]\nid = \"u1012\"\nusername = \"1012\"\npassword = \"{}\"\n\
             domain = \"{}\"\nregistrar = \"{}\"\ntransport = \"tls\"\ntls_verify = false\n\
             expires = 300\nkeepalive_secs = 0\nsrtp = \"mandatory\"\n\
             audio_source = \"silence\"\naudio_sink = \"null\"\n",
            env["PBX_PASSWORD"], env["PBX_DOMAIN"], env["PBX_HOST"]
        ),
    )
    .unwrap();
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
    c.hello(Some(app.token().expose()), "soak").await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let l = c.call("account.list", Value::Null).await.unwrap();
        if l["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "u1012" && a["registration"]["state"] == "registered")
        {
            break;
        }
        assert!(Instant::now() < deadline, "registration timed out: {l}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let d = c
        .call(
            "call.dial",
            json!({"account":"u1012","target":env["PBX_ECHO_EXT"],"source":"tone:700","sink":"null","srtp":"mandatory"}),
        )
        .await
        .unwrap();
    let call = d["call_id"].as_str().unwrap().to_string();
    let active = c
        .call(
            "call.wait_for",
            json!({"call_id": call, "state":"active","timeout_ms":20_000}),
        )
        .await
        .unwrap();
    eprintln!("SOAK negotiated: {}", active["call"]["negotiated"]);
    assert!(
        active["call"]["negotiated"]["srtp_suite"].is_string(),
        "SRTP not negotiated: {active}"
    );

    let start = Instant::now();
    let mut checks = 0u32;
    let mut failures: Vec<String> = Vec::new();
    while start.elapsed() < TOTAL {
        tokio::time::sleep(CHECK_EVERY).await;
        checks += 1;
        let t = start.elapsed().as_secs();
        let det = c
            .call(
                "call.detect_tone",
                json!({"call_id": call, "freq_hz": 700.0, "timeout_ms": 8000}),
            )
            .await;
        let g = c.call("call.get", json!({"call_id": call})).await.unwrap();
        let m = &g["media"];
        let tone = det.as_ref().map(|d| d["detected"] == true).unwrap_or(false);
        eprintln!(
            "SOAK t={t:>4}s state={} tone={tone} tx={} rx={} auth_fail={} rx_drops={} lost={} late={}",
            g["state"],
            m["tx_packets"],
            m["rx_packets"],
            m["srtp_auth_failures"],
            m["rx_source_drops"],
            m["jitter"]["lost"],
            m["jitter"]["late"],
        );
        if g["state"] != "active" {
            failures.push(format!("t={t}s call state {}", g["state"]));
            break;
        }
        if !tone {
            failures.push(format!("t={t}s tone not detected: {det:?}"));
        }
        if m["srtp_auth_failures"].as_u64() != Some(0) {
            failures.push(format!(
                "t={t}s srtp_auth_failures {}",
                m["srtp_auth_failures"]
            ));
        }
    }
    let trace = c
        .call("call.sip_trace", json!({"call_id": call}))
        .await
        .map(|t| {
            // Each message starts with a "=====" trace header line; keep the request / status
            // line after it for session-timer traffic.
            t["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|m| {
                    m.contains("Session-Expires")
                        || m.contains(" UPDATE ")
                        || m.contains("CSeq: 2 INVITE")
                })
                .map(|m| {
                    m.lines()
                        .filter(|l| !l.starts_with("=====") && !l.is_empty())
                        .take(1)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    eprintln!("SOAK session-timer related SIP lines: {trace:?}");
    let _ = c.call("call.hangup", json!({"call_id": call})).await;
    app.shutdown().await;
    eprintln!("SOAK done: {checks} checks, {} failures", failures.len());
    assert!(failures.is_empty(), "soak failures: {failures:#?}");
}
