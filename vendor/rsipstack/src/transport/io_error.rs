//! siple: what the transport receive / accept loops do after a socket error.
//!
//! Upstream logged every error and retried at once. A socket that fails every call (macOS
//! "defunct" sockets: `recvfrom` keeps returning `ENOTCONN`; a closed descriptor: `EBADF`) then
//! pinned a core and flooded the log forever, and nobody learned that the socket was dead.
//!
//! [`ErrorRun`] counts the errors since the last success and decides per error:
//! - [`ErrorClass::Fatal`]: the socket itself is broken. Stop the loop and report the
//!   connection as closed (`TransportEvent::Closed`), so the owner can replace it.
//! - [`ErrorClass::Benign`]: tied to one datagram or one peer (an ICMP port unreachable reported
//!   as `WSAECONNRESET` on Windows, a connection aborted before `accept`, `EINTR`). Retry; never
//!   fatal.
//! - [`ErrorClass::Resource`]: the system is short of descriptors / buffers / memory. Retry;
//!   never fatal (it recovers when the shortage ends).
//! - [`ErrorClass::Unknown`]: retry, but a run of [`FATAL_AFTER`] in a row counts as a dead
//!   socket.
//!
//! The first [`BACKOFF_AFTER`] errors of a run are retried at once (a burst of queued ICMP
//! errors must not delay the next datagram); after that the loop waits
//! [`BACKOFF_STEP`] · 2ⁿ, at most [`MAX_BACKOFF`], before each retry.

use core::time::Duration;

/// Errors in a row that are retried without waiting.
pub const BACKOFF_AFTER: u32 = 8;
/// First wait once the run is longer than [`BACKOFF_AFTER`]; doubles per error.
pub const BACKOFF_STEP: Duration = Duration::from_millis(10);
/// Longest wait between two retries.
pub const MAX_BACKOFF: Duration = Duration::from_secs(1);
/// [`ErrorClass::Unknown`] errors in a row after which the socket counts as dead.
pub const FATAL_AFTER: u32 = 64;

/// How a receive / accept error is treated; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Benign,
    Resource,
    Unknown,
    Fatal,
}

/// What the loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Retry after this long (zero: at once).
    Retry(Duration),
    /// The socket is dead: stop and report it.
    Stop,
}

/// Errors since the last successful receive / accept.
#[derive(Debug, Default)]
pub struct ErrorRun {
    consecutive: u32,
}

impl ErrorRun {
    /// A receive / accept succeeded; returns the length of the run that just ended.
    pub fn success(&mut self) -> u32 {
        core::mem::take(&mut self.consecutive)
    }

    /// Errors in the current run (including the one just passed to [`Self::error`]).
    pub fn consecutive(&self) -> u32 {
        self.consecutive
    }

    /// Records an error of `class` and says what to do.
    pub fn error(&mut self, class: ErrorClass) -> Next {
        self.consecutive = self.consecutive.saturating_add(1);
        match class {
            ErrorClass::Fatal => Next::Stop,
            ErrorClass::Unknown if self.consecutive >= FATAL_AFTER => Next::Stop,
            _ => Next::Retry(backoff(self.consecutive)),
        }
    }

    /// Whether the error just recorded should be logged: the first of a run, and the one at
    /// which the backoff starts (later ones are at debug level).
    pub fn should_log(&self) -> bool {
        self.consecutive == 1 || self.consecutive == BACKOFF_AFTER + 1
    }
}

/// The wait before retrying the `n`-th error of a run.
pub fn backoff(n: u32) -> Duration {
    if n <= BACKOFF_AFTER {
        return Duration::ZERO;
    }
    let shift = (n - BACKOFF_AFTER - 1).min(16);
    BACKOFF_STEP.saturating_mul(1 << shift).min(MAX_BACKOFF)
}

/// Classifies an error of the transport layer (receive errors arrive as `Error::IoError`).
pub fn classify(e: &crate::Error) -> ErrorClass {
    #[cfg(feature = "std")]
    if let crate::Error::IoError(io) = e {
        return classify_io(io);
    }
    let _ = e;
    ErrorClass::Unknown
}

