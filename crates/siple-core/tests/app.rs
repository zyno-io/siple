//! End-to-end tests of [`App`]: startup, control API, dynamic reconfiguration.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::control::client::ControlClient;
use siple_core::events::EventFilter;
use siple_core::{App, AppError, AppOptions, LoggingMode};

fn account(id: &str, extra: &str) -> String {
    format!(
        "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\npassword = \"pw\"\ndomain = \"127.0.0.1\"\n\
         transport = \"udp\"\nregister = false\n{extra}\n"
    )
}

fn config_text(ids: &[&str]) -> String {
    ids.iter().map(|i| account(i, "")).collect()
}

fn write_atomic(path: &Path, text: &str) {
    let tmp = path.with_extension("swp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, path).unwrap();
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
    fn write(&self, text: &str) {
        write_atomic(&self.path(), text);
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

fn ids(status: &Value) -> Vec<String> {
    status["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn status_shape() {
    let env = Env::new(&config_text(&["a1", "a2"]));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(ids(&s), ["a1", "a2"]);
    assert_eq!(s["accounts"][0]["registration"]["state"], "unregistered");
    assert_eq!(s["calls"], json!([]));
    assert_eq!(s["audio"]["enabled"], false);
    assert_eq!(s["audio"]["effective"]["input"]["source"], "default");
    assert!(s["audio"]["mic_permission"].is_string());
    assert_eq!(s["config"]["last_error"], Value::Null);
    assert!(
        s["config"]["path"]
            .as_str()
            .unwrap()
            .ends_with("config.toml")
    );
    assert!(s["event_seq"].as_u64().unwrap() >= 1); // app.started
    // methods advertise schemas
    let m = c.call("methods.list", Value::Null).await.unwrap();
    let names: Vec<&str> = m["methods"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    for n in [
        "status",
        "account.get",
        "audio.set_devices",
        "audio.mic_level",
        "audio.echo_test",
        "audio.test_speaker",
        "config.reload",
        "call.sip_trace",
    ] {
        assert!(names.contains(&n), "{n} missing");
    }
    for n in [
        "call.dial",
        "call.answer",
        "call.wait_for",
        "call.detect_tone",
        "account.set_dnd",
    ] {
        assert!(names.contains(&n), "{n} missing");
    }
    // The catalog served by a live instance is exactly the static one the MCP bridge uses.
    let catalog: Vec<String> = siple_core::backend::method_catalog()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert_eq!(
        names,
        catalog.iter().map(String::as_str).collect::<Vec<_>>()
    );
    drop(c);
    app.shutdown().await;
    assert!(!env.dir.path().join("runtime.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn account_methods_and_errors() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    let g = c
        .call("account.get", json!({"account":"a1"}))
        .await
        .unwrap();
    assert_eq!(g["id"], "a1");
    assert_eq!(g["config"]["password"], "***");
    let e = c
        .call("account.get", json!({"account":"nope"}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("not_found"), "{e}");
    let e = c.call("account.get", json!({})).await.unwrap_err();
    assert!(e.to_string().contains("invalid_params"), "{e}");
    let r = c
        .call(
            "account.set_sip_trace",
            json!({"account":"a1","enabled":false}),
        )
        .await
        .unwrap();
    assert!(r["event_seq"].is_u64());
    let cfg = c.call("config.get", Value::Null).await.unwrap();
    let text = cfg.to_string();
    assert!(text.contains("***") && !text.contains("\"pw\""), "{text}");
    let e = c.call("blf.subscribe", json!({})).await.unwrap_err();
    assert!(e.to_string().contains("method_not_found"), "{e}");
    let t = c
        .call("call.sip_trace", json!({"call_id":"unknown"}))
        .await
        .unwrap();
    assert_eq!(t["count"], 0);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_adds_removes_and_changes_accounts() {
    let env = Env::new(&config_text(&["a1", "a2"]));
    let app = env.start(false).await;
    let since = app.events().current_seq();

    // remove a1, change a2 (hot: dnd), add a3
    let mut text = account("a2", "dnd = true");
    text.push_str(&account("a3", ""));
    env.write(&text);
    let report = app.reload().await;
    assert!(report.applied && report.changed, "{report:?}");
    assert!(report.errors.is_empty(), "{report:?}");
    let joined = report.summary.join("\n");
    assert!(joined.contains("account removed: a1"), "{joined}");
    assert!(joined.contains("account changed: a2 (dnd)"), "{joined}");
    assert!(joined.contains("account added: a3"), "{joined}");

    let mut c = client(&app).await;
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(ids(&s), ["a2", "a3"]);
    assert_eq!(s["accounts"][0]["dnd"], true);
    assert_eq!(app.config().accounts.len(), 2);

    let ev = app
        .events()
        .since(since, &EventFilter::kinds(["config.applied"]), 10);
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data["summary"].as_array().unwrap().len(), 3);

    // Reloading the same content changes nothing and emits nothing.
    let n = app.events().current_seq();
    let r = app.reload().await;
    assert!(r.applied && !r.changed);
    assert_eq!(app.events().current_seq(), n);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_config_keeps_last_good_and_emits_error() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    let since = app.events().current_seq();

    // Parse error.
    env.write("this is = not [valid\n");
    let r = c.call("config.reload", Value::Null).await.unwrap_err();
    assert!(r.to_string().contains("invalid_state"), "{r}");
    // Validation error (duplicate id).
    env.write(&(config_text(&["a1"]) + &account("a1", "")));
    let r = app.reload().await;
    assert!(!r.applied && !r.errors.is_empty(), "{r:?}");

    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(ids(&s), ["a1"]);
    assert!(s["config"]["last_error"]["errors"][0].is_string());
    let errs = app
        .events()
        .since(since, &EventFilter::kinds(["config.error"]), 10);
    assert_eq!(errs.len(), 2);
    assert!(errs[0].data["errors"].as_array().unwrap().len() == 1);

    // Fixing the file clears the error and applies.
    env.write(&config_text(&["a1", "a2"]));
    let r = app.reload().await;
    assert!(r.applied && r.changed, "{r:?}");
    assert!(app.last_config_error().is_none());
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_config_at_start_has_no_side_effects() {
    let env = Env::new("[[account]]\nid = \"x\"\n");
    let mut o = AppOptions::new(env.path());
    o.logging = LoggingMode::Detached;
    o.enable_audio = false;
    let err = App::start(o).await.err().unwrap();
    assert!(
        matches!(err, AppError::Invalid { .. } | AppError::Config(_)),
        "{err}"
    );
    assert!(!env.dir.path().join("siple.lock").exists());
    assert!(!env.dir.path().join("runtime.json").exists());
}

/// `audio.mic_level` / `audio.echo_test` / `audio.test_speaker` without an audio layer, and
/// `App::call` (the in-process path of the GUI) serves the same methods.
#[tokio::test(flavor = "multi_thread")]
async fn mic_level_and_echo_test_without_audio() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    let mut c = client(&app).await;
    let r = c.call("audio.mic_level", Value::Null).await.unwrap();
    assert_eq!(r["available"], false);
    assert!(r["level_dbfs"].is_null());
    let r = c
        .call("audio.echo_test", json!({"action":"status"}))
        .await
        .unwrap();
    assert_eq!(r["phase"], "idle");
    let e = c.call("audio.echo_test", json!({})).await.unwrap_err();
    assert!(e.to_string().contains("invalid_state"), "{e}");
    let e = c
        .call("audio.echo_test", json!({"action":"bogus"}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("invalid_params"), "{e}");
    let e = c.call("audio.test_speaker", Value::Null).await.unwrap_err();
    assert!(e.to_string().contains("invalid_state"), "{e}");
    let v = app.call("config.view", Value::Null).await.unwrap();
    assert!(v["hash"].is_string());
    drop(c);
    app.shutdown().await;
}

/// A file that parses but fails validation is rejected by the reload; `config.view` still gives
/// the hash of what is on disk, so an edit based on it (disabling the bad account) repairs it
/// (plan section 4.2) instead of conflicting forever.
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_file_can_be_repaired_by_an_edit_based_on_the_viewed_hash() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    let before = app.config_view();
    // Parses, but the account has no username: validation refuses it.
    env.write(&config_text(&["a1"]).replace("username = \"a1\"\n", "username = \"\"\n"));
    let report = app.reload().await;
    assert!(!report.applied, "{report:?}");
    let view = app.config_view();
    assert_ne!(
        view["hash"], before["hash"],
        "hash follows the file on disk"
    );
    assert_eq!(view["accepted_hash"], before["hash"]);
    let fixed = app
        .edit_config(
            vec![siple_core::config::edit::ConfigEdit::SetAccount {
                id: "a1".into(),
                field: siple_core::config::edit::AccountField::Enabled,
                value: siple_core::config::edit::EditValue::Bool(false),
            }],
            view["hash"].as_str().map(str::to_string),
        )
        .await
        .expect("conflict-free repair");
    assert!(fixed.applied, "{fixed:?}");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_devices_writes_audio_to_the_config_and_follows_it() {
    let env = Env::new(&config_text(&["a1"]));
    {
        let app = env.start(false).await;
        let mut c = client(&app).await;
        let e = c.call("audio.set_devices", json!({})).await.unwrap_err();
        assert!(e.to_string().contains("invalid_params"), "{e}");
        let r = c
            .call("audio.set_devices", json!({"input":"Test Mic"}))
            .await
            .unwrap();
        assert_eq!(r["effective"]["input"]["name"], "Test Mic");
        assert_eq!(r["effective"]["input"]["source"], "config");
        assert_eq!(r["persisted"], true);
        let text = std::fs::read_to_string(env.path()).unwrap();
        assert!(text.contains("[audio]\ninput = \"Test Mic\"\n"), "{text}");
        assert!(!env.dir.path().join("state.toml").exists());

        // An external edit of [audio] is hot-reloaded like any other.
        env.write(
            &text
                .replace("Test Mic", "Cfg Mic")
                .replace("[audio]\n", "[audio]\noutput = \"Cfg Spk\"\n"),
        );
        let rep = app.reload().await;
        assert!(
            rep.summary.iter().any(|s| s.contains("audio devices")),
            "{rep:?}"
        );
        let e = app.effective_devices();
        assert_eq!(
            (e.input.name.as_str(), e.input.source),
            ("Cfg Mic", siple_core::state::DeviceSource::Config)
        );

        // A GUI/API choice writes the key in place; other slots keep the file's values.
        let r = c
            .call("audio.set_devices", json!({"input":"Newer Mic"}))
            .await
            .unwrap();
        assert_eq!(r["effective"]["input"]["name"], "Newer Mic");
        assert_eq!(r["effective"]["output"]["name"], "Cfg Spk");
        // Back to the system default: the key stays, set to "".
        let r = c
            .call("audio.set_devices", json!({"input":""}))
            .await
            .unwrap();
        assert_eq!(r["effective"]["input"]["source"], "default");
        drop(c);
        app.shutdown().await;
    }
    // Restart: the file is the only source.
    let app = env.start(false).await;
    let e = app.effective_devices();
    assert_eq!(e.input.name, "");
    assert_eq!(e.output.name, "Cfg Spk");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watcher_end_to_end_adds_account() {
    let env = Env::new(&config_text(&["a1", "a2"]));
    let app = env.start(true).await;
    let mut c = client(&app).await;
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(ids(&s), ["a1", "a2"]);
    let since = s["event_seq"].as_u64().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Same content rewritten (editor "save" without changes): nothing happens.
    env.write(&config_text(&["a1", "a2"]));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        app.events().current_seq(),
        since,
        "unchanged hash must be skipped"
    );

    // Real edit via atomic rename.
    env.write(&config_text(&["a1", "a2", "a3"]));
    let r = c
        .call(
            "events.poll",
            json!({"since": since, "kinds": ["config.applied"], "timeout_ms": 15000}),
        )
        .await
        .unwrap();
    let events = r["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{r}");
    assert_eq!(events[0]["data"]["summary"][0], "account added: a3");
    let s = c.call("status", Value::Null).await.unwrap();
    assert_eq!(ids(&s), ["a1", "a2", "a3"]);
    drop(c);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn control_endpoint_change_rewrites_runtime_json() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    let before = app.control_endpoints().await;
    assert!(before.tcp.is_none());
    env.write(&format!(
        "[control]\ntcp_listen = \"127.0.0.1:0\"\n{}",
        config_text(&["a1"])
    ));
    // Port 0 is "no change" for from_config's parse? It parses to 127.0.0.1:0 -> new endpoints.
    let rep = app.reload().await;
    assert!(
        rep.summary.iter().any(|s| s.contains("control endpoints")),
        "{rep:?}"
    );
    let after = app.control_endpoints().await;
    assert!(after.tcp.is_some());
    let on_disk: Value = serde_json::from_str(
        &std::fs::read_to_string(env.dir.path().join("runtime.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk["control"]["tcp"], json!(after.tcp));
    // Still reachable (new listeners).
    let mut c = client(&app).await;
    c.call("ping", Value::Null).await.unwrap();
    drop(c);
    app.shutdown().await;
    assert!(!env.dir.path().join("runtime.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn log_level_change_is_applied() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    env.write(&format!(
        "[general]\nlog_level = \"debug\"\n{}",
        config_text(&["a1"])
    ));
    let rep = app.reload().await;
    assert!(
        rep.summary.iter().any(|s| s.contains("log level")),
        "{rep:?}"
    );
    assert_eq!(app.logging().unwrap().filter(), "debug,rsipstack=warn");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn account_toggle_persists_and_reuses_config_reload() {
    let text = format!(
        "# Keep this comment\n{}{}",
        account("a1", "enabled = true # line comment"),
        account("a2", "")
    );
    let env = Env::new(&text);
    let app = env.start(false).await;
    let (first, second) = tokio::join!(
        app.set_account_enabled("a1", false),
        app.set_account_enabled("a2", false)
    );
    for report in [first, second] {
        assert!(report.applied && report.errors.is_empty(), "{report:?}");
    }
    for status in app.stack().accounts_status() {
        assert!(!status.enabled);
        assert_eq!(status.registration, siple_sip::RegState::Disabled);
    }
    let saved = std::fs::read_to_string(env.path()).unwrap();
    assert!(saved.starts_with("# Keep this comment\n"));
    assert!(saved.contains("enabled = false # line comment"));
    assert!(app.config().accounts.iter().all(|a| !a.enabled));
    let missing = app.set_account_enabled("missing", true).await;
    assert!(!missing.applied && !missing.errors.is_empty());
    assert_eq!(std::fs::read_to_string(env.path()).unwrap(), saved);
    app.shutdown().await;

    let app = env.start(false).await;
    assert!(app.config().accounts.iter().all(|a| !a.enabled));
    let report = app.set_account_enabled("a1", true).await;
    assert!(report.applied && report.errors.is_empty(), "{report:?}");
    assert!(app.config().accounts[0].enabled);
    assert!(!app.config().accounts[1].enabled);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn account_toggle_rejects_invalid_config_without_overwriting_it() {
    let env = Env::new(&config_text(&["a1"]));
    let app = env.start(false).await;
    for bad in [
        "password = invalid-secret-should-not-leak [",
        "[[account]]\nid = 'a1'\ntransport = 'bogus'\n",
    ] {
        env.write(bad);
        let report = app.set_account_enabled("a1", false).await;
        assert!(!report.applied && !report.errors.is_empty());
        assert!(
            !report
                .errors
                .join(";")
                .contains("invalid-secret-should-not-leak")
        );
        assert!(app.config().accounts[0].enabled);
        assert_eq!(std::fs::read_to_string(env.path()).unwrap(), bad);
    }
    app.shutdown().await;
}
