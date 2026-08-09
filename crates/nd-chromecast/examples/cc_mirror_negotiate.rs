//! Negotiates a Cast **mirroring** session and prints the result.
//!
//! cargo run -p nd-chromecast --example cc_mirror_negotiate -- <IP>
//!
//! It streams nothing yet: it checks that the device accepts the offer and
//! reports which UDP port it expects RTP on. This is the step that confirms
//! the low-latency path before investing in packetisation.

use std::net::IpAddr;

use nd_chromecast::cast::CastChannel;
use nd_chromecast::mirror::{self, MirrorConfig, MIRRORING_APP_ID};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,nd_chromecast=debug")),
        )
        .init();

    let ip: IpAddr = std::env::args()
        .nth(1)
        .ok_or("usage: cc_mirror_negotiate <receiver IP>")?
        .parse()?;

    let with_audio = std::env::args().nth(2).as_deref() != Some("video-only");

    println!("conectando a {ip}…");
    let channel = CastChannel::connect(ip).await?;

    println!("iniciando o app de espelhamento {MIRRORING_APP_ID}…");
    let app = channel.launch(MIRRORING_APP_ID).await?;
    println!("app: transport={}", app.transport_id);

    let cfg = MirrorConfig {
        with_audio,
        ..Default::default()
    };
    println!(
        "oferecendo h264 {}x{}@{}{}…",
        cfg.width,
        cfg.height,
        cfg.fps,
        if with_audio {
            " + opus"
        } else {
            " (video only)"
        }
    );

    match mirror::negotiate(&channel, &app, ip, &cfg).await {
        Ok(session) => {
            println!("\n✅ espelhamento negociado");
            println!("   receiver UDP port: {}", session.answer.udp_port);
            println!(
                "   streams aceitas:       {:?}",
                session.answer.send_indexes
            );
            println!("   receiver SSRCs:    {:?}", session.answer.ssrcs);
            if let Some(v) = session.video() {
                println!("   video: ssrc={} pt={}", v.ssrc, v.payload_type);
            }
            if let Some(a) = session.audio() {
                println!("   audio: ssrc={} pt={}", a.ssrc, a.payload_type);
            }
        }
        Err(err) => {
            println!("\n❌ falhou: {err}");
            return Err(err.into());
        }
    }

    let _ = channel.stop_app(&app).await;
    Ok(())
}
