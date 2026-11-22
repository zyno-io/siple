//! Transfers end to end, no PBX: three peer-mode accounts (alice, bob, carol) in ONE app,
//! driven only through the control API; audio proves who is bridged to whom.
#![cfg(unix)]

mod common;

use common::{free_tcp_port, free_udp_port};
use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

struct Rig {
    _dir: tempfile::TempDir,
    _app: App,
    c: ControlClient,
    bob_uri: String,
    carol_uri: String,
    /// Rings (no auto answer), so a consult towards it can be pending or refused.
    dave_uri: String,
}

#[derive(Default)]
struct Opts {
    alice_extra: &'static str,
    bob_extra: &'static str,
    tcp: bool,
}

async fn rig(alice_extra: &'static str) -> Rig {
    rig_with(Opts {
        alice_extra,
        ..Default::default()
    })
    .await
}

async fn rig_with(o: Opts) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let port = |_: &str| {
        if o.tcp {
            free_tcp_port()
        } else {
            free_udp_port()
        }
    };
    let (pa, pb, pc, pd) = (port("a"), port("b"), port("c"), port("d"));
    let transport = if o.tcp { "tcp" } else { "udp" };
    let acct = |id: &str, port: u16, extra: &str| {
        format!(
            "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\npassword = \"pw\"\n\
             domain = \"127.0.0.1\"\ntransport = \"{transport}\"\nregister = false\n\
             bind_address = \"127.0.0.1\"\nlocal_port = {port}\n\
             audio_sink = \"null\"\n{extra}\n"
        )
    };
    let text = format!(
        "{}{}{}{}",
        acct(
            "alice",
            pa,
            &format!("audio_source = \"tone:1000\"\n{}", o.alice_extra)
        ),
        acct(
            "bob",
            pb,
            &format!("audio_source = \"tone:1500\"\n{}", o.bob_extra)
        ),
        acct(
            "carol",
            pc,
            "audio_source = \"tone:2000\"\nauto_answer = true\n"
        ),
        acct("dave", pd, "audio_source = \"tone:2500\"\n"),
    );
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    let mut opts = AppOptions::new(&path);
    opts.logging = LoggingMode::Detached;
    opts.stderr_log = false;
    opts.enable_audio = false;
    opts.watch_config = false;
    let app = App::start(opts).await.unwrap();
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "ctl").await.unwrap();
    let sfx = if o.tcp { ";transport=tcp" } else { "" };
    Rig {
        _dir: dir,
        _app: app,
        c,
        bob_uri: format!("sip:bob@127.0.0.1:{pb}{sfx}"),
        carol_uri: format!("sip:carol@127.0.0.1:{pc}{sfx}"),
        dave_uri: format!("sip:dave@127.0.0.1:{pd}{sfx}"),
    }
}

impl Rig {
    async fn call(&mut self, m: &str, p: Value) -> Value {
        self.c
            .call(m, p)
            .await
            .unwrap_or_else(|e| panic!("{m}: {e}"))
    }

    async fn seq(&mut self) -> u64 {
        self.call("events.current_seq", Value::Null).await["seq"]
            .as_u64()
            .unwrap()
    }

    async fn wait(&mut self, id: &str, state: &str) -> Value {
        self.call(
            "call.wait_for",
            json!({"call_id": id, "state": state, "timeout_ms": 30_000}),
        )
        .await
    }

    async fn hears(&mut self, id: &str, hz: f64) -> bool {
        self.call(
            "call.detect_tone",
            json!({"call_id": id, "freq_hz": hz, "timeout_ms": 5000}),
        )
        .await["detected"]
            == true
    }

