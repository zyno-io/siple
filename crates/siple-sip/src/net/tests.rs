use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;

use super::*;

// --- fakes -----------------------------------------------------------------------------------------

#[derive(Default)]
struct FakeMonitor {
    sink: Mutex<Option<mpsc::UnboundedSender<NetReason>>>,
    starts: AtomicUsize,
    fail: bool,
}

impl FakeMonitor {
    fn fire(&self, what: &str) {
        let s = self.sink.lock().unwrap();
        s.as_ref()
            .expect("monitor started")
            .send(NetReason::Os(what.into()))
            .unwrap();
    }
    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
}

impl NetMonitor for FakeMonitor {
    fn start(&self, sink: mpsc::UnboundedSender<NetReason>) -> io::Result<Box<dyn Send>> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(io::Error::other("no watcher"));
        }
        *self.sink.lock().unwrap() = Some(sink);
        Ok(Box::new(()))
    }
}

struct FakeProbe(
    Mutex<Option<IpAddr>>,
    Mutex<Option<Vec<InterfaceInfo>>>,
    Mutex<Option<String>>,
);

impl FakeProbe {
    fn new(ip: Option<&str>) -> Arc<Self> {
        Arc::new(Self(
            Mutex::new(ip.map(|s| s.parse().unwrap())),
            Mutex::new(None),
            Mutex::new(None),
        ))
    }
    fn set(&self, ip: Option<&str>) {
        *self.0.lock().unwrap() = ip.map(|s| s.parse().unwrap());
    }
    fn set_gateway(&self, g: Option<&str>) {
        *self.2.lock().unwrap() = g.map(Into::into);
    }
    fn set_ifaces(&self, list: Option<Vec<InterfaceInfo>>) {
        *self.1.lock().unwrap() = list;
    }
}

#[async_trait::async_trait]
impl NetProbe for FakeProbe {
    fn local_ip_toward(&self, dest: SocketAddr) -> io::Result<IpAddr> {
        match *self.0.lock().unwrap() {
            Some(ip) if dest.is_ipv4() => Ok(ip),
            _ => Err(io::Error::from(io::ErrorKind::NetworkUnreachable)),
        }
    }
    fn gateway(&self) -> Option<String> {
        self.2.lock().unwrap().clone()
    }
    fn interfaces(&self) -> io::Result<Vec<InterfaceInfo>> {
        self.1
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))
    }
    async fn resolve(&self, _: &str, _: Option<u16>, _: Transport) -> Result<Vec<SocketAddr>> {
        Err(crate::SipError::Dns("not used".into()))
    }
}

struct FakeClock(Mutex<SystemTime>);

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000),
        )))
    }
    fn advance(&self, d: Duration) {
        *self.0.lock().unwrap() += d;
    }
}

impl WallClock for FakeClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

fn cfg(probe_secs: u32) -> NetworkConfig {
    NetworkConfig {
        watch: true,
        probe_secs,
        debounce_ms: 1000,
        move_calls: true,
    }
}

struct Rig {
    svc: NetService,
    mon: Arc<FakeMonitor>,
    probe: Arc<FakeProbe>,
    clock: Arc<FakeClock>,
    rx: broadcast::Receiver<NetEvent>,
}

async fn rig(c: NetworkConfig, mon: FakeMonitor) -> Rig {
    let mon = Arc::new(mon);
    let probe = FakeProbe::new(Some("10.0.0.5"));
    let clock = FakeClock::new();
    let svc = NetService::spawn(c, mon.clone(), probe.clone(), clock.clone());
    let rx = svc.subscribe();
    tokio::task::yield_now().await; // let the task start the monitor
    Rig {
        svc,
        mon,
        probe,
        clock,
        rx,
    }
}

fn changed(e: NetEvent) -> NetChange {
    match e {
        NetEvent::Changed(c) => c,
        other => panic!("expected Changed, got {other:?}"),
    }
}

// --- pure pieces ---------------------------------------------------------------------------------------

