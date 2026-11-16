//! sipp edge-case scenarios against a local Siple stack on 127.0.0.1.
//!
//! Gated: runs only with `SIPLE_SIPP=1` and `/opt/homebrew/bin/sipp` present, otherwise every
//! test returns at once. Scenarios live in `tests/sipp/*.xml` at the repo root; sipp's exit code
//! asserts what Siple put on the wire (checks and `abortunexp` inside the scenarios), the
//! test bodies assert the `CallInfo` / events.
//!
//! ```sh
//! SIPLE_SIPP=1 cargo test -p siple-sip --test sipp -- --test-threads=1
//! ```

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use siple_config::AccountConfig;
use siple_sip::{
    AnswerOptions, CallState, EndedBy, NullMediaProvider, SipEvent, SipStack, SipStackOptions,
    WireEvent, WireSink,
};
use tokio::sync::mpsc::unbounded_channel;

/// The sipp binary: `SIPLE_SIPP_BIN` if set, else `sipp` found on `PATH`.
fn sipp_bin() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("SIPLE_SIPP_BIN").filter(|p| !p.is_empty()) {
        let p = std::path::PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(if cfg!(windows) { "sipp.exe" } else { "sipp" }))
        .find(|p| p.is_file())
}

fn enabled() -> bool {
    let on = std::env::var("SIPLE_SIPP").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("sipp tests skipped (set SIPLE_SIPP=1)");
        return false;
    }
    let have = sipp_bin().is_some();
    if !have {
        eprintln!(
            "sipp tests skipped: sipp not found (install it, or set SIPLE_SIPP_BIN to its path)"
        );
    }
    have
}

macro_rules! gate {
    () => {
        if !enabled() {
            return;
        }
    };
}

fn scenario(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/sipp")
        .join(format!("{name}.xml"))
}

/// A running sipp process with its working directory (message / error logs).
struct Sipp {
    child: Child,
    dir: tempfile::TempDir,
    name: String,
}

impl Sipp {
    fn spawn(name: &str, args: &[String]) -> Sipp {
        let dir = tempfile::tempdir().unwrap();
        let child = Command::new(sipp_bin().expect("sipp checked by enabled()"))
            .current_dir(dir.path())
            .args(["-sf", scenario(name).to_str().unwrap()])
            .args(["-i", "127.0.0.1", "-m", "1", "-nostdin"])
            .args([
                "-timeout",
                "60s",
                "-timeout_error",
                "-recv_timeout",
                "10000",
            ])
            .args([
                "-trace_err",
                "-trace_msg",
                "-default_behaviors",
                "abortunexp",
            ])
            .args(["-s", "b", "-t", "u1"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start sipp");
        Sipp {
            child,
            dir,
            name: name.into(),
        }
    }

    /// sipp as the caller of `target` (`ip:port` of the stack account).
    fn uac(name: &str, target: &str) -> Sipp {
        Sipp::spawn(name, &[target.to_string()])
    }

    /// [`Sipp::uac`] without sipp's UDP retransmission handling (`-nr`), for scenarios that
    /// receive several requests of the same method (sipp would take the second for a
    /// retransmission of the first).
    fn uac_nr(name: &str, target: &str) -> Sipp {
        Sipp::spawn(name, &[target.to_string(), "-nr".to_string()])
    }

    /// sipp as the callee, listening on a free port.
    fn uas(name: &str) -> (Sipp, u16) {
        let port = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap().port()
        };
        let s = Sipp::spawn(name, &["-p".into(), port.to_string()]);
        (s, port)
    }

    fn logs(dir: &std::path::Path) -> String {
        let mut logs = String::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "log") {
                logs.push_str(&format!(
                    "\n--- {} ---\n{}",
                    p.file_name().unwrap().to_string_lossy(),
                    std::fs::read_to_string(&p).unwrap_or_default()
                ));
            }
        }
        logs
    }

    /// Waits for sipp to finish; panics with its logs if the scenario failed.
    async fn finish(mut self) {
        let mut child = std::mem::replace(
            &mut self.child,
            Command::new("true").spawn().expect("placeholder"),
        );
        let name = self.name.clone();
        let dir = self.dir.path().to_path_buf();
        let res = tokio::time::timeout(
            Duration::from_secs(70),
            tokio::task::spawn_blocking(move || {
                let st = child.wait().unwrap();
                let mut out = String::new();
                let mut err = String::new();
                use std::io::Read;
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_string(&mut out);
                }
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                (st, out, err)
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("sipp scenario {name} timed out"))
        .unwrap();
        let (st, out, err) = res;
        if !st.success() {
            let logs = Sipp::logs(&dir);
            panic!("sipp scenario {name} failed ({st}):\n{out}\n{err}\n{logs}");
        }
    }
}

