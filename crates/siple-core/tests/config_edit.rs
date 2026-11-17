//! Typed config write-back through a real `App`: `config.view`, `config.edit`, the ConfigActor,
//! hot reload of external edits, the device migration out of `state.toml`, and credential
//! redaction of logs and SIP traces.
#![cfg(unix)]

use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::events::EventFilter;
use siple_core::{App, AppOptions, LoggingMode};

mod common;

const PASSWORD: &str = "s3cret-pw-1234";

/// UDP registrar. With `challenge` it answers a REGISTER without `Authorization` with a digest
/// 401 and the retry with 200; otherwise every REGISTER gets a 200.
struct Registrar {
    addr: SocketAddr,
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Drop for Registrar {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn registrar(challenge: bool) -> Registrar {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let r = Registrar {
        addr: sock.local_addr().unwrap(),
        count: Arc::default(),
        stop: Arc::default(),
    };
    let (count, stop) = (r.count.clone(), r.stop.clone());
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while !stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = sock.recv_from(&mut buf) else {
                continue;
            };
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            if !req.starts_with("REGISTER ") {
                continue;
            }
            count.fetch_add(1, Ordering::SeqCst);
            let unauthorized = challenge && !req.to_ascii_lowercase().contains("\nauthorization:");
            let mut lines = vec![if unauthorized {
                "SIP/2.0 401 Unauthorized".to_string()
            } else {
                "SIP/2.0 200 OK".to_string()
            }];
            for l in req.lines() {
                let low = l.to_ascii_lowercase();
                if low.starts_with("via:") {
                    let base: String = l
                        .split(';')
                        .filter(|p| !p.trim_start().starts_with("rport"))
                        .collect::<Vec<_>>()
                        .join(";");
                    lines.push(format!("{base};rport={}", from.port()));
                } else if low.starts_with("to:") {
                    lines.push(if l.contains("tag=") {
                        l.to_string()
                    } else {
                        format!("{l};tag=fakereg")
                    });
                } else if ["from:", "call-id:", "cseq:"]
                    .iter()
                    .any(|p| low.starts_with(p))
                {
                    lines.push(l.to_string());
                } else if low.starts_with("contact:") && !unauthorized {
                    let c = l.split_once(':').unwrap().1.trim();
                    lines.push(format!("Contact: {c};expires=300"));
                }
            }
            if unauthorized {
                lines.push(
                    "WWW-Authenticate: Digest realm=\"fakereg\", nonce=\"servernonce0123456789\", algorithm=MD5, qop=\"auth\""
                        .into(),
                );
            }
            lines.push("Content-Length: 0".into());
            let resp = format!("{}\r\n\r\n", lines.join("\r\n"));
            let _ = sock.send_to(resp.as_bytes(), from);
        }
    });
    r
}

impl Registrar {
    async fn wait_count(&self, n: usize) {
        for _ in 0..250 {
            if self.count.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!(
            "expected {n} REGISTERs within 5 s, saw {}",
            self.count.load(Ordering::SeqCst)
        );
    }
}

/// A commented config with one registering account `a1`.
fn config(reg: &Registrar, extra_top: &str, extra_account: &str) -> String {
    format!(
        "# my phone\n{extra_top}\n[[account]]   # the desk line\nid = \"a1\"\nusername = \"1001\"   # extension\n\
         password = \"{PASSWORD}\"\ndomain = \"127.0.0.1\"\nregistrar = \"{}\"\ntransport = \"udp\"\n\
         keepalive_secs = 0\n{extra_account}\n",
        reg.addr
    )
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new(text: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), text).unwrap();
        Env { dir }
    }
    fn path(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }
    fn text(&self) -> String {
        std::fs::read_to_string(self.path()).unwrap()
    }
    fn write(&self, text: &str) {
        let tmp = self.path().with_extension("swp");
        std::fs::write(&tmp, text).unwrap();
        std::fs::rename(&tmp, self.path()).unwrap();
    }
    async fn start(&self, watch: bool) -> App {
        let mut o = AppOptions::new(self.path());
        o.logging = LoggingMode::Detached;
        o.stderr_log = false;
        o.enable_audio = false;
        o.watch_config = watch;
        o.watch_debounce = Duration::from_millis(200);
        App::start(o).await.unwrap()
    }
}

async fn client(app: &App) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "test").await.unwrap();
    c
}

