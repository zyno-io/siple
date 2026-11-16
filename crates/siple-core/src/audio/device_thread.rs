//! The audio-device thread: owns every `cpal::Stream`, rebuilds them on
//! selection change / stream error / hot-plug, and exposes lock-free-ish
//! ring access to the (single) media thread.
//!
//! Hand-off design: each of capture / playback / ringer endpoints lives in an
//! `ArcSwapOption`. A rebuild creates fresh rings and swaps in a new endpoint
//! (bumping `generation`). The media thread's accessors do one wait-free
//! `load()` and then lock a `Mutex` that only the media thread ever takes, so
//! they never block on the device thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, StreamConfig, SupportedBufferSize, SupportedStreamConfig};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

#[cfg(target_os = "linux")]
use super::devices::{AlsaLog, default_device};
use super::devices::{DeviceInfo, Direction, Enumerated, enumerate, resolve_device};
use super::rings::{
    CaptureReader, CaptureWriter, Notifier, PlaybackWriter, RingCounters, capture_ring,
    playback_ring,
};

/// How often the device list is re-polled for hot-plug / default changes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Delay before rebuilding after a stream error callback.
const ERROR_REBUILD_DELAY: Duration = Duration::from_millis(500);
/// How long `list()` waits for the device thread.
const LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// Selection value that disables a slot entirely (no device is opened).
pub const NO_DEVICE: &str = "<none>";

/// Configured device names; `""` means system default (ringer `""` = same as speaker).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSelection {
    /// Microphone.
    pub input: String,
    /// Speaker.
    pub output: String,
    /// Ringer (separate stream only if it resolves to a different device than the speaker).
    pub ringer: String,
}

/// A device currently open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ActiveDevice {
    /// Device name.
    pub name: String,
    /// Device (ring) sample rate in Hz.
    pub sample_rate: u32,
    /// Device channel count.
    pub channels: u16,
    /// The configured name was missing, so the system default is in use.
    pub fallback_to_default: bool,
    /// Native cpal sample format, e.g. `"F32"`.
    pub sample_format: String,
    /// Fixed callback buffer size in frames, if one could be requested.
    pub buffer_frames: Option<u32>,
}

/// Snapshot of the audio layer.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AudioStatus {
    /// Open microphone, if any.
    pub input: Option<ActiveDevice>,
    /// Open speaker, if any.
    pub output: Option<ActiveDevice>,
    /// Ringer device; equals `output` when `ringer_shared`.
    pub ringer: Option<ActiveDevice>,
    /// Ringer audio goes through the speaker stream.
    pub ringer_shared: bool,
    /// Capture samples dropped (ring full), cumulative.
    pub input_overflows: u64,
    /// Speaker callbacks that ran dry after playback started, cumulative.
    pub output_underflows: u64,
    /// Same for the separate ringer stream.
    pub ringer_underflows: u64,
    /// Incremented on every stream (re)build; consumers reset resamplers when it changes.
    pub generation: u64,
    /// Most recent stream/build error.
    pub last_error: Option<String>,
}

/// Enumerated devices.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceList {
    /// Input devices.
    pub inputs: Vec<DeviceInfo>,
    /// Output devices.
    pub outputs: Vec<DeviceInfo>,
}

struct CaptureEnd {
    rate: u32,
    reader: Mutex<CaptureReader>,
}

struct PlaybackEnd {
    rate: u32,
    writer: Mutex<PlaybackWriter>,
}

#[derive(Default)]
struct StatusInner {
    input: Option<ActiveDevice>,
    output: Option<ActiveDevice>,
    ringer: Option<ActiveDevice>,
    ringer_shared: bool,
    last_error: Option<String>,
}

struct Shared {
    notifier: Arc<Notifier>,
    capture: ArcSwapOption<CaptureEnd>,
    playback: ArcSwapOption<PlaybackEnd>,
    ringer: ArcSwapOption<PlaybackEnd>,
    in_counters: Arc<RingCounters>,
    out_counters: Arc<RingCounters>,
    ringer_counters: Arc<RingCounters>,
    generation: AtomicU64,
    status: Mutex<StatusInner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Input = 0,
    Output = 1,
    Ringer = 2,
}

enum Cmd {
    SetDevices(DeviceSelection),
    Rescan,
    List(Sender<DeviceList>),
    StreamError { slot: Slot, token: u64, msg: String },
    Shutdown,
}

