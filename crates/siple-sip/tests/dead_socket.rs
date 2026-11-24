//! An account's UDP SIP socket that the OS tears down mid-call (macOS "defunct" sockets: every
//! send `EPIPE`, every receive `ENOTCONN`) is replaced by a new socket on the same address: the
//! account sends and receives again, a live call keeps working, and so does a call being set
//! up (its transactions hold the connection, which now has the new socket under it).

mod common;

use common::{active_pair, add, incoming_on, peer, rig, state_of, until};
use siple_sip::{AnswerOptions, CallState, EndedBy, SipEvent};

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
