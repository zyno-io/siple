//! Local integration tests: no PBX. Peer-mode accounts talking to each other over loopback,
//! raw-socket probes of the router, and a scripted fake registrar for the REGISTER state machine.

mod common;

use common::free_tcp_port as free_port;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Secret, Transport};
use siple_sip::{
    ALLOW, AccountStatus, RegState, SipEvent, SipStack, SipStackOptions, UpdateOutcome,
    WireDirection, WireEvent, WireSink, WireTransport,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

// --- helpers ------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Wire {
    account: String,
    direction: WireDirection,
    transport: WireTransport,
    text: String,
}

#[derive(Default)]
struct Capture(Mutex<Vec<Wire>>);

impl WireSink for Capture {
    fn on_wire(&self, account: &str, ev: &WireEvent<'_>) {
        self.0.lock().unwrap().push(Wire {
            account: account.to_string(),
            direction: ev.direction,
            transport: ev.transport,
            text: String::from_utf8_lossy(ev.bytes).into_owned(),
        });
    }
}

struct Harness {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    wire: Arc<Capture>,
}

/// Default account: `replaces`, `100rel` (prack = supported) and `timer` (session_timers).
const SUPPORTED_ALL: &str = "replaces, 100rel, timer";

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

// --- OPTIONS between two accounts ----------------------------------------------------------------

async fn options_between(transport: Transport) {
    let mut h = harness();
    let b_port = if transport == Transport::Udp {
        0
    } else {
        free_port()
    };
    add(&h, peer("a", transport, 0)).await;
    add(&h, peer("b", transport, b_port)).await;
    let b = local_addr(&h, "b");
    let target = format!("sip:b@{b}");

    let resp = h.stack.send_options("a", &target).await.expect("OPTIONS");
    assert_eq!(resp.code, 200, "{resp:?}");
    assert_eq!(resp.header("Allow"), Some(ALLOW));
    assert_eq!(resp.header("Supported"), Some(SUPPORTED_ALL));
    assert!(resp.header("Accept").is_some());

    let ev = next_event(&mut h, |e| matches!(e, SipEvent::OptionsReceived { .. })).await;
    let SipEvent::OptionsReceived { account, from } = ev else {
        unreachable!()
    };
    assert_eq!(account, "b");
    assert!(from.contains("a@peer.test"), "{from}");

    // The wire sink saw both directions on both accounts, over the right transport.
    let wire = h.wire.0.lock().unwrap().clone();
    let tp = match transport {
        Transport::Udp => WireTransport::Udp,
        Transport::Tcp => WireTransport::Tcp,
        Transport::Tls => WireTransport::Tls,
        Transport::Auto => unreachable!("peer accounts use a fixed transport"),
    };
    let find = |acct: &str, dir: WireDirection, prefix: &str| {
        wire.iter().any(|w| {
            w.account == acct
                && w.direction == dir
                && w.transport == tp
                && w.text.starts_with(prefix)
        })
    };
    assert!(
        find("a", WireDirection::Send, "OPTIONS "),
        "a sent OPTIONS: {wire:#?}"
    );
    assert!(
        find("b", WireDirection::Recv, "OPTIONS "),
        "b received OPTIONS"
    );
    assert!(find("b", WireDirection::Send, "SIP/2.0 200"), "b sent 200");
    assert!(
        find("a", WireDirection::Recv, "SIP/2.0 200"),
        "a received 200"
    );
    let sent = wire
        .iter()
        .find(|w| w.account == "a" && w.text.starts_with("OPTIONS "))
        .unwrap();
    assert!(
        sent.text.contains("User-Agent: Siple-test/1"),
        "{}",
        sent.text
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn options_over_udp() {
    options_between(Transport::Udp).await;
}

#[tokio::test]
async fn options_over_tcp() {
    options_between(Transport::Tcp).await;
}

#[tokio::test]
async fn options_over_tls_with_self_signed_listener() {
    options_between(Transport::Tls).await;
}

#[tokio::test]
async fn trace_toggle_installs_and_removes_the_tap() {
    let h = harness();
    let mut a = peer("a", Transport::Udp, 0);
    a.sip_trace = false;
    add(&h, a).await;
    add(&h, peer("b", Transport::Udp, 0)).await; // traced
    let target = format!("sip:b@{}", local_addr(&h, "b"));
    h.stack.send_options("a", &target).await.unwrap();
    assert!(
        !h.wire.0.lock().unwrap().iter().any(|w| w.account == "a"),
        "tap not installed while sip_trace is off"
    );
    assert!(!h.stack.account_status("a").unwrap().trace_enabled);

    h.stack.set_sip_trace("a", true).unwrap();
    wait_status(&h, "a", "trace on", |s| s.trace_enabled).await;
    h.stack.send_options("a", &target).await.unwrap();
    assert!(
        h.wire
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.account == "a" && w.direction == WireDirection::Send)
    );

    h.stack.set_sip_trace("a", false).unwrap();
    wait_status(&h, "a", "trace off", |s| !s.trace_enabled).await;
    let before = h
        .wire
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|w| w.account == "a")
        .count();
    h.stack.send_options("a", &target).await.unwrap();
    assert_eq!(
        h.wire
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|w| w.account == "a")
            .count(),
        before
    );
    h.stack.shutdown().await;
}

