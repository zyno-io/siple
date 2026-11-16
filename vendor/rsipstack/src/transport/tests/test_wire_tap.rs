//! siple: WireTap tests.
use crate::transport::{
    connection::KEEPALIVE_REQUEST,
    tcp::TcpConnection,
    udp::UdpConnection,
    wire_tap::{WireDirection, WireEvent, WireTap},
    SipAddr, SipConnection, TransportLayer,
};
use crate::Result;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::sleep;

#[derive(Debug, Clone, PartialEq)]
struct Rec {
    dir: WireDirection,
    transport: crate::sip::Transport,
    local: std::net::SocketAddr,
    remote: std::net::SocketAddr,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Rec>>);

impl WireTap for Recorder {
    fn on_wire(&self, ev: WireEvent<'_>) {
        self.0.lock().unwrap().push(Rec {
            dir: ev.direction,
            transport: ev.transport,
            local: ev.local,
            remote: ev.remote,
            bytes: ev.bytes.to_vec(),
        });
    }
}

impl Recorder {
    fn take(&self) -> Vec<Rec> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

const REGISTER: &str = "REGISTER sip:bob@example.com SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5061;branch=z9hG4bKnashd92\r\nCSeq: 1 REGISTER\r\n\r\n";

#[tokio::test]
async fn test_wire_tap_udp_send_recv_exact_bytes() -> Result<()> {
    let alice = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    let bob = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    let alice_tap = Arc::new(Recorder::default());
    let bob_tap = Arc::new(Recorder::default());
    alice.set_wire_tap(Some(alice_tap.clone()));
    bob.set_wire_tap(Some(bob_tap.clone()));

    let (alice_tx, _alice_rx) = unbounded_channel();
    let (bob_tx, mut bob_rx) = unbounded_channel();
    let a = alice.clone();
    let b = bob.clone();
    tokio::spawn(async move { a.serve_loop(alice_tx).await });
    tokio::spawn(async move { b.serve_loop(bob_tx).await });
    sleep(Duration::from_millis(20)).await;

    let msg = crate::sip::SipMessage::try_from(REGISTER.as_bytes())?;
    let wire = msg.to_bytes();
    alice.send(msg, Some(bob.get_addr())).await?;
    tokio::time::timeout(Duration::from_secs(2), bob_rx.recv())
        .await
        .expect("bob recv timeout");

    let sent = alice_tap.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].dir, WireDirection::Send);
    assert_eq!(sent[0].transport, crate::sip::Transport::Udp);
    assert_eq!(sent[0].local, alice.get_addr().get_socketaddr()?);
    assert_eq!(sent[0].remote, bob.get_addr().get_socketaddr()?);
    assert_eq!(sent[0].bytes, wire.to_vec());

    let recvd = bob_tap.take();
    assert_eq!(recvd.len(), 1);
    assert_eq!(recvd[0].dir, WireDirection::Recv);
    assert_eq!(recvd[0].local, bob.get_addr().get_socketaddr()?);
    assert_eq!(recvd[0].remote, alice.get_addr().get_socketaddr()?);
    assert_eq!(recvd[0].bytes, wire.to_vec());
    Ok(())
}

#[tokio::test]
async fn test_wire_tap_udp_garbage_and_keepalive_are_tapped() -> Result<()> {
    let alice = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    let bob = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    let bob_tap = Arc::new(Recorder::default());
    bob.set_wire_tap(Some(bob_tap.clone()));
    let (bob_tx, mut bob_rx) = unbounded_channel();
    let b = bob.clone();
    tokio::spawn(async move { b.serve_loop(bob_tx).await });
    sleep(Duration::from_millis(20)).await;

    let garbage = b"\x00\x01 this is not SIP at all";
    alice.send_raw(garbage, bob.get_addr()).await?;
    alice.send_raw(KEEPALIVE_REQUEST, bob.get_addr()).await?;
    // keepalive response goes back to alice
    let mut buf = [0u8; 64];
    alice.recv_raw(&mut buf).await?;
    // no SIP event must be produced for garbage
    assert!(bob_rx.try_recv().is_err());

    let recs = bob_tap.take();
    assert_eq!(recs.len(), 3, "{recs:?}");
    assert_eq!(recs[0].dir, WireDirection::Recv);
    assert_eq!(recs[0].bytes, garbage.to_vec());
    assert_eq!(recs[1].dir, WireDirection::Recv);
    assert_eq!(recs[1].bytes, KEEPALIVE_REQUEST.to_vec());
    assert_eq!(recs[2].dir, WireDirection::Send);
    assert_eq!(recs[2].bytes, b"\r\n".to_vec());
    Ok(())
}

