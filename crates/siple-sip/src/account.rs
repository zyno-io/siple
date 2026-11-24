//! The account actor: owns one rsipstack endpoint (transport layer + dialog layer), the
//! registration state machine and the incoming-request router. All mutation goes through its
//! command channel; readers use the status watch.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use crate::authz::AuthState;
use chrono::Utc;
use rsipstack::dialog::authenticate::Credential;
use rsipstack::dialog::dialog_layer::DialogLayer;
use rsipstack::sip::{Header, Host, SipMessage, Transport as SipTransport, Uri};
use rsipstack::transaction::EndpointBuilder;
use rsipstack::transaction::endpoint::{EndpointOption, MessageInspector, TransportEventInspector};
use rsipstack::transport::stream::StreamConnection;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{
    SipAddr, SipConnection, TcpListenerConnection, TlsListenerConnection, TransportEvent,
    TransportLayer, WireEvent, WireTap,
};
use siple_config::{AccountConfig, GeneralConfig, KeepaliveMethod, MediaConfig, Transport};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::calls::{AccountCalls, CallEnv, CallManager, RtEnv};
use crate::client::{Client, ContactSel, RegIdentity, RegOk, RegResult, build_contact};
use crate::dns::{self, SipDomainResolver, SystemSrvSource, split_host_port};
use crate::error::{Result, SipError};
use crate::net::{NetProbe, NetService, is_unreachable_io, is_unreachable_text};
use crate::reg_logic;
use crate::router::{AccountCtx, HandlerSlot, Router};
use crate::subscribe::{SubEnv, SubManager};
use crate::transport::{AccountUdp, client_tls_config, init_crypto, listener_tls_config};
use crate::types::{AccountStatus, OptionsResponse, RegState, SipEvent, UpdateOutcome, WireSink};
use crate::update;

/// Bound on the best-effort unregister on remove / disable / shutdown.
/// Host part of generated Call-IDs (rsipstack's default would advertise its own domain).
pub(crate) const CALLID_SUFFIX: &str = "siple";

const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(3);

/// Binding a dead socket's address again: attempts while the old descriptor is still held, and
/// the wait between them.
const REBIND_TRIES: u32 = 20;
const REBIND_RETRY: Duration = Duration::from_millis(50);

/// A replaced UDP socket that dies within this long counts as dying right away.
const QUICK_SOCKET_DEATH: Duration = Duration::from_secs(10);

/// Wait before replacing a socket that died for the `deaths`-th time in a row right after a
/// replacement: none for the first death, then 0.5 s doubling up to 30 s.
fn socket_replace_delay(deaths: u32) -> Duration {
    if deaths == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(500)
        .saturating_mul(1 << (deaths - 1).min(6))
        .min(Duration::from_secs(30))
}

/// Test hook: give up on a REGISTER after this long (0 = the transaction's own 32 s).
static REGISTER_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[doc(hidden)]
pub fn set_register_timeout_ms(ms: u64) {
    REGISTER_TIMEOUT_MS.store(ms, std::sync::atomic::Ordering::Relaxed);
}

/// Stack-wide state shared with every account actor.
pub(crate) struct Shared {
    pub events: UnboundedSender<SipEvent>,
    pub wire_sink: Arc<dyn WireSink>,
    pub default_ua: String,
    pub handler: HandlerSlot,
    pub dns: Arc<SystemSrvSource>,
    pub calls: Arc<CallManager>,
    /// Network-change source (generation counter, debounced events).
    pub net: NetService,
    /// Route / DNS probe (fakeable).
    pub probe: Arc<dyn NetProbe>,
}

pub(crate) enum Cmd {
    RegisterNow,
    /// Republish the status (call code changed something it reports).
    Refresh,
    Unregister,
    SetTrace(bool),
    Update {
        cfg: Box<AccountConfig>,
        reply: oneshot::Sender<UpdateOutcome>,
    },
    SetGeneral(Box<GeneralConfig>),
    /// New `[media]` values for subsequent calls.
    SetMedia(Box<MediaConfig>),
    SendOptions {
        target: String,
        reply: oneshot::Sender<Result<OptionsResponse>>,
    },
    CallsChanged(usize),
    SetDnd {
        enabled: bool,
        reply: oneshot::Sender<()>,
    },
    DropFlow(oneshot::Sender<bool>),
    /// Test hook: make the UDP socket fail like one the OS has torn down. Replies whether there
    /// was one.
    KillUdp(oneshot::Sender<bool>),
    /// The network changed (debounced): re-probe now and re-register.
    NetworkChanged {
        generation: u64,
        reasons: Vec<String>,
    },
    /// Periodic probe tick: re-probe the next hop; act only on a difference.
    NetProbeTick,
    /// Immediate re-register (`account.reregister`).
    Reregister,
    Shutdown(oneshot::Sender<()>),
}

enum Internal {
    Built {
        epoch: u64,
        result: Result<Box<Runtime>>,
    },
    RegDone {
        epoch: u64,
        /// Sequence of the request (`Actor::reg_seq` when it started): a result of a request
        /// that was dropped on a rebind is stale and ignored.
        seq: u64,
        unreg: bool,
        result: Box<Result<RegResult>>,
    },
    FlowClosed {
        epoch: u64,
        local: SipAddr,
        remote: Option<SipAddr>,
    },
    /// A listener's socket died: rsipstack ended its receive / accept loop (see
    /// `rsipstack::transport::io_error`).
    ListenerDead { epoch: u64, local: SipAddr },
    /// The delayed replacement of a socket that died right after it was replaced.
    ReplaceSocket { epoch: u64 },
    /// Result of re-probing the next hop / local address.
    Probed {
        epoch: u64,
        /// Build sequence the probe started under; a result from before the current runtime was
        /// built is stale (it describes a network the runtime was not built on).
        seq: u64,
        cause: Cause,
        result: Result<Probed>,
    },
    /// Outcome of one keep-alive.
    Keepalive { epoch: u64, outcome: KaOutcome },
}

/// Why a re-probe was started.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cause {
    /// A REGISTER failed at transport level (reactive; only a changed probe acts).
    Failure,
    /// Periodic probe tick (only a changed probe, or waiting for the network, acts).
    Tick,
    /// A network-change signal, a dead flow or an explicit re-register: re-register at once.
    Network {
        generation: u64,
        reason: String,
        kind: NetKind,
    },
}

/// How strong a network signal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetKind {
    /// OS event, wake, `network.refresh`: re-register now, but a pending server `Retry-After`
    /// still wins and live calls keep their TCP/TLS connection.
    Signal,
    /// `account.reregister` / `register_now`: the user asked; `Retry-After` is overridden.
    Manual,
    /// A flow proven dead (keep-alive threshold, unreachable send error): it may be redialled
    /// even with live calls, since their signalling over it is already broken.
    Dead,
}

/// What a probe found, relative to what the runtime was built on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Same next hop and local address.
    Same,
    /// Different next hop or local address (or no runtime yet): the transport must be rebuilt.
    Changed,
    /// The OS has no route toward the next hop.
    NoRoute(String),
    /// The probe failed for another reason (DNS, config): the old runtime is kept.
    Failed(String),
}

/// Compares a probe result with what the runtime was built on (`None` = no runtime).
pub(crate) fn verdict(was: Option<&Probed>, now: &Result<Probed>) -> Verdict {
    match now {
        Ok(n) if was.is_some_and(|w| same_path(w, n)) => Verdict::Same,
        Ok(_) => Verdict::Changed,
        Err(SipError::NoRoute(m)) => Verdict::NoRoute(m.clone()),
        Err(e) => Verdict::Failed(e.to_string()),
    }
}

/// Same network path: same local / advertised address, and the current next hop is still among
/// the resolved candidates (equal-priority SRV records come back in a different weighted-random
/// order on every lookup, which is not a change).
fn same_path(was: &Probed, now: &Probed) -> bool {
    was.transport == now.transport
        && was.local_ip == now.local_ip
        && was.advertise_ip == now.advertise_ip
        && (was.target == now.target || was.target.is_some_and(|t| now.candidates.contains(&t)))
}

/// A failed keep-alive: `unreachable` when the error text says the OS has no route / address.
fn ka_miss(error: String) -> KaOutcome {
    KaOutcome::Miss {
        unreachable: is_unreachable_text(&error),
        error,
    }
}

/// Outcome of one keep-alive, as reported to the actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KaOutcome {
    /// CRLF sent / any SIP response to the OPTIONS.
    Alive,
    /// OPTIONS unanswered, or the send failed (`unreachable`: the OS says no route / address).
    Miss { unreachable: bool, error: String },
}

// --- rsipstack glue -----------------------------------------------------------------------------

/// Forwards wire events to the stack's sink, labelled with the account.
struct TapProxy {
    account: String,
    sink: Arc<dyn WireSink>,
}

impl WireTap for TapProxy {
    fn on_wire(&self, ev: WireEvent<'_>) {
        self.sink.on_wire(&self.account, &ev);
    }
}

/// Rewrites `User-Agent` on every outgoing request and response from a shared cell (hot
/// `user_agent` change without an endpoint rebuild), and keeps the digest response of a 2xx ACK
/// away from hops that are not the provider (`authz::strip_foreign_ack_credentials`).
struct UaInspector(Arc<RwLock<String>>, Arc<AuthState>);

impl UaInspector {
    fn apply(&self, headers: &mut rsipstack::sip::Headers) {
        let ua = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        headers.retain(|h| !matches!(h, Header::UserAgent(_)));
        headers.push(Header::UserAgent(ua.into()));
    }
}

impl MessageInspector for UaInspector {
    fn before_send(&self, msg: SipMessage, dest: Option<&SipAddr>) -> SipMessage {
        match msg {
            SipMessage::Request(mut r) => {
                self.apply(&mut r.headers);
                crate::authz::strip_foreign_ack_credentials(&self.1, &mut r, dest);
                SipMessage::Request(r)
            }
            SipMessage::Response(mut r) => {
                self.apply(&mut r.headers);
                SipMessage::Response(r)
            }
        }
    }

    fn after_received(&self, msg: SipMessage, _from: Option<&SipAddr>) -> SipMessage {
        msg
    }
}

/// Reports closed stream connections, and listeners whose socket died, to the actor.
struct FlowWatcher {
    epoch: u64,
    tx: UnboundedSender<Internal>,
}

#[async_trait::async_trait]
impl TransportEventInspector for FlowWatcher {
    async fn handle(&self, event: TransportEvent) -> Option<TransportEvent> {
        if let TransportEvent::Closed(c) = &event {
            let listener = matches!(
                c,
                SipConnection::Udp(_)
                    | SipConnection::TcpListener(_)
                    | SipConnection::TlsListener(_)
            );
            let _ = self.tx.send(if listener {
                Internal::ListenerDead {
                    epoch: self.epoch,
                    local: c.get_addr().clone(),
                }
            } else {
                Internal::FlowClosed {
                    epoch: self.epoch,
                    local: c.get_addr().clone(),
                    remote: c.get_remote_addr().cloned(),
                }
            });
        }
        Some(event)
    }
}

/// An old UDP listener whose receive loop still runs (see [`Runtime::lingering`]).
struct Linger {
    ip: IpAddr,
    addr: SocketAddr,
    tok: CancellationToken,
    conn: SipConnection,
    sock: Arc<AccountUdp>,
    /// True while it lingers; set false when it is re-installed, which stops its expiry task.
    lingering: Arc<std::sync::atomic::AtomicBool>,
}

/// Everything built for one transport generation.
pub(crate) struct Runtime {
    token: CancellationToken,
    endpoint: rsipstack::transaction::Endpoint,
    target: Option<SipAddr>,
    public_host: Option<Host>,
    ua: Arc<RwLock<String>>,
    tap: Arc<TapProxy>,
    contact_cell: Arc<Mutex<Option<Uri>>>,
    local_addr: Option<SocketAddr>,
    /// Cancels the receive loop of the UDP socket (replaced when the socket moves in place).
    listener_token: CancellationToken,
    /// The current UDP listener (kept so it can linger after a move and be re-used on a flap back).
    listener_conn: Option<SipConnection>,
    /// Its socket (re-bound on the same address when it dies: `Actor::on_listener_dead`).
    listener_sock: Option<Arc<AccountUdp>>,
    /// Bumped when the stream connections are retired (see `RtEnv::stream_gen`).
    stream_gen: u64,
    /// Old UDP listeners still receiving after a move. A flap back to their address re-installs
    /// them (same socket, same port: calls bound to their Contact stay reachable, a fixed
    /// `local_port` is not bound twice); otherwise they are cancelled once no call uses them.
    lingering: Vec<Linger>,
    /// Router context (dialog layer, dialog-state sender) for calls.
    ctx: AccountCtx,
    /// Address RTP sockets bind to.
    bind_ip: IpAddr,
    /// Address written into SDP.
    advertise_ip: IpAddr,
    /// What the runtime was built on (re-probed after transport failures).
    probed: Probed,
    /// Config this runtime was built from (rebuild-only fields are read from here). With
    /// `auto`, `transport` is the concrete one that was picked.
    built_for: AccountConfig,
    /// The account is configured with `transport = "auto"`.
    auto: bool,
}

impl Runtime {
    /// The endpoint's transport layer (rsipstack's `TransportLayer` is single-owner: the
    /// endpoint holds it).
    fn tl(&self) -> &TransportLayer {
        &self.endpoint.inner.transport_layer
    }
}

