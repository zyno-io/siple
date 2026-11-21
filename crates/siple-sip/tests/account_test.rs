//! `account.test` (one-off REGISTER diagnosis) and `transport = "auto"` against in-process fake
//! registrars on loopback: UDP (ok / 401 / 403 / 404 / silence) and TLS with a self-signed
//! certificate (untrusted with its fingerprint, then trusted by pinning it).

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Secret, Transport};
use siple_sip::net::{NetMonitor, NetProbe, NetReason, NetSetup};
use siple_sip::{Diagnosis, NullWireSink, Problem, RegState, SipError, SipStack, SipStackOptions};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

// --- fake registrars ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Reply {
    /// Never answer.
    Silent,
    /// Final answer after the digest round.
    Code(u16, &'static str),
}

/// Registrar behaviour: 401 digest challenge for a request without `Authorization`, then `reply`.
struct Behaviour {
    reply: Reply,
    /// Requests seen: (authenticated, expires).
    seen: Mutex<Vec<(bool, Option<u32>)>>,
}

impl Behaviour {
    fn new(reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            reply,
            seen: Mutex::default(),
        })
    }

    /// The response text for `req`, or `None` to stay silent.
    fn answer(&self, req: &str, from: SocketAddr) -> Option<String> {
        if !req.starts_with("REGISTER ") {
            return None;
        }
        let header = |n: &str| {
            req.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case(n)
                    .then(|| v.trim().to_string())
            })
        };
        let authed = header("Authorization").is_some();
        let expires = header("Expires").and_then(|e| e.parse().ok());
        self.seen.lock().unwrap().push((authed, expires));
        let (code, reason, extra): (u16, &str, Vec<String>) = if !authed {
            (
                401,
                "Unauthorized",
                vec![
                    "WWW-Authenticate: Digest realm=\"fake\", nonce=\"n0nc3\", algorithm=MD5"
                        .into(),
                ],
            )
        } else {
            match self.reply {
                Reply::Silent => return None,
                Reply::Code(c, r) => (c, r, vec![]),
            }
        };
        let mut lines = vec![format!("SIP/2.0 {code} {reason}")];
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
        lines.extend(extra);
        if code == 200
            && let Some(c) = header("Contact")
        {
            lines.push(format!("Contact: {c};expires={}", expires.unwrap_or(0)));
        }
        lines.push("Content-Length: 0".into());
        Some(format!("{}\r\n\r\n", lines.join("\r\n")))
    }
}

struct Fake {
    addr: SocketAddr,
    behaviour: Arc<Behaviour>,
    stop: Arc<AtomicBool>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn udp_registrar(reply: Reply) -> Fake {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let addr = sock.local_addr().unwrap();
    let behaviour = Behaviour::new(reply);
    let stop = Arc::new(AtomicBool::new(false));
    let (b, s) = (behaviour.clone(), stop.clone());
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while !s.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
            if let Some(r) = b.answer(&text, from) {
                let _ = sock.send_to(r.as_bytes(), from);
            }
        }
    });
    Fake {
        addr,
        behaviour,
        stop,
    }
}

/// TLS registrar with a fresh self-signed certificate; returns it with the certificate's SHA-256.
fn tls_registrar(reply: Reply) -> (Fake, String) {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    siple_sip::init_crypto();
    let (cert_pem, key_pem) =
        siple_sip::transport::self_signed("127.0.0.1".parse().unwrap()).unwrap();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    let fp: String = siple_sip::transport::cert_sha256(&certs[0])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).unwrap();
    let cfg = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let behaviour = Behaviour::new(reply);
    let stop = Arc::new(AtomicBool::new(false));
    let (b, s) = (behaviour.clone(), stop.clone());
    std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let Ok((tcp, from)) = listener.accept() else {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            };
            let (cfg, b, s) = (cfg.clone(), b.clone(), s.clone());
            std::thread::spawn(move || {
                tcp.set_nonblocking(false).unwrap();
                tcp.set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let conn = rustls::ServerConnection::new(cfg).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                let mut buf = vec![0u8; 65536];
                while !s.load(Ordering::Relaxed) {
                    match tls.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                            if let Some(r) = b.answer(&text, from)
                                && tls.write_all(r.as_bytes()).is_err()
                            {
                                break;
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
            });
        }
    });
    (
        Fake {
            addr,
            behaviour,
            stop,
        },
        fp,
    )
}

// --- the stack under test --------------------------------------------------------------------------------