async fn wait_registered(c: &mut ControlClient) {
    for _ in 0..250 {
        let s = c.call("status", Value::Null).await.unwrap();
        if s["accounts"][0]["registration"]["state"] == "registered" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("not registered");
}

/// The wire form of an API error: `{code, message, data}`.
async fn api_err(c: &mut ControlClient, method: &str, params: Value) -> Value {
    match c.call(method, params).await {
        Err(ClientError::Api(e)) => e.to_wire(),
        other => panic!("expected an API error, got {other:?}"),
    }
}

fn set(id: &str, field: &str, value: Value) -> Value {
    json!({"op":"set_account","id":id,"field":field,"value":value})
}

fn sec(section: &str, key: &str, value: Value) -> Value {
    json!({"op":"set_section","section":section,"key":key,"value":value})
}

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn username_edit_reregisters_ui_edit_does_not_and_comments_survive() {
    let reg = registrar(false);
    let env = Env::new(&config(&reg, "", ""));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    wait_registered(&mut c).await;
    let before = reg.count.load(Ordering::SeqCst);

    // An account edit: written in place, the account re-registers.
    let r = c
        .call(
            "config.edit",
            json!({"edits":[set("a1","username",json!("1005"))]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let summary = r["report"]["summary"].to_string();
    assert!(
        summary.contains("account changed: a1 (username)") && summary.contains("re-registering"),
        "{summary}"
    );
    // The AOR changed: the old binding is removed, the new one registered.
    reg.wait_count(before + 2).await;
    // Let the REGISTER exchange settle (slow under load) before counting again.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let text = env.text();
    assert!(text.contains("username = \"1005\"   # extension"), "{text}");
    assert!(text.starts_with("# my phone\n") && text.contains("[[account]]   # the desk line"));
    assert_eq!(r["hash"], sha256_hex(&text));
    assert_eq!(app.config().accounts[0].username, "1005");

    // A [ui] edit: applied, no account touched, no re-register.
    let seen = reg.count.load(Ordering::SeqCst);
    let r = c
        .call(
            "config.edit",
            json!({"base_hash": r["hash"], "edits":[sec("ui","theme",json!("dark")), sec("ui","blf_click_in_call",json!("consult"))]}),
        )
        .await
        .unwrap();
    let summary = r["report"]["summary"].to_string();
    assert!(summary.contains("ui settings updated"), "{summary}");
    assert!(!summary.contains("account changed"), "{summary}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(reg.count.load(Ordering::SeqCst), seen, "no re-register");
    let text = env.text();
    assert!(
        text.contains("[ui]\ntheme = \"dark\"\nblf_click_in_call = \"consult\"\n"),
        "{text}"
    );
    assert!(text.contains("# the desk line") && text.contains("# extension"));
    let v = c.call("config.view", Value::Null).await.unwrap();
    assert_eq!(v["config"]["ui"]["theme"], "dark");
    assert_eq!(v["provenance"]["ui"]["theme"], "file");
    assert_eq!(v["provenance"]["ui"]["density"], "default");
    assert_eq!(v["hash"], r["hash"]);

    // Setting a key to its default adds nothing to the file.
    let before_text = env.text();
    let r = c
        .call(
            "config.edit",
            json!({"edits":[set("a1","dnd",json!(false)), sec("calls","decline_code",json!(486))]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true);
    assert_eq!(env.text(), before_text);
    // GUI account toggle goes through the same path.
    let rep = app.set_account_enabled("a1", false).await;
    assert!(rep.applied && rep.errors.is_empty(), "{rep:?}");
    assert!(env.text().contains("enabled = false"));
    drop(c);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn base_hash_conflict_and_invalid_edits_change_nothing() {
    let reg = registrar(false);
    let env = Env::new(&config(&reg, "", ""));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    let v = c.call("config.view", Value::Null).await.unwrap();
    let h1 = v["hash"].as_str().unwrap().to_string();
    assert_eq!(h1, sha256_hex(&env.text()));

    // Someone edits the file in between.
    env.write(&format!("{}# external\n", env.text()));
    let e = api_err(
        &mut c,
        "config.edit",
        json!({"base_hash": h1, "edits":[set("a1","dnd",json!(true))]}),
    )
    .await;
    assert_eq!(e["data"]["kind"], "invalid_state", "{e}");
    assert_eq!(e["data"]["reason"], "conflict");
    assert_eq!(e["data"]["current_hash"], sha256_hex(&env.text()));
    assert!(!env.text().contains("dnd"));
    // Retry with the current hash.
    let cur = e["data"]["current_hash"].clone();
    let r = c
        .call(
            "config.edit",
            json!({"base_hash": cur, "edits":[set("a1","dnd",json!(true))]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true);
    assert!(env.text().contains("dnd = true") && env.text().contains("# external"));

    // Invalid edits are rejected with the path of the key, and the file is untouched.
    let untouched = env.text();
    for (edits, path) in [
        (
            json!([set("a1", "username", json!("a b"))]),
            "account[0].username",
        ),
        (
            json!([set("a1", "local_port", json!(70000))]),
            "account[0].local_port",
        ),
        (json!([sec("ui", "theme", json!("blue"))]), "ui.theme"),
        (json!([sec("media", "codecs", json!("opus"))]), "media"),
        (
            json!([set("nobody", "dnd", json!(true))]),
            "account[id=nobody]",
        ),
        (
            json!([sec("calls", "decline_code", json!(200))]),
            "calls.decline_code",
        ),
    ] {
        let e = api_err(&mut c, "config.edit", json!({"edits": edits})).await;
        assert_eq!(e["data"]["kind"], "invalid_params", "{e}");
        assert_eq!(e["data"]["reason"], "invalid");
        assert_eq!(e["data"]["errors"][0]["path"], path, "{e}");
        assert_eq!(env.text(), untouched);
    }
    // A malformed edit (bad op, masked value) is invalid_params too.
    for edits in [
        json!([{"op":"explode"}]),
        json!([set("a1", "password", json!({"set":true}))]),
        json!([]),
    ] {
        let e = api_err(&mut c, "config.edit", json!({"edits": edits})).await;
        assert_eq!(e["data"]["kind"], "invalid_params", "{e}");
        assert_eq!(env.text(), untouched);
    }
    // A masked string is refused as a password, too.
    let e = api_err(
        &mut c,
        "config.edit",
        json!({"edits":[set("a1","password",json!("***"))]}),
    )
    .await;
    assert_eq!(e["data"]["errors"][0]["path"], "account[0].password");
    assert!(env.text().contains(PASSWORD));

    // A file that does not parse refuses edits with file_invalid (even with a stale base_hash);
    // fixing it lifts that.
    env.write("not [toml");
    let e = api_err(
        &mut c,
        "config.edit",
        json!({"base_hash": "00", "edits":[set("a1","dnd",json!(false))]}),
    )
    .await;
    assert_eq!(e["data"]["kind"], "invalid_state", "{e}");
    assert_eq!(e["data"]["reason"], "file_invalid");
    env.write(&untouched);
    c.call(
        "config.edit",
        json!({"edits":[set("a1","dnd",json!(false))]}),
    )
    .await
    .unwrap();
    drop(c);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn view_reports_provenance_and_never_reveals_secrets() {
    let reg = registrar(false);
    let env = Env::new(&config(
        &reg,
        "[logging]\nredact_srtp_keys = true\n",
        "profile = \"yealink\"\ndnd_reject_code = 603\nuser_agent = \"\"",
    ));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    let v = c.call("config.view", Value::Null).await.unwrap();
    assert!(v["path"].as_str().unwrap().ends_with("config.toml"));
    let acct = &v["config"]["account"][0];
    let prov = &v["provenance"]["account"]["a1"];
    // file: set in the file; profile: filled by the Yealink bundle; default: nobody set it.
    assert_eq!(prov["username"], "file");
    assert_eq!(prov["dnd_reject_code"], "file");
    assert_eq!(acct["dnd_reject_code"], 603);
    assert_eq!(prov["hold_style"], "profile");
    assert_eq!(acct["hold_style"], "sendonly");
    assert_eq!(prov["prack"], "profile");
    assert_eq!(acct["prack"], "disabled");
    // An empty user_agent counts as unset, so the profile fills it.
    assert_eq!(prov["user_agent"], "profile");
    assert!(acct["user_agent"].as_str().unwrap().starts_with("Yealink"));
    assert_eq!(prov["dnd"], "default");
    assert_eq!(prov["tls_verify"], "default");
    assert_eq!(v["provenance"]["logging"]["redact_srtp_keys"], "file");
    assert_eq!(v["provenance"]["logging"]["redact_credentials"], "default");
    assert_eq!(v["provenance"]["general"]["log_level"], "default");

    // Secrets are masked: a flag, never the value.
    assert_eq!(acct["password"], json!({"set": true}));
    assert_eq!(v["secrets"]["control_token"], json!({"set": true}));
    let dump = v.to_string();
    assert!(!dump.contains(PASSWORD), "{dump}");
    assert!(!dump.contains(app.token().expose()), "{dump}");
    // The edit schema lists what is editable.
    assert!(
        v["editable"]["account_fields"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "password")
    );
    assert!(
        v["editable"]["sections"]["ui"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "theme")
    );

    // The mask cannot be written back.
    let e = api_err(
        &mut c,
        "config.edit",
        json!({"edits":[set("a1","password",acct["password"].clone())]}),
    )
    .await;
    assert_eq!(e["data"]["kind"], "invalid_params", "{e}");
    assert!(env.text().contains(PASSWORD));
    // Resetting a key removes it, and the profile fills it again.
    let r = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"reset_account","id":"a1","field":"profile"}]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true);
    let v = c.call("config.view", Value::Null).await.unwrap();
    assert_eq!(v["provenance"]["account"]["a1"]["profile"], "default");
    // config.get keeps masking passwords.
    let g = c.call("config.get", Value::Null).await.unwrap();
    assert!(!g.to_string().contains(PASSWORD));
    drop(c);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn external_edits_hot_reload_and_our_own_write_is_not_applied_twice() {
    let reg = registrar(false);
    let env = Env::new(&config(&reg, "", ""));
    let app = env.start(true).await;
    let mut c = client(&app).await;
    wait_registered(&mut c).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let since = app.events().current_seq();

    // Our own edit: exactly one apply, even though the watcher sees the write.
    c.call(
        "config.edit",
        json!({"edits":[sec("ui","tray",json!(false))]}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let applied = app
        .events()
        .since(since, &EventFilter::kinds(["config.applied"]), 10);
    assert_eq!(applied.len(), 1, "{applied:?}");
    let seen = reg.count.load(Ordering::SeqCst);

    // An external edit (an editor's atomic save) is hot-reloaded and shows up in the view.
    // `hash` follows the file on disk at once; `accepted_hash` moves when the reload is applied.
    let old_hash = c.call("config.view", Value::Null).await.unwrap()["accepted_hash"].clone();
    env.write(
        &env.text()
            .replace("username = \"1001\"", "username = \"1002\""),
    );
    let mut v = Value::Null;
    for _ in 0..100 {
        v = c.call("config.view", Value::Null).await.unwrap();
        if v["accepted_hash"] != old_hash {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(v["config"]["account"][0]["username"], "1002", "{v}");
    assert_eq!(v["accepted_hash"], v["hash"]);
    assert_eq!(v["provenance"]["account"]["a1"]["username"], "file");
    assert_eq!(v["hash"], sha256_hex(&env.text()));
    reg.wait_count(seen + 1).await;
    // And edits through the API carry on from the externally changed file.
    c.call(
        "config.edit",
        json!({"base_hash": v["hash"], "edits":[sec("ui","tray",json!(true))]}),
    )
    .await
    .unwrap();
    assert!(env.text().contains("username = \"1002\""));
    drop(c);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn add_remove_rename_and_blf_edits() {
    let reg = registrar(false);
    let shipped = siple_core::config::EXAMPLE_CONFIG.to_string();
    let env = Env::new(&shipped);
    let app = env.start(false).await;
    let mut c = client(&app).await;
    // The wizard path: replace the untouched example.
    let r = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"add_account","replace_example":true,"draft":{"id":"desk",
                "fields":{"username":"1001","domain":"127.0.0.1","registrar":reg.addr.to_string(),"password":"pw"}}}]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true, "{r}");
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(s["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(s["accounts"][0]["id"], "desk");
    assert!(!env.text().contains("Sample account"));
    c.call("config.edit", json!({"edits":[
        {"op":"set_blf","account":"desk","entries":[{"uri":"1002","label":"Bob"},{"uri":"1003","label":"Cy"}]},
        {"op":"rename_account","from":"desk","to":"desk-1"},
    ]})).await.unwrap();
    let v = c.call("config.view", Value::Null).await.unwrap();
    assert_eq!(v["config"]["account"][0]["id"], "desk-1");
    assert_eq!(v["config"]["account"][0]["blf"][1]["uri"], "1003");
    // Reorder: the file order is the display order.
    c.call("config.edit", json!({"edits":[{"op":"add_account","draft":{"id":"second",
        "fields":{"username":"1002","domain":"127.0.0.1","registrar":reg.addr.to_string(),"password":"pw"}}}]}))
        .await
        .unwrap();
    let r = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"move_account","id":"desk-1","index":1}]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(r["report"]["changed"], true, "{r}");
    assert!(
        r["report"]["summary"][0]
            .as_str()
            .unwrap()
            .contains("account order"),
        "{r}"
    );
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(s["accounts"][0]["id"], "second");
    let v = c.call("config.view", Value::Null).await.unwrap();
    assert_eq!(v["config"]["account"][0]["id"], "second");
    assert_eq!(v["config"]["account"][1]["id"], "desk-1");
    c.call(
        "config.edit",
        json!({"edits":[{"op":"remove_account","id":"desk-1"},{"op":"remove_account","id":"second"}]}),
    )
    .await
    .unwrap();
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(s["accounts"].as_array().unwrap().len(), 0);
    drop(c);
    app.shutdown().await;
}

fn legacy_state(input: &str, output: &str) -> String {
    format!(
        "[audio]\nupdated_at = \"2026-10-01T10:00:00Z\"\n[audio.input]\nname = \"{input}\"\nat = \"2026-10-01T10:00:00Z\"\n\
         [audio.output]\nname = \"{output}\"\nat = \"2026-10-01T10:00:00Z\"\n\
         [config_seen.input]\nname = \"\"\nat = \"2026-09-01T10:00:00Z\"\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn state_toml_device_choice_moves_into_the_config_once() {
    let reg = registrar(false);
    // [audio] sets the output already (and so wins); the input is empty (so the choice moves).
    let env = Env::new(&config(
        &reg,
        "[audio]\noutput = \"Cfg Spk\"   # desk\n",
        "",
    ));
    let state = env.dir.path().join("state.toml");
    std::fs::write(&state, legacy_state("Legacy Mic", "Legacy Spk")).unwrap();
    let app = env.start(false).await;
    let e = app.effective_devices();
    assert_eq!(e.input.name, "Legacy Mic");
    assert_eq!(
        e.output.name, "Cfg Spk",
        "an existing [audio] value is not overridden"
    );
    let text = env.text();
    assert!(text.contains("output = \"Cfg Spk\"   # desk"), "{text}");
    assert!(text.contains("input = \"Legacy Mic\""), "{text}");
    assert!(!text.contains("Legacy Spk"));
    assert!(
        !state.exists(),
        "the migrated state is removed from state.toml"
    );
    let v = client(&app)
        .await
        .call("config.view", Value::Null)
        .await
        .unwrap();
    assert_eq!(v["hash"], sha256_hex(&text));
    app.shutdown().await;

    // Second start: nothing left to migrate, nothing changes.
    let before = env.text();
    let app = env.start(false).await;
    assert_eq!(app.effective_devices().input.name, "Legacy Mic");
    assert_eq!(env.text(), before);
    app.shutdown().await;

    // The state choice was newer than the config value it saw: it overwrites that value.
    let env = Env::new(&config(
        &reg,
        "[audio]\noutput = \"Old Cfg\"   # desk\n",
        "",
    ));
    let state = env.dir.path().join("state.toml");
    std::fs::write(
        &state,
        "[audio.output]\nname = \"Chosen Spk\"\nat = \"2026-10-01T10:00:00Z\"\n\
         [config_seen.output]\nname = \"Old Cfg\"\nat = \"2026-08-01T10:00:00Z\"\n",
    )
    .unwrap();
    let app = env.start(false).await;
    assert_eq!(app.effective_devices().output.name, "Chosen Spk");
    assert!(
        env.text().contains("output = \"Chosen Spk\"   # desk"),
        "{}",
        env.text()
    );
    assert!(!state.exists());
    app.shutdown().await;

    // A state file with only an explicit "system default" has nothing to write but is cleaned up.
    let env = Env::new(&config(&reg, "", ""));
    let state = env.dir.path().join("state.toml");
    std::fs::write(&state, legacy_state("", "")).unwrap();
    let before = env.text();
    let app = env.start(false).await;
    assert_eq!(env.text(), before);
    assert!(!state.exists());
    assert_eq!(app.effective_devices().input.name, "");
    app.shutdown().await;
}

// ---- redaction ----------------------------------------------------------------------------------

fn read_all(dir: &Path, prefix: &str) -> String {
    let mut out = String::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.push_str(&read_all(&p, prefix));
        } else if e.file_name().to_string_lossy().starts_with(prefix) {
            out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
        }
    }
    out
}

fn authorization_lines(trace: &str) -> Vec<String> {
    trace
        .lines()
        .filter(|l| l.to_ascii_lowercase().starts_with("authorization:"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn credentials_are_masked_when_asked_and_passwords_are_never_logged() {
    let reg = registrar(true);
    let env = Env::new(&config(
        &reg,
        "[general]\nlog_level = \"trace,rsipstack=trace\"\n",
        "",
    ));
    // The only test of this binary that installs the global subscriber, so the app log sees
    // every event (including those of the other tests' apps).
    let mut o = AppOptions::new(env.path());
    o.logging = LoggingMode::Install;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    let app = App::start(o).await.unwrap();
    let mut c = client(&app).await;
    wait_registered(&mut c).await;
    let log_dir = env.dir.path().join("logs");

    // Defaults are unchanged: the digest response is in the trace.
    c.call("account.reregister", json!({"account":"a1"}))
        .await
        .unwrap();
    reg.wait_count(4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Hot switch on, then re-register again.
    let r = c
        .call(
            "config.edit",
            json!({"edits":[sec("logging","redact_credentials",json!(true))]}),
        )
        .await
        .unwrap();
    assert!(
        r["report"]["summary"]
            .to_string()
            .contains("logging settings updated"),
        "{r}"
    );
    tracing::info!("MARKER-redaction-on");
    let marker_before = read_all(&log_dir.join("sip"), "sip").len();
    c.call("account.reregister", json!({"account":"a1"}))
        .await
        .unwrap();
    reg.wait_count(6).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Passwords reach neither the ring nor any file, with redaction off or on; the edit that
    // carries one is not logged either.
    c.call(
        "config.edit",
        json!({"edits":[set("a1","password",json!("another-pw-98765"))]}),
    )
    .await
    .unwrap();
    c.call("config.view", Value::Null).await.unwrap();
    let token = app.token().expose().to_string();
    let ring = app.logging().unwrap().tail(100_000).join("\n");
    drop(c);
    app.shutdown().await;

    let trace = read_all(&log_dir.join("sip"), "sip");
    let (early, late) = trace.split_at(marker_before.min(trace.len()));
    let raw_responses = |t: &str| {
        authorization_lines(t)
            .into_iter()
            .filter(|l| l.contains("response=\"") && !l.contains("response=\"<redacted>\""))
            .count()
    };
    assert!(
        raw_responses(early) >= 1,
        "default keeps the digest response:\n{early}"
    );
    let masked = authorization_lines(late);
    assert!(!masked.is_empty(), "{late}");
    for l in &masked {
        assert!(
            l.contains("response=\"<redacted>\"") && l.contains("nonce=\"<redacted>\""),
            "{l}"
        );
        assert!(l.contains("username=\"1001\""), "{l}");
    }
    // The application log, after the switch: no raw digest response on any line.
    let app_log = read_all(&log_dir, "siple");
    let after_switch = app_log
        .split_once("MARKER-redaction-on")
        .map_or("", |(_, rest)| rest);
    assert!(!after_switch.is_empty(), "the app log is written");
    for l in after_switch.lines().filter(|l| {
        let low = l.to_ascii_lowercase();
        low.contains("authorization:") && low.contains("response")
    }) {
        assert!(
            !l.contains("response=\"") || l.contains("response=\"<redacted>\""),
            "{l}"
        );
    }
    for (name, text) in [
        ("sip trace", &trace),
        ("app log", &app_log),
        ("ring", &ring),
    ] {
        for secret in [PASSWORD, "another-pw-98765"] {
            assert!(!text.contains(secret), "{name} contains a password");
        }
    }
    assert!(!app_log.contains(&token) && !ring.contains(&token));
}

async fn dial(a: &mut ControlClient, target: &str) -> Value {
    a.call(
        "call.dial",
        json!({"account":"alice","target":target,"source":"silence","sink":"null"}),
    )
    .await
    .unwrap()
}

async fn ringing(b: &mut ControlClient, since: u64) -> String {
    let r = b
        .call(
            "call.wait_for",
            json!({"account":"bob","state":"ringing","since":since,"timeout_ms":30_000}),
        )
        .await
        .unwrap();
    r["call"]["id"].as_str().unwrap().to_string()
}

async fn ended_code(a: &mut ControlClient, id: String) -> Value {
    let r = a
        .call(
            "call.wait_for",
            json!({"call_id": id,"state":"ended","timeout_ms":30_000}),
        )
        .await
        .unwrap();
    r["call"]["end"]["code"].clone()
}

/// The GUI's Reject uses `[calls] decline_code` while another call is live on any account (call
/// waiting), and the plain 486 otherwise; the far end of the same call (a self-call between two
/// local accounts) is not another call.
#[tokio::test(flavor = "multi_thread")]
async fn reject_uses_decline_code_only_while_another_call_is_live() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, pb) = (common::free_udp_port(), common::free_udp_port());
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[calls]\ndecline_code = 603\n{}{}",
            common::acct("alice", pa, ""),
            common::acct("bob", pb, "")
        ),
    )
    .unwrap();
    let app = common::start_app(&path).await;
    let mut a = common::client(&app, "a").await;
    let mut b = common::client(&app, "b").await;
    let bob = format!("sip:bob@127.0.0.1:{pb}");
    // Alone: plain 486.
    let seq = common::seq(&mut b).await;
    let d = dial(&mut a, &bob).await;
    let b1 = ringing(&mut b, seq).await;
    app.reject_call(&b1).await.unwrap();
    assert_eq!(
        ended_code(&mut a, d["call_id"].as_str().unwrap().into()).await,
        486
    );

    // With a live call: the configured decline code.
    let seq = common::seq(&mut b).await;
    let d = dial(&mut a, &bob).await;
    let b2 = ringing(&mut b, seq).await;
    b.call("call.answer", json!({"call_id": b2})).await.unwrap();
    a.call(
        "call.wait_for",
        json!({"call_id": d["call_id"],"state":"active","timeout_ms":30_000}),
    )
    .await
    .unwrap();
    let seq = common::seq(&mut b).await;
    let d3 = dial(&mut a, &bob).await;
    let b3 = ringing(&mut b, seq).await;
    app.reject_call(&b3).await.unwrap();
    assert_eq!(
        ended_code(&mut a, d3["call_id"].as_str().unwrap().into()).await,
        603
    );
    // The API still takes an explicit code.
    let seq = common::seq(&mut b).await;
    let d4 = dial(&mut a, &bob).await;
    let b4 = ringing(&mut b, seq).await;
    b.call("call.reject", json!({"call_id": b4, "code": 480}))
        .await
        .unwrap();
    assert_eq!(
        ended_code(&mut a, d4["call_id"].as_str().unwrap().into()).await,
        480
    );
    drop((a, b));
    app.shutdown().await;
}

/// Credentials are masked from the very first REGISTER retry when `redact_credentials` is on at
/// startup (the redactor is primed before the accounts start).
#[tokio::test(flavor = "multi_thread")]
async fn startup_redaction_covers_the_first_register_retry() {
    let reg = registrar(true);
    let second = format!(
        "[[account]]\nid = \"a2\"\nusername = \"1002\"\npassword = \"{PASSWORD}-2\"\ndomain = \"127.0.0.1\"\n\
         registrar = \"{}\"\ntransport = \"udp\"\nkeepalive_secs = 0\n",
        reg.addr
    );
    let env = Env::new(&format!(
        "{}{second}",
        config(&reg, "[logging]\nredact_credentials = true\n", "")
    ));
    let app = env.start(false).await;
    reg.wait_count(4).await; // two accounts, each: 401 then the authenticated retry
    tokio::time::sleep(Duration::from_millis(400)).await;
    app.shutdown().await;
    let trace = read_all(&env.dir.path().join("logs").join("sip"), "sip");
    let auth = authorization_lines(&trace);
    assert!(auth.len() >= 2, "{trace}");
    for l in &auth {
        assert!(
            l.contains("response=\"<redacted>\"") && l.contains("nonce=\"<redacted>\""),
            "{l}"
        );
    }
    assert!(!trace.contains(PASSWORD));
}
