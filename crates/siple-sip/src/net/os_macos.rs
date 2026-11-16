//! macOS: SystemConfiguration dynamic store notifications on a run-loop thread.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use core_foundation::array::CFArray;
use core_foundation::runloop::{CFRunLoop, CFRunLoopRunResult, kCFRunLoopDefaultMode};
use core_foundation::string::CFString;
use system_configuration::dynamic_store::{
    SCDynamicStore, SCDynamicStoreBuilder, SCDynamicStoreCallBackContext,
};
use tokio::sync::mpsc::UnboundedSender;

use super::super::NetReason;

pub struct Guard {
    stop: Arc<AtomicBool>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed); // the run loop wakes within a second
    }
}

/// Hint detail for one changed dynamic-store key; `None` for interfaces that never carry the
/// registrar path (awdl, llw, bridge, ...), filtered here by key name.
fn classify_key(key: &str) -> Option<&'static str> {
    if key.contains("/Global/DNS") {
        return Some("dns");
    }
    if key.contains("/Global/") {
        return Some("global");
    }
    let iface = key
        .split("/Interface/")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or("");
    if super::super::ignored_interface(iface) {
        return None;
    }
    Some(if key.ends_with("/Link") {
        "link"
    } else {
        "addr"
    })
}

/// The most specific hint among the changed keys (dns > global > link > addr).
fn classify_keys<'a>(keys: impl Iterator<Item = &'a str>) -> Option<&'static str> {
    let rank = |w: &str| {
        ["dns", "global", "link", "addr"]
            .iter()
            .position(|x| *x == w)
    };
    keys.filter_map(classify_key).min_by_key(|w| rank(w))
}

fn changed(_store: SCDynamicStore, keys: CFArray<CFString>, sink: &mut UnboundedSender<NetReason>) {
    let names: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    if let Some(what) = classify_keys(names.iter().map(String::as_str)) {
        let _ = sink.send(NetReason::Os(what.into()));
    }
}

/// Router address plus the primary interface's network signature (changes with the Wi-Fi
/// network even when the IPv4 address stays the same).
pub fn gateway() -> Option<String> {
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::propertylist::CFPropertyList;

    fn dict(p: CFPropertyList) -> Option<CFDictionary<CFString, core_foundation::base::CFType>> {
        p.downcast_into::<CFDictionary>()
            .map(|d| unsafe { CFDictionary::wrap_under_get_rule(d.as_concrete_TypeRef()) })
    }
    fn text(
        d: &CFDictionary<CFString, core_foundation::base::CFType>,
        key: &str,
    ) -> Option<String> {
        let k = CFString::new(key);
        d.find(&k)?.downcast::<CFString>().map(|s| s.to_string())
    }
    let store = SCDynamicStoreBuilder::new("siple-gateway").build()?;
    let global = dict(store.get("State:/Network/Global/IPv4")?)?;
    let router = text(&global, "Router").unwrap_or_default();
    let iface = text(&global, "PrimaryInterface").unwrap_or_default();
    let signature = store
        .get(format!("State:/Network/Interface/{iface}/IPv4").as_str())
        .and_then(dict)
        .and_then(|d| text(&d, "NetworkSignature"))
        .unwrap_or_default();
    Some(format!("{iface}|{router}|{signature}"))
}

fn watch(sink: UnboundedSender<NetReason>) -> io::Result<CFRunLoopRunner> {
    let store = SCDynamicStoreBuilder::new("siple-netwatch")
        .callback_context(SCDynamicStoreCallBackContext {
            callout: changed,
            info: sink,
        })
        .build()
        .ok_or_else(|| io::Error::other("cannot create an SCDynamicStore session"))?;
    let keys = CFArray::from_CFTypes(&[
        CFString::new("State:/Network/Global/IPv4"),
        CFString::new("State:/Network/Global/IPv6"),
        CFString::new("State:/Network/Global/DNS"),
    ]);
    let patterns = CFArray::from_CFTypes(&[
        CFString::new("State:/Network/Interface/.*/IPv4"),
        CFString::new("State:/Network/Interface/.*/IPv6"),
        CFString::new("State:/Network/Interface/.*/Link"),
    ]);
    if !store.set_notification_keys(&keys, &patterns) {
        return Err(io::Error::other("SCDynamicStoreSetNotificationKeys failed"));
    }
    let source = store
        .create_run_loop_source()
        .ok_or_else(|| io::Error::other("cannot create a run-loop source"))?;
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopDefaultMode });
    Ok(CFRunLoopRunner {
        _store: store,
        _source: source,
    })
}

/// Keeps the store and its source alive while the loop runs.
struct CFRunLoopRunner {
    _store: SCDynamicStore,
    _source: core_foundation::runloop::CFRunLoopSource,
}

pub fn start(sink: UnboundedSender<NetReason>) -> io::Result<Guard> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<io::Result<()>>();
    std::thread::Builder::new()
        .name("net-watch".into())
        .spawn(move || {
            let _alive = match watch(sink) {
                Ok(a) => {
                    let _ = ready_tx.send(Ok(()));
                    a
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            while !stop2.load(Ordering::Relaxed) {
                let r = CFRunLoop::run_in_mode(
                    unsafe { kCFRunLoopDefaultMode },
                    Duration::from_secs(1),
                    false,
                );
                if r == CFRunLoopRunResult::Finished {
                    std::thread::sleep(Duration::from_secs(1)); // nothing to run: do not spin
                }
            }
        })?;
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(Guard { stop }),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the network watcher thread did not start",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_link_and_global_keys_are_decisive_and_virtual_interfaces_are_dropped() {
        assert_eq!(classify_key("State:/Network/Global/DNS"), Some("dns"));
        assert_eq!(classify_key("State:/Network/Global/IPv4"), Some("global"));
        assert_eq!(
            classify_key("State:/Network/Interface/en0/Link"),
            Some("link")
        );
        assert_eq!(
            classify_key("State:/Network/Interface/en0/IPv4"),
            Some("addr")
        );
        assert_eq!(
            classify_key("State:/Network/Interface/utun3/IPv6"),
            Some("addr")
        );
        for n in ["awdl0", "llw0", "bridge100", "vmenet1", "anpi0"] {
            assert_eq!(
                classify_key(&format!("State:/Network/Interface/{n}/Link")),
                None,
                "{n}"
            );
            assert_eq!(
                classify_key(&format!("State:/Network/Interface/{n}/IPv6")),
                None,
                "{n}"
            );
        }
        // The most specific hint of a batch wins; a batch of noise sends nothing.
        let batch = [
            "State:/Network/Interface/awdl0/Link",
            "State:/Network/Interface/en0/IPv4",
            "State:/Network/Interface/en0/Link",
        ];
        assert_eq!(classify_keys(batch.into_iter()), Some("link"));
        assert_eq!(
            classify_keys(["State:/Network/Interface/awdl0/IPv6"].into_iter()),
            None
        );
    }
}
