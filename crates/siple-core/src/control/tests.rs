//! End-to-end tests of the control server over real sockets.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::client::{ClientError, ControlClient};
use super::methods::{ControlBackend, ControlContext, MethodInfo, StubBackend, wait_for};
use super::protocol::{ApiError, ErrorKind};
use super::server::{ControlServer, ServerError, effective_unix_path};
use crate::config::Secret;
use crate::events::{EventFilter, EventLog};
use crate::instance::ControlEndpoints;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "tok-0123456789";

/// Backend with a slow method to test concurrency.
struct TestBackend;

#[async_trait]
impl ControlBackend for TestBackend {
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "slow" => {
                tokio::time::sleep(Duration::from_millis(params["ms"].as_u64().unwrap_or(0))).await;
                Ok(json!({ "slept": params["ms"] }))
            }
            "fail" => Err(ApiError::sip_error(486, "Busy Here")),
            other => StubBackend.call(other, params).await,
        }
    }
    fn methods(&self) -> Vec<MethodInfo> {
        let mut m = StubBackend.methods();
        m.push(MethodInfo::new("slow", "sleeps", json!({"type":"object"})));
        m
    }
}

struct Fixture {
    server: ControlServer,
    log: Arc<EventLog>,
    _dir: tempfile::TempDir,
    cancel: CancellationToken,
}

impl Fixture {
    async fn new(tcp: bool) -> Self {
        let log = Arc::new(EventLog::new(1000));
        let ctx = ControlContext::new(log.clone(), Arc::new(TestBackend), None, Secret::new(TOKEN));
        Self::with_ctx(tcp, log, ctx).await
    }
    async fn with_ctx(tcp: bool, log: Arc<EventLog>, ctx: ControlContext) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let req = ControlEndpoints {
            unix_socket: Some(dir.path().join("c.sock").to_string_lossy().into_owned()),
            tcp: tcp.then(|| "127.0.0.1:0".to_string()),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let server = ControlServer::start(&req, ctx, cancel.clone())
            .await
            .unwrap();
        Self {
            server,
            log,
            _dir: dir,
            cancel,
        }
    }
    fn sock(&self) -> String {
        self.server.endpoints().unix_socket.clone().unwrap()
    }
    async fn client(&self) -> ControlClient {
        let mut c = ControlClient::connect_unix(self.sock()).await.unwrap();
        c.hello(None, "test").await.unwrap();
        c
    }
}

async fn raw(
    path: &str,
) -> (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
) {
    let s = tokio::net::UnixStream::connect(path).await.unwrap();
    let (r, w) = s.into_split();
    (BufReader::new(r), w)
}

async fn read_json(r: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
    let mut l = String::new();
    tokio::time::timeout(Duration::from_secs(3), r.read_line(&mut l))
        .await
        .expect("timed out")
        .unwrap();
    serde_json::from_str(&l).unwrap_or_else(|_| panic!("not json: {l:?}"))
}

#[tokio::test]
async fn hello_then_ping_and_methods() {
    let f = Fixture::new(false).await;
    let mut c = ControlClient::connect_unix(f.sock()).await.unwrap();
    let h = c.hello(None, "t").await.unwrap();
    assert_eq!(h["protocol"], 1);
    assert_eq!(h["current_seq"], 0);
    assert_eq!(
        c.call("ping", Value::Null).await.unwrap(),
        json!({"pong":true})
    );
    let m = c.call("methods.list", Value::Null).await.unwrap();
    let names: Vec<_> = m["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    for n in [
        "hello",
        "ping",
        "methods.list",
        "events.poll",
        "events.subscribe",
        "logs.tail",
        "status",
        "echo",
        "slow",
    ] {
        assert!(names.contains(&n), "missing {n}: {names:?}");
    }
    assert_eq!(m["methods"][0]["params_schema"]["type"], "object");
    assert_eq!(
        c.call("echo", json!({"a":1})).await.unwrap(),
        json!({"a":1})
    );
    assert_eq!(c.call("status", Value::Null).await.unwrap()["stub"], true);
    f.server.shutdown().await;
}

#[tokio::test]
async fn errors_are_typed() {
    let f = Fixture::new(false).await;
    let mut c = f.client().await;
    let kind = |e: ClientError| match e {
        ClientError::Api(a) => a,
        other => panic!("{other}"),
    };
    assert_eq!(
        kind(c.call("nope", Value::Null).await.unwrap_err()).kind,
        ErrorKind::MethodNotFound
    );
    let e = kind(c.call("fail", Value::Null).await.unwrap_err());
    assert_eq!((e.kind, e.sip_code), (ErrorKind::SipError, Some(486)));
    let e = kind(c.call("events.poll", json!({"bogus":1})).await.unwrap_err());
    assert_eq!(e.kind, ErrorKind::InvalidParams);
    let e = kind(c.call("logs.tail", Value::Null).await.unwrap_err());
    assert_eq!(e.kind, ErrorKind::Internal);
    f.server.shutdown().await;
}

#[tokio::test]
async fn hello_required_first() {
    let f = Fixture::new(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    let v = read_json(&mut r).await;
    assert_eq!(v["error"]["data"]["kind"], "unauthorized");
    let mut rest = Vec::new();
    // Connection is closed after the error.
    tokio::time::timeout(Duration::from_secs(3), r.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert!(rest.is_empty());
    f.server.shutdown().await;
}

#[tokio::test]
async fn non_json_closes_connection() {
    let f = Fixture::new(false).await;
    for payload in [
        &b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..],
        b"\n",
        b"{broken\n",
    ] {
        let (mut r, mut w) = raw(&f.sock()).await;
        w.write_all(payload).await.unwrap();
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), r.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap();
        assert!(
            rest.is_empty(),
            "got reply: {:?}",
            String::from_utf8_lossy(&rest)
        );
    }
    // After hello, garbage also closes.
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    assert!(read_json(&mut r).await["result"].is_object());
    w.write_all(b"garbage\n").await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), r.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert!(rest.is_empty());
    f.server.shutdown().await;
}

