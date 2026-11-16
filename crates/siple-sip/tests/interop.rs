//! Interop against a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"). Gated: set `SIPLE_INTEROP=1`.
//! Credentials come from `tests/interop/pbx.env` at the repo root. Uses users 1003 (UDP), 1004
//! (TCP) and 1005 (TLS, `tls_verify = false`) only. Registration state on the PBX is verified
//! through `ssh $PBX_SSH ... fs_cli` when that works non-interactively; otherwise that part is
//! skipped (with a printed note) and only the client-side state is asserted.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Secret, Transport};
use siple_sip::{
    AccountStatus, RegState, SipEvent, SipStack, SipStackOptions, WireDirection, WireEvent,
    WireSink,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// The tests share PBX users 1003-1005, so they run one at a time.
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

type WireRecord = (String, WireDirection, String, Vec<u8>);

#[derive(Default)]
struct Capture(Mutex<Vec<WireRecord>>);

impl WireSink for Capture {
    fn on_wire(&self, account: &str, ev: &WireEvent<'_>) {
        self.0.lock().unwrap().push((
            account.to_string(),
            ev.direction,
            ev.transport.to_string(),
            ev.bytes.to_vec(),
        ));
    }
}

struct Rig {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    wire: Arc<Capture>,
    env: HashMap<String, String>,
}

fn rig() -> Rig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (tx, events) = unbounded_channel();
    let wire = Arc::new(Capture::default());
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-interop/0".into(),
        events: tx,
        wire_sink: wire.clone(),
    });
    Rig {
        stack,
        events,
        wire,
        env: pbx_env(),
    }
}

fn account(env: &HashMap<String, String>, user: &str, transport: Transport) -> AccountConfig {
    AccountConfig {
        id: format!("it-{user}"),
        username: user.into(),
        password: Secret::new(env["PBX_PASSWORD"].clone()),
        domain: env["PBX_DOMAIN"].clone(),
        registrar: env["PBX_HOST"].clone(),
        transport,
        tls_verify: false,
        expires: 120,
        keepalive_secs: 0,
        sip_trace: true,
        ..Default::default()
    }
}

async fn wait(
    stack: &SipStack,
    id: &str,
    what: &str,
    pred: impl Fn(&AccountStatus) -> bool,
) -> AccountStatus {
    let mut rx = stack.watch_status(id).unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
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
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {what}: {:?}",
            stack.account_status(id)
        )
    })
}

fn registered(s: &AccountStatus) -> bool {
    matches!(s.registration, RegState::Registered { .. })
}

/// `sofia status profile internal reg` via ssh; `None` when ssh is not usable non-interactively.
fn fs_reg() -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            pbx_env().get("PBX_SSH").map_or("", String::as_str),
            "docker exec siple-fs fs_cli -x 'sofia status profile internal reg'",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The registration block of `user` in the fs_cli output, if any (blocks start at `Call-ID:`).
fn fs_block(dump: &str, user: &str) -> Option<String> {
    dump.split("Call-ID:")
        .skip(1)
        .find(|b| {
            b.lines().any(|l| {
                l.split_once(':')
                    .is_some_and(|(k, v)| k.trim() == "Auth-User" && v.trim() == user)
            })
        })
        .map(|b| format!("Call-ID:{b}"))
}

fn port_of(contact: &str) -> String {
    contact
        .split('@')
        .nth(1)
        .and_then(|h| h.split([';', '>']).next())
        .and_then(|h| h.rsplit(':').next())
        .unwrap_or_default()
        .to_string()
}