// --- router: raw probes --------------------------------------------------------------------------

struct Probe {
    sock: UdpSocket,
    dest: SocketAddr,
}

impl Probe {
    fn new(dest: SocketAddr) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Probe { sock, dest }
    }

    /// Sends a request and returns the first final (>= 200) response as text.
    fn request(&self, method: &str, to_tag: Option<&str>, extra: &str, body: &str) -> String {
        let me = self.sock.local_addr().unwrap();
        let tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        let msg = format!(
            "{method} sip:b@{dest} SIP/2.0\r\nVia: SIP/2.0/UDP {me};branch=z9hG4bK{n}\r\nMax-Forwards: 70\r\nFrom: <sip:probe@probe.test>;tag=ptag\r\nTo: <sip:b@{dest}>{tag}\r\nCall-ID: probe-{n}@test\r\nCSeq: 1 {method}\r\nContact: <sip:probe@{me}>\r\n{extra}Content-Length: {len}\r\n\r\n{body}",
            dest = self.dest,
            n = rand_n(),
            len = body.len(),
        );
        self.sock.send_to(msg.as_bytes(), self.dest).unwrap();
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, _) = self
                .sock
                .recv_from(&mut buf)
                .expect("no response from router");
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            let code: u16 = text
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            if code >= 200 {
                return text;
            }
        }
    }
}

fn rand_n() -> u64 {
    use std::sync::atomic::AtomicU64;
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed) + std::process::id() as u64 * 1000
}

fn status_line(resp: &str) -> &str {
    resp.lines().next().unwrap()
}

