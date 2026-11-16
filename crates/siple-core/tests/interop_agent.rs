//! Agent audio API (call.play / call.listen / call.stream) through a FreeSWITCH/Asterisk test
//! PBX (see CONTRIBUTING.md, "Interop tests"). Gated: `SIPLE_INTEROP=1`. Uses user 1001 over UDP only.
//! `SIPLE_INTEROP_RTP_RANGE=START-END` keeps our RTP ports clear of FreeSWITCH's.
//! Optional STT check of the 5000 IVR prompt: `SIPLE_INTEROP_STT=1` plus
//! `SIPLE_INTEROP_STT_CMD='["whisper-cli","-m","...","-f","{in}","-nt","-np"]'`.
#![cfg(unix)]

use std::collections::HashMap;
use std::io::Cursor;
use std::process::Command;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use siple_core::agent::stream::{FRAME_AUDIO, encode_frame, read_client_frame};
use siple_core::agent::wav::encode_wav_i16;
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};
use siple_media::goertzel;
use tokio::io::AsyncWriteExt;

const USER: &str = "1001";

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

/// Hangs up every PBX channel if the test panics (best effort: needs ssh to the PBX host, which
/// is absent inside the build container; the app's own drop ends the calls then).
struct HangupOnPanic;

impl Drop for HangupOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=5",
                    pbx_env().get("PBX_SSH").map_or("", String::as_str),
                    "docker exec siple-fs fs_cli -x 'hupall NORMAL_CLEARING'",
                ])
                .output();
        }
    }
}

fn media_prefix() -> String {
    let Ok(r) = std::env::var("SIPLE_INTEROP_RTP_RANGE") else {
        return String::new();
    };
    let (a, b) = r
        .split_once('-')
        .expect("SIPLE_INTEROP_RTP_RANGE=START-END");
    format!("[media]\nrtp_port_range = [{}, {}]\n\n", a.trim(), b.trim())
}

fn stt_cmd() -> Option<String> {
    if std::env::var("SIPLE_INTEROP_STT").is_ok_and(|v| v == "1") {
        std::env::var("SIPLE_INTEROP_STT_CMD").ok()
    } else {
        None
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    app: App,
    c: ControlClient,
}

async fn control(app: &App, name: &str) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), name).await.unwrap();
    c
}

