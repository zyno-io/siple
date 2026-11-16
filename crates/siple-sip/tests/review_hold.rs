//! Regressions from the hold / transfer review.
mod common;

use std::time::Duration;

use common::*;
use siple_config::HoldStyle;
use siple_sip::{AnswerOptions, CallState, DialOptions, SipEvent};
use tokio::net::UdpSocket;

fn sdp_dir_of(m: &str) -> String {
    m.lines()
        .find(|l| {
            matches!(
                l.trim(),
                "a=sendrecv" | "a=sendonly" | "a=recvonly" | "a=inactive"
            )
        })
        .unwrap_or("?")
        .trim()
        .to_string()
}

/// `Event: refer` NOTIFY with a sipfrag body from the raw probe.
fn refer_notify(raw: &Raw, tag: &str, cseq: u32, event: &str, state: &str, frag: &str) -> String {
    raw.msg(
        "NOTIFY",
        Some(tag),
        cseq,
        &format!(
            "Event: {event}\r\nSubscription-State: {state}\r\nContent-Type: message/sipfrag\r\n"
        ),
        frag,
    )
    .replace("Content-Type: application/sdp\r\n", "")
}

/// H1: a hold whose caller gave up (the 6 s bound in `hold_for_transfer`) still finishes; when
/// the re-INVITE is finally refused the session direction is rolled back.
#[tokio::test]
async fn dropped_hold_is_rolled_back_when_it_fails() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, c_call, tag) = established(&mut r, &c).await;
    let stack = r.stack.clone();
    let id = c_call.clone();
    let t = tokio::spawn(async move { stack.transfer_blind(&id, "sip:x@127.0.0.1:9").await });
    // Ignore the hold re-INVITE (and retransmissions) until the REFER shows up.
    let (hold_invite, refer) = {
        let mut inv = None;
        loop {
            let m = raw.recv_any(12).await.expect("message from the stack");
            if m.starts_with("INVITE ") && inv.is_none() {
                assert_eq!(sdp_dir_of(&m), "a=sendonly");
                inv = Some(m);
            } else if m.starts_with("REFER ") {
                break (inv.expect("hold re-INVITE first"), m);
            }
        }
    };
    raw.reply(&refer, "403 Forbidden", None).await;
    let _ = t.await.unwrap();
    // The abandoned hold is still outstanding; the remote refuses it now.
    raw.reply(&hold_invite, "488 Not Acceptable Here", None)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let info = r.stack.call(&c_call).unwrap();
    assert_eq!(info.state, CallState::Active);
    assert!(!info.hold.local, "{:?}", info.hold);
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendrecv", 2)))
        .await;
    let ok = raw.final_response("CSeq: 2 INVITE").await;
    assert_eq!(sdp_dir_of(&ok), "a=sendrecv", "{ok}");
    r.stack.shutdown().await;
}

/// H1: an abandoned hold that the remote accepts still completes (Held, hold flag set).
#[tokio::test]
async fn dropped_hold_completes_when_it_succeeds() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, c_call, _tag) = established(&mut r, &c).await;
    let stack = r.stack.clone();
    let id = c_call.clone();
    let h = tokio::spawn(async move { stack.hold(&id).await });
    let inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    h.abort();
    let _ = h.await;
    raw.reply(&inv, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    let info = updated(&mut r, &c_call, |i| i.hold.local).await;
    assert_eq!(info.state, CallState::Held);
    r.stack.shutdown().await;
}

/// H5: a failed inbound REFER leaves a call the user held on hold.
#[tokio::test]
async fn failed_inbound_refer_keeps_user_hold() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let b = add(&r, peer("b")).await;
    r.stack.set_dnd("b", true).await.unwrap();
    let (raw, c_call, tag) = established(&mut r, &c).await;
    let stack = r.stack.clone();
    let id = c_call.clone();
    let h = tokio::spawn(async move { stack.hold(&id).await });
    let inv = raw.request("INVITE", 5).await.expect("hold re-INVITE");
    raw.reply(&inv, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
        .await;
    h.await.unwrap().unwrap();
    assert!(r.stack.call(&c_call).unwrap().hold.local);

    let extra = format!("Refer-To: <sip:b@{b}>\r\n");
    raw.send(&raw.msg("REFER", Some(&tag), 2, &extra, "")).await;
    let mut saw_final = false;
    let mut reinvites = 0;
    while let Some(m) = raw.recv_any(4).await {
        if m.starts_with("NOTIFY ") {
            if m.contains("SIP/2.0 486") || m.contains("SIP/2.0 4") {
                saw_final = true;
            }
            raw.ok_to(&m).await;
        } else if m.starts_with("INVITE ") {
            reinvites += 1;
            raw.reply(&m, "200 OK", Some(&raw.sdp_dir("sendrecv", 3)))
                .await;
        }
    }
    let info = r.stack.call(&c_call).unwrap();
    assert!(saw_final, "no failure NOTIFY");
    assert_eq!(reinvites, 0, "the user's hold was undone");
    assert!(info.hold.local, "{:?}", info.hold);
    r.stack.shutdown().await;
}