#[tokio::test]
async fn test_wire_tap_applies_via_transport_layer() -> Result<()> {
    let tl = TransportLayer::new(tokio_util::sync::CancellationToken::new());
    let early = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    tl.add_transport(SipConnection::Udp(early.clone())); // added BEFORE the tap
    let tap = Arc::new(Recorder::default());
    tl.set_wire_tap(tap.clone());
    let late = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    tl.add_transport(SipConnection::Udp(late.clone())); // added AFTER the tap

    let peer = UdpConnection::create_connection("127.0.0.1:0".parse()?, None, None).await?;
    let msg = crate::sip::SipMessage::try_from(REGISTER.as_bytes())?;
    early.send(msg.clone(), Some(peer.get_addr())).await?;
    late.send(msg, Some(peer.get_addr())).await?;
    assert_eq!(tap.take().len(), 2);

    tl.clear_wire_tap();
    let msg = crate::sip::SipMessage::try_from(REGISTER.as_bytes())?;
    early.send(msg, Some(peer.get_addr())).await?;
    assert!(tap.take().is_empty());
    Ok(())
}

#[tokio::test]
async fn test_wire_tap_tcp_send_recv_and_unparsable() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let remote = SipAddr {
        r#type: Some(crate::sip::Transport::Tcp),
        addr: listener.local_addr()?.into(),
    };
    let client = TcpConnection::connect(&remote, None).await?;
    let (mut server, _) = listener.accept().await?;
    let tap = Arc::new(Recorder::default());
    SipConnection::Tcp(client.clone()).set_wire_tap(Some(tap.clone()));

    let (tx, mut rx) = unbounded_channel();
    let c = client.clone();
    tokio::spawn(async move {
        use crate::transport::stream::StreamConnection;
        c.serve_loop(tx).await
    });

    // send path
    let msg = crate::sip::SipMessage::try_from(REGISTER.as_bytes())?;
    let wire = msg.to_bytes();
    SipConnection::Tcp(client.clone()).send(msg, None).await?;
    let mut got = vec![0u8; wire.len()];
    server.read_exact(&mut got).await?;
    assert_eq!(got, wire.to_vec());

    // recv path: a valid message split over two writes is one tap event
    let (a, b) = REGISTER.as_bytes().split_at(20);
    server.write_all(a).await?;
    sleep(Duration::from_millis(30)).await;
    server.write_all(b).await?;
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout");

    let recs = tap.take();
    assert_eq!(recs.len(), 2, "{recs:?}");
    assert_eq!(recs[0].dir, WireDirection::Send);
    assert_eq!(recs[0].bytes, wire.to_vec());
    assert_eq!(recs[0].transport, crate::sip::Transport::Tcp);
    assert_eq!(recs[1].dir, WireDirection::Recv);
    assert_eq!(recs[1].bytes, REGISTER.as_bytes().to_vec());

    // an unparsable (but framed) message is still tapped before parsing
    let junk = "NOT A SIP MESSAGE\r\n\r\n";
    server.write_all(junk.as_bytes()).await?;
    sleep(Duration::from_millis(100)).await;
    let recs = tap.take();
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0].bytes, junk.as_bytes().to_vec());
    Ok(())
}
