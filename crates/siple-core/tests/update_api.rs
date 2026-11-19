//! The update check through the control API: `app.update_status`, `app.check_updates`, the
//! `status.update` summary, `[updates]` hot reload and the `app.update_available` event. The
//! releases list comes from a fake fetcher: no test touches the network.
#![cfg(unix)]

use siple_core::App;
use siple_core::control::client::ControlClient;

async fn client(app: &App) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "test").await.unwrap();
    c
}

#[cfg(not(feature = "store"))]
mod github {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use siple_core::events::EventFilter;
    use siple_core::update::{FetchError, FetchOutcome, ReleaseFetcher};
    use siple_core::{App, AppOptions, LoggingMode};

    use super::client;

    const RELEASES: &str = r#"[
  {"tag_name":"v99.0.0-rc.1","draft":false,"prerelease":true,"html_url":"https://github.com/zyno-io/siple/releases/tag/v99.0.0-rc.1","published_at":"2099-01-01T00:00:00Z"},
  {"tag_name":"v98.0.0","draft":false,"prerelease":false,"html_url":"https://github.com/zyno-io/siple/releases/tag/v98.0.0","published_at":"2098-01-01T00:00:00Z"},
  {"tag_name":"v97.0.0","draft":true,"prerelease":false,"html_url":"https://github.com/zyno-io/siple/releases/tag/v97.0.0"},
  {"tag_name":"v0.0.1","draft":false,"prerelease":false,"html_url":"https://github.com/zyno-io/siple/releases/tag/v0.0.1"}
]"#;

    struct Fake {
        calls: AtomicUsize,
        last_ua: Mutex<String>,
        fail: bool,
    }

    #[async_trait]
    impl ReleaseFetcher for Fake {
        async fn fetch(&self, _etag: Option<&str>, ua: &str) -> Result<FetchOutcome, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_ua.lock().unwrap() = ua.to_string();
            if self.fail {
                return Err(FetchError::Network("offline".into()));
            }
            Ok(FetchOutcome::Body {
                etag: Some("\"e\"".into()),
                body: RELEASES.as_bytes().to_vec(),
            })
        }
    }

    async fn start(config: &str, fail: bool) -> (tempfile::TempDir, App, Arc<Fake>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let fake = Arc::new(Fake {
            calls: AtomicUsize::new(0),
            last_ua: Mutex::new(String::new()),
            fail,
        });
        let mut o = AppOptions::new(dir.path().join("config.toml"));
        o.logging = LoggingMode::Detached;
        o.stderr_log = false;
        o.enable_audio = false;
        o.watch_config = false;
        o.update_providers.github = fake.clone();
        let app = App::start(o).await.unwrap();
        (dir, app, fake)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_before_any_check_is_quiet() {
        let (_d, app, fake) = start("", false).await;
        let mut c = client(&app).await;
        let s = c.call("app.update_status", Value::Null).await.unwrap();
        assert_eq!(s["enabled"], true);
        assert_eq!(s["channel"], "stable");
        assert_eq!(
            (s["source"].as_str(), s["action"].as_str()),
            (Some("github"), Some("none"))
        );
        assert_eq!(s["current"], siple_core::update::full_version());
        assert_eq!(s["available"], false);
        for k in [
            "latest",
            "release_url",
            "store_url",
            "disabled_reason",
            "mandatory",
            "published_at",
            "checked_at",
            "error",
        ] {
            assert!(s.get(k).is_none(), "{k} must be absent: {s}");
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0, "no check within 30 s");
        let st = c.call("status", Value::Null).await.unwrap();
        assert_eq!(st["version"], siple_core::update::full_version());
        assert_eq!(st["update"]["enabled"], true);
        assert_eq!(st["update"]["available"], false);
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn check_updates_finds_the_release_announces_it_and_is_rate_limited() {
        let (_d, app, fake) = start("", false).await;
        let mut c = client(&app).await;
        let seq = app.events().current_seq();
        let s = c.call("app.check_updates", Value::Null).await.unwrap();
        assert_eq!(s["available"], true, "{s}");
        assert_eq!(
            s["latest"], "98.0.0",
            "stable channel skips the rc and the draft"
        );
        assert_eq!(
            s["release_url"],
            "https://github.com/zyno-io/siple/releases/tag/v98.0.0"
        );
        assert_eq!(s["published_at"], "2098-01-01T00:00:00Z");
        assert!(s["checked_at"].is_string() && s.get("error").is_none());
        assert_eq!(
            *fake.last_ua.lock().unwrap(),
            format!("Siple/{}", siple_core::update::full_version())
        );
        // The same answer without network from update_status, and in status.
        let s2 = c.call("app.update_status", Value::Null).await.unwrap();
        assert_eq!(s2, s);
        let st = c.call("status", Value::Null).await.unwrap();
        assert_eq!(st["update"]["latest"], "98.0.0");
        assert_eq!(st["update"]["available"], true);
        // Announced once.
        let ev = app
            .events()
            .since(seq, &EventFilter::kinds(["app.update_available"]), 10);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0].data,
            json!({"version": "98.0.0", "url": s["release_url"], "action": "open_release_page"})
        );
        // A second forced check within a minute does not fetch again nor announce again.
        let again = c.call("app.check_updates", Value::Null).await.unwrap();
        assert_eq!(again, s);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.events()
                .since(seq, &EventFilter::kinds(["app.update_available"]), 10)
                .len(),
            1
        );
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failures_are_in_the_status_not_an_error() {
        let (_d, app, _fake) = start("", true).await;
        let mut c = client(&app).await;
        let s = c.call("app.check_updates", Value::Null).await.unwrap();
        assert_eq!(s["enabled"], true);
        assert_eq!(s["available"], false);
        assert_eq!(s["error"], "offline");
        assert!(s.get("checked_at").is_none());
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disabled_in_config_never_fetches() {
        let (_d, app, fake) = start("[updates]\ncheck = false\n", false).await;
        let mut c = client(&app).await;
        let s = c.call("app.check_updates", Value::Null).await.unwrap();
        assert_eq!(s["enabled"], false);
        assert_eq!(s["available"], false);
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        let st = c.call("status", Value::Null).await.unwrap();
        assert_eq!(st["update"]["enabled"], false);
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_edit_switches_the_channel_and_the_check_hot() {
        let (_d, app, _fake) = start("", false).await;
        let mut c = client(&app).await;
        c.call("app.check_updates", Value::Null).await.unwrap();
        let edit = |key: &str, value: Value| json!({"edits":[{"op":"set_section","section":"updates","key":key,"value":value}]});
        // prerelease channel: the release candidate is newest; no new fetch needed.
        let r = c
            .call("config.edit", edit("channel", json!("prerelease")))
            .await
            .unwrap();
        assert_eq!(r["applied"], true, "{r}");
        let s = c.call("app.update_status", Value::Null).await.unwrap();
        assert_eq!(
            (s["channel"].as_str(), s["latest"].as_str()),
            (Some("prerelease"), Some("99.0.0-rc.1"))
        );
        // Back to automatic.
        let r = c
            .call(
                "config.edit",
                json!({"edits":[{"op":"reset_section","section":"updates","key":"channel"}]}),
            )
            .await
            .unwrap();
        assert_eq!(r["applied"], true, "{r}");
        assert_eq!(
            c.call("app.update_status", Value::Null).await.unwrap()["channel"],
            "stable"
        );
        // Switching the check off hides the result.
        c.call("config.edit", edit("check", json!(false)))
            .await
            .unwrap();
        let s = c.call("app.update_status", Value::Null).await.unwrap();
        assert_eq!(
            (s["enabled"].as_bool(), s["available"].as_bool()),
            (Some(false), Some(false))
        );
        // A bad channel is refused like any other edit.
        let e = c
            .call("config.edit", edit("channel", json!("nightly")))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("channel"), "{e}");
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn methods_are_in_the_catalog() {
        let (_d, app, _fake) = start("", false).await;
        let mut c = client(&app).await;
        let m = c.call("methods.list", Value::Null).await.unwrap();
        let names: Vec<&str> = m["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"app.update_status") && names.contains(&"app.check_updates"));
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skipped_version_is_remembered_across_restarts() {
        let (dir, app, _fake) = start("", false).await;
        assert_eq!(app.skipped_update(), None);
        app.skip_update("98.0.0").unwrap();
        assert_eq!(app.skipped_update().as_deref(), Some("98.0.0"));
        // The control API is unfiltered: agents still see the release.
        app.check_updates().await;
        assert!(app.update_status().available);
        app.shutdown().await;
        let mut o = AppOptions::new(dir.path().join("config.toml"));
        o.logging = LoggingMode::Detached;
        o.stderr_log = false;
        o.enable_audio = false;
        o.watch_config = false;
        let app = App::start(o).await.unwrap();
        assert_eq!(app.skipped_update().as_deref(), Some("98.0.0"));
        app.shutdown().await;
    }
}

/// A store build run outside any store package has no update source at all.
#[cfg(feature = "store")]
#[tokio::test(flavor = "multi_thread")]
async fn a_store_build_outside_a_store_does_not_check() {
    use serde_json::Value;
    use siple_core::{AppOptions, LoggingMode};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "").unwrap();
    let mut o = AppOptions::new(dir.path().join("config.toml"));
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    o.channel = Some(siple_core::config::channel::Channel::DIRECT);
    let app = App::start(o).await.unwrap();
    let mut c = client(&app).await;
    let s = c.call("app.check_updates", Value::Null).await.unwrap();
    assert_eq!(s["enabled"], false);
    assert_eq!(s["source"], "none");
    assert_eq!(s["action"], "none");
    assert_eq!(s["disabled_reason"], "store_build_outside_store");
    assert_eq!(s["available"], false);
    app.shutdown().await;
}
