//! `network.refresh` and `account.reregister` through the control API trigger a REGISTER;
//! `network.changed` / `account.reconnecting` events and the `[network]` hot reload.
#![cfg(unix)]

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

/// UDP registrar that answers every REGISTER with a 200 and counts them. A retransmission (the
/// same Call-ID and CSeq: the client did not see the 200 within T1, as happens on a loaded
/// machine) is answered again but is not counted as another REGISTER.
struct Registrar {
    addr: SocketAddr,
    count: Arc<AtomicUsize>,
    /// `Call-ID | CSeq` of every REGISTER received, retransmissions included.
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
}

impl Registrar {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
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
        seen: Arc::default(),
        stop: Arc::default(),
    };
    let (count, seen, stop) = (r.count.clone(), r.seen.clone(), r.stop.clone());
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
            let key = req
                .lines()
                .filter(|l| {
                    let low = l.to_ascii_lowercase();
                    low.starts_with("call-id:") || low.starts_with("cseq:")
                })
                .collect::<Vec<_>>()
                .join(" | ");
            {
                let mut seen = seen.lock().unwrap();
                if !seen.contains(&key) {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                seen.push(key);
            }
            let mut lines = vec!["SIP/2.0 200 OK".to_string()];
            for l in req.lines() {
                let low = l.to_ascii_lowercase();
                if low.starts_with("via:") {
                    let base: String = l
                        .split(';')
                        .filter(|p| !p.trim_start().starts_with("rport"))
                        .collect::<Vec<_>>()
                        .join(";");
                    lines.push(format!("{base};rport={}", from.port()));
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
            let resp = format!("{}\r\n\r\n", lines.join("\r\n"));
            let _ = sock.send_to(resp.as_bytes(), from);
        }
    });
    r
}

fn config(reg: &Registrar, network: &str) -> String {
    format!(
        "[network]\n{network}\n\
         [[account]]\nid = \"a1\"\nusername = \"a1\"\npassword = \"pw\"\ndomain = \"127.0.0.1\"\n\
         registrar = \"{}\"\ntransport = \"udp\"\nexpires = 300\nkeepalive_secs = 0\n",
        reg.addr
    )
}

async fn start(dir: &tempfile::TempDir, text: &str) -> (App, PathBuf) {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let mut o = AppOptions::new(path.clone());
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    (App::start(o).await.unwrap(), path)
}

async fn client(app: &App) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "test").await.unwrap();
    c
}

async fn wait_count(reg: &Registrar, n: usize) {
    for _ in 0..150 {
        if reg.count.load(Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "expected {n} REGISTERs within 3 s, saw {}: {:?}",
        reg.count.load(Ordering::SeqCst),
        reg.seen()
    );
}

async fn wait_registered(c: &mut ControlClient) {
    for _ in 0..150 {
        let s = c.call("status", Value::Null).await.unwrap();
        if s["accounts"][0]["registration"]["state"] == "registered" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("not registered");
}

#[tokio::test(flavor = "multi_thread")]
async fn network_refresh_and_account_reregister_send_a_register() {
    let reg = registrar();
    let dir = tempfile::tempdir().unwrap();
    // Watching off so real OS events cannot interfere with the test.
    let (app, _) = start(&dir, &config(&reg, "watch = false\nprobe_secs = 0")).await;
    let mut c = client(&app).await;
    wait_registered(&mut c).await;
    assert_eq!(reg.count.load(Ordering::SeqCst), 1, "{:?}", reg.seen());
    let st = c.call("status", Value::Null).await.unwrap();
    assert_eq!(st["network"]["generation"], 0);

    let r = c
        .call("account.reregister", json!({"account":"a1"}))
        .await
        .unwrap();
    assert_eq!(r["ok"], true);
    wait_count(&reg, 2).await;

    let r = c.call("network.refresh", Value::Null).await.unwrap();
    assert_eq!(r["generation"], 1);
    wait_count(&reg, 3).await;
    let r = c.call("network.refresh", Value::Null).await.unwrap();
    assert_eq!(r["generation"], 2);
    wait_count(&reg, 4).await;

    // Both are in the event log, with the documented shape.
    let ev = c
        .call(
            "events.poll",
            json!({"since": 0, "timeout_ms": 1000, "kinds": ["network.*", "account.reconnecting"]}),
        )
        .await
        .unwrap();
    let evs = ev["events"].as_array().unwrap();
    let changed: Vec<&Value> = evs
        .iter()
        .filter(|e| e["kind"] == "network.changed")
        .collect();
    assert_eq!(changed.len(), 2, "{evs:?}");
    assert_eq!(changed[0]["data"]["generation"], 1);
    assert_eq!(changed[0]["data"]["reasons"], json!(["manual"]));
    assert!(changed[0]["data"].get("old_local_ip").is_some());
    let reconnecting: Vec<&Value> = evs
        .iter()
        .filter(|e| e["kind"] == "account.reconnecting")
        .collect();
    assert!(reconnecting.len() >= 3, "{evs:?}");
    assert!(reconnecting.iter().all(|e| e["account"] == "a1"));
    assert_eq!(reconnecting[0]["data"]["reason"], "manual");

    // Errors and discoverability.
    let e = c
        .call("account.reregister", json!({"account":"nope"}))
        .await
        .unwrap_err();
    assert!(format!("{e:?}").contains("NotFound"), "{e:?}");
    let m = c.call("methods.list", Value::Null).await.unwrap();
    let names: Vec<&str> = m["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"network.refresh") && names.contains(&"account.reregister"));
    drop(c);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn network_section_reloads_hot_without_re_registering() {
    let reg = registrar();
    let dir = tempfile::tempdir().unwrap();
    let (app, path) = start(&dir, &config(&reg, "watch = false\nprobe_secs = 0")).await;
    let mut c = client(&app).await;
    wait_registered(&mut c).await;

    std::fs::write(
        &path,
        config(&reg, "watch = false\nprobe_secs = 60\ndebounce_ms = 500"),
    )
    .unwrap();
    let r = c.call("config.reload", Value::Null).await.unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let summary = r["summary"].to_string();
    assert!(summary.contains("network settings updated"), "{summary}");
    assert!(!summary.contains("re-registering"), "{summary}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        reg.count.load(Ordering::SeqCst),
        1,
        "no re-register for a [network] change: {:?}",
        reg.seen()
    );

    // An invalid range is rejected and the previous settings stay.
    std::fs::write(&path, config(&reg, "debounce_ms = 5")).unwrap();
    let e = c.call("config.reload", Value::Null).await.unwrap_err();
    assert!(format!("{e:?}").contains("debounce_ms"), "{e:?}");
    drop(c);
    app.shutdown().await;
}