/// Entry point.
pub struct AudioDevices;

impl AudioDevices {
    /// Spawn the device thread and open the selected devices. Never fails:
    /// with no audio hardware the status simply shows no devices.
    pub fn start(selection: DeviceSelection) -> AudioHandle {
        let shared = Arc::new(Shared {
            notifier: Notifier::new(),
            capture: ArcSwapOption::empty(),
            playback: ArcSwapOption::empty(),
            ringer: ArcSwapOption::empty(),
            in_counters: Arc::default(),
            out_counters: Arc::default(),
            ringer_counters: Arc::default(),
            generation: AtomicU64::new(0),
            status: Mutex::new(StatusInner::default()),
        });
        let (tx, rx) = mpsc::channel();
        let join = {
            let shared = shared.clone();
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("siple-audio-dev".into())
                .spawn(move || run(selection, shared, tx, rx))
                .ok()
        };
        if join.is_none() {
            warn!("could not spawn audio device thread; audio disabled");
        }
        AudioHandle {
            inner: Arc::new(Inner {
                shared,
                tx,
                join: Mutex::new(join),
            }),
        }
    }
}

/// Feeds mono samples into the capture ring of a headless handle
/// ([`AudioDevices::start_injected`]), as the device callback would.
pub struct CaptureInjector {
    writer: Mutex<CaptureWriter>,
}

impl CaptureInjector {
    /// Pushes mono samples (`-1.0..=1.0`, at the handle's capture rate) and wakes the media
    /// thread. Samples that do not fit the ring are dropped and counted, like a real overflow.
    pub fn inject(&self, samples: &[f32]) {
        lock(&self.writer).write_interleaved(samples);
    }
}

impl AudioDevices {
    /// A handle with no hardware: a real capture ring of `rate` Hz that the caller fills through
    /// the returned [`CaptureInjector`] (no playback device: speaker samples are discarded).
    /// Lets tests and headless runs drive the media thread's capture path.
    pub fn start_injected(rate: u32) -> (AudioHandle, CaptureInjector) {
        let notifier = Notifier::new();
        let in_counters = Arc::<RingCounters>::default();
        let (writer, reader) = capture_ring(rate, 1, in_counters.clone(), notifier.clone());
        let shared = Arc::new(Shared {
            notifier,
            capture: ArcSwapOption::new(Some(Arc::new(CaptureEnd {
                rate,
                reader: Mutex::new(reader),
            }))),
            playback: ArcSwapOption::empty(),
            ringer: ArcSwapOption::empty(),
            in_counters,
            out_counters: Arc::default(),
            ringer_counters: Arc::default(),
            generation: AtomicU64::new(0),
            status: Mutex::new(StatusInner::default()),
        });
        // No device thread: commands are dropped (the receiver is gone).
        let (tx, _rx) = mpsc::channel();
        (
            AudioHandle {
                inner: Arc::new(Inner {
                    shared,
                    tx,
                    join: Mutex::new(None),
                }),
            },
            CaptureInjector {
                writer: Mutex::new(writer),
            },
        )
    }
}

struct Inner {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(j) = lock(&self.join).take() {
            let _ = j.join();
        }
    }
}

/// Cloneable handle: device control plus ring access for the media thread.
/// The device thread stops when the last clone is dropped.
///
/// The ring accessors (`capture_read`, `playback_write`, ...) are meant for a
/// single consumer thread; they never block on the device thread.
#[derive(Clone)]
pub struct AudioHandle {
    inner: Arc<Inner>,
}

impl AudioHandle {
    fn shared(&self) -> &Shared {
        &self.inner.shared
    }

    // ---- control ---------------------------------------------------------

    /// Change the selected devices (asynchronous; streams are rebuilt only for
    /// slots whose resolved device actually changes).
    pub fn set_devices(&self, selection: DeviceSelection) {
        let _ = self.inner.tx.send(Cmd::SetDevices(selection));
    }

    /// Force a device rescan now (e.g. when a settings UI opens).
    pub fn rescan(&self) {
        let _ = self.inner.tx.send(Cmd::Rescan);
    }

