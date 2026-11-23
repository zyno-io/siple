//! Multi-line behavior end to end, no PBX: peer-mode accounts of ONE app call each other over
//! loopback UDP, driven through the control API: `call.activate`, answer modes,
//! `status.active_line`, media focus, `[calls] hold_scope` hot reload and end causes.
#![cfg(unix)]

use std::time::Duration;

mod common;

use common::free_udp_port;
use serde_json::{Value, json};
use siple_core::control::ErrorKind;
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::{App, AppOptions, LoggingMode};

struct Rig {
    dir: tempfile::TempDir,
    app: App,
    c: ControlClient,
    /// `(id, uri)` per account.
    accounts: Vec<(String, String)>,
}

fn config_text(accounts: &[(String, u16)], calls: &str) -> String {
    let mut text = format!("{calls}\n");
    for (id, port) in accounts {
        text.push_str(&format!(
            "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\npassword = \"pw\"\n\
             domain = \"127.0.0.1\"\ntransport = \"udp\"\nregister = false\n\
             bind_address = \"127.0.0.1\"\nlocal_port = {port}\n\
             audio_source = \"device\"\naudio_sink = \"device\"\n\n"
        ));
    }
    text
}

async fn rig(ids: &[&str]) -> Rig {
    rig_with(ids, "").await
}

async fn rig_with(ids: &[&str], calls: &str) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let ports: Vec<(String, u16)> = ids
        .iter()
        .map(|i| (i.to_string(), free_udp_port()))
        .collect();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, config_text(&ports, calls)).unwrap();
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
    c.hello(Some(app.token().expose()), "lines").await.unwrap();
    let accounts = ports
        .iter()
        .map(|(id, p)| (id.clone(), format!("sip:{id}@127.0.0.1:{p}")))
        .collect();
    Rig {
        dir,
        app,
        c,
        accounts,
    }
}

impl Rig {
    fn uri(&self, account: &str) -> String {
        self.accounts
            .iter()
            .find(|(i, _)| i == account)
            .unwrap()
            .1
            .clone()
    }

    async fn wait(&mut self, call: &str, state: &str) -> Value {
        self.c
            .call(
                "call.wait_for",
                json!({"call_id": call, "state": state, "timeout_ms": 30_000}),
            )
            .await
            .unwrap_or_else(|e| panic!("wait_for {call} {state}: {e}"))
    }

    async fn get(&mut self, call: &str) -> Value {
        self.c
            .call("call.get", json!({"call_id": call}))
            .await
            .unwrap()
    }

    /// `from` dials `to` (device source and sink); returns `(from call, to call)` with the far
    /// call still ringing.
    async fn dial(&mut self, from: &str, to: &str) -> (String, String) {
        let d = self
            .c
            .call(
                "call.dial",
                json!({"account":from,"target":self.uri(to),"source":"device","sink":"device"}),
            )
            .await
            .unwrap();
        let ringing = self
            .c
            .call(
                "call.wait_for",
                json!({"account":to,"state":"ringing","since":d["event_seq"],"timeout_ms":30_000}),
            )
            .await
            .unwrap();
        (
            d["call_id"].as_str().unwrap().to_string(),
            ringing["call"]["id"].as_str().unwrap().to_string(),
        )
    }

    /// A call from `from` to `to`, answered: `(from call, to call)`.
    async fn establish(&mut self, from: &str, to: &str) -> (String, String) {
        let (a, b) = self.dial(from, to).await;
        self.c
            .call(
                "call.answer",
                json!({"call_id": b, "source":"device","sink":"device"}),
            )
            .await
            .unwrap();
        self.wait(&a, "active").await;
        self.wait(&b, "active").await;
        (a, b)
    }

    async fn active_line(&mut self) -> Value {
        self.c.call("status", Value::Null).await.unwrap()["active_line"].clone()
    }

    async fn engine_focus(&self, want: &str) {
        for _ in 0..100 {
            if self.app.media().focus().as_deref() == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "media focus is {:?}, wanted {want}",
            self.app.media().focus()
        );
    }
}

