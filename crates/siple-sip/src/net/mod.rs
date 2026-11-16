//! Network-change detection.
//!
//! [`NetService`] turns three kinds of signals into debounced "the network changed" events with a
//! process-wide, monotonically increasing *generation*:
//!
//! 1. **OS address / route events** from a [`NetMonitor`] ([`os::OsMonitor`]: macOS
//!    SystemConfiguration, Windows `NotifyIpInterfaceChange`, Linux rtnetlink). A monitor that
//!    cannot start degrades to probe-only with one INFO log.
//! 2. **Sleep / wake**, detected portably: a 5 s ticker flags a wall-clock jump of more than
//!    20 s beyond the expected interval ([`GapDetector`]).
//! 3. **A periodic route probe** every `probe_secs` (a changed default-route source address
//!    counts as a change; every account additionally re-probes its own next hop on each probe
//!    tick, see [`NetEvent::Probe`]).
//!
//! Triggers are coalesced for `debounce_ms` (sliding, capped at 4x). All OS access sits behind
//! the [`NetMonitor`], [`NetProbe`] and [`WallClock`] traits so tests inject fakes.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use siple_config::{NetworkConfig, Transport};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::dns::{self, SystemSrvSource};
use crate::error::Result;

pub mod os;

#[cfg(unix)]
#[path = "ifaddrs_unix.rs"]
mod ifaddrs;
#[cfg(windows)]
#[path = "ifaddrs_windows.rs"]
mod ifaddrs;

/// OS hint details that always publish a change: `dns` and `link` (macOS, filtered by interface
/// name) and `global` (the macOS global IPv4/IPv6 state: primary interface / router).
pub const DECISIVE_HINTS: [&str; 3] = ["dns", "link", "global"];

/// Interval of the sleep/wake ticker.
pub const WAKE_TICK: Duration = Duration::from_secs(5);
/// A tick later than `WAKE_TICK` by more than this means the machine slept.
pub const WAKE_GAP: Duration = Duration::from_secs(20);
/// The debounce window never grows beyond this multiple of `debounce_ms`.
const DEBOUNCE_CAP_FACTOR: u32 = 4;
/// Destinations used to learn which source address the OS would pick (no packet is sent).
const PROBE_V4: ([u8; 4], u16) = ([8, 8, 8, 8], 53);
const PROBE_V6: ([u16; 8], u16) = ([0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888], 53);

/// Why a network-change signal was raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetReason {
    /// The OS reported an address / route / interface change (`detail` is monitor specific).
    Os(String),
    /// The machine woke from sleep; `gap` is how long the wall clock jumped beyond the tick.
    Wake {
        /// Excess wall-clock time since the previous tick.
        gap: Duration,
    },
    /// The periodic probe saw a different default route (`detail` describes it).
    Route(String),
    /// Requested through the API (`network.refresh`).
    Manual,
}

impl NetReason {
    /// Short stable label used in events and logs.
    pub fn label(&self) -> String {
        match self {
            NetReason::Os(d) => format!("os:{d}"),
            NetReason::Wake { .. } => "wake".into(),
            NetReason::Route(_) => "route_changed".into(),
            NetReason::Manual => "manual".into(),
        }
    }
}

/// A debounced network change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetChange {
    /// Process-wide generation after this change (starts at 0, first change is 1).
    pub generation: u64,
    /// De-duplicated reason labels, in the order they first occurred.
    pub reasons: Vec<String>,
    /// Source address toward the public internet (IPv4) before this change.
    pub old_local_ip: Option<String>,
    /// Source address toward the public internet (IPv4) after this change.
    pub new_local_ip: Option<String>,
}

/// What [`NetService`] publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetEvent {
    /// The network changed (debounced). Every account re-probes at once and re-registers.
    Changed(NetChange),
    /// Periodic probe tick (every `probe_secs`): accounts re-probe their next hop and act only on
    /// a difference, or when waiting for the network.
    Probe,
}

