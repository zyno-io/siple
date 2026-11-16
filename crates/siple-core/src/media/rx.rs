//! Per-call RTP receive task (tokio): demux RTP/RTCP, SRTP/SRTCP unprotect,
//! symmetric-RTP latching, then hand packets to the media thread.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use crossbeam_channel::{Sender, TrySendError};
use siple_media::rtp::RtpHeader;
use siple_media::srtp::{SrtcpContext, SrtpContext, SrtpSuite};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc::UnboundedReceiver};

/// How long the previous inbound SRTP context stays valid after a remote rekey.
pub(crate) const OLD_KEY_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct RxPacket {
    pub seq: u16,
    pub ts: u32,
    pub marker: bool,
    pub ssrc: u32,
    pub pt: u8,
    pub payload: Vec<u8>,
    pub arrival: Instant,
}

pub(crate) enum RxMsg {
    Rtp(RxPacket),
    /// Plain (already unprotected) RTCP compound packet.
    Rtcp(Vec<u8>),
}

struct OldIn {
    rtp: SrtpContext,
    rtcp: SrtcpContext,
    until: Instant,
}

/// Inbound SRTP state: current contexts plus, shortly after a remote rekey,
/// the previous ones.
pub(crate) struct SrtpIn {
    pub rtp: SrtpContext,
    pub rtcp: SrtcpContext,
    old: Option<OldIn>,
}

impl SrtpIn {
    pub fn new(suite: SrtpSuite, key: &str) -> anyhow::Result<Self> {
        Ok(Self {
            rtp: SrtpContext::from_inline(suite, key)?,
            rtcp: SrtcpContext::from_inline(suite, key)?,
            old: None,
        })
    }

    /// Replace the current contexts by `next`, keeping `self`'s as the old
    /// pair for [`OLD_KEY_GRACE`].
    pub fn rotate(self, mut next: SrtpIn, now: Instant) -> SrtpIn {
        next.old = Some(OldIn {
            rtp: self.rtp,
            rtcp: self.rtcp,
            until: now + OLD_KEY_GRACE,
        });
        next
    }

    fn expire(&mut self, now: Instant) {
        if self.old.as_ref().is_some_and(|o| now >= o.until) {
            self.old = None;
        }
    }

    fn unprotect_rtp(&mut self, pkt: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.expire(now);
        match self.rtp.unprotect(pkt) {
            Ok(v) => Some(v),
            Err(_) => self.old.as_mut().and_then(|o| o.rtp.unprotect(pkt).ok()),
        }
    }

    fn unprotect_rtcp(&mut self, pkt: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.expire(now);
        match self.rtcp.unprotect_rtcp(pkt) {
            Ok(v) => Some(v),
            Err(_) => self
                .old
                .as_mut()
                .and_then(|o| o.rtcp.unprotect_rtcp(pkt).ok()),
        }
    }
}

/// How the RX task's inbound SRTP state changes on `apply()`.
pub(crate) enum SrtpUpdate {
    Keep,
    Clear,
    /// New contexts; any existing ones are kept as the old pair for the grace period.
    Replace(Box<SrtpIn>),
}

/// Configuration pushed from the media thread to the RX task on `apply()`.
/// The task owns all inbound state (including SRTP contexts), so the media
/// thread never takes a lock the task holds while doing crypto.
pub(crate) struct RxConfig {
    pub mux: bool,
    pub symmetric: bool,
    /// Forget the latched remote address (the negotiated remote moved).
    pub reset_latch: bool,
    /// Payload types that count as authenticated media when SRTP is off.
    pub audio_pts: Vec<u8>,
    pub srtp: SrtpUpdate,
}

/// What the media thread tells the RX task.
pub(crate) enum RxCtl {
    /// New negotiated parameters (see [`RxConfig`]).
    Config(RxConfig),
    /// The call's local address moved (network change): receive on these sockets from now on.
    /// SRTP contexts, the latched remote and the counters stay.
    Rebind(Arc<UdpSocket>, Arc<UdpSocket>),
}

/// Remote address latched by symmetric RTP; written by the RX task, read
/// lock-free by the media thread (cleared by the media thread on `apply()`).
pub(crate) type Latched = Arc<ArcSwapOption<SocketAddr>>;

/// RX-task-owned state.
#[derive(Default)]
struct RxState {
    /// False until the first config; earlier datagrams are dropped.
    ready: bool,
    mux: bool,
    symmetric: bool,
    latched: Option<SocketAddr>,
    audio_pts: Vec<u8>,
    srtp: Option<SrtpIn>,
}

#[derive(Default)]
pub(crate) struct RxCounters {
    pub rtp_packets: AtomicU64,
    pub rtp_bytes: AtomicU64,
    pub rtcp_packets: AtomicU64,
    pub auth_failures: AtomicU64,
    pub source_drops: AtomicU64,
    pub queue_drops: AtomicU64,
    pub early_drops: AtomicU64,
    pub malformed: AtomicU64,
}

