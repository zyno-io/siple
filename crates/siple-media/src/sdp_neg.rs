//! SDP offer/answer for a single audio stream (RFC 3264) with SDES SRTP.
//!
//! [`MediaSession`] holds the state that must stay stable for the life of a
//! call: `o=` session id/version, local SRTP keys, local hold state and the
//! last negotiated result. The `o=` version is bumped only when the SDP body
//! (everything except the `o=` line) changes, so a session refresh gets an
//! identical answer.

use std::net::{IpAddr, SocketAddr};

use crate::codec::{AudioCodec, clamp_opus_ptime};
use crate::sdp::{CryptoLine, Direction, MediaDescription, SessionDescription};
use crate::srtp::{SrtpContext, SrtpKeyMaterial, SrtpSuite};

/// Local SRTP policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SrtpPolicy {
    /// `RTP/AVP`, no crypto; SAVP-only offers are rejected (488).
    #[default]
    Off,
    /// Offer `RTP/AVP` + `a=crypto` (OSRTP, RFC 8643); use SRTP when the peer
    /// supplies acceptable crypto.
    Optional,
    /// Offer `RTP/SAVP`; 488 when the peer offers no acceptable crypto.
    Mandatory,
}

/// Which codec to choose when answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnswerOrder {
    /// First codec in the offer's order that we support.
    #[default]
    Offer,
    /// First codec in our list that is present in the offer.
    Local,
}

/// Opus `a=fmtp` parameters (RFC 7587). Local values describe what we can
/// *receive*; remote values (see [`Negotiated::opus_remote`]) constrain what
/// we send. We always signal `stereo=0;sprop-stereo=0` (mono).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusSdpParams {
    pub useinbandfec: bool,
    pub maxplaybackrate: u32,
    pub maxaveragebitrate: Option<u32>,
}

impl Default for OpusSdpParams {
    fn default() -> Self {
        Self {
            useinbandfec: true,
            maxplaybackrate: 48000,
            maxaveragebitrate: None,
        }
    }
}

impl OpusSdpParams {
    fn fmtp(&self) -> String {
        let mut s = format!(
            "useinbandfec={};maxplaybackrate={};stereo=0;sprop-stereo=0",
            self.useinbandfec as u8, self.maxplaybackrate
        );
        if let Some(b) = self.maxaveragebitrate {
            s.push_str(&format!(";maxaveragebitrate={b}"));
        }
        s
    }

    /// Parse the remote's fmtp for `pt`; absent parameters take RFC 7587 defaults.
    fn from_remote(m: &MediaDescription, pt: u8) -> Self {
        Self {
            useinbandfec: m.fmtp_param(pt, "useinbandfec") == Some("1"),
            maxplaybackrate: m
                .fmtp_param(pt, "maxplaybackrate")
                .and_then(|v| v.parse().ok())
                .unwrap_or(48000),
            maxaveragebitrate: m
                .fmtp_param(pt, "maxaveragebitrate")
                .and_then(|v| v.parse().ok()),
        }
    }
}

/// Local capabilities / configuration for SDP generation and selection.
#[derive(Debug, Clone)]
pub struct LocalCaps {
    /// Codecs in preference order.
    pub codecs: Vec<AudioCodec>,
    pub telephone_event: bool,
    pub comfort_noise: bool,
    pub srtp: SrtpPolicy,
    /// Suites we offer / accept, in preference order.
    pub srtp_suites: Vec<SrtpSuite>,
    pub rtcp_mux: bool,
    /// Preferred packet time (10..=60). Snapped to what the negotiated codec
    /// can encode (Opus: 10/20/40/60).
    pub ptime_ms: Option<u32>,
    pub answer_order: AnswerOrder,
    pub opus: OpusSdpParams,
    /// Dynamic PT used for Opus in our offers (96..=127).
    pub opus_pt: u8,
    /// Dynamic PT of telephone-event/8000 in our offers; `/48000` uses `te_pt + 1`.
    ///
    /// [`MediaSession::new`] validates `opus_pt`/`te_pt`: values outside
    /// 96..=127 or colliding with each other are automatically reassigned to
    /// free dynamic PTs; [`MediaSession::caps`] returns the effective values.
    pub te_pt: u8,
}

impl Default for LocalCaps {
    fn default() -> Self {
        Self {
            codecs: vec![
                AudioCodec::Opus,
                AudioCodec::G722,
                AudioCodec::Pcmu,
                AudioCodec::Pcma,
            ],
            telephone_event: true,
            comfort_noise: false,
            srtp: SrtpPolicy::Off,
            srtp_suites: SrtpSuite::ALL.to_vec(),
            rtcp_mux: false,
            ptime_ms: None,
            answer_order: AnswerOrder::Offer,
            opus: OpusSdpParams::default(),
            opus_pt: 111,
            te_pt: 101,
        }
    }
}

