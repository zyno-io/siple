//! In-place install through the control API (`app.install_update`, `app.cancel_update`, the
//! progress events, the restart request to the GUI). The release list, the downloads and the
//! platform are fakes: no test touches the network or replaces a file.
#![cfg(unix)]

use siple_core::control::client::ControlClient;
use siple_core::{App, AppOptions, LoggingMode};

async fn client(app: &App) -> ControlClient {
    let ep = app.control_endpoints().await;
    let mut c = ControlClient::connect_unix(ep.unix_socket.unwrap())
        .await
        .unwrap();
    c.hello(Some(app.token().expose()), "test").await.unwrap();
    c
}

async fn start(o: impl FnOnce(&mut AppOptions)) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "").unwrap();
    let mut opts = AppOptions::new(dir.path().join("config.toml"));
    opts.logging = LoggingMode::Detached;
    opts.stderr_log = false;
    opts.enable_audio = false;
    opts.watch_config = false;
    o(&mut opts);
    (dir, App::start(opts).await.unwrap())
}

#[cfg(feature = "store")]
#[tokio::test(flavor = "multi_thread")]
async fn store_builds_report_unsupported_and_have_no_installer() {
    use serde_json::json;
    let (_d, app) = start(|_| {}).await;
    let mut c = client(&app).await;
    for m in ["app.install_update", "app.cancel_update"] {
        let e = c.call(m, json!({"version": "99.0.0"})).await.expect_err(m);
        assert!(
            format!("{e:?}").to_lowercase().contains("unsupported"),
            "{m}: {e:?}"
        );
    }
    app.shutdown().await;
}

