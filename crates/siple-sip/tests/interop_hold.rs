//! Hold / OSRTP interop against a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"). Gated:
//! `SIPLE_INTEROP=1`. Uses users 1012 and 1013 only.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Secret, SrtpPolicy, Transport};
use siple_sip::{
    AnswerOptions, CallInfo, CallState, DialOptions, NullMediaProvider, RegState, SipEvent,
    SipStack, SipStackOptions,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn enabled() -> bool {
    std::env::var("SIPLE_INTEROP").is_ok_and(|v| v == "1")
}

fn pbx_env() -> HashMap<String, String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/interop/pbx.env");
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            l.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

/// Prints the SIP trace when `SIPLE_WIRE=1` (accounts then have `sip_trace` on).
struct PrintSink;

impl siple_sip::WireSink for PrintSink {
    fn on_wire(&self, account: &str, ev: &siple_sip::WireEvent<'_>) {
        println!(
            "---- {account} {:?} {}\n{}",
            ev.direction,
            ev.remote,
            String::from_utf8_lossy(ev.bytes)
        );
    }
}

fn wire() -> bool {
    std::env::var("SIPLE_WIRE").is_ok_and(|v| v == "1")
}

fn account(env: &HashMap<String, String>, user: &str, srtp: SrtpPolicy) -> AccountConfig {
    AccountConfig {
        id: format!("it-{user}"),
        username: user.into(),
        password: Secret::new(env["PBX_PASSWORD"].clone()),
        domain: env["PBX_DOMAIN"].clone(),
        registrar: env["PBX_HOST"].clone(),
        transport: Transport::Udp,
        srtp,
        tls_verify: false,
        expires: 120,
        keepalive_secs: 0,
        sip_trace: wire(),
        ..Default::default()
    }
}

/// Hangs up every PBX channel if the test panics, so a failed run doesn't leave calls (and
/// their RTP streams) running on the shared test PBX.
struct HangupOnPanic;

impl Drop for HangupOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = Command::new("ssh")
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=5",
                    pbx_env().get("PBX_SSH").map_or("", String::as_str),
                    "docker exec siple-fs fs_cli -x 'hupall NORMAL_CLEARING'",
                ])
                .output();
        }
    }
}

