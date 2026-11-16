//! Vendor profiles end to end: a config parsed from TOML with `profile = "..."` shapes the
//! messages the stack sends (User-Agent, Supported, DND code) unless the file sets the option.

mod common;

use common::*;
use siple_config::Config;
use tokio::net::UdpSocket;

fn account_from(toml: &str) -> siple_config::AccountConfig {
    let cfg = Config::parse(&format!(
        "[[account]]\nid = \"p\"\nusername = \"p\"\ndomain = \"peer.test\"\nregister = false\nbind_address = \"127.0.0.1\"\ntls_verify = false\nsip_trace = false\n{toml}"
    ))
    .unwrap();
    assert_eq!(cfg.validate(), Ok(()));
    cfg.accounts.into_iter().next().unwrap()
}

fn value(m: &str, name: &str) -> String {
    hdr(m, name)
        .first()
        .map(|l| l.split_once(':').unwrap().1.trim().to_string())
        .unwrap_or_default()
}

async fn invite_seen_by_probe(cfg: siple_config::AccountConfig) -> String {
    let r = rig();
    add(&r, cfg).await;
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let _ = r
        .stack
        .dial("p", &format!("sip:probe@{addr}"), Default::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let msg = String::from_utf8_lossy(&buf[..n]).into_owned();
    r.stack.shutdown().await;
    msg
}

#[tokio::test]
async fn yealink_profile_shapes_the_invite() {
    let inv = invite_seen_by_probe(account_from("profile = \"yealink\"\n")).await;
    assert_eq!(
        value(&inv, "user-agent"),
        "Yealink SIP-T54W 96.86.0.70",
        "{inv}"
    );
    // Yealink bundle: prack disabled, no session timers.
    let sup = value(&inv, "supported");
    assert!(!sup.contains("100rel") && !sup.contains("timer"), "{inv}");
    assert!(hdr(&inv, "session-expires").is_empty(), "{inv}");
}

#[tokio::test]
async fn explicit_options_beat_the_profile() {
    let inv = invite_seen_by_probe(account_from(
        "profile = \"yealink\"\nuser_agent = \"Mine/1\"\nsession_timers = true\nprack = \"supported\"\n",
    ))
    .await;
    assert_eq!(value(&inv, "user-agent"), "Mine/1", "{inv}");
    let sup = value(&inv, "supported");
    assert!(sup.contains("100rel") && sup.contains("timer"), "{inv}");
    assert!(!hdr(&inv, "session-expires").is_empty(), "{inv}");
}

#[tokio::test]
async fn profile_dnd_code_is_used_for_rejections() {
    let mut cfg = account_from("profile = \"cisco\"\ndnd = true\n");
    assert_eq!(cfg.dnd_reject_code, 480);
    cfg.id = "b".into();
    let r = rig();
    let b = add(&r, cfg).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "", &raw.sdp())).await;
    let resp = raw.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 480"), "{resp}");
    r.stack.shutdown().await;
}
