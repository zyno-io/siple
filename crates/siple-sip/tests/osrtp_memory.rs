mod common;

use std::time::Duration;

use common::*;
use siple_config::SrtpPolicy;
use siple_sip::{CallState, DialOptions};
use tokio::net::UdpSocket;

fn base(invite: &str, caddr: std::net::SocketAddr, tag: bool) -> String {
    let to = hdr(invite, "to")[0];
    format!(
        "{}\r\n{}\r\n{}{}\r\n{}\r\n{}\r\nContact: <sip:x@{caddr}>\r\n",
        hdr(invite, "via").join("\r\n"),
        hdr(invite, "from")[0],
        to,
        if tag { ";tag=callee1" } else { "" },
        hdr(invite, "call-id")[0],
        hdr(invite, "cseq")[0]
    )
}

async fn next_invite(callee: &UdpSocket) -> (String, std::net::SocketAddr) {
    let mut buf = vec![0u8; 8192];
    loop {
        let (n, from) = tokio::time::timeout(Duration::from_secs(5), callee.recv_from(&mut buf))
            .await
            .expect("no INVITE")
            .unwrap();
        let m = String::from_utf8_lossy(&buf[..n]).into_owned();
        if m.starts_with("INVITE") {
            return (m, from);
        }
    }
}

async fn send(
    callee: &UdpSocket,
    from: std::net::SocketAddr,
    line: &str,
    invite: &str,
    tag: bool,
    extra: &str,
) {
    let caddr = callee.local_addr().unwrap();
    let b = base(invite, caddr, tag);
    let resp = format!("SIP/2.0 {line}\r\n{b}{extra}Content-Length: 0\r\n\r\n");
    callee.send_to(resp.as_bytes(), from).await.unwrap();
}

async fn ok200(callee: &UdpSocket, from: std::net::SocketAddr, invite: &str) {
    let caddr = callee.local_addr().unwrap();
    let b = base(invite, caddr, true);
    let sdp = "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 4000 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\n";
    let ok = format!(
        "SIP/2.0 200 OK\r\n{b}Content-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
        sdp.len()
    );
    callee.send_to(ok.as_bytes(), from).await.unwrap();
}

fn mem(r: &Rig) -> Vec<String> {
    r.stack
        .account_status("a")
        .unwrap()
        .srtp_optional_downgraded
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

async fn account_optional(r: &Rig) {
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Optional;
    add(r, acfg).await;
}

/// Brings `key`'s destination into the memory: OSRTP refused (488), plain retry answered 200.
async fn remember(r: &mut Rig, callee: &UdpSocket) {
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:seed@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(callee).await;
    assert!(i1.contains("a=crypto"));
    send(callee, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(callee).await;
    assert!(!i2.contains("a=crypto"));
    ok200(callee, from, &i2).await;
    state_of(r, &id, CallState::Active).await;
    settle().await;
    assert_eq!(mem(r), vec![caddr.to_string()]);
    let _ = r.stack.hangup(&id).await;
}

/// A `[media].codecs` change (inherited by the account) clears the refusal memory; an unrelated
/// `[media]` change keeps it.
#[tokio::test]
async fn media_codec_change_clears_memory() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    remember(&mut r, &callee).await;
    let mut m = siple_config::MediaConfig::default();
    m.rtcp_mux = !m.rtcp_mux;
    r.stack.set_media(&m);
    settle().await;
    assert_eq!(mem(&r).len(), 1, "non-codec media change cleared memory");
    m.codecs = vec![siple_config::Codec::Pcmu];
    r.stack.set_media(&m);
    settle().await;
    assert!(
        mem(&r).is_empty(),
        "codec change kept memory: {:?}",
        mem(&r)
    );
    r.stack.shutdown().await;
}

/// Late-negotiating B2BUA: rings, then refuses both offers. Provisionals prove nothing.
#[tokio::test]
async fn ringing_then_488_on_both_offers_remembers_nothing() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee).await;
    assert!(i1.contains("a=crypto"));
    send(&callee, from, "180 Ringing", &i1, true, "").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&callee, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(&callee).await;
    assert!(!i2.contains("a=crypto"));
    send(&callee, from, "180 Ringing", &i2, true, "").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(mem(&r).is_empty(), "provisional remembered: {:?}", mem(&r));
    send(&callee, from, "488 Not Acceptable Here", &i2, true, "").await;
    state_of(&mut r, &id, CallState::Ended).await;
    settle().await;
    assert!(
        mem(&r).is_empty(),
        "both offers refused but remembered: {:?}",
        mem(&r)
    );
    r.stack.shutdown().await;
}

/// A 183 without a To-tag, then 488: nothing remembered.
#[tokio::test]
async fn tagless_183_then_488_remembers_nothing() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee).await;
    send(&callee, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(&callee).await;
    send(&callee, from, "183 Session Progress", &i2, false, "").await;
    settle().await;
    assert!(mem(&r).is_empty(), "tagless 183 remembered: {:?}", mem(&r));
    send(&callee, from, "488 Not Acceptable Here", &i2, true, "").await;
    state_of(&mut r, &id, CallState::Ended).await;
    settle().await;
    assert!(mem(&r).is_empty(), "{:?}", mem(&r));
    r.stack.shutdown().await;
}

