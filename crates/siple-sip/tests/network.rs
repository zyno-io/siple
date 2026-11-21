//! Network-change resilience: fake `NetMonitor` / `NetProbe` against a scripted
//! registrar on loopback. Covers an injected change while registered, "no route" ->
//! `WaitingForNetwork` -> route back, a retry out of a long back-off, dead-flow detection with
//! OPTIONS keep-alives, and a changed next hop. The `moves_*` / `move_*` tests at the end cover
//! live calls: an established call is moved onto the new local address
//! with a re-INVITE (new Contact and `c=`), keeps its hold, survives a 491, and ends with
//! `network_lost` when the peer does not follow or `move_calls` is off.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use siple_config::{
    AccountConfig, GeneralConfig, KeepaliveMethod, MediaConfig, NetworkConfig, Secret, Transport,
};
use siple_sip::net::{InterfaceInfo, NetMonitor, NetProbe, NetReason, NetSetup};
use siple_sip::{
    AccountStatus, AnswerOptions, CallInfo, CallState, EndCause, NullMediaProvider, NullWireSink,
    RegState, SipError, SipEvent, SipStack, SipStackOptions,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

mod common;

// --- fakes ---------------------------------------------------------------------------------------------

/// Route + DNS that tests flip at will. `127.0.0.1` literals resolve to themselves.
struct FakeProbe {
    route: Mutex<Result<IpAddr, io::ErrorKind>>,
    hosts: Mutex<HashMap<String, SocketAddr>>,
    /// Added to every name lookup (to hold a probe open while something else happens).
    delay: Mutex<Duration>,
    /// Interfaces the fingerprint is built from (`judged()` is exercised).
    ifaces: Mutex<Vec<InterfaceInfo>>,
}

impl FakeProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            route: Mutex::new(Ok("127.0.0.1".parse().unwrap())),
            hosts: Mutex::default(),
            delay: Mutex::default(),
            ifaces: Mutex::new(vec![InterfaceInfo {
                name: "en0".into(),
                up: true,
                addrs: vec!["10.0.0.1".parse().unwrap()],
                gateways: vec![],
            }]),
        })
    }
    fn no_route(&self) {
        *self.route.lock().unwrap() = Err(io::ErrorKind::NetworkUnreachable);
    }
    fn route_back(&self) {
        *self.route.lock().unwrap() = Ok("127.0.0.1".parse().unwrap());
    }
    /// A relevant address appears (what a real network change looks like to the fingerprint).
    fn bump(&self) {
        let mut l = self.ifaces.lock().unwrap();
        let n = l[0].addrs.len();
        l[0].addrs.push(format!("10.0.1.{n}").parse().unwrap());
    }
    fn set_v6_prefix(&self, p: &str) {
        let mut l = self.ifaces.lock().unwrap();
        l[0].addrs.retain(|a| a.is_ipv4());
        l[0].addrs.push(p.parse().unwrap());
    }
    /// The route now leaves through `ip` (a local address change).
    fn set_local(&self, ip: IpAddr) {
        *self.route.lock().unwrap() = Ok(ip);
    }
    fn set_delay(&self, d: Duration) {
        *self.delay.lock().unwrap() = d;
    }
    fn map(&self, host: &str, to: SocketAddr) {
        self.hosts.lock().unwrap().insert(host.into(), to);
    }
}

#[async_trait::async_trait]
impl NetProbe for FakeProbe {
    fn local_ip_toward(&self, _: SocketAddr) -> io::Result<IpAddr> {
        (*self.route.lock().unwrap()).map_err(io::Error::from)
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
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        // Offline, names do not resolve either.
        if self.route.lock().unwrap().is_err() {
            return Err(SipError::Dns(format!("{host}: network is down")));
        }
        if let Some(a) = self.hosts.lock().unwrap().get(host) {
            return Ok(vec![*a]);
        }
        host.parse::<IpAddr>()
            .map(|ip| vec![SocketAddr::new(ip, port.unwrap_or(5060))])
            .map_err(|_| SipError::Dns(format!("{host}: nxdomain")))
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

// --- scripted UDP registrar ------------------------------------------------------------------------------

struct Script {
    code: u16,
    reason: &'static str,
    extra: Vec<String>,
}

fn ok() -> Script {
    Script {
        code: 200,
        reason: "OK",
        extra: vec![],
    }
}

struct Registrar {
    addr: SocketAddr,
    registers: Arc<Mutex<Vec<Instant>>>,
    options: Arc<AtomicUsize>,
    answer_options: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// TCP connections accepted (always 0 for the UDP registrar).
    accepts: Arc<AtomicUsize>,
    /// TCP: ordinal (1-based) of the accepted connection whose REGISTERs go unanswered.
    mute_conn: Arc<AtomicUsize>,
    /// TCP: parameters appended to the Contact of a 200 to REGISTER (before `;expires=`).
    contact_params: Arc<Mutex<String>>,
}

impl Registrar {
    fn count(&self) -> usize {
        self.registers.lock().unwrap().len()
    }
}

impl Drop for Registrar {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// `decide(n)` answers the n-th REGISTER (0-based). OPTIONS are counted and answered only while
/// `answer_options` is set.
fn registrar(decide: impl Fn(usize) -> Script + Send + 'static) -> Registrar {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let addr = sock.local_addr().unwrap();
    let r = Registrar {
        addr,
        registers: Arc::default(),
        options: Arc::default(),
        answer_options: Arc::default(),
        stop: Arc::default(),
        accepts: Arc::default(),
        mute_conn: Arc::default(),
        contact_params: Arc::default(),
    };
    let (regs, opts, ans, stop) = (
        r.registers.clone(),
        r.options.clone(),
        r.answer_options.clone(),
        r.stop.clone(),
    );
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while !stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            let script = if text.starts_with("REGISTER ") {
                let mut g = regs.lock().unwrap();
                let i = g.len();
                g.push(Instant::now());
                drop(g);
                decide(i)
            } else if text.starts_with("OPTIONS ") {
                opts.fetch_add(1, Ordering::SeqCst);
                if !ans.load(Ordering::SeqCst) {
                    continue;
                }
                ok()
            } else {
                continue;
            };
            let contact = text
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("contact:"))
                .map(|l| l.split_once(':').unwrap().1.trim().to_string());
            let _ = sock.send_to(response(&text, &script, contact, from).as_bytes(), from);
        }
    });
    r
}

