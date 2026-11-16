//! Media abstraction used by calls. The core crate implements [`MediaProvider`] with the real
//! media engine; [`NullMediaProvider`] is a socket-only stand-in for tests.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use siple_config::{AudioSink, AudioSource};
use siple_media::sdp_neg::Negotiated;

/// What a call needs from the media layer when it starts.
#[derive(Debug, Clone)]
pub struct CallMediaRequest {
    /// Call id (`c1`, ...).
    pub call_id: String,
    /// Account id.
    pub account_id: String,
    /// Local address to bind the RTP socket on.
    pub bind_ip: IpAddr,
    /// Address written into SDP (`public_address` or the bind address).
    pub advertise_ip: IpAddr,
    /// Where transmitted audio comes from.
    pub source: AudioSource,
    /// Where received audio goes.
    pub sink: AudioSink,
    /// `source == Device || sink == Device`.
    pub device_bound: bool,
}

/// Media of one call. All methods are non-blocking.
pub trait CallMedia: Send + Sync {
    /// RTP port to advertise in SDP (allocated at creation).
    fn local_rtp_port(&self) -> u16;
    /// Applies a completed offer/answer (remote address, codec, SRTP keys, directions).
    fn apply(&self, negotiated: &Negotiated);
    /// Local hold: stop transmitting audio.
    fn set_hold(&self, held: bool);
    /// Local mute: transmit silence.
    fn set_mute(&self, muted: bool);
    /// Sends digits as RFC 4733 telephone events.
    fn send_dtmf_rfc4733(&self, digits: &str, duration_ms: u32, gap_ms: u32) -> anyhow::Result<()>;
    /// Sends digits as in-band tones.
    fn send_dtmf_inband(&self, digits: &str, duration_ms: u32, gap_ms: u32) -> anyhow::Result<()>;
    /// Makes this call the one bound to the audio devices.
    fn set_focus(&self);
    /// Moves the media to a new local address after a network change: allocates a new RTP/RTCP
    /// pair on `bind_ip`, switches transmit and receive to it and returns the new RTP port to
    /// advertise. Codec state, SRTP contexts, SSRC and the jitter buffer stay, so audio resumes
    /// without a re-key. `advertise_ip` is the address that goes into SDP (informational here).
    /// The default refuses, which ends the call with `network_lost`.
    fn rebind(&self, bind_ip: IpAddr, advertise_ip: IpAddr) -> anyhow::Result<u16> {
        let _ = (bind_ip, advertise_ip);
        Err(anyhow::anyhow!("this media cannot be rebound"))
    }
    /// Releases everything (idempotent).
    fn close(&self);
}

/// Creates the media of a call.
pub trait MediaProvider: Send + Sync {
    /// Allocates media for a call; called before the SDP offer/answer is generated.
    fn create(&self, req: CallMediaRequest) -> anyhow::Result<Arc<dyn CallMedia>>;

    /// Id of the call that currently owns the audio devices (the media focus), if any.
    fn focus(&self) -> Option<String> {
        None
    }
}

/// Observable state of a [`NullMediaProvider`] call.
#[derive(Debug, Clone, Default)]
pub struct NullMediaStats {
    /// RTP datagrams received on the call's socket.
    pub rx_packets: u64,
    /// Last negotiation passed to `apply`.
    pub negotiated: Option<Negotiated>,
    /// Number of `apply` calls.
    pub applies: u32,
    /// Local hold flag.
    pub held: bool,
    /// Local mute flag.
    pub muted: bool,
    /// Number of `set_focus` calls.
    pub focus: u32,
    /// Number of successful `rebind` calls.
    pub rebinds: u32,
    /// Current local RTP port (changes with `rebind`).
    pub rtp_port: u16,
    /// Address the current RTP socket is bound to.
    pub bind_ip: Option<IpAddr>,
    /// `close` was called.
    pub closed: bool,
    /// `(mode, digits)` of every DTMF send request (`mode` is `rfc4733` or `inband`).
    pub dtmf: Vec<(String, String)>,
    /// Requested source.
    pub source: Option<AudioSource>,
    /// Requested sink.
    pub sink: Option<AudioSink>,
}

struct NullShared {
    rx: AtomicU64,
    state: Mutex<NullMediaStats>,
    closed: AtomicBool,
}

/// Media provider for tests: binds a real UDP socket on `bind_ip` (so SDP carries a valid
/// port) and counts received RTP datagrams. It transmits only one empty RTP packet per new
/// remote address (as real phones start sending at once), which opens stateful firewalls and
/// ACLs that drop unsolicited inbound UDP.
#[derive(Default)]
pub struct NullMediaProvider {
    calls: Mutex<HashMap<String, Arc<NullShared>>>,
    /// Call that last got `set_focus` (cleared when it closes).
    focus: Arc<Mutex<Option<String>>>,
}

