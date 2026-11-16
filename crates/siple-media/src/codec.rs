//! Audio codecs: PCMU, PCMA, G.722, Opus.
//!
//! All PCM is **mono `i16` at the codec's [`AudioCodec::pcm_sample_rate`]**
//! (8 kHz for G.711, 16 kHz for G.722, 48 kHz for Opus). Resampling to the
//! engine rate is the caller's job.
//!
//! Output-vector convention: `encode`, `decode`, `conceal` and `decode_fec`
//! **replace** the contents of `out` (it is cleared first).
//!
//! Loss handling lives on the decoder: [`AudioDecoder::conceal`] is PLC and
//! [`AudioDecoder::decode_fec`] recovers a lost frame from the *next* packet
//! where the codec supports it (Opus inband FEC).

use anyhow::{Result, bail};

use crate::g711;

/// Supported audio codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    /// G.711 mu-law, static PT 0.
    Pcmu,
    /// G.711 A-law, static PT 8.
    Pcma,
    /// G.722, static PT 9. RTP clock is 8000 even though audio is 16 kHz.
    G722,
    /// Opus, dynamic PT, `opus/48000/2` in SDP (we encode and decode mono).
    Opus,
}

impl AudioCodec {
    /// Static RTP payload type (RFC 3551), `None` for dynamic codecs (Opus).
    pub fn static_payload_type(&self) -> Option<u8> {
        match self {
            AudioCodec::Pcmu => Some(0),
            AudioCodec::Pcma => Some(8),
            AudioCodec::G722 => Some(9),
            AudioCodec::Opus => None,
        }
    }

    /// Identify a codec from a *static* payload type. Dynamic PTs (Opus)
    /// must be resolved from SDP rtpmap instead.
    pub fn from_static_payload_type(pt: u8) -> Option<Self> {
        match pt {
            0 => Some(AudioCodec::Pcmu),
            8 => Some(AudioCodec::Pcma),
            9 => Some(AudioCodec::G722),
            _ => None,
        }
    }

    /// Encoding name as used in SDP `a=rtpmap` (`PCMU`, `PCMA`, `G722`, `opus`).
    pub fn sdp_encoding_name(&self) -> &'static str {
        match self {
            AudioCodec::Pcmu => "PCMU",
            AudioCodec::Pcma => "PCMA",
            AudioCodec::G722 => "G722",
            AudioCodec::Opus => "opus",
        }
    }

    /// Channel count as written in the rtpmap (`opus/48000/2`); `None` when
    /// the rtpmap carries no channel parameter.
    pub fn sdp_channels(&self) -> Option<u8> {
        match self {
            AudioCodec::Opus => Some(2),
            _ => None,
        }
    }

    /// Case-insensitive lookup by SDP encoding name or common alias.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "pcmu" | "g711u" | "g.711u" | "ulaw" => Some(AudioCodec::Pcmu),
            "pcma" | "g711a" | "g.711a" | "alaw" => Some(AudioCodec::Pcma),
            "g722" | "g.722" => Some(AudioCodec::G722),
            "opus" => Some(AudioCodec::Opus),
            _ => None,
        }
    }

    /// RTP timestamp clock rate (G.722 is 8000, a historical RFC 3551 quirk).
    pub fn rtp_clock_rate(&self) -> u32 {
        match self {
            AudioCodec::Pcmu | AudioCodec::Pcma | AudioCodec::G722 => 8000,
            AudioCodec::Opus => 48000,
        }
    }

    /// Actual PCM sample rate of the encoder input / decoder output.
    pub fn pcm_sample_rate(&self) -> u32 {
        match self {
            AudioCodec::Pcmu | AudioCodec::Pcma => 8000,
            AudioCodec::G722 => 16000,
            AudioCodec::Opus => 48000,
        }
    }

    /// PCM samples (mono) in one `ptime_ms` frame, at [`Self::pcm_sample_rate`].
    pub fn samples_per_frame(&self, ptime_ms: u32) -> usize {
        (self.pcm_sample_rate() as u64 * ptime_ms as u64 / 1000) as usize
    }

    /// RTP timestamp increment for one `ptime_ms` frame.
    pub fn rtp_ts_per_frame(&self, ptime_ms: u32) -> u32 {
        (self.rtp_clock_rate() as u64 * ptime_ms as u64 / 1000) as u32
    }
}

/// Encodes mono PCM to codec payloads.
pub trait AudioEncoder: Send {
    /// Encode `pcm` into `out` (cleared first). `pcm` must be exactly one
    /// codec frame/packet worth of audio (e.g. one `ptime`): G.711/G.722
    /// accept any whole number of samples; Opus requires a single
    /// 10/20/40/60 ms frame (see [`opus_supported_ptime`]).
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()>;
    fn codec(&self) -> AudioCodec;
}

