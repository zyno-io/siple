//! `password_store = "keyring"` and `account.test` through a real `App`, against an in-memory
//! `keyring-core` mock store (the real Keychain is never touched): resolution by the loader,
//! `account.set_password`, store moves/renames/removals through `config.edit`, the loud failure
//! without a credential store, and the one-off REGISTER diagnosis API.
#![cfg(unix)]

use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use siple_core::config::secrets::{self, KeyringVault, Vault, VaultError};
use siple_core::control::client::{ClientError, ControlClient};
use siple_core::{App, AppOptions, LoggingMode};

/// A mock credential store shared by every test of this binary; accounts of the tests are
/// unique, so they do not interfere. Account ids starting with `nokr` act as a machine without a
/// credential store.
struct TestVault {
    inner: KeyringVault,
}

impl Vault for TestVault {
    fn get(&self, id: &str) -> Result<Option<String>, VaultError> {
        if id.starts_with("nokr") {
            return Err(VaultError::Unavailable);
        }
        self.inner.get(id)
    }
    fn set(&self, id: &str, pw: &str) -> Result<(), VaultError> {
        if id.starts_with("nokr") {
            return Err(VaultError::Unavailable);
        }
        self.inner.set(id, pw)
    }
    fn delete(&self, id: &str) -> Result<(), VaultError> {
        if id.starts_with("nokr") {
            return Err(VaultError::Unavailable);
        }
        self.inner.delete(id)
    }
}

fn vault() -> Arc<dyn Vault> {
    static V: OnceLock<Arc<TestVault>> = OnceLock::new();
    let v = V
        .get_or_init(|| {
            Arc::new(TestVault {
                inner: KeyringVault::with_store(keyring_core::mock::Store::new().unwrap()),
            })
        })
        .clone();
    secrets::set_vault(Some(v.clone()));
    v
}

// --- fake registrar -----------------------------------------------------------------------------

/// UDP registrar: digest 401 for a request without `Authorization`, then `code` (200 grants
/// 300 s). `code = 0` stays silent after the challenge.
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

fn registrar(code: u16) -> Registrar {
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
            let authed = req.to_ascii_lowercase().contains("\nauthorization:");
            let status = if !authed {
                401
            } else if code == 0 {
                continue;
            } else {
                code
            };
            let mut lines = vec![format!("SIP/2.0 {status} X")];
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
                } else if low.starts_with("contact:") && status == 200 {
                    let c = l.split_once(':').unwrap().1.trim();
                    lines.push(format!("Contact: {c};expires=300"));
                }
            }
            if status == 401 {
                lines.push(
                    "WWW-Authenticate: Digest realm=\"fakereg\", nonce=\"servernonce0123456789\", algorithm=MD5, qop=\"auth\""
                        .into(),
                );
            }
            lines.push("Content-Length: 0".into());
            let _ = sock.send_to(format!("{}\r\n\r\n", lines.join("\r\n")).as_bytes(), from);
        }
    });
    r
}