#[tokio::test]
async fn valid_json_but_bad_request_gets_error_not_close() {
    let f = Fixture::new(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\"}\n[1,2]\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n").await.unwrap();
    assert!(read_json(&mut r).await["result"].is_object());
    assert_eq!(
        read_json(&mut r).await["error"]["data"]["kind"],
        "invalid_request"
    );
    assert_eq!(read_json(&mut r).await["result"]["pong"], true);
    f.server.shutdown().await;
}

#[tokio::test]
async fn oversized_line_closes() {
    let f = Fixture::new(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    let big = vec![b'a'; 1024 * 1024 + 10];
    let _ = w.write_all(&big).await;
    let _ = w.write_all(b"\n").await;
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), r.read_to_end(&mut rest))
        .await
        .unwrap();
    assert!(rest.is_empty());
    f.server.shutdown().await;
}

#[tokio::test]
async fn unix_token_optional_but_must_match_if_given() {
    let f = Fixture::new(false).await;
    let mut c = ControlClient::connect_unix(f.sock()).await.unwrap();
    assert!(c.hello(Some(TOKEN), "t").await.is_ok());
    let mut c = ControlClient::connect_unix(f.sock()).await.unwrap();
    let e = c.hello(Some("wrong"), "t").await.unwrap_err();
    assert!(
        matches!(e, ClientError::Api(ref a) if a.kind == ErrorKind::Unauthorized),
        "{e}"
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn tcp_requires_token() {
    let f = Fixture::new(true).await;
    let addr = f.server.endpoints().tcp.clone().unwrap();
    assert!(
        addr.starts_with("127.0.0.1:") && !addr.ends_with(":0"),
        "{addr}"
    );
    for tok in [None, Some("wrong"), Some("")] {
        let mut c = ControlClient::connect_tcp(&addr).await.unwrap();
        let e = c.hello(tok, "t").await.unwrap_err();
        assert!(
            matches!(e, ClientError::Api(ref a) if a.kind == ErrorKind::Unauthorized),
            "{tok:?}: {e}"
        );
        // Connection is dropped after failed auth.
        assert!(c.call("ping", Value::Null).await.is_err());
    }
    let mut c = ControlClient::connect_tcp(&addr).await.unwrap();
    c.hello(Some(TOKEN), "t").await.unwrap();
    assert_eq!(c.call("ping", Value::Null).await.unwrap()["pong"], true);
    f.server.shutdown().await;
}

#[tokio::test]
async fn tcp_non_loopback_rejected() {
    let log = Arc::new(EventLog::new(10));
    let ctx = ControlContext::new(log, Arc::new(StubBackend), None, Secret::new(TOKEN));
    let req = ControlEndpoints {
        tcp: Some("0.0.0.0:0".into()),
        ..Default::default()
    };
    let r = ControlServer::start(&req, ctx, CancellationToken::new()).await;
    assert!(matches!(r, Err(ServerError::Config(_))));
}

#[tokio::test]
async fn concurrent_requests_interleave() {
    let f = Fixture::new(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    read_json(&mut r).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"slow\",\"method\":\"slow\",\"params\":{\"ms\":400}}\n{\"jsonrpc\":\"2.0\",\"id\":\"fast\",\"method\":\"ping\"}\n").await.unwrap();
    // The fast response overtakes the slow one.
    assert_eq!(read_json(&mut r).await["id"], "fast");
    assert_eq!(read_json(&mut r).await["id"], "slow");
    f.server.shutdown().await;
}

#[tokio::test]
async fn events_push_only_after_subscribe() {
    let f = Fixture::new(false).await;
    let mut c = f.client().await;
    f.log.emit(
        "call.state",
        Some("a"),
        Some("c1"),
        json!({"state":"ringing"}),
    );
    // Not subscribed: nothing pushed.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), c.next_event())
            .await
            .is_err()
    );
    let r = c
        .call("events.subscribe", json!({"kinds":["call.*"]}))
        .await
        .unwrap();
    assert_eq!(r["current_seq"], 1);
    f.log
        .emit("account.registration", Some("a"), None, json!({}));
    f.log.emit(
        "call.state",
        Some("a"),
        Some("c1"),
        json!({"state":"active"}),
    );
    let e = tokio::time::timeout(Duration::from_secs(2), c.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (e.seq, e.kind.as_str(), e.data["state"].as_str()),
        (3, "call.state", Some("active"))
    );
    // Unsubscribe stops the stream.
    c.call("events.unsubscribe", Value::Null).await.unwrap();
    f.log.emit("call.state", None, None, json!({}));
    assert!(
        tokio::time::timeout(Duration::from_millis(150), c.next_event())
            .await
            .is_err()
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn events_poll_long_poll_wakes_and_times_out() {
    let f = Fixture::new(false).await;
    let mut c = f.client().await;
    // Timeout path.
    let t = std::time::Instant::now();
    let r = c
        .call("events.poll", json!({"timeout_ms":100}))
        .await
        .unwrap();
    assert_eq!(r["events"], json!([]));
    assert!(t.elapsed() >= Duration::from_millis(90));
    // Wake path: a second connection emits via the log.
    let log = f.log.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        log.emit("call.incoming", Some("a"), Some("c1"), json!({"from":"x"}));
    });
    let t = std::time::Instant::now();
    let r = c
        .call(
            "events.poll",
            json!({"since":0,"timeout_ms":5000,"kinds":["call.*"]}),
        )
        .await
        .unwrap();
    assert!(t.elapsed() < Duration::from_secs(2));
    assert_eq!(r["events"][0]["kind"], "call.incoming");
    assert_eq!(r["next_seq"], 1);
    assert_eq!(r["truncated"], false);
    // Timeout is clamped, current_seq works.
    assert_eq!(
        c.call("events.current_seq", Value::Null).await.unwrap()["seq"],
        1
    );
    f.server.shutdown().await;
}

#[tokio::test]
async fn wait_for_is_race_free_and_times_out() {
    let log = EventLog::new(100);
    let base = log.current_seq();
    // Event happens before we start waiting.
    log.emit("call.state", None, Some("c1"), json!({"state":"active"}));
    let e = wait_for(
        &log,
        base,
        &EventFilter::kinds(["call.state"]),
        |e| e.data["state"] == "active",
        Duration::from_secs(1),
    )
    .await
    .unwrap();
    assert_eq!(e.seq, 1);
    let err = wait_for(
        &log,
        1,
        &EventFilter::any(),
        |_| true,
        Duration::from_millis(50),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    // Non-matching predicate events are skipped, later one is found.
    let log = Arc::new(log);
    let l2 = log.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        l2.emit("call.state", None, None, json!({"state":"ringing"}));
        l2.emit("call.state", None, None, json!({"state":"ended"}));
    });
    let e = wait_for(
        &log,
        1,
        &EventFilter::any(),
        |e| e.data["state"] == "ended",
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(e.seq, 3);
}

#[tokio::test]
async fn socket_permissions_stale_cleanup_and_shutdown() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(false).await;
    let sock = f.sock();
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // A second server on a live socket is refused.
    let ctx = ControlContext::new(
        f.log.clone(),
        Arc::new(StubBackend),
        None,
        Secret::new(TOKEN),
    );
    let req = ControlEndpoints {
        unix_socket: Some(sock.clone()),
        ..Default::default()
    };
    let r = ControlServer::start(&req, ctx.clone(), CancellationToken::new()).await;
    assert!(matches!(r, Err(ServerError::InUse(_))), "{:?}", r.err());

    f.cancel.cancel();
    f.server.shutdown().await;
    assert!(
        !std::path::Path::new(&sock).exists(),
        "socket removed on shutdown"
    );

    // A stale socket file (no listener) is replaced.
    {
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(l); // file stays, nobody listens
    }
    assert!(std::path::Path::new(&sock).exists());
    let s = ControlServer::start(&req, ctx.clone(), CancellationToken::new())
        .await
        .unwrap();
    let mut c = ControlClient::connect_unix(&sock).await.unwrap();
    c.hello(None, "t").await.unwrap();
    s.shutdown().await;

    // A regular file is never clobbered.
    std::fs::write(&sock, "precious").unwrap();
    let r = ControlServer::start(&req, ctx, CancellationToken::new()).await;
    assert!(matches!(r, Err(ServerError::Config(_))));
    assert_eq!(std::fs::read_to_string(&sock).unwrap(), "precious");
}