async fn register_one(
    rig: &mut Rig,
    user: &str,
    transport: Transport,
    fs_tag: &str,
) -> AccountStatus {
    let cfg = account(&rig.env, user, transport);
    let id = cfg.id.clone();
    rig.stack
        .add_account(
            cfg,
            &GeneralConfig {
                user_agent: "Siple-interop/0".into(),
                ..Default::default()
            },
            &MediaConfig::default(),
        )
        .await
        .unwrap();
    let s = wait(&rig.stack, &id, "registered", registered).await;
    println!("[{user} {fs_tag}] {s:?}");
    let RegState::Registered { granted_secs, .. } = s.registration else {
        unreachable!()
    };
    assert!(
        granted_secs > 0 && granted_secs <= 120,
        "granted {granted_secs}"
    );
    let contact = s.contact.clone().expect("contact");
    assert!(contact.starts_with(&format!("sip:{user}@")), "{contact}");
    match transport {
        Transport::Udp => assert!(!contact.to_ascii_lowercase().contains("transport=")),
        Transport::Tcp => assert!(
            contact.to_ascii_lowercase().ends_with("transport=tcp"),
            "{contact}"
        ),
        Transport::Tls => assert!(
            contact.to_ascii_lowercase().ends_with("transport=tls") && contact.starts_with("sip:"),
            "{contact}"
        ),
        Transport::Auto => unreachable!("interop accounts use a fixed transport"),
    }
    if let Some(dump) = fs_reg() {
        let block = fs_block(&dump, user)
            .unwrap_or_else(|| panic!("FS has no registration for {user}:\n{dump}"));
        println!("[{user}] FS says:\n{block}");
        assert!(block.contains(&format!("Registered({fs_tag})")), "{block}");
        assert!(
            block.lines().any(|l| l
                .split_once(':')
                .is_some_and(|(k, v)| k.trim() == "Port" && v.trim() == port_of(&contact))),
            "FS sees our contact port {}:\n{block}",
            port_of(&contact)
        );
    } else {
        println!("[{user}] ssh/fs_cli not available non-interactively; PBX-side check skipped");
    }
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_udp_tcp_tls_then_unregister() {
    if !enabled() {
        eprintln!("skipped: set SIPLE_INTEROP=1");
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut rig = rig();
    let udp = register_one(&mut rig, "1003", Transport::Udp, "UDP").await;
    let tcp = register_one(&mut rig, "1004", Transport::Tcp, "TCP").await;
    let tls = register_one(&mut rig, "1005", Transport::Tls, "TLS").await;

    // Registration events were emitted for all three.
    let mut seen = std::collections::HashSet::new();
    while let Ok(e) = rig.events.try_recv() {
        if let SipEvent::Registration {
            account,
            state: RegState::Registered { .. },
        } = e
        {
            seen.insert(account);
        }
    }
    assert_eq!(seen.len(), 3, "{seen:?}");

    // Wire trace saw digest auth round trips on each account.
    {
        let wire = rig.wire.0.lock().unwrap();
        for acct in ["it-1003", "it-1004", "it-1005"] {
            let texts: Vec<String> = wire
                .iter()
                .filter(|w| w.0 == acct)
                .map(|w| String::from_utf8_lossy(&w.3).into_owned())
                .collect();
            assert!(texts.iter().any(|t| t.starts_with("REGISTER ")), "{acct}");
            assert!(
                texts.iter().any(|t| t.starts_with("SIP/2.0 401")),
                "{acct}: digest challenge seen"
            );
            assert!(texts.iter().any(|t| t.starts_with("SIP/2.0 200")), "{acct}");
        }
    }

    // OPTIONS to the PBX (FreeSWITCH answers 200).
    let r = rig
        .stack
        .send_options("it-1003", &format!("sip:{}", rig.env["PBX_HOST"]))
        .await;
    println!("OPTIONS -> {r:?}");
    assert!(r.is_ok_and(|r| r.code == 200));

    // UDP had no NAT: no rewrite happened, so there is exactly one authenticated REGISTER.
    assert_eq!(
        udp.public_addr.as_deref(),
        udp.local_addr.as_deref(),
        "no NAT on the path to the test PBX"
    );
    let _ = (tcp, tls);

    // Unregister each; the binding disappears on the PBX.
    for id in ["it-1003", "it-1004", "it-1005"] {
        rig.stack.unregister(id).unwrap();
        wait(&rig.stack, id, "unregistered", |s| {
            s.registration == RegState::Unregistered
        })
        .await;
    }
    if let Some(dump) = fs_reg() {
        for u in ["1003", "1004", "1005"] {
            assert!(fs_block(&dump, u).is_none(), "FS still has {u}:\n{dump}");
        }
        println!("FS bindings for 1003-1005 removed after unregister");
    }
    rig.stack.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_flow_loss_redials_and_reregisters_with_new_contact_port() {
    if !enabled() {
        eprintln!("skipped: set SIPLE_INTEROP=1");
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut rig = rig();
    let first = register_one(&mut rig, "1004", Transport::Tcp, "TCP").await;
    let port1 = port_of(first.contact.as_deref().unwrap());

    assert!(
        rig.stack.simulate_flow_loss("it-1004").await.unwrap(),
        "a connection was closed"
    );
    // The close is reported, then the account re-registers over a new connection.
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(SipEvent::TransportClosed { account, remote }) = rig.events.recv().await
                && account == "it-1004"
            {
                return remote;
            }
        }
    })
    .await
    .expect("TransportClosed event");
    println!("closed flow to {closed}");
    let second = wait(&rig.stack, "it-1004", "re-registered on a new port", |s| {
        registered(s) && s.contact.as_deref().is_some_and(|c| port_of(c) != port1)
    })
    .await;
    let port2 = port_of(second.contact.as_deref().unwrap());
    println!("contact port {port1} -> {port2}");
    if let Some(dump) = fs_reg() {
        let block = fs_block(&dump, "1004").expect("FS binding");
        assert!(
            block.contains(&port2),
            "FS has the new port {port2}:\n{block}"
        );
    }
    rig.stack.unregister("it-1004").unwrap();
    wait(&rig.stack, "it-1004", "unregistered", |s| {
        s.registration == RegState::Unregistered
    })
    .await;
    rig.stack.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tcp_keepalive_crlf_is_sent_and_ponged() {
    if !enabled() {
        eprintln!("skipped: set SIPLE_INTEROP=1");
        return;
    }
    let _serial = SERIAL.lock().await;
    let mut rig = rig();
    let mut cfg = account(&rig.env, "1003", Transport::Tcp);
    cfg.id = "it-ka".into();
    cfg.keepalive_secs = 1;
    rig.stack
        .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
        .await
        .unwrap();
    wait(&rig.stack, "it-ka", "registered", registered).await;
    let t0 = Instant::now();
    loop {
        {
            let wire = rig.wire.0.lock().unwrap();
            let sent = wire
                .iter()
                .filter(|w| w.0 == "it-ka" && w.1 == WireDirection::Send && w.3 == b"\r\n\r\n")
                .count();
            let pong = wire
                .iter()
                .filter(|w| w.0 == "it-ka" && w.1 == WireDirection::Recv && w.3 == b"\r\n")
                .count();
            if sent >= 2 && pong >= 1 {
                println!("keep-alives sent={sent} pongs={pong}");
                break;
            }
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "no keep-alive exchange"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    rig.stack.shutdown().await;
    let _ = &mut rig.events;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_verify_rejects_the_self_signed_pbx_certificate() {
    if !enabled() {
        eprintln!("skipped: set SIPLE_INTEROP=1");
        return;
    }
    let _serial = SERIAL.lock().await;
    let rig = rig();
    let mut cfg = account(&rig.env, "1005", Transport::Tls);
    cfg.tls_verify = true;
    rig.stack
        .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
        .await
        .unwrap();
    let s = wait(&rig.stack, "it-1005", "failed", |s| {
        matches!(s.registration, RegState::Failed { .. })
    })
    .await;
    let RegState::Failed { code, reason, .. } = s.registration else {
        unreachable!()
    };
    println!("tls_verify=true -> Failed {{ code: {code}, reason: {reason:?} }}");
    assert_eq!(code, 0, "local TLS failure, not a SIP status");
    assert!(
        reason.to_ascii_lowercase().contains("cert")
            || reason.to_ascii_lowercase().contains("tls")
            || reason.to_ascii_lowercase().contains("handshake"),
        "{reason}"
    );
    rig.stack.shutdown().await;
}
