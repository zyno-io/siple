//! `provision.parse` / `provision.build` and `app.open_uri` with setup links.
#![cfg(unix)]

mod common;

use common::{client, start_app};
use serde_json::{Value, json};
use siple_core::control::client::ClientError;
use siple_core::control::{ApiError, ErrorKind};

const EXAMPLE: &str = "siple:provision?d=eyJ2IjoxLCJhY2NvdW50Ijp7InVzZXJuYW1lIjoiMTAwMSIsImRvbWFpbiI6ImV4YW1wbGUudGVzdCIsImRpc3BsYXlfbmFtZSI6IkZyb250IGRlc2siLCJyZWdpc3RyYXIiOiJwYnguZXhhbXBsZS50ZXN0OjUwNjEiLCJ0cmFuc3BvcnQiOiJ0bHMiLCJzcnRwIjoibWFuZGF0b3J5In19";

fn api(e: ClientError) -> ApiError {
    match e {
        ClientError::Api(a) => a,
        other => panic!("expected an API error, got {other}"),
    }
}

#[tokio::test]
async fn parse_is_pure_and_its_draft_adds_an_account() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "").unwrap();
    let app = start_app(&path).await;
    let mut c = client(&app, "t").await;
    let before = std::fs::read(&path).unwrap();
    let r = c
        .call("provision.parse", json!({"link": EXAMPLE}))
        .await
        .unwrap();
    assert_eq!(r["version"], 1);
    assert_eq!(r["account"]["username"], "1001");
    assert_eq!(r["next_hop"], "pbx.example.test:5061");
    assert_eq!(r["warnings"], json!([]));
    assert_eq!(r["existing"], Value::Null);
    assert_eq!(r["draft"]["id"], "1001.example.test");
    assert_eq!(r["draft"]["fields"]["transport"], "tls");
    assert!(r["draft"]["fields"].get("password").is_none());
    assert_eq!(std::fs::read(&path).unwrap(), before, "parse wrote a file");
    assert_eq!(
        c.call("account.list", json!({})).await.unwrap()["accounts"],
        json!([])
    );

    // The draft is accepted as-is by config.edit add_account.
    let draft = r["draft"].clone();
    let e = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"add_account","draft":draft}]}),
        )
        .await
        .unwrap();
    assert_eq!(e["applied"], true, "{e}");
    let v = c.call("config.view", json!({})).await.unwrap();
    assert_eq!(v["config"]["account"][0]["id"], "1001.example.test");
    assert_eq!(v["config"]["account"][0]["srtp"], "mandatory");

    // Now the link is a duplicate.
    let r = c
        .call("provision.parse", json!({"link": EXAMPLE}))
        .await
        .unwrap();
    assert_eq!(r["existing"], "1001.example.test");

    // Errors carry a stable code and never the value.
    let e = api(c
        .call("provision.parse", json!({"link": "sip:a@b.test"}))
        .await
        .unwrap_err());
    assert_eq!(e.kind, ErrorKind::InvalidParams);
    assert_eq!(e.extra.as_ref().unwrap()["code"], "not_a_setup_link");
    assert_eq!(e.extra.as_ref().unwrap()["reason"], "invalid_link");
    app.shutdown().await;
}

#[tokio::test]
async fn build_round_trips_and_never_carries_a_password() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[[account]]\nid = \"desk\"\nusername = \"1001\"\npassword = \"hunter2\"\n\
         domain = \"127.0.0.1\"\ntransport = \"udp\"\nregister = false\nbind_address = \"127.0.0.1\"\n\
         tls_fingerprint = \"\"\nsrtp = \"mandatory\"\n",
    )
    .unwrap();
    let app = start_app(&path).await;
    let mut c = client(&app, "t").await;

    // From fields.
    let r = c
        .call(
            "provision.build",
            json!({"fields": {"username": "1001", "domain": "Example.Test", "transport": "tls"}}),
        )
        .await
        .unwrap();
    let link = r["link"].as_str().unwrap().to_string();
    assert_eq!(r["length"], link.len());
    assert_eq!(r["fields"]["domain"], "example.test");
    assert_eq!(r["omitted"], json!([]));
    let p = c
        .call("provision.parse", json!({"link": link}))
        .await
        .unwrap();
    assert_eq!(p["account"], r["fields"]);

    // A password is refused.
    let e = api(c
        .call(
            "provision.build",
            json!({"fields": {"username": "1001", "domain": "a.test", "password": "x"}}),
        )
        .await
        .unwrap_err());
    assert_eq!(e.kind, ErrorKind::InvalidParams);
    assert_eq!(e.extra.as_ref().unwrap()["code"], "password_not_allowed");
    assert!(!e.message.contains('x') || !e.message.contains("\"x\""));

    // From an account that has a password.
    let r = c
        .call("provision.build", json!({"account": "desk"}))
        .await
        .unwrap();
    let text = r["link"].as_str().unwrap();
    assert!(!text.contains("hunter2"));
    assert!(r["fields"].get("password").is_none());
    assert_eq!(r["fields"]["srtp"], "mandatory");
    let omitted = r["omitted"].as_array().unwrap();
    assert!(omitted.contains(&json!("password")), "{omitted:?}");
    assert!(omitted.contains(&json!("bind_address")), "{omitted:?}");
    assert!(!omitted.contains(&json!("username")), "{omitted:?}");
    assert!(!omitted.contains(&json!("id")), "{omitted:?}");
    let p = c
        .call("provision.parse", json!({"link": text}))
        .await
        .unwrap();
    assert_eq!(p["account"], r["fields"]);

    // Selectors.
    let e = api(c
        .call("provision.build", json!({"account": "nope"}))
        .await
        .unwrap_err());
    assert_eq!(e.kind, ErrorKind::NotFound);
    let e = api(c.call("provision.build", json!({})).await.unwrap_err());
    assert_eq!(e.kind, ErrorKind::InvalidParams);
    let e = api(c
        .call(
            "provision.build",
            json!({"account": "desk", "fields": {"username": "a", "domain": "b.test"}}),
        )
        .await
        .unwrap_err());
    assert_eq!(e.kind, ErrorKind::InvalidParams);
    app.shutdown().await;
}

#[tokio::test]
async fn open_uri_headless_has_no_effect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "").unwrap();
    let app = start_app(&path).await;
    let mut c = client(&app, "t").await;
    let before = std::fs::read(&path).unwrap();
    let e = api(c
        .call("app.open_uri", json!({"uri": EXAMPLE}))
        .await
        .unwrap_err());
    assert_eq!(e.kind, ErrorKind::InvalidState);
    assert!(e.message.contains("no GUI"), "{}", e.message);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    app.shutdown().await;
}

#[tokio::test]
async fn catalog_lists_the_methods() {
    let names: Vec<String> = siple_core::backend::method_catalog()
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(names.contains(&"provision.parse".to_string()));
    assert!(names.contains(&"provision.build".to_string()));
    let open = siple_core::backend::method_catalog()
        .into_iter()
        .find(|m| m.name == "app.open_uri")
        .unwrap();
    assert_eq!(open.params_schema["properties"]["uri"]["maxLength"], 2048);
}
