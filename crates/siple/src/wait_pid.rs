//! `--wait-pid <pid>`: wait for another process to exit before starting.
//!
//! A restarted instance (an update relaunch, a "restart now" of an agent) is started by the old
//! one while it is still shutting down; waiting for the old pid first means the instance lock and
//! the control socket are free when the new process looks for them. **The flag is a permanent
//! contract of the command line: later versions rely on an older binary understanding it. Never
//! remove or rename it.**

use std::time::Duration;

/// Longest wait of `--wait-pid`.
pub const WAIT_LIMIT: Duration = Duration::from_secs(30);

/// How often a unix process is looked at.
#[cfg(unix)]
const POLL: Duration = Duration::from_millis(50);

/// Waits until process `pid` has exited, at most `timeout`. `true`: it has exited (or never
/// existed, or is this process, or is not a valid pid). `false`: still running after `timeout`;
/// on every platform at least `timeout` has then passed by [`std::time::Instant`] (unless the
/// Windows wait itself fails, which also returns `false`).
pub fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    if pid == 0 || pid == std::process::id() {
        return true;
    }
    wait_impl(pid, timeout)
}

#[cfg(unix)]
fn wait_impl(pid: u32, timeout: Duration) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // SAFETY: signal 0 only checks that the process exists.
        let rc = unsafe { libc::kill(pid, 0) };
        // EPERM: it exists but is not ours; still running.
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
}

#[cfg(windows)]
fn wait_impl(pid: u32, timeout: Duration) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // A wait's timeout runs on the system timer tick (about 15.6 ms) and can expire up to a tick
    // before `timeout` has passed by the monotonic clock (`Instant`), so the rest is waited again
    // until the deadline has really passed, like the unix poll.
    let deadline = std::time::Instant::now() + timeout;
    // SAFETY: plain Win32 calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if h.is_null() {
            // Gone (or not ours to wait for).
            return true;
        }
        let done = loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            // Rounded up, so a sub-millisecond rest is one short wait rather than a busy loop;
            // below `INFINITE` (u32::MAX).
            let ms = u32::try_from(left.as_nanos().div_ceil(1_000_000))
                .unwrap_or(u32::MAX)
                .min(u32::MAX - 1);
            match WaitForSingleObject(h, ms) {
                WAIT_OBJECT_0 => break true,
                WAIT_TIMEOUT if std::time::Instant::now() < deadline => {}
                // Timed out at the deadline (or the wait failed: treated as still running).
                _ => break false,
            }
        };
        CloseHandle(h);
        done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};
    use std::time::Instant;

    /// A child that lives for about `secs` seconds.
    fn sleeper(secs: u32) -> Child {
        #[cfg(unix)]
        let mut c = Command::new("sleep");
        #[cfg(unix)]
        c.arg(secs.to_string());
        #[cfg(windows)]
        let mut c = Command::new("ping");
        #[cfg(windows)]
        c.args(["-n", &(secs + 1).to_string(), "127.0.0.1"]);
        c.stdout(std::process::Stdio::null()).spawn().unwrap()
    }

    #[test]
    fn returns_once_the_process_has_exited() {
        let mut child = sleeper(1);
        let pid = child.id();
        // Reap the child (a zombie would still count as existing).
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        let t = Instant::now();
        assert!(wait_for_exit(pid, Duration::from_secs(20)));
        assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
        assert!(
            t.elapsed() >= Duration::from_millis(300),
            "{:?}",
            t.elapsed()
        );
        reaper.join().unwrap();
    }

    #[test]
    fn times_out_while_the_process_runs() {
        let mut child = sleeper(10);
        let t = Instant::now();
        assert!(!wait_for_exit(child.id(), Duration::from_millis(300)));
        // The full timeout by `Instant` on every platform (on Windows the wait is repeated when
        // the timer tick ends it early).
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(300), "{waited:?}");
        assert!(waited < Duration::from_secs(5), "{waited:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_process_that_is_gone_or_invalid_returns_at_once() {
        let mut child = sleeper(0);
        let pid = child.id();
        child.wait().unwrap();
        let t = Instant::now();
        assert!(wait_for_exit(pid, Duration::from_secs(20)));
        assert!(wait_for_exit(0, Duration::from_secs(20)));
        assert!(wait_for_exit(std::process::id(), Duration::from_secs(20)));
        assert!(t.elapsed() < Duration::from_secs(5));
    }
}
