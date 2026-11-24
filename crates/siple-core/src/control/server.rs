//! Control-plane listeners and per-connection protocol handling.
//!
//! Security model:
//! - Unix socket: mode 0600, so only the owning user (and root) can connect. The *parent
//!   directory* should also not be writable by others (the default is the user's config dir,
//!   or `$TMPDIR`, which is per-user 0700 on macOS). There is a sub-millisecond window between
//!   `bind` and `chmod` in which the socket carries umask-derived permissions; it is only
//!   exploitable if the parent directory is reachable by other users.
//! - Windows named pipe: DACL granting full access to the current user SID only (SYSTEM and
//!   administrators are *not* granted access), first instance created with
//!   `FILE_FLAG_FIRST_PIPE_INSTANCE` (squatting protection), remote clients rejected.
//! - TCP: loopback only, token mandatory in `hello`.
//! - Non-JSON input closes the connection at once (defeats cross-protocol HTTP attacks); the
//!   first message must be `hello`; lines are capped at 1 MiB.

use std::net::SocketAddr;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::methods::{BIG_PERMIT, ControlContext, parse_params};
use super::protocol::{
    ApiError, ErrorKind, PROTOCOL_VERSION, Request, encode_error, encode_event, encode_result,
};
use crate::events::EventFilter;
use crate::instance::ControlEndpoints;

/// Longest accepted request line before `hello` (and the size above which an authenticated
/// line needs a [`ControlContext::big_requests`] permit).
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Longest accepted request line after `hello` (inline audio for `call.play` and friends).
pub const MAX_AUTH_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Time a new unix-socket / named-pipe connection has to complete `hello`.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// Time a new TCP connection has to complete `hello` (any local user can connect).
pub const HELLO_TIMEOUT_TCP: Duration = Duration::from_secs(3);
/// Simultaneous connections on the unix-socket / named-pipe listener.
pub const MAX_LOCAL_CONNECTIONS: usize = 64;
/// Simultaneous connections on the TCP listener.
pub const MAX_TCP_CONNECTIONS: usize = 16;
/// Simultaneous not-yet-authenticated connections on a local listener.
pub const MAX_LOCAL_UNAUTH: usize = 16;
/// Simultaneous not-yet-authenticated connections on the TCP listener.
pub const MAX_TCP_UNAUTH: usize = 4;
/// In-flight requests per connection; beyond this the server stops reading.
pub const MAX_INFLIGHT_PER_CONN: usize = 32;
/// A queued response that cannot be delivered within this long means the
/// client is not reading: the connection is closed.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest a single write to the client may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the writer may flush after the connection ends.
const FLUSH_GRACE: Duration = Duration::from_secs(2);
/// `sun_path` is 104 bytes on macOS (108 on Linux), including the NUL.
pub const MAX_UNIX_PATH: usize = 100;

/// Failure to start a listener.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// OS-level failure.
    #[error("{what}: {source}")]
    Io {
        /// What was being done.
        what: String,
        /// Underlying error.
        source: std::io::Error,
    },
    /// Another process is already serving this endpoint.
    #[error("control endpoint {0} is already in use by another process")]
    InUse(String),
    /// Configuration is unusable (e.g. non-loopback TCP address).
    #[error("{0}")]
    Config(String),
}

fn io_err(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> ServerError {
    let what = what.into();
    move |source| ServerError::Io { what, source }
}

/// Connection caps for one listener.
#[derive(Clone)]
struct Limits {
    conns: Arc<Semaphore>,
    /// Connections that have not completed `hello` yet.
    unauth: Arc<Semaphore>,
}

impl Limits {
    fn new(conns: usize, unauth: usize) -> Self {
        Self {
            conns: Arc::new(Semaphore::new(conns)),
            unauth: Arc::new(Semaphore::new(unauth)),
        }
    }

    fn local() -> Self {
        Self::new(MAX_LOCAL_CONNECTIONS, MAX_LOCAL_UNAUTH)
    }

    fn tcp() -> Self {
        Self::new(MAX_TCP_CONNECTIONS, MAX_TCP_UNAUTH)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    /// Unix socket / named pipe: protected by filesystem permissions or DACL.
    Local,
    /// Loopback TCP: token required.
    Tcp,
}

/// How long the connections of a listener removed by [`ControlServer::reconfigure`] may stay
/// open after it stopped accepting. This lets the reply to the `config.reload` that caused the
/// change (possibly sent over that very endpoint) reach its client.
pub const RETIRE_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Unix,
    Pipe,
    Tcp,
}

impl Kind {
    const ALL: [Kind; 3] = [Kind::Unix, Kind::Pipe, Kind::Tcp];

    fn of(self, e: &ControlEndpoints) -> Option<&String> {
        match self {
            Kind::Unix => e.unix_socket.as_ref(),
            Kind::Pipe => e.named_pipe.as_ref(),
            Kind::Tcp => e.tcp.as_ref(),
        }
    }

    fn set(self, e: &mut ControlEndpoints, v: Option<String>) {
        match self {
            Kind::Unix => e.unix_socket = v,
            Kind::Pipe => e.named_pipe = v,
            Kind::Tcp => e.tcp = v,
        }
    }
}

/// A bound endpoint that does not accept yet.
enum Bound {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, PathBuf),
    #[cfg(windows)]
    Pipe(winpipe::PipeListener, String),
    Tcp(tokio::net::TcpListener, SocketAddr),
}

