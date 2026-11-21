//! Which digest challenges the account's credential may answer.
//!
//! A 401/407 is only answered when it plausibly comes from the user's own provider; otherwise a
//! rogue host (`sip:x@attacker.test`) could collect a digest response for the SIP password. The
//! decision is made per challenge, inside rsipstack's single authentication funnel
//! (`Credential::guard`), so it covers INVITE, in-dialog requests, SUBSCRIBE and REGISTER.
//!
//! Rule ([`decide`]): answer when `auth_foreign = true`, or when the **actual destination** of
//! the challenged transaction (the address the transport sent it to, `Transaction::destination`
//! after `send`) is one of the account's provider addresses: the configured `outbound_proxy`,
//! registrar and `domain` (IP literals / names), and every address the next hop and the registrar
//! resolved to (all SRV / A candidates of the running transport, accumulated so dialogs started
//! on an older edge keep working, replaced when a transport for other hosts is built). A
//! destination from a TCP / TLS connection matches the resolved addresses by IP only.
//! Nothing a peer can set (Request-URI, Contact, Record-Route) is trusted: those only steer
//! where a request goes, and the destination is what is checked. Any realm is accepted from a
//! provider address (PBXes use odd ones).

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

use rsipstack::dialog::authenticate::AuthGuard;
use rsipstack::sip::prelude::*;
use rsipstack::sip::{Header, Request, Response, StatusCode};
use rsipstack::transport::SipAddr;
use siple_config::AccountConfig;

/// Start of the error text of a refused challenge (`Authentication refused: <host> is not your
/// provider`); the call end / request error shown to users and agents.
pub(crate) const REFUSED: &str = "Authentication refused: ";

/// Refusals logged per account before the oldest Call-ID is forgotten.
const LOGGED_CAP: usize = 64;

/// Resolved provider addresses remembered per account.
const RESOLVED_CAP: usize = 64;

/// A `host[:port]` pair; hosts are lower-cased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: Option<u16>,
}

impl Endpoint {
    pub(crate) fn new(host: &str, port: Option<u16>) -> Self {
        Self {
            host: host
                .trim_matches(|c| c == '[' || c == ']')
                .to_ascii_lowercase(),
            port,
        }
    }

    /// Parses `host`, `host:port`, `[v6]:port`, optionally with a `sip:` / `sips:` scheme,
    /// userinfo and `;params`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s
            .strip_prefix("sips:")
            .or_else(|| s.strip_prefix("sip:"))
            .unwrap_or(s);
        let s = s.rsplit_once('@').map_or(s, |(_, h)| h);
        let s = s.split([';', '?']).next().unwrap_or(s);
        if s.is_empty() {
            return None;
        }
        if let Some(rest) = s.strip_prefix('[') {
            let (h, tail) = rest.split_once(']')?;
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return Some(Self::new(h, port));
        }
        match s.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => {
                Some(Self::new(h, p.parse().ok()))
            }
            _ => Some(Self::new(s, None)),
        }
    }

    pub(crate) fn from_addr(a: &SipAddr) -> Self {
        Self::new(
            &a.addr.host.to_string(),
            a.addr.port.as_ref().map(|p| u16::from(*p)),
        )
    }

    /// Same IP, any port (a provider-opened TCP / TLS connection shows the peer's ephemeral
    /// source port).
    fn same_ip(&self, hop: &Endpoint) -> bool {
        self.host == hop.host
    }

    /// Whether `hop` is this provider endpoint. With a port, the hop must use it (no port = a
    /// default one). Without: a name matches any port, an IP literal only the default SIP
    /// ports (a shared IP can host other services; the real provider port is in the resolved
    /// set anyway).
    fn matches(&self, hop: &Endpoint) -> bool {
        let default = |p: u16| p == 5060 || p == 5061;
        self.host == hop.host
            && match (self.port, hop.port) {
                (Some(p), Some(h)) => p == h,
                (Some(p), None) => default(p),
                (None, Some(h)) if self.host.parse::<IpAddr>().is_ok() => default(h),
                (None, _) => true,
            }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let v6 = self.host.contains(':');
        match (v6, self.port) {
            (true, Some(p)) => write!(f, "[{}]:{p}", self.host),
            (true, None) => write!(f, "{}", self.host),
            (false, Some(p)) => write!(f, "{}:{p}", self.host),
            (false, None) => write!(f, "{}", self.host),
        }
    }
}

