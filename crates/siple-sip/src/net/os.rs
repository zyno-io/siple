//! OS network-change watchers. Each platform module is self-contained (std + the platform API
//! only), so it can be type-checked on its own.
//!
//! - **macOS**: an `SCDynamicStore` session (SystemConfiguration) watching the global
//!   IPv4/IPv6/DNS state and the per-interface IPv4/IPv6/link keys, on a dedicated run-loop
//!   thread.
//! - **Windows**: `NotifyIpInterfaceChange`, `NotifyUnicastIpAddressChange` and
//!   `NotifyRouteChange2` (iphlpapi).
//! - **Linux**: an `AF_NETLINK`/`NETLINK_ROUTE` socket subscribed to the link, address and route
//!   multicast groups, read on a dedicated thread. Messages are not parsed: any of them is a
//!   trigger (the service debounces and the account re-probes). Pure libc syscalls, no extra
//!   shared libraries.

use std::io;

use tokio::sync::mpsc::UnboundedSender;

use super::{NetMonitor, NetReason};

#[cfg(target_os = "linux")]
#[path = "os_linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "os_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "os_windows.rs"]
mod platform;

/// Identity of the default gateway / network, where the OS tells it cheaply: macOS router address
/// plus network signature from SystemConfiguration, Linux the default route's gateway from
/// `/proc/net/route`. `None` elsewhere (Windows lists gateways with the adapters).
pub fn default_gateway() -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::gateway()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// [`NetMonitor`] for the running OS.
pub struct OsMonitor;

impl NetMonitor for OsMonitor {
    fn start(&self, sink: UnboundedSender<NetReason>) -> io::Result<Box<dyn Send>> {
        start(sink)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn start(sink: UnboundedSender<NetReason>) -> io::Result<Box<dyn Send>> {
    platform::start(sink).map(|g| Box::new(g) as Box<dyn Send>)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn start(_sink: UnboundedSender<NetReason>) -> io::Result<Box<dyn Send>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no OS network watcher for this platform",
    ))
}
