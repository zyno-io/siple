//! H3: a hang-up while a 3xx is being followed must not leave a ghost call. Own binary: the
//! redirect-hop latency hook is process-wide.
mod common;

use std::time::Duration;

use common::*;
use siple_sip::DialOptions;
use tokio::net::UdpSocket;

#[tokio::test]
async fn hangup_during_redirect_follow_sends_nothing() {
    siple_sip::set_redirect_hop_delay_ms(1000);
    let r = rig();
    add(&r, peer("a")).await;
    let c1 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let c1a = c1.local_addr().unwrap();
    let c2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let c2a = c2.local_addr().unwrap();
    let id = r
        .stack
        .dial("a", &format!("sip:x@{c1a}"), DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, from) = c1.recv_from(&mut buf).await.unwrap();
    let inv = String::from_utf8_lossy(&buf[..n]).into_owned();
    let base = format!(
        "{}\r\n{}\r\n{};tag=c1\r\n{}\r\n{}\r\n",
        hdr(&inv, "via").join("\r\n"),
        hdr(&inv, "from")[0],
        hdr(&inv, "to")[0],
        hdr(&inv, "call-id")[0],
        hdr(&inv, "cseq")[0]
    );
    let moved = format!(
        "SIP/2.0 302 Moved Temporarily\r\n{base}Contact: <sip:y@{c2a}>\r\nContent-Length: 0\r\n\r\n"
    );
    c1.send_to(moved.as_bytes(), from).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    r.stack.hangup(&id).await.unwrap();
    // The redirect target must never see an INVITE.
    let got = tokio::time::timeout(Duration::from_secs(3), c2.recv_from(&mut buf)).await;
    assert!(got.is_err(), "ghost INVITE to the redirect target");
    r.stack.shutdown().await;
}
