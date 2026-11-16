//! Audio container and sample-format helpers shared by the file, inline and speech paths.

use std::io::Read;

use anyhow::{Result, bail};

use siple_media::g711;

use crate::audio::Resampler;
use crate::media::sources::ENGINE_RATE;

/// Sample encodings accepted for inline `pcm_base64` audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmEncoding {
    /// Signed 16-bit little endian.
    S16le,
    /// 32-bit float little endian.
    F32le,
    /// G.711 mu-law.
    Mulaw,
    /// G.711 A-law.
    Alaw,
}

impl PcmEncoding {
    /// Parses the wire name (`s16le|f32le|mulaw|alaw`).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "s16le" => Self::S16le,
            "f32le" => Self::F32le,
            "mulaw" => Self::Mulaw,
            "alaw" => Self::Alaw,
            _ => return None,
        })
    }

    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S16le => "s16le",
            Self::F32le => "f32le",
            Self::Mulaw => "mulaw",
            Self::Alaw => "alaw",
        }
    }

    /// Bytes per sample.
    pub fn sample_bytes(self) -> usize {
        match self {
            Self::S16le => 2,
            Self::F32le => 4,
            Self::Mulaw | Self::Alaw => 1,
        }
    }
}

/// Raw PCM description: mono samples at `sample_rate` Hz (8000..=48000) in `encoding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub encoding: PcmEncoding,
}

/// Decodes raw mono PCM to 48 kHz `f32`. `data` must be a whole number of samples and
/// `format.sample_rate` within 8000..=48000. Rejects audio longer than `max_secs`.
pub fn decode_pcm_48k(data: &[u8], format: PcmFormat, max_secs: u32) -> Result<Vec<f32>> {
    if !(8000..=48000).contains(&format.sample_rate) {
        bail!(
            "pcm sample_rate {} out of range (8000..=48000)",
            format.sample_rate
        );
    }
    let sb = format.encoding.sample_bytes();
    if !data.len().is_multiple_of(sb) {
        bail!(
            "pcm data length {} is not a multiple of {} bytes ({})",
            data.len(),
            sb,
            format.encoding.as_str()
        );
    }
    if data.is_empty() {
        bail!("contains no audio");
    }
    // Duration cap before any decoding allocates.
    if (data.len() / sb) as u64 > u64::from(max_secs) * u64::from(format.sample_rate) {
        bail!("audio longer than {max_secs} s");
    }
    let mono: Vec<f32> = match format.encoding {
        PcmEncoding::S16le => data
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect(),
        PcmEncoding::F32le => data
            .chunks_exact(4)
            .map(|b| {
                let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                if v.is_finite() {
                    v.clamp(-1.0, 1.0)
                } else {
                    0.0
                }
            })
            .collect(),
        PcmEncoding::Mulaw => data
            .iter()
            .map(|&b| g711::ulaw_to_linear(b) as f32 / 32768.0)
            .collect(),
        PcmEncoding::Alaw => data
            .iter()
            .map(|&b| g711::alaw_to_linear(b) as f32 / 32768.0)
            .collect(),
    };
    resample(&mono, format.sample_rate, ENGINE_RATE)
}

