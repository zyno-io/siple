//! Regression tests for review findings (runtime leaks, unregister races, flow
//! loss, Contact parsing, disable with calls, SNI scope, hot User-Agent, tls_fingerprint, ...).
//! No PBX: peer-mode accounts over loopback and a scripted fake registrar.

mod common;

use common::free_tcp_port as free_port;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Secret, Transport};
use siple_sip::{
    AccountStatus, AnswerOptions, CallState, DialOptions, NullMediaProvider, RegState, SipEvent,
    SipStack, SipStackOptions, UpdateOutcome, WireEvent, WireSink,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

// --- helpers ------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Wire {
    account: String,
    text: String,
}

#[derive(Default)]
struct Capture(Mutex<Vec<Wire>>);

impl WireSink for Capture {
    fn on_wire(&self, account: &str, ev: &WireEvent<'_>) {
        self.0.lock().unwrap().push(Wire {
            account: account.to_string(),
            text: String::from_utf8_lossy(ev.bytes).into_owned(),
        });
    }
}

struct Harness {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    #[allow(dead_code)]
    wire: Arc<Capture>,
}

fn harness() -> Harness {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (tx, rx) = unbounded_channel();
    let wire = Arc::new(Capture::default());
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-test/1".into(),
        events: tx,
        wire_sink: wire.clone(),
    });
    stack.set_media_provider(NullMediaProvider::new());
    Harness {
        stack,
        events: rx,
        wire,
    }
}

fn peer(id: &str, transport: Transport, local_port: u16) -> AccountConfig {
    AccountConfig {
        id: id.into(),
        username: id.into(),
        domain: "peer.test".into(),
        register: false,
        transport,
        local_port,
        bind_address: "127.0.0.1".into(),
        tls_verify: false,
        sip_trace: true,
        ..Default::default()
    }
}

fn general() -> GeneralConfig {
    GeneralConfig {
        user_agent: "Siple-test/1".into(),
        ..Default::default()
    }
}

async fn add(h: &Harness, cfg: AccountConfig) {
    h.stack
        .add_account(cfg, &general(), &MediaConfig::default())
        .await
        .unwrap();
}

async fn wait_status(
    h: &Harness,
    id: &str,
    what: &str,
    pred: impl Fn(&AccountStatus) -> bool,
) -> AccountStatus {
    let mut rx = h.stack.watch_status(id).unwrap();
    let res = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            {
                let s = rx.borrow_and_update().clone();
                if pred(&s) {
                    return s;
                }
            }
            rx.changed().await.unwrap();
        }
    })
    .await;
    res.unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {what}; status = {:?}",
            h.stack.account_status(id)
        )
    })
}

fn local_addr(h: &Harness, id: &str) -> SocketAddr {
    h.stack
        .account_status(id)
        .unwrap()
        .local_addr
        .expect("local_addr")
        .parse()
        .unwrap()
}

// --- registration against a scripted registrar ----------------------------------------------------

/// What the fake registrar saw.
#[derive(Clone, Debug)]
struct SeenRegister {
    text: String,
}

impl SeenRegister {
    fn header(&self, name: &str) -> Option<&str> {
        self.text.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
    fn expires(&self) -> Option<u32> {
        self.header("Expires")?.parse().ok()
    }
    fn contact_uri(&self) -> String {
        let c = self.header("Contact").unwrap_or_default();
        c.split(['<', '>']).nth(1).unwrap_or(c).to_string()
    }
    fn authed(&self) -> bool {
        self.header("Authorization").is_some()
    }
}

/// Behavior of one fake-registrar answer.
#[derive(Clone, Default)]
struct Answer {
    code: u16,
    reason: &'static str,
    extra: Vec<String>,
    /// `Contact: <ours>;expires=N` in a 200.
    granted: Option<u32>,
    /// Override of the Via `received` / `rport` the registrar claims to have seen.
    seen: Option<(String, u16)>,
}

struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<SeenRegister>>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// UDP registrar: 401 until an Authorization header arrives, then asks `decide(register_index,
/// request)` (index counts authenticated REGISTERs only).
fn fake_registrar(decide: impl Fn(usize, &SeenRegister) -> Answer + Send + 'static) -> Fake {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let addr = sock.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (seen2, stop2) = (seen.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        let mut authed_n = 0usize;
        while !stop2.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            if !text.starts_with("REGISTER ") {
                continue;
            }
            let req = SeenRegister { text };
            let answer = if !req.authed() {
                Answer {
                    code: 401,
                    reason: "Unauthorized",
                    extra: vec![
                        "WWW-Authenticate: Digest realm=\"fake\", nonce=\"n0nc3\", algorithm=MD5"
                            .into(),
                    ],
                    ..Default::default()
                }
            } else {
                let a = decide(authed_n, &req);
                authed_n += 1;
                a
            };
            seen2.lock().unwrap().push(req.clone());
            let resp = build_response(&req, &answer, from);
            sock.send_to(resp.as_bytes(), from).unwrap();
        }
    });
    Fake { addr, seen, stop }
}