/// Local hold state (what we want our direction to be).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalHold {
    #[default]
    None,
    SendOnly,
    Inactive,
}

impl LocalHold {
    fn direction(self) -> Direction {
        match self {
            Self::None => Direction::SendRecv,
            Self::SendOnly => Direction::SendOnly,
            Self::Inactive => Direction::Inactive,
        }
    }
}

/// Negotiation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegError {
    /// Respond 488 Not Acceptable Here.
    NotAcceptable(String),
    /// Respond 400 Bad Request.
    Malformed(String),
}

impl NegError {
    /// SIP status code to use.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotAcceptable(_) => 488,
            Self::Malformed(_) => 400,
        }
    }
}

impl std::fmt::Display for NegError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAcceptable(s) => write!(f, "not acceptable: {s}"),
            Self::Malformed(s) => write!(f, "malformed SDP: {s}"),
        }
    }
}
impl std::error::Error for NegError {}

/// Negotiated SRTP parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrtpParams {
    pub suite: SrtpSuite,
    /// Our key: plain base64 of master key || salt
    /// (use with [`SrtpContext::from_sdes_key`]).
    pub local_key_b64: String,
    /// The remote's key-params as written in SDP, e.g. `inline:KEY|2^31`
    /// (use with [`SrtpContext::from_inline`]).
    pub remote_key_b64: String,
    /// Remote key (or suite) differs from the previous negotiation: the engine
    /// should build a new inbound context and keep the old one ~2 s.
    pub remote_changed: bool,
}

/// Result of a completed offer/answer.
///
/// `pt`, `te` and `cn_pt` are the payload types **we send** with. When we are
/// the offerer the remote answerer sends using the PTs from *our* offer (RFC
/// 3264 §6.1), which can differ from the ones in its answer; those are the
/// `rx_*` fields. When we are the answerer both directions use the offer's
/// PTs, so `rx_*` equals the tx fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    pub codec: AudioCodec,
    /// Payload type we send audio with.
    pub pt: u8,
    /// RTP clock rate of `pt`.
    pub clock_rate: u32,
    pub remote_rtp: SocketAddr,
    /// `None` when RTCP is multiplexed on the RTP port.
    pub remote_rtcp: Option<SocketAddr>,
    pub rtcp_mux: bool,
    /// Packetization time **we send** with (always encodable by `codec`:
    /// Opus 10/20/40/60, others 10..=60 in steps of 10; capped by the
    /// remote's `a=maxptime`). Default 20.
    pub ptime_ms: u32,
    /// telephone-event `(pt, clock_rate)` we send with.
    pub te: Option<(u8, u32)>,
    /// Comfort-noise PT we send with.
    pub cn_pt: Option<u8>,
    pub srtp: Option<SrtpParams>,
    /// Direction written in our most recent SDP.
    pub local_direction: Direction,
    pub send_enabled: bool,
    pub recv_enabled: bool,
    /// Remote sendonly/inactive, or `c=0.0.0.0`.
    pub remote_hold: bool,
    /// Whether `remote_hold` is meaningful. `false` for the answer to an offer of ours that
    /// could not receive (`sendonly` / `inactive`): the remote's `inactive` / `recvonly` then
    /// only mirrors our offer and says nothing about it holding us.
    pub remote_hold_known: bool,
    /// Remote's Opus fmtp (only when `codec == Opus`).
    pub opus_remote: Option<OpusSdpParams>,
    /// Primary payload type the remote sends audio to us with.
    pub rx_pt: u8,
    /// telephone-event `(pt, clock_rate)` the remote sends to us.
    pub rx_te: Option<(u8, u32)>,
    /// Comfort-noise PT the remote sends to us.
    pub rx_cn_pt: Option<u8>,
    /// Further payload types (from either SDP) that map to `codec`; the
    /// engine should also accept these on receive. Excludes `rx_pt`.
    pub rx_alt_pts: Vec<u8>,
}

// ── internal description of what we emit ────────────────────────────────

#[derive(Debug, Clone)]
struct PtEntry {
    pt: u8,
    name: &'static str,
    clock: u32,
    channels: Option<u8>,
    fmtp: Option<String>,
}

#[derive(Debug, Clone)]
struct AudioOut {
    proto: String,
    pts: Vec<PtEntry>,
    ptime: Option<u32>,
    mux: bool,
    crypto: Vec<(u32, SrtpSuite)>,
}

#[derive(Debug, Clone)]
enum OutMedia {
    Audio(AudioOut),
    Rejected {
        kind: String,
        proto: String,
        fmts: String,
    },
}

fn random_u32() -> u32 {
    use rand::TryRng;
    let mut b = [0u8; 4];
    rand::rngs::SysRng
        .try_fill_bytes(&mut b)
        .expect("OS CSPRNG failure");
    u32::from_be_bytes(b)
}

