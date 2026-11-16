//! siple patch: the CANCEL that terminates a server INVITE is kept on the dialog so the
//! application can read its `Reason` header (RFC 3326, "Call completed elsewhere").
use crate::dialog::{
    dialog::{DialogState, TerminatedReason},
    dialog_layer::DialogLayer,
};
use crate::sip::Method;
use crate::transport::{udp::UdpConnection, TransportLayer};
use crate::EndpointBuilder;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_cancel_request_is_available_after_termination() -> crate::Result<()> {
    let token = CancellationToken::new();
    let transport_layer = TransportLayer::new(token.child_token());
    let udp = UdpConnection::create_connection(
        "127.0.0.1:0".parse().unwrap(),
        None,
        Some(token.child_token()),
    )
    .await?;
    let uas = udp.get_addr().get_socketaddr()?;
    transport_layer.add_transport(udp.into());
    let endpoint = EndpointBuilder::new()
        .with_user_agent("rsipstack-test")
        .with_transport_layer(transport_layer)
        .with_cancel_token(token.child_token())
        .build();
    let dialog_layer = Arc::new(DialogLayer::new(endpoint.inner.clone()));
    let mut incoming = endpoint.incoming_transactions()?;
    let endpoint_inner = endpoint.inner.clone();
    tokio::spawn(async move {
        let _ = endpoint_inner.serve().await;
    });
    let (state_sender, mut states) = unbounded_channel();
    let (dialog_sender, mut dialogs) = unbounded_channel();
    tokio::spawn(async move {
        while let Some(mut tx) = incoming.recv().await {
            if tx.original.method == Method::Invite {
                let dialog = dialog_layer
                    .get_or_create_server_invite(&tx, state_sender.clone(), None, None)
                    .expect("server dialog");
                dialog_sender.send(dialog.clone()).unwrap();
                let mut d = dialog;
                tokio::spawn(async move {
                    let _ = d.handle(&mut tx).await;
                });
            }
        }
    });

    let sock = UdpSocket::bind("127.0.0.1:0").await?;
    let addr = sock.local_addr()?;
    let msg = |method: &str, extra: &str| {
        format!(
            "{method} sip:bob@{uas} SIP/2.0\r\nVia: SIP/2.0/UDP {addr};branch=z9hG4bK-cancel-test\r\n\
             Max-Forwards: 70\r\nFrom: <sip:alice@{addr}>;tag=uac\r\nTo: <sip:bob@{uas}>\r\n\
             Call-ID: cancel-reason-test\r\nCSeq: 1 {method}\r\nContact: <sip:alice@{addr}>\r\n\
             {extra}Content-Length: 0\r\n\r\n"
        )
    };
    sock.send_to(msg("INVITE", "").as_bytes(), uas).await?;
    let dialog = tokio::time::timeout(Duration::from_secs(2), dialogs.recv())
        .await
        .expect("server dialog")
        .unwrap();
    assert!(dialog.last_remote_cancel().is_none());
    sock.send_to(
        msg("CANCEL", "Reason: SIP;cause=200;text=\"Call completed elsewhere\"\r\n").as_bytes(),
        uas,
    )
    .await?;
    loop {
        let st = tokio::time::timeout(Duration::from_secs(2), states.recv())
            .await
            .expect("state")
            .unwrap();
        if let DialogState::Terminated(_, reason) = st {
            assert!(matches!(reason, TerminatedReason::UacCancel));
            break;
        }
    }
    let cancel = dialog.last_remote_cancel().expect("CANCEL is recorded");
    assert_eq!(cancel.method, Method::Cancel);
    let reason = cancel
        .headers
        .iter()
        .find(|h| h.name().eq_ignore_ascii_case("reason"))
        .expect("Reason header")
        .value()
        .to_string();
    assert!(reason.contains("cause=200"), "{reason}");
    token.cancel();
    Ok(())
}
