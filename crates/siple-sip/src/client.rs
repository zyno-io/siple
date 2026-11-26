//! Out-of-dialog client requests (REGISTER, OPTIONS) over raw transactions, following the
//! recommended pattern: look up (and for TCP/TLS dial) the connection first so the Via
//! and Contact carry the real local address, then run the transaction with one digest round.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rsipstack::dialog::authenticate::{Credential, handle_client_authenticate};
use rsipstack::sip::prelude::*;
use rsipstack::sip::{
    self, Header, Host, HostWithPort, Method, Param, Request, Response, Scheme, SipMessage,
    StatusCode, StatusCodeKind, Transport, Uri,
};
use rsipstack::transaction::endpoint::EndpointInner;
use rsipstack::transaction::key::{TransactionKey, TransactionRole};
use rsipstack::transaction::{Transaction, make_call_id, make_tag};
use rsipstack::transport::{SipAddr, SipConnection};

use crate::error::{Result, SipError};
use crate::reg_logic;
use crate::types::{ACCEPT, ALLOW, OptionsResponse};

/// Bound on dialing a TCP/TLS connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `sip:` Contact with `;transport=` for non-UDP; `sips:` only when asked for on TLS.
pub fn build_contact(user: &str, hp: HostWithPort, transport: Transport, use_sips: bool) -> Uri {
    let sips = use_sips && transport == Transport::Tls;
    let mut params = vec![];
    if transport != Transport::Udp && !sips {
        params.push(Param::Transport(transport));
    }
    Uri {
        scheme: Some(if sips { Scheme::Sips } else { Scheme::Sip }),
        auth: Some(sip::Auth {
            user: user.to_string(),
            password: None,
        }),
        host_with_port: hp,
        params,
        headers: vec![],
    }
}

/// Host and port of a [`HostWithPort`] as a socket address when the host is an IP literal.
pub fn hp_to_socket(hp: &HostWithPort) -> Option<SocketAddr> {
    match (&hp.host, hp.port) {
        (Host::IpAddr(ip), Some(p)) => Some(SocketAddr::new(*ip, u16::from(p))),
        _ => None,
    }
}

/// Which Contact to put in a REGISTER.
#[derive(Debug, Clone)]
pub enum ContactSel {
    /// Derived from the connection (plus `public_address`).
    Auto,
    /// Host/port learned from `received` / `rport`.
    Learned(SocketAddr),
    /// Exactly this URI.
    Exact(Uri),
    /// The Contact of the binding this identity may hold at the registrar: the last one a
    /// non-zero REGISTER was *sent* with (recorded before sending, so an in-flight first
    /// REGISTER counts). Resolved once the request lock is held, i.e. after any in-flight
    /// REGISTER finished. Yields [`RegResult::NothingBound`] when there is none. Used to
    /// unregister.
    Bound,
}

/// Result of one REGISTER exchange.
#[derive(Debug)]
pub enum RegResult {
    /// 2xx.
    Ok(Box<RegOk>),
    /// `ContactSel::Bound` with no binding to remove; nothing was sent.
    NothingBound,
    /// Final non-2xx response.
    Rejected {
        /// Status code.
        code: u16,
        /// Reason phrase.
        reason: String,
        /// `Retry-After` seconds.
        retry_after: Option<u32>,
    },
}

/// Successful REGISTER.
#[derive(Debug)]
pub struct RegOk {
    /// Granted lifetime (requested lifetime for an unregister).
    pub granted: u32,
    /// Lifetime finally requested (after any 423 bump).
    pub requested: u32,
    /// Contact that was sent.
    pub contact: Uri,
    /// Host/port of the Contact we sent (None when it is a host name).
    pub advertised: Option<SocketAddr>,
    /// `received` from the response Via.
    pub received: Option<IpAddr>,
    /// `rport` from the response Via.
    pub rport: Option<u16>,
    /// Address advertised in Via (connection address with `public_address` applied).
    pub local: SipAddr,
    /// Local address of the connection used (flow identity for TCP/TLS).
    pub flow: SipAddr,
}

/// Stable identity of the REGISTER dialog-less "session": fresh per process run.
#[derive(Clone)]
pub struct RegIdentity {
    /// Call-ID.
    pub call_id: String,
    /// From tag.
    pub from_tag: String,
    /// CSeq counter.
    pub cseq: Arc<AtomicU32>,
    /// Serializes REGISTER exchanges of this identity: they share the CSeq counter and the
    /// registrar must see them in order (a refresh racing an unregister could otherwise
    /// resurrect the binding).
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Contact of the binding the registrar may hold (see [`ContactSel::Bound`]).
    bound: Arc<Mutex<Option<Uri>>>,
}

