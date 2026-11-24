//! Subscription client (BLF / MWI / generic) against a scripted raw-UDP notifier on loopback.
//! No PBX needed.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::{Rig, add, peer, rig, state_of, until};
use siple_config::{AccountConfig, BlfConfig, MwiMode};
use siple_sip::{BlfState, SipEvent, SubKind, SubState};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

// --- the fake notifier ----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Msg {
    raw: String,
    src: SocketAddr,
}

impl Msg {
    fn start_line(&self) -> &str {
        self.raw.lines().next().unwrap_or("")
    }
    fn method(&self) -> &str {
        self.start_line().split(' ').next().unwrap_or("")
    }
    fn is_request(&self) -> bool {
        !self.start_line().starts_with("SIP/2.0")
    }
    fn code(&self) -> u16 {
        self.start_line()
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }
    fn headers(&self, name: &str) -> Vec<String> {
        let head = self.raw.split("\r\n\r\n").next().unwrap_or("");
        head.lines()
            .skip(1)
            .filter_map(|l| {
                let (n, v) = l.split_once(':')?;
                n.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| v.trim().to_string())
            })
            .collect()
    }
    fn header(&self, name: &str) -> String {
        self.headers(name).into_iter().next().unwrap_or_default()
    }
    fn call_id(&self) -> String {
        self.header("call-id")
    }
    fn cseq(&self) -> u32 {
        self.header("cseq")
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    }
    fn expires(&self) -> Option<u32> {
        self.header("expires").parse().ok()
    }
    fn event(&self) -> String {
        self.header("event")
    }
    fn tag_of(h: &str) -> Option<String> {
        h.split(';')
            .find_map(|p| p.trim().strip_prefix("tag=").map(str::to_string))
    }
    fn to_tag(&self) -> Option<String> {
        Self::tag_of(&self.header("to"))
    }
    fn contact_uri(&self) -> String {
        let c = self.header("contact");
        match (c.find('<'), c.find('>')) {
            (Some(a), Some(b)) => c[a + 1..b].to_string(),
            _ => c,
        }
    }
}

struct Fake {
    sock: Arc<UdpSocket>,
    addr: SocketAddr,
    rx: UnboundedReceiver<Msg>,
    buf: Vec<Msg>,
    /// (Call-ID, CSeq) of SUBSCRIBEs already handed out; UDP retransmissions of them (sent when a
    /// response is slow under load) are not new SUBSCRIBEs.
    seen: Vec<(String, String)>,
    branch: AtomicU32,
}

impl Fake {
    async fn start() -> Fake {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        let (tx, rx) = unbounded_channel();
        let s = sock.clone();
        tokio::spawn(async move {
            let mut b = vec![0u8; 16384];
            while let Ok((n, src)) = s.recv_from(&mut b).await {
                let raw = String::from_utf8_lossy(&b[..n]).into_owned();
                if raw.trim().is_empty() {
                    continue; // keep-alives
                }
                let _ = tx.send(Msg { raw, src });
            }
        });
        Fake {
            sock,
            addr,
            rx,
            buf: Vec::new(),
            seen: Vec::new(),
            branch: AtomicU32::new(1),
        }
    }

    /// Waits for a message satisfying `pred` (others stay buffered for later waits).
    async fn wait(&mut self, what: &str, secs: u64, pred: impl Fn(&Msg) -> bool) -> Option<Msg> {
        if let Some(i) = self.buf.iter().position(&pred) {
            return Some(self.buf.remove(i));
        }
        let end = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            match tokio::time::timeout_at(end, self.rx.recv()).await {
                Ok(Some(m)) => {
                    if pred(&m) {
                        return Some(m);
                    }
                    self.buf.push(m);
                }
                _ => {
                    eprintln!("(fake) gave up waiting for {what}");
                    return None;
                }
            }
        }
    }

    async fn subscribe(&mut self) -> Msg {
        let m = self
            .wait("SUBSCRIBE", 10, |m| {
                m.is_request() && m.method() == "SUBSCRIBE"
            })
            .await
            .expect("no SUBSCRIBE");
        self.seen.push((m.call_id(), m.header("cseq")));
        m
    }

    async fn subscribe_for(&mut self, event: &str) -> Msg {
        let ev = event.to_string();
        let m = self
            .wait("SUBSCRIBE", 10, move |m| {
                m.is_request() && m.method() == "SUBSCRIBE" && m.event().starts_with(&ev)
            })
            .await
            .unwrap_or_else(|| panic!("no SUBSCRIBE for {event}"));
        self.seen.push((m.call_id(), m.header("cseq")));
        m
    }

    async fn no_subscribe(&mut self, secs: u64) -> bool {
        let seen = self.seen.clone();
        self.wait("no SUBSCRIBE", secs, |m| {
            m.is_request()
                && m.method() == "SUBSCRIBE"
                && !seen.contains(&(m.call_id(), m.header("cseq")))
        })
        .await
        .is_none()
    }

    async fn send(&self, to: SocketAddr, raw: String) {
        self.sock.send_to(raw.as_bytes(), to).await.unwrap();
    }

    /// Response to `req`; `to_tag` is added to To when it has none.
    async fn respond(&self, req: &Msg, status: &str, to_tag: Option<&str>, extra: &[(&str, &str)]) {
        let vias = req.headers("via").join("\r\nVia: ");
        let mut to = req.header("to");
        if let (None, Some(t)) = (Msg::tag_of(&to), to_tag) {
            to = format!("{to};tag={t}");
        }
        let mut r = format!(
            "SIP/2.0 {status}\r\nVia: {vias}\r\nFrom: {}\r\nTo: {to}\r\nCall-ID: {}\r\nCSeq: {}\r\n",
            req.header("from"),
            req.call_id(),
            req.header("cseq"),
        );
        for (k, v) in extra {
            r.push_str(&format!("{k}: {v}\r\n"));
        }
        r.push_str("Content-Length: 0\r\n\r\n");
        self.send(req.src, r).await;
    }

    fn dialog(&self, sub: &Msg, our_tag: &str) -> Dialog {
        Dialog {
            call_id: sub.call_id(),
            ntf_from: format!("{};tag={our_tag}", strip_tag(&sub.header("to"))),
            ntf_to: sub.header("from"),
            target: sub.contact_uri(),
            src: sub.src,
            cseq: 0,
        }
    }

    async fn notify(&self, d: &mut Dialog, event: &str, state: &str, ct: &str, body: &str) -> u32 {
        d.cseq += 1;
        let b = self.branch.fetch_add(1, Ordering::SeqCst);
        let mut r = format!(
            "NOTIFY {} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bKfake{b}\r\nMax-Forwards: 70\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: {} NOTIFY\r\nContact: <sip:notifier@{}>\r\nEvent: {event}\r\nSubscription-State: {state}\r\n",
            d.target, self.addr, d.ntf_from, d.ntf_to, d.call_id, d.cseq, self.addr
        );
        if !body.is_empty() {
            r.push_str(&format!("Content-Type: {ct}\r\n"));
        }
        r.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        self.send(d.src, r).await;
        d.cseq
    }

    /// The stack's response to our NOTIFY number `cseq`.
    async fn notify_reply(&mut self, cseq: u32) -> Msg {
        self.wait("NOTIFY response", 5, move |m| {
            !m.is_request() && m.header("cseq") == format!("{cseq} NOTIFY")
        })
        .await
        .expect("no response to NOTIFY")
    }

    /// NOTIFY and wait for its response code.
    async fn notify_ok(
        &mut self,
        d: &mut Dialog,
        event: &str,
        state: &str,
        ct: &str,
        body: &str,
    ) -> u16 {
        let c = self.notify(d, event, state, ct, body).await;
        self.notify_reply(c).await.code()
    }
}

fn strip_tag(h: &str) -> String {
    h.split(';')
        .filter(|p| !p.trim().starts_with("tag="))
        .collect::<Vec<_>>()
        .join(";")
}

struct Dialog {
    call_id: String,
    ntf_from: String,
    ntf_to: String,
    target: String,
    src: SocketAddr,
    cseq: u32,
}

// --- helpers ------------------------------------------------------------------------------------

const DI: &str = "application/dialog-info+xml";
const MS: &str = "application/simple-message-summary";

fn dialog_info(version: u32, state: &str, dialogs: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<dialog-info xmlns=\"urn:ietf:params:xml:ns:dialog-info\" version=\"{version}\" state=\"{state}\" entity=\"sip:1018@x\">{dialogs}</dialog-info>\r\n"
    )
}