    /// Enumerate devices (executed on the device thread; empty on timeout).
    pub fn list(&self) -> DeviceList {
        let (tx, rx) = mpsc::channel();
        if self.inner.tx.send(Cmd::List(tx)).is_err() {
            return DeviceList::default();
        }
        rx.recv_timeout(LIST_TIMEOUT).unwrap_or_default()
    }

    /// Current status snapshot.
    pub fn status(&self) -> AudioStatus {
        let s = self.shared();
        let st = lock(&s.status);
        AudioStatus {
            input: st.input.clone(),
            output: st.output.clone(),
            ringer: st.ringer.clone(),
            ringer_shared: st.ringer_shared,
            input_overflows: s.in_counters.overflows.load(Ordering::Relaxed),
            output_underflows: s.out_counters.underflows.load(Ordering::Relaxed),
            ringer_underflows: s.ringer_counters.underflows.load(Ordering::Relaxed),
            generation: s.generation.load(Ordering::Acquire),
            last_error: st.last_error.clone(),
        }
    }

    // ---- media-thread API -----------------------------------------------

    /// Wake-up source for the media thread: callbacks call `notify()` after
    /// every capture write and playback read.
    pub fn notifier(&self) -> &Arc<Notifier> {
        &self.shared().notifier
    }

    /// Bumped on every stream rebuild.
    pub fn generation(&self) -> u64 {
        self.shared().generation.load(Ordering::Acquire)
    }

    /// Pop mono capture samples (device rate). Returns 0 with no input device.
    pub fn capture_read(&self, out: &mut [f32]) -> usize {
        match self.shared().capture.load().as_ref() {
            Some(e) => lock(&e.reader).read(out),
            None => 0,
        }
    }

    /// Samples waiting in the capture ring.
    pub fn capture_available(&self) -> usize {
        match self.shared().capture.load().as_ref() {
            Some(e) => lock(&e.reader).available(),
            None => 0,
        }
    }

    /// Capture device rate, `None` without an input device.
    pub fn capture_rate(&self) -> Option<u32> {
        self.shared().capture.load().as_ref().map(|e| e.rate)
    }

    /// Queue mono samples (device rate) for the speaker. Returns the number
    /// accepted (ring may be full). With no output device the samples are
    /// discarded and `samples.len()` is returned.
    pub fn playback_write(&self, samples: &[f32]) -> usize {
        match self.shared().playback.load().as_ref() {
            Some(e) => lock(&e.writer).write(samples),
            None => samples.len(),
        }
    }

    /// Samples queued in the speaker ring (0 with no device).
    pub fn playback_fill(&self) -> usize {
        match self.shared().playback.load().as_ref() {
            Some(e) => lock(&e.writer).fill(),
            None => 0,
        }
    }

    /// Speaker ring capacity in samples (0 with no device).
    pub fn playback_capacity(&self) -> usize {
        match self.shared().playback.load().as_ref() {
            Some(e) => lock(&e.writer).capacity(),
            None => 0,
        }
    }

    /// Speaker device rate, `None` without an output device.
    pub fn playback_rate(&self) -> Option<u32> {
        self.shared().playback.load().as_ref().map(|e| e.rate)
    }

    /// Queue ringer samples. Goes to the dedicated ringer stream when one is
    /// open, otherwise to the speaker ring (the caller must then mix, since
    /// the same ring is shared with `playback_write`).
    pub fn ringer_write(&self, samples: &[f32]) -> usize {
        match self.shared().ringer.load().as_ref() {
            Some(e) => lock(&e.writer).write(samples),
            None => self.playback_write(samples),
        }
    }

    /// Ringer ring fill (falls back to the speaker ring).
    pub fn ringer_fill(&self) -> usize {
        match self.shared().ringer.load().as_ref() {
            Some(e) => lock(&e.writer).fill(),
            None => self.playback_fill(),
        }
    }

    /// Rate the ringer samples must be at (falls back to the speaker rate).
    pub fn ringer_rate(&self) -> Option<u32> {
        match self.shared().ringer.load().as_ref() {
            Some(e) => Some(e.rate),
            None => self.playback_rate(),
        }
    }

    /// True when ringer audio is routed through a dedicated stream.
    pub fn ringer_is_separate(&self) -> bool {
        self.shared().ringer.load().is_some()
    }
}

