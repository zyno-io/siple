//! An account's UDP SIP socket that the OS tears down mid-call (macOS "defunct" sockets: every
//! send `EPIPE`, every receive `ENOTCONN`) is replaced by a new socket on the same address: the
//! account sends and receives again, a live call keeps working, and so does a call being set
//! up (its transactions hold the connection, which now has the new socket under it).

mod common;

use std::time::Duration;

use common::{active_pair, add, free_udp_port, incoming_on, peer, rig, state_of, until};
use siple_sip::{AnswerOptions, CallState, EndedBy, SipEvent, UpdateOutcome};

async fn wait_replaced(r: &mut common::Rig, account: &str) {
    let account = account.to_string();
    until(r, "transport_dead reconnect", |e| {
        matches!(e, SipEvent::AccountReconnecting { account: a, reason }
            if *a == account && reason == "transport_dead")
    })
    .await;
}

#[tokio::test]
async fn dead_udp_socket_is_replaced_and_the_call_and_account_keep_working() {
    let mut r = rig();
    let (a_call, b_call) = active_pair(&mut r).await;
    let a_addr = r.stack.account_status("a").unwrap().local_addr.unwrap();
    let b_addr = r.stack.account_status("b").unwrap().local_addr.unwrap();

    assert!(r.stack.simulate_udp_socket_death("a").await.unwrap());
    wait_replaced(&mut r, "a").await;

    // The event is sent once the new socket is in place: `a` sends from it and receives the
    // answer.
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
    assert!(r.stack.simulate_udp_socket_death("b").await.unwrap());
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

/// Each replacement dies again right away: replaced every time, later and later (0.5 s, 1 s).
#[tokio::test]
async fn dead_udp_socket_of_an_idle_account_is_replaced_more_than_once() {
    let mut r = rig();
    let a = add(&r, peer("a")).await;
    let b = add(&r, peer("b")).await;
    for round in 0..3 {
        assert!(
            r.stack.simulate_udp_socket_death("a").await.unwrap(),
            "round {round}"
        );
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

/// Waits until the account reports a delayed socket replacement as pending (or not); returns
/// the quick deaths in a row.
async fn until_pending(r: &common::Rig, account: &str, pending: bool) -> u32 {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (p, deaths) = r.stack.socket_replacement_state(account).await.unwrap();
        if p == pending {
            return deaths;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{account}: replacement pending never became {pending}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A delayed replacement is pending when the runtime is rebuilt: the rebuild drops it (and the
/// dead socket), the new runtime's first death is replaced at once, and the old runtime's timer
/// firing later does not cancel the new runtime's own delayed replacement.
#[tokio::test]
async fn a_rebuild_while_a_replacement_is_pending_starts_afresh() {
    let mut r = rig();
    let mut cfg = peer("a");
    add(&r, cfg.clone()).await;
    let b = add(&r, peer("b")).await;

    // First death: replaced at once. Second, right after: delayed (0.5 s).
    assert!(r.stack.simulate_udp_socket_death("a").await.unwrap());
    wait_replaced(&mut r, "a").await;
    assert!(r.stack.simulate_udp_socket_death("a").await.unwrap());
    assert_eq!(until_pending(&r, "a", true).await, 1);

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
        "the rebuild forgets the pending replacement and the deaths"
    );

    // The new runtime's first death is a first one again: replaced at once.
    assert!(r.stack.simulate_udp_socket_death("a").await.unwrap());
    wait_replaced(&mut r, "a").await;
    assert_eq!(
        r.stack.socket_replacement_state("a").await.unwrap(),
        (false, 0)
    );
    // A quick second death is delayed; the old runtime's timer (due about now) must not
    // cancel it.
    assert!(r.stack.simulate_udp_socket_death("a").await.unwrap());
    wait_replaced(&mut r, "a").await;

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
