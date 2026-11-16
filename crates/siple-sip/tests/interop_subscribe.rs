//! BLF / MWI interop against a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"). Gated:
//! `SIPLE_INTEROP=1`; the voicemail part (it leaves a message in box 1017) additionally needs
//! `SIPLE_INTEROP_VM=1`. Uses users 1017 (watcher), 1018 and 1019 only; prints every state
//! transition it sees so the run doubles as an observation log.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use siple_config::{
    AccountConfig, BlfConfig, Codec, GeneralConfig, MediaConfig, MwiMode, Secret, SrtpPolicy,
    Transport,
};
use siple_media::sdp_neg::Negotiated;
use siple_sip::{
    AnswerOptions, BlfState, CallMedia, CallMediaRequest, CallState, DialOptions, MediaProvider,
    NullWireSink, RegState, SipEvent, SipStack, SipStackOptions,
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn enabled() -> bool {
    std::env::var("SIPLE_INTEROP").is_ok_and(|v| v == "1")
}

fn vm_enabled() -> bool {
    std::env::var("SIPLE_INTEROP_VM").is_ok_and(|v| v == "1")
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

fn account(env: &HashMap<String, String>, user: &str) -> AccountConfig {
    AccountConfig {
        id: format!("it-{user}"),
        username: user.into(),
        password: Secret::new(env["PBX_PASSWORD"].clone()),
        domain: env["PBX_DOMAIN"].clone(),
        registrar: env["PBX_HOST"].clone(),
        transport: Transport::Udp,
        srtp: SrtpPolicy::Off,
        codecs: vec![Codec::Pcmu],
        expires: 120,
        keepalive_secs: 0,
        sip_trace: false,
        mwi: MwiMode::Off,
        ..Default::default()
    }
}

// --- a media provider that sends a 1 kHz PCMU tone (so FreeSWITCH has something to record) -------

fn ulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    let mut s = i32::from(sample);
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    let s = s.min(32635) + BIAS;
    let mut exp = 7;
    let mut mask = 0x4000;
    while exp > 0 && s & mask == 0 {
        exp -= 1;
        mask >>= 1;
    }
    let mant = (s >> (exp + 3)) & 0x0f;
    !((sign | (exp << 4) | mant) as u8)
}

struct ToneProvider {
    handle: tokio::runtime::Handle,
}

struct ToneMedia {
    sock: Arc<UdpSocket>,
    port: u16,
    handle: tokio::runtime::Handle,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    held: Arc<std::sync::atomic::AtomicBool>,
}

impl MediaProvider for ToneProvider {
    fn create(&self, req: CallMediaRequest) -> anyhow::Result<Arc<dyn CallMedia>> {
        let std_sock = std::net::UdpSocket::bind(SocketAddr::new(req.bind_ip, 0))?;
        std_sock.set_nonblocking(true)?;
        let port = std_sock.local_addr()?.port();
        let _g = self.handle.enter();
        Ok(Arc::new(ToneMedia {
            sock: Arc::new(UdpSocket::from_std(std_sock)?),
            port,
            handle: self.handle.clone(),
            task: Mutex::new(None),
            held: Arc::default(),
        }))
    }
}

impl CallMedia for ToneMedia {
    fn local_rtp_port(&self) -> u16 {
        self.port
    }
    fn apply(&self, n: &Negotiated) {
        let mut slot = self.task.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_some() || n.remote_hold {
            return;
        }
        let (sock, dest, pt, held) = (self.sock.clone(), n.remote_rtp, n.pt, self.held.clone());
        *slot = Some(self.handle.spawn(async move {
            let (mut seq, mut ts) = (0u16, 0u32);
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            loop {
                tick.tick().await;
                if held.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let mut p = vec![0x80, pt, (seq >> 8) as u8, seq as u8];
                p.extend_from_slice(&ts.to_be_bytes());
                p.extend_from_slice(&0x5a5a_0001u32.to_be_bytes());
                for i in 0..160u32 {
                    let t = f64::from(ts + i) / 8000.0;
                    p.push(ulaw(
                        (8000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16,
                    ));
                }
                let _ = sock.send_to(&p, dest).await;
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(160);
            }
        }));
    }
    fn set_hold(&self, h: bool) {
        self.held.store(h, std::sync::atomic::Ordering::Relaxed);
    }
    fn set_mute(&self, _: bool) {}
    fn send_dtmf_rfc4733(&self, _: &str, _: u32, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_dtmf_inband(&self, _: &str, _: u32, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_focus(&self) {}
    fn close(&self) {
        if let Some(t) = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            t.abort();
        }
    }
}

// --- rig ---------------------------------------------------------------------------------------

struct Rig {
    stack: SipStack,
    events: UnboundedReceiver<SipEvent>,
    buf: Vec<SipEvent>,
    t0: Instant,
}

impl Rig {
    fn note(&self, e: &SipEvent) {
        let at = self.t0.elapsed().as_secs_f32();
        match e {
            SipEvent::BlfState {
                account,
                uri,
                state,
                dialogs,
                ..
            } => println!(
                "[{at:6.2}s] BLF {account} {uri}: {} dialogs={:?}",
                state.name(),
                dialogs
                    .iter()
                    .map(|d| format!(
                        "{:?}/{:?}/{}",
                        d.state,
                        d.direction,
                        d.remote_identity.as_deref().unwrap_or("-")
                    ))
                    .collect::<Vec<_>>()
            ),
            SipEvent::Mwi { account, status } => println!(
                "[{at:6.2}s] MWI {account}: waiting={} voice={:?} uri={:?}",
                status.waiting, status.voice, status.account_uri
            ),
            SipEvent::SubscriptionState {
                account,
                uri,
                event,
                state,
                ..
            } => println!("[{at:6.2}s] SUB {account} {event} {uri}: {state:?}"),
            SipEvent::CallState { call } => println!(
                "[{at:6.2}s] CALL {} {} {:?} remote={}",
                call.account, call.id, call.state, call.remote_uri
            ),
            _ => {}
        }
    }

    async fn until_for(
        &mut self,
        what: &str,
        secs: u64,
        pred: impl Fn(&SipEvent) -> bool,
    ) -> SipEvent {
        if let Some(i) = self.buf.iter().position(&pred) {
            return self.buf.remove(i);
        }
        tokio::time::timeout(Duration::from_secs(secs), async {
            loop {
                let e = self.events.recv().await.expect("events closed");
                self.note(&e);
                if pred(&e) {
                    return e;
                }
                self.buf.push(e);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
    }

    async fn until(&mut self, what: &str, pred: impl Fn(&SipEvent) -> bool) -> SipEvent {
        self.until_for(what, 20, pred).await
    }

    /// Drops buffered BLF events so the next wait only sees fresh ones.
    fn forget_blf(&mut self) {
        self.buf.retain(|e| !matches!(e, SipEvent::BlfState { .. }));
    }

    async fn blf(&mut self, uri: &str, st: BlfState) {
        let uri = uri.to_string();
        // The status may already be there (event consumed while waiting for something else).
        if self
            .stack
            .blf_status(None)
            .iter()
            .any(|b| b.uri == uri && b.state == st)
        {
            return;
        }
        let u = uri.clone();
        self.until(
            &format!("BLF {uri} {}", st.name()),
            move |e| matches!(e, SipEvent::BlfState { uri, state, .. } if *uri == u && *state == st),
        )
        .await;
    }

    async fn call_state(&mut self, id: &str, st: CallState) {
        let id = id.to_string();
        self.until_for(
            &format!("{id} {st:?}"),
            45,
            move |e| matches!(e, SipEvent::CallState { call } if call.id == id && call.state == st),
        )
        .await;
    }

    async fn incoming(&mut self, account: &str) -> String {
        let account = account.to_string();
        match self
            .until_for(
                "incoming call",
                45,
                move |e| matches!(e, SipEvent::CallIncoming { call } if call.account == account),
            )
            .await
        {
            SipEvent::CallIncoming { call } => call.id,
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

#[tokio::test(flavor = "multi_thread")]
async fn blf_and_mwi_against_freeswitch() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
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
    stack.set_media_provider(Arc::new(ToneProvider {
        handle: tokio::runtime::Handle::current(),
    }));
    let mut r = Rig {
        stack,
        events,
        buf: Vec::new(),
        t0: Instant::now(),
    };

    // 1017 watches 1018 and 1019 and its own mailbox; 1018 and 1019 are plain phones.
    let mut a1017 = account(&env, "1017");
    a1017.mwi = MwiMode::Subscribe;
    a1017.blf = vec![
        BlfConfig {
            uri: "1018".into(),
            label: "Eighteen".into(),
            ..Default::default()
        },
        BlfConfig {
            uri: "1019".into(),
            label: "Nineteen".into(),
            ..Default::default()
        },
    ];
    for cfg in [a1017, account(&env, "1018"), account(&env, "1019")] {
        r.stack
            .add_account(cfg, &GeneralConfig::default(), &MediaConfig::default())
            .await
            .unwrap();
    }
    for a in ["it-1017", "it-1018", "it-1019"] {
        r.registered(a).await;
    }

    // 0) Initial state: both lamps idle, mailbox known.
    println!("== initial");
    r.blf("1018", BlfState::Idle).await;
    r.blf("1019", BlfState::Idle).await;
    r.until("initial MWI", |e| matches!(e, SipEvent::Mwi { .. }))
        .await;
    let mwi0 = r.stack.mwi_status("it-1017").expect("mwi status");
    println!("initial mailbox: {mwi0:?}");
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(10)
        && !r
            .stack
            .subscriptions(Some("it-1017"))
            .iter()
            .all(|s| matches!(s.state, siple_sip::SubState::Active))
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("all subscriptions active after {:?}", t.elapsed());
    for s in r.stack.subscriptions(Some("it-1017")) {
        println!("subscription {} {} {} {:?}", s.id, s.event, s.uri, s.state);
        assert!(matches!(s.state, siple_sip::SubState::Active), "{s:?}");
    }
    let st = r.stack.account_status("it-1017").unwrap();
    assert!(st.mwi.is_some(), "AccountStatus.mwi");

    // 1) 1018 -> 9999 (tone): 1017 sees 1018 busy (FS reports `confirmed` for the caller), then
    //    idle after the hang-up.
    println!("== 1018 calls 9999");
    r.forget_blf();
    let c = r
        .stack
        .dial("it-1018", "9999", DialOptions::default())
        .await
        .unwrap();
    r.call_state(&c, CallState::Active).await;
    r.blf("1018", BlfState::Busy).await;
    let b = r.stack.blf_status(Some("it-1017"));
    let b18 = b.iter().find(|b| b.uri == "1018").unwrap();
    println!("1018 dialogs while calling 9999: {:?}", b18.dialogs);
    assert_eq!(b18.label, "Eighteen");
    tokio::time::sleep(Duration::from_secs(1)).await;
    r.stack.hangup(&c).await.unwrap();
    r.call_state(&c, CallState::Ended).await;
    r.blf("1018", BlfState::Idle).await;

    // 2) 1019 -> 1018, unanswered: 1018 rings (early/recipient), 1019 is the caller.
    println!("== 1019 calls 1018, ringing");
    r.forget_blf();
    let c = r
        .stack
        .dial("it-1019", "1018", DialOptions::default())
        .await
        .unwrap();
    let inc = r.incoming("it-1018").await;
    r.blf("1018", BlfState::Ringing).await;
    let b = r.stack.blf_status(None);
    println!(
        "1018 dialogs while ringing: {:?}\n1019 state while calling: {:?}",
        b.iter().find(|b| b.uri == "1018").unwrap().dialogs,
        b.iter().find(|b| b.uri == "1019").map(|b| b.state)
    );
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 3) 1018 answers -> busy; hold -> observe (`held` needs +sip.rendering from the PBX).
    println!("== 1018 answers, holds");
    r.stack
        .answer(&inc, AnswerOptions::default())
        .await
        .unwrap();
    r.call_state(&c, CallState::Active).await;
    r.blf("1018", BlfState::Busy).await;
    r.stack.hold(&inc).await.unwrap();
    r.blf("1018", BlfState::Held).await;
    let held = r
        .stack
        .blf_status(None)
        .into_iter()
        .find(|b| b.uri == "1018")
        .unwrap();
    println!(
        "1018 while holding: {} dialogs={:?}",
        held.state.name(),
        held.dialogs
    );
    r.stack.unhold(&inc).await.unwrap();
    r.blf("1018", BlfState::Busy).await;
    r.stack.hangup(&c).await.unwrap();
    r.call_state(&c, CallState::Ended).await;
    r.blf("1018", BlfState::Idle).await;
    r.blf("1019", BlfState::Idle).await;

    // 4) Voicemail for 1017: 1019 calls 1017, 1017 declines (FS' stock dialplan continues to
    //    voicemail on any bridge failure), 1019 stays on the line with a tone.
    if vm_enabled() {
        println!("== voicemail for 1017");
        let before = r.stack.mwi_status("it-1017").unwrap();
        let c = r
            .stack
            .dial("it-1019", "1017", DialOptions::default())
            .await
            .unwrap();
        let inc = r.incoming("it-1017").await;
        r.stack
            .reject(&inc, Some(486), Some("Busy Here".into()))
            .await
            .unwrap();
        r.call_state(&c, CallState::Active).await; // voicemail answers
        println!("voicemail answered; recording a tone for 20 s");
        tokio::time::sleep(Duration::from_secs(20)).await;
        r.stack.hangup(&c).await.unwrap();
        r.call_state(&c, CallState::Ended).await;
        let ev = r
            .until_for(
                "MWI after voicemail",
                30,
                |e| matches!(e, SipEvent::Mwi { status, .. } if status.waiting),
            )
            .await;
        let SipEvent::Mwi { status, .. } = ev else {
            unreachable!()
        };
        println!(
            "mailbox before: {:?}\nmailbox after:  {:?}",
            before.voice, status.voice
        );
        assert!(status.waiting);
        assert!(status.voice.new >= 1, "{status:?}");
        assert!(status.voice.new + status.voice.old > before.voice.new + before.voice.old);
        let st = r.stack.account_status("it-1017").unwrap();
        println!("AccountStatus.mwi = {:?}", st.mwi);
    } else {
        println!("voicemail part skipped (set SIPLE_INTEROP_VM=1)");
    }

    // 5) Shutdown unsubscribes (`Expires: 0`) and unregisters.
    println!("== shutdown");
    r.stack.shutdown().await;
}
