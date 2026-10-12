//! Integration with the system's network services over D-Bus.
//!
//! - NetworkManager: Wi-Fi Direct discovery and groups for Miracast
//!   ([`p2p`]), and the LAN addresses DLNA discovery binds to.
//! - firewalld: the WFD ports, opened on the P2P interface's zone for the
//!   length of a session ([`firewall`]).
//! - The receiver round trip shown while streaming ([`probe`]).
//!
//! The system bus is unavailable under Flatpak, so the D-Bus parts degrade to
//! errors or no-ops there.

pub mod firewall;
mod networkmanager;
pub mod p2p;
pub mod probe;
