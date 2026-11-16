//! Reliable provisional responses as the UAS (RFC 3262): `Require: 100rel` + `RSeq`
//! on the 180, T1-doubling retransmission until the PRACK, PRACK matching (200 / 481), and no
//! 2xx while the PRACK is outstanding. Raw-UDP probe against a peer-mode account.

mod common;

use std::time::Duration;

use common::*;
use siple_config::PrackMode;
use siple_sip::{AnswerOptions, CallState, SipEvent};

/// Messages the probe receives within `ms` milliseconds.
async fn drain(raw: &Raw, ms: u64) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    let mut out = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return out;
        }
        let mut buf = vec![0u8; 8192];
        match tokio::time::timeout(left, raw.sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => out.push(String::from_utf8_lossy(&buf[..n]).into_owned()),
            _ => return out,
        }
    }
}

fn rseq_of(msg: &str) -> u32 {
    hdr(msg, "rseq")[0]
        .split(':')
        .nth(1)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn is_180(m: &str) -> bool {
    m.starts_with("SIP/2.0 180")
}

fn peer_with(id: &str, mode: PrackMode) -> siple_config::AccountConfig {
    let mut c = peer(id);
    c.prack = mode;
    c
}

async fn setup(mode: PrackMode) -> (Rig, Raw) {
    set_prack_t1_ms(50);
    let r = rig();
    let b = add(&r, peer_with("b", mode)).await;
    let raw = Raw::new(&b).await;
    (r, raw)
}

use siple_sip::set_prack_t1_ms;

#[tokio::test]
async fn required_by_uac_sends_reliable_180_and_holds_the_200_until_prack() {
    let (mut r, raw) = setup(PrackMode::Supported).await;
    raw.send(&raw.msg(
        "INVITE",
        None,
        1,
        "Supported: 100rel\r\nRequire: 100rel\r\n",
        &raw.sdp(),
    ))
    .await;
    let call = incoming(&mut r).await;
    let first = raw.response("CSeq: 1 INVITE").await;
    let first = if first.starts_with("SIP/2.0 100") {
        raw.response("CSeq: 1 INVITE").await
    } else {
        first
    };
    assert!(is_180(&first), "{first}");
    assert!(
        hdr(&first, "require")[0].to_lowercase().contains("100rel"),
        "{first}"
    );
    let rseq = rseq_of(&first);
    assert!(
        hdr(&first, "supported")[0].contains("100rel"),
        "advertised: {first}"
    );
    let tag = to_tag(&first);

    // The application answers at once; the 200 must wait for the PRACK, while the 180 is
    // retransmitted with the same RSeq (T1 = 50 ms: 100, 200, 400 ... ms).
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let during = drain(&raw, 900).await;
    assert!(
        during.iter().all(|m| is_180(m) && rseq_of(m) == rseq),
        "only identical reliable 180s expected before the PRACK: {during:#?}"
    );
    assert!(
        during.len() >= 2,
        "180 retransmitted at T1 doubling: {} copies",
        during.len()
    );
    assert_eq!(r.stack.call(&call.id).unwrap().state, CallState::Ringing);

    // PRACK -> 200 (PRACK), then the 200 (INVITE).
    raw.send(&raw.msg(
        "PRACK",
        Some(&tag),
        2,
        &format!("RAck: {rseq} 1 INVITE\r\n"),
        "",
    ))
    .await;
    let prack_ok = raw.final_response("CSeq: 2 PRACK").await;
    assert!(prack_ok.starts_with("SIP/2.0 200"), "{prack_ok}");
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    assert!(ok.contains("m=audio"), "{ok}");
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    state_of(&mut r, &call.id, CallState::Active).await;

    // No more 180 retransmissions after the PRACK.
    let rest = drain(&raw, 400).await;
    assert!(rest.iter().all(|m| !is_180(m)), "{rest:#?}");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn answer_after_prack_is_immediate_and_retransmitted_prack_is_ok() {
    let (mut r, raw) = setup(PrackMode::Supported).await;
    raw.send(&raw.msg("INVITE", None, 1, "Require: 100rel\r\n", &raw.sdp()))
        .await;
    let call = incoming(&mut r).await;
    let mut first = raw.response("CSeq: 1 INVITE").await;
    if first.starts_with("SIP/2.0 100") {
        first = raw.response("CSeq: 1 INVITE").await;
    }
    let (rseq, tag) = (rseq_of(&first), to_tag(&first));
    let prack = raw.msg(
        "PRACK",
        Some(&tag),
        2,
        &format!("RAck: {rseq} 1 INVITE\r\n"),
        "",
    );
    raw.send(&prack).await;
    assert!(
        raw.final_response("CSeq: 2 PRACK")
            .await
            .starts_with("SIP/2.0 200")
    );
    // Same PRACK again (retransmission): still 200.
    raw.send(&prack).await;
    assert!(
        raw.final_response("CSeq: 2 PRACK")
            .await
            .starts_with("SIP/2.0 200")
    );
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let ok = raw.final_response("CSeq: 1 INVITE").await;
    assert!(ok.starts_with("SIP/2.0 200"), "{ok}");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn unmatched_prack_gets_481_and_garbage_400() {
    let (mut r, raw) = setup(PrackMode::Supported).await;
    raw.send(&raw.msg("INVITE", None, 1, "Require: 100rel\r\n", &raw.sdp()))
        .await;
    incoming(&mut r).await;
    let mut first = raw.response("CSeq: 1 INVITE").await;
    if first.starts_with("SIP/2.0 100") {
        first = raw.response("CSeq: 1 INVITE").await;
    }
    let (rseq, tag) = (rseq_of(&first), to_tag(&first));
    raw.send(&raw.msg(
        "PRACK",
        Some(&tag),
        2,
        &format!("RAck: {} 1 INVITE\r\n", rseq + 1),
        "",
    ))
    .await;
    let r2 = raw.final_response("CSeq: 2 PRACK").await;
    assert!(r2.starts_with("SIP/2.0 481"), "wrong RSeq: {r2}");
    raw.send(&raw.msg("PRACK", Some(&tag), 3, "RAck: bogus\r\n", ""))
        .await;
    let r3 = raw.final_response("CSeq: 3 PRACK").await;
    assert!(r3.starts_with("SIP/2.0 400"), "{r3}");
    raw.send(&raw.msg(
        "PRACK",
        Some(&tag),
        4,
        &format!("RAck: {rseq} 9 INVITE\r\n"),
        "",
    ))
    .await;
    let r4 = raw.final_response("CSeq: 4 PRACK").await;
    assert!(r4.starts_with("SIP/2.0 481"), "wrong CSeq: {r4}");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn no_prack_in_64_t1_rejects_with_504() {
    set_prack_t1_ms(50); // 64 * 50 ms = 3.2 s
    let mut r = rig();
    let b = add(&r, peer_with("b", PrackMode::Supported)).await;
    let raw = Raw::new(&b).await;
    raw.send(&raw.msg("INVITE", None, 1, "Require: 100rel\r\n", &raw.sdp()))
        .await;
    let call = incoming(&mut r).await;
    let fin = raw.final_response("CSeq: 1 INVITE").await;
    assert!(fin.starts_with("SIP/2.0 504"), "{fin}");
    let info = state_of(&mut r, &call.id, CallState::Ended).await;
    assert_eq!(info.end.as_ref().map(|e| e.code), Some(504));
    set_prack_t1_ms(50);
    r.stack.shutdown().await;
}

#[tokio::test]
async fn reject_while_prack_outstanding_stops_retransmissions() {
    let (mut r, raw) = setup(PrackMode::Supported).await;
    raw.send(&raw.msg("INVITE", None, 1, "Require: 100rel\r\n", &raw.sdp()))
        .await;
    let call = incoming(&mut r).await;
    let mut first = raw.response("CSeq: 1 INVITE").await;
    if first.starts_with("SIP/2.0 100") {
        first = raw.response("CSeq: 1 INVITE").await;
    }
    let tag = to_tag(&first);
    r.stack.reject(&call.id, Some(486), None).await.unwrap();
    let fin = raw.final_response("CSeq: 1 INVITE").await;
    assert!(fin.starts_with("SIP/2.0 486"), "{fin}");
    raw.send(&raw.msg("ACK", Some(&tag), 1, "", "")).await;
    let rest = drain(&raw, 500).await;
    assert!(rest.iter().all(|m| !is_180(m)), "{rest:#?}");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn mode_matrix_decides_reliability() {
    set_prack_t1_ms(50);
    // (mode, UAC headers, expects a reliable 180, expects 420)
    let cases = [
        (PrackMode::Supported, "", false, false),
        (PrackMode::Supported, "Supported: 100rel\r\n", false, false),
        (PrackMode::Supported, "Require: 100rel\r\n", true, false),
        (PrackMode::Required, "", false, false),
        (PrackMode::Required, "Supported: 100rel\r\n", true, false),
        (PrackMode::Disabled, "Supported: 100rel\r\n", false, false),
        (PrackMode::Disabled, "Require: 100rel\r\n", false, true),
    ];
    for (mode, hdrs, reliable, bad_ext) in cases {
        let mut r = rig();
        let b = add(&r, peer_with("b", mode)).await;
        let raw = Raw::new(&b).await;
        raw.send(&raw.msg("INVITE", None, 1, hdrs, &raw.sdp()))
            .await;
        if bad_ext {
            let fin = raw.final_response("CSeq: 1 INVITE").await;
            assert!(
                fin.starts_with("SIP/2.0 420") && fin.contains("100rel"),
                "{mode:?} {hdrs:?}: {fin}"
            );
        } else {
            incoming(&mut r).await;
            let mut m = raw.response("CSeq: 1 INVITE").await;
            if m.starts_with("SIP/2.0 100") {
                m = raw.response("CSeq: 1 INVITE").await;
            }
            assert!(is_180(&m), "{mode:?} {hdrs:?}: {m}");
            assert_eq!(
                m.to_lowercase().contains("rseq:"),
                reliable,
                "{mode:?} {hdrs:?}: {m}"
            );
            assert_eq!(
                hdr(&m, "require")
                    .iter()
                    .any(|h| h.to_lowercase().contains("100rel")),
                reliable,
                "{mode:?} {hdrs:?}: {m}"
            );
            // Advertised unless disabled.
            assert_eq!(
                hdr(&m, "supported")[0].contains("100rel"),
                mode != PrackMode::Disabled,
                "{mode:?}: {m}"
            );
        }
        r.stack.shutdown().await;
    }
}

#[allow(dead_code)]
fn _uses(_: SipEvent) {}