fn build_response(req: &SeenRegister, a: &Answer, from: SocketAddr) -> String {
    let mut lines = vec![format!("SIP/2.0 {} {}", a.code, a.reason)];
    for l in req.text.lines() {
        let low = l.to_ascii_lowercase();
        if low.starts_with("via:") {
            // Claim we saw the sender at `seen` (or its real address).
            let (ip, port) = a
                .seen
                .clone()
                .unwrap_or((from.ip().to_string(), from.port()));
            let base: String = l
                .split(';')
                .filter(|p| {
                    !p.trim_start().starts_with("rport") && !p.trim_start().starts_with("received")
                })
                .collect::<Vec<_>>()
                .join(";");
            lines.push(format!("{base};received={ip};rport={port}"));
        } else if ["from:", "call-id:", "cseq:"]
            .iter()
            .any(|p| low.starts_with(p))
        {
            lines.push(l.to_string());
        } else if low.starts_with("to:") {
            lines.push(if l.contains("tag=") {
                l.to_string()
            } else {
                format!("{l};tag=fakereg")
            });
        }
    }
    lines.extend(a.extra.iter().cloned());
    if let Some(g) = a.granted {
        lines.push(format!("Contact: <{}>;expires={g}", req.contact_uri()));
    }
    lines.push("Content-Length: 0".into());
    format!("{}\r\n\r\n", lines.join("\r\n"))
}

fn reg_account(id: &str, fake: &Fake) -> AccountConfig {
    AccountConfig {
        id: id.into(),
        username: id.into(),
        password: Secret::new("secret"),
        domain: "fake.test".into(),
        registrar: fake.addr.to_string(),
        transport: Transport::Udp,
        bind_address: "127.0.0.1".into(),
        expires: 120,
        keepalive_secs: 0,
        ..Default::default()
    }
}

fn ok_answer(granted: u32) -> Answer {
    Answer {
        code: 200,
        reason: "OK",
        granted: Some(granted),
        ..Default::default()
    }
}

// ================= item 1: runtime leak =================

