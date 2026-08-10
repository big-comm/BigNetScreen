//! Entry point of the BigNetScreen graphical application.

mod app;
mod i18n;

use app::AppModel;
use relm4::RelmApp;

const APP_ID: &str = "br.com.biglinux.BigNetScreen";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    i18n::init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        sandboxed = nd_capture::is_sandboxed(),
        "BigNetScreen starting"
    );

    // Useful in a bug report: which encoder will actually be used.
    let driver = nd_net::detect_gpu_driver();
    match nd_core::pipeline::best_encoder(driver) {
        Ok(encoder) => tracing::info!(?driver, ?encoder, "video encoding"),
        Err(err) => tracing::error!(?driver, %err, "no H.264 encoder available"),
    }

    let app = RelmApp::new(APP_ID);
    app.run::<AppModel>(());
}