/// DNS / route behaviour tests switch; `127.0.0.1` literals resolve to themselves.
#[derive(Default)]
struct Probe {
    dns_fail: AtomicBool,
    no_route: AtomicBool,
    /// What `transport = "auto"` resolves to.
    auto: Mutex<Option<(Transport, SocketAddr)>>,
    auto_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl NetProbe for Probe {
    fn local_ip_toward(&self, _: SocketAddr) -> std::io::Result<IpAddr> {
        if self.no_route.load(Ordering::SeqCst) {
            Err(std::io::Error::from(std::io::ErrorKind::NetworkUnreachable))
        } else {
            Ok("127.0.0.1".parse().unwrap())
        }
    }
    async fn resolve(
        &self,
        host: &str,
        port: Option<u16>,
        _: Transport,
    ) -> siple_sip::Result<Vec<SocketAddr>> {
        if self.no_route.load(Ordering::SeqCst) {
            return Err(SipError::Dns(format!("{host}: network is down")));
        }
        if self.dns_fail.load(Ordering::SeqCst) {
            return Err(SipError::Dns(format!("{host}: no such host")));
        }
        host.parse::<IpAddr>()
            .map(|ip| vec![SocketAddr::new(ip, port.unwrap_or(5060))])
            .map_err(|_| SipError::Dns(format!("{host}: nxdomain")))
    }
    async fn resolve_auto(
        &self,
        host: &str,
        port: Option<u16>,
    ) -> siple_sip::Result<(Transport, Vec<SocketAddr>)> {
        self.auto_calls.fetch_add(1, Ordering::SeqCst);
        if let Some((t, a)) = *self.auto.lock().unwrap() {
            return Ok((t, vec![a]));
        }
        Ok((
            Transport::Udp,
            self.resolve(host, port, Transport::Udp).await?,
        ))
    }
}

struct QuietMonitor;

impl NetMonitor for QuietMonitor {
    fn start(&self, _: UnboundedSender<NetReason>) -> std::io::Result<Box<dyn Send>> {
        Ok(Box::new(()))
    }
}

fn stack(probe: Arc<Probe>) -> SipStack {
    let (tx, rx) = unbounded_channel();
    std::mem::forget(rx);
    SipStack::with_net(
        SipStackOptions {
            user_agent_default: "Siple-test/1".into(),
            events: tx,
            wire_sink: Arc::new(NullWireSink),
        },
        NetSetup {
            probe: Some(probe),
            monitor: Some(Arc::new(QuietMonitor)),
            ..Default::default()
        },
    )
}

fn account(addr: SocketAddr, transport: Transport) -> AccountConfig {
    AccountConfig {
        id: "t".into(),
        username: "1001".into(),
        password: Secret::new("s3cret-pw"),
        domain: "fake.test".into(),
        registrar: addr.to_string(),
        transport,
        bind_address: "127.0.0.1".into(),
        keepalive_secs: 0,
        ..Default::default()
    }
}

const SHORT: Duration = Duration::from_secs(5);

// --- account.test ----------------------------------------------------------------------------------

#[tokio::test]
async fn ok_registers_then_unregisters() {
    let fake = udp_registrar(Reply::Code(200, "OK"));
    let s = stack(Arc::default());
    let d = s
        .test_account(account(fake.addr, Transport::Udp), SHORT)
        .await;
    let Diagnosis::Ok {
        transport,
        auto,
        server,
        granted_secs,
        ..
    } = &d
    else {
        panic!("{d:?}");
    };
    assert_eq!((*transport, *auto), (Transport::Udp, false));
    assert_eq!(server, &fake.addr.to_string());
    assert_eq!(*granted_secs, 60);
    assert!(d.message().starts_with("Registered via UDP in "));
    let seen = fake.behaviour.seen.lock().unwrap().clone();
    // challenge + authenticated register (60 s), then the unregister (challenge + expires 0)
    assert!(seen.contains(&(true, Some(60))), "{seen:?}");
    assert!(
        seen.iter().any(|(_, e)| *e == Some(0)),
        "unregistered: {seen:?}"
    );
    s.shutdown().await;
}

#[tokio::test]
async fn wrong_password_is_auth_failed() {
    for code in [401, 403] {
        let fake = udp_registrar(Reply::Code(code, "No"));
        let s = stack(Arc::default());
        let d = s
            .test_account(account(fake.addr, Transport::Udp), SHORT)
            .await;
        assert_eq!(d, Diagnosis::AuthFailed { code });
        assert_eq!(d.problem(), Some(Problem::AuthFailed));
        assert!(d.message().contains("Wrong username or password"));
        s.shutdown().await;
    }
}

#[tokio::test]
async fn unknown_user_is_not_found() {
    let fake = udp_registrar(Reply::Code(404, "Not Found"));
    let s = stack(Arc::default());
    let d = s
        .test_account(account(fake.addr, Transport::Udp), SHORT)
        .await;
    assert_eq!(d, Diagnosis::NotFound);
    assert_eq!(d.problem(), Some(Problem::UserNotFound));
    s.shutdown().await;
}

#[tokio::test]
async fn other_status_is_reported_with_code_and_reason() {
    let fake = udp_registrar(Reply::Code(503, "Service Unavailable"));
    let s = stack(Arc::default());
    let d = s
        .test_account(account(fake.addr, Transport::Udp), SHORT)
        .await;
    assert_eq!(
        d,
        Diagnosis::Other {
            code: 503,
            reason: "Service Unavailable".into()
        }
    );
    assert_eq!(d.problem(), Some(Problem::ServerError));
    s.shutdown().await;
}

#[tokio::test]
async fn silence_times_out_with_a_tcp_tls_hint_on_udp() {
    let fake = udp_registrar(Reply::Silent);
    let s = stack(Arc::default());
    let started = std::time::Instant::now();
    let d = s
        .test_account(
            account(fake.addr, Transport::Udp),
            Duration::from_millis(1500),
        )
        .await;
    assert!(started.elapsed() < Duration::from_secs(5), "bounded");
    let Diagnosis::Timeout { transport, hint } = &d else {
        panic!("{d:?}");
    };
    assert_eq!(*transport, Transport::Udp);
    assert!(hint.contains("TCP or TLS"), "{hint}");
    assert_eq!(d.problem(), Some(Problem::NoResponse));
    s.shutdown().await;
}

#[tokio::test]
async fn dns_failure_and_no_route_are_told_apart() {
    let probe = Arc::new(Probe::default());
    let s = stack(probe.clone());
    let mut cfg = account("127.0.0.1:5060".parse().unwrap(), Transport::Udp);
    cfg.registrar = "pbx.invalid".into();
    probe.dns_fail.store(true, Ordering::SeqCst);
    let d = s.test_account(cfg.clone(), SHORT).await;
    assert!(matches!(d, Diagnosis::DnsFailed { .. }), "{d:?}");
    assert_eq!(d.problem(), Some(Problem::DnsFailed));
    probe.dns_fail.store(false, Ordering::SeqCst);
    probe.no_route.store(true, Ordering::SeqCst);
    cfg.bind_address.clear(); // an explicit bind address skips the route check
    let d = s.test_account(cfg, SHORT).await;
    assert!(matches!(d, Diagnosis::NetworkUnreachable { .. }), "{d:?}");
    assert_eq!(d.problem(), Some(Problem::NetworkUnreachable));
    s.shutdown().await;
}

#[tokio::test]
async fn the_password_never_appears_in_a_diagnosis() {
    let fake = udp_registrar(Reply::Code(401, "No"));
    let s = stack(Arc::default());
    let d = s
        .test_account(account(fake.addr, Transport::Udp), SHORT)
        .await;
    let text = format!(
        "{d:?} {} {}",
        d.message(),
        serde_json::to_string(&d).unwrap()
    );
    assert!(!text.contains("s3cret-pw"));
    s.shutdown().await;
}

#[tokio::test]
async fn self_signed_tls_is_untrusted_with_its_fingerprint_then_trusted_by_pinning_it() {
    let (fake, fp) = tls_registrar(Reply::Code(200, "OK"));
    let s = stack(Arc::default());
    let cfg = account(fake.addr, Transport::Tls);
    let d = s.test_account(cfg.clone(), SHORT).await;
    let Diagnosis::TlsUntrusted {
        fingerprint,
        reason,
    } = &d
    else {
        panic!("{d:?}");
    };
    assert_eq!(fingerprint, &fp);
    assert!(!reason.is_empty());
    assert!(d.message().contains(&fp));
    assert_eq!(d.problem(), Some(Problem::TlsUntrusted));

    // "Trust this server": the fingerprint goes to `tls_fingerprint`; the same test now registers.
    let mut pinned = cfg;
    pinned.tls_fingerprint = fingerprint.clone();
    let d = s.test_account(pinned, SHORT).await;
    assert!(
        matches!(
            d,
            Diagnosis::Ok {
                transport: Transport::Tls,
                ..
            }
        ),
        "{d:?}"
    );
    assert_eq!(d.problem(), None);
    s.shutdown().await;
}

#[tokio::test]
async fn a_running_tls_account_with_an_untrusted_certificate_fails_with_tls_untrusted() {
    let (fake, _fp) = tls_registrar(Reply::Code(200, "OK"));
    let s = stack(Arc::default());
    s.add_account(
        account(fake.addr, Transport::Tls),
        &GeneralConfig::default(),
        &MediaConfig::default(),
    )
    .await
    .unwrap();
    until_status(&s, "failed", |st| {
        matches!(st.registration, RegState::Failed { .. })
    })
    .await;
    let reg = s.account_status("t").unwrap().registration;
    assert_eq!(reg.problem(), Some(Problem::TlsUntrusted), "{reg:?}");
    s.shutdown().await;
}

#[tokio::test]
async fn a_wrong_pin_reports_the_actual_fingerprint() {
    let (fake, fp) = tls_registrar(Reply::Code(200, "OK"));
    let s = stack(Arc::default());
    let mut cfg = account(fake.addr, Transport::Tls);
    cfg.tls_fingerprint = "00".repeat(32);
    let d = s.test_account(cfg, SHORT).await;
    let Diagnosis::TlsUntrusted {
        fingerprint,
        reason,
    } = &d
    else {
        panic!("{d:?}");
    };
    assert_eq!(fingerprint, &fp);
    assert!(reason.contains("pinned"), "{reason}");
    s.shutdown().await;
}

#[tokio::test]
async fn the_test_does_not_disturb_a_running_account() {
    let fake = udp_registrar(Reply::Code(200, "OK"));
    let s = stack(Arc::default());
    let cfg = account(fake.addr, Transport::Udp);
    s.add_account(
        cfg.clone(),
        &GeneralConfig::default(),
        &MediaConfig::default(),
    )
    .await
    .unwrap();
    let before = s.account_status("t").unwrap();
    let mut probe_cfg = cfg;
    probe_cfg.id = "draft".into();
    let d = s.test_account(probe_cfg, SHORT).await;
    assert!(d.is_ok(), "{d:?}");
    let after = s.account_status("t").unwrap();
    assert_eq!(before.local_addr, after.local_addr, "same socket");
    assert!(s.account_status("draft").is_err(), "no account was created");
    s.shutdown().await;
}

// --- transport = "auto" ----------------------------------------------------------------------------

async fn until_status(s: &SipStack, what: &str, pred: impl Fn(&siple_sip::AccountStatus) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let st = s.account_status("t").unwrap();
        if pred(&st) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out: {what}: {st:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn auto_registers_on_the_picked_transport_and_reports_it() {
    let fake = udp_registrar(Reply::Code(200, "OK"));
    let probe = Arc::new(Probe::default());
    let s = stack(probe.clone());
    let mut cfg = account(fake.addr, Transport::Auto);
    cfg.expires = 120;
    s.add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
        .await
        .unwrap();
    until_status(&s, "registered", |st| {
        matches!(st.registration, RegState::Registered { .. })
    })
    .await;
    let st = s.account_status("t").unwrap();
    assert_eq!(st.transport, Transport::Auto);
    assert_eq!(st.transport_effective, Some(Transport::Udp));
    assert!(probe.auto_calls.load(Ordering::SeqCst) >= 1);
    s.shutdown().await;
}

#[tokio::test]
async fn a_reprobe_may_reselect_the_transport_without_calls() {
    let fake = udp_registrar(Reply::Code(200, "OK"));
    let probe = Arc::new(Probe::default());
    let s = stack(probe.clone());
    s.add_account(
        account(fake.addr, Transport::Auto),
        &GeneralConfig::default(),
        &MediaConfig::default(),
    )
    .await
    .unwrap();
    until_status(&s, "registered", |st| {
        matches!(st.registration, RegState::Registered { .. })
    })
    .await;
    assert_eq!(
        s.account_status("t").unwrap().transport_effective,
        Some(Transport::Udp)
    );

    // DNS now prefers TCP (nothing listens there: the account fails, but it was rebuilt for TCP).
    let dead_tcp = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    *probe.auto.lock().unwrap() = Some((Transport::Tcp, dead_tcp));
    s.network_refresh().await;
    until_status(&s, "re-selected tcp", |st| {
        st.transport_effective == Some(Transport::Tcp)
    })
    .await;
    assert_eq!(s.account_status("t").unwrap().transport, Transport::Auto);
    s.shutdown().await;
}

#[tokio::test]
async fn a_fixed_transport_reports_itself_as_effective() {
    let fake = udp_registrar(Reply::Code(200, "OK"));
    let s = stack(Arc::default());
    s.add_account(
        account(fake.addr, Transport::Udp),
        &GeneralConfig::default(),
        &MediaConfig::default(),
    )
    .await
    .unwrap();
    let st = s.account_status("t").unwrap();
    assert_eq!(st.transport_effective, Some(Transport::Udp));
    s.shutdown().await;
}