/// OS access: starts watching for address / route changes.
pub trait NetMonitor: Send + Sync + 'static {
    /// Starts watching. Every change must be reported as a [`NetReason`] on `sink` (from any
    /// thread). Dropping the returned guard stops the watcher. An error means "unavailable":
    /// the service carries on with the periodic probe and sleep/wake detection only.
    fn start(&self, sink: mpsc::UnboundedSender<NetReason>) -> io::Result<Box<dyn Send>>;
}

/// Wall clock (injectable so the sleep-gap detector is testable).
pub trait WallClock: Send + Sync + 'static {
    /// Current wall-clock time.
    fn now(&self) -> SystemTime;
}

/// The real wall clock.
pub struct SystemClock;

impl WallClock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// OS access: route and name lookups, injectable so tests can fake a changed or missing route.
#[async_trait::async_trait]
pub trait NetProbe: Send + Sync + 'static {
    /// Source address the OS would use toward `dest`. No packet is sent. An unreachable error
    /// (see [`is_unreachable_io`]) means "no route".
    fn local_ip_toward(&self, dest: SocketAddr) -> io::Result<IpAddr>;
    /// Identity of the default gateway / network (router address, macOS network signature), for
    /// the change fingerprint. `None` = unknown.
    fn gateway(&self) -> Option<String> {
        None
    }
    /// Network interfaces with their addresses, for the change fingerprint. An error (the
    /// default) means "cannot tell": OS notifications are then trusted without filtering.
    fn interfaces(&self) -> io::Result<Vec<InterfaceInfo>> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
    /// Resolves `host[:port]` for `transport` to candidate socket addresses, best first.
    async fn resolve(
        &self,
        host: &str,
        port: Option<u16>,
        transport: Transport,
    ) -> Result<Vec<SocketAddr>>;

    /// Resolves for `transport = "auto"`: the transport DNS SRV selects (`_sips._tcp` >
    /// `_sip._tcp` > `_sip._udp`, else UDP 5060) and its candidates. The default knows no SRV
    /// and answers UDP; [`SystemProbe`] does the real lookup.
    async fn resolve_auto(
        &self,
        host: &str,
        port: Option<u16>,
    ) -> Result<(Transport, Vec<SocketAddr>)> {
        Ok((
            Transport::Udp,
            self.resolve(host, port, Transport::Udp).await?,
        ))
    }
}

/// [`NetProbe`] backed by the OS routing table and the system resolver.
pub struct SystemProbe {
    dns: Arc<SystemSrvSource>,
}

impl SystemProbe {
    /// Probe using `dns` for name lookups.
    pub fn new(dns: Arc<SystemSrvSource>) -> Self {
        Self { dns }
    }
}

#[async_trait::async_trait]
impl NetProbe for SystemProbe {
    fn local_ip_toward(&self, dest: SocketAddr) -> io::Result<IpAddr> {
        crate::transport::local_ip_toward(dest)
    }

    #[cfg(any(unix, windows))]
    fn interfaces(&self) -> io::Result<Vec<InterfaceInfo>> {
        ifaddrs::list()
    }

    fn gateway(&self) -> Option<String> {
        #[cfg(windows)]
        {
            let mut g: Vec<String> = ifaddrs::list()
                .ok()?
                .iter()
                .filter(|i| i.up && !ignored_interface(&i.name))
                .flat_map(|i| i.gateways.iter().map(|a| a.to_string()))
                .collect();
            g.sort();
            (!g.is_empty()).then(|| g.join(","))
        }
        #[cfg(not(windows))]
        {
            os::default_gateway()
        }
    }

    async fn resolve(
        &self,
        host: &str,
        port: Option<u16>,
        transport: Transport,
    ) -> Result<Vec<SocketAddr>> {
        dns::resolve(self.dns.as_ref(), host, port, transport).await
    }

