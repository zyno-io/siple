//! macOS integration: activation policy (Dock icon), `sip:` / `tel:` Apple Events, the Dock
//! "reopen" Apple Event and the app menu.
//!
//! AppKit calls must run on the main thread. [`early_init`] runs from `main()` before the event
//! loop so the launch URL (delivered while the app finishes launching, before eframe's creator
//! runs) is not lost; URLs queue until the shell attaches with [`attach`].

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSObject};
use tray_icon::menu::accelerator::{Accelerator, Code, Modifiers};
use tray_icon::menu::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu};

use super::ShellTx;
use super::edit_cmd::EditCmd;

/// Shows (`true`) or removes (`false`) the Dock icon by switching the activation policy.
/// Showing also brings the app to the front, so the window it is about to show is key.
pub fn set_dock_icon(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("set_dock_icon called off the main thread");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    if !app.setActivationPolicy(policy) {
        tracing::warn!("macOS refused the activation policy change");
    }
    if visible {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

// ---- Apple Events: links and Dock reopen ---------------------------------------------------

/// Where events go once the shell exists.
static SINK: Mutex<Option<ShellTx>> = Mutex::new(None);
/// Links that arrived before the shell attached (a launch by link), newest last.
static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Cap on queued links: a launch carries one; anything more is noise.
const MAX_PENDING: usize = 8;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Longest URL taken from an Apple Event, in bytes. A setup link is at most 2 KiB; anything far
/// beyond that is not one of ours, and is dropped before it is queued or parsed.
const MAX_URL_BYTES: usize = 8 * 1024;

fn deliver_url(url: String) {
    if url.len() > MAX_URL_BYTES {
        tracing::warn!(len = url.len(), "ignoring an oversized URL");
        return;
    }
    match lock(&SINK).as_ref() {
        Some(tx) => tx.send(super::ShellMsg::Url(url)),
        None => {
            let mut q = lock(&PENDING);
            if q.len() < MAX_PENDING {
                q.push(url);
            }
        }
    }
}

fn deliver_reopen() {
    if let Some(tx) = lock(&SINK).as_ref() {
        tx.send(super::ShellMsg::Reopen);
    }
}

/// Connects the queue to the shell and replays links that arrived earlier.
pub fn attach(tx: ShellTx) {
    let pending = std::mem::take(&mut *lock(&PENDING));
    for u in pending {
        tx.send(super::ShellMsg::Url(u));
    }
    *lock(&SINK) = Some(tx);
}

/// `'GURL'`: both the Internet event class and the get-URL event id.
const GURL: u32 = u32::from_be_bytes(*b"GURL");
/// `'aevt'` / `'rapp'`: the "reopen application" event (Dock click on a running app).
const AEVT: u32 = u32::from_be_bytes(*b"aevt");
const RAPP: u32 = u32::from_be_bytes(*b"rapp");
/// `'----'`: the direct-object keyword of an Apple Event.
const KEY_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no ivars, no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SipleEventHandler"]
    struct EventHandler;

    impl EventHandler {
        #[unsafe(method(handleGetURL:withReplyEvent:))]
        fn handle_get_url(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            match event
                .paramDescriptorForKeyword(KEY_DIRECT_OBJECT)
                .and_then(|d| d.stringValue())
            {
                Some(url) => deliver_url(url.to_string()),
                None => tracing::warn!("get-URL Apple Event without a URL"),
            }
        }

        #[unsafe(method(handleReopen:withReplyEvent:))]
        fn handle_reopen(&self, _event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            deliver_reopen();
        }
    }
);

/// Registers the Apple Event handlers. Must run after AppKit installed its own (it registers a
/// reopen handler of its own while launching), so [`early_init`] calls it from the
/// will-finish-launching notification.
fn install_event_handlers() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let handler: Retained<EventHandler> = unsafe { msg_send![EventHandler::alloc(mtm), init] };
    let mgr = NSAppleEventManager::sharedAppleEventManager();
    let target: &AnyObject = &handler;
    // SAFETY: both selectors exist on `EventHandler` with the (event, reply) signature.
    unsafe {
        mgr.setEventHandler_andSelector_forEventClass_andEventID(
            target,
            sel!(handleGetURL:withReplyEvent:),
            GURL,
            GURL,
        );
        mgr.setEventHandler_andSelector_forEventClass_andEventID(
            target,
            sel!(handleReopen:withReplyEvent:),
            AEVT,
            RAPP,
        );
    }
    std::mem::forget(handler); // the manager does not retain its handler
}

