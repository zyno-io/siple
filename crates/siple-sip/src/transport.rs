//! Socket and TLS helpers.

use std::net::{IpAddr, SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::Arc;

use ring::digest as ring_digest;
use rsipstack::transport::{TlsConfig, TransportLayer};
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use siple_config::AccountConfig;
use socket2::{Domain, Protocol, Socket, Type};

use crate::authz::{AuthState, Endpoint};
use crate::error::{Result, SipError};

/// Installs the `ring` rustls crypto provider as the process default. Idempotent; rsipstack's
/// TLS client uses the process default, and a second provider in the dependency graph would
/// otherwise make it panic.
pub fn init_crypto() {
    if CryptoProvider::get_default().is_none() {
        // Err means another thread won the race; that is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

fn ring() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// UDP-`connect()` a throwaway socket to the next hop and read the local address the
/// OS routes from. No packet is sent.
pub fn local_ip_toward(dest: SocketAddr) -> std::io::Result<IpAddr> {
    let bind: SocketAddr = if dest.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let s = StdUdpSocket::bind(bind)?;
    s.connect(dest)?;
    Ok(s.local_addr()?.ip())
}

/// Binds the account's UDP socket. No `SO_REUSEADDR` (a fixed `local_port` is exclusive);
/// on Windows `SO_EXCLUSIVEADDRUSE` is set explicitly so no other process can hijack the port.
pub fn bind_udp(addr: SocketAddr) -> std::io::Result<tokio::net::UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    #[cfg(windows)]
    set_exclusive_addr_use(&socket)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    tokio::net::UdpSocket::from_std(socket.into())
}

/// An account's UDP SIP socket as handed to rsipstack: [`AccountUdp::close`] releases the
/// socket (and so its port) at once, although transactions and dialogs still hold clones of the
/// connection around it, so a dead socket can be replaced by a new one on the same address.
pub(crate) struct AccountUdp {
    sock: std::sync::RwLock<Option<Arc<tokio::net::UdpSocket>>>,
    local: SocketAddr,
    /// Wakes a pending receive when the socket is closed or (test hook) killed.
    wake: tokio::sync::Notify,
    /// Test hook ([`AccountUdp::simulate_death`]): every call fails like on a socket the OS has
    /// torn down (macOS "defunct": send `EPIPE`, receive `ENOTCONN`).
    dead: std::sync::atomic::AtomicBool,
}

impl AccountUdp {
    /// Binds `addr` ([`bind_udp`]).
    pub(crate) fn bind(addr: SocketAddr) -> std::io::Result<Arc<Self>> {
        let sock = bind_udp(addr)?;
        let local = sock.local_addr()?;
        Ok(Arc::new(Self {
            sock: std::sync::RwLock::new(Some(Arc::new(sock))),
            local,
            wake: tokio::sync::Notify::new(),
            dead: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// The bound address.
    pub(crate) fn local(&self) -> SocketAddr {
        self.local
    }

    /// Closes the socket: later calls fail with `NotConnected`, a pending receive ends, and the
    /// descriptor is released once a send already in progress returns.
    pub(crate) fn close(&self) {
        let sock = self
            .sock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(sock);
        self.wake.notify_waiters();
    }

    /// Replaces a dead socket by a new one bound to the same address. The old descriptor is
    /// released first (still held for a moment by a send in progress on it: then this fails
    /// with `AddrInUse` and can be retried). Until it succeeds, calls fail with `NotConnected`.
    pub(crate) fn rebind(&self) -> std::io::Result<()> {
        let mut slot = self
            .sock
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(slot.take());
        let sock = bind_udp(self.local)?;
        *slot = Some(Arc::new(sock));
        self.dead.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Test hook: from now on every call fails as on a socket the OS has torn down.
    pub(crate) fn simulate_death(&self) {
        self.dead.store(true, std::sync::atomic::Ordering::SeqCst);
        self.wake.notify_waiters();
    }

    fn current(&self, dead: std::io::ErrorKind) -> std::io::Result<Arc<tokio::net::UdpSocket>> {
        if self.dead.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(dead.into());
        }
        self.sock
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| std::io::ErrorKind::NotConnected.into())
    }
}

#[async_trait::async_trait]
impl rsipstack::platform::net::UdpSocket for AccountUdp {
    async fn recv_from(&self, buf: &mut [u8]) -> rsipstack::Result<(usize, SocketAddr)> {
        // Registered before the state is read, so a close / kill in between is not missed.
        let woken = self.wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        let sock = self.current(std::io::ErrorKind::NotConnected)?;
        tokio::select! {
            r = sock.recv_from(buf) => {
                let (len, from) = r?;
                Ok((len, rsipstack::platform::net::canonical_socket_addr(from)))
            }
            () = woken => {
                // Closed or killed (a spurious wake-up reads as an interrupted call).
                let e = match self.current(std::io::ErrorKind::NotConnected) {
                    Err(e) => e,
                    Ok(_) => std::io::ErrorKind::Interrupted.into(),
                };
                Err(e.into())
            }
        }
    }

    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> rsipstack::Result<usize> {
        let sock = self.current(std::io::ErrorKind::BrokenPipe)?;
        Ok(sock.send_to(buf, addr).await?)
    }

    fn local_addr(&self) -> rsipstack::Result<SocketAddr> {
        Ok(self.local)
    }
}

#[cfg(windows)]
fn set_exclusive_addr_use(socket: &Socket) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET_ERROR, SOL_SOCKET, setsockopt,
    };
    let on: u32 = 1;
    // SAFETY: `socket` is a valid open socket for the duration of the call and `on` outlives it.
    let rc = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const u32).cast(),
            std::mem::size_of::<u32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `tls_verify = false`, for the account's provider only ([`ScopedVerifier`]): accepts any
/// server certificate (signatures are still checked structurally by the provider).
#[derive(Debug)]
pub struct AcceptAllVerifier(Arc<CryptoProvider>);

impl AcceptAllVerifier {
    /// New verifier backed by the ring provider.
    pub fn new() -> Self {
        Self(ring())
    }
}

impl Default for AcceptAllVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerCertVerifier for AcceptAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Refuses every server certificate: the verifier for non-provider hosts when no CA roots can be
/// loaded (see [`make_verifier`]), so the provider's pin / no-verify still works.
#[derive(Debug)]
struct DenyAllVerifier(Arc<CryptoProvider>);

impl ServerCertVerifier for DenyAllVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Err(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ))
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `tls_fingerprint`: accepts exactly the server leaf certificate whose SHA-256 (of the DER)
/// matches, regardless of chain, validity period or name. Handshake signatures are still verified
/// against that certificate's key, so possession of the private key is proven.
#[derive(Debug)]
pub struct FingerprintVerifier {
    sha256: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl FingerprintVerifier {
    /// Verifier pinning `sha256`.
    pub fn new(sha256: [u8; 32]) -> Self {
        Self {
            sha256,
            provider: ring(),
        }
    }
}

/// SHA-256 of a certificate's DER encoding.
pub fn cert_sha256(cert: &CertificateDer<'_>) -> [u8; 32] {
    let d = ring_digest::digest(&ring_digest::SHA256, cert.as_ref());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        if cert_sha256(end_entity) == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Reads PEM certificates from `path`.
pub fn read_pem_certs(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    CertificateDer::pem_file_iter(path)
        .map_err(|e| SipError::Tls(format!("{path}: {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| SipError::Tls(format!("{path}: {e}")))
}

/// Which TLS server names belong to the account's provider: the registrar, outbound proxy,
/// `domain`, `tls_server_name` and their addresses. `tls_fingerprint` and `tls_verify = false`
/// apply to these only.
#[derive(Clone)]
pub(crate) struct TlsScope {
    /// Hosts of the runtime's config (what its hop connection is verified under).
    hosts: Vec<String>,
    /// Live provider description: current `domain` / proxy / registrar and resolved addresses.
    auth: Arc<AuthState>,
}

impl TlsScope {
    /// Scope of the transport built for `cfg`.
    pub(crate) fn new(cfg: &AccountConfig, auth: Arc<AuthState>) -> Self {
        let mut hosts: Vec<String> = Vec::new();
        for s in [
            cfg.tls_server_name.as_str(),
            cfg.outbound_proxy.as_str(),
            cfg.effective_registrar(),
        ] {
            // rustls verifies DNS names without their trailing dot.
            if let Some(e) = Endpoint::parse(s)
                && let h = e.host.trim_end_matches('.')
                && !h.is_empty()
                && !hosts.iter().any(|x| x == h)
            {
                hosts.push(h.to_string());
            }
        }
        Self { hosts, auth }
    }

    /// Whether a connection verified under `name` is to the provider.
    pub(crate) fn contains(&self, name: &ServerName<'_>) -> bool {
        match name {
            ServerName::DnsName(d) => {
                let n = d.as_ref().trim_end_matches('.').to_ascii_lowercase();
                self.hosts.contains(&n) || self.auth.is_provider_name(&n)
            }
            ServerName::IpAddress(ip) => {
                let ip = IpAddr::from(*ip).to_canonical();
                self.hosts
                    .iter()
                    .any(|h| h.parse::<IpAddr>().is_ok_and(|x| x.to_canonical() == ip))
                    || self.auth.is_provider_ip(ip)
            }
            _ => false,
        }
    }
}

/// Verifier that applies the account's pin / accept-all (`provider`) to the provider's server
/// names and the OS verifier plus `tls_ca_file` (`others`) to every other name.
struct ScopedVerifier {
    scope: TlsScope,
    provider: Arc<dyn ServerCertVerifier>,
    others: Arc<dyn ServerCertVerifier>,
    crypto: Arc<CryptoProvider>,
}

impl std::fmt::Debug for ScopedVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedVerifier")
            .field("hosts", &self.scope.hosts)
            .finish_non_exhaustive()
    }
}

impl ServerCertVerifier for ScopedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let provider = self.scope.contains(server_name);
        tracing::debug!(name = ?server_name, provider, "TLS certificate check");
        let v = if provider {
            &self.provider
        } else {
            &self.others
        };
        v.verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &self.crypto.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.crypto.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.crypto
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The OS verifier (`rustls-platform-verifier`) plus the extra roots from `tls_ca_file`.
/// Platform verifiers take extra roots natively, which also works for self-signed `CA:TRUE` PBX
/// certificates that plain webpki would reject (`CaUsedAsEndEntity`).
fn os_verifier(cfg: &AccountConfig) -> Result<Arc<dyn ServerCertVerifier>> {
    let extra = if cfg.tls_ca_file.is_empty() {
        Vec::new()
    } else {
        read_pem_certs(&cfg.tls_ca_file)?
    };
    let v = if extra.is_empty() {
        rustls_platform_verifier::Verifier::new(ring())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra, ring())
    }
    .map_err(|e| SipError::Tls(e.to_string()))?;
    Ok(Arc::new(v))
}

/// Server certificate verifier for an account, decided per server name. For the provider
/// (`scope`):
/// 1. `tls_fingerprint` set: pin exactly that leaf certificate ([`FingerprintVerifier`]);
/// 2. `tls_verify = false`: accept-all.
///
/// Every other name, and the provider when neither is set, gets the OS verifier plus the extra
/// roots from `tls_ca_file`. A bad `tls_ca_file` is an error, except when a pin / no-verify
/// setting exists: then only non-provider hosts lose the extra roots.
///
/// Caveat: the macOS platform verifier enforces Apple's certificate policy even for roots
/// supplied through `tls_ca_file`; a self-signed CA whose validity exceeds 825 days is rejected
/// ("not standards compliant", -67901). Use `tls_fingerprint` for such certificates.
pub(crate) fn make_verifier(
    cfg: &AccountConfig,
    scope: TlsScope,
) -> Result<Arc<dyn ServerCertVerifier>> {
    let provider: Arc<dyn ServerCertVerifier> = if !cfg.tls_fingerprint.is_empty() {
        let fp = siple_config::parse_fingerprint(&cfg.tls_fingerprint)
            .ok_or_else(|| SipError::Tls("tls_fingerprint is not a SHA-256 hex digest".into()))?;
        Arc::new(FingerprintVerifier::new(fp))
    } else if !cfg.tls_verify {
        tracing::warn!(
            account = %cfg.id,
            "TLS certificate verification is DISABLED for the provider (tls_verify = false); \
             other TLS hosts are verified"
        );
        Arc::new(AcceptAllVerifier::new())
    } else {
        return os_verifier(cfg);
    };
    // The provider's pin / no-verify must keep working when the other hosts' verifier cannot be
    // built (unreadable `tls_ca_file`, or no OS roots at all: Linux without ca-certificates).
    let others: Arc<dyn ServerCertVerifier> = match os_verifier(cfg) {
        Ok(v) => v,
        Err(e) => match (
            cfg.tls_ca_file.is_empty(),
            rustls_platform_verifier::Verifier::new(ring()),
        ) {
            (false, Ok(v)) => {
                tracing::warn!(
                    account = %cfg.id, error = %e,
                    "tls_ca_file unusable; TLS hosts other than the provider are checked \
                     against the OS roots only"
                );
                Arc::new(v)
            }
            _ => {
                tracing::warn!(
                    account = %cfg.id, error = %e,
                    "no usable CA roots; TLS hosts other than the provider are refused"
                );
                Arc::new(DenyAllVerifier(ring()))
            }
        },
    };
    Ok(Arc::new(ScopedVerifier {
        scope,
        provider,
        others,
        crypto: ring(),
    }))
}

/// Client-side TLS settings for the transport layer. The server name for SNI and verification
/// applies only to the registrar / outbound-proxy connection (`hop`, its resolved address):
/// `tls_server_name` if set, else `default_sni` (the hop host when it is a name). Every other
/// destination uses its own host name (see SIPLE_PATCHES.md, `sni_by_addr`).
pub(crate) fn client_tls_config(
    cfg: &AccountConfig,
    default_sni: Option<String>,
    hop: Option<SocketAddr>,
    scope: TlsScope,
) -> Result<TlsConfig> {
    let sni = if cfg.tls_server_name.is_empty() {
        default_sni
    } else {
        Some(cfg.tls_server_name.clone())
    };
    let mut tls = TlsConfig {
        verifier: Some(make_verifier(cfg, scope)?),
        ..Default::default()
    };
    if let Some(addr) = hop {
        // No name (IP-literal registrar): pin the IP itself, which is what is verified then,
        // so a name learned for another request to this address cannot replace it.
        tls.sni_by_addr
            .insert(addr, sni.unwrap_or_else(|| addr.ip().to_string()));
    }
    if !cfg.tls_cert.is_empty() && !cfg.tls_key.is_empty() {
        tls.client_cert = Some(read_file(&cfg.tls_cert)?);
        tls.client_key = Some(read_file(&cfg.tls_key)?);
    }
    Ok(tls)
}

/// Builds an account's client TLS config, also on demand: a UDP / TCP account gets one the first
/// time Siple sends a TLS request (`sips:` / `;transport=tls`), so bad `tls_*` settings fail
/// only TLS requests.
pub(crate) struct ClientTls {
    /// The config the runtime was built for.
    cfg: AccountConfig,
    auth: Arc<AuthState>,
}

impl ClientTls {
    pub(crate) fn new(cfg: &AccountConfig, auth: Arc<AuthState>) -> Arc<Self> {
        Arc::new(Self {
            cfg: cfg.clone(),
            auth,
        })
    }

    /// The TLS config ([`client_tls_config`]) for the runtime's settings.
    pub(crate) fn config(
        &self,
        default_sni: Option<String>,
        hop: Option<SocketAddr>,
    ) -> Result<TlsConfig> {
        client_tls_config(
            &self.cfg,
            default_sni,
            hop,
            TlsScope::new(&self.cfg, self.auth.clone()),
        )
    }

    /// Installs a config on `tl` unless it has one. A failure is not remembered: the next TLS
    /// request retries.
    pub(crate) fn ensure(&self, tl: &TransportLayer) -> Result<()> {
        if tl.has_tls_config() {
            return Ok(());
        }
        let t = self.config(None, None)?;
        if tl.set_tls_config_if_absent(t) {
            tracing::debug!(account = %self.cfg.id, "client TLS config installed on demand");
        }
        Ok(())
    }
}

fn read_file(path: &str) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| SipError::Tls(format!("{path}: {e}")))
}

/// Server-side TLS settings for the optional listener: `tls_cert`/`tls_key` PEMs, else a
/// self-signed certificate for `local_ip`/`localhost` generated with rcgen.
pub fn listener_tls_config(cfg: &AccountConfig, local_ip: IpAddr) -> Result<TlsConfig> {
    let (cert, key) = if !cfg.tls_cert.is_empty() && !cfg.tls_key.is_empty() {
        (read_file(&cfg.tls_cert)?, read_file(&cfg.tls_key)?)
    } else {
        let (c, k) = self_signed(local_ip)?;
        (c.into_bytes(), k.into_bytes())
    };
    Ok(TlsConfig {
        cert: Some(cert),
        key: Some(key),
        ..Default::default()
    })
}

/// Self-signed certificate + key (PEM) valid for `ip` and `localhost`.
pub fn self_signed(ip: IpAddr) -> Result<(String, String)> {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .map_err(|e| SipError::Tls(e.to_string()))?;
    params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "Siple");
    let key = rcgen::KeyPair::generate().map_err(|e| SipError::Tls(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| SipError::Tls(e.to_string()))?;
    Ok((cert.pem(), key.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_crypto_is_idempotent() {
        init_crypto();
        init_crypto();
        assert!(CryptoProvider::get_default().is_some());
    }

    #[test]
    fn local_ip_toward_loopback_is_loopback() {
        let ip = local_ip_toward("127.0.0.1:5060".parse().unwrap()).unwrap();
        assert!(ip.is_loopback());
    }

    #[tokio::test]
    async fn bind_udp_ephemeral_and_exclusive() {
        let a = bind_udp("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = a.local_addr().unwrap();
        assert_ne!(addr.port(), 0);
        // The same fixed port cannot be bound twice (no SO_REUSEADDR).
        assert!(bind_udp(addr).is_err());
    }

    fn scope(cfg: &AccountConfig) -> TlsScope {
        TlsScope::new(cfg, AuthState::new())
    }

    #[test]
    fn verifier_selection() {
        init_crypto();
        let mut cfg = AccountConfig {
            id: "t".into(),
            ..Default::default()
        };
        assert!(
            make_verifier(&cfg, scope(&cfg)).is_ok(),
            "platform verifier"
        );
        cfg.tls_verify = false;
        assert!(make_verifier(&cfg, scope(&cfg)).is_ok(), "accept-all");
        cfg.tls_verify = true;
        cfg.tls_ca_file = "/nonexistent/ca.pem".into();
        assert!(matches!(
            make_verifier(&cfg, scope(&cfg)),
            Err(SipError::Tls(_))
        ));
    }

    /// A self-signed leaf certificate (DER) and its pin as hex.
    fn leaf() -> (CertificateDer<'static>, String) {
        let (pem, _) = self_signed("127.0.0.1".parse().unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.pem");
        std::fs::write(&path, pem).unwrap();
        let cert = read_pem_certs(path.to_str().unwrap()).unwrap().remove(0);
        let hex = cert_sha256(&cert)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        (cert, hex)
    }

    fn verifies(v: &Arc<dyn ServerCertVerifier>, cert: &CertificateDer<'_>, name: &str) -> bool {
        let name = ServerName::try_from(name.to_string()).unwrap();
        v.verify_server_cert(cert, &[], &name, &[], UnixTime::now())
            .is_ok()
    }

    #[test]
    fn scoped_verifier_applies_pin_to_provider_names_only() {
        init_crypto();
        let (cert, hex) = leaf();
        let cfg = AccountConfig {
            id: "t".into(),
            domain: "pbx.example.com".into(),
            tls_server_name: "sni.example.com".into(),
            tls_fingerprint: hex,
            ..Default::default()
        };
        let auth = AuthState::new();
        let resolved: SocketAddr = "192.0.2.7:5061".parse().unwrap();
        auth.update(&cfg, Some((&cfg, &[resolved])));
        let v = make_verifier(&cfg, TlsScope::new(&cfg, auth.clone())).unwrap();
        for n in [
            "pbx.example.com",
            "PBX.Example.COM",
            "pbx.example.com.",
            "sni.example.com",
            "192.0.2.7",
        ] {
            assert!(verifies(&v, &cert, n), "{n} is the provider");
        }
        for n in ["other.example", "192.0.2.8"] {
            assert!(!verifies(&v, &cert, n), "{n} is not the provider");
        }
        // `auth_foreign` does not widen the scope.
        let foreign = AccountConfig {
            auth_foreign: true,
            ..cfg.clone()
        };
        auth.update(&foreign, None);
        assert!(!verifies(&v, &cert, "other.example"));
        // A hot `domain` change is in scope live.
        let moved = AccountConfig {
            domain: "new.example.com".into(),
            ..cfg.clone()
        };
        assert!(!verifies(&v, &cert, "new.example.com"));
        auth.update(&moved, None);
        assert!(verifies(&v, &cert, "new.example.com"));
        // Configured hosts with a trailing dot match the dot-less name rustls verifies.
        let dotted = AccountConfig {
            domain: "dot.example.com.".into(),
            registrar: "reg.example.com.:5061".into(),
            ..cfg.clone()
        };
        let dauth = AuthState::new();
        dauth.update(&dotted, None);
        let dv = make_verifier(&dotted, TlsScope::new(&dotted, dauth)).unwrap();
        for n in ["dot.example.com", "reg.example.com"] {
            assert!(verifies(&dv, &cert, n), "{n} is the provider");
        }
    }

    #[test]
    fn deny_all_refuses_and_keeps_signatures() {
        init_crypto();
        let (cert, _) = leaf();
        let v: Arc<dyn ServerCertVerifier> = Arc::new(DenyAllVerifier(ring()));
        assert!(!verifies(&v, &cert, "other.example"));
        assert!(!v.supported_verify_schemes().is_empty());
    }

    #[test]
    fn accept_all_is_provider_only() {
        init_crypto();
        let (cert, _) = leaf();
        let cfg = AccountConfig {
            id: "t".into(),
            registrar: "pbx.example.com:5061".into(),
            tls_verify: false,
            ..Default::default()
        };
        let v = make_verifier(&cfg, scope(&cfg)).unwrap();
        assert!(verifies(&v, &cert, "pbx.example.com"));
        assert!(!verifies(&v, &cert, "other.example"));
    }

    #[test]
    fn bad_ca_file_is_soft_with_pin_or_no_verify_and_fatal_otherwise() {
        init_crypto();
        let path = "/nonexistent/ca.pem";
        let mut cfg = AccountConfig {
            id: "t".into(),
            tls_ca_file: path.into(),
            tls_verify: false,
            ..Default::default()
        };
        assert!(make_verifier(&cfg, scope(&cfg)).is_ok(), "no-verify");
        cfg.tls_verify = true;
        cfg.tls_fingerprint = "00".repeat(32);
        assert!(make_verifier(&cfg, scope(&cfg)).is_ok(), "pin");
        cfg.tls_fingerprint.clear();
        let Err(SipError::Tls(m)) = make_verifier(&cfg, scope(&cfg)) else {
            panic!("a bad tls_ca_file is fatal without a pin");
        };
        assert!(m.contains(path), "{m}");
        cfg.tls_fingerprint = "nope".into();
        assert!(matches!(
            make_verifier(&cfg, scope(&cfg)),
            Err(SipError::Tls(_))
        ));
    }

    #[tokio::test]
    async fn client_tls_ensure_installs_once() {
        use rsipstack::transport::TransportLayer;
        init_crypto();
        let cfg = AccountConfig {
            id: "t".into(),
            ..Default::default()
        };
        let tl = TransportLayer::new(tokio_util::sync::CancellationToken::new());
        let tls = ClientTls::new(&cfg, AuthState::new());
        assert!(!tl.has_tls_config());
        tls.ensure(&tl).unwrap();
        assert!(tl.has_tls_config());
        tls.ensure(&tl).unwrap();

        let bad = AccountConfig {
            tls_ca_file: "/nonexistent/ca.pem".into(),
            ..cfg
        };
        let tl = TransportLayer::new(tokio_util::sync::CancellationToken::new());
        let tls = ClientTls::new(&bad, AuthState::new());
        assert!(matches!(tls.ensure(&tl), Err(SipError::Tls(_))));
        assert!(!tl.has_tls_config(), "a failure installs nothing");
    }

    #[test]
    fn ca_file_extra_roots_are_loaded() {
        init_crypto();
        let dir = tempfile::tempdir().unwrap();
        let (cert, _) = self_signed("127.0.0.1".parse().unwrap()).unwrap();
        let path = dir.path().join("ca.pem");
        std::fs::write(&path, cert).unwrap();
        assert_eq!(read_pem_certs(path.to_str().unwrap()).unwrap().len(), 1);
        let cfg = AccountConfig {
            id: "t".into(),
            tls_ca_file: path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert!(
            make_verifier(&cfg, scope(&cfg)).is_ok(),
            "platform verifier with extra roots"
        );
        std::fs::write(&path, "not a pem").unwrap();
        assert_eq!(
            read_pem_certs(path.to_str().unwrap())
                .map(|v| v.len())
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn self_signed_cert_is_pem() {
        let (c, k) = self_signed("127.0.0.1".parse().unwrap()).unwrap();
        assert!(c.contains("BEGIN CERTIFICATE") && k.contains("PRIVATE KEY"));
    }

    #[test]
    fn sni_override() {
        init_crypto();
        let hop: SocketAddr = "192.0.2.1:5061".parse().unwrap();
        let mut cfg = AccountConfig {
            tls_verify: false,
            ..Default::default()
        };
        let sni = |cfg: &AccountConfig, d: Option<&str>, h: Option<SocketAddr>| {
            let t = client_tls_config(cfg, d.map(Into::into), h, scope(cfg)).unwrap();
            assert!(t.sni_hostname.is_none(), "no layer-wide SNI");
            t.sni_by_addr.get(&hop).cloned()
        };
        assert_eq!(
            sni(&cfg, Some("pbx.example"), Some(hop)).as_deref(),
            Some("pbx.example")
        );
        // Not for any other destination.
        let t =
            client_tls_config(&cfg, Some("pbx.example".into()), Some(hop), scope(&cfg)).unwrap();
        assert!(
            !t.sni_by_addr
                .contains_key(&"192.0.2.2:5061".parse().unwrap())
        );
        cfg.tls_server_name = "other.example".into();
        assert_eq!(
            sni(&cfg, Some("pbx.example"), Some(hop)).as_deref(),
            Some("other.example")
        );
        assert_eq!(sni(&cfg, None, Some(hop)).as_deref(), Some("other.example"));
        assert_eq!(sni(&cfg, None, None), None);
        // An IP-literal hop without a name is pinned to its IP (nothing can be learned for it).
        cfg.tls_server_name.clear();
        assert_eq!(sni(&cfg, None, Some(hop)).as_deref(), Some("192.0.2.1"));
    }

    #[test]
    fn fingerprint_verifier_pins_leaf() {
        init_crypto();
        let (pem, _) = self_signed("127.0.0.1".parse().unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.pem");
        std::fs::write(&path, pem).unwrap();
        let cert = read_pem_certs(path.to_str().unwrap()).unwrap().remove(0);
        let fp = cert_sha256(&cert);
        let hex: String = fp
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":");
        let cfg = AccountConfig {
            tls_fingerprint: hex,
            registrar: "whatever.example".into(),
            ..Default::default()
        };
        let v = make_verifier(&cfg, scope(&cfg)).unwrap();
        let name = ServerName::try_from("whatever.example").unwrap();
        let now = UnixTime::now();
        assert!(v.verify_server_cert(&cert, &[], &name, &[], now).is_ok());
        let other = FingerprintVerifier::new([0; 32]);
        assert!(
            other
                .verify_server_cert(&cert, &[], &name, &[], now)
                .is_err()
        );
    }
}
