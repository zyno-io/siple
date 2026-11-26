//! Launch at login in the Flatpak edition: `org.freedesktop.portal.Background.RequestBackground`
//! with `autostart`. The portal writes or removes `~/.config/autostart/io.zyno.siple.desktop` on
//! the host (one entry per app, `flatpak run --command=siple io.zyno.siple ...`). [`MARKER`] in the
//! profile's config dir means "an entry may exist", so turning the setting off can remove it
//! without prompting a user who never enabled it.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::autostart::login_entry_args;

/// File in the config dir: written before an enable request, removed once the portal says
/// autostart is off, so a crash cannot orphan an entry.
pub(super) const MARKER: &str = "flatpak-autostart";
/// How long the whole request may take, including the user's answer to the desktop's prompt.
const WAIT: Duration = Duration::from_secs(180);
/// How long closing an abandoned request may take.
const CLOSE_WAIT: Duration = Duration::from_secs(5);
const REASON: &str = "Start Siple when you log in, so it can take calls";

/// What [`sync`] does for a setting value.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Nothing,
    Request { autostart: bool },
}

/// on: request autostart; off with a marker: request removal; off without a marker: nothing (no
/// permission prompt on a first start with the default `false`).
pub(super) fn decide(enabled: bool, marker_exists: bool) -> Step {
    if enabled {
        Step::Request { autostart: true }
    } else if marker_exists {
        Step::Request { autostart: false }
    } else {
        Step::Nothing
    }
}

/// `["siple", "--at-login"]` followed by `--config <path>` for a non-default profile.
pub(super) fn commandline(config: Option<&Path>) -> Vec<String> {
    let mut v = vec!["siple".to_string()];
    v.extend(login_entry_args(&[], config));
    v
}

/// The object path of the request that `unique_name` (":1.42") made with `token`.
pub(super) fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{token}")
}

/// The `options` argument of `RequestBackground`.
pub(super) fn options(
    autostart: bool,
    commandline: &[String],
    token: &str,
) -> HashMap<&'static str, Value<'static>> {
    let mut m: HashMap<&'static str, Value<'static>> = HashMap::new();
    m.insert("handle_token", Value::from(token.to_string()));
    m.insert("reason", Value::from(REASON));
    m.insert("autostart", Value::from(autostart));
    m.insert("commandline", Value::from(commandline.to_vec()));
    m.insert("dbus-activatable", Value::from(false));
    m
}

/// Makes the desktop's autostart entry for this app match `enabled`. Runs on the OS-entries
/// thread (not a runtime thread); `rt` drives the D-Bus conversation. On an error (no portal,
/// timeout) the marker stays, so the next start retries.
pub(super) fn sync(
    enabled: bool,
    config: Option<&Path>,
    config_dir: &Path,
    rt: &tokio::runtime::Handle,
) -> std::io::Result<()> {
    sync_with(enabled, config, config_dir, |autostart, cmd| {
        rt.block_on(request(autostart, cmd))
    })
}

/// [`sync`] with the portal conversation injected (`Ok(true)`: autostart is on afterwards).
/// Marker order: written before an enable request; kept on any error; removed only after the
/// portal answered that autostart is off.
fn sync_with(
    enabled: bool,
    config: Option<&Path>,
    config_dir: &Path,
    ask: impl FnOnce(bool, &[String]) -> std::io::Result<bool>,
) -> std::io::Result<()> {
    let marker = config_dir.join(MARKER);
    let Step::Request { autostart } = decide(enabled, marker.exists()) else {
        return Ok(());
    };
    let cmd = commandline(config);
    if autostart {
        std::fs::create_dir_all(config_dir)?;
        let json = serde_json::to_string(&cmd).map_err(std::io::Error::other)?;
        std::fs::write(&marker, json)?;
    }
    let on = ask(autostart, &cmd)?;
    if !on {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    if autostart && !on {
        return Err(std::io::Error::other(
            "the desktop did not allow Siple to start at login",
        ));
    }
    Ok(())
}

fn other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn map_zbus(e: zbus::Error) -> std::io::Error {
    let text = e.to_string();
    let missing = matches!(
        &e,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.ServiceUnknown"
                    | "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.UnknownObject"
            )
    );
    if missing {
        std::io::Error::other("this desktop has no Background portal (xdg-desktop-portal)")
    } else {
        std::io::Error::other(text)
    }
}

