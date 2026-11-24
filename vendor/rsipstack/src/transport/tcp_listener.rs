use crate::platform::CancellationToken;
use crate::transport::io_error::{classify_accept_io, wait_or_cancelled, ErrorRun, Next};
use crate::transport::tcp::TcpConnection;
use crate::transport::transport_layer::TransportLayerInnerRef;
use crate::transport::SipAddr;
use crate::transport::SipConnection;
use crate::transport::TransportEvent;
use crate::Result;
use socket2::{Protocol, Type};
use std::fmt;
use std::{net::SocketAddr, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};
pub struct TcpListenerConnectionInner {
    pub local_addr: SipAddr,
    pub external: Option<SipAddr>,
}

#[derive(Clone)]
pub struct TcpListenerConnection {
    pub inner: Arc<TcpListenerConnectionInner>,
}

impl TcpListenerConnection {
    pub async fn new(local_addr: SocketAddr, external: Option<SocketAddr>) -> Result<Self> {
        let inner = TcpListenerConnectionInner {
            local_addr: SipAddr {
                r#type: Some(crate::sip::transport::Transport::Tcp),
                addr: local_addr.into(),
            },
            external: external.map(|addr| SipAddr {
                r#type: Some(crate::sip::transport::Transport::Tcp),
                addr: addr.into(),
            }),
        };
        Ok(TcpListenerConnection {
            inner: Arc::new(inner),
        })
    }

    pub async fn serve_listener(
        &self,
        transport_layer_inner: TransportLayerInnerRef,
    ) -> Result<()> {
        let local = self.inner.local_addr.get_socketaddr()?;
        let socket = crate::platform::net::socket_for(local, Type::STREAM, Protocol::TCP)?;
        if let Err(e) = socket.set_reuse_address(true) {
            warn!(error = %e, "failed to set SO_REUSEADDR on TCP listener");
        }
        socket.set_nonblocking(true)?;
        socket.bind(&local.into())?;
        socket.listen(128)?;
        let listener = TcpListener::from_std(socket.into())?;
        let listener_local_addr = self.get_addr().clone();
        // siple: reported as closed if its socket dies (see `after_accept_error`)
        let this = SipConnection::TcpListener(self.clone());
        crate::platform::spawn(async move {
            let mut errors = ErrorRun::default();
            loop {
                let (stream, remote_addr) =
                    match accept_or_cancelled(&listener, &transport_layer_inner.cancel_token).await
                    {
                        Some(Ok((stream, remote_addr))) => {
                            errors.success();
                            (stream, remote_addr)
                        }
                        Some(Err(e)) => {
                            if after_accept_error(&e, &mut errors, &this, &transport_layer_inner)
                                .await
                            {
                                continue;
                            }
                            break;
                        }
                        None => {
                            debug!(local = %listener_local_addr, "TCP listener cancelled");
                            break;
                        }
                    };
                if !transport_layer_inner.is_whitelisted(remote_addr.ip()).await {
                    debug!(remote = %remote_addr, "tcp connection rejected by whitelist");
                    continue;
                }
                let local_addr = listener_local_addr.clone();
                let tcp_connection = match TcpConnection::from_stream(
                    stream,
                    local_addr.clone(),
                    Some(transport_layer_inner.cancel_token.child_token()),
                ) {
                    Ok(tcp_connection) => tcp_connection,
                    Err(e) => {
                        warn!(error = ?e, %local_addr, "Failed to create TCP connection");
                        continue;
                    }
                };
                let sip_connection = SipConnection::Tcp(tcp_connection.clone());
                transport_layer_inner.add_connection(sip_connection.clone());
                debug!(?local_addr, "new tcp connection");
            }
        });
        Ok(())
    }
}

/// siple: what an accept loop does after `accept` failed (see `io_error`; upstream retried at
/// once, so a broken listening socket, or a process out of descriptors, spun the loop). Returns
/// `false` when the loop must stop: the layer was cancelled during the backoff, or the socket
/// is dead, which is then reported as `TransportEvent::Closed(listener)`.
pub(crate) async fn after_accept_error(
    e: &std::io::Error,
    errors: &mut ErrorRun,
    listener: &SipConnection,
    transport_layer_inner: &TransportLayerInnerRef,
) -> bool {
    match errors.error(classify_accept_io(e)) {
        Next::Stop => {
            warn!(
                local = %listener.get_addr(),
                error = %e,
                errors = errors.consecutive(),
                "listening socket is dead; stopping its accept loop"
            );
            let _ = transport_layer_inner
                .transport_tx
                .send(TransportEvent::Closed(listener.clone()));
            false
        }
        Next::Retry(delay) => {
            if errors.should_log() {
                warn!(local = %listener.get_addr(), error = %e, errors = errors.consecutive(), "failed to accept a connection");
            } else {
                debug!(local = %listener.get_addr(), error = %e, errors = errors.consecutive(), "failed to accept a connection");
            }
            wait_or_cancelled(delay, Some(&transport_layer_inner.cancel_token)).await
        }
    }
}

/// Accepts the next connection, or returns `None` once `cancel_token` fires.
///
/// Listener accept loops run in detached tasks; racing the token lets them exit
/// and release the listening socket (and the transport layer they hold) when the
/// transport layer is cancelled.
pub(crate) async fn accept_or_cancelled(
    listener: &TcpListener,
    cancel_token: &CancellationToken,
) -> Option<std::io::Result<(TcpStream, SocketAddr)>> {
    let mut cancel_f = core::pin::pin!(cancel_token.cancelled());
    let mut accept_f = core::pin::pin!(listener.accept());
    match crate::platform::select::select2(&mut cancel_f, &mut accept_f).await {
        crate::platform::select::Either::A(()) => None,
        crate::platform::select::Either::B(accepted) => Some(
            accepted
                .map(|(stream, addr)| (stream, crate::platform::net::canonical_socket_addr(addr))),
        ),
    }
}

impl TcpListenerConnection {
    pub fn get_addr(&self) -> &SipAddr {
        if let Some(external) = &self.inner.external {
            external
        } else {
            &self.inner.local_addr
        }
    }

    pub async fn close(&self) -> Result<()> {
        Ok(())
    }
}

impl fmt::Display for TcpListenerConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TCP Listener {}", self.get_addr())
    }
}

impl fmt::Debug for TcpListenerConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
