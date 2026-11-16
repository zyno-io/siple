//! RTP/RTCP socket pair allocation from `[media].rtp_port_range`.
//!
//! RTP gets an even port and RTCP the next (odd) one. The scan starts at a
//! random even port and probes linearly with wrap-around. Sockets are created
//! with `socket2`; on Windows `SO_EXCLUSIVEADDRUSE` is set (and
//! `SO_REUSEADDR` never is) so another process cannot share the port.

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};

use anyhow::{Result, bail};
use socket2::{Domain, Protocol, Socket, Type};

/// A freshly bound (non-blocking, std) RTP/RTCP pair.
#[derive(Debug)]
pub struct PortPair {
    pub rtp: UdpSocket,
    pub rtcp: UdpSocket,
    /// Even RTP port; RTCP is `rtp_port + 1`.
    pub rtp_port: u16,
}

#[cfg(windows)]
fn set_exclusive(s: &Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        SO_EXCLUSIVEADDRUSE, SOCKET, SOL_SOCKET, setsockopt,
    };
    let on: i32 = 1;
    // SAFETY: valid socket handle and a pointer to a live 4-byte option value.
    let r = unsafe {
        setsockopt(
            s.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            &on as *const i32 as *const u8,
            std::mem::size_of::<i32>() as i32,
        )
    };
    if r != 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_exclusive(_: &Socket) -> io::Result<()> {
    Ok(())
}

/// Bind one non-blocking UDP socket (exclusive on Windows).
pub fn bind_udp(addr: SocketAddr) -> io::Result<UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let s = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    set_exclusive(&s)?;
    // Bursty senders + a busy tokio runtime: give the kernel some slack.
    let _ = s.set_recv_buffer_size(512 * 1024);
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    Ok(s.into())
}

fn random_u16() -> u16 {
    let mut b = [0u8; 2];
    if getrandom::fill(&mut b).is_err() {
        return 0;
    }
    u16::from_be_bytes(b)
}

fn in_use(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
    )
}

/// Allocate an even/odd pair inside `range` (inclusive) on `bind_ip`.
pub fn allocate_pair(bind_ip: IpAddr, range: [u16; 2]) -> Result<PortPair> {
    let first = range[0].saturating_add(range[0] & 1); // round up to even
    let last = range[1];
    if last < first.saturating_add(1) {
        bail!("rtp_port_range {range:?} holds no even/odd port pair");
    }
    let count = ((last - 1 - first) / 2) as usize + 1;
    let start = random_u16() as usize % count;
    for i in 0..count {
        let port = first + (((start + i) % count) * 2) as u16;
        let rtp = match bind_udp(SocketAddr::new(bind_ip, port)) {
            Ok(s) => s,
            Err(e) if in_use(&e) => continue,
            Err(e) => bail!("cannot bind RTP socket on {bind_ip}:{port}: {e}"),
        };
        match bind_udp(SocketAddr::new(bind_ip, port + 1)) {
            Ok(rtcp) => {
                return Ok(PortPair {
                    rtp,
                    rtcp,
                    rtp_port: port,
                });
            }
            Err(e) if in_use(&e) => continue,
            Err(e) => bail!("cannot bind RTCP socket on {bind_ip}:{}: {e}", port + 1),
        }
    }
    bail!(
        "no free RTP/RTCP port pair in range {}-{}",
        range[0],
        range[1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const LO: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    #[test]
    fn even_rtp_and_adjacent_rtcp() {
        for _ in 0..20 {
            let p = allocate_pair(LO, [41000, 41099]).unwrap();
            assert_eq!(p.rtp_port % 2, 0);
            assert!((41000..=41098).contains(&p.rtp_port));
            assert_eq!(p.rtp.local_addr().unwrap().port(), p.rtp_port);
            assert_eq!(p.rtcp.local_addr().unwrap().port(), p.rtp_port + 1);
        }
    }

    #[test]
    fn odd_start_rounds_up() {
        let p = allocate_pair(LO, [42001, 42010]).unwrap();
        assert!(p.rtp_port >= 42002 && p.rtp_port.is_multiple_of(2));
    }

    #[test]
    fn exhausted_range_errors() {
        let range = [43100, 43103]; // two pairs
        let a = allocate_pair(LO, range).unwrap();
        let b = allocate_pair(LO, range).unwrap();
        assert_ne!(a.rtp_port, b.rtp_port);
        let err = allocate_pair(LO, range).unwrap_err();
        assert!(err.to_string().contains("no free"), "{err}");
        drop(a);
        assert!(allocate_pair(LO, range).is_ok());
    }

    #[test]
    fn degenerate_range_errors() {
        assert!(allocate_pair(LO, [44000, 44000]).is_err());
        assert!(allocate_pair(LO, [44001, 44001]).is_err());
    }

    #[test]
    fn non_local_ip_fails_fast() {
        let ip: IpAddr = "203.0.113.77".parse().unwrap();
        assert!(allocate_pair(ip, [45000, 45999]).is_err());
    }
}