/// Decodes codec payloads to mono PCM, with loss handling.
pub trait AudioDecoder: Send {
    /// Decode one RTP payload into `out` (cleared first).
    fn decode(&mut self, payload: &[u8], out: &mut Vec<i16>) -> Result<()>;

    /// Packet-loss concealment: synthesize `samples` PCM samples (at the
    /// codec PCM rate) for a missing frame into `out` (cleared first).
    fn conceal(&mut self, samples: usize, out: &mut Vec<i16>) -> Result<()>;

    /// Recover the *lost* frame preceding `next_payload` from in-band FEC
    /// carried by the next packet. `samples` is the duration of the lost frame.
    /// Returns `Ok(false)` (with `out` untouched) if the codec has no FEC; the
    /// caller should then use [`Self::conceal`]. The next packet itself must
    /// still be decoded normally with [`Self::decode`] afterwards.
    fn decode_fec(
        &mut self,
        next_payload: &[u8],
        samples: usize,
        out: &mut Vec<i16>,
    ) -> Result<bool>;

    fn codec(&self) -> AudioCodec;
}

// ── Repeat-and-fade concealment (G.711, G.722) ──────────────────────────

/// Concealment length after which output goes to silence.
const CONCEAL_MAX_MS: u32 = 60;
/// Tail of the last good frame that is repeated.
const CONCEAL_REPEAT_MS: u32 = 20;

/// Repeats the tail of the last good audio, fading about 6 dB per repeat,
/// and falls silent after ~60 ms of consecutive concealment.
struct RepeatFade {
    rate: u32,
    last: Vec<i16>,
    pos: usize,
    gain: f32,
    concealed: usize,
}

impl RepeatFade {
    fn new(rate: u32) -> Self {
        Self {
            rate,
            last: Vec::new(),
            pos: 0,
            gain: 1.0,
            concealed: 0,
        }
    }

    /// Record good decoded audio and reset concealment state.
    fn good(&mut self, pcm: &[i16]) {
        let keep = (self.rate * CONCEAL_REPEAT_MS / 1000) as usize;
        if !pcm.is_empty() {
            let start = pcm.len().saturating_sub(keep);
            self.last.clear();
            self.last.extend_from_slice(&pcm[start..]);
        }
        self.pos = 0;
        self.gain = 1.0;
        self.concealed = 0;
    }

    fn conceal(&mut self, samples: usize, out: &mut Vec<i16>) {
        out.clear();
        out.reserve(samples);
        let max = (self.rate * CONCEAL_MAX_MS / 1000) as usize;
        for _ in 0..samples {
            if self.last.is_empty() || self.concealed >= max {
                out.push(0);
                self.concealed = self.concealed.saturating_add(1);
                continue;
            }
            let len = self.last.len();
            // Linear ramp from `gain` to `gain / 2` across one repeat: -6 dB
            // per repeat, continuous at repeat boundaries (no clicks).
            let g = self.gain * (1.0 - 0.5 * self.pos as f32 / len as f32);
            out.push((self.last[self.pos] as f32 * g) as i16);
            self.pos += 1;
            self.concealed += 1;
            if self.pos == len {
                self.pos = 0;
                self.gain *= 0.5;
            }
        }
    }
}

// ── G.711 ───────────────────────────────────────────────────────────────

/// G.711 decoder (mu-law or A-law).
pub struct G711Decoder {
    codec: AudioCodec,
    decode: fn(u8) -> i16,
    plc: RepeatFade,
}

impl G711Decoder {
    pub fn pcmu() -> Self {
        Self {
            codec: AudioCodec::Pcmu,
            decode: g711::ulaw_to_linear,
            plc: RepeatFade::new(8000),
        }
    }
    pub fn pcma() -> Self {
        Self {
            codec: AudioCodec::Pcma,
            decode: g711::alaw_to_linear,
            plc: RepeatFade::new(8000),
        }
    }
}

impl AudioDecoder for G711Decoder {
    fn decode(&mut self, payload: &[u8], out: &mut Vec<i16>) -> Result<()> {
        out.clear();
        out.reserve(payload.len());
        out.extend(payload.iter().map(|&b| (self.decode)(b)));
        self.plc.good(out);
        Ok(())
    }

    fn conceal(&mut self, samples: usize, out: &mut Vec<i16>) -> Result<()> {
        self.plc.conceal(samples, out);
        Ok(())
    }

    fn decode_fec(&mut self, _: &[u8], _: usize, _: &mut Vec<i16>) -> Result<bool> {
        Ok(false)
    }

    fn codec(&self) -> AudioCodec {
        self.codec
    }
}

/// G.711 encoder (mu-law or A-law).
pub struct G711Encoder {
    codec: AudioCodec,
    encode: fn(i16) -> u8,
}

