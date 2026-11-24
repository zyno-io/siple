//! An account's UDP SIP socket that the OS tears down mid-call (macOS "defunct" sockets: every
//! send `EPIPE`, every receive `ENOTCONN`) is replaced by a new socket on the same address: the
//! account sends and receives again, and a live call keeps working.

mod common;

use common::{active_pair, rig, state_of, until};
use siple_sip::{CallState, EndedBy, SipEvent};

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

    // The account actor handles one message at a time: a command sent after the event runs
    // once the socket has been replaced. `a` sends from the new socket and receives the answer.
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

/// Each replacement dies again right away: replaced every time, later and later (0.5 s, 1 s).
#[tokio::test]
async fn dead_udp_socket_of_an_idle_account_is_replaced_more_than_once() {
    let mut r = rig();
    let a = common::add(&r, common::peer("a")).await;
    let b = common::add(&r, common::peer("b")).await;
    for round in 0..3 {
        assert!(
            r.stack.simulate_udp_socket_death("a").await.unwrap(),
            "round {round}"
        );
        wait_replaced(&mut r, "a").await;
        // A command to `a` first: it runs after the replacement (see above).
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