// ---------------------------------------------------------------------------
// Device thread
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SlotState {
    stream: Option<cpal::Stream>,
    active: Option<ActiveDevice>,
    token: u64,
    /// Last failure message already logged for the wanted device (avoid log spam on retry).
    logged_failure: Option<String>,
}

struct Thread {
    host: cpal::Host,
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    selection: DeviceSelection,
    slots: [SlotState; 3],
    next_token: u64,
}

fn run(selection: DeviceSelection, shared: Arc<Shared>, tx: Sender<Cmd>, rx: Receiver<Cmd>) {
    // alsa-lib prints enumeration/open failures to stderr; the handler is per thread, so it is
    // installed here, before cpal first touches ALSA.
    #[cfg(target_os = "linux")]
    let mut alsa_log = AlsaLog::install();
    let mut t = Thread {
        host: cpal::default_host(),
        shared: shared.clone(),
        tx,
        selection,
        slots: Default::default(),
        next_token: 1,
    };
    t.reconcile([false; 3]);
    #[cfg(target_os = "linux")]
    alsa_log.drain();
    let mut next_poll = Instant::now() + POLL_INTERVAL;
    let mut pending: Option<(Instant, [bool; 3])> = None;
    loop {
        let mut deadline = next_poll;
        if let Some((at, _)) = pending {
            deadline = deadline.min(at);
        }
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Cmd::SetDevices(sel)) => {
                t.selection = sel;
                t.reconcile([false; 3]);
            }
            Ok(Cmd::Rescan) => t.reconcile([false; 3]),
            Ok(Cmd::List(reply)) => {
                let _ = reply.send(DeviceList {
                    inputs: enumerate(&t.host, Direction::Input)
                        .into_iter()
                        .map(|e| e.info)
                        .collect(),
                    outputs: enumerate(&t.host, Direction::Output)
                        .into_iter()
                        .map(|e| e.info)
                        .collect(),
                });
            }
            Ok(Cmd::StreamError { slot, token, msg }) => {
                if t.slots[slot as usize].token == token {
                    warn!(?slot, "audio stream error: {msg}");
                    lock(&t.shared.status).last_error = Some(msg);
                    let mut mask = pending.map(|p| p.1).unwrap_or([false; 3]);
                    mask[slot as usize] = true;
                    pending = Some((Instant::now() + ERROR_REBUILD_DELAY, mask));
                }
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        if let Some((at, mask)) = pending
            && now >= at
        {
            pending = None;
            t.reconcile(mask);
            next_poll = now + POLL_INTERVAL;
        }
        if now >= next_poll {
            t.reconcile([false; 3]);
            next_poll = Instant::now() + POLL_INTERVAL;
        }
        #[cfg(target_os = "linux")]
        alsa_log.drain();
    }
    // Tear down: drop streams first, then clear endpoints.
    for s in &mut t.slots {
        s.stream = None;
    }
    shared.capture.store(None);
    shared.playback.store(None);
    shared.ringer.store(None);
}

/// What a slot should be open on.
struct Want {
    name: String,
    fallback: bool,
    dev: Enumerated,
}

fn pick(host: &cpal::Host, dir: Direction, requested: &str) -> Option<Want> {
    if requested == NO_DEVICE {
        return None;
    }
    // Linux: the default device needs no enumeration (which opens every ALSA hint PCM, every
    // poll). If it is unusable, fall through to the full list.
    #[cfg(target_os = "linux")]
    if requested.is_empty()
        && let Some(dev) = default_device(host, dir)
    {
        return Some(Want {
            name: dev.info.name.clone(),
            fallback: false,
            dev,
        });
    }
    let mut list = enumerate(host, dir);
    let infos: Vec<DeviceInfo> = list.iter().map(|e| e.info.clone()).collect();
    let res = resolve_device(requested, &infos);
    let idx = res.index?;
    let dev = list.swap_remove(idx);
    Some(Want {
        name: dev.info.name.clone(),
        fallback: res.fallback_to_default,
        dev,
    })
}