fn fs_channels() -> Option<String> {
    let out = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            pbx_env().get("PBX_SSH").map_or("", String::as_str),
            "docker exec siple-fs fs_cli -x 'show channels'",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

struct Rig {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    buf: Vec<SipEvent>,
    media: Arc<NullMediaProvider>,
}

impl Rig {
    async fn new(users: &[(&str, SrtpPolicy)]) -> Rig {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let env = pbx_env();
        let (tx, events) = unbounded_channel();
        let stack = SipStack::new(SipStackOptions {
            user_agent_default: "Siple-interop/0".into(),
            events: tx,
            wire_sink: Arc::new(PrintSink),
        });
        let media = NullMediaProvider::new();
        stack.set_media_provider(media.clone());
        let r = Rig {
            stack,
            events,
            buf: Vec::new(),
            media,
        };
        for (u, srtp) in users {
            r.stack
                .add_account(
                    account(&env, u, *srtp),
                    &GeneralConfig::default(),
                    &MediaConfig::default(),
                )
                .await
                .unwrap();
        }
        for (u, _) in users {
            r.registered(&format!("it-{u}")).await;
        }
        r
    }

    async fn until(&mut self, what: &str, pred: impl Fn(&SipEvent) -> bool) -> SipEvent {
        if let Some(i) = self.buf.iter().position(&pred) {
            return self.buf.remove(i);
        }
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let e = self.events.recv().await.expect("events closed");
                if pred(&e) {
                    return e;
                }
                self.buf.push(e);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    async fn state(&mut self, id: &str, st: CallState) -> CallInfo {
        let id = id.to_string();
        match self
            .until(
                &format!("{id} {st:?}"),
                |e| matches!(e, SipEvent::CallState { call } if call.id == id && call.state == st),
            )
            .await
        {
            SipEvent::CallState { call } => call,
            _ => unreachable!(),
        }
    }

    async fn registered(&self, acct: &str) {
        let mut rx = self.stack.watch_status(acct).unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if matches!(
                    rx.borrow_and_update().registration,
                    RegState::Registered { .. }
                ) {
                    return;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{acct} did not register"));
    }

    fn rx(&self, id: &str) -> u64 {
        self.media.stats(id).unwrap().rx_packets
    }

    /// RTP packets received in `ms`.
    async fn rx_in(&self, id: &str, ms: u64) -> u64 {
        let before = self.rx(id);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        self.rx(id) - before
    }
}

async fn check_clean() {
    tokio::time::sleep(Duration::from_secs(1)).await;
    match fs_channels() {
        Some(out) => {
            println!("fs show channels: {}", out.trim());
            assert!(out.contains("0 total"), "channels left: {out}");
        }
        None => println!("ssh to the PBX is not usable; channel check skipped"),
    }
}

/// 1012 (`srtp = optional`) -> 9999: the OSRTP offer (retried plain if refused), hold ->
/// FreeSWITCH answers recvonly and stops sending, unhold -> RTP flows again.
#[tokio::test]
async fn hold_against_freeswitch_tone() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _hangup_guard = HangupOnPanic;
    let mut r = Rig::new(&[("1012", SrtpPolicy::Optional)]).await;
    let c = r
        .stack
        .dial("it-1012", "9999", DialOptions::default())
        .await
        .unwrap();
    let active = r.state(&c, CallState::Active).await;
    println!(
        "9999 active: srtp_fallback={} negotiated={:?}",
        active.srtp_fallback, active.negotiated
    );
    let flowing = r.rx_in(&c, 1500).await;
    println!("rx before hold: {flowing} packets / 1.5 s");
    assert!(flowing > 30, "no RTP from 9999: {flowing}");

    r.stack.hold(&c).await.unwrap();
    let held = r.stack.call(&c).unwrap();
    assert_eq!(held.state, CallState::Held);
    let n = r.media.stats(&c).unwrap().negotiated.unwrap();
    println!("held negotiation: {n:?}");
    assert!(!n.recv_enabled, "FS must answer recvonly: {n:?}");
    assert!(r.media.stats(&c).unwrap().held);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let while_held = r.rx_in(&c, 1500).await;
    println!("rx while held: {while_held} packets / 1.5 s");
    assert!(while_held < 10, "FS keeps sending while held: {while_held}");

    r.stack.unhold(&c).await.unwrap();
    assert_eq!(r.stack.call(&c).unwrap().state, CallState::Active);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resumed = r.rx_in(&c, 1500).await;
    println!("rx after unhold: {resumed} packets / 1.5 s");
    assert!(resumed > 30, "RTP did not resume: {resumed}");

    r.stack.hangup(&c).await.unwrap();
    r.state(&c, CallState::Ended).await;
    r.stack.shutdown().await;
    check_clean().await;
}

/// 1012 -> 1013 through the PBX; 1013 holds 1012 (and again from 1012's side).
#[tokio::test]
async fn pbx_relays_hold_between_two_of_ours() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _hangup_guard = HangupOnPanic;
    let mut r = Rig::new(&[("1012", SrtpPolicy::Off), ("1013", SrtpPolicy::Off)]).await;
    let c12 = r
        .stack
        .dial("it-1012", "1013", DialOptions::default())
        .await
        .unwrap();
    let inc = match r
        .until(
            "incoming at 1013",
            |e| matches!(e, SipEvent::CallIncoming { call } if call.account == "it-1013"),
        )
        .await
    {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    };
    let c13 = inc.id.clone();
    r.stack
        .answer(&c13, AnswerOptions::default())
        .await
        .unwrap();
    r.state(&c12, CallState::Active).await;
    r.state(&c13, CallState::Active).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    r.stack.hold(&c13).await.unwrap();
    println!("1013 after hold: {:?}", r.stack.call(&c13).unwrap().state);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let i12 = r.stack.call(&c12).unwrap();
    println!(
        "1012 after 1013 held it: state={:?} hold={:?} negotiated={:?}",
        i12.state,
        i12.hold,
        r.media.stats(&c12).unwrap().negotiated
    );
    let moh = r.rx_in(&c12, 1500).await;
    println!("1012 rx while held (FS MOH?): {moh} packets / 1.5 s");

    r.stack.unhold(&c13).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!(
        "after unhold: 1012={:?} 1013={:?}",
        r.stack.call(&c12).unwrap().state,
        r.stack.call(&c13).unwrap().state
    );

    r.stack.hangup(&c12).await.unwrap();
    r.state(&c12, CallState::Ended).await;
    r.state(&c13, CallState::Ended).await;
    r.stack.shutdown().await;
    check_clean().await;
}
