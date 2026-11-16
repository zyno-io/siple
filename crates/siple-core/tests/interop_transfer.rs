//! Transfers through a FreeSWITCH/Asterisk test PBX (see CONTRIBUTING.md, "Interop tests"), via the control API.
//! Gated: `SIPLE_INTEROP=1`. Uses users 1014, 1015 and 1016 only. FreeSWITCH is a B2BUA:
//! a REFER on the 1014 leg is handled by FreeSWITCH, which moves the other leg.
#![cfg(unix)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

fn account(env: &HashMap<String, String>, user: &str, source: &str, extra: &str) -> String {
    format!(
        "[[account]]\nid = \"u{user}\"\nusername = \"{user}\"\npassword = \"{}\"\n\
         domain = \"{}\"\nregistrar = \"{}\"\ntransport = \"udp\"\nsrtp = \"off\"\n\
         expires = 120\nkeepalive_secs = 0\naudio_source = \"{source}\"\naudio_sink = \"null\"\n{extra}\n",
        env["PBX_PASSWORD"], env["PBX_DOMAIN"], env["PBX_HOST"]
    )
}

struct Rig {
    _dir: tempfile::TempDir,
    app: App,
    c: ControlClient,
}

/// `SIPLE_INTEROP_RTP_RANGE=20000-20999` keeps our RTP ports clear of FreeSWITCH's
/// 16384-16683 when both share a host network.
fn media_prefix() -> String {
    let Ok(r) = std::env::var("SIPLE_INTEROP_RTP_RANGE") else {
        return String::new();
    };
    let (a, b) = r
        .split_once('-')
        .expect("SIPLE_INTEROP_RTP_RANGE=START-END");
    format!("[media]\nrtp_port_range = [{}, {}]\n\n", a.trim(), b.trim())
}

