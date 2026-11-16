#![allow(dead_code)]
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

/// Ports already handed out by this process. The OS may give a just-released ephemeral port to
/// the next `bind(0)` (a parallel test's, or another account of the same rig) before the first
/// owner has bound it; two apps then fight over one port and a call lands on the wrong peer.
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
pub fn free_udp_port() -> u16 {
    unique_port(|| {
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    })
}

/// A free TCP port that this process has not handed out before.
pub fn free_tcp_port() -> u16 {
    unique_port(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    })
}

pub fn acct(id: &str, port: u16, extra: &str) -> String {
    format!(
        "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\npassword = \"pw\"\n\
         domain = \"127.0.0.1\"\ntransport = \"udp\"\nregister = false\n\
         bind_address = \"127.0.0.1\"\nlocal_port = {port}\n\
         audio_source = \"silence\"\naudio_sink = \"null\"\n{extra}\n"
    )
}

pub fn write_atomic(path: &std::path::Path, text: &str) {
    let tmp = path.with_extension("swp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

pub async fn start_app(path: &std::path::Path) -> App {
    let mut o = AppOptions::new(path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    App::start(o).await.unwrap()
}

pub async fn client(app: &App, name: &str) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), name).await.unwrap();
    c
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub app: App,
    pub a: ControlClient,
    pub b: ControlClient,
    pub pa: u16,
    pub pb: u16,
    pub bob_uri: String,
}

impl Rig {
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }
    pub fn accounts_text(&self, extra: &str) -> String {
        format!(
            "{}{}",
            acct("alice", self.pa, extra),
            acct("bob", self.pb, extra)
        )
    }
}

pub async fn rig(extra: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let text = format!("{}{}", acct("alice", pa, extra), acct("bob", pb, extra));
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

pub async fn establish(r: &mut Rig) -> (String, String) {
    // B's own log position before dialing (never A's `event_seq` from another client/instance).
    let b_seq = seq(&mut r.b).await;
    let d =
        r.a.call(
            "call.dial",
            json!({"account":"alice","target":r.bob_uri,"source":"tone:1000","sink":"null"}),
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
    (a_call, b_call)
}

pub async fn seq(c: &mut ControlClient) -> u64 {
    c.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap()
}

pub const _D: Duration = Duration::from_millis(1);