async fn request_proxy<'a>(
    conn: &zbus::Connection,
    path: &str,
) -> std::io::Result<zbus::Proxy<'a>> {
    zbus::proxy::Builder::new(conn)
        .destination("org.freedesktop.portal.Desktop")
        .map_err(other)?
        .path(path.to_string())
        .map_err(other)?
        .interface("org.freedesktop.portal.Request")
        .map_err(other)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(map_zbus)
}

/// Runs `fut` for at most `wait`; on expiry runs `cleanup` (bounded by [`CLOSE_WAIT`]) and fails
/// with `TimedOut`.
async fn within<T>(
    wait: Duration,
    fut: impl Future<Output = std::io::Result<T>>,
    cleanup: impl Future<Output = ()>,
) -> std::io::Result<T> {
    match tokio::time::timeout(wait, fut).await {
        Ok(r) => r,
        Err(_) => {
            let _ = tokio::time::timeout(CLOSE_WAIT, cleanup).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the desktop did not answer the autostart request in time",
            ))
        }
    }
}

/// Asks the portal; `Ok(true)` when autostart is on afterwards. The whole conversation (session
/// bus, subscription, call, answer) is bounded by [`WAIT`]; on expiry the request is closed.
async fn request(autostart: bool, commandline: &[String]) -> std::io::Result<bool> {
    let open: std::sync::Mutex<Option<(zbus::Connection, String)>> = std::sync::Mutex::new(None);
    within(WAIT, request_inner(autostart, commandline, &open), async {
        // A late "Allow" must not land after a newer request.
        let target = open.lock().ok().and_then(|g| g.clone());
        if let Some((conn, path)) = target
            && let Ok(req) = request_proxy(&conn, &path).await
        {
            let _ = req.call_method("Close", &()).await;
        }
    })
    .await
}

async fn request_inner(
    autostart: bool,
    commandline: &[String],
    open: &std::sync::Mutex<Option<(zbus::Connection, String)>>,
) -> std::io::Result<bool> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let remember = |conn: &zbus::Connection, path: &str| {
        if let Ok(mut g) = open.lock() {
            *g = Some((conn.clone(), path.to_string()));
        }
    };
    let conn = zbus::Connection::session().await.map_err(map_zbus)?;
    let unique = conn
        .unique_name()
        .map(|n| n.to_string())
        .ok_or_else(|| other("no D-Bus unique name"))?;
    let token = format!(
        "siple{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    // Subscribe before the call: the Response can arrive before the method returns.
    let expected = request_path(&unique, &token);
    let mut req = request_proxy(&conn, &expected).await?;
    let mut stream = req.receive_signal("Response").await.map_err(map_zbus)?;

    let portal = zbus::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Background",
    )
    .await
    .map_err(map_zbus)?;
    // The request may exist as soon as the call is sent, even if the reply never arrives.
    remember(&conn, &expected);
    let reply = portal
        .call_method(
            "RequestBackground",
            &("", options(autostart, commandline, &token)),
        )
        .await
        .map_err(map_zbus)?;
    let handle: OwnedObjectPath = reply.body().deserialize().map_err(other)?;
    if handle.as_str() != expected {
        // portal < 0.9 ignores handle_token: follow the path it returned.
        tracing::debug!(%handle, %expected, "Background portal returned another request handle");
        req = request_proxy(&conn, handle.as_str()).await?;
        stream = req.receive_signal("Response").await.map_err(map_zbus)?;
        remember(&conn, handle.as_str());
    }

    let Some(msg) = stream.next().await else {
        return Err(other("the portal request ended without an answer"));
    };
    let (code, results): (u32, HashMap<String, OwnedValue>) =
        msg.body().deserialize().map_err(other)?;
    let granted = results
        .get("autostart")
        .and_then(|v| bool::try_from(v).ok());
    interpret(code, granted, autostart)
}

