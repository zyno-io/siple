//! An account's UDP SIP socket that the OS tears down mid-call (macOS "defunct" sockets: every
//! send `EPIPE`, every receive `ENOTCONN`) is replaced by a new socket on the same address: the
//! account sends and receives again, a live call keeps working, and so does a call being set
//! up (its transactions hold the connection, which now has the new socket under it).

mod common;

use std::time::Duration;

use common::{Rig, active_pair, add, free_udp_port, incoming_on, peer, rig, state_of, until};
use siple_sip::{AnswerOptions, CallState, EndedBy, SipEvent, UpdateOutcome};

fn is_transport_dead(e: &SipEvent, account: &str) -> bool {
    matches!(e, SipEvent::AccountReconnecting { account: a, reason }
        if a == account && reason == "transport_dead")
}

/// Kills the account's UDP socket, after dropping any `transport_dead` event already received
/// for it, so the next [`wait_replaced`] reads the one this death causes.
async fn kill(r: &mut Rig, account: &str) {
    while let Ok(e) = r.events.try_recv() {
        r.buf.push(e);
    }
    r.buf.retain(|e| !is_transport_dead(e, account));
    assert!(
        r.stack.simulate_udp_socket_death(account).await.unwrap(),
        "{account} has no UDP socket"
    );
}

/// Waits for the `transport_dead` event, which is sent once the new socket is in place.
async fn wait_replaced(r: &mut Rig, account: &str) {
    until(r, "transport_dead reconnect", |e| {
        is_transport_dead(e, account)
    })
    .await;
}

#[tokio::test]
async fn dead_udp_socket_is_replaced_and_the_call_and_account_keep_working() {
    let mut r = rig();
    let (a_call, b_call) = active_pair(&mut r).await;
    let a_addr = r.stack.account_status("a").unwrap().local_addr.unwrap();
    let b_addr = r.stack.account_status("b").unwrap().local_addr.unwrap();

    kill(&mut r, "a").await;
    wait_replaced(&mut r, "a").await;

    // `a` sends from the new socket and receives the answer.
    let resp = r
        .stack
        .send_options("a", &format!("sip:b@{b_addr}"))
        .await
        .expect("OPTIONS from a after the replacement");
    assert_eq!(resp.code, 200, "{resp:?}");
    // Same address (the Contact of the live call stays valid), and inbound requests reach it.
    assert_eq!(
        r.stack.account_status("a").unwrap().local_addr.unwrap(),
        a_addr
    );
    let resp = r
        .stack
        .send_options("b", &format!("sip:a@{a_addr}"))
        .await
        .expect("OPTIONS to a after the replacement");
    assert_eq!(resp.code, 200, "{resp:?}");

    // The far end hangs up the live call: its BYE reaches `a` on the new socket.
    r.stack.hangup(&b_call).await.unwrap();
    let ended = state_of(&mut r, &a_call, CallState::Ended).await;
    assert_eq!(
        ended.end.as_ref().map(|e| e.by),
        Some(EndedBy::Remote),
        "{ended:?}"
    );

    r.stack.shutdown().await;
}