/// Plain retry answered 2xx: remembered; the next call offers plain first.
#[tokio::test]
async fn plain_retry_2xx_is_remembered() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    remember(&mut r, &callee).await;
    let id = r
        .stack
        .dial("a", &format!("sip:n@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i, from) = next_invite(&callee).await;
    assert!(
        !i.contains("a=crypto"),
        "remembered destination must get plain first"
    );
    ok200(&callee, from, &i).await;
    state_of(&mut r, &id, CallState::Active).await;
    r.stack.shutdown().await;
}

/// 302 after the plain retry: the pending probe is dropped, B is evaluated on its own (OSRTP
/// first) and nothing is remembered for A or B.
#[tokio::test]
async fn redirect_cancels_probe_and_reevaluates_target() {
    let mut r = rig();
    account_optional(&r).await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let aaddr = a.local_addr().unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let baddr = b.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{aaddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&a).await;
    assert!(i1.contains("a=crypto"));
    send(&a, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(&a).await;
    assert!(!i2.contains("a=crypto"));
    let bb = base(&i2, aaddr, true).replace(&format!("Contact: <sip:x@{aaddr}>\r\n"), "");
    let resp = format!(
        "SIP/2.0 302 Moved Temporarily\r\n{bb}Contact: <sip:y@{baddr}>\r\nContent-Length: 0\r\n\r\n"
    );
    a.send_to(resp.as_bytes(), from).await.unwrap();
    let (i3, fromb) = next_invite(&b).await;
    assert!(
        i3.contains("a=crypto"),
        "B must be evaluated on its own: OSRTP first"
    );
    ok200(&b, fromb, &i3).await;
    state_of(&mut r, &id, CallState::Active).await;
    settle().await;
    assert!(
        mem(&r).is_empty(),
        "redirect taught something: {:?}",
        mem(&r)
    );
    r.stack.shutdown().await;
}

/// After a redirect, B's own refusal + plain 2xx is remembered for B, never for A.
#[tokio::test]
async fn redirect_target_refusal_is_remembered_for_target_only() {
    let mut r = rig();
    account_optional(&r).await;
    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let aaddr = a.local_addr().unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let baddr = b.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{aaddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&a).await;
    let bb = base(&i1, aaddr, true).replace(&format!("Contact: <sip:x@{aaddr}>\r\n"), "");
    let resp = format!(
        "SIP/2.0 302 Moved Temporarily\r\n{bb}Contact: <sip:y@{baddr}>\r\nContent-Length: 0\r\n\r\n"
    );
    a.send_to(resp.as_bytes(), from).await.unwrap();
    let (i2, fromb) = next_invite(&b).await;
    assert!(i2.contains("a=crypto"));
    send(&b, fromb, "488 Not Acceptable Here", &i2, true, "").await;
    let (i3, fromb) = next_invite(&b).await;
    assert!(!i3.contains("a=crypto"));
    ok200(&b, fromb, &i3).await;
    state_of(&mut r, &id, CallState::Active).await;
    settle().await;
    assert_eq!(mem(&r), vec![baddr.to_string()]);
    r.stack.shutdown().await;
}

/// Plain-first refused, OSRTP retry rings then 488: the entry is kept.
#[tokio::test]
async fn osrtp_retry_provisional_then_488_keeps_entry() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    remember(&mut r, &callee).await;
    let id = r
        .stack
        .dial("a", &format!("sip:n@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee).await;
    assert!(!i1.contains("a=crypto"));
    send(&callee, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(&callee).await;
    assert!(i2.contains("a=crypto"));
    send(&callee, from, "180 Ringing", &i2, true, "").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&callee, from, "488 Not Acceptable Here", &i2, true, "").await;
    state_of(&mut r, &id, CallState::Ended).await;
    settle().await;
    assert_eq!(
        mem(&r),
        vec![caddr.to_string()],
        "entry forgotten without a 2xx"
    );
    r.stack.shutdown().await;
}

/// Plain-first refused, OSRTP retry answered 2xx: forgotten.
#[tokio::test]
async fn osrtp_retry_2xx_forgets_entry() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    remember(&mut r, &callee).await;
    let id = r
        .stack
        .dial("a", &format!("sip:n@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee).await;
    send(&callee, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(&callee).await;
    assert!(i2.contains("a=crypto"));
    ok200(&callee, from, &i2).await;
    state_of(&mut r, &id, CallState::Active).await;
    settle().await;
    assert!(mem(&r).is_empty(), "{:?}", mem(&r));
    r.stack.shutdown().await;
}

/// A config change while the retry INVITE is in flight: its late 2xx must not re-add.
#[tokio::test]
async fn config_change_during_retry_blocks_stale_commit() {
    let mut r = rig();
    account_optional(&r).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{caddr}"), DialOptions::default())
        .await
        .unwrap();
    let (i1, from) = next_invite(&callee).await;
    send(&callee, from, "488 Not Acceptable Here", &i1, true, "").await;
    let (i2, from) = next_invite(&callee).await;
    assert!(!i2.contains("a=crypto"));
    let mut acfg = peer("a");
    acfg.srtp = SrtpPolicy::Off;
    r.stack.update_account(acfg).await.unwrap();
    ok200(&callee, from, &i2).await;
    state_of(&mut r, &id, CallState::Active).await;
    settle().await;
    assert!(mem(&r).is_empty(), "stale commit re-added: {:?}", mem(&r));
    r.stack.shutdown().await;
}