fn dlg(id: &str, dir: &str, st: &str, rendering: Option<&str>) -> String {
    let r = rendering
        .map(|v| format!("<param pname=\"+sip.rendering\" pvalue=\"{v}\"/>"))
        .unwrap_or_default();
    format!(
        "<dialog id=\"{id}\" call-id=\"{id}\" local-tag=\"l\" remote-tag=\"r\" direction=\"{dir}\"><state>{st}</state><local><identity>sip:1018@x</identity><target uri=\"sip:1018@x\">{r}</target></local><remote><identity display=\"Caller\">sip:1019@x</identity><target uri=\"sip:1019@x\"/></remote></dialog>"
    )
}

fn cfg(id: &str, fake: &Fake) -> AccountConfig {
    AccountConfig {
        domain: fake.addr.to_string(),
        mwi: MwiMode::Off,
        blf_expires: 30,
        ..peer(id)
    }
}

fn blf(uri: &str, label: &str) -> BlfConfig {
    BlfConfig {
        uri: uri.into(),
        label: label.into(),
        ..Default::default()
    }
}

async fn blf_state(r: &mut Rig, uri: &str, st: BlfState) -> SipEvent {
    let uri = uri.to_string();
    until(
        r,
        &format!("BLF {uri} {st:?}"),
        move |e| matches!(e, SipEvent::BlfState { uri: u, state, .. } if *u == uri && *state == st),
    )
    .await
}

async fn no_event(r: &mut Rig, ms: u64, pred: impl Fn(&SipEvent) -> bool) {
    assert!(!r.buf.iter().any(&pred), "unexpected buffered event");
    let end = tokio::time::Instant::now() + Duration::from_millis(ms);
    while let Ok(Some(e)) = tokio::time::timeout_at(end, r.events.recv()).await {
        assert!(!pred(&e), "unexpected event {e:?}");
        r.buf.push(e);
    }
}

fn sub_state(r: &Rig, id: &str) -> SubState {
    r.stack
        .subscriptions(None)
        .into_iter()
        .find(|s| s.id == id)
        .map(|s| s.state)
        .unwrap_or_else(|| panic!("no subscription {id}"))
}

// --- tests --------------------------------------------------------------------------------------

/// 407 digest round, NOTIFY before the 202, partial / full updates, stale versions, the
/// in-dialog refresh at 85 % and the unsubscribe on removal.
#[tokio::test]
async fn blf_dialog_auth_early_notify_updates_refresh() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "Bob")];
    c.password = siple_config::Secret::new("secret");
    add(&r, c).await;

    // First SUBSCRIBE: challenged.
    let s1 = f.subscribe().await;
    assert_eq!(s1.event(), "dialog");
    assert!(s1.header("accept").contains(DI), "{}", s1.raw);
    assert_eq!(s1.expires(), Some(30));
    assert!(s1.to_tag().is_none());
    assert!(s1.start_line().contains("sip:1018@"), "{}", s1.start_line());
    assert!(s1.header("contact").contains("sip:a@"), "{}", s1.raw);
    f.respond(
        &s1,
        "407 Proxy Authentication Required",
        Some("c1"),
        &[(
            "Proxy-Authenticate",
            "Digest realm=\"fake\", nonce=\"0123456789abcdef\", algorithm=MD5",
        )],
    )
    .await;
    // Retry with credentials, same dialog identifiers, next CSeq.
    let s2 = f.subscribe().await;
    assert_eq!(s2.call_id(), s1.call_id());
    assert_eq!(s2.cseq(), s1.cseq() + 1);
    assert!(
        s2.header("proxy-authorization").starts_with("Digest"),
        "{}",
        s2.raw
    );
    assert!(s2.header("proxy-authorization").contains("username=\"a\""));

    // NOTIFY beats the 202: it establishes the subscription.
    let mut d = f.dialog(&s2, "ntag1");
    let c1 = f
        .notify(
            &mut d,
            "dialog",
            "active;expires=3",
            DI,
            &dialog_info(0, "full", ""),
        )
        .await;
    assert_eq!(f.notify_reply(c1).await.code(), 200);
    blf_state(&mut r, "1018", BlfState::Idle).await;
    f.respond(
        &s2,
        "202 Accepted",
        Some("ntag1"),
        &[
            ("Expires", "30"),
            ("Contact", &format!("<sip:notifier@{}>", f.addr)),
        ],
    )
    .await;
    let st = r.stack.blf_status(Some("a"));
    assert_eq!(st.len(), 1);
    assert_eq!(
        (st[0].uri.as_str(), st[0].label.as_str(), st[0].state),
        ("1018", "Bob", BlfState::Idle)
    );
    let subs = r.stack.subscriptions(Some("a"));
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].kind, SubKind::Blf);
    assert_eq!(subs[0].event, "dialog");
    assert_eq!(subs[0].state, SubState::Active);
    assert!(subs[0].expires_at.is_some() && subs[0].last_notify_at.is_some());
    let sid = subs[0].id.clone();

    // Partial update: ringing; stale version ignored; full: held -> idle.
    let code = f
        .notify_ok(
            &mut d,
            "dialog",
            "active;expires=3",
            DI,
            &dialog_info(1, "partial", &dlg("d1", "recipient", "early", None)),
        )
        .await;
    assert_eq!(code, 200);
    let ev = blf_state(&mut r, "1018", BlfState::Ringing).await;
    if let SipEvent::BlfState { dialogs, label, .. } = ev {
        assert_eq!(label, "Bob");
        assert_eq!(dialogs.len(), 1);
        assert_eq!(dialogs[0].remote_identity.as_deref(), Some("sip:1019@x"));
        assert_eq!(dialogs[0].remote_display.as_deref(), Some("Caller"));
    }
    // Stale (version 0 < 1): acknowledged, ignored.
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=3",
        DI,
        &dialog_info(0, "full", ""),
    )
    .await;
    no_event(&mut r, 200, |e| matches!(e, SipEvent::BlfState { .. })).await;
    assert_eq!(r.stack.blf_status(None)[0].state, BlfState::Ringing);
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=3",
        DI,
        &dialog_info(2, "full", &dlg("d1", "recipient", "confirmed", None)),
    )
    .await;
    blf_state(&mut r, "1018", BlfState::Busy).await;
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=3",
        DI,
        &dialog_info(3, "full", &dlg("d1", "recipient", "confirmed", Some("no"))),
    )
    .await;
    blf_state(&mut r, "1018", BlfState::Held).await;
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=3",
        DI,
        &dialog_info(
            4,
            "partial",
            "<dialog id=\"d1\"><state>terminated</state></dialog>",
        ),
    )
    .await;
    blf_state(&mut r, "1018", BlfState::Idle).await;

    // Refresh at 85 % of the 3 s from Subscription-State (~2.55 s after the last NOTIFY).
    let t0 = tokio::time::Instant::now();
    let rf = f.subscribe().await;
    let waited = t0.elapsed();
    assert!(
        waited > Duration::from_millis(1500) && waited < Duration::from_secs(9),
        "{waited:?}"
    );
    assert_eq!(rf.call_id(), s1.call_id(), "refresh stays in the dialog");
    assert_eq!(rf.to_tag().as_deref(), Some("ntag1"));
    assert!(rf.cseq() > s2.cseq());
    assert_eq!(rf.expires(), Some(30));
    f.respond(&rf, "202 Accepted", None, &[("Expires", "30")])
        .await;
    let mut d2 = Dialog { cseq: d.cseq, ..d };
    f.notify_ok(
        &mut d2,
        "dialog",
        "active;expires=30",
        DI,
        &dialog_info(5, "full", ""),
    )
    .await;

    // Removing the BLF entry unsubscribes (Expires: 0, in dialog).
    let mut c = cfg("a", &f);
    c.password = siple_config::Secret::new("secret");
    let out = r.stack.update_account(c).await.unwrap();
    assert_eq!(out, siple_sip::UpdateOutcome::Hot);
    let un = f.subscribe().await;
    assert_eq!(un.expires(), Some(0));
    assert_eq!(un.call_id(), s1.call_id());
    f.respond(&un, "202 Accepted", None, &[("Expires", "0")])
        .await;
    // The final NOTIFY after our unsubscribe is accepted, a later one is not.
    let mut d3 = Dialog {
        cseq: d2.cseq,
        ..d2
    };
    assert_eq!(
        f.notify_ok(
            &mut d3,
            "dialog",
            "terminated;reason=timeout",
            DI,
            &dialog_info(6, "full", "")
        )
        .await,
        200
    );
    assert!(r.stack.blf_status(None).is_empty());
    assert!(r.stack.subscriptions(None).iter().all(|s| s.id != sid));
    r.stack.shutdown().await;
}

