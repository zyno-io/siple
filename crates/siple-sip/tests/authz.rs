//! Digest challenges are only answered when they come from the account's provider: scripted
//! raw-UDP servers on loopback play the provider and a foreign host.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::{Rig, add, hdr, peer, rig, state_of, until};
use siple_config::{AccountConfig, Secret};
use siple_sip::{CallInfo, CallState, DialOptions, RegState, SipEvent, UpdateOutcome};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// A scripted SIP endpoint. REGISTERs are answered by the task itself (challenge once, then
/// 200) when `auto_register`; everything else is queued for the test.
struct Srv {
    sock: Arc<UdpSocket>,
    addr: SocketAddr,
    rx: UnboundedReceiver<(String, SocketAddr)>,
    buf: Vec<(String, SocketAddr)>,
}

fn method(raw: &str) -> &str {
    raw.split(' ').next().unwrap_or("")
}

fn reply_text(req: &str, status: &str, to_tag: &str, extra: &str, body: &str) -> String {
    let to = hdr(req, "to")[0].to_string();
    let to = if to.contains("tag=") || to_tag.is_empty() {
        to
    } else {
        format!("{to};tag={to_tag}")
    };
    let ct = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/sdp\r\n"
    };
    format!(
        "SIP/2.0 {status}\r\n{}\r\n{}\r\n{to}\r\n{}\r\n{}\r\n{extra}{ct}Content-Length: {}\r\n\r\n{body}",
        hdr(req, "via").join("\r\n"),
        hdr(req, "from")[0],
        hdr(req, "call-id")[0],
        hdr(req, "cseq")[0],
        body.len()
    )
}

const CHALLENGE_401: &str =
    "WWW-Authenticate: Digest realm=\"rogue\", nonce=\"0123456789abcdef\", algorithm=MD5\r\n";
const CHALLENGE_407: &str =
    "Proxy-Authenticate: Digest realm=\"weird\", nonce=\"0123456789abcdef\", algorithm=MD5\r\n";

impl Srv {
    async fn start(realm: Option<&'static str>) -> Srv {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        let (tx, rx) = unbounded_channel();
        let s = sock.clone();
        tokio::spawn(async move {
            let mut b = vec![0u8; 16384];
            while let Ok((n, src)) = s.recv_from(&mut b).await {
                let raw = String::from_utf8_lossy(&b[..n]).into_owned();
                if raw.trim().is_empty() {
                    continue;
                }
                if let (Some(realm), "REGISTER") = (realm, method(&raw)) {
                    let resp = if hdr(&raw, "authorization").is_empty() {
                        reply_text(
                            &raw,
                            "401 Unauthorized",
                            "reg1",
                            &format!(
                                "WWW-Authenticate: Digest realm=\"{realm}\", nonce=\"0123456789abcdef\", algorithm=MD5\r\n"
                            ),
                            "",
                        )
                    } else {
                        let contact = hdr(&raw, "contact")[0].to_string();
                        reply_text(
                            &raw,
                            "200 OK",
                            "reg1",
                            &format!("{contact};expires=300\r\nExpires: 300\r\n"),
                            "",
                        )
                    };
                    let _ = s.send_to(resp.as_bytes(), src).await;
                    continue;
                }
                let _ = tx.send((raw, src));
            }
        });
        Srv {
            sock,
            addr,
            rx,
            buf: Vec::new(),
        }
    }

    async fn wait(&mut self, m: &str, secs: u64) -> Option<(String, SocketAddr)> {
        if let Some(i) = self.buf.iter().position(|(r, _)| method(r) == m) {
            return Some(self.buf.remove(i));
        }
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(end, self.rx.recv()).await {
                Ok(Some(x)) if method(&x.0) == m => return Some(x),
                Ok(Some(x)) => self.buf.push(x),
                _ => return None,
            }
        }
    }

    /// Next response whose status line contains `code`.
    async fn wait_status(&mut self, code: &str, secs: u64) -> Option<(String, SocketAddr)> {
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(end, self.rx.recv()).await {
                Ok(Some(x)) if x.0.starts_with(&format!("SIP/2.0 {code}")) => return Some(x),
                Ok(Some(x)) => self.buf.push(x),
                _ => return None,
            }
        }
    }

    async fn reply(&self, to: SocketAddr, req: &str, status: &str, extra: &str, body: &str) {
        let r = reply_text(req, status, "srv1", extra, body);
        self.sock.send_to(r.as_bytes(), to).await.unwrap();
    }

    fn sdp(&self) -> String {
        format!(
            "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=sendrecv\r\n",
            self.addr.port() + 1
        )
    }
}

