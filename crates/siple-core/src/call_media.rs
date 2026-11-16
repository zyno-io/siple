//! The SIP layer's `MediaProvider` implemented on top of the [`MediaEngine`].
//!
//! [`EngineMediaProvider`] allocates one media-engine call per SIP call, using the *current*
//! `[media]` settings (so config changes apply to the next call), and records every handle in a
//! [`MediaRegistry`] keyed by call id (`cN`). The control API uses the registry for the
//! media-side test helpers (`call.play`, `call.record_*`, `call.detect_tone`, stats).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use arc_swap::ArcSwap;
use serde_json::{Value, json};
use siple_config::{AudioSink, AudioSource};
use siple_media::sdp_neg::Negotiated;
use siple_sip::{CallMedia, CallMediaRequest, MediaProvider};

use crate::config::Config;
use crate::media::{CallMediaHandle, CallMediaSpec, CallMediaStats, DtmfSendMode, MediaEngine};

/// Most calls the registry remembers; the oldest closed ones are dropped beyond it.
const REGISTRY_LIMIT: usize = 256;

/// What the registry knows about one call's media.
#[derive(Clone)]
pub struct MediaEntry {
    /// The engine handle.
    pub handle: CallMediaHandle,
    /// Source the media was created with.
    pub source: AudioSource,
    /// Sink the media was created with.
    pub sink: AudioSink,
    /// Source or sink is the device (competes for focus, gets local tones).
    pub device_bound: bool,
    /// `close` was called.
    pub closed: bool,
}

#[derive(Default)]
struct RegistryInner {
    map: HashMap<String, MediaEntry>,
    order: VecDeque<String>,
}

/// `cN` -> media handle. Shared by the provider (writer) and the backend / tone controller.
#[derive(Default)]
pub struct MediaRegistry {
    inner: Mutex<RegistryInner>,
}

impl MediaRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn insert(&self, call_id: &str, entry: MediaEntry) {
        let mut g = self.lock();
        if g.map.insert(call_id.to_string(), entry).is_none() {
            g.order.push_back(call_id.to_string());
        }
        while g.map.len() > REGISTRY_LIMIT {
            let Some(pos) = g
                .order
                .iter()
                .position(|id| g.map.get(id).is_none_or(|e| e.closed))
            else {
                break;
            };
            if let Some(id) = g.order.remove(pos) {
                g.map.remove(&id);
            }
        }
    }

    fn mark_closed(&self, call_id: &str, handle: &CallMediaHandle) {
        if let Some(e) = self.lock().map.get_mut(call_id) {
            // A re-created media (answer with another source / sink) replaces the entry; only
            // flag the handle that was actually closed.
            if e.handle.local_rtp_port() == handle.local_rtp_port() {
                e.closed = true;
            }
        }
    }

    /// The entry of a call.
    pub fn get(&self, call_id: &str) -> Option<MediaEntry> {
        self.lock().map.get(call_id).cloned()
    }

    /// Whether the call is device-bound (false for unknown calls).
    pub fn device_bound(&self, call_id: &str) -> bool {
        self.lock().map.get(call_id).is_some_and(|e| e.device_bound)
    }
}

/// Bridges SIP calls to the media engine.
pub struct EngineMediaProvider {
    /// The engine that owns sockets, codecs and audio routing.
    pub engine: Arc<MediaEngine>,
    /// Live config snapshot; `.media` (and the account's `hold_send_cn`) is read when a call is
    /// created.
    pub config: Arc<ArcSwap<Config>>,
    /// Handles of all calls created through this provider.
    pub registry: Arc<MediaRegistry>,
}

impl EngineMediaProvider {
    /// New provider with an empty registry.
    pub fn new(engine: Arc<MediaEngine>, config: Arc<ArcSwap<Config>>) -> Arc<Self> {
        Arc::new(Self {
            engine,
            config,
            registry: Arc::new(MediaRegistry::default()),
        })
    }
}

struct EngineCallMedia {
    call_id: String,
    handle: CallMediaHandle,
    registry: Arc<MediaRegistry>,
}

