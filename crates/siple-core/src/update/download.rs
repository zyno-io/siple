//! The asset downloader: a streaming HTTPS GET of one release file with a closed set of rules.
//!
//! - The URL is built here from the validated version and a plain asset name
//!   (`https://github.com/zyno-io/siple/releases/download/v<version>/<name>`), never taken from
//!   an API response.
//! - Redirects (GitHub answers once, to `release-assets.githubusercontent.com`): at most
//!   [`MAX_HOPS`]; HTTPS on port 443 only; no user information; the host must be one of
//!   [`ALLOWED_HOSTS`].
//! - `Content-Length` is required and must equal the expected size; chunked and unframed
//!   responses are refused; the size is capped; every read has an idle limit and the whole
//!   transfer a total limit; the transfer can be cancelled.
//! - The body is streamed into a new file (created exclusively, mode 0600 on Unix) while a
//!   SHA-256 is computed; a failure removes the partial file.
//! - The request carries `User-Agent: Siple/<version>` and nothing else that identifies the
//!   install (see [`super::http`]).

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::install::{ErrorKind, InstallError};
use super::sums::valid_asset_name;
use super::version::Version;

/// Hosts a download may be served from.
pub const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];
/// Most redirects followed.
pub const MAX_HOPS: usize = 3;
/// Largest archive accepted.
pub const MAX_ASSET: u64 = 256 * 1024 * 1024;
/// No byte for this long ends the download.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a whole download may take.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_HEAD: usize = 32 * 1024;

/// What the file must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    /// The size, from the API's asset list; `Content-Length` must equal it.
    pub size: u64,
    /// The SHA-256 the file must have, once known (not for the checksum list itself).
    pub sha256: Option<[u8; 32]>,
    /// Largest size acceptable (checked before the request).
    pub max: u64,
}

/// Fetches one release file. Replaced in tests; [`HttpsFetcher`] is the real one.
#[async_trait]
pub trait AssetFetcher: Send + Sync {
    /// Downloads asset `name` of release `version` into the new file `dest` and returns its
    /// SHA-256. `progress` gets the byte count so far (it is called often; throttle there).
    async fn fetch(
        &self,
        name: &str,
        version: &Version,
        dest: &Path,
        expect: &Expected,
        progress: &(dyn Fn(u64) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<[u8; 32], InstallError>;
}

/// The fetcher of an app whose host did not opt in to the network: it never touches it.
pub struct OfflineAssets;

#[async_trait]
impl AssetFetcher for OfflineAssets {
    async fn fetch(
        &self,
        _: &str,
        _: &Version,
        _: &Path,
        _: &Expected,
        _: &(dyn Fn(u64) + Send + Sync),
        _: &CancellationToken,
    ) -> Result<[u8; 32], InstallError> {
        Err(InstallError::network("downloads are not enabled"))
    }
}

/// An open byte stream (TLS in production, a pipe in tests).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Opens connections; the real one does TCP and TLS.
#[async_trait]
pub trait Connect: Send + Sync {
    /// A connected, TLS-secured stream to `host:port`.
    async fn connect(&self, host: &str, port: u16) -> std::io::Result<Box<dyn Io>>;
}

struct TlsConnect;

#[async_trait]
impl Connect for TlsConnect {
    async fn connect(&self, host: &str, port: u16) -> std::io::Result<Box<dyn Io>> {
        let other = |e: &dyn std::fmt::Display| std::io::Error::other(e.to_string());
        let tcp = tokio::net::TcpStream::connect((host, port)).await?;
        let name =
            rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| other(&e))?;
        let config = super::http::tls_config().map_err(|e| other(&e))?;
        let tls = tokio_rustls::TlsConnector::from(config)
            .connect(name, tcp)
            .await?;
        Ok(Box::new(tls))
    }
}

/// The real downloader.
pub struct HttpsFetcher {
    connect: Arc<dyn Connect>,
    user_agent: String,
    idle: Duration,
    total: Duration,
}

impl HttpsFetcher {
    /// A downloader that identifies as `Siple/<version>`.
    pub fn new(version_text: &str) -> Self {
        Self::with(
            Arc::new(TlsConnect),
            version_text,
            IDLE_TIMEOUT,
            TOTAL_TIMEOUT,
        )
    }

