//! The one network code path of the update check: a minimal HTTPS/1.1 GET.
//!
//! Why hand-written instead of an HTTP client crate: the check is a single GET of a small JSON
//! document, and the workspace already has everything for TLS (`rustls` with `ring`, the OS
//! certificate verifier `rustls-platform-verifier`, `tokio-rustls`). A client crate (`ureq`,
//! `reqwest`) would add its own stack of HTTP, URL and proxy crates for this one call. The
//! request is `Connection: close` with `Accept-Encoding: identity`, so a response is read to the
//! end of the stream, bounded by [`MAX_BODY`]. No redirects are followed, no proxy is used, no
//! cookies are kept.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::TlsConnector;

use super::checker::{FetchError, FetchOutcome, ReleaseFetcher};

/// Largest response accepted (20 releases with notes are well under 1 MiB).
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Largest header block accepted.
const MAX_HEAD: usize = 64 * 1024;
/// Time limit of a whole request (connect, TLS, send, receive).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

const HOST: &str = "api.github.com";
const PATH: &str = "/repos/zyno-io/siple/releases?per_page=20";

/// What a response said, before it is interpreted.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Response {
    pub status: u16,
    pub etag: Option<String>,
    /// `Retry-After` in seconds.
    pub retry_after: Option<u64>,
    /// `X-RateLimit-Remaining`.
    pub ratelimit_remaining: Option<u64>,
    /// `X-RateLimit-Reset` (epoch seconds).
    pub ratelimit_reset: Option<u64>,
    pub body: Vec<u8>,
}

/// Fetches the releases of this project from GitHub over HTTPS.
pub struct GithubFetcher;

#[async_trait]
impl ReleaseFetcher for GithubFetcher {
    async fn fetch(
        &self,
        etag: Option<&str>,
        user_agent: &str,
    ) -> Result<FetchOutcome, FetchError> {
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, get(HOST, PATH, etag, user_agent))
            .await
            .map_err(|_| FetchError::Timeout)??;
        interpret(resp, chrono::Utc::now().timestamp().max(0) as u64)
    }
}

/// Maps a response to the outcome the checker acts on. `now` is epoch seconds.
pub(crate) fn interpret(r: Response, now: u64) -> Result<FetchOutcome, FetchError> {
    match r.status {
        200 => Ok(FetchOutcome::Body {
            etag: r.etag,
            body: r.body,
        }),
        304 => Ok(FetchOutcome::NotModified),
        429 => Err(FetchError::RateLimited {
            retry_after: wait(&r, now),
        }),
        // GitHub answers an exhausted anonymous limit (and secondary limits) with 403.
        403 if r.ratelimit_remaining == Some(0) || r.retry_after.is_some() => {
            Err(FetchError::RateLimited {
                retry_after: wait(&r, now),
            })
        }
        s => Err(FetchError::Status(s)),
    }
}

fn wait(r: &Response, now: u64) -> Option<Duration> {
    r.retry_after
        .or_else(|| r.ratelimit_reset.map(|t| t.saturating_sub(now)))
        .map(Duration::from_secs)
}

async fn get(
    host: &str,
    path: &str,
    etag: Option<&str>,
    user_agent: &str,
) -> Result<Response, FetchError> {
    let tcp = tokio::net::TcpStream::connect((host, 443))
        .await
        .map_err(|e| FetchError::Network(format!("cannot connect to {host}: {e}")))?;
    let server_name =
        ServerName::try_from(host.to_string()).map_err(|e| FetchError::Network(e.to_string()))?;
    let mut tls = TlsConnector::from(tls_config()?)
        .connect(server_name, tcp)
        .await
        .map_err(|e| FetchError::Network(format!("TLS handshake with {host} failed: {e}")))?;
    get_over(&mut tls, host, path, etag, user_agent).await
}

