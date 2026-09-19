//! Addresses, ports and secrets for a published session.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};

use nd_core::{NdError, Result};

/// The ports tried for the front door, in order.
///
/// A fixed, short port is part of the address the person types on a TV
/// remote, so the first one is what appears in the documentation and the
/// rest only exist so a busy machine still works.
pub const PORTS: [u16; 6] = [8080, 8081, 8082, 8083, 8084, 8085];

fn net_err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// The address of this machine on the network the receivers are on.
///
/// A "connected" UDP socket sends nothing but makes the kernel choose the
/// route, and with it the source address. On a machine with Docker bridges, a
/// VPN and Wi-Fi this picks the interface that reaches the outside world,
/// which is the one the TV and the phone are on too. Nothing leaves the
/// machine.
pub fn lan_ip() -> Result<IpAddr> {
    // Well-known public addresses; only routing is consulted, never contacted.
    for probe in [
        Ipv4Addr::new(1, 1, 1, 1),
        Ipv4Addr::new(8, 8, 8, 8),
        Ipv4Addr::new(192, 168, 1, 1),
    ] {
        if let Ok(ip) = source_towards(IpAddr::V4(probe)) {
            if !ip.is_loopback() && !ip.is_unspecified() {
                return Ok(ip);
            }
        }
    }
    Err(NdError::Network(
        "this computer is not connected to a network".into(),
    ))
}

fn source_towards(peer: IpAddr) -> Result<IpAddr> {
    let socket =
        UdpSocket::bind(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)).map_err(net_err)?;
    socket.connect((peer, 9)).map_err(net_err)?;
    Ok(socket.local_addr().map_err(net_err)?.ip())
}

/// A free TCP port on the loopback interface, for the WHEP element behind the
/// front door.
///
/// The port is released before being handed back, so another process could
/// in theory take it in between; the element then fails to bind and the
/// session reports it, rather than silently serving nothing.
pub fn free_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(net_err)?;
    Ok(listener.local_addr().map_err(net_err)?.port())
}

/// Binds the front door on `ip`, on the first free port of [`PORTS`].
pub async fn bind_front_door(ip: IpAddr) -> Result<tokio::net::TcpListener> {
    let mut last = None;
    for port in PORTS {
        match tokio::net::TcpListener::bind(SocketAddr::new(ip, port)).await {
            Ok(listener) => return Ok(listener),
            Err(err) => last = Some(err),
        }
    }
    Err(NdError::Network(format!(
        "no free port for the web page between {} and {}: {}",
        PORTS[0],
        PORTS[PORTS.len() - 1],
        last.map(|e| e.to_string()).unwrap_or_default()
    )))
}

fn random_bytes(buf: &mut [u8]) -> Result<()> {
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|e| NdError::Network(format!("could not generate a session secret: {e}")))
}

/// A random 128-bit token in hexadecimal: what the page holds once the PIN
/// has been accepted, and what every media request must carry.
pub fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A four-digit PIN, uniformly distributed.
pub fn random_pin() -> Result<String> {
    // Rejection sampling keeps every PIN equally likely: a plain modulo of a
    // 16-bit value would favour the low ones.
    loop {
        let mut bytes = [0u8; 2];
        random_bytes(&mut bytes)?;
        let value = u16::from_le_bytes(bytes);
        if value < 60_000 {
            return Ok(format!("{:04}", value % 10_000));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_have_four_digits() {
        for _ in 0..50 {
            let pin = random_pin().unwrap();
            assert_eq!(pin.len(), 4, "{pin}");
            assert!(pin.bytes().all(|b| b.is_ascii_digit()), "{pin}");
        }
    }

    #[test]
    fn tokens_are_128_bits_of_hex() {
        let token = random_token().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(token, random_token().unwrap());
    }

    #[test]
    fn a_loopback_port_is_handed_out() {
        assert_ne!(free_loopback_port().unwrap(), 0);
    }
}