fn state(v: &Value) -> &str {
    v["state"].as_str().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn activate_swaps_lines_and_moves_the_media_focus() {
    let mut r = rig(&["alice", "bob"]).await;
    let (a1, _b1) = r.establish("alice", "bob").await;
    // A second call: alice's dial auto-holds the first (account scope or all, same account).
    let (a2, b2) = r.dial("alice", "bob").await;
    assert_eq!(state(&r.get(&a1).await), "held");
    r.c.call(
        "call.answer",
        json!({"call_id": b2, "source":"device","sink":"device"}),
    )
    .await
    .unwrap();
    r.wait(&a2, "active").await;
    // Both legs of the new self-call are device-bound and answer concurrently: the tracked
    // active line is one of them (never stale, never null).
    let live = r.active_line().await;
    assert!(live == json!(a2) || live == json!(b2), "{live}");

    // call.activate: hold the live line (alice's a2 is the self-call peer of b2: left alone),
    // resume a1, move the focus.
    let res =
        r.c.call("call.activate", json!({"call_id": a1}))
            .await
            .unwrap();
    assert_eq!(res["ok"], true);
    assert_eq!(res["active_line"], json!(a1));
    assert_eq!(state(&r.get(&a1).await), "active");
    assert_eq!(state(&r.get(&a2).await), "held");
    assert_eq!(r.active_line().await, json!(a1));
    r.engine_focus(&a1).await;
    let info = r.get(&a2).await;
    assert!(info["hold_changed_at"].is_string(), "{info}");
    assert!(info["pending"].is_null(), "{info}");

    // Idempotent.
    let again =
        r.c.call("call.activate", json!({"call_id": a1}))
            .await
            .unwrap();
    assert_eq!(again["active_line"], json!(a1));
    // And back.
    r.c.call("call.activate", json!({"call_id": a2}))
        .await
        .unwrap();
    assert_eq!(state(&r.get(&a2).await), "active");
    assert_eq!(state(&r.get(&a1).await), "held");
    r.engine_focus(&a2).await;

    // Errors.
    let Err(ClientError::Api(e)) = r.c.call("call.activate", json!({"call_id":"c999"})).await
    else {
        panic!("expected an API error");
    };
    assert_eq!(e.kind, ErrorKind::NotFound);
    r.app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn answer_end_others_and_invalid_modes() {
    let mut r = rig(&["alice", "bob"]).await;
    let (a1, b1) = r.establish("alice", "bob").await;
    let (a2, b2) = r.dial("alice", "bob").await;

    // Bad combinations are refused before anything happens.
    for params in [
        json!({"call_id": b2, "mode":"end_others"}),
        json!({"call_id": b2, "mode":"hold_others", "end_call_id": b1}),
        json!({"call_id": b2, "end_call_id": b1}),
        json!({"call_id": b2, "mode":"everything"}),
        json!({"call_id": b2, "mode":"end_others", "end_call_id": "c999"}),
    ] {
        let Err(ClientError::Api(e)) = r.c.call("call.answer", params.clone()).await else {
            panic!("expected an API error for {params}");
        };
        assert!(
            matches!(e.kind, ErrorKind::InvalidParams | ErrorKind::NotFound),
            "{params}: {e:?}"
        );
    }
    assert_eq!(state(&r.get(&b2).await), "ringing");

    r.c.call(
        "call.answer",
        json!({"call_id": b2, "mode":"end_others", "end_call_id": b1,
               "source":"device","sink":"device"}),
    )
    .await
    .unwrap();
    r.wait(&a2, "active").await;
    let ended = r.wait(&b1, "ended").await;
    assert_eq!(ended["call"]["end"]["cause"], "hangup");
    let a1_ended = r.wait(&a1, "ended").await;
    assert_eq!(a1_ended["call"]["end"]["cause"], "remote_hangup");
    assert_eq!(state(&r.get(&b2).await), "active");
    r.app.shutdown().await;
}

/// `[calls] hold_scope` is hot: with `account` another account's call stays up, after a reload to
/// `all` it is put on hold.
#[tokio::test(flavor = "multi_thread")]
async fn hold_scope_reloads_hot() {
    let mut r = rig(&["alice", "bob", "carol", "dave"]).await;
    let ports: Vec<(String, u16)> = r
        .accounts
        .iter()
        .map(|(id, uri)| {
            (
                id.clone(),
                uri.rsplit(':').next().unwrap().parse::<u16>().unwrap(),
            )
        })
        .collect();
    let path = r.dir.path().join("config.toml");
    let reload = |scope: &str| {
        std::fs::write(
            &path,
            config_text(&ports, &format!("[calls]\nhold_scope = \"{scope}\"\n")),
        )
        .unwrap();
    };

    reload("account");
    r.c.call("config.reload", Value::Null).await.unwrap();
    let (a1, _b1) = r.establish("alice", "bob").await;
    let (c1, _d1) = r.establish("carol", "dave").await;
    assert_eq!(
        state(&r.get(&a1).await),
        "active",
        "account scope: untouched"
    );
    assert_eq!(state(&r.get(&c1).await), "active");

    reload("all");
    let res = r.c.call("config.reload", Value::Null).await.unwrap();
    assert!(
        res["summary"].to_string().contains("calls settings"),
        "{res}"
    );
    // A new call on carol's account now holds alice's call too (and carol's first one).
    let (c2, d2) = r.dial("carol", "dave").await;
    assert_eq!(
        state(&r.get(&a1).await),
        "held",
        "all scope: alice's call held"
    );
    assert_eq!(state(&r.get(&c1).await), "held");
    r.c.call("call.answer", json!({"call_id": d2}))
        .await
        .unwrap();
    r.wait(&c2, "active").await;
    r.app.shutdown().await;
}

/// With `hold_scope = "account"` two un-held device calls on different accounts coexist. When the
/// recorded active line goes away, `active_line` falls back to the call that now owns the
/// microphone instead of reporting none.
#[tokio::test(flavor = "multi_thread")]
async fn active_line_falls_back_to_the_focused_call() {
    let mut r = rig_with(
        &["alice", "bob", "carol", "dave"],
        "[calls]\nhold_scope = \"account\"\n",
    )
    .await;
    let (a1, b1) = r.establish("alice", "bob").await;
    let (c1, d1) = r.establish("carol", "dave").await;
    assert_eq!(
        state(&r.get(&a1).await),
        "active",
        "account scope: both live"
    );
    r.c.call("call.hangup", json!({"call_id": c1}))
        .await
        .unwrap();
    r.wait(&d1, "ended").await;
    let mut line = Value::Null;
    for _ in 0..100 {
        line = r.active_line().await;
        if !line.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(line == json!(a1) || line == json!(b1), "{line}");
    r.engine_focus(line.as_str().unwrap()).await;
    r.app.shutdown().await;
}