/// `terminated;reason=deactivated` resubscribes with a new dialog; `rejected` stops.
#[tokio::test]
async fn termination_reasons_resubscribe_or_stop() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "Bob")];
    add(&r, c).await;

    let s1 = f.subscribe().await;
    f.respond(
        &s1,
        "202 Accepted",
        Some("t1"),
        &[
            ("Expires", "30"),
            ("Contact", &format!("<sip:n@{}>", f.addr)),
        ],
    )
    .await;
    let mut d = f.dialog(&s1, "t1");
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=30",
        DI,
        &dialog_info(0, "full", &dlg("x", "initiator", "confirmed", None)),
    )
    .await;
    blf_state(&mut r, "1018", BlfState::Busy).await;

    // deactivated -> resubscribe soon, with a new Call-ID; the old dialog now answers 481.
    let sid = r.stack.subscriptions(None)[0].id.clone();
    assert_eq!(
        f.notify_ok(&mut d, "dialog", "terminated;reason=deactivated", DI, "")
            .await,
        200
    );
    assert_eq!(sub_state(&r, &sid), SubState::Terminated);
    let t0 = tokio::time::Instant::now();
    let s2 = f.subscribe().await;
    assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
    assert_ne!(s2.call_id(), s1.call_id());
    assert!(s2.to_tag().is_none(), "new dialog");
    assert_eq!(
        f.notify_ok(
            &mut d,
            "dialog",
            "active;expires=30",
            DI,
            &dialog_info(1, "full", "")
        )
        .await,
        481
    );
    f.respond(
        &s2,
        "202 Accepted",
        Some("t2"),
        &[
            ("Expires", "30"),
            ("Contact", &format!("<sip:n@{}>", f.addr)),
        ],
    )
    .await;
    let mut d2 = f.dialog(&s2, "t2");
    // Version numbering restarts with the new dialog; BLF state follows the fresh full state.
    f.notify_ok(
        &mut d2,
        "dialog",
        "active;expires=30",
        DI,
        &dialog_info(0, "full", ""),
    )
    .await;
    blf_state(&mut r, "1018", BlfState::Idle).await;
    assert_eq!(sub_state(&r, &sid), SubState::Active);

    // A To tag that is not ours -> 481.
    let mut bogus = f.dialog(&s2, "t2");
    bogus.ntf_to = bogus.ntf_to.replace("tag=", "tag=wrong");
    assert_eq!(
        f.notify_ok(&mut bogus, "dialog", "active", DI, "").await,
        481
    );

    // rejected -> stop for good.
    assert_eq!(
        f.notify_ok(&mut d2, "dialog", "terminated;reason=rejected", DI, "")
            .await,
        200
    );
    assert!(
        f.no_subscribe(2).await,
        "must not resubscribe after rejected"
    );
    match sub_state(&r, &sid) {
        SubState::Failed { reason, .. } => assert!(reason.contains("rejected"), "{reason}"),
        s => panic!("expected failed, got {s:?}"),
    }
    blf_state(&mut r, "1018", BlfState::Failed).await;
    assert_eq!(r.stack.blf_status(None)[0].state, BlfState::Failed);
    r.stack.shutdown().await;
}

/// A refresh answered with 481 starts a new subscription; 489 fails it.
#[tokio::test]
async fn refresh_481_resubscribes_and_489_fails() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let s1 = f.subscribe().await;
    f.respond(&s1, "202 Accepted", Some("t1"), &[("Expires", "3")])
        .await;
    // 2xx Expires drives the refresh when no NOTIFY gave a lifetime: 85 % of 3 s.
    let rf = f.subscribe().await;
    assert_eq!(rf.call_id(), s1.call_id());
    assert_eq!(rf.to_tag().as_deref(), Some("t1"));
    f.respond(&rf, "481 Call/Transaction Does Not Exist", None, &[])
        .await;
    let s2 = f.subscribe().await;
    assert_ne!(s2.call_id(), s1.call_id());
    assert!(s2.to_tag().is_none());
    f.respond(&s2, "489 Bad Event", Some("t9"), &[]).await;
    let sid = r.stack.subscriptions(None)[0].id.clone();
    until(&mut r, "failed", |e| {
        matches!(
            e,
            SipEvent::SubscriptionState {
                state: SubState::Failed { code: 489, .. },
                ..
            }
        )
    })
    .await;
    assert!(matches!(
        sub_state(&r, &sid),
        SubState::Failed { code: 489, .. }
    ));
    assert!(f.no_subscribe(2).await, "489 is permanent");
    r.stack.shutdown().await;
}

/// `subscribed_at` survives an in-dialog refresh, `last_refreshed_at` moves; a 481 to a refresh
/// clears both, and the new dialog's 2xx starts a new subscription.
#[tokio::test]
async fn blf_subscription_times_follow_the_dialog() {
    use siple_sip::BlfSubscription;
    async fn wait_sub(
        r: &Rig,
        what: &str,
        pred: impl Fn(&BlfSubscription) -> bool,
    ) -> BlfSubscription {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let sub = r.stack.blf_status(None)[0].subscription.clone();
            if pred(&sub) {
                return sub;
            }
            assert!(
                std::time::Instant::now() < until,
                "timed out waiting for {what}: {sub:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let s1 = f.subscribe().await;
    assert_eq!(r.stack.blf_status(None)[0].subscription.subscribed_at, None);
    f.respond(&s1, "202 Accepted", Some("t1"), &[("Expires", "3")])
        .await;
    let first = wait_sub(&r, "first 2xx", |s| s.subscribed_at.is_some()).await;
    assert_eq!(first.last_refreshed_at, first.subscribed_at);
    // In-dialog refresh: subscribed_at kept, last_refreshed_at moves.
    let rf = f.subscribe().await;
    assert_eq!(rf.call_id(), s1.call_id());
    f.respond(&rf, "202 Accepted", Some("t1"), &[("Expires", "3")])
        .await;
    let refreshed = wait_sub(&r, "refresh 2xx", |s| {
        s.last_refreshed_at > first.last_refreshed_at
    })
    .await;
    assert_eq!(refreshed.subscribed_at, first.subscribed_at);
    // 481 to the next refresh: a new dialog, the times reset until its 2xx.
    let rf2 = f.subscribe().await;
    assert_eq!(rf2.call_id(), s1.call_id());
    f.respond(&rf2, "481 Call/Transaction Does Not Exist", None, &[])
        .await;
    let s2 = f.subscribe().await;
    assert_ne!(s2.call_id(), s1.call_id());
    let cleared = r.stack.blf_status(None)[0].subscription.clone();
    assert_eq!(
        (cleared.subscribed_at, cleared.last_refreshed_at),
        (None, None)
    );
    f.respond(&s2, "202 Accepted", Some("t2"), &[("Expires", "60")])
        .await;
    let renewed = wait_sub(&r, "new dialog 2xx", |s| s.subscribed_at.is_some()).await;
    assert!(renewed.subscribed_at > first.subscribed_at);
    r.stack.shutdown().await;
}

/// 5xx is retried with back-off.
#[tokio::test]
async fn server_error_retries() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let s1 = f.subscribe().await;
    f.respond(&s1, "503 Service Unavailable", None, &[]).await;
    let s2 = f.subscribe().await;
    assert_ne!(s2.call_id(), s1.call_id(), "no dialog was established");
    f.respond(&s2, "202 Accepted", Some("t"), &[("Expires", "30")])
        .await;
    let sid = r.stack.subscriptions(None)[0].id.clone();
    until(&mut r, "pending again", |e| {
        matches!(
            e,
            SipEvent::SubscriptionState {
                state: SubState::Pending,
                ..
            }
        )
    })
    .await;
    let _ = sid;
    r.stack.shutdown().await;
}

/// MWI by subscription (mwi_uri), plus unsolicited NOTIFYs.
#[tokio::test]
async fn mwi_subscribe_and_unsolicited() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.mwi = MwiMode::Subscribe;
    c.mwi_uri = "*97".into();
    add(&r, c).await;
    let s1 = f.subscribe_for("message-summary").await;
    assert!(s1.start_line().contains("sip:*97@"), "{}", s1.start_line());
    assert!(s1.header("accept").contains(MS));
    f.respond(&s1, "202 Accepted", Some("m1"), &[("Expires", "3600")])
        .await;
    let mut d = f.dialog(&s1, "m1");
    assert!(r.stack.mwi_status("a").is_none());
    f.notify_ok(
        &mut d,
        "message-summary",
        "active;expires=3600",
        MS,
        "Messages-Waiting: no\r\nMessage-Account: sip:a@x\r\nVoice-Message: 0/0 (0/0)\r\n",
    )
    .await;
    let ev = until(&mut r, "mwi no", |e| matches!(e, SipEvent::Mwi { .. })).await;
    let SipEvent::Mwi { status, account } = ev else {
        unreachable!()
    };
    assert_eq!(account, "a");
    assert!(!status.waiting);
    f.notify_ok(&mut d, "message-summary", "active;expires=3600", MS, "Messages-Waiting: yes\r\nMessage-Account: sip:a@x\r\nVoice-Message: 2/5 (1/0)\r\nFax-Message: 1/0\r\n").await;
    let ev = until(
        &mut r,
        "mwi yes",
        |e| matches!(e, SipEvent::Mwi { status, .. } if status.waiting),
    )
    .await;
    let SipEvent::Mwi { status, .. } = ev else {
        unreachable!()
    };
    assert_eq!(
        (status.voice.new, status.voice.old, status.voice.urgent_new),
        (2, 5, 1)
    );
    assert_eq!(status.other["fax"].new, 1);
    let st = r.stack.account_status("a").unwrap();
    let m = st.mwi.unwrap();
    assert_eq!((m.waiting, m.new, m.old), (true, 2, 5));
    // Identical resend is not news.
    f.notify_ok(&mut d, "message-summary", "active;expires=3600", MS, "Messages-Waiting: yes\r\nMessage-Account: sip:a@x\r\nVoice-Message: 2/5 (1/0)\r\nFax-Message: 1/0\r\n").await;
    no_event(&mut r, 200, |e| matches!(e, SipEvent::Mwi { .. })).await;

    // Unsolicited out-of-dialog NOTIFY (own Call-ID, no To tag), as FreeSWITCH sends after
    // REGISTER, feeds the same status.
    let local: SocketAddr = r
        .stack
        .account_status("a")
        .unwrap()
        .local_addr
        .unwrap()
        .parse()
        .unwrap();
    let un = format!(
        "NOTIFY sip:a@{local} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bKunsol1\r\nMax-Forwards: 70\r\nFrom: <sip:pbx@x>;tag=px\r\nTo: <sip:a@x>\r\nCall-ID: unsolicited-1\r\nCSeq: 1 NOTIFY\r\nEvent: message-summary\r\nSubscription-State: terminated;reason=noresource\r\nContent-Type: {MS}\r\nContent-Length: 62\r\n\r\nMessages-Waiting: no\r\nMessage-Account: sip:a@x\r\nVoice-Message: 0/6\r\n",
        f.addr
    );
    let body = "Messages-Waiting: no\r\nMessage-Account: sip:a@x\r\nVoice-Message: 0/6\r\n";
    let un = un.replace(
        "Content-Length: 62",
        &format!("Content-Length: {}", body.len()),
    );
    f.send(local, un).await;
    let ok = f
        .wait("200", 5, |m| {
            !m.is_request() && m.header("call-id") == "unsolicited-1"
        })
        .await
        .unwrap();
    assert_eq!(ok.code(), 200);
    let ev = until(
        &mut r,
        "mwi cleared",
        |e| matches!(e, SipEvent::Mwi { status, .. } if !status.waiting),
    )
    .await;
    let SipEvent::Mwi { status, .. } = ev else {
        unreachable!()
    };
    assert_eq!((status.voice.new, status.voice.old), (0, 6));
    r.stack.shutdown().await;
}