fn valid_ptime(p: u32) -> bool {
    (10..=60).contains(&p)
}

/// Snap a wanted packet time to one `codec` can encode, not above `max`
/// (the remote's maxptime). Opus: 10/20/40/60 via [`clamp_opus_ptime`]
/// (rounding down to the largest allowed size when capped); G.711/G.722:
/// multiples of 10 in 10..=60.
fn snap_ptime(codec: AudioCodec, want: u32, max: Option<u32>) -> u32 {
    let cap = max.filter(|&m| m > 0).unwrap_or(60);
    if codec == AudioCodec::Opus {
        let p = clamp_opus_ptime(want);
        if p <= cap {
            p
        } else {
            [60, 40, 20, 10]
                .into_iter()
                .find(|&c| c <= cap)
                .unwrap_or(10)
        }
    } else {
        let p = ((want + 4) / 10 * 10).clamp(10, 60);
        if p <= cap {
            p
        } else {
            (cap / 10 * 10).clamp(10, 60)
        }
    }
}

/// Send ptime for `codec`/`pt` given the remote's media section (maxptime
/// from `a=maxptime` and the Opus-style `maxptime=` fmtp parameter).
fn tx_ptime(codec: AudioCodec, want: u32, m: &MediaDescription, pt: u8) -> u32 {
    let fmtp_max = m
        .fmtp_param(pt, "maxptime")
        .and_then(|v| v.parse::<u32>().ok());
    let max = [m.maxptime, fmtp_max].into_iter().flatten().min();
    snap_ptime(codec, want, max)
}

/// Whether we can honor every SDES session parameter on `c`. Only `KDR=0`
/// (equivalent to none) is accepted; UNENCRYPTED_SRTP/SRTCP,
/// UNAUTHENTICATED_SRTP, non-zero KDR, FEC_* and WSH are not implemented.
fn crypto_params_supported(c: &CryptoLine) -> bool {
    c.session_params.iter().all(|p| {
        p.split_once('=').is_some_and(|(k, v)| {
            k.eq_ignore_ascii_case("KDR") && v.trim().parse::<u32>().is_ok_and(|n| n == 0)
        })
    })
}

/// Dynamic PTs of `pts` (our offer) that carry `codec`.
fn our_codec_pts(pts: &[PtEntry], codec: AudioCodec) -> Vec<u8> {
    pts.iter()
        .filter(|p| {
            p.name.eq_ignore_ascii_case(codec.sdp_encoding_name())
                && p.clock == codec.rtp_clock_rate()
        })
        .map(|p| p.pt)
        .collect()
}

/// Make `opus_pt`/`te_pt` valid and non-colliding (see [`LocalCaps::te_pt`]).
fn sanitize_pts(caps: &mut LocalCaps) {
    let dynamic = |p: u8| (96..=127).contains(&p);
    let free_single = |used: &[u8]| (96u8..=127).rev().find(|p| !used.contains(p));
    let mut used: Vec<u8> = Vec::new();
    let has_opus = caps.codecs.contains(&AudioCodec::Opus);
    if has_opus {
        if !dynamic(caps.opus_pt) {
            caps.opus_pt = if dynamic(111) { 111 } else { 96 };
        }
        used.push(caps.opus_pt);
    }
    if caps.telephone_event {
        let n = if has_opus { 2 } else { 1 };
        let ok = |t: u8| {
            (0..n).all(|i| {
                t.checked_add(i)
                    .is_some_and(|p| dynamic(p) && !used.contains(&p))
            })
        };
        if !ok(caps.te_pt) {
            caps.te_pt = (96u8..=127 - (n - 1))
                .rev()
                .find(|&t| ok(t))
                .or_else(|| free_single(&used))
                .unwrap_or(96);
        }
    }
}

fn codec_entry(codec: AudioCodec, pt: u8, opus: &OpusSdpParams) -> PtEntry {
    PtEntry {
        pt,
        name: codec.sdp_encoding_name(),
        // rtpmap clock in SDP: G.722 is advertised as 8000.
        clock: codec.rtp_clock_rate(),
        channels: codec.sdp_channels(),
        fmtp: (codec == AudioCodec::Opus).then(|| opus.fmtp()),
    }
}

fn te_entry(pt: u8, clock: u32) -> PtEntry {
    PtEntry {
        pt,
        name: "telephone-event",
        clock,
        channels: None,
        fmtp: Some("0-16".into()),
    }
}

fn cn_entry(pt: u8) -> PtEntry {
    PtEntry {
        pt,
        name: "CN",
        clock: 8000,
        channels: None,
        fmtp: None,
    }
}