async fn rig() -> Rig {
    let env = pbx_env();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let speech = stt_cmd()
        .map(|c| format!("[speech]\nstt_command = {c}\n\n"))
        .unwrap_or_default();
    let cfg = format!(
        "{}{speech}[[account]]\nid = \"u{USER}\"\nusername = \"{USER}\"\npassword = \"{}\"\n\
         domain = \"{}\"\nregistrar = \"{}\"\ntransport = \"udp\"\nsrtp = \"off\"\n\
         expires = 120\nkeepalive_secs = 0\naudio_source = \"silence\"\naudio_sink = \"null\"\n",
        media_prefix(),
        env["PBX_PASSWORD"],
        env["PBX_DOMAIN"],
        env["PBX_HOST"]
    );
    std::fs::write(&path, cfg).unwrap();
    let mut o = AppOptions::new(&path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    let app = App::start(o).await.unwrap();
    let mut c = control(&app, "interop-agent").await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let l = c.call("account.list", Value::Null).await.unwrap();
        if l["accounts"].as_array().unwrap().iter().any(|a| {
            a["id"] == format!("u{USER}").as_str() && a["registration"]["state"] == "registered"
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "registration timed out: {l}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Rig { _dir: dir, app, c }
}

impl Rig {
    async fn dial(&mut self, target: &str) -> String {
        let d = self
            .c
            .call(
                "call.dial",
                json!({"account": format!("u{USER}"), "target": target, "sink": "null"}),
            )
            .await
            .unwrap();
        let id = d["call_id"].as_str().unwrap().to_string();
        let w = self
            .c
            .call(
                "call.wait_for",
                json!({"call_id": id, "state": "active", "timeout_ms": 20_000}),
            )
            .await;
        if let Err(e) = w {
            if let Ok(t) = self.c.call("call.sip_trace", json!({"call_id": id})).await {
                for m in t["messages"].as_array().into_iter().flatten() {
                    eprintln!("{}", m.as_str().unwrap_or(""));
                }
            }
            panic!("call to {target} did not become active: {e}");
        }
        id
    }

    async fn hangup(&mut self, id: &str) {
        let _ = self.c.call("call.hangup", json!({"call_id": id})).await;
        let _ = self
            .c
            .call(
                "call.wait_for",
                json!({"call_id": id, "state": "ended", "timeout_ms": 30_000}),
            )
            .await;
    }
}

fn sine(rate: u32, hz: f32, ms: u32, amp: f32) -> Vec<i16> {
    (0..u64::from(rate) * u64::from(ms) / 1000)
        .map(|n| {
            ((n as f32 * hz * std::f32::consts::TAU / rate as f32).sin() * amp * 32767.0) as i16
        })
        .collect()
}

fn decode_audio(res: &Value) -> (u32, Vec<i16>) {
    let a = &res["audio"];
    assert_eq!(a["format"], "wav", "{res}");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(a["base64"].as_str().expect("audio.base64"))
        .unwrap();
    let mut r = hound::WavReader::new(Cursor::new(bytes)).unwrap();
    let rate = r.spec().sample_rate;
    (rate, r.samples::<i16>().map(|s| s.unwrap()).collect())
}

/// 9196 echoes what we send: our played 1 kHz clip comes back and `listen` since the play's
/// start finds it.
#[tokio::test(flavor = "multi_thread")]
async fn echo_play_then_listen() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let _guard = HangupOnPanic;
    let env = pbx_env();
    let mut r = rig().await;
    let call = r.dial(&env["PBX_ECHO_EXT"]).await;

    let wav = encode_wav_i16(&sine(16_000, 1000.0, 1000, 0.5), 16_000);
    let play =
        r.c.call(
            "call.play",
            json!({"call_id": call,
                   "wav_base64": base64::engine::general_purpose::STANDARD.encode(wav),
                   "wait": true, "timeout_ms": 15_000}),
        )
        .await;
    let play = match play {
        Ok(p) => p,
        Err(e) => {
            r.hangup(&call).await;
            panic!("call.play failed: {e}");
        }
    };
    eprintln!("play: {play}");
    let start = play["rx_ms"].as_u64().expect("play.rx_ms");
    assert!(play["finished"]["rx_ms"].as_u64().unwrap() >= start);

    // since_rx_ms = the play's start: the echo (arriving ~RTT later) is inside the window.
    let l =
        r.c.call(
            "call.listen",
            json!({"call_id": call, "since_rx_ms": start, "max_ms": 3000,
                   "start_timeout_ms": 3000, "sample_rate": 16000}),
        )
        .await;
    r.hangup(&call).await;
    let l = l.expect("call.listen");
    eprintln!(
        "listen: reason={} speech={} level={}",
        l["reason"], l["speech"], l["level_dbfs"]
    );
    assert_eq!(l["speech"], true, "{}", l["reason"]);
    let (rate, samples) = decode_audio(&l);
    assert_eq!(rate, 16_000);
    let (ratio, _) = goertzel::detect(&samples, rate, 1000.0);
    eprintln!("1 kHz power ratio in echo: {ratio:.3}");
    assert!(
        ratio > 0.3,
        "no 1000 Hz in the echoed audio (ratio {ratio})"
    );
    r.app.shutdown().await;
}

/// 9999 sends a continuous tone: it counts as speech and listen ends at max_ms.
#[tokio::test(flavor = "multi_thread")]
async fn tone_stream_listen_max_duration() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let _guard = HangupOnPanic;
    let env = pbx_env();
    let mut r = rig().await;
    let call = r.dial(&env["PBX_TONE_EXT"]).await;
    let l =
        r.c.call(
            "call.listen",
            json!({"call_id": call, "max_ms": 2000, "audio": "none"}),
        )
        .await;
    r.hangup(&call).await;
    let l = l.expect("call.listen");
    eprintln!("listen: {l}");
    assert_eq!(l["speech"], true);
    assert_eq!(l["reason"], "max_duration");
    r.app.shutdown().await;
}

/// 5000 is the demo IVR: its prompt is detected as speech; transcribed when an STT is given.
#[tokio::test(flavor = "multi_thread")]
async fn ivr_prompt_detected() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let _guard = HangupOnPanic;
    let mut r = rig().await;
    let stt = stt_cmd().is_some();
    let call = r.dial("5000").await;
    let mut p = json!({"call_id": call, "start_timeout_ms": 10_000, "max_ms": 15_000});
    if stt {
        p["transcribe"] = true.into();
    } else {
        p["audio"] = "none".into();
    }
    let l = r.c.call("call.listen", p).await;
    r.hangup(&call).await;
    let l = l.expect("call.listen");
    eprintln!("ivr listen: {l}");
    assert_eq!(l["speech"], true, "no IVR prompt heard: {l}");
    if stt {
        let t = l["transcript"].as_str().unwrap_or("");
        eprintln!("transcript: {t:?}");
        assert!(!t.trim().is_empty(), "empty transcript: {l}");
    } else {
        eprintln!("STT part skipped (set SIPLE_INTEROP_STT=1 and SIPLE_INTEROP_STT_CMD)");
    }
    r.app.shutdown().await;
}