    async fn resolve_auto(
        &self,
        host: &str,
        port: Option<u16>,
    ) -> Result<(Transport, Vec<SocketAddr>)> {
        dns::resolve_auto(self.dns.as_ref(), host, port).await
    }
}

// --- error classification ------------------------------------------------------------------------

/// Whether an I/O error means the destination or the local address is unreachable
/// (`ENETUNREACH`, `EHOSTUNREACH`, `EADDRNOTAVAIL`, `ENETDOWN`), on every OS.
pub fn is_unreachable_io(e: &io::Error) -> bool {
    use io::ErrorKind::{AddrNotAvailable, HostUnreachable, NetworkDown, NetworkUnreachable};
    matches!(
        e.kind(),
        NetworkUnreachable | HostUnreachable | AddrNotAvailable | NetworkDown
    ) || e.raw_os_error().is_some_and(is_unreachable_errno)
}

/// Raw OS error numbers for the conditions above: Linux, macOS/BSD and Winsock.
fn is_unreachable_errno(n: i32) -> bool {
    matches!(
        n,
        // Linux: ENETDOWN, ENETUNREACH, EADDRNOTAVAIL, EHOSTUNREACH
        100 | 101 | 99 | 113
        // macOS / BSD: EADDRNOTAVAIL, ENETDOWN, ENETUNREACH, EHOSTUNREACH
        | 49 | 50 | 51 | 65
        // Winsock: WSAEADDRNOTAVAIL, WSAENETDOWN, WSAENETUNREACH, WSAEHOSTUNREACH
        | 10049 | 10050 | 10051 | 10065
    )
}

/// [`is_unreachable_io`] for errors that only survive as text (rsipstack wraps I/O errors into
/// strings): matches `os error N` and the usual strerror phrases.
pub fn is_unreachable_text(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    if let Some(pos) = m.find("os error ") {
        let digits: String = m[pos + 9..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.parse::<i32>().is_ok_and(is_unreachable_errno) {
            return true;
        }
    }
    [
        "network is unreachable",
        "no route to host",
        "network is down",
        "cannot assign requested address",
        "can't assign requested address",
        "the requested address is not valid in its context",
    ]
    .iter()
    .any(|p| m.contains(p))
}

// --- sleep/wake detector -----------------------------------------------------------------------

/// Flags a wall-clock jump: each [`GapDetector::tick`] compares the elapsed wall time with the
/// expected tick interval.
#[derive(Debug, Clone)]
pub struct GapDetector {
    interval: Duration,
    threshold: Duration,
    last: SystemTime,
}

impl GapDetector {
    /// Expects a tick every `interval`; a tick later by more than `threshold` is a gap.
    pub fn new(interval: Duration, threshold: Duration, now: SystemTime) -> Self {
        Self {
            interval,
            threshold,
            last: now,
        }
    }

    /// Records a tick at `now`; returns the excess gap when it exceeded the threshold. A clock
    /// set backwards never reports a gap.
    pub fn tick(&mut self, now: SystemTime) -> Option<Duration> {
        let elapsed = now.duration_since(self.last).unwrap_or(Duration::ZERO);
        self.last = now;
        let excess = elapsed.saturating_sub(self.interval);
        (excess > self.threshold).then_some(excess)
    }
}

// --- network fingerprint --------------------------------------------------------------------------------

/// One network interface as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceInfo {
    /// OS name (`en0`, `eth0`, `Wi-Fi`).
    pub name: String,
    /// Administratively up and link running.
    pub up: bool,
    /// All addresses of the interface.
    pub addrs: Vec<IpAddr>,
    /// Default-gateway addresses, where the OS lists them with the interface (Windows).
    pub gateways: Vec<IpAddr>,
}