#[tokio::test]
async fn long_socket_path_falls_back_to_short_temp_path() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("a".repeat(120)).join("control.sock");
    let long = long.to_string_lossy().into_owned();
    let short = effective_unix_path(&long);
    assert!(short.as_os_str().len() <= 100, "{short:?}");
    assert_eq!(short, effective_unix_path(&long), "stable");
    let log = Arc::new(EventLog::new(10));
    let ctx = ControlContext::new(log, Arc::new(StubBackend), None, Secret::new(TOKEN));
    let req = ControlEndpoints {
        unix_socket: Some(long.clone()),
        ..Default::default()
    };
    let s = ControlServer::start(&req, ctx, CancellationToken::new())
        .await
        .unwrap();
    let actual = s.endpoints().unix_socket.clone().unwrap();
    assert_ne!(actual, long);
    let mut c = ControlClient::connect_unix(&actual).await.unwrap();
    c.hello(None, "t").await.unwrap();
    s.shutdown().await;
    assert!(!std::path::Path::new(&actual).exists());
}

#[tokio::test]
async fn shutdown_closes_open_connections() {
    let f = Fixture::new(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    read_json(&mut r).await;
    f.server.shutdown().await;
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), r.read_to_end(&mut rest))
        .await
        .unwrap()
        .unwrap();
}

/// Idle unauthenticated TCP connections must not lock the owner out of the unix socket
/// (per-listener caps) and are dropped quickly (short TCP hello timeout).
#[tokio::test]
async fn idle_tcp_connections_do_not_lock_out_unix_socket() {
    let f = Fixture::new(true).await;
    let tcp = f.server.endpoints().tcp.clone().unwrap();
    let mut idle = Vec::new();
    for _ in 0..64 {
        idle.push(tokio::net::TcpStream::connect(&tcp).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut c = ControlClient::connect_unix(f.sock()).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(2), c.hello(None, "owner")).await;
    assert!(matches!(r, Ok(Ok(_))), "owner locked out: {r:?}");
    // Idle TCP connections are closed by the server after the pre-auth timeout.
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(
        Duration::from_secs(6),
        tokio::io::AsyncReadExt::read(&mut idle[0], &mut buf),
    )
    .await
    .expect("idle TCP connection not closed after hello timeout");
    assert!(matches!(n, Ok(0) | Err(_)));
    f.server.shutdown().await;
}

/// A client that pipelines requests without reading, then sends garbage: the server
/// must release the socket (EOF at the client) instead of leaving a stuck writer.
#[tokio::test]
async fn non_reading_client_connection_is_released() {
    let f = Fixture::new(false).await;
    let req = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"methods.list\"}\n".repeat(2000);
    let mut s = tokio::net::UnixStream::connect(f.sock()).await.unwrap();
    s.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    // Writes may stall once the server stops reading; that is fine.
    let _ = tokio::time::timeout(Duration::from_secs(1), s.write_all(req.as_bytes())).await;
    let _ = tokio::time::timeout(Duration::from_millis(200), s.write_all(b"garbage\n")).await;
    // Do not read before the server has given up on us (a queued reply not taken within
    // SEND_TIMEOUT, 5 s; a stalled write after WRITE_TIMEOUT, 10 s): reading earlier unblocks it,
    // and with the request stream cut mid-line by the timed-out writes it then rightly waits for
    // the rest of that line. The close shows on this side as a failing write (EPIPE), without
    // reading anything. Generous bound for a loaded machine.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        match tokio::time::timeout(Duration::from_millis(100), s.write_all(b" ")).await {
            Ok(Err(_)) => break,
            _ => assert!(
                tokio::time::Instant::now() < deadline,
                "server kept the socket of a non-reading client open"
            ),
        }
    }
    // Drain whatever was queued; the server side must be closed (EOF), not hanging.
    let mut sink = Vec::new();
    let r = tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut sink)).await;
    assert!(
        r.is_ok(),
        "server kept the socket of a non-reading client open"
    );
    // Shutdown is not blocked by such clients either.
    tokio::time::timeout(Duration::from_secs(15), f.server.shutdown())
        .await
        .expect("shutdown blocked");
}