/// call.stream on the echo extension: tone frames out, the same tone back on RX frames.
#[tokio::test(flavor = "multi_thread")]
async fn stream_echo_roundtrip() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let _guard = HangupOnPanic;
    let env = pbx_env();
    let mut r = rig().await;
    let call = r.dial(&env["PBX_ECHO_EXT"]).await;

    let sc = control(&r.app, "interop-stream").await;
    let st = sc
        .into_stream(json!({"call_id": call, "sample_rate": 16000}))
        .await;
    let mut st = match st {
        Ok(s) => s,
        Err(e) => {
            r.hangup(&call).await;
            panic!("call.stream failed: {e}");
        }
    };
    eprintln!("stream info: {}", st.info);
    let frame_bytes = st.info["frame_bytes"].as_u64().unwrap() as usize;
    let pcm: Vec<u8> = sine(16_000, 1000.0, 1000, 0.5)
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    let mut wr = st.wr;
    let pump = tokio::spawn(async move {
        for f in pcm.chunks(frame_bytes) {
            wr.write_all(&encode_frame(FRAME_AUDIO, f)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wr
    });
    let mut rx: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, read_client_frame(&mut st.rd)).await {
            Ok(Ok(Some((FRAME_AUDIO, b)))) => rx.extend_from_slice(&b),
            Ok(Ok(Some(_))) => {}
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    let mut wr = pump.await.unwrap();
    let _ = wr.shutdown().await;
    drop(wr);
    drop(st.rd);
    r.hangup(&call).await;
    let samples: Vec<i16> = rx
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    // Best 200 ms window: the tone is only a part of the 3 s capture.
    let ratio = samples
        .as_chunks::<3200>()
        .0
        .iter()
        .map(|w| goertzel::detect(w, 16_000, 1000.0).0)
        .fold(0.0f32, f32::max);
    eprintln!(
        "rx {} samples, best 200 ms window 1 kHz ratio {ratio:.3}",
        samples.len()
    );
    assert!(
        samples.len() > 16_000,
        "too little RX audio: {}",
        samples.len()
    );
    assert!(
        ratio > 0.6,
        "echoed tone not seen on the RX stream (ratio {ratio})"
    );
    r.app.shutdown().await;
}