/// `mwi = "unsolicited"` makes no SUBSCRIBE but accepts NOTIFYs; `"off"` ignores them.
#[tokio::test]
async fn mwi_modes_unsolicited_and_off() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("u", &f);
    c.mwi = MwiMode::Unsolicited;
    add(&r, c).await;
    let mut c = cfg("o", &f);
    c.mwi = MwiMode::Off;
    add(&r, c).await;
    assert!(
        f.no_subscribe(1).await,
        "no MWI SUBSCRIBE without mode=subscribe"
    );
    for acct in ["u", "o"] {
        let local: SocketAddr = r
            .stack
            .account_status(acct)
            .unwrap()
            .local_addr
            .unwrap()
            .parse()
            .unwrap();
        let body = "Messages-Waiting: yes\r\nVoice-Message: 3/0\r\n";
        let n = format!(
            "NOTIFY sip:{acct}@{local} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bKun{acct}\r\nFrom: <sip:pbx@x>;tag=px\r\nTo: <sip:{acct}@x>\r\nCall-ID: un-{acct}\r\nCSeq: 1 NOTIFY\r\nEvent: message-summary\r\nContent-Type: {MS}\r\nContent-Length: {}\r\n\r\n{body}",
            f.addr,
            body.len()
        );
        f.send(local, n).await;
        let ok = f
            .wait("200", 5, |m| {
                !m.is_request() && m.header("call-id") == format!("un-{acct}")
            })
            .await
            .unwrap();
        assert_eq!(ok.code(), 200);
    }
    until(&mut r, "mwi on u", |e| matches!(e, SipEvent::Mwi { account, status } if account == "u" && status.voice.new == 3)).await;
    assert!(r.stack.mwi_status("o").is_none());
    assert_eq!(r.stack.mwi_status("u").unwrap().voice.new, 3);
    // Both still report the unsolicited NOTIFY as an event.
    let _ = until(
        &mut r,
        "unhandled",
        |e| matches!(e, SipEvent::UnhandledNotify { account, .. } if account == "o"),
    )
    .await;
    r.stack.shutdown().await;
}

/// Generic subscription: raw bodies as events, state events, unsubscribe.
#[tokio::test]
async fn generic_subscription_delivers_raw_notifies() {
    let mut f = Fake::start().await;
    let mut r = rig();
    add(&r, cfg("a", &f)).await;
    let sid = r
        .stack
        .subscribe("a", "1018", "presence", None, Some(60))
        .unwrap();
    assert!(
        r.stack
            .subscribe("a", "x", "bad event", None, None)
            .is_err()
    );
    assert!(
        r.stack
            .subscribe("nope", "x", "presence", None, None)
            .is_err()
    );
    let s1 = f.subscribe_for("presence").await;
    assert_eq!(s1.expires(), Some(60));
    assert!(s1.header("accept").contains("pidf"), "{}", s1.raw);
    f.respond(&s1, "202 Accepted", Some("g1"), &[("Expires", "60")])
        .await;
    let mut d = f.dialog(&s1, "g1");
    let body = "<presence entity=\"sip:1018@x\"><tuple id=\"t\"><status><basic>open</basic></status></tuple></presence>";
    f.notify_ok(
        &mut d,
        "presence",
        "active;expires=60",
        "application/pidf+xml",
        body,
    )
    .await;
    let ev = until(&mut r, "notify", |e| {
        matches!(e, SipEvent::SubscriptionNotify { .. })
    })
    .await;
    let SipEvent::SubscriptionNotify {
        account,
        sub_id,
        event,
        content_type,
        body: b,
        subscription_state,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(
        (account.as_str(), sub_id.as_str(), event.as_str()),
        ("a", sid.as_str(), "presence")
    );
    assert_eq!(content_type, "application/pidf+xml");
    assert_eq!(b, body);
    assert_eq!(subscription_state, "active;expires=60");
    until(&mut r, "active", |e| matches!(e, SipEvent::SubscriptionState { sub_id: i, state: SubState::Active, kind: SubKind::Generic, .. } if *i == sid)).await;
    // Wrong Event package for the dialog -> 481.
    assert_eq!(f.notify_ok(&mut d, "dialog", "active", DI, "").await, 481);

    // Config-managed subscriptions cannot be unsubscribed through the API; unknown ids error.
    assert!(r.stack.unsubscribe("s999999").await.is_err());
    let r2 = &r.stack;
    let un = tokio::join!(r2.unsubscribe(&sid), async {
        let u = f.subscribe_for("presence").await;
        assert_eq!(u.expires(), Some(0));
        assert_eq!(u.call_id(), s1.call_id());
        f.respond(&u, "202 Accepted", None, &[("Expires", "0")])
            .await;
    });
    un.0.unwrap();
    assert!(r.stack.subscriptions(Some("a")).is_empty());
    until(&mut r, "terminated", |e| matches!(e, SipEvent::SubscriptionState { sub_id: i, state: SubState::Terminated, .. } if *i == sid)).await;
    r.stack.shutdown().await;
}

/// BLF list diff: add / remove / relabel without touching the others.
#[tokio::test]
async fn blf_reconfig_diff_by_uri() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "Bob"), blf("1019", "Carol")];
    add(&r, c.clone()).await;
    let mut dialogs = std::collections::HashMap::new();
    for _ in 0..2 {
        let s = f.subscribe().await;
        let who = s
            .start_line()
            .split(':')
            .nth(1)
            .unwrap()
            .split('@')
            .next()
            .unwrap()
            .to_string();
        f.respond(
            &s,
            "202 Accepted",
            Some(&format!("t{who}")),
            &[("Expires", "30")],
        )
        .await;
        dialogs.insert(who.clone(), (f.dialog(&s, &format!("t{who}")), s));
    }
    assert_eq!(
        r.stack
            .blf_status(Some("a"))
            .iter()
            .map(|b| b.uri.as_str())
            .collect::<Vec<_>>(),
        ["1018", "1019"]
    );
    let (d19, _) = dialogs.get_mut("1019").unwrap();
    f.notify_ok(
        d19,
        "dialog",
        "active;expires=30",
        DI,
        &dialog_info(0, "full", &dlg("z", "recipient", "early", None)),
    )
    .await;
    blf_state(&mut r, "1019", BlfState::Ringing).await;

    // Relabel 1019 (hot, event with the new label), drop 1018, add 1020.
    c.blf = vec![blf("1019", "Carol B."), blf("1020", "Dave")];
    assert_eq!(
        r.stack.update_account(c.clone()).await.unwrap(),
        siple_sip::UpdateOutcome::Hot
    );
    let ev = until(&mut r, "relabel", |e| matches!(e, SipEvent::BlfState { uri, label, .. } if uri == "1019" && label == "Carol B.")).await;
    assert!(matches!(
        ev,
        SipEvent::BlfState {
            state: BlfState::Ringing,
            ..
        }
    ));
    let mut saw_unsub = false;
    let mut saw_new = false;
    for _ in 0..2 {
        let s = f.subscribe().await;
        if s.expires() == Some(0) {
            assert!(s.start_line().contains("sip:1018@"), "{}", s.start_line());
            f.respond(&s, "202 Accepted", None, &[]).await;
            saw_unsub = true;
        } else {
            assert!(s.start_line().contains("sip:1020@"), "{}", s.start_line());
            assert!(s.to_tag().is_none());
            f.respond(&s, "202 Accepted", Some("t20"), &[("Expires", "30")])
                .await;
            saw_new = true;
        }
    }
    assert!(saw_unsub && saw_new);
    assert_eq!(
        r.stack
            .blf_status(Some("a"))
            .iter()
            .map(|b| b.uri.as_str())
            .collect::<Vec<_>>(),
        ["1019", "1020"]
    );
    assert_eq!(r.stack.blf_status(Some("a"))[1].state, BlfState::Unknown);
    // 1019 was not resubscribed.
    assert!(f.no_subscribe(1).await);
    r.stack.shutdown().await;
}

