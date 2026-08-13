//! Entry point of the BigNetScreen graphical application.

mod app;
mod i18n;
mod pages;

use app::AppModel;
use relm4::RelmApp;

const APP_ID: &str = "br.com.biglinux.BigNetScreen";

/// The application's version.
///
/// A plain literal on purpose: the packaging tool finds this constant by
/// pattern and rewrites the number when it publishes a release, so the version
/// in the About dialog is the version that was shipped, with nobody having to
/// remember to edit it.
///
/// That is also why it is not read from the environment. A second source would
/// win over this one and the tool's increment would never reach the interface.
pub const APP_VERSION: &str = "0.1.3";

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