#[tokio::test]
async fn a_call_being_set_up_survives_the_socket_death() {
    let mut r = rig();
    add(&r, peer("a")).await;
    let b = add(&r, peer("b")).await;
    let a_call = r
        .stack
        .dial("a", &format!("sip:b@{b}"), Default::default())
        .await
        .unwrap();
    let b_call = incoming_on(&mut r, "b").await;
    // The callee's socket dies while its INVITE server transaction is open.
    kill(&mut r, "b").await;
    wait_replaced(&mut r, "b").await;
    // The 200 OK leaves through that transaction (on the new socket), the ACK comes back on it.
    r.stack
        .answer(&b_call.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(&mut r, &a_call, CallState::Active).await;
    state_of(&mut r, &b_call.id, CallState::Active).await;
    r.stack.hangup(&a_call).await.unwrap();
    state_of(&mut r, &b_call.id, CallState::Ended).await;
    r.stack.shutdown().await;
}

/// Each replacement dies again right away: replaced every time, later and later (the normal
/// 0.5 s and 1 s delays; nothing here depends on how long they take).
#[tokio::test]
async fn dead_udp_socket_of_an_idle_account_is_replaced_more_than_once() {
    let mut r = rig();
    let a = add(&r, peer("a")).await;
    let b = add(&r, peer("b")).await;
    for round in 0..3 {
        kill(&mut r, "a").await;
        wait_replaced(&mut r, "a").await;
        let resp = r
            .stack
            .send_options("a", &format!("sip:b@{b}"))
            .await
            .expect("OPTIONS from a");
        assert_eq!(resp.code, 200, "round {round}: {resp:?}");
        let resp = r
            .stack
            .send_options("b", &format!("sip:a@{a}"))
            .await
            .expect("OPTIONS to a");
        assert_eq!(resp.code, 200, "round {round}: {resp:?}");
    }
    r.stack.shutdown().await;
}

/// Waits until the account reports a delayed socket replacement as pending; returns the quick
/// deaths in a row. The death was just caused, so the account handles it within moments; the
/// replacement it schedules waits for an hour (test hook), so the window cannot close here.
async fn until_pending(r: &Rig, account: &str) -> u32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (pending, deaths) = r.stack.socket_replacement_state(account).await.unwrap();
        if pending {
            return deaths;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{account}: no delayed replacement was scheduled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A delayed replacement is pending when the runtime is rebuilt: the rebuild drops it (and the
/// dead socket) and forgets the deaths, so the new runtime's first death is replaced at once
/// and its next quick one is delayed afresh.
#[tokio::test]
async fn a_rebuild_while_a_replacement_is_pending_starts_afresh() {
    let mut r = rig();
    let mut cfg = peer("a");
    add(&r, cfg.clone()).await;
    let b = add(&r, peer("b")).await;
    r.stack
        .set_socket_replace_delay("a", Some(Duration::from_secs(3600)))
        .unwrap();

    // First death: replaced at once. Second, right after: delayed (by an hour).
    kill(&mut r, "a").await;
    wait_replaced(&mut r, "a").await;
    kill(&mut r, "a").await;
    assert_eq!(until_pending(&r, "a").await, 1);

    // Rebuild the runtime while that replacement waits (a rebuild-only field changes).
    cfg.local_port = free_udp_port();
    assert_eq!(
        r.stack.update_account(cfg.clone()).await.unwrap(),
        UpdateOutcome::Rebuilt
    );
    let a = format!("127.0.0.1:{}", cfg.local_port);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while r.stack.account_status("a").unwrap().local_addr.as_deref() != Some(a.as_str()) {
        assert!(tokio::time::Instant::now() < deadline, "no rebuilt runtime");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        r.stack.socket_replacement_state("a").await.unwrap(),
        (false, 0),
        "the rebuild drops the pending replacement and forgets the deaths"
    );

    // The new runtime's first death is a first one again: replaced at once.
    kill(&mut r, "a").await;
    wait_replaced(&mut r, "a").await;
    assert_eq!(
        r.stack.socket_replacement_state("a").await.unwrap(),
        (false, 0)
    );
    // Its next quick death is delayed afresh (shortened here so the test can wait for it).
    r.stack
        .set_socket_replace_delay("a", Some(Duration::from_millis(100)))
        .unwrap();
    kill(&mut r, "a").await;
    wait_replaced(&mut r, "a").await;
    assert_eq!(
        r.stack.socket_replacement_state("a").await.unwrap(),
        (false, 1)
    );

    let resp = r
        .stack
        .send_options("a", &format!("sip:b@{b}"))
        .await
        .expect("OPTIONS from a");
    assert_eq!(resp.code, 200, "{resp:?}");
    let resp = r
        .stack
        .send_options("b", &format!("sip:a@{a}"))
        .await
        .expect("OPTIONS to a");
    assert_eq!(resp.code, 200, "{resp:?}");
    r.stack.shutdown().await;
}