/// Call from `main()` on the main thread, before the event loop starts: installs the Apple
/// Event handlers as the app is about to finish launching, which is before the launch URL is
/// dispatched. Links queue until [`attach`].
pub fn early_init() {
    use block2::RcBlock;
    use objc2_app_kit::NSApplicationWillFinishLaunchingNotification;
    use objc2_foundation::{NSNotification, NSNotificationCenter};
    use std::ptr::NonNull;
    let center = NSNotificationCenter::defaultCenter();
    let block = RcBlock::new(|_n: NonNull<NSNotification>| install_event_handlers());
    // SAFETY: valid notification name and block; the observer token is intentionally leaked.
    let token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSApplicationWillFinishLaunchingNotification),
            None,
            None,
            &block,
        )
    };
    std::mem::forget(token);
}

// ---- app menu --------------------------------------------------------------------------------

/// The menu bar. Keep it alive for as long as the app runs.
pub struct AppMenu {
    _menu: Menu,
}

/// Menu item id of "Settings…".
const SETTINGS_ID: &str = "settings";

/// Builds and installs the menu: Siple (About, Settings…, Hide, Quit), Edit and Window.
///
/// The Edit items are custom menu items, not the predefined responder-chain ones. Those send
/// `copy:` / `paste:` / ... to a nil target, nothing in winit's view implements them, so AppKit
/// shows them disabled, yet `NSMenu performKeyEquivalent:` still returns YES for a matching
/// disabled item and swallows the key: egui never sees Cmd+C. Custom items route through the
/// global `MenuEvent` handler as [`EditCmd`]s (see `shell::edit_cmd`) and become egui input.
pub fn install_app_menu() -> Result<AppMenu, String> {
    let menu = Menu::new();
    let app = Submenu::new("Siple", true);
    let settings = MenuItem::with_id(
        SETTINGS_ID,
        "Settings…",
        true,
        Some(Accelerator::new(Modifiers::META, Code::Comma)),
    );
    app.append_items(&[
        &PredefinedMenuItem::about(
            None,
            Some(AboutMetadata {
                name: Some("Siple".into()),
                version: Some(siple_core::update::full_version().into()),
                ..Default::default()
            }),
        ),
        &PredefinedMenuItem::separator(),
        &settings,
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::services(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::quit(None),
    ])
    .map_err(|e| e.to_string())?;
    let edit = Submenu::new("Edit", true);
    let item = |c: EditCmd, mods: Modifiers, code: Code| {
        MenuItem::with_id(
            c.to_id(),
            c.title(),
            true,
            Some(Accelerator::new(mods, code)),
        )
    };
    let undo = item(EditCmd::Undo, Modifiers::META, Code::KeyZ);
    let redo = item(
        EditCmd::Redo,
        Modifiers::META | Modifiers::SHIFT,
        Code::KeyZ,
    );
    let cut = item(EditCmd::Cut, Modifiers::META, Code::KeyX);
    let copy = item(EditCmd::Copy, Modifiers::META, Code::KeyC);
    let paste = item(EditCmd::Paste, Modifiers::META, Code::KeyV);
    let select_all = item(EditCmd::SelectAll, Modifiers::META, Code::KeyA);
    edit.append_items(&[
        &undo,
        &redo,
        &PredefinedMenuItem::separator(),
        &cut,
        &copy,
        &paste,
        &select_all,
    ])
    .map_err(|e| e.to_string())?;
    let window = Submenu::new("Window", true);
    window
        .append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::close_window(None),
        ])
        .map_err(|e| e.to_string())?;
    menu.append_items(&[&app, &edit, &window])
        .map_err(|e| e.to_string())?;
    menu.init_for_nsapp();
    window.set_as_windows_menu_for_nsapp();
    Ok(AppMenu { _menu: menu })
}