impl G711Encoder {
    pub fn pcmu() -> Self {
        Self {
            codec: AudioCodec::Pcmu,
            encode: g711::linear_to_ulaw,
        }
    }
    pub fn pcma() -> Self {
        Self {
            codec: AudioCodec::Pcma,
            encode: g711::linear_to_alaw,
        }
    }
}

impl AudioEncoder for G711Encoder {
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        out.reserve(pcm.len());
        out.extend(pcm.iter().map(|&s| (self.encode)(s)));
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        self.codec
    }
}

// ── G.722 ───────────────────────────────────────────────────────────────

/// G.722 decoder (64 kbit/s, 16 kHz output).
///
/// Concealment repeats and fades the last output; the ADPCM predictor state is
/// *not* advanced during concealment, so the first frame after a loss decodes
/// from stale state and converges within a few ms (inherent to G.722).
pub struct G722Decoder {
    decoder: ezk_g722::libg722::decoder::Decoder,
    plc: RepeatFade,
}

impl Default for G722Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G722Decoder {
    pub fn new() -> Self {
        Self {
            decoder: ezk_g722::libg722::decoder::Decoder::new(
                ezk_g722::libg722::Bitrate::Mode1_64000,
                false, // packed
                false, // eight_k (false = 16 kHz output)
            ),
            plc: RepeatFade::new(16000),
        }
    }
}

impl AudioDecoder for G722Decoder {
    fn decode(&mut self, payload: &[u8], out: &mut Vec<i16>) -> Result<()> {
        let samples = self.decoder.decode(payload);
        out.clear();
        out.extend_from_slice(&samples);
        self.plc.good(out);
        Ok(())
    }

    fn conceal(&mut self, samples: usize, out: &mut Vec<i16>) -> Result<()> {
        self.plc.conceal(samples, out);
        Ok(())
    }

    fn decode_fec(&mut self, _: &[u8], _: usize, _: &mut Vec<i16>) -> Result<bool> {
        Ok(false)
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::G722
    }
}

/// G.722 encoder (64 kbit/s, 16 kHz input).
pub struct G722Encoder {
    encoder: ezk_g722::libg722::encoder::Encoder,
}

impl Default for G722Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl G722Encoder {
    pub fn new() -> Self {
        Self {
            encoder: ezk_g722::libg722::encoder::Encoder::new(
                ezk_g722::libg722::Bitrate::Mode1_64000,
                false, // eight_k (false = 16 kHz input)
                false, // packed
            ),
        }
    }
}

impl AudioEncoder for G722Encoder {
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()> {
        let encoded = self.encoder.encode(pcm);
        out.clear();
        out.extend_from_slice(&encoded);
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::G722
    }
}

// ── Opus ────────────────────────────────────────────────────────────────

/// Opus encoder settings. Defaults: 24 kbit/s VBR, no FEC, 48 kHz bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusEncoderConfig {
    /// Target bitrate in bit/s. `None` = 24000.
    pub bitrate: Option<u32>,
    /// Upper bound on the bitrate, from the remote `maxaveragebitrate`. The
    /// effective bitrate is `min(target, max_bitrate)`. `None` = no cap.
    pub max_bitrate: Option<u32>,
    /// Emit in-band FEC (enable when the remote signals `useinbandfec=1`).
    pub inband_fec: bool,
    /// Expected packet loss percentage (0..=100) used to tune FEC strength.
    pub packet_loss_pct: u8,
    /// Remote `maxplaybackrate` in Hz; caps the coded audio bandwidth.
    pub max_playback_rate: u32,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            bitrate: None,
            max_bitrate: None,
            inband_fec: false,
            packet_loss_pct: 0,
            max_playback_rate: 48000,
        }
    }
}

impl OpusEncoderConfig {
    /// Effective bitrate: `min(target, max_bitrate)`, clamped to libopus's
    /// supported 6..=510 kbit/s.
    pub fn effective_bitrate(&self) -> u32 {
        let target = self.bitrate.unwrap_or(24000);
        let capped = self.max_bitrate.map_or(target, |cap| target.min(cap));
        capped.clamp(6000, 510_000)
    }
}

/// Encoder construction settings for [`make_encoder`]. Fields for codecs
/// other than the one being built are ignored.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderConfig {
    pub opus: OpusEncoderConfig,
}

/// True if `ms` is a packet duration [`OpusEncoder`] can encode as a single
/// Opus frame: 10, 20, 40 or 60 ms. (30 and 50 ms would need a multi-frame
/// Opus packet, which this encoder does not produce.)
pub fn opus_supported_ptime(ms: u32) -> bool {
    matches!(ms, 10 | 20 | 40 | 60)
}

/// Nearest supported Opus ptime (10/20/40/60 ms) to `ms`; ties resolve toward
/// 20 ms (so 15 -> 20, 30 -> 20, 50 -> 40). For the negotiation/engine layer.
pub fn clamp_opus_ptime(ms: u32) -> u32 {
    [20u32, 10, 40, 60]
        .into_iter()
        .min_by_key(|c| c.abs_diff(ms))
        .unwrap_or(20)
}