fn response(req: &str, s: &Script, contact: Option<String>, from: SocketAddr) -> String {
    let mut lines = vec![format!("SIP/2.0 {} {}", s.code, s.reason)];
    for l in req.lines() {
        let low = l.to_ascii_lowercase();
        if low.starts_with("via:") {
            let base: String = l
                .split(';')
                .filter(|p| {
                    !p.trim_start().starts_with("rport") && !p.trim_start().starts_with("received")
                })
                .collect::<Vec<_>>()
                .join(";");
            lines.push(format!(
                "{base};received={};rport={}",
                from.ip(),
                from.port()
            ));
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
    lines.extend(s.extra.iter().cloned());
    if s.code == 200
        && req.starts_with("REGISTER")
        && let Some(c) = contact
    {
        lines.push(format!("Contact: {c};expires=120"));
    }
    lines.push("Content-Length: 0".into());
    format!("{}\r\n\r\n", lines.join("\r\n"))
}

// --- harness ---------------------------------------------------------------------------------------------

struct Rig {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    /// Events received while waiting for something else (matched first by later waits).
    buf: Vec<SipEvent>,
    probe: Arc<FakeProbe>,
    mon: Arc<FakeMonitor>,
}

fn rig() -> Rig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (tx, rx) = unbounded_channel();
    let probe = FakeProbe::new();
    let mon = Arc::new(FakeMonitor::default());
    let stack = SipStack::with_net(
        SipStackOptions {
            user_agent_default: "Siple-test/1".into(),
            events: tx,
            wire_sink: Arc::new(NullWireSink),
        },
        NetSetup {
            config: Some(NetworkConfig {
                watch: true,
                probe_secs: 0,
                debounce_ms: 100,
                move_calls: true,
            }),
            monitor: Some(mon.clone()),
            probe: Some(probe.clone()),
            clock: None,
        },
    );
    Rig {
        stack,
        events: rx,
        buf: Vec::new(),
        probe,
        mon,
    }
}

impl Rig {
    /// A real change: a relevant address appears, then the OS says so.
    fn change(&self, what: &str) {
        self.probe.bump();
        self.mon.fire(what);
    }
}

fn account(id: &str, reg: &Registrar) -> AccountConfig {
    AccountConfig {
        id: id.into(),
        username: id.into(),
        password: Secret::new("secret"),
        domain: "fake.test".into(),
        registrar: reg.addr.to_string(),
        transport: Transport::Udp,
        // No bind_address: the local address comes from the (fake) route probe.
        expires: 120,
        keepalive_secs: 0,
        ..Default::default()
    }
}

async fn add(r: &Rig, cfg: AccountConfig) {
    r.stack
        .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
        .await
        .unwrap();
}

async fn wait_state(r: &Rig, id: &str, what: &str, pred: impl Fn(&RegState) -> bool) {
    let mut rx = r.stack.watch_status(id).unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let s: AccountStatus = rx.borrow_and_update().clone();
            if pred(&s.registration) {
                return;
            }
            rx.changed().await.unwrap();
        }
    })
    .await;
    assert!(
        res.is_ok(),
        "timed out waiting for {what}; status = {:?}",
        r.stack.account_status(id).map(|s| s.registration)
    );
}

fn registered(s: &RegState) -> bool {
    matches!(s, RegState::Registered { .. })
}

