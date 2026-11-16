//! siple: per-connection TLS server certificate verifier tests.
use super::test_tls_reload::generate_test_cert;
use crate::sip::{Host, HostWithPort, Transport};
use crate::transport::tls::{TlsConfig, TlsConnection};
use crate::transport::SipAddr;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::{rustls::ServerConfig, TlsAcceptor};

#[derive(Debug)]
struct AcceptAll;

impl ServerCertVerifier for AcceptAll {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
    fn root_hint_subjects(&self) -> Option<&[DistinguishedName]> {
        None
    }
}

/// Spawn a one-shot TLS server with a self-signed cert; returns its address
/// and a receiver yielding the SNI name seen by the server (None if absent).
async fn spawn_server() -> (SipAddr, tokio::sync::oneshot::Receiver<Option<String>>) {
    let (cert, key) = generate_test_cert("localhost").expect("cert");
    let certs = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        if let Ok(tls) = acceptor.accept(stream).await {
            let sni = tls.get_ref().1.server_name().map(|s| s.to_string());
            let _ = tx.send(sni);
            // keep the stream alive briefly so the client side can finish
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    });
    let addr = SipAddr {
        r#type: Some(Transport::Tls),
        addr: HostWithPort {
            host: Host::IpAddr("127.0.0.1".parse().unwrap()),
            port: Some(port.into()),
        },
    };
    (addr, rx)
}

#[tokio::test]
async fn test_tls_connect_with_accept_all_verifier_succeeds() {
    let (addr, sni_rx) = spawn_server().await;
    let config = TlsConfig {
        verifier: Some(Arc::new(AcceptAll)),
        sni_hostname: Some("localhost".to_string()),
        ..Default::default()
    };
    // Verifier supplied via TlsConfig only (as TransportLayer does).
    let conn = TlsConnection::connect(&addr, Some(&config), None, None).await;
    assert!(conn.is_ok(), "connect failed: {:?}", conn.err());
    // SNI is still set from the configured/target name.
    let sni = tokio::time::timeout(std::time::Duration::from_secs(2), sni_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sni.as_deref(), Some("localhost"));
}

#[tokio::test]
async fn test_tls_connect_with_explicit_verifier_argument_succeeds() {
    let (addr, _rx) = spawn_server().await;
    let verifier: Arc<dyn ServerCertVerifier> = Arc::new(AcceptAll);
    let conn = TlsConnection::connect(&addr, None, Some(verifier), None).await;
    assert!(conn.is_ok(), "connect failed: {:?}", conn.err());
}

#[tokio::test]
async fn test_tls_connect_without_verifier_fails_on_self_signed() {
    let (addr, _rx) = spawn_server().await;
    let config = TlsConfig {
        sni_hostname: Some("localhost".to_string()),
        ..Default::default()
    };
    let conn = TlsConnection::connect(&addr, Some(&config), None, None).await;
    assert!(conn.is_err(), "self-signed cert must be rejected");
}

#[test]
fn test_tls_config_debug_shows_verifier_presence_only() {
    let with = TlsConfig {
        verifier: Some(Arc::new(AcceptAll)),
        ..Default::default()
    };
    let without = TlsConfig::default();
    assert!(format!("{:?}", with).contains("verifier: Some(\"<set>\")"));
    assert!(format!("{:?}", without).contains("verifier: None"));
    let _clone = with.clone();
}

// siple: `sni_by_addr` applies to the listed destination only.
#[tokio::test]
async fn test_sni_by_addr_applies_only_to_listed_destination() {
    use crate::transport::TransportLayer;
    use tokio_util::sync::CancellationToken;

    let (listed, listed_rx) = spawn_server().await;
    let (other, other_rx) = spawn_server().await;
    let listed_sock: std::net::SocketAddr = listed.addr.clone().try_into().unwrap();

    let tl = TransportLayer::new(CancellationToken::new());
    tl.set_tls_config(TlsConfig {
        verifier: Some(Arc::new(AcceptAll)),
        sni_by_addr: [(listed_sock, "override.example".to_string())].into(),
        ..Default::default()
    });
    tl.lookup(&listed, None).await.expect("listed connects");
    tl.lookup(&other, None).await.expect("other connects");
    let t = std::time::Duration::from_secs(2);
    let sni = tokio::time::timeout(t, listed_rx).await.unwrap().unwrap();
    assert_eq!(sni.as_deref(), Some("override.example"));
    // An IP-literal destination without an override sends no SNI (its own host).
    let sni = tokio::time::timeout(t, other_rx).await.unwrap().unwrap();
    assert_eq!(sni, None);
}
