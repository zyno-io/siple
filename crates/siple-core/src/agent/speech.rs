//! External TTS / STT commands (`[speech]` config).
//!
//! Commands are argv vectors from the config file; **no shell is ever involved**. User text
//! reaches the child only on stdin and in the `{text_file}` file, never in argv. Placeholders
//! are substituted inside argv elements, and the same values are exported as `ZP_*`
//! environment variables.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;

use super::MAX_CLIP_SECS;
use super::wav::{decode_wav_48k, encode_wav_i16, f32_to_i16};
use crate::config::{SpeechConfig, is_speech_token, template_placeholders};
use crate::control::{ApiError, ErrorKind};

/// Largest accepted TTS stdout / output file.
pub const MAX_TTS_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Largest accepted STT stdout / output file.
pub const MAX_STT_OUTPUT_BYTES: usize = 1024 * 1024;
/// Stderr kept for error reports.
pub const STDERR_TAIL_BYTES: usize = 64 * 1024;

static TTS_SLOTS: Semaphore = Semaphore::const_new(2);
static STT_SLOTS: Semaphore = Semaphore::const_new(2);

/// Result of [`synthesize`].
#[derive(Debug, Clone, PartialEq)]
pub struct Synthesis {
    /// Mono `i16` at 48 kHz, at most [`super::MAX_CLIP_SECS`] long.
    pub samples48: Vec<i16>,
    /// Wall time the TTS command took.
    pub tts_ms: u64,
}

/// Result of [`transcribe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    /// Wall time the STT command took.
    pub stt_ms: u64,
}

/// Why a speech job failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpeechError {
    /// No command configured (`"tts"` or `"stt"`).
    #[error("{0}_command is not configured in [speech]")]
    NotConfigured(&'static str),
    /// A per-request voice / language that fails the charset check.
    #[error("{0}")]
    InvalidArgument(String),
    /// The command ran past its timeout and was killed.
    #[error("{what} command timed out after {ms} ms")]
    Timeout { what: &'static str, ms: u64 },
    /// Non-zero exit, spawn failure or unusable output.
    #[error("{message}")]
    Failed {
        message: String,
        exit_code: Option<i32>,
        /// Last 64 KiB of stderr.
        stderr_tail: String,
    },
    /// Anything else (including "not implemented yet").
    #[error("{0}")]
    Internal(String),
    /// This edition cannot run external commands (Mac App Store build).
    #[error(
        "speech hooks are not available in this edition (the Mac App Store and Flatpak editions cannot run \
         external commands); use `siple-cli stream` or base64 audio with your own TTS/STT instead"
    )]
    Unsupported,
}

/// Fails with [`SpeechError::Unsupported`] when `channel` cannot run external commands.
pub fn check_supported(channel: &siple_config::channel::Channel) -> Result<(), SpeechError> {
    if channel.can_exec {
        Ok(())
    } else {
        Err(SpeechError::Unsupported)
    }
}

/// Logs that `[speech]` commands are configured but ignored by `channel`.
pub fn warn_if_unsupported(cfg: &SpeechConfig, channel: &siple_config::channel::Channel) {
    if !channel.can_exec && (!cfg.tts_command.is_empty() || !cfg.stt_command.is_empty()) {
        tracing::warn!(
            "[speech] tts_command / stt_command are ignored: speech hooks are not available in this edition"
        );
    }
}

impl From<SpeechError> for ApiError {
    fn from(e: SpeechError) -> Self {
        match &e {
            SpeechError::NotConfigured(_) => ApiError::new(ErrorKind::InvalidState, e.to_string()),
            SpeechError::InvalidArgument(_) => ApiError::invalid_params(e.to_string()),
            SpeechError::Timeout { .. } => ApiError::timeout(e.to_string()),
            SpeechError::Failed {
                exit_code,
                stderr_tail,
                ..
            } => ApiError::internal(e.to_string()).with_extra(
                serde_json::json!({ "exit_code": exit_code, "stderr_tail": stderr_tail }),
            ),
            SpeechError::Internal(_) => ApiError::internal(e.to_string()),
            SpeechError::Unsupported => ApiError::new(ErrorKind::Unsupported, e.to_string()),
        }
    }
}

/// Replaces `{name}` with its value and `{{` / `}}` with literal braces. Unknown names and
/// unbalanced braces are errors.
pub fn expand_template(arg: &str, vars: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::with_capacity(arg.len() + 32);
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(ch) => name.push(ch),
                        None => return Err("unclosed `{`".into()),
                    }
                }
                match vars.iter().find(|(k, _)| *k == name) {
                    Some((_, v)) => out.push_str(v),
                    None => return Err(format!("unknown placeholder `{{{name}}}`")),
                }
            }
            '}' => return Err("unmatched `}`".into()),
            _ => out.push(c),
        }
    }
    Ok(out)
}