/// Dropping a runtime stops it. rsipstack's endpoint (and the transport layer it owns) is kept
/// alive by its own serve and router tasks, which wait on this token, so nothing else would end
/// them, e.g. for a runtime whose build finished after the account actor had gone.
impl Drop for Runtime {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

pub(crate) fn sip_transport(t: Transport) -> SipTransport {
    match t {
        Transport::Tcp => SipTransport::Tcp,
        Transport::Tls => SipTransport::Tls,
        // Resolved to a concrete transport before anything is built (`probe`).
        Transport::Udp | Transport::Auto => SipTransport::Udp,
    }
}

fn effective_ua(cfg: &AccountConfig, general: &GeneralConfig, fallback: &str) -> String {
    let ua = cfg.effective_user_agent(general);
    if ua.is_empty() {
        fallback.to_string()
    } else {
        ua.to_string()
    }
}

/// Next hop (outbound proxy, else registrar) as configured.
fn hop_of(cfg: &AccountConfig) -> String {
    if cfg.outbound_proxy.is_empty() {
        cfg.effective_registrar().to_string()
    } else {
        cfg.outbound_proxy.clone()
    }
}

fn public_host_of(cfg: &AccountConfig) -> Option<Host> {
    (!cfg.public_address.is_empty()).then(|| match cfg.public_address.parse::<IpAddr>() {
        Ok(ip) => Host::IpAddr(ip),
        Err(_) => Host::Domain(cfg.public_address.clone().into()),
    })
}

/// Network facts a runtime is built on; compared on re-probe to notice changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Probed {
    /// Resolved next hop (`None` in peer mode without a resolvable registrar).
    pub(crate) target: Option<SocketAddr>,
    /// Every address the next hop resolved to, best first (`target` is the first).
    candidates: Vec<SocketAddr>,
    /// Addresses of the provider for digest authentication: `candidates` plus what the other
    /// configured hosts (registrar / domain) resolve to. Never part of change detection.
    pub(crate) provider_addrs: Vec<SocketAddr>,
    /// Address sockets bind to / the OS routes from toward the next hop.
    pub(crate) local_ip: IpAddr,
    /// Address written into SDP.
    advertise_ip: IpAddr,
    /// The transport this probe selected: the configured one, or for `auto` what DNS SRV chose.
    /// A different value on re-probe is a changed path.
    pub(crate) transport: Transport,
}

/// Resolves the next hop and the local / advertised addresses. A failure is an
/// error, never a silent loopback fallback: callers retry with back-off.
pub(crate) async fn probe(
    cfg: &AccountConfig,
    net: &dyn NetProbe,
    hint: Option<SocketAddr>,
) -> Result<Probed> {
    let hop = hop_of(cfg);
    let hop_host = split_host_port(&hop);
    let mut transport = if cfg.transport.is_auto() {
        Transport::Udp
    } else {
        cfg.transport
    };
    let (target, candidates) = if hop.is_empty() {
        (None, Vec::new())
    } else {
        let resolved = if cfg.transport.is_auto() {
            net.resolve_auto(&hop_host.0, hop_host.1)
                .await
                .map(|(t, addrs)| {
                    transport = t;
                    addrs
                })
        } else {
            net.resolve(&hop_host.0, hop_host.1, cfg.transport).await
        };
        match resolved {
            Ok(addrs) => (Some(addrs[0]), addrs),
            // Peer mode does not need the registrar to exist.
            Err(e) if !cfg.register => {
                tracing::debug!(account = %cfg.id, error = %e, "registrar does not resolve (register = false); continuing");
                (None, Vec::new())
            }
            // Offline, name resolution fails before the route check would: tell "no route" (wait
            // for the network) from a real DNS problem (back off).
            Err(e) => {
                if cfg.bind_address.is_empty() {
                    // "No route" means no route in either family: an IPv6-only network has no
                    // IPv4 route but is perfectly online.
                    let mut targets: Vec<SocketAddr> = hint.into_iter().collect();
                    targets.push(([192, 0, 2, 1], 53).into());
                    targets.push(
                        (std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1), 53).into(),
                    );
                    let errs: Vec<_> = targets
                        .iter()
                        .filter_map(|t| net.local_ip_toward(*t).err())
                        .filter(is_unreachable_io)
                        .collect();
                    if errs.len() == targets.len() {
                        return Err(SipError::NoRoute(format!(
                            "cannot resolve {} and no route toward {}: {}",
                            hop_host.0, targets[0], errs[0]
                        )));
                    }
                }
                return Err(e);
            }
        }
    };

    // Every address of the provider, not just the next hop: a Record-Route / inbound call may
    // come from another edge, and a challenge from it is the provider's. With an outbound proxy
    // the next hop is the proxy, so the registrar is resolved too. The AOR `domain` is not:
    // with a separate registrar it may be an unrelated web host. Best effort, bounded.
    let mut provider_addrs = candidates.clone();
    if cfg.register && !cfg.outbound_proxy.is_empty() {
        let (host, port) = split_host_port(cfg.effective_registrar());
        if !host.is_empty()
            && host.parse::<IpAddr>().is_err()
            && host != hop_host.0
            && let Ok(Ok(addrs)) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                net.resolve(&host, port, cfg.transport),
            )
            .await
        {
            provider_addrs.extend(addrs);
        }
    }

    let local_ip: IpAddr = if !cfg.bind_address.is_empty() {
        cfg.bind_address.parse().map_err(|_| {
            SipError::Config(format!(
                "bind_address {:?} is not an IP address",
                cfg.bind_address
            ))
        })?
    } else {
        let toward = target.unwrap_or_else(|| ([8, 8, 8, 8], 53).into());
        net.local_ip_toward(toward).map_err(|e| {
            let msg = format!("cannot determine local address toward {toward}: {e}");
            if is_unreachable_io(&e) {
                SipError::NoRoute(msg)
            } else {
                SipError::Transport(msg)
            }
        })?
    };

    // SDP address: the public address when it is an IP (or resolves to one), else the local one.
    let advertise_ip = match public_host_of(cfg) {
        Some(Host::IpAddr(ip)) => ip,
        Some(Host::Domain(d)) => tokio::net::lookup_host((d.to_string(), 0))
            .await
            .map_err(|e| SipError::Dns(format!("public_address {d}: {e}")))?
            .next()
            .map(|a| a.ip())
            .ok_or_else(|| SipError::Dns(format!("public_address {d}: no addresses")))?,
        None => local_ip,
    };
    Ok(Probed {
        target,
        candidates,
        provider_addrs,
        local_ip,
        advertise_ip,
        transport,
    })
}

