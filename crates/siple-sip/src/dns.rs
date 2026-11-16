//! DNS: SRV via hickory (`_sip._udp`, `_sip._tcp`, `_sips._tcp`), A/AAAA via
//! `tokio::net::lookup_host` so macOS scoped/VPN DNS keeps working. No NAPTR.
//!
//! The RFC 3263-style target selection (IP literal or explicit port -> no SRV; SRV ordering by
//! priority/weight; A/AAAA fallback on the transport's default port) is rsipstack's
//! `resolve_logic`; this module supplies the lookup backend.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use rsipstack::resolver::sip_resolver::{LookupSource, SrvRecord, resolve_logic};
use rsipstack::sip::{Domain, Port, Transport as SipTransport};
use rsipstack::transport::SipAddr;
use rsipstack::transport::transport_layer::DomainResolver;
use siple_config::Transport;

use crate::error::{Result, SipError};

/// Lookup backend: hickory for SRV, the OS resolver for addresses.
pub struct SystemSrvSource {
    srv: Option<Arc<TokioResolver>>,
}

impl SystemSrvSource {
    /// Builds the hickory SRV resolver from the system config; without one SRV is skipped.
    pub fn new() -> Self {
        let srv = TokioResolver::builder_tokio()
            .and_then(|b| b.build())
            .map(Arc::new)
            .map_err(|e| tracing::warn!(error = %e, "no SRV resolver; using A/AAAA only"))
            .ok();
        Self { srv }
    }
}

impl Default for SystemSrvSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LookupSource for SystemSrvSource {
    async fn lookup_srv(&self, name: &str) -> std::result::Result<Vec<SrvRecord>, String> {
        let Some(resolver) = &self.srv else {
            return Err("no SRV resolver".into());
        };
        let lookup = resolver.srv_lookup(name).await.map_err(|e| e.to_string())?;
        Ok(lookup
            .message()
            .all_sections()
            .filter_map(|r| match &r.data {
                RData::SRV(srv) => Some(SrvRecord {
                    target: srv.target.to_string().trim_end_matches('.').to_string(),
                    port: srv.port,
                    priority: srv.priority,
                    weight: srv.weight,
                }),
                _ => None,
            })
            .collect())
    }

    async fn lookup_a_aaaa(&self, name: &str) -> std::result::Result<Vec<IpAddr>, String> {
        let addrs = tokio::net::lookup_host((name, 0))
            .await
            .map_err(|e| format!("lookup {name}: {e}"))?;
        let mut v: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
        // IPv4 first: the PBXs this tool targets are overwhelmingly v4 and a v6 AAAA that does
        // not route would otherwise stall registration.
        v.sort_by_key(|ip| ip.is_ipv6());
        v.dedup();
        Ok(v)
    }
}

/// Splits `host[:port]` / `[v6]:port` / bare IPv6.
pub fn split_host_port(s: &str) -> (String, Option<u16>) {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[')
        && let Some((h, after)) = rest.split_once(']')
    {
        return (
            h.to_string(),
            after.strip_prefix(':').and_then(|p| p.parse().ok()),
        );
    }
    if s.matches(':').count() > 1 {
        return (s.to_string(), None); // bare IPv6
    }
    match s.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().ok()),
        _ => (s.to_string(), None),
    }
}

fn sip_transport(t: Transport) -> SipTransport {
    match t {
        Transport::Tcp => SipTransport::Tcp,
        Transport::Tls => SipTransport::Tls,
        // Not a wire transport; callers resolve `auto` first ([`resolve_auto`]).
        Transport::Udp | Transport::Auto => SipTransport::Udp,
    }
}

/// Whether an SRV lookup error means "no such record" (or no SRV resolver at all), as opposed to
/// a failure to ask.
fn is_no_record(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    e.contains("no record") || e.contains("nxdomain") || e.contains("no srv resolver")
}

/// `transport = "auto"`: the first of `_sips._tcp` (TLS), `_sip._tcp` (TCP),
/// `_sip._udp` (UDP) that has an SRV record for `host` wins, with its own targets and ports;
/// otherwise UDP on port 5060. No NAPTR. An IP literal or an explicit port never queries SRV
/// and picks UDP. A transport whose SRV targets do not resolve is skipped.
pub async fn resolve_auto(
    source: &dyn LookupSource,
    host: &str,
    port: Option<u16>,
) -> Result<(Transport, Vec<SocketAddr>)> {
    if port.is_none() && host.parse::<IpAddr>().is_err() {
        for (t, name) in [
            (Transport::Tls, "_sips._tcp"),
            (Transport::Tcp, "_sip._tcp"),
            (Transport::Udp, "_sip._udp"),
        ] {
            let has_srv = match source.lookup_srv(&format!("{name}.{host}")).await {
                Ok(records) => !records.is_empty(),
                // Only a definitive "no such record" lets `auto` fall through to the next
                // transport; a timeout or SERVFAIL must not silently downgrade (to UDP).
                Err(e) if is_no_record(&e) => false,
                Err(e) => return Err(SipError::Dns(format!("SRV lookup {name}.{host}: {e}"))),
            };
            if has_srv && let Ok(addrs) = resolve_concrete(source, host, None, t).await {
                return Ok((t, addrs));
            }
        }
    }
    Ok((
        Transport::Udp,
        resolve_concrete(source, host, port, Transport::Udp).await?,
    ))
}