#[test]
fn gap_detector_flags_only_large_jumps() {
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
    let mut g = GapDetector::new(WAKE_TICK, WAKE_GAP, t0);
    let mut now = t0;
    // Regular ticks and modest scheduling delays are fine.
    for dt in [5, 5, 6, 12, 25] {
        now += Duration::from_secs(dt);
        assert_eq!(g.tick(now), None, "dt={dt}");
    }
    // 5 s expected + 20 s threshold: 25 s is the boundary (not above), 26 s is a gap.
    now += Duration::from_secs(26);
    assert_eq!(g.tick(now), Some(Duration::from_secs(21)));
    // The detector re-arms from the new time.
    now += Duration::from_secs(5);
    assert_eq!(g.tick(now), None);
    // A long sleep.
    now += Duration::from_secs(3600);
    assert_eq!(g.tick(now), Some(Duration::from_secs(3595)));
    // The clock set backwards is not a wake.
    now -= Duration::from_secs(600);
    assert_eq!(g.tick(now), None);
    now += Duration::from_secs(5);
    assert_eq!(g.tick(now), None);
}

#[test]
fn unreachable_classification() {
    use io::ErrorKind as K;
    for k in [
        K::NetworkUnreachable,
        K::HostUnreachable,
        K::AddrNotAvailable,
        K::NetworkDown,
    ] {
        assert!(is_unreachable_io(&io::Error::from(k)), "{k:?}");
    }
    for n in [
        101, 113, 99, 100, 51, 65, 49, 50, 10051, 10065, 10049, 10050,
    ] {
        assert!(
            is_unreachable_io(&io::Error::from_raw_os_error(n)),
            "errno {n}"
        );
    }
    for k in [
        K::ConnectionRefused,
        K::TimedOut,
        K::PermissionDenied,
        K::Other,
    ] {
        assert!(!is_unreachable_io(&io::Error::from(k)), "{k:?}");
    }
    for t in [
        "transport: Network is unreachable (os error 101)",
        "send failed: No route to host (os error 113)",
        "Can't assign requested address (os error 49)",
        "failed (os error 10051)",
        "cannot assign requested address",
    ] {
        assert!(is_unreachable_text(t), "{t}");
    }
    for t in [
        "Connection refused (os error 111)",
        "request timed out",
        "os error x",
        "",
    ] {
        assert!(!is_unreachable_text(t), "{t}");
    }
}

#[test]
fn reason_labels() {
    assert_eq!(NetReason::Os("addr".into()).label(), "os:addr");
    assert_eq!(
        NetReason::Wake {
            gap: Duration::from_secs(9)
        }
        .label(),
        "wake"
    );
    assert_eq!(NetReason::Route("x".into()).label(), "route_changed");
    assert_eq!(NetReason::Manual.label(), "manual");
}

// --- the service (tokio paused time) ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn triggers_coalesce_into_one_generation() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    assert_eq!(r.svc.generation(), 0);
    r.mon.fire("addr");
    r.mon.fire("route");
    r.mon.fire("addr");
    r.svc.trigger(NetReason::Manual);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(r.rx.try_recv().is_err(), "still inside the debounce window");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let c = changed(r.rx.try_recv().expect("one event after the window"));
    assert_eq!(c.generation, 1);
    let mut got = c.reasons.clone();
    got.sort();
    assert_eq!(
        got,
        ["manual", "os:addr", "os:route"],
        "de-duplicated: {c:?}"
    );
    assert!(r.rx.try_recv().is_err(), "exactly one event for the burst");
    assert_eq!(r.svc.generation(), 1);
    // A later trigger is a new generation.
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(changed(r.rx.try_recv().unwrap()).generation, 2);
}

#[tokio::test(start_paused = true)]
async fn flapping_slides_the_window_but_is_capped() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    // A trigger every 600 ms keeps the (1000 ms) window open...
    for _ in 0..4 {
        r.mon.fire("flap");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(r.rx.try_recv().is_err());
    }
    // ...until the 4x cap (4000 ms after the first trigger) forces it out.
    for _ in 0..10 {
        r.mon.fire("flap");
        tokio::time::sleep(Duration::from_millis(600)).await;
        if let Ok(e) = r.rx.try_recv() {
            let c = changed(e);
            assert_eq!(c.generation, 1);
            assert_eq!(c.reasons, ["os:flap"]);
            return;
        }
    }
    panic!("the debounce cap never fired");
}

#[tokio::test(start_paused = true)]
async fn refresh_is_immediate_and_returns_the_generation() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_millis(100)).await; // pending, window still open
    let g = r.svc.refresh().await;
    assert_eq!(g, 1);
    let c = changed(r.rx.try_recv().unwrap());
    assert_eq!(
        c.reasons,
        ["os:addr", "manual"],
        "pending triggers are merged"
    );
    // The merged burst does not fire again later.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(r.rx.try_recv().is_err());
    assert_eq!(r.svc.refresh().await, 2);
}

