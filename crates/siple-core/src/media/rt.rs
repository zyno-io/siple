//! Real-time scheduling for the media thread, per OS.
//!
//! macOS / Windows use `audio_thread_priority` (Mach time-constraint policy /
//! MMCSS). Linux asks the kernel for `SCHED_RR` directly: the crate's rtkit
//! path would hard-link `libdbus-1`, which headless images do not have.

/// Opaque guard; pass back to [`demote`] when the thread finishes.
pub(crate) struct RtGuard(Inner);

#[cfg(any(target_os = "macos", windows))]
type Inner = audio_thread_priority::RtPriorityHandle;
#[cfg(target_os = "linux")]
type Inner = LinuxRt;
#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
type Inner = ();

/// Promote the calling thread. Logs and returns `None` when unavailable.
pub(crate) fn promote(buffer_frames: u32, rate: u32) -> Option<RtGuard> {
    #[cfg(any(target_os = "macos", windows))]
    {
        match audio_thread_priority::promote_current_thread_to_real_time(buffer_frames, rate) {
            Ok(h) => Some(RtGuard(h)),
            Err(e) => {
                tracing::warn!("media thread: real-time priority unavailable: {e}");
                None
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = (buffer_frames, rate);
        linux_promote().map(RtGuard)
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        let _ = (buffer_frames, rate);
        None
    }
}

/// Undo [`promote`] (best effort).
pub(crate) fn demote(guard: RtGuard) {
    #[cfg(any(target_os = "macos", windows))]
    {
        let _ = audio_thread_priority::demote_current_thread_from_real_time(guard.0);
    }
    #[cfg(target_os = "linux")]
    {
        linux_demote(guard.0);
    }
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    {
        let _ = guard;
    }
}

/// Scheduling state to restore on demote.
#[cfg(target_os = "linux")]
pub(crate) struct LinuxRt {
    policy: libc::c_int,
    param: libc::sched_param,
}

/// `SCHED_RR` priority requested (low end of the RT range; the thread only
/// needs to beat normal tasks).
#[cfg(target_os = "linux")]
const RT_PRIORITY: libc::c_int = 10;

#[cfg(target_os = "linux")]
fn linux_promote() -> Option<LinuxRt> {
    // SAFETY: plain libc calls on the current thread with valid out-pointers.
    unsafe {
        let me = libc::pthread_self();
        let mut policy: libc::c_int = 0;
        let mut param: libc::sched_param = std::mem::zeroed();
        if libc::pthread_getschedparam(me, &mut policy, &mut param) != 0 {
            tracing::info!(
                "media thread: cannot read scheduling parameters; running at normal priority"
            );
            return None;
        }
        let prev = LinuxRt { policy, param };
        let mut rt: libc::sched_param = std::mem::zeroed();
        rt.sched_priority = RT_PRIORITY;
        let rc = libc::pthread_setschedparam(me, libc::SCHED_RR, &rt);
        if rc != 0 {
            tracing::info!(
                "media thread: real-time priority unavailable ({}); running at normal priority \
                 (grant an rtprio limit or CAP_SYS_NICE to enable)",
                std::io::Error::from_raw_os_error(rc)
            );
            return None;
        }
        Some(prev)
    }
}

#[cfg(target_os = "linux")]
fn linux_demote(prev: LinuxRt) {
    // SAFETY: as above.
    unsafe {
        let _ = libc::pthread_setschedparam(libc::pthread_self(), prev.policy, &prev.param);
    }
}
