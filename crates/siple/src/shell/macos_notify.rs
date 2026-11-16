//! `UNUserNotificationCenter` posting (bundled apps only; see `notify`).
//!
//! Setup (delegate, authorization request) happens once per process. The delegate lets
//! notifications show as banners while Siple is the frontmost app; without it macOS silences
//! them exactly when the window is up.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_foundation::{NSError, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

static SEQ: AtomicU64 = AtomicU64::new(0);
static SETUP_DONE: AtomicBool = AtomicBool::new(false);
static AUTH_LOGGED: AtomicBool = AtomicBool::new(false);
static POST_LOGGED: AtomicBool = AtomicBool::new(false);

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no ivars, no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SipleNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::List
                | UNNotificationPresentationOptions::Sound,));
        }
    }
);

thread_local! {
    /// The center keeps its delegate weakly: hold it for the life of the main thread.
    static DELEGATE: RefCell<Option<Retained<NotificationDelegate>>> = const { RefCell::new(None) };
}

fn log_once(flag: &AtomicBool, msg: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        tracing::info!("{msg}");
    }
}

/// First use only: delegate (main thread) and the authorization request (prompts the user).
fn setup(center: &UNUserNotificationCenter) {
    if SETUP_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(mtm) = MainThreadMarker::new() {
        let d: Retained<NotificationDelegate> =
            unsafe { msg_send![NotificationDelegate::alloc(mtm), init] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*d)));
        DELEGATE.with(|s| *s.borrow_mut() = Some(d));
    } else {
        tracing::warn!("notification delegate not installed (not the main thread)");
    }
    let auth = RcBlock::new(|granted: objc2::runtime::Bool, err: *mut NSError| {
        if !granted.as_bool() {
            let why = if err.is_null() {
                "permission not granted".to_string()
            } else {
                // SAFETY: non-null NSError from the completion handler.
                unsafe { (*err).localizedDescription().to_string() }
            };
            log_once(&AUTH_LOGGED, &format!("notification authorization: {why}"));
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &auth,
    );
}

/// Posts the notification; `on_posted` runs once the OS accepted it (not on failure).
pub fn post(
    title: &str,
    body: &str,
    identifier: Option<String>,
    on_posted: Option<Box<dyn FnOnce() + Send>>,
) -> Result<(), String> {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    setup(&center);
    let content: Retained<UNMutableNotificationContent> = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let id = NSString::from_str(
        &identifier.unwrap_or_else(|| format!("siple-{}", SEQ.fetch_add(1, Ordering::Relaxed))),
    );
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    let on_posted = std::sync::Mutex::new(on_posted);
    let done = RcBlock::new(move |err: *mut NSError| {
        if err.is_null() {
            if let Some(f) = on_posted.lock().ok().and_then(|mut g| g.take()) {
                f();
            }
        } else {
            // SAFETY: non-null NSError from the completion handler.
            let why = unsafe { (*err).localizedDescription().to_string() };
            log_once(&POST_LOGGED, &format!("notification not posted: {why}"));
        }
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&done));
    Ok(())
}

/// Removes a delivered notification (the call stopped ringing).
pub fn withdraw(identifier: &str) {
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let ids = objc2_foundation::NSArray::from_retained_slice(&[NSString::from_str(identifier)]);
    center.removeDeliveredNotificationsWithIdentifiers(&ids);
}