#[tokio::test(start_paused = true)]
async fn sleep_gap_is_a_wake_trigger() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    // Normal ticks: nothing.
    for _ in 0..3 {
        r.clock.advance(WAKE_TICK);
        tokio::time::sleep(WAKE_TICK).await;
    }
    assert!(r.rx.try_recv().is_err());
    // The machine slept for 10 minutes: one monotonic tick, a big wall-clock jump.
    r.clock.advance(Duration::from_secs(600));
    tokio::time::sleep(WAKE_TICK).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let c = changed(r.rx.try_recv().expect("wake event"));
    assert_eq!(c.reasons, ["wake"]);
}

#[tokio::test(start_paused = true)]
async fn no_wake_detection_or_os_events_when_watch_is_off() {
    let mut c = cfg(0);
    c.watch = false;
    let mut r = rig(c, FakeMonitor::default()).await;
    assert_eq!(r.mon.starts(), 0);
    r.clock.advance(Duration::from_secs(600));
    tokio::time::sleep(Duration::from_millis(30_100)).await;
    assert!(r.rx.try_recv().is_err());
    // The API still works, and turning watch on starts the monitor.
    r.svc.trigger(NetReason::Manual);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(changed(r.rx.try_recv().unwrap()).reasons, ["manual"]);
    r.svc.set_config(&cfg(0));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(r.mon.starts(), 1);
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(changed(r.rx.try_recv().unwrap()).reasons, ["os:addr"]);
}

