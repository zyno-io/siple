//! Logging: reloadable filter, stderr, daily rolling file, and an in-memory ring for `logs.tail`.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::{DynFilterFn, EnvFilter};
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, Registry, reload};

use crate::config::LOG_FILE;
use crate::redact::{RedactMakeWriter, Redactor};

/// File name prefix of the application log (`siple.<date>.log`).
pub const LOG_PREFIX: &str = "siple";
/// Number of formatted lines kept in memory.
pub const RING_CAPACITY: usize = 5000;

pub use siple_config::log_level::{DEFAULT_FILTER, effective_filter, validate_log_level};

/// Failure to set up or reconfigure logging.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// The filter string is not valid `EnvFilter` syntax.
    #[error("invalid log filter {filter:?}: {message}")]
    InvalidFilter {
        /// The rejected filter.
        filter: String,
        /// Parser message.
        message: String,
    },
    /// The log directory or file could not be prepared.
    #[error("cannot prepare log directory {}: {source}", path.display())]
    Io {
        /// Directory involved.
        path: PathBuf,
        /// Underlying error.
        source: io::Error,
    },
    /// The filter could not be swapped (subscriber gone).
    #[error("cannot reload log filter: {0}")]
    Reload(String),
    /// A global subscriber is already installed.
    #[error("a global tracing subscriber is already installed")]
    AlreadyInitialized,
}

/// Startup options for [`init`] / [`build`].
#[derive(Debug, Clone)]
pub struct LoggingOptions {
    /// Initial `EnvFilter` string (see [`effective_filter`]).
    pub filter: String,
    /// Directory for `siple.<date>.log`.
    pub log_dir: PathBuf,
    /// Keep this many daily files (pruned continuously by the appender, plus a startup sweep of
    /// legacy `siple.log.<date>` files); 0 keeps everything.
    pub retention_days: u32,
    /// Whether the stderr layer starts enabled.
    pub stderr: bool,
}

/// Fixed-capacity buffer of recent formatted log lines.
#[derive(Debug)]
pub struct RingBuffer {
    lines: Mutex<VecDeque<String>>,
    capacity: usize,
}

impl RingBuffer {
    /// Creates an empty buffer holding at most `capacity` lines.
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Appends a line, evicting the oldest when full.
    pub fn push(&self, line: String) {
        let mut q = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if q.len() == self.capacity {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// The last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let q = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        q.iter().skip(q.len().saturating_sub(n)).cloned().collect()
    }
}

#[derive(Clone)]
struct RingMakeWriter(Arc<RingBuffer>);

struct RingLineWriter {
    ring: Arc<RingBuffer>,
    buf: Vec<u8>,
}

impl Write for RingLineWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RingLineWriter {
    fn drop(&mut self) {
        for line in String::from_utf8_lossy(&self.buf).lines() {
            self.ring.push(line.to_string());
        }
    }
}

impl<'a> MakeWriter<'a> for RingMakeWriter {
    type Writer = RingLineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        RingLineWriter {
            ring: Arc::clone(&self.0),
            buf: Vec::new(),
        }
    }
}

/// Runtime control over the installed logging pipeline. Keep it alive for the life of the
/// process: dropping it flushes and stops the background file writer.
pub struct LoggingHandle {
    filter: reload::Handle<EnvFilter, Registry>,
    current_filter: Mutex<String>,
    stderr_enabled: Arc<AtomicBool>,
    ring: Arc<RingBuffer>,
    log_dir: PathBuf,
    redactor: Arc<Redactor>,
    _file_guard: WorkerGuard,
}

impl LoggingHandle {
    /// Replaces the active filter at runtime. Invalid input leaves the old filter in place.
    pub fn set_filter(&self, filter: &str) -> Result<(), LoggingError> {
        let new = EnvFilter::try_new(filter).map_err(|e| LoggingError::InvalidFilter {
            filter: filter.to_string(),
            message: e.to_string(),
        })?;
        self.filter
            .reload(new)
            .map_err(|e| LoggingError::Reload(e.to_string()))?;
        *self
            .current_filter
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = filter.to_string();
        Ok(())
    }

    /// The filter string currently in effect.
    pub fn filter(&self) -> String {
        self.current_filter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Enables or disables the stderr layer.
    pub fn set_stderr(&self, enabled: bool) {
        self.stderr_enabled.store(enabled, Ordering::Relaxed);
    }

    /// Whether the stderr layer is enabled.
    pub fn stderr_enabled(&self) -> bool {
        self.stderr_enabled.load(Ordering::Relaxed)
    }

    /// The last `n` formatted log lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<String> {
        self.ring.tail(n)
    }

    /// Directory the log file is written to.
    pub fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    /// The redaction policy applied to every line of the stderr, file and `logs.tail` outputs
    /// (`[logging] redact_credentials` / `redact_srtp_keys`, set by the application on every
    /// config change). Share it with other writers, such as the SIP trace.
    pub fn redactor(&self) -> &Arc<Redactor> {
        &self.redactor
    }
}