#[cfg(not(feature = "store"))]
mod direct {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};
    use siple_core::control::ApiError;
    use siple_core::events::EventFilter;

    use async_trait::async_trait;
    use sha2::{Digest, Sha256};
    use siple_core::state::InstalledUpdate;
    use siple_core::ui_host::{RelaunchSpec, UiHost, UiRequest};
    use siple_core::update::download::{AssetFetcher, Expected};
    use siple_core::update::install::{
        Committed, Expect, InstallError, InstallProviders, NotPossible, Platform, Staged, Target,
    };
    use siple_core::update::sums::AssetPlatform;
    use siple_core::update::{FetchError, FetchOutcome, ReleaseFetcher, Version};
    use tokio_util::sync::CancellationToken;

    use super::*;

    const KEY: &str = include_str!("fixtures/update/test-key.pub");
    const SUMS: &[u8] = include_bytes!("fixtures/update/SHA256SUMS.99.0.0");
    const SIG: &[u8] = include_bytes!("fixtures/update/SHA256SUMS.99.0.0.minisig");
    const ARCHIVE: &str = "Siple-99.0.0-macos.zip";

    fn content() -> Vec<u8> {
        format!("99.0.0/{ARCHIVE}").into_bytes()
    }

    struct List;
    #[async_trait]
    impl ReleaseFetcher for List {
        async fn fetch(&self, _: Option<&str>, _: &str) -> Result<FetchOutcome, FetchError> {
            let body = json!([{
                "tag_name": "v99.0.0",
                "assets": [
                    {"name": "SHA256SUMS", "size": SUMS.len(), "state": "uploaded"},
                    {"name": "SHA256SUMS.minisig", "size": SIG.len(), "state": "uploaded"},
                    {"name": ARCHIVE, "size": content().len(), "state": "uploaded"},
                ]
            }]);
            Ok(FetchOutcome::Body {
                etag: None,
                body: body.to_string().into_bytes(),
            })
        }
    }

    struct Assets;
    #[async_trait]
    impl AssetFetcher for Assets {
        async fn fetch(
            &self,
            name: &str,
            _: &Version,
            dest: &Path,
            expect: &Expected,
            progress: &(dyn Fn(u64) + Send + Sync),
            _: &CancellationToken,
        ) -> Result<[u8; 32], InstallError> {
            let body = match name {
                "SHA256SUMS" => SUMS.to_vec(),
                "SHA256SUMS.minisig" => SIG.to_vec(),
                _ => content(),
            };
            assert_eq!(body.len() as u64, expect.size);
            std::fs::write(dest, &body).unwrap();
            progress(body.len() as u64);
            Ok(Sha256::digest(&body).into())
        }
    }

    struct Plat {
        root: PathBuf,
        committed: AtomicUsize,
    }
    impl Platform for Plat {
        fn asset(&self) -> Option<AssetPlatform> {
            Some(AssetPlatform::MacosZip)
        }
        fn probe(&self) -> Result<Target, NotPossible> {
            Ok(Target {
                root: self.root.clone(),
                exe: self.root.join("siple"),
            })
        }
        fn stage_dir(&self, _: &Target) -> Result<PathBuf, InstallError> {
            let d = self.root.join("stage");
            std::fs::create_dir_all(&d).unwrap();
            Ok(d)
        }
        fn stage(
            &self,
            _: &Path,
            d: &Path,
            _: &Target,
            _: &Expect,
        ) -> Result<Staged, InstallError> {
            Ok(Staged {
                dir: d.to_path_buf(),
                payload: d.join("new"),
            })
        }
        fn commit(&self, s: &Staged, t: &Target) -> Result<Committed, InstallError> {
            self.committed.fetch_add(1, Ordering::SeqCst);
            Ok(Committed {
                relaunch: RelaunchSpec::Exe(t.exe.clone()),
                cleanup: vec![s.dir.clone()],
            })
        }
        fn recover(&self) {}
        fn cleanup(&self, _: &InstalledUpdate) {}
        fn discard(&self, d: &Path) {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    struct Gui(Mutex<Vec<RelaunchSpec>>);
    impl UiHost for Gui {
        fn request(&self, req: UiRequest) -> Result<Value, ApiError> {
            if let UiRequest::RelaunchForUpdate(s) = req {
                self.0.lock().unwrap().push(s);
            }
            Ok(json!({}))
        }
    }

    async fn rig(with_gui: bool) -> (tempfile::TempDir, App, Arc<Plat>, Arc<Gui>) {
        let root = tempfile::tempdir().unwrap();
        let plat = Arc::new(Plat {
            root: root.path().to_path_buf(),
            committed: AtomicUsize::new(0),
        });
        let p = plat.clone();
        let (d, app) = start(move |o| {
            o.update_providers.github = Arc::new(List);
            o.update_providers.install = InstallProviders {
                assets: Arc::new(Assets),
                platform: p,
                keys: vec![KEY.lines().nth(1).unwrap().to_string()],
                quiet: Duration::from_millis(100),
                poll: Duration::from_millis(20),
                cleanup_delay: Duration::from_secs(3600),
            };
        })
        .await;
        std::mem::forget(root); // the fake platform's root lives for the test process
        let gui = Arc::new(Gui(Mutex::default()));
        if with_gui {
            app.set_ui_host(gui.clone());
        }
        (d, app, plat, gui)
    }

    async fn wait_state(c: &mut ControlClient, want: &str) -> Value {
        for _ in 0..200 {
            let s = c.call("app.update_status", Value::Null).await.unwrap();
            if s["install"]["state"] == want {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "never reached {want}: {}",
            c.call("app.update_status", Value::Null).await.unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_runs_end_to_end_and_asks_the_gui_to_restart() {
        let (_d, app, plat, gui) = rig(true).await;
        let mut c = client(&app).await;
        let seq = app.events().current_seq();
        let s = c.call("app.check_updates", Value::Null).await.unwrap();
        assert_eq!(s["latest"], "99.0.0");
        assert_eq!(s["action"], "install", "{s}");
        assert_eq!(s["install"]["possible"], true);
        assert_eq!(s["install"]["state"], "idle");
        let ev = app
            .events()
            .since(seq, &EventFilter::kinds(["app.update_available"]), 10);
        assert_eq!(ev[0].data["action"], "install");

        // Wrong version: refused, with the offered one named.
        let e = c
            .call("app.install_update", json!({"version": "98.0.0"}))
            .await
            .unwrap_err();
        let text = format!("{e:?}");
        assert!(
            text.contains("not_offered") && text.contains("99.0.0"),
            "{text}"
        );
        // Bad restart value, unknown parameter.
        assert!(
            c.call(
                "app.install_update",
                json!({"version": "99.0.0", "restart": "soon"})
            )
            .await
            .is_err()
        );
        assert!(
            c.call("app.install_update", json!({"version": "99.0.0", "x": 1}))
                .await
                .is_err()
        );
        assert!(c.call("app.install_update", json!({})).await.is_err());

        let r = c
            .call("app.install_update", json!({"version": "99.0.0"}))
            .await
            .unwrap();
        assert_eq!(r["install"]["version"], "99.0.0");
        let s = wait_state(&mut c, "restarting").await;
        assert_eq!(s["action"], "install");
        assert_eq!(plat.committed.load(Ordering::SeqCst), 1);
        assert_eq!(gui.0.lock().unwrap().len(), 1);
        let evs = app
            .events()
            .since(seq, &EventFilter::kinds(["app.update_progress"]), 100);
        let states: Vec<_> = evs
            .iter()
            .map(|e| e.data["state"].as_str().unwrap().to_string())
            .collect();
        assert!(
            states.contains(&"downloading".to_string()) && states.last().unwrap() == "restarting",
            "{states:?}"
        );
        // The summary in `status`.
        let st = c.call("status", Value::Null).await.unwrap();
        assert_eq!(st["update"]["install_state"], "restarting");
        assert_eq!(st["update"]["action"], "install");
        // Too late to cancel.
        let e = c.call("app.cancel_update", Value::Null).await.unwrap_err();
        assert!(format!("{e:?}").contains("too_late"));
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn manual_restart_waits_and_cancel_discards() {
        let (_d, app, plat, gui) = rig(true).await;
        let mut c = client(&app).await;
        c.call("app.check_updates", Value::Null).await.unwrap();
        // Nothing to cancel yet.
        let r = c.call("app.cancel_update", Value::Null).await.unwrap();
        assert_eq!(r["cancelled"], false);
        c.call(
            "app.install_update",
            json!({"version": "99.0.0", "restart": "manual"}),
        )
        .await
        .unwrap();
        let s = wait_state(&mut c, "ready").await;
        assert_eq!(s["install"]["restart"], "manual");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            plat.committed.load(Ordering::SeqCst),
            0,
            "manual never swaps by itself"
        );
        let r = c.call("app.cancel_update", Value::Null).await.unwrap();
        assert_eq!(r["cancelled"], true);
        wait_state(&mut c, "idle").await;
        assert!(gui.0.lock().unwrap().is_empty());
        assert!(
            !plat.root.join("stage").exists(),
            "the staging area is gone"
        );
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_headless_instance_cannot_install_and_says_why() {
        let (_d, app, _plat, _gui) = rig(false).await;
        let mut c = client(&app).await;
        let s = c.call("app.check_updates", Value::Null).await.unwrap();
        assert_eq!(s["action"], "open_release_page");
        assert_eq!(s["install"]["possible"], false);
        assert_eq!(s["install"]["reason"], "headless");
        let e = c
            .call("app.install_update", json!({"version": "99.0.0"}))
            .await
            .unwrap_err();
        let text = format!("{e:?}");
        assert!(
            text.contains("install_not_possible")
                && text.contains("headless")
                && text.contains("releases/tag/v99.0.0"),
            "{text}"
        );
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_off_in_the_config_is_refused_and_hot() {
        let (d, app, _plat, _gui) = rig(true).await;
        let mut c = client(&app).await;
        c.call("app.check_updates", Value::Null).await.unwrap();
        c.call(
            "config.edit",
            json!({"edits": [{"op": "set_section", "section": "updates", "key": "install", "value": false}]}),
        )
        .await
        .unwrap();
        let s = c.call("app.update_status", Value::Null).await.unwrap();
        assert_eq!(s["action"], "open_release_page");
        assert_eq!(s["install"]["reason"], "disabled");
        let e = c
            .call("app.install_update", json!({"version": "99.0.0"}))
            .await
            .unwrap_err();
        assert!(format!("{e:?}").contains("disabled"));
        assert!(
            std::fs::read_to_string(d.path().join("config.toml"))
                .unwrap()
                .contains("install = false")
        );
        app.shutdown().await;
    }
}