/// What the account's provider is, as far as digest authentication goes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Provider {
    pub allow_foreign: bool,
    /// Configured outbound proxy, registrar and domain.
    pub configured: Vec<Endpoint>,
    /// Addresses they resolved to, oldest first. Only grows (capped) until the configured
    /// hosts change, so a dialog started on an edge that DNS no longer returns stays trusted.
    pub resolved: Vec<Endpoint>,
    /// (outbound proxy, effective registrar) of the runtime `resolved` belongs to.
    pub resolved_for: (String, String),
}

/// Outcome of [`decide`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Refuse,
}

/// Decides on the actual destination of a challenged request (`None` = unknown: refuse).
/// `reliable`: the destination came from a TCP / TLS connection, whose peer port may be the
/// provider's ephemeral source port, so resolved provider addresses match by IP alone.
pub(crate) fn decide(p: &Provider, hop: Option<&Endpoint>, reliable: bool) -> Decision {
    if p.allow_foreign {
        return Decision::Allow;
    }
    let Some(h) = hop else {
        return Decision::Refuse;
    };
    let by_addr = p.configured.iter().chain(&p.resolved).any(|e| e.matches(h));
    if by_addr || (reliable && p.resolved.iter().any(|e| e.same_ip(h))) {
        Decision::Allow
    } else {
        Decision::Refuse
    }
}

#[derive(Default)]
struct Inner {
    provider: Provider,
    /// SIP Call-IDs whose refusal was already logged.
    logged: VecDeque<String>,
}

/// Per-account authentication policy, shared by every credential clone (calls in flight see
/// config changes immediately).
#[derive(Default)]
pub(crate) struct AuthState(Mutex<Inner>);