// --- env ---------------------------------------------------------------------------------------

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
    async fn start(&self) -> App {
        let mut o = AppOptions::new(self.path());
        o.logging = LoggingMode::Detached;
        o.stderr_log = false;
        o.enable_audio = false;
        o.watch_config = false;
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

fn account(id: &str, reg: &Registrar, extra: &str) -> String {
    format!(
        "[[account]]\nid = \"{id}\"\nusername = \"{id}\"\ndomain = \"127.0.0.1\"\n\
         registrar = \"{}\"\ntransport = \"udp\"\nkeepalive_secs = 0\n{extra}\n",
        reg.addr
    )
}

async fn account_state(c: &mut ControlClient, id: &str) -> Value {
    let s = c.call("status", Value::Null).await.unwrap();
    s["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == id)
        .unwrap_or_else(|| panic!("no account {id}: {s}"))
        .clone()
}

async fn wait_registered(c: &mut ControlClient, id: &str) {
    for _ in 0..250 {
        if account_state(c, id).await["registration"]["state"] == "registered" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{id} not registered: {}", account_state(c, id).await);
}

async fn api_err(c: &mut ControlClient, method: &str, params: Value) -> Value {
    match c.call(method, params).await {
        Err(ClientError::Api(e)) => e.to_wire(),
        other => panic!("expected an API error, got {other:?}"),
    }
}

fn set(id: &str, field: &str, value: Value) -> Value {
    json!({"op":"set_account","id":id,"field":field,"value":value})
}

const PW: &str = "kr-s3cret-4711";

// --- loader --------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_loader_fills_the_password_from_the_keyring_once_and_never_leaks_it() {
    let v = vault();
    v.set("kr-load", PW).unwrap();
    let reg = registrar(200);
    let env = Env::new(&account("kr-load", &reg, "password_store = \"keyring\""));
    let app = env.start().await;
    let mut c = client(&app).await;
    wait_registered(&mut c, "kr-load").await;
    // In memory the password is there (the digest answer used it); on disk it never was.
    assert_eq!(app.config().accounts[0].password.expose(), PW);
    assert!(!env.text().contains(PW));

    let view = c.call("config.view", Value::Null).await.unwrap();
    assert_eq!(view["config"]["account"][0]["password_store"], "keyring");
    assert_eq!(
        view["config"]["account"][0]["password"],
        json!({"set": true})
    );
    let get = c.call("config.get", Value::Null).await.unwrap();
    let acct = c
        .call("account.get", json!({"account":"kr-load"}))
        .await
        .unwrap();
    for out in [view.to_string(), get.to_string(), acct.to_string()] {
        assert!(!out.contains(PW), "password leaked: {out}");
    }
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_keyring_entry_fails_that_account_loudly_and_only_that_one() {
    vault();
    let reg = registrar(200);
    let env = Env::new(&format!(
        "{}{}",
        account("kr-missing", &reg, "password_store = \"keyring\""),
        account("kr-file", &reg, "password = \"filepw\"")
    ));
    let app = env.start().await;
    let mut c = client(&app).await;
    wait_registered(&mut c, "kr-file").await;
    let st = account_state(&mut c, "kr-missing").await;
    assert_eq!(st["enabled"], false, "{st}");
    assert_eq!(st["registration"]["state"], "disabled");
    let status = c.call("status", Value::Null).await.unwrap();
    let errors = status["config"]["last_error"]["errors"].to_string();
    assert!(
        errors.contains("kr-missing") && errors.contains("no password in the keyring"),
        "{errors}"
    );
    // A reload reports it too, and the file is untouched.
    let r = c.call("config.reload", Value::Null).await.unwrap();
    assert!(r["errors"].to_string().contains("kr-missing"), "{r}");
    assert!(env.text().contains("password_store = \"keyring\""));
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_credential_store_never_falls_back_to_plaintext() {
    vault();
    let reg = registrar(200);
    // `nokr*` ids see an unavailable credential store. A plaintext `password` is refused by
    // validation, so there is nothing to fall back to.
    let env = Env::new(&account("nokr-1", &reg, "password_store = \"keyring\""));
    let app = env.start().await;
    let mut c = client(&app).await;
    let st = account_state(&mut c, "nokr-1").await;
    assert_eq!(st["enabled"], false);
    let status = c.call("status", Value::Null).await.unwrap();
    assert!(
        status["config"]["last_error"]["errors"]
            .to_string()
            .contains("keyring unavailable; set password_store = \\\"file\\\""),
        "{status}"
    );
    assert_eq!(reg.count.load(Ordering::SeqCst), 0, "never registered");

    let e = api_err(
        &mut c,
        "account.set_password",
        json!({"account":"nokr-1","password":"x"}),
    )
    .await;
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("keyring unavailable"),
        "{e}"
    );
    app.shutdown().await;

    // A password next to `keyring` is a validation error.
    let env = Env::new(&account(
        "kr-both",
        &reg,
        "password_store = \"keyring\"\npassword = \"x\"",
    ));
    let mut o = AppOptions::new(env.path());
    o.logging = LoggingMode::Detached;
    o.enable_audio = false;
    assert!(App::start(o).await.is_err());
}

// --- account.set_password -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn set_password_writes_the_keyring_idempotently_and_applies_it() {
    let v = vault();
    v.set("kr-set", "old-pw").unwrap();
    let reg = registrar(200);
    let env = Env::new(&account("kr-set", &reg, "password_store = \"keyring\""));
    let app = env.start().await;
    let mut c = client(&app).await;
    wait_registered(&mut c, "kr-set").await;
    let before = env.text();

    for _ in 0..2 {
        let r = c
            .call(
                "account.set_password",
                json!({"account":"kr-set","password":PW}),
            )
            .await
            .unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["password_store"], "keyring");
        assert!(!r.to_string().contains(PW), "{r}");
    }
    assert_eq!(v.get("kr-set").unwrap().as_deref(), Some(PW));
    assert_eq!(app.config().accounts[0].password.expose(), PW, "reloaded");
    assert_eq!(env.text(), before, "the file is not touched");

    // Clearing deletes the entry; the account then fails loudly at the reload.
    let r = c
        .call(
            "account.set_password",
            json!({"account":"kr-set","password":""}),
        )
        .await
        .unwrap();
    assert!(
        r["report"]["errors"]
            .to_string()
            .contains("no password in the keyring")
    );
    assert_eq!(v.get("kr-set").unwrap(), None);

    let e = api_err(
        &mut c,
        "account.set_password",
        json!({"account":"nope","password":"x"}),
    )
    .await;
    assert_eq!(
        e["data"]["kind"].as_str().unwrap_or("not_found"),
        "not_found"
    );
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn set_password_on_a_file_account_writes_the_file() {
    vault();
    let reg = registrar(200);
    let env = Env::new(&account("kr-fileacct", &reg, "password = \"old\""));
    let app = env.start().await;
    let mut c = client(&app).await;
    let r = c
        .call(
            "account.set_password",
            json!({"account":"kr-fileacct","password":PW}),
        )
        .await
        .unwrap();
    assert_eq!(r["password_store"], "file");
    assert!(!r.to_string().contains(PW), "{r}");
    assert!(env.text().contains(&format!("password = \"{PW}\"")));
    assert_eq!(app.config().accounts[0].password.expose(), PW);
    app.shutdown().await;
}

// --- config.edit moves -------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn switching_the_store_moves_the_password_both_ways() {
    let v = vault();
    let reg = registrar(200);
    let env = Env::new(&account("kr-move", &reg, &format!("password = \"{PW}\"")));
    let app = env.start().await;
    let mut c = client(&app).await;
    wait_registered(&mut c, "kr-move").await;

    // file -> keyring: the password leaves the file, lands in the entry, the account keeps working.
    let r = c
        .call(
            "config.edit",
            json!({"edits":[set("kr-move","password_store",json!("keyring"))]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true, "{r}");
    assert_eq!(v.get("kr-move").unwrap().as_deref(), Some(PW));
    let text = env.text();
    assert!(
        !text.contains(PW) && text.contains("password_store = \"keyring\""),
        "{text}"
    );
    assert_eq!(app.config().accounts[0].password.expose(), PW);

    // keyring -> file: back into the file, entry deleted.
    c.call(
        "config.edit",
        json!({"edits":[set("kr-move","password_store",json!("file"))]}),
    )
    .await
    .unwrap();
    assert_eq!(v.get("kr-move").unwrap(), None);
    assert!(
        env.text().contains(&format!("password = \"{PW}\"")),
        "{}",
        env.text()
    );
    assert_eq!(app.config().accounts[0].password.expose(), PW);
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_moves_the_entry_and_remove_deletes_it() {
    let v = vault();
    v.set("kr-old", PW).unwrap();
    let reg = registrar(200);
    let env = Env::new(&account("kr-old", &reg, "password_store = \"keyring\""));
    let app = env.start().await;
    let mut c = client(&app).await;
    wait_registered(&mut c, "kr-old").await;

    c.call(
        "config.edit",
        json!({"edits":[{"op":"rename_account","from":"kr-old","to":"kr-new"}]}),
    )
    .await
    .unwrap();
    assert_eq!(v.get("kr-old").unwrap(), None, "old entry gone");
    assert_eq!(v.get("kr-new").unwrap().as_deref(), Some(PW), "moved");
    assert_eq!(app.config().accounts[0].password.expose(), PW);

    c.call(
        "config.edit",
        json!({"edits":[{"op":"add_account","draft":{"id":"kr-keep","fields":{
            "username":"9","domain":"127.0.0.1","enabled":false}}}]}),
    )
    .await
    .unwrap();
    c.call(
        "config.edit",
        json!({"edits":[{"op":"remove_account","id":"kr-new"}]}),
    )
    .await
    .unwrap();
    assert_eq!(v.get("kr-new").unwrap(), None, "deleted with the account");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn add_account_with_keyring_stores_the_draft_password_in_the_entry() {
    let v = vault();
    let reg = registrar(200);
    let env = Env::new(&account(
        "kr-seed",
        &reg,
        "enabled = false\npassword = \"x\"",
    ));
    let app = env.start().await;
    let mut c = client(&app).await;
    let r = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"add_account","draft":{"id":"kr-add","fields":{
                "username":"7","domain":"127.0.0.1","registrar":reg.addr.to_string(),
                "password":PW,"password_store":"keyring"}}}]}),
        )
        .await
        .unwrap();
    assert!(!r.to_string().contains(PW), "{r}");
    assert_eq!(v.get("kr-add").unwrap().as_deref(), Some(PW));
    assert!(!env.text().contains(PW), "{}", env.text());
    wait_registered(&mut c, "kr-add").await;

    // A failed edit leaves no entry behind.
    let e = api_err(
        &mut c,
        "config.edit",
        json!({"edits":[{"op":"add_account","draft":{"id":"kr-bad","fields":{
            "password":PW,"password_store":"keyring","local_port":99999}}}]}),
    )
    .await;
    assert!(!e.to_string().contains(PW), "{e}");
    assert_eq!(v.get("kr-bad").unwrap(), None, "rolled back");
    app.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn password_store_is_an_editable_field_and_transport_accepts_auto() {
    vault();
    let reg = registrar(200);
    let env = Env::new(&account("kr-auto", &reg, "password = \"p\""));
    let app = env.start().await;
    let mut c = client(&app).await;
    let view = c.call("config.view", Value::Null).await.unwrap();
    assert!(
        view["editable"]["account_fields"]
            .as_array()
            .unwrap()
            .contains(&json!("password_store"))
    );
    let r = c
        .call(
            "config.edit",
            json!({"edits":[set("kr-auto","transport",json!("auto"))]}),
        )
        .await
        .unwrap();
    assert_eq!(r["applied"], true, "{r}");
    // `auto` on a literal IP with an explicit port resolves to UDP.
    for _ in 0..250 {
        let st = account_state(&mut c, "kr-auto").await;
        if st["transport_effective"] == "udp" && st["registration"]["state"] == "registered" {
            assert_eq!(st["transport"], "auto");
            app.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "auto account did not register: {}",
        account_state(&mut c, "kr-auto").await
    );
}

// --- account.test ----------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn account_test_by_id_and_by_draft() {
    let v = vault();
    v.set("kr-test", PW).unwrap();
    let reg_ok = registrar(200);
    let reg_401 = registrar(401);
    let reg_404 = registrar(404);
    let reg_mute = registrar(0);
    let env = Env::new(&format!(
        "{}{}{}",
        account("kr-test", &reg_ok, "password_store = \"keyring\""),
        account("t-401", &reg_401, "enabled = false\npassword = \"p\""),
        account("t-mute", &reg_mute, "enabled = false\npassword = \"p\""),
    ));
    let app = env.start().await;
    let mut c = client(&app).await;
    wait_registered(&mut c, "kr-test").await;
    let before = account_state(&mut c, "kr-test").await["local_addr"].clone();

    // A configured account with a keyring password, while it is running.
    let r = c
        .call("account.test", json!({"account":"kr-test"}))
        .await
        .unwrap();
    assert_eq!(r["diagnosis"], "ok", "{r}");
    assert_eq!(
        (r["ok"].as_bool(), r["transport"].as_str()),
        (Some(true), Some("udp"))
    );
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .starts_with("Registered via UDP in ")
    );
    assert!(!r.to_string().contains(PW));
    assert_eq!(
        account_state(&mut c, "kr-test").await["local_addr"],
        before,
        "undisturbed"
    );

    // Stored but disabled accounts can be tested too.
    let r = c
        .call("account.test", json!({"account":"t-401"}))
        .await
        .unwrap();
    assert_eq!(r["diagnosis"], "auth_failed");
    assert_eq!(r["code"], 401);
    assert_eq!(r["ok"], false);
    let r = c
        .call("account.test", json!({"account":"t-mute","timeout_secs":1}))
        .await
        .unwrap();
    assert_eq!(r["diagnosis"], "timeout", "{r}");
    assert!(r["hint"].as_str().unwrap().contains("TCP or TLS"));

    // Drafts: nothing is saved; the password is used but never echoed.
    let draft = |reg: &Registrar| {
        json!({"draft":{"fields":{
            "username":"2000","password":PW,"domain":"example.test",
            "registrar":reg.addr.to_string(),"transport":"udp"}}})
    };
    let r = c.call("account.test", draft(&reg_ok)).await.unwrap();
    assert_eq!(r["diagnosis"], "ok", "{r}");
    let r = c.call("account.test", draft(&reg_404)).await.unwrap();
    assert_eq!(r["diagnosis"], "not_found");
    assert!(!r.to_string().contains(PW));
    let text_after = env.text();
    assert!(!text_after.contains("2000"), "nothing was saved");

    // Errors.
    let e = api_err(&mut c, "account.test", json!({})).await;
    assert!(
        e["message"].as_str().unwrap().contains("exactly one"),
        "{e}"
    );
    let e = api_err(
        &mut c,
        "account.test",
        json!({"account":"kr-test","draft":{"fields":{}}}),
    )
    .await;
    assert!(
        e["message"].as_str().unwrap().contains("exactly one"),
        "{e}"
    );
    let e = api_err(&mut c, "account.test", json!({"account":"nope"})).await;
    assert!(
        e["message"].as_str().unwrap().contains("unknown account"),
        "{e}"
    );
    let e = api_err(
        &mut c,
        "account.test",
        json!({"draft":{"fields":{"transport":"carrier-pigeon","password":PW}}}),
    )
    .await;
    assert_eq!(e["data"]["reason"], "invalid");
    assert!(!e.to_string().contains(PW), "{e}");
    let e = api_err(
        &mut c,
        "account.test",
        json!({"draft":{"fields":{"password":PW}}}),
    )
    .await;
    assert!(e["data"]["errors"].to_string().contains("domain"), "{e}");
    let e = api_err(
        &mut c,
        "account.test",
        json!({"account":"kr-test","timeout_secs":99}),
    )
    .await;
    assert!(
        e["message"].as_str().unwrap().contains("timeout_secs"),
        "{e}"
    );
    app.shutdown().await;
}