impl Bound {
    fn kind(&self) -> Kind {
        match self {
            #[cfg(unix)]
            Bound::Unix(..) => Kind::Unix,
            #[cfg(windows)]
            Bound::Pipe(..) => Kind::Pipe,
            Bound::Tcp(..) => Kind::Tcp,
        }
    }

    /// The address to record in `runtime.json`.
    fn actual(&self) -> String {
        match self {
            #[cfg(unix)]
            Bound::Unix(_, p) => p.to_string_lossy().into_owned(),
            #[cfg(windows)]
            Bound::Pipe(_, n) => n.clone(),
            Bound::Tcp(_, a) => a.to_string(),
        }
    }
}

/// Bound endpoints that have not been handed to a task yet. Dropping them (an error while
/// binding a later endpoint) releases the sockets and removes the socket files, so a failed
/// start leaves nothing behind.
#[derive(Default)]
struct BoundSet(Vec<Bound>);

impl Drop for BoundSet {
    fn drop(&mut self) {
        #[cfg(unix)]
        for b in &self.0 {
            if let Bound::Unix(_, p) = b {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Binds one endpoint. `None` when the platform does not support the kind.
async fn bind_endpoint(kind: Kind, requested: &str) -> Result<Option<Bound>, ServerError> {
    match kind {
        Kind::Unix => {
            #[cfg(unix)]
            {
                let (listener, actual) = bind_unix(requested)?;
                Ok(Some(Bound::Unix(listener, actual)))
            }
            #[cfg(not(unix))]
            {
                tracing::warn!(
                    path = requested,
                    "unix sockets are not supported on this platform; ignored"
                );
                Ok(None)
            }
        }
        Kind::Pipe => {
            #[cfg(windows)]
            {
                Ok(Some(Bound::Pipe(
                    winpipe::bind(requested)?,
                    requested.to_string(),
                )))
            }
            #[cfg(not(windows))]
            {
                tracing::warn!(
                    name = requested,
                    "named pipes are not supported on this platform; ignored"
                );
                Ok(None)
            }
        }
        Kind::Tcp => {
            let addr: SocketAddr = requested.parse().map_err(|e| {
                ServerError::Config(format!("invalid TCP listen address {requested}: {e}"))
            })?;
            if !addr.ip().is_loopback() {
                return Err(ServerError::Config(format!(
                    "TCP control listener must bind to loopback, got {addr}"
                )));
            }
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .map_err(io_err(format!("cannot bind control TCP {addr}")))?;
            let actual = listener.local_addr().map_err(io_err("local_addr"))?;
            Ok(Some(Bound::Tcp(listener, actual)))
        }
    }
}

/// Binds every kind in `kinds` that `requested` asks for. All-or-nothing.
async fn bind_all(requested: &ControlEndpoints, kinds: &[Kind]) -> Result<BoundSet, ServerError> {
    let mut set = BoundSet::default();
    for &k in kinds {
        if let Some(req) = k.of(requested)
            && let Some(b) = bind_endpoint(k, req).await?
        {
            set.0.push(b);
        }
    }
    Ok(set)
}

/// One accepting endpoint.
struct Listener {
    kind: Kind,
    /// Stops the accept loop.
    accept: CancellationToken,
    /// Parent of the tokens of this listener's connections.
    conns: CancellationToken,
    task: JoinHandle<()>,
    /// Socket file to remove when the listener goes away.
    cleanup: Option<PathBuf>,
}

impl Listener {
    fn remove_files(&self) {
        if let Some(p) = &self.cleanup {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Running listeners. Dropping the server (or [`shutdown`](Self::shutdown)) stops them, closes
/// their connections and removes the socket files.
pub struct ControlServer {
    /// What was asked for (before shortening paths / resolving ports).
    requested: ControlEndpoints,
    /// What is actually bound.
    endpoints: ControlEndpoints,
    listeners: Vec<Listener>,
    ctx: Arc<ControlContext>,
    /// Child of the token given to `start`; canceled by `shutdown` and `Drop`.
    cancel: CancellationToken,
}

impl ControlServer {
    /// Binds every endpoint in `requested`, then starts accepting. Returns once all are bound,
    /// so errors surface synchronously; if any endpoint fails, none stays bound.
    /// [`endpoints`](Self::endpoints) holds the *actual* addresses (shortened socket path,
    /// resolved TCP port) to record in `runtime.json`.
    pub async fn start(
        requested: &ControlEndpoints,
        ctx: ControlContext,
        cancel: CancellationToken,
    ) -> Result<Self, ServerError> {
        let bound = bind_all(requested, &Kind::ALL).await?;
        let mut server = Self {
            requested: requested.clone(),
            endpoints: ControlEndpoints::default(),
            listeners: Vec::new(),
            ctx: Arc::new(ctx),
            cancel: cancel.child_token(),
        };
        server.adopt(bound);
        Ok(server)
    }

    /// Starts accepting on already-bound endpoints.
    fn adopt(&mut self, mut set: BoundSet) {
        for b in std::mem::take(&mut set.0) {
            let kind = b.kind();
            kind.set(&mut self.endpoints, Some(b.actual()));
            let accept = self.cancel.child_token();
            let conns = self.cancel.child_token();
            let ctx = self.ctx.clone();
            let (task, cleanup) = match b {
                #[cfg(unix)]
                Bound::Unix(l, path) => (
                    tokio::spawn(accept_unix(
                        l,
                        ctx,
                        Limits::local(),
                        accept.clone(),
                        conns.clone(),
                    )),
                    Some(path),
                ),
                #[cfg(windows)]
                Bound::Pipe(l, _) => (
                    l.spawn(ctx, Limits::local(), accept.clone(), conns.clone()),
                    None,
                ),
                Bound::Tcp(l, _) => (
                    tokio::spawn(accept_tcp(
                        l,
                        ctx,
                        Limits::tcp(),
                        accept.clone(),
                        conns.clone(),
                    )),
                    None,
                ),
            };
            self.listeners.push(Listener {
                kind,
                accept,
                conns,
                task,
                cleanup,
            });
        }
    }

    /// Moves the server to `requested`, touching only the endpoints whose requested address
    /// changed: unchanged listeners keep running with all their connections. New endpoints are
    /// bound first; if any cannot be, nothing changes and the error is returned. Replaced
    /// listeners stop accepting at once and their connections are closed after
    /// [`RETIRE_GRACE`]. Returns whether anything changed.
    pub async fn reconfigure(&mut self, requested: &ControlEndpoints) -> Result<bool, ServerError> {
        let changed: Vec<Kind> = Kind::ALL
            .into_iter()
            .filter(|k| k.of(&self.requested) != k.of(requested))
            .collect();
        if changed.is_empty() {
            return Ok(false);
        }
        let bound = bind_all(requested, &changed).await?;
        let (retired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.listeners)
            .into_iter()
            .partition(|l| changed.contains(&l.kind));
        self.listeners = kept;
        for k in &changed {
            k.set(&mut self.endpoints, None);
        }
        for l in retired {
            l.accept.cancel();
            let _ = tokio::time::timeout(Duration::from_secs(3), l.task).await;
            if let Some(p) = &l.cleanup {
                let _ = std::fs::remove_file(p);
            }
            let conns = l.conns;
            tokio::spawn(async move {
                tokio::time::sleep(RETIRE_GRACE).await;
                conns.cancel();
            });
        }
        self.adopt(bound);
        self.requested = requested.clone();
        Ok(true)
    }

    /// The endpoints actually bound.
    pub fn endpoints(&self) -> &ControlEndpoints {
        &self.endpoints
    }

    /// Stops accepting, closes connections, removes the unix socket file.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for l in self.listeners.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(3), l.task).await;
            if let Some(p) = &l.cleanup {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.cancel.cancel();
        for l in &self.listeners {
            l.task.abort();
            l.remove_files();
        }
    }
}

/// Unix socket path to use for `requested`: itself, or a short name derived from a hash of it
/// (in [`crate::instance::short_socket_dir`]) when it would overflow `sun_path`. If the short
/// directory cannot be used safely the long path is returned unchanged (binding then fails;
/// [`bind_unix`] reports the precise reason).
pub fn effective_unix_path(requested: &str) -> PathBuf {
    try_effective_unix_path(requested).unwrap_or_else(|_| PathBuf::from(requested))
}

fn try_effective_unix_path(requested: &str) -> std::io::Result<PathBuf> {
    if requested.len() <= MAX_UNIX_PATH {
        return Ok(PathBuf::from(requested));
    }
    crate::instance::short_socket_path(requested)
}

#[cfg(unix)]
fn bind_unix(requested: &str) -> Result<(tokio::net::UnixListener, PathBuf), ServerError> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let path = try_effective_unix_path(requested)
        .map_err(io_err("cannot choose a short control socket path"))?;
    if path.as_os_str() != Path::new(requested).as_os_str() {
        tracing::warn!(requested, actual = %path.display(),
            "control socket path too long for sun_path; using a shorter path");
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(io_err(format!("cannot create {}", parent.display())))?;
    }
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_socket() => {
            // Stale only if nobody answers.
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(ServerError::InUse(path.display().to_string()));
            }
            std::fs::remove_file(&path).map_err(io_err(format!(
                "cannot remove stale socket {}",
                path.display()
            )))?;
        }
        Ok(_) => {
            return Err(ServerError::Config(format!(
                "{} exists and is not a socket; refusing to replace it",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(format!("cannot stat {}", path.display()))(e)),
    }
    let listener = tokio::net::UnixListener::bind(&path)
        .map_err(io_err(format!("cannot bind {}", path.display())))?;
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&path);
        return Err(io_err(format!("cannot chmod {}", path.display()))(e));
    }
    Ok((listener, path))
}

#[cfg(unix)]
async fn accept_unix(
    listener: tokio::net::UnixListener,
    ctx: Arc<ControlContext>,
    limits: Limits,
    accept: CancellationToken,
    conns: CancellationToken,
) {
    loop {
        let stream = tokio::select! {
            _ = accept.cancelled() => break,
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(e) => { tracing::warn!("control accept failed: {e}"); tokio::time::sleep(Duration::from_millis(100)).await; continue; }
            },
        };
        spawn_connection(stream, Transport::Local, &ctx, &limits, &conns);
    }
}

async fn accept_tcp(
    listener: tokio::net::TcpListener,
    ctx: Arc<ControlContext>,
    limits: Limits,
    accept: CancellationToken,
    conns: CancellationToken,
) {
    loop {
        let stream = tokio::select! {
            _ = accept.cancelled() => break,
            r = listener.accept() => match r {
                Ok((s, peer)) if peer.ip().is_loopback() => s,
                Ok(_) => continue,
                Err(e) => { tracing::warn!("control accept failed: {e}"); tokio::time::sleep(Duration::from_millis(100)).await; continue; }
            },
        };
        let _ = stream.set_nodelay(true);
        spawn_connection(stream, Transport::Tcp, &ctx, &limits, &conns);
    }
}

fn spawn_connection<S>(
    stream: S,
    transport: Transport,
    ctx: &Arc<ControlContext>,
    limits: &Limits,
    cancel: &CancellationToken,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let Ok(permit) = limits.conns.clone().try_acquire_owned() else {
        tracing::warn!(
            ?transport,
            "control connection limit reached; dropping connection"
        );
        return;
    };
    let Ok(unauth) = limits.unauth.clone().try_acquire_owned() else {
        tracing::warn!(
            ?transport,
            "too many unauthenticated control connections; dropping connection"
        );
        return;
    };
    let ctx = ctx.clone();
    let cancel = cancel.clone();
    tokio::spawn(async move {
        serve_connection(stream, ctx, transport, cancel, unauth).await;
        drop(permit);
    });
}

enum Line {
    /// The line, plus the big-request permit if it outgrew [`MAX_LINE_BYTES`] (it moves into
    /// the request task, which may release it early).
    Data(Vec<u8>, Option<OwnedSemaphorePermit>),
    TooLong,
    /// No big-request permit became available in time; the line was read and discarded.
    Busy,
    Eof,
}

/// Limits for [`read_line`].
struct ReadLimits<'a> {
    max: usize,
    /// Gate for lines above [`MAX_LINE_BYTES`].
    gate: Option<&'a Arc<Semaphore>>,
    permit_wait: Duration,
    idle: Duration,
    /// Overall time a big line may take once it holds a permit (or is being discarded).
    total: Duration,
}

/// Reads one `\n`-terminated line, never buffering more than `max` bytes. With a gate, the
/// reader acquires a permit before buffering past [`MAX_LINE_BYTES`], which bounds how many
/// large lines are in memory at once. If the permit is not granted within `permit_wait` the
/// rest of the line is read and dropped ([`Line::Busy`]). While a big line is in progress
/// (permit held or discarding) a stall of `idle` is a read error, and so is exceeding `total`
/// in all (a client trickling bytes must not hold a permit forever).
async fn read_line<R: AsyncBufReadExt + Unpin>(
    r: &mut R,
    lim: ReadLimits<'_>,
) -> std::io::Result<Line> {
    let mut buf = Vec::new();
    let mut permit: Option<OwnedSemaphorePermit> = None;
    // Discard mode: bytes consumed so far (buf is empty).
    let mut discarded: Option<usize> = None;
    let mut big_deadline: Option<tokio::time::Instant> = None;
    loop {
        let chunk = if permit.is_some() || discarded.is_some() {
            let dl = *big_deadline.get_or_insert_with(|| tokio::time::Instant::now() + lim.total);
            let wait = lim
                .idle
                .min(dl.saturating_duration_since(tokio::time::Instant::now()));
            match tokio::time::timeout(wait, r.fill_buf()).await {
                Ok(c) => c?,
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "stalled or too slow while receiving a large request",
                    ));
                }
            }
        } else {
            r.fill_buf().await?
        };
        if chunk.is_empty() {
            return Ok(Line::Eof);
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        if let Some(n) = &mut discarded {
            if *n + take > lim.max {
                return Ok(Line::TooLong);
            }
            *n += take;
            r.consume(take + usize::from(done));
            if done {
                return Ok(Line::Busy);
            }
            continue;
        }
        if buf.len() + take > lim.max {
            return Ok(Line::TooLong);
        }
        if let Some(g) = lim.gate
            && permit.is_none()
            && buf.len() + take > MAX_LINE_BYTES
        {
            match tokio::time::timeout(lim.permit_wait, g.clone().acquire_owned()).await {
                Ok(Ok(p)) => permit = Some(p),
                Ok(Err(_)) => return Ok(Line::Eof),
                Err(_) => {
                    // Keep the connection usable: drop this line without buffering it.
                    discarded = Some(buf.len());
                    buf = Vec::new();
                    continue;
                }
            }
        }
        buf.extend_from_slice(&chunk[..take]);
        r.consume(take + usize::from(done));
        if done {
            return Ok(Line::Data(buf, permit));
        }
    }
}

fn id_of(req: &Request) -> Value {
    req.id.clone().unwrap_or(Value::Null)
}

fn check_hello(
    ctx: &ControlContext,
    transport: Transport,
    params: Value,
) -> Result<Value, ApiError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct P {
        token: Option<String>,
        client: Option<String>,
    }
    let p: P = parse_params(params)?;
    match (&p.token, transport) {
        (None, Transport::Tcp) => {
            return Err(ApiError::unauthorized("token required on TCP connections"));
        }
        (Some(t), _) if ctx.token.is_empty() || !ctx.token.ct_eq(t) => {
            return Err(ApiError::unauthorized("invalid token"));
        }
        _ => {}
    }
    if transport == Transport::Tcp && ctx.token.is_empty() {
        return Err(ApiError::unauthorized("TCP access is not configured"));
    }
    tracing::debug!(
        client = p.client.as_deref().unwrap_or("?"),
        ?transport,
        "control client connected"
    );
    Ok(json!({
        "server": "siple",
        "version": crate::update::full_version(),
        "protocol": PROTOCOL_VERSION,
        "current_seq": ctx.events.current_seq(),
        "instance_id": ctx.events.instance_id(),
        "started_at": ctx.events.started_at().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "build": ctx.build,
    }))
}