/// A free UDP port below the OS's ephemeral range, so parallel tests binding port 0 cannot be
/// handed the same number between our probe and the account's bind.
fn free_port_udp() -> u16 {
    static NEXT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    let base = 20_000 + (std::process::id() % 5_000) as u16;
    loop {
        let port = base + NEXT.fetch_add(1, Ordering::SeqCst) % 5_000;
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

async fn assert_udp_port_free(port: u16, what: &str) {
    for _ in 0..30 {
        if std::net::UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("UDP port {port} still held {what}");
}

#[tokio::test]
async fn fixed_udp_port_rebuild_rebinds_same_port() {
    let h = harness();
    let base = peer("p", Transport::Udp, free_port_udp());
    add(&h, base.clone()).await;
    assert!(h.stack.account_status("p").unwrap().local_addr.is_some());
    // Rebuild-class changes that keep local_port, several times in a row.
    let mut c = base.clone();
    for _ in 0..3 {
        c.tls_verify = !c.tls_verify;
        assert_eq!(
            h.stack.update_account(c.clone()).await.unwrap(),
            UpdateOutcome::Rebuilt
        );
        let s = wait_status(&h, "p", "rebuilt transport", |s| s.local_addr.is_some()).await;
        assert!(
            !matches!(s.registration, RegState::Failed { .. }),
            "rebuild on the same fixed port failed: {:?}",
            s.registration
        );
    }
    h.stack.shutdown().await;
}

#[tokio::test]
async fn fixed_udp_port_remove_then_add() {
    let h = harness();
    let port = free_port_udp();
    let base = peer("q", Transport::Udp, port);
    add(&h, base.clone()).await;
    h.stack.remove_account("q").await.unwrap();
    assert_udp_port_free(port, "after remove_account").await;
    add(&h, base).await;
    let s = h.stack.account_status("q").unwrap();
    assert!(
        s.local_addr.is_some(),
        "re-add failed: {:?}",
        s.registration
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn port_released_after_a_call_ended() {
    let mut h = harness();
    let port = free_port_udp();
    add(&h, peer("la", Transport::Udp, port)).await;
    add(&h, peer("lb", Transport::Udp, 0)).await;
    let b_addr = local_addr(&h, "lb");
    let call = h
        .stack
        .dial("la", &format!("sip:lb@{b_addr}"), DialOptions::default())
        .await
        .unwrap();
    let SipEvent::CallIncoming { call: incoming } =
        next_event(&mut h, |e| matches!(e, SipEvent::CallIncoming { .. })).await
    else {
        unreachable!()
    };
    h.stack
        .answer(&incoming.id, AnswerOptions::default())
        .await
        .unwrap();
    next_event(&mut h, |e| {
        matches!(e, SipEvent::CallState { call: c } if c.id == call && c.state == CallState::Active)
    })
    .await;
    h.stack.hangup(&call).await.unwrap();
    next_event(&mut h, |e| {
        matches!(e, SipEvent::CallState { call: c } if c.id == call && c.state == CallState::Ended)
    })
    .await;
    h.stack.remove_account("la").await.unwrap();
    assert_udp_port_free(port, "after the account with a finished call was removed").await;
    h.stack.shutdown().await;
}

async fn next_event(h: &mut Harness, pred: impl Fn(&SipEvent) -> bool) -> SipEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = h.events.recv().await.expect("event channel closed");
            if pred(&e) {
                return e;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

// ================= item 2 / 11: unregister races =================

#[tokio::test]
async fn unregister_while_first_register_inflight_removes_binding() {
    let fake = fake_registrar(|_, _| {
        std::thread::sleep(Duration::from_millis(800));
        ok_answer(120)
    });
    let h = harness();
    add(&h, reg_account("u1", &fake)).await;
    // The authenticated REGISTER is in flight (the fake sleeps while deciding).
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.stack.unregister("u1").unwrap();
    wait_status(&h, "u1", "unregistered", |s| {
        matches!(s.registration, RegState::Unregistered)
    })
    .await;
    let seen = fake.seen.lock().unwrap().clone();
    assert!(seen.iter().any(|s| s.authed() && s.expires() == Some(120)));
    let unreg = seen
        .iter()
        .filter(|s| s.authed() && s.expires() == Some(0))
        .collect::<Vec<_>>();
    assert_eq!(
        unreg.len(),
        1,
        "binding of the in-flight REGISTER must be removed"
    );
    let reg = seen.iter().find(|s| s.expires() == Some(120)).unwrap();
    assert_eq!(unreg[0].contact_uri(), reg.contact_uri(), "same Contact");
    h.stack.shutdown().await;
}

#[tokio::test]
async fn remove_while_first_register_inflight_removes_binding() {
    let fake = fake_registrar(|_, _| {
        std::thread::sleep(Duration::from_millis(800));
        ok_answer(120)
    });
    let h = harness();
    add(&h, reg_account("u2", &fake)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.stack.remove_account("u2").await.unwrap();
    let seen = fake.seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|s| s.authed() && s.expires() == Some(0)),
        "teardown must unregister the in-flight binding: {seen:?}"
    );
}

#[tokio::test]
async fn aor_change_unregisters_old_then_registers_new() {
    let fake = fake_registrar(|_, _| {
        std::thread::sleep(Duration::from_millis(150));
        ok_answer(120)
    });
    let h = harness();
    let cfg = reg_account("aor1", &fake);
    add(&h, cfg.clone()).await;
    wait_status(&h, "aor1", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let mut c = cfg.clone();
    c.username = "aor2".into();
    h.stack.update_account(c).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let seen: Vec<_> = fake
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.authed())
        .cloned()
        .collect();
    let to = |s: &SeenRegister| s.header("To").unwrap_or_default().to_string();
    let old_unreg = seen
        .iter()
        .position(|s| to(s).contains("aor1@") && s.expires() == Some(0))
        .expect("old AOR unregistered");
    let new_reg = seen
        .iter()
        .position(|s| to(s).contains("aor2@") && s.expires() == Some(120))
        .expect("new AOR registered");
    assert!(old_unreg < new_reg);
    // No REGISTER for the old AOR after its unregister, and a separate Call-ID for the new one.
    assert!(
        !seen[old_unreg + 1..]
            .iter()
            .any(|s| to(s).contains("aor1@"))
    );
    assert_ne!(
        seen[old_unreg].header("Call-ID"),
        seen[new_reg].header("Call-ID")
    );
    let st = h.stack.account_status("aor1").unwrap();
    assert!(
        matches!(st.registration, RegState::Registered { .. }),
        "{:?}",
        st.registration
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn aor_change_during_inflight_refresh_is_serialized() {
    // Refresh in flight (slow registrar) when the AOR changes: the old AOR's unregister must
    // follow the refresh (CSeq order) and the old Contact must not become "current".
    let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n2 = n.clone();
    let fake = fake_registrar(move |_, _| {
        if n2.fetch_add(1, Ordering::SeqCst) == 1 {
            std::thread::sleep(Duration::from_millis(700)); // the refresh
        }
        ok_answer(3) // refresh after ~1.5 s
    });
    let h = harness();
    let cfg = reg_account("aor3", &fake);
    add(&h, cfg.clone()).await;
    wait_status(&h, "aor3", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    // Wait for the refresh REGISTER to be in flight.
    for _ in 0..100 {
        if n.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = cfg.clone();
    c.username = "aor4".into();
    h.stack.update_account(c).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let seen: Vec<_> = fake
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.authed())
        .cloned()
        .collect();
    let cseq = |s: &SeenRegister| -> u32 {
        s.header("CSeq")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    let old: Vec<_> = seen
        .iter()
        .filter(|s| s.header("To").unwrap().contains("aor3@"))
        .collect();
    assert!(
        old.windows(2).all(|w| cseq(w[0]) <= cseq(w[1])), // == is a UDP retransmission
        "old-AOR REGISTERs reached the registrar out of CSeq order"
    );
    assert_eq!(
        old.last().unwrap().expires(),
        Some(0),
        "old AOR ends unregistered"
    );
    let st = h.stack.account_status("aor3").unwrap();
    assert!(
        st.contact.unwrap_or_default().contains("aor4"),
        "current contact is the new AOR's"
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn unregister_then_register_now_sends_one_register() {
    let fake = fake_registrar(|_, _| {
        std::thread::sleep(Duration::from_millis(200));
        ok_answer(120)
    });
    let h = harness();
    add(&h, reg_account("rr1", &fake)).await;
    wait_status(&h, "rr1", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let n0 = fake.seen.lock().unwrap().len();
    h.stack.unregister("rr1").unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.stack.register_now("rr1").unwrap();
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let after: Vec<_> = fake.seen.lock().unwrap()[n0..]
        .iter()
        .filter(|s| s.authed())
        .map(|s| s.expires())
        .collect();
    assert_eq!(
        after,
        [Some(0), Some(120)],
        "unregister, then exactly one REGISTER"
    );
    h.stack.shutdown().await;
}

// ================= item 3: flow closed right after the 200 =================

/// TCP registrar: 401, then 200 (expires=120) and immediately closes the connection. Counts
/// connections and REGISTERs.
fn tcp_close_after_200() -> (
    SocketAddr,
    Arc<Mutex<Vec<String>>>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let conns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (seen2, conns2) = (seen.clone(), conns.clone());
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { break };
            let n_conn = conns2.fetch_add(1, Ordering::SeqCst);
            let peer = s.peer_addr().unwrap();
            let seen3 = seen2.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    let Ok(n) = s.read(&mut tmp) else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let msg: Vec<u8> = buf.drain(..end + 4).collect();
                        let text = String::from_utf8_lossy(&msg).into_owned();
                        if !text.starts_with("REGISTER ") {
                            continue;
                        }
                        seen3.lock().unwrap().push(format!(
                            "conn{n_conn}: {}",
                            text.lines()
                                .find(|l| l.starts_with("Contact"))
                                .unwrap_or("")
                        ));
                        let req = SeenRegister { text };
                        let a = if !req.authed() {
                            Answer { code: 401, reason: "Unauthorized", extra: vec!["WWW-Authenticate: Digest realm=\"fake\", nonce=\"n0nc3\", algorithm=MD5".into()], ..Default::default() }
                        } else {
                            ok_answer(120)
                        };
                        let resp = build_response(&req, &a, peer);
                        s.write_all(resp.as_bytes()).unwrap();
                        if req.authed() && n_conn == 0 {
                            let _ = s.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                    }
                }
            });
        }
    });
    (addr, seen, conns)
}

#[tokio::test]
async fn tcp_flow_closed_right_after_200_is_noticed() {
    let (addr, _seen, conns) = tcp_close_after_200();
    let h = harness();
    let cfg = AccountConfig {
        id: "t1".into(),
        username: "t1".into(),
        password: Secret::new("secret"),
        domain: "fake.test".into(),
        registrar: addr.to_string(),
        transport: Transport::Tcp,
        bind_address: "127.0.0.1".into(),
        expires: 120,
        keepalive_secs: 0,
        ..Default::default()
    };
    add(&h, cfg).await;
    for _ in 0..80 {
        if conns.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let st = h.stack.account_status("t1").unwrap();
    assert!(
        conns.load(Ordering::SeqCst) >= 2,
        "registrar flow died after the 200 but no redial/re-REGISTER happened; state={:?}",
        st.registration
    );
    h.stack.shutdown().await;
}

// ================= item 4: comma-joined Contacts =================

#[tokio::test]
async fn comma_joined_contacts_grant_is_ours() {
    // The registrar lists another binding first, joined into one Contact header.
    let fake = fake_registrar(|_, req| Answer {
        code: 200,
        reason: "OK",
        extra: vec![format!(
            "Contact: <sip:other@10.9.9.9:5060>;expires=3600, <{}>;expires=7",
            req.contact_uri()
        )],
        ..Default::default()
    });
    let h = harness();
    add(&h, reg_account("cj", &fake)).await;
    let s = wait_status(&h, "cj", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let RegState::Registered { granted_secs, .. } = s.registration else {
        unreachable!()
    };
    assert_eq!(granted_secs, 7);
    h.stack.shutdown().await;
}

// ================= item 5: disable with active calls =================

#[tokio::test]
async fn disable_with_active_calls_is_immediate() {
    let fake = fake_registrar(|_, _| ok_answer(4));
    let h = harness();
    let cfg = reg_account("dis1", &fake);
    add(&h, cfg.clone()).await;
    wait_status(&h, "dis1", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    h.stack.set_active_calls("dis1", 1).unwrap();
    let mut c = cfg.clone();
    c.enabled = false;
    assert_eq!(
        h.stack.update_account(c).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    let st = h.stack.account_status("dis1").unwrap();
    assert!(!st.enabled);
    assert!(
        matches!(st.registration, RegState::Disabled),
        "{:?}",
        st.registration
    );
    assert!(!st.pending_rebuild);
    let n0 = fake.seen.lock().unwrap().len();
    assert!(
        fake.seen
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.authed() && s.expires() == Some(0)),
        "binding removed"
    );
    // Binding would have needed a refresh within ~2 s; nothing more may be sent.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(fake.seen.lock().unwrap().len(), n0);
    h.stack.shutdown().await;
}

#[tokio::test]
async fn disable_hangs_up_the_accounts_calls() {
    let mut h = harness();
    let cfg = peer("da", Transport::Udp, 0);
    add(&h, cfg.clone()).await;
    add(&h, peer("db", Transport::Udp, 0)).await;
    let b = local_addr(&h, "db");
    let call = h
        .stack
        .dial("da", &format!("sip:db@{b}"), DialOptions::default())
        .await
        .unwrap();
    let inc = match next_event(&mut h, |e| matches!(e, SipEvent::CallIncoming { .. })).await {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    };
    h.stack
        .answer(&inc.id, AnswerOptions::default())
        .await
        .unwrap();
    next_event(&mut h, |e| {
        matches!(e, SipEvent::CallState { call: c } if c.id == call && c.state == CallState::Active)
    })
    .await;
    let mut c = cfg;
    c.enabled = false;
    assert_eq!(
        h.stack.update_account(c).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    assert_eq!(h.stack.call(&call).unwrap().state, CallState::Ended);
    // The far side saw the BYE.
    next_event(&mut h, |e| {
        matches!(e, SipEvent::CallState { call: c } if c.id == inc.id && c.state == CallState::Ended)
    })
    .await;
    assert!(matches!(
        h.stack.account_status("da").unwrap().registration,
        RegState::Disabled
    ));
    h.stack.shutdown().await;
}

// ================= item 6: re-probe / peer-mode recovery =================

#[tokio::test]
async fn register_false_to_true_with_unresolvable_host_rebuilds() {
    let h = harness();
    let mut cfg = peer("rp", Transport::Udp, 0);
    cfg.domain = "no-such-host.invalid".into();
    add(&h, cfg.clone()).await;
    assert!(matches!(
        h.stack.account_status("rp").unwrap().registration,
        RegState::Unregistered
    ));
    cfg.register = true;
    assert_eq!(
        h.stack.update_account(cfg.clone()).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    // The rebuild re-resolves and reports the DNS failure (and retries), instead of "no registrar".
    let s = wait_status(
        &h,
        "rp",
        "dns failure",
        |s| matches!(&s.registration, RegState::Failed { reason, .. } if reason.contains("DNS")),
    )
    .await;
    assert_eq!(
        s.registration.problem(),
        Some(siple_sip::Problem::DnsFailed)
    );
    let RegState::Failed { reason, .. } = s.registration else {
        unreachable!()
    };
    assert!(!reason.contains("no registrar"), "{reason}");
    h.stack.shutdown().await;
}

#[tokio::test]
async fn register_false_to_true_with_resolvable_host_registers() {
    let fake = fake_registrar(|_, _| ok_answer(120));
    let h = harness();
    // Built without a registrar target (domain does not resolve), then pointed at a real one.
    let mut cfg = reg_account("rp2", &fake);
    cfg.register = false;
    cfg.registrar = String::new();
    cfg.domain = "no-such-host.invalid".into();
    add(&h, cfg.clone()).await;
    cfg.register = true;
    cfg.registrar = fake.addr.to_string();
    h.stack.update_account(cfg).await.unwrap();
    wait_status(&h, "rp2", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    h.stack.shutdown().await;
}

// ================= item 8: SNI scope =================

fn write_cert(dir: &std::path::Path, name: &str, days: Option<i64>, ca: bool) -> (String, String) {
    let mut p = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    p.subject_alt_names
        .push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
    p.distinguished_name
        .push(rcgen::DnType::CommonName, "Siple test");
    if let Some(d) = days {
        let now = std::time::SystemTime::now();
        p.not_before = (now - Duration::from_secs(86400)).into();
        p.not_after = (now + Duration::from_secs(86400 * d as u64)).into();
    }
    p.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    if ca {
        p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    }
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = p.self_signed(&key).unwrap();
    let c = dir.join(format!("{name}.crt"));
    let k = dir.join(format!("{name}.key"));
    std::fs::write(&c, cert.pem()).unwrap();
    std::fs::write(&k, key.serialize_pem()).unwrap();
    (
        c.to_string_lossy().into_owned(),
        k.to_string_lossy().into_owned(),
    )
}

/// A CA and a server leaf signed by it (SAN: localhost, 127.0.0.1): (leaf cert, leaf key, CA cert).
fn write_pki(dir: &std::path::Path, name: &str) -> (String, String, String) {
    write_pki_with(dir, name, true)
}

/// [`write_pki`]; `ip_san = false` leaves 127.0.0.1 out of the leaf (valid for `localhost` only).
fn write_pki_with(dir: &std::path::Path, name: &str, ip_san: bool) -> (String, String, String) {
    let now = std::time::SystemTime::now();
    let (nb, na) = (
        now - Duration::from_secs(86400),
        now + Duration::from_secs(86400 * 300),
    );
    let mut cp = rcgen::CertificateParams::default();
    cp.distinguished_name
        .push(rcgen::DnType::CommonName, "Siple test CA");
    cp.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    cp.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    (cp.not_before, cp.not_after) = (nb.into(), na.into());
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let ca_cert = cp.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::new(cp, ca_key);

    let mut lp = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    if ip_san {
        lp.subject_alt_names
            .push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
    }
    lp.distinguished_name
        .push(rcgen::DnType::CommonName, "Siple test");
    (lp.not_before, lp.not_after) = (nb.into(), na.into());
    lp.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = lp.signed_by(&leaf_key, &issuer).unwrap();

    let path = |ext: &str| dir.join(format!("{name}.{ext}"));
    std::fs::write(path("crt"), leaf.pem()).unwrap();
    std::fs::write(path("key"), leaf_key.serialize_pem()).unwrap();
    std::fs::write(path("ca"), ca_cert.pem()).unwrap();
    let s = |ext: &str| path(ext).to_string_lossy().into_owned();
    (s("crt"), s("key"), s("ca"))
}

#[tokio::test]
async fn tls_sni_is_not_pinned_to_the_registrar_for_other_targets() {
    let dir = tempfile::tempdir().unwrap();
    // A CA-signed leaf (SAN: localhost, 127.0.0.1) with the CA as the client's extra root. A
    // self-signed leaf used as its own anchor is rejected by the Windows platform verifier
    // (UnknownIssuer), which is about that setup, not about SNI.
    let (crt, key, ca) = write_pki(dir.path(), "sni");
    let h = harness();
    let port = free_port();
    let mut srv = peer("srvsni", Transport::Tls, port);
    srv.tls_cert = crt.clone();
    srv.tls_key = key.clone();
    add(&h, srv).await;
    // The account's registrar name (pbx.example.com) is not in the server certificate.
    let mut cli = peer("clisni", Transport::Tls, 0);
    cli.domain = "pbx.example.com".into();
    cli.tls_verify = true;
    cli.tls_ca_file = ca;
    add(&h, cli).await;
    for target in [
        format!("sip:x@127.0.0.1:{port};transport=tls"),
        format!("sip:x@localhost:{port};transport=tls"),
    ] {
        let r = h.stack.send_options("clisni", &target).await;
        assert_eq!(
            r.map(|r| r.code).map_err(|e| e.to_string()),
            Ok(200),
            "{target}"
        );
    }
    h.stack.shutdown().await;
}

/// A TLS request to a host that is not the account's registrar is verified against the host
/// NAME of the target, not the address Siple resolved it to: the certificate is valid for
/// `localhost` only (no IP SAN), so verifying the IP would fail with `NotValidForName`.
#[tokio::test]
async fn tls_request_to_other_host_verifies_the_name_not_the_ip() {
    let dir = tempfile::tempdir().unwrap();
    let (crt, key, ca) = write_pki_with(dir.path(), "namesan", false);
    let h = harness();
    let port = free_port();
    let mut srv = peer("srvname", Transport::Tls, port);
    srv.tls_cert = crt;
    srv.tls_key = key;
    add(&h, srv).await;
    let mut cli = peer("cliname", Transport::Tls, 0);
    cli.domain = "pbx.example.com".into();
    cli.tls_verify = true;
    cli.tls_ca_file = ca.clone();
    add(&h, cli).await;
    let cli_ca = ca;
    let by_name = format!("sip:x@localhost:{port};transport=tls");
    let r = h.stack.send_options("cliname", &by_name).await;
    assert_eq!(
        r.map(|r| r.code).map_err(|e| e.to_string()),
        Ok(200),
        "{by_name}"
    );
    // Other request kinds from fresh accounts (own transport layer: no cached connection, no
    // name learned by the OPTIONS above) resolve the name the same way.
    let client = |id: &str| {
        let mut c = peer(id, Transport::Tls, 0);
        c.domain = "pbx.example.com".into();
        c.tls_verify = true;
        c.tls_ca_file = cli_ca.clone();
        c
    };
    // INVITE (pre-connect and transaction).
    add(&h, client("clidial")).await;
    let call = h
        .stack
        .dial("clidial", &by_name, DialOptions::default())
        .await
        .unwrap_or_else(|e| panic!("dial {by_name}: {e}"));
    h.stack.hangup(&call).await.unwrap();
    // SUBSCRIBE (`dest_for`): the listener answers it (any final response), so the TLS
    // handshake got through; a verification failure leaves a local failure (code 0).
    add(&h, client("clisub")).await;
    let id = h
        .stack
        .subscribe("clisub", &by_name, "presence", None, None)
        .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let st = h
            .stack
            .subscriptions(Some("clisub"))
            .into_iter()
            .find(|s| s.id == id)
            .map(|s| s.state);
        match st {
            Some(siple_sip::SubState::Failed { code: 0, reason }) => {
                panic!("subscribe never reached the listener: {reason}")
            }
            Some(siple_sip::SubState::Pending) | None if std::time::Instant::now() < end => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            other => {
                assert!(
                    !matches!(other, Some(siple_sip::SubState::Pending) | None),
                    "subscribe: no answer"
                );
                break;
            }
        }
    }
    h.stack.shutdown().await;
}

/// Wire texts the capture saw for `account` starting with `method` (e.g. `"INVITE "`).
fn wire_requests(h: &Harness, account: &str, method: &str) -> Vec<String> {
    h.wire
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|w| w.account == account && w.text.starts_with(method))
        .map(|w| w.text.clone())
        .collect()
}

/// Waits for the first `method` request `account` put on the wire.
async fn wait_wire(h: &Harness, account: &str, method: &str) -> String {
    for _ in 0..200 {
        if let Some(t) = wire_requests(h, account, method).into_iter().next() {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no {method:?} request from {account} on the wire");
}

/// The Contact header of a request over TLS carries `;transport=tls` and the Via says TLS.
fn assert_tls_request(text: &str) {
    assert!(text.contains("SIP/2.0/TLS"), "Via: {text}");
    let contact = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("contact:"))
        .unwrap_or_else(|| panic!("no Contact: {text}"));
    assert!(
        contact.to_ascii_lowercase().contains(";transport=tls"),
        "{contact}"
    );
}

// ================= TLS from any account (on demand) =================

/// A UDP account sends TLS requests (OPTIONS, INVITE, SUBSCRIBE): its client TLS config is
/// installed on the first one. The certificate is valid for `localhost` only (no IP SAN), so the
/// name must be registered after the config exists.
#[tokio::test]
async fn udp_account_dials_tls_with_on_demand_config() {
    let dir = tempfile::tempdir().unwrap();
    let (crt, key, ca) = write_pki_with(dir.path(), "udptls", false);
    let h = harness();
    let port = free_port();
    let mut srv = peer("srvudptls", Transport::Tls, port);
    srv.tls_cert = crt;
    srv.tls_key = key;
    add(&h, srv).await;
    let mut cli = peer("cliudptls", Transport::Udp, 0);
    cli.tls_verify = true;
    cli.tls_ca_file = ca;
    add(&h, cli).await;
    let by_name = format!("sip:x@localhost:{port};transport=tls");

    let r = h.stack.send_options("cliudptls", &by_name).await;
    assert_eq!(
        r.map(|r| r.code).map_err(|e| e.to_string()),
        Ok(200),
        "{by_name}"
    );
    assert_tls_request(&wait_wire(&h, "cliudptls", "OPTIONS ").await);

    let call = h
        .stack
        .dial("cliudptls", &by_name, DialOptions::default())
        .await
        .unwrap_or_else(|e| panic!("dial {by_name}: {e}"));
    assert_tls_request(&wait_wire(&h, "cliudptls", "INVITE ").await);
    h.stack.hangup(&call).await.unwrap();

    h.stack
        .subscribe("cliudptls", &by_name, "presence", None, None)
        .unwrap();
    assert_tls_request(&wait_wire(&h, "cliudptls", "SUBSCRIBE ").await);

    // UDP requests still work, and go out over UDP.
    add(&h, peer("udpsrv", Transport::Udp, 0)).await;
    let udp = local_addr(&h, "udpsrv");
    let r = h
        .stack
        .send_options("cliudptls", &format!("sip:x@{udp}"))
        .await;
    assert_eq!(r.map(|r| r.code).map_err(|e| e.to_string()), Ok(200));
    h.stack.shutdown().await;
}

/// `tls_fingerprint` pins the provider only: other TLS hosts are verified with the OS roots and
/// `tls_ca_file` under the name dialed.
#[tokio::test]
async fn pinned_tls_account_calls_other_hosts_through_ca_file() {
    let dir = tempfile::tempdir().unwrap();
    let (pcrt, pkey) = write_cert(dir.path(), "provider", Some(1000), true);
    let cert = siple_sip::transport::read_pem_certs(&pcrt)
        .unwrap()
        .remove(0);
    let fp: String = siple_sip::transport::cert_sha256(&cert)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let (ocrt, okey, oca) = write_pki_with(dir.path(), "other", false);
    let h = harness();
    let (pport, oport) = (free_port(), free_port());
    let mut prov = peer("srvprov", Transport::Tls, pport);
    prov.tls_cert = pcrt;
    prov.tls_key = pkey;
    add(&h, prov).await;
    let mut other = peer("srvother", Transport::Tls, oport);
    other.tls_cert = ocrt;
    other.tls_key = okey;
    add(&h, other).await;

    let mut cli = peer("clipin", Transport::Tls, 0);
    cli.registrar = format!("127.0.0.1:{pport}");
    cli.tls_verify = true;
    cli.tls_fingerprint = fp;
    cli.tls_ca_file = oca;
    add(&h, cli).await;
    let to_provider = format!("sip:x@127.0.0.1:{pport};transport=tls");
    let to_other = format!("sip:x@localhost:{oport};transport=tls");
    for t in [&to_provider, &to_other] {
        let r = h.stack.send_options("clipin", t).await;
        assert_eq!(r.map(|r| r.code).map_err(|e| e.to_string()), Ok(200), "{t}");
    }
    let call = h
        .stack
        .dial("clipin", &to_other, DialOptions::default())
        .await
        .unwrap_or_else(|e| panic!("dial {to_other}: {e}"));
    h.stack.hangup(&call).await.unwrap();
    h.stack.shutdown().await;
}

/// `tls_verify = false` ignores certificate errors of the provider only.
#[tokio::test]
async fn tls_verify_false_is_provider_only() {
    let dir = tempfile::tempdir().unwrap();
    let (pcrt, pkey) = write_cert(dir.path(), "prov", None, false);
    let (ocrt, okey) = write_cert(dir.path(), "oth", None, false);
    let h = harness();
    let (pport, oport) = (free_port(), free_port());
    let mut prov = peer("srvp3", Transport::Tls, pport);
    prov.tls_cert = pcrt;
    prov.tls_key = pkey;
    add(&h, prov).await;
    let mut other = peer("srvo3", Transport::Tls, oport);
    other.tls_cert = ocrt;
    other.tls_key = okey;
    add(&h, other).await;

    let mut cli = peer("clinv", Transport::Tls, 0);
    cli.registrar = format!("127.0.0.1:{pport}");
    add(&h, cli).await; // tls_verify = false
    let r = h
        .stack
        .send_options("clinv", &format!("sip:x@127.0.0.1:{pport};transport=tls"))
        .await;
    assert_eq!(r.map(|r| r.code).map_err(|e| e.to_string()), Ok(200));
    let to_other = format!("sip:x@localhost:{oport};transport=tls");
    let e = h.stack.send_options("clinv", &to_other).await.unwrap_err();
    assert_eq!(e.problem(), Some(siple_sip::Problem::TlsUntrusted), "{e}");

    // A UDP account: its provider is not the target either.
    add(&h, peer("cliudpnv", Transport::Udp, 0)).await;
    let e = h
        .stack
        .dial("cliudpnv", &to_other, DialOptions::default())
        .await
        .unwrap_err();
    assert_eq!(e.problem(), Some(siple_sip::Problem::TlsUntrusted), "{e}");
    h.stack.shutdown().await;
}

/// Bad `tls_*` settings of a UDP account fail only its TLS requests.
#[tokio::test]
async fn bad_tls_settings_fail_only_tls_requests_of_a_udp_account() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir
        .path()
        .join("missing.pem")
        .to_string_lossy()
        .into_owned();
    let h = harness();
    add(&h, peer("udpok", Transport::Udp, 0)).await;
    let udp = format!("sip:x@{}", local_addr(&h, "udpok"));
    let tls_target = "sip:x@localhost:9;transport=tls";

    for (id, ca, fp) in [("badca", missing.as_str(), ""), ("badfp", "", "nope")] {
        let mut cfg = peer(id, Transport::Udp, 0);
        cfg.tls_verify = true;
        cfg.tls_ca_file = ca.into();
        cfg.tls_fingerprint = fp.into();
        add(&h, cfg).await;
        let ok = |r: siple_sip::Result<siple_sip::OptionsResponse>| {
            r.map(|r| r.code).map_err(|e| e.to_string())
        };
        assert_eq!(
            ok(h.stack.send_options(id, &udp).await),
            Ok(200),
            "{id} udp"
        );
        let e = h.stack.send_options(id, tls_target).await.unwrap_err();
        assert_eq!(
            e.problem(),
            Some(siple_sip::Problem::TlsConfig),
            "{id}: {e}"
        );
        if !ca.is_empty() {
            assert!(e.to_string().contains("missing.pem"), "{e}");
        }
        let e = h
            .stack
            .dial(id, tls_target, DialOptions::default())
            .await
            .unwrap_err();
        assert_eq!(
            e.problem(),
            Some(siple_sip::Problem::TlsConfig),
            "{id}: {e}"
        );
        let st = h.stack.account_status(id).unwrap();
        assert!(
            !matches!(st.registration, RegState::Failed { .. }),
            "{st:?}"
        );
        assert_eq!(
            ok(h.stack.send_options(id, &udp).await),
            Ok(200),
            "{id} udp again"
        );
    }
    h.stack.shutdown().await;
}

// ================= item 9: hot User-Agent =================

#[tokio::test]
async fn hot_user_agent_applies_to_responses() {
    let h = harness();
    let mut a = peer("uaA", Transport::Udp, 0);
    a.user_agent = "OldUA/1".into();
    add(&h, a.clone()).await;
    add(&h, peer("uaB", Transport::Udp, 0)).await;
    a.user_agent = "NewUA/2".into();
    assert_eq!(h.stack.update_account(a).await.unwrap(), UpdateOutcome::Hot);
    let a_addr = local_addr(&h, "uaA");
    let r = h
        .stack
        .send_options("uaB", &format!("sip:uaA@{a_addr}"))
        .await
        .unwrap();
    assert_eq!(r.header("User-Agent"), Some("NewUA/2"), "A's response");
    h.stack.shutdown().await;
}

// ================= item 10: tls_fingerprint =================

#[tokio::test]
async fn tls_fingerprint_pins_the_leaf_certificate() {
    let dir = tempfile::tempdir().unwrap();
    // 1000-day self-signed CA:TRUE certificate: what platform verifiers refuse.
    let (crt, key) = write_cert(dir.path(), "fp", Some(1000), true);
    let cert = siple_sip::transport::read_pem_certs(&crt)
        .unwrap()
        .remove(0);
    let fp: String = siple_sip::transport::cert_sha256(&cert)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":");
    let h = harness();
    let port = free_port();
    let mut srv = peer("srvfp", Transport::Tls, port);
    srv.tls_cert = crt.clone();
    srv.tls_key = key;
    add(&h, srv).await;
    let target = format!("sip:x@127.0.0.1:{port};transport=tls");

    // The server is the provider (registrar), which is where the pin applies.
    let mut ok = peer("cfp_ok", Transport::Tls, 0);
    ok.registrar = format!("127.0.0.1:{port}");
    ok.tls_verify = true;
    ok.tls_fingerprint = fp.clone();
    ok.tls_server_name = "name.that.does.not.match".into();
    add(&h, ok).await;
    assert_eq!(
        h.stack.send_options("cfp_ok", &target).await.unwrap().code,
        200
    );

    let mut bad = peer("cfp_bad", Transport::Tls, 0);
    bad.registrar = format!("127.0.0.1:{port}");
    bad.tls_verify = false; // the pin still applies
    bad.tls_fingerprint = "00".repeat(32);
    add(&h, bad).await;
    assert!(h.stack.send_options("cfp_bad", &target).await.is_err());

    // The pin is for the provider only: a server that is not the account's provider is
    // verified as usual (this self-signed certificate is not trusted).
    let mut elsewhere = peer("cfp_peer", Transport::Tls, 0);
    elsewhere.tls_verify = true;
    elsewhere.tls_fingerprint = fp.clone();
    add(&h, elsewhere).await;
    assert!(h.stack.send_options("cfp_peer", &target).await.is_err());
    h.stack.shutdown().await;
}

/// An INVITE that cannot fit one UDP datagram fails the dial at once (no 32 s timer B wait).
#[tokio::test(flavor = "multi_thread")]
async fn oversized_udp_invite_fails_fast() {
    let h = harness();
    add(&h, peer("oa", Transport::Udp, free_port())).await;
    let t = std::time::Instant::now();
    let opts = DialOptions {
        headers: vec![("X-Big".into(), "a".repeat(70_000))],
        ..Default::default()
    };
    let err = h
        .stack
        .dial("oa", "sip:ob@127.0.0.1:9", opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("too large for one UDP"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(3));
}