/// Resolves `host[:port]` for `transport` to candidate socket addresses, best first.
pub async fn resolve(
    source: &dyn LookupSource,
    host: &str,
    port: Option<u16>,
    transport: Transport,
) -> Result<Vec<SocketAddr>> {
    if transport == Transport::Auto {
        return resolve_auto(source, host, port).await.map(|(_, a)| a);
    }
    resolve_concrete(source, host, port, transport).await
}

async fn resolve_concrete(
    source: &dyn LookupSource,
    host: &str,
    port: Option<u16>,
    transport: Transport,
) -> Result<Vec<SocketAddr>> {
    let t = sip_transport(transport);
    let targets = resolve_logic(
        source,
        &Domain::from(host.to_string()),
        port.map(Port::from),
        Some(t),
        transport == Transport::Tls,
    )
    .await
    .map_err(SipError::Dns)?;
    let addrs: Vec<SocketAddr> = targets.into_iter().map(|t| t.addr).collect();
    if addrs.is_empty() {
        return Err(SipError::Dns(format!("no addresses for {host}")));
    }
    Ok(addrs)
}

/// rsipstack `DomainResolver` for destinations given as host names (e.g. dialing
/// `sip:x@other.example`). Picks the first candidate.
pub struct SipDomainResolver {
    source: Arc<SystemSrvSource>,
}

