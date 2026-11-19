//! The update check on store channels through the app: the App Store lookup and the Microsoft
//! Store query come from fakes (no test touches the network or Windows), the channel is injected.
//! No unix cfg: this runs on every platform.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use siple_core::config::channel::Channel;
use siple_core::events::EventFilter;
use siple_core::update::appstore::{LookupFetcher, StoreListing};
use siple_core::update::links::MAS_STORE_URL;
use siple_core::update::{FetchError, Version};
use siple_core::{App, AppOptions, LoggingMode};

struct FakeLookup {
    calls: AtomicUsize,
    last_ua: Mutex<String>,
    version: &'static str,
}

#[async_trait]
impl LookupFetcher for FakeLookup {
    async fn lookup(&self, ua: &str) -> Result<Option<StoreListing>, FetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_ua.lock().unwrap() = ua.to_string();
        Ok(Some(StoreListing {
            version: Version::parse(self.version).unwrap(),
            released_at: Some("2098-01-01T00:00:00Z".into()),
        }))
    }
}

async fn start(
    config: &str,
    channel: Channel,
    tweak: impl FnOnce(&mut AppOptions),
) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config).unwrap();
    let mut o = AppOptions::new(dir.path().join("config.toml"));
    o.logging = LoggingMode::Detached;
    o.stderr_log = false;
    o.enable_audio = false;
    o.watch_config = false;
    o.channel = Some(channel);
    tweak(&mut o);
    let app = App::start(o).await.unwrap();
    (dir, app)
}

async fn call(app: &App, method: &str) -> serde_json::Value {
    app.call(method, serde_json::Value::Null).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mac_app_store_offers_an_open_store_action_and_announces_it() {
    let fake = Arc::new(FakeLookup {
        calls: AtomicUsize::new(0),
        last_ua: Mutex::new(String::new()),
        version: "98.0.0",
    });
    let f = fake.clone();
    let (_d, app) = start("", Channel::MAC_APP_STORE, move |o| {
        o.update_providers.lookup = f;
    })
    .await;
    let seq = app.events().current_seq();
    let s = call(&app, "app.check_updates").await;
    assert_eq!(s["source"], "app_store", "{s}");
    assert_eq!(s["action"], "open_store");
    assert_eq!(s["store_url"], MAS_STORE_URL);
    assert_eq!(
        (s["enabled"].clone(), s["available"].clone()),
        (json!(true), json!(true))
    );
    assert_eq!(s["latest"], "98.0.0");
    assert_eq!(s["channel"], "stable");
    assert!(s.get("release_url").is_none(), "{s}");
    assert_eq!(
        *fake.last_ua.lock().unwrap(),
        format!("Siple/{}", siple_core::update::full_version())
    );
    let ev = app
        .events()
        .since(seq, &EventFilter::kinds(["app.update_available"]), 10);
    assert_eq!(ev.len(), 1);
    assert_eq!(
        ev[0].data,
        json!({"version": "98.0.0", "url": MAS_STORE_URL, "action": "open_store"})
    );
    // `status.update` carries the same action and link.
    let st = call(&app, "status").await;
    assert_eq!(st["update"]["action"], "open_store");
    assert_eq!(st["update"]["store_url"], MAS_STORE_URL);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn check_false_turns_the_store_check_off_too() {
    let fake = Arc::new(FakeLookup {
        calls: AtomicUsize::new(0),
        last_ua: Mutex::new(String::new()),
        version: "98.0.0",
    });
    let f = fake.clone();
    let (_d, app) = start(
        "[updates]\ncheck = false\n",
        Channel::MAC_APP_STORE,
        move |o| {
            o.update_providers.lookup = f;
        },
    )
    .await;
    let s = call(&app, "app.check_updates").await;
    assert_eq!(s["enabled"], false);
    assert_eq!(s["source"], "app_store");
    assert_eq!(s["available"], false);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    app.shutdown().await;
}

#[cfg(feature = "store")]
mod microsoft_store {
    use siple_core::update::links::MS_STORE_URL;
    use siple_core::update::msstore::{SignatureKind, StoreQuery, StoreUpdates};

    use super::*;

    struct FakeStore {
        kind: SignatureKind,
        updates: StoreUpdates,
        calls: AtomicUsize,
        last_pfn: Mutex<String>,
    }

    #[async_trait]
    impl StoreQuery for FakeStore {
        fn signature_kind(&self) -> SignatureKind {
            self.kind
        }
        async fn updates(&self, pfn: &str) -> Result<StoreUpdates, FetchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_pfn.lock().unwrap() = pfn.to_string();
            Ok(self.updates)
        }
    }

    const PFN: &str = "ZynoIO.Siple_abc123";

    fn fake(kind: SignatureKind, available: bool, mandatory: bool) -> Arc<FakeStore> {
        Arc::new(FakeStore {
            kind,
            updates: StoreUpdates {
                available,
                mandatory,
            },
            calls: AtomicUsize::new(0),
            last_pfn: Mutex::new(String::new()),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_signed_package_gets_a_versionless_offer_announced_once() {
        let q = fake(SignatureKind::Store, true, true);
        let qq = q.clone();
        let (_d, app) = start("", Channel::msix(PFN, None), move |o| {
            o.update_providers.store_query = qq;
        })
        .await;
        let seq = app.events().current_seq();
        let s = call(&app, "app.check_updates").await;
        assert_eq!(s["source"], "microsoft_store", "{s}");
        assert_eq!(s["action"], "open_store");
        assert_eq!(s["store_url"], MS_STORE_URL);
        assert_eq!(s["available"], true);
        assert_eq!(s["mandatory"], true);
        assert!(s.get("latest").is_none(), "{s}");
        assert_eq!(*q.last_pfn.lock().unwrap(), PFN);
        let ev = app
            .events()
            .since(seq, &EventFilter::kinds(["app.update_available"]), 10);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            ev[0].data,
            json!({"version": null, "url": MS_STORE_URL, "action": "open_store"})
        );
        app.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sideloaded_package_never_asks_the_store() {
        let q = fake(SignatureKind::Developer, true, false);
        let qq = q.clone();
        let (_d, app) = start("", Channel::msix(PFN, None), move |o| {
            o.update_providers.store_query = qq;
        })
        .await;
        let s = call(&app, "app.check_updates").await;
        assert_eq!(s["enabled"], false);
        assert_eq!(s["source"], "none");
        assert_eq!(s["disabled_reason"], "sideloaded");
        assert_eq!(q.calls.load(Ordering::SeqCst), 0);
        app.shutdown().await;
    }
}

/// A direct build inside a package does not check: the store owns the updates.
#[cfg(not(feature = "store"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_direct_build_in_an_msix_package_is_disabled() {
    let (_d, app) = start("", Channel::msix("ZynoIO.Siple_abc123", None), |_| {}).await;
    let s = call(&app, "app.check_updates").await;
    assert_eq!(s["enabled"], false);
    assert_eq!(s["source"], "none");
    assert_eq!(s["disabled_reason"], "direct_build_in_package");
    app.shutdown().await;
}