/// Account shutdown unsubscribes established subscriptions.
#[tokio::test]
async fn shutdown_unsubscribes() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let s1 = f.subscribe().await;
    f.respond(&s1, "202 Accepted", Some("t1"), &[("Expires", "30")])
        .await;
    let mut d = f.dialog(&s1, "t1");
    let c1 = f
        .notify(
            &mut d,
            "dialog",
            "active;expires=30",
            DI,
            &dialog_info(0, "full", ""),
        )
        .await;
    f.notify_reply(c1).await;
    let stack = r.stack.clone();
    let sd = tokio::spawn(async move { stack.shutdown().await });
    let un = f.subscribe().await;
    assert_eq!(un.expires(), Some(0));
    assert_eq!(un.call_id(), s1.call_id());
    f.respond(&un, "202 Accepted", None, &[]).await;
    sd.await.unwrap();
}

/// With `register = true` subscriptions wait for the registration and use the registered Contact.
#[tokio::test]
async fn subscriptions_start_after_registration() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.register = true;
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let reg = f
        .wait("REGISTER", 10, |m| {
            m.is_request() && m.method() == "REGISTER"
        })
        .await
        .unwrap();
    // No SUBSCRIBE while the REGISTER is unanswered.
    assert!(f.no_subscribe(1).await);
    f.respond(
        &reg,
        "200 OK",
        Some("rt"),
        &[
            ("Contact", &format!("{};expires=300", reg.header("contact"))),
            ("Expires", "300"),
        ],
    )
    .await;
    let s = f.subscribe().await;
    assert_eq!(
        s.contact_uri(),
        reg.contact_uri(),
        "SUBSCRIBE carries the registered Contact"
    );
    f.respond(&s, "202 Accepted", Some("t"), &[("Expires", "30")])
        .await;
    r.stack.shutdown().await;
}

// --- review regressions -------------------------------------------------------------------------

async fn establish(f: &mut Fake, tag: &str) -> (Msg, Dialog) {
    let s = f.subscribe().await;
    f.respond(&s, "202 Accepted", Some(tag), &[("Expires", "30")])
        .await;
    let d = f.dialog(&s, tag);
    (s, d)
}

/// S1: a server that accepts and immediately terminates every subscription must not be
/// hammered; the resubscribe delay grows (1, 2, 4, 8 s ...).
#[tokio::test]
async fn immediate_termination_backs_off() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let t0 = tokio::time::Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(18) {
        let Some(s) = f
            .wait("SUBSCRIBE", 3, |m| {
                m.is_request() && m.method() == "SUBSCRIBE"
            })
            .await
        else {
            break;
        };
        n += 1;
        let tag = format!("t{n}");
        f.respond(&s, "202 Accepted", Some(&tag), &[("Expires", "30")])
            .await;
        let mut d = f.dialog(&s, &tag);
        let reason = if n % 2 == 0 {
            "terminated"
        } else {
            "terminated;reason=deactivated"
        };
        f.notify_ok(&mut d, "dialog", reason, DI, "").await;
    }
    r.stack.shutdown().await;
    assert!(
        (3..=6).contains(&n),
        "expected a decaying rate, got {n} SUBSCRIBEs in 18 s"
    );
}

/// S1: `Retry-After: 0` does not make the retry immediate.
#[tokio::test]
async fn retry_after_zero_is_floored() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let t0 = tokio::time::Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(2) {
        let Some(s) = f
            .wait("SUBSCRIBE", 2, |m| {
                m.is_request() && m.method() == "SUBSCRIBE"
            })
            .await
        else {
            break;
        };
        n += 1;
        f.respond(&s, "503 Service Unavailable", None, &[("Retry-After", "0")])
            .await;
    }
    r.stack.shutdown().await;
    assert!(n <= 2, "{n} SUBSCRIBEs in 2 s");
}

/// S2: a terminating NOTIFY that beats the 2xx leaves the subscription failed (not Pending).
#[tokio::test]
async fn terminal_notify_before_2xx_is_not_overwritten() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let s = f.subscribe().await;
    let mut d = f.dialog(&s, "t1");
    let code = f
        .notify_ok(&mut d, "dialog", "terminated;reason=rejected", DI, "")
        .await;
    assert_eq!(code, 200);
    let sid = r.stack.subscriptions(None)[0].id.clone();
    f.respond(&s, "202 Accepted", Some("t1"), &[("Expires", "30")])
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let st = sub_state(&r, &sid);
    let resub = !f.no_subscribe(2).await;
    r.stack.shutdown().await;
    assert!(matches!(st, SubState::Failed { .. }), "got {st:?}");
    assert!(!resub, "rejected is final");
}

/// S4: `Event: presence;id=5` NOTIFYs match their subscription.
#[tokio::test]
async fn event_id_parameter_matches() {
    let mut f = Fake::start().await;
    let r = rig();
    add(&r, cfg("a", &f)).await;
    r.stack
        .subscribe("a", "1018", "presence;id=5", None, Some(60))
        .unwrap();
    let s = f.subscribe_for("presence").await;
    f.respond(&s, "202 Accepted", Some("g1"), &[("Expires", "60")])
        .await;
    let mut d = f.dialog(&s, "g1");
    let ok = f
        .notify_ok(
            &mut d,
            "presence;id=5",
            "active;expires=60",
            "application/pidf+xml",
            "<presence/>",
        )
        .await;
    let other = f
        .notify_ok(
            &mut d,
            "presence;id=6",
            "active;expires=60",
            "application/pidf+xml",
            "<presence/>",
        )
        .await;
    r.stack.shutdown().await;
    assert_eq!((ok, other), (200, 481));
}

/// S3 / S5: disabling the account sends `Expires: 0` and clears the lamp.
#[tokio::test]
async fn disabling_account_unsubscribes() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c.clone()).await;
    let (_s, mut d) = establish(&mut f, "t1").await;
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=30",
        DI,
        &dialog_info(0, "full", &dlg("x", "initiator", "confirmed", None)),
    )
    .await;
    blf_state(&mut r, "1018", BlfState::Busy).await;
    c.enabled = false;
    r.stack.update_account(c).await.unwrap();
    let un = f
        .wait("unsubscribe", 3, |m| {
            m.is_request() && m.method() == "SUBSCRIBE"
        })
        .await;
    let lamps: Vec<_> = r.stack.blf_status(None).iter().map(|b| b.state).collect();
    r.stack.shutdown().await;
    assert!(
        un.is_some_and(|m| m.expires() == Some(0)),
        "no Expires: 0 on disable"
    );
    assert!(
        lamps.iter().all(|s| *s == BlfState::Unknown),
        "stale lamp: {lamps:?}"
    );
}