impl SipDomainResolver {
    /// Resolver sharing a lookup backend.
    pub fn new(source: Arc<SystemSrvSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl DomainResolver for SipDomainResolver {
    async fn resolve(&self, target: &SipAddr) -> rsipstack::Result<SipAddr> {
        let host = target.addr.host.to_string();
        let transport = match target.r#type {
            Some(SipTransport::Tcp) => Transport::Tcp,
            Some(SipTransport::Tls) => Transport::Tls,
            _ => Transport::Udp,
        };
        let port = target.addr.port.map(u16::from);
        let addrs = resolve(self.source.as_ref(), &host, port, transport)
            .await
            .map_err(|e| rsipstack::Error::DnsResolutionError(format!("{target}: {e}")))?;
        Ok(SipAddr::new(sip_transport(transport), addrs[0].into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Records which names were queried and answers from fixed tables.
    struct Mock {
        srv: HashMap<String, Vec<SrvRecord>>,
        a: HashMap<String, Vec<IpAddr>>,
        queried: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LookupSource for Mock {
        async fn lookup_srv(&self, name: &str) -> std::result::Result<Vec<SrvRecord>, String> {
            self.queried.lock().unwrap().push(format!("SRV {name}"));
            self.srv
                .get(name)
                .cloned()
                .ok_or_else(|| "no records found".into())
        }
        async fn lookup_a_aaaa(&self, name: &str) -> std::result::Result<Vec<IpAddr>, String> {
            self.queried.lock().unwrap().push(format!("A {name}"));
            self.a.get(name).cloned().ok_or_else(|| "nx".into())
        }
    }

    fn mock() -> Mock {
        let mut srv = HashMap::new();
        srv.insert(
            "_sip._udp.example.com".into(),
            vec![SrvRecord {
                target: "sip1.example.com".into(),
                port: 5080,
                priority: 10,
                weight: 1,
            }],
        );
        srv.insert(
            "_sips._tcp.example.com".into(),
            vec![SrvRecord {
                target: "sip1.example.com".into(),
                port: 5099,
                priority: 10,
                weight: 1,
            }],
        );
        let mut a = HashMap::new();
        a.insert(
            "sip1.example.com".into(),
            vec!["192.0.2.10".parse().unwrap()],
        );
        a.insert("example.com".into(), vec!["192.0.2.1".parse().unwrap()]);
        a.insert(
            "plain.example.com".into(),
            vec!["192.0.2.2".parse().unwrap()],
        );
        Mock {
            srv,
            a,
            queried: Default::default(),
        }
    }

    #[tokio::test]
    async fn ip_literal_and_explicit_port_skip_srv() {
        let m = mock();
        let r = resolve(&m, "192.0.2.10", None, Transport::Udp)
            .await
            .unwrap();
        assert_eq!(r, ["192.0.2.10:5060".parse::<SocketAddr>().unwrap()]);
        let r = resolve(&m, "192.0.2.10", None, Transport::Tls)
            .await
            .unwrap();
        assert_eq!(r, ["192.0.2.10:5061".parse::<SocketAddr>().unwrap()]);
        let r = resolve(&m, "example.com", Some(5555), Transport::Udp)
            .await
            .unwrap();
        assert_eq!(r, ["192.0.2.1:5555".parse::<SocketAddr>().unwrap()]);
        assert!(
            m.queried
                .lock()
                .unwrap()
                .iter()
                .all(|q| q.starts_with("A ")),
            "{:?}",
            m.queried
        );
    }

    #[tokio::test]
    async fn srv_per_transport_then_a_fallback() {
        let m = mock();
        let udp = resolve(&m, "example.com", None, Transport::Udp)
            .await
            .unwrap();
        assert_eq!(udp, ["192.0.2.10:5080".parse::<SocketAddr>().unwrap()]);
        let tls = resolve(&m, "example.com", None, Transport::Tls)
            .await
            .unwrap();
        assert_eq!(tls, ["192.0.2.10:5099".parse::<SocketAddr>().unwrap()]);
        // No _sip._tcp record: falls back to A on the default port.
        let tcp = resolve(&m, "example.com", None, Transport::Tcp)
            .await
            .unwrap();
        assert_eq!(tcp, ["192.0.2.1:5060".parse::<SocketAddr>().unwrap()]);
        let plain = resolve(&m, "plain.example.com", None, Transport::Udp)
            .await
            .unwrap();
        assert_eq!(plain, ["192.0.2.2:5060".parse::<SocketAddr>().unwrap()]);
        assert!(
            resolve(&m, "nope.invalid", None, Transport::Udp)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn auto_prefers_sips_then_tcp_then_udp_then_default_udp() {
        let mut m = mock();
        // Both _sips._tcp and _sip._udp exist for example.com: TLS wins, on its own port.
        let (t, a) = resolve_auto(&m, "example.com", None).await.unwrap();
        assert_eq!(t, Transport::Tls);
        assert_eq!(a, ["192.0.2.10:5099".parse::<SocketAddr>().unwrap()]);
        // Only _sip._udp: UDP on the SRV port.
        m.srv.remove("_sips._tcp.example.com");
        let (t, a) = resolve_auto(&m, "example.com", None).await.unwrap();
        assert_eq!(t, Transport::Udp);
        assert_eq!(a, ["192.0.2.10:5080".parse::<SocketAddr>().unwrap()]);
        // _sip._tcp beats _sip._udp.
        m.srv.insert(
            "_sip._tcp.example.com".into(),
            vec![SrvRecord {
                target: "sip1.example.com".into(),
                port: 5070,
                priority: 1,
                weight: 1,
            }],
        );
        let (t, a) = resolve_auto(&m, "example.com", None).await.unwrap();
        assert_eq!(t, Transport::Tcp);
        assert_eq!(a, ["192.0.2.10:5070".parse::<SocketAddr>().unwrap()]);
        // No SRV at all: UDP 5060 on the A record.
        let (t, a) = resolve_auto(&m, "plain.example.com", None).await.unwrap();
        assert_eq!(t, Transport::Udp);
        assert_eq!(a, ["192.0.2.2:5060".parse::<SocketAddr>().unwrap()]);
        // `resolve` with auto agrees.
        let a = resolve(&m, "plain.example.com", None, Transport::Auto)
            .await
            .unwrap();
        assert_eq!(a, ["192.0.2.2:5060".parse::<SocketAddr>().unwrap()]);
        assert!(resolve_auto(&m, "nope.invalid", None).await.is_err());
    }

    #[tokio::test]
    async fn auto_skips_srv_for_ip_literal_and_explicit_port() {
        let m = mock();
        let (t, a) = resolve_auto(&m, "192.0.2.10", None).await.unwrap();
        assert_eq!(
            (t, a),
            (Transport::Udp, vec!["192.0.2.10:5060".parse().unwrap()])
        );
        let (t, a) = resolve_auto(&m, "example.com", Some(5555)).await.unwrap();
        assert_eq!(
            (t, a),
            (Transport::Udp, vec!["192.0.2.1:5555".parse().unwrap()])
        );
        assert!(
            m.queried
                .lock()
                .unwrap()
                .iter()
                .all(|q| q.starts_with("A "))
        );
    }

    #[tokio::test]
    async fn system_a_lookup_resolves_localhost() {
        let src = SystemSrvSource::new();
        let ips = src.lookup_a_aaaa("localhost").await.unwrap();
        assert!(ips.iter().any(|ip| ip.is_loopback()));
        // IPv4 sorted first.
        assert!(!ips[0].is_ipv6() || ips.iter().all(|i| i.is_ipv6()));
    }

    #[test]
    fn host_port_split() {
        assert_eq!(
            split_host_port("pbx.example.com"),
            ("pbx.example.com".into(), None)
        );
        assert_eq!(
            split_host_port("pbx.example.com:5070"),
            ("pbx.example.com".into(), Some(5070))
        );
        assert_eq!(
            split_host_port("10.0.0.1:5061"),
            ("10.0.0.1".into(), Some(5061))
        );
        assert_eq!(
            split_host_port("[2001:db8::1]:5070"),
            ("2001:db8::1".into(), Some(5070))
        );
        assert_eq!(
            split_host_port("[2001:db8::1]"),
            ("2001:db8::1".into(), None)
        );
        assert_eq!(split_host_port("2001:db8::1"), ("2001:db8::1".into(), None));
    }
}
