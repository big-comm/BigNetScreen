//! Entry point of the BigNetScreen graphical application.

mod app;
mod i18n;
mod pages;

use app::AppModel;
use relm4::RelmApp;

const APP_ID: &str = "br.com.biglinux.BigNetScreen";

/// The version this build reports.
///
/// `APP_VERSION` from the build environment when the packaging sets it — the
/// package is numbered by date and `Cargo.toml` cannot know that number — and
/// the crate's own version otherwise, so a plain `cargo build` still says
/// something true.
/// An **empty** value counts as absent, which is not pedantry: `make` passes
/// the variable through whether or not it was given one, so the ordinary
/// `make` would otherwise build a binary that reports no version at all.
pub const APP_VERSION: &str = match option_env!("APP_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

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
        version = APP_VERSION,
        sandboxed = nd_capture::is_sandboxed(),
        "BigNetScreen starting"
    );

    // Useful in a bug report: which encoder will actually be used.
    let driver = nd_net::detect_gpu_driver();
    match nd_core::pipeline::best_encoder(driver) {
        Ok(encoder) => tracing::info!(?driver, ?encoder, "video encoding"),
        Err(err) => tracing::error!(?driver, %err, "no H.264 encoder available"),
    }

    // Read once at start-up so that the first session already obeys them, and
    // so a broken settings file is reported here rather than at the moment
    // someone hits cast.
    let settings = nd_core::settings::current();
    tracing::info!(?settings, "preferences");

    let app = RelmApp::new(APP_ID);
    relm4::set_global_css(include_str!("style.css"));
    app.run::<AppModel>(());
}
