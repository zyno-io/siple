//! A call to a host that is not the account's provider, and that challenges, ends with the
//! refusal visible through the control API (what the GUI and agents see).
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::{acct, client, free_udp_port, start_app};
use serde_json::json;
use tokio::net::UdpSocket;

fn header(req: &str, name: &str) -> String {
    req.lines()
        .find(|l| l.to_ascii_lowercase().starts_with(&format!("{name}:")))
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn foreign_challenge_refusal_is_reported_to_control_clients() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    // The provider is another port on the same IP (never contacted).
    let provider = free_udp_port();
    let text = acct("alice", free_udp_port(), "").replace(
        "domain = \"127.0.0.1\"",
        &format!("domain = \"provider.test\"\nregistrar = \"127.0.0.1:{provider}\""),
    );
    std::fs::write(&path, text).unwrap();
    let app = start_app(&path).await;
    let mut c = client(&app, "ctl").await;

    // A foreign host that challenges every INVITE and counts the ones carrying credentials.
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let foreign = sock.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        let mut with_auth = 0u32;
        let end = tokio::time::Instant::now() + Duration::from_secs(4);
        while let Ok(Ok((n, src))) = tokio::time::timeout_at(end, sock.recv_from(&mut buf)).await {
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            if !req.starts_with("INVITE ") {
                continue;
            }
            if req.to_ascii_lowercase().contains("\r\nauthorization:") {
                with_auth += 1;
            }
            let resp = format!(
                "SIP/2.0 401 Unauthorized\r\n{}\r\n{}\r\n{};tag=x1\r\n{}\r\n{}\r\n\
                 WWW-Authenticate: Digest realm=\"rogue\", nonce=\"0123456789abcdef\", algorithm=MD5\r\n\
                 Content-Length: 0\r\n\r\n",
                req.lines()
                    .filter(|l| l.to_ascii_lowercase().starts_with("via:"))
                    .collect::<Vec<_>>()
                    .join("\r\n"),
                header(&req, "from"),
                header(&req, "to"),
                header(&req, "call-id"),
                header(&req, "cseq"),
            );
            let _ = sock.send_to(resp.as_bytes(), src).await;
        }
        with_auth
    });

    let d = c
        .call(
            "call.dial",
            json!({"account":"alice","target":format!("sip:x@{foreign}"),"source":"silence","sink":"null"}),
        )
        .await
        .unwrap();
    let ended = c
        .call(
            "call.wait_for",
            json!({"call_id": d["call_id"], "state":"ended", "timeout_ms": 10_000}),
        )
        .await
        .unwrap();
    let end = &ended["call"]["end"];
    assert_eq!(end["code"], 401, "{ended}");
    let reason = end["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("Authentication refused: ")
            && reason.contains(" is not your provider (set auth_foreign = true"),
        "{reason}"
    );
    assert!(!reason.contains("0123456789abcdef") && !reason.contains("pw"));
    assert_eq!(end["cause"], "failed");
    assert_eq!(server.await.unwrap(), 0, "credentials were sent");
}