/// Codec described by `pt` in `m`, if it is one we know (name + clock check).
fn codec_of(m: &MediaDescription, pt: u8) -> Option<AudioCodec> {
    let map = m.rtpmap_for(pt)?;
    let codec = AudioCodec::from_name(&map.name)?;
    (map.clock == codec.rtp_clock_rate()).then_some(codec)
}

fn is_name(m: &MediaDescription, pt: u8, name: &str) -> Option<u32> {
    let map = m.rtpmap_for(pt)?;
    map.name.eq_ignore_ascii_case(name).then_some(map.clock)
}

/// Pick the telephone-event PT matching `codec`'s clock.
fn pick_te(m: &MediaDescription, codec: AudioCodec) -> Option<(u8, u32)> {
    let tes: Vec<(u8, u32)> = m
        .fmts
        .iter()
        .filter_map(|&pt| is_name(m, pt, "telephone-event").map(|c| (pt, c)))
        .collect();
    let want: &[u32] = if codec == AudioCodec::Opus {
        &[48000, 8000]
    } else {
        &[8000]
    };
    want.iter()
        .find_map(|c| tes.iter().copied().find(|&(_, clk)| clk == *c))
}

fn pick_cn(m: &MediaDescription, codec: AudioCodec) -> Option<u8> {
    if codec == AudioCodec::Opus {
        return None;
    }
    m.fmts
        .iter()
        .copied()
        .find(|&pt| is_name(m, pt, "CN") == Some(8000))
}

fn is_unspecified(ip: IpAddr) -> bool {
    ip.is_unspecified()
}

/// Our direction = our wishes intersected with what the remote permits.
/// `c=0.0.0.0` means the remote does not want to receive: only its receive
/// side is cleared (sendrecv -> sendonly, recvonly -> inactive).
fn intersect(hold: LocalHold, remote: Direction, zero_conn: bool) -> Direction {
    let remote = Direction::from_flags(remote.sends(), remote.receives() && !zero_conn);
    let want = hold.direction();
    Direction::from_flags(
        want.sends() && remote.receives(),
        want.receives() && remote.sends(),
    )
}

/// Stable per-call SDP/negotiation state.
pub struct MediaSession {
    caps: LocalCaps,
    sess_id: u64,
    sess_version: u64,
    /// Last emitted SDP body, excluding the `o=` line.
    last_body: Option<String>,
    local_ip: IpAddr,
    /// Address written in `o=`; fixed for the session lifetime.
    origin_ip: IpAddr,
    rtp_port: u16,
    /// Index of the audio m-line in the outstanding/current offer-answer.
    audio_idx: usize,
    keys: Vec<(SrtpSuite, SrtpKeyMaterial)>,
    hold: LocalHold,
    negotiated: Option<Negotiated>,
    /// What our current SDP says (re-rendered on hold/address changes).
    desc: Vec<OutMedia>,
}

impl MediaSession {
    /// `caps.opus_pt`/`te_pt` are validated and auto-reassigned if invalid or
    /// colliding (see [`LocalCaps::te_pt`]).
    pub fn new(mut caps: LocalCaps, local_ip: IpAddr, rtp_port: u16) -> Self {
        sanitize_pts(&mut caps);
        let mut keys: Vec<(SrtpSuite, SrtpKeyMaterial)> = Vec::new();
        if caps.srtp != SrtpPolicy::Off {
            for &s in &caps.srtp_suites {
                if !keys.iter().any(|(k, _)| *k == s) {
                    keys.push((s, SrtpKeyMaterial::generate(s)));
                }
            }
        }
        Self {
            caps,
            sess_id: random_u32() as u64 + 1,
            sess_version: 1,
            last_body: None,
            local_ip,
            origin_ip: local_ip,
            rtp_port,
            audio_idx: 0,
            keys,
            hold: LocalHold::None,
            negotiated: None,
            desc: Vec::new(),
        }
    }

    pub fn sess_id(&self) -> u64 {
        self.sess_id
    }

    pub fn sess_version(&self) -> u64 {
        self.sess_version
    }

    pub fn caps(&self) -> &LocalCaps {
        &self.caps
    }

    pub fn negotiated(&self) -> Option<&Negotiated> {
        self.negotiated.as_ref()
    }

    pub fn local_hold(&self) -> LocalHold {
        self.hold
    }

    pub fn local_addr(&self) -> (IpAddr, u16) {
        (self.local_ip, self.rtp_port)
    }

    /// RTCP port we advertise (RTP + 1 unless multiplexed).
    pub fn local_rtcp_port(&self) -> u16 {
        self.rtp_port.wrapping_add(1)
    }

    pub fn set_local_address(&mut self, ip: IpAddr, rtp_port: u16) {
        self.local_ip = ip;
        self.rtp_port = rtp_port;
    }