/// Waits (polling) for the registrar to have seen at least `n` REGISTERs.
async fn registers_reach(reg: &Registrar, n: usize, within: Duration) -> Duration {
    let t0 = Instant::now();
    while reg.count() < n {
        assert!(
            t0.elapsed() < within,
            "expected {n} REGISTERs within {within:?}, saw {}",
            reg.count()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    t0.elapsed()
}

fn drain(r: &mut Rig) -> Vec<SipEvent> {
    std::iter::from_fn(|| r.events.try_recv().ok()).collect()
}

// --- (i) an injected change while Registered re-registers within 3 s ---------------------------------

#[tokio::test]
async fn os_event_while_registered_sends_a_new_register_within_3s() {
    let reg = registrar(|_| ok());
    let mut r = rig();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    assert_eq!(reg.count(), 1);
    drain(&mut r);

    r.change("addr");
    let took = registers_reach(&reg, 2, Duration::from_secs(3)).await;
    eprintln!("re-REGISTER after {took:?}");
    wait_state(&r, "a", "registered again", registered).await;

    // The change and the account's reaction are announced.
    let evs = drain(&mut r);
    assert!(
        evs.iter().any(|e| matches!(e,
            SipEvent::NetworkChanged { generation: 1, reasons, .. } if reasons == &["os:addr"])),
        "{evs:?}"
    );
    assert!(
        evs.iter().any(|e| matches!(e,
            SipEvent::AccountReconnecting { account, .. } if account == "a")),
        "{evs:?}"
    );
    assert_eq!(r.stack.network_generation(), 1);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn a_burst_of_os_events_causes_one_register() {
    let reg = registrar(|_| ok());
    let r = rig();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    r.probe.bump();
    for _ in 0..5 {
        r.mon.fire("addr");
    }
    registers_reach(&reg, 2, Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(reg.count(), 2, "coalesced into one re-register");
    assert_eq!(r.stack.network_generation(), 1);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn a_changed_next_hop_rebuilds_and_registers_with_the_new_registrar() {
    let old = registrar(|_| ok());
    let new = registrar(|_| ok());
    let r = rig();
    r.probe.map("reg.test", old.addr);
    let mut cfg = account("a", &old);
    cfg.registrar = "reg.test:5060".into();
    add(&r, cfg).await;
    wait_state(&r, "a", "registered", registered).await;
    assert_eq!((old.count(), new.count()), (1, 0));

    r.probe.map("reg.test", new.addr); // DNS moved (or a VPN changed the answer)
    r.mon.fire("dns");
    registers_reach(&new, 1, Duration::from_secs(3)).await;
    wait_state(&r, "a", "registered on the new registrar", registered).await;
    assert_eq!(old.count(), 1, "nothing more sent to the old registrar");
    r.stack.shutdown().await;
}

// --- (ii) no route -> WaitingForNetwork, nothing sent; route back -> REGISTER at once --------------------

#[tokio::test]
async fn no_route_waits_without_registering_then_route_back_registers_at_once() {
    let reg = registrar(|_| ok());
    let r = rig();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    let before = reg.count();

    r.probe.no_route();
    r.mon.fire("route");
    wait_state(&r, "a", "waiting for the network", |s| {
        matches!(s, RegState::WaitingForNetwork)
    })
    .await;
    // Suspended: no REGISTER while there is no route, even across further signals.
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(reg.count(), before, "no REGISTER while waiting");
    assert!(matches!(
        r.stack.account_status("a").unwrap().registration,
        RegState::WaitingForNetwork
    ));

    r.probe.route_back();
    r.mon.fire("route");
    let took = registers_reach(&reg, before + 1, Duration::from_secs(3)).await;
    eprintln!("REGISTER {took:?} after the route returned");
    wait_state(&r, "a", "registered", registered).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn starting_without_a_route_waits_and_a_probe_resumes() {
    let reg = registrar(|_| ok());
    let r = rig();
    r.probe.no_route();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "waiting for the network", |s| {
        matches!(s, RegState::WaitingForNetwork)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(reg.count(), 0);

    // Explicit register_now while waiting probes first; still no route -> still waiting.
    r.stack.register_now("a").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(reg.count(), 0);
    assert!(matches!(
        r.stack.account_status("a").unwrap().registration,
        RegState::WaitingForNetwork
    ));

    r.probe.route_back();
    r.stack.reregister("a").unwrap(); // a successful probe resumes without any OS event
    wait_state(&r, "a", "registered", registered).await;
    assert_eq!(reg.count(), 1);
    r.stack.shutdown().await;
}

// --- (iii) Failed in a back-off + a network signal -> immediate retry --------------------------------------

#[tokio::test]
async fn network_signal_cuts_our_own_backoff_short() {
    // 503 without Retry-After: the back-off is ours (about 5 s) and a signal may skip it.
    let reg = registrar(|i| {
        if i == 0 {
            Script {
                code: 503,
                reason: "Service Unavailable",
                extra: vec![],
            }
        } else {
            ok()
        }
    });
    let r = rig();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "failed", |s| matches!(s, RegState::Failed { .. })).await;
    let RegState::Failed { retry_at, .. } = r.stack.account_status("a").unwrap().registration
    else {
        unreachable!()
    };
    assert!(
        (retry_at - chrono::Utc::now()).num_milliseconds() > 3000,
        "a back-off is pending"
    );

    r.change("addr");
    let took = registers_reach(&reg, 2, Duration::from_secs(3)).await;
    eprintln!("retried {took:?} after the signal");
    wait_state(&r, "a", "registered", registered).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn server_retry_after_wins_over_a_plain_signal_but_not_over_a_request() {
    let reg = registrar(|i| {
        if i == 0 {
            Script {
                code: 503,
                reason: "Service Unavailable",
                extra: vec!["Retry-After: 120".into()],
            }
        } else {
            ok()
        }
    });
    let r = rig();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "failed", |s| matches!(s, RegState::Failed { .. })).await;

    // Same-path signals (OS event, network.refresh): the server said wait, so wait.
    r.change("addr");
    r.stack.network_refresh().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(reg.count(), 1, "no REGISTER before Retry-After expires");
    assert!(matches!(
        r.stack.account_status("a").unwrap().registration,
        RegState::Failed { code: 503, .. }
    ));

    // An explicit account.reregister overrides it.
    r.stack.reregister("a").unwrap();
    registers_reach(&reg, 2, Duration::from_secs(3)).await;
    wait_state(&r, "a", "registered", registered).await;
    r.stack.shutdown().await;
}

// --- (iv) OPTIONS keep-alives unanswered -> dead flow -> re-register ---------------------------------------

fn options_account(id: &str, reg: &Registrar, threshold: u32) -> AccountConfig {
    AccountConfig {
        keepalive_secs: 1,
        keepalive_method: KeepaliveMethod::Options,
        keepalive_fail_threshold: threshold,
        ..account(id, reg)
    }
}

#[tokio::test]
async fn unanswered_options_keepalives_mark_the_flow_dead_and_re_register() {
    let reg = registrar(|_| ok()); // never answers OPTIONS
    let mut r = rig();
    add(&r, options_account("a", &reg, 2)).await;
    wait_state(&r, "a", "registered", registered).await;
    drain(&mut r);
    assert_eq!(reg.count(), 1);

    // Two keep-alives (1 s apart, 2 s patience each) go unanswered -> re-REGISTER.
    let took = registers_reach(&reg, 2, Duration::from_secs(12)).await;
    eprintln!("flow declared dead and re-registered after {took:?}");
    assert!(
        reg.options.load(Ordering::SeqCst) >= 2,
        "the threshold needs at least two OPTIONS"
    );
    wait_state(&r, "a", "registered again", registered).await;
    let evs = drain(&mut r);
    assert!(
        evs.iter().any(|e| matches!(e,
            SipEvent::AccountReconnecting { reason, .. } if reason == "keepalive_timeout")),
        "{evs:?}"
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn answered_options_keepalives_keep_the_flow() {
    let reg = registrar(|_| ok());
    reg.answer_options.store(true, Ordering::SeqCst);
    let mut r = rig();
    add(&r, options_account("a", &reg, 2)).await;
    wait_state(&r, "a", "registered", registered).await;
    drain(&mut r);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        reg.options.load(Ordering::SeqCst) >= 4,
        "keep-alives are flowing"
    );
    assert_eq!(reg.count(), 1, "alive flow: no re-REGISTER");
    assert!(
        drain(&mut r)
            .iter()
            .all(|e| !matches!(e, SipEvent::AccountReconnecting { .. }))
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn threshold_zero_disables_dead_flow_detection() {
    let reg = registrar(|_| ok()); // never answers OPTIONS
    let r = rig();
    add(&r, options_account("a", &reg, 0)).await;
    wait_state(&r, "a", "registered", registered).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(reg.options.load(Ordering::SeqCst) >= 3);
    assert_eq!(reg.count(), 1, "threshold 0 = never declared dead");
    r.stack.shutdown().await;
}

// --- API on the stack -------------------------------------------------------------------------------------------

#[tokio::test]
async fn reregister_and_refresh_trigger_a_register() {
    let reg = registrar(|_| ok());
    let r = rig();
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;

    r.stack.reregister("a").unwrap();
    registers_reach(&reg, 2, Duration::from_secs(3)).await;

    let g = r.stack.network_refresh().await;
    assert_eq!(g, 1);
    registers_reach(&reg, 3, Duration::from_secs(3)).await;
    assert_eq!(r.stack.network_refresh().await, 2);
    registers_reach(&reg, 4, Duration::from_secs(3)).await;

    assert!(r.stack.reregister("nope").is_err());
    r.stack.shutdown().await;
}

// --- M1: a probe that started before the runtime was built is dropped ---------------------------------------

#[tokio::test]
async fn a_probe_started_during_the_build_does_not_rebuild_afterwards() {
    let reg = registrar(|_| ok());
    let r = rig();
    r.probe.set_delay(Duration::from_millis(400));
    let (stack, cfg) = (r.stack.clone(), account("a", &reg));
    let adding = tokio::spawn(async move {
        stack
            .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
            .await
            .unwrap();
    });
    // While the first build is still resolving, a network signal starts a probe of its own;
    // that probe lands after the build finished.
    tokio::time::sleep(Duration::from_millis(250)).await;
    r.change("addr");
    adding.await.unwrap();
    wait_state(&r, "a", "registered", registered).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        reg.count(),
        1,
        "the stale probe (judged against 'no runtime') must not rebuild and re-register"
    );
    r.stack.shutdown().await;
}

// --- M2: offline, a hostname registrar waits instead of failing -------------------------------------------

#[tokio::test]
async fn hostname_registrar_offline_waits_for_the_network() {
    let reg = registrar(|_| ok());
    let r = rig();
    r.probe.map("reg.test", reg.addr);
    let mut cfg = account("a", &reg);
    cfg.registrar = "reg.test:5060".into();
    add(&r, cfg.clone()).await;
    wait_state(&r, "a", "registered", registered).await;

    // Going offline: DNS fails first, but it is "no route", not a DNS failure.
    r.probe.no_route();
    r.mon.fire("route");
    wait_state(&r, "a", "waiting for the network", |s| {
        matches!(s, RegState::WaitingForNetwork)
    })
    .await;
    let n = reg.count();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(reg.count(), n);
    r.probe.route_back();
    r.mon.fire("route");
    wait_state(&r, "a", "registered", registered).await;

    // Starting offline reaches the same state.
    let r2 = rig();
    r2.probe.map("reg.test", reg.addr);
    r2.probe.no_route();
    add(&r2, cfg).await;
    wait_state(&r2, "a", "waiting for the network at startup", |s| {
        matches!(s, RegState::WaitingForNetwork)
    })
    .await;
    r.stack.shutdown().await;
    r2.stack.shutdown().await;
}

// --- H1/H2: TCP -----------------------------------------------------------------------------------------

/// TCP registrar: answers REGISTER (and OPTIONS when asked) on every accepted connection.
fn tcp_registrar() -> Registrar {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let r = Registrar {
        addr: listener.local_addr().unwrap(),
        registers: Arc::default(),
        options: Arc::default(),
        answer_options: Arc::default(),
        stop: Arc::default(),
        accepts: Arc::default(),
        mute_conn: Arc::default(),
        contact_params: Arc::default(),
    };
    let (regs, opts, ans, stop, accepts, mute, params) = (
        r.registers.clone(),
        r.options.clone(),
        r.answer_options.clone(),
        r.stop.clone(),
        r.accepts.clone(),
        r.mute_conn.clone(),
        r.contact_params.clone(),
    );
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let Ok((mut conn, peer)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            let ordinal = accepts.fetch_add(1, Ordering::SeqCst) + 1;
            let (mute, params) = (mute.clone(), params.clone());
            conn.set_nonblocking(false).unwrap();
            conn.set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let (regs, opts, ans, stop) = (regs.clone(), opts.clone(), ans.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut buf: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                while !stop.load(Ordering::Relaxed) {
                    match conn.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => return,
                    }
                    loop {
                        while buf.starts_with(b"\r\n") {
                            buf.drain(..2);
                        }
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            break;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.trim()
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() < end + 4 + len {
                            break;
                        }
                        buf.drain(..end + 4 + len);
                        let script = if head.starts_with("REGISTER ") {
                            regs.lock().unwrap().push(Instant::now());
                            if mute.load(Ordering::SeqCst) == ordinal {
                                continue; // this connection went silent
                            }
                            ok()
                        } else if head.starts_with("OPTIONS ") {
                            opts.fetch_add(1, Ordering::SeqCst);
                            if !ans.load(Ordering::SeqCst) {
                                continue;
                            }
                            ok()
                        } else {
                            continue;
                        };
                        let contact = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("contact:"))
                            .map(|l| l.split_once(':').unwrap().1.trim().to_string())
                            .map(|c| format!("{c}{}", params.lock().unwrap()));
                        let _ = conn.write_all(response(&head, &script, contact, peer).as_bytes());
                    }
                }
            });
        }
    });
    r
}

fn tcp_account(id: &str, reg: &Registrar) -> AccountConfig {
    AccountConfig {
        transport: Transport::Tcp,
        ..account(id, reg)
    }
}

fn no_failures(evs: &[SipEvent]) -> bool {
    !evs.iter().any(|e| {
        matches!(
            e,
            SipEvent::Registration {
                state: RegState::Failed { .. },
                ..
            }
        )
    })
}

async fn accepts_reach(reg: &Registrar, n: usize) {
    for _ in 0..150 {
        if reg.accepts.load(Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "expected {n} TCP connections, saw {}",
        reg.accepts.load(Ordering::SeqCst)
    );
}

/// Kamailio with `received_avp` answers a REGISTER over TCP / TLS with the observed flow as a
/// quoted URI carrying its own `;transport=`. rsipstack 0.8.6 split that quoted value at its `;`,
/// refused the whole 200 OK and closed the connection, so the account never registered (fixed
/// upstream by #259, backported in vendor/rsipstack).
#[tokio::test]
async fn tcp_register_ok_with_quoted_received_contact_param_registers() {
    let reg = tcp_registrar();
    *reg.contact_params.lock().unwrap() = ";q=1;received=\"sip:127.0.0.1:47522;transport=tcp\"\
         ;+sip.instance=\"<urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee>\";reg-id=1"
        .into();
    let mut r = rig();
    add(&r, tcp_account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let evs = drain(&mut r);
    assert!(no_failures(&evs), "no Failed: {evs:?}");
    assert_eq!(
        (reg.accepts.load(Ordering::SeqCst), reg.count()),
        (1, 1),
        "one connection, one REGISTER: the 200 was accepted"
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn tcp_signal_without_calls_redials_and_registers_without_a_503() {
    let reg = tcp_registrar();
    let mut r = rig();
    add(&r, tcp_account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    assert_eq!((reg.accepts.load(Ordering::SeqCst), reg.count()), (1, 1));
    drain(&mut r);

    r.change("addr");
    registers_reach(&reg, 2, Duration::from_secs(3)).await;
    wait_state(&r, "a", "registered again", registered).await;
    assert_eq!(
        reg.accepts.load(Ordering::SeqCst),
        2,
        "a fresh connection, not the half-closed one"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let evs = drain(&mut r);
    assert!(no_failures(&evs), "no local 503 / Failed: {evs:?}");
    assert_eq!(reg.count(), 2, "exactly one REGISTER for the signal");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn tcp_signal_with_a_live_call_keeps_the_connection_and_redials_afterwards() {
    let reg = tcp_registrar();
    let mut r = rig();
    add(&r, tcp_account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    r.stack.set_active_calls("a", 1).unwrap(); // a call rides on the registrar connection
    drain(&mut r);

    r.change("addr");
    registers_reach(&reg, 2, Duration::from_secs(3)).await;
    wait_state(&r, "a", "registered again", registered).await;
    assert_eq!(
        reg.accepts.load(Ordering::SeqCst),
        1,
        "same connection while a call is up"
    );
    assert!(no_failures(&drain(&mut r)));

    r.stack.set_active_calls("a", 0).unwrap(); // the call ended: redial now
    accepts_reach(&reg, 2).await;
    registers_reach(&reg, 3, Duration::from_secs(3)).await;
    wait_state(&r, "a", "registered on the new connection", registered).await;
    assert!(no_failures(&drain(&mut r)));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn tcp_dead_flow_is_redialled_even_with_a_live_call() {
    let reg = tcp_registrar(); // never answers OPTIONS
    let mut r = rig();
    add(
        &r,
        AccountConfig {
            transport: Transport::Tcp,
            ..options_account("a", &reg, 2)
        },
    )
    .await;
    wait_state(&r, "a", "registered", registered).await;
    r.stack.set_active_calls("a", 1).unwrap();
    drain(&mut r);

    registers_reach(&reg, 2, Duration::from_secs(12)).await;
    accepts_reach(&reg, 2).await;
    wait_state(&r, "a", "registered again", registered).await;
    let evs = drain(&mut r);
    assert!(no_failures(&evs), "{evs:?}");
    assert!(evs.iter().any(|e| matches!(e,
        SipEvent::AccountReconnecting { reason, .. } if reason == "keepalive_timeout")));
    r.stack.shutdown().await;
}

// --- round 3 ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_kept_but_dead_flow_is_redialled_when_the_register_times_out() {
    siple_sip::set_register_timeout_ms(2000);
    let reg = tcp_registrar();
    let mut r = rig();
    add(&r, tcp_account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    r.stack.set_active_calls("a", 1).unwrap(); // a call keeps us on this connection
    drain(&mut r);

    // The server stops answering on the kept connection (half-open: no FIN, no RST).
    reg.mute_conn.store(1, Ordering::SeqCst);
    r.change("addr");
    registers_reach(&reg, 2, Duration::from_secs(3)).await; // sent on the kept connection
    assert_eq!(reg.accepts.load(Ordering::SeqCst), 1);
    // The REGISTER times out: the flow is dead, redial and register again, no Failed in between.
    accepts_reach(&reg, 2).await;
    registers_reach(&reg, 3, Duration::from_secs(8)).await;
    wait_state(&r, "a", "registered on the new connection", registered).await;
    let evs = drain(&mut r);
    assert!(no_failures(&evs), "{evs:?}");
    assert!(evs.iter().any(|e| matches!(e,
        SipEvent::AccountReconnecting { reason, .. } if reason == "register_timeout")));
    siple_sip::set_register_timeout_ms(0);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn same_v4_on_a_new_v6_prefix_registers_through_the_real_gate() {
    let reg = registrar(|_| ok());
    let r = rig();
    r.probe.set_v6_prefix("2001:db8:1:1::5");
    // Re-baseline the service with the v6 address present.
    r.stack.network_refresh().await;
    add(&r, account("a", &reg)).await;
    wait_state(&r, "a", "registered", registered).await;
    let n = reg.count();

    // Privacy rotation inside the /64 is noise: no REGISTER, no new generation.
    let g = r.stack.network_generation();
    r.probe.set_v6_prefix("2001:db8:1:1:aaaa:bbbb:cccc:dddd");
    r.mon.fire("netlink");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!((reg.count(), r.stack.network_generation()), (n, g));

    // A different /64 with the same IPv4 is another network.
    r.probe.set_v6_prefix("2001:db8:2:2::5");
    r.mon.fire("netlink");
    registers_reach(&reg, n + 1, Duration::from_secs(3)).await;
    assert_eq!(r.stack.network_generation(), g + 1);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn dropped_noise_still_makes_accounts_re_probe_their_next_hop() {
    let old = registrar(|_| ok());
    let new = registrar(|_| ok());
    let r = rig();
    r.probe.map("reg.test", old.addr);
    let mut cfg = account("a", &old);
    cfg.registrar = "reg.test:5060".into();
    add(&r, cfg).await;
    wait_state(&r, "a", "registered", registered).await;
    let g = r.stack.network_generation();

    // DNS moved; the only notification is noise the fingerprint gate drops. Accounts are still
    // told to re-probe, so the account moves without any network.changed.
    r.probe.map("reg.test", new.addr);
    r.mon.fire("netlink");
    registers_reach(&new, 1, Duration::from_secs(3)).await;
    assert_eq!(r.stack.network_generation(), g, "nothing was published");
    wait_state(&r, "a", "registered on the new registrar", registered).await;
    r.stack.shutdown().await;
}

// --- live calls: moved with a re-INVITE -------------------------------------------------------

/// A second local address a socket can bind to: `127.0.0.2` (Linux), else the default-route
/// address (no packet is sent to find it). `None` on a host with neither: those tests skip.
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

macro_rules! alt_ip_or_skip {
    () => {
        match alt_local_ip() {
            Some(ip) => ip,
            None => {
                eprintln!("skipped: the host has no second local IPv4 address");
                return;
            }
        }
    };
}

/// `set_move_attempt_ms` is process-wide: the tests that set it run one at a time.
static MOVE_HOOK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn until(r: &mut Rig, what: &str, pred: impl Fn(&SipEvent) -> bool) -> SipEvent {
    if let Some(i) = r.buf.iter().position(&pred) {
        return r.buf.remove(i);
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let e = r.events.recv().await.expect("event channel closed");
            if pred(&e) {
                return e;
            }
            r.buf.push(e);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn state_of(r: &mut Rig, id: &str, st: CallState) -> CallInfo {
    let id = id.to_string();
    match until(
        r,
        &format!("{id} -> {st:?}"),
        |e| matches!(e, SipEvent::CallState { call } if call.id == id && call.state == st),
    )
    .await
    {
        SipEvent::CallState { call } => call,
        _ => unreachable!(),
    }
}

async fn reconnecting(r: &mut Rig, id: &str) {
    let id = id.to_string();
    until(
        r,
        "call.reconnecting",
        |e| matches!(e, SipEvent::CallReconnecting { call_id, .. } if *call_id == id),
    )
    .await;
}

async fn reconnected(r: &mut Rig, id: &str) {
    let id = id.to_string();
    until(
        r,
        "call.reconnected",
        |e| matches!(e, SipEvent::CallReconnected { call_id, .. } if *call_id == id),
    )
    .await;
}

/// Account `b` registered at a fake registrar (`reg.test`, so the next hop can be moved), media
/// from a [`NullMediaProvider`].
async fn call_rig(reg: &Registrar) -> (Rig, Arc<NullMediaProvider>, String) {
    call_rig_with(reg, |_| {}).await
}

async fn call_rig_with(
    reg: &Registrar,
    tweak: impl FnOnce(&mut AccountConfig),
) -> (Rig, Arc<NullMediaProvider>, String) {
    let r = rig();
    let media = NullMediaProvider::new();
    r.stack.set_media_provider(media.clone());
    r.probe.map("reg.test", reg.addr);
    let mut cfg = account("b", reg);
    cfg.registrar = "reg.test:5060".into();
    tweak(&mut cfg);
    add(&r, cfg).await;
    wait_state(&r, "b", "registered", registered).await;
    let local = r.stack.account_status("b").unwrap().local_addr.unwrap();
    (r, media, local)
}

/// A raw-UDP peer dials account `b`, `b` answers, the peer ACKs. Returns `(peer, call id, to-tag)`.
async fn established(r: &mut Rig, local: &str) -> (common::Raw, String, String) {
    established_extra(r, local, "").await
}

async fn established_extra(r: &mut Rig, local: &str, extra: &str) -> (common::Raw, String, String) {
    let raw = common::Raw::new(local).await;
    raw.send(&raw.msg("INVITE", None, 1, extra, &raw.sdp()))
        .await;
    let id = match until(r, "incoming call", |e| {
        matches!(e, SipEvent::CallIncoming { .. })
    })
    .await
    {
        SipEvent::CallIncoming { call } => call.id,
        _ => unreachable!(),
    };
    r.stack.answer(&id, AnswerOptions::default()).await.unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    let tag = common::to_tag(&ok);
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    state_of(r, &id, CallState::Active).await;
    (raw, id, tag)
}

/// `host:port` of the top Via of a request (where its responses go).
fn via_addr(req: &str) -> SocketAddr {
    let via = common::hdr(req, "via")[0];
    let sent_by = via.split_whitespace().nth(2).unwrap();
    sent_by.split(';').next().unwrap().parse().unwrap()
}

fn sdp_line<'a>(msg: &'a str, prefix: &str) -> Option<&'a str> {
    msg.lines().find(|l| l.starts_with(prefix))
}

fn origin_version(msg: &str) -> u64 {
    sdp_line(msg, "o=")
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap()
}

/// The peer answers a re-INVITE of ours with 200 and an SDP in direction `dir`, and takes the
/// ACK.
async fn answer_reinvite(raw: &common::Raw, inv: &str, dir: &str, version: u32) {
    raw.reply(inv, "200 OK", Some(&raw.sdp_dir(dir, version)))
        .await;
    assert!(
        raw.request("ACK", 5).await.is_some(),
        "no ACK for the 2xx of the re-INVITE"
    );
}

#[tokio::test]
async fn a_live_call_is_moved_with_a_reinvite_carrying_the_new_contact_and_c_line() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    let first = media.stats(&id).unwrap();
    let old_port = first.rtp_port;
    assert_eq!(first.bind_ip, Some("127.0.0.1".parse().unwrap()));
    drain(&mut r);

    r.probe.set_local(alt);
    r.change("addr");
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv);
    assert_eq!(
        raw.dest.ip(),
        alt,
        "the re-INVITE leaves from the new address"
    );
    assert!(
        sdp_line(&inv, "c=").unwrap().ends_with(&alt.to_string()),
        "c= carries the new address: {inv}"
    );
    let contact = common::hdr(&inv, "contact")[0].to_string();
    assert!(contact.contains(&alt.to_string()), "Contact: {contact}");
    let m = media.stats(&id).unwrap();
    assert_eq!(m.rebinds, 1);
    assert_eq!(m.bind_ip, Some(alt));
    assert_ne!(m.rtp_port, 0);
    assert!(
        sdp_line(&inv, "m=audio")
            .unwrap()
            .contains(&format!(" {} ", m.rtp_port)),
        "the SDP port is the rebound socket's: {inv}"
    );
    assert!(origin_version(&inv) > 1, "o= version is bumped: {inv}");
    assert!(inv.contains("a=sendrecv"), "direction kept: {inv}");
    let applies = m.applies;
    answer_reinvite(&raw, &inv, "sendrecv", 2).await;

    reconnecting(&mut r, &id).await;
    reconnected(&mut r, &id).await;
    let info = r.stack.call(&id).unwrap();
    assert_eq!(info.state, CallState::Active);
    assert!(!info.reconnecting);
    assert!(
        media.stats(&id).unwrap().applies > applies,
        "the answer was applied to the media"
    );
    let _ = old_port;
    // Registration followed the move.
    wait_state(&r, "b", "registered again", registered).await;
    assert!(reg.count() >= 2, "re-registered on the new address");
    // The call is alive: a BYE of the peer (to the new Contact, from its dialog) ends it.
    let bye = raw.msg("BYE", Some(&_tag), 2, "", "");
    raw.send(&bye).await;
    state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(
        r.stack.call(&id).unwrap().end.unwrap().cause,
        EndCause::RemoteHangup
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn a_481_to_the_move_ends_the_call_with_network_lost() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;

    r.probe.set_local(alt);
    r.change("addr");
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv);
    raw.reply(&inv, "481 Call/Transaction Does Not Exist", None)
        .await;
    reconnecting(&mut r, &id).await;
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    let end = ended.end.unwrap();
    assert_eq!(end.cause, EndCause::NetworkLost, "{end:?}");
    assert!(!ended.reconnecting);
    // `call.reconnected` never follows the end of the call.
    assert!(
        !drain(&mut r)
            .iter()
            .chain(r.buf.iter())
            .any(|e| matches!(e, SipEvent::CallReconnected { .. })),
        "call.reconnected after a failed move"
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn no_answer_to_the_move_ends_the_call_with_network_lost() {
    let alt = alt_ip_or_skip!();
    let _hook = MOVE_HOOK.lock().await;
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (raw, id, _tag) = established(&mut r, &local).await;
    siple_sip::set_move_attempt_ms(700); // stands for the 10 s of the plan
    r.probe.set_local(alt);
    r.change("addr");
    let t0 = Instant::now();
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    assert!(inv.starts_with("INVITE "));
    // The peer never answers.
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    siple_sip::set_move_attempt_ms(0);
    assert_eq!(ended.end.unwrap().cause, EndCause::NetworkLost);
    assert!(
        t0.elapsed() < Duration::from_secs(8),
        "gave up after {:?}",
        t0.elapsed()
    );
    r.stack.shutdown().await;
}

#[tokio::test]
async fn move_calls_false_ends_live_calls_with_network_lost_and_sends_no_reinvite() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (raw, id, _tag) = established(&mut r, &local).await;
    // Hot: applied to the running stack.
    r.stack.set_network_config(&NetworkConfig {
        watch: true,
        probe_secs: 0,
        debounce_ms: 100,
        move_calls: false,
    });
    r.probe.set_local(alt);
    r.change("addr");
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().cause, EndCause::NetworkLost);
    // No re-INVITE (the best-effort BYE may arrive).
    assert!(raw.request("INVITE", 1).await.is_none());
    // The account itself re-registered on the new address.
    registers_reach(&reg, 2, Duration::from_secs(4)).await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn a_held_call_stays_held_after_the_move() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    let stack = r.stack.clone();
    let cid = id.clone();
    let hold = tokio::spawn(async move { stack.hold(&cid).await });
    let inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    assert!(inv.contains("a=sendonly"), "{inv}");
    raw.reply(&inv, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    assert!(raw.request("ACK", 5).await.is_some());
    hold.await.unwrap().unwrap();
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Held);

    r.probe.set_local(alt);
    r.change("addr");
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv);
    assert!(
        inv.contains("a=sendonly"),
        "the hold direction is kept in the move offer: {inv}"
    );
    assert!(sdp_line(&inv, "c=").unwrap().ends_with(&alt.to_string()));
    answer_reinvite(&raw, &inv, "recvonly", 3).await;
    reconnected(&mut r, &id).await;
    let info = r.stack.call(&id).unwrap();
    assert_eq!(info.state, CallState::Held);
    assert!(info.hold.local && !info.reconnecting);
    assert!(media.stats(&id).unwrap().held, "media stays held");
    r.stack.shutdown().await;
}

/// The peer's own re-INVITE crosses ours (glare): it gets a 491, the peer answers our move with
/// a 491 as well, and the retry (RFC 3261 §14.1 back-off) goes through. A hold of ours issued
/// meanwhile waits for the move instead of failing or deadlocking.
#[tokio::test]
async fn a_move_racing_a_hold_resolves_through_the_491_path_without_deadlock() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (mut raw, id, tag) = established(&mut r, &local).await;

    r.probe.set_local(alt);
    r.change("addr");
    let inv1 = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv1);
    reconnecting(&mut r, &id).await;
    // Our own hold, started while the move is outstanding: it queues behind the move.
    let stack = r.stack.clone();
    let cid = id.clone();
    let hold = tokio::spawn(async move { stack.hold(&cid).await });
    // The peer's re-INVITE crosses ours: 491.
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 5)))
        .await;
    let busy = raw.final_response("CSeq: 2 INVITE").await;
    assert!(busy.starts_with("SIP/2.0 491"), "{busy}");
    // ...and it answers ours with a 491 too.
    raw.reply(&inv1, "491 Request Pending", None).await;
    // The move retries after the back-off and succeeds.
    let inv2 = raw
        .request("INVITE", 8)
        .await
        .expect("retried move re-INVITE");
    assert!(sdp_line(&inv2, "c=").unwrap().ends_with(&alt.to_string()));
    answer_reinvite(&raw, &inv2, "sendrecv", 6).await;
    reconnected(&mut r, &id).await;
    // Then the queued hold runs.
    let inv3 = raw.request("INVITE", 8).await.expect("hold re-INVITE");
    assert!(inv3.contains("a=sendonly"), "{inv3}");
    answer_reinvite(&raw, &inv3, "recvonly", 7).await;
    tokio::time::timeout(Duration::from_secs(10), hold)
        .await
        .expect("hold deadlocked behind the move")
        .unwrap()
        .unwrap();
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Held);
    r.stack.shutdown().await;
}

/// Only the registrar moved (same local address): the call is not touched.
#[tokio::test]
async fn a_moved_registrar_alone_leaves_a_live_call_alone() {
    let old = registrar(|_| ok());
    let new = registrar(|_| ok());
    let (mut r, media, local) = call_rig(&old).await;
    let (raw, id, _tag) = established(&mut r, &local).await;
    r.probe.map("reg.test", new.addr);
    r.mon.fire("dns");
    registers_reach(&new, 1, Duration::from_secs(4)).await;
    assert!(raw.request("INVITE", 1).await.is_none(), "no re-INVITE");
    assert_eq!(r.stack.call(&id).unwrap().state, CallState::Active);
    assert_eq!(media.stats(&id).unwrap().rebinds, 0);
    r.stack.shutdown().await;
}

/// The new address cannot be bound (the probe lies): last resort, the call ends with
/// `network_lost` and the account is rebuilt.
#[tokio::test]
async fn a_rebind_that_fails_ends_the_call_as_the_last_resort() {
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (_raw, id, _tag) = established(&mut r, &local).await;
    r.probe.set_local("203.0.113.77".parse().unwrap()); // TEST-NET-3: not a local address
    r.change("addr");
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().cause, EndCause::NetworkLost);
    r.stack.shutdown().await;
}

/// A second change arrives while the first move's re-INVITE is still unanswered (it sits on a
/// path that is gone): the call is not ended by the overtaken move, it follows the newest path.
#[tokio::test]
async fn a_second_network_change_during_a_move_moves_the_call_again_instead_of_ending_it() {
    let alt = alt_ip_or_skip!();
    let _hook = MOVE_HOOK.lock().await;
    let reg = registrar(|_| ok());
    let (mut r, media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    siple_sip::set_move_attempt_ms(1200);

    r.probe.set_local(alt);
    r.change("addr");
    let inv1 = raw
        .request("INVITE", 8)
        .await
        .expect("first move re-INVITE");
    assert!(sdp_line(&inv1, "c=").unwrap().ends_with(&alt.to_string()));
    // Never answered; the network changes again (back to the first address).
    let back: IpAddr = "127.0.0.1".parse().unwrap();
    r.probe.set_local(back);
    r.change("addr");
    let inv2 = loop {
        let inv = raw
            .request("INVITE", 12)
            .await
            .expect("second move re-INVITE");
        if sdp_line(&inv, "c=").unwrap().ends_with("127.0.0.1") {
            break inv;
        }
    };
    siple_sip::set_move_attempt_ms(0);
    raw.dest = via_addr(&inv2);
    assert!(origin_version(&inv2) > origin_version(&inv1), "{inv2}");
    answer_reinvite(&raw, &inv2, "sendrecv", 2).await;
    reconnected(&mut r, &id).await;
    let info = r.stack.call(&id).unwrap();
    assert_eq!(info.state, CallState::Active, "{info:?}");
    assert!(!info.reconnecting);
    assert_eq!(media.stats(&id).unwrap().bind_ip, Some(back));
    r.stack.shutdown().await;
}

/// A→B→A before the first move ran (registrar keeps failing, so the move waits MOVE_WAIT_REGISTER):
/// is the call still reachable at its Contact (the original listener)?
#[tokio::test]
async fn flap_back_before_move_keeps_signalling() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|i| {
        if i == 0 {
            ok()
        } else {
            Script {
                code: 503,
                reason: "Service Unavailable",
                extra: vec![],
            }
        }
    });
    let (mut r, _media, local) = call_rig(&reg).await;
    let (raw, id, tag) = established(&mut r, &local).await;
    r.probe.set_local(alt);
    r.change("addr");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    // Wait past MOVE_WAIT_REGISTER.
    let inv = raw.request("INVITE", 6).await;
    eprintln!("re-INVITE after flap back: {}", inv.is_some());
    // The peer sends a BYE to our (unchanged) Contact = the original listener.
    let bye = raw.msg("BYE", Some(&tag), 2, "", "");
    raw.send(&bye).await;
    let resp = raw.recv_any(3).await;
    eprintln!(
        "response to BYE at original Contact: {:?}",
        resp.as_deref().map(|s| s.lines().next())
    );
    eprintln!("call state: {:?}", r.stack.call(&id).unwrap().state);
    assert!(resp.is_some(), "BYE to the call's Contact was not answered");
    r.stack.shutdown().await;
}

/// Fixed local_port, A→B (moved), then B→A: does the call survive?
#[tokio::test]
async fn fixed_port_return_to_first_address() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let r0 = rig();
    let mut r = r0;
    let media = NullMediaProvider::new();
    r.stack.set_media_provider(media.clone());
    r.probe.map("reg.test", reg.addr);
    let port = {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let mut cfg = account("b", &reg);
    cfg.registrar = "reg.test:5060".into();
    cfg.local_port = port;
    add(&r, cfg).await;
    wait_state(&r, "b", "registered", registered).await;
    let local = r.stack.account_status("b").unwrap().local_addr.unwrap();
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    r.probe.set_local(alt);
    r.change("addr");
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv);
    answer_reinvite(&raw, &inv, "sendrecv", 2).await;
    reconnected(&mut r, &id).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    let inv2 = raw.request("INVITE", 8).await;
    eprintln!("second move re-INVITE: {}", inv2.is_some());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = r.stack.call(&id).unwrap();
    eprintln!("state {:?} end {:?}", info.state, info.end);
    assert_ne!(info.state, CallState::Ended, "call ended: {:?}", info.end);
    r.stack.shutdown().await;
}

/// A ringing call on A, the network goes A→B→A (each settles), then the call is answered.
#[tokio::test]
async fn early_call_across_flap_back() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let raw = common::Raw::new(&local).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let id = match until(&mut r, "incoming call", |e| {
        matches!(e, SipEvent::CallIncoming { .. })
    })
    .await
    {
        SipEvent::CallIncoming { call } => call.id,
        _ => unreachable!(),
    };
    r.probe.set_local(alt);
    r.change("addr");
    tokio::time::sleep(Duration::from_secs(2)).await;
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    tokio::time::sleep(Duration::from_secs(2)).await;
    r.stack.answer(&id, AnswerOptions::default()).await.unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    let tag = common::to_tag(&ok);
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bye = raw.msg("BYE", Some(&tag), 2, "", "");
    raw.send(&bye).await;
    let resp = tokio::time::timeout(Duration::from_secs(4), raw.final_response("CSeq: 2 BYE"))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    eprintln!(
        "early: BYE answered: {:?}; state {:?}",
        resp.as_deref().map(|s| s.lines().next()),
        r.stack.call(&id).unwrap().state
    );
    assert!(
        resp.is_some_and(|s| s.contains("BYE")),
        "BYE to the early call's Contact was not answered"
    );
    r.stack.shutdown().await;
}

/// Same, but A→B only (control: the lingering listener serves the early call).
#[tokio::test]
async fn early_call_across_single_move() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let raw = common::Raw::new(&local).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let id = match until(&mut r, "incoming call", |e| {
        matches!(e, SipEvent::CallIncoming { .. })
    })
    .await
    {
        SipEvent::CallIncoming { call } => call.id,
        _ => unreachable!(),
    };
    r.probe.set_local(alt);
    r.change("addr");
    tokio::time::sleep(Duration::from_secs(2)).await;
    r.stack.answer(&id, AnswerOptions::default()).await.unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    let tag = common::to_tag(&ok);
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let bye = raw.msg("BYE", Some(&tag), 2, "", "");
    raw.send(&bye).await;
    let resp = tokio::time::timeout(Duration::from_secs(4), raw.final_response("CSeq: 2 BYE"))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    eprintln!(
        "early-control: BYE answered: {:?}; state {:?}",
        resp.as_deref().map(|s| s.lines().next()),
        r.stack.call(&id).unwrap().state
    );
    assert!(resp.is_some_and(|s| s.contains("BYE")));
    r.stack.shutdown().await;
}

