//! `siple-cli events` must fail (non-zero exit, message on stderr) when the server goes
//! away, instead of ending quietly like a normal end of stream.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use siple_core::config::Secret;
use siple_core::control::{CancellationToken, ControlContext, ControlServer, StubBackend};
use siple_core::events::EventLog;
use siple_core::instance::ControlEndpoints;

#[tokio::test(flavor = "multi_thread")]
async fn events_exits_nonzero_when_the_server_closes() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("c.sock");
    let log = Arc::new(EventLog::new(10));
    let ctx = ControlContext::new(log.clone(), Arc::new(StubBackend), None, Secret::new("tok"));
    let req = ControlEndpoints {
        unix_socket: Some(sock.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let server = ControlServer::start(&req, ctx, CancellationToken::new())
        .await
        .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_siple-cli"))
        .args(["events", "--socket"])
        .arg(&sock)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Collect stdout on a thread so the test can poll it.
    let out = Arc::new(Mutex::new(String::new()));
    let reader = {
        let out = out.clone();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let mut o = out.lock().unwrap();
                o.push_str(&line);
                o.push('\n');
            }
        })
    };
    // Process start-up can take a while (first launch of a fresh binary is scanned by the OS),
    // so emit events until one reaches stdout: that proves the subscription is active.
    let deadline = Instant::now() + Duration::from_secs(15);
    while !out.lock().unwrap().contains("test.event") {
        assert!(
            Instant::now() < deadline,
            "events never printed an event; stdout: {:?}",
            out.lock().unwrap()
        );
        log.emit("test.event", None, None, serde_json::Value::Null);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    server.shutdown().await;

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "events did not exit after the server closed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    reader.join().unwrap();
    let out = out.lock().unwrap().clone();
    assert!(out.contains("test.event"), "stdout: {out:?}");
    assert!(!status.success(), "exit status {status}, stderr: {err}");
    assert!(err.contains("closed"), "stderr: {err:?}");
}
