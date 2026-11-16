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
use crate::uri::{LinkTarget, OpenUriParams, parse_link};

/// Names served by [`AppBackend::ui_method`].
pub const UI_METHODS: &[&str] = &[
    "ui.show",
    "ui.hide",
    "ui.state",
    "ui.attention",
    "app.open_uri",
];

/// A request to the GUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiRequest {
    /// Show and raise the window (and restore the Dock / taskbar presence).
    Show,
    /// Hide the window to the tray / menu bar.
    Hide,
    /// Report window and tray state.
    State,
    /// Ask the OS to draw the user's attention (taskbar flash, Dock bounce).
    Attention,
    /// Prefill the dial field with a validated link and raise the window. Never dials.
    OpenLink(LinkTarget),
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
            "GUI window state: {visible, focused, tray (an icon exists), close_to_tray (closing hides)}. Fails with invalid_state `no GUI` when headless.",
            none_params(),
        ),
        MethodInfo::new(
            "ui.attention",
            "Ask the OS to draw attention to the window (taskbar flash / Dock bounce / urgency hint) until the user focuses it. Result {ok: true}. Fails with invalid_state `no GUI` when headless.",
            none_params(),
        ),
        MethodInfo::new(
            "app.open_uri",
            "Hand a `sip:`, `sips:`, `tel:` or `callto:` link to the GUI, as the OS URL handler does: it is validated (URI headers and parameters other than `transport` are dropped; control characters and links over 256 bytes are refused), put into the dial field and the window is raised. It never dials: the user confirms with Call. Result {ok, target, scheme}. Errors: invalid_params for a refused link; invalid_state `no GUI` when headless.",
            json!({"type":"object","properties":{"uri":{"type":"string","maxLength":crate::uri::MAX_LINK_LEN,"description":"The link, e.g. sip:1001@pbx.example.test or tel:+15551234567."}},"required":["uri"],"additionalProperties":false}),
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
            "ui.attention" => UiRequest::Attention,
            "app.open_uri" => {
                let p: OpenUriParams = parse_params(params)?;
                let link =
                    parse_link(&p.uri).map_err(|e| ApiError::invalid_params(e.to_string()))?;
                UiRequest::OpenLink(link)
            }
            other => return Err(ApiError::method_not_found(other)),
        };
        self.ui_host()?.request(req)
    }
}