    /// The local media address moved (network change): the next SDP carries the new `c=` and
    /// `m=` port (and `a=rtcp`), and its `o=` version is bumped because the body changed (RFC
    /// 3264 §8). The `o=` address stays (it only names the originator), and the hold direction,
    /// codecs and SDES keys are kept, so a re-offer after a rebind neither re-keys SRTP nor
    /// drops a hold.
    pub fn rebind(&mut self, ip: IpAddr, rtp_port: u16) {
        self.set_local_address(ip, rtp_port);
    }

    /// Change the local hold state; takes effect in the next SDP we emit.
    pub fn set_local_hold(&mut self, hold: LocalHold) {
        self.hold = hold;
    }

    fn key_for(&self, suite: SrtpSuite) -> Option<String> {
        self.keys
            .iter()
            .find(|(s, _)| *s == suite)
            .map(|(_, k)| k.to_base64())
    }

    // ── emission ────────────────────────────────────────────────────────

    fn render(&mut self, dir: Direction) -> String {
        let fam = if self.local_ip.is_ipv4() {
            "IP4"
        } else {
            "IP6"
        };
        let mut body = format!("s=-\r\nc=IN {fam} {}\r\nt=0 0\r\n", self.local_ip);
        for m in &self.desc {
            match m {
                OutMedia::Rejected { kind, proto, fmts } => {
                    body.push_str(&format!("m={kind} 0 {proto} {fmts}\r\n"));
                }
                OutMedia::Audio(a) => {
                    let fmts: Vec<String> = a.pts.iter().map(|p| p.pt.to_string()).collect();
                    body.push_str(&format!(
                        "m=audio {} {} {}\r\n",
                        self.rtp_port,
                        a.proto,
                        fmts.join(" ")
                    ));
                    for p in &a.pts {
                        match p.channels {
                            Some(c) => body.push_str(&format!(
                                "a=rtpmap:{} {}/{}/{}\r\n",
                                p.pt, p.name, p.clock, c
                            )),
                            None => body
                                .push_str(&format!("a=rtpmap:{} {}/{}\r\n", p.pt, p.name, p.clock)),
                        }
                        if let Some(f) = &p.fmtp {
                            body.push_str(&format!("a=fmtp:{} {}\r\n", p.pt, f));
                        }
                    }
                    if let Some(pt) = a.ptime {
                        body.push_str(&format!("a=ptime:{pt}\r\n"));
                    }
                    body.push_str(&format!("a={}\r\n", dir.as_str()));
                    if a.mux {
                        body.push_str("a=rtcp-mux\r\n");
                    } else {
                        body.push_str(&format!("a=rtcp:{}\r\n", self.local_rtcp_port()));
                    }
                    for &(tag, suite) in &a.crypto {
                        if let Some(k) = self.key_for(suite) {
                            body.push_str(&format!(
                                "a=crypto:{tag} {} inline:{k}\r\n",
                                suite.sdp_name()
                            ));
                        }
                    }
                }
            }
        }
        if self.last_body.as_deref().is_some_and(|b| b != body) {
            self.sess_version += 1;
        }
        let ofam = if self.origin_ip.is_ipv4() {
            "IP4"
        } else {
            "IP6"
        };
        let o = format!(
            "v=0\r\no=- {} {} IN {ofam} {}\r\n",
            self.sess_id, self.sess_version, self.origin_ip
        );
        let out = format!("{o}{body}");
        self.last_body = Some(body);
        out
    }

    fn build_offer_media(&self) -> AudioOut {
        let c = &self.caps;
        let mut pts: Vec<PtEntry> = Vec::new();
        let mut have_opus = false;
        for &codec in &c.codecs {
            let pt = codec.static_payload_type().unwrap_or(c.opus_pt);
            if pts.iter().any(|p| p.pt == pt) {
                continue;
            }
            have_opus |= codec == AudioCodec::Opus;
            pts.push(codec_entry(codec, pt, &c.opus));
        }
        if c.telephone_event {
            pts.push(te_entry(c.te_pt, 8000));
            if have_opus {
                pts.push(te_entry(c.te_pt.wrapping_add(1), 48000));
            }
        }
        if c.comfort_noise {
            pts.push(cn_entry(13));
        }
        let mut crypto = Vec::new();
        if c.srtp != SrtpPolicy::Off {
            for (i, (s, _)) in self.keys.iter().enumerate() {
                crypto.push((i as u32 + 1, *s));
            }
        }
        let proto = match (c.srtp, crypto.is_empty()) {
            (SrtpPolicy::Mandatory, false) => "RTP/SAVP",
            _ => "RTP/AVP",
        };
        AudioOut {
            proto: proto.into(),
            pts,
            ptime: c.ptime_ms.filter(|&p| valid_ptime(p)).map(|p| {
                snap_ptime(
                    c.codecs.first().copied().unwrap_or(AudioCodec::Pcmu),
                    p,
                    None,
                )
            }),
            mux: c.rtcp_mux,
            crypto,
        }
    }

