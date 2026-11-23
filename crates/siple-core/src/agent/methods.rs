//! Agent-audio API methods: the extended `call.play` / `call.stop_play`, plus
//! `call.say`, `call.listen`, `call.rx_audio`, `speech.transcribe` and `speech.synthesize`.
//!
//! Every play goes through [`AppBackend::start_play`]: the clip is decoded in the API layer,
//! the event-log position is captured before the clip is issued, and `wait` polls the log from
//! that position for the play's `call.play_finished` (or the call ending), so a finish can never
//! be missed.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use siple_config::{AudioSource, SpeechConfig};
use siple_sip::CallState;
use tokio::time::Instant;

use super::hub::{HubError, ListenOutcome, ListenParams, SnapshotRange};
use super::wav::{self, PcmEncoding, PcmFormat};
use super::{MAX_CLIP_SECS, MAX_INLINE_AUDIO_BYTES, speech};
use crate::backend::AppBackend;
use crate::call_media::MediaEntry;
use crate::control::methods::parse_params;
use crate::control::{ApiError, MethodInfo, release_big_request_permit};
use crate::events::EventFilter;
use crate::media::{ClipSource, PlayError, PlayRequest};

/// Names served by [`AppBackend::agent_method`].
pub const AGENT_METHODS: &[&str] = &[
    "call.say",
    "call.listen",
    "call.rx_audio",
    "speech.transcribe",
    "speech.synthesize",
];

/// Longest `call.play` `duration_ms`.
const MAX_PLAY_MS: u32 = 600_000;
/// Longest `wait` of `call.play` / `call.say`.
const MAX_PLAY_WAIT_MS: u64 = 120_000;
/// Longest `call.say` text, in characters.
const MAX_TEXT_CHARS: usize = 2000;
/// Longest wall-clock `timeout_ms` of `call.listen`.
const MAX_LISTEN_WALL_MS: u64 = 180_000;
/// Longest `max_ms` of `call.listen`.
const MAX_LISTEN_MAX_MS: u32 = 120_000;
/// Longest `start_timeout_ms` of `call.listen`.
const MAX_LISTEN_START_MS: u32 = 180_000;
/// Longest `last_ms` of `call.rx_audio`.
const MAX_RX_AUDIO_MS: u64 = 3_600_000;
/// Longest audio `speech.transcribe` takes, in seconds.
const MAX_TRANSCRIBE_SECS: u64 = 600;
/// Longest audio `call.rx_audio` returns inline as base64 (same as the clip cap).
const MAX_INLINE_RX_MS: u64 = MAX_CLIP_SECS as u64 * 1000;
/// How long a `wait` lingers for the play's own finish event after the call ended.
const ENDED_GRACE: Duration = Duration::from_millis(500);
/// DTMF events this close outside a listen window still count (event timestamps are wall time
/// of emission, the window is rx timeline).
const DTMF_SLACK_MS: i64 = 200;

// ---- params ------------------------------------------------------------------------------

/// `pcm_format` of `call.play`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PcmFormatParam {
    sample_rate: u32,
    encoding: String,
}

/// Params of `call.play`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlayParam {
    call_id: String,
    source: Option<String>,
    wav_base64: Option<String>,
    pcm_base64: Option<String>,
    pcm_format: Option<PcmFormatParam>,
    #[serde(rename = "loop")]
    looped: Option<bool>,
    duration_ms: Option<u32>,
    queue: Option<bool>,
    wait: Option<bool>,
    timeout_ms: Option<u64>,
    barge_in: Option<bool>,
    gain_db: Option<f32>,
}

