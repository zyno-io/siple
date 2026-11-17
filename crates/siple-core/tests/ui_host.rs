//! `ui.*` / `app.open_uri`: "no GUI" when headless, forwarding and hand-off when a host exists.
#![cfg(unix)]

mod common;

use std::sync::{Arc, Mutex};

use common::{client, start_app};
use serde_json::{Value, json};
use siple_core::control::client::ClientError;
use siple_core::control::{ApiError, ErrorKind};
use siple_core::ui_host::{UiHost, UiRequest};

#[derive(Default)]
struct Recorder(Mutex<Vec<UiRequest>>);

impl UiHost for Recorder {
    fn request(&self, req: UiRequest) -> Result<Value, ApiError> {
        self.0.lock().unwrap().push(req);
        Ok(json!({"ok": true}))
    }
}

fn kind(e: ClientError) -> ErrorKind {
    match e {
        ClientError::Api(a) => a.kind,
        other => panic!("expected an API error, got {other}"),
    }
}

#[tokio::test]
async fn ui_methods_need_a_gui() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "").unwrap();
    let app = start_app(&path).await;
    let mut c = client(&app, "t").await;
    for m in [
        "ui.show",
        "ui.hide",
        "ui.state",
        "ui.set_panels",
        "ui.attention",
    ] {
        let e = c.call(m, json!({})).await.unwrap_err();
        assert!(e.to_string().contains("no GUI"), "{m}: {e}");
        assert_eq!(kind(e), ErrorKind::InvalidState);
    }
    // A bad link is refused before the GUI question.
    let e = c
        .call("app.open_uri", json!({"uri": "http://x"}))
        .await
        .unwrap_err();
    assert_eq!(kind(e), ErrorKind::InvalidParams);
    let e = c
        .call("app.open_uri", json!({"uri": "sip:1001@example.test"}))
        .await
        .unwrap_err();
    assert_eq!(kind(e), ErrorKind::InvalidState);
    // The hand-off client tells "headless" apart from "nobody there".
    assert_eq!(
        siple_core::uri::hand_off(dir.path(), None).await,
        siple_core::uri::HandOff::NoGui
    );
    app.shutdown().await;
    assert!(matches!(
        siple_core::uri::hand_off(dir.path(), None).await,
        siple_core::uri::HandOff::Unreachable(_)
    ));
}

#[tokio::test]
async fn ui_methods_reach_the_host_and_hand_off_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "").unwrap();
    let app = start_app(&path).await;
    let host = Arc::new(Recorder::default());
    app.set_ui_host(host.clone());
    let mut c = client(&app, "t").await;
    c.call("ui.show", json!({})).await.unwrap();
    c.call("ui.hide", json!({})).await.unwrap();
    c.call("ui.set_panels", json!({"lines_collapsed": false}))
        .await
        .unwrap();
    c.call("ui.set_panels", json!({"extensions_open": true}))
        .await
        .unwrap();
    // Unknown fields are refused before the GUI is asked.
    let e = c
        .call("ui.set_panels", json!({"rail": true}))
        .await
        .unwrap_err();
    assert_eq!(kind(e), ErrorKind::InvalidParams);
    c.call(
        "app.open_uri",
        json!({"uri": "sip:bob@example.test?Replaces=x"}),
    )
    .await
    .unwrap();
    use siple_core::uri::{HandOff, hand_off};
    assert_eq!(
        hand_off(dir.path(), Some("tel:+1-555-0100")).await,
        HandOff::Done
    );
    assert_eq!(hand_off(dir.path(), None).await, HandOff::Done);
    let got = host.0.lock().unwrap().clone();
    assert_eq!(got.len(), 7, "{got:?}");
    assert_eq!(got[0], UiRequest::Show);
    assert_eq!(got[1], UiRequest::Hide);
    assert_eq!(
        got[2],
        UiRequest::SetPanels {
            lines_collapsed: Some(false),
            extensions_open: None
        }
    );
    assert_eq!(
        got[3],
        UiRequest::SetPanels {
            lines_collapsed: None,
            extensions_open: Some(true)
        }
    );
    let UiRequest::OpenLink(l) = &got[4] else {
        panic!()
    };
    assert_eq!(l.target, "sip:bob@example.test");
    let UiRequest::OpenLink(l) = &got[5] else {
        panic!()
    };
    assert_eq!(l.target, "+15550100");
    assert_eq!(got[6], UiRequest::Show);
    app.shutdown().await;
}