#[test]
fn the_catalog_lists_the_new_methods_with_schemas() {
    let cat = siple_core::backend::method_catalog();
    for name in ["account.test", "account.set_password"] {
        let m = cat
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("{name}"));
        assert!(m.description.len() > 50);
        assert_eq!(m.params_schema["type"], "object");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clashing_renames_and_re_added_ids_never_lose_a_password() {
    let v = vault();
    v.set("kr-a", "pw-a").unwrap();
    v.set("kr-b", "pw-b").unwrap();
    let reg = registrar(200);
    let env = Env::new(&format!(
        "{}{}",
        account("kr-a", &reg, "password_store = \"keyring\""),
        account("kr-b", &reg, "password_store = \"keyring\"")
    ));
    let app = env.start().await;
    let mut c = client(&app).await;
    // Rename onto an existing id, or onto itself: refused/ignored, both entries survive.
    let _ = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"rename_account","from":"kr-a","to":"kr-b"}]}),
        )
        .await;
    let _ = c
        .call(
            "config.edit",
            json!({"edits":[{"op":"rename_account","from":"kr-a","to":"kr-a"}]}),
        )
        .await;
    assert_eq!(v.get("kr-a").unwrap().as_deref(), Some("pw-a"));
    assert_eq!(v.get("kr-b").unwrap().as_deref(), Some("pw-b"));
    // Remove + add the same id in one batch: the new entry is kept.
    c.call(
        "config.edit",
        json!({"edits":[
            {"op":"remove_account","id":"kr-b"},
            {"op":"add_account","draft":{"id":"kr-b","fields":{
                "username":"5","domain":"127.0.0.1","registrar":reg.addr.to_string(),
                "password":"pw-new","password_store":"keyring"}}}]}),
    )
    .await
    .unwrap();
    assert_eq!(v.get("kr-b").unwrap().as_deref(), Some("pw-new"));
    // A mask is not a password.
    let e = api_err(
        &mut c,
        "account.set_password",
        json!({"account":"kr-a","password":"***"}),
    )
    .await;
    assert!(e["message"].as_str().unwrap().contains("mask"), "{e}");
    assert_eq!(v.get("kr-a").unwrap().as_deref(), Some("pw-a"));
    app.shutdown().await;
}
