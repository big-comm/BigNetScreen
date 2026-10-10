//! Who joined a Wi-Fi Direct group, according to the kernel's neighbour table.

/// Which peers took an address on this link, according to the kernel.
///
/// Used to tell two very different failures apart when the receiver never
/// opens the RTSP connection:
///
/// - **nothing here**: the receiver completed the pairing and then never
///   joined the group. Re-forming the group is worth a try; telling the person
///   to check their mirroring menu is not, because the receiver never got far
///   enough for that to matter.
/// - **something here**: it joined, took an address, and then went quiet —
///   which usually means the mirroring screen was closed on the device.
///
/// The limit is worth stating: an entry appears once there has been traffic
/// with that peer, so an empty result is evidence, not proof.
pub fn peers_on_link(interface: &str) -> Vec<String> {
    match std::fs::read_to_string("/proc/net/arp") {
        Ok(table) => parse_arp(&table, interface),
        Err(err) => {
            tracing::debug!(%err, "could not read the neighbour table");
            Vec::new()
        }
    }
}

/// Parses `/proc/net/arp`, keeping the peers on one interface.
///
/// Columns: address, HW type, flags, HW address, mask, device. A flags value
/// of `0x0` is an *incomplete* entry — an address the kernel asked about and
/// never got an answer for — so it is not a peer that is there.
fn parse_arp(table: &str, interface: &str) -> Vec<String> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let address = fields.next()?;
            let _hw_type = fields.next()?;
            let flags = fields.next()?;
            let _hw_address = fields.next()?;
            let _mask = fields.next()?;
            let device = fields.next()?;
            (device == interface && flags != "0x0").then(|| address.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_arp;

    const TABLE: &str = "IP address       HW type     Flags       HW address            Mask     Device\n\
192.168.68.115   0x1         0x2         dc:a3:a2:08:73:2f     *        wlp2s0\n\
10.42.0.215      0x1         0x2         56:44:a3:49:c2:ae     *        p2p-wlp2s0-0\n\
10.42.0.99       0x1         0x0         00:00:00:00:00:00     *        p2p-wlp2s0-0\n";

    #[test]
    fn only_peers_on_the_asked_for_link_are_returned() {
        // The machine is on its home network at the same time; a receiver
        // there is not a peer of the Wi-Fi Direct group.
        assert_eq!(super::parse_arp(TABLE, "p2p-wlp2s0-0"), vec!["10.42.0.215"]);
        assert_eq!(parse_arp(TABLE, "wlp2s0"), vec!["192.168.68.115"]);
    }

    #[test]
    fn an_unanswered_entry_is_not_a_peer() {
        // Flags `0x0` is an address the kernel asked about and never heard
        // back from. Counting it would report a receiver that joined when
        // nothing did, and turn a useful retry into a misleading message.
        assert!(!parse_arp(TABLE, "p2p-wlp2s0-0").contains(&"10.42.0.99".to_string()));
    }

    #[test]
    fn a_link_with_nobody_on_it_is_empty() {
        assert!(parse_arp(TABLE, "p2p-wlp2s0-9").is_empty());
        assert!(parse_arp("", "p2p-wlp2s0-0").is_empty());
        // A truncated line must not panic.
        assert!(parse_arp("IP address\n10.42.0.1 0x1\n", "p2p-wlp2s0-0").is_empty());
    }
}