/// Classifies an I/O error of `recv_from` / `accept`.
#[cfg(feature = "std")]
pub fn classify_io(e: &std::io::Error) -> ErrorClass {
    use std::io::ErrorKind as K;
    if let Some(code) = e.raw_os_error() {
        if raw::BAD_SOCKET.contains(&code) {
            return ErrorClass::Fatal;
        }
        if raw::RESOURCE.contains(&code) {
            return ErrorClass::Resource;
        }
        if raw::BENIGN.contains(&code) {
            return ErrorClass::Benign;
        }
    }
    match e.kind() {
        K::NotConnected | K::BrokenPipe => ErrorClass::Fatal,
        K::OutOfMemory => ErrorClass::Resource,
        K::ConnectionReset
        | K::ConnectionRefused
        | K::ConnectionAborted
        | K::HostUnreachable
        | K::NetworkUnreachable
        | K::Interrupted
        | K::WouldBlock
        | K::TimedOut => ErrorClass::Benign,
        _ => ErrorClass::Unknown,
    }
}

/// Raw OS error codes `std::io::ErrorKind` does not name.
#[cfg(feature = "std")]
mod raw {
    #[cfg(target_os = "linux")]
    pub const BAD_SOCKET: &[i32] = &[9 /* EBADF */, 88 /* ENOTSOCK */];
    #[cfg(target_os = "linux")]
    pub const RESOURCE: &[i32] = &[
        23,  /* ENFILE */
        24,  /* EMFILE */
        105, /* ENOBUFS */
    ];
    #[cfg(target_os = "linux")]
    pub const BENIGN: &[i32] = &[90 /* EMSGSIZE */];

    #[cfg(all(unix, not(target_os = "linux")))]
    pub const BAD_SOCKET: &[i32] = &[9 /* EBADF */, 38 /* ENOTSOCK */];
    #[cfg(all(unix, not(target_os = "linux")))]
    pub const RESOURCE: &[i32] = &[
        23, /* ENFILE */
        24, /* EMFILE */
        55, /* ENOBUFS */
    ];
    #[cfg(all(unix, not(target_os = "linux")))]
    pub const BENIGN: &[i32] = &[40 /* EMSGSIZE */];

    #[cfg(windows)]
    pub const BAD_SOCKET: &[i32] = &[10009 /* WSAEBADF */, 10038 /* WSAENOTSOCK */];
    #[cfg(windows)]
    pub const RESOURCE: &[i32] = &[10024 /* WSAEMFILE */, 10055 /* WSAENOBUFS */];
    #[cfg(windows)]
    pub const BENIGN: &[i32] = &[10040 /* WSAEMSGSIZE */, 10052 /* WSAENETRESET */];

    #[cfg(not(any(unix, windows)))]
    pub const BAD_SOCKET: &[i32] = &[];
    #[cfg(not(any(unix, windows)))]
    pub const RESOURCE: &[i32] = &[];
    #[cfg(not(any(unix, windows)))]
    pub const BENIGN: &[i32] = &[];
}

