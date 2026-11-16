//! Shared harness of the call regression / hold tests: a stack with a [`NullMediaProvider`],
//! peer-mode accounts on loopback and a raw-UDP SIP probe.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Transport};
use siple_sip::{
    AnswerOptions, CallInfo, CallState, NullMediaProvider, NullWireSink, SipEvent, SipStack,
    SipStackOptions,
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// Ports already handed out by this process. The OS may give a just-released ephemeral port to
/// the next `bind(0)` probe (another test's, or another account of the same rig) before the first
/// owner has bound it; two stacks then fight over one port.
static USED_PORTS: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());

fn unique_port(probe: impl Fn() -> u16) -> u16 {
    loop {
        let port = probe();
        let mut used = USED_PORTS.lock().unwrap();
        if !used.contains(&port) {
            used.push(port);
            return port;
        }
    }
}

/// A free UDP port that this process has not handed out before.
pub fn free_udp_port() -> u16 {
    unique_port(|| {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    })
}

/// A free TCP port that this process has not handed out before.
pub fn free_tcp_port() -> u16 {
    unique_port(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    })
}

pub struct Rig {
    pub stack: SipStack,
    pub events: UnboundedReceiver<SipEvent>,
    /// Events received while waiting for something else; matched first by later waits.
    pub buf: Vec<SipEvent>,
    pub media: Arc<NullMediaProvider>,
}

pub fn rig() -> Rig {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let (tx, rx) = unbounded_channel();
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-test/1".into(),
        events: tx,
        wire_sink: Arc::new(NullWireSink),
    });
    let media = NullMediaProvider::new();
    stack.set_media_provider(media.clone());
    Rig {
        stack,
        events: rx,
        buf: Vec::new(),
        media,
    }
}

pub fn peer(id: &str) -> AccountConfig {
    AccountConfig {
        id: id.into(),
        username: id.into(),
        domain: "peer.test".into(),
        register: false,
        transport: Transport::Udp,
        bind_address: "127.0.0.1".into(),
        tls_verify: false,
        sip_trace: false,
        ..Default::default()
    }
}

/// Adds the account; returns its local `ip:port`.
pub async fn add(r: &Rig, cfg: AccountConfig) -> String {
    let id = cfg.id.clone();
    r.stack
        .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
        .await
        .unwrap();
    r.stack
        .account_status(&id)
        .unwrap()
        .local_addr
        .unwrap_or_default()
}

pub async fn until(r: &mut Rig, what: &str, pred: impl Fn(&SipEvent) -> bool) -> SipEvent {
    if let Some(i) = r.buf.iter().position(&pred) {
        return r.buf.remove(i);
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let e = r.events.recv().await.expect("event channel closed");
            if pred(&e) {
                return e;
            }
            r.buf.push(e);
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

pub async fn state_of(r: &mut Rig, id: &str, st: CallState) -> CallInfo {
    let id = id.to_string();
    match until(
        r,
        &format!("{id} -> {st:?}"),
        |e| matches!(e, SipEvent::CallState { call } if call.id == id && call.state == st),
    )
    .await
    {
        SipEvent::CallState { call } => call,
        _ => unreachable!(),
    }
}

/// Next `CallUpdated` of `id` that satisfies `pred`.
pub async fn updated(r: &mut Rig, id: &str, pred: impl Fn(&CallInfo) -> bool) -> CallInfo {
    let id = id.to_string();
    match until(
        r,
        &format!("{id} updated"),
        |e| matches!(e, SipEvent::CallUpdated { call } if call.id == id && pred(call)),
    )
    .await
    {
        SipEvent::CallUpdated { call } => call,
        _ => unreachable!(),
    }
}

pub async fn incoming(r: &mut Rig) -> CallInfo {
    match until(r, "incoming call", |e| {
        matches!(e, SipEvent::CallIncoming { .. })
    })
    .await
    {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    }
}

pub async fn incoming_on(r: &mut Rig, account: &str) -> CallInfo {
    let account = account.to_string();
    match until(
        r,
        "incoming call",
        |e| matches!(e, SipEvent::CallIncoming { call } if call.account == account),
    )
    .await
    {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    }
}

/// Peer accounts `a` (dialer) and `b`; a calls b, b answers. Returns `(a_call, b_call)` once
/// both are Active.
pub async fn active_pair(r: &mut Rig) -> (String, String) {
    add(r, peer("a")).await;
    let b = add(r, peer("b")).await;
    let a_id = r
        .stack
        .dial("a", &format!("sip:b@{b}"), Default::default())
        .await
        .unwrap();
    let b_call = incoming_on(r, "b").await;
    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(r, &a_id, CallState::Active).await;
    state_of(r, &b_call.id, CallState::Active).await;
    (a_id, b_call.id)
}

pub fn hdr<'a>(m: &'a str, n: &str) -> Vec<&'a str> {
    m.lines()
        .filter(|l| l.to_ascii_lowercase().starts_with(&format!("{n}:")))
        .collect()
}

