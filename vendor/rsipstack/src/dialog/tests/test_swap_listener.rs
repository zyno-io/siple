//! siple: a network change moves the UDP socket of a running endpoint
//! (`TransportLayer::swap_listener`) and the dialog's Contact
//! (`InviteDialog::set_local_contact`); the next in-dialog request leaves from the new socket
//! with a Via and Contact of the new address, and datagrams sent to the new socket are served.
use crate::dialog::{
    dialog_layer::DialogLayer, invitation::InviteOption, invite_dialog::InviteDialog,
};
use crate::sip::{
    prelude::HeadersExt,
    Method, Request, SipMessage, Uri,
};
use crate::transport::{udp::UdpConnection, TransportLayer};
use crate::EndpointBuilder;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

fn response(req: &Request, code: u16, reason: &str, extra: &str) -> String {
    let to = req.to_header().unwrap().value().to_string();
    let to = if to.contains(";tag=") {
        to
    } else {
        format!("{to};tag=peer-tag")
    };
    format!(
        "SIP/2.0 {code} {reason}\r\nVia: {}\r\nFrom: {}\r\nTo: {to}\r\nCall-ID: {}\r\nCSeq: {}\r\n{extra}Content-Length: 0\r\n\r\n",
        req.via_header().unwrap().value(),
        req.from_header().unwrap().value(),
        req.call_id_header().unwrap().value(),
        req.cseq_header().unwrap().value(),
    )
}

async fn recv_request(sock: &UdpSocket, method: Method) -> (Request, SocketAddr) {
    let mut buf = vec![0u8; 8192];
    loop {
        let (n, from) = tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut buf))
            .await
            .unwrap_or_else(|_| panic!("timeout waiting for {method}"))
            .expect("recv");
        if let Ok(SipMessage::Request(req)) =
            SipMessage::try_from(String::from_utf8_lossy(&buf[..n]).as_ref())
        {
            if req.method == method {
                return (req, from);
            }
        }
    }
}

fn dialog_of(
    layer: DialogLayer,
    opt: InviteOption,
) -> tokio::task::JoinHandle<crate::Result<InviteDialog>> {
    let (state_sender, _rx) = unbounded_channel();
    tokio::spawn(async move {
        let (d, _) = layer.do_invite(opt, state_sender).await?;
        Ok(d)
    })
}

#[tokio::test]
async fn swapped_udp_listener_carries_the_next_re_invite_with_the_new_contact() -> crate::Result<()>
{
    let token = CancellationToken::new();
    let peer = UdpSocket::bind("127.0.0.1:0").await?;
    let peer_addr = peer.local_addr()?;

    let tl = TransportLayer::new(token.child_token());
    let old_tok = token.child_token();
    let old = UdpConnection::create_connection(
        "127.0.0.1:0".parse().unwrap(),
        None,
        Some(old_tok.clone()),
    )
    .await?;
    let old_addr = old.get_addr().clone();
    tl.add_transport(old.into());
    let endpoint = EndpointBuilder::new()
        .with_user_agent("rsipstack-test")
        .with_transport_layer(tl)
        .with_cancel_token(token.child_token())
        .build();
    let mut incoming = endpoint.incoming_transactions()?;
    let inner = endpoint.inner.clone();
    tokio::spawn(async move {
        let _ = inner.serve().await;
    });
    let layer = DialogLayer::new(endpoint.inner.clone());
    let opt = InviteOption {
        caller: Uri::try_from("sip:alice@example.com")?,
        callee: Uri::try_from(format!("sip:bob@{peer_addr}").as_str())?,
        contact: Uri::try_from(format!("sip:alice@{}", old_addr.addr).as_str())?,
        ..Default::default()
    };
    let invite = dialog_of(layer, opt);
    let (req, from) = recv_request(&peer, Method::Invite).await;
    assert_eq!(from.port(), old_addr.addr.port.unwrap().value());
    let contact = format!("Contact: <sip:bob@{peer_addr}>\r\n");
    peer.send_to(response(&req, 200, "OK", &contact).as_bytes(), from)
        .await?;
    recv_request(&peer, Method::Ack).await;
    let dialog = invite.await.unwrap()?;

    // The network changed: a new socket takes over, the old one stops.
    let new = UdpConnection::create_connection(
        "127.0.0.1:0".parse().unwrap(),
        None,
        Some(token.child_token()),
    )
    .await?;
    let new_addr = new.get_addr().clone();
    assert_ne!(new_addr, old_addr);
    endpoint
        .inner
        .transport_layer
        .swap_listener(Some(&old_addr), new.into()).await?;
    old_tok.cancel();
    assert_eq!(endpoint.inner.transport_layer.get_addrs(), vec![new_addr.clone()]);
    let new_contact = Uri::try_from(format!("sip:alice@{}", new_addr.addr).as_str())?;
    dialog.set_local_contact(new_contact.clone());
    assert_eq!(dialog.local_contact(), Some(new_contact));

    let re = {
        let d = dialog.clone();
        tokio::spawn(async move { d.reinvite(None, None).await })
    };
    let (req, from) = recv_request(&peer, Method::Invite).await;
    assert_eq!(
        from.port(),
        new_addr.addr.port.unwrap().value(),
        "the re-INVITE leaves from the new socket"
    );
    let via = req.via_header().unwrap().value().to_string();
    assert!(via.contains(&new_addr.addr.to_string()), "Via: {via}");
    let c = req.contact_header().unwrap().value().to_string();
    assert!(c.contains(&new_addr.addr.to_string()), "Contact: {c}");
    peer.send_to(response(&req, 200, "OK", &contact).as_bytes(), from)
        .await?;
    recv_request(&peer, Method::Ack).await;
    let resp = tokio::time::timeout(Duration::from_secs(3), re)
        .await
        .expect("re-INVITE timed out")
        .unwrap()?;
    assert_eq!(resp.map(|r| r.status_code.code()), Some(200));

    // A request sent to the new socket is served (receive loop started by the swap).
    let bye = format!(
        "BYE sip:alice@{} SIP/2.0\r\nVia: SIP/2.0/UDP {peer_addr};branch=z9hG4bKbye1\r\nFrom: {}\r\nTo: {}\r\nCall-ID: {}\r\nCSeq: 7 BYE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n",
        new_addr.addr,
        req.to_header().unwrap().value(),
        req.from_header().unwrap().value(),
        req.call_id_header().unwrap().value(),
    );
    let new_sock: SocketAddr = new_addr.addr.try_into()?;
    peer.send_to(bye.as_bytes(), new_sock).await?;
    let tx = tokio::time::timeout(Duration::from_secs(3), incoming.recv())
        .await
        .expect("a BYE sent to the new socket never reached the endpoint")
        .expect("incoming transactions closed");
    assert_eq!(tx.original.method, Method::Bye);
    token.cancel();
    Ok(())
}
