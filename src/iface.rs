use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use crate::wire::MAGIC;

/// Arbitrary high port. Only ever carries a four-byte probe and a
/// six-byte reply.
pub const DISCOVERY_PORT: u16 = 47654;
const PROBE: u8 = b'?';
const REPLY: u8 = b'!';

#[derive(Debug, Clone, Copy)]
pub struct Bridge {
    pub addr: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub mtu: Option<u32>,
}

impl Bridge {
    /// The subnet-directed broadcast address: network bits kept, host bits set.
    pub fn broadcast(&self) -> Ipv4Addr {
        let addr = u32::from(self.addr);
        let mask = u32::from(self.netmask);
        Ipv4Addr::from(addr | !mask)
    }

    /// Whether an address sits on this interface's subnet.
    pub fn contains(&self, other: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        (u32::from(self.addr) & mask) == (u32::from(other) & mask)
    }
}

fn no_bridge_error(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "Thunderbolt Bridge ({name}) has no IPv4 address.\n\
             Connect the cable, then check System Settings > Network > Thunderbolt Bridge."
        ),
    )
}

/// Look up the address, netmask and MTU of a network interface.
///
/// There is deliberately no fallback to another interface or to 0.0.0.0:
/// binding only to the Thunderbolt link is the entire security model.
pub fn find_bridge(name: &str) -> io::Result<Bridge> {
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut found: Option<(Ipv4Addr, Ipv4Addr)> = None;
    let mut mtu: Option<u32> = None;

    let mut cursor = list;
    while !cursor.is_null() {
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;

        if entry.ifa_name.is_null() {
            continue;
        }
        let entry_name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) };
        if entry_name.to_bytes() != name.as_bytes() {
            continue;
        }
        if entry.ifa_addr.is_null() {
            continue;
        }

        let family = unsafe { (*entry.ifa_addr).sa_family } as libc::c_int;
        if family == libc::AF_INET && !entry.ifa_netmask.is_null() {
            let addr = unsafe { *entry.ifa_addr.cast::<libc::sockaddr_in>() };
            let mask = unsafe { *entry.ifa_netmask.cast::<libc::sockaddr_in>() };
            found = Some((
                Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)),
                Ipv4Addr::from(u32::from_be(mask.sin_addr.s_addr)),
            ));
        } else if family == libc::AF_LINK && !entry.ifa_data.is_null() {
            // MTU only drives a printed hint, so treat it as best-effort.
            let data = unsafe { *entry.ifa_data.cast::<libc::if_data>() };
            mtu = Some(data.ifi_mtu);
        }
    }

    unsafe { libc::freeifaddrs(list) };

    let (addr, netmask) = found.ok_or_else(|| no_bridge_error(name))?;
    Ok(Bridge { addr, netmask, mtu })
}

/// Answer discovery probes from the Thunderbolt subnet. Runs until the
/// process exits; call it on a detached thread.
///
/// Binds the wildcard address because a socket bound to a specific unicast
/// address does not receive broadcast packets on BSD. Probes from outside
/// the bridge subnet are ignored, and no probe can cause a write.
pub fn serve_discovery(bridge: Bridge, tcp_port: u16) -> io::Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT))?;
    let mut buf = [0u8; 8];

    loop {
        let (len, from) = socket.recv_from(&mut buf)?;
        let std::net::SocketAddr::V4(from) = from else {
            continue;
        };
        if !bridge.contains(*from.ip()) {
            continue;
        }
        if len < 5 || u32::from_le_bytes(buf[..4].try_into().unwrap()) != MAGIC || buf[4] != PROBE {
            continue;
        }

        let mut reply = Vec::with_capacity(7);
        reply.extend_from_slice(&MAGIC.to_le_bytes());
        reply.push(REPLY);
        reply.extend_from_slice(&tcp_port.to_le_bytes());
        let _ = socket.send_to(&reply, from);
    }
}

/// Broadcast on the bridge subnet and return the first peer that answers.
pub fn find_peer(bridge: &Bridge) -> io::Result<SocketAddrV4> {
    let socket = UdpSocket::bind((bridge.addr, 0))?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;

    let mut probe = Vec::with_capacity(5);
    probe.extend_from_slice(&MAGIC.to_le_bytes());
    probe.push(PROBE);

    let target = SocketAddrV4::new(bridge.broadcast(), DISCOVERY_PORT);
    let mut buf = [0u8; 8];

    for _ in 0..3 {
        socket.send_to(&probe, target)?;
        match socket.recv_from(&mut buf) {
            Ok((len, std::net::SocketAddr::V4(from))) => {
                if len >= 7
                    && u32::from_le_bytes(buf[..4].try_into().unwrap()) == MAGIC
                    && buf[4] == REPLY
                {
                    let port = u16::from_le_bytes([buf[5], buf[6]]);
                    return Ok(SocketAddrV4::new(*from.ip(), port));
                }
            }
            Ok(_) => continue,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::TimedOut => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "No thunderflash receiver answered on the Thunderbolt link.\n\
         Check that the other Mac is running `tf recv`, the cable is connected,\n\
         and Thunderbolt Bridge is enabled on both machines.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn bridge(addr: [u8; 4], mask: [u8; 4]) -> Bridge {
        Bridge {
            addr: addr.into(),
            netmask: mask.into(),
            mtu: None,
        }
    }

    #[test]
    fn broadcast_is_the_host_bits_set() {
        let b = bridge([169, 254, 12, 7], [255, 255, 0, 0]);
        assert_eq!(b.broadcast(), Ipv4Addr::new(169, 254, 255, 255));

        let b = bridge([10, 0, 0, 1], [255, 255, 255, 0]);
        assert_eq!(b.broadcast(), Ipv4Addr::new(10, 0, 0, 255));
    }

    #[test]
    fn contains_accepts_addresses_on_the_same_subnet() {
        let b = bridge([169, 254, 12, 7], [255, 255, 0, 0]);
        assert!(b.contains(Ipv4Addr::new(169, 254, 200, 3)));
        assert!(b.contains(Ipv4Addr::new(169, 254, 12, 7)));
    }

    #[test]
    fn contains_rejects_addresses_from_other_networks() {
        let b = bridge([169, 254, 12, 7], [255, 255, 0, 0]);
        assert!(!b.contains(Ipv4Addr::new(192, 168, 1, 50)));
        assert!(!b.contains(Ipv4Addr::new(10, 0, 0, 1)));
    }

    #[test]
    fn find_bridge_reports_a_clear_error_for_an_absent_interface() {
        let err = find_bridge("nosuchif0").unwrap_err();
        assert!(
            err.to_string().contains("Thunderbolt Bridge"),
            "error should tell the user what to do, got: {err}"
        );
    }

    #[test]
    fn discovery_round_trip_over_loopback() {
        // Loopback stands in for bridge0. macOS does not deliver
        // 127.255.255.255 broadcasts, so a /32 netmask makes broadcast()
        // return 127.0.0.1 itself and the probe goes straight there.
        let b = bridge([127, 0, 0, 1], [255, 255, 255, 255]);
        let responder = b;
        std::thread::spawn(move || {
            let _ = serve_discovery(responder, 4321);
        });
        std::thread::sleep(std::time::Duration::from_millis(200));

        let peer = find_peer(&b).expect("sender should find the responder");
        assert_eq!(peer.port(), 4321);
    }
}
