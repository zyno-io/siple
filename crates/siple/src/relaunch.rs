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
//! The Linux AppImage is started as the file itself (never its temporary mount); a copy that ran
//! without FUSE restarts the new one the same way with `--appimage-extract-and-run`, which the
//! runtime only honours as the first argument.
//!
//! Carried over: `--config <absolute path>` when the config is not the default one (the macOS
//! `open -n` drops environment variables such as `SIPLE_CONFIG`) and `--no-audio`. A link argument
//! is never carried over. The window state at the moment of the update is passed as
//! `--window shown|hidden`, so the new version reopens (or stays in the tray) as the old one was,
//! whatever `[ui] start_hidden` says.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use siple_core::ui_host::RelaunchSpec;

use crate::WindowArg;

fn window_arg(w: WindowArg) -> &'static str {
    match w {
        WindowArg::Shown => "shown",
        WindowArg::Hidden => "hidden",
    }
}

static PENDING: Mutex<Option<(RelaunchSpec, WindowArg)>> = Mutex::new(None);
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

/// Remembers what to start once this process has shut down, and whether the window should come
/// back shown (see `shell::policy::relaunch_shown`: only a tray-hidden window comes back hidden).
pub fn request(spec: RelaunchSpec, window_shown: bool) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((spec, WindowArg::from_shown(window_shown)));
}

/// The program and arguments that start `spec` from process `pid`.
pub fn command_line(
    spec: &RelaunchSpec,
    pid: u32,
    carried: &[String],
    window: WindowArg,
) -> (String, Vec<String>) {
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
            args.extend(["--window".to_string(), window_arg(window).to_string()]);
            ("/usr/bin/open".to_string(), args)
        }
        RelaunchSpec::Exe(exe) => {
            let mut args = vec!["--wait-pid".to_string(), pid];
            args.extend(carried.iter().cloned());
            args.extend(["--window".to_string(), window_arg(window).to_string()]);
            (exe.display().to_string(), args)
        }
        RelaunchSpec::AppImage {
            file,
            extract_and_run,
        } => {
            // A runtime option counts only as the first argument that starts with `--`.
            let mut args = Vec::new();
            if *extract_and_run {
                args.push("--appimage-extract-and-run".to_string());
            }
            args.extend(["--wait-pid".to_string(), pid]);
            args.extend(carried.iter().cloned());
            args.extend(["--window".to_string(), window_arg(window).to_string()]);
            (file.display().to_string(), args)
        }
    }
}

/// Starts the pending relaunch, if any. Called once, at the end of the shutdown.
pub fn spawn_pending() {
    let Some((spec, window)) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let carried = CARRIED.get().map(Vec::as_slice).unwrap_or_default();
    let (program, args) = command_line(&spec, std::process::id(), carried, window);
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
    fn appimage_relaunch_runs_the_file_not_the_mount() {
        let spec = |extract_and_run| RelaunchSpec::AppImage {
            file: "/home/u/Applications/Siple.AppImage".into(),
            extract_and_run,
        };
        let (p, a) = command_line(&spec(false), 7, &[], WindowArg::Shown);
        assert_eq!(p, "/home/u/Applications/Siple.AppImage");
        assert_eq!(a, ["--wait-pid", "7", "--window", "shown"]);
        let carried = ["--no-audio".to_string()];
        let (_, a) = command_line(&spec(true), 7, &carried, WindowArg::Hidden);
        assert_eq!(
            a,
            [
                "--appimage-extract-and-run",
                "--wait-pid",
                "7",
                "--no-audio",
                "--window",
                "hidden"
            ]
        );
    }

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
            WindowArg::Hidden,
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
                "--no-audio",
                "--window",
                "hidden"
            ]
        );
    }

    #[test]
    fn windows_and_linux_run_the_executable_itself() {
        let exe = RelaunchSpec::Exe("C:\\Siple\\siple.exe".into());
        let (p, a) = command_line(&exe, 7, &[], WindowArg::Shown);
        assert_eq!(p, "C:\\Siple\\siple.exe");
        assert_eq!(a, ["--wait-pid", "7", "--window", "shown"]);
        let (_, a) = command_line(&exe, 7, &["--no-audio".into()], WindowArg::Hidden);
        assert_eq!(a, ["--wait-pid", "7", "--no-audio", "--window", "hidden"]);
    }

    #[test]
    fn mac_passes_the_window_state_after_the_carried_arguments() {
        let (_, a) = command_line(
            &RelaunchSpec::MacBundle("/Applications/Siple.app".into()),
            1,
            &[],
            WindowArg::Shown,
        );
        assert_eq!(
            a,
            [
                "-n",
                "/Applications/Siple.app",
                "--args",
                "--wait-pid",
                "1",
                "--window",
                "shown"
            ]
        );
    }

    #[test]
    fn window_state_follows_visibility() {
        assert_eq!(WindowArg::from_shown(true), WindowArg::Shown);
        assert_eq!(WindowArg::from_shown(false), WindowArg::Hidden);
    }

    #[test]
    fn a_pending_relaunch_is_taken_once() {
        request(RelaunchSpec::Exe("/nonexistent/siple".into()), true);
        // Starting a program that does not exist reports and returns; the request is gone.
        spawn_pending();
        assert!(PENDING.lock().unwrap().is_none());
    }
}
