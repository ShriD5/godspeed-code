//! Port blocks for worktrees: consecutive ports, aligned to the block size,
//! below the OS ephemeral range and clear of other reservations and of any
//! port something on this machine is already listening on.

#[cfg(not(unix))]
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub const DEFAULT: Self = Self {
        start: 41000,
        end: 48999,
    };
}

/// The lowest aligned base whose whole block is unreserved and free.
pub fn allocate(
    range: PortRange,
    count: u16,
    reserved: &[(u16, u16)],
    is_free: impl Fn(u16) -> bool,
) -> Option<u16> {
    if count == 0 {
        return None;
    }
    let mut base = range.start;
    loop {
        let last = base.checked_add(count - 1)?;
        if last > range.end {
            return None;
        }
        let overlaps = reserved
            .iter()
            .any(|&(start, end)| base <= end && start <= last);
        if !overlaps && (base..=last).all(&is_free) {
            return Some(base);
        }
        base = base.checked_add(count)?;
    }
}

/// Nothing on this machine holds `port`, on any IPv4 or IPv6 address.
#[cfg(unix)]
pub fn is_free(port: u16) -> bool {
    !bind_in_use(libc::AF_INET, port) && !bind_in_use(libc::AF_INET6, port)
}

/// A bare wildcard `bind` without `SO_REUSEADDR` conflicts with every
/// address already bound to the port. It never listens, so it can't trip a
/// firewall prompt.
#[cfg(unix)]
fn bind_in_use(family: libc::c_int, port: u16) -> bool {
    // SAFETY: a fresh socket owned by this function, bound to a stack
    // address of the matching family and closed on every path.
    unsafe {
        let socket = libc::socket(family, libc::SOCK_STREAM, 0);
        if socket < 0 {
            return false;
        }
        let result = if family == libc::AF_INET6 {
            let only: libc::c_int = 1;
            libc::setsockopt(
                socket,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                (&only as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let mut address: libc::sockaddr_in6 = std::mem::zeroed();
            address.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            address.sin6_port = port.to_be();
            libc::bind(
                socket,
                (&address as *const libc::sockaddr_in6).cast(),
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        } else {
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_port = port.to_be();
            libc::bind(
                socket,
                (&address as *const libc::sockaddr_in).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        let in_use =
            result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EADDRINUSE);
        libc::close(socket);
        in_use
    }
}

#[cfg(not(unix))]
pub fn is_free(port: u16) -> bool {
    let addresses = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ];
    addresses
        .into_iter()
        .all(|address| match TcpListener::bind(address) {
            Ok(_) => true,
            Err(error) => error.kind() != std::io::ErrorKind::AddrInUse,
        })
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, TcpListener};

    use super::*;

    const RANGE: PortRange = PortRange {
        start: 4000,
        end: 4019,
    };

    #[test]
    fn skips_reserved_and_busy_blocks() {
        assert_eq!(allocate(RANGE, 5, &[], |_| true), Some(4000));
        assert_eq!(allocate(RANGE, 5, &[(4000, 4004)], |_| true), Some(4005));
        assert_eq!(allocate(RANGE, 5, &[(4003, 4003)], |_| true), Some(4005));
        assert_eq!(allocate(RANGE, 5, &[], |port| port != 4007), Some(4000));
        assert_eq!(
            allocate(RANGE, 5, &[(4000, 4004)], |port| port != 4007),
            Some(4010)
        );
        assert_eq!(
            allocate(RANGE, 5, &[(4000, 4014)], |port| port < 4015),
            None
        );
        assert_eq!(allocate(RANGE, 21, &[], |_| true), None);
    }

    #[test]
    fn a_listening_port_is_not_free() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_free(port));
        drop(listener);
        assert!(is_free(port));
    }
}
