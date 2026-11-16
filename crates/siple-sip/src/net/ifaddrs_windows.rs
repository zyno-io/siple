//! Interface listing via `GetAdaptersAddresses` (Windows).

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
};

use super::InterfaceInfo;

pub fn list() -> io::Result<Vec<InterfaceInfo>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut len: u32 = 16 * 1024;
    // 8-byte aligned buffer.
    let mut buf: Vec<u64> = Vec::new();
    let mut filled = false;
    for _ in 0..4 {
        buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` is at least `len` bytes and suitably aligned for the adapter list.
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &mut len,
            )
        };
        match rc {
            0 => {
                filled = true;
                break;
            }
            111 => continue, // ERROR_BUFFER_OVERFLOW: `len` now holds the size needed
            e => return Err(io::Error::from_raw_os_error(e as i32)),
        }
    }
    if !filled {
        // Never parse a buffer GetAdaptersAddresses did not fill.
        return Err(io::Error::other(
            "GetAdaptersAddresses kept reporting a too-small buffer",
        ));
    }
    let mut out = Vec::new();
    // SAFETY: the list was filled by GetAdaptersAddresses and lives in `buf`.
    unsafe {
        let mut cur = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        while !cur.is_null() {
            let a = &*cur;
            cur = a.Next;
            let name = if a.FriendlyName.is_null() {
                String::new()
            } else {
                let mut n = 0;
                while *a.FriendlyName.add(n) != 0 {
                    n += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(a.FriendlyName, n))
            };
            let mut addrs = Vec::new();
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = &*u;
                u = ua.Next;
                addrs.extend(parse(ua.Address.lpSockaddr));
            }
            let mut gateways = Vec::new();
            let mut g = a.FirstGatewayAddress;
            while !g.is_null() {
                let ga = &*g;
                g = ga.Next;
                gateways.extend(parse(ga.Address.lpSockaddr));
            }
            out.push(InterfaceInfo {
                name,
                up: a.OperStatus == IfOperStatusUp,
                addrs,
                gateways,
            });
        }
    }
    Ok(out)
}

/// # Safety
/// `sa` must be null or point to a valid `SOCKADDR_IN` / `SOCKADDR_IN6`.
unsafe fn parse(sa: *const windows_sys::Win32::Networking::WinSock::SOCKADDR) -> Option<IpAddr> {
    if sa.is_null() {
        return None;
    }
    // SAFETY: per the contract above.
    unsafe {
        match (*sa).sa_family {
            f if f == AF_INET => {
                let s = &*(sa as *const SOCKADDR_IN);
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    s.sin_addr.S_un.S_addr,
                ))))
            }
            f if f == AF_INET6 => {
                let s = &*(sa as *const SOCKADDR_IN6);
                Some(IpAddr::V6(Ipv6Addr::from(s.sin6_addr.u.Byte)))
            }
            _ => None,
        }
    }
}