const OPUS_MAX_PACKET: usize = 4000;
/// Largest Opus frame: 120 ms at 48 kHz.
const OPUS_MAX_FRAME: usize = 5760;
/// Opus PLC/FEC frame sizes must be a multiple of 2.5 ms (120 samples @ 48k).
const OPUS_QUANTUM: usize = 120;

// `opus_packet_has_lbrr` is part of the bundled libopus (linked via opus2 ->
// libopus_sys) but not wrapped by opus2.
unsafe extern "C" {
    fn opus_packet_has_lbrr(packet: *const u8, len: i32) -> i32;
}

/// True if the Opus packet carries in-band FEC (LBRR) data for the previous frame.
fn opus_packet_has_fec(packet: &[u8]) -> bool {
    let Ok(len) = i32::try_from(packet.len()) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    // SAFETY: pointer/length describe a valid readable slice; libopus only reads it.
    unsafe { opus_packet_has_lbrr(packet.as_ptr(), len) == 1 }
}

/// Opus encoder: mono, 48 kHz, VOIP application.
pub struct OpusEncoder {
    encoder: opus2::Encoder,
    /// Reused output scratch (avoids zero-filling a fresh buffer per call).
    scratch: Vec<u8>,
}

impl OpusEncoder {
    pub fn new(cfg: &OpusEncoderConfig) -> Result<Self> {
        let mut encoder =
            opus2::Encoder::new(48000, opus2::Channels::Mono, opus2::Application::Voip)?;
        encoder.set_bitrate(opus2::Bitrate::Bits(cfg.effective_bitrate() as i32))?;
        encoder.set_inband_fec(cfg.inband_fec)?;
        encoder.set_packet_loss_perc(cfg.packet_loss_pct.min(100) as i32)?;
        if cfg.max_playback_rate < 48000 {
            let bandwidth = match cfg.max_playback_rate {
                0..=11999 => opus2::Bandwidth::Narrowband,
                12000..=15999 => opus2::Bandwidth::Mediumband,
                16000..=23999 => opus2::Bandwidth::Wideband,
                24000..=47999 => opus2::Bandwidth::Superwideband,
                _ => opus2::Bandwidth::Fullband,
            };
            encoder.set_max_bandwidth(bandwidth)?;
        }
        Ok(Self {
            encoder,
            scratch: vec![0u8; OPUS_MAX_PACKET],
        })
    }
}

impl AudioEncoder for OpusEncoder {
    /// Encodes exactly one Opus packet from `pcm`, which must be a single
    /// 10, 20, 40 or 60 ms frame (480/960/1920/2880 samples at 48 kHz); see
    /// [`opus_supported_ptime`]. Other lengths (including 30/50 ms) error.
    fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<()> {
        let ms = pcm.len() / 48;
        if !pcm.len().is_multiple_of(48) || !opus_supported_ptime(ms as u32) {
            bail!(
                "Opus encoder needs a 10, 20, 40 or 60 ms frame (480/960/1920/2880 samples), got {} samples",
                pcm.len()
            );
        }
        let n = self.encoder.encode(pcm, &mut self.scratch)?;
        out.clear();
        out.extend_from_slice(&self.scratch[..n]);
        Ok(())
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Opus
    }
}

/// Opus decoder: mono, 48 kHz. PLC and FEC use libopus.
///
/// Scratch buffers are preallocated, so decode/conceal do not allocate when
/// the caller reuses its `out` vector.
pub struct OpusDecoder {
    decoder: opus2::Decoder,
    scratch: Vec<i16>,
    /// Concealment samples generated beyond what was requested (libopus PLC
    /// works in 2.5 ms steps); handed out first by the next `conceal`.
    pending: Vec<i16>,
}