pub fn to_tag(resp: &str) -> String {
    let to = hdr(resp, "to")[0];
    to.split("tag=").nth(1).unwrap().trim().to_string()
}

/// A raw-UDP SIP endpoint talking to the stack account at `dest`.
pub struct Raw {
    pub sock: UdpSocket,
    pub dest: std::net::SocketAddr,
    pub call_id: String,
    pub local: std::net::SocketAddr,
}

impl Raw {
    pub async fn new(dest: &str) -> Raw {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let local = sock.local_addr().unwrap();
        Raw {
            sock,
            dest: dest.parse().unwrap(),
            call_id: format!("raw-{}@probe", local.port()),
            local,
        }
    }

    pub fn sdp(&self) -> String {
        self.sdp_dir("sendrecv", 1)
    }

    pub fn sdp_dir(&self, dir: &str, version: u32) -> String {
        format!(
            "v=0\r\no=- 1 {version} IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio {} RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na={dir}\r\n",
            self.local.port() + 1
        )
    }

    pub fn msg(
        &self,
        method: &str,
        to_tag: Option<&str>,
        cseq: u32,
        extra: &str,
        body: &str,
    ) -> String {
        let to = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
        let ct = if body.is_empty() {
            String::new()
        } else {
            "Content-Type: application/sdp\r\n".into()
        };
        format!(
            "{method} sip:b@{} SIP/2.0\r\nVia: SIP/2.0/UDP {};branch=z9hG4bK{}{cseq}\r\nMax-Forwards: 70\r\nFrom: \"Probe\" <sip:probe@probe.test>;tag=ptag\r\nTo: <sip:b@{}>{to}\r\nCall-ID: {}\r\nCSeq: {cseq} {method}\r\nContact: <sip:probe@{}>\r\n{ct}{extra}Content-Length: {}\r\n\r\n{body}",
            self.dest,
            self.local,
            method.to_lowercase(),
            self.dest,
            self.call_id,
            self.local,
            body.len()
        )
    }

    pub async fn send(&self, m: &str) {
        self.sock.send_to(m.as_bytes(), self.dest).await.unwrap();
    }

    pub async fn recv_any(&self, secs: u64) -> Option<String> {
        let mut buf = vec![0u8; 8192];
        match tokio::time::timeout(Duration::from_secs(secs), self.sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(String::from_utf8_lossy(&buf[..n]).into_owned()),
            _ => None,
        }
    }

    pub async fn response(&self, cseq_line: &str) -> String {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let s = self.recv_any(10).await.unwrap();
                if s.starts_with("SIP/2.0") && s.contains(cseq_line) {
                    return s;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no response for {cseq_line}"))
    }

    pub async fn final_response(&self, cseq_line: &str) -> String {
        loop {
            let r = self.response(cseq_line).await;
            if !r.starts_with("SIP/2.0 1") {
                return r;
            }
        }
    }

    /// Waits for a request with method `method` and returns it.
    pub async fn request(&self, method: &str, secs: u64) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let mut buf = vec![0u8; 8192];
            match tokio::time::timeout(left, self.sock.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    let s = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if s.starts_with(&format!("{method} ")) {
                        return Some(s);
                    }
                }
                _ => return None,
            }
        }
    }

    /// 200 OK to a request we received.
    pub async fn ok_to(&self, req: &str) {
        let resp = format!(
            "SIP/2.0 200 OK\r\n{}\r\n{}\r\n{}\r\n{}\r\n{}\r\nContent-Length: 0\r\n\r\n",
            hdr(req, "via").join("\r\n"),
            hdr(req, "from")[0],
            hdr(req, "to")[0],
            hdr(req, "call-id")[0],
            hdr(req, "cseq")[0]
        );
        self.send_to_dest(&resp).await;
    }

    /// Answers `req` with `status` ("488 Not Acceptable Here") and an optional SDP body.
    pub async fn reply(&self, req: &str, status: &str, body: Option<&str>) {
        let body = body.unwrap_or("");
        let ct = if body.is_empty() {
            ""
        } else {
            "Content-Type: application/sdp\r\n"
        };
        let resp = format!(
            "SIP/2.0 {status}\r\n{}\r\n{}\r\n{}\r\n{}\r\n{}\r\nContact: <sip:probe@{}>\r\n{ct}Content-Length: {}\r\n\r\n{body}",
            hdr(req, "via").join("\r\n"),
            hdr(req, "from")[0],
            hdr(req, "to")[0],
            hdr(req, "call-id")[0],
            hdr(req, "cseq")[0],
            self.local,
            body.len()
        );
        self.send_to_dest(&resp).await;
    }

    async fn send_to_dest(&self, m: &str) {
        self.sock.send_to(m.as_bytes(), self.dest).await.unwrap();
    }
}

/// Inbound call from a raw probe, answered and ACKed. Returns `(raw, b call id, our tag)`.
pub async fn established(r: &mut Rig, b: &str) -> (Raw, String, String) {
    let raw = Raw::new(b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let call = incoming(r).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    let tag = to_tag(&ok);
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    state_of(r, &call.id, CallState::Active).await;
    (raw, call.id, tag)
}
