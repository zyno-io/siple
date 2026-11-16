//! Linux: rtnetlink multicast groups over a raw `AF_NETLINK` socket.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::UnboundedSender;

use super::super::NetReason;

pub struct Guard {
    stop: Arc<AtomicBool>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed); // the thread notices within its poll timeout
    }
}

/// Owned netlink socket (closed on drop).
struct Fd(libc::c_int);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: we own the descriptor.
        unsafe { libc::close(self.0) };
    }
}

fn open() -> io::Result<Fd> {
    // SAFETY: plain syscalls with valid arguments; the descriptor is owned by `Fd` right away.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_ROUTE,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = Fd(fd);
        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        addr.nl_groups = (libc::RTMGRP_LINK
            | libc::RTMGRP_IPV4_IFADDR
            | libc::RTMGRP_IPV4_ROUTE
            | libc::RTMGRP_IPV6_IFADDR
            | libc::RTMGRP_IPV6_ROUTE) as u32;
        let rc = libc::bind(
            fd.0,
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

pub fn start(sink: UnboundedSender<NetReason>) -> io::Result<Guard> {
    let fd = open()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::Builder::new()
        .name("net-watch".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            while !stop2.load(Ordering::Relaxed) {
                let mut pfd = libc::pollfd {
                    fd: fd.0,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: one valid pollfd.
                let n = unsafe { libc::poll(&mut pfd, 1, 1000) };
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if n == 0 {
                    continue;
                }
                // Drain everything queued; one report per burst is enough.
                let mut got = false;
                loop {
                    // SAFETY: `buf` is valid for `buf.len()` bytes.
                    let r = unsafe { libc::recv(fd.0, buf.as_mut_ptr().cast(), buf.len(), 0) };
                    if r > 0 {
                        got = true;
                    } else {
                        // EAGAIN (drained), ENOBUFS (overrun: still a change) or an error.
                        if r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS)
                        {
                            got = true;
                            continue;
                        }
                        break;
                    }
                }
                if got && sink.send(NetReason::Os("netlink".into())).is_err() {
                    break;
                }
            }
        })?;
    Ok(Guard { stop })
}

/// Default gateway(s) from `/proc/net/route` (IPv4; hex little-endian addresses).
pub fn gateway() -> Option<String> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    let mut gws: Vec<String> = text
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (iface, dest, gw, flags) = (f.first()?, f.get(1)?, f.get(2)?, f.get(3)?);
            let flags = u32::from_str_radix(flags, 16).ok()?;
            if *dest != "00000000" || flags & 0x2 == 0 {
                return None; // not a default route via a gateway
            }
            let g = u32::from_str_radix(gw, 16).ok()?.to_le_bytes();
            Some(format!("{iface}:{}.{}.{}.{}", g[0], g[1], g[2], g[3]))
        })
        .collect();
    gws.sort();
    (!gws.is_empty()).then(|| gws.join(","))
}
