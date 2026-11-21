//! The GUI side of the API: window control, attention and link hand-off.
//!
//! The engine knows nothing about windows. A GUI process registers a [`UiHost`] with
//! [`crate::App::set_ui_host`]; the `ui.*` and `app.open_uri` methods forward to it. A headless
//! instance has no host, and those methods fail with a clear "no GUI" error (so an agent can
//! tell the difference between "not supported here" and "bad request").

use std::sync::Arc;

use serde_json::{Value, json};

use crate::backend::AppBackend;
use crate::control::methods::{MethodInfo, parse_params};
use crate::control::{ApiError, ErrorKind};
use crate::provision::SetupLink;
use crate::uri::{IncomingLink, LinkTarget, OpenUriParams, parse_any};

/// Names served by [`AppBackend::ui_method`].
pub const UI_METHODS: &[&str] = &[
    "ui.show",
    "ui.hide",
    "ui.state",
    "ui.set_panels",
    "ui.attention",
    "app.open_uri",
];

/// What the GUI starts when it quits for an installed update.
#[cfg(not(feature = "store"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaunchSpec {
    /// macOS: open this `.app` bundle.
    MacBundle(std::path::PathBuf),
    /// Windows and Linux: run this executable.
    Exe(std::path::PathBuf),
}

/// A request to the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiRequest {
    /// Show and raise the window (and restore the Dock / taskbar presence).
    Show,
    /// Hide the window to the tray / menu bar.
    Hide,
    /// Report window and tray state.
    State,
    /// Collapse / expand the Lines rail and open / close the extensions panel (`None` = leave).
    SetPanels {
        /// Lines rail collapsed.
        lines_collapsed: Option<bool>,
        /// Extensions panel open.
        extensions_open: Option<bool>,
    },
    /// Ask the OS to draw the user's attention (taskbar flash, Dock bounce).
    Attention,
    /// Prefill the dial field with a validated link and raise the window. Never dials.
    OpenLink(LinkTarget),
    /// Open the Review screen of a validated `siple:provision` link and raise the window.
    /// Nothing is tested or saved until the user continues, types the password and tests.
    OpenSetup(SetupLink),
    /// A link that was refused: the GUI shows `String` (fixed text, no submitted values).
    LinkRefused(String),
    /// Quit (as the tray's Quit does) and start the new version of the installed update. Only the
    /// updater sends it, after the files were swapped and no call is active.
    #[cfg(not(feature = "store"))]
    RelaunchForUpdate(RelaunchSpec),
}

/// Parameters of `ui.set_panels`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPanelsParams {
    lines_collapsed: Option<bool>,
    extensions_open: Option<bool>,
}

/// Implemented by the GUI. Called from control-server tasks, so it must be quick and
/// thread-safe (hand the request to the UI thread and return).
pub trait UiHost: Send + Sync {
    /// Executes `req`; the value is the method result.
    fn request(&self, req: UiRequest) -> Result<Value, ApiError>;
}

pub(crate) type UiHostSlot = std::sync::Mutex<Option<Arc<dyn UiHost>>>;

fn none_params() -> Value {
    json!({"type":"object","properties":{},"additionalProperties":false})
}