/// Decodes standard base64 (optionally behind a `data:...;base64,` prefix, ignoring
/// whitespace; padding optional) after checking the encoded length against `max_decoded`
/// bytes. Nothing is allocated for the payload before the cap check passes.
pub fn decode_base64_capped(b64: &str, max_decoded: usize) -> Result<Vec<u8>> {
    use base64::Engine as _;
    use base64::alphabet::STANDARD;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

    let mut body = b64.trim_start();
    if body.len() >= 5 && body.as_bytes()[..5].eq_ignore_ascii_case(b"data:") {
        let Some(comma) = body.find(',') else {
            bail!("data URI has no `,` before the payload");
        };
        let head = &body[5..comma];
        if !head.to_ascii_lowercase().contains(";base64") {
            bail!("data URI is not base64-encoded");
        }
        body = &body[comma + 1..];
    }
    let mut n = 0usize;
    let mut pad = 0usize;
    for b in body.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
        } else if pad > 0 {
            bail!("invalid base64: data after padding");
        }
        n += 1;
    }
    let implied = (n - pad) / 4 * 3
        + match (n - pad) % 4 {
            0 => 0,
            r => r - 1,
        };
    if implied > max_decoded {
        bail!(
            "encoded audio too large: {implied} bytes decoded exceeds the {max_decoded} byte limit"
        );
    }
    let clean: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let engine = GeneralPurpose::new(
        &STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let out = engine
        .decode(clean.as_bytes())
        .map_err(|e| anyhow::anyhow!("invalid base64: {e}"))?;
    if out.len() > max_decoded {
        bail!("decoded audio exceeds the {max_decoded} byte limit");
    }
    Ok(out)
}

/// Accepted source sample-rate range for resampling. rubato's FFT size scales with the input
/// rate for coprime rate pairs, so an unbounded rate means unbounded memory.
pub const RESAMPLE_RATE_RANGE: std::ops::RangeInclusive<u32> = 8_000..=192_000;

/// Resamples mono `f32` from `from` Hz to `to` Hz, compensating filter delay so the output
/// length is `round(input * to / from)`. Errors if either rate is outside
/// [`RESAMPLE_RATE_RANGE`].
pub fn resample(mono: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    if !RESAMPLE_RATE_RANGE.contains(&from) || !RESAMPLE_RATE_RANGE.contains(&to) {
        bail!(
            "sample rate {from} -> {to} out of range ({}..={})",
            RESAMPLE_RATE_RANGE.start(),
            RESAMPLE_RATE_RANGE.end()
        );
    }
    if from == to || mono.is_empty() {
        return Ok(mono.to_vec());
    }
    let chunk = (from / 100).max(1) as usize;
    let mut rs = Resampler::new(from, to, chunk)
        .map_err(|e| anyhow::anyhow!("resampler {from} -> {to}: {e}"))?;
    let want = ((mono.len() as u128 * to as u128 + from as u128 / 2) / from as u128) as usize;
    let delay = rs.delay_frames();
    let mut out = Vec::with_capacity(want + delay + 4096);
    rs.process(mono, &mut out);
    // Flush with silence until the delayed output covers the expected length. The FFT
    // resampler only emits whole input chunks (up to `from` frames for coprime rates), so each
    // pad is one second of silence, which always completes a chunk.
    let pad = vec![0.0f32; from as usize];
    for _ in 0..8 {
        if out.len() >= want + delay {
            break;
        }
        rs.process(&pad, &mut out);
    }
    let delay = delay.min(out.len());
    out.drain(..delay);
    out.truncate(want);
    Ok(out)
}

/// `f32` (-1..1) to `i16` with clamping and rounding.
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

/// Downsamples 48 kHz mono `i16` to `to_rate` (e.g. 16000 or 8000) for returning audio to
/// agents. `to_rate` must be in 8000..=48000.
pub fn downsample_i16(samples48: &[i16], to_rate: u32) -> Result<Vec<i16>> {
    if !(8000..=ENGINE_RATE).contains(&to_rate) {
        bail!("sample rate {to_rate} out of range (8000..=48000)");
    }
    if to_rate == ENGINE_RATE {
        return Ok(samples48.to_vec());
    }
    let f: Vec<f32> = samples48.iter().map(|&s| s as f32 / 32768.0).collect();
    Ok(f32_to_i16(&resample(&f, ENGINE_RATE, to_rate)?))
}