fn account(id: &str, provider: &Srv) -> AccountConfig {
    AccountConfig {
        domain: "provider.test".into(),
        registrar: provider.addr.to_string(),
        password: Secret::new("secret"),
        ..peer(id)
    }
}

async fn ended(r: &mut Rig, id: &str) -> CallInfo {
    state_of(r, id, CallState::Ended).await
}

fn assert_refused(c: &CallInfo, host: &SocketAddr) {
    let end = c.end.as_ref().expect("end");
    assert_eq!(end.code, 401, "{end:?}");
    assert!(
        end.reason.starts_with("Authentication refused: ")
            && end.reason.contains(&host.to_string())
            && end
                .reason
                .contains(" is not your provider (set auth_foreign = true"),
        "{end:?}"
    );
    assert!(
        !end.reason.contains("0123456789abcdef"),
        "nonce leaked: {end:?}"
    );
    assert!(!end.reason.contains("secret"));
}

/// Dial to a host that is not the provider and challenges: no Authorization, clear error.
#[tokio::test]
async fn foreign_direct_dial_is_not_answered() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    add(&r, account("a", &prov)).await;
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    assert!(hdr(&inv, "authorization").is_empty());
    foreign
        .reply(src, &inv, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    let c = ended(&mut r, &id).await;
    assert_refused(&c, &foreign.addr);
    assert!(
        foreign.wait("INVITE", 1).await.is_none(),
        "a second INVITE (with credentials) was sent"
    );
}

/// `auth_foreign = true` (applied hot) restores answering any challenge.
#[tokio::test]
async fn auth_foreign_true_answers_after_hot_reload() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let mut cfg = account("a", &prov);
    add(&r, cfg.clone()).await;
    cfg.auth_foreign = true;
    assert_eq!(
        r.stack.update_account(cfg).await.unwrap(),
        UpdateOutcome::Hot
    );
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    foreign
        .reply(src, &inv, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    let (inv2, src2) = foreign.wait("INVITE", 5).await.expect("retried INVITE");
    assert!(hdr(&inv2, "authorization")[0].contains("Digest"), "{inv2}");
    foreign.reply(src2, &inv2, "486 Busy Here", "", "").await;
    let c = ended(&mut r, &id).await;
    assert_eq!(c.end.unwrap().code, 486);
}