/// A burst of slow requests beyond the per-connection in-flight limit still all complete
/// (backpressure, not rejection).
#[tokio::test]
async fn inflight_limit_applies_backpressure_not_loss() {
    let f = Fixture::new(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    let mut payload = String::from("{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\"}\n");
    for i in 1..=100 {
        payload.push_str(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{i},\"method\":\"slow\",\"params\":{{\"ms\":20}}}}\n"
        ));
    }
    w.write_all(payload.as_bytes()).await.unwrap();
    let mut seen = std::collections::HashSet::new();
    while seen.len() < 101 {
        let v = read_json(&mut r).await;
        seen.insert(v["id"].as_u64().unwrap());
    }
    f.server.shutdown().await;
}

fn ctx_for(log: &Arc<EventLog>) -> ControlContext {
    ControlContext::new(log.clone(), Arc::new(TestBackend), None, Secret::new(TOKEN))
}

#[tokio::test]
async fn partial_bind_failure_leaves_nothing_bound() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("c.sock");
    let log = Arc::new(EventLog::new(10));
    // unix binds fine, TCP is on a busy port: the unix socket must not survive.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let req = ControlEndpoints {
        unix_socket: Some(sock.to_string_lossy().into_owned()),
        tcp: Some(busy.local_addr().unwrap().to_string()),
        ..Default::default()
    };
    let r = ControlServer::start(&req, ctx_for(&log), CancellationToken::new()).await;
    assert!(matches!(r, Err(ServerError::Io { .. })), "{:?}", r.err());
    assert!(!sock.exists(), "socket file left behind");
    assert!(ControlClient::connect_unix(&sock).await.is_err());
    // Same for a rejected (non-loopback) address.
    let req = ControlEndpoints {
        tcp: Some("10.1.2.3:5079".into()),
        ..req
    };
    let r = ControlServer::start(&req, ctx_for(&log), CancellationToken::new()).await;
    assert!(matches!(r, Err(ServerError::Config(_))));
    assert!(!sock.exists());
}

#[tokio::test]
async fn dropping_the_server_stops_it_and_removes_the_socket() {
    let f = Fixture::new(true).await;
    let sock = f.sock();
    let tcp = f.server.endpoints().tcp.clone().unwrap();
    let mut c = f.client().await;
    c.call("ping", Value::Null).await.unwrap();
    drop(f.server);
    assert!(!std::path::Path::new(&sock).exists());
    // The connection and the TCP listener go away shortly after.
    let gone = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if c.call("ping", Value::Null).await.is_err()
                && tokio::net::TcpStream::connect(&tcp).await.is_err()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "listeners kept running after drop");
}

#[tokio::test]
async fn reconfigure_restarts_only_changed_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(EventLog::new(10));
    let sock = dir.path().join("a.sock").to_string_lossy().into_owned();
    let mut req = ControlEndpoints {
        unix_socket: Some(sock.clone()),
        ..Default::default()
    };
    let mut server = ControlServer::start(&req, ctx_for(&log), CancellationToken::new())
        .await
        .unwrap();
    let mut a = ControlClient::connect_unix(&sock).await.unwrap();
    a.hello(None, "a").await.unwrap();

    // No change: nothing happens.
    assert!(!server.reconfigure(&req).await.unwrap());

    // Add TCP: the unix connection is untouched.
    req.tcp = Some("127.0.0.1:0".into());
    assert!(server.reconfigure(&req).await.unwrap());
    let tcp = server.endpoints().tcp.clone().unwrap();
    a.call("ping", Value::Null).await.unwrap();
    assert_eq!(
        server.endpoints().unix_socket.as_deref(),
        Some(sock.as_str())
    );

    // Change TCP to a busy port: error, and everything stays as it was.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let wanted = ControlEndpoints {
        tcp: Some(busy.local_addr().unwrap().to_string()),
        ..req.clone()
    };
    assert!(server.reconfigure(&wanted).await.is_err());
    assert_eq!(server.endpoints().tcp.as_deref(), Some(tcp.as_str()));
    a.call("ping", Value::Null).await.unwrap();
    let mut t = ControlClient::connect_tcp(&tcp).await.unwrap();
    t.hello(Some(TOKEN), "t").await.unwrap();
    // The failed attempt did not disturb the TCP connection either.
    t.call("ping", Value::Null).await.unwrap();

    // Remove TCP: it stops accepting at once; its connection lives on for the grace period,
    // the unix one is never touched.
    req.tcp = None;
    assert!(server.reconfigure(&req).await.unwrap());
    assert!(server.endpoints().tcp.is_none());
    assert!(ControlClient::connect_tcp(&tcp).await.is_err());
    t.call("ping", Value::Null).await.unwrap();
    a.call("ping", Value::Null).await.unwrap();
    let closed = tokio::time::timeout(
        super::server::RETIRE_GRACE + Duration::from_secs(3),
        async {
            while t.call("ping", Value::Null).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        },
    )
    .await;
    assert!(closed.is_ok(), "retired connection stayed open");
    a.call("ping", Value::Null).await.unwrap();

    // Move the unix socket: new path serves, old file is gone.
    let sock2 = dir.path().join("b.sock").to_string_lossy().into_owned();
    req.unix_socket = Some(sock2.clone());
    assert!(server.reconfigure(&req).await.unwrap());
    assert!(!std::path::Path::new(&sock).exists());
    let mut b = ControlClient::connect_unix(&sock2).await.unwrap();
    b.hello(None, "b").await.unwrap();
    server.shutdown().await;
    assert!(!std::path::Path::new(&sock2).exists());
    drop(busy);
}

