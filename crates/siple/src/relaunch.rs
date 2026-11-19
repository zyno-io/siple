//! Starting the new version after an in-place update.
//!
//! The updater (core) swaps the files, waits until no call is active and asks the GUI host to
//! restart ([`siple_core::ui_host::UiRequest::RelaunchForUpdate`]). The host remembers what to
//! start here and runs the normal quit. The process is started at the very end of
//! `run_shutdown_hook`, which every exit path runs (normal return, the macOS `atexit` handler,
//! the 4 s fallback thread), so the engine has released the instance lock and the control socket
//! by then; the new process also waits for this pid (`--wait-pid`, a permanent part of the command
//! line) before it looks for them.
//!
//! Carried over: `--config <absolute path>` when the config is not the default one (the macOS
//! `open -n` drops environment variables such as `SIPLE_CONFIG`) and `--no-audio`. A link argument
//! is never carried over.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use siple_core::ui_host::RelaunchSpec;

static PENDING: Mutex<Option<RelaunchSpec>> = Mutex::new(None);
static CARRIED: OnceLock<Vec<String>> = OnceLock::new();

/// The arguments of this run that a relaunch repeats (set once at start).
pub fn set_carried(config: Option<PathBuf>, no_audio: bool) {
    let mut v = Vec::new();
    if let Some(c) = config {
        v.push("--config".to_string());
        v.push(c.display().to_string());
    }
    if no_audio {
        v.push("--no-audio".to_string());
    }
    let _ = CARRIED.set(v);
}

/// Remembers what to start once this process has shut down.
pub fn request(spec: RelaunchSpec) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(spec);
}

/// The program and arguments that start `spec` from process `pid`.
pub fn command_line(spec: &RelaunchSpec, pid: u32, carried: &[String]) -> (String, Vec<String>) {
    let pid = pid.to_string();
    match spec {
        RelaunchSpec::MacBundle(bundle) => {
            let mut args = vec![
                "-n".to_string(),
                bundle.display().to_string(),
                "--args".to_string(),
                "--wait-pid".to_string(),
                pid,
            ];
            args.extend(carried.iter().cloned());
            ("/usr/bin/open".to_string(), args)
        }
        RelaunchSpec::Exe(exe) => {
            let mut args = vec!["--wait-pid".to_string(), pid];
            args.extend(carried.iter().cloned());
            (exe.display().to_string(), args)
        }
    }
}

/// Starts the pending relaunch, if any. Called once, at the end of the shutdown.
pub fn spawn_pending() {
    let Some(spec) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let carried = CARRIED.get().map(Vec::as_slice).unwrap_or_default();
    let (program, args) = command_line(&spec, std::process::id(), carried);
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(all(unix, not(target_os = "macos")))]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: the new copy outlives this one.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    match cmd.spawn() {
        Ok(_) => tracing::info!(%program, "started the new version"),
        Err(e) => eprintln!("cannot start the new version ({program}): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_uses_open_with_a_new_instance_and_the_old_pid() {
        let (p, a) = command_line(
            &RelaunchSpec::MacBundle("/Applications/Siple.app".into()),
            4242,
            &[
                "--config".into(),
                "/x/config.toml".into(),
                "--no-audio".into(),
            ],
        );
        assert_eq!(p, "/usr/bin/open");
        assert_eq!(
            a,
            [
                "-n",
                "/Applications/Siple.app",
                "--args",
                "--wait-pid",
                "4242",
                "--config",
                "/x/config.toml",
                "--no-audio"
            ]
        );
    }

    #[test]
    fn windows_and_linux_run_the_executable_itself() {
        let (p, a) = command_line(&RelaunchSpec::Exe("C:\\Siple\\siple.exe".into()), 7, &[]);
        assert_eq!(p, "C:\\Siple\\siple.exe");
        assert_eq!(a, ["--wait-pid", "7"]);
    }

    #[test]
    fn a_pending_relaunch_is_taken_once() {
        request(RelaunchSpec::Exe("/nonexistent/siple".into()));
        // Starting a program that does not exist reports and returns; the request is gone.
        spawn_pending();
        assert!(PENDING.lock().unwrap().is_none());
    }
}