/// Whether the login entry is on after a `Response(code, {autostart})`.
///
/// Code 1 (denied or canceled) to a *disable* request is an error: the portal then skips
/// writing the entry, so an existing one stays, and the caller must keep its marker to retry.
fn interpret(code: u32, granted: Option<bool>, autostart: bool) -> std::io::Result<bool> {
    match code {
        0 => Ok(granted.unwrap_or(autostart)),
        1 if autostart => Ok(false),
        1 => Err(other(
            "the desktop refused the request, so the login entry may still be there",
        )),
        n => Err(other(format!("the Background portal request failed ({n})"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_disable_is_an_error_so_the_marker_stays() {
        assert!(!interpret(0, Some(false), false).unwrap());
        assert!(interpret(0, Some(true), true).unwrap());
        assert!(interpret(0, None, true).unwrap());
        // Refused enable: off, reported by the caller.
        assert!(!interpret(1, None, true).unwrap());
        // Refused disable: the entry was not removed.
        assert!(interpret(1, Some(false), false).is_err());
        assert!(interpret(2, None, false).is_err());
    }

    #[test]
    fn decide_never_prompts_a_user_who_never_enabled_it() {
        assert_eq!(decide(true, false), Step::Request { autostart: true });
        assert_eq!(decide(true, true), Step::Request { autostart: true });
        assert_eq!(decide(false, true), Step::Request { autostart: false });
        assert_eq!(decide(false, false), Step::Nothing);
    }

    #[test]
    fn the_command_line_carries_a_non_default_profile() {
        assert_eq!(commandline(None), vec!["siple", "--at-login"]);
        assert_eq!(
            commandline(Some(Path::new("/p/a b/config.toml"))),
            vec!["siple", "--at-login", "--config", "/p/a b/config.toml"]
        );
    }

    #[test]
    fn the_command_line_parses_back_with_the_login_flag() {
        use clap::Parser;
        let p = Path::new("/p/a b/config.toml");
        let c = crate::Cli::try_parse_from(commandline(Some(p))).unwrap();
        assert!(c.at_login);
        assert_eq!(c.config.as_deref(), Some(p));
    }

    #[test]
    fn the_request_path_escapes_the_sender() {
        assert_eq!(
            request_path(":1.42", "t"),
            "/org/freedesktop/portal/desktop/request/1_42/t"
        );
    }

    #[test]
    fn the_options_ask_for_autostart_with_the_command_line() {
        let cmd = commandline(None);
        let o = options(true, &cmd, "tok");
        let mut keys: Vec<_> = o.keys().copied().collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "autostart",
                "commandline",
                "dbus-activatable",
                "handle_token",
                "reason"
            ]
        );
        assert!(bool::try_from(&o["autostart"]).unwrap());
        assert!(!bool::try_from(&o["dbus-activatable"]).unwrap());
        assert_eq!(<&str>::try_from(&o["handle_token"]).unwrap(), "tok");
    }

    fn state(d: &Path) -> (bool, std::path::PathBuf) {
        let m = d.join(MARKER);
        (m.exists(), m)
    }

    #[test]
    fn the_marker_is_written_before_the_enable_request() {
        let d = tempfile::tempdir().unwrap();
        let seen = std::cell::Cell::new(None);
        sync_with(true, None, d.path(), |autostart, cmd| {
            assert!(autostart);
            assert_eq!(cmd, ["siple", "--at-login"]);
            seen.set(Some(
                std::fs::read_to_string(d.path().join(MARKER)).unwrap(),
            ));
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen.take().as_deref(), Some("[\"siple\",\"--at-login\"]"));
        assert!(state(d.path()).0);
    }

    #[test]
    fn errors_keep_the_marker() {
        let d = tempfile::tempdir().unwrap();
        let err = || Err(std::io::Error::other("no portal"));
        assert!(sync_with(true, None, d.path(), |_, _| err()).is_err());
        assert!(state(d.path()).0, "enable error");
        assert!(sync_with(false, None, d.path(), |_, _| err()).is_err());
        assert!(state(d.path()).0, "disable error");
    }

    #[test]
    fn disabling_removes_the_marker_only_after_success() {
        let d = tempfile::tempdir().unwrap();
        let (_, m) = state(d.path());
        std::fs::write(&m, "[]").unwrap();
        sync_with(false, None, d.path(), |autostart, _| {
            assert!(!autostart);
            assert!(m.exists(), "still there while the request runs");
            Ok(false)
        })
        .unwrap();
        assert!(!m.exists());
        // Nothing to undo: no request, no prompt.
        sync_with(false, None, d.path(), |_, _| panic!("no request expected")).unwrap();
    }

    #[test]
    fn a_refused_enable_drops_the_marker_and_reports_it() {
        let d = tempfile::tempdir().unwrap();
        let r = sync_with(true, None, d.path(), |_, _| Ok(false));
        assert!(r.is_err());
        assert!(!state(d.path()).0);
    }

    #[tokio::test]
    async fn a_stalled_request_times_out_and_is_closed() {
        let closed = std::cell::Cell::new(false);
        let r: std::io::Result<bool> =
            within(Duration::from_millis(20), std::future::pending(), async {
                closed.set(true)
            })
            .await;
        assert_eq!(r.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(closed.get());
        let ok = within(Duration::from_millis(20), async { Ok(7) }, async {
            panic!("no cleanup on success")
        })
        .await
        .unwrap();
        assert_eq!(ok, 7);
    }
}