impl std::fmt::Debug for LoggingHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoggingHandle")
            .field("filter", &self.filter())
            .field("log_dir", &self.log_dir)
            .finish_non_exhaustive()
    }
}

/// Builds the logging pipeline without installing it, returning the control handle and the
/// subscriber. Runs retention cleanup. Useful in tests (`tracing::subscriber::with_default`).
pub fn build(
    opts: &LoggingOptions,
) -> Result<
    (
        LoggingHandle,
        impl tracing::Subscriber + Send + Sync + 'static,
    ),
    LoggingError,
> {
    let env_filter = EnvFilter::try_new(&opts.filter).map_err(|e| LoggingError::InvalidFilter {
        filter: opts.filter.clone(),
        message: e.to_string(),
    })?;
    let io_err = |source| LoggingError::Io {
        path: opts.log_dir.clone(),
        source,
    };
    fs::create_dir_all(&opts.log_dir).map_err(io_err)?;
    if opts.retention_days > 0 {
        cleanup_old_logs(
            &opts.log_dir,
            Duration::from_secs(u64::from(opts.retention_days) * 86_400),
        )
        .map_err(io_err)?;
    }

    let (filter_layer, filter_handle) = reload::Layer::new(env_filter);
    let stderr_enabled = Arc::new(AtomicBool::new(opts.stderr));
    let toggle = Arc::clone(&stderr_enabled);
    let redactor = Redactor::new();
    let stderr_layer = fmt::layer()
        .with_writer(RedactMakeWriter::new(io::stderr, Arc::clone(&redactor)))
        .with_ansi(ansi_allowed(
            io::IsTerminal::is_terminal(&io::stderr()),
            std::env::var_os("NO_COLOR").as_deref(),
        ))
        .with_filter(DynFilterFn::new(move |_, _| toggle.load(Ordering::Relaxed)));

    let appender = rolling_appender(&opts.log_dir, LOG_PREFIX, opts.retention_days)?;
    let (file_writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(4096)
        .lossy(true)
        .finish(appender);
    let file_layer = fmt::layer()
        .with_writer(RedactMakeWriter::new(file_writer, Arc::clone(&redactor)))
        .with_ansi(false);

    let ring = Arc::new(RingBuffer::new(RING_CAPACITY));
    let ring_layer = fmt::layer()
        .with_writer(RedactMakeWriter::new(
            RingMakeWriter(Arc::clone(&ring)),
            Arc::clone(&redactor),
        ))
        .with_ansi(false);

    let subscriber = Registry::default()
        .with(filter_layer)
        .with(stderr_layer)
        .with(file_layer)
        .with(ring_layer);
    let handle = LoggingHandle {
        filter: filter_handle,
        current_filter: Mutex::new(opts.filter.clone()),
        stderr_enabled,
        ring,
        log_dir: opts.log_dir.clone(),
        redactor,
        _file_guard: guard,
    };
    Ok((handle, subscriber))
}

/// Colors only on a terminal, and not when `NO_COLOR` is set to a non-empty value.
fn ansi_allowed(is_terminal: bool, no_color: Option<&std::ffi::OsStr>) -> bool {
    is_terminal && no_color.is_none_or(|v| v.is_empty())
}

/// Builds the pipeline and installs it as the global subscriber.
pub fn init(opts: &LoggingOptions) -> Result<LoggingHandle, LoggingError> {
    let (handle, subscriber) = build(opts)?;
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| LoggingError::AlreadyInitialized)?;
    Ok(handle)
}

/// Daily-rolling file appender writing `<dir>/<prefix>.<YYYY-MM-DD>.log`, keeping at most
/// `retention_days` files (0 = unlimited) and pruning as it rolls. Creates `dir`. Never panics:
/// an unwritable directory is returned as [`LoggingError::Io`]. Reused for per-account SIP
/// traces (directory `sip/<account_id>`, prefix `sip`).
///
/// Pruning matches files by `starts_with(prefix)` and the `log` suffix, so a prefix that is a
/// leading substring of another (`sip-a` vs `sip-a2`) would share a retention pool; give each
/// appender a prefix that no other prefix starts with, or use a separate directory.
pub fn rolling_appender(
    dir: &Path,
    prefix: &str,
    retention_days: u32,
) -> Result<RollingFileAppender, LoggingError> {
    let io_err = |source| LoggingError::Io {
        path: dir.to_path_buf(),
        source,
    };
    fs::create_dir_all(dir).map_err(io_err)?;
    let mut builder = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log");
    if retention_days > 0 {
        builder = builder.max_log_files(retention_days as usize);
    }
    builder
        .build(dir)
        .map_err(|e| io_err(io::Error::other(e.to_string())))
}