impl OpusDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self {
            decoder: opus2::Decoder::new(48000, opus2::Channels::Mono)?,
            scratch: vec![0i16; OPUS_MAX_FRAME],
            pending: Vec::with_capacity(OPUS_MAX_FRAME),
        })
    }

    /// FEC-recover one frame of `samples` from `input`. libopus needs a
    /// multiple of 2.5 ms, so a ragged request is rounded up and the surplus
    /// discarded (FEC recovers exactly one frame; there is nothing to carry over).
    fn fec_frame(&mut self, input: &[u8], samples: usize, out: &mut Vec<i16>) -> Result<()> {
        let want = samples
            .div_ceil(OPUS_QUANTUM)
            .saturating_mul(OPUS_QUANTUM)
            .clamp(OPUS_QUANTUM, OPUS_MAX_FRAME);
        let got = self
            .decoder
            .decode(input, &mut self.scratch[..want], true)?;
        out.clear();
        out.extend_from_slice(&self.scratch[..got.min(samples)]);
        Ok(())
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, payload: &[u8], out: &mut Vec<i16>) -> Result<()> {
        if payload.is_empty() {
            bail!("empty Opus payload");
        }
        // Real audio supersedes any surplus concealment.
        self.pending.clear();
        let n = self.decoder.decode(payload, &mut self.scratch, false)?;
        out.clear();
        out.extend_from_slice(&self.scratch[..n]);
        Ok(())
    }

    /// Exactly `samples` samples are always returned. libopus PLC runs in
    /// 2.5 ms (120-sample) steps, so for other lengths the decoder rounds up
    /// and keeps the surplus buffered; the next `conceal` call consumes it
    /// first (a real `decode` discards it).
    fn conceal(&mut self, samples: usize, out: &mut Vec<i16>) -> Result<()> {
        out.clear();
        let from_pending = self.pending.len().min(samples);
        out.extend_from_slice(&self.pending[..from_pending]);
        self.pending.drain(..from_pending);
        let mut remaining = samples - from_pending;
        while remaining > 0 {
            let want = remaining
                .div_ceil(OPUS_QUANTUM)
                .saturating_mul(OPUS_QUANTUM)
                .min(OPUS_MAX_FRAME);
            let got = self.decoder.decode(&[], &mut self.scratch[..want], false)?;
            if got == 0 {
                bail!("Opus PLC produced no samples");
            }
            let take = got.min(remaining);
            out.extend_from_slice(&self.scratch[..take]);
            self.pending.extend_from_slice(&self.scratch[take..got]);
            remaining -= take;
        }
        Ok(())
    }

    /// Returns `Ok(false)` (leaving `out` untouched) when `next_payload` is
    /// empty or carries no in-band FEC (no LBRR data): libopus would only
    /// run plain PLC in that case, and the caller should use `conceal`.
    fn decode_fec(
        &mut self,
        next_payload: &[u8],
        samples: usize,
        out: &mut Vec<i16>,
    ) -> Result<bool> {
        if samples == 0 || !opus_packet_has_fec(next_payload) {
            return Ok(false);
        }
        self.pending.clear();
        self.fec_frame(next_payload, samples, out)?;
        Ok(true)
    }

    fn codec(&self) -> AudioCodec {
        AudioCodec::Opus
    }
}

// ── Factories ───────────────────────────────────────────────────────────

/// Build an encoder for `codec`.
pub fn make_encoder(codec: AudioCodec, cfg: &EncoderConfig) -> Result<Box<dyn AudioEncoder>> {
    Ok(match codec {
        AudioCodec::Pcmu => Box::new(G711Encoder::pcmu()),
        AudioCodec::Pcma => Box::new(G711Encoder::pcma()),
        AudioCodec::G722 => Box::new(G722Encoder::new()),
        AudioCodec::Opus => Box::new(OpusEncoder::new(&cfg.opus)?),
    })
}