    /// Build a fresh offer advertising all local capabilities.
    pub fn create_offer(&mut self) -> String {
        let a = self.build_offer_media();
        self.desc = vec![OutMedia::Audio(a)];
        self.audio_idx = 0;
        self.render(self.hold.direction())
    }

    /// Re-offer the current state (SDP-less re-INVITE / UPDATE, or an offer in
    /// a 2xx). Falls back to a fresh offer if nothing has been negotiated.
    pub fn current_sdp(&mut self) -> String {
        if self.desc.is_empty() {
            return self.create_offer();
        }
        self.render(self.hold.direction())
    }

    // ── applying an answer ──────────────────────────────────────────────

    pub fn apply_answer(&mut self, answer: &SessionDescription) -> Result<Negotiated, NegError> {
        let Some(OutMedia::Audio(ours)) = self.desc.get(self.audio_idx).cloned() else {
            return Err(NegError::Malformed(
                "answer without an outstanding offer".into(),
            ));
        };
        let m = answer
            .media
            .get(self.audio_idx)
            .ok_or_else(|| NegError::Malformed("answer has no media".into()))?;
        if !m.is_audio() {
            return Err(NegError::Malformed("answered media is not audio".into()));
        }
        if m.port == 0 {
            return Err(NegError::NotAcceptable(
                "remote rejected the audio stream".into(),
            ));
        }
        if !m.proto.is_rtp() {
            return Err(NegError::NotAcceptable(format!(
                "unsupported transport {}",
                m.proto.as_str()
            )));
        }
        let ip = answer
            .connection_for(m)
            .ok_or_else(|| NegError::Malformed("answer has no connection address".into()))?;

        // Codec: first answered format we support (and offered).
        let (pt, codec) = m
            .fmts
            .iter()
            .find_map(|&pt| {
                let c = codec_of(m, pt)?;
                self.caps.codecs.contains(&c).then_some((pt, c))
            })
            .ok_or_else(|| NegError::NotAcceptable("no acceptable codec in answer".into()))?;

        // SRTP.
        let srtp_sel = if m.crypto.is_empty() || ours.crypto.is_empty() {
            None
        } else {
            let c = &m.crypto[0];
            let suite = SrtpSuite::from_sdp_name(&c.suite).ok_or_else(|| {
                NegError::NotAcceptable(format!("answer crypto suite {} unsupported", c.suite))
            })?;
            if !ours.crypto.contains(&(c.tag, suite)) {
                return Err(NegError::NotAcceptable(format!(
                    "answer crypto tag {} / {} was not offered",
                    c.tag, c.suite
                )));
            }
            if !crypto_params_supported(c) {
                return Err(NegError::NotAcceptable(format!(
                    "answer crypto session params {:?} unsupported",
                    c.session_params
                )));
            }
            let key = self.validated_remote_key(suite, c)?;
            Some((suite, key))
        };
        if srtp_sel.is_none() {
            if self.caps.srtp == SrtpPolicy::Mandatory {
                return Err(NegError::NotAcceptable("answer lacks required SRTP".into()));
            }
            if m.proto.is_secure() {
                return Err(NegError::NotAcceptable("SAVP answer without crypto".into()));
            }
        }

        let zero = is_unspecified(ip);
        let dir = intersect(self.hold, m.direction, zero);
        let our_dir = self.hold.direction();
        let want = m
            .ptime
            .filter(|&p| valid_ptime(p))
            .or(self.caps.ptime_ms.filter(|&p| valid_ptime(p)))
            .unwrap_or(20);
        let ptime = tx_ptime(codec, want, m, pt);
        let mut neg = self.finish(
            m,
            ip,
            codec,
            pt,
            srtp_sel,
            ptime,
            our_dir,
            dir,
            zero,
            self.caps.rtcp_mux && m.rtcp_mux,
        );
        // The answerer sends with the PTs of *our* offer (RFC 3264 §6.1).
        let ours_codec = our_codec_pts(&ours.pts, codec);
        if !our_dir.receives() && !zero {
            neg.remote_hold_known = false;
        }
        neg.rx_pt = ours_codec.first().copied().unwrap_or(pt);
        neg.rx_te = neg.te.and_then(|(_, clk)| {
            ours.pts
                .iter()
                .find(|p| p.name == "telephone-event" && p.clock == clk)
                .map(|p| (p.pt, clk))
        });
        neg.rx_cn_pt = neg
            .cn_pt
            .and_then(|_| ours.pts.iter().find(|p| p.name == "CN").map(|p| p.pt));
        let mut alt: Vec<u8> = ours_codec;
        alt.extend(neg.rx_alt_pts.iter().copied());
        alt.push(pt);
        alt.retain(|&p| p != neg.rx_pt);
        let mut seen = Vec::new();
        alt.retain(|p| {
            let new = !seen.contains(p);
            seen.push(*p);
            new
        });
        neg.rx_alt_pts = alt;
        self.negotiated = Some(neg.clone());
        Ok(neg)
    }