#[tokio::test]
async fn router_answers_each_request_kind() {
    let mut h = harness();
    add(&h, peer("b", Transport::Udp, 0)).await;
    let dest = local_addr(&h, "b");
    // Raw blocking sockets must not run on the runtime thread that serves the account.
    let probe = Arc::new(Probe::new(dest));
    let run = |method: &'static str,
               to_tag: Option<&'static str>,
               extra: &'static str,
               body: &'static str| {
        let p = probe.clone();
        tokio::task::spawn_blocking(move || p.request(method, to_tag, extra, body))
    };

    // INVITE whose SDP offers no audio is refused before ringing (488); real call flows are
    // in tests/calls.rs.
    let r = run(
        "INVITE",
        None,
        "Content-Type: application/sdp\r\n",
        "v=0\r\n",
    )
    .await
    .unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 488"), "{r}");
    // Re-INVITE for an unknown dialog.
    let r = run("INVITE", Some("nodialog"), "", "").await.unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 481"), "{r}");
    // SUBSCRIBE -> 489 Bad Event.
    let r = run("SUBSCRIBE", None, "Event: presence\r\nExpires: 60\r\n", "")
        .await
        .unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 489"), "{r}");
    // In-dialog BYE for an unknown dialog -> 481.
    let r = run("BYE", Some("nodialog"), "", "").await.unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 481"), "{r}");
    // Methods we do not serve -> 405 with Allow.
    let r = run("REGISTER", None, "Contact: <sip:x@127.0.0.1>\r\n", "")
        .await
        .unwrap();
    assert!(
        status_line(&r).starts_with("SIP/2.0 405") && r.contains(ALLOW),
        "{r}"
    );

    // Unsolicited NOTIFY (FreeSWITCH message-summary after REGISTER) -> 200 + event.
    let r = run("NOTIFY", None, "Event: message-summary\r\nSubscription-State: terminated;reason=noresource\r\nContent-Type: application/simple-message-summary\r\n", "Messages-Waiting: no\r\n").await.unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 200"), "{r}");
    let ev = next_event(&mut h, |e| matches!(e, SipEvent::UnhandledNotify { .. })).await;
    let SipEvent::UnhandledNotify {
        account,
        event,
        body,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!((account.as_str(), event.as_str()), ("b", "message-summary"));
    assert!(body.contains("Messages-Waiting: no"));

    // MESSAGE -> 200 + event.
    let r = run(
        "MESSAGE",
        None,
        "Content-Type: text/plain\r\n",
        "hello there",
    )
    .await
    .unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 200"), "{r}");
    let ev = next_event(&mut h, |e| matches!(e, SipEvent::MessageReceived { .. })).await;
    let SipEvent::MessageReceived {
        content_type,
        body,
        from,
        ..
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(
        (content_type.as_str(), body.as_str()),
        ("text/plain", "hello there")
    );
    assert!(from.contains("probe@probe.test"), "{from}");

    // OPTIONS carries Allow / Accept / Supported.
    let r = run("OPTIONS", None, "", "").await.unwrap();
    assert!(status_line(&r).starts_with("SIP/2.0 200"), "{r}");
    assert!(
        r.contains(&format!("Allow: {ALLOW}"))
            && r.contains("Accept: application/sdp")
            && r.contains(&format!("Supported: {SUPPORTED_ALL}")),
        "{r}"
    );
    h.stack.shutdown().await;
}

// --- account management --------------------------------------------------------------------------

#[tokio::test]
async fn account_management_errors_and_status() {
    let h = harness();
    add(&h, peer("a", Transport::Udp, 0)).await;
    let dup = h
        .stack
        .add_account(
            peer("a", Transport::Udp, 0),
            &GeneralConfig::default(),
            &MediaConfig::default(),
        )
        .await;
    assert!(matches!(dup, Err(siple_sip::SipError::DuplicateAccount(_))));
    assert!(matches!(
        h.stack.account_status("zzz"),
        Err(siple_sip::SipError::UnknownAccount(_))
    ));
    assert!(matches!(
        h.stack.unregister("zzz"),
        Err(siple_sip::SipError::UnknownAccount(_))
    ));
    let s = h.stack.account_status("a").unwrap();
    assert_eq!(
        (s.enabled, s.aor.as_str(), s.registration.name()),
        (true, "sip:a@peer.test", "unregistered")
    );
    assert!(
        s.local_addr.is_some()
            && s.contact
                .as_deref()
                .is_some_and(|c| c.starts_with("sip:a@127.0.0.1:"))
    );
    // Serializes for the control API.
    let json = serde_json_like(&s);
    assert!(json.contains("unregistered"), "{json}");

    let mut off = peer("off", Transport::Udp, 0);
    off.enabled = false;
    add(&h, off).await;
    assert_eq!(
        h.stack.account_status("off").unwrap().registration,
        RegState::Disabled
    );
    assert!(h.stack.account_status("off").unwrap().local_addr.is_none());
    assert_eq!(
        h.stack
            .accounts_status()
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["a", "off"]
    );

    h.stack.remove_account("a").await.unwrap();
    assert!(h.stack.account_status("a").is_err());
    h.stack.shutdown().await;
    assert!(h.stack.accounts_status().is_empty());
}

fn serde_json_like(s: &AccountStatus) -> String {
    // siple-sip does not depend on serde_json; Debug of the Serialize-derived registration is
    // enough to prove the status is constructible, and the serde derive is compile-checked.
    fn assert_serialize<T: serde::Serialize>(_: &T) {}
    assert_serialize(s);
    s.registration.name().to_string()
}

#[tokio::test]
async fn update_classification_hot_reregister_rebuild_deferred() {
    let h = harness();
    let base = peer("a", Transport::Udp, 0);
    add(&h, base.clone()).await;
    let first_addr = local_addr(&h, "a");

    let mut c = base.clone();
    c.dnd = true;
    c.user_agent = "Other/2".into();
    assert_eq!(
        h.stack.update_account(c.clone()).await.unwrap(),
        UpdateOutcome::Hot
    );
    assert_eq!(
        local_addr(&h, "a"),
        first_addr,
        "hot change keeps the socket"
    );

    c.expires = 60;
    assert_eq!(
        h.stack.update_account(c.clone()).await.unwrap(),
        UpdateOutcome::Reregistered
    );
    assert_eq!(local_addr(&h, "a"), first_addr);

    // Active calls defer a rebuild.
    h.stack.set_active_calls("a", 1).unwrap();
    c.local_port = free_port();
    assert_eq!(
        h.stack.update_account(c.clone()).await.unwrap(),
        UpdateOutcome::RebuildDeferred
    );
    let s = wait_status(&h, "a", "pending_rebuild", |s| s.pending_rebuild).await;
    assert_eq!(
        s.local_addr.unwrap().parse::<SocketAddr>().unwrap(),
        first_addr
    );
    // ...and run when the last call ends.
    h.stack.set_active_calls("a", 0).unwrap();
    let s = wait_status(&h, "a", "rebuilt", |s| {
        !s.pending_rebuild
            && s.local_addr
                .as_deref()
                .and_then(|a| a.parse::<SocketAddr>().ok())
                .is_some_and(|a| a.port() == c.local_port)
    })
    .await;
    assert!(s.local_addr.is_some());

    // No calls: rebuild happens right away; disabling stops the account.
    c.local_port = 0;
    assert_eq!(
        h.stack.update_account(c.clone()).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    c.enabled = false;
    assert_eq!(
        h.stack.update_account(c.clone()).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    wait_status(&h, "a", "disabled", |s| {
        s.registration == RegState::Disabled && s.local_addr.is_none()
    })
    .await;
    c.enabled = true;
    assert_eq!(
        h.stack.update_account(c.clone()).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    wait_status(&h, "a", "restarted", |s| s.local_addr.is_some()).await;

    // reconfig_force overrides the deferral.
    h.stack.set_active_calls("a", 2).unwrap();
    h.stack.set_general(&GeneralConfig {
        reconfig_force: true,
        ..general()
    });
    c.local_port = free_port();
    assert_eq!(
        h.stack.update_account(c).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    h.stack.shutdown().await;
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

/// Behaviour of one fake-registrar answer.
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

#[tokio::test]
async fn register_digest_granted_expiry_refresh_and_unregister() {
    let fake = fake_registrar(|_, _| ok_answer(45));
    let mut h = harness();
    add(&h, reg_account("r1", &fake)).await;
    let s = wait_status(&h, "r1", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let RegState::Registered {
        granted_secs,
        expires_at,
        registered_at,
        last_renewed_at,
    } = s.registration
    else {
        unreachable!()
    };
    assert_eq!(
        granted_secs, 45,
        "taken from our Contact's expires param, not the 120 requested"
    );
    assert!(expires_at > chrono::Utc::now());
    assert_eq!(registered_at, last_renewed_at);
    assert_eq!((expires_at - last_renewed_at).num_seconds(), 45);
    assert!(
        s.contact
            .as_deref()
            .unwrap()
            .starts_with("sip:r1@127.0.0.1:")
    );
    {
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "401 round + authenticated");
        assert!(!seen[0].authed() && seen[1].authed());
        assert_eq!(seen[1].expires(), Some(120));
        assert_eq!(seen[0].header("Allow"), Some(ALLOW));
        assert_eq!(seen[0].header("Supported"), Some(SUPPORTED_ALL));
        assert_eq!(seen[0].header("User-Agent"), Some("Siple-test/1"));
        assert!(
            seen[0].text.starts_with("REGISTER sip:"),
            "{}",
            seen[0].text
        );
        assert_eq!(
            seen[0].header("To").map(|t| t.contains("sip:r1@fake.test")),
            Some(true),
            "AOR is user@domain, not the registrar"
        );
        // CSeq increments, Call-ID is stable.
        let (c0, c1) = (
            seen[0].header("CSeq").unwrap(),
            seen[1].header("CSeq").unwrap(),
        );
        assert_eq!((c0, c1), ("1 REGISTER", "2 REGISTER"));
        assert_eq!(seen[0].header("Call-ID"), seen[1].header("Call-ID"));
    }
    let ev = next_event(&mut h, |e| {
        matches!(
            e,
            SipEvent::Registration {
                state: RegState::Registered { .. },
                ..
            }
        )
    })
    .await;
    assert!(matches!(ev, SipEvent::Registration { ref account, .. } if account == "r1"));

    // Refresh at min(85 %, 45-30) = 15 s is too slow for a test; unregister instead.
    let contact = fake.seen.lock().unwrap()[1].contact_uri();
    h.stack.unregister("r1").unwrap();
    wait_status(&h, "r1", "unregistered", |s| {
        s.registration == RegState::Unregistered
    })
    .await;
    let last = fake.seen.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.expires(), Some(0), "unregister is Expires: 0");
    assert_eq!(
        last.contact_uri(),
        contact,
        "only our own Contact is removed (no Contact: *)"
    );
    // register_now brings it back.
    h.stack.register_now("r1").unwrap();
    wait_status(&h, "r1", "registered again", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    h.stack.shutdown().await;
    // shutdown unregistered too.
    assert_eq!(fake.seen.lock().unwrap().last().unwrap().expires(), Some(0));
}

#[tokio::test]
async fn register_refreshes_at_85_percent_or_expiry_minus_30() {
    // granted 4 s: refresh at max(min(3, 0), 2) = 2 s.
    let fake = fake_registrar(|_, _| ok_answer(4));
    let h = harness();
    add(&h, reg_account("r2", &fake)).await;
    let first = wait_status(&h, "r2", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let t0 = std::time::Instant::now();
    loop {
        let authed = fake
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.authed())
            .count();
        if authed >= 2 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "no refresh");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let dt = t0.elapsed();
    assert!(
        dt > Duration::from_millis(1200) && dt < Duration::from_secs(4),
        "refresh after {dt:?}"
    );
    let RegState::Registered {
        registered_at: first_at,
        last_renewed_at: first_renewal,
        ..
    } = first.registration
    else {
        unreachable!()
    };
    let refreshed = wait_status(&h, "r2", "renewed timestamp", |s| {
        matches!(s.registration, RegState::Registered { last_renewed_at, .. } if last_renewed_at > first_renewal)
    }).await;
    let RegState::Registered {
        registered_at,
        last_renewed_at,
        expires_at,
        ..
    } = refreshed.registration
    else {
        unreachable!()
    };
    assert_eq!(registered_at, first_at);
    assert!(last_renewed_at > first_renewal);
    assert_eq!((expires_at - last_renewed_at).num_seconds(), 4);
    h.stack.shutdown().await;
}

#[tokio::test]
async fn register_423_retries_with_min_expires() {
    let fake = fake_registrar(|i, req| {
        if req.expires() == Some(120) && i == 0 {
            Answer {
                code: 423,
                reason: "Interval Too Brief",
                extra: vec!["Min-Expires: 600".into()],
                ..Default::default()
            }
        } else {
            ok_answer(req.expires().unwrap_or(0))
        }
    });
    let h = harness();
    add(&h, reg_account("r3", &fake)).await;
    let s = wait_status(&h, "r3", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let RegState::Registered { granted_secs, .. } = s.registration else {
        unreachable!()
    };
    assert_eq!(granted_secs, 600);
    let expiries: Vec<_> = fake
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.authed())
        .map(|r| r.expires())
        .collect();
    assert_eq!(expiries, [Some(120), Some(600)]);
    h.stack.shutdown().await;
}

#[tokio::test]
async fn register_failure_honours_retry_after_and_recovers() {
    let fake = fake_registrar(|i, _| {
        if i == 0 {
            Answer {
                code: 503,
                reason: "Service Unavailable",
                extra: vec!["Retry-After: 2".into()],
                ..Default::default()
            }
        } else {
            ok_answer(60)
        }
    });
    let mut h = harness();
    add(&h, reg_account("r4", &fake)).await;
    let t0 = std::time::Instant::now();
    let s = wait_status(&h, "r4", "failed", |s| {
        matches!(s.registration, RegState::Failed { .. })
    })
    .await;
    let RegState::Failed {
        code,
        ref reason,
        retry_at,
    } = s.registration
    else {
        unreachable!()
    };
    assert_eq!((code, reason.as_str()), (503, "Service Unavailable"));
    let wait = (retry_at - chrono::Utc::now()).num_milliseconds();
    assert!(
        (500..=2100).contains(&wait),
        "retry_at is ~Retry-After away, got {wait} ms"
    );
    wait_status(&h, "r4", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    assert!(
        t0.elapsed() >= Duration::from_millis(1800),
        "not retried before Retry-After"
    );
    let ev = next_event(&mut h, |e| {
        matches!(
            e,
            SipEvent::Registration {
                state: RegState::Failed { .. },
                ..
            }
        )
    })
    .await;
    assert!(matches!(ev, SipEvent::Registration { .. }));
    h.stack.shutdown().await;
}

#[tokio::test]
async fn register_auth_rejected_is_failed_with_backoff() {
    let fake = fake_registrar(|_, _| Answer {
        code: 403,
        reason: "Forbidden",
        ..Default::default()
    });
    let h = harness();
    add(&h, reg_account("r5", &fake)).await;
    let s = wait_status(&h, "r5", "failed", |s| {
        matches!(s.registration, RegState::Failed { .. })
    })
    .await;
    let RegState::Failed { code, retry_at, .. } = s.registration else {
        unreachable!()
    };
    assert_eq!(code, 403);
    let wait = (retry_at - chrono::Utc::now()).num_milliseconds();
    // 5 s with +-20 % jitter (4000..=6000 ms), minus the time already spent.
    assert!(
        (3000..=6100).contains(&wait),
        "first back-off is 5 s +-20 %, got {wait} ms"
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn register_unreachable_registrar_reports_timeout_or_error() {
    // Nothing listens: ICMP unreachable on loopback surfaces as a send/receive error or a
    // timeout; either way the account is Failed and retrying, never stuck.
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let h = harness();
    let mut cfg = reg_account(
        "r6",
        &Fake {
            addr: format!("127.0.0.1:{port}").parse().unwrap(),
            seen: Default::default(),
            stop: Default::default(),
        },
    );
    cfg.id = "r6".into();
    add(&h, cfg).await;
    let mut rx = h.stack.watch_status("r6").unwrap();
    let res = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if matches!(rx.borrow_and_update().registration, RegState::Failed { .. }) {
                return;
            }
            rx.changed().await.unwrap();
        }
    })
    .await;
    assert!(
        res.is_ok(),
        "expected Failed within 45 s: {:?}",
        h.stack.account_status("r6")
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn contact_is_rewritten_once_to_what_the_registrar_saw() {
    // The registrar claims it sees us at 203.0.113.9:40000 (NAT).
    let fake = fake_registrar(|_, _| Answer {
        seen: Some(("203.0.113.9".into(), 40000)),
        ..ok_answer(60)
    });
    let h = harness();
    add(&h, reg_account("r7", &fake)).await;
    let s = wait_status(&h, "r7", "registered with rewritten contact", |s| {
        matches!(s.registration, RegState::Registered { .. })
            && s.contact
                .as_deref()
                .is_some_and(|c| c.contains("203.0.113.9:40000"))
    })
    .await;
    assert_eq!(s.public_addr.as_deref(), Some("203.0.113.9:40000"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let authed: Vec<_> = fake
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.authed())
        .map(|r| r.contact_uri())
        .collect();
    assert_eq!(
        authed.len(),
        2,
        "original then exactly one rewrite: {authed:?}"
    );
    assert!(authed[0].contains("127.0.0.1:") && !authed[0].contains("40000"));
    assert!(authed[1].contains("203.0.113.9:40000"), "{authed:?}");
    h.stack.shutdown().await;
}

#[tokio::test]
async fn contact_rewrite_can_be_disabled() {
    let fake = fake_registrar(|_, _| Answer {
        seen: Some(("203.0.113.9".into(), 40000)),
        ..ok_answer(60)
    });
    let h = harness();
    let mut cfg = reg_account("no-rewrite", &fake);
    cfg.rewrite_contact = false;
    add(&h, cfg).await;
    let status = wait_status(
        &h,
        "no-rewrite",
        "registered without Contact rewrite",
        |s| matches!(s.registration, RegState::Registered { .. }),
    )
    .await;
    assert!(
        status
            .contact
            .as_deref()
            .unwrap()
            .starts_with("sip:no-rewrite@127.0.0.1:")
    );
    assert_eq!(status.public_addr.as_deref(), Some("203.0.113.9:40000"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let contacts: Vec<_> = fake
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.authed())
        .map(|r| r.contact_uri())
        .collect();
    assert_eq!(contacts, vec![status.contact.unwrap()]);
    h.stack.shutdown().await;
}

#[tokio::test]
async fn contact_rewrite_reload_switches_contacts_without_rebuilding() {
    let fake = fake_registrar(|_, _| Answer {
        seen: Some(("203.0.113.9".into(), 40000)),
        ..ok_answer(60)
    });
    let h = harness();
    let mut cfg = reg_account("rewrite-toggle", &fake);
    add(&h, cfg.clone()).await;
    let original = wait_status(
        &h,
        "rewrite-toggle",
        "registered with public Contact",
        |s| {
            matches!(s.registration, RegState::Registered { .. })
                && s.contact
                    .as_deref()
                    .is_some_and(|c| c.contains("203.0.113.9:40000"))
        },
    )
    .await;

    cfg.rewrite_contact = false;
    let disabled = h.stack.update_account(cfg.clone()).await;
    assert_eq!(disabled.unwrap(), UpdateOutcome::Reregistered);
    let private = wait_status(
        &h,
        "rewrite-toggle",
        "returned to configured Contact",
        |s| {
            matches!(s.registration, RegState::Registered { .. })
                && s.contact
                    .as_deref()
                    .is_some_and(|c| c.starts_with("sip:rewrite-toggle@127.0.0.1:"))
        },
    )
    .await;
    assert_eq!(private.local_addr, original.local_addr);

    cfg.rewrite_contact = true;
    let enabled = h.stack.update_account(cfg).await;
    assert_eq!(enabled.unwrap(), UpdateOutcome::Reregistered);
    let public = wait_status(&h, "rewrite-toggle", "Contact rewrite enabled again", |s| {
        matches!(s.registration, RegState::Registered { .. })
            && s.contact
                .as_deref()
                .is_some_and(|c| c.contains("203.0.113.9:40000"))
    })
    .await;
    assert_eq!(public.local_addr, original.local_addr);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let contacts: Vec<_> = fake
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.authed())
        .map(|r| r.contact_uri())
        .collect();
    assert_eq!(
        contacts.len(),
        5,
        "initial rewrite, disable, and re-enable: {contacts:?}"
    );
    assert_eq!(contacts[0], contacts[2]);
    assert_eq!(contacts[2], contacts[3]);
    assert_eq!(contacts[1], contacts[4]);
    assert_eq!(contacts[2], private.contact.unwrap());
    assert_eq!(contacts[4], public.contact.unwrap());
    h.stack.shutdown().await;
}

#[tokio::test]
async fn public_address_overrides_and_disables_rewrite() {
    let fake = fake_registrar(|_, _| Answer {
        seen: Some(("203.0.113.9".into(), 40000)),
        ..ok_answer(60)
    });
    let h = harness();
    let mut cfg = reg_account("r8", &fake);
    cfg.public_address = "198.51.100.7".into();
    add(&h, cfg).await;
    let s = wait_status(&h, "r8", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    assert!(
        s.contact
            .as_deref()
            .unwrap()
            .starts_with("sip:r8@198.51.100.7:"),
        "{:?}",
        s.contact
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.authed())
            .count(),
        1,
        "no rewrite with public_address"
    );
    h.stack.shutdown().await;
}

#[tokio::test]
async fn config_update_reregisters_with_new_expires() {
    let fake = fake_registrar(|_, req| ok_answer(req.expires().unwrap_or(30)));
    let h = harness();
    let cfg = reg_account("r9", &fake);
    add(&h, cfg.clone()).await;
    wait_status(&h, "r9", "registered", |s| {
        matches!(s.registration, RegState::Registered { .. })
    })
    .await;
    let mut c = cfg;
    c.expires = 200;
    assert_eq!(
        h.stack.update_account(c).await.unwrap(),
        UpdateOutcome::Reregistered
    );
    wait_status(&h, "r9", "granted 200", |s| {
        matches!(
            s.registration,
            RegState::Registered {
                granted_secs: 200,
                ..
            }
        )
    })
    .await;
    h.stack.shutdown().await;
}