/// H4: semi-attended transfer refers to the dialled target, not the asserted identity.
#[tokio::test]
async fn semi_attended_uses_dialled_target() {
    let mut r = rig();
    add(&r, peer("a")).await;
    let b = add(&r, peer("b")).await;
    let a_b = r
        .stack
        .dial("a", &format!("sip:b@{b}"), Default::default())
        .await
        .unwrap();
    let bc = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&bc.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a_b, CallState::Active).await;
    let callee = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let caddr = callee.local_addr().unwrap();
    let dialled = format!("sip:600@{caddr}");
    let a_c = r
        .stack
        .dial("a", &dialled, DialOptions::default())
        .await
        .unwrap();
    let mut buf = vec![0u8; 8192];
    let invite = loop {
        let (n, _) = callee.recv_from(&mut buf).await.unwrap();
        let m = String::from_utf8_lossy(&buf[..n]).into_owned();
        if m.starts_with("INVITE") {
            break m;
        }
    };
    let base = format!(
        "{}\r\n{}\r\n{};tag=callee1\r\n{}\r\n{}\r\nContact: <sip:x@{caddr}>\r\n",
        hdr(&invite, "via").join("\r\n"),
        hdr(&invite, "from")[0],
        hdr(&invite, "to")[0],
        hdr(&invite, "call-id")[0],
        hdr(&invite, "cseq")[0]
    );
    let ringing = format!(
        "SIP/2.0 180 Ringing\r\n{base}P-Asserted-Identity: \"Member\" <sip:2001@10.9.9.9>\r\nContent-Length: 0\r\n\r\n"
    );
    let from = r.stack.account_status("a").unwrap().local_addr.unwrap();
    callee
        .send_to(ringing.as_bytes(), from.as_str())
        .await
        .unwrap();
    state_of(&mut r, &a_c, CallState::Early).await;
    r.stack.transfer_attended(&a_b, &a_c).await.unwrap();
    let ev = until(
        &mut r,
        "ReferReceived",
        |e| matches!(e, SipEvent::ReferReceived { call_id, .. } if *call_id == bc.id),
    )
    .await;
    let SipEvent::ReferReceived { refer_to, .. } = ev else {
        unreachable!()
    };
    assert!(
        refer_to.contains(&dialled),
        "Refer-To {refer_to} != dialled {dialled}"
    );
    assert!(!refer_to.contains("2001"), "{refer_to}");
    r.stack.shutdown().await;
}

/// H8: with `hold_style = inactive` the far end's mirrored `inactive` answer is not a hold.
#[tokio::test]
async fn inactive_hold_does_not_set_remote_flag() {
    let mut r = rig();
    let mut acfg = peer("a");
    acfg.hold_style = HoldStyle::Inactive;
    add(&r, acfg).await;
    let b = add(&r, peer("b")).await;
    let a = r
        .stack
        .dial("a", &format!("sip:b@{b}"), DialOptions::default())
        .await
        .unwrap();
    let bc = incoming_on(&mut r, "b").await;
    r.stack
        .answer(&bc.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a, CallState::Active).await;
    r.stack.hold(&a).await.unwrap();
    let ai = r.stack.call(&a).unwrap();
    assert!(ai.hold.local && !ai.hold.remote, "{:?}", ai.hold);
    assert_eq!(ai.state, CallState::Held);
    r.stack.shutdown().await;
}

