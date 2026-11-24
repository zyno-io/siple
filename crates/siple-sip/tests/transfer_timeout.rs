//! H2: the wait for `refer` NOTIFYs restarts on progress and survives a reported timeout.
//! Own binary: the shortened timeout is process-wide.
mod common;

use common::*;
use siple_sip::SipEvent;

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

/// Starts a transfer on an established inbound call; returns the CSeq of the accepted REFER.
async fn start(raw: &Raw, r: &Rig, call: &str) -> u32 {
    let stack = r.stack.clone();
    let id = call.to_string();
    let t = tokio::spawn(async move { stack.transfer_blind(&id, "sip:x@127.0.0.1:9").await });
    let refer = loop {
        let m = raw.recv_any(10).await.expect("msg");
        if m.starts_with("INVITE ") {
            raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 2)))
                .await;
        } else if m.starts_with("REFER ") {
            break m;
        }
    };
    raw.reply(&refer, "202 Accepted", None).await;
    t.await.unwrap().unwrap();
    hdr(&refer, "cseq")[0]
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

fn progress(e: &SipEvent, call: &str) -> Option<(u16, bool)> {
    match e {
        SipEvent::TransferProgress {
            call_id,
            code,
            is_final,
            ..
        } if call_id == call => Some((*code, *is_final)),
        _ => None,
    }
}