impl AuthState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Refreshes the provider description. `cfg` is the account's current config (configured
    /// hosts, `auth_foreign`). `runtime` is the built transport's config and the addresses its
    /// probe resolved the provider to (every SRV / A candidate of the next hop and the
    /// registrar). Those belong to the runtime: they accumulate (most recently seen kept) while
    /// the runtime's proxy / registrar stay the same and are replaced when another runtime
    /// is built for different hosts, so an address the provider gave up is not trusted forever.
    pub(crate) fn update(
        &self,
        cfg: &AccountConfig,
        runtime: Option<(&AccountConfig, &[SocketAddr])>,
    ) {
        let configured: Vec<Endpoint> = [
            cfg.outbound_proxy.as_str(),
            cfg.effective_registrar(),
            cfg.domain.as_str(),
        ]
        .iter()
        .filter_map(|s| Endpoint::parse(s))
        .collect();
        let mut g = self.lock();
        let p = &mut g.provider;
        p.allow_foreign = cfg.auth_foreign;
        p.configured = configured;
        let Some((built, addrs)) = runtime else {
            return;
        };
        let key = (
            built.outbound_proxy.clone(),
            built.effective_registrar().to_string(),
        );
        if p.resolved_for != key {
            p.resolved.clear();
            p.resolved_for = key;
        }
        for a in addrs {
            let e = Endpoint::new(&a.ip().to_string(), Some(a.port()));
            if let Some(i) = p.resolved.iter().position(|x| *x == e) {
                // Most recently seen last, so the current addresses are never evicted first.
                let e = p.resolved.remove(i);
                p.resolved.push(e);
            } else {
                if p.resolved.len() >= RESOLVED_CAP {
                    p.resolved.remove(0);
                }
                p.resolved.push(e);
            }
        }
    }

    /// The guard to put into the account's `Credential`.
    pub(crate) fn guard(self: &Arc<Self>) -> AuthGuard {
        let me = self.clone();
        AuthGuard(Arc::new(move |tx, resp| {
            me.check(&tx.original, tx.destination.as_ref(), resp)
        }))
    }

    /// Whether a request sent to `destination` (the resolved next hop) may carry the account's
    /// digest response: the same rule as for answering a challenge.
    pub(crate) fn allows_hop(&self, destination: Option<&SipAddr>) -> bool {
        let (hop, reliable) = hop_of(destination);
        decide(&self.lock().provider, hop.as_ref(), reliable) == Decision::Allow
    }

    fn check(
        &self,
        req: &Request,
        destination: Option<&SipAddr>,
        resp: &Response,
    ) -> Result<(), String> {
        let (hop, reliable) = hop_of(destination);
        let mut g = self.lock();
        tracing::debug!(hop = ?hop.as_ref().map(ToString::to_string), "auth decision");
        if decide(&g.provider, hop.as_ref(), reliable) == Decision::Allow {
            return Ok(());
        }
        let host = hop.map_or_else(|| "unknown host".to_string(), |h| h.to_string());
        let call_id = req
            .call_id_header()
            .map(|c| c.value().to_string())
            .unwrap_or_default();
        if !g.logged.contains(&call_id) {
            if g.logged.len() >= LOGGED_CAP {
                g.logged.pop_front();
            }
            g.logged.push_back(call_id);
            tracing::info!(
                host = %host,
                realm = challenge_realm(resp).as_deref().unwrap_or(""),
                code = resp.status_code.code(),
                "refusing digest challenge: host is not the account's provider \
                 (set auth_foreign = true to allow)"
            );
        }
        Err(format!(
            "{REFUSED}{host} is not your provider (set auth_foreign = true for this account \
             if your provider uses other servers)"
        ))
    }
}

/// The hop a request goes to, and whether it is a connection-oriented transport.
fn hop_of(destination: Option<&SipAddr>) -> (Option<Endpoint>, bool) {
    let reliable = destination.is_some_and(|d| {
        d.r#type
            .is_some_and(|t| !matches!(t, rsipstack::sip::Transport::Udp))
    });
    (destination.map(Endpoint::from_addr), reliable)
}

/// Removes `Authorization` / `Proxy-Authorization` from an ACK that is not going to the provider.
/// rsipstack copies the INVITE's credentials into the 2xx ACK (RFC 3261 §22.1), and over UDP that
/// ACK goes to the 2xx's Record-Route or Contact, which the peer controls. The digest response is
/// only for the provider that challenged us.
pub(crate) fn strip_foreign_ack_credentials(
    auth: &AuthState,
    req: &mut Request,
    destination: Option<&SipAddr>,
) {
    if req.method != rsipstack::sip::Method::Ack
        || !req
            .headers
            .iter()
            .any(|h| matches!(h, Header::Authorization(_) | Header::ProxyAuthorization(_)))
        || auth.allows_hop(destination)
    {
        return;
    }
    req.headers
        .retain(|h| !matches!(h, Header::Authorization(_) | Header::ProxyAuthorization(_)));
}

/// The realm of the 401 (`WWW-Authenticate`) / 407 (`Proxy-Authenticate`) challenge.
fn challenge_realm(resp: &Response) -> Option<String> {
    let proxy = resp.status_code == StatusCode::ProxyAuthenticationRequired;
    let find = |want_proxy: bool| {
        resp.headers.iter().find_map(|h| match (h, want_proxy) {
            (Header::ProxyAuthenticate(p), true) => p.typed().ok().map(|t| t.realm),
            (Header::WwwAuthenticate(w), false) => w.typed().ok().map(|t| t.realm),
            _ => None,
        })
    };
    find(proxy).or_else(|| find(!proxy))
}