/// S6: stale NOTIFYs of forgotten dialogs keep getting 481 even after many dialogs died.
#[tokio::test]
async fn dead_dialogs_are_remembered_beyond_64() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = (0..70).map(|i| blf(&format!("2{i:03}"), "")).collect();
    add(&r, c.clone()).await;
    let mut ds = Vec::new();
    for i in 0..70 {
        let s = f.subscribe().await;
        let tag = format!("t{i}");
        f.respond(&s, "202 Accepted", Some(&tag), &[("Expires", "300")])
            .await;
        ds.push(f.dialog(&s, &tag));
    }
    c.username = "b".into();
    r.stack.update_account(c).await.unwrap();
    let mut newn = 0;
    while let Some(s) = f
        .wait("SUBSCRIBE", 3, |m| {
            m.is_request() && m.method() == "SUBSCRIBE"
        })
        .await
    {
        if s.expires() == Some(0) {
            continue;
        }
        newn += 1;
        f.respond(&s, "202 Accepted", Some("nn"), &[("Expires", "300")])
            .await;
        if newn == 70 {
            break;
        }
    }
    let mut codes = Vec::new();
    for d in ds.iter_mut() {
        codes.push(
            f.notify_ok(
                d,
                "dialog",
                "active;expires=200",
                DI,
                &dialog_info(9, "full", ""),
            )
            .await,
        );
    }
    r.stack.shutdown().await;
    assert!(codes.iter().all(|c| *c == 481), "{codes:?}");
}

/// S7: an AOR change sends no new SUBSCRIBE with the old Contact / AOR after the unsubscribe.
#[tokio::test]
async fn aor_change_releases_old_subscriptions_first() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c.clone()).await;
    let (first, _d) = establish(&mut f, "t0").await;
    let old_from = first.header("from");
    c.username = "b".into();
    let up = r.stack.update_account(c);
    let pump = async {
        let mut log: Vec<(Option<u32>, String)> = Vec::new();
        let t0 = tokio::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            let Some(m) = f
                .wait("SUBSCRIBE", 1, |m| {
                    m.is_request() && m.method() == "SUBSCRIBE"
                })
                .await
            else {
                continue;
            };
            log.push((m.expires(), m.header("from")));
            f.respond(&m, "202 Accepted", Some("tx"), &[("Expires", "300")])
                .await;
        }
        log
    };
    let (_o, log) = tokio::join!(up, pump);
    r.stack.shutdown().await;
    let old: Vec<_> = log.iter().filter(|(_, fr)| *fr == old_from).collect();
    assert_eq!(old.len(), 1, "{log:?}");
    assert_eq!(old[0].0, Some(0), "{log:?}");
    assert!(
        log.iter().any(|(e, fr)| *fr != old_from && *e != Some(0)),
        "{log:?}"
    );
}

/// S8: a NOTIFY with a lower CSeq than one already applied is acknowledged but ignored.
#[tokio::test]
async fn reordered_notify_is_ignored() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let (_s, mut d) = establish(&mut f, "t1").await;
    d.cseq = 4;
    let busy = dialog_info(5, "full", &dlg("x", "initiator", "confirmed", None));
    assert_eq!(
        f.notify_ok(&mut d, "dialog", "active;expires=30", DI, &busy)
            .await,
        200
    );
    blf_state(&mut r, "1018", BlfState::Busy).await;
    d.cseq = 1; // next is CSeq 2 (< 5), with a newer dialog-info version
    let idle = dialog_info(9, "full", "");
    assert_eq!(
        f.notify_ok(&mut d, "dialog", "active;expires=30", DI, &idle)
            .await,
        200
    );
    no_event(&mut r, 300, |e| {
        matches!(
            e,
            SipEvent::BlfState {
                state: BlfState::Idle,
                ..
            }
        )
    })
    .await;
    r.stack.shutdown().await;
}

/// S9: `giveup` without retry-after stops; with retry-after it resubscribes.
#[tokio::test]
async fn giveup_and_invariant() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1018", "")];
    add(&r, c).await;
    let (_s, mut d) = establish(&mut f, "t1").await;
    f.notify_ok(
        &mut d,
        "dialog",
        "terminated;reason=giveup;retry-after=1",
        DI,
        "",
    )
    .await;
    let (_s, mut d) = establish(&mut f, "t2").await;
    f.notify_ok(&mut d, "dialog", "terminated;reason=invariant", DI, "")
        .await;
    assert!(f.no_subscribe(3).await, "invariant must not resubscribe");
    r.stack.shutdown().await;
}

/// S10: BLF status follows the configured order.
#[tokio::test]
async fn blf_status_follows_config_order() {
    let f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1030", ""), blf("1010", ""), blf("1020", "")];
    add(&r, c.clone()).await;
    let uris = |r: &Rig| -> Vec<String> {
        r.stack
            .blf_status(None)
            .into_iter()
            .map(|b| b.uri)
            .collect()
    };
    assert_eq!(uris(&r), ["1030", "1010", "1020"]);
    c.blf = vec![blf("1020", ""), blf("1040", ""), blf("1030", "")];
    r.stack.update_account(c).await.unwrap();
    assert_eq!(uris(&r), ["1020", "1040", "1030"]);
    r.stack.shutdown().await;
}

// --- BLF health, directed pickup, blf.dial ----------------------------

use siple_config::{OrInherit, PickupMode};
use siple_sip::{CallState, SipError, SubscriptionPhase};

impl Fake {
    async fn invite(&mut self) -> Msg {
        self.wait("INVITE", 10, |m| m.is_request() && m.method() == "INVITE")
            .await
            .expect("no INVITE")
    }

    async fn no_invite(&mut self, ms: u64) -> bool {
        self.wait("no INVITE", ms.div_ceil(1000), |m| {
            m.is_request() && m.method() == "INVITE"
        })
        .await
        .is_none()
    }
}

/// Account `a` watching 1004; the notifier reports `dialogs` (a ringing recipient dialog with
/// call-id `cid1`, tags `l` / `r`, by default). Returns the notifier's subscription dialog.
async fn blf_with(
    f: &mut Fake,
    r: &mut Rig,
    c: AccountConfig,
    dialogs: &str,
    want: BlfState,
) -> Dialog {
    add(r, c).await;
    let s = f.subscribe().await;
    let mut d = f.dialog(&s, "nt");
    let n = f
        .notify(
            &mut d,
            "dialog",
            "active;expires=300",
            DI,
            &dialog_info(1, "full", dialogs),
        )
        .await;
    assert_eq!(f.notify_reply(n).await.code(), 200);
    f.respond(
        &s,
        "202 Accepted",
        Some("nt"),
        &[
            ("Expires", "300"),
            ("Contact", &format!("<sip:notifier@{}>", f.addr)),
        ],
    )
    .await;
    blf_state(r, "1004", want).await;
    d
}

fn ringing_dialogs() -> String {
    dlg("cid1", "recipient", "early", None)
}

fn pickup_cfg(f: &Fake, mode: PickupMode) -> AccountConfig {
    let mut c = cfg("a", f);
    c.pickup = mode;
    c.blf = vec![blf("1004", "Support")];
    c
}

fn request_uri(m: &Msg) -> String {
    m.start_line().split(' ').nth(1).unwrap_or("").to_string()
}

fn call_state_err(e: SipError, needle: &str) {
    match e {
        SipError::CallState { msg, .. } => assert!(msg.contains(needle), "{msg}"),
        other => panic!("expected invalid_state, got {other:?}"),
    }
}

/// `call.pickup` with `pickup = "prefix"` dials `**<extension>`; the refusal ends the call with
/// its code. The BLF status exposes the effective method and group.
#[tokio::test]
async fn pickup_prefix_dials_feature_code() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = pickup_cfg(&f, PickupMode::Prefix);
    c.blf[0].group = "Support".into();
    blf_with(&mut f, &mut r, c, &ringing_dialogs(), BlfState::Ringing).await;
    let st = &r.stack.blf_status(Some("a"))[0];
    assert_eq!(
        (st.pickup, st.group.as_str()),
        (PickupMode::Prefix, "Support")
    );

    let id = r.stack.pickup("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert!(
        request_uri(&inv).starts_with("sip:**1004@"),
        "{}",
        inv.start_line()
    );
    assert!(inv.header("replaces").is_empty(), "{}", inv.raw);
    f.respond(&inv, "404 Not Found", Some("x1"), &[]).await;
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.expect("end").code, 404);
    r.stack.shutdown().await;
}