/// RTCP demux on a muxed port (RFC 5761): PT field 64..=95 (RTCP 192..=223).
fn is_rtcp(buf: &[u8]) -> bool {
    buf.len() >= 2 && (64..=95).contains(&(buf[1] & 0x7f))
}

struct Ctx {
    state: RxState,
    latched_out: Latched,
    counters: Arc<RxCounters>,
    tx: Sender<RxMsg>,
}

impl Ctx {
    fn push(&self, msg: RxMsg) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(msg) {
            self.counters.queue_drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn configure(&mut self, cfg: RxConfig) {
        let st = &mut self.state;
        st.ready = true;
        st.mux = cfg.mux;
        st.symmetric = cfg.symmetric;
        if cfg.reset_latch {
            st.latched = None;
            self.latched_out.store(None);
        }
        st.audio_pts = cfg.audio_pts;
        match cfg.srtp {
            SrtpUpdate::Keep => {}
            SrtpUpdate::Clear => st.srtp = None,
            SrtpUpdate::Replace(new) => {
                st.srtp = Some(match st.srtp.take() {
                    Some(old) => old.rotate(*new, Instant::now()),
                    None => *new,
                });
            }
        }
    }

    fn on_rtp(&mut self, buf: &[u8], from: SocketAddr) {
        let now = Instant::now();
        let st = &mut self.state;
        if !st.ready {
            self.counters.early_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if st.symmetric && st.latched.is_some_and(|l| l != from) {
            self.counters.source_drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let (plain, authed): (std::borrow::Cow<[u8]>, bool) = match st.srtp.as_mut() {
            Some(s) => match s.unprotect_rtp(buf, now) {
                Some(v) => (v.into(), true),
                None => {
                    self.counters.auth_failures.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            },
            None => (buf.into(), false),
        };
        let Some(hdr) = RtpHeader::parse(&plain).filter(|h| h.version == 2) else {
            self.counters.malformed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if st.symmetric
            && st.latched.is_none()
            && (authed || st.audio_pts.contains(&hdr.payload_type))
        {
            st.latched = Some(from);
            self.latched_out.store(Some(Arc::new(from)));
        }
        let payload = hdr.payload(&plain).to_vec();
        self.counters.rtp_packets.fetch_add(1, Ordering::Relaxed);
        self.counters
            .rtp_bytes
            .fetch_add(payload.len() as u64, Ordering::Relaxed);
        self.push(RxMsg::Rtp(RxPacket {
            seq: hdr.sequence_number,
            ts: hdr.timestamp,
            marker: hdr.marker,
            ssrc: hdr.ssrc,
            pt: hdr.payload_type,
            payload,
            arrival: now,
        }));
    }

    fn on_rtcp(&mut self, buf: &[u8]) {
        let now = Instant::now();
        let st = &mut self.state;
        if !st.ready {
            return;
        }
        let plain = match st.srtp.as_mut() {
            Some(s) => match s.unprotect_rtcp(buf, now) {
                Some(v) => v,
                None => {
                    self.counters.auth_failures.fetch_add(1, Ordering::Relaxed);
                    return;
                }
            },
            None => buf.to_vec(),
        };
        self.counters.rtcp_packets.fetch_add(1, Ordering::Relaxed);
        self.push(RxMsg::Rtcp(plain));
    }

    fn on_rtp_socket(&mut self, buf: &[u8], from: SocketAddr) {
        if self.state.mux && is_rtcp(buf) {
            self.on_rtcp(buf);
        } else {
            self.on_rtp(buf, from);
        }
    }
}

/// Receive loop; ends when `stop` is notified. Config updates from the media
/// thread are always applied before the next datagram.
pub(crate) async fn run(
    mut rtp: Arc<UdpSocket>,
    mut rtcp: Arc<UdpSocket>,
    mut cfg_rx: UnboundedReceiver<RxCtl>,
    latched_out: Latched,
    counters: Arc<RxCounters>,
    tx: Sender<RxMsg>,
    stop: Arc<Notify>,
) {
    let mut ctx = Ctx {
        state: RxState::default(),
        latched_out,
        counters,
        tx,
    };
    let mut b1 = vec![0u8; 2048];
    let mut b2 = vec![0u8; 2048];
    let mut cfg_open = true;
    loop {
        tokio::select! {
            biased;
            _ = stop.notified() => break,
            c = cfg_rx.recv(), if cfg_open => match c {
                Some(RxCtl::Config(c)) => ctx.configure(c),
                Some(RxCtl::Rebind(a, b)) => {
                    rtp = a;
                    rtcp = b;
                }
                None => cfg_open = false,
            },
            r = rtp.recv_from(&mut b1) => match r {
                Ok((n, from)) => ctx.on_rtp_socket(&b1[..n], from),
                Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
            },
            r = rtcp.recv_from(&mut b2) => match r {
                Ok((n, _)) => ctx.on_rtcp(&b2[..n]),
                Err(_) => tokio::time::sleep(Duration::from_millis(2)).await,
            },
        }
    }
}
