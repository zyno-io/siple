//! Linux: rtnetlink multicast groups over a raw `AF_NETLINK` socket.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::UnboundedSender;

use super::super::NetReason;

pub struct Guard {
    stop: Arc<AtomicBool>,
    /// The netlink descriptor (owned by the watcher thread); only tests look at it.
    #[cfg(test)]
    fd: libc::c_int,
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
    #[cfg(test)]
    let raw = fd.0;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::Builder::new()
        .name("net-watch".into())
        .spawn(move || {
            // Capture the whole `Fd`, not just `fd.0` (edition 2021 closures capture disjoint
            // fields): otherwise the owner is dropped when `start` returns, the descriptor is
            // closed under the thread, and its number is soon reused by an unrelated socket that
            // this loop then polls and drains.
            let fd = fd;
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
    Ok(Guard {
        stop,
        #[cfg(test)]
        fd: raw,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The watcher thread owns the netlink socket: it must still be open after `start` returned
    /// (a descriptor closed under the thread gets reused by unrelated sockets, whose data the
    /// thread would then steal) and stay a netlink socket.
    #[test]
    fn the_netlink_socket_stays_open_while_watching() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let guard = start(tx).expect("netlink socket");
        let fd = guard.fd;
        let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            open(fd),
            "netlink fd {fd} was closed while the watcher runs"
        );
        // Still a netlink socket, not a reused number (SO_PROTOCOL 0 would also match a Unix
        // socket, so check the domain too).
        let opt = |name| {
            let mut v: libc::c_int = -1;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    name,
                    std::ptr::addr_of_mut!(v).cast(),
                    &mut len,
                )
            };
            assert_eq!(rc, 0);
            v
        };
        assert_eq!(opt(libc::SO_DOMAIN), libc::AF_NETLINK);
        assert_eq!(opt(libc::SO_PROTOCOL), libc::NETLINK_ROUTE);
        drop(guard);
    }
}
