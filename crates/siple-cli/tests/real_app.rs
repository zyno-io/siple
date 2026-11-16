//! `siple-cli stream` and `siple-cli mcp` (the built binary) against a real app: two
//! peer accounts in one instance call each other over loopback (alice sends a 1000 Hz tone,
//! bob's source is silence); the CLI attaches to bob's call.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

struct Rig {
    _dir: tempfile::TempDir,
    config: PathBuf,
    app: App,
    _a: ControlClient,
    a_call: String,
    b_call: String,
}

fn acct(id: &str, port: u16) -> String {
    format!(
        "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\npassword = \"pw\"\n\
         domain = \"127.0.0.1\"\ntransport = \"udp\"\nregister = false\n\
         bind_address = \"127.0.0.1\"\nlocal_port = {port}\n\
         audio_source = \"silence\"\naudio_sink = \"null\"\n"
    )
}

async fn connect(app: &App, name: &str) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), name).await.unwrap();
    c
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let config = dir.path().join("config.toml");
    std::fs::write(&config, format!("{}{}", acct("alice", pa), acct("bob", pb))).unwrap();
    let mut o = AppOptions::new(&config);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    let app = App::start(o).await.unwrap();
    let mut a = connect(&app, "a").await;
    let mut b = connect(&app, "b").await;
    let b_seq = b.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();
    let d = a
        .call(
            "call.dial",
            json!({"account":"alice","target":format!("sip:bob@127.0.0.1:{pb}"),
                   "source":"tone:1000","sink":"null"}),
        )
        .await
        .unwrap();
    let a_call = d["call_id"].as_str().unwrap().to_string();
    let ringing = b
        .call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":b_seq,"timeout_ms":10_000}),
        )
        .await
        .unwrap();
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    b.call("call.answer", json!({"call_id": b_call}))
        .await
        .unwrap();
    a.call(
        "call.wait_for",
        json!({"call_id": a_call, "state":"active","timeout_ms":10_000}),
    )
    .await
    .unwrap();
    Rig {
        _dir: dir,
        config,
        app,
        _a: a,
        a_call,
        b_call,
    }
}

fn tone_pcm(hz: f32, ms: usize) -> Vec<u8> {
    (0..16 * ms)
        .flat_map(|i| {
            let v = (std::f32::consts::TAU * hz * i as f32 / 16_000.0).sin() * 0.5;
            ((v * 32767.0) as i16).to_le_bytes()
        })
        .collect()
}

/// `siple-cli stream --tx <file> --no-rx` sends the file, then exits by itself after the
/// `__tx_done` mark; the far side hears the tone.
#[tokio::test(flavor = "multi_thread")]
async fn cli_stream_tx_file_no_rx_exits_after_tx_done() {
    let r = rig().await;
    let pcm = r._dir.path().join("tone.raw");
    std::fs::write(&pcm, tone_pcm(700.0, 2500)).unwrap();
    // Detection must overlap the tone: run it on its own connection, started before the child.
    let mut det_client = connect(&r.app, "det").await;
    let det_call = r.a_call.clone();
    let det = tokio::spawn(async move {
        det_client
            .call(
                "call.detect_tone",
                json!({"call_id": det_call, "freq_hz": 700, "timeout_ms": 15_000}),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let child = Command::new(env!("CARGO_BIN_EXE_siple-cli"))
        .args(["stream", &r.b_call, "--tx"])
        .arg(&pcm)
        .arg("--no-rx")
        .arg("--config")
        .arg(&r.config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let waiter = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let out = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("siple-cli stream did not exit after __tx_done")
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{:?}: {err}", out.status);
    assert!(err.contains("__tx_done"), "{err}");
    assert!(out.stdout.is_empty(), "--no-rx writes nothing to stdout");
    let d = det.await.unwrap().unwrap();
    assert_eq!(d["detected"], true, "{d}");
    r.app.shutdown().await;
}

/// The MCP bridge against a real instance: `call_rx_audio` returns an `audio` content block
/// and no base64 in the text or the structured content.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_rx_audio_returns_an_audio_block_without_base64_text() {
    let r = rig().await;
    // Let some RX audio accumulate.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let config = r.config.clone();
    let call = r.b_call.clone();
    let (resp, init, tools) = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_siple-cli"))
            .args(["mcp", "--config"])
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut send = |v: Value| {
            writeln!(stdin, "{v}").unwrap();
            stdin.flush().unwrap();
        };
        let mut read_id = |id: u64| -> Value {
            loop {
                let l = lines.next().expect("mcp closed stdout").unwrap();
                let v: Value = serde_json::from_str(&l).unwrap();
                if v["id"] == id {
                    return v;
                }
            }
        };
        send(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},
            "clientInfo":{"name":"test","version":"0"}}}),
        );
        let init = read_id(1);
        send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
        let tools = read_id(2);
        send(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"call_rx_audio","arguments":{"call_id":call,"last_ms":1000}}}),
        );
        let resp = read_id(3);
        drop(stdin);
        let _ = child.wait();
        (resp, init, tools)
    })
    .await
    .unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "call_rx_audio")
    );
    let res = &resp["result"];
    assert_eq!(res["isError"], false, "{resp}");
    let content = res["content"].as_array().unwrap();
    let audio = content
        .iter()
        .find(|c| c["type"] == "audio")
        .unwrap_or_else(|| panic!("no audio content block: {resp}"));
    assert_eq!(audio["mimeType"], "audio/wav");
    let data = audio["data"].as_str().unwrap();
    assert!(
        data.len() > 10_000,
        "1 s of 16 kHz audio, base64: {}",
        data.len()
    );
    // The model-visible parts never carry the base64.
    for c in content.iter().filter(|c| c["type"] == "text") {
        let t = c["text"].as_str().unwrap();
        assert!(t.len() < 2_000 && !t.contains(&data[..64]), "{t}");
    }
    let structured = res["structuredContent"].to_string();
    assert!(!structured.contains(&data[..64]), "{structured}");
    assert_eq!(res["structuredContent"]["audio"]["base64_omitted"], true);
    assert!(res["structuredContent"]["audio"].get("base64").is_none());
    r.app.shutdown().await;
}

/// Ports already handed out by this process. The OS may give a just-released ephemeral port to
/// the next `bind(0)` probe (another test's, or another account of the same rig) before the first
/// owner has bound it; two stacks then fight over one port.
static USED_PORTS: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

fn unique_port(probe: impl Fn() -> u16) -> u16 {
    loop {
        let port = probe();
        let mut used = USED_PORTS.lock().unwrap();
        if !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

/// A free UDP port that this process has not handed out before.
fn free_udp_port() -> u16 {
    unique_port(|| {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    })
}