/// Interfaces that never carry the registrar path: Apple wireless direct / low latency / bridge /
/// VM networks, container and VM bridges. VPN interfaces (`utun`, `tun`, `wg`, `ppp`,
/// `tailscale`) are deliberately kept.
pub fn ignored_interface(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let digits_after = |p: &str| {
        n.strip_prefix(p)
            .is_some_and(|r| r.chars().all(|c| c.is_ascii_digit()))
    };
    [
        "awdl",
        "llw",
        "anpi",
        "bridge",
        "vmenet",
        "veth",
        "docker",
        "br-",
        "virbr",
        "vethernet",
        "lo",
    ]
    .iter()
    .any(|p| {
        if *p == "lo" {
            digits_after("lo")
        } else {
            n.starts_with(p)
        }
    }) || digits_after("ap")
}

/// Whether an address says something about reachability: not loopback, unspecified, multicast
/// or link-local.
pub fn relevant_addr(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            !(a.is_loopback() || a.is_unspecified() || a.is_link_local() || a.is_multicast())
        }
        IpAddr::V6(a) => {
            !(a.is_loopback()
                || a.is_unspecified()
                || a.is_multicast()
                || (a.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// What the interface looks like to the fingerprint: its IPv4 addresses and the /64 prefixes of
/// its IPv6 addresses (the interface identifier rotates with privacy extensions; the prefix
/// identifies the network and is stable).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IfaceState {
    /// Relevant IPv4 addresses.
    pub v4: std::collections::BTreeSet<IpAddr>,
    /// /64 prefixes of relevant IPv6 addresses.
    pub v6: std::collections::BTreeSet<[u8; 8]>,
}

/// Source addresses toward the public internet plus the usable interfaces; compared before and
/// after an OS notification to decide whether anything relevant changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteFingerprint {
    /// IPv4 source address toward the internet; `None` = no route.
    pub v4: Option<IpAddr>,
    /// An IPv6 route exists.
    pub v6: bool,
    /// Interface that owns the IPv4 source address (the default-route interface).
    pub route_if: Option<String>,
    /// Default gateway / network identity, where the OS can tell.
    pub gateway: Option<String>,
    /// Up interfaces with relevant addresses; `None` when the OS cannot list them.
    pub ifaces: Option<std::collections::BTreeMap<String, IfaceState>>,
}

impl RouteFingerprint {
    /// Probes the route and lists the interfaces.
    pub fn probe(p: &dyn NetProbe) -> Self {
        let v4 = p.local_ip_toward(PROBE_V4.into()).ok();
        let v6 = p.local_ip_toward(PROBE_V6.into()).is_ok();
        Self::from_parts(v4, v6, p.gateway(), p.interfaces().ok().as_deref())
    }

    /// Builds a fingerprint from already-gathered facts (pure; unit-tested).
    pub fn from_parts(
        v4: Option<IpAddr>,
        v6: bool,
        gateway: Option<String>,
        ifaces: Option<&[InterfaceInfo]>,
    ) -> Self {
        let map = ifaces.map(|list| {
            let mut m = std::collections::BTreeMap::new();
            for i in list.iter().filter(|i| i.up && !ignored_interface(&i.name)) {
                let mut st = IfaceState::default();
                for a in i.addrs.iter().filter(|a| relevant_addr(a)) {
                    match a {
                        IpAddr::V4(_) => {
                            st.v4.insert(*a);
                        }
                        IpAddr::V6(a) => {
                            let mut prefix = [0u8; 8];
                            prefix.copy_from_slice(&a.octets()[..8]);
                            st.v6.insert(prefix);
                        }
                    }
                }
                if !st.v4.is_empty() || !st.v6.is_empty() {
                    m.insert(i.name.clone(), st);
                }
            }
            m
        });
        let route_if = v4.and_then(|ip| {
            ifaces?
                .iter()
                .find(|i| i.up && i.addrs.contains(&ip))
                .map(|i| i.name.clone())
        });
        Self {
            v4,
            v6,
            route_if,
            gateway,
            ifaces: map,
        }
    }

    /// Whether the OS could list interfaces (otherwise notifications cannot be judged).
    pub fn judged(&self) -> bool {
        self.ifaces.is_some()
    }

    fn describe(&self) -> String {
        format!(
            "v4={} v6={} if={} gw={}",
            self.v4.map_or("none".to_string(), |ip| ip.to_string()),
            self.v6,
            self.route_if.as_deref().unwrap_or("?"),
            self.gateway.as_deref().unwrap_or("?")
        )
    }
}

/// What a [`crate::SipStack`] is built with; every part can be replaced by a fake in tests.
#[derive(Default)]
pub struct NetSetup {
    /// Initial `[network]` settings (default: [`NetworkConfig::default`], i.e. watching).
    pub config: Option<NetworkConfig>,
    /// OS watcher (default: [`os::OsMonitor`]).
    pub monitor: Option<Arc<dyn NetMonitor>>,
    /// Route / DNS probe (default: [`SystemProbe`]).
    pub probe: Option<Arc<dyn NetProbe>>,
    /// Wall clock for sleep/wake detection (default: [`SystemClock`]).
    pub clock: Option<Arc<dyn WallClock>>,
}

// --- the service -----------------------------------------------------------------------------------

struct Shared {
    generation: AtomicU64,
    events: broadcast::Sender<NetEvent>,
    trig: mpsc::UnboundedSender<NetReason>,
    refresh: mpsc::UnboundedSender<oneshot::Sender<u64>>,
    cfg: watch::Sender<NetworkConfig>,
    cancel: CancellationToken,
}

/// Debounced network-change source. Cheap to clone; the background task stops when the last
/// clone is dropped or [`NetService::shutdown`] is called.
#[derive(Clone)]
pub struct NetService {
    shared: Arc<Shared>,
    _guard: Arc<StopOnDrop>,
}

struct StopOnDrop(CancellationToken);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl NetService {
    /// Starts the service on the current tokio runtime.
    pub fn spawn(
        cfg: NetworkConfig,
        monitor: Arc<dyn NetMonitor>,
        probe: Arc<dyn NetProbe>,
        clock: Arc<dyn WallClock>,
    ) -> Self {
        let (events, _) = broadcast::channel(64);
        let (trig, trig_rx) = mpsc::unbounded_channel();
        let (refresh, refresh_rx) = mpsc::unbounded_channel();
        let (cfg_tx, cfg_rx) = watch::channel(cfg);
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            events,
            trig,
            refresh,
            cfg: cfg_tx,
            cancel: cancel.clone(),
        });
        let task = Task {
            shared: shared.clone(),
            monitor,
            probe,
            clock,
            cfg: cfg_rx,
            trig_rx,
            refresh_rx,
        };
        tokio::spawn(task.run());
        Self {
            shared,
            _guard: Arc::new(StopOnDrop(cancel)),
        }
    }

    /// Current generation (0 until the first change).
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// Subscribes to [`NetEvent`]s.
    pub fn subscribe(&self) -> broadcast::Receiver<NetEvent> {
        self.shared.events.subscribe()
    }

    /// Raises a trigger; it is coalesced with others for `debounce_ms`.
    pub fn trigger(&self, reason: NetReason) {
        let _ = self.shared.trig.send(reason);
    }

    /// Forces a change now (no debounce, pending triggers are merged) and returns the new
    /// generation.
    pub async fn refresh(&self) -> u64 {
        let (tx, rx) = oneshot::channel();
        if self.shared.refresh.send(tx).is_err() {
            return self.generation();
        }
        rx.await.unwrap_or_else(|_| self.generation())
    }

    /// The `[network]` settings currently in force.
    pub fn config(&self) -> NetworkConfig {
        self.shared.cfg.borrow().clone()
    }

    /// Applies new `[network]` settings to the running monitor.
    pub fn set_config(&self, cfg: &NetworkConfig) {
        self.shared.cfg.send_if_modified(|c| {
            if c == cfg {
                false
            } else {
                *c = cfg.clone();
                true
            }
        });
    }

    /// Stops the background task and the OS watcher.
    pub fn shutdown(&self) {
        self.shared.cancel.cancel();
    }
}