impl Drop for Sipp {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "sipp scenario {} (test failed first):{}",
                self.name,
                Sipp::logs(self.dir.path())
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn b_cfg() -> AccountConfig {
    let mut c = peer("b");
    c.min_se = 1;
    c.session_expires = 4;
    c
}

/// A rig that also records the wire (our side) per account.
#[derive(Default)]
struct Capture(Mutex<Vec<String>>);

impl WireSink for Capture {
    fn on_wire(&self, _account: &str, ev: &WireEvent<'_>) {
        self.0
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(ev.bytes).into_owned());
    }
}

fn rig_with_wire() -> (Rig, Arc<Capture>) {
    let (tx, rx) = unbounded_channel();
    let wire = Arc::new(Capture::default());
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-test/1".into(),
        events: tx,
        wire_sink: wire.clone(),
    });
    let media = NullMediaProvider::new();
    stack.set_media_provider(media.clone());
    (
        Rig {
            stack,
            events: rx,
            buf: Vec::new(),
            media,
        },
        wire,
    )
}

/// Rig with `b` listening; returns its address.
async fn b_rig() -> (Rig, String) {
    let r = rig();
    let b = add(&r, b_cfg()).await;
    (r, b)
}

/// Answers the next incoming call at once and waits until it is active.
async fn answer_active(r: &mut Rig) -> String {
    let call = incoming(r).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    state_of(r, &call.id, CallState::Active).await;
    call.id
}