#[tokio::test]
async fn hello_reports_instance_id_and_results_carry_it() {
    struct Seq;
    #[async_trait]
    impl ControlBackend for Seq {
        async fn call(&self, method: &str, _p: Value) -> Result<Value, ApiError> {
            match method {
                "act" => Ok(json!({"ok": true, "event_seq": 7})),
                "read" => Ok(json!({"x": 1})),
                m => Err(ApiError::method_not_found(m)),
            }
        }
        fn methods(&self) -> Vec<MethodInfo> {
            vec![]
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(EventLog::new(10));
    let ctx = ControlContext::new(log.clone(), Arc::new(Seq), None, Secret::new(TOKEN));
    let req = ControlEndpoints {
        unix_socket: Some(dir.path().join("c.sock").to_string_lossy().into_owned()),
        ..Default::default()
    };
    let server = ControlServer::start(&req, ctx, CancellationToken::new())
        .await
        .unwrap();
    let mut c = ControlClient::connect_unix(server.endpoints().unix_socket.clone().unwrap())
        .await
        .unwrap();
    let h = c.hello(None, "t").await.unwrap();
    assert_eq!(h["instance_id"], log.instance_id());
    assert!(h["started_at"].is_string());
    assert_eq!(c.instance_id(), Some(log.instance_id()));
    let r = c.call("act", Value::Null).await.unwrap();
    assert_eq!(r["instance_id"], log.instance_id());
    assert_eq!(r["event_seq"], 7);
    let r = c.call("read", Value::Null).await.unwrap();
    assert!(r.get("instance_id").is_none());
    server.shutdown().await;
}

#[tokio::test]
async fn poll_with_since_ahead_of_the_log_is_invalid_params() {
    let f = Fixture::new(false).await;
    f.log.emit("x", None, None, Value::Null);
    let mut c = f.client().await;
    let e = c
        .call("events.poll", json!({"since": 99, "timeout_ms": 5000}))
        .await
        .unwrap_err();
    let ClientError::Api(e) = e else {
        panic!("{e}")
    };
    assert_eq!(e.kind, ErrorKind::InvalidParams);
    assert!(e.message.contains("events.current_seq"), "{}", e.message);
    // since == current is fine.
    let r = c
        .call("events.poll", json!({"since": 1, "timeout_ms": 10}))
        .await
        .unwrap();
    assert_eq!(r["events"], json!([]));
    f.server.shutdown().await;
}

#[tokio::test]
async fn client_requests_time_out_and_timeouts_scale_with_the_wait() {
    use super::client::{WAIT_REQUEST_TIMEOUT, is_idempotent, request_timeout};
    let f = Fixture::new(false).await;
    let mut c = f.client().await;
    let t = std::time::Instant::now();
    let e = c
        .call_timeout("slow", json!({"ms": 5000}), Duration::from_millis(200))
        .await
        .unwrap_err();
    assert!(matches!(e, ClientError::Timeout { .. }), "{e}");
    assert!(t.elapsed() < Duration::from_secs(2));
    f.server.shutdown().await;

    assert_eq!(
        request_timeout("status", &Value::Null),
        Duration::from_secs(30)
    );
    assert_eq!(
        request_timeout("events.poll", &json!({"timeout_ms": 1000})),
        WAIT_REQUEST_TIMEOUT
    );
    assert_eq!(
        request_timeout("call.wait_for", &json!({"timeout_ms": 120_000})),
        Duration::from_secs(125)
    );
    assert!(request_timeout("call.dtmf", &json!({"digits":"123"})) > Duration::from_secs(30));
    for m in [
        "call.answer",
        "call.dial",
        "call.activate",
        "call.pickup",
        "blf.dial",
    ] {
        assert_eq!(
            request_timeout(m, &json!({})),
            Duration::from_secs(60),
            "{m}"
        );
    }
    for m in [
        "status",
        "call.list",
        "account.get",
        "events.poll",
        "methods.list",
        "ping",
    ] {
        assert!(is_idempotent(m), "{m}");
    }
    for m in [
        "call.dial",
        "call.dtmf",
        "config.reload",
        "call.hangup",
        "account.register",
    ] {
        assert!(!is_idempotent(m), "{m}");
    }
}

// ---- big (> 1 MiB) request lines -------------------------------------------------------------

/// Backend for the big-request tests. `hold` waits for the gate; `release_hold` releases the
/// big-request permit first and then waits for the gate; `len` returns `params.data.len()`.
struct BigBackend {
    gate: tokio::sync::watch::Receiver<bool>,
    started: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl ControlBackend for BigBackend {
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let len = params["data"].as_str().map_or(0, str::len);
        match method {
            "len" => Ok(json!({ "len": len })),
            "hold" | "release_hold" => {
                drop(params);
                if method == "release_hold" {
                    super::release_big_request_permit();
                }
                self.started
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut g = self.gate.clone();
                let _ = g.wait_for(|open| *open).await;
                Ok(json!({ "len": len }))
            }
            _ => Err(ApiError::method_not_found(method)),
        }
    }
    fn methods(&self) -> Vec<MethodInfo> {
        Vec::new()
    }
}

struct BigFixture {
    f: Fixture,
    open_gate: tokio::sync::watch::Sender<bool>,
    started: Arc<std::sync::atomic::AtomicUsize>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl BigFixture {
    async fn new(permits: usize, wait: Duration, idle: Duration) -> Self {
        Self::with_total(permits, wait, idle, super::methods::BIG_READ_TOTAL).await
    }
    async fn with_total(permits: usize, wait: Duration, idle: Duration, total: Duration) -> Self {
        let (open_gate, gate) = tokio::sync::watch::channel(false);
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = Arc::new(EventLog::new(100));
        let mut ctx = ControlContext::new(
            log.clone(),
            Arc::new(BigBackend {
                gate,
                started: started.clone(),
            }),
            None,
            Secret::new(TOKEN),
        );
        let permits = Arc::new(tokio::sync::Semaphore::new(permits));
        ctx.big_requests = permits.clone();
        ctx.big_permit_wait = wait;
        ctx.big_read_idle = idle;
        ctx.big_read_total = total;
        Self {
            f: Fixture::with_ctx(false, log, ctx).await,
            open_gate,
            started,
            permits,
        }
    }
    /// A connection that has completed `hello`.
    async fn conn(
        &self,
    ) -> (
        BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let (mut r, mut w) = raw(&self.f.sock()).await;
        w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"hello\"}\n")
            .await
            .unwrap();
        assert!(read_json(&mut r).await["result"].is_object());
        (r, w)
    }
    async fn wait_started(&self, n: usize) {
        for _ in 0..500 {
            if self.started.load(std::sync::atomic::Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("backend did not start {n} requests");
    }
}

fn big_line(id: u64, method: &str, data_len: usize) -> Vec<u8> {
    let mut v = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"{method}\",\"params\":{{\"data\":\""
    )
    .into_bytes();
    v.resize(v.len() + data_len, b'a');
    v.extend_from_slice(b"\"}}\n");
    v
}

/// Sends `bytes` from a task (the write may block until the server reads it).
fn send_in_background(
    mut w: tokio::net::unix::OwnedWriteHalf,
    bytes: Vec<u8>,
) -> tokio::task::JoinHandle<tokio::net::unix::OwnedWriteHalf> {
    tokio::spawn(async move {
        let _ = w.write_all(&bytes).await;
        w
    })
}

#[tokio::test]
async fn big_line_after_hello_is_accepted() {
    let b = BigFixture::new(4, Duration::from_secs(30), Duration::from_secs(10)).await;
    let (mut r, w) = b.conn().await;
    let n = 1024 * 1024 + 4096;
    let _w = send_in_background(w, big_line(1, "len", n));
    let v = read_json(&mut r).await;
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["len"], n);
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn big_line_before_hello_closes() {
    let b = BigFixture::new(4, Duration::from_secs(30), Duration::from_secs(10)).await;
    let (mut r, w) = raw(&b.f.sock()).await;
    let _w = send_in_background(w, big_line(1, "hello", 1024 * 1024 + 4096));
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), r.read_to_end(&mut rest))
        .await
        .expect("connection not closed")
        .ok();
    assert!(rest.is_empty());
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn line_over_16_mib_closes() {
    let b = BigFixture::new(4, Duration::from_secs(30), Duration::from_secs(10)).await;
    let (mut r, w) = b.conn().await;
    let _w = send_in_background(w, big_line(1, "len", 16 * 1024 * 1024 + 4096));
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), r.read_to_end(&mut rest))
        .await
        .expect("connection not closed")
        .ok();
    assert!(rest.is_empty());
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn fifth_concurrent_big_line_waits_for_a_permit() {
    let b = BigFixture::new(4, Duration::from_secs(30), Duration::from_secs(10)).await;
    let n = 1024 * 1024 + 4096;
    let mut holders = Vec::new();
    for i in 0..4 {
        let (r, w) = b.conn().await;
        let w = send_in_background(w, big_line(10 + i, "hold", n));
        holders.push((r, w));
    }
    b.wait_started(4).await;
    let (mut r5, w5) = b.conn().await;
    let _w5 = send_in_background(w5, big_line(99, "len", n));
    // No permit: nothing comes back yet.
    let mut l = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(400), r5.read_line(&mut l))
            .await
            .is_err()
    );
    // Holders finish, a permit frees, the fifth proceeds.
    b.open_gate.send(true).unwrap();
    let v = read_json(&mut r5).await;
    assert_eq!(v["id"], 99);
    assert_eq!(v["result"]["len"], n);
    for (mut r, _) in holders {
        assert_eq!(read_json(&mut r).await["result"]["len"], n);
    }
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn permit_wait_timeout_returns_error_and_keeps_connection() {
    // No permit will ever be free.
    let b = BigFixture::new(0, Duration::from_millis(200), Duration::from_secs(10)).await;
    let (mut r, mut w) = b.conn().await;
    let n = 2 * 1024 * 1024;
    let writer = send_in_background(w, big_line(1, "len", n));
    let v = read_json(&mut r).await;
    assert_eq!(v["id"], Value::Null);
    assert_eq!(v["error"]["data"]["kind"], "internal");
    w = writer.await.unwrap();
    // The rest of that line was discarded; the connection still works.
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    let v = read_json(&mut r).await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["pong"], true);
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn stalled_big_line_closes_the_connection() {
    let b = BigFixture::new(4, Duration::from_secs(30), Duration::from_millis(300)).await;
    let (mut r, mut w) = b.conn().await;
    // Part of a big line, then silence.
    let mut part = big_line(1, "len", 2 * 1024 * 1024);
    part.truncate(1024 * 1024 + 2048);
    w.write_all(&part).await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), r.read_to_end(&mut rest))
        .await
        .expect("connection not closed")
        .ok();
    assert!(rest.is_empty());
    // The permit came back.
    for _ in 0..100 {
        if b.permits.available_permits() == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(b.permits.available_permits(), 4);
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn backend_can_release_the_big_permit_early() {
    let b = BigFixture::new(1, Duration::from_secs(30), Duration::from_secs(10)).await;
    let n = 1024 * 1024 + 4096;
    let (mut r1, w1) = b.conn().await;
    let _w1 = send_in_background(w1, big_line(1, "release_hold", n));
    b.wait_started(1).await;
    // The first request is still running, but its permit is free again.
    let (mut r2, w2) = b.conn().await;
    let _w2 = send_in_background(w2, big_line(2, "len", n));
    let v = read_json(&mut r2).await;
    assert_eq!(v["id"], 2);
    assert_eq!(v["result"]["len"], n);
    b.open_gate.send(true).unwrap();
    assert_eq!(read_json(&mut r1).await["result"]["len"], n);
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn server_busy_null_id_error_answers_the_inflight_request() {
    // No permit ever frees up, so the server replies with a null-id "busy" error.
    let b = BigFixture::new(0, Duration::from_millis(200), Duration::from_secs(10)).await;
    let mut c = ControlClient::connect_unix(b.f.sock()).await.unwrap();
    c.hello(None, "t").await.unwrap();
    let params = json!({ "data": "a".repeat(2 * 1024 * 1024) });
    let r = tokio::time::timeout(Duration::from_secs(5), c.call("len", params))
        .await
        .expect("client waited for its own long timeout");
    match r {
        Err(ClientError::Api(e)) => assert_eq!(e.kind, ErrorKind::Internal),
        other => panic!("{other:?}"),
    }
    // Still usable.
    assert_eq!(c.call("ping", Value::Null).await.unwrap()["pong"], true);
    b.f.server.shutdown().await;
}

#[tokio::test]
async fn trickled_big_line_hits_the_total_deadline() {
    let b = BigFixture::with_total(
        4,
        Duration::from_secs(30),
        Duration::from_secs(10),
        Duration::from_millis(500),
    )
    .await;
    let (mut r, mut w) = b.conn().await;
    let mut part = big_line(1, "len", 2 * 1024 * 1024);
    part.truncate(1024 * 1024 + 2048);
    w.write_all(&part).await.unwrap();
    // One byte every 100 ms never trips the idle limit, only the total one.
    let trickle = tokio::spawn(async move {
        for _ in 0..100 {
            if w.write_all(b"a").await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), r.read_to_end(&mut rest))
        .await
        .expect("connection not closed")
        .ok();
    trickle.abort();
    for _ in 0..100 {
        if b.permits.available_permits() == 4 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(b.permits.available_permits(), 4);
    b.f.server.shutdown().await;
}

// ---- call.stream upgrade -------------------------------------------------------------------

/// Backend whose sessions echo every received frame back.
struct StreamTestBackend {
    open: std::sync::Mutex<std::collections::HashSet<String>>,
}

struct EchoSession {
    call: String,
    backend: Arc<StreamTestBackend>,
}

impl Drop for EchoSession {
    fn drop(&mut self) {
        self.backend.open.lock().unwrap().remove(&self.call);
    }
}

#[async_trait]
impl super::methods::StreamSession for EchoSession {
    async fn run(
        self: Box<Self>,
        mut rd: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        mut wr: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
        conn: CancellationToken,
    ) {
        let mut buf = [0u8; 256];
        loop {
            let n = tokio::select! {
                _ = conn.cancelled() => break,
                n = rd.read(&mut buf) => match n { Ok(n) if n > 0 => n, _ => break },
            };
            if wr.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    }
}

struct StreamBackendHandle(Arc<StreamTestBackend>);

#[async_trait]
impl ControlBackend for StreamBackendHandle {
    async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        TestBackend.call(method, params).await
    }
    fn methods(&self) -> Vec<MethodInfo> {
        TestBackend.methods()
    }
    async fn open_stream(
        &self,
        params: Value,
    ) -> Result<(Value, Box<dyn super::methods::StreamSession>), ApiError> {
        let call = params["call_id"].as_str().unwrap_or("c1").to_string();
        if params["ringing"] == true {
            return Err(ApiError::invalid_state("call is ringing"));
        }
        if !self.0.open.lock().unwrap().insert(call.clone()) {
            return Err(ApiError::invalid_state("call already has a stream"));
        }
        Ok((
            json!({"stream_id": 1, "frame_bytes": 4}),
            Box::new(EchoSession {
                call,
                backend: self.0.clone(),
            }),
        ))
    }
}

impl Fixture {
    async fn stream_fixture(tcp: bool) -> (Self, Arc<StreamTestBackend>) {
        let be = Arc::new(StreamTestBackend {
            open: Default::default(),
        });
        let log = Arc::new(EventLog::new(100));
        let ctx = ControlContext::new(
            log.clone(),
            Arc::new(StreamBackendHandle(be.clone())),
            None,
            Secret::new(TOKEN),
        );
        (Self::with_ctx(tcp, log, ctx).await, be)
    }
}

const STREAM_REQ: &[u8] =
    b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"call.stream\",\"params\":{\"call_id\":\"c1\"}}\n";

async fn expect_ping(
    r: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    w: &mut tokio::net::unix::OwnedWriteHalf,
) {
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"ping\"}\n")
        .await
        .unwrap();
    let v = read_json(r).await;
    assert_eq!(v["id"], 99);
    assert_eq!(v["result"]["pong"], true);
}

#[tokio::test]
async fn stream_upgrade_keeps_pipelined_bytes_and_closes() {
    let (f, be) = Fixture::stream_fixture(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    read_json(&mut r).await;
    // Request line and the first binary bytes in a single write.
    let mut first = STREAM_REQ.to_vec();
    first.extend_from_slice(&[0x01, 0, 0, 2, 0xAA, 0xBB]);
    w.write_all(&first).await.unwrap();
    let v = read_json(&mut r).await;
    assert_eq!(v["id"], 7);
    assert_eq!(v["result"]["stream_id"], 1);
    let mut echo = [0u8; 6];
    tokio::time::timeout(Duration::from_secs(3), r.read_exact(&mut echo))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(echo, [0x01, 0, 0, 2, 0xAA, 0xBB]);
    // Later bytes flow too, then EOF ends the session and the connection.
    w.write_all(&[0x02, 0, 0, 0]).await.unwrap();
    let mut e2 = [0u8; 4];
    r.read_exact(&mut e2).await.unwrap();
    drop(w);
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), r.read_to_end(&mut rest))
        .await
        .unwrap()
        .ok();
    assert!(rest.is_empty());
    assert!(be.open.lock().unwrap().is_empty());
    f.server.shutdown().await;
}

#[tokio::test]
async fn stream_preconditions_leave_the_connection_in_json_mode() {
    let (f, be) = Fixture::stream_fixture(false).await;
    // Request in flight.
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    read_json(&mut r).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"slow\",\"params\":{\"ms\":400}}\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    w.write_all(STREAM_REQ).await.unwrap();
    let v = read_json(&mut r).await;
    assert_eq!(v["id"], 7);
    assert_eq!(v["error"]["data"]["kind"], "invalid_state");
    assert_eq!(read_json(&mut r).await["id"], 2);
    expect_ping(&mut r, &mut w).await;

    // Active events subscription.
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"events.subscribe\"}\n")
        .await
        .unwrap();
    assert_eq!(read_json(&mut r).await["id"], 3);
    w.write_all(STREAM_REQ).await.unwrap();
    let v = read_json(&mut r).await;
    assert_eq!(v["error"]["data"]["kind"], "invalid_state");
    expect_ping(&mut r, &mut w).await;
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"events.unsubscribe\"}\n")
        .await
        .unwrap();
    assert_eq!(read_json(&mut r).await["id"], 4);

    // Backend refusals (ringing call) keep JSON mode as well.
    w.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"call.stream\",\"params\":{\"ringing\":true}}\n")
        .await
        .unwrap();
    assert_eq!(
        read_json(&mut r).await["error"]["data"]["kind"],
        "invalid_state"
    );
    expect_ping(&mut r, &mut w).await;

    // A second stream on the same call is refused while the first one lives.
    let (mut r2, mut w2) = raw(&f.sock()).await;
    w2.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"hello\"}\n")
        .await
        .unwrap();
    read_json(&mut r2).await;
    w2.write_all(STREAM_REQ).await.unwrap();
    assert_eq!(read_json(&mut r2).await["result"]["stream_id"], 1);
    w.write_all(STREAM_REQ).await.unwrap();
    assert_eq!(
        read_json(&mut r).await["error"]["data"]["kind"],
        "invalid_state"
    );
    expect_ping(&mut r, &mut w).await;
    // Closing the first frees the call.
    drop(w2);
    drop(r2);
    for _ in 0..100 {
        if be.open.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.write_all(STREAM_REQ).await.unwrap();
    assert_eq!(read_json(&mut r).await["result"]["stream_id"], 1);
    f.server.shutdown().await;
}