struct Task {
    shared: Arc<Shared>,
    monitor: Arc<dyn NetMonitor>,
    probe: Arc<dyn NetProbe>,
    clock: Arc<dyn WallClock>,
    cfg: watch::Receiver<NetworkConfig>,
    trig_rx: mpsc::UnboundedReceiver<NetReason>,
    refresh_rx: mpsc::UnboundedReceiver<oneshot::Sender<u64>>,
}

/// Coalescing state: sliding window from the latest trigger, capped relative to the first.
#[derive(Default)]
struct Pending {
    reasons: Vec<NetReason>,
    first: Option<Instant>,
    deadline: Option<Instant>,
    /// An OS notification arrived while the fingerprint differed from the last published one
    /// (catches an interface that went down and came back within the window).
    saw_diff: bool,
}

impl Pending {
    fn add(&mut self, r: NetReason, now: Instant, debounce: Duration) {
        self.reasons.push(r);
        let first = *self.first.get_or_insert(now);
        let cap = first + debounce * DEBOUNCE_CAP_FACTOR;
        self.deadline = Some((now + debounce).min(cap));
    }

    fn take(&mut self) -> Vec<NetReason> {
        self.first = None;
        self.deadline = None;
        self.saw_diff = false;
        std::mem::take(&mut self.reasons)
    }
}