#[tokio::test]
async fn fixed_port_return_after_linger_expiry() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let r0 = rig();
    let mut r = r0;
    let media = NullMediaProvider::new();
    r.stack.set_media_provider(media.clone());
    r.probe.map("reg.test", reg.addr);
    let port = {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let mut cfg = account("b", &reg);
    cfg.registrar = "reg.test:5060".into();
    cfg.local_port = port;
    add(&r, cfg).await;
    wait_state(&r, "b", "registered", registered).await;
    let local = r.stack.account_status("b").unwrap().local_addr.unwrap();
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    r.probe.set_local(alt);
    r.change("addr");
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv);
    answer_reinvite(&raw, &inv, "sendrecv", 2).await;
    reconnected(&mut r, &id).await;
    tokio::time::sleep(Duration::from_secs(37)).await;
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    let inv2 = raw.request("INVITE", 8).await;
    eprintln!("second move re-INVITE: {}", inv2.is_some());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = r.stack.call(&id).unwrap();
    eprintln!("state {:?} end {:?}", info.state, info.end);
    assert_ne!(info.state, CallState::Ended, "call ended: {:?}", info.end);
    r.stack.shutdown().await;
}

/// A REGISTER in flight on the old path (its registrar went silent) must not hold the new path's
/// registration back: the rebind drops it and registers at once.
#[tokio::test]
async fn a_register_hanging_on_the_old_path_does_not_delay_the_new_registration() {
    let muted = Arc::new(AtomicBool::new(false));
    let m = muted.clone();
    let old = registrar(move |_| {
        if m.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(8));
        }
        ok()
    });
    let new = registrar(|_| ok());
    let r = rig();
    r.probe.map("reg.test", old.addr);
    let mut cfg = account("a", &old);
    cfg.registrar = "reg.test:5060".into();
    add(&r, cfg).await;
    wait_state(&r, "a", "registered", registered).await;
    r.stack.set_active_calls("a", 1).unwrap(); // a live call: the rebind happens in place
    muted.store(true, Ordering::SeqCst);
    r.stack.register_now("a").unwrap(); // goes to the old registrar and hangs
    registers_reach(&old, 2, Duration::from_secs(3)).await;
    r.probe.map("reg.test", new.addr);
    r.mon.fire("dns");
    let took = registers_reach(&new, 1, Duration::from_secs(4)).await;
    assert!(
        took < Duration::from_millis(2500),
        "re-REGISTER took {took:?}"
    );
    r.stack.shutdown().await;
}