/// Queues `line` for the writer. `false` when the connection is gone, or when
/// the client has not consumed output for [`SEND_TIMEOUT`] (the connection is
/// then canceled). Never blocks past cancellation.
async fn send_line(tx: &mpsc::Sender<String>, conn: &CancellationToken, line: String) -> bool {
    tokio::select! {
        _ = conn.cancelled() => false,
        r = tokio::time::timeout(SEND_TIMEOUT, tx.send(line)) => match r {
            Ok(r) => r.is_ok(),
            Err(_) => {
                tracing::debug!("control client not reading; closing connection");
                conn.cancel();
                false
            }
        },
    }
}

/// Serves one connection until EOF, protocol violation or cancellation.
async fn serve_connection<S>(
    stream: S,
    ctx: Arc<ControlContext>,
    transport: Transport,
    cancel: CancellationToken,
    unauth_permit: OwnedSemaphorePermit,
) where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let (tx, mut rx) = mpsc::channel::<String>(256);
    // The writer hands its half back (for `call.stream`); the normal exit shuts it down.
    let wstop = CancellationToken::new();
    let wstop_w = wstop.clone();
    let mut writer = tokio::spawn(async move {
        let mut stopping = false;
        loop {
            let line = if stopping {
                rx.try_recv().ok()
            } else {
                tokio::select! {
                    l = rx.recv() => l,
                    _ = wstop_w.cancelled() => {
                        // Flush what is queued; later sends fail.
                        rx.close();
                        stopping = true;
                        continue;
                    }
                }
            };
            let Some(mut line) = line else { break };
            line.push('\n');
            let w = async {
                wr.write_all(line.as_bytes()).await?;
                wr.flush().await
            };
            if !matches!(tokio::time::timeout(WRITE_TIMEOUT, w).await, Ok(Ok(()))) {
                return (wr, false);
            }
        }
        (wr, true)
    });

    let conn = cancel.child_token();
    let mut unauth = Some(unauth_permit);
    let mut sub: Option<JoinHandle<()>> = None;
    let mut writer_taken = false;
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_PER_CONN));
    let hello_timeout = match transport {
        Transport::Local => HELLO_TIMEOUT,
        Transport::Tcp => HELLO_TIMEOUT_TCP,
    };
    let hello_deadline = tokio::time::Instant::now() + hello_timeout;

    loop {
        let authed = unauth.is_none();
        let read = async {
            if authed {
                read_line(
                    &mut rd,
                    ReadLimits {
                        max: MAX_AUTH_LINE_BYTES,
                        gate: Some(&ctx.big_requests),
                        permit_wait: ctx.big_permit_wait,
                        idle: ctx.big_read_idle,
                        total: ctx.big_read_total,
                    },
                )
                .await
            } else {
                match tokio::time::timeout_at(
                    hello_deadline,
                    read_line(
                        &mut rd,
                        ReadLimits {
                            max: MAX_LINE_BYTES,
                            gate: None,
                            permit_wait: ctx.big_permit_wait,
                            idle: ctx.big_read_idle,
                            total: ctx.big_read_total,
                        },
                    ),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Ok(Line::Eof),
                }
            }
        };
        let (line, big_permit) = tokio::select! {
            _ = conn.cancelled() => break,
            // The writer gave up (client not reading / gone).
            _ = tx.closed() => break,
            r = read => match r {
                Ok(Line::Data(l, p)) => (l, p),
                Ok(Line::Busy) => {
                    let e = ApiError::internal(
                        "server busy: too many large requests in progress; request discarded",
                    );
                    if !send_line(&tx, &conn, encode_error(&Value::Null, &e)).await {
                        break;
                    }
                    continue;
                }
                _ => break,
            },
        };
        let text = line.strip_suffix(b"\r").unwrap_or(&line);
        if authed && text.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Anything that is not JSON is dropped without a reply (cross-protocol defense).
        let Ok(value) = serde_json::from_slice::<Value>(text) else {
            break;
        };
        // The encoded line is no longer needed once parsed.
        drop(line);
        let req = match Request::from_value(value) {
            Ok(r) => r,
            Err(e) => {
                if !send_line(&tx, &conn, encode_error(&Value::Null, &e)).await {
                    break;
                }
                if authed {
                    continue;
                }
                break;
            }
        };

        if !authed {
            if req.method != "hello" {
                let e = ApiError::unauthorized("first message must be `hello`");
                send_line(&tx, &conn, encode_error(&id_of(&req), &e)).await;
                break;
            }
            match check_hello(&ctx, transport, req.params.clone()) {
                Ok(r) => {
                    unauth = None;
                    if !send_line(&tx, &conn, encode_result(&id_of(&req), r)).await {
                        break;
                    }
                    continue;
                }
                Err(e) => {
                    send_line(&tx, &conn, encode_error(&id_of(&req), &e)).await;
                    break;
                }
            }
        }

        match req.method.as_str() {
            "hello" => {
                let out = check_hello(&ctx, transport, req.params.clone());
                if let Some(id) = &req.id {
                    let line = match out {
                        Ok(r) => encode_result(id, r),
                        Err(e) => encode_error(id, &e),
                    };
                    if !send_line(&tx, &conn, line).await {
                        break;
                    }
                }
            }
            "events.subscribe" => {
                let out = serde_json::from_value::<EventFilter>(if req.params.is_null() {
                    json!({})
                } else {
                    req.params.clone()
                })
                .map_err(ApiError::from)
                .map(|f| ctx.events.subscribe(f));
                let line = match out {
                    Ok(mut s) => {
                        if let Some(old) = sub.take() {
                            old.abort();
                        }
                        let seq = ctx.events.current_seq();
                        let reply = encode_result(
                            &id_of(&req),
                            json!({ "subscribed": true, "current_seq": seq }),
                        );
                        if !send_line(&tx, &conn, reply).await {
                            break;
                        }
                        let txe = tx.clone();
                        let c = conn.clone();
                        sub = Some(tokio::spawn(async move {
                            loop {
                                let ev = tokio::select! {
                                    _ = c.cancelled() => break,
                                    e = s.next() => match e { Some(e) => e, None => break },
                                };
                                if !send_line(&txe, &c, encode_event(&ev)).await {
                                    break;
                                }
                            }
                        }));
                        continue;
                    }
                    Err(e) => encode_error(&id_of(&req), &e),
                };
                if !send_line(&tx, &conn, line).await {
                    break;
                }
            }
            "call.stream" => {
                // Upgrade to a binary stream. Every precondition failure leaves
                // the connection in JSON mode.
                let id = id_of(&req);
                let pre = if req.id.is_none() {
                    Some(ApiError::invalid_params("call.stream needs a request id"))
                } else if inflight.available_permits() != MAX_INFLIGHT_PER_CONN {
                    Some(ApiError::invalid_state(
                        "call.stream needs a connection with no requests in flight",
                    ))
                } else if sub.is_some() {
                    Some(ApiError::invalid_state(
                        "call.stream cannot be combined with events.subscribe on one connection",
                    ))
                } else {
                    None
                };
                let opened = match pre {
                    Some(e) => Err(e),
                    None => ctx.backend.open_stream(req.params.clone()).await,
                };
                let (result, session) = match opened {
                    Ok(x) => x,
                    Err(e) => {
                        if !send_line(&tx, &conn, encode_error(&id, &e)).await {
                            break;
                        }
                        continue;
                    }
                };
                if !send_line(&tx, &conn, encode_result(&id, result)).await {
                    // The call's stream slot is released (and `hangup_on_close` honored).
                    session.abort(abort_reason(&conn)).await;
                    break;
                }
                // Stop the writer and take the write half back. Stray `tx` clones (an aborted
                // forwarder) cannot keep it waiting.
                wstop.cancel();
                writer_taken = true;
                let wr = match tokio::time::timeout(FLUSH_GRACE, &mut writer).await {
                    Ok(Ok((wr, true))) => wr,
                    Ok(_) => {
                        session.abort(abort_reason(&conn)).await;
                        break;
                    }
                    Err(_) => {
                        writer.abort();
                        session.abort(abort_reason(&conn)).await;
                        break;
                    }
                };
                session.run(Box::new(rd), Box::new(wr), conn.clone()).await;
                conn.cancel();
                return;
            }
            "events.unsubscribe" => {
                let was = sub.take().map(|h| h.abort()).is_some();
                let reply = encode_result(&id_of(&req), json!({ "unsubscribed": was }));
                if !send_line(&tx, &conn, reply).await {
                    break;
                }
            }
            _ => {
                // Concurrent: each request runs in its own task; responses may interleave.
                // Bounded per connection: when full, stop reading (backpressure).
                let permit = tokio::select! {
                    _ = conn.cancelled() => break,
                    _ = tx.closed() => break,
                    p = inflight.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => break,
                    },
                };
                let ctx = ctx.clone();
                let tx = tx.clone();
                let c = conn.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    // The permit is released by the backend after decoding big params
                    // (`release_big_request_permit`) or, at the latest, when this task ends.
                    let out = tokio::select! {
                        _ = c.cancelled() => return,
                        r = BIG_PERMIT.scope(
                            std::cell::RefCell::new(big_permit),
                            ctx.dispatch(&req.method, req.params),
                        ) => r,
                    };
                    if let Some(id) = req.id {
                        let line = match out {
                            Ok(r) => encode_result(&id, r),
                            Err(e) => {
                                if e.kind == ErrorKind::Internal {
                                    tracing::warn!(method = %req.method, "control request failed: {e}");
                                }
                                encode_error(&id, &e)
                            }
                        };
                        send_line(&tx, &c, line).await;
                    }
                });
            }
        }
    }

    // Stop request tasks and the event forwarder, then let the writer flush what is queued
    // (bounded; a client that does not read must not keep the socket and task alive).
    conn.cancel();
    if let Some(s) = sub {
        s.abort();
    }
    drop(tx);
    if !writer_taken {
        match tokio::time::timeout(FLUSH_GRACE, &mut writer).await {
            Ok(Ok((mut wr, _))) => {
                let _ = tokio::time::timeout(FLUSH_GRACE, wr.shutdown()).await;
            }
            _ => writer.abort(),
        }
    }
}

