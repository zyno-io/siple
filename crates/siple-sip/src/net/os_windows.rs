//! Windows: IP Helper change notifications (interface, unicast address, route).

use std::ffi::c_void;
use std::io;

use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, MIB_IPFORWARD_ROW2, MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE,
    MIB_UNICASTIPADDRESS_ROW, NotifyIpInterfaceChange, NotifyRouteChange2,
    NotifyUnicastIpAddressChange,
};
use windows_sys::Win32::Networking::WinSock::AF_UNSPEC;

use super::super::NetReason;

pub struct Guard {
    handles: [HANDLE; 3],
    ctx: *mut UnboundedSender<NetReason>,
}

// SAFETY: the handles are opaque tokens only passed to `CancelMibChangeNotify2`, and the context
// holds a `Send + Sync` sender.
unsafe impl Send for Guard {}

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: each handle came from a successful Notify* call; cancelling waits for running
        // callbacks, after which nothing uses `ctx` any more.
        unsafe {
            for h in self.handles {
                CancelMibChangeNotify2(h);
            }
            drop(Box::from_raw(self.ctx));
        }
    }
}

fn report(ctx: *const c_void, what: &str) {
    // SAFETY: `ctx` is the sender leaked in `start`, alive until `Guard::drop`.
    let sink = unsafe { &*(ctx as *const UnboundedSender<NetReason>) };
    let _ = sink.send(NetReason::Os(what.into()));
}

unsafe extern "system" fn on_interface(
    ctx: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    report(ctx, "interface");
}

unsafe extern "system" fn on_address(
    ctx: *const c_void,
    _row: *const MIB_UNICASTIPADDRESS_ROW,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    report(ctx, "addr");
}

unsafe extern "system" fn on_route(
    ctx: *const c_void,
    _row: *const MIB_IPFORWARD_ROW2,
    _kind: MIB_NOTIFICATION_TYPE,
) {
    report(ctx, "route");
}

pub fn start(sink: UnboundedSender<NetReason>) -> io::Result<Guard> {
    let ctx = Box::into_raw(Box::new(sink));
    let mut handles: [HANDLE; 3] = [std::ptr::null_mut(); 3];
    let af = AF_UNSPEC as _;
    // SAFETY: `ctx` outlives the registrations (freed in `Guard::drop` after cancelling them).
    let codes = unsafe {
        [
            NotifyIpInterfaceChange(af, Some(on_interface), ctx.cast(), false, &mut handles[0]),
            NotifyUnicastIpAddressChange(af, Some(on_address), ctx.cast(), false, &mut handles[1]),
            NotifyRouteChange2(af, Some(on_route), ctx.cast(), false, &mut handles[2]),
        ]
    };
    if let Some(&code) = codes.iter().find(|c| **c != 0) {
        // SAFETY: only the registrations that succeeded have non-null handles.
        unsafe {
            for h in handles.into_iter().filter(|h| !h.is_null()) {
                CancelMibChangeNotify2(h);
            }
            drop(Box::from_raw(ctx));
        }
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(Guard { handles, ctx })
}
