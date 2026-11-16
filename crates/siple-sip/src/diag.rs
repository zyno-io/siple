//! `account.test`: a one-off REGISTER (then unREGISTER) with given account settings,
//! on its own socket and endpoint, so a running account is not disturbed, and a structured
//! diagnosis of the outcome.
//!
//! The probe uses an ephemeral local port, never the account's `local_port`. A registrar that
//! allows only one binding per address of record may drop the running account's binding for the
//! moment the test is registered; the next refresh of the account restores it.
//!
//! TLS: the server certificate verifier is wrapped so the handshake records the leaf
//! certificate's SHA-256. A verification failure is reported as [`Diagnosis::TlsUntrusted`] with
//! that fingerprint, which a UI can offer to pin (`tls_fingerprint`).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rsipstack::sip::Uri;
use rsipstack::transaction::EndpointBuilder;
use rsipstack::transaction::endpoint::EndpointOption;
use rsipstack::transport::udp::UdpConnection;
use rsipstack::transport::{SipAddr, SipConnection, TransportLayer};
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde::Serialize;
use siple_config::{AccountConfig, Transport};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::account::{CALLID_SUFFIX, credential, probe, sip_transport};
use crate::authz::AuthState;
use crate::client::{Client, ContactSel, RegIdentity, RegResult};
use crate::dns::split_host_port;
use crate::error::SipError;
use crate::net::{NetProbe, is_unreachable_text};
use crate::transport::{bind_udp, cert_sha256, client_tls_config, init_crypto};

/// Longest time an `account.test` may take, and the default.
pub const TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Bound on the unREGISTER that follows a successful test.
const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(3);

/// Outcome of [`SipStack::test_account`](crate::SipStack::test_account).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "diagnosis", rename_all = "snake_case")]
pub enum Diagnosis {
    /// The registrar accepted the REGISTER.
    Ok {
        /// Transport used (what `auto` picked, if configured).
        transport: Transport,
        /// The account asked for `transport = "auto"`.
        auto: bool,
        /// Registrar address the request went to.
        server: String,
        /// Registration lifetime the registrar granted, in seconds.
        granted_secs: u32,
        /// Time from start to the 2xx.
        elapsed_ms: u64,
    },
    /// 401 / 403 / 407 after the digest answer: wrong username or password.
    AuthFailed {
        /// Final status code.
        code: u16,
    },
    /// 404: the registrar does not know this user.
    NotFound,
    /// No answer in time.
    Timeout {
        /// Transport that was tried.
        transport: Transport,
        /// What to try next.
        hint: String,
    },
    /// The TLS handshake was refused because the server certificate is not trusted (unknown
    /// issuer, wrong name, expired, or a different pinned fingerprint).
    TlsUntrusted {
        /// SHA-256 of the server's leaf certificate (DER), lowercase hex without separators:
        /// the value to write to `tls_fingerprint`.
        fingerprint: String,
        /// Why verification failed.
        reason: String,
    },
    /// The server name did not resolve.
    DnsFailed {
        /// Resolver message.
        message: String,
    },
    /// There is no route toward the server (network down).
    NetworkUnreachable {
        /// What the OS said.
        message: String,
    },
    /// Anything else: another SIP status, a refused connection, a bad setting.
    Other {
        /// SIP status code, 0 for local failures.
        code: u16,
        /// Reason phrase or local error text.
        reason: String,
    },
}

impl Diagnosis {
    /// Whether the registrar accepted the account.
    pub fn is_ok(&self) -> bool {
        matches!(self, Diagnosis::Ok { .. })
    }

    /// One line for a user.
    pub fn message(&self) -> String {
        match self {
            Diagnosis::Ok {
                transport,
                auto,
                elapsed_ms,
                ..
            } => format!(
                "Registered via {}{} in {:.1} s",
                transport_label(*transport),
                if *auto { " (auto)" } else { "" },
                *elapsed_ms as f64 / 1000.0
            ),
            Diagnosis::AuthFailed { code } => {
                format!("Wrong username or password (the server answered {code})")
            }
            Diagnosis::NotFound => "User not found on this server".into(),
            Diagnosis::Timeout { hint, .. } => hint.clone(),
            Diagnosis::TlsUntrusted {
                fingerprint,
                reason,
            } => format!(
                "The server's TLS certificate is not trusted ({reason}); its SHA-256 fingerprint is {fingerprint}"
            ),
            Diagnosis::DnsFailed { message } => format!("Cannot find the server: {message}"),
            Diagnosis::NetworkUnreachable { message } => {
                format!("No network route to the server: {message}")
            }
            Diagnosis::Other { code: 0, reason } => reason.clone(),
            Diagnosis::Other { code, reason } => format!("The server answered {code} {reason}"),
        }
    }
}

fn transport_label(t: Transport) -> &'static str {
    match t {
        Transport::Udp => "UDP",
        Transport::Tcp => "TCP",
        Transport::Tls => "TLS",
        Transport::Auto => "auto",
    }
}

