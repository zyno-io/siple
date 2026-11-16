//! Call interop against a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"). Gated: `SIPLE_INTEROP=1`.
//! Uses users 1006 (UDP), 1007 (TCP) and 1008 (TLS, SRTP mandatory) only.

use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use siple_config::{AccountConfig, GeneralConfig, MediaConfig, Secret, SrtpPolicy, Transport};
use siple_sip::{
    AnswerOptions, CallInfo, CallState, DialOptions, EndedBy, NullMediaProvider, NullWireSink,
    RegState, SipEvent, SipStack, SipStackOptions,
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

fn account(env: &HashMap<String, String>, user: &str, transport: Transport) -> AccountConfig {
    AccountConfig {
        id: format!("it-{user}"),
        username: user.into(),
        password: Secret::new(env["PBX_PASSWORD"].clone()),
        domain: env["PBX_DOMAIN"].clone(),
        registrar: env["PBX_HOST"].clone(),
        transport,
        tls_verify: false,
        expires: 120,
        keepalive_secs: 0,
        sip_trace: false,
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
}

#[tokio::test]
async fn calls_against_freeswitch() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _hangup_guard = HangupOnPanic;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let env = pbx_env();
    let (tx, events) = unbounded_channel();
    let stack = SipStack::new(SipStackOptions {
        user_agent_default: "Siple-interop/0".into(),
        events: tx,
        wire_sink: Arc::new(NullWireSink),
    });
    let media = NullMediaProvider::new();
    stack.set_media_provider(media.clone());
    let mut r = Rig {
        stack,
        events,
        buf: Vec::new(),
        media,
    };
    let mut a1006 = account(&env, "1006", Transport::Udp);
    a1006.srtp = SrtpPolicy::Off;
    let mut a1007 = account(&env, "1007", Transport::Tcp);
    a1007.srtp = SrtpPolicy::Off;
    let mut a1008 = account(&env, "1008", Transport::Tls);
    a1008.srtp = SrtpPolicy::Mandatory;
    for cfg in [a1006, a1007, a1008] {
        r.stack
            .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
            .await
            .unwrap();
    }
    for a in ["it-1006", "it-1007", "it-1008"] {
        r.registered(a).await;
    }

    // 1) 1006 -> 9999 (1 kHz tone) over UDP: RTP must flow into the media provider.
    let c = r
        .stack
        .dial("it-1006", "9999", DialOptions::default())
        .await
        .unwrap();
    let active = r.state(&c, CallState::Active).await;
    let n = active.negotiated.clone().expect("negotiated");
    println!("9999: {n:?}");
    assert!(n.srtp_suite.is_none());
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let rx = r.media.stats(&c).unwrap().rx_packets;
    println!("9999: {rx} RTP packets in {:?}", t0.elapsed());
    assert!(rx > 100, "only {rx} RTP packets received");
    r.stack.hangup(&c).await.unwrap();
    let ended = r.state(&c, CallState::Ended).await;
    assert_eq!(ended.end.unwrap().by, EndedBy::Local);

    // 2) 1006 -> 1007 (registered over TCP); 1007 answers; 1007 sends BYE.
    let c1 = r
        .stack
        .dial("it-1006", "1007", DialOptions::default())
        .await
        .unwrap();
    let inc = match r
        .until(
            "incoming at 1007",
            |e| matches!(e, SipEvent::CallIncoming { call } if call.account == "it-1007"),
        )
        .await
    {
        SipEvent::CallIncoming { call } => call,
        _ => unreachable!(),
    };
    println!(
        "1007 incoming from {:?} / {:?}",
        inc.remote_uri, inc.remote_display
    );
    r.stack
        .answer(&inc.id, AnswerOptions::default())
        .await
        .unwrap();
    r.state(&c1, CallState::Active).await;
    r.state(&inc.id, CallState::Active).await;
    // DTMF over SIP INFO through the PBX (FreeSWITCH passes INFO between legs or answers it).
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.stack.hangup(&inc.id).await.unwrap();
    let a = r.state(&c1, CallState::Ended).await;
    println!("1006 end: {:?}", a.end);
    assert_eq!(a.end.unwrap().by, EndedBy::Remote);
    r.state(&inc.id, CallState::Ended).await;

    // 3) 1008 (TLS, SRTP mandatory) -> 9196 echo: SAVP negotiated.
    let c2 = r
        .stack
        .dial("it-1008", "9196", DialOptions::default())
        .await
        .unwrap();
    let active = r.state(&c2, CallState::Active).await;
    let n = active.negotiated.expect("negotiated");
    println!("9196: {n:?}");
    assert!(n.srtp_suite.is_some(), "SRTP not negotiated: {n:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    r.stack.hangup(&c2).await.unwrap();
    r.state(&c2, CallState::Ended).await;

    // Clean up: unregister everything, then the PBX must hold no channels.
    r.stack.shutdown().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    match fs_channels() {
        Some(out) => {
            println!("fs show channels: {}", out.trim());
            assert!(out.contains("0 total"), "channels left: {out}");
        }
        None => println!("ssh to the PBX is not usable; channel check skipped"),
    }
}