/// Only the registrar moved on a TCP account with a fixed `local_port` and a live call: the
/// transport is kept (no rebuild, no `Unregistered`), the account re-registers at the new next hop.
#[tokio::test]
async fn a_moved_registrar_on_a_fixed_port_tcp_account_does_not_rebuild_under_live_calls() {
    let old = tcp_registrar();
    let new = tcp_registrar();
    let mut r = rig();
    r.probe.map("reg.test", old.addr);
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cfg = tcp_account("a", &old);
    cfg.registrar = "reg.test:5060".into();
    cfg.local_port = port;
    add(&r, cfg).await;
    wait_state(&r, "a", "registered", registered).await;
    r.stack.set_active_calls("a", 1).unwrap();
    drain(&mut r);
    r.probe.map("reg.test", new.addr);
    r.mon.fire("dns");
    registers_reach(&new, 1, Duration::from_secs(5)).await;
    wait_state(&r, "a", "registered at the new registrar", registered).await;
    let evs = drain(&mut r);
    assert!(
        !evs.iter().any(|e| matches!(
            e,
            SipEvent::Registration {
                state: RegState::Unregistered,
                ..
            }
        )),
        "the transport was rebuilt: {evs:?}"
    );
    r.stack.shutdown().await;
}

/// A hold re-INVITE is in flight when the network changes; its 408 arrives once the move has
/// started. It must not end the dialog (`Failed`): the move proceeds and keeps the call.
#[tokio::test]
async fn a_hold_that_times_out_during_a_move_does_not_end_the_call() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    let stack = r.stack.clone();
    let cid = id.clone();
    let hold = tokio::spawn(async move { stack.hold(&cid).await });
    let hold_inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    r.probe.set_local(alt);
    r.change("addr");
    reconnecting(&mut r, &id).await;
    raw.reply(&hold_inv, "408 Request Timeout", None).await;
    assert!(hold.await.unwrap().is_err());
    let inv = raw.request("INVITE", 8).await.expect("move re-INVITE");
    raw.dest = via_addr(&inv);
    answer_reinvite(&raw, &inv, "sendrecv", 2).await;
    reconnected(&mut r, &id).await;
    let info = r.stack.call(&id).unwrap();
    assert_eq!(info.state, CallState::Active, "{info:?}");
    assert!(r.stack.call(&id).unwrap().end.is_none());
    r.stack.shutdown().await;
}

