//! Live calls over a network change, through the real media engine and the control
//! API: alice (registered, fake network probe / monitor) calls bob (peer account, same app); the
//! injected local-address change moves the call with a re-INVITE and the audio (tones) keeps
//! flowing both ways; with `move_calls = false` the call ends with `network_lost`.
#![cfg(unix)]

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use siple_config::Transport;
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};
use siple_sip::net::{InterfaceInfo, NetMonitor, NetProbe, NetReason, NetSetup};
use tokio::sync::mpsc::UnboundedSender;

// --- fakes ---------------------------------------------------------------------------------------

struct FakeProbe {
    local: Mutex<IpAddr>,
    hosts: Mutex<HashMap<String, SocketAddr>>,
    ifaces: Mutex<Vec<InterfaceInfo>>,
}

impl FakeProbe {
    fn new(reg: SocketAddr) -> Arc<Self> {
        Arc::new(Self {
            local: Mutex::new("127.0.0.1".parse().unwrap()),
            hosts: Mutex::new(HashMap::from([("reg.test".to_string(), reg)])),
            ifaces: Mutex::new(vec![InterfaceInfo {
                name: "en0".into(),
                up: true,
                addrs: vec!["10.0.0.1".parse().unwrap()],
                gateways: vec![],
            }]),
        })
    }
    /// The route now leaves through `ip`, and a relevant address appears (what a real change
    /// looks like to the fingerprint).
    fn move_to(&self, ip: IpAddr) {
        *self.local.lock().unwrap() = ip;
        let mut l = self.ifaces.lock().unwrap();
        let n = l[0].addrs.len();
        l[0].addrs.push(format!("10.0.1.{n}").parse().unwrap());
    }
}

#[async_trait::async_trait]
impl NetProbe for FakeProbe {
    fn local_ip_toward(&self, _: SocketAddr) -> io::Result<IpAddr> {
        Ok(*self.local.lock().unwrap())
    }
    fn interfaces(&self) -> io::Result<Vec<InterfaceInfo>> {
        Ok(self.ifaces.lock().unwrap().clone())
    }
    async fn resolve(
        &self,
        host: &str,
        port: Option<u16>,
        _: Transport,
    ) -> siple_sip::Result<Vec<SocketAddr>> {
        if let Some(a) = self.hosts.lock().unwrap().get(host) {
            return Ok(vec![*a]);
        }
        host.parse::<IpAddr>()
            .map(|ip| vec![SocketAddr::new(ip, port.unwrap_or(5060))])
            .map_err(|_| siple_sip::SipError::Dns(format!("{host}: nxdomain")))
    }
}

#[derive(Default)]
struct FakeMonitor {
    sink: Mutex<Option<UnboundedSender<NetReason>>>,
}

impl FakeMonitor {
    fn fire(&self, what: &str) {
        self.sink
            .lock()
            .unwrap()
            .as_ref()
            .expect("monitor started")
            .send(NetReason::Os(what.into()))
            .unwrap();
    }
}

impl NetMonitor for FakeMonitor {
    fn start(&self, sink: UnboundedSender<NetReason>) -> io::Result<Box<dyn Send>> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(Box::new(()))
    }
}

/// UDP registrar answering every REGISTER with a 200 (echoing the Contact).
struct Registrar {
    addr: SocketAddr,
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for Registrar {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn registrar() -> Registrar {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let r = Registrar {
        addr: sock.local_addr().unwrap(),
        count: Arc::default(),
        stop: Arc::default(),
    };
    let (count, stop) = (r.count.clone(), r.stop.clone());
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while !stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            if !req.starts_with("REGISTER ") {
                continue;
            }
            count.fetch_add(1, Ordering::SeqCst);
            let mut lines = vec!["SIP/2.0 200 OK".to_string()];
            for l in req.lines() {
                let low = l.to_ascii_lowercase();
                if low.starts_with("via:") {
                    let base: String = l
                        .split(';')
                        .filter(|p| {
                            !p.trim_start().starts_with("rport")
                                && !p.trim_start().starts_with("received")
                        })
                        .collect::<Vec<_>>()
                        .join(";");
                    lines.push(format!(
                        "{base};received={};rport={}",
                        from.ip(),
                        from.port()
                    ));
                } else if low.starts_with("to:") {
                    lines.push(if l.contains("tag=") {
                        l.to_string()
                    } else {
                        format!("{l};tag=fakereg")
                    });
                } else if ["from:", "call-id:", "cseq:"]
                    .iter()
                    .any(|p| low.starts_with(p))
                {
                    lines.push(l.to_string());
                } else if low.starts_with("contact:") {
                    let c = l.split_once(':').unwrap().1.trim();
                    lines.push(format!("Contact: {c};expires=300"));
                }
            }
            lines.push("Content-Length: 0".into());
            let _ = sock.send_to(format!("{}\r\n\r\n", lines.join("\r\n")).as_bytes(), from);
        }
    });
    r
}

