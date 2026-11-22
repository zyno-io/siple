//! Startup check of the GUI libraries winit loads with dlopen (X11, Wayland, xkbcommon), and the
//! error message for a missing one. Without it a minimal desktop got a panic inside winit
//! ("Library libxkbcommon-x11.so could not be loaded") and, launched from a file manager, no message at all.
//!
//! The AppImage bundles the libraries that are not on the AppImageCommunity excludelist
//! (`scripts/appimage-libs.sh`); the tarball needs them installed. Keep this table in step with
//! that list: it names the sonames the dependencies try (xkbcommon-dl, x11-dl, wayland-sys) and the
//! packages that ship them.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(target_os = "linux")]
use std::process::ExitCode;

/// The install guide.
pub const INSTALL_GUIDE_URL: &str = "https://zyno-io.github.io/siple/guide/install";

/// A library the GUI loads at run time.
#[derive(Debug, PartialEq, Eq)]
pub struct GuiLib {
    /// The soname winit and wgpu try first.
    pub soname: &'static str,
    /// Debian / Ubuntu package.
    pub debian: &'static str,
    /// Fedora package.
    pub fedora: &'static str,
}

static XKBCOMMON: GuiLib = GuiLib {
    soname: "libxkbcommon.so.0",
    debian: "libxkbcommon0",
    fedora: "libxkbcommon",
};
static XKBCOMMON_X11: GuiLib = GuiLib {
    soname: "libxkbcommon-x11.so.0",
    debian: "libxkbcommon-x11-0",
    fedora: "libxkbcommon-x11",
};
static X11: [GuiLib; 4] = [
    GuiLib {
        soname: "libX11.so.6",
        debian: "libx11-6",
        fedora: "libX11",
    },
    GuiLib {
        soname: "libX11-xcb.so.1",
        debian: "libx11-xcb1",
        fedora: "libX11-xcb",
    },
    GuiLib {
        soname: "libxcb.so.1",
        debian: "libxcb1",
        fedora: "libxcb",
    },
    GuiLib {
        soname: "libXcursor.so.1",
        debian: "libxcursor1",
        fedora: "libXcursor",
    },
];
static XI: GuiLib = GuiLib {
    soname: "libXi.so.6",
    debian: "libxi6",
    fedora: "libXi",
};
static WAYLAND: [GuiLib; 2] = [
    GuiLib {
        soname: "libwayland-client.so.0",
        debian: "libwayland-client0",
        fedora: "libwayland-client",
    },
    GuiLib {
        soname: "libwayland-cursor.so.0",
        debian: "libwayland-cursor0",
        fedora: "libwayland-cursor",
    },
];
/// Loaded only by the GL path of wgpu, so it is not part of the preflight, but a panic naming it
/// gets the same message.
static WAYLAND_EGL: GuiLib = GuiLib {
    soname: "libwayland-egl.so.1",
    debian: "libwayland-egl1",
    fedora: "libwayland-egl",
};

fn known() -> Vec<&'static GuiLib> {
    let mut v: Vec<&'static GuiLib> = vec![&XKBCOMMON, &XKBCOMMON_X11, &XI, &WAYLAND_EGL];
    v.extend(X11.iter());
    v.extend(WAYLAND.iter());
    v
}

fn set(v: Option<&str>) -> bool {
    v.is_some_and(|s| !s.is_empty())
}

/// The libraries the window backend that winit will pick needs: Wayland when `WAYLAND_DISPLAY` (or
/// `WAYLAND_SOCKET`) is set, else X11 when `DISPLAY` is set; nothing when neither is.
pub fn needed_libs(
    display: Option<&str>,
    wayland_display: Option<&str>,
    wayland_socket: Option<&str>,
) -> Vec<&'static GuiLib> {
    let wayland = set(wayland_display) || set(wayland_socket);
    let x11 = set(display) && !wayland;
    let mut v: Vec<&'static GuiLib> = Vec::new();
    if wayland || x11 {
        v.push(&XKBCOMMON);
    }
    if x11 {
        v.extend(X11.iter());
        v.push(&XKBCOMMON_X11);
        v.push(&XI);
    }
    if wayland {
        v.extend(WAYLAND.iter());
    }
    v
}