impl RegIdentity {
    /// New random identity.
    pub fn new() -> Self {
        Self {
            call_id: make_call_id(Some(crate::account::CALLID_SUFFIX), Default::default())
                .value()
                .to_string(),
            from_tag: make_tag().to_string(),
            cseq: Arc::new(AtomicU32::new(0)),
            lock: Arc::default(),
            bound: Arc::default(),
        }
    }

    /// Contact of the binding the registrar may hold for this identity.
    pub fn bound(&self) -> Option<Uri> {
        self.bound
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn set_bound(&self, c: Option<Uri>) {
        *self.bound.lock().unwrap_or_else(PoisonError::into_inner) = c;
    }

    /// Forgets the binding (after teardown, or when giving up on removing it).
    pub fn clear_bound(&self) {
        self.set_bound(None);
    }
}

impl Default for RegIdentity {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything a request needs, snapshotted from the account config + runtime.
#[derive(Clone)]
pub struct Client {
    /// Note: never clone the `TransportLayer` out of this; dropping any clone cancels the whole
    /// layer (rsipstack `Drop`), so short-lived holders go through the endpoint instead.
    pub endpoint: Arc<EndpointInner>,
    /// Registrar / proxy next hop (resolved); `None` for peer-mode accounts without one.
    pub target: Option<SipAddr>,
    pub transport: Transport,
    pub use_sips: bool,
    pub public_host: Option<Host>,
    pub cred: Credential,
    pub user: String,
    pub display_name: String,
    pub aor: Uri,
    /// Request-URI of REGISTER / keep-alive OPTIONS (`None` without a registrar).
    pub registrar_uri: Option<Uri>,
    pub extra_headers: Vec<(String, String)>,
    /// `Supported` header value (`AccountConfig::supported_tokens`).
    pub supported: String,
    pub route: Option<String>,
    pub identity: RegIdentity,
}

fn text_of(h: &Header) -> String {
    h.to_string()
        .split_once(':')
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

impl Client {
    /// Connection toward `dest` (dialing TCP/TLS when needed) and the address to advertise.
    pub async fn flow(&self, dest: &SipAddr) -> Result<(SipConnection, SipAddr)> {
        let (conn, _) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.endpoint.transport_layer.lookup(dest, None),
        )
        .await
        .map_err(|_| SipError::Timeout)??;
        let local = conn.get_addr().clone();
        let mut adv = local.clone();
        if let Some(h) = &self.public_host {
            adv.addr.host = h.clone();
        }
        Ok((conn, adv))
    }

    fn identity_headers(&self) -> (sip::typed::From, sip::typed::To) {
        let from = sip::typed::From {
            display_name: (!self.display_name.is_empty()).then(|| self.display_name.clone()),
            uri: self.aor.clone(),
            params: vec![],
        }
        .with_tag(self.identity.from_tag.clone().into());
        let to = sip::typed::To {
            display_name: None,
            uri: self.aor.clone(),
            params: vec![],
        };
        (from, to)
    }

    pub(crate) fn decorate(&self, req: &mut Request) {
        req.headers.unique_push(Header::Allow(ALLOW.into()));
        req.headers
            .unique_push(Header::Supported(self.supported.clone().into()));
        for (k, v) in &self.extra_headers {
            req.headers.push(Header::Other(k.clone(), v.clone()));
        }
        if let Some(r) = &self.route {
            req.headers.push(Header::Route(format!("<{r};lr>").into()));
        }
    }

    /// Runs a client transaction toward `dest`, answering one 401/407 digest challenge.
    pub async fn send_with_auth(&self, req: Request, dest: &SipAddr) -> Result<Response> {
        self.send_with_cseq(req, dest, &self.identity.cseq).await
    }

    /// Like [`Client::send_with_auth`] with the CSeq counter of another dialog-like exchange
    /// (subscriptions): the digest retry takes the next number from `cseq`.
    pub async fn send_with_cseq(
        &self,
        req: Request,
        dest: &SipAddr,
        cseq: &AtomicU32,
    ) -> Result<Response> {
        let key = TransactionKey::from_request(&req, TransactionRole::Client)?;
        let mut tx = Transaction::new_client(key, req, self.endpoint.clone(), None);
        tx.destination = Some(dest.clone());
        tx.send().await?;
        let mut authed = false;
        while let Some(msg) = tx.receive().await {
            let SipMessage::Response(resp) = msg else {
                continue;
            };
            match resp.status_code {
                StatusCode::Trying => continue,
                StatusCode::ProxyAuthenticationRequired | StatusCode::Unauthorized if !authed => {
                    let n = cseq.fetch_add(1, Ordering::SeqCst) + 1;
                    tx = handle_client_authenticate(n, &tx, resp, &self.cred).await?;
                    tx.destination = Some(dest.clone());
                    tx.send().await?;
                    authed = true;
                }
                ref c if matches!(c.kind(), StatusCodeKind::Provisional) => continue,
                _ => return Ok(resp),
            }
        }
        Err(SipError::Timeout)
    }

    /// Takes the REGISTER lock if it is free (used to start a refresh synchronously, so that a
    /// later unregister queues behind it deterministically).
    pub fn try_lock_register(&self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.identity.lock.clone().try_lock_owned().ok()
    }

    /// One REGISTER exchange (handling 401/407 and up to two 423 retries), serialized with every
    /// other REGISTER of this identity.
    pub async fn register(&self, expires: u32, sel: ContactSel) -> Result<RegResult> {
        let _guard = self.identity.lock.clone().lock_owned().await;
        self.register_locked(expires, sel).await
    }

    /// Like [`Client::register`] with the lock already held (from [`Client::try_lock_register`]).
    pub async fn register_held(
        &self,
        _guard: tokio::sync::OwnedMutexGuard<()>,
        expires: u32,
        sel: ContactSel,
    ) -> Result<RegResult> {
        self.register_locked(expires, sel).await
    }

    async fn register_locked(&self, expires: u32, sel: ContactSel) -> Result<RegResult> {
        let (target, registrar_uri) = match (&self.target, &self.registrar_uri) {
            (Some(t), Some(u)) => (t, u),
            _ => return Err(SipError::Config("no registrar or domain configured".into())),
        };
        let mut expires = expires;
        let mut retries_423 = 0;
        loop {
            let (conn, adv) = self.flow(target).await?;
            let contact = match &sel {
                ContactSel::Exact(u) => u.clone(),
                ContactSel::Bound => match self.identity.bound() {
                    Some(u) => u,
                    None => return Ok(RegResult::NothingBound),
                },
                ContactSel::Learned(sa) => {
                    build_contact(&self.user, (*sa).into(), self.transport, self.use_sips)
                }
                ContactSel::Auto => {
                    build_contact(&self.user, adv.addr.clone(), self.transport, self.use_sips)
                }
            };
            if expires > 0 {
                // Publish before sending: if this exchange is overtaken by a teardown or an
                // unregister, the binding it may create can still be removed.
                self.identity.set_bound(Some(contact.clone()));
            }
            let via = self.endpoint.get_via(Some(adv.clone()), None)?;
            let (from, to) = self.identity_headers();
            let seq = self.identity.cseq.fetch_add(1, Ordering::SeqCst) + 1;
            let mut req = self.endpoint.make_request(
                Method::Register,
                registrar_uri.clone(),
                via,
                from,
                to,
                seq,
                Some(self.identity.call_id.clone().into()),
            );
            req.headers.unique_push(
                sip::typed::Contact {
                    display_name: None,
                    uri: contact.clone(),
                    params: vec![],
                }
                .into(),
            );
            req.headers
                .unique_push(sip::headers::Expires::from(expires).into());
            self.decorate(&mut req);
            let resp = self.send_with_auth(req, target).await?;

            let code = resp.status_code.code();
            if (200..300).contains(&code) {
                if expires == 0
                    && self
                        .identity
                        .bound()
                        .is_some_and(|b| b.to_string() == contact.to_string())
                {
                    self.identity.clear_bound();
                }
                let via_raw = resp.via_header().map(|v| v.to_string()).unwrap_or_default();
                let (received, rport) = reg_logic::via_received_rport(&via_raw);
                let contacts: Vec<String> = resp
                    .headers
                    .iter()
                    .filter(|h| matches!(h, Header::Contact(_)))
                    .map(text_of)
                    .collect();
                let expires_header = resp
                    .headers
                    .iter()
                    .find_map(|h| {
                        matches!(h, Header::Expires(_)).then(|| text_of(h).parse::<u32>().ok())
                    })
                    .flatten();
                let granted = reg_logic::granted_expiry(
                    &contacts,
                    &contact.to_string(),
                    expires_header,
                    expires,
                );
                return Ok(RegResult::Ok(Box::new(RegOk {
                    granted,
                    requested: expires,
                    advertised: hp_to_socket(&contact.host_with_port),
                    flow: conn.get_addr().clone(),
                    contact,
                    received,
                    rport,
                    local: adv,
                })));
            }
            let header_u32 = |f: fn(&Header) -> bool| {
                resp.headers
                    .iter()
                    .find(|h| f(h))
                    .and_then(|h| reg_logic::parse_retry_after(&text_of(h)))
            };
            if code == 423 && retries_423 < 2 {
                let min = header_u32(|h| matches!(h, Header::MinExpires(_)));
                if let Some(e) = reg_logic::expires_after_423(expires, min) {
                    tracing::info!(
                        min_expires = e,
                        "REGISTER got 423; retrying with Min-Expires"
                    );
                    expires = e;
                    retries_423 += 1;
                    continue;
                }
            }
            return Ok(RegResult::Rejected {
                code,
                reason: resp.status_code.text().to_string(),
                retry_after: header_u32(|h| matches!(h, Header::RetryAfter(_))),
            });
        }
    }

    /// OPTIONS to `target` via `dest` (used by `send_options` and OPTIONS keep-alives).
    pub async fn options(&self, target: &Uri, dest: &SipAddr) -> Result<OptionsResponse> {
        let (_conn, adv) = self.flow(dest).await?;
        let via = self.endpoint.get_via(Some(adv.clone()), None)?;
        let (from, _) = self.identity_headers();
        // Fresh dialog identifiers for every OPTIONS.
        let from = from.with_tag(make_tag());
        let to = sip::typed::To {
            display_name: None,
            uri: target.clone(),
            params: vec![],
        };
        let seq = AtomicU32::new(1);
        let mut req =
            self.endpoint
                .make_request(Method::Options, target.clone(), via, from, to, 1, None);
        // The connection's transport: a UDP account's OPTIONS over TLS advertises `;transport=tls`.
        let transport = adv.r#type.or(dest.r#type).unwrap_or(self.transport);
        let contact = build_contact(&self.user, adv.addr.clone(), transport, self.use_sips);
        req.headers.unique_push(
            sip::typed::Contact {
                display_name: None,
                uri: contact,
                params: vec![],
            }
            .into(),
        );
        req.headers.unique_push(Header::Accept(ACCEPT.into()));
        self.decorate(&mut req);
        // A private CSeq counter: this request is its own dialog-less exchange.
        let this = Client {
            identity: RegIdentity {
                cseq: Arc::new(seq),
                ..self.identity.clone()
            },
            ..self.clone()
        };
        let resp = this.send_with_auth(req, dest).await?;
        Ok(OptionsResponse {
            code: resp.status_code.code(),
            reason: resp.status_code.text().to_string(),
            headers: resp
                .headers
                .iter()
                .filter_map(|h| {
                    h.to_string()
                        .split_once(':')
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hp(s: &str) -> HostWithPort {
        s.parse::<SocketAddr>().unwrap().into()
    }

    #[test]
    fn contact_forms() {
        assert_eq!(
            build_contact("1003", hp("10.0.0.5:5060"), Transport::Udp, false).to_string(),
            "sip:1003@10.0.0.5:5060"
        );
        let tcp = build_contact("1003", hp("10.0.0.5:54892"), Transport::Tcp, false).to_string();
        assert!(
            tcp.starts_with("sip:1003@10.0.0.5:54892;transport="),
            "{tcp}"
        );
        assert!(tcp.to_ascii_lowercase().ends_with("transport=tcp"));
        let tls = build_contact("1003", hp("10.0.0.5:54893"), Transport::Tls, false).to_string();
        assert!(
            tls.starts_with("sip:") && tls.to_ascii_lowercase().ends_with("transport=tls"),
            "{tls}"
        );
        assert_eq!(
            build_contact("1003", hp("10.0.0.5:5061"), Transport::Tls, true).to_string(),
            "sips:1003@10.0.0.5:5061"
        );
        // use_sips applies to TLS only.
        assert!(
            build_contact("1003", hp("10.0.0.5:5060"), Transport::Tcp, true)
                .to_string()
                .starts_with("sip:")
        );
    }

    #[test]
    fn hp_socket_conversion() {
        assert_eq!(
            hp_to_socket(&hp("10.0.0.5:5060")),
            Some("10.0.0.5:5060".parse().unwrap())
        );
        let domain = HostWithPort {
            host: Host::Domain("pub.example".into()),
            port: Some(5060.into()),
        };
        assert_eq!(hp_to_socket(&domain), None);
    }
}