/// A second local IPv4 address a socket can bind to (see `siple-sip/tests/network.rs`).
fn alt_local_ip() -> Option<IpAddr> {
    let can_bind = |ip: IpAddr| UdpSocket::bind(SocketAddr::new(ip, 0)).is_ok();
    let lo2: IpAddr = "127.0.0.2".parse().unwrap();
    if can_bind(lo2) {
        return Some(lo2);
    }
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified() && can_bind(ip)).then_some(ip)
}

// --- harness -------------------------------------------------------------------------------------

struct Rig {
    _dir: tempfile::TempDir,
    app: App,
    a: ControlClient,
    b: ControlClient,
    probe: Arc<FakeProbe>,
    mon: Arc<FakeMonitor>,
    reg: Registrar,
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

async fn rig(move_calls: bool) -> Rig {
    let reg = registrar();
    let probe = FakeProbe::new(reg.addr);
    let mon = Arc::new(FakeMonitor::default());
    let pb = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = tempfile::tempdir().unwrap();
    let text = format!(
        "[network]\nwatch = true\nprobe_secs = 0\ndebounce_ms = 100\nmove_calls = {move_calls}\n\
         [[account]]\nid = \"alice\"\nusername = \"alice\"\npassword = \"pw\"\ndomain = \"reg.test\"\n\
         registrar = \"reg.test:5060\"\ntransport = \"udp\"\nexpires = 300\nkeepalive_secs = 0\n\
         audio_source = \"silence\"\naudio_sink = \"null\"\n\
         [[account]]\nid = \"bob\"\nusername = \"bob\"\npassword = \"pw\"\ndomain = \"127.0.0.1\"\n\
         transport = \"udp\"\nregister = false\nbind_address = \"127.0.0.1\"\nlocal_port = {pb}\n\
         audio_source = \"silence\"\naudio_sink = \"null\"\n"
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let mut o = AppOptions::new(&path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    o.net = Some(NetSetup {
        config: None,
        monitor: Some(mon.clone()),
        probe: Some(probe.clone()),
        clock: None,
    });
    let app = App::start(o).await.unwrap();
    let a = client(&app, "alice-ctl").await;
    let b = client(&app, "bob-ctl").await;
    Rig {
        _dir: dir,
        app,
        a,
        b,
        probe,
        mon,
        reg,
        bob_uri: format!("sip:bob@127.0.0.1:{pb}"),
    }
}

async fn wait_registered(c: &mut ControlClient) {
    for _ in 0..200 {
        let s = c.call("status", Value::Null).await.unwrap();
        if s["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "alice" && a["registration"]["state"] == "registered")
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("alice did not register");
}

async fn wait_state(c: &mut ControlClient, call_id: &str, state: &str) -> Value {
    c.call(
        "call.wait_for",
        json!({"call_id": call_id, "state": state, "timeout_ms": 10_000}),
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

/// alice dials bob (1000 Hz), bob answers (1500 Hz); both active. `(alice call, bob call)`.
async fn establish(r: &mut Rig) -> (String, String) {
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
            json!({"account":"bob","state":"ringing","since":d["event_seq"],"timeout_ms":10_000}),
        )
        .await
        .unwrap();
    let b_call = ringing["call"]["id"].as_str().unwrap().to_string();
    r.b.call(
        "call.answer",
        json!({"call_id": b_call, "source":"tone:1500", "sink":"null"}),
    )
    .await
    .unwrap();
    wait_state(&mut r.a, &a_call, "active").await;
    wait_state(&mut r.b, &b_call, "active").await;
    (a_call, b_call)
}

async fn current_seq(c: &mut ControlClient) -> u64 {
    c.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap()
}

async fn poll(c: &mut ControlClient, since: u64, kinds: &[&str], call_id: &str) -> Value {
    c.call(
        "events.poll",
        json!({"since": since, "kinds": kinds, "call_id": call_id, "timeout_ms": 10_000}),
    )
    .await
    .unwrap()
}

// --- tests ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_moved_call_keeps_its_audio_both_ways_and_reports_reconnecting_reconnected() {
    let Some(alt) = alt_local_ip() else {
        eprintln!("skipped: the host has no second local IPv4 address");
        return;
    };
    let mut r = rig(true).await;
    wait_registered(&mut r.a).await;
    let (a_call, b_call) = establish(&mut r).await;
    // Audio before the change.
    assert_eq!(detect(&mut r.a, &a_call, 1500.0).await["detected"], true);
    assert_eq!(detect(&mut r.b, &b_call, 1000.0).await["detected"], true);
    let before =
        r.b.call("call.get", json!({"call_id": b_call}))
            .await
            .unwrap();
    let remote_before = before["negotiated"]["remote_rtp"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(remote_before.starts_with("127.0.0.1:"), "{remote_before}");

    let seq = current_seq(&mut r.a).await;
    r.probe.move_to(alt);
    r.mon.fire("addr");

    let ev = poll(&mut r.a, seq, &["call.reconnecting"], &a_call).await;
    let e = &ev["events"][0];
    assert_eq!(e["kind"], "call.reconnecting", "{ev}");
    assert_eq!(e["account"], "alice");
    assert_eq!(e["data"]["call_id"], a_call.as_str());
    assert_eq!(e["data"]["account"], "alice");
    let ev = poll(&mut r.a, seq, &["call.reconnected"], &a_call).await;
    assert_eq!(ev["events"][0]["kind"], "call.reconnected", "{ev}");
    assert_eq!(ev["events"][0]["data"]["call_id"], a_call.as_str());

    // The call is up, no longer "reconnecting", and bob now sends to alice's new address.
    let a =
        r.a.call("call.get", json!({"call_id": a_call}))
            .await
            .unwrap();
    assert_eq!(a["state"], "active", "{a}");
    assert_eq!(a["reconnecting"], false);
    let after =
        r.b.call("call.get", json!({"call_id": b_call}))
            .await
            .unwrap();
    assert_eq!(after["state"], "active");
    let remote_after = after["negotiated"]["remote_rtp"].as_str().unwrap();
    assert!(
        remote_after.starts_with(&format!("{alt}:")),
        "bob sends RTP to the new address: {remote_after} (was {remote_before})"
    );
    // Audio both ways through the new sockets.
    let (da, db) = (
        detect(&mut r.a, &a_call, 1500.0).await,
        detect(&mut r.b, &b_call, 1000.0).await,
    );
    assert_eq!(da["detected"], true, "alice lost bob's tone: {da}");
    assert_eq!(db["detected"], true, "bob lost alice's tone: {db}");
    assert!(r.reg.count.load(Ordering::SeqCst) >= 2, "re-registered");

    r.a.call("call.hangup", json!({"call_id": a_call}))
        .await
        .unwrap();
    wait_state(&mut r.b, &b_call, "ended").await;
    r.app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn move_calls_false_ends_the_call_with_network_lost_through_the_api() {
    let Some(alt) = alt_local_ip() else {
        eprintln!("skipped: the host has no second local IPv4 address");
        return;
    };
    let mut r = rig(false).await;
    wait_registered(&mut r.a).await;
    let (a_call, _b_call) = establish(&mut r).await;
    r.probe.move_to(alt);
    r.mon.fire("addr");
    let ended = wait_state(&mut r.a, &a_call, "ended").await;
    assert_eq!(ended["call"]["end"]["cause"], "network_lost", "{ended}");
    r.app.shutdown().await;
}