/// Encodes mono 16-bit PCM as a canonical WAV file.
pub fn encode_wav_i16(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate.saturating_mul(2)).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Decode a WAV (8/16/24/32-bit int or 32-bit float, any channel count) as 48 kHz mono `f32`.
///
/// Rejects sample rates outside [`RESAMPLE_RATE_RANGE`] and files whose header duration
/// exceeds `max_secs`, both before any samples are collected.
pub fn decode_wav_48k<R: Read>(reader: R, max_secs: u32) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::new(reader)?;
    let spec = r.spec();
    if !RESAMPLE_RATE_RANGE.contains(&spec.sample_rate) {
        bail!(
            "WAV sample rate {} out of range ({}..={})",
            spec.sample_rate,
            RESAMPLE_RATE_RANGE.start(),
            RESAMPLE_RATE_RANGE.end()
        );
    }
    let ch = spec.channels.max(1) as usize;
    // `duration()` is frames per channel, from the header.
    if u64::from(r.duration()) > u64::from(max_secs) * u64::from(spec.sample_rate) {
        bail!("WAV longer than {max_secs} s");
    }
    let raw: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Float, 32) => r.samples::<f32>().collect::<Result<_, _>>()?,
        (hound::SampleFormat::Int, b @ 1..=32) => {
            let scale = 1.0 / (1u64 << (b - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
        (f, b) => bail!("unsupported WAV format {f:?}/{b} bit"),
    };
    let mono: Vec<f32> = if ch == 1 {
        raw
    } else {
        raw.chunks(ch)
            .map(|c| c.iter().sum::<f32>() / c.len() as f32)
            .collect()
    };
    if mono.is_empty() {
        bail!("contains no audio");
    }
    resample(&mono, spec.sample_rate, ENGINE_RATE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, n: usize) -> Vec<i16> {
        (0..n)
            .map(|i| {
                (8000.0 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin())
                    as i16
            })
            .collect()
    }

    fn s16_bytes(v: &[i16]) -> Vec<u8> {
        v.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn encodings_parse_and_size() {
        for e in [
            PcmEncoding::S16le,
            PcmEncoding::F32le,
            PcmEncoding::Mulaw,
            PcmEncoding::Alaw,
        ] {
            assert_eq!(PcmEncoding::parse(e.as_str()), Some(e));
        }
        assert_eq!(PcmEncoding::parse("u8"), None);
    }

    #[test]
    fn pcm_formats_and_rates_give_48k_lengths() {
        for rate in [8000u32, 11025, 16000, 22050, 24000, 44100, 48000] {
            let n = rate as usize / 2; // 0.5 s
            let want = (n * 48000 + rate as usize / 2) / rate as usize;
            let s = tone(rate, n);
            let f32s: Vec<u8> = s
                .iter()
                .flat_map(|&v| (v as f32 / 32768.0).to_le_bytes())
                .collect();
            let mu: Vec<u8> = s.iter().map(|&v| g711::linear_to_ulaw(v)).collect();
            let al: Vec<u8> = s.iter().map(|&v| g711::linear_to_alaw(v)).collect();
            for (enc, bytes) in [
                (PcmEncoding::S16le, s16_bytes(&s)),
                (PcmEncoding::F32le, f32s),
                (PcmEncoding::Mulaw, mu),
                (PcmEncoding::Alaw, al),
            ] {
                let out = decode_pcm_48k(
                    &bytes,
                    PcmFormat {
                        sample_rate: rate,
                        encoding: enc,
                    },
                    10,
                )
                .unwrap();
                assert_eq!(out.len(), want, "{rate} {enc:?}");
                let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                assert!((0.15..0.4).contains(&peak), "{rate} {enc:?} peak {peak}");
            }
        }
    }

    #[test]
    fn pcm_rejects_bad_input() {
        let f = |rate, enc| PcmFormat {
            sample_rate: rate,
            encoding: enc,
        };
        assert!(decode_pcm_48k(&[0; 4], f(7999, PcmEncoding::S16le), 10).is_err());
        assert!(decode_pcm_48k(&[0; 4], f(48001, PcmEncoding::S16le), 10).is_err());
        assert!(decode_pcm_48k(&[0; 3], f(8000, PcmEncoding::S16le), 10).is_err());
        assert!(decode_pcm_48k(&[0; 6], f(8000, PcmEncoding::F32le), 10).is_err());
        assert!(decode_pcm_48k(&[], f(8000, PcmEncoding::Mulaw), 10).is_err());
        // NaN / inf floats become silence rather than poisoning the output.
        let mut b = f32::NAN.to_le_bytes().to_vec();
        b.extend_from_slice(&f32::INFINITY.to_le_bytes());
        let out = decode_pcm_48k(&b, f(48000, PcmEncoding::F32le), 10).unwrap();
        assert_eq!(out, vec![0.0, 0.0]);
    }

    #[test]
    fn g711_round_trip_close() {
        let s = tone(8000, 800);
        for (enc, bytes) in [
            (
                PcmEncoding::Mulaw,
                s.iter()
                    .map(|&v| g711::linear_to_ulaw(v))
                    .collect::<Vec<_>>(),
            ),
            (
                PcmEncoding::Alaw,
                s.iter()
                    .map(|&v| g711::linear_to_alaw(v))
                    .collect::<Vec<_>>(),
            ),
        ] {
            let out = decode_pcm_48k(
                &bytes,
                PcmFormat {
                    sample_rate: 8000,
                    encoding: enc,
                },
                10,
            )
            .unwrap();
            // Back at 8 kHz, compared to the original within companding error.
            let back = downsample_i16(&f32_to_i16(&out), 8000).unwrap();
            assert_eq!(back.len(), s.len());
            let err: f64 = s[100..700]
                .iter()
                .zip(&back[100..700])
                .map(|(a, b)| (f64::from(*a) - f64::from(*b)).abs())
                .sum::<f64>()
                / 600.0;
            assert!(err < 300.0, "{enc:?} mean abs err {err}");
        }
        for v in [-32000i16, -1000, -8, 0, 8, 1000, 32000] {
            let u = g711::ulaw_to_linear(g711::linear_to_ulaw(v));
            let a = g711::alaw_to_linear(g711::linear_to_alaw(v));
            assert!((i32::from(u) - i32::from(v)).abs() <= (i32::from(v).abs() / 16 + 16));
            assert!((i32::from(a) - i32::from(v)).abs() <= (i32::from(v).abs() / 16 + 16));
        }
    }

    #[test]
    fn base64_prefix_whitespace_padding() {
        use base64::Engine as _;
        let raw: Vec<u8> = (0..100u8).collect();
        let enc = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert_eq!(decode_base64_capped(&enc, 100).unwrap(), raw);
        let prefixed = format!("data:audio/wav;base64,{enc}");
        assert_eq!(decode_base64_capped(&prefixed, 100).unwrap(), raw);
        let spaced: String = enc
            .as_bytes()
            .chunks(7)
            .map(|c| format!("{}\r\n ", std::str::from_utf8(c).unwrap()))
            .collect();
        assert_eq!(
            decode_base64_capped(&format!("  {spaced}"), 100).unwrap(),
            raw
        );
        let unpadded = enc.trim_end_matches('=');
        assert_eq!(decode_base64_capped(unpadded, 100).unwrap(), raw);
        assert!(decode_base64_capped("data:audio/wav,AAAA", 100).is_err());
        assert!(decode_base64_capped("data:audio/wav;base64", 100).is_err());
        assert!(decode_base64_capped("AA!A", 100).is_err());
        assert!(decode_base64_capped("AA=A", 100).is_err());
        assert_eq!(decode_base64_capped("", 10).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn base64_cap_checked_before_decoding() {
        use base64::Engine as _;
        let raw = vec![7u8; 1000];
        let enc = base64::engine::general_purpose::STANDARD.encode(&raw);
        assert!(decode_base64_capped(&enc, 1000).is_ok());
        let e = decode_base64_capped(&enc, 999).unwrap_err().to_string();
        assert!(e.contains("too large"), "{e}");
        // Invalid content that is also too large reports the size, proving no decode happened.
        let junk = "!".repeat(4000);
        let e = decode_base64_capped(&junk, 10).unwrap_err().to_string();
        assert!(e.contains("too large"), "{e}");
    }

    #[test]
    fn wav_encode_decode_round_trip() {
        let s = tone(16000, 8000);
        let wav = encode_wav_i16(&s, 16000);
        assert_eq!(wav.len(), 44 + 16000);
        let mut r = hound::WavReader::new(std::io::Cursor::new(&wav)).unwrap();
        assert_eq!(r.spec().sample_rate, 16000);
        assert_eq!(r.spec().channels, 1);
        let back: Vec<i16> = r.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(back, s);
        let out = decode_wav_48k(std::io::Cursor::new(&wav), 10).unwrap();
        assert_eq!(out.len(), 24000);
        let empty = encode_wav_i16(&[], 8000);
        assert!(decode_wav_48k(std::io::Cursor::new(&empty), 10).is_err());
    }

    #[test]
    fn downsample_lengths_and_tone_survives() {
        let s = tone(48000, 48000);
        for rate in [16000u32, 8000] {
            let d = downsample_i16(&s, rate).unwrap();
            assert_eq!(d.len(), rate as usize);
            let peak = d[rate as usize / 4..]
                .iter()
                .map(|v| v.abs())
                .max()
                .unwrap();
            assert!((6500..9500).contains(&peak), "{rate} peak {peak}");
        }
        assert_eq!(downsample_i16(&s, 48000).unwrap(), s);
        assert!(downsample_i16(&s, 4000).is_err());
        assert!(downsample_i16(&[], 16000).unwrap().is_empty());
    }

    #[test]
    fn pcm_duration_cap() {
        let f = PcmFormat {
            sample_rate: 8000,
            encoding: PcmEncoding::Mulaw,
        };
        assert!(decode_pcm_48k(&vec![0xffu8; 8000], f, 1).is_ok());
        let e = decode_pcm_48k(&vec![0xffu8; 8001], f, 1).unwrap_err();
        assert!(e.to_string().contains("longer"), "{e}");
    }

    #[test]
    fn wav_rate_and_duration_bounds() {
        // Tiny WAV claiming an absurd rate must be rejected before resampling.
        for rate in [7999u32, 192_001, 44_101 * 1000] {
            let wav = encode_wav_i16(&[0, 1, 2, 3], rate);
            let e = decode_wav_48k(std::io::Cursor::new(&wav), 10).unwrap_err();
            assert!(e.to_string().contains("out of range"), "{rate}: {e}");
        }
        // hound itself refuses rates whose byte rate overflows; still an error either way.
        let wav = encode_wav_i16(&[0, 1, 2, 3], 4_000_000_000);
        assert!(decode_wav_48k(std::io::Cursor::new(&wav), 10).is_err());
        assert!(resample(&[0.0; 10], 48_000, 1_000_000).is_err());
        assert!(resample(&[0.0; 10], 1, 48_000).is_err());
        // Duration cap from the header.
        let wav = encode_wav_i16(&vec![0i16; 16_001], 16_000);
        assert!(decode_wav_48k(std::io::Cursor::new(&wav), 1).is_err());
        let wav = encode_wav_i16(&vec![0i16; 16_000], 16_000);
        assert!(decode_wav_48k(std::io::Cursor::new(&wav), 1).is_ok());
    }

    #[test]
    fn resample_odd_rates_full_length() {
        for rate in [47_999u32, 44_101, 8_001] {
            let n = rate as usize; // 1 s
            let x: Vec<f32> = tone(rate, n).iter().map(|&v| v as f32 / 32768.0).collect();
            let out = resample(&x, rate, ENGINE_RATE).unwrap();
            let want = 48_000f64;
            let got = out.len() as f64;
            assert!((got - want).abs() / want <= 0.01, "{rate}: {got}");
        }
    }
}
