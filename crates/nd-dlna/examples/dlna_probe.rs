//! Drives a renderer through one session by hand, printing each step.
//!
//! The session path has three places where a television can disagree with us
//! and only one of them shows up in the service's log as anything but a
//! timeout: discovery, the description, and each SOAP action. This runs them
//! in order and prints what came back, which is how the `Content-Length`
//! framing bug was found.
//!
//! ```sh
//! cargo run -p nd-dlna --example dlna_probe
//! ```

use nd_core::sink::Sink;
use nd_dlna::{avtransport, ssdp, upnp, DlnaSink};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
        )
        .init();

    println!("searching for renderers…");
    let found = ssdp::search().await?;
    if found.is_empty() {
        println!("none answered");
        return Ok(());
    }
    for announcement in &found {
        println!("  {} at {}", announcement.usn, announcement.location);
    }

    let announcement = &found[0];
    let sink = DlnaSink::describe(announcement).await?;
    let location = upnp::Endpoint::parse(&announcement.location).unwrap();
    let description = upnp::describe(&location).await?;
    let control = location
        .resolve(&upnp::control_url(&description, "AVTransport:1").unwrap())
        .unwrap();
    println!("\n{} → {}", sink.info().display_name, control.url());

    // A URL that answers nothing: this probe is about whether the *control*
    // conversation works, and a renderer will still accept and report on one.
    let url = "http://127.0.0.1:1/probe.ts";

    for (name, result) in [
        ("Stop", avtransport::stop(&control).await),
        (
            "SetAVTransportURI",
            avtransport::set_uri(&control, url, "BigNetScreen", (1920, 1080)).await,
        ),
    ] {
        let at = std::time::Instant::now();
        match result {
            Ok(()) => println!("{name}: ok in {:?}", at.elapsed()),
            Err(err) => println!("{name}: FAILED — {err}"),
        }
    }
    match avtransport::transport_state(&control).await {
        Ok(state) => println!("GetTransportInfo: {state:?}"),
        Err(err) => println!("GetTransportInfo: FAILED — {err}"),
    }
    let _ = avtransport::stop(&control).await;
    Ok(())
}