    fn validated_remote_key(&self, suite: SrtpSuite, c: &CryptoLine) -> Result<String, NegError> {
        let inline = c
            .first_inline()
            .ok_or_else(|| NegError::NotAcceptable("crypto line without inline key".into()))?;
        SrtpContext::from_inline(suite, inline)
            .map_err(|e| NegError::NotAcceptable(format!("bad SRTP key: {e}")))?;
        Ok(inline.to_string())
    }

    /// Build the result; `rx_*` default to the tx values (answerer case).
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &self,
        m: &MediaDescription,
        ip: IpAddr,
        codec: AudioCodec,
        pt: u8,
        srtp_sel: Option<(SrtpSuite, String)>,
        ptime: u32,
        local_direction: Direction,
        effective: Direction,
        zero_conn: bool,
        mux: bool,
    ) -> Negotiated {
        let remote_rtp = SocketAddr::new(ip, m.port);
        let remote_rtcp = if mux {
            None
        } else {
            Some(match &m.rtcp {
                Some(r) => SocketAddr::new(r.addr.unwrap_or(ip), r.port),
                None => SocketAddr::new(ip, m.port.wrapping_add(1)),
            })
        };
        let srtp = srtp_sel.and_then(|(suite, remote)| {
            let local = self.key_for(suite)?;
            let remote_changed = self
                .negotiated
                .as_ref()
                .and_then(|n| n.srtp.as_ref())
                .is_some_and(|p| p.suite != suite || p.remote_key_b64 != remote);
            Some(SrtpParams {
                suite,
                local_key_b64: local,
                remote_key_b64: remote,
                remote_changed,
            })
        });
        let te = if self.caps.telephone_event {
            pick_te(m, codec)
        } else {
            None
        };
        let cn_pt = if self.caps.comfort_noise {
            pick_cn(m, codec)
        } else {
            None
        };
        let rx_alt_pts = m
            .fmts
            .iter()
            .copied()
            .filter(|&p| p != pt && codec_of(m, p) == Some(codec))
            .collect();
        Negotiated {
            codec,
            pt,
            clock_rate: codec.rtp_clock_rate(),
            remote_rtp,
            remote_rtcp,
            rtcp_mux: mux,
            ptime_ms: ptime,
            te,
            cn_pt,
            srtp,
            local_direction,
            send_enabled: effective.sends(),
            recv_enabled: effective.receives(),
            remote_hold: zero_conn
                || matches!(m.direction, Direction::SendOnly | Direction::Inactive),
            remote_hold_known: true,
            opus_remote: (codec == AudioCodec::Opus).then(|| OpusSdpParams::from_remote(m, pt)),
            rx_pt: pt,
            rx_te: te,
            rx_cn_pt: cn_pt,
            rx_alt_pts,
        }
    }

    // ── applying an offer ───────────────────────────────────────────────

    /// Evaluate one offered audio m-line; `Err` if it is not acceptable.
    fn pick_audio(
        &self,
        offer: &SessionDescription,
        idx: usize,
        m: &MediaDescription,
    ) -> Result<OfferPick, NegError> {
        if !m.proto.is_rtp() {
            return Err(NegError::NotAcceptable(format!(
                "unsupported transport {}",
                m.proto.as_str()
            )));
        }
        let ip = offer
            .connection_for(m)
            .ok_or_else(|| NegError::Malformed("offer has no connection address".into()))?;
        if ip.is_ipv4() != self.local_ip.is_ipv4() {
            return Err(NegError::NotAcceptable(format!(
                "address family mismatch (offer {ip}, local {})",
                self.local_ip
            )));
        }

        // Codec selection.
        let supported = |pt: u8| codec_of(m, pt).filter(|c| self.caps.codecs.contains(c));
        let picked = match self.caps.answer_order {
            AnswerOrder::Offer => m.fmts.iter().find_map(|&pt| supported(pt).map(|c| (pt, c))),
            AnswerOrder::Local => self.caps.codecs.iter().find_map(|&lc| {
                m.fmts
                    .iter()
                    .find_map(|&pt| (supported(pt) == Some(lc)).then_some((pt, lc)))
            }),
        };
        let (pt, codec) =
            picked.ok_or_else(|| NegError::NotAcceptable("no common audio codec".into()))?;

        // SRTP. Crypto lines carrying session params we do not implement are
        // skipped so a later line can win.
        let secure = m.proto.is_secure();
        let usable = if self.caps.srtp == SrtpPolicy::Off {
            None
        } else {
            m.crypto.iter().find_map(|c| {
                if !crypto_params_supported(c) {
                    return None;
                }
                let suite = SrtpSuite::from_sdp_name(&c.suite)?;
                if !self.keys.iter().any(|(s, _)| *s == suite) {
                    return None;
                }
                let key = self.validated_remote_key(suite, c).ok()?;
                Some((c.tag, suite, key))
            })
        };
        match (self.caps.srtp, &usable) {
            (SrtpPolicy::Off, _) if secure => {
                return Err(NegError::NotAcceptable("SRTP offered but disabled".into()));
            }
            (SrtpPolicy::Optional | SrtpPolicy::Mandatory, None) if secure => {
                return Err(NegError::NotAcceptable(
                    "no acceptable SRTP crypto suite offered".into(),
                ));
            }
            (SrtpPolicy::Mandatory, None) => {
                return Err(NegError::NotAcceptable(
                    "SRTP required but not offered".into(),
                ));
            }
            _ => {}
        }

        // Build the answer description.
        let te = if self.caps.telephone_event {
            pick_te(m, codec)
        } else {
            None
        };
        let cn = if self.caps.comfort_noise {
            pick_cn(m, codec)
        } else {
            None
        };
        let mut pts = vec![codec_entry(codec, pt, &self.caps.opus)];
        if let Some((tpt, clk)) = te {
            pts.push(te_entry(tpt, clk));
        }
        if let Some(c) = cn {
            pts.push(cn_entry(c));
        }
        // Wanted ptime: remote's, else ours; snapped to what `codec` can
        // encode and the remote's maxptime. The same value is written in our
        // `a=ptime` and used for sending.
        let want = m
            .ptime
            .filter(|&p| valid_ptime(p))
            .or(self.caps.ptime_ms.filter(|&p| valid_ptime(p)));
        let ptime = tx_ptime(codec, want.unwrap_or(20), m, pt);
        let mux = self.caps.rtcp_mux && m.rtcp_mux;
        let audio = AudioOut {
            proto: m.proto.as_str().to_string(),
            pts,
            ptime: want.map(|_| ptime),
            mux,
            crypto: usable.iter().map(|(t, s, _)| (*t, *s)).collect(),
        };
        Ok(OfferPick {
            idx,
            ip,
            pt,
            codec,
            usable,
            audio,
            ptime,
            mux,
        })
    }

    /// Process a remote offer; returns the SDP answer and the negotiated result.
    ///
    /// The first *acceptable* audio m-line is answered; all other m-lines
    /// (other audio sections included) are rejected with port 0. An
    /// unparseable m-line makes the whole offer malformed (400).
    pub fn apply_offer(
        &mut self,
        offer: &SessionDescription,
    ) -> Result<(String, Negotiated), NegError> {
        if offer.media.iter().any(|om| om.kind.is_empty()) {
            return Err(NegError::Malformed("unparseable m= line".into()));
        }
        let mut first_err = None;
        let mut pick = None;
        for (i, m) in offer
            .media
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_audio() && m.port != 0)
        {
            match self.pick_audio(offer, i, m) {
                Ok(p) => {
                    pick = Some(p);
                    break;
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        let Some(pick) = pick else {
            return Err(first_err.unwrap_or_else(|| {
                NegError::NotAcceptable("no active audio stream offered".into())
            }));
        };
        let OfferPick {
            idx,
            ip,
            pt,
            codec,
            usable,
            audio,
            ptime,
            mux,
        } = pick;
        let m = &offer.media[idx];

        let desc: Vec<OutMedia> = offer
            .media
            .iter()
            .enumerate()
            .map(|(i, om)| {
                if i == idx {
                    OutMedia::Audio(audio.clone())
                } else {
                    OutMedia::Rejected {
                        kind: om.kind.clone(),
                        proto: om.proto.as_str().to_string(),
                        fmts: if om.fmts_raw.is_empty() {
                            "0".into()
                        } else {
                            om.fmts_raw.join(" ")
                        },
                    }
                }
            })
            .collect();

        let zero = is_unspecified(ip);
        let dir = intersect(self.hold, m.direction, zero);
        let neg = self.finish(
            m,
            ip,
            codec,
            pt,
            usable.map(|(_, s, k)| (s, k)),
            ptime,
            dir,
            dir,
            zero,
            mux,
        );
        self.desc = desc;
        self.audio_idx = idx;
        let sdp = self.render(dir);
        self.negotiated = Some(neg.clone());
        Ok((sdp, neg))
    }
}

/// An accepted offered audio m-line.
struct OfferPick {
    idx: usize,
    ip: IpAddr,
    pt: u8,
    codec: AudioCodec,
    usable: Option<(u32, SrtpSuite, String)>,
    audio: AudioOut,
    ptime: u32,
    mux: bool,
}

#[cfg(test)]
mod tests;