/// Deletes legacy `siple.log.*` files (the pre-builder naming) in `dir` whose modification time is older than `max_age`.
/// Returns the number of files removed.
pub fn cleanup_old_logs(dir: &Path, max_age: Duration) -> io::Result<usize> {
    let prefix = format!("{LOG_FILE}.");
    let cutoff = SystemTime::now().checked_sub(max_age);
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let meta = entry.metadata()?;
        let old = matches!((meta.modified(), cutoff), (Ok(m), Some(c)) if m < c);
        if meta.is_file() && old && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn ansi_rules() {
        use std::ffi::OsStr;
        assert!(ansi_allowed(true, None));
        assert!(ansi_allowed(true, Some(OsStr::new(""))));
        assert!(!ansi_allowed(true, Some(OsStr::new("1"))));
        assert!(!ansi_allowed(false, None));
    }

    fn opts(dir: &Path) -> LoggingOptions {
        LoggingOptions {
            filter: DEFAULT_FILTER.into(),
            log_dir: dir.to_path_buf(),
            retention_days: 14,
            stderr: false,
        }
    }

    #[test]
    fn filter_expansion() {
        assert_eq!(effective_filter("info"), "info,rsipstack=warn");
        assert_eq!(effective_filter(" debug "), "debug,rsipstack=warn");
        assert_eq!(effective_filter(""), DEFAULT_FILTER);
        assert_eq!(
            effective_filter("warn,rsipstack=trace"),
            "warn,rsipstack=trace"
        );
    }

    #[test]
    fn ring_keeps_last_lines() {
        let r = RingBuffer::new(3);
        for i in 0..5 {
            r.push(format!("l{i}"));
        }
        assert_eq!(r.tail(10), ["l2", "l3", "l4"]);
        assert_eq!(r.tail(2), ["l3", "l4"]);
        assert!(r.tail(0).is_empty());
    }

    #[test]
    fn ring_and_reloadable_filter() {
        let dir = tempfile::tempdir().unwrap();
        let (handle, subscriber) = build(&opts(dir.path())).unwrap();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(account = "a1", "hello ring");
            tracing::debug!("hidden debug");
            handle.set_filter("debug").unwrap();
            assert_eq!(handle.filter(), "debug");
            tracing::debug!("visible debug");
            assert!(handle.set_filter("=[").is_err());
            assert_eq!(handle.filter(), "debug");
        });
        let lines = handle.tail(10);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("hello ring") && lines[0].contains("account"));
        assert!(lines[1].contains("visible debug"));
        assert!(!handle.stderr_enabled());
        handle.set_stderr(true);
        assert!(handle.stderr_enabled());
    }

    #[test]
    fn file_sink_writes_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("a").join("logs");
        let (handle, subscriber) = build(&opts(&logs)).unwrap();
        tracing::subscriber::with_default(subscriber, || tracing::info!("to the file"));
        drop(handle); // flush the non-blocking writer
        let text: String = fs::read_dir(&logs)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_name().to_string_lossy().starts_with("siple."))
            .map(|e| fs::read_to_string(e.path()).unwrap())
            .collect();
        assert!(text.contains("to the file"), "{text:?}");
    }

    #[test]
    fn retention_removes_only_old_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, age_days: u64| {
            let f = File::create(dir.path().join(name)).unwrap();
            f.set_modified(SystemTime::now() - Duration::from_secs(age_days * 86_400))
                .unwrap();
        };
        mk("siple.log.2020-01-01", 400);
        mk("siple.log.2999-01-01", 1);
        mk("siple.log", 400);
        mk("other.log.2020-01-01", 400);
        let removed = cleanup_old_logs(dir.path(), Duration::from_secs(14 * 86_400)).unwrap();
        assert_eq!(removed, 1);
        let mut left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["other.log.2020-01-01", "siple.log", "siple.log.2999-01-01"]
        );
    }

    #[test]
    fn log_level_validation() {
        for ok in [
            "",
            "info",
            "INFO",
            "debug,rsipstack=trace",
            "siple_core::sip=debug",
            "warn, rsipstack=off",
        ] {
            assert_eq!(validate_log_level(ok), Ok(()), "{ok:?}");
        }
        for bad in ["inf", "info,", "siple_core::sip", "info,=bad["] {
            assert!(validate_log_level(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn unwritable_log_dir_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("afile");
        fs::write(&file, "x").unwrap();
        // A directory path that is below a regular file can never be created.
        let r = rolling_appender(&file.join("logs"), "siple", 14);
        assert!(matches!(r, Err(LoggingError::Io { .. })));
        assert!(matches!(
            build(&opts(&file.join("logs"))),
            Err(LoggingError::Io { .. })
        ));
    }

    #[test]
    fn rolling_appender_names_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = rolling_appender(dir.path(), "sip-acct1", 14).unwrap();
        a.write_all(b"hello\n").unwrap();
        a.flush().unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(
            names[0].starts_with("sip-acct1.") && names[0].ends_with(".log"),
            "{names:?}"
        );
    }

    #[test]
    fn invalid_initial_filter_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut o = opts(dir.path());
        o.filter = "=[".into();
        assert!(matches!(build(&o), Err(LoggingError::InvalidFilter { .. })));
    }
}
