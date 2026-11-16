//! Subscription methods and events of the control API (BLF / MWI / generic), against a tiny
//! scripted UDP notifier. No PBX needed.
#![cfg(unix)]

use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};
use tokio::net::UdpSocket;

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new(text: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), text).unwrap();
        Env { dir }
    }
    async fn start(&self) -> App {
        let mut o = AppOptions::new(self.dir.path().join("config.toml"));
        o.logging = LoggingMode::Detached;
        o.stderr_log = false;
        o.enable_audio = false;
        o.watch_config = false;
        App::start(o).await.unwrap()
    }
}

async fn client(app: &App) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "test").await.unwrap();
    c
}

fn account(domain: &str, extra: &str) -> String {
    format!(
        "[[account]]\nid = \"a\"\nusername = \"1017\"\npassword = \"pw\"\ndomain = \"{domain}\"\n\
         transport = \"udp\"\nregister = false\nbind_address = \"127.0.0.1\"\n{extra}\n"
    )
}

fn header(raw: &str, name: &str) -> String {
    raw.lines()
        .find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

/// Waits for one SUBSCRIBE, answers 202, then sends a NOTIFY with `body`. Returns the datagram
/// source and the dialog pieces for further NOTIFYs.
async fn serve_subscribe(sock: &UdpSocket, event: &str, ct: &str, body: &str) -> String {
    let mut buf = vec![0u8; 8192];
    let (call_id, src) = loop {
        let (n, src) = tokio::time::timeout(Duration::from_secs(10), sock.recv_from(&mut buf))
            .await
            .expect("no SUBSCRIBE")
            .unwrap();
        let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
        if !raw.starts_with("SUBSCRIBE ") || !header(&raw, "event").starts_with(event) {
            continue;
        }
        let via = raw
            .lines()
            .filter(|l| l.to_ascii_lowercase().starts_with("via:"))
            .collect::<Vec<_>>()
            .join("\r\n");
        let resp = format!(
            "SIP/2.0 202 Accepted\r\n{via}\r\nFrom: {}\r\nTo: {};tag=nt1\r\nCall-ID: {}\r\nCSeq: {}\r\nExpires: 60\r\nContact: <sip:n@{}>\r\nContent-Length: 0\r\n\r\n",
            header(&raw, "from"),
            header(&raw, "to"),
            header(&raw, "call-id"),
            header(&raw, "cseq"),
            sock.local_addr().unwrap()
        );
        sock.send_to(resp.as_bytes(), src).await.unwrap();
        let contact = header(&raw, "contact");
        let target = contact
            .trim_start_matches('<')
            .split('>')
            .next()
            .unwrap()
            .to_string();
        let notify = format!(
            "NOTIFY {target} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bKcoretest{}\r\nFrom: {};tag=nt1\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: 1 NOTIFY\r\nContact: <sip:n@{}>\r\nEvent: {event}\r\nSubscription-State: active;expires=60\r\nContent-Type: {ct}\r\nContent-Length: {}\r\n\r\n{body}",
            sock.local_addr().unwrap(),
            event.len(),
            header(&raw, "to"),
            header(&raw, "from"),
            header(&raw, "call-id"),
            sock.local_addr().unwrap(),
            body.len()
        );
        sock.send_to(notify.as_bytes(), src).await.unwrap();
        break (header(&raw, "call-id"), src);
    };
    let _ = src;
    call_id
}

async fn poll(c: &mut ControlClient, since: u64, kind: &str) -> Value {
    let r = c
        .call(
            "events.poll",
            json!({"since": since, "kinds": [kind], "timeout_ms": 10000}),
        )
        .await
        .unwrap();
    let events = r["events"].as_array().unwrap();
    assert!(!events.is_empty(), "no {kind} event: {r}");
    events[0].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn blf_mwi_and_generic_methods_and_events() {
    let notifier = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let naddr: SocketAddr = notifier.local_addr().unwrap();
    let env = Env::new(&account(
        &naddr.to_string(),
        "mwi = \"unsolicited\"\n[[account.blf]]\nuri = \"1018\"\nlabel = \"Bob\"\n",
    ));
    let app = env.start().await;
    let mut c = client(&app).await;
    let seq0 = c.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();

    // Catalog.
    let m = c.call("methods.list", Value::Null).await.unwrap();
    let names: Vec<&str> = m["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    for n in [
        "blf.list",
        "blf.dial",
        "call.pickup",
        "mwi.get",
        "subscribe",
        "unsubscribe",
        "subscriptions.list",
    ] {
        assert!(names.contains(&n), "{n} missing");
        assert!(
            siple_core::backend::method_catalog()
                .iter()
                .any(|m| m.name == n)
        );
    }

    // BLF: the configured entry subscribes; a NOTIFY makes it busy and emits blf.state.
    serve_subscribe(
        &notifier,
        "dialog",
        "application/dialog-info+xml",
        "<dialog-info xmlns=\"urn:ietf:params:xml:ns:dialog-info\" version=\"1\" state=\"full\" entity=\"sip:1018@x\"><dialog id=\"d\" direction=\"initiator\"><state>confirmed</state></dialog></dialog-info>",
    )
    .await;
    let e = poll(&mut c, seq0, "blf.state").await;
    assert_eq!(e["account"], "a");
    assert_eq!(e["data"]["uri"], "1018");
    assert_eq!(e["data"]["label"], "Bob");
    assert_eq!(e["data"]["state"], "busy");
    assert_eq!(e["data"]["dialogs"][0]["state"], "confirmed");
    let l = c.call("blf.list", json!({"account": "a"})).await.unwrap();
    assert_eq!(l["blf"][0]["state"], "busy");
    assert_eq!(l["blf"][0]["label"], "Bob");
    // Health fields (additive): healthy, active, no pickup configured.
    assert_eq!(l["blf"][0]["subscription"]["phase"], "active");
    assert_eq!(l["blf"][0]["stale"], false);
    assert_eq!(l["blf"][0]["pickup"], "off");
    assert_eq!(l["blf"][0]["group"], "");
    // call.pickup / blf.dial: errors map to the API kinds.
    for (m, p, kind) in [
        (
            "call.pickup",
            json!({"account":"a","uri":"1018"}),
            "invalid_state",
        ),
        ("call.pickup", json!({"account":"a","uri":"9"}), "not_found"),
        ("blf.dial", json!({"account":"a","uri":"9"}), "not_found"),
        (
            "blf.dial",
            json!({"account":"zz","uri":"1018"}),
            "not_found",
        ),
        ("call.pickup", json!({"account":"a"}), "invalid_params"),
    ] {
        let e = c.call(m, p).await.unwrap_err();
        assert!(e.to_string().contains(kind), "{m}: {e}");
    }
    let e = c
        .call("blf.list", json!({"account": "zz"}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(s["accounts"][0]["blf"][0]["state"], "busy");
    assert!(s["accounts"][0]["mwi"].is_null());

    // subscriptions.list shows it (kind blf, active).
    let subs = c.call("subscriptions.list", json!({})).await.unwrap();
    assert_eq!(subs["subscriptions"][0]["kind"], "blf");
    assert_eq!(subs["subscriptions"][0]["state"]["state"], "active");

    // MWI: unsolicited NOTIFY on the account's own socket.
    assert!(c.call("mwi.get", json!({"account": "a"})).await.unwrap()["mwi"].is_null());
    let local = s["accounts"][0]["local_addr"].as_str().unwrap().to_string();
    let body =
        "Messages-Waiting: yes\r\nMessage-Account: sip:1017@x\r\nVoice-Message: 2/1 (1/0)\r\n";
    let n = format!(
        "NOTIFY sip:1017@{local} SIP/2.0\r\nVia: SIP/2.0/UDP {naddr};branch=z9hG4bKmwi1\r\nFrom: <sip:pbx@x>;tag=p\r\nTo: <sip:1017@x>\r\nCall-ID: mwi-unsol\r\nCSeq: 1 NOTIFY\r\nEvent: message-summary\r\nContent-Type: application/simple-message-summary\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    notifier.send_to(n.as_bytes(), &local).await.unwrap();
    let e = poll(&mut c, seq0, "account.mwi").await;
    assert_eq!(e["account"], "a");
    assert_eq!(e["data"]["waiting"], true);
    assert_eq!(e["data"]["voice"]["new"], 2);
    assert_eq!(e["data"]["voice"]["urgent_new"], 1);
    let g = c.call("mwi.get", json!({"account": "a"})).await.unwrap();
    assert_eq!(g["mwi"]["voice"]["old"], 1);
    let all = c.call("mwi.get", json!({})).await.unwrap();
    assert_eq!(all["mwi"].as_array().unwrap().len(), 1);
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(s["accounts"][0]["mwi"]["new"], 2);
    assert_eq!(s["accounts"][0]["mwi"]["waiting"], true);

    // Generic subscribe / notify / unsubscribe.
    let seq1 = c.call("events.current_seq", Value::Null).await.unwrap()["seq"]
        .as_u64()
        .unwrap();
    let r = c
        .call(
            "subscribe",
            json!({"account":"a","uri":"1018","event":"presence","expires":60}),
        )
        .await
        .unwrap();
    let sid = r["sub_id"].as_str().unwrap().to_string();
    assert!(r["event_seq"].is_u64());
    let pidf = "<presence entity=\"sip:1018@x\"/>";
    serve_subscribe(&notifier, "presence", "application/pidf+xml", pidf).await;
    let e = poll(&mut c, seq1, "subscription.notify").await;
    assert_eq!(e["data"]["sub_id"], sid);
    assert_eq!(e["data"]["body"], pidf);
    assert_eq!(e["data"]["content_type"], "application/pidf+xml");
    assert_eq!(e["data"]["matched"], true);
    let e = poll(&mut c, seq1, "subscription.state").await;
    assert_eq!(e["account"], "a");
    assert!(e["data"]["state"].is_string(), "{e}");
    let subs = c
        .call("subscriptions.list", json!({"account": "a"}))
        .await
        .unwrap();
    assert_eq!(subs["subscriptions"].as_array().unwrap().len(), 2);

    // Managed subscriptions cannot be unsubscribed; unknown ids are not_found.
    let blf_id = subs["subscriptions"][0]["id"].as_str().unwrap().to_string();
    let e = c
        .call("unsubscribe", json!({"sub_id": blf_id}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("invalid_params"), "{e}");
    let e = c
        .call("unsubscribe", json!({"sub_id": "s99999"}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
    let e = c
        .call(
            "subscribe",
            json!({"account":"nope","uri":"1","event":"presence"}),
        )
        .await
        .unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
    // Unsubscribe: answer the Expires: 0 request so it does not wait for the timeout.
    let un = {
        let sid = sid.clone();
        let mut c2 = client(&app).await;
        tokio::spawn(async move { c2.call("unsubscribe", json!({"sub_id": sid})).await })
    };
    let mut buf = vec![0u8; 8192];
    loop {
        let (n, src) = tokio::time::timeout(Duration::from_secs(10), notifier.recv_from(&mut buf))
            .await
            .expect("no unsubscribe")
            .unwrap();
        let raw = String::from_utf8_lossy(&buf[..n]).into_owned();
        if raw.starts_with("SUBSCRIBE ") && header(&raw, "expires") == "0" {
            let via = raw
                .lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("via:"))
                .collect::<Vec<_>>()
                .join("\r\n");
            let resp = format!(
                "SIP/2.0 202 Accepted\r\n{via}\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n",
                header(&raw, "from"),
                header(&raw, "to"),
                header(&raw, "call-id"),
                header(&raw, "cseq")
            );
            notifier.send_to(resp.as_bytes(), src).await.unwrap();
            break;
        }
    }
    let r = un.await.unwrap().unwrap();
    assert_eq!(r["ok"], true);
    let subs = c
        .call("subscriptions.list", json!({"account": "a"}))
        .await
        .unwrap();
    assert_eq!(subs["subscriptions"].as_array().unwrap().len(), 1);

    drop(c);
    app.shutdown().await;
}