/// A refresh re-INVITE is in flight when the network flaps A -> B -> A; its 408 arrives while the
/// move is pending. The session timer must keep running (a refresh follows), also when the move
/// finds the call already on the newest generation and does nothing.
#[tokio::test]
async fn the_session_timer_survives_a_skipped_refresh_during_a_flap() {
    let alt = alt_ip_or_skip!();
    // The registrar answers the first REGISTER only: the move waits its 3 s for the Contact.
    let reg = registrar(|i| {
        if i == 0 {
            ok()
        } else {
            Script {
                code: 503,
                reason: "Service Unavailable",
                extra: vec![],
            }
        }
    });
    let (mut r, media, local) = call_rig_with(&reg, |c| {
        c.session_expires = 4;
        c.min_se = 2;
    })
    .await;
    let (raw, id, _tag) = established_extra(
        &mut r,
        &local,
        "Supported: timer\r\nSession-Expires: 4;refresher=uas\r\nAllow: INVITE, ACK, BYE\r\n",
    )
    .await;
    let refresh = raw.request("INVITE", 6).await.expect("session refresh");
    r.probe.set_local(alt);
    r.change("addr");
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    tokio::time::sleep(Duration::from_millis(500)).await;
    raw.reply(&refresh, "408 Request Timeout", None).await;
    // Past the 3 s wait the move finds the call on the newest generation: the timer must still
    // refresh the session afterwards.
    let cseq = |m: &str| -> u32 {
        common::hdr(m, "cseq")[0]
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    };
    let first = cseq(&refresh);
    // (Retransmissions of the unanswered refresh may still be queued: wait for a newer CSeq.)
    let again = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            match raw.request("INVITE", 12).await {
                Some(m) if cseq(&m) > first => return Some(m),
                Some(_) => {}
                None => return None,
            }
        }
    })
    .await
    .unwrap_or(None);
    let again = again.expect("the session timer stopped for good");
    assert_eq!(
        media.stats(&id).unwrap().rebinds,
        0,
        "the call must not have moved: {again}"
    );
    r.stack.shutdown().await;
}

