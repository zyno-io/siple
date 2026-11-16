//! Interface listing via `getifaddrs` (macOS, Linux, BSD).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::InterfaceInfo;

pub fn list() -> io::Result<Vec<InterfaceInfo>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: standard getifaddrs/freeifaddrs pairing; the list is only read before it is freed.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut out: Vec<InterfaceInfo> = Vec::new();
        let mut cur = head;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name)
                .to_string_lossy()
                .into_owned();
            let up = ifa.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0
                && ifa.ifa_flags & (libc::IFF_RUNNING as libc::c_uint) != 0;
            let addr = if ifa.ifa_addr.is_null() {
                None
            } else {
                match libc::c_int::from((*ifa.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                        Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            sin.sin_addr.s_addr,
                        ))))
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(ifa.ifa_addr as *const libc::sockaddr_in6);
                        Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
                    }
                    _ => None,
                }
            };
            let entry = match out.iter_mut().find(|i| i.name == name) {
                Some(e) => e,
                None => {
                    out.push(InterfaceInfo {
                        name,
                        up,
                        addrs: Vec::new(),
                        gateways: Vec::new(),
                    });
                    out.last_mut().expect("just pushed")
                }
            };
            entry.up |= up;
            entry.addrs.extend(addr);
        }
        libc::freeifaddrs(head);
        Ok(out)
    }
}
