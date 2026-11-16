//! TX audio sources (silence / tone / looped WAV).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use siple_media::tone::{ToneGenerator, ToneSpec};

use crate::config::AudioSource;

pub(crate) const ENGINE_RATE: u32 = 48_000;
/// Peak amplitude of generated test tones (about -10 dBFS).
const TONE_AMPLITUDE: i16 = 10_000;

/// A generator for 48 kHz mono `f32` TX audio.
pub(crate) enum TxSource {
    /// Microphone frames supplied by the media thread (zeros when absent).
    Device,
    Silence,
    Tone {
        tgen: ToneGenerator,
        buf: Vec<i16>,
    },
    File {
        data: Arc<Vec<f32>>,
        pos: usize,
    },
}

impl TxSource {
    pub fn load(spec: &AudioSource) -> Result<Self> {
        Ok(match spec {
            AudioSource::Device => Self::Device,
            AudioSource::Silence => Self::Silence,
            AudioSource::Tone(hz) => Self::Tone {
                tgen: ToneGenerator::new(ToneSpec::sine(*hz), TONE_AMPLITUDE),
                buf: Vec::new(),
            },
            AudioSource::File(p) => Self::File {
                data: Arc::new(load_wav_48k(p)?),
                pos: 0,
            },
        })
    }

    /// Fill `out` with the next samples. `Device` yields zeros (the caller
    /// substitutes mic audio).
    pub fn fill(&mut self, out: &mut [f32]) {
        match self {
            Self::Device | Self::Silence => out.fill(0.0),
            Self::Tone { tgen, buf } => {
                buf.resize(out.len(), 0);
                tgen.fill(buf);
                for (o, s) in out.iter_mut().zip(buf.iter()) {
                    *o = *s as f32 / 32768.0;
                }
            }
            Self::File { data, pos } => {
                if data.is_empty() {
                    out.fill(0.0);
                    return;
                }
                for o in out.iter_mut() {
                    *o = data[*pos];
                    *pos += 1;
                    if *pos >= data.len() {
                        *pos = 0;
                    }
                }
            }
        }
    }
}

/// Longest config/file WAV source accepted, in seconds (generous but finite; bounds the
/// decoded `f32` buffer at about 115 MB).
pub(crate) const MAX_FILE_WAV_SECS: u32 = 600;

/// Load a WAV file as 48 kHz mono `f32` (see [`crate::agent::wav::decode_wav_48k`]), capped at
/// [`MAX_FILE_WAV_SECS`].
pub(crate) fn load_wav_48k(path: &Path) -> Result<Vec<f32>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    crate::agent::wav::decode_wav_48k(std::io::BufReader::new(file), MAX_FILE_WAV_SECS)
        .map_err(|e| anyhow::anyhow!("{}: {e:#}", path.display()))
}