/// Two changes close together: the first move finishes while the second rebind's move has not
/// started. A hold that times out meanwhile must still not end the dialog.
#[tokio::test]
async fn a_finished_move_does_not_clear_the_pending_flag_of_a_newer_change() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|i| {
        if i == 2 {
            std::thread::sleep(Duration::from_millis(3000)); // the second change's REGISTER
        }
        ok()
    });
    let (mut r, _media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    r.probe.set_local(alt);
    r.change("addr");
    let inv1 = raw
        .request("INVITE", 8)
        .await
        .expect("first move re-INVITE");
    raw.dest = via_addr(&inv1);
    // The network changes again while the first move is still unanswered.
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    tokio::time::sleep(Duration::from_millis(500)).await;
    raw.reply(&inv1, "200 OK", Some(&raw.sdp_dir("sendrecv", 2)))
        .await;
    assert!(raw.request("ACK", 5).await.is_some());
    reconnected(&mut r, &id).await;
    // The second move is still waiting for its REGISTER: a hold times out now.
    let stack = r.stack.clone();
    let cid = id.clone();
    let hold = tokio::spawn(async move { stack.hold(&cid).await });
    let hold_inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    raw.reply(&hold_inv, "408 Request Timeout", None).await;
    assert!(hold.await.unwrap().is_err());
    assert_ne!(
        r.stack.call(&id).unwrap().state,
        CallState::Ended,
        "the dialog was ended by the failed hold: {:?}",
        r.stack.call(&id).unwrap().end
    );
    r.stack.shutdown().await;
}