#[cfg(windows)]
mod winpipe {
    //! Named-pipe listener with a current-user-only DACL.
    //!
    //! NOTE: written without access to a Windows machine; compile-checked via
    //! `cargo check --target x86_64-pc-windows-msvc` only.
    use std::ffi::c_void;
    use std::io;
    use std::sync::Arc;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{ControlContext, Limits, ServerError, Transport, io_err, spawn_connection};

    /// Owned self-relative security descriptor (LocalAlloc'd).
    struct SecurityDescriptor(*mut c_void);
    // SAFETY: the descriptor is immutable after creation and only read by the kernel.
    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}
    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe { LocalFree(self.0) };
        }
    }

    fn wide_to_string(p: *const u16) -> String {
        // SAFETY: p is a NUL-terminated UTF-16 string returned by the OS.
        unsafe {
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
        }
    }

    fn current_user_sid_string() -> io::Result<String> {
        // SAFETY: plain Win32 calls with correctly sized buffers; handles are closed.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                let e = io::Error::last_os_error();
                CloseHandle(token);
                return Err(e);
            }
            CloseHandle(token);
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid_str) == 0 {
                return Err(io::Error::last_os_error());
            }
            let s = wide_to_string(sid_str);
            LocalFree(sid_str.cast());
            Ok(s)
        }
    }

    impl SecurityDescriptor {
        /// `D:P(A;;GA;;;<sid>)`: protected DACL, generic-all for the current user only.
        fn for_current_user() -> io::Result<Self> {
            let sddl = format!("D:P(A;;GA;;;{})", current_user_sid_string()?);
            let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
            let mut sd: *mut c_void = std::ptr::null_mut();
            // SAFETY: `wide` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(sd))
        }
    }

    fn create_instance(
        name: &str,
        first: bool,
        sd: &SecurityDescriptor,
    ) -> io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: `sa` and the descriptor outlive the call; the kernel copies the descriptor.
        unsafe {
            opts.create_with_security_attributes_raw(
                name,
                (&mut sa as *mut SECURITY_ATTRIBUTES).cast(),
            )
        }
    }

    /// A created first pipe instance that does not accept yet.
    pub(super) struct PipeListener {
        name: String,
        sd: SecurityDescriptor,
        server: NamedPipeServer,
    }

    pub(super) fn bind(name: &str) -> Result<PipeListener, ServerError> {
        let sd = SecurityDescriptor::for_current_user()
            .map_err(io_err("cannot build pipe security descriptor"))?;
        let server = create_instance(name, true, &sd).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                ServerError::InUse(name.to_string())
            } else {
                io_err(format!("cannot create named pipe {name}"))(e)
            }
        })?;
        Ok(PipeListener {
            name: name.to_string(),
            sd,
            server,
        })
    }

    impl PipeListener {
        pub(super) fn spawn(
            self,
            ctx: Arc<ControlContext>,
            limits: Limits,
            accept: CancellationToken,
            conns: CancellationToken,
        ) -> JoinHandle<()> {
            let PipeListener {
                name,
                sd,
                mut server,
            } = self;
            tokio::spawn(async move {
                loop {
                    let r = tokio::select! {
                        _ = accept.cancelled() => break,
                        r = server.connect() => r,
                    };
                    if let Err(e) = r {
                        tracing::warn!("control pipe connect failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    let next = loop {
                        match create_instance(&name, false, &sd) {
                            Ok(n) => break n,
                            Err(e) => {
                                tracing::warn!("cannot create next control pipe instance: {e}");
                                tokio::select! {
                                    _ = accept.cancelled() => return,
                                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
                                }
                            }
                        }
                    };
                    let conn = std::mem::replace(&mut server, next);
                    spawn_connection(conn, Transport::Local, &ctx, &limits, &conns);
                }
            })
        }
    }
}

/// Why a half-opened stream is torn down: the server going away must not hang the call up.
fn abort_reason(conn: &CancellationToken) -> &'static str {
    if conn.is_cancelled() {
        "server_shutdown"
    } else {
        "connection_lost"
    }
}