/// Builds sockets, transport layer, endpoint, dialog layer and router for `cfg`.
#[allow(clippy::too_many_arguments)]
async fn build_runtime(
    cfg: AccountConfig,
    ua: String,
    trace: bool,
    epoch: u64,
    shared: Arc<Shared>,
    int_tx: UnboundedSender<Internal>,
    calls: Arc<AccountCalls>,
    subs: Arc<SubManager>,
) -> Result<Runtime> {
    init_crypto();
    let hop = hop_of(&cfg);
    if cfg.register && hop.is_empty() {
        return Err(SipError::Config(
            "registrar or domain required to register".into(),
        ));
    }
    let hop_host = split_host_port(&hop);
    let probed = probe(&cfg, shared.probe.as_ref(), None).await?;
    let public_host = public_host_of(&cfg);
    // `transport = "auto"`: build for the transport the probe picked; the runtime remembers that
    // it was auto so a re-probe may re-select (see `start_probe`).
    let auto = cfg.transport.is_auto();
    let mut cfg = cfg;
    cfg.transport = probed.transport;

    let token = CancellationToken::new();
    let result = build_inner(
        &cfg,
        ua,
        trace,
        epoch,
        &shared,
        int_tx,
        &token,
        probed.target,
        (probed.local_ip, probed.advertise_ip),
        public_host,
        hop_host.0,
        calls,
        subs,
    )
    .await
    .map(|mut rt| {
        rt.auto = auto;
        rt.probed.candidates = probed.candidates;
        rt.probed.provider_addrs = probed.provider_addrs;
        rt
    });
    if result.is_err() {
        token.cancel();
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn build_inner(
    cfg: &AccountConfig,
    ua: String,
    trace: bool,
    epoch: u64,
    shared: &Arc<Shared>,
    int_tx: UnboundedSender<Internal>,
    token: &CancellationToken,
    target_sock: Option<SocketAddr>,
    (local_ip, advertise_ip): (IpAddr, IpAddr),
    public_host: Option<Host>,
    hop_host: String,
    calls: Arc<AccountCalls>,
    subs: Arc<SubManager>,
) -> Result<Runtime> {
    let transport = cfg.transport;
    let tl = TransportLayer::new_with_domain_resolver(
        token.clone(),
        Box::new(SipDomainResolver::new(shared.dns.clone())),
    );
    // Since 0.7.7 rsipstack closes a TCP / TLS connection after 5 min without inbound bytes.
    // The registrar flow can legitimately be that quiet (servers that ignore CRLF pings, long
    // re-REGISTER intervals); losing it would drop incoming calls. Liveness is ours to judge
    // (keep-alive + flow watcher), so keep the old "never idle out" behaviour.
    tl.set_stream_idle_timeout(Duration::ZERO);
    let tap = Arc::new(TapProxy {
        account: cfg.id.clone(),
        sink: shared.wire_sink.clone(),
    });
    if trace {
        tl.set_wire_tap(tap.clone());
    }
    if transport == Transport::Tls {
        let sni = hop_host
            .parse::<IpAddr>()
            .is_err()
            .then_some(hop_host)
            .filter(|h| !h.is_empty());
        tl.set_tls_config(client_tls_config(cfg, sni, target_sock)?);
    }

    // Listeners. UDP always (it is also the only send socket). TCP/TLS are client-only unless a
    // fixed `local_port` asks for a listener (peer mode / tests).
    let mut local_addr = None;
    let listener_token = token.child_token();
    let mut listener_conn = None;
    let mut listener_sock = None;
    match transport {
        Transport::Udp => {
            let sock = AccountUdp::bind(SocketAddr::new(local_ip, cfg.local_port))?;
            local_addr = Some(sock.local());
            let conn = UdpConnection::from_socket(sock.clone(), None, Some(listener_token.clone()))
                .await?;
            let conn = SipConnection::Udp(conn);
            listener_conn = Some(conn.clone());
            listener_sock = Some(sock);
            tl.add_transport(conn);
        }
        Transport::Tcp if cfg.local_port != 0 => {
            let addr = SocketAddr::new(local_ip, cfg.local_port);
            local_addr = Some(addr);
            tl.add_transport(SipConnection::TcpListener(
                TcpListenerConnection::new(addr, None).await?,
            ));
        }
        Transport::Tls if cfg.local_port != 0 => {
            let addr = SocketAddr::new(local_ip, cfg.local_port);
            local_addr = Some(addr);
            let conf = listener_tls_config(cfg, local_ip)?;
            tl.add_transport(SipConnection::TlsListener(
                TlsListenerConnection::new(addr, None, conf).await?,
            ));
        }
        _ => {}
    }

    let ua_cell = Arc::new(RwLock::new(ua.clone()));
    let mut builder = EndpointBuilder::new();
    let endpoint = builder
        .with_cancel_token(token.clone())
        .with_transport_layer(tl)
        .with_user_agent(&ua)
        .with_inspector(Box::new(UaInspector(ua_cell.clone(), calls.auth.clone())))
        .with_transport_inspector(Box::new(FlowWatcher { epoch, tx: int_tx }))
        .with_option(EndpointOption {
            use_sips_for_tls: cfg.use_sips_uri,
            callid_suffix: Some(CALLID_SUFFIX.into()),
            ..Default::default()
        })
        .build();
    let incoming = endpoint.incoming_transactions()?;
    let serving = endpoint.inner.clone();
    tokio::spawn(async move {
        let _ = serving.serve().await;
    });

    let dialogs = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let contact_cell = Arc::new(Mutex::new(local_contact(
        cfg,
        public_host.as_ref(),
        local_addr,
    )));
    let (router, states) = Router::new(
        cfg.id.clone(),
        shared.events.clone(),
        dialogs,
        shared.handler.clone(),
        contact_cell.clone(),
        calls,
        subs,
    );
    router.spawn(token.clone(), incoming, states);

    Ok(Runtime {
        token: token.clone(),
        endpoint,
        target: target_sock.map(|a| SipAddr::new(sip_transport(transport), a.into())),
        public_host,
        ua: ua_cell,
        tap,
        contact_cell,
        local_addr,
        listener_token,
        listener_conn,
        listener_sock,
        stream_gen: 0,
        lingering: Vec::new(),
        ctx: router.ctx.clone(),
        bind_ip: local_ip,
        advertise_ip,
        probed: Probed {
            target: target_sock,
            candidates: target_sock.into_iter().collect(),
            provider_addrs: target_sock.into_iter().collect(),
            local_ip,
            advertise_ip,
            transport,
        },
        built_for: cfg.clone(),
        auto: false,
    })
}

/// The Contact an account advertises before it registered: its listener address (or the public
/// host with the listener's port). `None` for client-only TCP / TLS (the flow decides).
fn local_contact(
    cfg: &AccountConfig,
    public_host: Option<&Host>,
    local_addr: Option<SocketAddr>,
) -> Option<Uri> {
    let la = local_addr?;
    let adv = public_host
        .cloned()
        .map(|h| rsipstack::sip::HostWithPort {
            host: h,
            port: Some(la.port().into()),
        })
        .unwrap_or_else(|| la.into());
    Some(build_contact(
        &cfg.username,
        adv,
        sip_transport(cfg.transport),
        cfg.use_sips_uri,
    ))
}

/// The account's digest credential; `auth` decides which challenges it may answer.
pub(crate) fn credential(cfg: &AccountConfig, auth: &Arc<AuthState>) -> Credential {
    Credential {
        username: cfg.username.clone(),
        password: cfg.password.expose().to_string(),
        realm: None,
        auth_username: Some(cfg.effective_auth_username().to_string()),
        guard: Some(auth.guard()),
    }
}

// --- the actor ----------------------------------------------------------------------------------

struct Reg {
    state: RegState,
    inflight: bool,
    /// The user asked for `unregister`; sticky until `register_now`.
    manual_off: bool,
    fail_count: u32,
    /// Public address learned from `received` / `rport`; used as Contact until the flow changes.
    learned: Option<SocketAddr>,
    /// A rewrite re-REGISTER is already underway this cycle (no rewrite loops).
    rewritten: bool,
    /// Raised by a 423 so later refreshes ask for a lifetime the registrar accepts.
    expires_floor: Option<u32>,
    /// Config changed while a REGISTER was in flight: run again when it lands.
    restart: bool,
    /// Stream connections (local addresses) that closed while the REGISTER in flight was
    /// outstanding; a 200 that arrives over one of them left us on a dead flow.
    closed_flows: Vec<SipAddr>,
    /// A re-probe of the next hop is outstanding (one at a time).
    reprobing: bool,
    /// A network-change probe asked for while another probe was outstanding; run when it lands.
    net_pending: Option<Cause>,
    /// A network signal arrived while calls used the registrar connection: redial at call end.
    redial_pending: bool,
    /// End of a server-supplied `Retry-After`; a plain network signal does not cut it short.
    retry_after_until: Option<Instant>,
    last_flow_loss: Option<Instant>,
    /// Consecutive unanswered OPTIONS keep-alives.
    ka_misses: u32,
}

pub(crate) struct Actor {
    id: String,
    cfg: AccountConfig,
    general: GeneralConfig,
    media: MediaConfig,
    shared: Arc<Shared>,
    status_tx: watch::Sender<AccountStatus>,
    rt: Option<Runtime>,
    building: bool,
    epoch: u64,
    trace: bool,
    reg: Reg,
    calls: usize,
    /// Cell shared with the call code (effective DND, config + runtime snapshot).
    call_cell: Arc<AccountCalls>,
    /// Runtime `set_dnd` override; cleared when the config's `dnd` changes.
    override_dnd: Option<bool>,
    pending_rebuild: bool,
    /// The pending rebuild comes from a config change (a probe-driven one is never forced by
    /// `reconfig_force`).
    pending_config: bool,
    identity: RegIdentity,
    int_tx: UnboundedSender<Internal>,
    /// Bumped whenever a runtime finishes building; stamps probes (see [`Internal::Probed`]).
    build_seq: u64,
    timer: Option<Instant>,
    flow: Option<SipAddr>,
    contact: Option<String>,
    public_addr: Option<SocketAddr>,
    keepalive: Option<tokio::time::Interval>,
    subs: Arc<SubManager>,
    /// Contact registered on the current flow; subscriptions use it (and restart when it changes).
    sub_contact: Option<Uri>,
    /// The transport moved in place under live calls: they are moved with a re-INVITE once the
    /// re-registration settled (the new Contact is known), or at this instant at the latest.
    move_due: Option<Instant>,
    /// Abort handle of the REGISTER in flight (dropped on a rebind: it runs on the old path).
    reg_abort: Option<tokio::task::AbortHandle>,
    /// Sequence of the request in flight (results of older ones are dropped).
    reg_seq: u64,
    /// The request in flight is an unregister (never aborted by a rebind).
    reg_unreg: bool,
    /// Bumped on every address-changing rebind; tags the pending moves of the calls.
    rebind_seq: u64,
    /// When the UDP socket was last replaced after it died.
    socket_replaced_at: Option<Instant>,
    /// Deaths in a row of replaced sockets within [`QUICK_SOCKET_DEATH`] of their replacement.
    socket_deaths: u32,
}

/// How long the move of live calls waits for the re-REGISTER (the new Contact) before it goes
/// ahead with what it has.
const MOVE_WAIT_REGISTER: Duration = Duration::from_secs(3);

/// Handle held by the stack.
pub(crate) struct AccountHandle {
    pub tx: UnboundedSender<Cmd>,
    pub calls: Arc<AccountCalls>,
    pub status: watch::Receiver<AccountStatus>,
    pub cfg_id: String,
    pub subs: Arc<SubManager>,
}

fn initial_status(cfg: &AccountConfig) -> AccountStatus {
    AccountStatus {
        id: cfg.id.clone(),
        enabled: cfg.enabled,
        transport: cfg.transport,
        transport_effective: (!cfg.transport.is_auto()).then_some(cfg.transport),
        registrar: cfg.effective_registrar().to_string(),
        aor: cfg.aor(),
        contact: None,
        registration: if cfg.enabled {
            RegState::Unregistered
        } else {
            RegState::Disabled
        },
        local_addr: None,
        public_addr: None,
        trace_enabled: cfg.sip_trace,
        pending_rebuild: false,
        dnd: cfg.dnd,
        mwi: None,
        srtp_optional_downgraded: Vec::new(),
    }
}

/// Spawns the actor; the returned future-less handle is usable immediately. `first_build`
/// resolves once the first runtime build attempt finished (success or failure).
pub(crate) fn spawn_account(
    cfg: AccountConfig,
    general: GeneralConfig,
    media: MediaConfig,
    shared: Arc<Shared>,
) -> (AccountHandle, oneshot::Receiver<()>) {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (int_tx, int_rx) = unbounded_channel();
    let (status_tx, status_rx) = watch::channel(initial_status(&cfg));
    let (ready_tx, ready_rx) = oneshot::channel();
    let auth = AuthState::new();
    auth.update(&cfg, None);
    let call_cell = AccountCalls::new(
        cfg.id.clone(),
        cmd_tx.clone(),
        cfg.dnd,
        credential(&cfg, &auth),
        auth,
    );
    let subs = SubManager::new(cfg.id.clone(), shared.events.clone(), call_cell.clone());
    subs.configure(&cfg);
    let handle = AccountHandle {
        tx: cmd_tx,
        calls: call_cell.clone(),
        status: status_rx,
        cfg_id: cfg.id.clone(),
        subs: subs.clone(),
    };
    let actor = Actor {
        id: cfg.id.clone(),
        trace: cfg.sip_trace,
        cfg,
        general,
        media,
        shared,
        status_tx,
        rt: None,
        building: false,
        epoch: 0,
        reg: Reg {
            state: RegState::Unregistered,
            inflight: false,
            manual_off: false,
            fail_count: 0,
            learned: None,
            rewritten: false,
            expires_floor: None,
            restart: false,
            closed_flows: Vec::new(),
            reprobing: false,
            net_pending: None,
            redial_pending: false,
            retry_after_until: None,
            last_flow_loss: None,
            ka_misses: 0,
        },
        calls: 0,
        call_cell,
        override_dnd: None,
        pending_rebuild: false,
        pending_config: false,
        identity: RegIdentity::new(),
        int_tx,
        build_seq: 0,
        timer: None,
        flow: None,
        contact: None,
        public_addr: None,
        keepalive: None,
        subs,
        sub_contact: None,
        move_due: None,
        reg_abort: None,
        reg_seq: 0,
        reg_unreg: false,
        rebind_seq: 0,
        socket_replaced_at: None,
        socket_deaths: 0,
    };
    tokio::spawn(actor.run(cmd_rx, int_rx, ready_tx));
    (handle, ready_rx)
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

impl Actor {
    async fn run(
        mut self,
        mut cmd_rx: UnboundedReceiver<Cmd>,
        mut int_rx: UnboundedReceiver<Internal>,
        ready: oneshot::Sender<()>,
    ) {
        let mut ready = Some(ready);
        self.start();
        if !self.cfg.enabled
            && let Some(r) = ready.take()
        {
            let _ = r.send(());
        }
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Shutdown(reply)) => {
                        self.teardown(true).await;
                        let _ = reply.send(());
                        break;
                    }
                    Some(c) => self.on_cmd(c).await,
                    None => {
                        self.teardown(true).await;
                        break;
                    }
                },
                Some(ev) = int_rx.recv() => {
                    let was_build = matches!(ev, Internal::Built { .. });
                    self.on_internal(ev).await;
                    if was_build && let Some(r) = ready.take() {
                        let _ = r.send(());
                    }
                }
                () = sleep_until_opt(self.timer) => {
                    self.timer = None;
                    self.on_timer();
                }
                () = sleep_until_opt(self.move_due) => self.move_calls_now(),
                () = tick_opt(&mut self.keepalive) => self.on_keepalive(),
            }
        }
        tracing::debug!(account = %self.id, "account actor stopped");
    }

    fn clear_pending_rebuild(&mut self) {
        self.pending_rebuild = false;
        self.pending_config = false;
    }

    fn want_registered(&self) -> bool {
        self.cfg.enabled && self.cfg.register && !self.reg.manual_off
    }

    fn start(&mut self) {
        if !self.cfg.enabled {
            self.set_state(RegState::Disabled);
            return;
        }
        self.begin_build();
    }

    // --- status -------------------------------------------------------------------------------

    fn publish(&self) {
        let status = AccountStatus {
            id: self.id.clone(),
            enabled: self.cfg.enabled,
            transport: self.cfg.transport,
            transport_effective: self
                .rt
                .as_ref()
                .map(|r| r.built_for.transport)
                .or_else(|| (!self.cfg.transport.is_auto()).then_some(self.cfg.transport)),
            registrar: self.cfg.effective_registrar().to_string(),
            aor: self.cfg.aor(),
            contact: self.contact.clone(),
            registration: self.reg.state.clone(),
            local_addr: self
                .rt
                .as_ref()
                .and_then(|r| r.local_addr)
                .map(|a| a.to_string())
                .or_else(|| self.flow.as_ref().map(|f| f.addr.to_string())),
            public_addr: self.public_addr.map(|a| a.to_string()),
            trace_enabled: self.trace,
            pending_rebuild: self.pending_rebuild,
            dnd: self.effective_dnd(),
            mwi: self.subs.mwi().map(|m| m.summary()),
            srtp_optional_downgraded: self.call_cell.srtp_downgraded(),
        };
        self.status_tx.send_replace(status);
    }

    fn effective_dnd(&self) -> bool {
        self.override_dnd.unwrap_or(self.cfg.dnd)
    }

    /// Mirrors the effective DND into the call cell and reports a change.
    fn apply_dnd(&mut self) {
        let now = self.effective_dnd();
        let was = self
            .call_cell
            .dnd
            .swap(now, std::sync::atomic::Ordering::SeqCst);
        if was != now {
            tracing::info!(account = %self.id, dnd = now, "do not disturb");
            let _ = self.shared.events.send(SipEvent::Dnd {
                account: self.id.clone(),
                enabled: now,
            });
        }
        self.publish();
    }

    /// Publishes the config and runtime snapshot the call code works from.
    fn publish_env(&self) {
        let rt = self.rt.as_ref().map(|rt| {
            Arc::new(RtEnv {
                endpoint: rt.endpoint.inner.clone(),
                dialogs: rt.ctx.dialogs.clone(),
                state_tx: rt.ctx.state_tx.clone(),
                contact: rt.contact_cell.clone(),
                bind_ip: rt.bind_ip,
                advertise_ip: rt.advertise_ip,
                target: rt.target.clone(),
                transport: sip_transport(rt.built_for.transport),
                use_sips: rt.built_for.use_sips_uri,
                public_host: rt.public_host.clone(),
                stream_gen: rt.stream_gen,
                listener: rt.local_addr,
            })
        });
        let mut provider_addrs: Vec<SocketAddr> = Vec::new();
        if let Some(rt) = &self.rt {
            provider_addrs.extend(rt.probed.provider_addrs.iter().copied());
            provider_addrs.extend(rt.probed.target);
        }
        self.call_cell.auth.update(
            &self.cfg,
            self.rt
                .as_ref()
                .map(|rt| (&rt.built_for, provider_addrs.as_slice())),
        );
        self.call_cell
            .set_credential(credential(&self.cfg, &self.call_cell.auth));
        // Generation read here, after any clear: every clear path is followed by a publish (or
        // by `drop_runtime`, which publishes), so new snapshots carry the post-clear generation
        // and calls on an older snapshot hold an older one (their commits are rejected).
        let srtp_gen = self.call_cell.srtp_generation();
        // Calls read the transport from the config: give them the concrete one for `auto`.
        let mut env_cfg = self.cfg.clone();
        if let Some(rt) = &self.rt {
            env_cfg.transport = rt.built_for.transport;
        }
        self.call_cell.set_env(Some(CallEnv {
            cfg: Arc::new(env_cfg),
            media: Arc::new(self.media.clone()),
            rt,
            srtp_gen,
        }));
    }

    fn set_state(&mut self, new: RegState) {
        let transition = self.reg.state.is_transition_to(&new);
        if transition {
            match &new {
                RegState::Failed {
                    code,
                    reason,
                    retry_at,
                } => {
                    tracing::warn!(account = %self.id, code, %reason, %retry_at, "registration failed");
                }
                other => {
                    tracing::info!(account = %self.id, state = other.name(), "registration state")
                }
            }
        }
        self.reg.state = new.clone();
        self.publish();
        if transition {
            let _ = self.shared.events.send(SipEvent::Registration {
                account: self.id.clone(),
                state: new,
            });
        }
    }

    fn schedule(&mut self, delay: Duration) {
        self.timer = Some(Instant::now() + delay);
    }

    // --- build / teardown ---------------------------------------------------------------------

    fn begin_build(&mut self) {
        self.epoch += 1;
        self.building = true;
        self.reg.reprobing = false; // a probe of the previous epoch is dropped on arrival
        self.reg.net_pending = None;
        if self.want_registered() && !matches!(self.reg.state, RegState::Failed { .. }) {
            self.set_state(RegState::Registering);
        } else if !self.want_registered() && !matches!(self.reg.state, RegState::Failed { .. }) {
            self.set_state(RegState::Unregistered);
        }
        let ua = effective_ua(&self.cfg, &self.general, &self.shared.default_ua);
        let (cfg, trace, epoch, shared, tx, calls, subs) = (
            self.cfg.clone(),
            self.trace,
            self.epoch,
            self.shared.clone(),
            self.int_tx.clone(),
            self.call_cell.clone(),
            self.subs.clone(),
        );
        tokio::spawn(async move {
            let result = build_runtime(cfg, ua, trace, epoch, shared, tx.clone(), calls, subs)
                .await
                .map(Box::new);
            let _ = tx.send(Internal::Built { epoch, result });
        });
    }

    /// Stops the runtime. With `unregister`, first removes our binding (best effort, bounded).
    async fn teardown(&mut self, unregister: bool) {
        self.timer = None;
        self.keepalive = None;
        // Subscriptions go first, while the registration (and the flow) still exist.
        self.subs.release_all(true).await;
        if unregister && self.want_registered() && self.should_unregister() {
            let cfg = self.cfg.clone();
            self.unregister_inline(&cfg).await;
        }
        self.drop_runtime();
        if self.cfg.enabled {
            self.set_state(RegState::Unregistered);
        } else {
            self.set_state(RegState::Disabled);
        }
    }

    fn drop_runtime(&mut self) {
        self.epoch += 1; // stale results / close events of the old runtime are ignored
        self.building = false;
        if let Some(rt) = self.rt.take() {
            rt.token.cancel();
            // Ended calls (and their `RtEnv`) outlive the runtime in the call table and keep the
            // endpoint, hence the transport layer, alive; let go of the sockets explicitly so a
            // fixed `local_port` is free for the next runtime.
            for a in rt.tl().get_addrs() {
                rt.tl().del_transport(&a);
                // Address-scoped on purpose: the whole layer is being dismantled.
                #[allow(deprecated)]
                rt.tl().del_connection(&a);
            }
        }
        self.publish_env();
        self.sub_contact = None;
        self.sync_subs();
        self.flow = None;
        self.reg.inflight = false;
        self.reg.learned = None;
        self.reg.rewritten = false;
        self.public_addr = None;
        self.identity.clear_bound();
        self.reg.closed_flows.clear();
        self.reg.reprobing = false;
        self.reg.redial_pending = false;
        self.move_due = None;
    }

    /// Worth sending an unregister on teardown: a binding may exist and the registrar was not
    /// failing (an unreachable one would only cost the full timeout).
    fn should_unregister(&self) -> bool {
        self.maybe_bound()
            && self.rt.is_some()
            && !matches!(self.reg.state, RegState::Failed { .. })
    }

    /// The registrar may hold a binding of ours: one was sent, or a first REGISTER is still in
    /// flight (the lock in [`Client::register`] makes an unregister wait for it).
    fn maybe_bound(&self) -> bool {
        self.identity.bound().is_some() || self.reg.inflight
    }

    /// Removes the binding with `cfg`'s identity. Waits for an in-flight REGISTER first (they
    /// share the CSeq counter and must reach the registrar in order), all within the bound.
    async fn unregister_inline(&mut self, cfg: &AccountConfig) {
        let Some(client) = self.client_for(cfg) else {
            return;
        };
        match tokio::time::timeout(UNREGISTER_TIMEOUT, client.register(0, ContactSel::Bound)).await
        {
            Ok(Ok(RegResult::NothingBound)) => {}
            Ok(Ok(RegResult::Ok(_))) => tracing::info!(account = %self.id, "unregistered"),
            Ok(Ok(RegResult::Rejected { code, reason, .. })) => {
                tracing::warn!(account = %self.id, code, %reason, "unregister rejected")
            }
            Ok(Err(e)) => tracing::warn!(account = %self.id, error = %e, "unregister failed"),
            Err(_) => tracing::warn!(account = %self.id, "unregister timed out"),
        }
        self.identity.clear_bound();
    }

    fn client(&self) -> Option<Client> {
        self.client_for(&self.cfg)
    }

    /// A request client for `cfg` (rebuild-only fields come from the runtime).
    fn client_for(&self, cfg: &AccountConfig) -> Option<Client> {
        let rt = self.rt.as_ref()?;
        let registrar = cfg.effective_registrar();
        Some(Client {
            endpoint: rt.endpoint.inner.clone(),
            target: rt.target.clone(),
            transport: sip_transport(rt.built_for.transport),
            use_sips: rt.built_for.use_sips_uri,
            public_host: rt.public_host.clone(),
            cred: credential(cfg, &self.call_cell.auth),
            user: cfg.username.clone(),
            display_name: cfg.display_name.clone(),
            aor: Uri::try_from(cfg.aor().as_str()).ok()?,
            registrar_uri: (!registrar.is_empty())
                .then(|| Uri::try_from(format!("sip:{registrar}").as_str()).ok())
                .flatten(),
            extra_headers: cfg
                .extra_headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            supported: cfg.supported_tokens(),
            route: (rt.built_for.outbound_proxy_route && !rt.built_for.outbound_proxy.is_empty())
                .then(|| format!("sip:{}", rt.built_for.outbound_proxy)),
            identity: self.identity.clone(),
        })
    }

    fn stream_transport(&self) -> bool {
        self.rt
            .as_ref()
            .is_some_and(|r| r.built_for.transport != Transport::Udp)
    }

    fn reset_keepalive(&mut self) {
        self.reg.ka_misses = 0;
        let secs = u64::from(self.cfg.keepalive_secs);
        self.keepalive = (secs > 0 && self.cfg.register && self.cfg.enabled && self.rt.is_some())
            .then(|| {
                let mut i = tokio::time::interval_at(
                    Instant::now() + Duration::from_secs(secs),
                    Duration::from_secs(secs),
                );
                i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                i
            });
    }

    // --- timers -------------------------------------------------------------------------------

    fn on_timer(&mut self) {
        if !self.cfg.enabled {
            return;
        }
        if matches!(self.reg.state, RegState::WaitingForNetwork) {
            return; // suspended until a network signal or a successful probe
        }
        if self.rt.is_none() {
            if !self.building {
                self.begin_build();
            }
            return;
        }
        if self.want_registered() && !self.reg.inflight {
            self.start_register();
        }
    }

    fn start_register(&mut self) {
        let Some(client) = self.client() else { return };
        let Some(guard) = client.try_lock_register() else {
            // A superseded REGISTER of this identity is still finishing; do not overlap it.
            self.schedule(Duration::from_millis(250));
            return;
        };
        self.reg.inflight = true;
        self.reg.retry_after_until = None;
        // Whatever asked for a re-run is satisfied by the REGISTER starting now.
        self.reg.restart = false;
        self.reg.closed_flows.clear();
        if matches!(self.reg.state, RegState::Unregistered | RegState::Disabled) {
            self.set_state(RegState::Registering);
        }
        let expires = self.cfg.expires.max(self.reg.expires_floor.unwrap_or(0));
        let sel = self
            .reg
            .learned
            .map_or(ContactSel::Auto, ContactSel::Learned);
        self.reg_seq += 1;
        self.reg_unreg = false;
        let (epoch, seq, tx) = (self.epoch, self.reg_seq, self.int_tx.clone());
        let task = tokio::spawn(async move {
            let fut = client.register_held(guard, expires, sel);
            let ms = REGISTER_TIMEOUT_MS.load(std::sync::atomic::Ordering::Relaxed);
            let result = if ms > 0 {
                tokio::time::timeout(Duration::from_millis(ms), fut)
                    .await
                    .unwrap_or(Err(SipError::Timeout))
            } else {
                fut.await
            };
            let _ = tx.send(Internal::RegDone {
                epoch,
                seq,
                unreg: false,
                result: Box::new(result),
            });
        });
        self.reg_abort = Some(task.abort_handle());
    }

    fn spawn_unregister(&mut self) {
        let Some(client) = self.client() else {
            self.set_state(RegState::Unregistered);
            return;
        };
        if self.identity.bound().is_none() {
            self.set_state(RegState::Unregistered);
            return;
        }
        self.reg.inflight = true;
        self.reg_seq += 1;
        self.reg_unreg = true;
        let (epoch, seq, tx) = (self.epoch, self.reg_seq, self.int_tx.clone());
        let task = tokio::spawn(async move {
            let result = match tokio::time::timeout(
                UNREGISTER_TIMEOUT,
                client.register(0, ContactSel::Bound),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(SipError::Timeout),
            };
            let _ = tx.send(Internal::RegDone {
                epoch,
                seq,
                unreg: true,
                result: Box::new(result),
            });
        });
        self.reg_abort = Some(task.abort_handle());
    }

    /// After a transport-level failure: re-resolve the next hop and re-derive the local /
    /// advertised address off the actor's loop; a change rebuilds the runtime.
    fn maybe_reprobe(&mut self) {
        if self.rt.is_none() || !self.want_registered() {
            return;
        }
        self.start_probe(Cause::Failure);
    }

    /// Re-probes the next hop and local address off the actor's loop (one at a time; a network
    /// signal that arrives meanwhile runs right after). Works without a runtime (the first build
    /// failed): any usable result then means "build now".
    fn start_probe(&mut self, cause: Cause) {
        if !self.cfg.enabled {
            return;
        }
        if self.reg.reprobing {
            if matches!(cause, Cause::Network { .. }) {
                self.reg.net_pending = Some(cause);
            }
            return;
        }
        self.reg.reprobing = true;
        let (cfg, hint) = match &self.rt {
            Some(rt) => {
                let mut cfg = rt.built_for.clone();
                // `auto` may re-select the transport (new SRV records, another network), but
                // never under live calls: they ride the current transport, which is then kept.
                if rt.auto && self.calls == 0 {
                    cfg.transport = Transport::Auto;
                }
                (cfg, rt.probed.target)
            }
            None => (self.cfg.clone(), None),
        };
        let (net, epoch, seq, tx) = (
            self.shared.probe.clone(),
            self.epoch,
            self.build_seq,
            self.int_tx.clone(),
        );
        tokio::spawn(async move {
            let result = probe(&cfg, net.as_ref(), hint).await;
            if let Err(e) = &result {
                tracing::debug!(account = %cfg.id, error = %e, "re-probe failed");
            }
            let _ = tx.send(Internal::Probed {
                epoch,
                seq,
                cause,
                result,
            });
        });
    }

    /// A network-change signal for this account: tell listeners, then re-probe at once.
    fn on_network_changed(&mut self, generation: u64, reasons: &[String], kind: NetKind) {
        if !self.want_registered() {
            return;
        }
        let reason = if reasons.is_empty() {
            "network_changed".to_string()
        } else {
            reasons.join(",")
        };
        tracing::info!(account = %self.id, generation, %reason, "network changed; re-probing");
        let _ = self.shared.events.send(SipEvent::AccountReconnecting {
            account: self.id.clone(),
            reason: reason.clone(),
        });
        self.start_probe(Cause::Network {
            generation,
            reason,
            kind,
        });
    }

    /// No route toward the registrar: suspend REGISTER attempts (no retry storm, no misleading
    /// 408s) until a network signal or a successful probe.
    fn enter_waiting(&mut self, why: &str) {
        self.timer = None;
        self.reg.fail_count = 0;
        self.reg.retry_after_until = None;
        if !matches!(self.reg.state, RegState::WaitingForNetwork) {
            tracing::info!(account = %self.id, %why, "no route to the registrar; waiting for the network");
        }
        self.set_state(RegState::WaitingForNetwork);
    }

    /// Acts on a probe result.
    async fn on_verdict(&mut self, cause: Cause, v: Verdict, now: Option<Probed>) {
        if !self.cfg.enabled || !self.want_registered() {
            return;
        }
        // The next hop is unchanged, but the provider's other addresses may not be: keep the
        // trusted set current (a failed lookup earlier is retried by every probe).
        if matches!(v, Verdict::Same)
            && let Some(now) = &now
            && let Some(rt) = self.rt.as_mut()
        {
            rt.probed.provider_addrs.clone_from(&now.provider_addrs);
            self.publish_env();
        }
        let waiting = matches!(self.reg.state, RegState::WaitingForNetwork);
        match v {
            Verdict::NoRoute(msg) => self.enter_waiting(&msg),
            Verdict::Changed if self.rt.is_none() => {
                // The first build failed (or never ran) and the network answers now: build at once.
                self.reg.fail_count = 0;
                self.timer = None;
                if !self.building {
                    self.begin_build();
                }
            }
            Verdict::Changed => {
                if self.calls > 0
                    && let Some(now) = &now
                    && self
                        .rt
                        .as_ref()
                        .is_some_and(|r| r.probed.transport != now.transport)
                {
                    // `auto` found another transport after calls began: they cannot move across
                    // transports. The next probe without calls re-selects.
                    tracing::info!(account = %self.id, "auto transport would change; deferred until the calls end");
                } else if self.calls > 0
                    && let Some(now) = now
                {
                    // Live calls: keep the runtime (endpoint, dialogs), move the socket and the
                    // calls onto the new path. Hanging them up is the last resort.
                    tracing::info!(account = %self.id, "registrar address or local address changed; moving the transport in place");
                    self.rebind_in_place(now).await;
                } else {
                    tracing::info!(account = %self.id, "registrar address or local address changed; rebuilding transport");
                    self.rebuild_no_unregister().await;
                }
            }
            Verdict::Same | Verdict::Failed(_) => match cause {
                Cause::Failure => {}
                Cause::Tick if !waiting => {}
                // Waiting, or a network signal: the route is usable, re-register right now.
                Cause::Tick => self.refresh_now(NetKind::Signal).await,
                Cause::Network { kind, .. } => {
                    // A server `Retry-After` is a promise to the server; a plain signal does not
                    // break it. Leaving WaitingForNetwork, a changed path (handled above) or an
                    // explicit request does.
                    let held = !waiting
                        && kind == NetKind::Signal
                        && self
                            .reg
                            .retry_after_until
                            .is_some_and(|t| t > Instant::now());
                    if held {
                        tracing::debug!(account = %self.id, "network signal; server Retry-After still pending, not retrying early");
                    } else {
                        self.refresh_now(kind).await;
                    }
                }
            },
        }
    }

    /// Re-registers right now on the current runtime: resets the back-off, skips any remaining
    /// delay and, on a stream transport, redials (the old connection may be half-open).
    async fn refresh_now(&mut self, kind: NetKind) {
        self.reg.fail_count = 0;
        if self.rt.is_none() {
            if !self.building {
                self.timer = None;
                self.begin_build();
            }
            return;
        }
        if self.stream_transport() && self.flow.is_some() {
            if self.calls > 0 && kind != NetKind::Dead {
                // The registrar connection also carries the calls' dialogs (connections are keyed
                // by peer): closing it would break in-dialog signalling. Re-REGISTER on it and
                // redial when the last call ends.
                self.reg.redial_pending = true;
            } else {
                self.redial().await;
            }
        }
        if matches!(self.reg.state, RegState::WaitingForNetwork) {
            self.set_state(RegState::Registering);
        }
        if self.reg.inflight {
            self.reg.restart = true;
        } else {
            self.schedule(Duration::ZERO);
        }
    }

    /// Drops the registrar connection and forgets what was learned over it; the next REGISTER
    /// dials a new one.
    async fn redial(&mut self) {
        self.close_flow().await;
        self.on_flow_lost();
        self.reg.last_flow_loss = None;
        self.reg.redial_pending = false;
    }

    /// Closes the registrar connection (bounded) and removes it from the transport cache, so the
    /// next lookup dials fresh instead of reusing the half-closed (write-shutdown) connection.
    async fn close_flow(&self) {
        let Some((ep, target)) = self
            .rt
            .as_ref()
            .and_then(|r| r.target.clone().map(|t| (r.endpoint.inner.clone(), t)))
        else {
            return;
        };
        let close = async {
            if let Ok((conn, _)) = ep.transport_layer.lookup(&target, None).await {
                let _ = conn.close().await;
                // Retire it (cancels its token: dialogs stop choosing it for affinity), then drop
                // whatever else is cached for that peer: since rsipstack 0.7.5 the cache keeps
                // several connections per remote address, and they all share the dead path.
                ep.transport_layer.remove_connection(&conn);
                if let Some(remote) = conn.get_remote_addr() {
                    #[allow(deprecated)]
                    ep.transport_layer.del_connection(remote);
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), close).await;
    }

    /// The flow is dead (keep-alives unanswered): forget what was learned over it, re-probe and
    /// re-register at once. Unlike a plain network signal this redials even with live calls: a
    /// flow proven dead has already broken their signalling.
    async fn on_flow_dead(&mut self, reason: &str) {
        if !self.want_registered() {
            return;
        }
        tracing::warn!(account = %self.id, %reason, "registrar flow looks dead; re-registering");
        let _ = self.shared.events.send(SipEvent::AccountReconnecting {
            account: self.id.clone(),
            reason: reason.to_string(),
        });
        if self.stream_transport() {
            self.close_flow().await;
        }
        self.on_flow_lost();
        self.reg.last_flow_loss = None;
        self.reg.fail_count = 0;
        if !self.reg.inflight {
            self.schedule(Duration::ZERO);
        }
        self.start_probe(Cause::Failure);
    }

    /// The local address (or the registrar) changed under live calls: moves the transport to the
    /// new path without dropping the endpoint or any dialog (rsipstack listener swap), then
    /// re-registers at once and moves the calls (`[network] move_calls`). A failed rebind falls
    /// back to ending the calls and rebuilding.
    async fn rebind_in_place(&mut self, now: Probed) {
        match self.try_rebind(now).await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(account = %self.id, error = %e, "cannot move the transport in place; ending the calls and rebuilding");
                self.shared
                    .calls
                    .end_live_network_lost(&self.id, "transport could not be moved");
                self.rebuild_no_unregister().await;
            }
        }
    }

    async fn try_rebind(&mut self, now: Probed) -> Result<()> {
        let Some(rt) = self.rt.as_ref() else {
            return Err(SipError::Transport("no runtime".into()));
        };
        let cfg = rt.built_for.clone();
        let transport = cfg.transport;
        let stream = transport != Transport::Udp;
        let ip_moved = rt.bind_ip != now.local_ip || rt.advertise_ip != now.advertise_ip;
        if stream && cfg.local_port != 0 && rt.bind_ip != now.local_ip {
            // A TCP / TLS listener on a fixed port cannot follow the address.
            return Err(SipError::Transport(
                "a TCP/TLS listener on a fixed port cannot be moved in place".into(),
            ));
        }
        let old_addr = rt
            .local_addr
            .map(|a| SipAddr::new(SipTransport::Udp, a.into()));
        let udp_moves = transport == Transport::Udp && rt.bind_ip != now.local_ip;

        // 1. The socket: a new UDP listener takes over; the old one keeps listening for a while
        //    (a peer that still sends to the old address while both networks are up is served).
        //    Back at an address whose old listener still lingers, that listener is re-installed.
        let mut moved: Option<(
            SocketAddr,
            CancellationToken,
            SipConnection,
            Option<Arc<AccountUdp>>,
        )> = None;
        if udp_moves {
            let Some(rt) = self.rt.as_mut() else {
                return Err(SipError::Transport("no runtime".into()));
            };
            rt.lingering.retain(|l| !l.tok.is_cancelled());
            // The expiry task and this path race for a lingering listener: whoever flips the
            // flag first owns it (a listener the expiry task won is being cancelled: bind anew).
            let mut reuse = None;
            while let Some(i) = rt.lingering.iter().position(|l| {
                l.ip == now.local_ip && (cfg.local_port == 0 || l.addr.port() == cfg.local_port)
            }) {
                let l = rt.lingering.remove(i);
                if l.lingering
                    .compare_exchange(
                        true,
                        false,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    reuse = Some(l);
                    break;
                }
            }
            if let Some(l) = reuse {
                rt.tl()
                    .reactivate_listener(old_addr.as_ref(), l.conn.clone());
                moved = Some((l.addr, l.tok, l.conn, Some(l.sock)));
            } else {
                let sock = AccountUdp::bind(SocketAddr::new(now.local_ip, cfg.local_port))?;
                let addr = sock.local();
                let tok = rt.token.child_token();
                let conn = SipConnection::Udp(
                    UdpConnection::from_socket(sock.clone(), None, Some(tok.clone())).await?,
                );
                if let Err(e) = rt.tl().swap_listener(old_addr.as_ref(), conn.clone()).await {
                    tok.cancel();
                    return Err(e.into());
                }
                moved = Some((addr, tok, conn, Some(sock)));
            }
        }
        // 2. The registrar flow: a stream transport redials; with the address gone every cached
        //    stream connection is dead (calls included). A moved registrar alone only affects the
        //    registrar flow.
        if stream
            && ip_moved
            && let Some(rt) = self.rt.as_mut()
        {
            // (A moved registrar alone leaves the connections alone: calls may still ride the
            // old one, and REGISTER goes to the new next hop over a connection of its own.)
            rt.tl().retire_stream_connections();
            rt.stream_gen += 1;
        }
        // 3. Runtime facts.
        let Some(rt) = self.rt.as_mut() else {
            return Err(SipError::Transport("no runtime".into()));
        };
        if transport == Transport::Tls {
            let hop = split_host_port(&hop_of(&cfg)).0;
            let sni = hop
                .parse::<IpAddr>()
                .is_err()
                .then_some(hop)
                .filter(|h| !h.is_empty());
            rt.tl()
                .set_tls_config(client_tls_config(&cfg, sni, now.target)?);
        }
        if let Some((addr, tok, conn, sock)) = moved {
            let old_addr = rt.local_addr;
            let old_tok = std::mem::replace(&mut rt.listener_token, tok);
            let old_conn = rt.listener_conn.replace(conn);
            let old_sock = std::mem::replace(&mut rt.listener_sock, sock);
            rt.local_addr = Some(addr);
            if let (Some(old_addr), Some(old_conn), Some(old_sock)) = (old_addr, old_conn, old_sock)
            {
                let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
                rt.lingering.push(Linger {
                    ip: rt.bind_ip,
                    addr: old_addr,
                    tok: old_tok.clone(),
                    conn: old_conn,
                    sock: old_sock,
                    lingering: flag.clone(),
                });
                // The old socket keeps receiving for at least 30 s and while a call still uses
                // its address (a call that was ringing when the account moved keeps that
                // Contact). A flap back re-installs it (`flag` false) and ends this task.
                let (calls, account) = (self.shared.calls.clone(), self.id.clone());
                let flag = flag.clone();
                let active = {
                    let flag = flag.clone();
                    move || flag.load(std::sync::atomic::Ordering::SeqCst)
                };
                tokio::spawn(async move {
                    tokio::select! {
                        () = old_tok.cancelled() => return,
                        () = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                    while active() && calls.any_live_on(&account, old_addr) {
                        tokio::select! {
                            () = old_tok.cancelled() => return,
                            () = tokio::time::sleep(Duration::from_secs(5)) => {}
                        }
                    }
                    if flag
                        .compare_exchange(
                            true,
                            false,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        old_tok.cancel();
                    }
                });
            }
        }
        rt.bind_ip = now.local_ip;
        rt.advertise_ip = now.advertise_ip;
        rt.target = now
            .target
            .map(|a| SipAddr::new(sip_transport(transport), a.into()));
        rt.probed = now;
        if transport == Transport::Udp || cfg.local_port != 0 {
            *rt.contact_cell
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                local_contact(&cfg, rt.public_host.as_ref(), rt.local_addr);
        } else if ip_moved {
            // Client-only stream: the REGISTER over the new connection tells our Contact.
            *rt.contact_cell
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
        self.contact = rt
            .contact_cell
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(ToString::to_string);
        self.build_seq += 1; // probes of the old path are stale
        self.publish_env();
        self.reset_keepalive();
        // 4. Register on the new path at once (same Call-ID, CSeq + 1; subscriptions follow the
        //    flow change).
        self.restart_registration();
        // 5. The calls.
        if ip_moved {
            self.rebind_seq += 1;
            self.shared.calls.mark_moving(&self.id, self.rebind_seq);
            if !self.shared.net.config().move_calls {
                self.shared
                    .calls
                    .end_live_network_lost(&self.id, "move_calls is off");
            } else if self.want_registered() {
                // Wait for the new Contact (the re-REGISTER), but not for long.
                self.move_due = Some(Instant::now() + MOVE_WAIT_REGISTER);
            } else {
                self.move_calls_now();
            }
        }
        Ok(())
    }

    /// Registers on a new path at once (same Call-ID, CSeq + 1; subscriptions follow the flow
    /// change): after the transport moved in place or its socket was replaced.
    fn restart_registration(&mut self) {
        self.on_flow_lost();
        self.reg.last_flow_loss = None;
        self.reg.fail_count = 0;
        self.reg.retry_after_until = None;
        if matches!(self.reg.state, RegState::WaitingForNetwork) {
            self.set_state(RegState::Registering);
        }
        if self.reg.inflight && !self.reg_unreg {
            // A REGISTER on the old path may hang for its full timeout while holding the
            // register lock: drop it (and ignore a result it may already have queued, `reg_seq`),
            // the new path registers now. An unregister is left to finish.
            if let Some(h) = self.reg_abort.take() {
                h.abort();
            }
            self.reg_seq += 1;
            self.reg.inflight = false;
            self.reg.restart = false;
        }
        self.schedule(Duration::ZERO);
        self.publish();
    }

    /// A listener's socket died (rsipstack ended its receive / accept loop; seen with macOS
    /// "defunct" sockets: every send `EPIPE`, every receive `ENOTCONN`).
    ///
    /// The current UDP socket is replaced in place by a new one on the same address, under the
    /// same rsipstack connection: the runtime, its dialogs, transactions in flight and the
    /// Contact stay, so live calls (and calls being set up) carry on, and the account
    /// re-registers at once. If the address
    /// cannot be bound again, the calls end and the runtime is rebuilt, as when a network change
    /// cannot move the transport in place. A new socket that dies again within
    /// [`QUICK_SOCKET_DEATH`] is replaced after [`socket_replace_delay`]. A lingering old
    /// socket is just dropped. A dead TCP / TLS listener (fixed `local_port`) rebuilds the
    /// runtime.
    async fn on_listener_dead(&mut self, local: SipAddr) {
        let Some(rt) = self.rt.as_mut() else { return };
        if let Some(i) = rt
            .lingering
            .iter()
            .position(|l| l.conn.get_addr() == &local)
        {
            let l = rt.lingering.remove(i);
            l.lingering
                .store(false, std::sync::atomic::Ordering::SeqCst);
            l.tok.cancel();
            l.sock.close();
            tracing::info!(account = %self.id, %local, "an old SIP socket kept after a move died; dropped");
            return;
        }
        let current_udp = rt
            .listener_conn
            .as_ref()
            .is_some_and(|c| c.get_addr() == &local);
        let stream_listener = rt.built_for.transport != Transport::Udp && rt.local_addr.is_some();
        if !current_udp && !stream_listener {
            tracing::debug!(account = %self.id, %local, "a closed listener that is not ours; ignored");
            return;
        }
        tracing::warn!(account = %self.id, %local, "the SIP socket died");
        if stream_listener {
            self.emit_transport_dead();
            self.rebuild_no_unregister().await;
            return;
        }
        // A socket that dies again right after being replaced is replaced later and later, so
        // a socket that never works cannot turn this into a loop of its own.
        let quick = self
            .socket_replaced_at
            .is_some_and(|t| t.elapsed() < QUICK_SOCKET_DEATH);
        self.socket_deaths = if quick { self.socket_deaths + 1 } else { 0 };
        let delay = socket_replace_delay(self.socket_deaths);
        if delay.is_zero() {
            self.replace_dead_socket().await;
        } else {
            tracing::warn!(account = %self.id, ?delay, deaths = self.socket_deaths, "the new SIP socket died right away; replacing it later");
            let (tx, epoch) = (self.int_tx.clone(), self.epoch);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                let _ = tx.send(Internal::ReplaceSocket { epoch });
            });
        }
    }

    fn emit_transport_dead(&self) {
        let _ = self.shared.events.send(SipEvent::AccountReconnecting {
            account: self.id.clone(),
            reason: "transport_dead".into(),
        });
    }

    /// Replaces the dead UDP socket now (see [`Self::on_listener_dead`]). The
    /// `account.reconnecting` event is sent once the new socket is in place (or, when it cannot
    /// be, before the rebuild).
    async fn replace_dead_socket(&mut self) {
        tracing::info!(account = %self.id, "replacing the dead SIP socket");
        match self.replace_udp_socket().await {
            Ok(()) => {
                self.socket_replaced_at = Some(Instant::now());
                self.emit_transport_dead();
                self.reset_keepalive();
                if matches!(self.reg.state, RegState::WaitingForNetwork) {
                    // No route: stay suspended (the next network signal or probe registers);
                    // only forget the flow the old socket had.
                    self.on_flow_lost();
                } else {
                    self.restart_registration();
                }
            }
            Err(e) => {
                tracing::warn!(account = %self.id, error = %e, "cannot replace the dead SIP socket; ending the calls and rebuilding");
                self.emit_transport_dead();
                self.shared
                    .calls
                    .end_live_network_lost(&self.id, "transport died");
                self.rebuild_no_unregister().await;
            }
        }
    }

    /// Replaces the current UDP listener by a new socket on the same address (see
    /// [`Self::on_listener_dead`]).
    async fn replace_udp_socket(&mut self) -> Result<()> {
        let Some(rt) = self.rt.as_ref() else {
            return Err(SipError::Transport("no runtime".into()));
        };
        let (Some(sock), Some(conn)) = (rt.listener_sock.clone(), rt.listener_conn.clone()) else {
            return Err(SipError::Transport("no UDP listener".into()));
        };
        // The new socket goes under the same connection: the transactions and dialogs that
        // hold it (a call being set up, a BYE being retransmitted) carry on over the new
        // socket. The dead one's descriptor is released first; a send that was in progress on
        // it may hold it for a moment, so the bind is retried briefly.
        let mut tries = 0;
        loop {
            match sock.rebind() {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < REBIND_TRIES => {
                    tries += 1;
                    tokio::time::sleep(REBIND_RETRY).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
        // Its receive loop ended when the socket died: start it again.
        rt.tl().restart_listener(conn).await?;
        tracing::info!(account = %self.id, addr = %sock.local(), "new SIP socket bound");
        Ok(())
    }

    /// Starts the re-INVITEs of the live calls onto the current runtime.
    fn move_calls_now(&mut self) {
        self.move_due = None;
        let Some(rt) = self.call_cell.env().and_then(|e| e.rt) else {
            return;
        };
        self.shared.calls.move_live(&self.id, &rt, self.rebind_seq);
    }

    /// Drops the runtime without unregistering (its transport is what broke) and builds anew.
    async fn rebuild_no_unregister(&mut self) {
        // The calls run on the runtime that is about to go: end them (bounded) first.
        self.shared.calls.hangup_live(Some(&self.id)).await;
        self.calls = 0;
        self.timer = None;
        self.keepalive = None;
        self.clear_pending_rebuild();
        if self.call_cell.clear_srtp_downgraded() {
            tracing::info!(account = %self.id, "OSRTP refusal memory cleared (runtime rebuilt)");
        }
        self.drop_runtime();
        self.reg.fail_count = 0;
        self.reg.expires_floor = None;
        self.identity = RegIdentity::new();
        self.set_state(RegState::Unregistered);
        self.begin_build();
    }

    // --- internal events ----------------------------------------------------------------------

    async fn on_internal(&mut self, ev: Internal) {
        match ev {
            Internal::Built { epoch, result } => {
                if epoch != self.epoch {
                    if let Ok(rt) = result {
                        rt.token.cancel();
                    }
                    return;
                }
                self.building = false;
                match result {
                    Ok(rt) => self.on_built(*rt),
                    Err(SipError::NoRoute(why)) if self.want_registered() => {
                        self.enter_waiting(&why)
                    }
                    Err(e) => self.on_failed(0, e.to_string(), None),
                }
            }
            Internal::RegDone {
                epoch,
                seq,
                unreg,
                result,
            } => {
                if epoch != self.epoch || seq != self.reg_seq {
                    return;
                }
                self.reg.inflight = false;
                let result = *result;
                if unreg {
                    match result {
                        Ok(RegResult::Ok(_)) => tracing::info!(account = %self.id, "unregistered"),
                        Ok(RegResult::NothingBound) => {}
                        Ok(RegResult::Rejected { code, reason, .. }) => {
                            tracing::warn!(account = %self.id, code, %reason, "unregister rejected")
                        }
                        Err(e) => {
                            tracing::warn!(account = %self.id, error = %e, "unregister failed")
                        }
                    }
                    self.identity.clear_bound();
                    // A `register_now` that arrived meanwhile is served by the fresh REGISTER
                    // scheduled below, not by a second one when that lands.
                    self.reg.restart = false;
                    if !self.want_registered() {
                        self.set_state(RegState::Unregistered);
                    } else {
                        self.schedule(Duration::ZERO);
                    }
                    return;
                }
                if !self.want_registered() {
                    // Unregistered (or disabled) while this REGISTER was in flight: whatever it
                    // bound is removed now (the Contact it sent was published before sending).
                    self.reg.restart = false;
                    self.spawn_unregister();
                    return;
                }
                if std::mem::take(&mut self.reg.restart) {
                    // Config changed underneath; whatever this said is stale.
                    self.schedule(Duration::ZERO);
                    return;
                }
                match result {
                    Ok(RegResult::Ok(ok)) => {
                        if self.stream_transport() && self.reg.closed_flows.contains(&ok.flow) {
                            // The 200 came over a connection that has since closed: the binding
                            // points at a dead flow. Redial and register again.
                            tracing::info!(account = %self.id, flow = %ok.flow, "registrar connection closed while registering; re-registering");
                            self.on_flow_lost();
                        } else {
                            self.on_registered(*ok);
                        }
                    }
                    Ok(RegResult::NothingBound) => {}
                    Ok(RegResult::Rejected {
                        code,
                        reason,
                        retry_after,
                    }) => self.on_failed(code, reason, retry_after),
                    // The REGISTER went over a connection we kept for live calls (a plain network
                    // signal does not redial then) and got nothing back: that flow is dead.
                    Err(e)
                        if self.stream_transport()
                            && (self.reg.redial_pending || self.calls > 0)
                            && !matches!(self.reg.state, RegState::WaitingForNetwork) =>
                    {
                        let why = if matches!(e, SipError::Timeout) {
                            "register_timeout"
                        } else {
                            "register_failed"
                        };
                        self.on_flow_dead(why).await;
                    }
                    // While waiting for the network a failed attempt must not leave that state.
                    Err(SipError::Timeout) => {
                        if !matches!(self.reg.state, RegState::WaitingForNetwork) {
                            self.on_failed(408, "Request Timeout (no response)".into(), None);
                        }
                        self.maybe_reprobe();
                    }
                    Err(e) => {
                        if self.stream_transport() {
                            self.flow = None;
                        }
                        if !matches!(self.reg.state, RegState::WaitingForNetwork) {
                            self.on_failed(0, e.to_string(), None);
                        }
                        self.maybe_reprobe();
                    }
                }
            }
            Internal::FlowClosed {
                epoch,
                local,
                remote,
            } => {
                if epoch != self.epoch {
                    return;
                }
                let remote_s = remote.as_ref().map(ToString::to_string).unwrap_or_default();
                let _ = self.shared.events.send(SipEvent::TransportClosed {
                    account: self.id.clone(),
                    remote: remote_s.clone(),
                });
                if self.reg.inflight {
                    self.reg.closed_flows.push(local.clone());
                }
                if self.flow.as_ref() != Some(&local) {
                    tracing::debug!(account = %self.id, %local, "closed connection is not the registrar flow");
                    return;
                }
                tracing::info!(account = %self.id, %local, remote = %remote_s, "registrar connection closed; re-registering");
                self.on_flow_lost();
            }
            Internal::ListenerDead { epoch, local } => {
                if epoch == self.epoch {
                    self.on_listener_dead(local).await;
                }
            }
            Internal::ReplaceSocket { epoch } => {
                if epoch == self.epoch {
                    self.replace_dead_socket().await;
                }
            }
            Internal::Probed {
                epoch,
                seq,
                cause,
                result,
            } => {
                if epoch != self.epoch {
                    return;
                }
                self.reg.reprobing = false;
                if seq == self.build_seq {
                    // Judged against the runtime that is current now, not the one at start.
                    let was = self.rt.as_ref().map(|r| r.probed.clone());
                    let v = verdict(was.as_ref(), &result);
                    self.on_verdict(cause, v, result.ok()).await;
                } else {
                    tracing::debug!(account = %self.id, "probe predates the current runtime; dropped");
                }
                if let Some(c) = self.reg.net_pending.take() {
                    self.start_probe(c);
                }
            }
            Internal::Keepalive { epoch, outcome } => {
                if epoch != self.epoch {
                    return;
                }
                self.on_keepalive_result(outcome).await;
            }
        }
    }

    /// The registrar flow is gone: forget what was learned over it and register again over a new
    /// connection (shortly, or after the minimum back-off when flows keep dying).
    fn on_flow_lost(&mut self) {
        self.flow = None;
        self.reg.redial_pending = false;
        self.sub_contact = None;
        self.sync_subs();
        self.reg.learned = None; // the ephemeral source port changes with the new connection
        self.reg.rewritten = false;
        self.public_addr = None;
        if self.want_registered() {
            let quick = self
                .reg
                .last_flow_loss
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10));
            self.reg.last_flow_loss = Some(Instant::now());
            if matches!(self.reg.state, RegState::Registered { .. }) {
                self.set_state(RegState::Registering);
            }
            if !self.reg.inflight {
                self.schedule(if quick {
                    reg_logic::BACKOFF_MIN
                } else {
                    Duration::from_millis(250)
                });
            }
        } else {
            self.publish();
        }
    }

    fn on_built(&mut self, rt: Runtime) {
        self.build_seq += 1;
        tracing::info!(account = %self.id, local = ?rt.local_addr, transport = ?self.cfg.transport, "transport ready");
        self.contact = rt
            .contact_cell
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(ToString::to_string);
        self.rt = Some(rt);
        self.publish_env();
        self.clear_pending_rebuild();
        self.reset_keepalive();
        self.sync_subs();
        if self.want_registered() {
            self.schedule(Duration::ZERO);
            self.publish();
        } else if !matches!(self.reg.state, RegState::Unregistered) {
            self.set_state(RegState::Unregistered);
        } else {
            self.publish();
        }
    }

    fn on_registered(&mut self, ok: RegOk) {
        self.reg.fail_count = 0;
        self.reg.ka_misses = 0;
        if ok.requested > self.cfg.expires {
            self.reg.expires_floor = Some(ok.requested);
        }
        if self.stream_transport() {
            self.flow = Some(ok.flow.clone());
        }
        self.contact = Some(ok.contact.to_string());
        if let Some(rt) = &self.rt {
            *rt.contact_cell
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(ok.contact.clone());
        }

        // NAT / flow awareness: re-register once with the address the registrar saw.
        if let Some(adv) = ok.advertised {
            let seen = (ok.received, ok.rport);
            if self.cfg.rewrite_contact
                && self.cfg.public_address.is_empty()
                && !self.reg.rewritten
                && let Some(new) = reg_logic::contact_rewrite(adv, seen.0, seen.1)
            {
                tracing::info!(account = %self.id, advertised = %adv, seen = %new, "registrar sees a different address; re-registering with it");
                self.reg.learned = Some(new);
                self.reg.rewritten = true;
                self.public_addr = Some(new);
                self.schedule(Duration::ZERO);
                self.publish();
                return;
            }
            if self.cfg.public_address.is_empty() {
                self.public_addr = Some(SocketAddr::new(
                    ok.received.unwrap_or(adv.ip()),
                    ok.rport.unwrap_or(adv.port()),
                ));
            }
        }
        self.reg.rewritten = false;
        self.sub_contact = Some(ok.contact.clone());
        self.sync_subs();

        let granted = ok.granted;
        let state = self.reg.state.renewed(Utc::now(), granted);
        self.schedule(reg_logic::refresh_after(granted));
        self.set_state(state);
        // The new Contact is known: move the live calls onto it (calls that moved with a
        // provisional Contact announce the real one).
        if self.move_due.is_some() {
            self.move_calls_now();
        } else if let Some(rt) = self.call_cell.env().and_then(|e| e.rt) {
            self.shared.calls.refresh_fallback_contacts(&self.id, &rt);
        }
    }

    fn on_failed(&mut self, code: u16, reason: String, retry_after: Option<u32>) {
        self.reg.fail_count += 1;
        let delay = reg_logic::retry_delay(self.reg.fail_count, retry_after);
        let retry_at = Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default();
        self.reg.retry_after_until = retry_after.map(|_| Instant::now() + delay);
        self.schedule(delay);
        self.set_state(RegState::Failed {
            code,
            reason,
            retry_at,
        });
    }

    // --- keep-alive ---------------------------------------------------------------------------

    /// Keep-alives (and with them dead-flow detection) run while Registered, and also while
    /// Registering or Failed on a stream flow we kept: a REGISTER that never gets answered must
    /// not blind the detector.
    fn keepalive_eligible(&self) -> bool {
        self.reg.state.is_registered()
            || (self.stream_transport()
                && self.flow.is_some()
                && matches!(
                    self.reg.state,
                    RegState::Registering | RegState::Failed { .. }
                ))
    }

    fn on_keepalive(&mut self) {
        if !self.keepalive_eligible() {
            return;
        }
        let Some(rt) = &self.rt else { return };
        let stream = rt.built_for.transport != Transport::Udp;
        if stream && self.flow.is_none() {
            return; // flow is down; re-registration will redial
        }
        let Some(target) = rt.target.clone() else {
            return;
        };
        let ep = rt.endpoint.inner.clone();
        let (account, epoch, tx) = (self.id.clone(), self.epoch, self.int_tx.clone());
        match self.cfg.keepalive_method {
            KeepaliveMethod::Crlf => {
                tokio::spawn(async move {
                    let res = async {
                        let (conn, _) = ep.transport_layer.lookup(&target, None).await?;
                        match &conn {
                            SipConnection::Udp(c) => c.send_raw(b"\r\n\r\n", &target).await,
                            SipConnection::Tcp(c) => c.send_raw(b"\r\n\r\n").await,
                            SipConnection::Tls(c) => c.send_raw(b"\r\n\r\n").await,
                            _ => Ok(()),
                        }
                    }
                    .await;
                    let outcome = match res {
                        Ok(()) => KaOutcome::Alive,
                        Err(e) => {
                            tracing::debug!(%account, error = %e, "keep-alive send failed");
                            ka_miss(e.to_string())
                        }
                    };
                    let _ = tx.send(Internal::Keepalive { epoch, outcome });
                });
            }
            KeepaliveMethod::Options => {
                let Some(client) = self.client() else { return };
                let Some(uri) = client.registrar_uri.clone() else {
                    return;
                };
                // Any SIP response proves the flow; give up on one probe after a few seconds so
                // `keepalive_fail_threshold` misses add up quickly.
                let wait = Duration::from_secs(u64::from(self.cfg.keepalive_secs).clamp(2, 8));
                tokio::spawn(async move {
                    let outcome =
                        match tokio::time::timeout(wait, client.options(&uri, &target)).await {
                            Ok(Ok(r)) => {
                                tracing::trace!(%account, code = r.code, "keep-alive OPTIONS");
                                KaOutcome::Alive
                            }
                            Ok(Err(e)) => {
                                tracing::debug!(%account, error = %e, "keep-alive OPTIONS failed");
                                ka_miss(e.to_string())
                            }
                            Err(_) => {
                                tracing::debug!(%account, "keep-alive OPTIONS unanswered");
                                KaOutcome::Miss {
                                    unreachable: false,
                                    error: "no response".into(),
                                }
                            }
                        };
                    let _ = tx.send(Internal::Keepalive { epoch, outcome });
                });
            }
        }
    }

    /// Counts unanswered OPTIONS keep-alives (`keepalive_fail_threshold` in a row = dead flow) and
    /// turns "unreachable" send errors into a network-change signal for this account.
    async fn on_keepalive_result(&mut self, outcome: KaOutcome) {
        if !self.keepalive_eligible() {
            return;
        }
        let options = self.cfg.keepalive_method == KeepaliveMethod::Options;
        match outcome {
            KaOutcome::Alive => self.reg.ka_misses = 0,
            KaOutcome::Miss { unreachable, error } => {
                if options {
                    self.reg.ka_misses += 1;
                }
                if unreachable {
                    self.reg.ka_misses = 0;
                    tracing::info!(account = %self.id, %error, "keep-alive cannot reach the registrar");
                    let g = self.shared.net.generation();
                    self.on_network_changed(g, &["send_error".to_string()], NetKind::Dead);
                } else if options
                    && self.cfg.keepalive_fail_threshold > 0
                    && self.reg.ka_misses >= self.cfg.keepalive_fail_threshold
                {
                    self.reg.ka_misses = 0;
                    self.on_flow_dead("keepalive_timeout").await;
                }
            }
        }
    }

    // --- commands -----------------------------------------------------------------------------

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::RegisterNow => {
                if !self.cfg.enabled || !self.cfg.register {
                    tracing::warn!(account = %self.id, "register_now ignored (disabled or register = false)");
                    return;
                }
                self.reg.manual_off = false;
                self.reg.fail_count = 0;
                if matches!(self.reg.state, RegState::WaitingForNetwork) {
                    // Suspended for lack of a route: probe first; a usable route registers at once.
                    let g = self.shared.net.generation();
                    self.on_network_changed(g, &["manual".to_string()], NetKind::Manual);
                } else if self.rt.is_none() {
                    if !self.building {
                        self.begin_build();
                    }
                } else if self.reg.inflight {
                    self.reg.restart = true;
                } else {
                    self.schedule(Duration::ZERO);
                }
            }
            Cmd::Reregister => {
                if !self.cfg.enabled || !self.cfg.register {
                    tracing::warn!(account = %self.id, "reregister ignored (disabled or register = false)");
                    return;
                }
                self.reg.manual_off = false;
                let g = self.shared.net.generation();
                self.on_network_changed(g, &["manual".to_string()], NetKind::Manual);
            }
            Cmd::NetworkChanged {
                generation,
                reasons,
            } => self.on_network_changed(generation, &reasons, NetKind::Signal),
            Cmd::NetProbeTick => {
                if self.want_registered() {
                    self.start_probe(Cause::Tick);
                }
            }
            Cmd::Unregister => {
                self.reg.manual_off = true;
                self.timer = None;
                if self.reg.inflight {
                    // Handled when the in-flight REGISTER lands.
                } else if self.identity.bound().is_some() && self.rt.is_some() {
                    self.spawn_unregister();
                } else {
                    self.set_state(RegState::Unregistered);
                }
            }
            Cmd::Refresh => self.publish(),
            Cmd::SetTrace(on) => self.set_trace(on),
            Cmd::Update { cfg, reply } => {
                let out = self.update(*cfg).await;
                let _ = reply.send(out);
            }
            Cmd::SetMedia(m) => {
                // The account's effective codec list (own, or inherited from `[media]`) feeds
                // the OSRTP offer capabilities: a change invalidates what the refusal memory
                // learned. Cleared before publishing the new env (generation bump first).
                let codecs_changed =
                    self.cfg.effective_codecs(&self.media) != self.cfg.effective_codecs(&m);
                self.media = *m;
                let cleared = codecs_changed && self.call_cell.clear_srtp_downgraded();
                if cleared {
                    tracing::info!(account = %self.id, "OSRTP refusal memory cleared (codecs changed)");
                }
                self.publish_env();
                if cleared {
                    self.publish();
                }
            }
            Cmd::SetGeneral(g) => {
                self.general = *g;
                self.refresh_ua();
                if self.pending_rebuild && self.pending_config && self.general.reconfig_force {
                    self.rebuild().await;
                }
            }
            Cmd::SendOptions { target, reply } => {
                let client = self.client();
                let dns = self.shared.dns.clone();
                let transport = self
                    .rt
                    .as_ref()
                    .map_or(self.cfg.transport, |r| r.built_for.transport);
                tokio::spawn(async move {
                    let res = match client {
                        None => Err(SipError::Transport("account is not running".into())),
                        Some(c) => send_options(&c, dns.as_ref(), transport, &target).await,
                    };
                    let _ = reply.send(res);
                });
            }
            Cmd::CallsChanged(n) => {
                self.calls = n;
                if n == 0 && self.pending_rebuild {
                    self.rebuild().await;
                } else if n == 0
                    && std::mem::take(&mut self.reg.redial_pending)
                    && self.want_registered()
                    && self.stream_transport()
                    && self.flow.is_some()
                {
                    // A network signal arrived during the calls: redial now that nothing rides on
                    // the old connection.
                    self.redial().await;
                    if !self.reg.inflight {
                        self.schedule(Duration::ZERO);
                    }
                }
            }
            Cmd::SetDnd { enabled, reply } => {
                self.override_dnd = Some(enabled);
                self.apply_dnd();
                let _ = reply.send(());
            }
            Cmd::DropFlow(reply) => {
                let target = self
                    .rt
                    .as_ref()
                    .and_then(|r| r.target.clone().map(|t| (r.endpoint.inner.clone(), t)));
                tokio::spawn(async move {
                    let ok = match target {
                        Some((ep, t)) => match ep.transport_layer.lookup(&t, None).await {
                            Ok((conn, _)) => conn.close().await.is_ok(),
                            Err(_) => false,
                        },
                        None => false,
                    };
                    let _ = reply.send(ok);
                });
            }
            Cmd::KillUdp(reply) => {
                let sock = self.rt.as_ref().and_then(|r| r.listener_sock.clone());
                if let Some(s) = &sock {
                    s.simulate_death();
                }
                let _ = reply.send(sock.is_some());
            }
            Cmd::Shutdown(_) => unreachable!("handled in run()"),
        }
    }

    /// Tells the subscriptions which environment (flow, Contact, AOR) they run on; a different
    /// one makes them subscribe anew, none parks them until the transport is back.
    fn sync_subs(&mut self) {
        let env = (|| {
            if !self.cfg.enabled || (self.cfg.register && self.sub_contact.is_none()) {
                return None;
            }
            let rt = self.rt.as_ref()?;
            let client = self.client()?;
            let key = format!(
                "{}|{}|{}",
                self.epoch,
                client.aor,
                self.sub_contact
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            );
            Some(Arc::new(SubEnv {
                client,
                contact: self.sub_contact.clone(),
                key,
                dns: self.shared.dns.clone(),
                transport: rt.built_for.transport,
            }))
        })();
        self.subs.set_env(env);
    }

    fn set_trace(&mut self, on: bool) {
        self.trace = on;
        if let Some(rt) = &self.rt {
            if on {
                rt.tl().set_wire_tap(rt.tap.clone());
            } else {
                rt.tl().clear_wire_tap();
            }
        }
        tracing::info!(account = %self.id, on, "sip trace");
        self.publish();
    }

    fn refresh_ua(&mut self) {
        if let Some(rt) = &self.rt {
            let ua = effective_ua(&self.cfg, &self.general, &self.shared.default_ua);
            *rt.ua.write().unwrap_or_else(PoisonError::into_inner) = ua;
        }
    }

    /// Applies a changed config.
    async fn update(&mut self, new: AccountConfig) -> UpdateOutcome {
        let old = std::mem::replace(&mut self.cfg, new.clone());
        let mut need = update::classify(&old, &new);
        // `register` false -> true on a runtime built without a next hop (the host did not
        // resolve in peer mode): only a rebuild re-resolves it.
        if need == update::Need::Reregister
            && new.register
            && !hop_of(&new).is_empty()
            && self.rt.as_ref().is_some_and(|r| r.probed.target.is_none())
        {
            need = update::Need::Rebuild;
        }
        let outcome = update::outcome(need, self.calls > 0, self.general.reconfig_force);
        tracing::info!(account = %self.id, ?outcome, "account config updated");

        // Hot parts apply immediately in every case.
        if old.dnd != new.dnd {
            self.override_dnd = None; // a config change replaces a runtime override
        }
        if (old.srtp != new.srtp
            || old.registrar != new.registrar
            || old.outbound_proxy != new.outbound_proxy
            || old.domain != new.domain
            || old.transport != new.transport
            || old.codecs != new.codecs
            || old.srtp_suites != new.srtp_suites
            || need == update::Need::Rebuild)
            && self.call_cell.clear_srtp_downgraded()
        {
            tracing::info!(account = %self.id, "OSRTP refusal memory cleared (config changed)");
        }
        self.publish_env();
        self.apply_dnd();
        self.refresh_ua();
        if old.sip_trace != new.sip_trace {
            self.set_trace(new.sip_trace); // a config change overrides a runtime toggle
        }
        if old.keepalive_secs != new.keepalive_secs || old.register != new.register {
            self.reset_keepalive();
        }
        // BLF list / MWI settings are hot; an AOR change restarts them through the
        // environment key below.
        // Disabling or moving to another AOR ends the subscriptions properly (Expires: 0 while
        // the old flow / Contact still exist) instead of silently forgetting them; for an AOR
        // change the old Contact must not be reused by the restart wave either.
        let aor_moved = update::aor_changed(&old, &new);
        if need == update::Need::Disable || aor_moved {
            self.subs.release_all(false).await;
            if aor_moved {
                self.sub_contact = None;
            }
        }
        self.subs.configure(&new);
        self.sync_subs();

        if need == update::Need::Disable {
            self.disable_now(&old).await;
            return outcome;
        }
        match outcome {
            UpdateOutcome::Hot => self.publish(),
            UpdateOutcome::Reregistered => self.reregister(&old).await,
            UpdateOutcome::Rebuilt => {
                self.clear_pending_rebuild();
                self.rebuild_from(&old).await;
            }
            UpdateOutcome::RebuildDeferred => {
                self.pending_rebuild = true;
                self.pending_config = true;
                self.publish();
            }
        }
        outcome
    }

    /// `enabled` true -> false: never deferred. Hangs up the account's calls, removes the
    /// binding, stops everything and reports `Disabled`.
    async fn disable_now(&mut self, old: &AccountConfig) {
        self.clear_pending_rebuild();
        self.timer = None;
        self.keepalive = None;
        self.shared.calls.hangup_live(Some(&self.id)).await;
        self.calls = 0;
        self.rebuild_from(old).await;
    }

    async fn reregister(&mut self, old: &AccountConfig) {
        self.subs.kick_failed();
        if old.rewrite_contact != self.cfg.rewrite_contact {
            self.reg.learned = None;
            self.reg.rewritten = false;
        }
        if update::aor_changed(old, &self.cfg) && self.maybe_bound() {
            // The REGISTER in flight (if any) is for the old AOR; the unregister below queues
            // behind it on the shared CSeq / lock, and its result is dropped via `restart`.
            if self.reg.inflight {
                self.reg.restart = true;
            }
            self.unregister_inline(old).await;
            self.identity = RegIdentity::new();
            self.reg.learned = None;
        }
        self.reg.fail_count = 0;
        self.reg.expires_floor = None;
        if !self.want_registered() {
            if self.identity.bound().is_some() && !self.reg.inflight {
                self.spawn_unregister();
            } else if !self.reg.inflight {
                self.set_state(RegState::Unregistered);
            }
        } else if self.rt.is_some() {
            if self.reg.inflight {
                self.reg.restart = true;
            } else {
                self.schedule(Duration::ZERO);
            }
        }
        self.publish();
    }

    /// Tears the runtime down (unregistering with the config it registered with) and starts anew.
    async fn rebuild_from(&mut self, old: &AccountConfig) {
        self.timer = None;
        self.keepalive = None;
        // Calls cannot outlive their transport: hang up (bounded, then force-end) before the
        // runtime is dropped. A no-op when `disable_now` already did.
        self.shared.calls.hangup_live(Some(&self.id)).await;
        self.calls = 0;
        // The runtime (and maybe the next hop) changes: remembered refusals no longer apply.
        self.call_cell.clear_srtp_downgraded();
        self.subs.release_all(false).await;
        if old.register && self.should_unregister() {
            self.unregister_inline(old).await;
        }
        self.drop_runtime();
        self.reg.fail_count = 0;
        self.reg.expires_floor = None;
        self.identity = RegIdentity::new();
        if self.cfg.enabled {
            self.set_state(RegState::Unregistered);
            self.begin_build();
        } else {
            self.set_state(RegState::Disabled);
        }
    }

    async fn rebuild(&mut self) {
        let old = self
            .rt
            .as_ref()
            .map_or_else(|| self.cfg.clone(), |r| r.built_for.clone());
        self.clear_pending_rebuild();
        self.rebuild_from(&old).await;
    }
}

