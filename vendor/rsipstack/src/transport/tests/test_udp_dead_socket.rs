//! siple: the UDP receive loop on a socket that fails (see `transport::io_error`): a dead socket
//! ends the loop with `TransportEvent::Closed` instead of spinning; per-datagram errors are
//! skipped. A scripted socket drives the loop, so nothing depends on timing.
use crate::transport::{
    connection::{KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE},
    io_error::{BACKOFF_AFTER, FATAL_AFTER},
    udp::UdpConnection,
    SipConnection, TransportEvent,
};
use std::collections::VecDeque;
use std::io::{Error, ErrorKind};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

/// What one `recv_from` call returns; past the end of the script it waits forever.
enum Step {
    Datagram(&'static [u8], SocketAddr),
    Fail(Error),
}

struct ScriptedSocket {
    local: SocketAddr,
    script: Mutex<VecDeque<Step>>,
    recv_calls: AtomicUsize,
    sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
}

impl ScriptedSocket {
    fn new(script: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            local: "127.0.0.1:5999".parse().unwrap(),
            script: Mutex::new(script.into()),
            recv_calls: AtomicUsize::new(0),
            sent: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl crate::platform::net::UdpSocket for ScriptedSocket {
    async fn recv_from(&self, buf: &mut [u8]) -> crate::Result<(usize, SocketAddr)> {
        self.recv_calls.fetch_add(1, Ordering::SeqCst);
        let step = self.script.lock().unwrap().pop_front();
        match step {
            Some(Step::Datagram(bytes, from)) => {
                buf[..bytes.len()].copy_from_slice(bytes);
                Ok((bytes.len(), from))
            }
            Some(Step::Fail(e)) => Err(e.into()),
            None => std::future::pending().await,
        }
    }
    async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> crate::Result<usize> {
        self.sent.lock().unwrap().push((buf.to_vec(), addr));
        Ok(buf.len())
    }
    fn local_addr(&self) -> crate::Result<SocketAddr> {
        Ok(self.local)
    }
}

fn peer() -> SocketAddr {
    "127.0.0.1:6000".parse().unwrap()
}

/// A socket the OS has torn down (`recvfrom` -> `ENOTCONN` on every call, as for a macOS
/// "defunct" socket): one call, then the loop ends and reports the listener as closed.
#[tokio::test]
async fn dead_socket_ends_the_loop_and_is_reported_closed() -> crate::Result<()> {
    for kind in [ErrorKind::NotConnected, ErrorKind::BrokenPipe] {
        // Far more failures than the loop may ever try: a spin would consume them all.
        let sock =
            ScriptedSocket::new((0..10_000).map(|_| Step::Fail(Error::from(kind))).collect());
        let conn = UdpConnection::from_socket(sock.clone(), None, None).await?;
        let (tx, mut rx) = unbounded_channel();
        let ended = tokio::time::timeout(Duration::from_secs(10), conn.serve_loop(tx))
            .await
            .expect("the receive loop must end on a dead socket");
        assert!(ended.is_err(), "{kind:?}: the loop ends with the error");
        assert_eq!(sock.recv_calls.load(Ordering::SeqCst), 1, "{kind:?}");
        match rx.try_recv() {
            Ok(TransportEvent::Closed(SipConnection::Udp(c))) => {
                assert_eq!(c.get_addr(), conn.get_addr());
                assert!(c.get_remote_addr().is_none());
            }
            _ => panic!("{kind:?}: expected Closed(Udp) for the listener"),
        }
        assert!(rx.try_recv().is_err(), "exactly one event");
    }
    Ok(())
}

/// Errors tied to one datagram (an ICMP port unreachable reported on the socket, `EINTR`)
/// are skipped without waiting: the datagram after them is served at once.
#[tokio::test]
async fn per_datagram_errors_are_skipped() -> crate::Result<()> {
    let mut script: Vec<Step> = [
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionRefused,
        ErrorKind::Interrupted,
    ]
    .into_iter()
    .cycle()
    .take(BACKOFF_AFTER as usize)
    .map(|k| Step::Fail(Error::from(k)))
    .collect();
    script.push(Step::Datagram(KEEPALIVE_REQUEST, peer()));
    let sock = ScriptedSocket::new(script);
    let conn = UdpConnection::from_socket(sock.clone(), None, None).await?;
    let (tx, mut rx) = unbounded_channel();
    let serve = conn.serve_loop(tx);
    tokio::pin!(serve);
    // The loop parks in `recv_from` once the script is used up; it must not have ended.
    tokio::select! {
        biased;
        r = &mut serve => panic!("the loop ended: {:?}", r.err()),
        _ = until_parked(&sock, BACKOFF_AFTER as usize + 2) => {}
    }
    assert_eq!(
        sock.sent.lock().unwrap().as_slice(),
        &[(KEEPALIVE_RESPONSE.to_vec(), peer())],
        "the keep-alive after the errors was answered"
    );
    assert!(rx.try_recv().is_err(), "no Closed event");
    Ok(())
}

/// An error that is neither clearly fatal nor tied to one datagram ends the loop only after a
/// long unbroken run, waiting more and more between attempts (no hot loop).
#[tokio::test(start_paused = true)]
async fn persistent_unknown_errors_back_off_then_end_the_loop() -> crate::Result<()> {
    let sock = ScriptedSocket::new(
        (0..10_000)
            .map(|_| Step::Fail(Error::other("mystery")))
            .collect(),
    );
    let conn = UdpConnection::from_socket(sock.clone(), None, None).await?;
    let (tx, mut rx) = unbounded_channel();
    let started = tokio::time::Instant::now();
    let ended = conn.serve_loop(tx).await;
    assert!(ended.is_err());
    assert_eq!(sock.recv_calls.load(Ordering::SeqCst), FATAL_AFTER as usize);
    // The waits between the attempts add up (on the paused clock).
    assert!(
        started.elapsed() >= Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(TransportEvent::Closed(SipConnection::Udp(_)))
    ));
    Ok(())
}

/// Cancelling the connection while the loop backs off ends it quietly (no Closed event).
#[tokio::test(start_paused = true)]
async fn cancel_during_backoff_ends_quietly() -> crate::Result<()> {
    let sock = ScriptedSocket::new(
        (0..10_000)
            .map(|_| Step::Fail(Error::other("mystery")))
            .collect(),
    );
    let token = crate::platform::CancellationToken::new();
    let conn = UdpConnection::from_socket(sock.clone(), None, Some(token.clone())).await?;
    let (tx, mut rx) = unbounded_channel();
    let serve = conn.serve_loop(tx);
    tokio::pin!(serve);
    // The first BACKOFF_AFTER errors are retried at once; the next one starts a wait.
    tokio::select! {
        biased;
        r = &mut serve => panic!("ended early: {:?}", r.err()),
        _ = until_parked(&sock, BACKOFF_AFTER as usize + 1) => {}
    }
    token.cancel();
    assert!(serve.await.is_ok());
    assert!(rx.try_recv().is_err());
    Ok(())
}

/// Resolves once `recv_from` has been called `n` times (yields between checks; the loop under
/// test runs on the same task, so this only observes progress, it never sleeps on the clock).
async fn until_parked(sock: &ScriptedSocket, n: usize) {
    while sock.recv_calls.load(Ordering::SeqCst) < n {
        tokio::task::yield_now().await;
    }
}