/// Params of `call.stop_play`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StopPlayParam {
    call_id: String,
    play_id: Option<u64>,
    clear_queue: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SayParam {
    call_id: String,
    text: String,
    voice: Option<String>,
    queue: Option<bool>,
    wait: Option<bool>,
    timeout_ms: Option<u64>,
    barge_in: Option<bool>,
    gain_db: Option<f32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenParam {
    call_id: String,
    since_rx_ms: Option<u64>,
    start_timeout_ms: Option<u32>,
    end_silence_ms: Option<u32>,
    max_ms: Option<u32>,
    min_speech_ms: Option<u32>,
    pre_roll_ms: Option<u32>,
    threshold_dbfs: Option<f32>,
    timeout_ms: Option<u64>,
    transcribe: Option<bool>,
    language: Option<String>,
    audio: Option<String>,
    path: Option<String>,
    sample_rate: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RxAudioParam {
    call_id: String,
    last_ms: Option<u64>,
    since_rx_ms: Option<u64>,
    until_rx_ms: Option<u64>,
    sample_rate: Option<u32>,
    audio: Option<String>,
    path: Option<String>,
    transcribe: Option<bool>,
    language: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscribeParam {
    path: Option<String>,
    wav_base64: Option<String>,
    language: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SynthesizeParam {
    text: String,
    voice: Option<String>,
    path: Option<String>,
}

// ---- helpers -----------------------------------------------------------------------------

fn hub_err(e: HubError) -> ApiError {
    e.into()
}

fn inline_rx_too_long() -> ApiError {
    ApiError::invalid_params(format!(
        "more than {MAX_CLIP_SECS} s of audio cannot be returned inline; use audio:\"file\" with `path` (or audio:\"none\")"
    ))
}

fn finite(v: f32) -> Option<f32> {
    v.is_finite().then_some(v)
}

/// Adds `extra` fields to an error, keeping the ones it already carries.
fn merge_extra(mut e: ApiError, extra: Value) -> ApiError {
    let merged = match (e.extra.take(), extra) {
        (Some(Value::Object(mut a)), Value::Object(b)) => {
            a.extend(b);
            Value::Object(a)
        }
        (_, b) => b,
    };
    e.with_extra(merged)
}

fn check_text(text: &str) -> Result<(), ApiError> {
    let n = text.chars().count();
    if text.trim().is_empty() || n > MAX_TEXT_CHARS {
        return Err(ApiError::invalid_params(format!(
            "text must be 1..{MAX_TEXT_CHARS} characters (not blank)"
        )));
    }
    Ok(())
}

fn check_rate(rate: Option<u32>) -> Result<u32, ApiError> {
    match rate.unwrap_or(16_000) {
        r @ (8_000 | 16_000) => Ok(r),
        r => Err(ApiError::invalid_params(format!(
            "sample_rate {r} not supported (8000|16000)"
        ))),
    }
}

fn check_range<T: PartialOrd + std::fmt::Display + Copy>(
    name: &str,
    v: T,
    lo: T,
    hi: T,
) -> Result<T, ApiError> {
    if v >= lo && v <= hi {
        Ok(v)
    } else {
        Err(ApiError::invalid_params(format!(
            "{name} must be between {lo} and {hi}"
        )))
    }
}

/// 16 kHz `i16` to `rate` (8000 or 16000).
fn from_16k(samples16: &[i16], rate: u32) -> Vec<i16> {
    if rate == 16_000 || samples16.is_empty() {
        return samples16.to_vec();
    }
    let f: Vec<f32> = samples16.iter().map(|&s| s as f32 / 32768.0).collect();
    // Fixed in-range rates: resample cannot fail here.
    wav::f32_to_i16(&wav::resample(&f, 16_000, rate).unwrap_or_default())
}

/// How a result carries captured audio.
enum AudioOut {
    Base64,
    None,
    File(PathBuf),
}

/// Whisper-style 16 kHz mono `i16` from 48 kHz `f32`.
fn to_16k(mono48: &[f32]) -> Vec<i16> {
    // Fixed in-range rates: resample cannot fail here.
    wav::f32_to_i16(&wav::resample(mono48, 48_000, 16_000).unwrap_or_default())
}

fn decode_wav_capped<R: std::io::Read>(r: R, max_secs: u32) -> anyhow::Result<Vec<f32>> {
    wav::decode_wav_48k(r, max_secs)
}

fn decode_pcm_capped(bytes: &[u8], fmt: PcmFormat, max_secs: u32) -> anyhow::Result<Vec<f32>> {
    wav::decode_pcm_48k(bytes, fmt, max_secs)
}

fn io_err(e: tokio::task::JoinError) -> ApiError {
    ApiError::internal(e.to_string())
}

impl AppBackend {
    /// `unsupported` when this edition cannot run `[speech]` hooks (checked before anything else
    /// so the answer does not depend on configuration or call state).
    fn require_speech(&self) -> Result<(), ApiError> {
        speech::check_supported(&self.inner.channel).map_err(ApiError::from)
    }

    /// Marks the call as captured by an agent (listening, plus transcribing) until dropped.
    fn capture_guard(&self, call_id: &str, transcribe: bool) -> super::capture::CaptureGuard {
        use super::capture::CaptureKind;
        let kinds: &[CaptureKind] = if transcribe {
            &[CaptureKind::Listen, CaptureKind::Transcribe]
        } else {
            &[CaptureKind::Listen]
        };
        self.inner.agent.capture.begin(call_id, kinds)
    }

    fn speech_snapshot(&self) -> SpeechConfig {
        self.inner.config.load().speech.clone()
    }

    async fn parse_audio_out(
        &self,
        audio: Option<&str>,
        path: Option<&str>,
        transcribe: bool,
    ) -> Result<AudioOut, ApiError> {
        let mode = audio.unwrap_or(if transcribe { "none" } else { "base64" });
        match (mode, path) {
            ("base64", None) => Ok(AudioOut::Base64),
            ("none", None) => Ok(AudioOut::None),
            ("file", Some(p)) if !p.trim().is_empty() => {
                let full = self.resolve_path(std::path::Path::new(p));
                let parent_ok = full
                    .parent()
                    .is_none_or(|d| d.as_os_str().is_empty() || d.is_dir());
                if !parent_ok {
                    return Err(ApiError::invalid_params(format!(
                        "path {}: the directory does not exist",
                        full.display()
                    )));
                }
                probe_writable(&full).await?;
                Ok(AudioOut::File(full))
            }
            ("file", _) => Err(ApiError::invalid_params("audio \"file\" needs a `path`")),
            ("base64" | "none", Some(_)) => Err(ApiError::invalid_params(
                "`path` is only valid with audio \"file\"",
            )),
            (other, _) => Err(ApiError::invalid_params(format!(
                "audio {}: expected base64|none|file",
                siple_config::echo(other)
            ))),
        }
    }

    async fn render_audio(
        &self,
        out: &AudioOut,
        samples16: &[i16],
        rate: u32,
    ) -> Result<Option<Value>, ApiError> {
        if matches!(out, AudioOut::None) {
            return Ok(None);
        }
        let samples = from_16k(samples16, rate);
        let duration_ms = samples.len() as u64 * 1000 / rate as u64;
        let bytes = wav::encode_wav_i16(&samples, rate);
        let mut v = json!({
            "format": "wav",
            "sample_rate": rate,
            "channels": 1,
            "duration_ms": duration_ms,
        });
        match out {
            AudioOut::Base64 => {
                v["base64"] = base64::engine::general_purpose::STANDARD
                    .encode(&bytes)
                    .into();
            }
            AudioOut::File(p) => {
                tokio::fs::write(p, &bytes).await.map_err(|e| {
                    ApiError::invalid_params(format!("writing {}: {e}", p.display()))
                })?;
                v["path"] = p.display().to_string().into();
            }
            AudioOut::None => {}
        }
        Ok(Some(v))
    }

    // ---- call.play ------------------------------------------------------------------------

    pub(crate) async fn call_play(&self, mut p: PlayParam) -> Result<Value, ApiError> {
        let inline = p.wav_base64.is_some() as u8 + p.pcm_base64.is_some() as u8;
        if p.source.is_some() as u8 + inline != 1 {
            return Err(ApiError::invalid_params(
                "give exactly one of `source`, `wav_base64`, `pcm_base64`",
            ));
        }
        if p.pcm_base64.is_some() != p.pcm_format.is_some() {
            return Err(ApiError::invalid_params(
                "`pcm_base64` and `pcm_format` go together",
            ));
        }
        let gain_db = check_range("gain_db", p.gain_db.unwrap_or(0.0), -40.0, 20.0)?;
        if let Some(d) = p.duration_ms {
            check_range("duration_ms", d, 1, MAX_PLAY_MS)?;
        }
        if let Some(t) = p.timeout_ms {
            check_range("timeout_ms", t, 0, MAX_PLAY_WAIT_MS)?;
        }
        // `device` restores the call's own source: stop everything.
        if let Some(src) = &p.source
            && matches!(self.parse_source(Some(src))?, Some(AudioSource::Device))
        {
            let entry = self.media_entry(&p.call_id)?;
            let seq = self.event_seq();
            entry.handle.stop_play();
            return Ok(json!({
                "ok": true, "call_id": p.call_id, "play_id": null, "duration_ms": null,
                "queued": false, "rx_ms": entry.handle.rx_ms(), "event_seq": seq,
            }));
        }
        let parsed_source = self.parse_source(p.source.as_deref())?;
        // Fail early for unknown / ended calls; the handle itself is taken after decoding.
        self.media_entry(&p.call_id)?;

        let (clip, default_loop, len_ms) = self.decode_clip(&mut p, parsed_source).await?;
        let entry = self.media_entry(&p.call_id)?;
        let looped = p.looped.unwrap_or(default_loop);
        self.start_play(
            &entry,
            PlaySpec {
                clip,
                looped,
                clip_ms: len_ms,
                duration_ms: p.duration_ms,
                queue: p.queue.unwrap_or(false),
                gain_db,
                barge_in: p.barge_in.unwrap_or(false),
                wait: p.wait.unwrap_or(false),
                timeout_ms: p.timeout_ms,
            },
        )
        .await
    }

    /// Decodes the audio of a `call.play`; returns the clip, whether it loops by default and
    /// the length of one pass (`None` for generators).
    async fn decode_clip(
        &self,
        p: &mut PlayParam,
        source: Option<AudioSource>,
    ) -> Result<(ClipSource, bool, Option<u64>), ApiError> {
        // Inline audio is capped at MAX_CLIP_SECS; files (which used to loop as sources of any
        // length up to the file-source cap) keep their larger limit.
        let mut max_secs = MAX_CLIP_SECS;
        if matches!(source, Some(AudioSource::File(_))) {
            max_secs = crate::media::sources::MAX_FILE_WAV_SECS;
        }
        let max_samples = max_secs as usize * 48_000;
        let too_long = || ApiError::invalid_params(format!("clip is longer than {max_secs} s"));
        let (samples, default_loop): (Vec<i16>, bool) = match source {
            Some(AudioSource::File(path)) => {
                let shown = path.display().to_string();
                let v = tokio::task::spawn_blocking(move || {
                    crate::media::sources::load_wav_48k(&path).map(|v| wav::f32_to_i16(&v))
                })
                .await
                .map_err(io_err)?
                .map_err(|e| {
                    ApiError::invalid_params(format!(
                        "source file {}: {e:#}",
                        siple_config::echo(&shown)
                    ))
                })?;
                (v, true)
            }
            Some(other) => return Ok((ClipSource::Spec(other), true, None)),
            None => {
                let v = if let Some(b64) = p.wav_base64.take() {
                    let bytes = wav::decode_base64_capped(&b64, MAX_INLINE_AUDIO_BYTES)
                        .map_err(|e| ApiError::invalid_params(format!("wav_base64: {e:#}")))?;
                    drop(b64);
                    release_big_request_permit();
                    tokio::task::spawn_blocking(move || {
                        decode_wav_capped(Cursor::new(bytes), MAX_CLIP_SECS)
                            .map(|v| wav::f32_to_i16(&v))
                    })
                    .await
                    .map_err(io_err)?
                    .map_err(|e| ApiError::invalid_params(format!("wav_base64: {e:#}")))?
                } else {
                    let b64 = p.pcm_base64.take().unwrap_or_default();
                    let f = p.pcm_format.take().ok_or_else(|| {
                        ApiError::invalid_params("`pcm_base64` needs `pcm_format`")
                    })?;
                    let encoding = PcmEncoding::parse(&f.encoding).ok_or_else(|| {
                        ApiError::invalid_params(format!(
                            "pcm_format.encoding {}: expected s16le|f32le|mulaw|alaw",
                            siple_config::echo(&f.encoding)
                        ))
                    })?;
                    let bytes = wav::decode_base64_capped(&b64, MAX_INLINE_AUDIO_BYTES)
                        .map_err(|e| ApiError::invalid_params(format!("pcm_base64: {e:#}")))?;
                    drop(b64);
                    release_big_request_permit();
                    let fmt = PcmFormat {
                        sample_rate: f.sample_rate,
                        encoding,
                    };
                    tokio::task::spawn_blocking(move || {
                        decode_pcm_capped(&bytes, fmt, MAX_CLIP_SECS).map(|v| wav::f32_to_i16(&v))
                    })
                    .await
                    .map_err(io_err)?
                    .map_err(|e| ApiError::invalid_params(format!("pcm_base64: {e:#}")))?
                };
                (v, false)
            }
        };
        if samples.is_empty() {
            return Err(ApiError::invalid_params("the audio is empty"));
        }
        if samples.len() > max_samples {
            return Err(too_long());
        }
        let ms = samples.len() as u64 / 48;
        Ok((ClipSource::Pcm(Arc::from(samples)), default_loop, Some(ms)))
    }

    /// Issues a clip and optionally waits for it to finish.
    async fn start_play(&self, entry: &MediaEntry, s: PlaySpec) -> Result<Value, ApiError> {
        // Length of the play: a generator or a looping clip is endless without `duration_ms`.
        let total_ms: Option<u64> = match (s.clip_ms, s.looped, s.duration_ms) {
            (Some(c), false, d) => Some(d.map_or(c, |d| c.min(d as u64))),
            (_, _, Some(d)) => Some(d as u64),
            _ => None,
        };
        if s.wait && total_ms.is_none() {
            return Err(ApiError::invalid_params(
                "`wait` needs a finite play: set loop:false or duration_ms",
            ));
        }
        let timeout = Duration::from_millis(
            s.timeout_ms
                .unwrap_or_else(|| (total_ms.unwrap_or(0) + 30_000).min(MAX_PLAY_WAIT_MS)),
        );
        // Live counters (the stats snapshot lags by ~50 ms): back-to-back plays report `queued`.
        let mut busy = entry.handle.play_busy();
        // The position before the clip exists: its finish event is always after it.
        let seq = self.event_seq();
        let req = PlayRequest {
            source: s.clip,
            looped: s.looped,
            duration_ms: s.duration_ms,
            queue: s.queue,
            gain_db: s.gain_db,
        };
        let mut entry = entry.clone();
        let mut retried = false;
        let play_id = loop {
            match entry.handle.play_clip(req.clone()) {
                Ok(id) => break id,
                Err(PlayError::QueueFull) => {
                    return Err(ApiError::invalid_state(PlayError::QueueFull.to_string()));
                }
                Err(PlayError::Source(m)) => return Err(ApiError::invalid_params(m)),
                // Media closed or recreated at answer: re-fetch the entry once and retry.
                Err(e @ PlayError::Closed) => {
                    if retried {
                        return Err(ApiError::invalid_state(e.to_string()));
                    }
                    retried = true;
                    entry = self.media_entry(entry.handle.call_id())?;
                    busy = entry.handle.play_busy();
                }
            }
        };
        let hub = entry.handle.hub().clone();
        // The clip already plays: failing to arm barge-in must not hide its play_id.
        let mut barge_in_armed = false;
        let mut barge_in_err: Option<String> = None;
        if s.barge_in {
            match hub.watch_barge_in(play_id, None).await {
                Ok(()) => barge_in_armed = true,
                Err(e) => {
                    tracing::warn!(play_id, "barge-in not armed: {e}");
                    barge_in_err = Some(e.to_string());
                }
            }
        }
        let mut res = json!({
            "ok": true,
            "call_id": entry.handle.call_id(),
            "play_id": play_id,
            "duration_ms": total_ms,
            "queued": s.queue && busy,
            "rx_ms": entry.handle.rx_ms(),
            "event_seq": seq,
        });
        if let Some(e) = barge_in_err {
            res["barge_in"] = false.into();
            res["barge_in_error"] = e.into();
        }
        if s.wait {
            let call_id = entry.handle.call_id().to_string();
            let r = self
                .wait_play_finished(&call_id, play_id, seq, timeout, hub.rx_ms())
                .await;
            // After a timeout the play keeps going: its barge-in stays armed.
            if barge_in_armed && r.is_ok() {
                hub.unwatch_barge_in(play_id);
            }
            res["finished"] = r.map_err(|e| {
                merge_extra(
                    e,
                    json!({"play_id": play_id, "call_id": call_id, "event_seq": seq}),
                )
            })?;
        }
        Ok(res)
    }

    /// Waits for `call.play_finished {play_id}` after `since`, or for the call to end.
    async fn wait_play_finished(
        &self,
        call_id: &str,
        play_id: u64,
        since: u64,
        timeout: Duration,
        rx_ms_hint: u64,
    ) -> Result<Value, ApiError> {
        let filter = EventFilter {
            kinds: Some(vec!["call.play_finished".into(), "call.state".into()]),
            account: None,
            call_id: Some(call_id.to_string()),
        };
        let deadline = Instant::now() + timeout;
        let mut cursor = since;
        let mut ended_at: Option<Instant> = None;
        loop {
            // Read before polling: everything logged before the end is seen by the poll below.
            let ended = self
                .inner
                .stack
                .call(call_id)
                .is_none_or(|c| c.state == CallState::Ended);
            let now = Instant::now();
            if ended && ended_at.is_none() {
                ended_at = Some(now);
            }
            let wait_until = match ended_at {
                Some(t) => (t + ENDED_GRACE).min(deadline.max(t)),
                None => deadline,
            };
            let r = self
                .inner
                .events
                .poll(
                    cursor,
                    &filter,
                    500,
                    wait_until.saturating_duration_since(now),
                )
                .await;
            for e in &r.events {
                if e.kind == "call.play_finished" && e.data["play_id"].as_u64() == Some(play_id) {
                    let mut fin = json!({
                        "reason": e.data["reason"],
                        "played_ms": e.data["played_ms"],
                        "rx_ms": e.data["rx_ms"],
                    });
                    if let Some(b) = e.data.get("barge_in_rx_ms") {
                        fin["barge_in_rx_ms"] = b.clone();
                    }
                    return Ok(fin);
                }
            }
            cursor = r.next_seq;
            let now = Instant::now();
            if let Some(t) = ended_at {
                if now >= (t + ENDED_GRACE).min(deadline.max(t)) {
                    return Ok(json!({
                        "reason": "call_ended", "played_ms": null, "rx_ms": rx_ms_hint,
                    }));
                }
            } else if now >= deadline {
                return Err(ApiError::timeout(format!(
                    "play {play_id} did not finish within {} ms (it keeps playing)",
                    timeout.as_millis()
                )));
            }
        }
    }

    pub(crate) fn call_stop_play(&self, p: StopPlayParam) -> Result<Value, ApiError> {
        let entry = self.media_entry(&p.call_id)?;
        let seq = self.event_seq();
        entry
            .handle
            .stop_play_id(p.play_id, p.clear_queue.unwrap_or(true));
        Ok(json!({ "ok": true, "call_id": p.call_id, "event_seq": seq }))
    }

    // ---- dispatch -------------------------------------------------------------------------

    /// Dispatches one of [`AGENT_METHODS`].
    pub(crate) async fn agent_method(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        match method {
            "call.say" => self.call_say(parse_params(params)?).await,
            "call.listen" => self.call_listen(parse_params(params)?).await,
            "call.rx_audio" => self.call_rx_audio(parse_params(params)?).await,
            "speech.transcribe" => self.speech_transcribe(parse_params(params)?).await,
            "speech.synthesize" => self.speech_synthesize(parse_params(params)?).await,
            other => Err(ApiError::method_not_found(other)),
        }
    }

    // ---- call.say -------------------------------------------------------------------------

    async fn call_say(&self, p: SayParam) -> Result<Value, ApiError> {
        self.require_speech()?;
        check_text(&p.text)?;
        let gain_db = check_range("gain_db", p.gain_db.unwrap_or(0.0), -40.0, 20.0)?;
        if let Some(t) = p.timeout_ms {
            check_range("timeout_ms", t, 0, MAX_PLAY_WAIT_MS)?;
        }
        self.media_entry(&p.call_id)?;
        let cfg = self.speech_snapshot();
        let syn = speech::synthesize(&cfg, &self.inner.config_dir, &p.text, p.voice.as_deref())
            .await
            .map_err(ApiError::from)?;
        let clip_ms = syn.samples48.len() as u64 / 48;
        // The call may have ended while the TTS ran.
        let entry = self.media_entry(&p.call_id)?;
        let mut res = self
            .start_play(
                &entry,
                PlaySpec {
                    clip: ClipSource::Pcm(Arc::from(syn.samples48)),
                    looped: false,
                    clip_ms: Some(clip_ms),
                    duration_ms: None,
                    queue: p.queue.unwrap_or(false),
                    gain_db,
                    barge_in: p.barge_in.unwrap_or(false),
                    wait: p.wait.unwrap_or(false),
                    timeout_ms: p.timeout_ms,
                },
            )
            .await?;
        res["tts_ms"] = syn.tts_ms.into();
        Ok(res)
    }

    // ---- call.listen ----------------------------------------------------------------------

    async fn call_listen(&self, p: ListenParam) -> Result<Value, ApiError> {
        let start_timeout_ms = check_range(
            "start_timeout_ms",
            p.start_timeout_ms.unwrap_or(8_000),
            20,
            MAX_LISTEN_START_MS,
        )?;
        let end_silence_ms = check_range(
            "end_silence_ms",
            p.end_silence_ms.unwrap_or(800),
            20,
            10_000,
        )?;
        let max_ms = check_range("max_ms", p.max_ms.unwrap_or(30_000), 100, MAX_LISTEN_MAX_MS)?;
        let min_speech_ms =
            check_range("min_speech_ms", p.min_speech_ms.unwrap_or(120), 20, 5_000)?;
        let pre_roll_ms = check_range("pre_roll_ms", p.pre_roll_ms.unwrap_or(300), 0, 5_000)?;
        if let Some(t) = p.threshold_dbfs {
            check_range("threshold_dbfs", t, -90.0, 0.0)?;
        }
        if let Some(t) = p.timeout_ms {
            check_range("timeout_ms", t, 1, MAX_LISTEN_WALL_MS)?;
        }
        let rate = check_rate(p.sample_rate)?;
        let transcribe = p.transcribe.unwrap_or(false);
        if transcribe {
            self.require_speech()?;
        }
        let out = self
            .parse_audio_out(p.audio.as_deref(), p.path.as_deref(), transcribe)
            .await?;
        let cfg = self.speech_snapshot();
        if transcribe && cfg.stt_command.is_empty() {
            return Err(ApiError::invalid_state(
                "stt_command is not configured in [speech]",
            ));
        }
        let hub = self.media_entry(&p.call_id)?.handle.hub().clone();
        // Shown on the call until this request ends, however it ends.
        let _capture = self.capture_guard(&p.call_id, transcribe);
        if let Some(since) = p.since_rx_ms
            && since > hub.rx_ms() + 10_000
        {
            return Err(ApiError::invalid_params(format!(
                "since_rx_ms {since} is more than 10 s ahead of the RX timeline (now {} ms)",
                hub.rx_ms()
            )));
        }
        // A window starting in the future cannot finish before it starts: add the wait.
        let ahead_ms = p.since_rx_ms.map_or(0, |s| s.saturating_sub(hub.rx_ms()));
        let wall_ms = p
            .timeout_ms
            .unwrap_or(start_timeout_ms as u64 + max_ms as u64 + 5_000 + ahead_ms);
        let seq = self.event_seq();
        let wall0 = chrono::Utc::now();
        let rx0 = hub.rx_ms();
        let job = hub.listen(ListenParams {
            since_rx_ms: p.since_rx_ms,
            start_timeout_ms,
            end_silence_ms,
            max_ms,
            min_speech_ms,
            pre_roll_ms,
            threshold_dbfs: p.threshold_dbfs,
            // Always 16 kHz here: STT wants it; 8 kHz output is derived below.
            sample_rate: 16_000,
            timeout_ms: Some(wall_ms.min(u32::MAX as u64) as u32),
        });
        let o: ListenOutcome = tokio::time::timeout(Duration::from_millis(wall_ms + 5_000), job)
            .await
            .map_err(|_| ApiError::timeout("call.listen did not finish in time"))?
            .map_err(hub_err)?;

        let to_wall =
            |rx_ms: u64| wall0 + chrono::Duration::milliseconds(rx_ms as i64 - rx0 as i64);
        let dtmf = self.dtmf_between(
            &p.call_id,
            to_wall(o.start_rx_ms) - chrono::Duration::milliseconds(DTMF_SLACK_MS),
            to_wall(o.end_rx_ms) + chrono::Duration::milliseconds(DTMF_SLACK_MS),
        );

        let mut res = json!({
            "speech": o.speech,
            "reason": o.reason.as_str(),
            "start_rx_ms": o.start_rx_ms,
            "end_rx_ms": o.end_rx_ms,
            "speech_start_rx_ms": o.speech_start_rx_ms,
            "speech_end_rx_ms": o.speech_end_rx_ms,
            "speech_ms": o.speech_ms,
            "level_dbfs": {"peak": finite(o.level_peak_dbfs), "mean": finite(o.level_mean_dbfs)},
            "noise_floor_dbfs": finite(o.noise_floor_dbfs),
            "truncated": o.truncated,
            "dtmf": dtmf,
            "event_seq": seq,
        });
        if !o.speech {
            return Ok(res);
        }
        if transcribe {
            let t = speech::transcribe(
                &cfg,
                &self.inner.config_dir,
                &o.samples,
                p.language.as_deref(),
            )
            .await
            .map_err(|e| {
                merge_extra(
                    ApiError::from(e),
                    json!({
                        "start_rx_ms": o.start_rx_ms,
                        "end_rx_ms": o.end_rx_ms,
                        "speech_start_rx_ms": o.speech_start_rx_ms,
                        "speech_end_rx_ms": o.speech_end_rx_ms,
                    }),
                )
            })?;
            res["transcript"] = t.text.into();
            res["stt_ms"] = t.stt_ms.into();
        }
        if let Some(a) = self.render_audio(&out, &o.samples, rate).await? {
            res["audio"] = a;
        }
        Ok(res)
    }

    /// Digits of `call.dtmf` events of the call stamped within `[from, to]`.
    fn dtmf_between(
        &self,
        call_id: &str,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let filter = EventFilter {
            kinds: Some(vec!["call.dtmf".into()]),
            account: None,
            call_id: Some(call_id.to_string()),
        };
        // Page through the whole log: a bounded first page would only hold the oldest digits.
        const PAGE: usize = 1000;
        let mut digits = String::new();
        let mut cursor = 0u64;
        loop {
            let page = self.inner.events.since(cursor, &filter, PAGE);
            for e in &page {
                if chrono::DateTime::parse_from_rfc3339(&e.ts).is_ok_and(|t| t >= from && t <= to)
                    && let Some(d) = e.data["digit"].as_str()
                {
                    digits.push_str(d);
                }
            }
            match page.last() {
                Some(last) if page.len() == PAGE => cursor = last.seq,
                _ => break,
            }
        }
        digits
    }

    // ---- call.rx_audio --------------------------------------------------------------------

    async fn call_rx_audio(&self, p: RxAudioParam) -> Result<Value, ApiError> {
        let rate = check_rate(p.sample_rate)?;
        let transcribe = p.transcribe.unwrap_or(false);
        if transcribe {
            self.require_speech()?;
        }
        let out = self
            .parse_audio_out(p.audio.as_deref(), p.path.as_deref(), transcribe)
            .await?;
        let range = match (p.last_ms, p.since_rx_ms) {
            (Some(_), Some(_)) => {
                return Err(ApiError::invalid_params(
                    "give `last_ms` or `since_rx_ms`, not both",
                ));
            }
            (None, None) | (Some(_), None) => {
                if p.until_rx_ms.is_some() {
                    return Err(ApiError::invalid_params(
                        "`until_rx_ms` goes with `since_rx_ms`",
                    ));
                }
                let ms = check_range("last_ms", p.last_ms.unwrap_or(5_000), 1, MAX_RX_AUDIO_MS)?;
                if matches!(out, AudioOut::Base64) && ms > MAX_INLINE_RX_MS {
                    return Err(inline_rx_too_long());
                }
                SnapshotRange::Last { ms }
            }
            (None, Some(since)) => SnapshotRange::Span {
                since_rx_ms: since,
                until_rx_ms: p.until_rx_ms,
            },
        };
        let cfg = self.speech_snapshot();
        if transcribe && cfg.stt_command.is_empty() {
            return Err(ApiError::invalid_state(
                "stt_command is not configured in [speech]",
            ));
        }
        let hub = self.media_entry(&p.call_id)?.handle.hub().clone();
        let _capture = self.capture_guard(&p.call_id, transcribe);
        let seq = self.event_seq();
        let snap = hub.snapshot(range).await.map_err(hub_err)?;
        if matches!(out, AudioOut::Base64) && snap.samples16.len() as u64 > MAX_INLINE_RX_MS * 16 {
            return Err(inline_rx_too_long());
        }
        let mut res = json!({
            "start_rx_ms": snap.start_rx_ms,
            "end_rx_ms": snap.end_rx_ms,
            "duration_ms": snap.samples16.len() as u64 / 16,
            "truncated": snap.truncated,
            "level_dbfs": {
                "peak": finite(snap.level_peak_dbfs), "mean": finite(snap.level_mean_dbfs)
            },
            "event_seq": seq,
        });
        if transcribe && !snap.samples16.is_empty() {
            let t = speech::transcribe(
                &cfg,
                &self.inner.config_dir,
                &snap.samples16,
                p.language.as_deref(),
            )
            .await
            .map_err(ApiError::from)?;
            res["transcript"] = t.text.into();
            res["stt_ms"] = t.stt_ms.into();
        }
        if let Some(a) = self.render_audio(&out, &snap.samples16, rate).await? {
            res["audio"] = a;
        }
        Ok(res)
    }

    // ---- speech.* -------------------------------------------------------------------------

    async fn speech_transcribe(&self, mut p: TranscribeParam) -> Result<Value, ApiError> {
        self.require_speech()?;
        if p.path.is_some() == p.wav_base64.is_some() {
            return Err(ApiError::invalid_params(
                "give exactly one of `path` and `wav_base64`",
            ));
        }
        let cfg = self.speech_snapshot();
        if cfg.stt_command.is_empty() {
            return Err(ApiError::invalid_state(
                "stt_command is not configured in [speech]",
            ));
        }
        let samples16: Vec<i16> = if let Some(b64) = p.wav_base64.take() {
            let bytes = wav::decode_base64_capped(&b64, MAX_INLINE_AUDIO_BYTES)
                .map_err(|e| ApiError::invalid_params(format!("wav_base64: {e:#}")))?;
            drop(b64);
            release_big_request_permit();
            tokio::task::spawn_blocking(move || {
                decode_wav_capped(Cursor::new(bytes), MAX_TRANSCRIBE_SECS as u32)
                    .map(|v| to_16k(&v))
            })
            .await
            .map_err(io_err)?
            .map_err(|e| ApiError::invalid_params(format!("wav_base64: {e:#}")))?
        } else {
            let path = self.resolve_path(std::path::Path::new(p.path.as_deref().unwrap_or("")));
            let shown = path.display().to_string();
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<i16>> {
                let open = || std::fs::File::open(&path).map(std::io::BufReader::new);
                let r = hound::WavReader::new(open()?)?;
                let secs = r.duration() as u64 / r.spec().sample_rate.max(1) as u64;
                if secs > MAX_TRANSCRIBE_SECS {
                    anyhow::bail!("audio is longer than {MAX_TRANSCRIBE_SECS} s");
                }
                drop(r);
                Ok(to_16k(&decode_wav_capped(
                    open()?,
                    MAX_TRANSCRIBE_SECS as u32,
                )?))
            })
            .await
            .map_err(io_err)?
            .map_err(|e| {
                ApiError::invalid_params(format!("{}: {e:#}", siple_config::echo(&shown)))
            })?
        };
        let t = speech::transcribe(
            &cfg,
            &self.inner.config_dir,
            &samples16,
            p.language.as_deref(),
        )
        .await
        .map_err(ApiError::from)?;
        Ok(json!({
            "transcript": t.text,
            "stt_ms": t.stt_ms,
            "duration_ms": samples16.len() as u64 / 16,
        }))
    }

    async fn speech_synthesize(&self, p: SynthesizeParam) -> Result<Value, ApiError> {
        self.require_speech()?;
        check_text(&p.text)?;
        let out = self
            .parse_audio_out(
                Some(if p.path.is_some() { "file" } else { "base64" }),
                p.path.as_deref(),
                false,
            )
            .await?;
        let cfg = self.speech_snapshot();
        let syn = speech::synthesize(&cfg, &self.inner.config_dir, &p.text, p.voice.as_deref())
            .await
            .map_err(ApiError::from)?;
        let duration_ms = syn.samples48.len() as u64 / 48;
        let mut res = json!({"duration_ms": duration_ms, "tts_ms": syn.tts_ms});
        match out {
            AudioOut::File(path) => {
                // Files keep the full 48 kHz; inline audio is sent at 16 kHz to stay small.
                let bytes = wav::encode_wav_i16(&syn.samples48, 48_000);
                tokio::fs::write(&path, &bytes).await.map_err(|e| {
                    ApiError::invalid_params(format!("writing {}: {e}", path.display()))
                })?;
                res["path"] = path.display().to_string().into();
            }
            _ => {
                let s16 = wav::downsample_i16(&syn.samples48, 16_000)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                let bytes = wav::encode_wav_i16(&s16, 16_000);
                res["audio"] = json!({
                    "format": "wav", "sample_rate": 16_000, "channels": 1,
                    "duration_ms": duration_ms,
                    "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                });
            }
        }
        Ok(res)
    }
}

/// Fails with `invalid_params` now when `path` cannot be written (unwritable directory or
/// file), instead of after the capture / synthesis. A file that did not exist is removed again.
/// Runs on the blocking pool; existing non-regular files (FIFOs, devices) and dangling symlinks
/// are not opened at all.
async fn probe_writable(path: &std::path::Path) -> Result<(), ApiError> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || probe_writable_blocking(&p))
        .await
        .map_err(io_err)?
}

fn probe_writable_blocking(path: &std::path::Path) -> Result<(), ApiError> {
    let err = |e: std::io::Error| {
        ApiError::invalid_params(format!("cannot write {}: {e}", path.display()))
    };
    if std::fs::symlink_metadata(path).is_ok() {
        // Something is there: follow symlinks to see what it is. Only a regular file is
        // probed (never created, so a dangling link does not grow a stray target).
        return match std::fs::metadata(path) {
            Ok(m) if m.is_file() => std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .map(drop)
                .map_err(err),
            _ => Ok(()),
        };
    }
    match std::fs::OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(path)
    {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(path);
            Ok(())
        }
        // Created by someone else in between: it exists now, which is fine.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(err(e)),
    }
}

/// A decoded play request.
struct PlaySpec {
    clip: ClipSource,
    looped: bool,
    /// Length of one pass of the clip; `None` for generators.
    clip_ms: Option<u64>,
    duration_ms: Option<u32>,
    queue: bool,
    gain_db: f32,
    barge_in: bool,
    wait: bool,
    timeout_ms: Option<u64>,
}

// ---- schemas -----------------------------------------------------------------------------

/// Schemas of [`AGENT_METHODS`], chained into `backend_methods()`.
pub fn agent_methods() -> Vec<MethodInfo> {
    let call_id = json!({"type":"string","description":"Call id (c1, c2, ...)."});
    let obj = |props: Value, required: &[&str]| json!({"type":"object","properties":props,"required":required,"additionalProperties":false});
    let audio_mode = json!({"type":"string","enum":["base64","none","file"],
        "description":"How to return the audio: base64 (a WAV in audio.base64; the default), none (the default with transcribe), or file (write to `path`)."});
    let path = json!({"type":"string","description":"Output .wav path for audio:\"file\" (relative paths: the config file's directory)."});
    let rate = json!({"type":"integer","enum":[16000,8000],"description":"Sample rate of the returned WAV (default 16000)."});
    let gain =
        json!({"type":"number","minimum":-40,"maximum":20,"description":"Gain in dB (default 0)."});
    let timeout_ms = json!({"type":"integer","minimum":0,"maximum":MAX_PLAY_WAIT_MS,
        "description":"With wait: give up after this long (default: clip length + 30 s, at most 120000). The play keeps going; the error carries play_id."});
    vec![
        MethodInfo::new(
            "call.say",
            "Speak `text` into the call with the configured text-to-speech command ([speech] tts_command; without one this fails with invalid_state). Same options and result as call.play (play_id, duration_ms, rx_ms, finished with wait) plus tts_ms. For turn-taking use wait:true, then call.listen {since_rx_ms: finished.barge_in_rx_ms if present else finished.rx_ms}.",
            obj(
                json!({"call_id":call_id.clone(),
                "text":{"type":"string","minLength":1,"maxLength":MAX_TEXT_CHARS},
                "voice":{"type":"string","description":"TTS voice (letters, digits and _.+:- only); default [speech] tts_voice."},
                "queue":{"type":"boolean","description":"Queue behind the current play instead of replacing it (default false)."},
                "wait":{"type":"boolean","description":"Return when the speech has finished playing (result.finished)."},
                "timeout_ms":timeout_ms.clone(),
                "barge_in":{"type":"boolean","description":"Stop the speech when the far end starts talking (default false; echo paths trigger it themselves)."},
                "gain_db":gain.clone()}),
                &["call_id", "text"],
            ),
        ),
        MethodInfo::new(
            "call.listen",
            "Wait for the far end to say something and return it. Voice activity detection finds the utterance on the call's receive timeline. Returns {speech, reason: end_of_speech|max_duration|no_speech|call_ended|timeout, start_rx_ms, end_rx_ms, speech_start_rx_ms, speech_end_rx_ms, speech_ms, level_dbfs{peak,mean}, noise_floor_dbfs, truncated, dtmf (digits received in the window), transcript?, stt_ms?, audio?{format:wav, sample_rate, channels, duration_ms, base64|path}, event_seq}. After you speak (call.say/call.play with wait), pass its finished.barge_in_rx_ms if present (the caller interrupted: where they began), else finished.rx_ms (end of your own audio), as since_rx_ms; when listening again without speaking in between, pass the previous end_rx_ms. since_rx_ms in the past (up to rx_history_secs, at least 5 s) is analysed from recorded audio, so thinking time between turns loses nothing, and start_timeout_ms counts only from the moment of the request. rx_ms restarts if the call media is recreated at answer with a different source/sink; a listen issued before that ends with call_ended (see call.answer). With transcribe:true the audio is returned only if audio is set explicitly. Tones count as speech.",
            obj(
                json!({"call_id":call_id.clone(),
                "since_rx_ms":{"type":"integer","minimum":0,"description":"Start of the window on the receive timeline (default: now). Older than the history: clamped, truncated:true. More than 10 s ahead of now: invalid_params. After speaking use finished.barge_in_rx_ms (present when the caller interrupted) else finished.rx_ms from call.play/say; listening again without speaking, use end_rx_ms from the previous listen."},
                "start_timeout_ms":{"type":"integer","minimum":20,"maximum":MAX_LISTEN_START_MS,"description":"Give up with reason no_speech when nothing is said within this long (default 8000)."},
                "end_silence_ms":{"type":"integer","minimum":20,"maximum":10000,"description":"Silence that ends the utterance (default 800)."},
                "max_ms":{"type":"integer","minimum":100,"maximum":MAX_LISTEN_MAX_MS,"description":"Longest utterance (default 30000)."},
                "min_speech_ms":{"type":"integer","minimum":20,"maximum":5000,"description":"Voiced time that counts as speech (default 120)."},
                "pre_roll_ms":{"type":"integer","minimum":0,"maximum":5000,"description":"Audio kept before the speech starts, within the window (default 300)."},
                "threshold_dbfs":{"type":"number","minimum":-90,"maximum":0,"description":"Lowest level that counts as voice (default -45; the detector also adapts to the noise floor)."},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_LISTEN_WALL_MS,"description":"Wall-clock limit for the whole call (default start_timeout_ms + max_ms + 5 s)."},
                "transcribe":{"type":"boolean","description":"Run the [speech] stt_command on the speech (invalid_state when not configured)."},
                "language":{"type":"string","description":"STT language (letters, digits and _.+:- only); default [speech] stt_language."},
                "audio":audio_mode.clone(),"path":path.clone(),"sample_rate":rate.clone()}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "call.rx_audio",
            "Fetch audio that was already received on the call, from the receive history (rx_history_secs, at least 5 s). Give last_ms (default 5000) or since_rx_ms [+ until_rx_ms]. Returns {start_rx_ms, end_rx_ms, duration_ms, truncated, level_dbfs{peak,mean}, audio?, transcript?, stt_ms?}.",
            obj(
                json!({"call_id":call_id.clone(),
                "last_ms":{"type":"integer","minimum":1,"maximum":MAX_RX_AUDIO_MS,"description":"The most recent this many ms (default 5000). Not with since_rx_ms. Inline base64 audio is limited to 120 s: longer ranges need audio:\"file\" (or \"none\")."},
                "since_rx_ms":{"type":"integer","minimum":0},
                "until_rx_ms":{"type":"integer","minimum":0,"description":"End of the range (default now)."},
                "sample_rate":rate,
                "audio":audio_mode,"path":path.clone(),
                "transcribe":{"type":"boolean"},"language":{"type":"string"}}),
                &["call_id"],
            ),
        ),
        MethodInfo::new(
            "speech.transcribe",
            "Transcribe a WAV file or inline WAV with the [speech] stt_command (invalid_state when not configured). Returns {transcript, stt_ms, duration_ms}.",
            obj(
                json!({"path":{"type":"string","description":"WAV file (relative paths: the config file's directory)."},
                "wav_base64":{"type":"string","description":"Base64 WAV (at most 10 MiB decoded). Give this or path."},
                "language":{"type":"string"}}),
                &[],
            ),
        ),
        MethodInfo::new(
            "speech.synthesize",
            "Turn text into a WAV with the [speech] tts_command (invalid_state when not configured). With `path` the 48 kHz WAV is written there; otherwise audio.base64 carries a 16 kHz WAV. Returns {duration_ms, tts_ms, path|audio}.",
            obj(
                json!({"text":{"type":"string","minLength":1,"maxLength":MAX_TEXT_CHARS},
                "voice":{"type":"string"},
                "path":{"type":"string","description":"Output .wav path (relative paths: the config file's directory)."}}),
                &["text"],
            ),
        ),
    ]
}

#[cfg(test)]
mod probe_tests {
    use super::probe_writable;

    #[tokio::test]
    async fn probe_leaves_no_traces() {
        let dir = std::env::temp_dir().join(format!("zp-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // New file: probed and removed again.
        let new = dir.join("new.wav");
        probe_writable(&new).await.unwrap();
        assert!(!new.exists());
        // Existing file stays, content untouched.
        let old = dir.join("old.wav");
        std::fs::write(&old, b"x").unwrap();
        probe_writable(&old).await.unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), b"x");
        // Unwritable directory fails.
        assert!(probe_writable(&dir.join("missing/x.wav")).await.is_err());
        #[cfg(unix)]
        {
            // Dangling symlink: it stays, and its target is not created.
            let target = dir.join("target.wav");
            let link = dir.join("link.wav");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            probe_writable(&link).await.unwrap();
            assert!(std::fs::symlink_metadata(&link).is_ok());
            assert!(!target.exists());
            // A symlink to an existing file is left alone too.
            let link2 = dir.join("link2.wav");
            std::os::unix::fs::symlink(&old, &link2).unwrap();
            probe_writable(&link2).await.unwrap();
            assert!(std::fs::symlink_metadata(&link2).is_ok());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