    /// With a given connector and limits (tests).
    pub fn with(
        connect: Arc<dyn Connect>,
        version_text: &str,
        idle: Duration,
        total: Duration,
    ) -> Self {
        Self {
            connect,
            user_agent: format!("Siple/{version_text}"),
            idle,
            total,
        }
    }
}

/// `https://github.com/zyno-io/siple/releases/download/v<version>/<name>` as host and path, or
/// `None` for a name that is not a plain file name.
pub fn asset_location(version: &Version, name: &str) -> Option<(&'static str, String)> {
    valid_asset_name(name).then(|| {
        (
            "github.com",
            format!("/zyno-io/siple/releases/download/v{version}/{name}"),
        )
    })
}

/// A redirect target after the checks.
#[derive(Debug, PartialEq, Eq)]
struct Target {
    host: String,
    path: String,
}

/// Resolves a `Location` header against the current host: absolute `https` URLs and
/// absolute-path references only.
fn resolve_location(current_host: &str, location: &str) -> Result<Target, String> {
    if location.is_empty()
        || location.len() > 4096
        || location
            .bytes()
            .any(|b| b <= b' ' || b == 0x7f || b == b'\\')
    {
        return Err("the redirect target is not a plain URL".into());
    }
    let (host, path) = if let Some(rest) = location.strip_prefix("https://") {
        let (authority, path) = match rest.find(['/', '?', '#']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.contains('@') {
            return Err("a redirect carried user information".into());
        }
        let host = match authority.rsplit_once(':') {
            Some((h, "443")) => h,
            Some((_, _)) => return Err("a redirect named a port other than 443".into()),
            None => authority,
        };
        (host.to_ascii_lowercase(), path.to_string())
    } else if location.starts_with("//") {
        return Err("a scheme-relative redirect".into());
    } else if location.starts_with('/') {
        (current_host.to_string(), location.to_string())
    } else if location.contains(':') {
        return Err("a redirect to a scheme other than https".into());
    } else {
        return Err("a relative redirect".into());
    };
    if !ALLOWED_HOSTS.contains(&host.as_str()) {
        return Err(format!(
            "a redirect to {host}, which is not an allowed host"
        ));
    }
    let path = match path.split_once('#') {
        Some((p, _)) => p.to_string(),
        None => path,
    };
    let path = if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    Ok(Target { host, path })
}

/// The parts of a response head that matter.
#[derive(Debug, Default)]
struct Head {
    status: u16,
    location: Option<String>,
    content_length: Option<u64>,
    framed_other: bool,
    encoded: bool,
}

fn parse_head(raw: &[u8]) -> Result<Head, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "non-UTF-8 response headers")?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err("not an HTTP/1.x response".into());
    }
    let mut head = Head {
        status: parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or("bad status code")?,
        ..Head::default()
    };
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "location" => head.location = Some(v.to_string()),
            "content-length" => {
                // Two different lengths are a smuggling attempt, whatever they say.
                let n: u64 = v.parse().map_err(|_| "bad Content-Length")?;
                if head.content_length.is_some_and(|old| old != n) {
                    return Err("conflicting Content-Length headers".into());
                }
                head.content_length = Some(n);
            }
            "transfer-encoding" => head.framed_other = true,
            "content-encoding" if !v.eq_ignore_ascii_case("identity") => head.encoded = true,
            _ => {}
        }
    }
    Ok(head)
}

fn net<T: std::fmt::Display>(m: T) -> InstallError {
    InstallError::network(m.to_string())
}