/// The message for a missing library.
pub fn missing_message(lib: &GuiLib) -> String {
    format!(
        "Siple can't start its window: {} is missing. Install {} (Debian/Ubuntu) or {} (Fedora). \
         See {INSTALL_GUIDE_URL}",
        lib.soname, lib.debian, lib.fedora
    )
}

/// The message for a startup error whose text is `raw` (a panic payload or an eframe error): the
/// missing-library message when it names a known library (`Library libxkbcommon-x11.so could not be
/// loaded.`, `libX11.so.6: cannot open shared object file`), otherwise a generic one carrying the
/// raw text.
pub fn message_for_error(raw: &str) -> String {
    let base = |s: &str| s.split(".so").next().unwrap_or(s).to_string();
    for word in raw.split(|c: char| c.is_whitespace() || c == ':' || c == '\'' || c == '"') {
        let word = word.trim_matches(|c: char| ".,;()".contains(c));
        if word.starts_with("lib") && word.contains(".so") {
            let b = base(word);
            if let Some(lib) = known().into_iter().find(|l| base(l.soname) == b) {
                return missing_message(lib);
            }
        }
    }
    format!(
        "Siple can't start its window: {raw}. A GUI library may be missing: see {INSTALL_GUIDE_URL}, \
         or run headless with `siple-cli run --headless`."
    )
}

/// An external program that can show a message to someone who launched Siple from a file manager.
#[derive(Debug, PartialEq, Eq)]
pub struct DialogCmd {
    pub program: &'static str,
    pub args: Vec<String>,
    /// Only a zero exit status counts as shown (a notification needs a daemon); a dialog's status
    /// also reflects the button pressed.
    pub must_succeed: bool,
}

/// Dialog helpers in the order they are tried.
pub fn dialog_commands(msg: &str) -> Vec<DialogCmd> {
    let c = |program, args: &[&str], must_succeed| DialogCmd {
        program,
        args: args.iter().map(|a| a.to_string()).collect(),
        must_succeed,
    };
    let mut v = vec![
        c(
            "notify-send",
            &["-u", "critical", "-a", "Siple", "Siple"],
            true,
        ),
        c(
            "zenity",
            &["--error", "--title=Siple", "--no-markup"],
            false,
        ),
        c("kdialog", &["--error"], false),
        c("xmessage", &["-center"], false),
    ];
    // The text goes last (after `--` where the tool honours it) so it is never read as an option.
    v[0].args.extend(["--".to_string(), msg.to_string()]);
    v[1].args.push(format!("--text={msg}"));
    v[2].args
        .extend([msg.to_string(), "--title".into(), "Siple".into()]);
    v[3].args.push(msg.to_string());
    v
}