/// What the handshake saw of the server certificate.
#[derive(Default)]
struct Seen {
    fingerprint: Option<String>,
    /// Set when verification failed.
    failure: Option<String>,
}

/// Wraps the account's verifier and records the leaf certificate and the verdict.
#[derive(Debug)]
struct RecordingVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    seen: Arc<Mutex<Seen>>,
    failed: Arc<Notify>,
}

impl std::fmt::Debug for Seen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seen")
    }
}

impl ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp: String = cert_sha256(end_entity)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let verdict =
            self.inner
                .verify_server_cert(end_entity, intermediates, server_name, ocsp, now);
        {
            let mut s = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            s.fingerprint = Some(fp);
            if let Err(e) = &verdict {
                s.failure = Some(describe_tls_error(e));
            }
        }
        if verdict.is_err() {
            self.failed.notify_one();
        }
        verdict
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn describe_tls_error(e: &rustls::Error) -> String {
    match e {
        rustls::Error::InvalidCertificate(c) => match c {
            rustls::CertificateError::UnknownIssuer => "unknown issuer".into(),
            rustls::CertificateError::Expired | rustls::CertificateError::ExpiredContext { .. } => {
                "certificate expired".into()
            }
            rustls::CertificateError::NotValidForName
            | rustls::CertificateError::NotValidForNameContext { .. } => {
                "certificate is for another name".into()
            }
            rustls::CertificateError::ApplicationVerificationFailure => {
                "does not match the pinned fingerprint".into()
            }
            other => format!("{other:?}"),
        },
        other => other.to_string(),
    }
}

/// Registers `cfg` once (and unregisters), within `total`. Never panics; every failure is a
/// [`Diagnosis`].
pub(crate) async fn run(
    mut cfg: AccountConfig,
    probe_src: Arc<dyn NetProbe>,
    default_ua: &str,
    total: Duration,
) -> Diagnosis {
    let started = Instant::now();
    let deadline = total;
    // The REGISTER gets `total`; the unREGISTER that follows a success has its own allowance.
    match tokio::time::timeout(
        deadline + UNREGISTER_TIMEOUT + Duration::from_secs(1),
        attempt(&mut cfg, probe_src, default_ua, started, total),
    )
    .await
    {
        Ok(d) => d,
        Err(_) => timeout_diagnosis(cfg.transport),
    }
}

fn timeout_diagnosis(transport: Transport) -> Diagnosis {
    let hint = match transport {
        Transport::Udp => {
            "No response over UDP: check the server address and the firewall, or try TCP or TLS"
        }
        Transport::Auto => "No response: check the server address and the firewall",
        _ => "No response: check the server address, the port and the firewall",
    };
    Diagnosis::Timeout {
        transport,
        hint: hint.into(),
    }
}

fn from_error(e: &SipError, transport: Transport) -> Diagnosis {
    match e {
        SipError::Dns(m) => Diagnosis::DnsFailed { message: m.clone() },
        SipError::NoRoute(m) => Diagnosis::NetworkUnreachable { message: m.clone() },
        SipError::Timeout => timeout_diagnosis(transport),
        other => {
            let text = other.to_string();
            if is_unreachable_text(&text) {
                Diagnosis::NetworkUnreachable { message: text }
            } else {
                Diagnosis::Other {
                    code: 0,
                    reason: text,
                }
            }
        }
    }
}