#[tokio::test]
async fn sipp_100rel_uac_requires_prack() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_100rel", &b);
    // Answer at once: the 200 must still wait for sipp's PRACK (1.8 s later).
    let id = answer_active(&mut r).await;
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().by, EndedBy::Remote);
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_early_update_changes_the_pending_answer() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_early_update", &b);
    let call = incoming(&mut r).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    r.stack
        .answer(&call.id, AnswerOptions::default())
        .await
        .unwrap();
    let active = state_of(&mut r, &call.id, CallState::Active).await;
    assert_eq!(
        active.negotiated.as_ref().map(|n| n.codec.as_str()),
        Some("pcma"),
        "{active:?}"
    );
    state_of(&mut r, &call.id, CallState::Ended).await;
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_glare_our_reinvite_crossed_gets_491_then_hold_completes() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_glare_491_from_us", &b);
    let id = answer_active(&mut r).await;
    r.stack.hold(&id).await.expect("hold completes");
    assert!(r.stack.call(&id).unwrap().hold.local);
    state_of(&mut r, &id, CallState::Ended).await;
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_glare_peer_answers_491_we_retry() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_glare_491_from_peer", &b);
    let id = answer_active(&mut r).await;
    r.stack
        .hold(&id)
        .await
        .expect("hold completes after the retry");
    assert!(r.stack.call(&id).unwrap().hold.local);
    state_of(&mut r, &id, CallState::Ended).await;
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_unsolicited_notify_message_summary() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_notify_mwi", &b);
    let ev = until(&mut r, "mwi", |e| matches!(e, SipEvent::Mwi { .. })).await;
    let SipEvent::Mwi { account, status } = ev else {
        unreachable!()
    };
    assert_eq!(account, "b");
    assert!(status.waiting);
    assert_eq!((status.voice.new, status.voice.old), (2, 8));
    assert_eq!((status.voice.urgent_new, status.voice.urgent_old), (1, 2));
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_invite_without_sdp() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_invite_no_sdp", &b);
    let id = answer_active(&mut r).await;
    // The answer arrives in the ACK: media gets negotiated then.
    let media = until(
        &mut r,
        "media",
        |e| matches!(e, SipEvent::CallMedia { call_id, .. } if *call_id == id),
    )
    .await;
    let SipEvent::CallMedia { negotiated, .. } = media else {
        unreachable!()
    };
    assert_eq!(negotiated.codec, "pcmu");
    state_of(&mut r, &id, CallState::Ended).await;
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_reinvite_without_sdp_keeps_the_sdp_version() {
    gate!();
    let (mut r, wire) = rig_with_wire();
    let mut cfg = b_cfg();
    cfg.sip_trace = true;
    let b = add(&r, cfg).await;
    let s = Sipp::uac("uac_reinvite_no_sdp", &b);
    let id = answer_active(&mut r).await;
    state_of(&mut r, &id, CallState::Ended).await;
    s.finish().await;
    let sent = wire.0.lock().unwrap().clone();
    let o_lines: Vec<String> = sent
        .iter()
        .filter(|m| m.starts_with("SIP/2.0 200") && m.contains("CSeq:") && m.contains("INVITE"))
        .filter_map(|m| m.lines().find(|l| l.starts_with("o=")).map(String::from))
        .collect();
    assert_eq!(o_lines.len(), 2, "both 200s with SDP captured: {sent:#?}");
    assert_eq!(o_lines[0], o_lines[1], "o= changed between the 200s");
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_bye_with_q850_reason() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_bye_q850", &b);
    let id = answer_active(&mut r).await;
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    let end = ended.end.unwrap();
    assert_eq!(end.by, EndedBy::Remote);
    assert_eq!(end.q850_cause, Some(16), "{end:?}");
    assert_eq!(end.code, 200, "a Q.850 cause is not a SIP status: {end:?}");
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_cancel_with_reason_sip_cause_200_is_not_missed() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_cancel_reason", &b);
    let call = incoming(&mut r).await;
    let ended = state_of(&mut r, &call.id, CallState::Ended).await;
    let end = ended.end.unwrap();
    assert_eq!(end.code, 487);
    assert!(
        !end.missed,
        "answered elsewhere is not a missed call: {end:?}"
    );
    assert_eq!(end.reason, "Call completed elsewhere");
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_session_timer_refreshes_from_sipp_keep_the_call_up() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac("uac_session_timer_uac", &b);
    let id = answer_active(&mut r).await;
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    let end = ended.end.unwrap();
    assert_eq!(
        end.by,
        EndedBy::Remote,
        "ended by sipp's BYE, not a timeout: {end:?}"
    );
    assert_eq!(end.code, 200);
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_session_timer_we_refresh_as_refresher() {
    gate!();
    let (mut r, b) = b_rig().await;
    let s = Sipp::uac_nr("uac_session_timer_uas", &b);
    let id = answer_active(&mut r).await;
    let ended = state_of(&mut r, &id, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().by, EndedBy::Remote);
    s.finish().await;
    r.stack.shutdown().await;
}

#[tokio::test]
async fn sipp_forked_early_dialogs_second_tag_wins() {
    gate!();
    let mut r = rig();
    add(&r, peer("a")).await;
    let (s, port) = Sipp::uas("uas_fork");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let id = r
        .stack
        .dial("a", &format!("sip:b@127.0.0.1:{port}"), Default::default())
        .await
        .unwrap();
    state_of(&mut r, &id, CallState::Active).await;
    r.stack.hangup(&id).await.unwrap();
    state_of(&mut r, &id, CallState::Ended).await;
    s.finish().await;
    r.stack.shutdown().await;
}