impl HttpsFetcher {
    async fn run(
        &self,
        name: &str,
        version: &Version,
        dest: &Path,
        expect: &Expected,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<[u8; 32], InstallError> {
        if expect.size == 0 || expect.size > expect.max.min(MAX_ASSET) {
            return Err(net(format!(
                "{name}: the size {} is outside what is accepted",
                expect.size
            )));
        }
        let (host, path) =
            asset_location(version, name).ok_or_else(|| net("not a plain asset name"))?;
        let mut target = Target {
            host: host.to_string(),
            path,
        };
        let mut hops = 0;
        loop {
            let mut stream = self
                .connect
                .connect(&target.host, 443)
                .await
                .map_err(|e| net(format!("cannot connect to {}: {e}", target.host)))?;
            let req = format!(
                "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
                target.path, target.host, self.user_agent
            );
            stream.write_all(req.as_bytes()).await.map_err(net)?;
            stream.flush().await.map_err(net)?;

            // Head.
            let mut buf = Vec::with_capacity(8192);
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i;
                }
                if buf.len() > MAX_HEAD {
                    return Err(net("the response headers are too large"));
                }
                let n = tokio::time::timeout(self.idle, stream.read(&mut chunk))
                    .await
                    .map_err(|_| net("timed out waiting for the response"))?
                    .map_err(net)?;
                if n == 0 {
                    return Err(net(
                        "the connection closed before the response headers ended",
                    ));
                }
                buf.extend_from_slice(&chunk[..n]);
            };
            let head = parse_head(&buf[..head_end]).map_err(net)?;
            let first_body = buf[head_end + 4..].to_vec();

            match head.status {
                301 | 302 | 303 | 307 | 308 => {
                    hops += 1;
                    if hops > MAX_HOPS {
                        return Err(net("too many redirects"));
                    }
                    let loc = head
                        .location
                        .ok_or_else(|| net("a redirect without a target"))?;
                    target = resolve_location(&target.host, &loc).map_err(net)?;
                    continue;
                }
                200 => {}
                s => return Err(net(format!("{} answered HTTP {s}", target.host))),
            }
            if head.framed_other || head.encoded {
                return Err(net(
                    "the response is chunked or encoded; a plain Content-Length body is required",
                ));
            }
            let Some(len) = head.content_length else {
                return Err(net("the response has no Content-Length"));
            };
            if len != expect.size {
                return Err(net(format!(
                    "the server announced {len} bytes, the release lists {}",
                    expect.size
                )));
            }
            return self
                .stream_body(&mut stream, first_body, dest, expect, progress)
                .await;
        }
    }

    async fn stream_body(
        &self,
        stream: &mut Box<dyn Io>,
        first: Vec<u8>,
        dest: &Path,
        expect: &Expected,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<[u8; 32], InstallError> {
        let mut file = create_new(dest)
            .map_err(|e| InstallError::io("cannot create the download file", &e))?;
        let result = async {
            let mut hasher = Sha256::new();
            let mut got = 0u64;
            let mut feed = |data: &[u8], got: &mut u64| -> Result<(), InstallError> {
                *got += data.len() as u64;
                if *got > expect.size {
                    return Err(net("the server sent more than it announced"));
                }
                hasher.update(data);
                file.write_all(data)
                    .map_err(|e| InstallError::io("cannot write the download", &e))?;
                progress(*got);
                Ok(())
            };
            if !first.is_empty() {
                feed(&first, &mut got)?;
            }
            let mut chunk = vec![0u8; 64 * 1024];
            while got < expect.size {
                let n = tokio::time::timeout(self.idle, stream.read(&mut chunk))
                    .await
                    .map_err(|_| net("the download stalled"))?
                    .map_err(net)?;
                if n == 0 {
                    return Err(net(format!(
                        "the download ended after {got} of {} bytes",
                        expect.size
                    )));
                }
                feed(&chunk[..n], &mut got)?;
            }
            // Anything after the announced length is a protocol error: it would be unhashed.
            let mut probe = [0u8; 1];
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_millis(50), stream.read(&mut probe)).await
                && n > 0
            {
                return Err(net("the server sent more than it announced"));
            }
            file.sync_all()
                .map_err(|e| InstallError::io("cannot write the download", &e))?;
            let digest: [u8; 32] = hasher.finalize().into();
            if let Some(want) = expect.sha256
                && want != digest
            {
                return Err(InstallError::untrusted(
                    "the download does not match its checksum",
                ));
            }
            Ok(digest)
        }
        .await;
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(dest);
        }
        result
    }
}

