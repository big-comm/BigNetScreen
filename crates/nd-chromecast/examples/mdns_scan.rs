//! Diagnóstico: usa o NOSSO mdns-sd para varrer vários serviços mDNS e
//! imprimir tudo que encontrar. Serve para descobrir se o stack está vendo a
//! rede certa (ex.: se o Tailscale está atrapalhando a seleção de interface).
//!
//!   cargo run -p nd-chromecast --example mdns_scan

use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent};

fn main() {
    let daemon = ServiceDaemon::new().expect("criar ServiceDaemon");

    let types = [
        "_googlecast._tcp.local.",
        "_airplay._tcp.local.",
        "_spotify-connect._tcp.local.",
        "_androidtvremote2._tcp.local.",
        "_raop._tcp.local.",
    ];

    let receivers: Vec<_> = types
        .iter()
        .map(|t| (*t, daemon.browse(t).expect("browse")))
        .collect();

    println!("varrendo por 12s (nosso mdns-sd)…\n");
    let start = Instant::now();
    let mut count = 0;
    while start.elapsed() < Duration::from_secs(12) {
        for (ty, rx) in &receivers {
            while let Ok(event) = rx.recv_timeout(Duration::from_millis(80)) {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        count += 1;
                        println!(
                            "[RESOLVED] {ty}\n    nome: {}\n    ip:   {:?}\n    porta: {}\n    fn:   {:?}",
                            info.get_fullname(),
                            info.get_addresses_v4(),
                            info.get_port(),
                            info.get_property_val_str("fn"),
                        );
                    }
                    ServiceEvent::ServiceFound(_, name) => {
                        println!("[found]    {ty} -> {name}");
                    }
                    _ => {}
                }
            }
        }
    }
    println!("\n--- fim: {count} serviços resolvidos ---");
}