impl CallMedia for EngineCallMedia {
    fn local_rtp_port(&self) -> u16 {
        self.handle.local_rtp_port()
    }
    fn apply(&self, negotiated: &Negotiated) {
        self.handle.apply(negotiated.clone());
    }
    fn set_hold(&self, held: bool) {
        self.handle.set_hold(held);
    }
    fn set_mute(&self, muted: bool) {
        self.handle.set_mute(muted);
    }
    fn send_dtmf_rfc4733(&self, digits: &str, duration_ms: u32, gap_ms: u32) -> anyhow::Result<()> {
        self.handle
            .send_dtmf(digits, DtmfSendMode::Rfc4733, duration_ms, gap_ms)
    }
    fn send_dtmf_inband(&self, digits: &str, duration_ms: u32, gap_ms: u32) -> anyhow::Result<()> {
        self.handle
            .send_dtmf(digits, DtmfSendMode::Inband, duration_ms, gap_ms)
    }
    fn set_focus(&self) {
        self.handle.set_focus();
    }
    fn rebind(
        &self,
        bind_ip: std::net::IpAddr,
        advertise_ip: std::net::IpAddr,
    ) -> anyhow::Result<u16> {
        self.handle.rebind(bind_ip, advertise_ip)
    }
    fn close(&self) {
        self.registry.mark_closed(&self.call_id, &self.handle);
        self.handle.close();
    }
}

impl MediaProvider for EngineMediaProvider {
    fn create(&self, req: CallMediaRequest) -> anyhow::Result<Arc<dyn CallMedia>> {
        let cfg = self.config.load();
        let media = &cfg.media;
        let hold_send_cn = cfg
            .accounts
            .iter()
            .find(|a| a.id == req.account_id)
            .is_some_and(|a| a.hold_send_cn);
        let mut spec = CallMediaSpec::new(req.call_id.clone(), req.bind_ip, req.advertise_ip);
        spec.port_range = media.rtp_port_range;
        spec.source = req.source.clone();
        spec.sink = req.sink.clone();
        spec.device_bound = req.device_bound;
        spec.symmetric_rtp = media.symmetric_rtp;
        spec.hold_send_cn = hold_send_cn;
        spec.jitter_min_ms = media.jitter_min_ms;
        spec.jitter_max_ms = media.jitter_max_ms;
        spec.rx_history_secs = media.rx_history_secs;
        // cname stays `<call_id>@siple` (CallMediaSpec::new).
        let handle = self.engine.create_call(spec)?;
        self.registry.insert(
            &req.call_id,
            MediaEntry {
                handle: handle.clone(),
                source: req.source,
                sink: req.sink,
                device_bound: req.device_bound,
                closed: false,
            },
        );
        Ok(Arc::new(EngineCallMedia {
            call_id: req.call_id,
            handle,
            registry: self.registry.clone(),
        }))
    }

    fn focus(&self) -> Option<String> {
        self.engine.focus()
    }
}

/// JSON view of a stats snapshot (`call.get` -> `media`).
pub fn stats_json(s: &CallMediaStats) -> Value {
    json!({
        "codec": s.codec.map(|c| c.sdp_encoding_name().to_ascii_lowercase()),
        "pt": s.pt,
        "ptime_ms": s.ptime_ms,
        "srtp_suite": s.srtp_suite.map(|x| x.sdp_name()),
        "remote_rtp": s.remote_rtp.map(|a| a.to_string()),
        "tx_packets": s.tx_packets,
        "tx_bytes": s.tx_bytes,
        "tx_dropped": s.tx_dropped,
        "tx_errors": s.tx_errors,
        "rx_packets": s.rx_packets,
        "rx_bytes": s.rx_bytes,
        "rx_unknown_pt": s.rx_unknown_pt,
        "rx_queue_drops": s.rx_queue_drops,
        "rx_source_drops": s.rx_source_drops,
        "rx_early_drops": s.rx_early_drops,
        "rtcp_rx_packets": s.rtcp_rx_packets,
        "srtp_auth_failures": s.srtp_auth_failures,
        "rx_active": s.rx_active,
        "rx_idle_ms": s.last_rx_at.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)),
        "rx_level_dbfs": finite(s.rx_level_dbfs),
        "tx_paused": s.tx_paused,
        "muted": s.muted,
        "held": s.held,
        "jitter": {
            "received": s.jitter.received,
            "lost": s.jitter.lost,
            "late": s.jitter.late,
            "duplicate": s.jitter.duplicate,
            "concealed": s.jitter.concealed,
            "current_delay_ms": s.jitter.current_delay_ms,
            "target_delay_ms": s.jitter.target_delay_ms,
            "jitter_ms": finite(s.jitter.jitter_ms),
        },
        "rtcp": {
            "rx_cumulative_lost": s.rtcp.rx_cumulative_lost,
            "rx_jitter_ms": finite(s.rtcp.rx_jitter_ms),
            "remote_loss": s.rtcp.remote_loss,
            "remote_jitter_ms": s.rtcp.remote_jitter_ms.and_then(finite),
            "rtt_ms": s.rtcp.rtt_ms,
        },
    })
}

/// JSON cannot carry NaN / infinity (silence is -inf dBFS): those become `null`.
fn finite(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}