#[async_trait]
impl AssetFetcher for HttpsFetcher {
    async fn fetch(
        &self,
        name: &str,
        version: &Version,
        dest: &Path,
        expect: &Expected,
        progress: &(dyn Fn(u64) + Send + Sync),
        cancel: &CancellationToken,
    ) -> Result<[u8; 32], InstallError> {
        let work =
            tokio::time::timeout(self.total, self.run(name, version, dest, expect, progress));
        tokio::select! {
            () = cancel.cancelled() => {
                let _ = std::fs::remove_file(dest);
                Err(InstallError::cancelled())
            }
            r = work => match r {
                Ok(r) => r,
                Err(_) => {
                    let _ = std::fs::remove_file(dest);
                    Err(InstallError::new(ErrorKind::Network, "the download took too long"))
                }
            },
        }
    }
}

/// Creates `path` exclusively (it must not exist), readable by the user only.
pub(crate) fn create_new(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::duplex;

    type Script = Box<dyn Fn(&str) -> Vec<u8> + Send + Sync>;

    /// Hosts answer with a canned response; the requests are recorded as (host, request text).
    #[derive(Default)]
    struct Fake {
        hosts: Mutex<HashMap<String, Arc<Script>>>,
        seen: Arc<Mutex<Vec<(String, u16, String)>>>,
    }

    impl Fake {
        fn host(&self, h: &str, f: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) {
            self.hosts
                .lock()
                .unwrap()
                .insert(h.into(), Arc::new(Box::new(f)));
        }
    }

    #[async_trait]
    impl Connect for Fake {
        async fn connect(&self, host: &str, port: u16) -> std::io::Result<Box<dyn Io>> {
            let script = self
                .hosts
                .lock()
                .unwrap()
                .get(host)
                .cloned()
                .ok_or_else(|| std::io::Error::other("no such host"))?;
            let (client, mut server) = duplex(1 << 20);
            let host = host.to_string();
            let seen = self.seen.clone();
            tokio::spawn(async move {
                let mut got = vec![0u8; 4096];
                let n = server.read(&mut got).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&got[..n]).to_string();
                seen.lock().unwrap().push((host, port, req.clone()));
                let _ = server.write_all(&script(&req)).await;
                let _ = server.shutdown().await;
            });
            Ok(Box::new(client))
        }
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut v =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        v.extend_from_slice(body);
        v
    }

    fn redirect(to: &str) -> Vec<u8> {
        format!("HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\n\r\n").into_bytes()
    }

    fn sha(b: &[u8]) -> [u8; 32] {
        Sha256::digest(b).into()
    }

    fn ver() -> Version {
        Version::parse("0.2.0").unwrap()
    }

    async fn run(
        fake: Arc<Fake>,
        body: &[u8],
        size: u64,
        hash: bool,
    ) -> (
        Result<[u8; 32], InstallError>,
        std::path::PathBuf,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dl");
        let f = HttpsFetcher::with(
            fake,
            "0.1.0",
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let exp = Expected {
            size,
            sha256: hash.then(|| sha(body)),
            max: MAX_ASSET,
        };
        let r = f
            .fetch(
                "Siple-0.2.0-macos.zip",
                &ver(),
                &dest,
                &exp,
                &|_| {},
                &CancellationToken::new(),
            )
            .await;
        (r, dest, dir)
    }

    #[tokio::test]
    async fn follows_the_one_github_redirect_and_hashes() {
        let fake = Arc::new(Fake::default());
        fake.host("github.com", |_| {
            redirect("https://release-assets.githubusercontent.com/x/y?sig=1")
        });
        fake.host("release-assets.githubusercontent.com", |_| {
            ok(b"hello archive")
        });
        let (r, dest, _d) = run(fake.clone(), b"hello archive", 13, true).await;
        assert_eq!(r.unwrap(), sha(b"hello archive"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"hello archive");
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen[0].0, "github.com");
        assert_eq!(seen[0].1, 443);
        assert!(seen[0].2.starts_with(
            "GET /zyno-io/siple/releases/download/v0.2.0/Siple-0.2.0-macos.zip HTTP/1.1\r\n"
        ));
        assert_eq!(seen[1].0, "release-assets.githubusercontent.com");
        assert!(seen[1].2.starts_with("GET /x/y?sig=1 HTTP/1.1\r\n"));
        // Anonymous: the base four headers only.
        let mut names: Vec<_> = seen[1]
            .2
            .split("\r\n")
            .skip(1)
            .filter_map(|l| l.split_once(':').map(|(k, _)| k.to_ascii_lowercase()))
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["accept-encoding", "connection", "host", "user-agent"]
        );
        assert!(seen[1].2.contains("User-Agent: Siple/0.1.0\r\n"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn refuses_redirects_to_disallowed_places() {
        for (loc, why) in [
            ("https://evil.example/x", "not an allowed host"),
            ("http://github.com/x", "scheme other than https"),
            ("https://github.com:8443/x", "port other than 443"),
            ("https://user:pw@github.com/x", "user information"),
            ("https://github.com@evil.example/x", "user information"),
            ("//evil.example/x", "scheme-relative"),
            ("ftp://github.com/x", "scheme other than https"),
            ("https://github.com.evil.example/x", "not an allowed host"),
            ("https://evilgithub.com/x", "not an allowed host"),
            ("file:///etc/passwd", "scheme other than https"),
            ("relative/path", "relative redirect"),
            ("", "plain URL"),
            ("https://github.com/x y", "plain URL"),
        ] {
            let fake = Arc::new(Fake::default());
            let l = loc.to_string();
            fake.host("github.com", move |_| redirect(&l));
            fake.host("evil.example", |_| ok(b"pwned"));
            let (r, dest, _d) = run(fake.clone(), b"hello", 5, false).await;
            let e = r.unwrap_err();
            assert!(e.message.contains(why), "{loc}: {e}");
            assert!(!dest.exists());
            assert_eq!(
                fake.seen.lock().unwrap().len(),
                1,
                "{loc} must not be requested"
            );
        }
    }

    #[tokio::test]
    async fn at_most_three_hops() {
        let fake = Arc::new(Fake::default());
        fake.host("github.com", |_| {
            redirect("https://objects.githubusercontent.com/a")
        });
        fake.host("objects.githubusercontent.com", |req| {
            if req.starts_with("GET /a ") {
                redirect("/b")
            } else if req.starts_with("GET /b ") {
                redirect("/c")
            } else if req.starts_with("GET /c ") {
                redirect("/d")
            } else {
                ok(b"hello")
            }
        });
        let (r, _, _d) = run(fake.clone(), b"hello", 5, false).await;
        assert!(r.unwrap_err().message.contains("too many redirects"));
        // Three hops are fine.
        let fake = Arc::new(Fake::default());
        fake.host("github.com", |_| {
            redirect("https://objects.githubusercontent.com/a")
        });
        fake.host("objects.githubusercontent.com", |req| {
            if req.starts_with("GET /a ") {
                redirect("/b")
            } else if req.starts_with("GET /b ") {
                redirect("/c")
            } else {
                ok(b"hello")
            }
        });
        assert!(run(fake, b"hello", 5, true).await.0.is_ok());
    }

    #[tokio::test]
    async fn the_size_must_match_and_the_framing_be_plain() {
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (ok(b"hello"), "announced 5 bytes, the release lists 6"),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"
                    .to_vec(),
                "chunked",
            ),
            (
                b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello!".to_vec(),
                "no Content-Length",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nContent-Length: 7\r\n\r\nhello!".to_vec(),
                "conflicting",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 6\r\n\r\nhello!"
                    .to_vec(),
                "encoded",
            ),
            (
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                "HTTP 404",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nhel".to_vec(),
                "ended after 3 of 6",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nhello!extra".to_vec(),
                "more than it announced",
            ),
            (b"garbage".to_vec(), "closed before"),
        ];
        for (resp, why) in cases {
            let fake = Arc::new(Fake::default());
            fake.host("github.com", move |_| resp.clone());
            let (r, dest, _d) = run(fake, b"hello!", 6, false).await;
            let e = r.unwrap_err();
            assert!(e.message.contains(why), "{why}: {e}");
            assert_eq!(e.kind, ErrorKind::Network);
            assert!(!dest.exists(), "{why}: partial file left");
        }
    }

    #[tokio::test]
    async fn a_hash_mismatch_removes_the_file() {
        let fake = Arc::new(Fake::default());
        fake.host("github.com", |_| ok(b"hello!"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dl");
        let f = HttpsFetcher::with(
            fake,
            "0.1.0",
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let exp = Expected {
            size: 6,
            sha256: Some([7; 32]),
            max: MAX_ASSET,
        };
        let e = f
            .fetch(
                "Siple-0.2.0-macos.zip",
                &ver(),
                &dest,
                &exp,
                &|_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Untrusted);
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn the_size_cap_applies_before_any_request() {
        let fake = Arc::new(Fake::default());
        fake.host("github.com", |_| ok(b""));
        let dir = tempfile::tempdir().unwrap();
        let f = HttpsFetcher::with(
            fake.clone(),
            "0.1.0",
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        for size in [0, MAX_ASSET + 1] {
            let exp = Expected {
                size,
                sha256: None,
                max: MAX_ASSET,
            };
            let e = f
                .fetch(
                    "Siple-0.2.0-macos.zip",
                    &ver(),
                    &dir.path().join("a"),
                    &exp,
                    &|_| {},
                    &CancellationToken::new(),
                )
                .await
                .unwrap_err();
            assert!(e.message.contains("outside what is accepted"), "{e}");
        }
        let exp = Expected {
            size: 100,
            sha256: None,
            max: 64,
        };
        assert!(
            f.fetch(
                "x.zip",
                &ver(),
                &dir.path().join("a"),
                &exp,
                &|_| {},
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert!(fake.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bad_asset_names_are_not_requested() {
        let fake = Arc::new(Fake::default());
        let dir = tempfile::tempdir().unwrap();
        let f = HttpsFetcher::with(
            fake.clone(),
            "0.1.0",
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let exp = Expected {
            size: 5,
            sha256: None,
            max: MAX_ASSET,
        };
        for name in ["../x", "a/b", "", "a b", "x?y=1"] {
            assert!(
                f.fetch(
                    name,
                    &ver(),
                    &dir.path().join("a"),
                    &exp,
                    &|_| {},
                    &CancellationToken::new()
                )
                .await
                .is_err()
            );
        }
        assert!(fake.seen.lock().unwrap().is_empty());
        assert!(asset_location(&ver(), "../x").is_none());
    }

    #[tokio::test]
    async fn progress_counts_bytes_up_to_the_size() {
        let body = vec![7u8; 200_000];
        let fake = Arc::new(Fake::default());
        let b = body.clone();
        fake.host("github.com", move |_| ok(&b));
        let dir = tempfile::tempdir().unwrap();
        let f = HttpsFetcher::with(
            fake,
            "0.1.0",
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let last = AtomicU64::new(0);
        let calls = AtomicU64::new(0);
        let exp = Expected {
            size: 200_000,
            sha256: Some(sha(&body)),
            max: MAX_ASSET,
        };
        f.fetch(
            "Siple-0.2.0-macos.zip",
            &ver(),
            &dir.path().join("a"),
            &exp,
            &|n| {
                assert!(n >= last.swap(n, Ordering::SeqCst));
                calls.fetch_add(1, Ordering::SeqCst);
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(last.load(Ordering::SeqCst), 200_000);
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn an_existing_file_is_never_overwritten() {
        let fake = Arc::new(Fake::default());
        fake.host("github.com", |_| ok(b"hello"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dl");
        std::fs::write(&dest, b"mine").unwrap();
        let f = HttpsFetcher::with(
            fake,
            "0.1.0",
            Duration::from_secs(5),
            Duration::from_secs(60),
        );
        let exp = Expected {
            size: 5,
            sha256: None,
            max: MAX_ASSET,
        };
        let e = f
            .fetch(
                "Siple-0.2.0-macos.zip",
                &ver(),
                &dest,
                &exp,
                &|_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Io);
    }

    /// A server that sends the head and part of the body, then goes quiet.
    struct Stall;
    #[async_trait]
    impl Connect for Stall {
        async fn connect(&self, _: &str, _: u16) -> std::io::Result<Box<dyn Io>> {
            let (client, mut server) = duplex(1 << 16);
            tokio::spawn(async move {
                let mut b = [0u8; 4096];
                let _ = server.read(&mut b).await;
                let _ = server
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc")
                    .await;
                tokio::time::sleep(Duration::from_secs(3600)).await;
                drop(server);
            });
            Ok(Box::new(client))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_download_times_out_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dl");
        let f = HttpsFetcher::with(
            Arc::new(Stall),
            "0.1.0",
            Duration::from_secs(30),
            Duration::from_secs(900),
        );
        let exp = Expected {
            size: 10,
            sha256: None,
            max: MAX_ASSET,
        };
        let e = f
            .fetch(
                "Siple-0.2.0-macos.zip",
                &ver(),
                &dest,
                &exp,
                &|_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(e.message.contains("stalled"), "{e}");
        assert!(!dest.exists());
    }

    #[tokio::test(start_paused = true)]
    async fn the_total_limit_ends_a_slow_download() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dl");
        // Idle limit longer than the total limit: the total one fires.
        let f = HttpsFetcher::with(
            Arc::new(Stall),
            "0.1.0",
            Duration::from_secs(3000),
            Duration::from_secs(900),
        );
        let exp = Expected {
            size: 10,
            sha256: None,
            max: MAX_ASSET,
        };
        let e = f
            .fetch(
                "Siple-0.2.0-macos.zip",
                &ver(),
                &dest,
                &exp,
                &|_| {},
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(e.message.contains("too long"), "{e}");
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn cancelling_stops_the_transfer_and_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("dl");
        let f = HttpsFetcher::with(
            Arc::new(Stall),
            "0.1.0",
            Duration::from_secs(30),
            Duration::from_secs(900),
        );
        let exp = Expected {
            size: 10,
            sha256: None,
            max: MAX_ASSET,
        };
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        let e = f
            .fetch(
                "Siple-0.2.0-macos.zip",
                &ver(),
                &dest,
                &exp,
                &|_| {},
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert!(!dest.exists());
    }

    #[test]
    fn location_resolution() {
        assert_eq!(
            resolve_location(
                "github.com",
                "https://objects.githubusercontent.com:443/a?b=c#frag"
            )
            .unwrap(),
            Target {
                host: "objects.githubusercontent.com".into(),
                path: "/a?b=c".into()
            }
        );
        assert_eq!(
            resolve_location("objects.githubusercontent.com", "/z")
                .unwrap()
                .host,
            "objects.githubusercontent.com"
        );
        assert_eq!(
            resolve_location("github.com", "https://GitHub.com").unwrap(),
            Target {
                host: "github.com".into(),
                path: "/".into()
            }
        );
    }
}