/// A move waits for the re-INVITE slot (a hold is in flight) while the network flaps back: the
/// obsolete move must not send its offer (it names the address that is gone again).
#[tokio::test]
async fn an_obsolete_move_does_not_send_its_offer_after_the_slot_frees() {
    let alt = alt_ip_or_skip!();
    let reg = registrar(|_| ok());
    let (mut r, _media, local) = call_rig(&reg).await;
    let (mut raw, id, _tag) = established(&mut r, &local).await;
    let stack = r.stack.clone();
    let cid = id.clone();
    let hold = tokio::spawn(async move { stack.hold(&cid).await });
    let hold_inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    r.probe.set_local(alt);
    r.change("addr");
    reconnecting(&mut r, &id).await; // move 1 waits for the slot
    // Back to the first address (the lingering listener is re-installed); the call has not
    // moved yet.
    r.probe.set_local("127.0.0.1".parse().unwrap());
    r.change("addr");
    tokio::time::sleep(Duration::from_millis(800)).await;
    raw.reply(&hold_inv, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    assert!(raw.request("ACK", 5).await.is_some());
    hold.await.unwrap().unwrap();
    // Whatever re-INVITE follows must carry the current address, never the abandoned one.
    if let Some(inv) = raw.request("INVITE", 6).await {
        raw.dest = via_addr(&inv);
        assert!(
            !sdp_line(&inv, "c=").unwrap().ends_with(&alt.to_string()),
            "an obsolete move offered the abandoned address: {inv}"
        );
        answer_reinvite(&raw, &inv, "recvonly", 3).await;
    }
    assert_ne!(r.stack.call(&id).unwrap().state, CallState::Ended);
    r.stack.shutdown().await;
}