/// Waits `delay` unless `cancel` fires first; returns `false` when it was cancelled.
pub(crate) async fn wait_or_cancelled(
    delay: Duration,
    cancel: Option<&crate::platform::CancellationToken>,
) -> bool {
    if delay.is_zero() {
        return true;
    }
    match cancel {
        Some(token) => {
            let mut cancelled = core::pin::pin!(token.cancelled());
            let mut slept = core::pin::pin!(crate::platform::sleep(delay));
            !matches!(
                crate::platform::select::select2(&mut cancelled, &mut slept).await,
                crate::platform::select::Either::A(())
            )
        }
        None => {
            crate::platform::sleep(delay).await;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_after_a_burst_and_is_capped() {
        for n in 1..=BACKOFF_AFTER {
            assert_eq!(backoff(n), Duration::ZERO, "n = {n}");
        }
        assert_eq!(backoff(BACKOFF_AFTER + 1), BACKOFF_STEP);
        assert_eq!(backoff(BACKOFF_AFTER + 2), BACKOFF_STEP * 2);
        assert_eq!(backoff(BACKOFF_AFTER + 3), BACKOFF_STEP * 4);
        assert_eq!(backoff(BACKOFF_AFTER + 20), MAX_BACKOFF);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
    }

    #[test]
    fn fatal_stops_at_once_and_unknown_after_a_long_run() {
        let mut run = ErrorRun::default();
        assert_eq!(run.error(ErrorClass::Fatal), Next::Stop);

        let mut run = ErrorRun::default();
        for n in 1..FATAL_AFTER {
            assert!(
                matches!(run.error(ErrorClass::Unknown), Next::Retry(_)),
                "n = {n}"
            );
        }
        assert_eq!(run.error(ErrorClass::Unknown), Next::Stop);
    }

    #[test]
    fn benign_and_resource_errors_never_stop_the_loop_but_back_off() {
        for class in [ErrorClass::Benign, ErrorClass::Resource] {
            let mut run = ErrorRun::default();
            let mut waited = Duration::ZERO;
            for _ in 0..10 * FATAL_AFTER {
                match run.error(class) {
                    Next::Retry(d) => waited += d,
                    Next::Stop => panic!("{class:?} stopped the loop"),
                }
            }
            assert_eq!(run.error(class), Next::Retry(MAX_BACKOFF));
            assert!(waited > Duration::from_secs(60), "{class:?}: {waited:?}");
        }
    }

    #[test]
    fn a_success_ends_the_run() {
        let mut run = ErrorRun::default();
        for _ in 0..FATAL_AFTER - 1 {
            let _ = run.error(ErrorClass::Unknown);
        }
        assert_eq!(run.success(), FATAL_AFTER - 1);
        assert_eq!(run.consecutive(), 0);
        assert_eq!(run.error(ErrorClass::Unknown), Next::Retry(Duration::ZERO));
        assert!(run.should_log());
    }

    #[test]
    fn only_the_first_error_and_the_start_of_the_backoff_are_logged() {
        let mut run = ErrorRun::default();
        let mut logged = alloc::vec::Vec::new();
        for _ in 0..40 {
            let _ = run.error(ErrorClass::Benign);
            if run.should_log() {
                logged.push(run.consecutive());
            }
        }
        assert_eq!(logged, [1, BACKOFF_AFTER + 1]);
    }

    #[cfg(feature = "std")]
    #[test]
    fn io_errors_are_classified() {
        use std::io::{Error, ErrorKind};
        let kind = |k| classify_io(&Error::from(k));
        assert_eq!(kind(ErrorKind::NotConnected), ErrorClass::Fatal);
        assert_eq!(kind(ErrorKind::BrokenPipe), ErrorClass::Fatal);
        assert_eq!(kind(ErrorKind::ConnectionReset), ErrorClass::Benign);
        assert_eq!(kind(ErrorKind::ConnectionRefused), ErrorClass::Benign);
        assert_eq!(kind(ErrorKind::ConnectionAborted), ErrorClass::Benign);
        assert_eq!(kind(ErrorKind::Interrupted), ErrorClass::Benign);
        assert_eq!(kind(ErrorKind::WouldBlock), ErrorClass::Benign);
        assert_eq!(kind(ErrorKind::OutOfMemory), ErrorClass::Resource);
        assert_eq!(kind(ErrorKind::Other), ErrorClass::Unknown);
        let os = |c| classify_io(&Error::from_raw_os_error(c));
        for &c in raw::BAD_SOCKET {
            assert_eq!(os(c), ErrorClass::Fatal, "{c}");
        }
        for &c in raw::RESOURCE {
            assert_eq!(os(c), ErrorClass::Resource, "{c}");
        }
        for &c in raw::BENIGN {
            assert_eq!(os(c), ErrorClass::Benign, "{c}");
        }
        assert_eq!(
            classify(&crate::Error::IoError(Error::from(ErrorKind::NotConnected))),
            ErrorClass::Fatal
        );
        assert_eq!(
            classify(&crate::Error::Error("x".into())),
            ErrorClass::Unknown
        );
    }
}