async fn sleep_until_opt(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

async fn tick_opt(i: &mut Option<tokio::time::Interval>) {
    match i {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn interval(period: Duration) -> tokio::time::Interval {
    let mut i = tokio::time::interval_at(Instant::now() + period, period);
    i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    i
}

impl Task {
    async fn run(mut self) {
        let cancel = self.shared.cancel.clone();
        let (mon_tx, mut mon_rx) = mpsc::unbounded_channel::<NetReason>();
        let mut cfg = self.cfg.borrow_and_update().clone();
        let mut guard: Option<Box<dyn Send>> = None;
        let mut monitor_failed = false;
        let mut wake_tick: Option<tokio::time::Interval> = None;
        let mut probe_tick: Option<tokio::time::Interval> = None;
        let mut gap = GapDetector::new(WAKE_TICK, WAKE_GAP, self.clock.now());
        let mut pending = Pending::default();
        let mut fp = RouteFingerprint::probe(self.probe.as_ref());
        let mut configured = false;
        let mut last_cfg = cfg.clone();

        loop {
            if !configured || cfg != last_cfg {
                // (Re)apply settings: watcher on/off, tickers re-armed.
                if cfg.watch {
                    if guard.is_none() && !monitor_failed {
                        match self.monitor.start(mon_tx.clone()) {
                            Ok(g) => {
                                tracing::debug!("OS network watcher started");
                                guard = Some(g);
                            }
                            Err(e) => {
                                monitor_failed = true;
                                tracing::info!(error = %e, "OS network watcher unavailable; using the periodic probe and sleep/wake detection only");
                            }
                        }
                    }
                    if wake_tick.is_none() || !configured {
                        gap = GapDetector::new(WAKE_TICK, WAKE_GAP, self.clock.now());
                        wake_tick = Some(interval(WAKE_TICK));
                    }
                } else {
                    guard = None;
                    monitor_failed = false;
                    wake_tick = None;
                }
                if !configured || cfg.probe_secs != last_cfg.probe_secs {
                    probe_tick = (cfg.probe_secs > 0)
                        .then(|| interval(Duration::from_secs(u64::from(cfg.probe_secs))));
                }
                configured = true;
                last_cfg = cfg.clone();
            }
            let debounce = Duration::from_millis(u64::from(cfg.debounce_ms));

            tokio::select! {
                () = cancel.cancelled() => break,
                Some(r) = self.trig_rx.recv() => self.hint(&mut pending, &fp, r, debounce),
                Some(r) = mon_rx.recv() => {
                    if cfg.watch {
                        self.hint(&mut pending, &fp, r, debounce);
                    }
                }
                Some(reply) = self.refresh_rx.recv() => {
                    pending.reasons.push(NetReason::Manual);
                    let g = self.emit(&mut pending, &mut fp);
                    let _ = reply.send(g);
                }
                () = sleep_until_opt(pending.deadline) => {
                    // OS notifications are hints (IPv6 RA lifetimes, scans, container churn):
                    // publish only if the network really changed, or flapped inside the window.
                    let only_hints = pending.reasons.iter().all(|r| matches!(r, NetReason::Os(_)));
                    // DNS, link and global-state hints are specific enough to always publish.
                    let decisive = pending.reasons.iter().any(|r| matches!(r, NetReason::Os(d) if DECISIVE_HINTS.contains(&d.as_str())));
                    if only_hints && !decisive {
                        let now_fp = RouteFingerprint::probe(self.probe.as_ref());
                        if now_fp.judged() && !pending.saw_diff && now_fp == fp {
                            tracing::debug!(hints = pending.reasons.len(), "network notification without a relevant change; accounts only re-probe");
                            pending.take();
                            // No generation bump and no forced REGISTER, but every account still
                            // re-probes its own next hop (DNS moves, split routes).
                            let _ = self.shared.events.send(NetEvent::Probe);
                            continue;
                        }
                    }
                    self.emit(&mut pending, &mut fp);
                }
                () = tick_opt(&mut wake_tick) => {
                    if let Some(excess) = gap.tick(self.clock.now()) {
                        tracing::info!(gap_secs = excess.as_secs(), "wall-clock jump (sleep/wake)");
                        pending.add(NetReason::Wake { gap: excess }, Instant::now(), debounce);
                    }
                }
                () = tick_opt(&mut probe_tick) => {
                    let now_fp = RouteFingerprint::probe(self.probe.as_ref());
                    if now_fp != fp {
                        pending.add(NetReason::Route(now_fp.describe()), Instant::now(), debounce);
                    }
                    let _ = self.shared.events.send(NetEvent::Probe);
                }
                res = self.cfg.changed() => {
                    if res.is_err() { break; }
                    cfg = self.cfg.borrow_and_update().clone();
                }
            }
        }
        drop(guard);
    }

    /// Adds a trigger. An OS notification is sampled against the last published fingerprint so a
    /// down/up flap inside the debounce window is remembered.
    fn hint(&self, pending: &mut Pending, fp: &RouteFingerprint, r: NetReason, debounce: Duration) {
        if matches!(r, NetReason::Os(_)) && !pending.saw_diff {
            let sample = RouteFingerprint::probe(self.probe.as_ref());
            if sample != *fp {
                pending.saw_diff = true;
            }
        }
        pending.add(r, Instant::now(), debounce);
    }

    /// Publishes the coalesced change and returns its generation.
    fn emit(&self, pending: &mut Pending, fp: &mut RouteFingerprint) -> u64 {
        let mut reasons: Vec<String> = Vec::new();
        for r in pending.take() {
            let l = r.label();
            if !reasons.contains(&l) {
                reasons.push(l);
            }
        }
        let generation = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let new_fp = RouteFingerprint::probe(self.probe.as_ref());
        let old = std::mem::replace(fp, new_fp.clone());
        let change = NetChange {
            generation,
            reasons,
            old_local_ip: old.v4.map(|a| a.to_string()),
            new_local_ip: new_fp.v4.map(|a| a.to_string()),
        };
        tracing::info!(
            generation,
            reasons = %change.reasons.join(","),
            old = ?change.old_local_ip,
            new = ?change.new_local_ip,
            "network changed"
        );
        let _ = self.shared.events.send(NetEvent::Changed(change));
        generation
    }
}

#[cfg(test)]
mod tests;