#[tokio::test]
async fn progress_notifies_extend_the_timeout_and_late_success_counts() {
    siple_sip::set_transfer_timeout_ms(1000);
    let mut r = rig();
    let c = add(&r, peer("c")).await;
    // Case 1: progress every 700 ms for 2.8 s: no timeout, then a final 200.
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start(&raw, &r, &call).await;
    let ev = format!("refer;id={cseq}");
    for (i, frag) in ["SIP/2.0 100 Trying\r\n", "SIP/2.0 180 Ringing\r\n"]
        .iter()
        .cycle()
        .take(4)
        .enumerate()
    {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        raw.send(&refer_notify(
            &raw,
            &tag,
            10 + i as u32,
            &ev,
            "active;expires=60",
            frag,
        ))
        .await;
        raw.final_response(&format!("CSeq: {} NOTIFY", 10 + i))
            .await;
    }
    raw.send(&refer_notify(
        &raw,
        &tag,
        20,
        &ev,
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let _ = raw.final_response("CSeq: 20 NOTIFY").await;
    until(&mut r, "final 200", |e| {
        progress(e, &call) == Some((200, true))
    })
    .await;
    assert!(
        !r.buf
            .iter()
            .any(|e| progress(e, &call) == Some((408, true))),
        "timed out although progress kept arriving"
    );
    state_of(&mut r, &call, siple_sip::CallState::Ended).await;
    r.buf.clear();

    // Case 2: silence -> 408; a late final 200 is still honored; NOTIFYs of an older REFER
    // (lower id) are ignored.
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start(&raw, &r, &call).await;
    until(&mut r, "timeout", |e| {
        progress(e, &call) == Some((408, true))
    })
    .await;
    raw.send(&refer_notify(
        &raw,
        &tag,
        30,
        &format!("refer;id={}", cseq - 1),
        "active;expires=60",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let _ = raw.final_response("CSeq: 30 NOTIFY").await;
    raw.send(&refer_notify(
        &raw,
        &tag,
        31,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let _ = raw.final_response("CSeq: 31 NOTIFY").await;
    until(&mut r, "late final 200", |e| {
        progress(e, &call) == Some((200, true))
    })
    .await;
    state_of(&mut r, &call, siple_sip::CallState::Ended).await;
    r.stack.shutdown().await;
}

async fn wait_phase(
    r: &Rig,
    call: &str,
    phase: siple_sip::TransferPhase,
) -> siple_sip::TransferSession {
    let sid = r
        .stack
        .call(call)
        .unwrap()
        .transfer
        .expect("session")
        .session_id;
    for _ in 0..500 {
        let s = r.stack.transfer(&sid).unwrap();
        if s.phase == phase {
            return s;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!(
        "session {sid} never reached {phase:?}: {:?}",
        r.stack.transfer(&sid)
    );
}

/// 32 s (here 1 s) without a final NOTIFY: UNCONFIRMED, with no automatic action (the call stays
/// held, nothing is sent); a late final NOTIFY then decides: 2xx -> TRANSFERRED, >= 300 ->
/// FAILED (and the auto-resume of a blind transfer).
#[tokio::test]
async fn unconfirmed_takes_no_action_and_a_late_final_notify_decides() {
    use siple_sip::{CallState, EndCause, TransferPhase};
    siple_sip::set_transfer_timeout_ms(1000);
    let mut r = rig();
    let c = add(&r, peer("c")).await;

    // Late 200 -> TRANSFERRED.
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start(&raw, &r, &call).await;
    let s = wait_phase(&r, &call, TransferPhase::Unconfirmed).await;
    assert_eq!(s.kind, siple_sip::TransferKind::Blind);
    assert!(s.ended_at.is_none() && s.last_error.is_none());
    assert_eq!(r.stack.call(&call).unwrap().state, CallState::Held);
    assert!(
        raw.request("INVITE", 1).await.is_none(),
        "no automatic resume after the timeout"
    );
    assert_eq!(
        r.stack.transfer(&s.id).unwrap().phase,
        TransferPhase::Unconfirmed
    );
    raw.send(&refer_notify(
        &raw,
        &tag,
        30,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 200 OK\r\n",
    ))
    .await;
    let _ = raw.final_response("CSeq: 30 NOTIFY").await;
    let done = wait_phase(&r, &call, TransferPhase::Transferred).await;
    assert_eq!(done.last_notify.as_ref().map(|n| n.code), Some(200));
    let end = state_of(&mut r, &call, CallState::Ended).await.end.unwrap();
    assert_eq!(end.cause, EndCause::Transferred);

    // Late 486 -> FAILED; the blind transfer held the call, so it is resumed now.
    let (raw, call, tag) = established(&mut r, &c).await;
    let cseq = start(&raw, &r, &call).await;
    wait_phase(&r, &call, TransferPhase::Unconfirmed).await;
    raw.send(&refer_notify(
        &raw,
        &tag,
        31,
        &format!("refer;id={cseq}"),
        "terminated;reason=noresource",
        "SIP/2.0 486 Busy Here\r\n",
    ))
    .await;
    let _ = raw.final_response("CSeq: 31 NOTIFY").await;
    let req = raw
        .request("INVITE", 8)
        .await
        .expect("auto-resume re-INVITE");
    raw.reply(&req, "200 OK", Some(&raw.sdp_dir("sendrecv", 4)))
        .await;
    let failed = wait_phase(&r, &call, TransferPhase::Failed).await;
    assert_eq!(failed.last_error.as_ref().map(|e| e.code), Some(486));
    state_of(&mut r, &call, CallState::Active).await;
    r.stack.shutdown().await;
}

/// Two raw calls on a non-device account `c` (so answering the second holds nothing), linked
/// into a consult session. Returns `(stack calls, probes with tags)`.
async fn two_raw_calls(
    r: &mut Rig,
    on: siple_config::AttendedReferOn,
) -> (
    (String, String),
    (std::sync::Arc<Raw>, String),
    (std::sync::Arc<Raw>, String),
) {
    let mut cfg = peer("c");
    cfg.audio_source = siple_config::AudioSource::Silence;
    cfg.audio_sink = siple_config::AudioSink::Null;
    cfg.attended_refer_on = on;
    let c = add(r, cfg).await;
    let (raw1, a, tag1) = established(r, &c).await;
    let (raw2, b, tag2) = established(r, &c).await;
    (
        (a, b),
        (std::sync::Arc::new(raw1), tag1),
        (std::sync::Arc::new(raw2), tag2),
    )
}

/// Answers re-INVITEs (200) and REFERs (202, no NOTIFY ever) for `secs`; resolves to the number
/// of REFERs seen.
fn answer_refers(raw: std::sync::Arc<Raw>, secs: u64) -> tokio::task::JoinHandle<usize> {
    tokio::spawn(async move {
        let end = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        let (mut refers, mut invites) = (0usize, 0u32);
        while tokio::time::Instant::now() < end {
            let Some(m) = raw.recv_any(1).await else {
                continue;
            };
            if m.starts_with("INVITE ") {
                invites += 1;
                raw.reply(&m, "200 OK", Some(&raw.sdp_dir("recvonly", 1 + invites)))
                    .await;
            } else if m.starts_with("REFER ") {
                refers += 1;
                raw.reply(&m, "202 Accepted", None).await;
            }
        }
        refers
    })
}

async fn wait_session(r: &Rig, sid: &str, phase: siple_sip::TransferPhase) {
    for _ in 0..200 {
        if r.stack.transfer(sid).unwrap().phase == phase {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{sid} never reached {phase:?}: {:?}", r.stack.transfer(sid));
}

/// The legacy `transfer_attended` retried after UNCONFIRMED supersedes the unconfirmed session
/// and sends a new REFER (it used to return the old session and do nothing).
#[tokio::test]
async fn legacy_attended_retry_after_unconfirmed_starts_a_new_session() {
    use siple_sip::TransferPhase;
    siple_sip::set_transfer_timeout_ms(1000);
    let mut r = rig();
    let ((a, b), (raw1, _), (_raw2, _)) =
        two_raw_calls(&mut r, siple_config::AttendedReferOn::Original).await;
    let server = answer_refers(raw1, 8);
    let first = r.stack.transfer_attended(&a, &b).await.unwrap();
    wait_session(&r, &first.id, TransferPhase::Unconfirmed).await;
    let second = r.stack.transfer_attended(&a, &b).await.unwrap();
    assert_ne!(first.id, second.id, "a new session");
    assert_eq!(
        r.stack.transfer(&first.id).unwrap().phase,
        TransferPhase::Abandoned
    );
    assert_eq!(
        r.stack
            .transfer(&first.id)
            .unwrap()
            .last_error
            .unwrap()
            .stage,
        "superseded"
    );
    assert!(second.phase.refer_outstanding(), "{second:?}");
    r.stack.shutdown().await;
    assert_eq!(server.await.unwrap(), 2, "a second REFER was sent");
}

/// With `attended_refer_on = "consult"` the REFER travels on the consult call; when the original
/// ends while the session is UNCONFIRMED the session ends too (it does not outlive A).
#[tokio::test]
async fn unconfirmed_session_ends_when_the_original_ends_refer_on_consult() {
    use siple_sip::TransferPhase;
    siple_sip::set_transfer_timeout_ms(1000);
    let mut r = rig();
    let ((a, b), (raw1, tag1), (raw2, _)) =
        two_raw_calls(&mut r, siple_config::AttendedReferOn::Consult).await;
    let server1 = answer_refers(raw1.clone(), 6);
    let server2 = answer_refers(raw2, 6);
    let s = r.stack.consult_link(&a, &b).await.unwrap();
    r.stack
        .transfer_complete(&s.id, siple_sip::CompleteMode::Attended)
        .await
        .unwrap();
    assert_eq!(
        r.stack.transfer(&s.id).unwrap().refer_call.as_deref(),
        Some(b.as_str())
    );
    wait_session(&r, &s.id, TransferPhase::Unconfirmed).await;
    raw1.send(&raw1.msg("BYE", Some(&tag1), 40, "", "")).await;
    wait_session(&r, &s.id, TransferPhase::Abandoned).await;
    r.stack.shutdown().await;
    let _ = (server1.await, server2.await);
}

/// REFER on the consult call, the original ends before the timeout: the session goes straight to
/// ABANDONED, never UNCONFIRMED.
#[tokio::test]
async fn timeout_after_the_original_ended_abandons_instead_of_unconfirmed() {
    use siple_sip::TransferPhase;
    siple_sip::set_transfer_timeout_ms(1000);
    let mut r = rig();
    let ((a, b), (raw1, tag1), (raw2, _)) =
        two_raw_calls(&mut r, siple_config::AttendedReferOn::Consult).await;
    let server1 = answer_refers(raw1.clone(), 5);
    let server2 = answer_refers(raw2, 5);
    let s = r.stack.consult_link(&a, &b).await.unwrap();
    r.stack
        .transfer_complete(&s.id, siple_sip::CompleteMode::Attended)
        .await
        .unwrap();
    raw1.send(&raw1.msg("BYE", Some(&tag1), 40, "", "")).await;
    wait_session(&r, &s.id, TransferPhase::Abandoned).await;
    while let Ok(e) = r.events.try_recv() {
        r.buf.push(e);
    }
    assert!(
        !r.buf
            .iter()
            .any(|e| matches!(e, SipEvent::TransferSession { session }
            if session.id == s.id && session.phase == TransferPhase::Unconfirmed)),
        "never unconfirmed"
    );
    r.stack.shutdown().await;
    let _ = (server1.await, server2.await);
}