impl Thread {
    fn reconcile(&mut self, force: [bool; 3]) {
        let want_in = pick(&self.host, Direction::Input, &self.selection.input);
        let want_out = pick(&self.host, Direction::Output, &self.selection.output);
        let ringer_req = if self.selection.ringer.is_empty() {
            self.selection.output.clone()
        } else {
            self.selection.ringer.clone()
        };
        let want_ringer = pick(&self.host, Direction::Output, &ringer_req);
        let ringer_shared = match (&want_out, &want_ringer) {
            (Some(o), Some(r)) => o.name == r.name,
            (_, None) => true,
            (None, Some(_)) => false,
        };

        let mut rebuilt = false;
        let mut failed = false;
        rebuilt |= self.sync(Slot::Input, want_in, force[0], &mut failed);
        rebuilt |= self.sync(Slot::Output, want_out, force[1], &mut failed);
        let want_ringer = if ringer_shared { None } else { want_ringer };
        rebuilt |= self.sync(Slot::Ringer, want_ringer, force[2], &mut failed);

        let mut st = lock(&self.shared.status);
        st.input = self.slots[0].active.clone();
        st.output = self.slots[1].active.clone();
        st.ringer_shared = ringer_shared;
        st.ringer = if ringer_shared {
            self.slots[1].active.clone()
        } else {
            self.slots[2].active.clone()
        };
        if rebuilt && !failed {
            st.last_error = None;
        }
    }

    /// Bring one slot in line with `want`. Returns true if streams changed.
    fn sync(&mut self, slot: Slot, want: Option<Want>, force: bool, failed: &mut bool) -> bool {
        let idx = slot as usize;
        let want_id = want.as_ref().map(|w| (w.name.clone(), w.fallback));
        let have_id = self.slots[idx]
            .active
            .as_ref()
            .map(|a| (a.name.clone(), a.fallback_to_default));
        if !force && want_id == have_id {
            return false;
        }
        // Tear down the old stream first (releases the device), then open anew.
        let had = self.slots[idx].stream.is_some();
        self.slots[idx].stream = None;
        self.slots[idx].active = None;
        self.next_token += 1;
        let token = self.next_token;
        self.slots[idx].token = token;
        match slot {
            Slot::Input => self.shared.capture.store(None),
            Slot::Output => self.shared.playback.store(None),
            Slot::Ringer => self.shared.ringer.store(None),
        }
        let Some(want) = want else {
            if had {
                info!(?slot, "audio device closed (none available)");
                self.shared.generation.fetch_add(1, Ordering::AcqRel);
            }
            self.slots[idx].logged_failure = None;
            return had;
        };
        if want.fallback {
            warn!(
                ?slot,
                "configured audio device not found; using default '{}'", want.name
            );
        }
        match self.open(slot, token, &want) {
            Ok((stream, active)) => {
                info!(
                    ?slot,
                    "audio device open: '{}' {} Hz x{} {}",
                    active.name,
                    active.sample_rate,
                    active.channels,
                    active.sample_format
                );
                self.slots[idx].stream = Some(stream);
                self.slots[idx].active = Some(active);
                self.slots[idx].logged_failure = None;
                self.shared.generation.fetch_add(1, Ordering::AcqRel);
                true
            }
            Err(msg) => {
                let full = format!("{slot:?} '{}': {msg}", want.name);
                if self.slots[idx].logged_failure.as_deref() != Some(&full) {
                    warn!("audio device open failed: {full}");
                    self.slots[idx].logged_failure = Some(full.clone());
                } else {
                    debug!("audio device open still failing: {full}");
                }
                match slot {
                    Slot::Input => self.shared.capture.store(None),
                    Slot::Output => self.shared.playback.store(None),
                    Slot::Ringer => self.shared.ringer.store(None),
                }
                lock(&self.shared.status).last_error = Some(full);
                *failed = true;
                had
            }
        }
    }