/// Schemas of [`UI_METHODS`], chained into `backend_methods()`.
pub fn ui_methods() -> Vec<MethodInfo> {
    vec![
        MethodInfo::new(
            "ui.show",
            "Show and raise the GUI window (restores the Dock / taskbar presence it gave up while hidden in the tray). Result {visible: true}. Fails with invalid_state `no GUI` when this instance runs headless.",
            none_params(),
        ),
        MethodInfo::new(
            "ui.hide",
            "Hide the GUI window to the tray / menu bar; the engine keeps running (registrations, calls, control API). Needs `[ui] tray = true`, else invalid_state. Result {visible: false}. Fails with invalid_state `no GUI` when headless.",
            none_params(),
        ),
        MethodInfo::new(
            "ui.state",
            "GUI window state: {visible, focused, tray (an icon exists), close_to_tray (closing hides), lines_collapsed (the Lines rail is the slim strip), extensions_open (the extensions panel is open; in a narrow window, where the panel is a tab, the saved or session panel choice, false when none)}. Fails with invalid_state `no GUI` when headless.",
            none_params(),
        ),
        MethodInfo::new(
            "ui.set_panels",
            "Collapse or expand the Lines rail and open or close the extensions panel, as the GUI chevrons do; the choice is remembered across launches (state.toml). Omitted fields are left alone. Both settings are for the wide and medium layouts: in a narrow window they are stored (and applied when the window widens) but the narrow view does not change, and the narrow extensions tab is never affected. Result: the same fields as ui.state's `lines_collapsed` and `extensions_open`, as applied (reported once the GUI processed the request; may lag one frame). Fails with invalid_state `no GUI` when headless.",
            json!({"type":"object","properties":{"lines_collapsed":{"type":"boolean","description":"true = slim strip of line badges, false = full Lines list."},"extensions_open":{"type":"boolean","description":"true = show the extensions panel, false = hide it."}},"additionalProperties":false}),
        ),
        MethodInfo::new(
            "ui.attention",
            "Ask the OS to draw attention to the window (taskbar flash / Dock bounce / urgency hint) until the user focuses it. Result {ok: true}. Fails with invalid_state `no GUI` when headless.",
            none_params(),
        ),
        MethodInfo::new(
            "app.open_uri",
            &format!(
                "Hand a `sip:`, `sips:`, `tel:`, `callto:` or `siple:` link to the GUI, as the OS URL handler does, and raise the window. Dial links (`sip:`, `sips:`, `tel:`, `callto:`) are validated (URI headers and parameters other than `transport` are dropped; control characters and links over 256 bytes are refused), put into the dial field, and never dial: the user confirms with Call. A `siple:provision` setup link (up to {} bytes, the same checks as `provision.parse`) opens the Review screen; nothing is tested or saved until the user reads it, continues, types the password and runs Test, and no parameter skips that. If the user is already past another link's Review or a test runs, the GUI keeps that setup and shows a notice. Result {{ok, kind: dial|provision, scheme, target? (dial), next_hop? (provision)}}. Errors: invalid_params for a refused link, with data {{reason: \"invalid_link\", code, field?}} (the GUI also shows why, rate-limited); invalid_state `rate_limited`; invalid_state `no GUI` when headless.",
                crate::provision::MAX_LINK_LEN
            ),
            json!({"type":"object","properties":{"uri":{"type":"string","maxLength":crate::provision::MAX_LINK_LEN,"description":"The link, e.g. sip:1001@pbx.example.test, tel:+15551234567 or siple:provision?d=..."}},"required":["uri"],"additionalProperties":false}),
        ),
    ]
}

impl AppBackend {
    fn ui_host(&self) -> Result<Arc<dyn UiHost>, ApiError> {
        self.inner
            .ui_host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::InvalidState,
                    "no GUI: this instance runs headless (start `siple` without --headless)",
                )
                .with_extra(json!({"reason": "no_gui"}))
            })
    }

    /// Dispatches one of [`UI_METHODS`].
    pub(crate) async fn ui_method(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let req = match method {
            "ui.show" => UiRequest::Show,
            "ui.hide" => UiRequest::Hide,
            "ui.state" => UiRequest::State,
            "ui.set_panels" => {
                let p: SetPanelsParams = parse_params(params)?;
                UiRequest::SetPanels {
                    lines_collapsed: p.lines_collapsed,
                    extensions_open: p.extensions_open,
                }
            }
            "ui.attention" => UiRequest::Attention,
            "app.open_uri" => {
                let p: OpenUriParams = parse_params(params)?;
                match parse_any(&p.uri) {
                    Ok(IncomingLink::Dial(l)) => UiRequest::OpenLink(l),
                    Ok(IncomingLink::Setup(l)) => UiRequest::OpenSetup(l),
                    Err(e) => {
                        if let Ok(host) = self.ui_host() {
                            let _ = host.request(UiRequest::LinkRefused(e.to_string()));
                        }
                        return Err(e.to_api_error());
                    }
                }
            }
            other => return Err(ApiError::method_not_found(other)),
        };
        self.ui_host()?.request(req)
    }
}
