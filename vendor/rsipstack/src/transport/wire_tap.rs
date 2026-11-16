//! siple: raw wire-level SIP trace hook (not part of upstream rsipstack).
//!
//! A [`WireTap`] installed on a [`TransportLayer`](super::TransportLayer) is
//! invoked with the exact bytes sent to / received from the network by every
//! connection that layer owns (UDP, outbound and accepted TCP/TLS). On
//! receive the tap fires before any parsing, so unparsable messages,
//! keepalive CRLFs and retransmissions are all observed; on send it fires
//! after serialization, immediately before the bytes are written.
use crate::platform::sync::RwLock;
use crate::sip::transport::Transport;
use alloc::sync::Arc;
use core::net::SocketAddr;

/// Direction of a [`WireEvent`] relative to this endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireDirection {
    Send,
    Recv,
}

/// One datagram (UDP) or one framed message / keepalive (stream transports).
#[derive(Debug, Clone, Copy)]
pub struct WireEvent<'a> {
    pub direction: WireDirection,
    pub transport: Transport,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub bytes: &'a [u8],
}

/// Callback receiving raw wire events. Must not block.
pub trait WireTap: Send + Sync {
    fn on_wire(&self, ev: WireEvent<'_>);
}

/// Shared, swappable tap holder; clones share the same slot. Reading an empty
/// slot is a single uncontended lock acquisition.
#[derive(Clone, Default)]
pub struct WireTapSlot(Arc<RwLock<Option<Arc<dyn WireTap>>>>);

impl WireTapSlot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, tap: Option<Arc<dyn WireTap>>) {
        *self.0.write() = tap;
    }

    pub fn get(&self) -> Option<Arc<dyn WireTap>> {
        self.0.read().clone()
    }

    #[inline]
    pub(crate) fn emit(
        &self,
        direction: WireDirection,
        transport: Transport,
        local: Option<SocketAddr>,
        remote: Option<SocketAddr>,
        bytes: &[u8],
    ) {
        let tap = match &*self.0.read() {
            Some(tap) => tap.clone(),
            None => return,
        };
        let unspecified = SocketAddr::from(([0, 0, 0, 0], 0));
        tap.on_wire(WireEvent {
            direction,
            transport,
            local: local.unwrap_or(unspecified),
            remote: remote.unwrap_or(unspecified),
            bytes,
        });
    }
}

impl core::fmt::Debug for WireTapSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WireTapSlot")
            .field("set", &self.0.read().is_some())
            .finish()
    }
}