/// Calls routed via the registrar are answered, whatever the target and the realm.
#[tokio::test]
async fn provider_routed_calls_are_answered() {
    let mut prov = Srv::start(Some("regrealm")).await;
    let mut r = rig();
    let mut cfg = account("a", &prov);
    cfg.register = true;
    add(&r, cfg).await;
    // Wait for the registration (realm "regrealm" is then known).
    until(&mut r, "registered", |e| {
        matches!(
            e,
            SipEvent::Registration {
                state: RegState::Registered { .. },
                ..
            }
        )
    })
    .await;

    for (target, via) in [
        ("sip:x@foreign.test", true),
        ("sip:100@provider.test", false),
    ] {
        let id = r
            .stack
            .dial(
                "a",
                target,
                DialOptions {
                    via_registrar: via,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (inv, src) = prov.wait("INVITE", 5).await.expect("INVITE");
        assert!(hdr(&inv, "proxy-authorization").is_empty());
        // Realm differs from the REGISTER one: the hop is the provider, so it is answered.
        prov.reply(
            src,
            &inv,
            "407 Proxy Authentication Required",
            CHALLENGE_407,
            "",
        )
        .await;
        let (inv2, src2) = prov.wait("INVITE", 5).await.expect("retried INVITE");
        assert!(
            hdr(&inv2, "proxy-authorization")[0].contains("Digest"),
            "{inv2}"
        );
        prov.reply(src2, &inv2, "486 Busy Here", "", "").await;
        assert_eq!(ended(&mut r, &id).await.end.unwrap().code, 486);
    }
}

/// With an outbound proxy every call goes through it, so a challenge from it is answered.
#[tokio::test]
async fn outbound_proxy_hop_is_answered() {
    let mut proxy = Srv::start(None).await;
    let other = Srv::start(None).await;
    let mut r = rig();
    let mut cfg = account("a", &other);
    cfg.outbound_proxy = proxy.addr.to_string();
    add(&r, cfg).await;
    let id = r
        .stack
        .dial("a", "sip:x@foreign.test", DialOptions::default())
        .await
        .unwrap();
    let (inv, src) = proxy.wait("INVITE", 5).await.expect("INVITE");
    proxy
        .reply(
            src,
            &inv,
            "407 Proxy Authentication Required",
            CHALLENGE_407,
            "",
        )
        .await;
    let (inv2, src2) = proxy.wait("INVITE", 5).await.expect("retried INVITE");
    assert!(hdr(&inv2, "proxy-authorization")[0].contains("Digest"));
    proxy.reply(src2, &inv2, "486 Busy Here", "", "").await;
    assert_eq!(ended(&mut r, &id).await.end.unwrap().code, 486);
}

/// Established direct call to a foreign peer; the peer challenges its BYE / re-INVITE.
async fn foreign_call(r: &mut Rig, foreign: &mut Srv, prov: &Srv) -> (String, SocketAddr) {
    add(r, account("a", prov)).await;
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    let contact = format!("Contact: <sip:x@{}>\r\n", foreign.addr);
    foreign
        .reply(src, &inv, "200 OK", &contact, &foreign.sdp())
        .await;
    state_of(r, &id, CallState::Active).await;
    (id, src)
}

#[tokio::test]
async fn in_dialog_bye_challenged_by_foreign_peer_is_not_answered() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let (id, src) = foreign_call(&mut r, &mut foreign, &prov).await;
    let _ = foreign.wait("ACK", 5).await.expect("ACK");
    let _ = r.stack.hangup(&id).await;
    let (bye, _) = foreign.wait("BYE", 5).await.expect("BYE");
    assert!(hdr(&bye, "authorization").is_empty());
    foreign
        .reply(src, &bye, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    assert!(
        foreign.wait("BYE", 1).await.is_none(),
        "BYE was re-sent with credentials"
    );
}

#[tokio::test]
async fn in_dialog_reinvite_challenged_by_foreign_peer_is_not_answered() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let (id, src) = foreign_call(&mut r, &mut foreign, &prov).await;
    let _ = foreign.wait("ACK", 5).await.expect("ACK");
    let hold = {
        let stack = r.stack.clone();
        let id = id.clone();
        tokio::spawn(async move { stack.hold(&id).await })
    };
    let (re, _) = foreign.wait("INVITE", 5).await.expect("re-INVITE");
    assert!(hdr(&re, "authorization").is_empty());
    foreign
        .reply(src, &re, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    assert!(
        foreign.wait("INVITE", 1).await.is_none(),
        "re-INVITE was re-sent with credentials"
    );
    let res = hold.await.unwrap();
    let err = res.expect_err("hold must fail").to_string();
    assert!(err.contains("Authentication refused"), "{err}");
}

/// REPRO: a directly dialled foreign peer Record-Routes itself and sets its Contact host to the
/// account's domain; the BYE (sent to the foreign peer) is challenged.
async fn spoof_call(r: &mut Rig, foreign: &mut Srv, cfg: AccountConfig) -> (String, SocketAddr) {
    add(r, cfg).await;
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    let extra = format!(
        "Contact: <sip:x@provider.test>\r\nRecord-Route: <sip:{};lr>\r\n",
        foreign.addr
    );
    foreign
        .reply(src, &inv, "200 OK", &extra, &foreign.sdp())
        .await;
    state_of(r, &id, CallState::Active).await;
    let _ = foreign.wait("ACK", 5).await.expect("ACK");
    (id, src)
}

#[tokio::test]
async fn record_route_contact_spoof_is_not_answered() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let (id, src) = spoof_call(&mut r, &mut foreign, account("a", &prov)).await;
    let _ = r.stack.hangup(&id).await;
    let (bye, _) = foreign.wait("BYE", 5).await.expect("BYE");
    eprintln!("BYE1:\n{bye}");
    foreign
        .reply(src, &bye, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    let got = foreign.wait("BYE", 3).await;
    if let Some((b2, _)) = &got {
        eprintln!("BYE2:\n{b2}");
    }
    assert!(
        got.is_none(),
        "BYPASS: BYE re-sent with credentials to the foreign host"
    );
}

#[tokio::test]
async fn record_route_contact_spoof_with_registered_realm_is_not_answered() {
    let prov = Srv::start(Some("regrealm")).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let mut cfg = account("a", &prov);
    cfg.register = true;
    add(&r, cfg.clone()).await;
    until(&mut r, "registered", |e| {
        matches!(
            e,
            SipEvent::Registration {
                state: RegState::Registered { .. },
                ..
            }
        )
    })
    .await;
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    let extra = format!(
        "Contact: <sip:x@provider.test>\r\nRecord-Route: <sip:{};lr>\r\n",
        foreign.addr
    );
    foreign
        .reply(src, &inv, "200 OK", &extra, &foreign.sdp())
        .await;
    state_of(&mut r, &id, CallState::Active).await;
    let _ = foreign.wait("ACK", 5).await.expect("ACK");
    let _ = r.stack.hangup(&id).await;
    let (bye, _) = foreign.wait("BYE", 5).await.expect("BYE");
    foreign
        .reply(
            src,
            &bye,
            "401 Unauthorized",
            "WWW-Authenticate: Digest realm=\"regrealm\", nonce=\"feedface\", algorithm=MD5\r\n",
            "",
        )
        .await;
    let got = foreign.wait("BYE", 3).await;
    if let Some((b2, _)) = &got {
        eprintln!("BYE2:\n{b2}");
    }
    assert!(
        got.is_none(),
        "BYPASS: BYE re-sent with credentials to the foreign host"
    );
}

/// An inbound call from an attacker that Record-Routes itself and names the provider in its
/// Contact: our BYE goes to the attacker, so its challenge is not answered.
#[tokio::test]
async fn inbound_call_from_attacker_is_not_answered() {
    let prov = Srv::start(None).await;
    let mut attacker = Srv::start(None).await;
    let mut r = rig();
    let a = add(&r, account("a", &prov)).await;
    let invite = format!(
        "INVITE sip:a@{a} SIP/2.0\r\nVia: SIP/2.0/UDP {att};branch=z9hG4bKatk1\r\nMax-Forwards: 70\r\n\
         From: <sip:x@attacker.test>;tag=atag\r\nTo: <sip:a@{a}>\r\nCall-ID: atk-call@attacker\r\n\
         CSeq: 1 INVITE\r\nContact: <sip:x@provider.test>\r\nRecord-Route: <sip:{att};lr>\r\n\
         Content-Type: application/sdp\r\nContent-Length: {len}\r\n\r\n{sdp}",
        att = attacker.addr,
        len = attacker.sdp().len(),
        sdp = attacker.sdp()
    );
    let dest: SocketAddr = a.parse().unwrap();
    attacker
        .sock
        .send_to(invite.as_bytes(), dest)
        .await
        .unwrap();
    let call = common::incoming(&mut r).await;
    r.stack
        .answer(&call.id, siple_sip::AnswerOptions::default())
        .await
        .unwrap();
    let (ok, _) = attacker.wait_status("200", 5).await.expect("200 OK");
    let to = hdr(&ok, "to")[0].to_string();
    let ack = format!(
        "ACK sip:a@{a} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bKatk2\r\nMax-Forwards: 70\r\n\
         From: <sip:x@attacker.test>;tag=atag\r\n{to}\r\nCall-ID: atk-call@attacker\r\nCSeq: 1 ACK\r\n\
         Content-Length: 0\r\n\r\n",
        attacker.addr
    );
    attacker.sock.send_to(ack.as_bytes(), dest).await.unwrap();
    state_of(&mut r, &call.id, CallState::Active).await;
    let _ = r.stack.hangup(&call.id).await;
    let (bye, src) = attacker.wait("BYE", 5).await.expect("BYE");
    assert!(hdr(&bye, "authorization").is_empty());
    attacker
        .reply(src, &bye, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    assert!(
        attacker.wait("BYE", 1).await.is_none(),
        "BYE re-sent with credentials"
    );
}

/// A refused session refresh is retried until the session expires, not abandoned.
#[tokio::test]
async fn refused_session_refresh_keeps_retrying() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let mut cfg = account("a", &prov);
    cfg.session_expires = 6;
    cfg.min_se = 1;
    add(&r, cfg).await;
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    let extra = format!(
        "Contact: <sip:x@{}>\r\nSupported: timer\r\nSession-Expires: 6;refresher=uac\r\n\
         Allow: INVITE, ACK, BYE, UPDATE\r\n",
        foreign.addr
    );
    foreign
        .reply(src, &inv, "200 OK", &extra, &foreign.sdp())
        .await;
    state_of(&mut r, &id, CallState::Active).await;
    for n in 1..=2 {
        let (upd, _) = foreign
            .wait("UPDATE", 6)
            .await
            .unwrap_or_else(|| panic!("session refresh attempt {n}"));
        assert!(hdr(&upd, "authorization").is_empty());
        foreign
            .reply(src, &upd, "401 Unauthorized", CHALLENGE_401, "")
            .await;
    }
}

/// A BLF refresh to a foreign target whose Contact names the provider is not answered.
#[tokio::test]
async fn blf_refresh_to_foreign_target_is_not_answered() {
    let prov = Srv::start(None).await;
    let mut foreign = Srv::start(None).await;
    let r = rig();
    let mut cfg = account("a", &prov);
    cfg.blf_expires = 2;
    cfg.mwi = siple_config::MwiMode::Off;
    cfg.blf = vec![siple_config::BlfConfig {
        uri: format!("sip:1018@{}", foreign.addr),
        ..Default::default()
    }];
    add(&r, cfg).await;
    let (sub, src) = foreign.wait("SUBSCRIBE", 5).await.expect("SUBSCRIBE");
    let extra = "Contact: <sip:x@provider.test>\r\nExpires: 2\r\n";
    foreign.reply(src, &sub, "200 OK", extra, "").await;
    // The refresh goes to the foreign host (R-URI = the spoofed Contact).
    let (re, _) = foreign.wait("SUBSCRIBE", 5).await.expect("refresh");
    assert!(hdr(&re, "authorization").is_empty());
    foreign
        .reply(src, &re, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    assert!(
        foreign.wait("SUBSCRIBE", 1).await.is_none(),
        "SUBSCRIBE re-sent with credentials"
    );
}

/// `domain` is a port-less IP literal: other ports on that IP are not the provider.
#[tokio::test]
async fn portless_ip_domain_does_not_trust_other_ports() {
    let mut foreign = Srv::start(None).await;
    let mut r = rig();
    let mut cfg = peer("a");
    cfg.domain = "127.0.0.1".into();
    cfg.password = Secret::new("secret");
    add(&r, cfg).await;
    let id = r
        .stack
        .dial(
            "a",
            &format!("sip:x@{}", foreign.addr),
            DialOptions::default(),
        )
        .await
        .unwrap();
    let (inv, src) = foreign.wait("INVITE", 5).await.expect("INVITE");
    foreign
        .reply(src, &inv, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    let c = ended(&mut r, &id).await;
    assert_refused(&c, &foreign.addr);
    assert!(foreign.wait("INVITE", 1).await.is_none());
}

/// After the registrar changes (hot config update, rebuild), the old provider's address is not
/// trusted any more.
#[tokio::test]
async fn old_provider_address_is_refused_after_a_provider_change() {
    let old = Srv::start(None).await;
    let mut new = Srv::start(None).await;
    let mut r = rig();
    let mut cfg = account("a", &old);
    add(&r, cfg.clone()).await;
    // Sanity: the old registrar's address answers challenges.
    let id = r
        .stack
        .dial("a", "sip:x@provider.test", DialOptions::default())
        .await
        .unwrap();
    let mut old = old;
    let (inv, src) = old.wait("INVITE", 5).await.expect("INVITE");
    old.reply(
        src,
        &inv,
        "407 Proxy Authentication Required",
        CHALLENGE_407,
        "",
    )
    .await;
    let (inv2, src2) = old.wait("INVITE", 5).await.expect("retry");
    old.reply(src2, &inv2, "486 Busy Here", "", "").await;
    assert_eq!(ended(&mut r, &id).await.end.unwrap().code, 486);

    cfg.registrar = new.addr.to_string();
    r.stack.update_account(cfg).await.unwrap();
    // The old host is now just another host.
    let id = r
        .stack
        .dial("a", &format!("sip:x@{}", old.addr), DialOptions::default())
        .await
        .unwrap();
    let (inv, src) = old.wait("INVITE", 5).await.expect("INVITE to the old host");
    old.reply(src, &inv, "401 Unauthorized", CHALLENGE_401, "")
        .await;
    assert_refused(&ended(&mut r, &id).await, &old.addr);
    assert!(
        old.wait("INVITE", 1).await.is_none(),
        "answered the old provider"
    );
    let _ = new.wait("INVITE", 0).await;
}