/// The user-facing message when `e` is a refused challenge.
pub(crate) fn refusal(e: &rsipstack::Error) -> Option<String> {
    match e {
        rsipstack::Error::DialogError(msg, ..) if msg.starts_with(REFUSED) => Some(msg.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(s: &str) -> Endpoint {
        Endpoint::parse(s).unwrap()
    }

    fn provider() -> Provider {
        Provider {
            allow_foreign: false,
            configured: vec![
                ep("sip:proxy.example:5070;lr"),
                ep("pbx.example"),
                ep("10.0.0.5"),
            ],
            resolved: vec![ep("198.51.100.7:5080")],
            resolved_for: Default::default(),
        }
    }

    fn run(p: &Provider, hop: &str) -> Decision {
        decide(p, Some(&ep(hop)), false)
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(
            ep("Pbx.Example:5080"),
            Endpoint::new("pbx.example", Some(5080))
        );
        assert_eq!(
            ep("sips:u@h.example:5061;transport=tls").to_string(),
            "h.example:5061"
        );
        assert_eq!(ep("[::1]:5060").to_string(), "[::1]:5060");
        assert_eq!(ep("::1").host, "::1");
        assert!(Endpoint::parse("").is_none());
    }

    #[test]
    fn provider_addresses_are_answered() {
        let p = provider();
        assert_eq!(run(&p, "proxy.example:5070"), Decision::Allow);
        assert_eq!(run(&p, "pbx.example:5099"), Decision::Allow);
        assert_eq!(run(&p, "198.51.100.7:5080"), Decision::Allow);
        assert_eq!(run(&p, "10.0.0.5:5060"), Decision::Allow);
        assert_eq!(run(&p, "10.0.0.5"), Decision::Allow);
    }

    #[test]
    fn foreign_destination_is_refused() {
        let p = provider();
        assert_eq!(run(&p, "attacker.test:5060"), Decision::Refuse);
        assert_eq!(run(&p, "proxy.example:5071"), Decision::Refuse);
        assert_eq!(run(&p, "198.51.100.7:5060"), Decision::Refuse);
        assert_eq!(decide(&p, None, false), Decision::Refuse);
    }

    #[test]
    fn portless_ip_literal_trusts_only_default_ports() {
        let p = provider();
        assert_eq!(run(&p, "10.0.0.5:5061"), Decision::Allow);
        assert_eq!(run(&p, "10.0.0.5:5070"), Decision::Refuse);
        assert_eq!(run(&p, "10.0.0.6:5060"), Decision::Refuse);
    }

    #[test]
    fn reliable_destinations_match_resolved_addresses_by_ip() {
        let p = provider();
        let hop = ep("198.51.100.7:49152");
        assert_eq!(decide(&p, Some(&hop), false), Decision::Refuse);
        assert_eq!(decide(&p, Some(&hop), true), Decision::Allow);
        // Only resolved provider addresses; not any port of a configured IP literal.
        assert_eq!(
            decide(&p, Some(&ep("10.0.0.5:49152")), true),
            Decision::Refuse
        );
        assert_eq!(
            decide(&p, Some(&ep("203.0.113.1:49152")), true),
            Decision::Refuse
        );
    }

    #[test]
    fn current_addresses_are_not_evicted_first() {
        let st = AuthState::new();
        let cfg = AccountConfig {
            domain: "pbx.example".into(),
            ..Default::default()
        };
        let keep: SocketAddr = "192.0.2.1:5060".parse().unwrap();
        st.update(&cfg, Some((&cfg, &[keep])));
        for i in 0..RESOLVED_CAP {
            let a: SocketAddr = format!("198.51.100.{}:{}", i % 200, 6000 + i)
                .parse()
                .unwrap();
            // Seen again on every probe, like the registrar's current address.
            st.update(&cfg, Some((&cfg, &[a, keep])));
        }
        assert_eq!(run(&st.lock().provider, "192.0.2.1:5060"), Decision::Allow);
    }

    #[test]
    fn allow_foreign_answers_everything() {
        let mut p = provider();
        p.allow_foreign = true;
        assert_eq!(run(&p, "attacker.test"), Decision::Allow);
        assert_eq!(decide(&p, None, false), Decision::Allow);
    }

    #[test]
    fn resolved_addresses_accumulate_until_the_config_changes() {
        let st = AuthState::new();
        let cfg = AccountConfig {
            domain: "pbx.example".into(),
            registrar: "edge.example".into(),
            ..Default::default()
        };
        let a: SocketAddr = "192.0.2.1:5060".parse().unwrap();
        let b: SocketAddr = "192.0.2.2:5060".parse().unwrap();
        st.update(&cfg, Some((&cfg, &[a])));
        st.update(&cfg, Some((&cfg, &[b])));
        let g = st.lock();
        assert_eq!(run(&g.provider, "192.0.2.1:5060"), Decision::Allow);
        assert_eq!(run(&g.provider, "192.0.2.2:5060"), Decision::Allow);
        drop(g);
        let moved = AccountConfig {
            registrar: "other.example".into(),
            ..cfg.clone()
        };
        // The config moved but the old runtime is still the one running: its addresses stay.
        st.update(&moved, Some((&cfg, &[b])));
        assert_eq!(run(&st.lock().provider, "192.0.2.1:5060"), Decision::Allow);
        // A runtime built for the new hosts replaces them.
        st.update(&moved, Some((&moved, &[b])));
        let g = st.lock();
        assert_eq!(run(&g.provider, "192.0.2.1:5060"), Decision::Refuse);
        assert_eq!(run(&g.provider, "192.0.2.2:5060"), Decision::Allow);
    }

    #[test]
    fn ack_credentials_only_go_to_the_provider() {
        let st = AuthState::new();
        let cfg = AccountConfig {
            domain: "pbx.example".into(),
            ..Default::default()
        };
        let provider: SocketAddr = "192.0.2.1:5060".parse().unwrap();
        st.update(&cfg, Some((&cfg, &[provider])));
        let msg = |method: &str| {
            Request::try_from(
                format!(
                    "{method} sip:bob@203.0.113.9:5060 SIP/2.0\r\n\
                     Via: SIP/2.0/UDP 192.0.2.50:5060;branch=z9hG4bKack1\r\n\
                     From: <sip:alice@pbx.example>;tag=a\r\n\
                     To: <sip:bob@pbx.example>;tag=b\r\n\
                     Call-ID: ack-creds\r\n\
                     CSeq: 2 {method}\r\n\
                     Proxy-Authorization: Digest username=\"alice\", realm=\"pbx\", \
                     nonce=\"n\", uri=\"sip:bob@pbx.example\", response=\"0123\"\r\n\
                     Content-Length: 0\r\n\r\n"
                )
                .as_str(),
            )
            .unwrap()
        };
        let has_creds = |r: &Request| {
            r.headers
                .iter()
                .any(|h| matches!(h, Header::Authorization(_) | Header::ProxyAuthorization(_)))
        };
        let udp = |a: &str| {
            SipAddr::new(
                rsipstack::sip::Transport::Udp,
                a.parse::<SocketAddr>().unwrap().into(),
            )
        };

        // To the provider: kept (RFC 3261 §22.1).
        let mut ack = msg("ACK");
        strip_foreign_ack_credentials(&st, &mut ack, Some(&udp("192.0.2.1:5060")));
        assert!(has_creds(&ack));
        // Straight to the callee's Contact: removed.
        strip_foreign_ack_credentials(&st, &mut ack, Some(&udp("203.0.113.9:5060")));
        assert!(!has_creds(&ack));
        // Other methods are left to the challenge guard.
        let mut invite = msg("INVITE");
        strip_foreign_ack_credentials(&st, &mut invite, Some(&udp("203.0.113.9:5060")));
        assert!(has_creds(&invite));
    }
}