/// A per-BLF `pickup_target` wins over the prefix, and a per-BLF `pickup` beats the account's
/// `off`.
#[tokio::test]
async fn pickup_per_blf_target_and_override() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = pickup_cfg(&f, PickupMode::Off);
    c.blf[0].pickup = OrInherit::Value(PickupMode::Prefix);
    c.blf[0].pickup_prefix = "*97".into();
    c.blf[0].pickup_target = "8004".into();
    blf_with(
        &mut f,
        &mut r,
        c.clone(),
        &ringing_dialogs(),
        BlfState::Ringing,
    )
    .await;
    assert_eq!(r.stack.blf_status(None)[0].pickup, PickupMode::Prefix);
    r.stack.pickup("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert!(
        request_uri(&inv).starts_with("sip:8004@"),
        "{}",
        inv.start_line()
    );
    // Without the target the entry's own prefix is used.
    r.stack.hangup(&r.stack.calls()[0].id).await.unwrap();
    c.blf[0].pickup_target.clear();
    assert_eq!(
        r.stack.update_account(c).await.unwrap(),
        siple_sip::UpdateOutcome::Hot
    );
    assert!(
        f.no_subscribe(1).await,
        "pickup keys are hot: no resubscribe"
    );
    r.stack.pickup("a", "1004").await.unwrap();
    let inv = f
        .wait("2nd INVITE", 10, |m| {
            m.is_request() && m.method() == "INVITE" && m.start_line().contains("*971004@")
        })
        .await;
    assert!(inv.is_some(), "expected an INVITE to *971004");
    r.stack.shutdown().await;
}

/// Refused (`invalid_state`) when not configured or not ringing; unknown entries are not found;
/// switching `pickup` on is hot and needs no resubscribe.
#[tokio::test]
async fn pickup_refused_when_not_ringing_or_not_configured() {
    let mut f = Fake::start().await;
    let mut r = rig();
    // Ringing, but pickup is off.
    let c = pickup_cfg(&f, PickupMode::Off);
    let mut d = blf_with(
        &mut f,
        &mut r,
        c.clone(),
        &ringing_dialogs(),
        BlfState::Ringing,
    )
    .await;
    call_state_err(
        r.stack.pickup("a", "1004").await.unwrap_err(),
        "pickup not configured",
    );
    assert!(matches!(
        r.stack.pickup("a", "9999").await.unwrap_err(),
        SipError::UnknownBlf(_)
    ));
    assert!(matches!(
        r.stack.pickup("nope", "1004").await.unwrap_err(),
        SipError::UnknownAccount(_)
    ));
    // Turned on while ringing: hot, works.
    let mut on = c.clone();
    on.pickup = PickupMode::Prefix;
    assert_eq!(
        r.stack.update_account(on.clone()).await.unwrap(),
        siple_sip::UpdateOutcome::Hot
    );
    assert!(f.no_subscribe(1).await);
    // Answered elsewhere (confirmed): refused, nothing is sent.
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=300",
        DI,
        &dialog_info(2, "full", &dlg("cid1", "recipient", "confirmed", None)),
    )
    .await;
    blf_state(&mut r, "1004", BlfState::Busy).await;
    call_state_err(
        r.stack.pickup("a", "1004").await.unwrap_err(),
        "not ringing",
    );
    // A ringing call that *we* see as the monitored phone dialing out is not pickable either.
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=300",
        DI,
        &dialog_info(3, "full", &dlg("cid2", "initiator", "early", None)),
    )
    .await;
    blf_state(&mut r, "1004", BlfState::Dialing).await;
    call_state_err(
        r.stack.pickup("a", "1004").await.unwrap_err(),
        "not ringing",
    );
    assert!(f.no_invite(700).await, "a refused pickup sends nothing");
    r.stack.shutdown().await;
}

/// A ringing recipient dialog (call-id `cid1`, tags `l` / `r`) whose caller target is `remote`.
fn dlg_remote(remote: &str) -> String {
    format!(
        "<dialog id=\"cid1\" call-id=\"cid1\" local-tag=\"l\" remote-tag=\"r\" direction=\"recipient\"><state>early</state><local><identity>sip:1004@x</identity><target uri=\"sip:1004@x\"/></local><remote><identity display=\"Caller\">sip:1019@x</identity><target uri=\"{remote}\"/></remote></dialog>"
    )
}

/// `pickup = "replaces"` (RFC 5359 §2.16): the INVITE goes to the caller's dialog-info target
/// with `Require: replaces` and `to-tag = remote-tag`, `from-tag = local-tag`.
#[tokio::test]
async fn pickup_replaces_invites_the_caller() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let c = pickup_cfg(&f, PickupMode::Replaces);
    let dialogs = dlg_remote(&format!("sip:1019@{}", f.addr));
    blf_with(&mut f, &mut r, c, &dialogs, BlfState::Ringing).await;
    r.stack.pickup("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert!(
        request_uri(&inv).starts_with("sip:1019@"),
        "to the caller: {}",
        inv.start_line()
    );
    assert_eq!(
        inv.header("replaces"),
        "cid1;to-tag=r;from-tag=l;early-only",
        "{}",
        inv.raw
    );
    assert_eq!(inv.header("require"), "replaces", "{}", inv.raw);
    r.stack.shutdown().await;
}

/// `pickup_replaces_target = "monitored"`: INVITE to the monitored URI, same tags.
#[tokio::test]
async fn pickup_replaces_monitored_variant() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = pickup_cfg(&f, PickupMode::Replaces);
    c.pickup_replaces_target = siple_config::PickupReplacesTarget::Monitored;
    blf_with(&mut f, &mut r, c, &ringing_dialogs(), BlfState::Ringing).await;
    r.stack.pickup("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert!(
        request_uri(&inv).starts_with("sip:1004@"),
        "{}",
        inv.start_line()
    );
    assert_eq!(
        inv.header("replaces"),
        "cid1;to-tag=r;from-tag=l;early-only",
        "{}",
        inv.raw
    );
    assert_eq!(inv.header("require"), "replaces");
    r.stack.shutdown().await;
}

/// `replaces` needs the dialog tags and the caller's target: otherwise `invalid_state`.
#[tokio::test]
async fn pickup_replaces_without_tags_is_refused() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let c = pickup_cfg(&f, PickupMode::Replaces);
    let bare = "<dialog id=\"x\" direction=\"recipient\"><state>early</state></dialog>";
    blf_with(&mut f, &mut r, c, bare, BlfState::Ringing).await;
    call_state_err(
        r.stack.pickup("a", "1004").await.unwrap_err(),
        "dialog-info",
    );
    assert!(f.no_invite(500).await);
    r.stack.shutdown().await;
}

/// A crafted NOTIFY (`&#13;&#10;` in the call-id) cannot inject headers: the pickup is refused
/// and nothing is sent.
#[tokio::test]
async fn pickup_replaces_refuses_injected_call_id() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let c = pickup_cfg(&f, PickupMode::Replaces);
    let evil = format!(
        "<dialog id=\"x\" call-id=\"cid1&#13;&#10;Require: foo\" local-tag=\"l\" remote-tag=\"r\" direction=\"recipient\"><state>early</state><remote><identity>sip:1019@x</identity><target uri=\"sip:1019@{}\"/></remote></dialog>",
        f.addr
    );
    blf_with(&mut f, &mut r, c, &evil, BlfState::Ringing).await;
    let st = r.stack.blf_status(None).remove(0);
    assert!(st.dialogs[0].call_id.is_none(), "{:?}", st.dialogs[0]);
    call_state_err(r.stack.pickup("a", "1004").await.unwrap_err(), "call-id");
    assert!(f.no_invite(500).await);
    r.stack.shutdown().await;
}

/// `blf.dial`: the entry's URI is dialed on its account; unknown entries are refused.
#[tokio::test]
async fn blf_dial_calls_the_entry() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let c = pickup_cfg(&f, PickupMode::Off);
    blf_with(&mut f, &mut r, c, "", BlfState::Idle).await;
    let id = r.stack.blf_dial("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert!(
        request_uri(&inv).starts_with("sip:1004@"),
        "{}",
        inv.start_line()
    );
    assert!(inv.header("replaces").is_empty());
    f.respond(&inv, "486 Busy Here", Some("x2"), &[]).await;
    assert_eq!(
        state_of(&mut r, &id, CallState::Ended)
            .await
            .end
            .unwrap()
            .code,
        486
    );
    assert!(matches!(
        r.stack.blf_dial("a", "1005").await.unwrap_err(),
        SipError::UnknownBlf(_)
    ));
    assert!(matches!(
        r.stack.blf_dial("zz", "1004").await.unwrap_err(),
        SipError::UnknownAccount(_)
    ));
    r.stack.shutdown().await;
}