fn tls_config() -> Result<Arc<ClientConfig>, FetchError> {
    let err = |e: &dyn std::fmt::Display| FetchError::Network(format!("TLS setup failed: {e}"));
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier =
        rustls_platform_verifier::Verifier::new(provider.clone()).map_err(|e| err(&e))?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| err(&e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Sends the request over `stream` and reads the whole response.
pub(crate) async fn get_over<S>(
    stream: &mut S,
    host: &str,
    path: &str,
    etag: Option<&str>,
    user_agent: &str,
) -> Result<Response, FetchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {user_agent}\r\n\
         Accept: application/vnd.github+json\r\nAccept-Encoding: identity\r\n\
         X-GitHub-Api-Version: 2022-11-28\r\nConnection: close\r\n"
    );
    if let Some(t) = etag.filter(|t| !t.contains(['\r', '\n'])) {
        req.push_str(&format!("If-None-Match: {t}\r\n"));
    }
    req.push_str("\r\n");
    let net = |what: &str, e: std::io::Error| FetchError::Network(format!("{what}: {e}"));
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| net("send failed", e))?;
    stream.flush().await.map_err(|e| net("send failed", e))?;

    let mut raw = Vec::new();
    // A peer that closes without a TLS close_notify is an error to rustls; what arrived is
    // validated below (length / chunk framing), so it is not fatal by itself.
    let read = (&mut *stream)
        .take((MAX_HEAD + MAX_BODY + 1) as u64)
        .read_to_end(&mut raw)
        .await;
    parse(&raw, read.is_ok())
}