/// H7: the transferee BYEs right before its final NOTIFY: the NOTIFY is answered 200 and the
/// transfer is reported as successful.
#[tokio::test]
async fn final_refer_notify_after_bye_counts() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, c_call, tag) = established(&mut r, &c).await;
    let stack = r.stack.clone();
    let id = c_call.clone();
    let t = tokio::spawn(async move { stack.transfer_blind(&id, "sip:x@127.0.0.1:9").await });
    loop {
        let m = raw.recv_any(8).await.expect("msg");
        if m.starts_with("INVITE ") {
            raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
                .await;
        } else if m.starts_with("REFER ") {
            raw.reply(&m, "202 Accepted", None).await;
            break;
        }
    }
    t.await.unwrap().unwrap();
    raw.send(&raw.msg("BYE", Some(&tag), 3, "", "")).await;
    let _ = raw.final_response("CSeq: 3 BYE").await;
    raw.send(&refer_notify(
        &raw,
        &tag,
        4,
        "refer",
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let resp = raw.final_response("CSeq: 4 NOTIFY").await;
    assert!(resp.starts_with("SIP/2.0 200"), "{resp}");
    let ev = until(&mut r, "final progress", |e| {
        matches!(e, SipEvent::TransferProgress { call_id, is_final: true, .. } if *call_id == c_call)
    })
    .await;
    assert!(
        matches!(ev, SipEvent::TransferProgress { code: 200, .. }),
        "{ev:?}"
    );
    r.stack.shutdown().await;
}

/// H6: a transfer_blind future that is dropped mid-REFER neither blocks later transfers nor
/// loses the result.
#[tokio::test]
async fn dropped_transfer_future_still_completes() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, c_call, tag) = established(&mut r, &c).await;
    let stack = r.stack.clone();
    let id = c_call.clone();
    let first = tokio::spawn(async move { stack.transfer_blind(&id, "sip:x@127.0.0.1:9").await });
    let refer = loop {
        let m = raw.recv_any(12).await.expect("msg");
        if m.starts_with("INVITE ") {
            raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
                .await;
        } else if m.starts_with("REFER ") {
            break m;
        }
    };
    first.abort();
    let _ = first.await;
    // The REFER is accepted late and succeeds: the result is still reported.
    raw.reply(&refer, "202 Accepted", None).await;
    raw.send(&refer_notify(
        &raw,
        &tag,
        3,
        "refer",
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let ev = until(&mut r, "final progress", |e| {
        matches!(e, SipEvent::TransferProgress { call_id, is_final: true, .. } if *call_id == c_call)
    })
    .await;
    assert!(
        matches!(ev, SipEvent::TransferProgress { code: 200, .. }),
        "{ev:?}"
    );
    r.stack.shutdown().await;
}

/// H6: a REFER refused outright does not block a retry.
#[tokio::test]
async fn refused_refer_allows_retry() {
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    let (raw, c_call, _tag) = established(&mut r, &c).await;
    let stack = r.stack.clone();
    let id = c_call.clone();
    let probe = tokio::spawn(async move {
        let mut refers = 0;
        while let Some(m) = raw.recv_any(6).await {
            if m.starts_with("INVITE ") {
                raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
                    .await;
            } else if m.starts_with("REFER ") {
                refers += 1;
                raw.reply(&m, "403 Forbidden", None).await;
            }
        }
        refers
    });
    assert!(
        stack
            .transfer_blind(&id, "sip:x@127.0.0.1:9")
            .await
            .is_err()
    );
    assert!(
        stack
            .transfer_blind(&id, "sip:y@127.0.0.1:9")
            .await
            .is_err()
    );
    r.stack.shutdown().await;
    assert_eq!(probe.await.unwrap(), 2);
}

/// H9: we hold a call the remote holds (inactive); when the remote resumes we are back to
/// `sendonly`, not stuck at `inactive`.
#[tokio::test]
async fn hold_direction_follows_remote_resume() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b_addr).await;
    raw.send(&raw.msg("INVITE", Some(&tag), 2, "", &raw.sdp_dir("sendonly", 2)))
        .await;
    let _ = raw.final_response("CSeq: 2 INVITE").await;
    raw.send(&raw.msg("ACK", Some(&tag), 2, "", "")).await;
    state_of(&mut r, &id, CallState::RemoteHeld).await;
    let stack = r.stack.clone();
    let id2 = id.clone();
    let h = tokio::spawn(async move { stack.hold(&id2).await });
    let req = raw.request("INVITE", 5).await.expect("re-INVITE");
    assert_eq!(sdp_dir_of(&req), "a=inactive");
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("inactive", 3)))
        .await;
    h.await.unwrap().unwrap();
    assert!(r.stack.call(&id).unwrap().hold.local);
    // Remote resumes.
    raw.send(&raw.msg("INVITE", Some(&tag), 3, "", &raw.sdp_dir("sendrecv", 4)))
        .await;
    let ok = raw.final_response("CSeq: 3 INVITE").await;
    raw.send(&raw.msg("ACK", Some(&tag), 3, "", "")).await;
    assert_eq!(sdp_dir_of(&ok), "a=sendonly", "{ok}");
    let c = r.stack.call(&id).unwrap();
    assert!(c.hold.local && !c.hold.remote, "{:?}", c.hold);
    r.stack.shutdown().await;
}

/// Replaces naming a dialog that already ended: 603, not 481.
#[tokio::test]
async fn replaces_of_ended_dialog_is_603() {
    let mut r = rig();
    let b_addr = add(&r, peer("b")).await;
    let (raw, id, tag) = established(&mut r, &b_addr).await;
    r.stack.hangup(&id).await.unwrap();
    state_of(&mut r, &id, CallState::Ended).await;
    let sip_call_id = r.stack.call(&id).unwrap().sip_call_id;
    let raw2 = Raw::new(&b_addr).await;
    let extra = format!("Replaces: {sip_call_id};to-tag={tag};from-tag=ptag\r\n");
    raw2.send(&raw2.msg("INVITE", None, 1, &extra, &raw2.sdp()))
        .await;
    let resp = raw2.final_response("CSeq: 1 INVITE").await;
    assert!(resp.starts_with("SIP/2.0 603"), "{resp}");
    drop(raw);
    r.stack.shutdown().await;
}