async fn rig(accounts: &str, ids: &[&str]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, format!("{}{accounts}", media_prefix())).unwrap();
    let mut o = AppOptions::new(&path);
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    let app = App::start(o).await.unwrap();
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "interop")
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let l = c.call("account.list", Value::Null).await.unwrap();
        let reg = |id: &str| {
            l["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["id"] == id && a["registration"]["state"] == "registered")
        };
        if ids.iter().all(|i| reg(i)) {
            break;
        }
        assert!(Instant::now() < deadline, "registration timed out: {l}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Rig { _dir: dir, app, c }
}

impl Rig {
    async fn call(&mut self, m: &str, p: Value) -> Value {
        self.c
            .call(m, p)
            .await
            .unwrap_or_else(|e| panic!("{m}: {e}"))
    }

    async fn wait(&mut self, id: &str, state: &str) -> Value {
        self.call(
            "call.wait_for",
            json!({"call_id": id, "state": state, "timeout_ms": 20_000}),
        )
        .await
    }

    async fn hears(&mut self, id: &str, hz: f64) -> bool {
        self.call(
            "call.detect_tone",
            json!({"call_id": id, "freq_hz": hz, "timeout_ms": 8000}),
        )
        .await["detected"]
            == true
    }

    async fn seq(&mut self) -> u64 {
        self.call("events.current_seq", Value::Null).await["seq"]
            .as_u64()
            .unwrap()
    }

    /// `from` dials `to`; `answerer` answers it. Returns (from call, answerer call).
    async fn establish(&mut self, from: &str, to: &str, answerer: &str) -> (String, String) {
        let seq = self.seq().await;
        let d = self
            .call("call.dial", json!({"account": from, "target": to}))
            .await;
        let a = d["call_id"].as_str().unwrap().to_string();
        let ringing = self
            .call(
                "call.wait_for",
                json!({"account": answerer, "state":"ringing","since":seq,"timeout_ms":20_000}),
            )
            .await;
        let b = ringing["call"]["id"].as_str().unwrap().to_string();
        self.call("call.answer", json!({"call_id": b})).await;
        self.wait(&a, "active").await;
        self.wait(&b, "active").await;
        (a, b)
    }

    async fn all_events(&mut self, since: u64, kind: &str) -> Vec<Value> {
        let mut out: Vec<Value> = vec![];
        let mut cursor = since;
        for _ in 0..20 {
            let evs = self
                .call(
                    "events.poll",
                    json!({"since": cursor, "kinds":[kind], "timeout_ms": 1500}),
                )
                .await["events"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            match evs.last() {
                Some(l) => cursor = l["seq"].as_u64().unwrap(),
                None => break,
            }
            out.extend(evs);
            if out.iter().any(|e| e["data"]["final"] == true) {
                break;
            }
        }
        out
    }

    async fn trace(&mut self, id: &str) {
        if let Ok(t) = self.c.call("call.sip_trace", json!({"call_id": id})).await {
            for m in t["messages"].as_array().into_iter().flatten() {
                eprintln!("{}", m.as_str().unwrap_or(""));
            }
        }
    }

    async fn hangup_all(&mut self) {
        let l = self.c.call("call.list", json!({"active_only": true})).await;
        for c in l.iter().flat_map(|l| l["calls"].as_array().unwrap()) {
            let _ = self
                .c
                .call("call.hangup", json!({"call_id": c["id"]}))
                .await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// 1014 calls 1015 through the PBX, then blind-transfers 1015 to the tone extension.
#[tokio::test(flavor = "multi_thread")]
async fn pbx_blind_transfer_to_tone_extension() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let env = pbx_env();
    let cfg = format!(
        "{}{}",
        account(&env, "1014", "tone:1200", ""),
        account(&env, "1015", "tone:1500", "")
    );
    let mut r = rig(&cfg, &["u1014", "u1015"]).await;
    let (a, b) = r.establish("u1014", "1015", "u1015").await;
    assert!(
        r.hears(&a, 1500.0).await,
        "1014 hears 1015 before the transfer"
    );
    assert!(
        r.hears(&b, 1200.0).await,
        "1015 hears 1014 before the transfer"
    );

    let seq = r.seq().await;
    let tone_ext = env["PBX_TONE_EXT"].clone();
    let res =
        r.c.call(
            "call.transfer_blind",
            json!({"call_id": a, "target": tone_ext}),
        )
        .await;
    if let Err(e) = &res {
        r.trace(&a).await;
        panic!("transfer_blind: {e}");
    }
    let progress = r.all_events(seq, "call.transfer_progress").await;
    eprintln!("blind transfer progress events: {progress:#?}");
    let ok = progress.iter().any(|e| {
        e["data"]["final"] == true
            && e["data"]["code"]
                .as_u64()
                .is_some_and(|c| (200..300).contains(&c))
    });
    if !ok {
        r.trace(&a).await;
    }
    assert!(ok, "no final 2xx progress: {progress:?}");
    let ended = r.wait(&a, "ended").await;
    assert_eq!(ended["call"]["end"]["reason"], "transferred", "{ended}");

    // FreeSWITCH moved 1015's leg to the 1 kHz tone.
    let heard = r.hears(&b, 1000.0).await;
    let g = r.call("call.get", json!({"call_id": b})).await;
    eprintln!(
        "1015 after the transfer: state={} hears 1 kHz={heard}",
        g["state"]
    );
    if !heard {
        r.trace(&b).await;
    }
    assert!(
        heard,
        "1015 does not hear the 1 kHz tone after the transfer: {g}"
    );
    r.hangup_all().await;
    r.app.shutdown().await;
}

/// 1014 talks to 1015, consults 1016, then attended-transfers: FreeSWITCH bridges 1015 and 1016.
#[tokio::test(flavor = "multi_thread")]
async fn pbx_attended_transfer_bridges_the_other_two() {
    if !enabled() {
        eprintln!("skipped (set SIPLE_INTEROP=1)");
        return;
    }
    let _serial = SERIAL.lock().await;
    let env = pbx_env();
    let cfg = format!(
        "{}{}{}",
        account(&env, "1014", "tone:1200", ""),
        account(&env, "1015", "tone:1500", ""),
        account(&env, "1016", "tone:2000", "")
    );
    let mut r = rig(&cfg, &["u1014", "u1015", "u1016"]).await;
    let (a_b, b) = r.establish("u1014", "1015", "u1015").await;
    let (a_c, c) = r.establish("u1014", "1016", "u1016").await;

    let seq = r.seq().await;
    let res =
        r.c.call(
            "call.transfer_attended",
            json!({"call_id": a_b, "consult_call_id": a_c}),
        )
        .await;
    if let Err(e) = &res {
        r.trace(&a_b).await;
        panic!("transfer_attended: {e}");
    }
    let progress = r.all_events(seq, "call.transfer_progress").await;
    eprintln!("attended transfer progress events: {progress:#?}");
    let fin = progress.iter().find(|e| e["data"]["final"] == true);
    if fin.is_none_or(|f| {
        f["data"]["code"]
            .as_u64()
            .is_none_or(|c| !(200..300).contains(&c))
    }) {
        r.trace(&a_b).await;
        r.trace(&a_c).await;
    }
    let fin = fin.unwrap_or_else(|| panic!("no final progress: {progress:?}"));
    assert!(
        (200..300).contains(&fin["data"]["code"].as_u64().unwrap()),
        "{fin}"
    );
    let e1 = r.wait(&a_b, "ended").await;
    // The consult call goes away by itself or after the 5 s grace.
    let t0 = Instant::now();
    let e2 = r.wait(&a_c, "ended").await;
    eprintln!(
        "original ended: {}\nconsult ended after {:?}: {}",
        e1["call"]["end"],
        t0.elapsed(),
        e2["call"]["end"]
    );
    let (h1, h2) = (r.hears(&b, 2000.0).await, r.hears(&c, 1500.0).await);
    eprintln!("after attended transfer: 1015 hears 2000={h1}, 1016 hears 1500={h2}");
    assert!(h1 && h2, "1015 and 1016 are not bridged ({h1}/{h2})");
    r.hangup_all().await;
    r.app.shutdown().await;
}