/// Parses a raw HTTP/1.1 response. `clean_eof`: the stream ended normally.
fn parse(raw: &[u8], clean_eof: bool) -> Result<Response, FetchError> {
    let bad = |m: &str| FetchError::Network(format!("malformed response: {m}"));
    let head_end = find(raw, b"\r\n\r\n").ok_or_else(|| bad("no header block"))?;
    if head_end > MAX_HEAD {
        return Err(bad("header block too large"));
    }
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| bad("non-UTF-8 headers"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(bad("not an HTTP/1.x status line"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("bad status code"))?;

    let mut resp = Response {
        status,
        ..Response::default()
    };
    let (mut chunked, mut length) = (false, None::<usize>);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "etag" => resp.etag = Some(v.to_string()),
            "retry-after" => resp.retry_after = v.parse().ok(),
            "x-ratelimit-remaining" => resp.ratelimit_remaining = v.parse().ok(),
            "x-ratelimit-reset" => resp.ratelimit_reset = v.parse().ok(),
            "content-length" => length = v.parse().ok(),
            "transfer-encoding" => chunked = v.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let body = &raw[head_end + 4..];
    if body.len() > MAX_BODY {
        return Err(bad("body too large"));
    }
    resp.body = if status == 304 || status == 204 {
        Vec::new()
    } else if chunked {
        dechunk(body).ok_or_else(|| bad("truncated or invalid chunked body"))?
    } else if let Some(n) = length {
        if body.len() < n {
            return Err(bad("body shorter than Content-Length"));
        }
        body[..n].to_vec()
    } else if clean_eof {
        body.to_vec()
    } else {
        return Err(bad("connection closed before the end of the body"));
    };
    Ok(resp)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decodes a `Transfer-Encoding: chunked` body; `None` when it is invalid, incomplete or larger
/// than [`MAX_BODY`]. Arithmetic and slicing are checked: no input can overflow or panic.
fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = find(b, b"\r\n")?;
        let size_text = std::str::from_utf8(&b[..eol]).ok()?;
        let size_text = size_text.split(';').next()?.trim();
        if size_text.is_empty() || !size_text.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let size = usize::from_str_radix(size_text, 16).ok()?;
        b = &b[eol + 2..];
        if size == 0 {
            // Trailers (if any) end with a blank line; their absence means truncation.
            return (b.starts_with(b"\r\n") || find(b, b"\r\n\r\n").is_some()).then_some(out);
        }
        if size > MAX_BODY - out.len() {
            return None;
        }
        let end = size.checked_add(2)?;
        if b.get(size..end)? != b"\r\n" {
            return None;
        }
        out.extend_from_slice(b.get(..size)?);
        b = b.get(end..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn serve(reply: &'static [u8]) -> (Response, String) {
        let (mut client, mut server) = duplex(64 * 1024);
        let srv = tokio::spawn(async move {
            let mut got = vec![0u8; 4096];
            let n = server.read(&mut got).await.unwrap();
            server.write_all(reply).await.unwrap();
            drop(server);
            String::from_utf8_lossy(&got[..n]).to_string()
        });
        let r = get_over(
            &mut client,
            "h.test",
            "/x?y=1",
            Some("\"abc\""),
            "Siple/9.9.9",
        )
        .await
        .unwrap();
        (r, srv.await.unwrap())
    }

    #[tokio::test]
    async fn sends_the_documented_request_and_reads_content_length() {
        let (r, req) = serve(
            b"HTTP/1.1 200 OK\r\nETag: W/\"e1\"\r\nContent-Length: 4\r\nX-RateLimit-Remaining: 59\r\n\r\n[1,2]ignored",
        )
        .await;
        assert!(req.starts_with("GET /x?y=1 HTTP/1.1\r\n"), "{req}");
        for h in [
            "Host: h.test\r\n",
            "User-Agent: Siple/9.9.9\r\n",
            "Accept: application/vnd.github+json\r\n",
            "If-None-Match: \"abc\"\r\n",
            "Connection: close\r\n",
        ] {
            assert!(req.contains(h), "{h} in {req}");
        }
        assert!(!req.to_ascii_lowercase().contains("cookie"));
        assert_eq!(r.status, 200);
        assert_eq!(r.etag.as_deref(), Some("W/\"e1\""));
        assert_eq!(r.ratelimit_remaining, Some(59));
        assert_eq!(r.body, b"[1,2");
    }

    #[tokio::test]
    async fn reads_chunked_and_close_delimited_bodies() {
        let (r, _) = serve(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
        )
        .await;
        assert_eq!(r.body, b"abcde");
        let (r, _) = serve(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello").await;
        assert_eq!(r.body, b"hello");
        let (r, _) = serve(b"HTTP/1.1 304 Not Modified\r\nETag: \"abc\"\r\n\r\n").await;
        assert_eq!((r.status, r.body.len()), (304, 0));
    }

    #[test]
    fn truncation_and_garbage_are_errors() {
        for raw in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n",
            b"SMTP ready\r\n\r\n",
            b"HTTP/1.1 200 OK\r\n",
            b"",
            b"HTTP/1.1 abc OK\r\n\r\n",
            // Chunk sizes that overflow, exceed the cap, are signed or empty.
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7fffffffffffffff\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffffffffff\r\nx",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n400001\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n+3\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\nabc\r\n0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n-1\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabcXY0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3",
        ] {
            assert!(
                parse(raw, true).is_err(),
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
        // No length and no clean end: not a complete message.
        assert!(parse(b"HTTP/1.1 200 OK\r\n\r\npartial", false).is_err());
    }

    #[test]
    fn chunks_summing_past_the_cap_are_refused() {
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let chunk = vec![b'x'; MAX_BODY / 2 + 1];
        for _ in 0..2 {
            raw.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            raw.extend_from_slice(&chunk);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        assert!(parse(&raw, true).is_err());
    }

    #[test]
    fn oversized_bodies_are_refused() {
        let mut raw = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        raw.resize(raw.len() + MAX_BODY + 1, b'x');
        assert!(parse(&raw, true).is_err());
    }

    fn resp(status: u16) -> Response {
        Response {
            status,
            ..Response::default()
        }
    }

    #[test]
    fn statuses_map_to_outcomes() {
        assert!(matches!(
            interpret(
                Response {
                    etag: Some("e".into()),
                    body: b"[]".to_vec(),
                    ..resp(200)
                },
                0
            ),
            Ok(FetchOutcome::Body { etag: Some(_), .. })
        ));
        assert!(matches!(
            interpret(resp(304), 0),
            Ok(FetchOutcome::NotModified)
        ));
        assert!(matches!(
            interpret(resp(500), 0),
            Err(FetchError::Status(500))
        ));
        assert!(matches!(
            interpret(resp(404), 0),
            Err(FetchError::Status(404))
        ));
        // A plain 403 is not rate limiting.
        assert!(matches!(
            interpret(resp(403), 0),
            Err(FetchError::Status(403))
        ));
    }

    #[test]
    fn rate_limits_carry_their_wait() {
        let limited = |r: Response| match interpret(r, 1_000) {
            Err(FetchError::RateLimited { retry_after }) => retry_after,
            other => panic!("{other:?}"),
        };
        assert_eq!(limited(resp(429)), None);
        assert_eq!(
            limited(Response {
                retry_after: Some(120),
                ..resp(429)
            }),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            limited(Response {
                ratelimit_remaining: Some(0),
                ratelimit_reset: Some(1_600),
                ..resp(403)
            }),
            Some(Duration::from_secs(600))
        );
        assert_eq!(
            limited(Response {
                retry_after: Some(30),
                ..resp(403)
            }),
            Some(Duration::from_secs(30))
        );
    }
}