impl NullMediaProvider {
    /// New provider.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Snapshot of one call's media state (kept after `close`).
    pub fn stats(&self, call_id: &str) -> Option<NullMediaStats> {
        let shared = self
            .calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(call_id)
            .cloned()?;
        let mut st = shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        st.rx_packets = shared.rx.load(Ordering::Relaxed);
        Some(st)
    }

    /// Ids of all calls this provider created media for.
    pub fn call_ids(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

struct NullCallMedia {
    call_id: String,
    focus: Arc<Mutex<Option<String>>>,
    shared: Arc<NullShared>,
    /// Current socket (replaced by `rebind`; the receive task re-reads it every round).
    sock: Arc<Mutex<Arc<tokio::net::UdpSocket>>>,
}

impl NullCallMedia {
    fn with<T>(&self, f: impl FnOnce(&mut NullMediaStats) -> T) -> T {
        f(&mut self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
    }
}

impl CallMedia for NullCallMedia {
    fn local_rtp_port(&self) -> u16 {
        self.with(|s| s.rtp_port)
    }
    fn rebind(&self, bind_ip: IpAddr, _advertise_ip: IpAddr) -> anyhow::Result<u16> {
        let std_sock = std::net::UdpSocket::bind(SocketAddr::new(bind_ip, 0))?;
        std_sock.set_nonblocking(true)?;
        let port = std_sock.local_addr()?.port();
        let sock = Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
        *self.sock.lock().unwrap_or_else(PoisonError::into_inner) = sock;
        self.with(|s| {
            s.rebinds += 1;
            s.rtp_port = port;
            s.bind_ip = Some(bind_ip);
            // The next `apply` announces the new socket to the (unchanged) remote again.
            s.negotiated = None;
        });
        Ok(port)
    }
    fn apply(&self, negotiated: &Negotiated) {
        let new_remote = self.with(|s| {
            let changed =
                s.negotiated.as_ref().map(|n| n.remote_rtp) != Some(negotiated.remote_rtp);
            s.negotiated = Some(negotiated.clone());
            s.applies += 1;
            changed
        });
        if new_remote && !negotiated.remote_rtp.ip().is_unspecified() {
            // RTP v2 header only: marker set, seq 0, ts 0, SSRC 1.
            let mut pkt = [0u8; 12];
            pkt[0] = 0x80;
            pkt[1] = 0x80 | (negotiated.pt & 0x7f);
            pkt[11] = 1;
            let sock = self
                .sock
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let _ = sock.try_send_to(&pkt, negotiated.remote_rtp);
        }
    }
    fn set_hold(&self, held: bool) {
        self.with(|s| s.held = held);
    }
    fn set_mute(&self, muted: bool) {
        self.with(|s| s.muted = muted);
    }
    fn send_dtmf_rfc4733(&self, digits: &str, _d: u32, _g: u32) -> anyhow::Result<()> {
        self.with(|s| s.dtmf.push(("rfc4733".into(), digits.into())));
        Ok(())
    }
    fn send_dtmf_inband(&self, digits: &str, _d: u32, _g: u32) -> anyhow::Result<()> {
        self.with(|s| s.dtmf.push(("inband".into(), digits.into())));
        Ok(())
    }
    fn set_focus(&self) {
        self.with(|s| s.focus += 1);
        *self.focus.lock().unwrap_or_else(PoisonError::into_inner) = Some(self.call_id.clone());
    }
    fn close(&self) {
        {
            let mut f = self.focus.lock().unwrap_or_else(PoisonError::into_inner);
            if f.as_deref() == Some(self.call_id.as_str()) {
                *f = None;
            }
        }
        self.shared.closed.store(true, Ordering::SeqCst);
        self.with(|s| s.closed = true);
    }
}

impl MediaProvider for NullMediaProvider {
    fn create(&self, req: CallMediaRequest) -> anyhow::Result<Arc<dyn CallMedia>> {
        let std_sock = std::net::UdpSocket::bind(SocketAddr::new(req.bind_ip, 0))?;
        std_sock.set_nonblocking(true)?;
        let port = std_sock.local_addr()?.port();
        let sock = Arc::new(tokio::net::UdpSocket::from_std(std_sock)?);
        let shared = Arc::new(NullShared {
            rx: AtomicU64::new(0),
            state: Mutex::new(NullMediaStats {
                rtp_port: port,
                bind_ip: Some(req.bind_ip),
                source: Some(req.source.clone()),
                sink: Some(req.sink.clone()),
                ..Default::default()
            }),
            closed: AtomicBool::new(false),
        });
        let sock = Arc::new(Mutex::new(sock));
        let task_shared = shared.clone();
        let task_sock = sock.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while !task_shared.closed.load(Ordering::SeqCst) {
                let sock = task_sock
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone();
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sock.recv_from(&mut buf),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        task_shared.rx.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
        });
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(req.call_id.clone(), shared.clone());
        Ok(Arc::new(NullCallMedia {
            call_id: req.call_id,
            focus: self.focus.clone(),
            shared,
            sock,
        }))
    }

    fn focus(&self) -> Option<String> {
        self.focus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}