/// Build a decoder for `codec`.
pub fn make_decoder(codec: AudioCodec) -> Result<Box<dyn AudioDecoder>> {
    Ok(match codec {
        AudioCodec::Pcmu => Box::new(G711Decoder::pcmu()),
        AudioCodec::Pcma => Box::new(G711Decoder::pcma()),
        AudioCodec::G722 => Box::new(G722Decoder::new()),
        AudioCodec::Opus => Box::new(OpusDecoder::new()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [AudioCodec; 4] = [
        AudioCodec::Pcmu,
        AudioCodec::Pcma,
        AudioCodec::G722,
        AudioCodec::Opus,
    ];

    fn sine(rate: u32, hz: f64, amp: f64, n: usize, start: usize) -> Vec<i16> {
        (start..start + n)
            .map(|i| ((i as f64 * hz * std::f64::consts::TAU / rate as f64).sin() * amp) as i16)
            .collect()
    }

    fn energy(s: &[i16]) -> f64 {
        s.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / s.len().max(1) as f64
    }

    #[test]
    fn codec_properties() {
        assert_eq!(AudioCodec::Pcmu.static_payload_type(), Some(0));
        assert_eq!(AudioCodec::Pcma.static_payload_type(), Some(8));
        assert_eq!(AudioCodec::G722.static_payload_type(), Some(9));
        assert_eq!(AudioCodec::Opus.static_payload_type(), None);
        assert_eq!(AudioCodec::Pcmu.sdp_encoding_name(), "PCMU");
        assert_eq!(AudioCodec::Pcma.sdp_encoding_name(), "PCMA");
        assert_eq!(AudioCodec::G722.sdp_encoding_name(), "G722");
        assert_eq!(AudioCodec::Opus.sdp_encoding_name(), "opus");
        assert_eq!(AudioCodec::G722.rtp_clock_rate(), 8000);
        assert_eq!(AudioCodec::G722.pcm_sample_rate(), 16000);
        assert_eq!(AudioCodec::Opus.rtp_clock_rate(), 48000);
        assert_eq!(AudioCodec::Pcma.pcm_sample_rate(), 8000);
        assert_eq!(AudioCodec::Pcmu.samples_per_frame(20), 160);
        assert_eq!(AudioCodec::G722.samples_per_frame(20), 320);
        assert_eq!(AudioCodec::G722.rtp_ts_per_frame(20), 160);
        assert_eq!(AudioCodec::Opus.samples_per_frame(20), 960);
        assert_eq!(AudioCodec::Opus.rtp_ts_per_frame(20), 960);
        assert_eq!(AudioCodec::Opus.samples_per_frame(10), 480);
        assert_eq!(AudioCodec::from_name("OPUS"), Some(AudioCodec::Opus));
        assert_eq!(AudioCodec::from_name("pcma"), Some(AudioCodec::Pcma));
        assert_eq!(
            AudioCodec::from_static_payload_type(8),
            Some(AudioCodec::Pcma)
        );
        assert_eq!(AudioCodec::from_static_payload_type(111), None);
    }

    #[test]
    fn g711_roundtrip() {
        for codec in [AudioCodec::Pcmu, AudioCodec::Pcma] {
            let mut enc = make_encoder(codec, &EncoderConfig::default()).unwrap();
            let mut dec = make_decoder(codec).unwrap();
            let input = sine(8000, 440.0, 10000.0, 160, 0);
            let (mut e, mut d) = (Vec::new(), Vec::new());
            enc.encode(&input, &mut e).unwrap();
            assert_eq!(e.len(), 160);
            dec.decode(&e, &mut d).unwrap();
            assert_eq!(d.len(), 160);
            for (a, b) in input.iter().zip(&d) {
                assert!((*a as i32 - *b as i32).abs() < 500, "{codec:?}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn pcma_differs_from_pcmu_and_known_values() {
        let mut u = G711Encoder::pcmu();
        let mut a = G711Encoder::pcma();
        let (mut eu, mut ea) = (Vec::new(), Vec::new());
        u.encode(&[0, 1000, -1000], &mut eu).unwrap();
        a.encode(&[0, 1000, -1000], &mut ea).unwrap();
        assert_ne!(eu, ea);
        // A-law silence is 0xD5, mu-law silence is 0xFF
        assert_eq!(ea[0], 0xD5);
        assert_eq!(eu[0], 0xFF);
    }

    #[test]
    fn g722_roundtrip() {
        let mut enc = G722Encoder::new();
        let mut dec = G722Decoder::new();
        let input = sine(16000, 800.0, 10000.0, 320, 0);
        let (mut e, mut d) = (Vec::new(), Vec::new());
        enc.encode(&input, &mut e).unwrap();
        assert_eq!(e.len(), 160); // 64 kbit/s = 160 bytes per 20 ms
        dec.decode(&e, &mut d).unwrap();
        assert_eq!(d.len(), 320);
    }

    #[test]
    fn opus_roundtrip() {
        let mut enc = OpusEncoder::new(&OpusEncoderConfig::default()).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let (mut e, mut d) = (Vec::new(), Vec::new());
        for n in 0..5 {
            let input = sine(48000, 440.0, 10000.0, 960, n * 960);
            enc.encode(&input, &mut e).unwrap();
            assert!(!e.is_empty() && e.len() < 960);
            dec.decode(&e, &mut d).unwrap();
            assert_eq!(d.len(), 960);
        }
        assert!(energy(&d) > 1e5, "decoded signal should carry energy");
    }

    #[test]
    fn opus_config_honors_bitrate_and_bandwidth() {
        let cfg = OpusEncoderConfig {
            bitrate: Some(12000),
            max_playback_rate: 8000,
            ..Default::default()
        };
        let mut enc = OpusEncoder::new(&cfg).unwrap();
        let mut e = Vec::new();
        for n in 0..10 {
            let input = sine(48000, 600.0, 18000.0, 960, n * 960);
            enc.encode(&input, &mut e).unwrap();
        }
        assert!(e.len() <= 60, "12 kbit/s 20 ms packet too big: {}", e.len());
        assert_eq!(
            opus2::packet::get_bandwidth(&e).unwrap(),
            opus2::Bandwidth::Narrowband
        );
    }

    #[test]
    fn opus_plc_produces_requested_length() {
        let mut enc = OpusEncoder::new(&OpusEncoderConfig::default()).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let (mut e, mut d) = (Vec::new(), Vec::new());
        for n in 0..5 {
            enc.encode(&sine(48000, 440.0, 10000.0, 960, n * 960), &mut e)
                .unwrap();
            dec.decode(&e, &mut d).unwrap();
        }
        for want in [960, 480, 1000, 120, 1] {
            dec.conceal(want, &mut d).unwrap();
            assert_eq!(d.len(), want);
        }
        dec.conceal(960, &mut d).unwrap();
        assert!(
            energy(&d) > 0.0,
            "PLC right after speech should not be silent"
        );
    }

    #[test]
    fn opus_encode_frame_sizes() {
        let mut enc = OpusEncoder::new(&OpusEncoderConfig::default()).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let (mut e, mut d) = (Vec::new(), Vec::new());
        for ms in [10usize, 20, 40, 60] {
            let n = ms * 48;
            enc.encode(&sine(48000, 440.0, 10000.0, n, 0), &mut e)
                .unwrap();
            dec.decode(&e, &mut d).unwrap();
            assert_eq!(d.len(), n, "{ms} ms");
        }
        for bad in [0usize, 1, 240, 1440, 2400, 3840, 960 + 7] {
            let err = enc.encode(&vec![0i16; bad], &mut e);
            assert!(err.is_err(), "{bad} samples should be rejected");
        }
        let msg = enc
            .encode(&vec![0i16; 1440], &mut e)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("10, 20, 40 or 60"), "{msg}");
    }

    #[test]
    fn opus_ptime_helpers() {
        for ms in [10, 20, 40, 60] {
            assert!(opus_supported_ptime(ms));
            assert_eq!(clamp_opus_ptime(ms), ms);
        }
        for ms in [0, 5, 30, 50, 80, 120] {
            assert!(!opus_supported_ptime(ms));
        }
        assert_eq!(clamp_opus_ptime(0), 10);
        assert_eq!(clamp_opus_ptime(5), 10);
        assert_eq!(clamp_opus_ptime(15), 20);
        assert_eq!(clamp_opus_ptime(30), 20);
        assert_eq!(clamp_opus_ptime(50), 40);
        assert_eq!(clamp_opus_ptime(90), 60);
        assert_eq!(clamp_opus_ptime(u32::MAX), 60);
    }

    #[test]
    fn opus_max_bitrate_caps_target() {
        let cfg = |bitrate, max_bitrate| OpusEncoderConfig {
            bitrate,
            max_bitrate,
            ..Default::default()
        };
        assert_eq!(cfg(None, None).effective_bitrate(), 24000);
        assert_eq!(cfg(Some(32000), Some(16000)).effective_bitrate(), 16000);
        assert_eq!(cfg(Some(16000), Some(32000)).effective_bitrate(), 16000);
        assert_eq!(cfg(None, Some(12000)).effective_bitrate(), 12000);
        assert_eq!(cfg(None, Some(100)).effective_bitrate(), 6000);
        let mut enc = OpusEncoder::new(&cfg(Some(64000), Some(8000))).unwrap();
        let mut e = Vec::new();
        for n in 0..10 {
            enc.encode(&sine(48000, 600.0, 18000.0, 960, n * 960), &mut e)
                .unwrap();
        }
        assert!(e.len() <= 40, "capped 8 kbit/s packet too big: {}", e.len());
    }

    #[test]
    fn opus_fec_false_without_lbrr() {
        // Encoder without FEC => packets carry no LBRR data.
        let mut enc = OpusEncoder::new(&OpusEncoderConfig::default()).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let (mut e, mut d) = (Vec::new(), vec![7i16; 3]);
        for n in 0..4 {
            enc.encode(&sine(48000, 440.0, 10000.0, 960, n * 960), &mut e)
                .unwrap();
            dec.decode(&e, &mut d).unwrap();
        }
        let mut rec = vec![7i16; 3];
        assert!(!dec.decode_fec(&e, 960, &mut rec).unwrap());
        assert_eq!(rec, vec![7i16; 3], "out untouched when no FEC");
        assert!(!dec.decode_fec(&[0xff, 0xff], 960, &mut rec).unwrap());
    }

    #[test]
    fn opus_plc_ragged_lengths_keep_remainder() {
        let mut enc = OpusEncoder::new(&OpusEncoderConfig::default()).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let (mut e, mut d) = (Vec::new(), Vec::new());
        for n in 0..5 {
            enc.encode(&sine(48000, 440.0, 10000.0, 960, n * 960), &mut e)
                .unwrap();
            dec.decode(&e, &mut d).unwrap();
        }
        // Ragged request: 100 samples rounds up to 120; the other 20 buffered.
        dec.conceal(100, &mut d).unwrap();
        assert_eq!(d.len(), 100);
        assert_eq!(dec.pending.len(), 20);
        dec.conceal(20, &mut d).unwrap();
        assert_eq!(d.len(), 20);
        assert!(dec.pending.is_empty(), "buffered remainder consumed first");
        // 7 x 1-sample requests: 1 decode step, rest from the buffer.
        for _ in 0..7 {
            dec.conceal(1, &mut d).unwrap();
            assert_eq!(d.len(), 1);
        }
        assert_eq!(dec.pending.len(), 113);
        // A real packet discards the surplus.
        enc.encode(&sine(48000, 440.0, 10000.0, 960, 5 * 960), &mut e)
            .unwrap();
        dec.decode(&e, &mut d).unwrap();
        assert!(dec.pending.is_empty());
    }

    #[test]
    fn opus_fec_recovers_lost_frame() {
        let cfg = OpusEncoderConfig {
            bitrate: Some(32000),
            inband_fec: true,
            packet_loss_pct: 30,
            ..Default::default()
        };
        let mut enc = OpusEncoder::new(&cfg).unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let mut packets = Vec::new();
        for n in 0..12 {
            let mut e = Vec::new();
            enc.encode(&sine(48000, 440.0, 12000.0, 960, n * 960), &mut e)
                .unwrap();
            packets.push(e);
        }
        let mut d = Vec::new();
        for p in &packets[..8] {
            dec.decode(p, &mut d).unwrap();
        }
        // packet 8 is lost; recover it from packet 9
        let mut rec = Vec::new();
        assert!(dec.decode_fec(&packets[9], 960, &mut rec).unwrap());
        assert_eq!(rec.len(), 960);
        assert!(energy(&rec) > 1e5, "FEC frame should carry the tone");
        dec.decode(&packets[9], &mut d).unwrap();
        assert_eq!(d.len(), 960);
        assert!(!dec.decode_fec(&[], 960, &mut rec).unwrap());
    }

    #[test]
    fn non_opus_has_no_fec() {
        for codec in [AudioCodec::Pcmu, AudioCodec::Pcma, AudioCodec::G722] {
            let mut dec = make_decoder(codec).unwrap();
            let mut out = vec![1, 2, 3];
            assert!(!dec.decode_fec(&[0u8; 80], 160, &mut out).unwrap());
            assert_eq!(out, vec![1, 2, 3], "out untouched when no FEC");
        }
    }

    #[test]
    fn repeat_fade_concealment_fades_6db_and_goes_silent() {
        for codec in [AudioCodec::Pcmu, AudioCodec::Pcma, AudioCodec::G722] {
            let rate = codec.pcm_sample_rate();
            let frame = codec.samples_per_frame(20);
            let mut enc = make_encoder(codec, &EncoderConfig::default()).unwrap();
            let mut dec = make_decoder(codec).unwrap();
            let (mut e, mut d) = (Vec::new(), Vec::new());
            // constant-ish loud tone; run a few frames so G.722 converges
            for n in 0..5 {
                enc.encode(&sine(rate, 400.0, 12000.0, frame, n * frame), &mut e)
                    .unwrap();
                dec.decode(&e, &mut d).unwrap();
            }
            let good = energy(&d);
            let mut c = Vec::new();
            let mut energies = Vec::new();
            for _ in 0..4 {
                dec.conceal(frame, &mut c).unwrap();
                assert_eq!(c.len(), frame);
                energies.push(energy(&c));
            }
            // first concealed frame is the repeat: close to the real frame, quieter
            assert!(
                energies[0] > good * 0.2 && energies[0] < good * 1.2,
                "{codec:?}"
            );
            assert!(energies[1] < energies[0], "{codec:?} must fade");
            assert!(energies[2] < energies[1], "{codec:?} must keep fading");
            // 60 ms elapsed (3 x 20 ms): frame 4 is pure silence
            assert_eq!(energies[3], 0.0, "{codec:?} silent after 60 ms");
            // a good packet resets concealment
            dec.decode(&e, &mut d).unwrap();
            dec.conceal(frame, &mut c).unwrap();
            assert!(energy(&c) > 0.0);
        }
    }

    #[test]
    fn conceal_without_history_is_silence() {
        let mut dec = G711Decoder::pcmu();
        let mut c = Vec::new();
        dec.conceal(160, &mut c).unwrap();
        assert_eq!(c, vec![0i16; 160]);
    }

    #[test]
    fn factory_builds_all() {
        for c in ALL {
            let e = make_encoder(c, &EncoderConfig::default()).unwrap();
            let d = make_decoder(c).unwrap();
            assert_eq!(e.codec(), c);
            assert_eq!(d.codec(), c);
        }
    }

    #[test]
    fn empty_inputs() {
        let mut enc = G711Encoder::pcmu();
        let mut dec = G711Decoder::pcmu();
        let (mut e, mut d) = (vec![1u8], vec![1i16]);
        enc.encode(&[], &mut e).unwrap();
        assert!(e.is_empty());
        dec.decode(&[], &mut d).unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn opus_decode_garbage_and_empty_error() {
        let mut dec = OpusDecoder::new().unwrap();
        let mut d = Vec::new();
        assert!(dec.decode(&[0xFF; 100], &mut d).is_err());
        assert!(dec.decode(&[], &mut d).is_err());
    }
}