async fn attempt(
    cfg: &mut AccountConfig,
    probe_src: Arc<dyn NetProbe>,
    default_ua: &str,
    started: Instant,
    budget: Duration,
) -> Diagnosis {
    init_crypto();
    let configured = cfg.transport;
    cfg.register = true;
    cfg.local_port = 0;
    let hop = if cfg.outbound_proxy.is_empty() {
        cfg.effective_registrar().to_string()
    } else {
        cfg.outbound_proxy.clone()
    };
    if hop.is_empty() {
        return Diagnosis::Other {
            code: 0,
            reason: "no domain or registrar to register with".into(),
        };
    }
    let probed = match probe(cfg, probe_src.as_ref(), None).await {
        Ok(p) => p,
        Err(e) => return from_error(&e, configured),
    };
    cfg.transport = probed.transport;
    let transport = probed.transport;
    let Some(target) = probed.target else {
        return Diagnosis::DnsFailed {
            message: format!("no addresses for {hop}"),
        };
    };

    let seen = Arc::new(Mutex::new(Seen::default()));
    let failed = Arc::new(Notify::new());
    let token = CancellationToken::new();
    let result = register_once(
        cfg,
        default_ua,
        &token,
        probed.local_ip,
        target,
        (&seen, &failed),
        configured,
        started,
        budget,
    )
    .await;
    token.cancel();
    // A TLS verification failure beats whatever the aborted handshake looked like afterwards.
    if transport == Transport::Tls && !result.is_ok() {
        let (failure, fingerprint) = {
            let s = seen.lock().unwrap_or_else(PoisonError::into_inner);
            (s.failure.clone(), s.fingerprint.clone())
        };
        if let Some(reason) = failure {
            return Diagnosis::TlsUntrusted {
                fingerprint: fingerprint.unwrap_or_default(),
                reason,
            };
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn register_once(
    cfg: &AccountConfig,
    default_ua: &str,
    token: &CancellationToken,
    local_ip: std::net::IpAddr,
    target: SocketAddr,
    (seen, failed): (&Arc<Mutex<Seen>>, &Arc<Notify>),
    configured: Transport,
    started: Instant,
    budget: Duration,
) -> Diagnosis {
    let transport = cfg.transport;
    let tl = TransportLayer::new(token.clone());
    let mut tls_config = None;
    if transport == Transport::Tls {
        let hop = split_host_port(if cfg.outbound_proxy.is_empty() {
            cfg.effective_registrar()
        } else {
            &cfg.outbound_proxy
        })
        .0;
        let sni = hop
            .parse::<std::net::IpAddr>()
            .is_err()
            .then_some(hop)
            .filter(|h| !h.is_empty());
        match client_tls_config(cfg, sni, Some(target)) {
            Ok(mut t) => {
                if let Some(inner) = t.verifier.take() {
                    t.verifier = Some(Arc::new(RecordingVerifier {
                        inner,
                        seen: seen.clone(),
                        failed: failed.clone(),
                    }));
                }
                tls_config = Some(t);
            }
            Err(e) => return from_error(&e, configured),
        }
    }
    if let Some(t) = tls_config {
        tl.set_tls_config(t);
    }
    if transport == Transport::Udp {
        let sock = match bind_udp(SocketAddr::new(local_ip, 0)) {
            Ok(s) => s,
            Err(e) => return from_error(&e.into(), configured),
        };
        match UdpConnection::from_socket(Arc::new(sock), None, Some(token.child_token())).await {
            Ok(c) => tl.add_transport(SipConnection::Udp(c)),
            Err(e) => return from_error(&e.into(), configured),
        }
    }
    let endpoint = EndpointBuilder::new()
        .with_cancel_token(token.clone())
        .with_transport_layer(tl.clone())
        .with_user_agent(if cfg.user_agent.is_empty() {
            default_ua
        } else {
            &cfg.user_agent
        })
        .with_option(EndpointOption {
            use_sips_for_tls: cfg.use_sips_uri,
            callid_suffix: Some(CALLID_SUFFIX.into()),
            ..Default::default()
        })
        .build();
    let _incoming = endpoint.incoming_transactions();
    let serving = endpoint.inner.clone();
    tokio::spawn(async move {
        let _ = serving.serve().await;
    });

    let registrar = cfg.effective_registrar().to_string();
    let (Ok(aor), Ok(registrar_uri)) = (
        Uri::try_from(cfg.aor().as_str()),
        Uri::try_from(format!("sip:{registrar}").as_str()),
    ) else {
        return Diagnosis::Other {
            code: 0,
            reason: "username or domain is not usable in a SIP address".into(),
        };
    };
    let probe_target = SipAddr::new(sip_transport(transport), target.into());
    let auth = AuthState::new();
    auth.update(cfg, Some((cfg, &[target])));
    let client = Client {
        endpoint: endpoint.inner.clone(),
        target: Some(probe_target),
        transport: sip_transport(transport),
        use_sips: cfg.use_sips_uri,
        public_host: None,
        cred: credential(cfg, &auth),
        user: cfg.username.clone(),
        display_name: cfg.display_name.clone(),
        aor,
        registrar_uri: Some(registrar_uri),
        extra_headers: Vec::new(),
        supported: cfg.supported_tokens(),
        route: (cfg.outbound_proxy_route && !cfg.outbound_proxy.is_empty())
            .then(|| format!("sip:{}", cfg.outbound_proxy)),
        identity: RegIdentity::new(),
    };

    let outcome = tokio::select! {
        r = client.register(60, ContactSel::Auto) => r,
        () = failed.notified() => Err(SipError::Tls("certificate not trusted".into())),
        () = tokio::time::sleep(budget.saturating_sub(started.elapsed())) => Err(SipError::Timeout),
    };
    match outcome {
        Ok(RegResult::Ok(ok)) => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            // Leave no binding behind (best effort, bounded).
            let _ = tokio::time::timeout(UNREGISTER_TIMEOUT, client.register(0, ContactSel::Bound))
                .await;
            Diagnosis::Ok {
                transport,
                auto: configured.is_auto(),
                server: target.to_string(),
                granted_secs: ok.granted,
                elapsed_ms,
            }
        }
        Ok(RegResult::NothingBound) => Diagnosis::Other {
            code: 0,
            reason: "nothing registered".into(),
        },
        Ok(RegResult::Rejected { code, reason, .. }) => match code {
            401 | 403 | 407 => Diagnosis::AuthFailed { code },
            404 => Diagnosis::NotFound,
            _ => Diagnosis::Other { code, reason },
        },
        Err(e) => from_error(&e, configured),
    }
}