fn references(argv: &[String], name: &str) -> bool {
    argv.iter()
        .any(|a| template_placeholders(a).is_ok_and(|p| p.iter().any(|n| n == name)))
}

struct Job<'a> {
    what: &'static str,
    argv: &'a [String],
    vars: Vec<(&'static str, String)>,
    env: Vec<(&'static str, String)>,
    stdin: Option<Vec<u8>>,
    cwd: &'a Path,
    /// Time left for the run (the configured timeout minus the slot wait).
    timeout: Duration,
    /// The configured timeout, reported in `SpeechError::Timeout`.
    timeout_ms: u64,
    stdout_cap: usize,
}

struct Output {
    stdout: Vec<u8>,
}

fn failed(message: impl Into<String>, exit_code: Option<i32>, stderr_tail: String) -> SpeechError {
    SpeechError::Failed {
        message: message.into(),
        exit_code,
        stderr_tail,
    }
}

fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // The child leads its own process group (`process_group(0)`), so its pid is the pgid.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Kills the child's whole process group on drop unless disarmed. `kill_on_drop` only reaches
/// the direct child, so without this a dropped job future (client disconnect) would leave
/// grandchildren running after its semaphore slot is released.
struct GroupKillGuard(Option<u32>);

impl GroupKillGuard {
    /// Called once the child has been reaped (its pid/pgid may be recycled from then on).
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// Waits for a job slot, bounded by `total`. Returns the permit and the time left for the run,
/// so the caller's timeout covers queueing plus execution.
async fn acquire_slot(
    sem: &'static Semaphore,
    what: &'static str,
    total: Duration,
) -> Result<(tokio::sync::SemaphorePermit<'static>, Duration), SpeechError> {
    let t = Instant::now();
    let ms = total.as_millis() as u64;
    match tokio::time::timeout(total, sem.acquire()).await {
        Err(_) => Err(SpeechError::Timeout { what, ms }),
        Ok(Err(_)) => Err(SpeechError::Internal(format!("{what} slots closed"))),
        Ok(Ok(p)) => Ok((p, total.saturating_sub(t.elapsed()))),
    }
}

/// Reads up to `cap` bytes; anything beyond is drained and discarded. Returns whether it
/// overflowed (killing the process group on first overflow).
async fn read_capped<R: AsyncRead + Unpin>(
    mut r: R,
    cap: usize,
    pid: Option<u32>,
) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut tmp = vec![0u8; 16 * 1024];
    let mut over = false;
    loop {
        match r.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if over {
                    continue;
                }
                if buf.len() + n > cap {
                    over = true;
                    buf.clear();
                    kill_group(pid);
                } else {
                    buf.extend_from_slice(&tmp[..n]);
                }
            }
        }
    }
    (buf, over)
}

/// Keeps only the last [`STDERR_TAIL_BYTES`] bytes.
async fn read_tail<R: AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > 2 * STDERR_TAIL_BYTES {
                    buf.drain(..buf.len() - STDERR_TAIL_BYTES);
                }
            }
        }
    }
    if buf.len() > STDERR_TAIL_BYTES {
        buf.drain(..buf.len() - STDERR_TAIL_BYTES);
    }
    buf
}