/// Prints `msg`, logs it and, when stderr is not a terminal (a launch from a file manager),
/// tries to show it in a dialog; best effort. Returns the exit code to use.
#[cfg(target_os = "linux")]
pub fn fatal(msg: &str) -> ExitCode {
    use std::io::IsTerminal;
    eprintln!("{msg}");
    tracing::error!("{msg}");
    if !std::io::stderr().is_terminal() {
        for d in dialog_commands(msg) {
            let mut cmd = std::process::Command::new(d.program);
            siple_core::config::channel::apply_host_env(&mut cmd);
            cmd.args(&d.args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            match cmd.status() {
                Ok(st) if st.success() || !d.must_succeed => break,
                _ => {}
            }
        }
    }
    ExitCode::from(1)
}

/// `dlopen`s `libs` (RTLD_LAZY, closed again) and returns the first that cannot be loaded.
#[cfg(target_os = "linux")]
pub fn first_missing(libs: &[&'static GuiLib]) -> Option<&'static GuiLib> {
    use std::ffi::{CStr, CString};
    for lib in libs {
        let Ok(name) = CString::new(lib.soname) else {
            continue;
        };
        // SAFETY: a valid NUL-terminated name; the handle is only passed back to dlclose.
        let h = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_LAZY) };
        if h.is_null() {
            // SAFETY: dlerror returns null or a NUL-terminated string valid until the next dl call.
            let why = unsafe {
                let e = libc::dlerror();
                (!e.is_null()).then(|| CStr::from_ptr(e).to_string_lossy().into_owned())
            };
            tracing::error!(soname = lib.soname, ?why, "cannot load a GUI library");
            if let Some(why) = why {
                eprintln!("{why}");
            }
            return Some(lib);
        }
        // SAFETY: `h` came from dlopen above.
        unsafe { libc::dlclose(h) };
    }
    None
}

/// The startup check: the message to fail with, or `None` when the libraries load.
#[cfg(target_os = "linux")]
pub fn preflight() -> Option<String> {
    let var = |k| std::env::var(k).ok();
    let libs = needed_libs(
        var("DISPLAY").as_deref(),
        var("WAYLAND_DISPLAY").as_deref(),
        var("WAYLAND_SOCKET").as_deref(),
    );
    first_missing(&libs).map(missing_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sonames(v: &[&GuiLib]) -> Vec<&'static str> {
        v.iter().map(|l| l.soname).collect()
    }

    #[test]
    fn backend_choice() {
        assert!(needed_libs(None, None, None).is_empty());
        assert!(needed_libs(Some(""), Some(""), None).is_empty());
        let x = sonames(&needed_libs(Some(":0"), None, None));
        assert!(x.contains(&"libxkbcommon-x11.so.0") && x.contains(&"libX11.so.6"));
        assert!(!x.contains(&"libwayland-client.so.0"));
        // Wayland wins when both are set, as in winit.
        let w = sonames(&needed_libs(Some(":0"), Some("wayland-0"), None));
        assert!(w.contains(&"libwayland-client.so.0") && w.contains(&"libxkbcommon.so.0"));
        assert!(!w.contains(&"libX11.so.6"));
        let s = sonames(&needed_libs(None, None, Some("3")));
        assert!(s.contains(&"libwayland-cursor.so.0"));
    }

    #[test]
    fn missing_message_names_the_soname_and_packages() {
        let m = missing_message(&XKBCOMMON_X11);
        assert_eq!(
            m,
            "Siple can't start its window: libxkbcommon-x11.so.0 is missing. Install libxkbcommon-x11-0 \
             (Debian/Ubuntu) or libxkbcommon-x11 (Fedora). See https://zyno-io.github.io/siple/guide/install"
        );
    }

    #[test]
    fn panic_text_maps_to_the_same_message() {
        let raw = "app creation error: Library libxkbcommon-x11.so could not be loaded.";
        assert_eq!(message_for_error(raw), missing_message(&XKBCOMMON_X11));
        assert_eq!(
            message_for_error("libXi.so.6: cannot open shared object file: No such file"),
            missing_message(&XI)
        );
        assert_eq!(
            message_for_error("Library libxkbcommon.so could not be loaded."),
            missing_message(&XKBCOMMON)
        );
        // Another library, or no library at all: the raw text is kept.
        let m = message_for_error("app creation error: Adapter not found");
        assert!(m.contains("Adapter not found") && m.contains(INSTALL_GUIDE_URL));
        assert!(message_for_error("libfoo.so.1 missing").contains("libfoo.so.1 missing"));
    }

    #[test]
    fn dialogs_come_in_order_with_the_text_last_or_attached() {
        let d = dialog_commands("boom");
        let names: Vec<_> = d.iter().map(|c| c.program).collect();
        assert_eq!(names, ["notify-send", "zenity", "kdialog", "xmessage"]);
        assert_eq!(d[0].args.last().map(String::as_str), Some("boom"));
        assert!(d[0].args.contains(&"--".to_string()) && d[0].must_succeed);
        assert!(d[1].args.contains(&"--text=boom".to_string()));
        assert_eq!(d[3].args, ["-center", "boom"]);
        assert!(d[1..].iter().all(|c| !c.must_succeed));
    }
}