impl RegState {
    pub(crate) fn is_registered(&self) -> bool {
        matches!(self, RegState::Registered { .. })
    }
}

/// OPTIONS toward `target` (`sip:` URI or `host[:port]`).
async fn send_options(
    client: &Client,
    dns: &SystemSrvSource,
    default_transport: Transport,
    target: &str,
) -> Result<OptionsResponse> {
    let uri = parse_target(target)?;
    let (host, port, transport) = uri_destination(&uri, default_transport);
    let addrs = dns::resolve(dns, &host, port, transport).await?;
    let dest = SipAddr::new(sip_transport(transport), addrs[0].into());
    client.options(&uri, &dest).await
}

/// Accepts `sip:user@host:port;transport=tcp`, `user@host`, or `host:port`.
pub(crate) fn parse_target(t: &str) -> Result<Uri> {
    let t = t.trim();
    let s = if t.starts_with("sip:") || t.starts_with("sips:") {
        t.to_string()
    } else {
        format!("sip:{t}")
    };
    Uri::try_from(s.as_str()).map_err(|e| SipError::Config(format!("bad target {t:?}: {e}")))
}

/// Host, explicit port and transport a URI asks for (`;transport=` param, `sips:` implies TLS).
pub(crate) fn uri_destination(uri: &Uri, default: Transport) -> (String, Option<u16>, Transport) {
    let mut transport = if matches!(uri.scheme, Some(rsipstack::sip::Scheme::Sips)) {
        Transport::Tls
    } else {
        default
    };
    for p in &uri.params {
        if let rsipstack::sip::Param::Transport(t) = p {
            transport = match t {
                SipTransport::Tcp => Transport::Tcp,
                SipTransport::Tls => Transport::Tls,
                _ => Transport::Udp,
            };
        }
    }
    (
        uri.host_with_port.host.to_string(),
        uri.host_with_port.port.map(u16::from),
        transport,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsipstack::resolver::sip_resolver::{LookupSource, SrvRecord};

    /// Resolver whose answer can change between probes.
    struct Flip(Mutex<Option<IpAddr>>);

    #[async_trait::async_trait]
    impl LookupSource for Flip {
        async fn lookup_srv(&self, _: &str) -> std::result::Result<Vec<SrvRecord>, String> {
            Err("nx".into())
        }
        async fn lookup_a_aaaa(&self, n: &str) -> std::result::Result<Vec<IpAddr>, String> {
            self.0
                .lock()
                .unwrap()
                .map(|ip| vec![ip])
                .ok_or_else(|| format!("lookup {n}: nxdomain"))
        }
    }

    /// [`NetProbe`] over a resolver whose answer can change and a switchable route.
    struct TestProbe {
        dns: Flip,
        route: Mutex<std::result::Result<IpAddr, std::io::ErrorKind>>,
        /// Hosts `resolve` was asked for.
        asked: Mutex<Vec<String>>,
        /// IPv4 destinations are unreachable (an IPv6-only network).
        v4_down: Mutex<bool>,
    }

    impl TestProbe {
        fn new(registrar: Option<&str>) -> Self {
            Self {
                dns: Flip(Mutex::new(registrar.map(|s| s.parse().unwrap()))),
                route: Mutex::new(Ok("10.1.1.1".parse().unwrap())),
                asked: Mutex::new(Vec::new()),
                v4_down: Mutex::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl NetProbe for TestProbe {
        fn local_ip_toward(&self, dest: SocketAddr) -> std::io::Result<IpAddr> {
            if dest.is_ipv4() && *self.v4_down.lock().unwrap() {
                return Err(std::io::Error::from(std::io::ErrorKind::NetworkUnreachable));
            }
            (*self.route.lock().unwrap()).map_err(std::io::Error::from)
        }
        async fn resolve(
            &self,
            host: &str,
            port: Option<u16>,
            transport: Transport,
        ) -> Result<Vec<SocketAddr>> {
            self.asked.lock().unwrap().push(host.to_string());
            crate::dns::resolve(&self.dns, host, port, transport).await
        }
    }

    #[tokio::test]
    async fn probe_detects_registrar_change_and_failure() {
        let src = TestProbe::new(Some("192.0.2.1"));
        let mut cfg = AccountConfig {
            domain: "pbx.example".into(),
            bind_address: "127.0.0.1".into(),
            ..Default::default()
        };
        let a = probe(&cfg, &src, None).await.unwrap();
        assert_eq!(a.target, Some("192.0.2.1:5060".parse().unwrap()));
        assert_eq!(a, probe(&cfg, &src, None).await.unwrap());
        *src.dns.0.lock().unwrap() = Some("192.0.2.9".parse().unwrap());
        assert_ne!(
            a,
            probe(&cfg, &src, None).await.unwrap(),
            "new registrar address"
        );
        // Unresolvable: an error when registering, a hop-less runtime in peer mode.
        *src.dns.0.lock().unwrap() = None;
        assert!(probe(&cfg, &src, None).await.is_err());
        cfg.register = false;
        assert_eq!(probe(&cfg, &src, None).await.unwrap().target, None);
        // A public_address that does not resolve is a failure, not a silent local fallback.
        cfg.public_address = "no-such-host.invalid".into();
        assert!(probe(&cfg, &src, None).await.is_err());
    }

    #[tokio::test]
    async fn probe_collects_every_provider_address() {
        let src = TestProbe::new(Some("192.0.2.1"));
        let cfg = AccountConfig {
            domain: "pbx.example".into(),
            registrar: "edge.example".into(),
            outbound_proxy: "proxy.example:5070".into(),
            bind_address: "127.0.0.1".into(),
            ..Default::default()
        };
        let p = probe(&cfg, &src, None).await.unwrap();
        // The AOR domain is not looked up (it may be an unrelated web host).
        assert!(!src.asked.lock().unwrap().iter().any(|h| h == "pbx.example"));
        assert_eq!(p.candidates, vec!["192.0.2.1:5070".parse().unwrap()]);
        // The registrar / domain resolve too (their own default port).
        for want in ["192.0.2.1:5070", "192.0.2.1:5060"] {
            assert!(p.provider_addrs.contains(&want.parse().unwrap()), "{want}");
        }
    }

    #[tokio::test]
    async fn probe_diff_verdicts() {
        use std::io::ErrorKind as K;
        let src = TestProbe::new(Some("192.0.2.1"));
        // No bind_address: the local address comes from the (fake) route.
        let cfg = AccountConfig {
            domain: "pbx.example".into(),
            ..Default::default()
        };
        let first = probe(&cfg, &src, None).await;
        assert_eq!(
            first.as_ref().unwrap().local_ip,
            "10.1.1.1".parse::<IpAddr>().unwrap()
        );
        // No runtime yet: any usable result means "build".
        assert_eq!(verdict(None, &first), Verdict::Changed);
        let was = first.as_ref().unwrap().clone();
        assert_eq!(
            verdict(Some(&was), &probe(&cfg, &src, None).await),
            Verdict::Same
        );
        // New local address (interface switch).
        *src.route.lock().unwrap() = Ok("192.168.7.7".parse().unwrap());
        assert_eq!(
            verdict(Some(&was), &probe(&cfg, &src, None).await),
            Verdict::Changed
        );
        // New next hop (DNS moved).
        *src.route.lock().unwrap() = Ok("10.1.1.1".parse().unwrap());
        *src.dns.0.lock().unwrap() = Some("192.0.2.77".parse().unwrap());
        assert_eq!(
            verdict(Some(&was), &probe(&cfg, &src, None).await),
            Verdict::Changed
        );
        *src.dns.0.lock().unwrap() = Some("192.0.2.1".parse().unwrap());
        // The "no route" errors of every OS become NoRoute; other errors do not.
        for k in [
            K::NetworkUnreachable,
            K::HostUnreachable,
            K::AddrNotAvailable,
            K::NetworkDown,
        ] {
            *src.route.lock().unwrap() = Err(k);
            assert!(
                matches!(
                    verdict(Some(&was), &probe(&cfg, &src, None).await),
                    Verdict::NoRoute(_)
                ),
                "{k:?}"
            );
        }
        *src.route.lock().unwrap() = Err(K::PermissionDenied);
        assert!(matches!(
            verdict(Some(&was), &probe(&cfg, &src, None).await),
            Verdict::Failed(_)
        ));
        // DNS failure is Failed (the old runtime is kept), not NoRoute.
        *src.route.lock().unwrap() = Ok("10.1.1.1".parse().unwrap());
        *src.dns.0.lock().unwrap() = None;
        assert!(matches!(
            verdict(Some(&was), &probe(&cfg, &src, None).await),
            Verdict::Failed(_)
        ));
        // With a pinned bind_address the route is not consulted.
        let pinned = AccountConfig {
            bind_address: "127.0.0.1".into(),
            ..cfg
        };
        *src.dns.0.lock().unwrap() = Some("192.0.2.1".parse().unwrap());
        *src.route.lock().unwrap() = Err(K::NetworkUnreachable);
        assert!(probe(&pinned, &src, None).await.is_ok());
    }

    fn probed(target: &str, cands: &[&str]) -> Probed {
        let ip: IpAddr = "10.1.1.1".parse().unwrap();
        Probed {
            target: Some(target.parse().unwrap()),
            candidates: cands.iter().map(|c| c.parse().unwrap()).collect(),
            provider_addrs: Vec::new(),
            local_ip: ip,
            advertise_ip: ip,
            transport: Transport::Udp,
        }
    }

    #[test]
    fn srv_candidate_order_is_not_a_next_hop_change() {
        let was = probed("192.0.2.1:5060", &["192.0.2.1:5060"]);
        // Weighted-random SRV answers: the same set in any order, or just a subset, keeps the path.
        for order in [
            ["192.0.2.1:5060", "192.0.2.2:5060", "192.0.2.3:5060"],
            ["192.0.2.3:5060", "192.0.2.1:5060", "192.0.2.2:5060"],
            ["192.0.2.2:5060", "192.0.2.3:5060", "192.0.2.1:5060"],
        ] {
            let now = probed(order[0], &order);
            assert_eq!(verdict(Some(&was), &Ok(now)), Verdict::Same, "{order:?}");
        }
        // The current hop vanished from the answer: a real change.
        let gone = probed("192.0.2.2:5060", &["192.0.2.2:5060", "192.0.2.3:5060"]);
        assert_eq!(verdict(Some(&was), &Ok(gone)), Verdict::Changed);
        // Same hop but another local address: a real change.
        let mut other_ip = probed("192.0.2.1:5060", &["192.0.2.1:5060", "192.0.2.2:5060"]);
        other_ip.local_ip = "10.2.2.2".parse().unwrap();
        assert_eq!(verdict(Some(&was), &Ok(other_ip)), Verdict::Changed);
        // `auto` picked another transport on the same hop: the runtime must be rebuilt.
        let mut other_transport = probed("192.0.2.1:5060", &["192.0.2.1:5060"]);
        other_transport.transport = Transport::Tls;
        assert_eq!(verdict(Some(&was), &Ok(other_transport)), Verdict::Changed);
    }

    #[tokio::test]
    async fn resolution_failure_offline_is_no_route_not_dns() {
        use std::io::ErrorKind as K;
        let src = TestProbe::new(None); // nothing resolves
        let cfg = AccountConfig {
            domain: "pbx.example".into(),
            ..Default::default()
        };
        // Route fine: a real DNS failure.
        assert!(matches!(
            probe(&cfg, &src, None).await,
            Err(SipError::Dns(_))
        ));
        // Offline: the same failure is "no route", checked toward the old hop or a public probe.
        *src.route.lock().unwrap() = Err(K::NetworkUnreachable);
        assert!(matches!(
            probe(&cfg, &src, None).await,
            Err(SipError::NoRoute(_))
        ));
        assert!(matches!(
            probe(&cfg, &src, Some("192.0.2.9:5060".parse().unwrap())).await,
            Err(SipError::NoRoute(_))
        ));
        // IPv6-only network: no IPv4 route, but online. Not "no route".
        *src.route.lock().unwrap() = Ok("2001:db8::7".parse().unwrap());
        *src.v4_down.lock().unwrap() = true;
        assert!(matches!(
            probe(&cfg, &src, None).await,
            Err(SipError::Dns(_))
        ));
        assert!(matches!(
            probe(&cfg, &src, Some("192.0.2.9:5060".parse().unwrap())).await,
            Err(SipError::Dns(_))
        ));
        *src.v4_down.lock().unwrap() = false;
        *src.route.lock().unwrap() = Err(K::NetworkUnreachable);
        // A pinned bind_address skips the route check.
        let pinned = AccountConfig {
            bind_address: "127.0.0.1".into(),
            ..cfg
        };
        assert!(matches!(
            probe(&pinned, &src, None).await,
            Err(SipError::Dns(_))
        ));
    }

    #[test]
    fn a_socket_that_keeps_dying_is_replaced_later_and_later() {
        assert_eq!(socket_replace_delay(0), Duration::ZERO);
        assert_eq!(socket_replace_delay(1), Duration::from_millis(500));
        assert_eq!(socket_replace_delay(2), Duration::from_secs(1));
        assert_eq!(socket_replace_delay(3), Duration::from_secs(2));
        assert_eq!(socket_replace_delay(7), Duration::from_secs(30));
        assert_eq!(socket_replace_delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn keepalive_send_errors_classify_unreachable() {
        for e in [
            "transport error: Network is unreachable (os error 101)",
            "transport error: No route to host (os error 113)",
            "transport error: Cannot assign requested address (os error 99)",
            "transport error: Can't assign requested address (os error 49)",
            "transport error: An invalid address (os error 10049)",
        ] {
            assert!(
                matches!(
                    ka_miss(e.into()),
                    KaOutcome::Miss {
                        unreachable: true,
                        ..
                    }
                ),
                "{e}"
            );
        }
        for e in ["request timed out", "Connection refused (os error 111)"] {
            assert!(
                matches!(
                    ka_miss(e.into()),
                    KaOutcome::Miss {
                        unreachable: false,
                        ..
                    }
                ),
                "{e}"
            );
        }
    }

    #[test]
    fn target_forms() {
        let u = parse_target("sip:b@127.0.0.1:5070;transport=tcp").unwrap();
        assert_eq!(
            uri_destination(&u, Transport::Udp),
            ("127.0.0.1".into(), Some(5070), Transport::Tcp)
        );
        let u = parse_target("127.0.0.1:5070").unwrap();
        assert_eq!(
            uri_destination(&u, Transport::Udp),
            ("127.0.0.1".into(), Some(5070), Transport::Udp)
        );
        let u = parse_target("sips:pbx.example.com").unwrap();
        assert_eq!(
            uri_destination(&u, Transport::Udp),
            ("pbx.example.com".into(), None, Transport::Tls)
        );
        let u = parse_target("bob@pbx.example.com").unwrap();
        assert_eq!(
            uri_destination(&u, Transport::Tcp),
            ("pbx.example.com".into(), None, Transport::Tcp)
        );
    }
}