#[tokio::test]
async fn stream_before_hello_is_refused() {
    let (f, _be) = Fixture::stream_fixture(false).await;
    let (mut r, mut w) = raw(&f.sock()).await;
    w.write_all(STREAM_REQ).await.unwrap();
    let v = read_json(&mut r).await;
    assert_eq!(v["error"]["data"]["kind"], "unauthorized");
    f.server.shutdown().await;
}

#[tokio::test]
async fn stream_over_tcp_with_token_via_client() {
    let (f, _be) = Fixture::stream_fixture(true).await;
    let addr = f.server.endpoints().tcp.clone().unwrap();
    let mut c = ControlClient::connect_tcp(&addr).await.unwrap();
    c.hello(Some(TOKEN), "t").await.unwrap();
    let mut st = c.into_stream(json!({"call_id": "c9"})).await.unwrap();
    assert_eq!(st.info["frame_bytes"], 4);
    st.wr.write_all(&[0x01, 0, 0, 1, 0x55]).await.unwrap();
    let mut b = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(3), st.rd.read_exact(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b, [0x01, 0, 0, 1, 0x55]);
    f.server.shutdown().await;
}

#[tokio::test]
async fn shutdown_ends_a_running_stream() {
    let (f, be) = Fixture::stream_fixture(false).await;
    let mut c = ControlClient::connect_unix(f.sock()).await.unwrap();
    c.hello(None, "t").await.unwrap();
    let mut st = c.into_stream(json!({})).await.unwrap();
    f.server.shutdown().await;
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), st.rd.read_to_end(&mut rest))
        .await
        .expect("stream not closed")
        .ok();
    assert!(be.open.lock().unwrap().is_empty());
}