    /// alice dials `to`; `answerer` (account) answers it when it rings. Returns (alice, answerer).
    async fn establish(&mut self, to: &str, account: &str) -> (String, String) {
        let seq = self.seq().await;
        let d = self
            .call("call.dial", json!({"account":"alice","target":to}))
            .await;
        let a = d["call_id"].as_str().unwrap().to_string();
        let ringing = self
            .call(
                "call.wait_for",
                json!({"account":account,"state":"ringing","since":seq,"timeout_ms":30_000}),
            )
            .await;
        let b = ringing["call"]["id"].as_str().unwrap().to_string();
        self.call("call.answer", json!({"call_id": b})).await;
        self.wait(&a, "active").await;
        self.wait(&b, "active").await;
        (a, b)
    }

    async fn events(&mut self, since: u64, kind: &str) -> Vec<Value> {
        self.call(
            "events.poll",
            json!({"since": since, "kinds":[kind], "timeout_ms": 3000}),
        )
        .await["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// All `call.transfer_progress` events after `since`, up to and including the final one.
    async fn progress_until_final(&mut self, since: u64) -> Vec<Value> {
        let mut all: Vec<Value> = vec![];
        let mut cursor = since;
        for _ in 0..20 {
            let evs = self.events(cursor, "call.transfer_progress").await;
            if let Some(last) = evs.last() {
                cursor = last["seq"].as_u64().unwrap();
            }
            all.extend(evs);
            if all.iter().any(|e| e["data"]["final"] == true) {
                break;
            }
        }
        all
    }

    /// `transfer.session` events after `since`, polled until one of session `sid` reaches `phase`.
    /// The event log is fed asynchronously, so it can trail `transfer.get` by a moment: a single
    /// poll made right after `wait_phase` may not hold the last event yet.
    async fn session_events_until(&mut self, since: u64, sid: &str, phase: &str) -> Vec<Value> {
        let mut all: Vec<Value> = vec![];
        let mut cursor = since;
        for _ in 0..20 {
            let evs = self.events(cursor, "transfer.session").await;
            if let Some(last) = evs.last() {
                cursor = last["seq"].as_u64().unwrap();
            }
            all.extend(evs);
            if all.iter().any(|e| {
                e["data"]["session"]["id"] == sid && e["data"]["session"]["phase"] == phase
            }) {
                break;
            }
        }
        all
    }

    /// `transfer.get` -> the session.
    async fn session(&mut self, sid: &str) -> Value {
        self.call("transfer.get", json!({"session_id": sid})).await["session"].clone()
    }

    async fn wait_phase(&mut self, sid: &str, phase: &str) -> Value {
        for _ in 0..200 {
            let s = self.session(sid).await;
            if s["phase"] == phase {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "session {sid} never reached {phase}: {}",
            self.session(sid).await
        );
    }

    async fn state_of(&mut self, id: &str) -> String {
        self.call("call.get", json!({"call_id": id})).await["state"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn wait_state(&mut self, id: &str, state: &str) {
        for _ in 0..200 {
            if self.state_of(id).await == state {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let info = self.call("call.get", json!({"call_id": id})).await;
        let transfers = self.call("transfer.list", json!({})).await;
        panic!("call {id} never reached {state}: {info}; transfers: {transfers}");
    }

    /// The first (only) call of `account` (the far end of a consult).
    async fn only_call(&mut self, account: &str) -> String {
        for _ in 0..100 {
            if let Some(c) = self.calls_of(account).await.first() {
                return c["id"].as_str().unwrap().to_string();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("no call at {account}");
    }

    /// The call that rings at `account` after `since`.
    async fn ringing(&mut self, account: &str, since: u64) -> String {
        let w = self
            .call(
                "call.wait_for",
                json!({"account":account,"state":"ringing","since":since,"timeout_ms":30_000}),
            )
            .await;
        w["call"]["id"].as_str().unwrap().to_string()
    }

    /// alice (a) has an established call with bob; returns `(a, b)`; then alice consults
    /// `target`. Returns `(session id, consult call id)`.
    async fn consult(&mut self, a: &str, target: &str) -> (String, String) {
        let r = self
            .call(
                "call.consult_start",
                json!({"call_id": a, "target": target}),
            )
            .await;
        assert_eq!(r["ok"], true);
        (
            r["session_id"].as_str().unwrap().to_string(),
            r["consult_call_id"].as_str().unwrap().to_string(),
        )
    }

    async fn calls_of(&mut self, account: &str) -> Vec<Value> {
        self.call("call.list", json!({"account": account})).await["calls"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn blind_transfer_bridges_bob_to_carol_with_audio() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    assert!(
        r.hears(&b, 1000.0).await,
        "bob hears alice before the transfer"
    );

    let seq = r.seq().await;
    let carol = r.carol_uri.clone();
    let res = r
        .call(
            "call.transfer_blind",
            json!({"call_id": a, "target": carol}),
        )
        .await;
    assert_eq!(res["ok"], true);
    assert!(res["event_seq"].as_u64().is_some());

    // Progress ends with a final 200; alice's call ends (transferred).
    let finals = r.progress_until_final(seq).await;
    let fin = finals
        .iter()
        .find(|e| e["data"]["final"] == true)
        .unwrap_or_else(|| panic!("no final progress: {finals:?}"));
    assert_eq!(fin["data"]["code"], 200);
    assert_eq!(fin["call_id"], a.as_str());
    let ended = r.wait(&a, "ended").await;
    assert_eq!(ended["call"]["end"]["reason"], "transferred");

    // bob received the REFER.
    let refers = r.events(seq, "call.refer_received").await;
    assert_eq!(refers.len(), 1, "{refers:?}");
    assert_eq!(refers[0]["call_id"], b.as_str());
    assert_eq!(refers[0]["data"]["replaces"], false);

    // bob <-> carol: bob's new call is linked to the original, audio flows both ways.
    let bob_calls = r.calls_of("bob").await;
    let bob_new = bob_calls
        .iter()
        .find(|c| c["transferred_from"] == b.as_str())
        .unwrap_or_else(|| panic!("{bob_calls:?}"));
    assert_eq!(bob_new["state"], "active");
    let bob_new = bob_new["id"].as_str().unwrap().to_string();
    let carol_calls = r.calls_of("carol").await;
    let carol_call = carol_calls[0]["id"].as_str().unwrap().to_string();
    assert!(r.hears(&carol_call, 1500.0).await, "carol hears bob's tone");
    assert!(r.hears(&bob_new, 2000.0).await, "bob hears carol's tone");
    r.wait(&b, "ended").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attended_transfer_bridges_bob_to_carol_with_audio() {
    let mut r = rig("").await;
    let (a_b, _b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let seq = r.seq().await;
    let d = r
        .call("call.dial", json!({"account":"alice","target":carol}))
        .await;
    let a_c = d["call_id"].as_str().unwrap().to_string();
    r.wait(&a_c, "active").await;

    r.call(
        "call.transfer_attended",
        json!({"call_id": a_b, "consult_call_id": a_c}),
    )
    .await;
    let finals = r.progress_until_final(seq).await;
    assert!(
        finals
            .iter()
            .any(|e| e["data"]["final"] == true && e["data"]["code"] == 200),
        "{finals:?}"
    );
    r.wait(&a_b, "ended").await;
    r.wait(&a_c, "ended").await;
    let refers = r.events(seq, "call.refer_received").await;
    assert_eq!(refers[0]["data"]["replaces"], true, "{refers:?}");

    // carol's replacing call is active and linked; the bridged audio is bob's tone.
    let carol_calls = r.calls_of("carol").await;
    let replacing = carol_calls
        .iter()
        .find(|c| c["replaces"].is_string())
        .unwrap_or_else(|| panic!("{carol_calls:?}"));
    let id = replacing["id"].as_str().unwrap().to_string();
    assert_eq!(replacing["state"], "active");
    assert!(
        r.hears(&id, 1500.0).await,
        "carol hears bob after the replace"
    );
    let bob_new = r
        .calls_of("bob")
        .await
        .into_iter()
        .find(|c| c["transferred_from"].is_string() && c["state"] == "active")
        .expect("bob's new call");
    assert!(r.hears(bob_new["id"].as_str().unwrap(), 2000.0).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn transfer_methods_validate() {
    let mut r = rig("").await;
    for (m, p) in [
        (
            "call.transfer_blind",
            json!({"call_id":"c999","target":"x"}),
        ),
        (
            "call.transfer_attended",
            json!({"call_id":"c999","consult_call_id":"c998"}),
        ),
    ] {
        let e = r.c.call(m, p).await.unwrap_err();
        assert!(
            e.to_string().contains("not_found") || e.to_string().to_lowercase().contains("unknown"),
            "{m}: {e}"
        );
    }
    let e =
        r.c.call("call.transfer_blind", json!({"call_id":"c1","target":" "}))
            .await
            .unwrap_err();
    assert!(
        e.to_string().contains("invalid_params") || e.to_string().contains("empty"),
        "{e}"
    );
    let m = r.call("methods.list", Value::Null).await;
    let names: Vec<&str> = m["methods"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["name"].as_str())
        .collect();
    assert!(names.contains(&"call.transfer_blind") && names.contains(&"call.transfer_attended"));
    for n in [
        "call.consult_start",
        "call.consult_link",
        "transfer.complete",
        "transfer.swap",
        "transfer.cancel",
        "transfer.get",
        "transfer.list",
    ] {
        assert!(names.contains(&n), "{n} missing from the catalog");
    }
}

// ---- transfer sessions ---------------------------------------------------------------------

async fn consult_complete_with_audio(tcp: bool) {
    let mut r = rig_with(Opts {
        tcp,
        ..Default::default()
    })
    .await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let seq = r.seq().await;
    let carol = r.carol_uri.clone();
    let (sid, a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    // A is held, the consult talks with alice's audio.
    assert_eq!(r.state_of(&a).await, "held");
    let carol_call = r.only_call("carol").await;
    assert!(
        r.hears(&carol_call, 1000.0).await,
        "carol hears the consult"
    );
    assert!(r.hears(&a_c, 2000.0).await, "alice hears carol");
    let call = r.call("call.get", json!({"call_id": a})).await;
    assert_eq!(call["transfer"]["session_id"], sid.as_str());
    assert_eq!(call["transfer"]["role"], "original");

    let done = r
        .call(
            "transfer.complete",
            json!({"session_id": sid, "mode": "attended"}),
        )
        .await;
    assert_eq!(done["session_id"], sid.as_str());
    let s = r.wait_phase(&sid, "transferred").await;
    assert_eq!(s["kind"], "attended");
    assert_eq!(s["last_notify"]["code"], 200);
    assert_eq!(s["refer_call"], a.as_str());
    let ended = r.wait(&a, "ended").await;
    assert_eq!(ended["call"]["end"]["cause"], "transferred");

    // The event stream saw the whole path.
    let evs = r.session_events_until(seq, &sid, "transferred").await;
    let phases: Vec<&str> = evs
        .iter()
        .filter(|e| e["data"]["session"]["id"] == sid.as_str())
        .filter_map(|e| e["data"]["session"]["phase"].as_str())
        .collect();
    for want in [
        "consult_dialing",
        "consult_connected",
        "referring",
        "transferring",
        "transferred",
    ] {
        assert!(phases.contains(&want), "{want} missing in {phases:?}");
    }
    let pos = |p: &str| phases.iter().position(|x| *x == p).unwrap();
    assert!(pos("consult_connected") < pos("referring") && pos("referring") < pos("transferred"));

    // bob <-> carol, audio both ways.
    let bob_new = r
        .calls_of("bob")
        .await
        .into_iter()
        .find(|c| c["transferred_from"] == b.as_str())
        .expect("bob's new call");
    let carol_calls = r.calls_of("carol").await;
    let replacing = carol_calls
        .iter()
        .find(|c| c["replaces"].is_string())
        .unwrap_or_else(|| panic!("{carol_calls:?}"));
    assert!(
        r.hears(replacing["id"].as_str().unwrap(), 1500.0).await,
        "carol hears bob"
    );
    assert!(
        r.hears(bob_new["id"].as_str().unwrap(), 2000.0).await,
        "bob hears carol"
    );
    // The list carries it.
    let list = r.call("transfer.list", json!({})).await;
    assert!(
        list["transfers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == sid.as_str() && t["phase"] == "transferred")
    );
    let status = r.call("status", Value::Null).await;
    assert!(
        status["transfers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["id"] == sid.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn consult_then_complete_bridges_bob_to_carol_with_audio() {
    consult_complete_with_audio(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn consult_then_complete_over_tcp() {
    consult_complete_with_audio(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn consult_cancel_resumes_alice_with_audio() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let (sid, a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    assert_eq!(r.state_of(&b).await, "remote_held");
    let res = r.call("transfer.cancel", json!({"session_id": sid})).await;
    assert_eq!(res["session"]["phase"], "cancelled");
    r.wait(&a_c, "ended").await;
    r.wait_state(&a, "active").await;
    // alice's microphone feeds the original call again.
    assert!(
        r.hears(&b, 1000.0).await,
        "bob hears alice after the cancel"
    );
    // A second cancel is a no-op.
    let again = r.call("transfer.cancel", json!({"session_id": sid})).await;
    assert_eq!(again["session"]["phase"], "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn attended_complete_is_rejected_while_ringing_and_blind_now_goes_semi() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let seq = r.seq().await;
    let dave = r.dave_uri.clone();
    let (sid, a_d) = r.consult(&a, &dave).await;
    let d1 = r.ringing("dave", seq).await;
    r.wait_phase(&sid, "consult_ringing").await;
    let e =
        r.c.call(
            "transfer.complete",
            json!({"session_id": sid, "mode": "attended"}),
        )
        .await
        .unwrap_err();
    let e = e.to_string();
    assert!(
        e.contains("invalid_state") && e.contains("consult not answered; use blind_now"),
        "{e}"
    );
    assert_eq!(r.session(&sid).await["phase"], "consult_ringing");

    let seq = r.seq().await;
    let res = r
        .call(
            "transfer.complete",
            json!({"session_id": sid, "mode": "blind_now"}),
        )
        .await;
    assert_eq!(res["session"]["kind"], "semi");
    r.wait(&a_d, "ended").await;
    r.wait(&d1, "ended").await;
    // bob now calls dave himself.
    let d2 = r.ringing("dave", seq).await;
    assert_ne!(d1, d2);
    r.call("call.answer", json!({"call_id": d2})).await;
    let s = r.wait_phase(&sid, "transferred").await;
    assert_eq!(s["kind"], "semi");
    r.wait(&a, "ended").await;
    let refers = r.events(seq, "call.refer_received").await;
    assert_eq!(refers[0]["data"]["replaces"], false, "{refers:?}");
    let bob_new = r
        .calls_of("bob")
        .await
        .into_iter()
        .find(|c| c["transferred_from"] == b.as_str())
        .expect("bob's new call");
    assert!(r.hears(&d2, 1500.0).await, "dave hears bob");
    assert!(
        r.hears(bob_new["id"].as_str().unwrap(), 2500.0).await,
        "bob hears dave"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_consult_target_fails_the_consult_with_alice_still_held() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let seq = r.seq().await;
    let dave = r.dave_uri.clone();
    let (sid, _a_d) = r.consult(&a, &dave).await;
    let d = r.ringing("dave", seq).await;
    r.call("call.reject", json!({"call_id": d, "code": 486}))
        .await;
    let s = r.wait_phase(&sid, "consult_failed").await;
    assert_eq!(s["last_error"]["code"], 486);
    assert_eq!(s["last_error"]["stage"], "consult");
    assert_eq!(r.state_of(&a).await, "held");
    // Nothing to complete; Cancel returns to bob.
    assert!(
        r.c.call(
            "transfer.complete",
            json!({"session_id": sid, "mode": "attended"})
        )
        .await
        .is_err()
    );
    r.call("transfer.cancel", json!({"session_id": sid})).await;
    r.wait_state(&a, "active").await;
    assert!(r.hears(&b, 1000.0).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_refer_returns_to_the_consult_with_an_error() {
    let mut r = rig_with(Opts {
        bob_extra: "accept_refer = false\n",
        ..Default::default()
    })
    .await;
    let (a, _b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let (sid, a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    let e =
        r.c.call(
            "transfer.complete",
            json!({"session_id": sid, "mode": "attended"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("603"), "{e}");
    let s = r.session(&sid).await;
    assert_eq!(s["phase"], "consult_connected", "{s}");
    assert_eq!(s["last_error"]["code"], 603);
    assert_eq!(s["last_error"]["stage"], "refer");
    assert_eq!(r.state_of(&a).await, "held");
    assert_eq!(r.state_of(&a_c).await, "active");
    r.call("transfer.cancel", json!({"session_id": sid})).await;
    r.wait_state(&a, "active").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_blind_transfer_resumes_alice() {
    let mut r = rig_with(Opts {
        bob_extra: "accept_refer = false\n",
        ..Default::default()
    })
    .await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let e =
        r.c.call(
            "call.transfer_blind",
            json!({"call_id": a, "target": carol}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("603"), "{e}");
    let list = r.call("transfer.list", json!({})).await;
    let t = list["transfers"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (t["kind"].as_str(), t["phase"].as_str()),
        (Some("blind"), Some("failed"))
    );
    assert_eq!(t["we_held_original"], true);
    r.wait_state(&a, "active").await;
    assert!(r.hears(&b, 1000.0).await, "bob hears alice again");
}

#[tokio::test(flavor = "multi_thread")]
async fn blind_and_attended_results_carry_the_session_id() {
    let mut r = rig("").await;
    let (a, _b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let res = r
        .call(
            "call.transfer_blind",
            json!({"call_id": a, "target": carol}),
        )
        .await;
    assert_eq!(res["ok"], true);
    let sid = res["session_id"].as_str().unwrap().to_string();
    let s = r.wait_phase(&sid, "transferred").await;
    assert_eq!(s["kind"], "blind");
    // Legacy attended: an implicit session.
    let (a2, _b2) = r.establish(&r.bob_uri.clone(), "bob").await;
    let d = r
        .call(
            "call.dial",
            json!({"account":"alice","target": r.carol_uri.clone()}),
        )
        .await;
    let c2 = d["call_id"].as_str().unwrap().to_string();
    r.wait(&c2, "active").await;
    let res = r
        .call(
            "call.transfer_attended",
            json!({"call_id": a2, "consult_call_id": c2}),
        )
        .await;
    let sid = res["session_id"].as_str().unwrap().to_string();
    let s = r.wait_phase(&sid, "transferred").await;
    assert_eq!(s["kind"], "attended");
}

#[tokio::test(flavor = "multi_thread")]
async fn double_complete_sends_one_refer() {
    let mut r = rig("").await;
    let (a, _b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let (sid, _a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    let seq = r.seq().await;
    for _ in 0..2 {
        let res = r
            .call(
                "transfer.complete",
                json!({"session_id": sid, "mode": "attended"}),
            )
            .await;
        assert_eq!(res["session_id"], sid.as_str());
    }
    r.wait_phase(&sid, "transferred").await;
    let res = r
        .call(
            "transfer.complete",
            json!({"session_id": sid, "mode": "attended"}),
        )
        .await;
    assert_eq!(res["session"]["phase"], "transferred");
    r.wait(&a, "ended").await;
    let refers = r.events(seq, "call.refer_received").await;
    assert_eq!(refers.len(), 1, "{refers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_moves_alice_between_bob_and_carol() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let (sid, a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    let carol_call = r.only_call("carol").await;
    let s = r.call("transfer.swap", json!({"session_id": sid})).await;
    assert_eq!(s["session"]["talking_to"], a.as_str());
    assert_eq!(r.state_of(&a).await, "active");
    assert_eq!(r.state_of(&a_c).await, "held");
    assert!(r.hears(&b, 1000.0).await, "bob hears alice after the swap");
    let s = r.call("transfer.swap", json!({"session_id": sid})).await;
    assert_eq!(s["session"]["talking_to"], a_c.as_str());
    assert_eq!(r.state_of(&a).await, "held");
    assert!(
        r.hears(&carol_call, 1000.0).await,
        "carol hears alice again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_hangup_of_the_consult_and_remote_hangup_of_the_original() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let carol = r.carol_uri.clone();
    let (sid, a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    r.call("call.hangup", json!({"call_id": a_c})).await;
    let s = r.wait_phase(&sid, "consult_failed").await;
    assert!(s["talking_to"].is_null());
    assert_eq!(r.state_of(&a).await, "held", "A stays held");
    r.call("transfer.cancel", json!({"session_id": sid})).await;
    r.wait_state(&a, "active").await;

    // A second session: bob hangs up while alice consults carol again.
    let (sid, a_c) = r.consult(&a, &carol).await;
    r.wait_phase(&sid, "consult_connected").await;
    r.call("call.hangup", json!({"call_id": b})).await;
    r.wait_phase(&sid, "abandoned").await;
    assert_eq!(r.state_of(&a_c).await, "active", "the consult is untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn hold_is_rejected_while_transferring() {
    let mut r = rig("").await;
    let (a, _b) = r.establish(&r.bob_uri.clone(), "bob").await;
    let seq = r.seq().await;
    let dave = r.dave_uri.clone();
    let res = r
        .call("call.transfer_blind", json!({"call_id": a, "target": dave}))
        .await;
    let sid = res["session_id"].as_str().unwrap().to_string();
    // bob calls dave, who rings: the transfer is in flight.
    let d = r.ringing("dave", seq).await;
    let s = r.session(&sid).await;
    assert_eq!(s["phase"], "transferring", "{s}");
    for m in ["call.hold", "call.unhold"] {
        let e =
            r.c.call(m, json!({"call_id": a}))
                .await
                .unwrap_err()
                .to_string();
        assert!(e.contains("invalid_state"), "{m}: {e}");
    }
    // Cancel is refused too: the PBX has the REFER.
    let e =
        r.c.call("transfer.cancel", json!({"session_id": sid}))
            .await
            .unwrap_err()
            .to_string();
    assert!(e.contains("already handed to the PBX"), "{e}");
    r.call("call.answer", json!({"call_id": d})).await;
    r.wait_phase(&sid, "transferred").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn consult_link_requires_the_same_account() {
    let mut r = rig("").await;
    let (a, b) = r.establish(&r.bob_uri.clone(), "bob").await;
    // alice's call and bob's call are on different accounts.
    let e =
        r.c.call(
            "call.consult_link",
            json!({"call_id": a, "consult_call_id": b}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("same account"), "{e}");
    let e =
        r.c.call("transfer.get", json!({"session_id": "t999"}))
            .await
            .unwrap_err()
            .to_string();
    assert!(e.contains("not_found"), "{e}");
    let e =
        r.c.call(
            "transfer.complete",
            json!({"session_id": "t999", "mode": "sideways"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("invalid_params"), "{e}");
}

/// Regression: rigs picked ports with a bare `bind(0)` probe, so parallel rigs (or two accounts of
/// one rig) could get the same just-released port and a consult landed on the wrong peer.
#[test]
fn allocated_ports_never_repeat() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..3000 {
        assert!(seen.insert(free_udp_port()), "udp port handed out twice");
    }
    for _ in 0..3000 {
        assert!(seen.insert(free_tcp_port()), "port handed out twice");
    }
}