fn tail_string(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Spawns the job's argv (never through a shell) and collects stdout and the stderr tail.
async fn run(job: Job<'_>) -> Result<Output, SpeechError> {
    let vars: Vec<(&str, &str)> = job.vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut args = Vec::with_capacity(job.argv.len());
    for (i, a) in job.argv.iter().enumerate() {
        args.push(expand_template(a, &vars).map_err(|e| {
            SpeechError::Internal(format!("{}_command element {i}: {e}", job.what))
        })?);
    }
    let Some((prog, rest)) = args.split_first() else {
        return Err(SpeechError::NotConfigured(job.what));
    };
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .current_dir(job.cwd)
        .stdin(if job.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // The AppImage's bundled libraries are for Siple only; the hook gets the host's library path.
    siple_config::channel::apply_host_env(cmd.as_std_mut());
    for (k, v) in &job.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| {
        failed(
            format!("cannot start {} command `{prog}`: {e}", job.what),
            None,
            String::new(),
        )
    })?;
    let pid = child.id();
    let mut guard = GroupKillGuard(pid);
    let stdin = child.stdin.take();
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(SpeechError::Internal("child pipes unavailable".into()));
    };
    let input = job.stdin.clone();
    let cap = job.stdout_cap;
    let io = async {
        let write = async {
            if let (Some(mut si), Some(data)) = (stdin, input) {
                let _ = si.write_all(&data).await;
                let _ = si.shutdown().await;
            }
        };
        let ((), out, err) = tokio::join!(write, read_capped(stdout, cap, pid), read_tail(stderr));
        let status = child.wait().await;
        (out, err, status)
    };
    let ms = job.timeout_ms;
    match tokio::time::timeout(job.timeout, io).await {
        Err(_) => {
            // `guard` kills the whole group on return; `child` died with the canceled future.
            Err(SpeechError::Timeout { what: job.what, ms })
        }
        Ok(((stdout, over), err, status)) => {
            if status.is_ok() {
                guard.disarm();
            }
            let tail = tail_string(&err);
            let status = status.map_err(|e| {
                failed(
                    format!("waiting for {} command: {e}", job.what),
                    None,
                    tail.clone(),
                )
            })?;
            if over {
                return Err(failed(
                    format!("{} command output exceeds {} bytes", job.what, cap),
                    status.code(),
                    tail,
                ));
            }
            if !status.success() {
                let msg = match status.code() {
                    Some(c) => format!("{} command exited with status {c}", job.what),
                    None => format!("{} command was terminated by a signal ({status})", job.what),
                };
                return Err(failed(msg, status.code(), tail));
            }
            Ok(Output { stdout })
        }
    }
}

fn read_file_capped(path: &Path, cap: usize, what: &str) -> Result<Vec<u8>, SpeechError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| {
        failed(
            format!("{what} command produced no output file: {e}"),
            None,
            String::new(),
        )
    })?;
    // Read through `take(cap + 1)`: a file that grows, or a symlink to /dev/zero, can never
    // allocate more than the cap.
    let mut buf = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| failed(format!("reading {what} output: {e}"), None, String::new()))?;
    if buf.len() > cap {
        return Err(failed(
            format!("{what} output file exceeds {cap} bytes"),
            None,
            String::new(),
        ));
    }
    Ok(buf)
}

fn new_tempdir() -> Result<tempfile::TempDir, SpeechError> {
    // tempfile creates the directory with mode 0700 on unix.
    tempfile::Builder::new()
        .prefix("siple-speech-")
        .tempdir()
        .map_err(|e| SpeechError::Internal(format!("cannot create temp dir: {e}")))
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn pick_token(explicit: Option<&str>, default: &str, what: &str) -> Result<String, SpeechError> {
    let v = match explicit {
        Some(v) if !v.is_empty() => v,
        _ => default,
    };
    if !v.is_empty() && !is_speech_token(v) {
        return Err(SpeechError::InvalidArgument(format!(
            "{what} must match [A-Za-z0-9][A-Za-z0-9_.+:-]{{0,63}}"
        )));
    }
    Ok(v.to_string())
}

/// Speaks `text` with `cfg.tts_command`. `voice` overrides `cfg.tts_voice`; `config_dir` is the
/// working directory of the command. `cfg` is a snapshot taken when the job starts.
///
/// Output is the file named by `{out}` when the argv references it, else stdout, which must be
/// a WAV. The result is mono 48 kHz `i16`, at most [`MAX_CLIP_SECS`] long.
pub async fn synthesize(
    cfg: &SpeechConfig,
    config_dir: &Path,
    text: &str,
    voice: Option<&str>,
) -> Result<Synthesis, SpeechError> {
    if cfg.tts_command.is_empty() {
        return Err(SpeechError::NotConfigured("tts"));
    }
    let voice = pick_token(voice, &cfg.tts_voice, "voice")?;
    let started = Instant::now();
    // The configured timeout covers the wait for a slot as well as the run itself.
    let (_permit, run_budget) = acquire_slot(
        &TTS_SLOTS,
        "tts",
        Duration::from_millis(u64::from(cfg.tts_timeout_ms)),
    )
    .await?;
    let dir = new_tempdir()?;
    let text_file = dir.path().join("text.txt");
    let out_file = dir.path().join("out.wav");
    let mut payload = text.as_bytes().to_vec();
    payload.push(b'\n');
    std::fs::write(&text_file, &payload)
        .map_err(|e| SpeechError::Internal(format!("cannot write text file: {e}")))?;
    let use_file = references(&cfg.tts_command, "out");
    let (tf, of) = (path_str(&text_file), path_str(&out_file));
    let out = run(Job {
        what: "tts",
        argv: &cfg.tts_command,
        vars: vec![
            ("text_file", tf.clone()),
            ("out", of.clone()),
            ("voice", voice.clone()),
        ],
        env: vec![("ZP_TEXT_FILE", tf), ("ZP_OUT", of), ("ZP_VOICE", voice)],
        stdin: Some(payload),
        cwd: config_dir,
        timeout: run_budget,
        timeout_ms: u64::from(cfg.tts_timeout_ms),
        stdout_cap: MAX_TTS_OUTPUT_BYTES,
    })
    .await?;
    let wav = if use_file {
        read_file_capped(&out_file, MAX_TTS_OUTPUT_BYTES, "tts")?
    } else {
        out.stdout
    };
    drop(dir);
    let samples48 = tokio::task::spawn_blocking(move || decode_tts_wav(wav))
        .await
        .map_err(|e| SpeechError::Internal(format!("decode task: {e}")))??;
    Ok(Synthesis {
        samples48,
        tts_ms: started.elapsed().as_millis() as u64,
    })
}

/// Patches the RIFF and `data` chunk sizes of a streamed WAV from the actual buffer length.
/// Encoders writing to a pipe cannot seek back, so they leave placeholders (ffmpeg writes
/// 0xFFFFFFFF, some tools 0x7ffff000). Sizes that fit the buffer are left alone.
fn patch_streamed_wav(wav: &mut [u8]) {
    if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return;
    }
    let mut off = 12usize;
    while off + 8 <= wav.len() {
        let size = u32::from_le_bytes([wav[off + 4], wav[off + 5], wav[off + 6], wav[off + 7]]);
        let body = off + 8;
        let remaining = wav.len() - body;
        if &wav[off..off + 4] == b"data" {
            if size as usize > remaining {
                wav[off + 4..off + 8].copy_from_slice(&(remaining as u32).to_le_bytes());
            }
            let riff = (wav.len() - 8).min(u32::MAX as usize) as u32;
            wav[4..8].copy_from_slice(&riff.to_le_bytes());
            return;
        }
        // Chunks are word aligned.
        let next = body as u64 + u64::from(size) + u64::from(size & 1);
        if next > wav.len() as u64 {
            return;
        }
        off = next as usize;
    }
}