/// A refused SUBSCRIBE: `phase = failed` with the code, the lamp is `failed`; a retryable
/// failure is `retrying` with `retry_at` and stale.
#[tokio::test]
async fn subscription_failure_is_reported_with_code() {
    let mut f = Fake::start().await;
    let r = rig();
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1004", "Support"), blf("1005", "Other")];
    add(&r, c).await;
    let mut seen = 0;
    while seen < 2 {
        let s = f.subscribe().await;
        if s.start_line().contains("sip:1004@") {
            f.respond(&s, "404 Not Found", Some("t"), &[]).await;
        } else {
            f.respond(
                &s,
                "503 Service Unavailable",
                Some("t"),
                &[("Retry-After", "60")],
            )
            .await;
        }
        seen += 1;
    }
    let by_uri = |r: &Rig, u: &str| {
        r.stack
            .blf_status(None)
            .into_iter()
            .find(|b| b.uri == u)
            .unwrap()
    };
    for _ in 0..100 {
        if by_uri(&r, "1004").subscription.phase == SubscriptionPhase::Failed
            && by_uri(&r, "1005").subscription.phase == SubscriptionPhase::Retrying
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let b = by_uri(&r, "1004");
    assert_eq!(b.state, BlfState::Failed);
    assert_eq!(b.subscription.phase, SubscriptionPhase::Failed);
    assert_eq!(b.subscription.last_code, Some(404));
    assert_eq!(b.subscription.last_reason.as_deref(), Some("Not Found"));
    assert!(b.subscription.retry_at.is_none());
    assert!(!b.stale, "a refused subscription shows Failed, not stale");
    let b = by_uri(&r, "1005");
    assert_eq!(b.subscription.phase, SubscriptionPhase::Retrying);
    assert_eq!(b.subscription.last_code, Some(503));
    assert!(b.subscription.retry_at.is_some());
    assert!(b.stale, "retrying counts as stale");
    r.stack.shutdown().await;
}

/// No NOTIFY or refresh past `expires_at` (+ grace): stale, with the last known state kept; a
/// refresh clears it. The 30 s grace is shortened through the test hook.
#[tokio::test]
async fn expiry_without_refresh_makes_the_key_stale() {
    let mut f = Fake::start().await;
    let mut r = rig();
    r.stack.set_blf_stale_grace(Duration::from_millis(300));
    let mut c = cfg("a", &f);
    c.blf = vec![blf("1004", "Support")];
    add(&r, c).await;
    let s = f.subscribe().await;
    let mut d = f.dialog(&s, "nt");
    let n = f
        .notify(
            &mut d,
            "dialog",
            "active;expires=1",
            DI,
            &dialog_info(1, "full", &ringing_dialogs()),
        )
        .await;
    assert_eq!(f.notify_reply(n).await.code(), 200);
    blf_state(&mut r, "1004", BlfState::Ringing).await;
    let b = r.stack.blf_status(None).remove(0);
    assert!(!b.stale);
    assert_eq!(b.subscription.phase, SubscriptionPhase::Active);
    assert!(b.subscription.expires_at.is_some());
    // The refresh at ~1 s is left unanswered: the entry goes stale ~0.3 s after expires_at.
    let refresh = f.subscribe().await;
    let mut stale = None;
    for _ in 0..100 {
        let b = r.stack.blf_status(None).remove(0);
        if b.stale {
            stale = Some(b);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let b = stale.expect("never became stale");
    assert_eq!(b.state, BlfState::Ringing, "the last known state stays");
    assert_eq!(b.subscription.phase, SubscriptionPhase::Active);
    // The notifier answers late: refresh accepted + NOTIFY with a fresh lifetime.
    f.respond(&refresh, "202 Accepted", None, &[("Expires", "30")])
        .await;
    f.notify_ok(
        &mut d,
        "dialog",
        "active;expires=30",
        DI,
        &dialog_info(2, "full", &ringing_dialogs()),
    )
    .await;
    assert!(!r.stack.blf_status(None).remove(0).stale);
    r.stack.shutdown().await;
}

async fn wait_phase(r: &Rig, phase: SubscriptionPhase) -> siple_sip::BlfStatus {
    for _ in 0..100 {
        let b = r.stack.blf_status(None).remove(0);
        if b.subscription.phase == phase {
            return b;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never reached {phase:?}: {:?}", r.stack.blf_status(None));
}

/// While a resubscribe is scheduled or in flight the phase stays `retrying` (with `retry_at`),
/// the entry is stale, the last known state stays, and pickup is refused.
#[tokio::test]
async fn retrying_keeps_last_state_and_is_stale_until_active() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let c = pickup_cfg(&f, PickupMode::Prefix);
    let mut d = blf_with(&mut f, &mut r, c, &ringing_dialogs(), BlfState::Ringing).await;
    let n = f
        .notify(&mut d, "dialog", "terminated;reason=deactivated", DI, "")
        .await;
    assert_eq!(f.notify_reply(n).await.code(), 200);
    let b = wait_phase(&r, SubscriptionPhase::Retrying).await;
    assert_eq!(b.state, BlfState::Ringing, "last known state stays");
    assert!(b.stale && b.subscription.retry_at.is_some());
    call_state_err(r.stack.pickup("a", "1004").await.unwrap_err(), "stale");
    // The resubscribe is now in flight (unanswered): still retrying, retry_at kept.
    let s2 = f.subscribe().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b = r.stack.blf_status(None).remove(0);
    assert_eq!(b.subscription.phase, SubscriptionPhase::Retrying);
    assert!(b.stale && b.subscription.retry_at.is_some(), "{b:?}");
    assert_eq!(b.state, BlfState::Ringing);
    // Accepted: active again, fresh.
    let mut d2 = f.dialog(&s2, "nt2");
    let n = f
        .notify(
            &mut d2,
            "dialog",
            "active;expires=300",
            DI,
            &dialog_info(1, "full", ""),
        )
        .await;
    assert_eq!(f.notify_reply(n).await.code(), 200);
    f.respond(&s2, "202 Accepted", Some("nt2"), &[("Expires", "300")])
        .await;
    blf_state(&mut r, "1004", BlfState::Idle).await;
    let b = r.stack.blf_status(None).remove(0);
    assert_eq!(b.subscription.phase, SubscriptionPhase::Active);
    assert!(!b.stale && b.subscription.retry_at.is_none());
    r.stack.shutdown().await;
}

/// Pickup is also refused while the entry is stale (expired without refresh).
#[tokio::test]
async fn pickup_refused_when_stale() {
    let mut f = Fake::start().await;
    let mut r = rig();
    r.stack.set_blf_stale_grace(Duration::from_millis(200));
    let c = pickup_cfg(&f, PickupMode::Prefix);
    add(&r, c).await;
    let s = f.subscribe().await;
    let mut d = f.dialog(&s, "nt");
    let n = f
        .notify(
            &mut d,
            "dialog",
            "active;expires=1",
            DI,
            &dialog_info(1, "full", &ringing_dialogs()),
        )
        .await;
    assert_eq!(f.notify_reply(n).await.code(), 200);
    blf_state(&mut r, "1004", BlfState::Ringing).await;
    assert!(r.stack.blf_status(None)[0].pickup_offered());
    for _ in 0..100 {
        if r.stack.blf_status(None)[0].stale {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(r.stack.blf_status(None)[0].stale);
    assert!(!r.stack.blf_status(None)[0].pickup_offered());
    call_state_err(
        r.stack.pickup("a", "1004").await.unwrap_err(),
        "BLF state is stale",
    );
    assert!(f.no_invite(300).await);
    r.stack.shutdown().await;
}

/// A NOTIFY-supplied caller target never chooses the next hop: the INVITE goes through the
/// account's registrar even though the URI names an unresolvable host, and URI parameters /
/// headers (`maddr`, `transport`, `?Route=`) are stripped from the Request-URI.
#[tokio::test]
async fn pickup_replaces_routes_via_registrar_and_strips_uri_params() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let c = pickup_cfg(&f, PickupMode::Replaces);
    let dialogs =
        dlg_remote("sip:1019@evil.invalid:5099;maddr=127.0.0.2;transport=tcp?Route=%3Csip:x%3E");
    blf_with(&mut f, &mut r, c, &dialogs, BlfState::Ringing).await;
    r.stack.pickup("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert_eq!(
        request_uri(&inv),
        "sip:1019@evil.invalid:5099",
        "{}",
        inv.raw
    );
    assert!(inv.header("route").is_empty(), "{}", inv.raw);
    r.stack.shutdown().await;
}

/// Prefix pickups also go through the registrar, even when `pickup_target` names another host.
#[tokio::test]
async fn pickup_prefix_target_on_another_host_still_routes_via_registrar() {
    let mut f = Fake::start().await;
    let mut r = rig();
    let mut c = pickup_cfg(&f, PickupMode::Prefix);
    c.blf[0].pickup_target = "sip:8004@elsewhere.invalid".into();
    blf_with(&mut f, &mut r, c, &ringing_dialogs(), BlfState::Ringing).await;
    r.stack.pickup("a", "1004").await.unwrap();
    let inv = f.invite().await;
    assert_eq!(
        request_uri(&inv),
        "sip:8004@elsewhere.invalid",
        "{}",
        inv.raw
    );
    r.stack.shutdown().await;
}