#[tokio::test(start_paused = true)]
async fn monitor_failure_degrades_to_probe_only() {
    let mut r = rig(
        cfg(10),
        FakeMonitor {
            fail: true,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(r.mon.starts(), 1, "tried once");
    // Sleep/wake detection and the probe still work.
    r.probe.set(Some("192.168.1.9"));
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(r.rx.try_recv().unwrap(), NetEvent::Probe);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let c = changed(r.rx.try_recv().expect("route change"));
    assert_eq!(c.reasons, ["route_changed"]);
    assert_eq!(c.old_local_ip.as_deref(), Some("10.0.0.5"));
    assert_eq!(c.new_local_ip.as_deref(), Some("192.168.1.9"));
    assert_eq!(r.mon.starts(), 1, "not retried every tick");
}

#[tokio::test(start_paused = true)]
async fn periodic_probe_acts_only_on_a_difference() {
    let mut r = rig(cfg(30), FakeMonitor::default()).await;
    // Unchanged route: a Probe tick every 30 s, never a Changed.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(30_100)).await;
        assert_eq!(r.rx.try_recv().unwrap(), NetEvent::Probe);
        assert!(r.rx.try_recv().is_err());
    }
    // No route at all is a difference too.
    r.probe.set(None);
    tokio::time::sleep(Duration::from_millis(30_100)).await;
    assert_eq!(r.rx.try_recv().unwrap(), NetEvent::Probe);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let c = changed(r.rx.try_recv().unwrap());
    assert_eq!(
        (c.old_local_ip.as_deref(), c.new_local_ip),
        (Some("10.0.0.5"), None)
    );
    // And it is not reported again while the route stays gone.
    tokio::time::sleep(Duration::from_millis(30_100)).await;
    assert_eq!(r.rx.try_recv().unwrap(), NetEvent::Probe);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(r.rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn dropping_the_service_stops_the_task() {
    let r = rig(cfg(30), FakeMonitor::default()).await;
    let mut rx = r.svc.subscribe();
    drop(r.svc);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(matches!(
        rx.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
}

// --- fingerprint (M3) -------------------------------------------------------------------------------------

fn iface(name: &str, up: bool, addrs: &[&str]) -> InterfaceInfo {
    InterfaceInfo {
        name: name.into(),
        up,
        addrs: addrs.iter().map(|a| a.parse().unwrap()).collect(),
        gateways: Vec::new(),
    }
}

fn wifi() -> Vec<InterfaceInfo> {
    vec![
        iface("lo0", true, &["127.0.0.1", "::1"]),
        iface("en0", true, &["192.168.1.20", "fe80::1", "2001:db8::5"]),
        iface("awdl0", true, &["fe80::2"]),
        iface("utun3", true, &["100.64.0.7"]),
        iface("docker0", true, &["172.17.0.1"]),
        iface("en5", false, &["10.9.9.9"]),
    ]
}

fn fp(ifs: &[InterfaceInfo]) -> RouteFingerprint {
    RouteFingerprint::from_parts(Some("192.168.1.20".parse().unwrap()), true, None, Some(ifs))
}

#[test]
fn interface_filter_keeps_vpns_and_drops_virtual_noise() {
    for n in [
        "awdl0",
        "llw0",
        "anpi0",
        "ap1",
        "bridge100",
        "vmenet0",
        "veth12ab",
        "docker0",
        "br-3f2a",
        "virbr0",
        "vEthernet (WSL)",
        "lo",
        "lo0",
    ] {
        assert!(ignored_interface(n), "{n}");
    }
    for n in [
        "en0",
        "eth0",
        "wlan0",
        "utun3",
        "tun0",
        "wg0",
        "ppp0",
        "tailscale0",
        "Wi-Fi",
        "Ethernet",
        "apple",
    ] {
        assert!(!ignored_interface(n), "{n}");
    }
    for a in [
        "127.0.0.1",
        "::1",
        "169.254.3.3",
        "fe80::1",
        "0.0.0.0",
        "224.0.0.1",
        "ff02::1",
    ] {
        assert!(!relevant_addr(&a.parse().unwrap()), "{a}");
    }
    for a in ["192.168.1.20", "100.64.0.7", "2001:db8::5"] {
        assert!(relevant_addr(&a.parse().unwrap()), "{a}");
    }
}

#[test]
fn fingerprint_diff_ignores_noise_but_sees_real_changes() {
    let base = fp(&wifi());
    assert!(base.judged());
    assert_eq!(base.route_if.as_deref(), Some("en0"));
    let names: Vec<_> = base.ifaces.as_ref().unwrap().keys().cloned().collect();
    assert_eq!(
        names,
        ["en0", "utun3"],
        "loopback, awdl, docker and down interfaces are out"
    );
    // Noise: link-local churn, a new IPv6 address (RA / privacy rotation), docker veth appearing,
    // awdl flapping, a down interface changing.
    let mut noisy = wifi();
    noisy[1].addrs.push("fe80::99".parse().unwrap());
    noisy[1].addrs.push("2001:db8::77".parse().unwrap());
    noisy.push(iface("veth1", true, &["172.18.0.1"]));
    noisy[2].up = false;
    noisy[5].addrs = vec!["10.9.9.10".parse().unwrap()];
    assert_eq!(fp(&noisy), base);
    // Real: a new IPv4 address on a real interface.
    let mut changed = wifi();
    changed[1].addrs.push("10.0.0.4".parse().unwrap());
    assert_ne!(fp(&changed), base);
    // Real: the VPN went away / the interface went down.
    let mut vpn_down = wifi();
    vpn_down[3].up = false;
    assert_ne!(fp(&vpn_down), base);
    // Real: a different default-route source address (Wi-Fi to Ethernet).
    let other =
        RouteFingerprint::from_parts(Some("10.9.9.9".parse().unwrap()), true, None, Some(&wifi()));
    assert_ne!(other, base);
    // Without an interface list nothing can be judged.
    assert!(!RouteFingerprint::from_parts(None, false, None, None).judged());
}

fn nets(ips: &[&str]) -> Vec<InterfaceInfo> {
    vec![iface("en0", true, ips)]
}

#[tokio::test(start_paused = true)]
async fn os_notifications_without_a_relevant_change_are_ignored() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    r.probe.set_ifaces(Some(nets(&["10.0.0.5"])));
    // The service sampled the (unjudgeable) start state; re-baseline with a Manual refresh.
    r.svc.refresh().await;
    let _ = r.rx.try_recv();
    // Lifetime refreshes, scans, container churn: same fingerprint.
    for _ in 0..5 {
        r.mon.fire("netlink");
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    // Accounts re-probe, but nothing is published.
    assert_eq!(r.rx.try_recv().unwrap(), NetEvent::Probe);
    assert!(r.rx.try_recv().is_err(), "no generation bump for noise");
    assert_eq!(r.svc.generation(), 1);
    // A new address is real.
    r.probe.set_ifaces(Some(nets(&["10.0.0.5", "10.0.0.6"])));
    r.mon.fire("netlink");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(changed(r.rx.try_recv().unwrap()).reasons, ["os:netlink"]);
}

#[tokio::test(start_paused = true)]
async fn a_link_flap_inside_the_window_still_counts() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    r.probe.set_ifaces(Some(nets(&["10.0.0.5"])));
    r.svc.refresh().await;
    let _ = r.rx.try_recv();
    // Wi-Fi network switch: the interface stays UP,RUNNING (as on macOS) but briefly loses its
    // address, then gets the same one back within the debounce window.
    r.probe.set_ifaces(Some(vec![iface("en0", true, &[])]));
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_millis(200)).await;
    r.probe.set_ifaces(Some(nets(&["10.0.0.5"])));
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        changed(r.rx.try_recv().expect("flap is a change")).generation,
        2
    );
}

#[tokio::test(start_paused = true)]
async fn wake_and_manual_always_trigger_even_when_nothing_changed() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    r.probe.set_ifaces(Some(nets(&["10.0.0.5"])));
    r.svc.refresh().await;
    let _ = r.rx.try_recv();
    r.svc.trigger(NetReason::Wake {
        gap: Duration::from_secs(60),
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(changed(r.rx.try_recv().unwrap()).reasons, ["wake"]);
    r.svc.trigger(NetReason::Manual);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(changed(r.rx.try_recv().unwrap()).reasons, ["manual"]);
}

// --- fingerprint blind spots (round 3) ---------------------------------------------------------------------

#[test]
fn ipv6_prefix_and_gateway_identity_are_in_the_fingerprint() {
    let v4 = Some("192.168.1.20".parse().unwrap());
    let with = |addrs: &[&str], gw: Option<&str>| {
        RouteFingerprint::from_parts(
            v4,
            true,
            gw.map(Into::into),
            Some(&[iface("en0", true, addrs)]),
        )
    };
    let base = with(&["192.168.1.20", "2001:db8:1:1::5"], Some("gw-a"));
    // Privacy-extension rotation inside the same /64: no change.
    assert_eq!(
        base,
        with(
            &["192.168.1.20", "2001:db8:1:1:aaaa:bbbb:cccc:dddd"],
            Some("gw-a")
        )
    );
    // Same IPv4, another dual-stack network (new /64): a change.
    assert_ne!(
        base,
        with(&["192.168.1.20", "2001:db8:2:2::5"], Some("gw-a"))
    );
    // IPv6-only prefix change.
    let v6only =
        |p: &str| RouteFingerprint::from_parts(None, true, None, Some(&[iface("en0", true, &[p])]));
    assert_ne!(v6only("2001:db8:1:1::5"), v6only("2001:db8:9:9::5"));
    // Same addresses, another gateway / network signature: a change.
    assert_ne!(
        base,
        with(&["192.168.1.20", "2001:db8:1:1::5"], Some("gw-b"))
    );
}

async fn baseline(r: &mut Rig) {
    r.probe
        .set_ifaces(Some(nets(&["10.0.0.5", "2001:db8:1:1::5"])));
    r.probe.set_gateway(Some("gw-a"));
    r.svc.refresh().await;
    let _ = r.rx.try_recv();
}

#[tokio::test(start_paused = true)]
async fn same_v4_with_a_new_v6_prefix_publishes() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    baseline(&mut r).await;
    r.probe
        .set_ifaces(Some(nets(&["10.0.0.5", "2001:db8:2:2::5"])));
    r.mon.fire("netlink");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        changed(r.rx.try_recv().expect("v6 prefix change")).reasons,
        ["os:netlink"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_gateway_change_alone_publishes() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    baseline(&mut r).await;
    r.probe.set_gateway(Some("gw-b"));
    r.mon.fire("addr");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        changed(r.rx.try_recv().expect("gateway change")).reasons,
        ["os:addr"]
    );
}

#[tokio::test(start_paused = true)]
async fn dns_link_and_global_hints_always_publish() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    baseline(&mut r).await;
    for (i, hint) in DECISIVE_HINTS.iter().enumerate() {
        r.mon.fire(hint); // nothing in the fingerprint changed
        tokio::time::sleep(Duration::from_secs(2)).await;
        let c = changed(
            r.rx.try_recv()
                .unwrap_or_else(|_| panic!("{hint} must publish")),
        );
        assert_eq!(c.reasons, [format!("os:{hint}")]);
        assert_eq!(c.generation, i as u64 + 2);
    }
}

#[tokio::test(start_paused = true)]
async fn noise_does_not_publish_but_accounts_still_re_probe() {
    let mut r = rig(cfg(0), FakeMonitor::default()).await;
    baseline(&mut r).await;
    for _ in 0..4 {
        r.mon.fire("netlink");
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        r.rx.try_recv().unwrap(),
        NetEvent::Probe,
        "accounts are told to re-probe"
    );
    assert!(r.rx.try_recv().is_err(), "but no network.changed");
    assert_eq!(r.svc.generation(), 1);
}