fn decode_tts_wav(mut wav: Vec<u8>) -> Result<Vec<i16>, SpeechError> {
    let bad = |m: String| {
        failed(
            format!("tts output is not usable audio: {m}"),
            None,
            String::new(),
        )
    };
    patch_streamed_wav(&mut wav);
    // The decoder rejects out-of-range sample rates and over-long headers before decoding.
    let f = decode_wav_48k(Cursor::new(&wav), MAX_CLIP_SECS).map_err(|e| bad(e.to_string()))?;
    if f.len() > MAX_CLIP_SECS as usize * 48_000 {
        return Err(bad(format!("longer than {MAX_CLIP_SECS} s")));
    }
    Ok(f32_to_i16(&f))
}

/// Transcribes mono 16 kHz audio with `cfg.stt_command`. `language` overrides
/// `cfg.stt_language`.
///
/// The input is written as a 16 kHz mono s16 WAV (`{in}`). The transcript is read from
/// `{out_txt}` when referenced (or from `{out_base}.txt` when only `{out_base}` is, falling
/// back to stdout if that file is missing), else from stdout; it is trimmed.
pub async fn transcribe(
    cfg: &SpeechConfig,
    config_dir: &Path,
    samples16: &[i16],
    language: Option<&str>,
) -> Result<Transcript, SpeechError> {
    if cfg.stt_command.is_empty() {
        return Err(SpeechError::NotConfigured("stt"));
    }
    let lang = pick_token(language, &cfg.stt_language, "language")?;
    let started = Instant::now();
    // The configured timeout covers the wait for a slot as well as the run itself.
    let (_permit, run_budget) = acquire_slot(
        &STT_SLOTS,
        "stt",
        Duration::from_millis(u64::from(cfg.stt_timeout_ms)),
    )
    .await?;
    let dir = new_tempdir()?;
    let in_file = dir.path().join("in.wav");
    let out_base = dir.path().join("out");
    let out_txt: PathBuf = dir.path().join("out.txt");
    std::fs::write(&in_file, encode_wav_i16(samples16, 16_000))
        .map_err(|e| SpeechError::Internal(format!("cannot write input wav: {e}")))?;
    let want_txt = references(&cfg.stt_command, "out_txt");
    let want_base = references(&cfg.stt_command, "out_base");
    let (inp, base, txt) = (path_str(&in_file), path_str(&out_base), path_str(&out_txt));
    let out = run(Job {
        what: "stt",
        argv: &cfg.stt_command,
        vars: vec![
            ("in", inp.clone()),
            ("lang", lang.clone()),
            ("out_base", base),
            ("out_txt", txt.clone()),
        ],
        env: vec![("ZP_IN", inp), ("ZP_LANG", lang), ("ZP_OUT_TXT", txt)],
        stdin: None,
        cwd: config_dir,
        timeout: run_budget,
        timeout_ms: u64::from(cfg.stt_timeout_ms),
        stdout_cap: MAX_STT_OUTPUT_BYTES,
    })
    .await?;
    let raw = if want_txt || (want_base && out_txt.exists()) {
        read_file_capped(&out_txt, MAX_STT_OUTPUT_BYTES, "stt")?
    } else {
        out.stdout
    };
    drop(dir);
    Ok(Transcript {
        text: String::from_utf8_lossy(&raw).trim().to_string(),
        stt_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_substitutes_and_escapes() {
        let v = [("out", "/t/o.wav"), ("voice", "en-us")];
        assert_eq!(expand_template("--o={out}", &v).unwrap(), "--o=/t/o.wav");
        assert_eq!(expand_template("{voice}{voice}", &v).unwrap(), "en-usen-us");
        assert_eq!(expand_template("{{out}} {{", &v).unwrap(), "{out} {");
        assert_eq!(expand_template("}}{{{out}}}", &v).unwrap(), "}{/t/o.wav}");
        assert_eq!(expand_template("plain", &v).unwrap(), "plain");
        assert!(expand_template("{nope}", &v).is_err());
        assert!(expand_template("{out", &v).is_err());
        assert!(expand_template("a}b", &v).is_err());
        // Substituted values are never re-expanded.
        assert_eq!(
            expand_template("{voice}", &[("voice", "{out}")]).unwrap(),
            "{out}"
        );
    }

    #[test]
    fn errors_map_to_api() {
        let e: ApiError = SpeechError::NotConfigured("tts").into();
        assert_eq!(e.kind, ErrorKind::InvalidState);
    }

    #[test]
    fn streamed_wav_placeholder_sizes_are_patched() {
        let good = encode_wav_i16(&vec![500i16; 4800], 24_000);
        for ph in [0xFFFF_FFFFu32, 0x7fff_f000] {
            let mut w = good.clone();
            w[4..8].copy_from_slice(&ph.to_le_bytes());
            w[40..44].copy_from_slice(&ph.to_le_bytes());
            let out = decode_tts_wav(w).unwrap();
            assert_eq!(out.len(), 9600);
        }
        // Extra chunk before `data` is walked past.
        let mut w = good[..36].to_vec();
        w.extend_from_slice(b"LIST");
        w.extend_from_slice(&4u32.to_le_bytes());
        w.extend_from_slice(&[1, 2, 3, 4]);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        w.extend_from_slice(&good[44..]);
        assert_eq!(decode_tts_wav(w).unwrap().len(), 9600);
    }

    #[test]
    fn tts_wav_rate_bounds() {
        for rate in [0u32, 7_999, 192_001, 3_000_000_000] {
            assert!(
                decode_tts_wav(encode_wav_i16(&[0; 8], rate)).is_err(),
                "{rate}"
            );
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::fs;

        struct Rig {
            dir: tempfile::TempDir,
        }

        impl Rig {
            fn new() -> Self {
                let dir = tempfile::tempdir().unwrap();
                let wav = encode_wav_i16(&vec![1000i16; 8000], 8000); // 1 s
                fs::write(dir.path().join("clip.wav"), wav).unwrap();
                Self { dir }
            }
            fn p(&self, name: &str) -> PathBuf {
                self.dir.path().join(name)
            }
            fn script(&self, body: &str) -> String {
                let p = self.p("stub.sh");
                fs::write(&p, body).unwrap();
                p.to_string_lossy().into_owned()
            }
            fn cfg(&self, body: &str, args: &[&str]) -> SpeechConfig {
                let mut argv = vec!["sh".to_string(), self.script(body)];
                argv.extend(args.iter().map(|s| s.to_string()));
                SpeechConfig {
                    tts_command: argv.clone(),
                    stt_command: argv,
                    tts_timeout_ms: 20_000,
                    stt_timeout_ms: 20_000,
                    ..Default::default()
                }
            }
            fn read(&self, name: &str) -> String {
                fs::read_to_string(self.p(name)).unwrap()
            }
        }

        /// Dead, or an unreaped zombie (containers without an init never reap orphans).
        fn pid_dead(pid: i32) -> bool {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit(')')
                        .next()
                        .map(|r| r.trim_start().starts_with('Z'))
                })
                .unwrap_or(false)
        }

        fn is_failed(e: &SpeechError) -> bool {
            matches!(e, SpeechError::Failed { .. })
        }

        #[tokio::test]
        async fn tts_substitution_env_and_file_mode() {
            let r = Rig::new();
            let mut cfg = r.cfg(
                "for a in \"$@\"; do printf '%s\\n' \"$a\" >> args.txt; done\n\
                 env | grep '^ZP_' | sort > env.txt\n\
                 pwd -P > cwd.txt\n\
                 cp clip.wav \"$ZP_OUT\"\n",
                &["x{voice}y", "{{lit}}", "--out={out}", "{text_file}"],
            );
            cfg.tts_voice = "en-us".into();
            cfg.tts_command.push("{out}".into());
            let s = synthesize(&cfg, r.dir.path(), "hello", None).await.unwrap();
            assert_eq!(s.samples48.len(), 48_000);
            let args: Vec<String> = r.read("args.txt").lines().map(String::from).collect();
            assert_eq!(args[0], "xen-usy");
            assert_eq!(args[1], "{lit}");
            let out = args[2].strip_prefix("--out=").unwrap().to_string();
            assert!(out.ends_with("/out.wav"), "{out}");
            assert_eq!(args[4], out);
            assert!(args[3].ends_with("/text.txt"));
            let env = r.read("env.txt");
            assert!(env.contains("ZP_VOICE=en-us"), "{env}");
            assert!(env.contains(&format!("ZP_OUT={out}")), "{env}");
            assert!(env.contains(&format!("ZP_TEXT_FILE={}", args[3])), "{env}");
            assert_eq!(
                fs::canonicalize(r.read("cwd.txt").trim()).unwrap(),
                fs::canonicalize(r.dir.path()).unwrap()
            );
            // The per-job temp dir is gone afterwards.
            assert!(!Path::new(&out).exists());
            // explicit voice overrides config
            synthesize(&cfg, r.dir.path(), "hi", Some("other"))
                .await
                .unwrap();
            assert!(r.read("args.txt").contains("xothery"));
        }

        #[tokio::test]
        async fn tts_stdout_mode_and_verbatim_text() {
            let r = Rig::new();
            let cfg = r.cfg(
                "cat > stdin.txt\ncp \"$ZP_TEXT_FILE\" textfile.txt\ncat clip.wav\n",
                &[],
            );
            let text = "$(rm -rf /)\n'\"`; echo pwned #\nline3 \u{e9}\u{4e2d} {out} {{x}}";
            let s = synthesize(&cfg, r.dir.path(), text, None).await.unwrap();
            assert_eq!(s.samples48.len(), 48_000);
            assert_eq!(r.read("stdin.txt"), format!("{text}\n"));
            assert_eq!(r.read("textfile.txt"), format!("{text}\n"));
        }

        #[tokio::test]
        async fn tts_text_never_in_argv() {
            let r = Rig::new();
            let cfg = r.cfg(
                "printf '%s\\n' \"$@\" > args.txt\ncat clip.wav\n",
                &["{text_file}"],
            );
            synthesize(&cfg, r.dir.path(), "SECRET-TEXT", None)
                .await
                .unwrap();
            assert!(!r.read("args.txt").contains("SECRET-TEXT"));
        }

        #[tokio::test]
        async fn nonzero_exit_and_stderr_tail() {
            let r = Rig::new();
            let cfg = r.cfg("echo oops >&2\nexit 3\n", &[]);
            match synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err() {
                SpeechError::Failed {
                    exit_code,
                    stderr_tail,
                    ..
                } => {
                    assert_eq!(exit_code, Some(3));
                    assert_eq!(stderr_tail.trim(), "oops");
                }
                e => panic!("{e:?}"),
            }
            let api: ApiError = synthesize(&cfg, r.dir.path(), "x", None)
                .await
                .unwrap_err()
                .into();
            assert_eq!(api.kind, ErrorKind::Internal);
            // Huge stderr: only the last 64 KiB survive.
            let cfg = r.cfg(
                "i=0; while [ $i -lt 4000 ]; do echo 0123456789012345678901234567890123456789012345678901234567890123456789; i=$((i+1)); done >&2\necho THE-END >&2\nexit 1\n",
                &[],
            );
            match transcribe(&cfg, r.dir.path(), &[0; 160], None)
                .await
                .unwrap_err()
            {
                SpeechError::Failed { stderr_tail, .. } => {
                    assert!(stderr_tail.len() <= STDERR_TAIL_BYTES);
                    assert!(stderr_tail.len() > STDERR_TAIL_BYTES - 200);
                    assert!(stderr_tail.trim_end().ends_with("THE-END"));
                }
                e => panic!("{e:?}"),
            }
        }

        #[tokio::test]
        async fn spawn_failure_and_bad_audio() {
            let r = Rig::new();
            let cfg = SpeechConfig {
                tts_command: vec!["/nonexistent/zp-tts".into()],
                ..Default::default()
            };
            assert!(is_failed(
                &synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err()
            ));
            let cfg = r.cfg("echo not-a-wav\n", &[]);
            assert!(is_failed(
                &synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err()
            ));
            let cfg = r.cfg("exit 0\n", &["{out}"]); // {out} referenced but never written
            assert!(is_failed(
                &synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err()
            ));
        }

        #[tokio::test]
        async fn timeout_kills_process_group() {
            let r = Rig::new();
            let mut cfg = r.cfg("sleep 30 &\necho $! > child.pid\nwait\n", &[]);
            cfg.tts_timeout_ms = 1500;
            let t = Instant::now();
            let e = synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err();
            assert!(
                matches!(
                    e,
                    SpeechError::Timeout {
                        what: "tts",
                        ms: 1500
                    }
                ),
                "{e:?}"
            );
            assert!(t.elapsed() < Duration::from_secs(10));
            let api: ApiError = e.into();
            assert_eq!(api.kind, ErrorKind::Timeout);
            let pid: i32 = r.read("child.pid").trim().parse().unwrap();
            let mut dead = false;
            for _ in 0..50 {
                if pid_dead(pid) {
                    dead = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(dead, "background child {pid} survived the timeout");
        }

        #[tokio::test]
        async fn dropped_job_kills_grandchildren() {
            let r = Rig::new();
            let cfg = r.cfg("sleep 60 &\necho $! > child.pid\nwait\n", &[]);
            let dir = r.dir.path().to_path_buf();
            let fut = synthesize(&cfg, &dir, "x", None);
            // Drop the future mid-run (client disconnect).
            let _ = tokio::time::timeout(Duration::from_millis(1500), fut).await;
            let pid: i32 = r.read("child.pid").trim().parse().unwrap();
            let mut dead = false;
            for _ in 0..50 {
                if pid_dead(pid) {
                    dead = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(dead, "grandchild {pid} survived a dropped job");
        }

        #[tokio::test]
        async fn slot_wait_counts_against_timeout() {
            let r = Rig::new();
            let mut cfg = r.cfg("sleep 30\n", &[]);
            cfg.tts_timeout_ms = 1200;
            let t = Instant::now();
            let (a, b, c) = tokio::join!(
                synthesize(&cfg, r.dir.path(), "x", None),
                synthesize(&cfg, r.dir.path(), "x", None),
                synthesize(&cfg, r.dir.path(), "x", None),
            );
            for e in [a, b, c] {
                assert!(matches!(
                    e.unwrap_err(),
                    SpeechError::Timeout { ms: 1200, .. }
                ));
            }
            assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        }

        #[tokio::test]
        async fn output_file_symlink_to_zero_is_capped() {
            let r = Rig::new();
            let p = r.p("z");
            std::os::unix::fs::symlink("/dev/zero", &p).unwrap();
            let e = read_file_capped(&p, 1000, "tts").unwrap_err();
            assert!(is_failed(&e));
        }

        #[tokio::test]
        async fn output_caps() {
            let r = Rig::new();
            let cfg = r.cfg("head -c 70000000 /dev/zero\n", &[]);
            match synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err() {
                SpeechError::Failed { message, .. } => {
                    assert!(message.contains("exceeds"), "{message}")
                }
                e => panic!("{e:?}"),
            }
            let cfg = r.cfg("head -c 2000000 /dev/zero | tr '\\0' a\n", &[]);
            match transcribe(&cfg, r.dir.path(), &[0; 160], None)
                .await
                .unwrap_err()
            {
                SpeechError::Failed { message, .. } => {
                    assert!(message.contains("exceeds"), "{message}")
                }
                e => panic!("{e:?}"),
            }
            // Output file over the cap.
            let cfg = r.cfg(
                "head -c 1100000 /dev/zero | tr '\\0' a > \"$ZP_OUT_TXT\"\n",
                &["{out_txt}"],
            );
            assert!(is_failed(
                &transcribe(&cfg, r.dir.path(), &[0; 160], None)
                    .await
                    .unwrap_err()
            ));
            // A clip over 120 s is rejected by its header, before decoding.
            let long = encode_wav_i16(&vec![0i16; 8000 * 121], 8000);
            fs::write(r.p("long.wav"), long).unwrap();
            let cfg = r.cfg("cat long.wav\n", &[]);
            match synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err() {
                SpeechError::Failed { message, .. } => {
                    assert!(message.contains("120"), "{message}")
                }
                e => panic!("{e:?}"),
            }
        }

        #[tokio::test]
        async fn stt_stdout_and_file_modes() {
            let r = Rig::new();
            let samples: Vec<i16> = (0..1600).map(|i| (i % 100) as i16 * 10).collect();
            // stdout mode
            let cfg = r.cfg(
                "cp \"$1\" got.wav\nprintf '  hello world \\n\\n'\n",
                &["{in}", "{lang}"],
            );
            let t = transcribe(&cfg, r.dir.path(), &samples, Some("de"))
                .await
                .unwrap();
            assert_eq!(t.text, "hello world");
            let mut rd = hound::WavReader::open(r.p("got.wav")).unwrap();
            assert_eq!(
                (
                    rd.spec().sample_rate,
                    rd.spec().channels,
                    rd.spec().bits_per_sample
                ),
                (16000, 1, 16)
            );
            let back: Vec<i16> = rd.samples::<i16>().map(Result::unwrap).collect();
            assert_eq!(back, samples);
            // lang default from config, env var set
            let mut cfg = r.cfg(
                "echo \"$ZP_LANG\" > lang.txt\necho \"$ZP_IN\" > in.txt\n",
                &["-l", "{lang}"],
            );
            cfg.stt_language = "fr".into();
            transcribe(&cfg, r.dir.path(), &samples, None)
                .await
                .unwrap();
            assert_eq!(r.read("lang.txt").trim(), "fr");
            assert!(r.read("in.txt").trim().ends_with("/in.wav"));
            // {out_txt} file mode ignores stdout
            let cfg = r.cfg(
                "echo ignored\nprintf 'from file\\n' > \"$2\"\n",
                &["{in}", "{out_txt}"],
            );
            assert_eq!(
                transcribe(&cfg, r.dir.path(), &samples, None)
                    .await
                    .unwrap()
                    .text,
                "from file"
            );
            // {out_base}: the tool appends .txt
            let cfg = r.cfg("printf 'based\\n' > \"$1.txt\"\n", &["{out_base}"]);
            assert_eq!(
                transcribe(&cfg, r.dir.path(), &samples, None)
                    .await
                    .unwrap()
                    .text,
                "based"
            );
        }

        #[tokio::test]
        async fn not_configured_and_invalid_tokens() {
            let r = Rig::new();
            let empty = SpeechConfig::default();
            assert_eq!(
                synthesize(&empty, r.dir.path(), "x", None)
                    .await
                    .unwrap_err(),
                SpeechError::NotConfigured("tts")
            );
            assert_eq!(
                transcribe(&empty, r.dir.path(), &[0; 16], None)
                    .await
                    .unwrap_err(),
                SpeechError::NotConfigured("stt")
            );
            let cfg = r.cfg("cat clip.wav\n", &[]);
            for bad in ["-rm", "a b", "a;b", "x/y", "$(id)", &"a".repeat(65)] {
                let e = synthesize(&cfg, r.dir.path(), "x", Some(bad))
                    .await
                    .unwrap_err();
                assert!(matches!(e, SpeechError::InvalidArgument(_)), "{bad}: {e:?}");
                let e = transcribe(&cfg, r.dir.path(), &[0; 16], Some(bad))
                    .await
                    .unwrap_err();
                assert!(matches!(e, SpeechError::InvalidArgument(_)), "{bad}: {e:?}");
            }
            let api: ApiError = synthesize(&cfg, r.dir.path(), "x", Some("-x"))
                .await
                .unwrap_err()
                .into();
            assert_eq!(api.kind, ErrorKind::InvalidParams);
            assert!(
                synthesize(&cfg, r.dir.path(), "x", Some("en_US-lessac.v2:1+x"))
                    .await
                    .is_ok()
            );
        }

        #[tokio::test]
        async fn unknown_placeholder_is_an_error_not_a_shell() {
            let r = Rig::new();
            let cfg = r.cfg("cat clip.wav\n", &["{bogus}"]);
            assert!(matches!(
                synthesize(&cfg, r.dir.path(), "x", None).await.unwrap_err(),
                SpeechError::Internal(_)
            ));
        }
    }
}