    fn open(
        &self,
        slot: Slot,
        token: u64,
        want: &Want,
    ) -> Result<(cpal::Stream, ActiveDevice), String> {
        let dev = &want.dev.device;
        let supported = match slot {
            Slot::Input => dev.default_input_config(),
            _ => dev.default_output_config(),
        }
        .map_err(|e| format!("no default config: {e}"))?;
        let rate = supported.sample_rate();
        let channels = supported.channels();
        let format = supported.sample_format();

        let tx = self.tx.clone();
        let err_cb = move || {
            let tx = tx.clone();
            move |e: cpal::Error| match e.kind() {
                cpal::ErrorKind::DeviceChanged
                | cpal::ErrorKind::Xrun
                | cpal::ErrorKind::RealtimeDenied => {}
                _ => {
                    let _ = tx.send(Cmd::StreamError {
                        slot,
                        token,
                        msg: e.to_string(),
                    });
                }
            }
        };

        let mut last_err = String::from("unknown error");
        let mut buffers = vec![fixed_buffer(&supported)];
        if buffers[0] != BufferSize::Default {
            buffers.push(BufferSize::Default);
        }
        for bs in buffers {
            let config = StreamConfig {
                channels,
                sample_rate: rate,
                buffer_size: bs,
            };
            let built = match slot {
                Slot::Input => self.build_input(dev, config, format, err_cb()),
                _ => self.build_output(slot, dev, config, format, err_cb()),
            };
            match built {
                Err(BuildErr::Cpal(e)) => last_err = e,
                Err(BuildErr::Format(f)) => return Err(f),
                Ok(stream) => {
                    stream.play().map_err(|e| format!("play: {e}"))?;
                    return Ok((
                        stream,
                        ActiveDevice {
                            name: want.name.clone(),
                            sample_rate: rate,
                            channels,
                            fallback_to_default: want.fallback,
                            sample_format: format!("{format:?}"),
                            buffer_frames: match bs {
                                BufferSize::Fixed(n) => Some(n),
                                BufferSize::Default => None,
                            },
                        },
                    ));
                }
            }
        }
        Err(last_err)
    }
}

/// Why a build attempt failed.
enum BuildErr {
    Cpal(String),
    Format(String),
}

fn fixed_buffer(cfg: &SupportedStreamConfig) -> BufferSize {
    match cfg.buffer_size() {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed((cfg.sample_rate() / 100).clamp(*min, *max))
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    }
}

macro_rules! dispatch_format {
    ($format:expr, $f:ident) => {
        match $format {
            SampleFormat::F32 => $f!(f32),
            SampleFormat::I16 => $f!(i16),
            SampleFormat::U16 => $f!(u16),
            SampleFormat::I32 => $f!(i32),
            other => {
                return Err(BuildErr::Format(format!(
                    "unsupported sample format {other:?}"
                )))
            }
        }
    };
}

impl Thread {
    /// Build an input stream and publish its capture endpoint.
    fn build_input(
        &self,
        dev: &cpal::Device,
        config: StreamConfig,
        format: SampleFormat,
        err_cb: impl FnMut(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream, BuildErr> {
        let (writer, reader) = capture_ring(
            config.sample_rate,
            config.channels,
            self.shared.in_counters.clone(),
            self.shared.notifier.clone(),
        );
        let mut writer = writer;
        macro_rules! go {
            ($t:ty) => {
                dev.build_input_stream::<$t, _, _>(
                    config,
                    move |d: &[$t], _| writer.write_interleaved(d),
                    err_cb,
                    None,
                )
            };
        }
        let stream = dispatch_format!(format, go).map_err(|e| BuildErr::Cpal(e.to_string()))?;
        self.shared.capture.store(Some(Arc::new(CaptureEnd {
            rate: config.sample_rate,
            reader: Mutex::new(reader),
        })));
        Ok(stream)
    }

    fn build_output(
        &self,
        slot: Slot,
        dev: &cpal::Device,
        config: StreamConfig,
        format: SampleFormat,
        err_cb: impl FnMut(cpal::Error) + Send + 'static,
    ) -> Result<cpal::Stream, BuildErr> {
        let counters = if slot == Slot::Ringer {
            self.shared.ringer_counters.clone()
        } else {
            self.shared.out_counters.clone()
        };
        let (writer, reader) = playback_ring(
            config.sample_rate,
            config.channels,
            Some(counters),
            self.shared.notifier.clone(),
        );
        let mut reader = reader;
        macro_rules! go {
            ($t:ty) => {
                dev.build_output_stream::<$t, _, _>(
                    config,
                    move |d: &mut [$t], _| reader.fill_interleaved(d),
                    err_cb,
                    None,
                )
            };
        }
        let stream = dispatch_format!(format, go).map_err(|e| BuildErr::Cpal(e.to_string()))?;
        let end = Arc::new(PlaybackEnd {
            rate: config.sample_rate,
            writer: Mutex::new(writer),
        });
        if slot == Slot::Ringer {
            self.shared.ringer.store(Some(end));
        } else {
            self.shared.playback.store(Some(end));
        }
        Ok(stream)
    }
}
